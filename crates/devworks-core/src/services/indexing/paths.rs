//! Repository-relative paths and stable graph identities.
//!
//! A unit key is the repository-relative path with `/` separators, exactly as Git records it
//! (case preserved). A path is accepted only when it is non-empty UTF-8 without NUL, absolute
//! form (`/x`, `C:`, `\\server`), `.`/`..` segments or a backslash, so no key can address
//! anything outside its repository on any platform. On case-insensitive file systems two keys
//! that differ only by case name the same file; [`fold`] detects that collision.
use crate::fingerprint::sha256_hex;

/// Longest unit key (bytes); longer paths are reported and skipped.
pub const MAX_KEY_BYTES: usize = 4096;

/// Whether `p` is a safe repository-relative path.
pub fn relative_ok(p: &str) -> bool {
    if p.is_empty() || p.len() > MAX_KEY_BYTES || p.contains(['\0', '\\']) {
        return false;
    }
    if p.starts_with('/') || p.as_bytes().get(1) == Some(&b':') {
        return false;
    }
    p.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
}

/// The unit key of a safe relative path (already `/`-separated).
pub fn unit_key(p: &str) -> String {
    p.to_owned()
}

/// The case-folded form two keys collide under on a case-insensitive file system.
pub fn fold(p: &str) -> String {
    p.to_lowercase()
}

/// The stable ID of a node: one repository, unit, kind and qualified name (plus an ordinal for
/// same-named overloads) keeps its ID across generations, so an overlay replacing the unit
/// keeps references to unchanged definitions resolvable.
pub fn node_id(
    repository: &str,
    unit: &str,
    kind: &str,
    qualified: &str,
    ordinal: usize,
) -> String {
    let d = sha256_hex(
        format!("{repository}\u{0}{unit}\u{0}{kind}\u{0}{qualified}\u{0}{ordinal}").as_bytes(),
    );
    format!("n-{}", &d[..40])
}

/// The ID of a relationship within its unit.
pub fn edge_id(source: &str, kind: &str, target: &str, ordinal: usize) -> String {
    let d = sha256_hex(format!("{source}\u{0}{kind}\u{0}{target}\u{0}{ordinal}").as_bytes());
    format!("e-{}", &d[..40])
}

/// The symbolic target of an unresolved name (the name is kept in the edge payload).
pub fn symbol_id(repository: &str, name: &str) -> String {
    let d = sha256_hex(format!("{repository}\u{0}{name}").as_bytes());
    format!("sym-{}", &d[..40])
}

/// The parent directory of a unit key (`""` at the root).
pub fn parent(p: &str) -> &str {
    p.rfind('/').map_or("", |i| &p[..i])
}

/// Paths at least this long keep the Windows verbatim prefix (the file APIs need it beyond the
/// classic 260-character limit).
pub const VERBATIM_MIN: usize = 248;

/// The identity form of a native absolute path (a registration location or a worktree path),
/// so a path reaching the indexer from the file system (`canonicalize`: `\\?\C:\x`) and one from
/// Git's output (`C:/x`, `//?/C:/x`) compare equal. On Windows: `/` becomes `\`, a verbatim
/// prefix is dropped while the path is shorter than [`VERBATIM_MIN`] (`\\?\UNC\server\share`
/// becomes `\\server\share`), the drive letter is upper case and a trailing separator is
/// removed (except at a drive root). Elsewhere the path is unchanged. `windows` is the target
/// platform (`cfg!(windows)`), a parameter so every platform tests the Windows rules.
pub fn native_location(raw: &str, windows: bool) -> String {
    if !windows {
        return raw.to_owned();
    }
    let mut s = raw.replace('/', "\\");
    if let Some(rest) = s.strip_prefix("\\\\?\\UNC\\") {
        if rest.len() + 2 < VERBATIM_MIN {
            s = format!("\\\\{rest}");
        }
    } else if let Some(rest) = s.strip_prefix("\\\\?\\")
        && rest.as_bytes().get(1) == Some(&b':')
        && rest.len() < VERBATIM_MIN
    {
        s = rest.to_owned();
    }
    // The drive letter: `c:\x` and `C:\x` name one directory.
    let drive_at = if s.starts_with("\\\\?\\") { 4 } else { 0 };
    if s.as_bytes().get(drive_at + 1) == Some(&b':')
        && let Some(c) = s.get(drive_at..drive_at + 1)
    {
        let upper = c.to_ascii_uppercase();
        s.replace_range(drive_at..drive_at + 1, &upper);
    }
    while s.len() > 3 && s.ends_with('\\') && !s.ends_with(":\\") {
        s.pop();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_contained_relative_paths_are_units() {
        for ok in ["a.rs", "src/lib.rs", "a b/ü.py", "x/.hidden/y"] {
            assert!(relative_ok(ok), "{ok}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a/./b",
            "C:/x",
            "c:x",
            "a\\b",
            "a//b",
            "a/",
            "x\0y",
        ] {
            assert!(!relative_ok(bad), "{bad:?}");
        }
        assert!(!relative_ok(&"a/".repeat(3000)));
        assert_eq!(fold("Src/Lib.RS"), fold("src/lib.rs"));
    }

    #[test]
    fn identities_are_stable_and_distinct() {
        let a = node_id("r", "src/a.rs", "Function", "main", 0);
        assert_eq!(a, node_id("r", "src/a.rs", "Function", "main", 0));
        assert_ne!(a, node_id("r", "src/b.rs", "Function", "main", 0));
        assert_ne!(a, node_id("r", "src/a.rs", "Function", "main", 1));
        assert!(a.len() <= 128 && symbol_id("r", "x").starts_with("sym-"));
        assert_eq!(parent("a/b/c.rs"), "a/b");
        assert_eq!(parent("c.rs"), "");
    }

    #[test]
    fn native_locations_compare_equal_across_their_windows_spellings() {
        let want = r"C:\Users\runneradmin\AppData\Local\Temp\src\app";
        for spelling in [
            r"\\?\C:\Users\runneradmin\AppData\Local\Temp\src\app",
            "C:/Users/runneradmin/AppData/Local/Temp/src/app",
            "//?/C:/Users/runneradmin/AppData/Local/Temp/src/app",
            r"c:\Users\runneradmin\AppData\Local\Temp\src\app\",
            want,
        ] {
            assert_eq!(native_location(spelling, true), want, "{spelling}");
        }
        assert_eq!(
            native_location(r"\\?\UNC\srv\share\r", true),
            r"\\srv\share\r"
        );
        assert_eq!(native_location("C:/", true), r"C:\");
        // Beyond the classic limit the verbatim form stays (the file APIs need it).
        let long = format!(r"\\?\C:\{}", "a".repeat(VERBATIM_MIN));
        assert_eq!(native_location(&long, true), long);
        assert_eq!(
            native_location(&long.replace('\\', "/"), true),
            long,
            "Git's spelling of a long path"
        );
        // Elsewhere a path is its own identity.
        for p in ["/private/var/x", "/a/b/", r"C:\x"] {
            assert_eq!(native_location(p, false), p);
        }
    }
}
