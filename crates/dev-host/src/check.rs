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
/// - Windows: the root's drive must be a mounted fixed volume other than the system drive.
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

        let Some(drive) = drive_letter(root) else {
            return Err(format!(
                "{} is not on a drive letter (expected a fixed volume such as E:)",
                root.display()
            ));
        };
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
        if system.eq_ignore_ascii_case(&format!("{drive}:")) {
            return Err(format!(
                "{} is on the system drive {system}",
                root.display()
            ));
        }
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
#[cfg_attr(not(windows), allow(dead_code))]
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
/// drive on Windows.
pub fn volume_of(root: &Path) -> Option<String> {
    let mut parts = root.components();
    if let Some(Component::Prefix(prefix)) = parts.clone().next() {
        return Some(prefix.as_os_str().to_string_lossy().into_owned());
    }
    match (parts.next(), parts.next(), parts.next()) {
        (
            Some(Component::RootDir),
            Some(Component::Normal(first)),
            Some(Component::Normal(name)),
        ) if first == "Volumes" => Some(Path::new("/Volumes").join(name).display().to_string()),
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
    let mount = missing_mount(&root);
    match std::fs::metadata(&root) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(format!("{ROOT_VAR}={raw} is not a folder")),
        Err(e) => {
            return Err(format!("{ROOT_VAR}={raw} is unavailable ({e}): {mount}"));
        }
    }
    probe
        .verify(&root)
        .map_err(|reason| format!("{ROOT_VAR}={raw}: {reason}: {mount}"))?;
    if !root.join(MARKER).is_file() {
        return Err(format!(
            "{ROOT_VAR}={raw} has no {MARKER} marker; it is not a set-up dev-host root \
             (either it was never set up or {mount})"
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
        // The wording depends on where the scratch folder lives (e.g. TMPDIR under /Volumes),
        // so compare with what `missing_mount` says for this root.
        assert!(err.contains(&missing_mount(&root)), "{err}");
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
        assert!(err.contains(&missing_mount(&root)), "{err}");
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
    fn drive_letter_reads_plain_and_verbatim_paths() {
        assert_eq!(drive_letter(Path::new(r"e:\dev-host")), Some('E'));
        assert_eq!(drive_letter(Path::new(r"\\?\E:\dev-host")), Some('E'));
        assert_eq!(drive_letter(Path::new(r"\\server\share\x")), None);
    }
}
