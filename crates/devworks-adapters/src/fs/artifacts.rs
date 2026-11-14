//! Managed artifact files beneath the application data root ([`ArtifactFiles`]).
//!
//! Layout (all paths constructed here, never from caller input):
//!
//! * `staging/<operation>.part` — in-flight bytes of one journaled staging operation;
//! * `artifacts/sha256/<first two hex>/<digest>.<generation>` — immutable blob generations;
//! * `quarantine/<digest>.<generation>.<n>` — corrupt generations kept as evidence;
//! * `<cache root>/artifact-previews/<preview id>` — disposable derived previews.
//!
//! **Generations.** A blob file is one immutable incarnation of a digest. The staging operation
//! that publishes new bytes names the generation (its own ID), so a generation's file is
//! written exactly once and every removal names an exact `(digest, generation)`: a stale
//! remover can never delete a newer incarnation of the same bytes.
//!
//! **Publication** streams the source in 64 KiB chunks into the staging file while hashing it
//! (bounded memory, at most the expected length plus one byte read), refuses another length or
//! digest, flushes the file, and then either reuses the verified existing generation or renames
//! the staged file into place as the new one. Every directory created is flushed into its
//! parent, and the destination directory after the rename.
//!
//! **Reads** verify the generation's digest on the open handle before serving any range of it,
//! and remember the verification against the handle's identity, length and timestamps (Unix:
//! device/inode, size, mtime, ctime; Windows: volume serial, file index, size, last write and
//! change time). A file whose timestamps fall within [`RACY_WINDOW`] of the verification is not
//! remembered (the racy-timestamp rule: a change within the timestamp granularity could
//! otherwise go unnoticed). Because generations are immutable, the remembered verification is
//! not repeated on every read: **bit rot after the first verification is caught by recovery**
//! (`ArtifactService::recover` re-hashes every Present generation) or an integrity check, and a
//! changed file is re-verified as soon as its metadata changes. Opening a stream hashes the
//! handle it then serves; export hashes while copying and refuses a mismatch before the
//! destination is touched.
//!
//! **Durability by platform.**
//! * Unix: `fsync` on the file, on each new directory's parent and on the destination
//!   directory after the rename.
//! * Windows: `FlushFileBuffers` on the file (`sync_all`), and renames use
//!   `MoveFileExW(MOVEFILE_WRITE_THROUGH)`, which returns only after the move is flushed. std
//!   offers no directory-handle flush; NTFS journals directory metadata (creation and rename
//!   are atomic and survive a crash once its log is flushed, which write-through forces for the
//!   rename), but a newly created directory's own entry is not separately forced to disk.
use devworks_core::authority::destructive::FileIdentity;
use devworks_core::ports::files::{ArtifactFiles, FileFault, PublishedBlob};
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// Timestamp granularity margin of the racy-timestamp rule (covers 2 s FAT granularity).
pub const RACY_WINDOW: Duration = Duration::from_secs(2);

/// Publication steps a test can crash at.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// The staged file is written but not flushed.
    Written,
    /// The staged file is flushed and verified but not renamed.
    Flushed,
    /// The blob is in place but its metadata is not committed.
    Renamed,
}

/// What a verification was made against: identity, length and timestamps of the open handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    identity: (u64, u64),
    len: u64,
    modified: Option<SystemTime>,
    changed: Option<SystemTime>,
}

#[cfg(unix)]
fn stamp(f: &File) -> io::Result<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let m = f.metadata()?;
    let changed = u64::try_from(m.ctime()).ok().map(|s| {
        SystemTime::UNIX_EPOCH
            + Duration::from_secs(s)
            + Duration::from_nanos(m.ctime_nsec().max(0) as u64)
    });
    Ok(Stamp {
        identity: (m.dev(), m.ino()),
        len: m.len(),
        modified: m.modified().ok(),
        changed,
    })
}

#[cfg(windows)]
fn stamp(f: &File) -> io::Result<Stamp> {
    use std::os::windows::io::AsRawHandle;
    /// `BY_HANDLE_FILE_INFORMATION` (fileapi.h).
    #[repr(C)]
    #[derive(Default)]
    struct HandleInformation {
        attributes: u32,
        times: [u32; 6],
        volume_serial: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }
    /// `FILE_BASIC_INFO` (winbase.h): 100 ns ticks since 1601.
    #[repr(C)]
    #[derive(Default)]
    struct BasicInformation {
        creation: i64,
        last_access: i64,
        last_write: i64,
        change: i64,
        attributes: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(
            file: *mut core::ffi::c_void,
            info: *mut HandleInformation,
        ) -> i32;
        fn GetFileInformationByHandleEx(
            file: *mut core::ffi::c_void,
            class: i32,
            info: *mut core::ffi::c_void,
            size: u32,
        ) -> i32;
    }
    const FILE_BASIC_INFO_CLASS: i32 = 0;
    let handle = f.as_raw_handle().cast();
    let mut info = HandleInformation::default();
    let mut basic = BasicInformation::default();
    // SAFETY: a live handle owned by `f` and valid, correctly laid out out pointers.
    unsafe {
        if GetFileInformationByHandle(handle, &mut info) == 0 {
            return Err(io::Error::last_os_error());
        }
        if GetFileInformationByHandleEx(
            handle,
            FILE_BASIC_INFO_CLASS,
            (&mut basic as *mut BasicInformation).cast(),
            std::mem::size_of::<BasicInformation>() as u32,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    let ticks = |t: i64| {
        // 1601-01-01 to 1970-01-01 in 100 ns ticks.
        let unix = t.checked_sub(116_444_736_000_000_000)?;
        Some(SystemTime::UNIX_EPOCH + Duration::from_nanos(u64::try_from(unix).ok()? * 100))
    };
    Ok(Stamp {
        identity: (
            u64::from(info.volume_serial),
            (u64::from(info.index_high) << 32) | u64::from(info.index_low),
        ),
        len: (u64::from(info.size_high) << 32) | u64::from(info.size_low),
        modified: ticks(basic.last_write),
        changed: ticks(basic.change),
    })
}

#[cfg(not(any(unix, windows)))]
fn stamp(f: &File) -> io::Result<Stamp> {
    let m = f.metadata()?;
    Ok(Stamp {
        identity: (0, 0),
        len: m.len(),
        modified: m.modified().ok(),
        changed: None,
    })
}

impl Stamp {
    /// The racy-timestamp rule: a stamp is only trusted when both timestamps are known and lie
    /// clearly before the verification.
    fn settled(&self, verified_at: SystemTime) -> bool {
        let Some(cutoff) = verified_at.checked_sub(RACY_WINDOW) else {
            return false;
        };
        [self.modified, self.changed]
            .iter()
            .all(|t| t.is_some_and(|t| t < cutoff))
    }
}

pub struct NativeArtifactFiles {
    data_root: PathBuf,
    cache_root: PathBuf,
    serial: AtomicU64,
    verified: Mutex<HashMap<(String, String), Stamp>>,
    #[cfg(test)]
    pub(crate) crash_at: Mutex<Option<Step>>,
    /// Directories flushed, in order (durability tests).
    #[cfg(test)]
    pub(crate) synced: Mutex<Vec<PathBuf>>,
}

fn fault(e: io::Error) -> FileFault {
    match e.kind() {
        io::ErrorKind::NotFound => FileFault::NotFound,
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => FileFault::DiskFull,
        io::ErrorKind::AlreadyExists => FileFault::Exists,
        k => FileFault::Io(k.to_string()),
    }
}
fn valid_digest(d: &str) -> Result<(), FileFault> {
    if d.len() == 64
        && d.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(FileFault::Unsupported("not a SHA-256 digest".into()))
    }
}
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn valid_generation(g: &str) -> Result<(), FileFault> {
    if valid_id(g) {
        Ok(())
    } else {
        Err(FileFault::Unsupported("not a blob generation".into()))
    }
}
fn valid_name(n: &str) -> Result<(), FileFault> {
    let stem = n.strip_suffix(".part").unwrap_or(n);
    if valid_id(stem) {
        Ok(())
    } else {
        Err(FileFault::Unsupported("not a managed file name".into()))
    }
}
fn hex(h: Sha256) -> String {
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}
/// Streams a reader's SHA-256 and length.
fn hash_reader(r: &mut dyn Read) -> io::Result<(String, u64)> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut n = 0u64;
    loop {
        let k = r.read(&mut buf)?;
        if k == 0 {
            break;
        }
        h.update(&buf[..k]);
        n += k as u64;
    }
    Ok((hex(h), n))
}
fn corrupt(digest: &str) -> FileFault {
    FileFault::Corrupt(format!(
        "managed file {digest} does not match its recorded identity"
    ))
}

#[cfg(windows)]
mod durable {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(Some(0)).collect()
    }
    /// A rename that returns only after it is flushed to disk; without `replace` an existing
    /// destination is an `AlreadyExists` error.
    pub fn rename(from: &Path, to: &Path, replace: bool) -> io::Result<()> {
        let (f, t) = (wide(from), wide(to));
        let flags = MOVEFILE_WRITE_THROUGH
            | if replace {
                MOVEFILE_REPLACE_EXISTING
            } else {
                0
            };
        // SAFETY: both buffers are NUL-terminated UTF-16 paths that outlive the call.
        if unsafe { MoveFileExW(f.as_ptr(), t.as_ptr(), flags) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl NativeArtifactFiles {
    /// `data_root` is the application data root; `cache_root` the disposable cache directory.
    pub fn new(data_root: impl Into<PathBuf>, cache_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
            cache_root: cache_root.into(),
            serial: AtomicU64::new(0),
            verified: Mutex::new(HashMap::new()),
            #[cfg(test)]
            crash_at: Mutex::new(None),
            #[cfg(test)]
            synced: Mutex::new(Vec::new()),
        }
    }
    fn staging(&self) -> PathBuf {
        self.data_root.join("staging")
    }
    /// The data-root-relative managed path recorded in the store (`/`-separated on every
    /// platform, so a record never depends on where it was written).
    fn relative(digest: &str, generation: &str) -> String {
        format!("artifacts/sha256/{}/{digest}.{generation}", &digest[..2])
    }
    /// The content-addressed store directory. Built one component at a time, as every native
    /// path under the data root is: a verbatim Windows root (`\\?\C:\...`) disables separator
    /// normalization, so a joined `/`-separated string would name one invalid component.
    fn store_dir(&self) -> PathBuf {
        self.data_root.join("artifacts").join("sha256")
    }
    fn blob_path(&self, digest: &str, generation: &str) -> Result<PathBuf, FileFault> {
        valid_digest(digest)?;
        valid_generation(generation)?;
        Ok(self
            .store_dir()
            .join(&digest[..2])
            .join(format!("{digest}.{generation}")))
    }
    fn previews(&self) -> PathBuf {
        self.cache_root.join("artifact-previews")
    }
    #[cfg(test)]
    fn step(&self, step: Step) {
        let hit = *self.crash_at.lock().unwrap() == Some(step);
        if hit {
            panic!("simulated crash at {step:?}");
        }
    }

    /// Flushes a directory's entries (Unix). See the module docs for Windows.
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        #[cfg(test)]
        self.synced.lock().unwrap().push(dir.to_path_buf());
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

    /// Creates `dir` and any missing ancestors, flushing each new directory's entry into its
    /// parent (a crash cannot lose the directory a published blob was renamed into).
    fn ensure_dir(&self, dir: &Path) -> io::Result<()> {
        let mut missing = Vec::new();
        let mut cur = dir;
        while std::fs::symlink_metadata(cur).is_err() {
            missing.push(cur.to_path_buf());
            match cur.parent() {
                Some(p) => cur = p,
                None => break,
            }
        }
        for d in missing.into_iter().rev() {
            match std::fs::create_dir(&d) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
            if let Some(parent) = d.parent() {
                self.sync_dir(parent)?;
            }
        }
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path, replace: bool) -> io::Result<()> {
        #[cfg(windows)]
        {
            durable::rename(from, to, replace)
        }
        #[cfg(not(windows))]
        {
            // Unix `rename` replaces; callers that must not replace use a fresh, never-used
            // destination name (a new generation) or the no-clobber link.
            let _ = replace;
            std::fs::rename(from, to)
        }
    }

    fn forget(&self, digest: &str, generation: &str) {
        self.verified
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&(digest.to_owned(), generation.to_owned()));
    }

    /// Reuses a settled proof only for the same open handle identity and timestamps. Unknown
    /// or changed generations are hashed in controlled chunks before any range is served.
    fn open_controlled_file(
        &self,
        digest: &str,
        generation: &str,
        byte_count: u64,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<File, FileFault> {
        let interrupted = || FileFault::Source("blob verification interrupted".into());
        if stopped() {
            return Err(interrupted());
        }
        // Match open_verified's racy-timestamp rule: verification time precedes the stamp,
        // and both native modification/change times must have settled before reuse.
        let verified_at = SystemTime::now();
        let mut file = File::open(self.blob_path(digest, generation)?).map_err(fault)?;
        let before = stamp(&file).map_err(fault)?;
        if before.len != byte_count {
            self.forget(digest, generation);
            return Err(corrupt(digest));
        }
        let key = (digest.to_owned(), generation.to_owned());
        let known = self
            .verified
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .copied();
        if known != Some(before) {
            let mut hash = Sha256::new();
            let mut bytes = 0u64;
            let mut buffer = [0; 64 * 1024];
            loop {
                if stopped() {
                    return Err(interrupted());
                }
                let count = file.read(&mut buffer).map_err(fault)?;
                if count == 0 {
                    break;
                }
                bytes = bytes
                    .checked_add(count as u64)
                    .ok_or_else(|| corrupt(digest))?;
                if bytes > byte_count {
                    self.forget(digest, generation);
                    return Err(corrupt(digest));
                }
                hash.update(&buffer[..count]);
            }
            if stopped() {
                return Err(interrupted());
            }
            if bytes != byte_count || hex(hash) != digest {
                self.forget(digest, generation);
                return Err(corrupt(digest));
            }
            let after = stamp(&file).map_err(fault)?;
            if after == before && before.settled(verified_at) {
                self.verified
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(key, before);
            }
            file.seek(SeekFrom::Start(0)).map_err(fault)?;
        }
        if stopped() {
            return Err(interrupted());
        }
        Ok(file)
    }

    fn open_verified(
        &self,
        digest: &str,
        generation: &str,
        byte_count: u64,
    ) -> Result<File, FileFault> {
        // The verification time is taken before the stamp: however long hashing takes, a
        // timestamp close to the stamp is never treated as settled.
        let verified_at = SystemTime::now();
        let mut f = File::open(self.blob_path(digest, generation)?).map_err(fault)?;
        let before = stamp(&f).map_err(fault)?;
        if before.len != byte_count {
            return Err(corrupt(digest));
        }
        let key = (digest.to_owned(), generation.to_owned());
        let known = self
            .verified
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .copied();
        if known != Some(before) {
            let (d, n) = hash_reader(&mut f).map_err(fault)?;
            if d != digest || n != byte_count {
                self.forget(digest, generation);
                return Err(corrupt(digest));
            }
            // Remember it only if the handle did not change while it was hashed and its
            // timestamps are settled (racy-timestamp rule).
            let after = stamp(&f).map_err(fault)?;
            if after == before && before.settled(verified_at) {
                self.verified
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(key, before);
            }
            f.seek(SeekFrom::Start(0)).map_err(fault)?;
        }
        Ok(f)
    }

    /// Streams at most `expected_bytes + 1` bytes of `source` into `staged`, returning its
    /// digest and length.
    fn stage(
        &self,
        staged: &Path,
        source: &mut dyn Read,
        expected_bytes: u64,
    ) -> Result<(String, u64), FileFault> {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(staged)
            .map_err(fault)?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut n = 0u64;
        loop {
            let room = expected_bytes.saturating_add(1).saturating_sub(n);
            if room == 0 {
                return Err(FileFault::Mismatch);
            }
            let want = buf.len().min(usize::try_from(room).unwrap_or(usize::MAX));
            let k = match source.read(&mut buf[..want]) {
                Ok(k) => k,
                Err(e) => return Err(FileFault::Source(e.to_string())),
            };
            if k == 0 {
                break;
            }
            n += k as u64;
            if n > expected_bytes {
                return Err(FileFault::Mismatch);
            }
            h.update(&buf[..k]);
            f.write_all(&buf[..k]).map_err(fault)?;
        }
        #[cfg(test)]
        self.step(Step::Written);
        f.sync_all().map_err(fault)?;
        Ok((hex(h), n))
    }

    fn publish_staged(
        &self,
        staged: &Path,
        source: &mut dyn Read,
        digest: &str,
        expected_bytes: u64,
        reuse: Option<&str>,
        generation: &str,
    ) -> Result<PublishedBlob, FileFault> {
        let (d, n) = self.stage(staged, source, expected_bytes)?;
        if d != digest || n != expected_bytes {
            return Err(FileFault::Mismatch);
        }
        #[cfg(test)]
        self.step(Step::Flushed);
        if let Some(existing) = reuse {
            match self.open_verified(digest, existing, expected_bytes) {
                Ok(_) => {
                    std::fs::remove_file(staged).map_err(fault)?;
                    return Ok(PublishedBlob {
                        generation: existing.to_owned(),
                        relative_path: Self::relative(digest, existing),
                        reused: true,
                    });
                }
                // A missing generation is not reused: the bytes are published anew.
                Err(FileFault::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        let dest = self.blob_path(digest, generation)?;
        let dir = dest.parent().expect("digest directory");
        self.ensure_dir(dir).map_err(fault)?;
        // A generation name is used once: an existing destination is never replaced.
        if std::fs::symlink_metadata(&dest).is_ok() {
            return Err(FileFault::Exists);
        }
        self.rename(staged, &dest, false).map_err(fault)?;
        self.sync_dir(dir).map_err(fault)?;
        #[cfg(test)]
        self.step(Step::Renamed);
        Ok(PublishedBlob {
            generation: generation.to_owned(),
            relative_path: Self::relative(digest, generation),
            reused: false,
        })
    }
}

impl ArtifactFiles for NativeArtifactFiles {
    fn publish(
        &self,
        staging_name: &str,
        source: &mut dyn Read,
        expected_digest: &str,
        expected_bytes: u64,
        reuse: Option<&str>,
        generation: &str,
    ) -> Result<PublishedBlob, FileFault> {
        valid_name(staging_name)?;
        valid_digest(expected_digest)?;
        valid_generation(generation)?;
        if let Some(r) = reuse {
            valid_generation(r)?;
        }
        let staging_dir = self.staging();
        self.ensure_dir(&staging_dir).map_err(fault)?;
        let staged = staging_dir.join(staging_name);
        let published = self.publish_staged(
            &staged,
            source,
            expected_digest,
            expected_bytes,
            reuse,
            generation,
        );
        if published.is_err() {
            let _ = std::fs::remove_file(&staged);
        }
        published
    }

    fn managed_path(&self, digest: &str, generation: &str) -> Result<String, FileFault> {
        valid_digest(digest)?;
        valid_generation(generation)?;
        Ok(Self::relative(digest, generation))
    }

    // `present` and `inspect` may race the removal of the generation they probe (collection
    // unlinking it while a save or verification looks at it). On Windows a file deleted while
    // another handle is open stays delete-pending and refuses opens as `PermissionDenied`
    // until it disappears, so both probes wait that out and then answer absent.
    fn present(&self, digest: &str, generation: &str, byte_count: u64) -> Result<bool, FileFault> {
        let path = self.blob_path(digest, generation)?;
        match super::retry_denied(|| std::fs::symlink_metadata(&path)) {
            Ok(m) => Ok(m.is_file() && m.len() == byte_count),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(fault(e)),
        }
    }

    fn inspect(&self, digest: &str, generation: &str) -> Result<Option<u64>, FileFault> {
        let path = self.blob_path(digest, generation)?;
        let mut f = match super::retry_denied(|| File::open(&path)) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(fault(e)),
        };
        let (d, n) = hash_reader(&mut f).map_err(fault)?;
        if d != digest {
            self.forget(digest, generation);
            return Err(corrupt(digest));
        }
        Ok(Some(n))
    }

    fn read_range(
        &self,
        digest: &str,
        generation: &str,
        byte_count: u64,
        offset: u64,
        max: usize,
    ) -> Result<Vec<u8>, FileFault> {
        let mut f = self.open_verified(digest, generation, byte_count)?;
        f.seek(SeekFrom::Start(offset)).map_err(fault)?;
        let mut out = Vec::with_capacity(max.min(byte_count.saturating_sub(offset) as usize));
        f.take(max as u64).read_to_end(&mut out).map_err(fault)?;
        Ok(out)
    }

    fn open(
        &self,
        digest: &str,
        generation: &str,
        byte_count: u64,
    ) -> Result<Box<dyn Read + Send>, FileFault> {
        // Verified on, and served from, the same handle.
        let mut f = File::open(self.blob_path(digest, generation)?).map_err(fault)?;
        let (d, n) = hash_reader(&mut f).map_err(fault)?;
        if d != digest || n != byte_count {
            self.forget(digest, generation);
            return Err(corrupt(digest));
        }
        f.seek(SeekFrom::Start(0)).map_err(fault)?;
        Ok(Box::new(f.take(byte_count)))
    }

    fn open_controlled(
        &self,
        digest: &str,
        generation: &str,
        byte_count: u64,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Box<dyn Read + Send>, FileFault> {
        let file = self.open_controlled_file(digest, generation, byte_count, stopped)?;
        Ok(Box::new(file.take(byte_count)))
    }

    fn read_range_controlled(
        &self,
        digest: &str,
        generation: &str,
        byte_count: u64,
        offset: u64,
        max: usize,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Vec<u8>, FileFault> {
        let mut file = self.open_controlled_file(digest, generation, byte_count, stopped)?;
        file.seek(SeekFrom::Start(offset)).map_err(fault)?;
        let count = max.min(byte_count.saturating_sub(offset) as usize);
        let mut bytes = Vec::with_capacity(count);
        let mut left = count;
        let mut buffer = [0; 64 * 1024];
        while left > 0 {
            if stopped() {
                return Err(FileFault::Source("blob range interrupted".into()));
            }
            let length = left.min(buffer.len());
            let n = file.read(&mut buffer[..length]).map_err(fault)?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..n]);
            left -= n;
        }
        Ok(bytes)
    }

    fn unlink(&self, digest: &str, generation: &str) -> Result<(), FileFault> {
        self.forget(digest, generation);
        match std::fs::remove_file(self.blob_path(digest, generation)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(fault(e)),
        }
    }

    fn quarantine(&self, digest: &str, generation: &str) -> Result<(), FileFault> {
        let path = self.blob_path(digest, generation)?;
        self.forget(digest, generation);
        let dir = self.data_root.join("quarantine");
        self.ensure_dir(&dir).map_err(fault)?;
        let n = self.serial.fetch_add(1, Ordering::Relaxed);
        let target = dir.join(format!("{digest}.{generation}.{}-{n}", std::process::id()));
        match self.rename(&path, &target, false) {
            Ok(()) => self.sync_dir(&dir).map_err(fault),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(fault(e)),
        }
    }

    fn staged(&self) -> Result<Vec<String>, FileFault> {
        let entries = match std::fs::read_dir(self.staging()) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(fault(e)),
        };
        let mut out = Vec::new();
        for e in entries {
            let e = e.map_err(fault)?;
            out.push(e.file_name().to_string_lossy().into_owned());
        }
        out.sort();
        Ok(out)
    }

    fn remove_staged(&self, name: &str) -> Result<(), FileFault> {
        valid_name(name)?;
        match std::fs::remove_file(self.staging().join(name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(fault(e)),
        }
    }

    fn stored_files(&self) -> Result<Vec<(String, String)>, FileFault> {
        let root = self.store_dir();
        let shards = match std::fs::read_dir(&root) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(fault(e)),
        };
        let mut out = Vec::new();
        for shard in shards {
            let shard = shard.map_err(fault)?;
            if !shard.file_type().map_err(fault)?.is_dir() {
                continue;
            }
            for f in std::fs::read_dir(shard.path()).map_err(fault)? {
                let name = f.map_err(fault)?.file_name().to_string_lossy().into_owned();
                if let Some((d, g)) = name.split_once('.')
                    && valid_digest(d).is_ok()
                    && valid_id(g)
                {
                    out.push((d.to_owned(), g.to_owned()));
                }
            }
        }
        out.sort();
        Ok(out)
    }

    fn write_derived(&self, key: &str, bytes: &[u8]) -> Result<(), FileFault> {
        valid_name(key)?;
        let dir = self.previews();
        std::fs::create_dir_all(&dir).map_err(fault)?;
        let n = self.serial.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!("{key}.{}-{n}.tmp", std::process::id()));
        std::fs::write(&tmp, bytes).map_err(fault)?;
        std::fs::rename(&tmp, dir.join(key)).map_err(fault)
    }

    fn derived_root(&self) -> Result<FileIdentity, FileFault> {
        let dir = self.previews();
        std::fs::create_dir_all(&dir).map_err(fault)?;
        let meta = std::fs::symlink_metadata(&dir).map_err(fault)?;
        super::identity_at(&dir, &meta)
            .ok_or_else(|| FileFault::Unsupported("cache root identity unavailable".into()))
    }

    fn remove_derived_remnants(&self, key: &str, root: FileIdentity) -> Result<(), FileFault> {
        let dir = self.previews();
        let Ok(meta) = std::fs::symlink_metadata(&dir) else {
            return Ok(());
        };
        if super::identity_at(&dir, &meta) != Some(root) {
            return Err(FileFault::Mismatch);
        }
        valid_name(key)?;
        let prefix = format!("{key}.");
        for entry in std::fs::read_dir(&dir).map_err(fault)? {
            let entry = entry.map_err(fault)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if (name == key || (name.starts_with(&prefix) && name.ends_with(".tmp")))
                && entry.file_type().map_err(fault)?.is_file()
            {
                std::fs::remove_file(entry.path()).map_err(fault)?;
            }
        }
        Ok(())
    }

    fn read_derived(&self, key: &str, max: usize) -> Result<Option<Vec<u8>>, FileFault> {
        valid_name(key)?;
        match File::open(self.previews().join(key)) {
            Ok(f) => {
                let mut out = Vec::new();
                f.take(max as u64).read_to_end(&mut out).map_err(fault)?;
                Ok(Some(out))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(fault(e)),
        }
    }

    fn collect_derived(&self) -> Result<u64, FileFault> {
        let entries = match std::fs::read_dir(self.previews()) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(fault(e)),
        };
        let mut n = 0;
        for e in entries {
            let e = e.map_err(fault)?;
            if e.file_type().map_err(fault)?.is_file() {
                std::fs::remove_file(e.path()).map_err(fault)?;
                n += 1;
            }
        }
        Ok(n)
    }

    fn export(
        &self,
        digest: &str,
        generation: &str,
        byte_count: u64,
        destination: &Path,
        overwrite: bool,
    ) -> Result<(), FileFault> {
        let mut source = File::open(self.blob_path(digest, generation)?).map_err(fault)?;
        let (Some(dir), Some(name)) = (destination.parent(), destination.file_name()) else {
            return Err(FileFault::Unsupported(
                "destination has no file name".into(),
            ));
        };
        // The temporary name is bounded whatever the destination name's length (NAME_MAX).
        let n = self.serial.fetch_add(1, Ordering::Relaxed);
        let mut h = Sha256::new();
        h.update(name.as_encoded_bytes());
        let tmp = dir.join(format!(
            ".dwx-{}-{}-{n}.tmp",
            &hex(h)[..16],
            std::process::id()
        ));
        // Copy from one open handle while hashing; a mismatch never reaches the destination.
        let mut copy = || -> Result<(), FileFault> {
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(fault)?;
            let mut h = Sha256::new();
            let mut buf = vec![0u8; 64 * 1024];
            let mut total = 0u64;
            loop {
                let k = source.read(&mut buf).map_err(fault)?;
                if k == 0 {
                    break;
                }
                total += k as u64;
                h.update(&buf[..k]);
                out.write_all(&buf[..k]).map_err(fault)?;
            }
            if total != byte_count || hex(h) != digest {
                return Err(corrupt(digest));
            }
            out.sync_all().map_err(fault)
        };
        if let Err(e) = copy() {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        let published = if overwrite {
            self.rename(&tmp, destination, true).map_err(fault)
        } else {
            match std::fs::hard_link(&tmp, destination) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(FileFault::Exists),
                // Volumes without hard links: create the destination exclusively and copy.
                Err(_) => OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(destination)
                    .map_err(fault)
                    .and_then(|mut out| {
                        io::copy(&mut File::open(&tmp).map_err(fault)?, &mut out).map_err(fault)?;
                        out.sync_all().map_err(fault)
                    }),
            }
        };
        let _ = std::fs::remove_file(&tmp);
        published?;
        self.sync_dir(dir).map_err(fault)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "devworks-artifact-files-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn digest(b: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(b);
        hex(h)
    }

    #[test]
    fn publication_flushes_every_directory_it_creates() {
        let data = root("dirs");
        let files = NativeArtifactFiles::new(&data, data.join("cache"));
        let bytes = b"durable";
        let d = digest(bytes);
        files
            .publish("op-1.part", &mut &bytes[..], &d, 7, None, "op-1")
            .unwrap();
        let synced = files.synced.lock().unwrap().clone();
        let shard = data.join(format!("artifacts/sha256/{}", &d[..2]));
        // New directories are flushed into their parents, and the shard after the rename.
        for dir in [
            data.clone(),
            data.join("artifacts"),
            data.join("artifacts/sha256"),
            shard.clone(),
        ] {
            assert!(synced.contains(&dir), "{dir:?} not flushed: {synced:?}");
        }
        assert_eq!(synced.last(), Some(&shard));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn publication_streams_bounded_by_the_expected_length() {
        let data = root("stream");
        let files = NativeArtifactFiles::new(&data, data.join("cache"));
        let big = vec![7u8; 300 * 1024];
        let d = digest(&big);
        let published = files
            .publish(
                "op-1.part",
                &mut &big[..],
                &d,
                big.len() as u64,
                None,
                "op-1",
            )
            .unwrap();
        assert!(!published.reused);
        assert_eq!(files.inspect(&d, "op-1").unwrap(), Some(big.len() as u64));
        // A longer source is a mismatch after at most one byte beyond the expected length.
        struct Endless(u64);
        impl Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.0 += buf.len() as u64;
                buf.fill(7);
                Ok(buf.len())
            }
        }
        let mut endless = Endless(0);
        assert_eq!(
            files.publish("op-2.part", &mut endless, &d, 1024, None, "op-2"),
            Err(FileFault::Mismatch)
        );
        assert!(endless.0 <= 1025, "read {} bytes", endless.0);
        assert_eq!(
            files.publish("op-3.part", &mut &b"other"[..], &d, 5, None, "op-3"),
            Err(FileFault::Mismatch)
        );
        // Reuse keeps the existing generation; a new generation never replaces a file.
        let again = files
            .publish(
                "op-4.part",
                &mut &big[..],
                &d,
                big.len() as u64,
                Some("op-1"),
                "op-4",
            )
            .unwrap();
        assert_eq!((again.generation.as_str(), again.reused), ("op-1", true));
        assert_eq!(
            files.publish(
                "op-5.part",
                &mut &big[..],
                &d,
                big.len() as u64,
                None,
                "op-1"
            ),
            Err(FileFault::Exists)
        );
        assert!(files.staged().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn ranges_are_never_served_from_a_corrupt_generation() {
        let data = root("verify");
        let files = NativeArtifactFiles::new(&data, data.join("cache"));
        let bytes = b"0123456789";
        let d = digest(bytes);
        files
            .publish("op-1.part", &mut &bytes[..], &d, 10, None, "op-1")
            .unwrap();
        assert_eq!(files.read_range(&d, "op-1", 10, 2, 3).unwrap(), b"234");
        // Same length, different content: verified again because the file changed.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(
            data.join(NativeArtifactFiles::relative(&d, "op-1")),
            b"9876543210",
        )
        .unwrap();
        assert!(matches!(
            files.read_range(&d, "op-1", 10, 2, 3),
            Err(FileFault::Corrupt(_))
        ));
        assert!(files.open(&d, "op-1", 10).is_err());
        assert!(matches!(
            files.inspect(&d, "op-1"),
            Err(FileFault::Corrupt(_))
        ));
        files.quarantine(&d, "op-1").unwrap();
        assert_eq!(files.inspect(&d, "op-1").unwrap(), None);
        assert_eq!(
            std::fs::read_dir(data.join("quarantine")).unwrap().count(),
            1
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn controlled_help_reads_cancel_the_first_hash_and_verify_the_full_source_before_ranges() {
        let data = root("controlled-help");
        let files = NativeArtifactFiles::new(&data, data.join("cache"));
        let bytes = vec![b'a'; 2 * 1024 * 1024];
        let d = digest(&bytes);
        let n = bytes.len() as u64;
        files
            .publish("op-1.part", &mut bytes.as_slice(), &d, n, None, "op-1")
            .unwrap();
        let mut calls = 0;
        assert!(
            files
                .open_controlled(&d, "op-1", n, &mut || {
                    calls += 1;
                    calls == 3
                })
                .is_err()
        );
        assert_eq!(
            calls, 3,
            "stop during the first hash, not after a whole source pass"
        );
        let mut calls = 0;
        assert!(
            files
                .read_range_controlled(&d, "op-1", n, n - 4, 4, &mut || {
                    calls += 1;
                    calls == 3
                })
                .is_err()
        );
        assert_eq!(calls, 3);
        assert_eq!(
            files
                .read_range_controlled(&d, "op-1", n, n - 4, 4, &mut || false)
                .unwrap(),
            b"aaaa"
        );
        // A changed byte outside the requested tail still invalidates the whole source.
        let path = files.blob_path(&d, "op-1").unwrap();
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all(b"b").unwrap();
        drop(file);
        assert!(matches!(
            files.read_range_controlled(&d, "op-1", n, n - 4, 4, &mut || false),
            Err(FileFault::Corrupt(_))
        ));
        let _ = std::fs::remove_dir_all(data);
    }

    #[test]
    fn controlled_ranges_reuse_only_settled_unchanged_verification() {
        let data = root("controlled-settled");
        let files = NativeArtifactFiles::new(&data, data.join("cache"));
        let bytes = vec![b'a'; 2 * 1024 * 1024];
        let d = digest(&bytes);
        let n = bytes.len() as u64;
        files
            .publish("op-1.part", &mut bytes.as_slice(), &d, n, None, "op-1")
            .unwrap();
        // A real unchanged file must age beyond the filesystem's racy timestamp window.
        // Backdating mtime alone would not qualify its native change-time stamp.
        std::thread::sleep(RACY_WINDOW + Duration::from_millis(100));
        let mut first = 0;
        assert_eq!(
            files
                .read_range_controlled(&d, "op-1", n, n - 4, 4, &mut || {
                    first += 1;
                    false
                })
                .unwrap(),
            b"aaaa"
        );
        assert!(
            first >= 32,
            "the first source verification is actually streamed"
        );
        assert_eq!(
            files.verified.lock().unwrap().len(),
            1,
            "settled same-handle verification must be reusable"
        );
        for offset in [0, n - 8] {
            let mut checks = 0;
            assert_eq!(
                files
                    .read_range_controlled(&d, "op-1", n, offset, 4, &mut || {
                        checks += 1;
                        false
                    })
                    .unwrap(),
                b"aaaa"
            );
            assert!(
                checks <= 4,
                "an unchanged page must not rehash the whole source: {checks} checks"
            );
        }
        assert!(
            files
                .read_range_controlled(&d, "op-1", n, 0, 4, &mut || true)
                .is_err(),
            "a cache hit never bypasses cancellation"
        );
        let path = files.blob_path(&d, "op-1").unwrap();
        let mut file = OpenOptions::new().write(true).open(path).unwrap();
        file.write_all(b"b").unwrap();
        file.sync_all().unwrap();
        drop(file);
        assert!(
            matches!(
                files.read_range_controlled(&d, "op-1", n, n - 4, 4, &mut || false),
                Err(FileFault::Corrupt(_))
            ),
            "a changed byte outside the page must invalidate the cached proof"
        );
        assert!(files.verified.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(data);
    }

    #[test]
    fn racy_timestamps_are_not_remembered() {
        let data = root("racy");
        let files = NativeArtifactFiles::new(&data, data.join("cache"));
        let bytes = b"just written";
        let d = digest(bytes);
        files
            .publish("op-1.part", &mut &bytes[..], &d, 12, None, "op-1")
            .unwrap();
        files.read_range(&d, "op-1", 12, 0, 4).unwrap();
        // Written moments ago: the verification is not cached, so the next read hashes again.
        assert!(files.verified.lock().unwrap().is_empty());
        let now = SystemTime::now();
        let old = now - Duration::from_secs(60);
        let stamp = |m, c| Stamp {
            identity: (1, 1),
            len: 1,
            modified: Some(m),
            changed: Some(c),
        };
        assert!(stamp(old, old).settled(now));
        assert!(!stamp(old, now).settled(now));
        assert!(!stamp(now, old).settled(now));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn managed_paths_are_built_one_component_at_a_time() {
        let data = root("components");
        let files = NativeArtifactFiles::new(&data, data.join("cache"));
        let d = digest(b"components");
        let path = files.blob_path(&d, "op-1").unwrap();
        let name = format!("{d}.op-1");
        // Every component under the data root is one normal component without a separator.
        let parts: Vec<String> = path
            .strip_prefix(&data)
            .unwrap()
            .components()
            .map(|c| match c {
                std::path::Component::Normal(p) => p.to_string_lossy().into_owned(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(parts, ["artifacts", "sha256", &d[..2], name.as_str()]);
        assert!(parts.iter().all(|p| !p.contains(['/', '\\'])));
        assert_eq!(
            path,
            data.join("artifacts")
                .join("sha256")
                .join(&d[..2])
                .join(&name)
        );
        assert_eq!(files.store_dir(), data.join("artifacts").join("sha256"));
        // The recorded managed path stays `/`-separated on every platform.
        assert_eq!(
            files.managed_path(&d, "op-1").unwrap(),
            format!("artifacts/sha256/{}/{name}", &d[..2])
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    /// A verbatim root (`\\?\`, what `canonicalize` returns on Windows) does not normalize `/`:
    /// every operation under it must still find the managed files.
    #[cfg(windows)]
    #[test]
    fn a_verbatim_windows_data_root_publishes_reads_lists_and_removes() {
        let data = std::fs::canonicalize(root("verbatim")).unwrap();
        assert!(data.to_string_lossy().starts_with(r"\\?\"), "{data:?}");
        let files = NativeArtifactFiles::new(&data, data.join("cache"));
        let bytes = b"verbatim root";
        let d = digest(bytes);
        let n = bytes.len() as u64;
        let published = files
            .publish("op-1.part", &mut &bytes[..], &d, n, None, "op-1")
            .unwrap();
        assert_eq!(
            published.relative_path,
            NativeArtifactFiles::relative(&d, "op-1")
        );
        assert!(files.present(&d, "op-1", n).unwrap());
        assert_eq!(files.inspect(&d, "op-1").unwrap(), Some(n));
        assert_eq!(files.read_range(&d, "op-1", n, 0, 64).unwrap(), bytes);
        let mut read = Vec::new();
        files
            .open(&d, "op-1", n)
            .unwrap()
            .read_to_end(&mut read)
            .unwrap();
        assert_eq!(read, bytes);
        assert_eq!(
            files.stored_files().unwrap(),
            vec![(d.clone(), "op-1".to_owned())]
        );
        // Reuse verifies the existing generation through the same path.
        let reused = files
            .publish("op-2.part", &mut &bytes[..], &d, n, Some("op-1"), "op-2")
            .unwrap();
        assert!(reused.reused);
        files.unlink(&d, "op-1").unwrap();
        assert!(!files.present(&d, "op-1", n).unwrap());
        assert!(files.stored_files().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn export_temporary_names_stay_bounded() {
        let data = root("export");
        let files = NativeArtifactFiles::new(&data, data.join("cache"));
        let bytes = b"export";
        let d = digest(bytes);
        files
            .publish("op-1.part", &mut &bytes[..], &d, 6, None, "op-1")
            .unwrap();
        let out = data.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let long = out.join(format!("{}.txt", "n".repeat(250)));
        files.export(&d, "op-1", 6, &long, false).unwrap();
        assert_eq!(std::fs::read(&long).unwrap(), bytes);
        // A corrupt generation never reaches the destination.
        std::fs::write(
            data.join(NativeArtifactFiles::relative(&d, "op-1")),
            b"EXPORT",
        )
        .unwrap();
        let other = out.join("other.txt");
        assert!(matches!(
            files.export(&d, "op-1", 6, &other, false),
            Err(FileFault::Corrupt(_))
        ));
        assert!(!other.exists());
        assert_eq!(
            std::fs::read_dir(&out).unwrap().count(),
            1,
            "no temporary left"
        );
        let _ = std::fs::remove_dir_all(&data);
    }
}
