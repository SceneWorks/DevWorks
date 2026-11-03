//! OS path adapter for configured directories: reports whether a configured directory is
//! currently usable without changing or creating anything (SET-08 health reporting).
use devworks_core::settings::service::{PathHealth, PathProbe};
use std::path::PathBuf;

/// Inspects directories on the local filesystem. `~` and `~/...` resolve against the user's
/// home directory (`HOME`, or `USERPROFILE` on Windows).
#[derive(Clone, Copy, Debug, Default)]
pub struct FsPathProbe;

fn expand(path: &str) -> Option<PathBuf> {
    let rest = match path {
        "~" => Some(""),
        _ => path.strip_prefix("~/"),
    };
    match rest {
        None => Some(PathBuf::from(path)),
        Some(rest) => {
            let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
            Some(PathBuf::from(home).join(rest))
        }
    }
}

impl PathProbe for FsPathProbe {
    fn probe(&self, path: &str) -> PathHealth {
        let Some(path) = expand(path) else {
            return PathHealth::Unknown("home directory is not set".into());
        };
        match std::fs::metadata(&path) {
            Ok(m) if !m.is_dir() => PathHealth::NotADirectory,
            Ok(m) if m.permissions().readonly() => PathHealth::NotWritable,
            Ok(_) => PathHealth::Available,
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
}
