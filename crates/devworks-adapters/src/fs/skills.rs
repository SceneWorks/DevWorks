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
/// Renames, retrying briefly on Windows, where another process (an indexer or scanner) can
/// hold a just-written file open and make a directory rename fail with a sharing violation.
fn rename(from: &Path, to: &Path) -> io::Result<()> {
    super::retry_denied(|| fs::rename(from, to))
}
/// Removes a tree this adapter wrote (staging or a retired tree). Windows refuses to delete a
/// read-only file, and carried-over user files may be read-only, so their attribute is cleared
/// there first; links are never followed.
fn remove_tree(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    clear_readonly(path)?;
    fs::remove_dir_all(path)
}
#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)] // Windows only: clears the read-only attribute
fn clear_readonly(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            clear_readonly(&path)?;
        } else if meta.permissions().readonly() {
            let mut p = meta.permissions();
            p.set_readonly(false);
            fs::set_permissions(&path, p)?;
        }
    }
    Ok(())
}
fn valid_segment(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', ':', '\0'])
}

/// Copies every entry under `from` that skill capture does not record (a name starting with
/// `.` or not valid UTF-8, a link, or anything but a regular file/directory) to the same place under `to`, unless
/// the new tree already has that path.
fn preserve_uncaptured(from: &Path, to: &Path) -> io::Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let source = entry.path();
        let target = to.join(&name);
        let kind = fs::symlink_metadata(&source)?.file_type();
        // Names capture cannot represent (not UTF-8) are never listed, so never captured: they
        // are kept like hidden entries rather than pruned with the old tree.
        let hidden = name.to_str().is_none_or(|n| n.starts_with('.'));
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
        let hidden = entry
            .file_name()
            .to_str()
            .is_none_or(|n| n.starts_with('.'));
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
/// Relative paths of every entry under `dir` that [`preserve_uncaptured`] carries over.
fn uncaptured_paths(dir: &Path, prefix: &str, out: &mut Vec<String>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let kind = fs::symlink_metadata(entry.path())?.file_type();
        let shown = name.to_string_lossy();
        let relative = if prefix.is_empty() {
            shown.into_owned()
        } else {
            format!("{prefix}/{shown}")
        };
        let hidden = name.to_str().is_none_or(|n| n.starts_with('.'));
        if kind.is_dir() && !hidden {
            uncaptured_paths(&entry.path(), &relative, out)?;
        } else if hidden || !(kind.is_file() || kind.is_dir()) {
            out.push(relative);
        }
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
        // Copied through the handle that created it, which can flush on every platform (Windows
        // denies flushing a read-only handle), then given the source's permissions, which may
        // make it read-only.
        let mut out = File::create_new(to)?;
        io::copy(&mut File::open(from)?, &mut out)?;
        out.sync_all()?;
        drop(out);
        fs::set_permissions(to, fs::metadata(from)?.permissions())?;
    }
    // Sockets, FIFOs and devices cannot be copied; they are left out rather than faked.
    Ok(())
}

/// Prefix of a swap journal: holds the skill name of an in-progress swap whose staging and
/// retired trees carry the same tag.
const JOURNAL: &str = ".skill.swap-";

/// Removes a directory tree, a file or a link.
fn remove_entry(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => remove_tree(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Completes or undoes every swap another process left behind. A live tree missing beside its
/// retired tree is moved back, so its uncaptured files (which no revision records) are kept;
/// otherwise the swap finished, and the retired and staging trees are leftovers. A journal of
/// this process belongs to a swap still in progress here and is left alone.
fn recover_swaps(root: &Path) -> io::Result<()> {
    let own = format!("{}-", std::process::id());
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let file = entry.file_name();
        let Some(tag) = file.to_str().and_then(|f| f.strip_prefix(JOURNAL)) else {
            continue;
        };
        if tag.starts_with(&own) {
            continue;
        }
        let retired = root.join(format!(".skill.old-{tag}"));
        let staging = root.join(format!(".skill.new-{tag}"));
        // A journal is complete before its live tree moves, so a partial one has no retired
        // tree and only its staging tree is cleaned up.
        let name = fs::read_to_string(entry.path()).unwrap_or_default();
        let live = (valid_segment(&name) && !name.starts_with('.')).then(|| root.join(&name));
        match live {
            Some(live)
                if fs::symlink_metadata(&live).is_err()
                    && fs::symlink_metadata(&retired).is_ok() =>
            {
                rename(&retired, &live)?
            }
            _ => remove_entry(&retired)?,
        }
        remove_entry(&staging)?;
        sync_dir(root)?;
        fs::remove_file(entry.path())?;
    }
    // A process that stopped while writing its staging tree had not journaled it yet: the live
    // tree never moved, so the staging tree (and any retired tree without a journal) is a
    // leftover. Left in place, it would count against the root's entry bound.
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let file = entry.file_name();
        let Some(tag) = file.to_str().and_then(|f| {
            f.strip_prefix(".skill.new-")
                .or_else(|| f.strip_prefix(".skill.old-"))
        }) else {
            continue;
        };
        if tag.starts_with(&own) || root.join(format!("{JOURNAL}{tag}")).exists() {
            continue;
        }
        remove_entry(&entry.path())?;
    }
    sync_dir(root)
}

impl SkillFiles for NativeSkillFiles {
    fn uncaptured_collision(
        &self,
        name: &str,
        paths: &[String],
    ) -> Result<Option<String>, SkillFileFault> {
        if !valid_segment(name) || name.starts_with('.') {
            return Err(SkillFileFault::InvalidName);
        }
        let live = self.root()?.join(name);
        if !fs::symlink_metadata(&live).is_ok_and(|m| m.file_type().is_dir()) {
            return Ok(None);
        }
        let mut kept = Vec::new();
        uncaptured_paths(&live, "", &mut kept).map_err(io_fault)?;
        // Compared case-insensitively: on a case-insensitive filesystem `Docs` and `docs` are
        // the same entry, and a false refusal elsewhere is safe.
        let within = |inner: &str, outer: &str| {
            inner.len() > outer.len()
                && inner.as_bytes()[outer.len()] == b'/'
                && inner[..outer.len()].eq_ignore_ascii_case(outer)
        };
        Ok(kept.into_iter().find(|u| {
            paths
                .iter()
                .any(|f| f.eq_ignore_ascii_case(u) || within(u, f) || within(f, u))
        }))
    }
    fn root(&self) -> Result<PathBuf, SkillFileFault> {
        fs::create_dir_all(&self.root).map_err(io_fault)?;
        let root = fs::canonicalize(&self.root).map_err(io_fault)?;
        recover_swaps(&root).map_err(io_fault)?;
        Ok(root)
    }
    fn materialize(&self, name: &str, files: &[SkillFile]) -> Result<(), SkillFileFault> {
        if !valid_segment(name) || name.starts_with('.') {
            return Err(SkillFileFault::InvalidName);
        }
        let root = self.root()?;
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        let tag = format!("{}-{}", std::process::id(), n);
        // Temporary names never include the skill name: a name near the filesystem's length
        // limit must still have a staging and a retired tree. Hidden, so scans skip them.
        let staging = root.join(format!(".skill.new-{tag}"));
        let retired = root.join(format!(".skill.old-{tag}"));
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
            let _ = remove_tree(&staging);
            return Err(e);
        }
        // Links in the managed root are never followed: a link at the live name is replaced.
        let had_live = fs::symlink_metadata(&live).is_ok();
        // The journal names the skill before its live tree moves aside, so a process that dies
        // between the two renames leaves a swap the next use of this root completes or undoes.
        let journal = root.join(format!("{JOURNAL}{tag}"));
        if had_live {
            let journaled = (|| -> io::Result<()> {
                let mut out = File::create_new(&journal)?;
                out.write_all(name.as_bytes())?;
                out.sync_all()?;
                sync_dir(&root)
            })();
            if let Err(e) = journaled {
                let _ = fs::remove_file(&journal);
                let _ = remove_tree(&staging);
                return Err(io_fault(e));
            }
            rename(&live, &retired).map_err(|e| {
                let _ = fs::remove_file(&journal);
                let _ = remove_tree(&staging);
                io_fault(e)
            })?;
        }
        if let Err(e) = rename(&staging, &live) {
            if had_live {
                let _ = rename(&retired, &live);
                let _ = fs::remove_file(&journal);
            }
            let _ = remove_tree(&staging);
            return Err(io_fault(e));
        }
        sync_dir(&root).map_err(io_fault)?;
        // The swap is done; a retired tree that cannot be removed now keeps its journal, so a
        // later process removes it.
        if had_live && remove_entry(&retired).is_ok() {
            let _ = fs::remove_file(&journal);
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

    fn skill(path: &str, bytes: &[u8]) -> SkillFile {
        SkillFile {
            path: path.into(),
            bytes: bytes.to_vec(),
            executable: false,
        }
    }

    #[test]
    fn a_swap_interrupted_between_its_renames_keeps_uncaptured_files() {
        let root = temp();
        let files = NativeSkillFiles::new(&root);
        files
            .materialize("alpha", &[skill("SKILL.md", b"one")])
            .unwrap();
        let base = files.root().unwrap();
        fs::write(base.join("alpha/.env"), b"secret").unwrap();
        // Another process journaled the swap, moved the live tree aside and died before
        // renaming its staging tree into place.
        fs::write(base.join(".skill.swap-0-7"), b"alpha").unwrap();
        fs::rename(base.join("alpha"), base.join(".skill.old-0-7")).unwrap();
        fs::create_dir(base.join(".skill.new-0-7")).unwrap();
        // A swap that finished but died before removing its retired tree.
        files
            .materialize("beta", &[skill("SKILL.md", b"b")])
            .unwrap();
        fs::create_dir(base.join(".skill.old-0-8")).unwrap();
        fs::write(base.join(".skill.swap-0-8"), b"beta").unwrap();
        files
            .materialize("alpha", &[skill("SKILL.md", b"two")])
            .unwrap();
        assert_eq!(fs::read(base.join("alpha/.env")).unwrap(), b"secret");
        assert_eq!(fs::read(base.join("alpha/SKILL.md")).unwrap(), b"two");
        assert_eq!(fs::read(base.join("beta/SKILL.md")).unwrap(), b"b");
        let mut left: Vec<String> = fs::read_dir(&base)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["alpha", "beta"]);
        // A process that died while writing its staging tree, before journaling it.
        fs::create_dir_all(base.join(".skill.new-0-9/scripts")).unwrap();
        fs::write(base.join(".skill.new-0-9/SKILL.md"), b"partial").unwrap();
        files.root().unwrap();
        let mut left: Vec<String> = fs::read_dir(&base)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["alpha", "beta"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_name_at_the_length_limit_still_materializes() {
        let root = temp();
        let files = NativeSkillFiles::new(&root);
        // 124 two-byte characters: 248 bytes, a valid directory name under Linux's 255-byte
        // limit that a name-derived temporary suffix would push past it.
        let name = "é".repeat(124);
        files
            .materialize(
                &name,
                &[SkillFile {
                    path: "SKILL.md".into(),
                    bytes: b"long".to_vec(),
                    executable: false,
                }],
            )
            .unwrap();
        assert_eq!(
            fs::read(root.join(&name).join("SKILL.md")).unwrap(),
            b"long"
        );
        let _ = remove_tree(&root);
    }

    #[test]
    fn uncaptured_collisions_are_found_before_a_tree_is_written() {
        let root = temp();
        let files = NativeSkillFiles::new(&root);
        let paths = |p: &[&str]| p.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            files
                .uncaptured_collision("demo", &paths(&["docs"]))
                .unwrap(),
            None
        );
        fs::create_dir_all(root.join("demo/docs")).unwrap();
        fs::write(root.join("demo/docs/ref.md"), "captured").unwrap();
        fs::write(root.join("demo/docs/.env"), "secret").unwrap();
        fs::write(root.join("demo/.notes"), "kept").unwrap();
        // A revision file where a live folder keeps an uncaptured entry cannot hold both.
        assert_eq!(
            files
                .uncaptured_collision("demo", &paths(&["SKILL.md", "docs"]))
                .unwrap()
                .as_deref(),
            Some("docs/.env")
        );
        // Captured files are replaced freely and hidden entries elsewhere are carried over.
        assert_eq!(
            files
                .uncaptured_collision("demo", &paths(&["SKILL.md", "docs/ref.md"]))
                .unwrap(),
            None
        );
        // The same path, or a revision folder over an uncaptured file, also collides.
        assert!(
            files
                .uncaptured_collision("demo", &paths(&[".notes"]))
                .unwrap()
                .is_some()
        );
        assert!(
            files
                .uncaptured_collision("demo", &paths(&[".notes/x.md"]))
                .unwrap()
                .is_some()
        );
        let _ = fs::remove_dir_all(&root);
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
        // A read-only uncaptured file is carried over too, still read-only.
        fs::write(root.join("demo/.readonly"), b"fixed").unwrap();
        let mut ro = fs::metadata(root.join("demo/.readonly"))
            .unwrap()
            .permissions();
        ro.set_readonly(true);
        fs::set_permissions(root.join("demo/.readonly"), ro).unwrap();
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
        assert_eq!(fs::read(root.join("demo/.readonly")).unwrap(), b"fixed");
        assert!(
            fs::metadata(root.join("demo/.readonly"))
                .unwrap()
                .permissions()
                .readonly()
        );
        assert_eq!(fs::read(root.join("demo/docs/.cache/x")).unwrap(), b"c");
        #[cfg(unix)]
        assert_eq!(
            fs::read_link(root.join("demo/link.md")).unwrap(),
            Path::new("SKILL.md")
        );
        // Linux allows non-UTF-8 names (APFS and NTFS do not): they are kept too.
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::ffi::OsStrExt;
            let odd = std::ffi::OsStr::from_bytes(b"notes-\xff");
            fs::write(root.join("demo").join(odd), b"odd").unwrap();
            files
                .materialize(
                    "demo",
                    &[SkillFile {
                        path: "SKILL.md".into(),
                        bytes: b"four".to_vec(),
                        executable: false,
                    }],
                )
                .unwrap();
            assert_eq!(fs::read(root.join("demo").join(odd)).unwrap(), b"odd");
        }
        let leftovers: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, ["demo"]);
        remove_tree(&root).unwrap();
    }
}
