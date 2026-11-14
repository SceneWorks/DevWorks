//! `dev-host` verifies a host's dev-host root and decides where Rust builds and temp files go.
//!
//! Every self-hosted runner and agent session keeps its disposable state under one root,
//! named by `DEV_HOST_ROOT`, on dedicated storage. The root holds a fixed layout
//! (`build/`, `t/`, `locks/`, `cache/`, ...) and a `.dev-host-root` marker written once at
//! setup. The tool never creates the root itself.
//!
//! Subcommands:
//! - `check`: verifies the root is set, on its own volume and marked ([`check`]).
//! - `env`: prints `CARGO_TARGET_DIR` (a sticky build slot, [`plan`], [`slots`]) and the temp
//!   folder variables for the current worktree.
//! - `cargo`: a backstop shim that refuses unsafe builds and fills in `CARGO_TARGET_DIR`
//!   ([`shim`]).
//! - `cleanup`: deletes idle and over-budget entries under the root, never anything in use
//!   ([`cleanup`]).
//! - `guard`: fails on new hardcoded machine paths and on agent docs that lack the dev-host
//!   items ([`guard`]).
//! - `install`: installs a released binary at a pinned version, checksum- and
//!   version-verified ([`install`]).
//!
//! Per-host values come from the environment and the optional `dev-host.conf` under the root
//! ([`config`]); the source holds no host names or paths.

pub mod check;
pub mod cleanup;
pub mod config;
pub mod guard;
pub mod install;
pub mod plan;
pub mod shim;
pub mod slots;

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

/// The variable naming the host's root.
pub const ROOT_VAR: &str = "DEV_HOST_ROOT";

/// A snapshot of environment variables. Commands read this instead of the live process
/// environment so tests can describe any environment without mutating the process.
#[derive(Clone, Debug, Default)]
pub struct Vars(BTreeMap<String, String>);

impl Vars {
    /// Captures the current process environment (non-UTF-8 entries are skipped).
    pub fn from_process() -> Self {
        std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect()
    }

    /// Returns a variable's value, treating an empty value as unset.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.0.insert(key.into(), value.into());
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for Vars {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self(
            iter.into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }
}

/// Makes `path` absolute against `cwd` and resolves it the way the operating system would
/// open it: the longest existing prefix (with its `.` and `..` still in place) is
/// canonicalized, so a `..` after a symlink leaves the symlink's target rather than the link,
/// and only the missing tail is normalized lexically. Two spellings of one location compare
/// equal.
pub fn resolve(path: &Path, cwd: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let parts: Vec<Component> = joined.components().collect();
    for split in (1..=parts.len()).rev() {
        let prefix: PathBuf = parts[..split].iter().collect();
        if let Ok(mut out) = prefix.canonicalize() {
            for part in &parts[split..] {
                match part {
                    Component::CurDir => {}
                    Component::ParentDir => {
                        out.pop();
                    }
                    other => out.push(other),
                }
            }
            return out;
        }
    }
    let mut normal = PathBuf::new();
    for component in parts {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    normal
}

/// Reports whether `path` lies at or below `root` once both are resolved.
pub fn is_under(path: &Path, root: &Path, cwd: &Path) -> bool {
    resolve(path, cwd).starts_with(resolve(root, cwd))
}

/// Whether `path` itself is a symlink (on Windows also a junction). dev-host never creates,
/// writes or deletes through a link under the root, since a link can lead off the root.
pub fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Refuses `path` when it is a symlink or junction, naming why.
pub fn refuse_link(path: &Path) -> Result<(), String> {
    if is_link(path) {
        Err(format!(
            "refusing {}: it is a symlink, and dev-host does not write through links under \
             {ROOT_VAR}; replace it with a real folder or file on the root's volume",
            path.display()
        ))
    } else {
        Ok(())
    }
}

/// Checks a path dev-host builds under `root` before anything is created or written there:
/// no existing level from `root` (exclusive) down to `path` may be a symlink or junction, and
/// `path` must resolve inside the resolved root. Returns the resolved path.
pub fn confine(path: &Path, root: &Path, cwd: &Path) -> Result<PathBuf, String> {
    let abs = |p: &Path| {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        }
    };
    let (path, root) = (abs(path), abs(root));
    let outside = || {
        format!(
            "refusing {}: it resolves outside {ROOT_VAR}={}",
            path.display(),
            root.display()
        )
    };
    let rel = path.strip_prefix(&root).map_err(|_| outside())?;
    let mut level = root.clone();
    for part in rel.components() {
        let Component::Normal(name) = part else {
            return Err(outside());
        };
        level.push(name);
        refuse_link(&level)?;
    }
    let resolved = resolve(&path, cwd);
    if resolved.starts_with(resolve(&root, cwd)) {
        Ok(resolved)
    } else {
        Err(outside())
    }
}

/// Lower-case hex SHA-256 of the parts, each terminated by a NUL so boundaries can't shift.
pub fn digest(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A uniquely named scratch folder removed on drop.
    pub struct Scratch(PathBuf);

    impl Scratch {
        pub fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir()
                .join(format!("dev-host-test-{}-{tag}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Self(dir.canonicalize().expect("canonical scratch dir"))
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use testutil::Scratch;

    #[test]
    fn empty_values_read_as_unset() {
        let vars: Vars = [("A", ""), ("B", "x")].into_iter().collect();
        assert_eq!(vars.get("A"), None);
        assert_eq!(vars.get("B"), Some("x"));
    }

    #[test]
    fn is_under_follows_dot_dot_and_missing_tails() {
        let scratch = Scratch::new("under");
        let root = scratch.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        assert!(is_under(
            &root.join("build/x/not-yet"),
            &root,
            scratch.path()
        ));
        assert!(is_under(Path::new("root/a"), &root, scratch.path()));
        assert!(!is_under(&root.join("../elsewhere"), &root, scratch.path()));
        assert!(!is_under(scratch.path(), &root, scratch.path()));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_leaves_a_symlink_target_on_dot_dot() {
        let scratch = Scratch::new("dotdot");
        let root = scratch.path().join("root");
        let outside = scratch.path().join("outside/deep");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        // The OS opens `root/link/..` as `outside`, not `root`.
        let escaped = root.join("link/../victim");
        assert_eq!(
            resolve(&escaped, scratch.path()),
            scratch.path().join("outside/victim")
        );
        assert!(!is_under(&escaped, &root, scratch.path()));
        assert!(!is_under(&root.join("link/../x/y"), &root, scratch.path()));
    }

    #[test]
    fn digest_separates_parts() {
        assert_ne!(digest(&["ab", "c"]), digest(&["a", "bc"]));
        assert_eq!(digest(&["x"]).len(), 64);
    }
}
