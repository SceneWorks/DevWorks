//! One administrator-provisioned Windows disk budget for this host. Each
//! reservation follows a non-breakaway Win32 Job through its last writer.

mod native;

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, KEY_WOW64_64KEY, REG_OPTION_NON_VOLATILE, REG_SZ,
    RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY, RegCloseKey, RegCreateKeyExW, RegGetValueW,
    RegSetValueExW,
};
use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};

use crate::disk_budget::{Admission, DiskBudget, Owner, Policy, Reservation, Writers};
use crate::host::Host;
use crate::{Vars, check, digest};
use native::{Anchor, Job, Process, anchor, free_bytes, wide};

const POLICY_KEY: &str = r"SOFTWARE\SceneWorks\DevHost";
const POLICY_VALUE: &str = "DiskBudget";
const LOCK_WAIT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_secs(2);
const KEEPER: &str = "__disk-budget-keeper";

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
        let encoded =
            read_policy()?.ok_or_else(|| failure("Windows disk budget is not provisioned"))?;
        let policy: Self = serde_json::from_str(&encoded).map_err(io::Error::other)?;
        if host.root().canonicalize()? != policy.root
            || host.locks().join("disk-budget").canonicalize()? != policy.directory
        {
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

    fn job_name(&self, owner: &Owner) -> String {
        let identity = digest(&[
            &self.anchor.volume,
            &self.generation,
            &owner.pid.to_string(),
            &owner.creation_time.to_string(),
        ]);
        format!(r"Global\DevHostDiskBudget-{identity}")
    }

    fn request(&self, owner: Owner, profile: &str) -> io::Result<Reservation> {
        let bytes = *self.profiles.get(profile).ok_or_else(|| {
            failure(format!(
                "disk budget profile {profile:?} is not provisioned"
            ))
        })?;
        Ok(Reservation {
            id: self.job_name(&owner),
            owner,
            bytes,
        })
    }

    fn admit(&self, request: Reservation) -> io::Result<()> {
        let ledger = self.ledger();
        let deadline = Instant::now() + Duration::from_secs(u64::from(self.wait_seconds));
        let mut reported_wait = false;
        loop {
            match ledger.reserve(request.clone(), || free_bytes(&self.anchor.volume))? {
                Admission::Granted(grant) => {
                    eprintln!(
                        "dev-host disk budget reserved {} bytes for PID {}",
                        grant.bytes, grant.owner.pid
                    );
                    return Ok(());
                }
                Admission::Wait {
                    available_bytes,
                    required_bytes,
                } => {
                    if Instant::now() >= deadline {
                        return Err(failure(format!(
                            "disk budget wait expired with {available_bytes} bytes free and {required_bytes} bytes required"
                        )));
                    }
                    if !reported_wait {
                        eprintln!(
                            "dev-host disk budget waiting with {available_bytes} bytes free and {required_bytes} bytes required"
                        );
                        reported_wait = true;
                    }
                    std::thread::sleep(
                        POLL.min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
            }
        }
    }

    fn enclosing_job(&self, process: &Process) -> io::Result<Option<Reservation>> {
        for lease in self.ledger().reservations()? {
            if lease.id != self.job_name(&lease.owner) {
                return Err(failure("disk budget has an invalid Windows job identity"));
            }
            // An uninspectable job cannot authorize an inherited reservation.
            if let Ok(job) = Job::open(&lease.id)
                && let Ok(owner) = Process::verified(&lease.owner)
                && job.contains(&owner)?
                && job.contains(process)?
            {
                return Ok(Some(lease));
            }
        }
        Ok(None)
    }
}

fn registry_result(status: WIN32_ERROR) -> io::Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

fn read_policy() -> io::Result<Option<String>> {
    let key = wide(OsStr::new(POLICY_KEY));
    let value = wide(OsStr::new(POLICY_VALUE));
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
    if size < 2 || size > 64 * 1024 || size % 2 != 0 {
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
    fn for_provisioning() -> io::Result<Self> {
        let key = wide(OsStr::new(POLICY_KEY));
        let mut handle = null_mut();
        // SAFETY: key is terminated and handle points to a writable HKEY.
        registry_result(unsafe {
            RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WOW64_64KEY | KEY_SET_VALUE,
                null(),
                &mut handle,
                null_mut(),
            )
        })?;
        Ok(Self(handle))
    }

    fn set_policy(&self, policy: &MachinePolicy) -> io::Result<()> {
        let value = wide(OsStr::new(POLICY_VALUE));
        let encoded = wide(OsStr::new(
            &serde_json::to_string(policy).map_err(io::Error::other)?,
        ));
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

fn provision(host: &Host, settings_path: &Path) -> io::Result<()> {
    let key = RegistryKey::for_provisioning()?;
    if read_policy()?.is_some() {
        return Err(failure("Windows disk budget is already provisioned"));
    }
    let settings: Settings =
        serde_json::from_slice(&std::fs::read(settings_path)?).map_err(io::Error::other)?;
    if settings.generation.trim().is_empty()
        || settings.minimum_free_bytes == 0
        || settings.wait_seconds == 0
        || !settings.profiles.contains_key("local")
        || settings.profiles.values().any(|&bytes| bytes == 0)
    {
        return Err(failure(
            "disk budget requires a generation, positive sizes and wait, and a local profile",
        ));
    }
    let root = host.root().canonicalize()?;
    let directory = crate::confine(&host.locks().join("disk-budget"), host.root(), host.root())
        .map_err(failure)?;
    let worker_images = settings
        .worker_images
        .iter()
        .map(|path| path.canonicalize())
        .collect::<io::Result<Vec<_>>>()?;
    if worker_images.iter().any(|image| {
        !image
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("Runner.Worker.exe"))
            || !image.is_file()
    }) {
        return Err(failure(
            "each worker image must be an installed Runner.Worker.exe",
        ));
    }
    // The operator pre-creates the ACL-protected directory and attests the entire
    // host is drained before invoking this command. Pin the directory's volume.
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
    key.set_policy(&MachinePolicy {
        root,
        directory,
        anchor,
        generation: settings.generation,
        minimum_free_bytes: settings.minimum_free_bytes,
        wait_seconds: settings.wait_seconds,
        profiles: settings.profiles,
        worker_images,
    })
}

fn checked_host(vars: &Vars) -> io::Result<Host> {
    check::check_host(vars).map_err(failure)
}

fn begin(policy: &MachinePolicy, process: &Process, profile: &str) -> io::Result<ChildStdin> {
    let request = policy.request(process.owner.clone(), profile)?;
    let job = Job::create(&request.id)?;
    let mut keeper = Command::new(std::env::current_exe()?)
        .args([
            KEEPER,
            &request.owner.pid.to_string(),
            &request.owner.creation_time.to_string(),
        ])
        .env_remove("RUNNER_TRACKING_ID")
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut pipe = keeper.stdin.take().expect("piped stdin");
    job.assign(process)?;
    let duplicated = job.duplicate_into(keeper.as_raw_handle())?;
    pipe.write_all(&duplicated.to_ne_bytes())?;
    let mut ready = String::new();
    BufReader::new(keeper.stdout.take().expect("piped stdout")).read_line(&mut ready)?;
    if ready != "ready\n" {
        return Err(failure(
            "the disk budget keeper could not hold the Windows Job",
        ));
    }
    policy.admit(request)?;
    Ok(pipe)
}

fn reap(policy: &MachinePolicy, owner: &Owner) -> io::Result<()> {
    let id = policy.job_name(owner);
    // The creator duplicates the held handle into the keeper. A later object
    // with the same name and a missing name are both inadequate drain proof.
    let mut pipe = io::stdin().lock();
    let mut raw = [0; std::mem::size_of::<usize>()];
    pipe.read_exact(&mut raw)?;
    let job = Job::from_duplicate(usize::from_ne_bytes(raw))?;
    if !job.contains(&Process::verified(owner)?)? {
        return Err(failure(
            "disk budget keeper received a Job without its verified owner",
        ));
    }
    println!("ready");
    io::stdout().flush()?;
    io::copy(&mut pipe, &mut io::sink())?;
    let ledger = policy.ledger();
    loop {
        match ledger.release(&id, owner, |_| job.writers()) {
            Ok(_) => return Ok(()),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                std::thread::sleep(POLL);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Verify the machine policy before the cargo shim creates any managed build paths.
pub fn verify_local_host(host: &Host) -> Result<(), String> {
    MachinePolicy::open(host)
        .and_then(|policy| policy.ledger().reservations())
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Admit this shim invocation, or confirm it is already covered by a parent Job.
pub fn local(vars: &Vars) -> io::Result<Option<ChildStdin>> {
    let policy = MachinePolicy::open(&checked_host(vars)?)?;
    let process = Process::current()?;
    if policy.enclosing_job(&process)?.is_some() {
        Ok(None)
    } else {
        begin(&policy, &process, "local").map(Some)
    }
}

pub fn command(vars: &Vars, args: &[OsString]) -> io::Result<i32> {
    let host = checked_host(vars)?;
    match args {
        [command, path, drained] if command == "provision" && drained == "--drained" => {
            provision(&host, Path::new(path))?;
        }
        [command] if command == "status" => {
            let policy = MachinePolicy::open(&host)?;
            let reservations = policy.ledger().reservations()?;
            println!(
                "{}",
                serde_json::to_string(&reservations).map_err(io::Error::other)?
            );
        }
        [command, profile] if command == "runner" => {
            let policy = MachinePolicy::open(&host)?;
            let worker = Process::current()?.runner_ancestor(&policy.worker_images)?;
            begin(&policy, &worker, text(profile)?)?;
        }
        [command, profile] if command == "grow" => {
            let policy = MachinePolicy::open(&host)?;
            let lease = policy
                .enclosing_job(&Process::current()?)?
                .ok_or_else(|| failure("disk budget top-up requires a live admitted Job"))?;
            policy.admit(policy.request(lease.owner, text(profile)?)?)?;
        }
        [command, profile, separator, executable, tail @ ..]
            if command == "run" && separator == "--" =>
        {
            let policy = MachinePolicy::open(&host)?;
            let process = Process::current()?;
            let _keeper = if let Some(lease) = policy.enclosing_job(&process)? {
                policy.admit(policy.request(lease.owner, text(profile)?)?)?;
                None
            } else {
                Some(begin(&policy, &process, text(profile)?)?)
            };
            return Ok(Command::new(executable)
                .args(tail)
                .status()?
                .code()
                .unwrap_or(1));
        }
        _ => {
            return Err(failure(
                "usage: dev-host disk-budget provision SETTINGS.json --drained | status | runner PROFILE | grow PROFILE | run PROFILE -- COMMAND [ARGS...]",
            ));
        }
    }
    Ok(0)
}

pub fn keeper(vars: &Vars, args: &[OsString]) -> io::Result<()> {
    let [pid, creation_time] = args else {
        return Err(failure("invalid disk budget keeper identity"));
    };
    let owner = Owner {
        pid: text(pid)?.parse().map_err(io::Error::other)?,
        creation_time: text(creation_time)?.parse().map_err(io::Error::other)?,
    };
    reap(&MachinePolicy::open(&checked_host(vars)?)?, &owner)
}

fn text(value: &OsStr) -> io::Result<&str> {
    value
        .to_str()
        .ok_or_else(|| failure("disk budget parameters must be valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;
    use std::fs;
    use std::process::{Child, ExitStatus};

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
        let process = Process::open_for_test(fixture.0.id()).unwrap();
        let request = policy.request(process.owner.clone(), "local").unwrap();
        let job = Job::create(&request.id).unwrap();
        job.assign(&process).unwrap();
        assert!(job.contains(&process).unwrap());

        let ledger = policy.ledger();
        let admit = |lease| ledger.reserve(lease, || Ok(211)).unwrap();
        assert!(matches!(admit(request.clone()), Admission::Granted(_)));
        let next = policy
            .request(Process::current().unwrap().owner, "local")
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
        old_owner.creation_time += 1;
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
    fn verifies_native_volume_and_refuses_stale_state_or_replacement_anchor() {
        let scratch = Scratch::new("budget-anchor");
        let policy = test_policy(&scratch, 1);
        let ledger = policy.ledger();
        let state = scratch.path().join("disk-budget.json");
        let stale = fs::read(&state).unwrap();
        let lease = policy
            .request(Process::current().unwrap().owner, "local")
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
