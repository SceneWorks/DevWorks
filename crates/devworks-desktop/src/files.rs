//! The desktop crate's only way to read a local file it did not create: regular files only,
//! never blocking on or following into special files, and never reading past a bound. Picked,
//! dropped and pasted attachments, their previews and the saved window state all read here.
use std::io::Read;
use std::path::Path;

/// Opens `path` for reading only if it is a regular file, without blocking on or following
/// into anything else: the name is checked without following links, the open itself neither
/// follows a final link nor waits for a FIFO writer (Unix `O_NOFOLLOW | O_NONBLOCK`; Windows
/// `FILE_FLAG_OPEN_REPARSE_POINT`), and the opened handle is checked again, so a FIFO, device,
/// socket, directory or link swapped in between is refused before a byte is read.
pub fn open_regular(path: &Path) -> std::io::Result<(std::fs::File, u64)> {
    let not_regular =
        || std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file");
    if !std::fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(not_regular());
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: open a link itself, never its target.
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Err(not_regular());
    }
    Ok((file, meta.len()))
}

/// Reads a whole regular file of at most `max` bytes (see [`open_regular`]); a larger file, or
/// one that grows past `max` while it is read, is refused with `FileTooLarge`.
pub fn read_regular(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    let too_large = || {
        std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!("larger than {max} bytes"),
        )
    };
    let (file, len) = open_regular(path)?;
    if len > max {
        return Err(too_large());
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(max.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(too_large());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_bounded_regular_files_are_read() {
        let dir = std::env::temp_dir().join(format!("devworks-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.json");
        std::fs::write(&file, b"{}").unwrap();
        assert_eq!(read_regular(&file, 2).unwrap(), b"{}");
        assert_eq!(
            read_regular(&file, 1).unwrap_err().kind(),
            std::io::ErrorKind::FileTooLarge
        );
        assert_eq!(
            read_regular(&dir, 10).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
