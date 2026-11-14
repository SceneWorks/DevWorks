//! `dev-host check`: the root is set, sits on its own volume, and carries the marker.
//!
//! Nothing here creates the root or any folder above it. A failure names the volume that
//! should be mounted, so a missing drive reads as a missing drive rather than a build error.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

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

/// Verifies the root named by `DEV_HOST_ROOT` and returns it.
pub fn check(vars: &Vars, probe: &dyn VolumeProbe) -> Result<PathBuf, String> {
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

/// Runs [`check`] on a worker thread and gives up after `timeout`, so a hung mount fails the
/// check instead of hanging the caller.
pub fn check_within(
    vars: &Vars,
    probe: Arc<dyn VolumeProbe>,
    timeout: Duration,
) -> Result<PathBuf, String> {
    let (tx, rx) = mpsc::channel();
    let owned = vars.clone();
    std::thread::spawn(move || {
        let _ = tx.send(check(&owned, probe.as_ref()));
    });
    rx.recv_timeout(timeout).unwrap_or_else(|_| {
        let root = vars.get(ROOT_VAR).unwrap_or_default();
        Err(format!(
            "{ROOT_VAR}={root} did not respond within {}s: its volume is hung or {}",
            timeout.as_secs(),
            missing_mount(Path::new(root))
        ))
    })
}

/// Runs the real check with the standard timeout.
pub fn check_host(vars: &Vars) -> Result<PathBuf, String> {
    check_within(vars, Arc::new(SystemVolumeProbe), CHECK_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    struct Fake(bool);
    impl VolumeProbe for Fake {
        fn verify(&self, _root: &Path) -> Result<(), String> {
            if self.0 {
                Ok(())
            } else {
                Err("simulated system volume".into())
            }
        }
    }

    struct Hang;
    impl VolumeProbe for Hang {
        fn verify(&self, _root: &Path) -> Result<(), String> {
            std::thread::sleep(Duration::from_secs(5));
            Ok(())
        }
    }

    fn vars_for(root: &Path) -> Vars {
        [(ROOT_VAR, root.display().to_string())]
            .into_iter()
            .collect()
    }

    fn marked_root(scratch: &Scratch) -> PathBuf {
        let root = scratch.path().join("dev-host");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(MARKER), "dev-host-root v1\n").unwrap();
        root
    }

    #[test]
    fn passes_on_marked_root_on_another_volume() {
        let scratch = Scratch::new("pass");
        let root = marked_root(&scratch);
        assert_eq!(check(&vars_for(&root), &Fake(true)), Ok(root));
    }

    #[test]
    fn fails_when_unset() {
        let err = check(&Vars::default(), &Fake(true)).unwrap_err();
        assert!(err.contains("DEV_HOST_ROOT is not set"), "{err}");
    }

    #[test]
    fn fails_on_system_volume_naming_the_mount() {
        let scratch = Scratch::new("sysvol");
        let root = marked_root(&scratch);
        let err = check(&vars_for(&root), &Fake(false)).unwrap_err();
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
        let err = check(&vars_for(&root), &Fake(true)).unwrap_err();
        assert!(err.contains(MARKER), "{err}");
        assert!(!root.join(MARKER).exists());
    }

    #[test]
    fn fails_on_missing_root_without_creating_it() {
        let scratch = Scratch::new("missing");
        let root = scratch.path().join("absent");
        let err = check(&vars_for(&root), &Fake(true)).unwrap_err();
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
        let started = std::time::Instant::now();
        let err =
            check_within(&vars_for(&root), Arc::new(Hang), Duration::from_millis(100)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(err.contains("did not respond"), "{err}");
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
        let target = PathBuf::from(std::env::var("SystemRoot").unwrap_or(r"C:\Windows".into()));
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
