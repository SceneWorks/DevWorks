//! OS path adapter for configured directories: reports whether a configured directory is
//! currently usable (SET-08 health reporting). The result is advisory: it never changes the
//! configured value, and the only thing it writes is a uniquely named probe file it removes
//! again at once.
use devworks_core::settings::service::{PathHealth, PathProbe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Inspects directories on the local filesystem. `~`, `~/...` and (Windows) `~\...` resolve
/// against the user's home directory (`HOME`, or `USERPROFILE` on Windows).
#[derive(Clone, Copy, Debug, Default)]
pub struct FsPathProbe;

fn expand(path: &str) -> Option<PathBuf> {
    let rest = match path {
        "~" => Some(""),
        _ => path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")),
    };
    match rest {
        None => Some(PathBuf::from(path)),
        Some(rest) => {
            let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
            Some(PathBuf::from(home).join(rest))
        }
    }
}

/// Whether a file can be created in `dir`: creates a uniquely named probe file exclusively and
/// removes it. Permission bits alone do not answer this (ACLs, read-only mounts, sandboxes and
/// the effective user all matter), so the operating system is asked directly.
fn writable(dir: &Path) -> std::io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = format!(
        ".devworks-write-probe-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let probe = dir.join(name);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    std::fs::remove_file(&probe)
}

impl PathProbe for FsPathProbe {
    fn probe(&self, path: &str) -> PathHealth {
        let Some(path) = expand(path) else {
            return PathHealth::Unknown("home directory is not set".into());
        };
        match std::fs::metadata(&path) {
            Ok(m) if !m.is_dir() => PathHealth::NotADirectory,
            Ok(_) => match writable(&path) {
                Ok(()) => PathHealth::Available,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::PermissionDenied
                            | std::io::ErrorKind::ReadOnlyFilesystem
                    ) =>
                {
                    PathHealth::NotWritable
                }
                Err(e) => PathHealth::Unknown(e.kind().to_string()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => PathHealth::Missing,
            Err(e) => PathHealth::Unknown(e.kind().to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_missing_files_and_available_directories() {
        let dir = std::env::temp_dir();
        assert_eq!(
            FsPathProbe.probe(dir.to_str().unwrap()),
            PathHealth::Available
        );
        let missing = dir.join("devworks-no-such-directory-for-probe");
        assert_eq!(
            FsPathProbe.probe(missing.to_str().unwrap()),
            PathHealth::Missing
        );
        let file = dir.join(format!("devworks-probe-file-{}", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(
            FsPathProbe.probe(file.to_str().unwrap()),
            PathHealth::NotADirectory
        );
        let _ = std::fs::remove_file(file);
    }

    /// A directory the process cannot create files in is reported as not writable, and the
    /// probe leaves nothing behind in a writable one.
    #[cfg(unix)]
    #[test]
    fn writability_is_probed_not_inferred_from_permission_bits() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("devworks-probe-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(
            FsPathProbe.probe(dir.to_str().unwrap()),
            PathHealth::Available
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "probe file left"
        );
        // The owner may not write but the group may: `Permissions::readonly()` is false (some
        // write bit is set), yet the owner cannot create a file here.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o575)).unwrap();
        assert!(!std::fs::metadata(&dir).unwrap().permissions().readonly());
        let denied = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join("x"))
            .is_err();
        let health = FsPathProbe.probe(dir.to_str().unwrap());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        // A privileged user may create files regardless of the mode; the probe then agrees
        // with the operating system rather than with the mode bits.
        if denied {
            assert_eq!(health, PathHealth::NotWritable);
        } else {
            assert_eq!(health, PathHealth::Available);
        }
    }

    #[test]
    fn home_relative_paths_expand_with_either_separator() {
        let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
        else {
            return;
        };
        let home = PathBuf::from(home);
        assert_eq!(expand("~"), Some(home.clone()));
        assert_eq!(expand("~/work"), Some(home.join("work")));
        assert_eq!(expand("~\\work"), Some(home.join("work")));
        assert_eq!(expand("/abs"), Some(PathBuf::from("/abs")));
    }
}
