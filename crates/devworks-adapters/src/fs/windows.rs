//! Windows verified reads with held, no-delete-share handles and reparse-point refusal.
//! Destructive steps are refused: without a stable file identity from std this adapter cannot
//! preserve its identity rule.
use super::{NativeFilesystem, normal_components};
use devworks_core::authority::destructive::{
    CheckedTarget, DestructiveExecutor, DestructiveKind, RemovalOutcome, ResolveError,
};
use devworks_core::authority::reads::{ReadRefusal, ReadRequest, VerifiedBytes, VerifiedReader};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_WRITE: u32 = 0x2;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const ERROR_SHARING_VIOLATION: i32 = 32;

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

impl VerifiedReader for NativeFilesystem {
    fn read(&self, req: &ReadRequest<'_>) -> Result<VerifiedBytes, ReadRefusal> {
        let parts = normal_components(req.relative).ok_or(ReadRefusal::Escape)?;
        if req.root.identity.is_some() {
            return Err(ReadRefusal::Unsupported(
                "file identities are not available on this platform".into(),
            ));
        }
        let mut held = Vec::with_capacity(parts.len() + 1);
        let root = open_held(&req.root.location).map_err(|e| refusal(&e))?;
        let meta = root.metadata().map_err(|e| refusal(&e))?;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !meta.is_dir() {
            return Err(ReadRefusal::RootChanged);
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
            #[cfg(test)]
            self.hook();
            let mut bytes = Vec::new();
            (&handle)
                .take(req.max_bytes.saturating_add(1))
                .read_to_end(&mut bytes)
                .map_err(|e| refusal(&e))?;
            if bytes.len() as u64 > req.max_bytes {
                return Err(ReadRefusal::TooLarge {
                    limit: req.max_bytes,
                });
            }
            return Ok(VerifiedBytes {
                bytes,
                identity: None,
            });
        }
        Err(ReadRefusal::Escape)
    }
}

impl DestructiveExecutor for NativeFilesystem {
    fn execute(
        &self,
        _: &CheckedTarget,
        _: DestructiveKind,
    ) -> Result<RemovalOutcome, ResolveError> {
        Err(ResolveError::Unsupported(
            "identity-preserving destructive steps are not qualified on Windows".into(),
        ))
    }
}
