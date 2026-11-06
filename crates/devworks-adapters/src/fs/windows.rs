//! Windows verified reads with held, no-delete-share handles and reparse-point refusal.
//! Read-only identities use native handles. Destructive steps remain unqualified and refused.
use super::{NativeFilesystem, normal_components};
use devworks_core::authority::destructive::{
    CheckedTarget, DestructiveExecutor, DestructiveKind, FileIdentity, RemovalOutcome, ResolveError,
};
use devworks_core::authority::reads::{
    ReadRefusal, ReadRequest, RewindRead, VerifiedBytes, VerifiedFile, VerifiedReader,
    VerifiedRewindFile,
};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;

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
impl HandleInformation {
    fn identity(&self) -> FileIdentity {
        FileIdentity {
            volume: u64::from(self.volume_serial),
            file: (u64::from(self.index_high) << 32) | u64::from(self.index_low),
        }
    }
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetFileInformationByHandle(
        file: *mut core::ffi::c_void,
        info: *mut HandleInformation,
    ) -> i32;
}
/// Link count and identity of an open handle.
fn handle_information(file: &File) -> io::Result<HandleInformation> {
    let mut info = HandleInformation::default();
    // SAFETY: a live handle owned by `file` and a valid, correctly laid out out pointer.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}

const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_WRITE: u32 = 0x2;
const FILE_SHARE_DELETE: u32 = 0x4;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const ERROR_SHARING_VIOLATION: i32 = 32;

/// Snapshot identity without retaining a lock or requiring file-content read access.
pub(super) fn path_identity(path: &Path) -> Option<FileIdentity> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .ok()?;
    Some(handle_information(&file).ok()?.identity())
}

/// Opens the object itself (never a reparse target) and holds it without delete sharing, so it
/// cannot be renamed or replaced while held.
fn open_held(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

fn refusal(e: &io::Error) -> ReadRefusal {
    match e.kind() {
        io::ErrorKind::NotFound => ReadRefusal::NotFound,
        _ if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => ReadRefusal::Changed,
        k => ReadRefusal::Io(k.to_string()),
    }
}

/// The opened file with every ancestor handle still held (no component can be renamed or
/// replaced while it is read).
struct HeldFile {
    _ancestors: Vec<File>,
    file: io::Take<File>,
    limit: u64,
}
impl Read for HeldFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl RewindRead for HeldFile {
    fn rewind(&mut self) -> io::Result<()> {
        std::io::Seek::rewind(self.file.get_mut())?;
        self.file.set_limit(self.limit);
        Ok(())
    }
}

impl VerifiedReader for NativeFilesystem {
    fn read(&self, req: &ReadRequest<'_>) -> Result<VerifiedBytes, ReadRefusal> {
        let (mut held, identity) = self.open_verified(req)?;
        let mut bytes = Vec::new();
        held.read_to_end(&mut bytes).map_err(|e| refusal(&e))?;
        if bytes.len() as u64 > req.max_bytes {
            return Err(ReadRefusal::TooLarge {
                limit: req.max_bytes,
            });
        }
        Ok(VerifiedBytes {
            bytes,
            identity: Some(identity),
        })
    }

    fn open_rewindable(&self, req: &ReadRequest<'_>) -> Result<VerifiedRewindFile, ReadRefusal> {
        let (held, identity) = self.open_verified(req)?;
        Ok(VerifiedRewindFile {
            reader: Box::new(held),
            identity: Some(identity),
        })
    }
    fn open(&self, req: &ReadRequest<'_>) -> Result<VerifiedFile, ReadRefusal> {
        let (held, identity) = self.open_verified(req)?;
        Ok(VerifiedFile {
            reader: Box::new(held),
            identity: Some(identity),
        })
    }
}

impl NativeFilesystem {
    fn open_verified(
        &self,
        req: &ReadRequest<'_>,
    ) -> Result<(HeldFile, FileIdentity), ReadRefusal> {
        let parts = normal_components(req.relative).ok_or(ReadRefusal::Escape)?;
        // Alternate data streams and reserved device names are not files beneath the root.
        if parts
            .iter()
            .any(|p| super::windows_unsafe_component(&p.to_string_lossy()))
        {
            return Err(ReadRefusal::Escape);
        }
        let mut held = Vec::with_capacity(parts.len() + 1);
        let root = open_held(&req.root.location).map_err(|e| refusal(&e))?;
        let meta = root.metadata().map_err(|e| refusal(&e))?;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !meta.is_dir() {
            return Err(ReadRefusal::RootChanged);
        }
        if let Some(expected) = req.root.identity {
            let actual = handle_information(&root)
                .map_err(|e| refusal(&e))?
                .identity();
            if actual != expected {
                return Err(ReadRefusal::RootChanged);
            }
        }
        held.push(root);
        let mut path = req.root.location.clone();
        let last = parts.len() - 1;
        for (i, part) in parts.iter().enumerate() {
            path.push(part);
            let handle = open_held(&path).map_err(|e| refusal(&e))?;
            let meta = handle.metadata().map_err(|e| refusal(&e))?;
            if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(ReadRefusal::LinkRefused);
            }
            if i < last {
                if !meta.is_dir() {
                    return Err(ReadRefusal::NotFound);
                }
                held.push(handle);
                continue;
            }
            if !meta.is_file() {
                return Err(ReadRefusal::NotRegularFile);
            }
            let info = handle_information(&handle).map_err(|e| refusal(&e))?;
            if info.links > 1 && !req.allow_hard_links {
                return Err(ReadRefusal::HardLinked);
            }
            let identity = info.identity();
            #[cfg(test)]
            self.hook();
            let file = handle.take(req.max_bytes.saturating_add(1));
            return Ok((
                HeldFile {
                    _ancestors: held,
                    file,
                    limit: req.max_bytes.saturating_add(1),
                },
                identity,
            ));
        }
        Err(ReadRefusal::Escape)
    }
}

impl DestructiveExecutor for NativeFilesystem {
    fn execute(
        &self,
        _: &CheckedTarget,
        _: DestructiveKind,
        _: &[FileIdentity],
    ) -> Result<RemovalOutcome, ResolveError> {
        Err(ResolveError::Unsupported(
            "identity-preserving destructive steps are not qualified on Windows".into(),
        ))
    }
}
