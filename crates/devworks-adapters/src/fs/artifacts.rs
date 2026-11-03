//! Managed artifact files beneath the application data root ([`ArtifactFiles`]).
//!
//! Layout (all paths constructed here, never from caller input):
//!
//! * `staging/<operation>.part` — in-flight bytes of one journaled staging operation;
//! * `artifacts/sha256/<first two hex>/<digest>` — immutable content-addressed blobs;
//! * `quarantine/<digest>.<n>` — corrupt digest files kept as evidence, never served;
//! * `<cache root>/artifact-previews/<preview id>` — disposable derived previews.
//!
//! **Publication** streams the source in 64 KiB chunks into the staging file while hashing it
//! (memory stays bounded whatever the file size), refuses a source above the byte bound or with
//! another length/digest, flushes the file, then renames it into place. Every directory it
//! creates is flushed into its parent, and the destination directory is flushed after the
//! rename. An existing digest file is re-verified before reuse; a mismatch is corruption and is
//! never overwritten or served.
//!
//! **Reads** verify the whole file's digest once before serving any range of it and remember
//! the verification against the file's length, modification time and (Unix) inode/change time,
//! so a file changed afterwards is verified again before another byte is served.
//!
//! **Durability by platform.**
//! * Unix: `fsync` on the file, on each new directory's parent and on the destination
//!   directory after the rename.
//! * Windows: `FlushFileBuffers` on the file (`sync_all`), and the rename uses
//!   `MoveFileExW(MOVEFILE_WRITE_THROUGH)`, which returns only after the move is flushed. std
//!   offers no directory-handle flush; NTFS journals directory metadata (creation and rename
//!   are atomic and survive a crash once its log is flushed, which write-through forces for the
//!   rename), but a newly created directory's own entry is not separately forced to disk.
//!
//! Export copies through a flushed temporary file (bounded name) in the destination directory
//! and publishes it with a no-clobber hard link (or `create_new` where the volume has no hard
//! links), or a write-through atomic replace when overwrite was explicitly requested.
use devworks_core::ports::files::{ArtifactFiles, FileFault, PublishedBlob};
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

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

/// What a verification was made against: a file changed since is verified again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    extra: (u64, i64, i64),
}
#[cfg(unix)]
fn stamp(m: &Metadata) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    Stamp {
        len: m.len(),
        modified: m.modified().ok(),
        extra: (m.ino(), m.ctime(), m.ctime_nsec()),
    }
}
#[cfg(not(unix))]
fn stamp(m: &Metadata) -> Stamp {
    Stamp {
        len: m.len(),
        modified: m.modified().ok(),
        extra: (0, 0, 0),
    }
}

pub struct NativeArtifactFiles {
    data_root: PathBuf,
    cache_root: PathBuf,
    serial: AtomicU64,
    verified: Mutex<HashMap<String, Stamp>>,
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
fn valid_name(n: &str) -> Result<(), FileFault> {
    let stem = n.strip_suffix(".part").unwrap_or(n);
    if !stem.is_empty()
        && n.len() <= 160
        && stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
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
    fn relative(digest: &str) -> String {
        format!("artifacts/sha256/{}/{digest}", &digest[..2])
    }
    fn blob_path(&self, digest: &str) -> Result<PathBuf, FileFault> {
        valid_digest(digest)?;
        Ok(self.data_root.join(Self::relative(digest)))
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
            // Unix `rename` replaces; callers that must not replace checked the destination and
            // only ever replace an identical, verified blob.
            let _ = replace;
            std::fs::rename(from, to)
        }
    }

    fn verify_path(path: &Path, digest: &str, byte_count: u64) -> Result<(), FileFault> {
        let (d, n) = hash_reader(&mut File::open(path).map_err(fault)?).map_err(fault)?;
        if n != byte_count || d != digest {
            return Err(FileFault::Corrupt(format!(
                "managed file {digest} does not match its recorded identity"
            )));
        }
        Ok(())
    }

    fn forget(&self, digest: &str) {
        self.verified
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(digest);
    }

    /// Streams `source` into `staged`, returning its digest and length.
    fn stage(
        &self,
        staged: &Path,
        source: &mut dyn Read,
        max_bytes: u64,
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
            let k = match source.read(&mut buf) {
                Ok(k) => k,
                Err(e) => return Err(FileFault::Source(e.to_string())),
            };
            if k == 0 {
                break;
            }
            n += k as u64;
            if n > max_bytes {
                return Err(FileFault::TooLarge);
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
        max_bytes: u64,
        digest: &str,
        expected_bytes: u64,
    ) -> Result<PublishedBlob, FileFault> {
        let dest = self.blob_path(digest)?;
        let (d, n) = self.stage(staged, source, max_bytes)?;
        if d != digest || n != expected_bytes {
            return Err(FileFault::Mismatch);
        }
        #[cfg(test)]
        self.step(Step::Flushed);
        let dir = dest.parent().expect("digest directory");
        self.ensure_dir(dir).map_err(fault)?;
        let reuse = || -> Result<PublishedBlob, FileFault> {
            Self::verify_path(&dest, digest, expected_bytes)?;
            std::fs::remove_file(staged).map_err(fault)?;
            Ok(PublishedBlob {
                relative_path: Self::relative(digest),
                reused: true,
            })
        };
        if dest.exists() {
            return reuse();
        }
        match self.rename(staged, &dest, false) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return reuse(),
            Err(e) => return Err(fault(e)),
        }
        self.sync_dir(dir).map_err(fault)?;
        #[cfg(test)]
        self.step(Step::Renamed);
        Ok(PublishedBlob {
            relative_path: Self::relative(digest),
            reused: false,
        })
    }
}

impl ArtifactFiles for NativeArtifactFiles {
    fn publish(
        &self,
        staging_name: &str,
        source: &mut dyn Read,
        max_bytes: u64,
        expected_digest: &str,
        expected_bytes: u64,
    ) -> Result<PublishedBlob, FileFault> {
        valid_name(staging_name)?;
        valid_digest(expected_digest)?;
        let staging_dir = self.staging();
        self.ensure_dir(&staging_dir).map_err(fault)?;
        let staged = staging_dir.join(staging_name);
        let published =
            self.publish_staged(&staged, source, max_bytes, expected_digest, expected_bytes);
        if published.is_err() {
            let _ = std::fs::remove_file(&staged);
        }
        published
    }

    fn managed_path(&self, digest: &str) -> Result<String, FileFault> {
        valid_digest(digest)?;
        Ok(Self::relative(digest))
    }

    fn present(&self, digest: &str, byte_count: u64) -> Result<bool, FileFault> {
        match std::fs::symlink_metadata(self.blob_path(digest)?) {
            Ok(m) => Ok(m.is_file() && m.len() == byte_count),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(fault(e)),
        }
    }

    fn inspect(&self, digest: &str) -> Result<Option<u64>, FileFault> {
        let path = self.blob_path(digest)?;
        let mut f = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(fault(e)),
        };
        let (d, n) = hash_reader(&mut f).map_err(fault)?;
        if d != digest {
            return Err(FileFault::Corrupt(format!(
                "managed file {digest} does not hash to its name"
            )));
        }
        Ok(Some(n))
    }

    fn read_range(
        &self,
        digest: &str,
        byte_count: u64,
        offset: u64,
        max: usize,
    ) -> Result<Vec<u8>, FileFault> {
        let mut f = File::open(self.blob_path(digest)?).map_err(fault)?;
        let meta = f.metadata().map_err(fault)?;
        if meta.len() != byte_count {
            return Err(FileFault::Corrupt(format!(
                "managed file {digest} has the wrong length"
            )));
        }
        let now = stamp(&meta);
        let known = self
            .verified
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(digest)
            .copied();
        if known != Some(now) {
            let (d, n) = hash_reader(&mut f).map_err(fault)?;
            if d != digest || n != byte_count {
                self.forget(digest);
                return Err(FileFault::Corrupt(format!(
                    "managed file {digest} does not match its recorded identity"
                )));
            }
            // Remember the verification only if the file did not change while it was hashed.
            if f.metadata().map(|m| stamp(&m)).ok() == Some(now) {
                self.verified
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(digest.to_owned(), now);
            }
        }
        f.seek(SeekFrom::Start(offset)).map_err(fault)?;
        let mut out = Vec::with_capacity(max.min(byte_count.saturating_sub(offset) as usize));
        f.take(max as u64).read_to_end(&mut out).map_err(fault)?;
        Ok(out)
    }

    fn verify(&self, digest: &str, byte_count: u64) -> Result<(), FileFault> {
        Self::verify_path(&self.blob_path(digest)?, digest, byte_count)
    }

    fn open(&self, digest: &str, byte_count: u64) -> Result<Box<dyn Read + Send>, FileFault> {
        let path = self.blob_path(digest)?;
        Self::verify_path(&path, digest, byte_count)?;
        Ok(Box::new(File::open(path).map_err(fault)?))
    }

    fn unlink(&self, digest: &str) -> Result<(), FileFault> {
        self.forget(digest);
        match std::fs::remove_file(self.blob_path(digest)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(fault(e)),
        }
    }

    fn quarantine(&self, digest: &str) -> Result<(), FileFault> {
        let path = self.blob_path(digest)?;
        self.forget(digest);
        let dir = self.data_root.join("quarantine");
        self.ensure_dir(&dir).map_err(fault)?;
        let n = self.serial.fetch_add(1, Ordering::Relaxed);
        let target = dir.join(format!("{digest}.{}-{n}", std::process::id()));
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

    fn stored_digests(&self) -> Result<Vec<String>, FileFault> {
        let root = self.data_root.join("artifacts/sha256");
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
                if valid_digest(&name).is_ok() {
                    out.push(name);
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
        byte_count: u64,
        destination: &Path,
        overwrite: bool,
    ) -> Result<(), FileFault> {
        let source = self.blob_path(digest)?;
        Self::verify_path(&source, digest, byte_count)?;
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
        let copy = || -> Result<(), FileFault> {
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(fault)?;
            io::copy(&mut File::open(&source).map_err(fault)?, &mut out).map_err(fault)?;
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
            .publish("op-1.part", &mut &bytes[..], 64, &d, bytes.len() as u64)
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
    fn publication_streams_and_refuses_bounds_and_mismatches_without_leftovers() {
        let data = root("stream");
        let files = NativeArtifactFiles::new(&data, data.join("cache"));
        let big = vec![7u8; 300 * 1024];
        let d = digest(&big);
        let published = files
            .publish(
                "op-1.part",
                &mut &big[..],
                big.len() as u64,
                &d,
                big.len() as u64,
            )
            .unwrap();
        assert!(!published.reused);
        assert_eq!(files.inspect(&d).unwrap(), Some(big.len() as u64));
        assert_eq!(
            files.publish("op-2.part", &mut &big[..], 1024, &d, big.len() as u64),
            Err(FileFault::TooLarge)
        );
        assert_eq!(
            files.publish("op-3.part", &mut &b"other"[..], 64, &d, 5),
            Err(FileFault::Mismatch)
        );
        assert!(files.staged().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn ranges_are_never_served_from_a_corrupt_file() {
        let data = root("verify");
        let files = NativeArtifactFiles::new(&data, data.join("cache"));
        let bytes = b"0123456789";
        let d = digest(bytes);
        files
            .publish("op-1.part", &mut &bytes[..], 64, &d, 10)
            .unwrap();
        assert_eq!(files.read_range(&d, 10, 2, 3).unwrap(), b"234");
        // Same length, different content: verified again because the file changed.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(data.join(NativeArtifactFiles::relative(&d)), b"9876543210").unwrap();
        assert!(matches!(
            files.read_range(&d, 10, 2, 3),
            Err(FileFault::Corrupt(_))
        ));
        assert!(matches!(files.inspect(&d), Err(FileFault::Corrupt(_))));
        files.quarantine(&d).unwrap();
        assert_eq!(files.inspect(&d).unwrap(), None);
        assert_eq!(
            std::fs::read_dir(data.join("quarantine")).unwrap().count(),
            1
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn export_temporary_names_stay_bounded() {
        let data = root("export");
        let files = NativeArtifactFiles::new(&data, data.join("cache"));
        let bytes = b"export";
        let d = digest(bytes);
        files
            .publish("op-1.part", &mut &bytes[..], 64, &d, 6)
            .unwrap();
        let out = data.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let long = out.join(format!("{}.txt", "n".repeat(250)));
        files.export(&d, 6, &long, false).unwrap();
        assert_eq!(std::fs::read(&long).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&data);
    }
}
