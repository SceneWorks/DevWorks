//! `dev-host check`: the root is set, sits on its own volume, and carries the marker; this
//! host's folder `hosts/<host>/` exists; and every `required_mount` in `dev-host.conf` is
//! mounted.
//!
//! Nothing here creates the root or any folder above it; only `dev-host init-host` creates the
//! host's folder ([`init_host`]). A failure names the volume or folder that is missing, so a
//! missing drive reads as a missing drive rather than a build error.

use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::{self, CONFIG_FILE, REQUIRED_MOUNT};
use crate::host::{self, Host};
use crate::{ROOT_VAR, Vars};

/// The marker file written once when a root is set up.
pub const MARKER: &str = ".dev-host-root";

/// How long `check` waits for the file system before reporting the volume as unresponsive.
/// Kept under the 10 s the requirements allow, leaving room for process start and output.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(8);

/// Decides whether a root sits on the intended volume rather than the system volume.
pub trait VolumeProbe: Send + Sync {
    /// Returns `Err(reason)` when `root` (which exists) is on the wrong volume.
    fn verify(&self, root: &Path) -> Result<(), String>;

    /// Returns `Err(reason)` when `path`, a `required_mount`, is not a mounted volume.
    fn verify_mount(&self, path: &Path) -> Result<(), String>;
}

/// The real probe for this platform.
///
/// - Unix: the root's device ID must differ from that of `/`. On macOS `/` and the Data
///   volume report the same device, so a folder anywhere on the internal disk fails.
/// - Windows: the drive of the resolved root (after junctions, directory symlinks and SUBST)
///   must be a mounted fixed volume other than the system drive.
///
/// Both follow symlinks: Unix `metadata` reports the device of the link's target.
pub struct SystemVolumeProbe;

#[cfg(unix)]
impl VolumeProbe for SystemVolumeProbe {
    fn verify(&self, root: &Path) -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        let root_dev = std::fs::metadata(root)
            .map_err(|e| format!("cannot stat {}: {e}", root.display()))?
            .dev();
        let system_dev = std::fs::metadata("/")
            .map_err(|e| format!("cannot stat /: {e}"))?
            .dev();
        if root_dev == system_dev {
            Err(format!(
                "{} is on the system volume (same device as /)",
                root.display()
            ))
        } else {
            Ok(())
        }
    }

    fn verify_mount(&self, path: &Path) -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).map_err(|e| format!("cannot stat it: {e}"))?;
        if !meta.is_dir() {
            return Err("it is not a folder".to_owned());
        }
        let system_dev = std::fs::metadata("/")
            .map_err(|e| format!("cannot stat /: {e}"))?
            .dev();
        if meta.dev() == system_dev {
            return Err("it is on the system volume (same device as /)".to_owned());
        }
        // A mount point sits on another device than the folder holding it; a plain folder on
        // some other mounted volume does not.
        let parent = path.parent().unwrap_or(path);
        let parent_dev = std::fs::metadata(parent)
            .map_err(|e| format!("cannot stat {}: {e}", parent.display()))?
            .dev();
        if parent != path && meta.dev() == parent_dev {
            return Err(format!(
                "it is a folder on the volume of {}, not a mount point",
                parent.display()
            ));
        }
        Ok(())
    }
}

#[cfg(windows)]
impl VolumeProbe for SystemVolumeProbe {
    fn verify(&self, root: &Path) -> Result<(), String> {
        use std::os::windows::ffi::OsStrExt;

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetDriveTypeW(root_path_name: *const u16) -> u32;
        }
        // winbase.h: DRIVE_FIXED.
        const DRIVE_FIXED: u32 = 3;

        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
        let drive_of = |path: &Path, shown: &str| -> Result<char, String> {
            let Some(drive) = drive_letter(path) else {
                return Err(format!(
                    "{shown} is not on a drive letter (expected a fixed volume such as E:)"
                ));
            };
            if system.eq_ignore_ascii_case(&format!("{drive}:")) {
                return Err(format!("{shown} is on the system drive {system}"));
            }
            Ok(drive)
        };
        drive_of(root, &root.display().to_string())?;
        // Junctions, directory symlinks and SUBST drives can send `E:\dev-host` to another
        // drive, so the drive that counts is the one the resolved folder is on.
        let resolved = root
            .canonicalize()
            .map_err(|e| format!("cannot resolve {}: {e}", root.display()))?;
        let drive = drive_of(
            &resolved,
            &format!("{} (resolved to {})", root.display(), resolved.display()),
        )?;
        let wide: Vec<u16> = std::ffi::OsStr::new(&format!("{drive}:\\"))
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call.
        let kind = unsafe { GetDriveTypeW(wide.as_ptr()) };
        if kind == DRIVE_FIXED {
            Ok(())
        } else {
            Err(format!(
                "drive {drive}: is not a mounted fixed volume (drive type {kind})"
            ))
        }
    }

    fn verify_mount(&self, path: &Path) -> Result<(), String> {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetDriveTypeW(root_path_name: *const u16) -> u32;
            fn GetVolumePathNameW(file_name: *const u16, volume_path: *mut u16, len: u32) -> i32;
        }
        // winbase.h: DRIVE_FIXED, DRIVE_REMOTE.
        const DRIVE_FIXED: u32 = 3;
        const DRIVE_REMOTE: u32 = 4;

        let meta = std::fs::metadata(path).map_err(|e| format!("cannot open it: {e}"))?;
        if !meta.is_dir() {
            return Err("it is not a folder".to_owned());
        }
        // Ask for the volume of the plain spelling (`M:\x`, `\\server\share`): the drive type
        // of a verbatim `\\?\` volume path is not reliable.
        let text = path.as_os_str().to_string_lossy();
        let plain = match text.strip_prefix(r"\\?\UNC\") {
            Some(rest) => format!(r"\\{rest}"),
            None => text.strip_prefix(r"\\?\").unwrap_or(&*text).to_owned(),
        };
        let wide: Vec<u16> = std::ffi::OsStr::new(&plain)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut volume = vec![0u16; 1024];
        // SAFETY: `wide` is NUL-terminated and `volume` is writable for its full length.
        let ok =
            unsafe { GetVolumePathNameW(wide.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) };
        if ok == 0 {
            return Err(format!(
                "cannot find its volume: {}",
                std::io::Error::last_os_error()
            ));
        }
        let end = volume.iter().position(|&c| c == 0).unwrap_or(volume.len());
        let shown = std::ffi::OsString::from_wide(&volume[..end]);
        // SAFETY: `volume` is NUL-terminated (GetVolumePathNameW succeeded).
        let kind = unsafe { GetDriveTypeW(volume.as_ptr()) };
        if kind == DRIVE_FIXED || kind == DRIVE_REMOTE {
            Ok(())
        } else {
            Err(format!(
                "its volume {} is not a mounted fixed or remote volume (drive type {kind})",
                shown.to_string_lossy()
            ))
        }
    }
}

/// The drive letter of a Windows path (`E:\x`, `\\?\E:\x`), if any.
fn drive_letter(path: &Path) -> Option<char> {
    match path.components().next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            std::path::Prefix::Disk(d) | std::path::Prefix::VerbatimDisk(d) => {
                Some((d as char).to_ascii_uppercase())
            }
            _ => None,
        },
        _ => None,
    }
}

/// The volume a root should live on, when its path names one: `/Volumes/<name>` on macOS, the
/// drive on Windows (`E:`, also for a `\\?\E:\` path).
pub fn volume_of(root: &Path) -> Option<String> {
    let mut parts = root.components();
    if let Some(Component::Prefix(prefix)) = parts.clone().next() {
        return Some(match drive_letter(root) {
            Some(drive) => format!("{drive}:"),
            None => prefix.as_os_str().to_string_lossy().into_owned(),
        });
    }
    match (parts.next(), parts.next(), parts.next()) {
        (
            Some(Component::RootDir),
            Some(Component::Normal(first)),
            Some(Component::Normal(name)),
        ) if first == "Volumes" => Some(format!("/Volumes/{}", name.to_string_lossy())),
        _ => None,
    }
}

/// Names the missing mount for an error message.
pub fn missing_mount(root: &Path) -> String {
    match volume_of(root) {
        Some(volume) => format!("volume {volume} is not mounted"),
        None => format!("no dev volume is mounted at {} or above it", root.display()),
    }
}

/// Whether the volume that should hold `root` is mounted: the nearest folder at or above `root`
/// exists and, on Unix, sits on a different device than `/`. Touches the file system, so it
/// runs only inside [`check`], never on the timeout path.
fn volume_mounted(root: &Path) -> bool {
    let Some(existing) = root.ancestors().find(|p| p.exists()) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(existing), std::fs::metadata("/")) {
            (Ok(found), Ok(system)) => found.dev() != system.dev(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        // A drive letter or share that resolves at all is mounted.
        let _ = existing;
        true
    }
}

/// The hint for a root that does not exist: its volume is missing, or the folder is.
fn unavailable_hint(root: &Path) -> String {
    if volume_mounted(root) {
        "its volume is mounted, so the folder itself is missing".to_owned()
    } else {
        missing_mount(root)
    }
}

/// The hint for a root the probe rejects: its volume is missing, or the variable names the
/// wrong volume.
fn wrong_volume_hint(root: &Path) -> String {
    if volume_mounted(root) {
        format!("set {ROOT_VAR} to a folder on the dev volume")
    } else {
        missing_mount(root)
    }
}

/// The path a check is probing, so a timeout can name the volume that hung.
#[derive(Default)]
struct Probing(Mutex<Option<PathBuf>>);

impl Probing {
    fn at(&self, path: &Path) {
        if let Ok(mut current) = self.0.lock() {
            *current = Some(path.to_path_buf());
        }
    }

    fn current(&self) -> Option<PathBuf> {
        self.0.lock().ok().and_then(|current| current.clone())
    }
}

/// The root checks: `DEV_HOST_ROOT` is set, names a folder on the intended volume, and holds
/// the marker.
fn verify_root(vars: &Vars, probe: &dyn VolumeProbe, probing: &Probing) -> Result<PathBuf, String> {
    let Some(raw) = vars.get(ROOT_VAR) else {
        return Err(format!(
            "{ROOT_VAR} is not set; set it to this host's root on its dev volume \
             (runner .env or agent environment)"
        ));
    };
    let root = PathBuf::from(raw);
    if !root.is_absolute() {
        return Err(format!("{ROOT_VAR}={raw} is not an absolute path"));
    }
    probing.at(&root);
    match std::fs::metadata(&root) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(format!("{ROOT_VAR}={raw} is not a folder")),
        Err(e) => {
            let hint = unavailable_hint(&root);
            return Err(format!("{ROOT_VAR}={raw} is unavailable ({e}): {hint}"));
        }
    }
    probe
        .verify(&root)
        .map_err(|reason| format!("{ROOT_VAR}={raw}: {reason}; {}", wrong_volume_hint(&root)))?;
    if !root.join(MARKER).is_file() {
        let why = if volume_mounted(&root) {
            "it was never set up".to_owned()
        } else {
            format!("either it was never set up or {}", missing_mount(&root))
        };
        return Err(format!(
            "{ROOT_VAR}={raw} has no {MARKER} marker; it is not a set-up dev-host root ({why})"
        ));
    }
    Ok(root)
}

/// One problem per `required_mount` in `<root>/dev-host.conf` that is not mounted.
fn missing_mounts(
    root: &Path,
    probe: &dyn VolumeProbe,
    probing: &Probing,
) -> Result<Vec<String>, String> {
    let config = config::read(root)?;
    let mut missing = Vec::new();
    for mount in &config.required_mounts {
        probing.at(mount);
        if let Err(reason) = probe.verify_mount(mount) {
            missing.push(format!(
                "{REQUIRED_MOUNT} {} is not mounted ({reason}); mount it, or remove the line \
                 from {}",
                mount.display(),
                root.join(CONFIG_FILE).display()
            ));
        }
    }
    Ok(missing)
}

fn joined(problems: Vec<String>) -> Result<(), String> {
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

fn root_with(vars: &Vars, probe: &dyn VolumeProbe, probing: &Probing) -> Result<PathBuf, String> {
    let root = verify_root(vars, probe, probing)?;
    joined(missing_mounts(&root, probe, probing)?)?;
    Ok(root)
}

fn host_with(
    vars: &Vars,
    probe: &dyn VolumeProbe,
    host_name: &str,
    probing: &Probing,
) -> Result<Host, String> {
    let root = verify_root(vars, probe, probing)?;
    let host = Host::new(root, host_name)?;
    let mut problems = Vec::new();
    let dir = host.dir();
    probing.at(&dir);
    if let Err(e) = crate::confine(&dir, host.root(), host.root()) {
        problems.push(e);
    } else if !dir.is_dir() {
        problems.push(format!(
            "this host's folder {} is missing; create it once with `dev-host init-host`",
            dir.display()
        ));
    }
    problems.extend(missing_mounts(host.root(), probe, probing)?);
    joined(problems)?;
    Ok(host)
}

/// The root checks plus every `required_mount`, without this host's folder: what
/// `dev-host init-host` needs before it creates that folder. Returns the root.
pub fn check_root(vars: &Vars, probe: &dyn VolumeProbe) -> Result<PathBuf, String> {
    root_with(vars, probe, &Probing::default())
}

/// The full check `dev-host check` runs: the root (set, on its own volume, marked), this host's
/// folder `hosts/<host_name>/` (a real folder, not a link), and every `required_mount`. A
/// failure names each missing piece.
pub fn check(vars: &Vars, probe: &dyn VolumeProbe, host_name: &str) -> Result<Host, String> {
    host_with(vars, probe, host_name, &Probing::default())
}

/// Runs `run` on a worker thread and gives up after `timeout`, so a hung mount fails the check
/// instead of hanging the caller. The error names the path being probed when time ran out.
fn check_within<T: Send + 'static>(
    vars: &Vars,
    timeout: Duration,
    run: impl FnOnce(&Vars, &Probing) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = mpsc::channel();
    let owned = vars.clone();
    let probing = Arc::new(Probing::default());
    let worker = Arc::clone(&probing);
    std::thread::spawn(move || {
        let _ = tx.send(run(&owned, &worker));
    });
    rx.recv_timeout(timeout).unwrap_or_else(|_| {
        let path = probing
            .current()
            .unwrap_or_else(|| PathBuf::from(vars.get(ROOT_VAR).unwrap_or_default()));
        Err(format!(
            "{} did not respond within {}s: its volume is hung or {}",
            path.display(),
            timeout.as_secs(),
            missing_mount(&path)
        ))
    })
}

/// Runs the real [`check`] for this machine, with its OS-reported host name and the standard
/// timeout.
pub fn check_host(vars: &Vars) -> Result<Host, String> {
    check_within(vars, CHECK_TIMEOUT, |vars, probing| {
        let name = host::local_name()?;
        host_with(vars, &SystemVolumeProbe, &name, probing)
    })
}

/// Runs the real [`check_root`] with the standard timeout and pairs the root with this
/// machine's host name, for `dev-host init-host`.
pub fn check_host_root(vars: &Vars) -> Result<Host, String> {
    check_within(vars, CHECK_TIMEOUT, |vars, probing| {
        let root = root_with(vars, &SystemVolumeProbe, probing)?;
        Host::new(root, &host::local_name()?)
    })
}

/// `dev-host init-host`: creates this host's folder and its `locks/` under a root that passes
/// [`check_root`]. Creates nothing else, and nothing above the host's folder except `hosts/`
/// itself; refuses a symlink at any level. Returns the host folder.
pub fn init_host(host: &Host) -> Result<PathBuf, String> {
    let locks = host.locks();
    crate::confine(&locks, host.root(), host.root())?;
    std::fs::create_dir_all(&locks)
        .map_err(|e| format!("cannot create {}: {e}", locks.display()))?;
    Ok(host.dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    const HOST: &str = "mac-a";

    /// Accepts or rejects the root; treats a required mount as mounted when its folder name
    /// starts with `mounted`.
    struct Fake(bool);
    impl VolumeProbe for Fake {
        fn verify(&self, _root: &Path) -> Result<(), String> {
            if self.0 {
                Ok(())
            } else {
                Err("simulated system volume".into())
            }
        }

        fn verify_mount(&self, path: &Path) -> Result<(), String> {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with("mounted") {
                Ok(())
            } else {
                Err("simulated unmounted share".into())
            }
        }
    }

    /// Hangs on the root (`Hang(false)`) or on a required mount (`Hang(true)`).
    struct Hang(bool);
    impl VolumeProbe for Hang {
        fn verify(&self, _root: &Path) -> Result<(), String> {
            if !self.0 {
                std::thread::sleep(Duration::from_secs(5));
            }
            Ok(())
        }

        fn verify_mount(&self, _path: &Path) -> Result<(), String> {
            if self.0 {
                std::thread::sleep(Duration::from_secs(5));
            }
            Ok(())
        }
    }

    fn vars_for(root: &Path) -> Vars {
        [(ROOT_VAR, root.display().to_string())]
            .into_iter()
            .collect()
    }

    /// A marked root with [`HOST`]'s folder.
    fn marked_root(scratch: &Scratch) -> PathBuf {
        let root = scratch.path().join("dev-host");
        std::fs::create_dir_all(root.join("hosts").join(HOST)).unwrap();
        std::fs::write(root.join(MARKER), "dev-host-root v1\n").unwrap();
        root
    }

    #[test]
    fn passes_on_marked_root_on_another_volume() {
        let scratch = Scratch::new("pass");
        let root = marked_root(&scratch);
        let host = check(&vars_for(&root), &Fake(true), HOST).unwrap();
        assert_eq!(host.root(), root);
        assert_eq!(host.dir(), root.join("hosts").join(HOST));
    }

    #[test]
    fn fails_naming_a_missing_host_folder_without_creating_it() {
        let scratch = Scratch::new("nohost");
        let root = marked_root(&scratch);
        let err = check(&vars_for(&root), &Fake(true), "mac-b").unwrap_err();
        let missing = root.join("hosts").join("mac-b");
        assert!(err.contains(&missing.display().to_string()), "{err}");
        assert!(err.contains("dev-host init-host"), "{err}");
        assert!(!missing.exists());
        // The root alone still passes: that is what init-host needs.
        assert_eq!(check_root(&vars_for(&root), &Fake(true)), Ok(root));
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_symlinked_host_folder() {
        let scratch = Scratch::new("hostlink");
        let root = marked_root(&scratch);
        let outside = scratch.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("hosts/mac-b")).unwrap();
        let err = check(&vars_for(&root), &Fake(true), "mac-b").unwrap_err();
        assert!(err.contains("is a symlink"), "{err}");
        let host = Host::new(root.clone(), "mac-b").unwrap();
        assert!(init_host(&host).unwrap_err().contains("is a symlink"));
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn fails_naming_every_unmounted_required_mount() {
        let scratch = Scratch::new("mounts");
        let root = marked_root(&scratch);
        let up = scratch.path().join("mounted-models");
        let down = scratch.path().join("models");
        let gone = scratch.path().join("work");
        std::fs::write(
            root.join(CONFIG_FILE),
            format!(
                "required_mount = {}\nrequired_mount = {}\nrequired_mount = {}\n",
                up.display(),
                down.display(),
                gone.display()
            ),
        )
        .unwrap();
        let err = check(&vars_for(&root), &Fake(true), "mac-b").unwrap_err();
        for named in [root.join("hosts/mac-b"), down.clone(), gone.clone()] {
            assert!(err.contains(&named.display().to_string()), "{err}");
        }
        assert!(!err.contains(&up.display().to_string()), "{err}");
        assert!(check_root(&vars_for(&root), &Fake(true)).is_err());
        std::fs::write(
            root.join(CONFIG_FILE),
            format!("required_mount = {}\n", up.display()),
        )
        .unwrap();
        assert!(check(&vars_for(&root), &Fake(true), HOST).is_ok());
    }

    #[test]
    fn init_host_creates_only_the_host_folder_and_its_locks() {
        let scratch = Scratch::new("init");
        let root = marked_root(&scratch);
        let host = Host::new(root.clone(), "mac-b").unwrap();
        assert_eq!(init_host(&host), Ok(root.join("hosts/mac-b")));
        assert!(root.join("hosts/mac-b/locks").is_dir());
        let made: Vec<_> = std::fs::read_dir(root.join("hosts/mac-b"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(made, vec![std::ffi::OsString::from("locks")]);
        // Idempotent, and the full check now passes for that host.
        assert!(init_host(&host).is_ok());
        assert!(check(&vars_for(&root), &Fake(true), "mac-b").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn real_mount_probe_rejects_folders_that_are_not_mount_points() {
        let scratch = Scratch::new("realmount");
        let plain = scratch.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        // A folder inside the scratch folder shares its parent's device whatever volume holds
        // it, so it is never a mount point.
        assert!(SystemVolumeProbe.verify_mount(&plain).is_err());
        assert!(
            SystemVolumeProbe
                .verify_mount(&scratch.path().join("absent"))
                .is_err()
        );
        assert!(SystemVolumeProbe.verify_mount(Path::new("/")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn real_mount_probe_accepts_a_fixed_drive_and_refuses_a_missing_folder() {
        let scratch = Scratch::new("realmount");
        assert_eq!(SystemVolumeProbe.verify_mount(scratch.path()), Ok(()));
        assert_eq!(
            SystemVolumeProbe.verify_mount(&std::env::temp_dir()),
            Ok(())
        );
        assert!(
            SystemVolumeProbe
                .verify_mount(&scratch.path().join("absent"))
                .is_err()
        );
    }

    #[test]
    fn fails_when_unset() {
        let err = check(&Vars::default(), &Fake(true), HOST).unwrap_err();
        assert!(err.contains("DEV_HOST_ROOT is not set"), "{err}");
    }

    #[test]
    fn fails_on_system_volume_naming_the_mount() {
        let scratch = Scratch::new("sysvol");
        let root = marked_root(&scratch);
        let err = check(&vars_for(&root), &Fake(false), HOST).unwrap_err();
        assert!(err.contains("simulated system volume"), "{err}");
        // The wording depends on where the scratch folder lives (TMPDIR under /Volumes, a
        // tmpfs, a Windows drive), so compare with the hint for this root.
        assert!(err.ends_with(&wrong_volume_hint(&root)), "{err}");
        if cfg!(windows) {
            // The scratch drive exists, so the message must not claim it is unmounted.
            assert!(!err.contains("not mounted"), "{err}");
        }
    }

    #[test]
    fn fails_without_marker_and_never_creates_it() {
        let scratch = Scratch::new("nomarker");
        let root = scratch.path().join("dev-host");
        std::fs::create_dir_all(&root).unwrap();
        let err = check(&vars_for(&root), &Fake(true), HOST).unwrap_err();
        assert!(err.contains(MARKER), "{err}");
        assert!(!root.join(MARKER).exists());
    }

    #[test]
    fn fails_on_missing_root_without_creating_it() {
        let scratch = Scratch::new("missing");
        let root = scratch.path().join("absent");
        let err = check(&vars_for(&root), &Fake(true), HOST).unwrap_err();
        assert!(err.ends_with(&unavailable_hint(&root)), "{err}");
        if cfg!(windows) {
            assert!(err.ends_with("the folder itself is missing"), "{err}");
        }
        assert!(!root.exists());
    }

    #[test]
    fn hung_probe_times_out() {
        let scratch = Scratch::new("hang");
        let root = marked_root(&scratch);
        let mount = scratch.path().join("models");
        std::fs::write(
            root.join(CONFIG_FILE),
            format!("required_mount = {}\n", mount.display()),
        )
        .unwrap();
        for (hang_on_mount, named) in [(false, &root), (true, &mount)] {
            let started = std::time::Instant::now();
            let err = check_within(&vars_for(&root), Duration::from_millis(100), move |v, p| {
                host_with(v, &Hang(hang_on_mount), HOST, p)
            })
            .unwrap_err();
            assert!(started.elapsed() < Duration::from_secs(2));
            assert!(err.contains("did not respond"), "{err}");
            assert!(
                err.starts_with(&named.display().to_string()),
                "names what hung: {err}"
            );
        }
    }

    #[test]
    fn missing_mount_names_the_volume() {
        assert_eq!(
            missing_mount(Path::new("/Volumes/Models/dev-host")),
            "volume /Volumes/Models is not mounted"
        );
        assert_eq!(
            missing_mount(Path::new("/srv/dev-host")),
            "no dev volume is mounted at /srv/dev-host or above it"
        );
    }

    #[test]
    fn volume_mounted_reads_the_nearest_existing_folder() {
        let scratch = Scratch::new("mounted");
        let missing = scratch.path().join("absent/deeper");
        // Whatever holds the scratch folder holds its missing children too.
        assert_eq!(volume_mounted(&missing), volume_mounted(scratch.path()));
        #[cfg(unix)]
        assert!(!volume_mounted(Path::new("/dev-host-no-such-folder/x")));
        #[cfg(windows)]
        {
            let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
            let absent = Path::new(&format!("{system}\\")).join("dev-host-no-such-folder");
            assert!(volume_mounted(&absent));
            assert!(volume_mounted(&PathBuf::from(format!(r"\\?\{system}\x"))));
        }
    }

    #[cfg(unix)]
    #[test]
    fn real_probe_rejects_the_system_root() {
        assert!(SystemVolumeProbe.verify(Path::new("/")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn real_probe_rejects_the_system_drive() {
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
        let err = SystemVolumeProbe
            .verify(&Path::new(&format!("{system}\\")).join("dev-host"))
            .unwrap_err();
        assert!(err.contains("system drive"), "{err}");
    }

    #[cfg(windows)]
    #[test]
    fn real_probe_follows_a_junction_to_the_system_drive() {
        // A junction on any drive pointing at the system drive must be rejected, even when the
        // junction itself sits on another fixed drive.
        let scratch = Scratch::new("junction");
        let link = scratch.path().join("link");
        let target =
            PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned()));
        let made = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        let err = SystemVolumeProbe.verify(&link).unwrap_err();
        // Remove the junction itself before the scratch folder is dropped.
        std::fs::remove_dir(&link).unwrap();
        assert!(err.contains("system drive"), "{err}");
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
        if drive_letter(scratch.path()).map(|d| format!("{d}:")) != Some(system.to_uppercase()) {
            // The junction sits on another drive, so only its target can be the reason.
            assert!(err.contains("resolved to"), "{err}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn drive_letter_reads_plain_and_verbatim_paths() {
        assert_eq!(drive_letter(Path::new(r"e:\dev-host")), Some('E'));
        assert_eq!(drive_letter(Path::new(r"\\?\E:\dev-host")), Some('E'));
        assert_eq!(drive_letter(Path::new(r"\\server\share\x")), None);
    }

    #[cfg(windows)]
    #[test]
    fn missing_mount_names_the_drive_without_the_verbatim_prefix() {
        assert_eq!(
            missing_mount(Path::new(r"\\?\e:\dev-host")),
            "volume E: is not mounted"
        );
        assert_eq!(
            missing_mount(Path::new(r"E:\dev-host")),
            "volume E: is not mounted"
        );
    }
}
