//! Unix descriptor-relative no-follow reads and destructive steps.
use super::{NativeFilesystem, normal_components};
use devworks_core::authority::destructive::{
    CheckedTarget, DestructiveExecutor, DestructiveKind, EntryKind, FileIdentity, RemovalOutcome,
    ResolveError,
};
use devworks_core::authority::reads::{ReadRefusal, ReadRequest, VerifiedBytes, VerifiedReader};
use std::ffi::{CStr, CString, OsStr};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

fn cname(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn stat_at(dir: RawFd, name: &CStr) -> io::Result<libc::stat> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: live directory fd, NUL-terminated name, `st` is a valid out pointer.
    let rc = unsafe {
        libc::fstatat(
            dir,
            name.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstatat succeeded and initialized `st`.
    Ok(unsafe { st.assume_init() })
}

fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: live fd and a valid out pointer.
    if unsafe { libc::fstat(fd, st.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstat succeeded and initialized `st`.
    Ok(unsafe { st.assume_init() })
}

fn open_at(dir: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: live directory fd, NUL-terminated name; no caller-controlled pointers.
    let fd = unsafe { libc::openat(dir, name.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a fresh descriptor this OwnedFd now solely owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn unlink_at(dir: RawFd, name: &CStr, directory: bool) -> io::Result<()> {
    let flags = if directory { libc::AT_REMOVEDIR } else { 0 };
    // SAFETY: live directory fd and NUL-terminated name.
    if unsafe { libc::unlinkat(dir, name.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// `st_dev`/`st_ino` widths differ across Unix targets.
#[allow(clippy::unnecessary_cast)]
fn ident(st: &libc::stat) -> FileIdentity {
    FileIdentity {
        volume: st.st_dev as u64,
        file: st.st_ino as u64,
    }
}
fn is_link(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFLNK
}
fn is_dir(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
}
fn is_reg(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFREG
}

const DIR_NOFOLLOW: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;

/// Opens an absolute directory path component by component without following any link.
fn open_dir_nofollow(path: &Path) -> io::Result<OwnedFd> {
    let mut dir = open_at(libc::AT_FDCWD, c"/", libc::O_RDONLY | libc::O_DIRECTORY)?;
    for c in path.components() {
        match c {
            Component::RootDir => {}
            Component::Normal(n) => dir = open_at(dir.as_raw_fd(), &cname(n)?, DIR_NOFOLLOW)?,
            _ => return Err(io::Error::from(io::ErrorKind::InvalidInput)),
        }
    }
    Ok(dir)
}

fn refusal(e: &io::Error, dir: RawFd, name: &CStr) -> ReadRefusal {
    match e.raw_os_error() {
        Some(libc::ELOOP | libc::EMLINK) => ReadRefusal::LinkRefused,
        Some(libc::ENOTDIR) => match stat_at(dir, name) {
            Ok(st) if is_link(&st) => ReadRefusal::LinkRefused,
            _ => ReadRefusal::NotRegularFile,
        },
        Some(libc::ENOENT) => ReadRefusal::NotFound,
        _ => ReadRefusal::Io(e.kind().to_string()),
    }
}

impl VerifiedReader for NativeFilesystem {
    fn read(&self, req: &ReadRequest<'_>) -> Result<VerifiedBytes, ReadRefusal> {
        let parts = normal_components(req.relative).ok_or(ReadRefusal::Escape)?;
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(&req.root.location)
            .map_err(|e| match e.raw_os_error() {
                Some(libc::ENOENT) => ReadRefusal::NotFound,
                _ => ReadRefusal::RootChanged,
            })?;
        let root_stat =
            fstat(root.as_raw_fd()).map_err(|e| ReadRefusal::Io(e.kind().to_string()))?;
        if req.root.identity.is_some_and(|id| id != ident(&root_stat)) {
            return Err(ReadRefusal::RootChanged);
        }
        let mut dir: OwnedFd = root.into();
        let last = parts.len() - 1;
        for (i, part) in parts.iter().enumerate() {
            let name = cname(part).map_err(|_| ReadRefusal::Escape)?;
            let flags = libc::O_RDONLY
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | if i < last { libc::O_DIRECTORY } else { 0 };
            let fd = open_at(dir.as_raw_fd(), &name, flags)
                .map_err(|e| refusal(&e, dir.as_raw_fd(), &name))?;
            if i < last {
                dir = fd;
                continue;
            }
            let opened =
                fstat(fd.as_raw_fd()).map_err(|e| ReadRefusal::Io(e.kind().to_string()))?;
            if !is_reg(&opened) {
                return Err(ReadRefusal::NotRegularFile);
            }
            if opened.st_nlink > 1 && !req.allow_hard_links {
                return Err(ReadRefusal::HardLinked);
            }
            #[cfg(test)]
            self.hook();
            // The path must still name the object that was opened: a swap between open and
            // this check is refused rather than published as the path's content.
            match stat_at(dir.as_raw_fd(), &name) {
                Ok(now) if !is_link(&now) && ident(&now) == ident(&opened) => {}
                _ => return Err(ReadRefusal::Changed),
            }
            let mut bytes = Vec::new();
            File::from(fd)
                .take(req.max_bytes.saturating_add(1))
                .read_to_end(&mut bytes)
                .map_err(|e| ReadRefusal::Io(e.kind().to_string()))?;
            if bytes.len() as u64 > req.max_bytes {
                return Err(ReadRefusal::TooLarge {
                    limit: req.max_bytes,
                });
            }
            return Ok(VerifiedBytes {
                bytes,
                identity: Some(ident(&opened)),
            });
        }
        Err(ReadRefusal::Escape)
    }
}

fn changed(what: &str) -> ResolveError {
    ResolveError::Changed(format!("{what} changed since it was checked"))
}
fn io_err(e: io::Error) -> ResolveError {
    ResolveError::Io(e.kind().to_string())
}

/// Removes everything inside `dir` (descriptor-relative, never following links, never crossing
/// a mount point). Entries it cannot remove are reported, not hidden.
fn erase(dir: &OwnedFd, path: &Path, volume: libc::dev_t, out: &mut RemovalOutcome, depth: u32) {
    if depth > 512 {
        out.remaining.push(path.to_string_lossy().into_owned());
        return;
    }
    // SAFETY: duplicating a live descriptor; the duplicate is owned by the DIR stream below.
    let dup = unsafe { libc::dup(dir.as_raw_fd()) };
    if dup < 0 {
        out.remaining.push(path.to_string_lossy().into_owned());
        return;
    }
    // SAFETY: `dup` is a fresh directory descriptor; fdopendir takes ownership of it.
    let stream = unsafe { libc::fdopendir(dup) };
    if stream.is_null() {
        // SAFETY: fdopendir failed, so `dup` is still ours to close.
        unsafe { libc::close(dup) };
        out.remaining.push(path.to_string_lossy().into_owned());
        return;
    }
    let mut names = Vec::new();
    loop {
        // SAFETY: `stream` is a live DIR*; readdir returns null at the end.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        // SAFETY: d_name of a returned dirent is a NUL-terminated name.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_owned();
        if name.as_bytes() != b"." && name.as_bytes() != b".." {
            names.push(name);
        }
    }
    // SAFETY: closes the stream (and its descriptor) exactly once.
    unsafe { libc::closedir(stream) };
    for name in names {
        let child = path.join(OsStr::from_bytes(name.as_bytes()));
        let child_text = || child.to_string_lossy().into_owned();
        let Ok(st) = stat_at(dir.as_raw_fd(), &name) else {
            out.remaining.push(child_text());
            continue;
        };
        if is_dir(&st) {
            if st.st_dev != volume {
                // A mount point inside the tree is never crossed.
                out.remaining.push(child_text());
                continue;
            }
            let before = out.remaining.len();
            match open_at(dir.as_raw_fd(), &name, DIR_NOFOLLOW) {
                Ok(fd) if fstat(fd.as_raw_fd()).is_ok_and(|s| ident(&s) == ident(&st)) => {
                    erase(&fd, &child, volume, out, depth + 1)
                }
                _ => {
                    out.remaining.push(child_text());
                    continue;
                }
            }
            if out.remaining.len() > before {
                continue;
            }
            match unlink_at(dir.as_raw_fd(), &name, true) {
                Ok(()) => out.removed_entries += 1,
                Err(_) => out.remaining.push(child_text()),
            }
        } else {
            match unlink_at(dir.as_raw_fd(), &name, false) {
                Ok(()) => out.removed_entries += 1,
                Err(_) => out.remaining.push(child_text()),
            }
        }
    }
}

impl DestructiveExecutor for NativeFilesystem {
    fn execute(
        &self,
        target: &CheckedTarget,
        kind: DestructiveKind,
    ) -> Result<RemovalOutcome, ResolveError> {
        let path = target.path();
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(ResolveError::Unsupported(
                "a root is never a destructive target".into(),
            ));
        };
        let parent_fd = open_dir_nofollow(parent).map_err(|_| changed("the parent directory"))?;
        let cname = cname(name).map_err(io_err)?;
        let mut outcome = RemovalOutcome {
            removed_entries: 0,
            remaining: Vec::new(),
            complete: true,
        };
        let st = match stat_at(parent_fd.as_raw_fd(), &cname) {
            Ok(st) => st,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                return if target.entry_kind() == EntryKind::Missing {
                    Ok(outcome)
                } else {
                    Err(changed(target.requested()))
                };
            }
            Err(e) => return Err(io_err(e)),
        };
        let expected_kind = if is_link(&st) {
            EntryKind::Symlink
        } else if is_dir(&st) {
            EntryKind::Directory
        } else if is_reg(&st) {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        if target.entry_identity() != Some(ident(&st)) || target.entry_kind() != expected_kind {
            return Err(changed(target.requested()));
        }
        let parent = parent_fd.as_raw_fd();
        match kind {
            DestructiveKind::Unlink => {
                unlink_at(parent, &cname, is_dir(&st)).map_err(io_err)?;
                outcome.removed_entries = 1;
            }
            DestructiveKind::Truncate => {
                if !is_reg(&st) {
                    return Err(ResolveError::Unsupported(
                        "only regular files are truncated".into(),
                    ));
                }
                let fd = open_at(
                    parent,
                    &cname,
                    libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                )
                .map_err(io_err)?;
                if fstat(fd.as_raw_fd()).map(|s| ident(&s)).map_err(io_err)? != ident(&st) {
                    return Err(changed(target.requested()));
                }
                // SAFETY: live descriptor opened for writing.
                if unsafe { libc::ftruncate(fd.as_raw_fd(), 0) } != 0 {
                    return Err(io_err(io::Error::last_os_error()));
                }
                outcome.removed_entries = 1;
            }
            DestructiveKind::RemoveTree | DestructiveKind::EraseContents => {
                if !is_dir(&st) {
                    if kind == DestructiveKind::EraseContents {
                        return Err(ResolveError::Unsupported(
                            "contents of a non-directory".into(),
                        ));
                    }
                    // A link is removed itself, never traversed.
                    unlink_at(parent, &cname, false).map_err(io_err)?;
                    outcome.removed_entries = 1;
                    return Ok(outcome);
                }
                let dir = open_at(parent, &cname, DIR_NOFOLLOW).map_err(io_err)?;
                if fstat(dir.as_raw_fd()).map(|s| ident(&s)).map_err(io_err)? != ident(&st) {
                    return Err(changed(target.requested()));
                }
                erase(&dir, &PathBuf::from(path), st.st_dev, &mut outcome, 0);
                if kind == DestructiveKind::RemoveTree && outcome.remaining.is_empty() {
                    drop(dir);
                    match unlink_at(parent, &cname, true) {
                        Ok(()) => outcome.removed_entries += 1,
                        Err(_) => outcome.remaining.push(path.to_string_lossy().into_owned()),
                    }
                }
                outcome.complete = outcome.remaining.is_empty();
            }
        }
        Ok(outcome)
    }
}
