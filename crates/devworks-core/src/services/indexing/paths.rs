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
}
