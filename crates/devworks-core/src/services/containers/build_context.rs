//! A deterministic digest of a container build context: every entry the engine can send, so an
//! approval of a build is bound to the exact files a Dockerfile can `COPY` or `RUN`.
//!
//! The digest covers the whole folder, including entries a `.dockerignore` would exclude. That
//! is deliberately stricter than the engine: a change to an ignored file refuses an approved
//! build too, but no file the engine sends can change unnoticed. It also covers the permission
//! bits the engine preserves for files and folders: on Unix `mode & 0o7777` (so removing a
//! script's exec bit is a change); on Windows, which has no Unix mode, the read-only attribute.
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// The most entries and bytes a reviewed build context may hold.
const MAX_ENTRIES: u64 = 50_000;
const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The digest of a build context, with its size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextDigest {
    pub sha256: String,
    pub entries: u64,
    pub bytes: u64,
}

/// Walks `root` in a fixed order (entries sorted by name, links never followed) and hashes, per
/// entry, its kind, its path relative to `root` and its content: a file's bytes, a link's target.
pub fn digest(root: &Path) -> Result<ContextDigest, &'static str> {
    let mut hasher = Sha256::new();
    let mut total = ContextDigest {
        sha256: String::new(),
        entries: 0,
        bytes: 0,
    };
    walk(root, "", &mut hasher, &mut total)?;
    total.sha256 = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(total)
}

fn walk(
    dir: &Path,
    prefix: &str,
    hasher: &mut Sha256,
    total: &mut ContextDigest,
) -> Result<(), &'static str> {
    let unreadable = "container build context unreadable";
    let mut entries = std::fs::read_dir(dir)
        .map_err(|_| unreadable)?
        .map(|entry| entry.map_err(|_| unreadable))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        total.entries += 1;
        if total.entries > MAX_ENTRIES {
            return Err("container build context too large to review");
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "container build context has a name that is not UTF-8")?;
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let kind = entry.file_type().map_err(|_| unreadable)?;
        // Length-prefixed fields, so no two different trees hash the same.
        let mut field = |tag: &[u8], bytes: &[u8]| {
            hasher.update(tag);
            hasher.update((bytes.len() as u64).to_be_bytes());
            hasher.update(bytes);
        };
        if kind.is_symlink() {
            let target = std::fs::read_link(entry.path()).map_err(|_| unreadable)?;
            field(b"L", relative.as_bytes());
            field(b"T", target.to_string_lossy().as_bytes());
        } else if kind.is_dir() {
            field(b"D", relative.as_bytes());
            field(b"M", &permissions(&entry)?.to_be_bytes());
            walk(&entry.path(), &relative, hasher, total)?;
        } else if kind.is_file() {
            field(b"F", relative.as_bytes());
            field(b"M", &permissions(&entry)?.to_be_bytes());
            let mut file = std::fs::File::open(entry.path()).map_err(|_| unreadable)?;
            let mut content = Sha256::new();
            let mut buffer = [0u8; 64 * 1024];
            let mut size = 0u64;
            loop {
                let n = file.read(&mut buffer).map_err(|_| unreadable)?;
                if n == 0 {
                    break;
                }
                size += n as u64;
                total.bytes += n as u64;
                if total.bytes > MAX_BYTES {
                    return Err("container build context too large to review");
                }
                content.update(&buffer[..n]);
            }
            hasher.update(size.to_be_bytes());
            hasher.update(content.finalize());
        } else {
            // A socket, device or pipe: the engine may read it at build time.
            return Err("container build context holds a special file");
        }
    }
    Ok(())
}

/// The permission bits an entry keeps in the build: `mode & 0o7777` on Unix; on Windows `1` for
/// a read-only entry and `0` otherwise. Read without following a link.
fn permissions(entry: &std::fs::DirEntry) -> Result<u32, &'static str> {
    let metadata = entry
        .metadata()
        .map_err(|_| "container build context unreadable")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(metadata.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        Ok(u32::from(metadata.permissions().readonly()))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_digest_changes_with_any_file_name_or_content_and_not_otherwise() {
        let root = std::env::temp_dir().join(format!(
            "devworks-build-context-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("scripts/run.sh"), "echo one").unwrap();
        std::fs::write(root.join("README"), "hi").unwrap();
        let first = super::digest(&root).unwrap();
        assert_eq!((first.entries, first.bytes), (3, 10));
        assert_eq!(super::digest(&root).unwrap(), first);
        std::fs::write(root.join("scripts/run.sh"), "echo two").unwrap();
        let edited = super::digest(&root).unwrap();
        assert_ne!(edited.sha256, first.sha256);
        std::fs::write(root.join("scripts/run.sh"), "echo one").unwrap();
        assert_eq!(super::digest(&root).unwrap(), first);
        std::fs::rename(root.join("README"), root.join("README.md")).unwrap();
        assert_ne!(super::digest(&root).unwrap().sha256, first.sha256);
        std::fs::rename(root.join("README.md"), root.join("README")).unwrap();
        assert_eq!(super::digest(&root).unwrap(), first);
        // A permission-only change is a change too.
        let script = root.join("scripts/run.sh");
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(permissions.mode() ^ 0o100);
        }
        #[cfg(not(unix))]
        permissions.set_readonly(!permissions.readonly());
        std::fs::set_permissions(&script, permissions).unwrap();
        assert_ne!(super::digest(&root).unwrap().sha256, first.sha256);
        let _ = std::fs::remove_dir_all(&root);
    }
}
