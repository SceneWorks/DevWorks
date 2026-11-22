//! One administrator-provisioned Windows disk budget for this host. Each
//! reservation follows a non-breakaway Win32 Job through its last writer.
//! The LocalSystem broker is the only ordinary writer of the reservation state.

mod broker;
mod native;

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::{null, null_mut};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, KEY_WOW64_64KEY, REG_CREATED_NEW_KEY,
    REG_OPTION_NON_VOLATILE, REG_OPTION_VOLATILE, REG_SZ, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY,
    RegCloseKey, RegCreateKeyExW, RegGetValueW, RegSetValueExW,
};

use crate::disk_budget::{DiskBudget, Owner, Policy, Reservation};
use crate::host::Host;
use crate::{Vars, check, digest};
use broker::{Request, Response};
use native::{Anchor, Process, SecurityDescriptor, anchor, checked, wide};

const POLICY_KEY: &str = r"SOFTWARE\SceneWorks\DevHost";
const POLICY_VALUE: &str = "DiskBudget";
const BOOT_KEY: &str = r"SOFTWARE\SceneWorks\DevHost\Boot";
const BOOT_VALUE: &str = "Id";
const LOCK_WAIT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_secs(2);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    generation: String,
    minimum_free_bytes: u64,
    wait_seconds: u32,
    profiles: BTreeMap<String, u64>,
    worker_images: Vec<PathBuf>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MachinePolicy {
    root: PathBuf,
    directory: PathBuf,
    anchor: Anchor,
    generation: String,
    minimum_free_bytes: u64,
    wait_seconds: u32,
    profiles: BTreeMap<String, u64>,
    worker_images: Vec<PathBuf>,
}

fn failure(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

fn disk_policy(volume: String, generation: String, minimum_free_bytes: u64) -> Policy {
    Policy {
        volume,
        generation,
        minimum_free_bytes,
    }
}

impl MachinePolicy {
    fn open(host: &Host) -> io::Result<Self> {
        let encoded = read_registry(POLICY_KEY, POLICY_VALUE)?
            .ok_or_else(|| failure("Windows disk budget is not provisioned"))?;
        let policy: Self = serde_json::from_str(&encoded).map_err(io::Error::other)?;
        if host.root().canonicalize()? != policy.root {
            return Err(failure(
                "DEV_HOST_ROOT does not match the provisioned disk budget",
            ));
        }
        Ok(policy)
    }

    fn ledger(&self) -> DiskBudget<impl Fn(&File) -> io::Result<()> + '_> {
        DiskBudget::new(
            self.directory.clone(),
            disk_policy(
                self.anchor.volume.clone(),
                self.generation.clone(),
                self.minimum_free_bytes,
            ),
            LOCK_WAIT,
            |file| {
                if anchor(file)? != self.anchor {
                    return Err(failure("Windows disk budget anchor identity changed"));
                }
                Ok(())
            },
        )
    }

    fn reservation_id(&self, boot: &str, owner: &Owner) -> String {
        let identity = digest(&[
            &self.anchor.volume,
            &self.generation,
            &owner.pid.to_string(),
            &owner.creation_time.to_string(),
        ]);
        format!("{boot}:{identity}")
    }

    fn previously_booted(&self, boot: &str, lease: &Reservation) -> bool {
        lease.id.split_once(':').is_some_and(|(previous, _)| {
            previous != boot
                && valid_boot_id(previous)
                && self.reservation_id(previous, &lease.owner) == lease.id
        })
    }

    fn profile(&self, name: &str) -> io::Result<u64> {
        self.profiles
            .get(name)
            .copied()
            .ok_or_else(|| failure(format!("disk budget profile {name:?} is not provisioned")))
    }

    fn request(&self, boot: &str, owner: Owner, profile: &str) -> io::Result<Reservation> {
        Ok(Reservation {
            id: self.reservation_id(boot, &owner),
            owner,
            bytes: self.profile(profile)?,
        })
    }

    fn wait(&self) -> Duration {
        Duration::from_secs(u64::from(self.wait_seconds))
    }
}

fn registry_result(status: WIN32_ERROR) -> io::Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

fn read_registry(key: &str, value: &str) -> io::Result<Option<String>> {
    let key = wide(OsStr::new(key));
    let value = wide(OsStr::new(value));
    let mut size = 0;
    let flags = RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY;
    // SAFETY: the two UTF-16 strings are terminated; this call obtains the byte count.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            flags,
            null_mut(),
            null_mut(),
            &mut size,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    registry_result(status)?;
    if !(2..=64 * 1024).contains(&size) || !size.is_multiple_of(2) {
        return Err(failure("invalid Windows disk budget policy length"));
    }
    let mut data = vec![0u16; size as usize / 2];
    // SAFETY: size is the allocated byte count and the input strings are still valid.
    registry_result(unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            flags,
            null_mut(),
            data.as_mut_ptr().cast(),
            &mut size,
        )
    })?;
    let units = data
        .get(..size as usize / 2)
        .ok_or_else(|| failure("invalid Windows disk budget policy length"))?;
    let [contents @ .., 0] = units else {
        return Err(failure("Windows disk budget policy is unterminated"));
    };
    String::from_utf16(contents)
        .map(Some)
        .map_err(io::Error::other)
}

struct RegistryKey(HKEY);

impl RegistryKey {
    fn create(path: &str, options: u32) -> io::Result<(Self, bool)> {
        let key = wide(OsStr::new(path));
        let mut handle = null_mut();
        let mut disposition = 0;
        let acl = SecurityDescriptor::new("D:P(A;;KA;;;SY)(A;;KA;;;BA)(A;;KR;;;AU)")?;
        // SAFETY: the UTF-16 key and attributes are valid and both outputs are writable.
        registry_result(unsafe {
            RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                0,
                null(),
                options,
                KEY_WOW64_64KEY | KEY_SET_VALUE,
                &acl.attributes(),
                &mut handle,
                &mut disposition,
            )
        })?;
        Ok((Self(handle), disposition == REG_CREATED_NEW_KEY))
    }

    fn set(&self, value: &str, text: &str) -> io::Result<()> {
        let value = wide(OsStr::new(value));
        let encoded = wide(OsStr::new(text));
        let bytes = u32::try_from(encoded.len() * 2).map_err(io::Error::other)?;
        if bytes > 64 * 1024 {
            return Err(failure("Windows disk budget policy is too large"));
        }
        // SAFETY: self owns the key and encoded and value remain valid for the call.
        registry_result(unsafe {
            RegSetValueExW(
                self.0,
                value.as_ptr(),
                0,
                REG_SZ,
                encoded.as_ptr().cast(),
                bytes,
            )
        })
    }
}

impl Drop for RegistryKey {
    fn drop(&mut self) {
        // SAFETY: for_provisioning returned an owned registry key handle.
        unsafe { RegCloseKey(self.0) };
    }
}

fn valid_boot_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn boot_id() -> io::Result<String> {
    let (key, fresh) = RegistryKey::create(BOOT_KEY, REG_OPTION_VOLATILE)?;
    if !fresh {
        let id = read_registry(BOOT_KEY, BOOT_VALUE)?
            .filter(|value| valid_boot_id(value))
            .ok_or_else(|| failure("Windows disk budget boot marker is unreadable"))?;
        return Ok(id);
    }
    // A volatile HKLM key survives broker restarts and disappears only when
    // Windows fully shuts down, so every writer from the former boot is gone.
    let mut random = [0u8; 16];
    // SAFETY: Windows fills this buffer from the system-preferred RNG.
    let status = unsafe {
        BCryptGenRandom(
            null_mut(),
            random.as_mut_ptr(),
            random.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(failure(format!(
            "Windows disk budget could not generate a boot marker ({status:#x})"
        )));
    }
    let id: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    key.set(BOOT_VALUE, &id)?;
    Ok(id)
}

fn provision(host: &Host, settings_path: &Path) -> io::Result<()> {
    let settings: Settings =
        serde_json::from_slice(&std::fs::read(settings_path)?).map_err(io::Error::other)?;
    if settings.generation.trim().is_empty()
        || settings.minimum_free_bytes == 0
        || settings.wait_seconds == 0
        || !settings.profiles.contains_key("local")
        || !settings.profiles.contains_key("runner")
        || settings.profiles.values().any(|&bytes| bytes == 0)
    {
        return Err(failure(
            "disk budget requires a generation, positive sizes and wait, and local and runner profiles",
        ));
    }
    let root = host.root().canonicalize()?;
    let directory = crate::confine(&host.locks().join("disk-budget"), host.root(), host.root())
        .map_err(failure)?;
    let worker_images = settings
        .worker_images
        .iter()
        .map(|path| worker_image(path))
        .collect::<io::Result<Vec<_>>>()?;
    // Atomically claim the single registry key across roots. Only System and
    // administrators can write it or the ledger. Keep interrupted setup closed.
    let (key, fresh) = RegistryKey::create(POLICY_KEY, REG_OPTION_NON_VOLATILE)?;
    if !fresh {
        return Err(failure(
            "Windows disk budget policy already exists or provisioning was interrupted",
        ));
    }
    let sddl = SecurityDescriptor::new("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)")?;
    let path = wide(directory.as_os_str());
    // SAFETY: path is terminated and the security attributes are valid.
    checked(unsafe { CreateDirectoryW(path.as_ptr(), &sddl.attributes()) })?;

    // The host operator first drains all runners and local writers. Pin the
    // native volume and the lock opened by the initialization transaction.
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    let folder = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(&directory)?;
    let volume = anchor(&folder)?.volume;
    let ledger = DiskBudget::new(
        directory.clone(),
        disk_policy(
            volume.clone(),
            settings.generation.clone(),
            settings.minimum_free_bytes,
        ),
        LOCK_WAIT,
        |_: &File| Ok(()),
    );
    let locked_file = ledger.initialize(|| Ok(()))?;
    let anchor = anchor(&locked_file)?;
    if anchor.volume != volume {
        return Err(failure(
            "the disk budget anchor moved to another volume during provisioning",
        ));
    }
    let policy = MachinePolicy {
        root,
        directory,
        anchor,
        generation: settings.generation,
        minimum_free_bytes: settings.minimum_free_bytes,
        wait_seconds: settings.wait_seconds,
        profiles: settings.profiles,
        worker_images,
    };
    key.set(
        POLICY_VALUE,
        &serde_json::to_string(&policy).map_err(io::Error::other)?,
    )
}

fn worker_image(path: &Path) -> io::Result<PathBuf> {
    let image = path.canonicalize()?;
    if !image
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("Runner.Worker.exe"))
        || !image.is_file()
    {
        return Err(failure("expected the installed Runner.Worker.exe"));
    }
    Ok(image)
}

fn runner_hook(vars: &Vars, path: &Path, profile: &OsStr) -> io::Result<i32> {
    let worker =
        Process::open(std::process::id(), false)?.runner_ancestor(&[worker_image(path)?])?;
    let admission = (|| {
        MachinePolicy::open(&checked_host(vars)?)?;
        grant(Request::Runner(text(profile)?.to_owned()))
    })();
    if let Err(error) = admission {
        // Actions can keep evaluating unconditional steps after a hook failure.
        // Stop this verified worker before control can return to those steps.
        eprintln!("dev-host refused runner disk admission: {error}");
        worker.refuse_runner_start()?;
        return Err(error);
    }
    Ok(0)
}

fn checked_host(vars: &Vars) -> io::Result<Host> {
    check::check_host(vars).map_err(failure)
}

fn grant(request: Request) -> io::Result<()> {
    match broker::client(request)? {
        Response::Granted(reservation) => {
            eprintln!(
                "dev-host disk budget reserved {} bytes for PID {}",
                reservation.bytes, reservation.owner.pid
            );
            Ok(())
        }
        _ => Err(failure(
            "disk budget broker returned an unexpected response",
        )),
    }
}

/// Admit this shim invocation, or confirm that its encompassing Job already holds a lease.
pub fn local(host: &Host) -> io::Result<()> {
    MachinePolicy::open(host)?;
    grant(Request::Local)
}

pub fn command(vars: &Vars, args: &[OsString]) -> io::Result<i32> {
    if let [command, flag, path, profile] = args
        && command == "runner"
        && flag == "--worker"
    {
        return runner_hook(vars, Path::new(path), profile);
    }
    let host = checked_host(vars)?;
    if let [command, path, drained] = args
        && command == "provision"
        && drained == "--drained"
    {
        provision(&host, Path::new(path))?;
        return Ok(0);
    }

    let policy = MachinePolicy::open(&host)?;
    match args {
        [command] if command == "serve" => broker::serve(policy)?,
        [command] if command == "status" => {
            let response = broker::client(Request::Status)?;
            println!(
                "{}",
                serde_json::to_string(&response).map_err(io::Error::other)?
            );
        }
        [command, profile] if command == "grow" => {
            grant(Request::Grow(text(profile)?.to_owned()))?;
        }
        [command, profile, separator, executable, tail @ ..]
            if command == "run" && separator == "--" =>
        {
            grant(Request::Run(text(profile)?.to_owned()))?;
            return Ok(Command::new(executable)
                .args(tail)
                .status()?
                .code()
                .unwrap_or(1));
        }
        _ => {
            return Err(failure(
                "usage: dev-host disk-budget provision SETTINGS.json --drained | serve | status | runner --worker WORKER.exe PROFILE | grow PROFILE | run PROFILE -- COMMAND [ARGS...]",
            ));
        }
    }
    Ok(0)
}

fn text(value: &OsStr) -> io::Result<&str> {
    value
        .to_str()
        .ok_or_else(|| failure("disk budget parameters must be valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk_budget::{Admission, Writers};
    use crate::testutil::Scratch;
    use io::ErrorKind;
    use native::{Job, free_bytes};
    use std::fs;
    use std::process::{Child, ExitStatus, Stdio};
    use std::time::Instant;

    const BOOT: &str = "11111111111111111111111111111111";

    const CHILD_TEST: &str = "windows_disk_budget::tests::writer_fixture";
    const FIXTURE: &str = "DEV_HOST_TEST_DISK_BUDGET_WRITER";

    fn test_policy(scratch: &Scratch, minimum_free_bytes: u64) -> MachinePolicy {
        let volume = anchor(&File::create(scratch.path().join("probe")).unwrap())
            .unwrap()
            .volume;
        let ledger = DiskBudget::new(
            scratch.path().to_owned(),
            disk_policy(volume, "test".to_owned(), minimum_free_bytes),
            LOCK_WAIT,
            |_: &File| Ok(()),
        );
        MachinePolicy {
            root: scratch.path().to_owned(),
            directory: scratch.path().to_owned(),
            anchor: anchor(&ledger.initialize(|| Ok(())).unwrap()).unwrap(),
            generation: "test".to_owned(),
            minimum_free_bytes,
            wait_seconds: 1,
            profiles: BTreeMap::from([("local".to_owned(), 100)]),
            worker_images: Vec::new(),
        }
    }

    fn until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ready() {
            assert!(Instant::now() < deadline, "timed out waiting for fixture");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    struct Fixture(Child);

    impl Fixture {
        fn launch(directory: &Path, role: &str) -> Self {
            Self(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--ignored", "--exact", CHILD_TEST])
                    .env(FIXTURE, role)
                    .current_dir(directory)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            )
        }

        fn wait(&mut self) -> ExitStatus {
            let mut status = None;
            until(|| {
                status = self.0.try_wait().unwrap();
                status.is_some()
            });
            status.unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    #[ignore = "helper launched by the Windows Job lifetime test"]
    fn writer_fixture() {
        let Ok(role) = std::env::var(FIXTURE) else {
            return;
        };
        if role == "owner" {
            until(|| Path::new("spawn").exists());
            let directory = std::env::current_dir().unwrap();
            let mut descendant = Fixture::launch(&directory, "descendant");
            until(|| Path::new("writing").exists());
            assert!(descendant.wait().success());
        } else {
            fs::write("writing", b"active").unwrap();
            until(|| Path::new("finish").exists());
        }
    }

    #[test]
    fn charges_live_descendant_after_owner_cancellation_and_admits_after_release() {
        let scratch = Scratch::new("budget-job");
        let policy = test_policy(&scratch, 10);
        let mut fixture = Fixture::launch(scratch.path(), "owner");
        let process = Process::open(fixture.0.id(), true).unwrap();
        let request = policy
            .request(BOOT, process.owner.clone(), "local")
            .unwrap();
        let job = Job::create().unwrap();
        job.assign(&process).unwrap();
        assert!(job.contains(&process).unwrap());

        let ledger = policy.ledger();
        let admit = |lease| ledger.reserve(lease, || Ok(211)).unwrap();
        assert!(matches!(admit(request.clone()), Admission::Granted(_)));
        let next = policy
            .request(BOOT, Process::current().unwrap().owner, "local")
            .unwrap();
        assert!(matches!(admit(next.clone()), Admission::Granted(_)));
        assert!(
            ledger
                .release(&next.id, &next.owner, |_| Writers::Stopped)
                .unwrap()
        );

        let too_large = Reservation { bytes: 102, ..next };
        assert!(matches!(admit(too_large.clone()), Admission::Wait { .. }));
        fs::write(scratch.path().join("spawn"), b"").unwrap();
        until(|| scratch.path().join("writing").exists());

        let mut old_owner = process.owner.clone();
        old_owner.creation_time -= 1;
        assert!(Process::verified(&old_owner).is_err());
        fixture.0.kill().unwrap();
        fixture.wait();
        assert_eq!(job.writers(), Writers::Active);
        assert!(matches!(admit(too_large.clone()), Admission::Wait { .. }));
        assert_eq!(
            ledger
                .release(&request.id, &request.owner, |_| job.writers())
                .unwrap_err()
                .kind(),
            ErrorKind::WouldBlock
        );
        let uninspectable = job.without_query_for_test().unwrap();
        assert_eq!(uninspectable.writers(), Writers::Unknown);
        assert_eq!(ledger.reconcile(|_| uninspectable.writers()).unwrap(), 0);

        fs::write(scratch.path().join("finish"), b"").unwrap();
        until(|| job.writers() == Writers::Stopped);
        assert!(
            ledger
                .release(&request.id, &request.owner, |_| job.writers())
                .unwrap()
        );
        assert!(
            !ledger
                .release(&request.id, &request.owner, |_| job.writers())
                .unwrap()
        );
        assert!(matches!(admit(too_large), Admission::Granted(_)));
    }

    #[test]
    fn admits_recovery_only_for_an_authenticated_previous_boot_identity() {
        let scratch = Scratch::new("budget-boot");
        let policy = test_policy(&scratch, 1);
        let previous_boot = "22222222222222222222222222222222";
        let process = Process::current().unwrap();
        let mut lease = policy
            .request(previous_boot, process.owner, "local")
            .unwrap();
        assert!(!policy.previously_booted(previous_boot, &lease));
        assert!(policy.previously_booted(BOOT, &lease));
        lease.owner.creation_time += 1;
        assert!(!policy.previously_booted(BOOT, &lease));
    }

    #[test]
    fn verifies_native_volume_and_refuses_stale_state_or_replacement_anchor() {
        let scratch = Scratch::new("budget-anchor");
        let policy = test_policy(&scratch, 1);
        let ledger = policy.ledger();
        let state = scratch.path().join("disk-budget.json");
        let stale = fs::read(&state).unwrap();
        let lease = policy
            .request(BOOT, Process::current().unwrap().owner, "local")
            .unwrap();
        assert!(matches!(
            ledger
                .reserve(lease, || free_bytes(&policy.anchor.volume))
                .unwrap(),
            Admission::Granted(_)
        ));
        let current = fs::read(&state).unwrap();
        fs::write(&state, stale).unwrap();
        assert_eq!(
            ledger.reservations().unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        fs::write(state, current).unwrap();

        let lock = scratch.path().join("disk-budget.lock");
        let replaced = scratch.path().join("old-anchor");
        fs::rename(&lock, &replaced).unwrap();
        fs::copy(replaced, lock).unwrap();
        assert_eq!(
            ledger.reservations().unwrap_err().to_string(),
            "Windows disk budget anchor identity changed"
        );
    }
}
