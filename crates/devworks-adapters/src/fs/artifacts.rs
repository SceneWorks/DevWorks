//! Managed artifact files beneath the application data root ([`ArtifactFiles`]).
//!
//! Layout (all paths constructed here, never from caller input):
//!
//! * `staging/<operation>.part` — in-flight bytes of one journaled staging operation;
//! * `artifacts/sha256/<first two hex>/<digest>` — immutable content-addressed blobs;
//! * `<cache root>/artifact-previews/<preview id>` — disposable derived previews.
//!
//! Publication writes and `fsync`s the staged file, reads it back to verify length and SHA-256,
//! renames it into place and `fsync`s the destination directory (Unix; Windows has no directory
//! handle flush through std, so the rename's metadata durability rests on NTFS journaling). An
//! existing digest file is re-verified before reuse; a mismatch is reported as corruption and
//! never overwritten or served. Export copies through a flushed temporary file in the
//! destination directory and publishes it with a no-clobber hard link (or `create_new` where
//! the volume has no hard links), or an atomic rename when overwrite was explicitly requested.
use devworks_core::ports::files::{ArtifactFiles, FileFault, PublishedBlob};
use sha2::{Digest as _, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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

pub struct NativeArtifactFiles {
    data_root: PathBuf,
    cache_root: PathBuf,
    serial: AtomicU64,
    #[cfg(test)]
    pub(crate) crash_at: std::sync::Mutex<Option<Step>>,
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
#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}
#[cfg(not(unix))]
fn sync_dir(_: &Path) -> io::Result<()> {
    Ok(())
}
/// Streams a file's SHA-256 and length.
fn hash_file(path: &Path) -> io::Result<(String, u64)> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut n = 0u64;
    loop {
        let k = f.read(&mut buf)?;
        if k == 0 {
            break;
        }
        h.update(&buf[..k]);
        n += k as u64;
    }
    let digest: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok((digest, n))
}

impl NativeArtifactFiles {
    /// `data_root` is the application data root; `cache_root` the disposable cache directory.
    pub fn new(data_root: impl Into<PathBuf>, cache_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
            cache_root: cache_root.into(),
            serial: AtomicU64::new(0),
            #[cfg(test)]
            crash_at: std::sync::Mutex::new(None),
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
        if *self.crash_at.lock().unwrap() == Some(step) {
            panic!("simulated crash at {step:?}");
        }
    }

    fn verify_path(path: &Path, digest: &str, byte_count: u64) -> Result<(), FileFault> {
        let (d, n) = hash_file(path).map_err(fault)?;
        if n != byte_count || d != digest {
            return Err(FileFault::Corrupt(format!(
                "managed file {digest} does not match its recorded identity"
            )));
        }
        Ok(())
    }
}

macro_rules! step {
    ($self:ident, $s:ident) => {
        #[cfg(test)]
        $self.step(Step::$s);
    };
}

impl ArtifactFiles for NativeArtifactFiles {
    fn publish(
        &self,
        staging_name: &str,
        bytes: &[u8],
        digest: &str,
    ) -> Result<PublishedBlob, FileFault> {
        valid_name(staging_name)?;
        let dest = self.blob_path(digest)?;
        let staging_dir = self.staging();
        std::fs::create_dir_all(&staging_dir).map_err(fault)?;
        let staged = staging_dir.join(staging_name);
        {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&staged)
                .map_err(fault)?;
            f.write_all(bytes).map_err(fault)?;
            step!(self, Written);
            f.sync_all().map_err(fault)?;
        }
        Self::verify_path(&staged, digest, bytes.len() as u64)?;
        step!(self, Flushed);
        let dir = dest.parent().expect("digest directory");
        std::fs::create_dir_all(dir).map_err(fault)?;
        let reuse = |staged: &Path| -> Result<PublishedBlob, FileFault> {
            Self::verify_path(&dest, digest, bytes.len() as u64)?;
            std::fs::remove_file(staged).map_err(fault)?;
            Ok(PublishedBlob {
                relative_path: Self::relative(digest),
                reused: true,
            })
        };
        if dest.exists() {
            return reuse(&staged);
        }
        match std::fs::rename(&staged, &dest) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return reuse(&staged),
            Err(e) => return Err(fault(e)),
        }
        sync_dir(dir).map_err(fault)?;
        step!(self, Renamed);
        Ok(PublishedBlob {
            relative_path: Self::relative(digest),
            reused: false,
        })
    }

    fn present(&self, digest: &str, byte_count: u64) -> Result<bool, FileFault> {
        match std::fs::symlink_metadata(self.blob_path(digest)?) {
            Ok(m) => Ok(m.is_file() && m.len() == byte_count),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(fault(e)),
        }
    }

    fn read_range(
        &self,
        digest: &str,
        byte_count: u64,
        offset: u64,
        max: usize,
    ) -> Result<Vec<u8>, FileFault> {
        let mut f = File::open(self.blob_path(digest)?).map_err(fault)?;
        if f.metadata().map_err(fault)?.len() != byte_count {
            return Err(FileFault::Corrupt(format!(
                "managed file {digest} has the wrong length"
            )));
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
        match std::fs::remove_file(self.blob_path(digest)?) {
            Ok(()) => Ok(()),
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
        let n = self.serial.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(
            ".{}.devworks-export-{}-{n}.tmp",
            name.to_string_lossy(),
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
            std::fs::rename(&tmp, destination).map_err(fault)
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
        sync_dir(dir).map_err(fault)
    }
}
