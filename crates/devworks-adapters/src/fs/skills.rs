//! The harness skill directory (`<data-root>/skills`): approved harness skill revisions are
//! written as a complete tree beside the live one and swapped in with renames, so a reader sees
//! the old or the new tree. Nothing outside this root is ever written; shared skill folders are
//! only read, through the source-tree adapter.
use devworks_core::ports::skill_files::{SkillFile, SkillFileFault, SkillFiles};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct NativeSkillFiles {
    root: PathBuf,
    next: AtomicU64,
}
impl NativeSkillFiles {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            next: AtomicU64::new(0),
        }
    }
}

fn io_fault(e: io::Error) -> SkillFileFault {
    SkillFileFault::Io(e.to_string())
}
fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}
fn valid_segment(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', ':', '\0'])
}

impl SkillFiles for NativeSkillFiles {
    fn root(&self) -> Result<PathBuf, SkillFileFault> {
        fs::create_dir_all(&self.root).map_err(io_fault)?;
        fs::canonicalize(&self.root).map_err(io_fault)
    }
    fn materialize(&self, name: &str, files: &[SkillFile]) -> Result<(), SkillFileFault> {
        if !valid_segment(name) || name.starts_with('.') {
            return Err(SkillFileFault::InvalidName);
        }
        let root = self.root()?;
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        let tag = format!("{}-{}", std::process::id(), n);
        let staging = root.join(format!(".{name}.new-{tag}"));
        let retired = root.join(format!(".{name}.old-{tag}"));
        let live = root.join(name);
        let written = (|| -> Result<(), SkillFileFault> {
            fs::create_dir(&staging).map_err(io_fault)?;
            for file in files {
                let parts: Vec<&str> = file.path.split('/').collect();
                if parts.iter().any(|p| !valid_segment(p)) {
                    return Err(SkillFileFault::InvalidPath(file.path.clone()));
                }
                let path = parts.iter().fold(staging.clone(), |p, s| p.join(s));
                let parent = path.parent().expect("inside staging").to_path_buf();
                fs::create_dir_all(&parent).map_err(io_fault)?;
                let mut out = File::create_new(&path).map_err(io_fault)?;
                out.write_all(&file.bytes).map_err(io_fault)?;
                out.sync_all().map_err(io_fault)?;
                #[cfg(unix)]
                if file.executable {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                        .map_err(io_fault)?;
                }
                sync_dir(&parent).map_err(io_fault)?;
            }
            sync_dir(&staging).map_err(io_fault)
        })();
        if let Err(e) = written {
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }
        // Links in the managed root are never followed: a link at the live name is replaced.
        let had_live = fs::symlink_metadata(&live).is_ok();
        if had_live {
            fs::rename(&live, &retired).map_err(|e| {
                let _ = fs::remove_dir_all(&staging);
                io_fault(e)
            })?;
        }
        if let Err(e) = fs::rename(&staging, &live) {
            if had_live {
                let _ = fs::rename(&retired, &live);
            }
            let _ = fs::remove_dir_all(&staging);
            return Err(io_fault(e));
        }
        sync_dir(&root).map_err(io_fault)?;
        if had_live {
            let meta = fs::symlink_metadata(&retired).map_err(io_fault)?;
            if meta.file_type().is_dir() {
                let _ = fs::remove_dir_all(&retired);
            } else {
                let _ = fs::remove_file(&retired);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "devworks-skills-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn materialize_replaces_the_whole_tree_and_refuses_escapes() {
        let root = temp();
        let files = NativeSkillFiles::new(&root);
        files
            .materialize(
                "demo",
                &[
                    SkillFile {
                        path: "SKILL.md".into(),
                        bytes: b"one".to_vec(),
                        executable: false,
                    },
                    SkillFile {
                        path: "scripts/run.sh".into(),
                        bytes: b"#!/bin/sh\n".to_vec(),
                        executable: true,
                    },
                ],
            )
            .unwrap();
        assert_eq!(fs::read(root.join("demo/SKILL.md")).unwrap(), b"one");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(root.join("demo/scripts/run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
        }
        files
            .materialize(
                "demo",
                &[SkillFile {
                    path: "SKILL.md".into(),
                    bytes: b"two".to_vec(),
                    executable: false,
                }],
            )
            .unwrap();
        assert_eq!(fs::read(root.join("demo/SKILL.md")).unwrap(), b"two");
        assert!(!root.join("demo/scripts").exists(), "the old tree is gone");
        assert!(matches!(
            files.materialize(
                "demo",
                &[SkillFile {
                    path: "../escape".into(),
                    bytes: vec![],
                    executable: false
                }]
            ),
            Err(SkillFileFault::InvalidPath(_))
        ));
        assert_eq!(fs::read(root.join("demo/SKILL.md")).unwrap(), b"two");
        assert!(matches!(
            files.materialize("../x", &[]),
            Err(SkillFileFault::InvalidName)
        ));
        let leftovers: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, ["demo"]);
        fs::remove_dir_all(root).unwrap();
    }
}
