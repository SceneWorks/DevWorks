//! Native filesystem adapter for the authority ports: target resolution for the destructive
//! preflight ([`FilesystemResolver`]), verified bounded no-follow reads ([`VerifiedReader`]) and
//! identity-checked destructive steps ([`DestructiveExecutor`]).
//!
//! * **Unix (macOS, Linux):** stable identity is `(st_dev, st_ino)`. Reads and destructive
//!   steps walk descriptor-relative from an opened directory with `openat(O_NOFOLLOW)`, check
//!   the entry with `fstatat(AT_SYMLINK_NOFOLLOW)` against the identity the preflight recorded
//!   and act with `unlinkat`/`ftruncate` on those descriptors; recursive removal never follows a
//!   link or crosses a mount point and reports what it could not remove.
//! * **Windows:** std exposes no stable file identity, so identities are absent and the
//!   preflight compares canonical paths case-insensitively. Reads open each component with
//!   `FILE_FLAG_OPEN_REPARSE_POINT` (symlinks and junctions refused by attribute) and hold every
//!   handle without `FILE_SHARE_DELETE`, so no component can be renamed or replaced during the
//!   read. Destructive steps are refused as unsupported because this adapter cannot preserve
//!   its identity rule there.
//!
//! Hard links, permissions, device files and adversarial mount interleavings beyond these
//! checks remain the platform qualification gate's work.
use devworks_core::authority::destructive::{
    EntryKind, FileIdentity, FilesystemResolver, Located, ResolveError, ResolvedPath,
};
use std::fs::Metadata;
use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

/// Hook run between opening a file and re-verifying its path (race tests only).
#[cfg(test)]
pub(crate) type Hook = Box<dyn Fn() + Send + Sync>;

/// The native filesystem adapter.
#[derive(Default)]
pub struct NativeFilesystem {
    #[cfg(test)]
    pub(crate) after_open: Option<Hook>,
}
impl NativeFilesystem {
    pub fn new() -> Self {
        Self::default()
    }
    #[cfg(test)]
    pub(crate) fn hook(&self) {
        if let Some(h) = &self.after_open {
            h();
        }
    }
}

pub(crate) fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(unix)]
pub(crate) fn identity_of(m: &Metadata) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    Some(FileIdentity {
        volume: m.dev(),
        file: m.ino(),
    })
}
#[cfg(not(unix))]
pub(crate) fn identity_of(_: &Metadata) -> Option<FileIdentity> {
    None
}

#[cfg(windows)]
fn reparse(m: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    m.file_attributes() & 0x400 != 0
}
#[cfg(not(windows))]
fn reparse(_: &Metadata) -> bool {
    false
}

fn kind_of(m: &Metadata) -> EntryKind {
    let t = m.file_type();
    if t.is_symlink() {
        EntryKind::Symlink
    } else if reparse(m) {
        EntryKind::ReparsePoint
    } else if t.is_dir() {
        EntryKind::Directory
    } else if t.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

fn expand_home(path: &str) -> Result<PathBuf, ResolveError> {
    let rest = match path {
        "~" => Some(""),
        _ => path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")),
    };
    match rest {
        None => Ok(PathBuf::from(path)),
        Some(rest) => home()
            .map(|h| h.join(rest))
            .ok_or_else(|| ResolveError::Invalid("home directory is not set".into())),
    }
}

fn absolute(path: &str, working_directory: Option<&str>) -> Result<PathBuf, ResolveError> {
    if path.is_empty() || path.contains('\0') {
        return Err(ResolveError::Invalid("empty or NUL path".into()));
    }
    let p = expand_home(path)?;
    if p.is_absolute() {
        return Ok(p);
    }
    match working_directory.map(expand_home).transpose()? {
        Some(cwd) if cwd.is_absolute() => Ok(cwd.join(p)),
        _ => Err(ResolveError::Invalid(
            "a relative target needs an absolute saved working directory".into(),
        )),
    }
}

/// Lexical normalization (only used where nothing exists to canonicalize).
fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `path` is a filesystem/volume root or a mount point.
fn volume_root(path: &Path, meta: Option<&Metadata>) -> bool {
    let Some(parent) = path.parent() else {
        return true;
    };
    match (
        meta.and_then(identity_of),
        std::fs::metadata(parent)
            .ok()
            .as_ref()
            .and_then(identity_of),
    ) {
        (Some(a), Some(b)) => a.volume != b.volume,
        _ => false,
    }
}

fn locate(path: PathBuf, meta: Option<&Metadata>, follow_meta: bool) -> Located {
    let ancestors = path
        .ancestors()
        .skip(1)
        .filter_map(|a| std::fs::metadata(a).ok().as_ref().and_then(identity_of))
        .collect();
    let is_directory = meta.is_some_and(|m| {
        if follow_meta {
            m.is_dir()
        } else {
            kind_of(m) == EntryKind::Directory
        }
    });
    Located {
        volume_root: is_directory && volume_root(&path, meta),
        identity: meta.and_then(identity_of),
        ancestors,
        is_directory,
        path,
    }
}

impl FilesystemResolver for NativeFilesystem {
    fn resolve(
        &self,
        path: &str,
        working_directory: Option<&str>,
    ) -> Result<ResolvedPath, ResolveError> {
        let joined = absolute(path, working_directory)?;
        let parent_traversal = joined.components().any(|c| c == Component::ParentDir);
        let components: Vec<Component<'_>> = joined.components().collect();
        let mut linked_ancestors = Vec::new();
        let mut prefix = PathBuf::new();
        for c in &components[..components.len().saturating_sub(1)] {
            prefix.push(c.as_os_str());
            if matches!(c, Component::Normal(_))
                && let Ok(m) = std::fs::symlink_metadata(&prefix)
                && matches!(kind_of(&m), EntryKind::Symlink | EntryKind::ReparsePoint)
            {
                linked_ancestors.push(prefix.clone());
            }
        }
        // The named entry: its ancestors resolved as the kernel would, the final component not
        // followed. A path ending in `..` or a root names a directory, which is resolved fully.
        let entry_path = match (joined.file_name(), joined.parent()) {
            (Some(name), Some(parent)) if !joined.ends_with("..") => {
                match std::fs::canonicalize(parent) {
                    Ok(p) => p.join(name),
                    Err(_) => lexical(&joined),
                }
            }
            _ => std::fs::canonicalize(&joined).unwrap_or_else(|_| lexical(&joined)),
        };
        let entry_meta = match std::fs::symlink_metadata(&entry_path) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(ResolveError::Io(e.kind().to_string())),
        };
        let entry_kind = entry_meta.as_ref().map_or(EntryKind::Missing, kind_of);
        let target = std::fs::canonicalize(&entry_path).ok().map(|t| {
            let meta = std::fs::metadata(&t).ok();
            locate(t, meta.as_ref(), true)
        });
        Ok(ResolvedPath {
            requested: path.to_owned(),
            entry: locate(entry_path, entry_meta.as_ref(), false),
            entry_kind,
            target,
            parent_traversal,
            linked_ancestors,
        })
    }

    fn expand(
        &self,
        pattern: &str,
        working_directory: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>, ResolveError> {
        let joined = absolute(pattern, working_directory)?;
        let mut bases = vec![PathBuf::new()];
        for c in joined.components() {
            let text = c.as_os_str().to_string_lossy();
            let wild = matches!(c, Component::Normal(_)) && text.contains(['*', '?', '[']);
            if !wild {
                for b in &mut bases {
                    b.push(c.as_os_str());
                }
                continue;
            }
            let mut next = Vec::new();
            for b in &bases {
                let Ok(entries) = std::fs::read_dir(b) else {
                    continue;
                };
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.') && !text.starts_with('.') {
                        continue;
                    }
                    if glob(text.as_bytes(), name.as_bytes()) {
                        next.push(b.join(&name));
                        if next.len() > limit {
                            return Err(ResolveError::TooMany { limit });
                        }
                    }
                }
            }
            next.sort();
            bases = next;
        }
        if bases.len() > limit {
            return Err(ResolveError::TooMany { limit });
        }
        Ok(bases
            .into_iter()
            .filter(|b| std::fs::symlink_metadata(b).is_ok())
            .map(|b| b.to_string_lossy().into_owned())
            .collect())
    }

    fn home(&self) -> Option<PathBuf> {
        home()
    }

    fn gitfile_target(&self, gitfile: &Path) -> Option<String> {
        use std::io::Read;
        let file = std::fs::OpenOptions::new().read(true).open(gitfile).ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        let mut head = String::new();
        file.take(4096).read_to_string(&mut head).ok()?;
        let pointer = head.lines().next()?.strip_prefix("gitdir:")?.trim();
        (!pointer.is_empty()).then(|| pointer.to_owned())
    }
}

/// A component Windows would not treat as a plain file beneath the root: an alternate data
/// stream (`name:stream`) or a reserved device name (`CON`, `NUL`, `COM1`, …, with any
/// extension and trailing dots or spaces).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn windows_unsafe_component(name: &str) -> bool {
    if name.contains(':') {
        return true;
    }
    let trimmed = name.trim_end_matches(['.', ' ']);
    let base = trimmed
        .split('.')
        .next()
        .unwrap_or(trimmed)
        .trim_end_matches(' ');
    let upper = base.to_ascii_uppercase();
    let numbered = |prefix: &str| {
        upper.strip_prefix(prefix).is_some_and(|d| {
            matches!(
                d,
                "0" | "1"
                    | "2"
                    | "3"
                    | "4"
                    | "5"
                    | "6"
                    | "7"
                    | "8"
                    | "9"
                    | "\u{b9}"
                    | "\u{b2}"
                    | "\u{b3}"
            )
        })
    };
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || numbered("COM")
        || numbered("LPT")
}

/// Glob match of one component: `*`, `?` and `[...]` classes (ranges, `!`/`^` negation).
fn glob(p: &[u8], s: &[u8]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(b'*') => (0..=s.len()).any(|i| glob(&p[1..], &s[i..])),
        Some(b'?') => !s.is_empty() && glob(&p[1..], &s[1..]),
        Some(b'[') => {
            let Some(end) = p.iter().skip(2).position(|&c| c == b']').map(|i| i + 2) else {
                return !s.is_empty() && s[0] == b'[' && glob(&p[1..], &s[1..]);
            };
            let Some(&c) = s.first() else { return false };
            let class = &p[1..end];
            let (negated, class) = match class.first() {
                Some(b'!' | b'^') => (true, &class[1..]),
                _ => (false, class),
            };
            let mut hit = false;
            let mut i = 0;
            while i < class.len() {
                if i + 2 < class.len() && class[i + 1] == b'-' {
                    hit |= class[i] <= c && c <= class[i + 2];
                    i += 3;
                } else {
                    hit |= class[i] == c;
                    i += 1;
                }
            }
            hit != negated && glob(&p[end + 1..], &s[1..])
        }
        Some(&c) => s.first() == Some(&c) && glob(&p[1..], &s[1..]),
    }
}

/// Relative paths of normal components only.
pub(crate) fn normal_components(relative: &str) -> Option<Vec<&std::ffi::OsStr>> {
    let p = Path::new(relative);
    let parts: Vec<_> = p
        .components()
        .map(|c| match c {
            Component::Normal(n) => Some(n),
            _ => None,
        })
        .collect::<Option<_>>()?;
    (!parts.is_empty() && !relative.contains('\0')).then_some(parts)
}

#[cfg(test)]
mod tests;
