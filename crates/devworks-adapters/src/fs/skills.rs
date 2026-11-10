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

/// Copies every entry under `from` that skill capture does not record (a name starting with
/// `.`, a link, or anything but a regular file/directory) to the same place under `to`, unless
/// the new tree already has that path.
fn preserve_uncaptured(from: &Path, to: &Path) -> io::Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let source = entry.path();
        let target = to.join(&name);
        let kind = fs::symlink_metadata(&source)?.file_type();
        let hidden = name.to_string_lossy().starts_with('.');
        if kind.is_dir() && !hidden {
            preserve_uncaptured_into(&source, &target)?;
        } else if (hidden || !(kind.is_file() || kind.is_dir()))
            && fs::symlink_metadata(&target).is_err()
        {
            copy_entry(&source, &target)?;
        }
    }
    Ok(())
}
fn preserve_uncaptured_into(from: &Path, to: &Path) -> io::Result<()> {
    // Only create a visible directory in the new tree when it has something to keep.
    let mut kept = false;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = fs::symlink_metadata(entry.path())?.file_type();
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        if hidden || !kind.is_file() {
            kept = true;
            break;
        }
    }
    if !kept {
        return Ok(());
    }
    let created = !to.exists();
    if created {
        fs::create_dir_all(to)?;
    }
    preserve_uncaptured(from, to)?;
    if created && fs::read_dir(to)?.next().is_none() {
        fs::remove_dir(to)?;
    }
    Ok(())
}
fn copy_entry(from: &Path, to: &Path) -> io::Result<()> {
    let kind = fs::symlink_metadata(from)?.file_type();
    if kind.is_symlink() {
        let target = fs::read_link(from)?;
        #[cfg(unix)]
        return std::os::unix::fs::symlink(target, to);
        #[cfg(windows)]
        return if fs::metadata(from).is_ok_and(|m| m.is_dir()) {
            std::os::windows::fs::symlink_dir(target, to)
        } else {
            std::os::windows::fs::symlink_file(target, to)
        };
    }
    if kind.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_entry(&entry.path(), &to.join(entry.file_name()))?;
        }
        return Ok(());
    }
    if kind.is_file() {
        fs::copy(from, to)?;
        File::open(to)?.sync_all()?;
    }
    // Sockets, FIFOs and devices cannot be copied; they are left out rather than faked.
    Ok(())
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
            // Entries skill capture never records (hidden files and folders, links, special
            // files) are user-owned and carried into the new tree unchanged, never deleted.
            if fs::symlink_metadata(&live).is_ok_and(|m| m.file_type().is_dir()) {
                preserve_uncaptured(&live, &staging).map_err(io_fault)?;
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
        assert!(
            !root.join("demo/scripts/run.sh").exists(),
            "captured files follow the revision"
        );
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
        // Uncaptured user files survive a whole-tree replacement.
        fs::write(root.join("demo/.env"), b"secret").unwrap();
        fs::create_dir_all(root.join("demo/docs/.cache")).unwrap();
        fs::write(root.join("demo/docs/.cache/x"), b"c").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("SKILL.md", root.join("demo/link.md")).unwrap();
        files
            .materialize(
                "demo",
                &[SkillFile {
                    path: "SKILL.md".into(),
                    bytes: b"three".to_vec(),
                    executable: false,
                }],
            )
            .unwrap();
        assert_eq!(fs::read(root.join("demo/SKILL.md")).unwrap(), b"three");
        assert_eq!(fs::read(root.join("demo/.env")).unwrap(), b"secret");
        assert_eq!(fs::read(root.join("demo/docs/.cache/x")).unwrap(), b"c");
        #[cfg(unix)]
        assert_eq!(
            fs::read_link(root.join("demo/link.md")).unwrap(),
            Path::new("SKILL.md")
        );
        let leftovers: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, ["demo"]);
        fs::remove_dir_all(root).unwrap();
    }
}
