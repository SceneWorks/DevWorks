use crate::authority::destructive::FileIdentity;
use crate::ports::storage::StoredFileScan;
use regex::Regex;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct FileScan {
    pub scanned: i64,
    pub progress: StoredFileScan,
    pub found: i64,
}

/// Scans complete lines and the fragment at EOF. The fragment is read again on growth so
/// patterns can span appends. A match already reported reserves its bytes through its end.
/// Returns None while the file cannot be read or its scan state has not changed.
pub(super) fn scan_file(
    path: &str,
    scanned: i64,
    progress: Option<StoredFileScan>,
    pattern: &str,
) -> Option<FileScan> {
    const MAX_READ: u64 = 16 * 1024 * 1024;
    // The path was resolved under its registered root (directory canonical, no symlink);
    // it must still resolve to itself, and the opened file must still be regular.
    if std::fs::canonicalize(path).ok()? != Path::new(path)
        || !std::fs::symlink_metadata(path).ok()?.file_type().is_file()
    {
        return None;
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A FIFO swapped in during open must not stall the pass.
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let mut file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    let identity = file_identity(&file, &metadata)?;
    let len = i64::try_from(metadata.len()).ok()?;
    let previous_offset = scanned;
    let (mut scanned, mut last_match_end) = match progress {
        Some(old) if old.identity != identity || len < old.read_through => (0, None),
        // A writer may finish or overwrite the unconsumed line without growing the file.
        Some(old) if len == scanned && len == old.read_through => return None,
        Some(old) => (scanned, old.last_match_end),
        // Upgraded watchers retain their previous complete-line offset.
        None if len < scanned => (0, None),
        None => (scanned, None),
    };
    file.seek(SeekFrom::Start(scanned as u64)).ok()?;
    let mut bytes = Vec::new();
    file.take(((len - scanned) as u64).min(MAX_READ))
        .read_to_end(&mut bytes)
        .ok()?;

    let regex = Regex::new(pattern).ok()?;
    let mut found = 0;
    let end = if bytes.len() as u64 == MAX_READ {
        // Keep the read bounded even when a single line is longer than one window.
        bytes
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(bytes.len(), |i| i + 1)
    } else {
        bytes.len()
    };
    let read_through = scanned + end as i64;
    for chunk in bytes[..end].split_inclusive(|&b| b == b'\n') {
        let line = chunk.strip_suffix(b"\n");
        let complete = line.is_some() || bytes.len() as u64 == MAX_READ;
        let line = line.unwrap_or(chunk);
        let text = String::from_utf8_lossy(if complete {
            line
        } else {
            without_incomplete_utf8(line)
        });
        let previous_match_end = last_match_end;
        for matched in regex.find_iter(&text) {
            if previous_match_end.is_none_or(|end| matched.start() >= end && matched.end() > end) {
                last_match_end = Some(matched.end());
                found += 1;
            }
        }
        if complete {
            scanned += chunk.len() as i64;
            last_match_end = None;
        }
    }

    let next = StoredFileScan {
        identity,
        read_through,
        last_match_end,
    };
    (scanned != previous_offset || progress != Some(next)).then_some(FileScan {
        scanned,
        progress: next,
        found,
    })
}

/// An incomplete character at EOF may finish on the next append. Hold it out of the lossy
/// conversion until then so stored match positions keep referring to the same text.
fn without_incomplete_utf8(line: &[u8]) -> &[u8] {
    if let Some(chunk) = line.utf8_chunks().last() {
        let invalid = chunk.invalid();
        if !invalid.is_empty()
            && std::str::from_utf8(invalid).is_err_and(|error| error.error_len().is_none())
        {
            return &line[..line.len() - invalid.len()];
        }
    }
    line
}

#[cfg(unix)]
fn file_identity(_file: &File, metadata: &Metadata) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    Some(FileIdentity {
        volume: metadata.dev(),
        file: metadata.ino(),
    })
}

#[cfg(windows)]
fn file_identity(file: &File, _metadata: &Metadata) -> Option<FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the file owns this live handle and info is writable for the duration of the call.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
    (ok != 0).then_some(FileIdentity {
        volume: u64::from(info.dwVolumeSerialNumber),
        file: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    })
}
