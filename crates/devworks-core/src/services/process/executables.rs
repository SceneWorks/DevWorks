//! The named executables a host lets process tools, dependency installs, container backends
//! and local MCP servers launch (`tools.processExecutables`).
//!
//! Only a configured command name is ever looked up, and only when a caller first selects it:
//! nothing scans PATH, so startup does no filesystem work here and a program that is not named
//! cannot be selected wherever it is installed. A name resolves to the first executable file
//! called that name in an absolute PATH directory, recorded with its canonical file. A found
//! executable is kept until the configured names or PATH change, so a binding still detects one
//! replaced after it was first resolved; a name not found is looked up again on its next use.
use crate::ports::storage::{ApplicationStore, ConfigScope, ReadOptions, read_snapshot};
use crate::settings::catalog::{PROCESS_EXECUTABLES_KEY, descriptor};
use crate::settings::model::{chain, current_layer};
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The configured names, or `None` when they cannot be read now.
pub type NameSource = Arc<dyn Fn() -> Option<Vec<String>> + Send + Sync>;
/// The search path names resolve against (the host's `PATH`).
pub type SearchPath = Arc<dyn Fn() -> Option<OsString> + Send + Sync>;

/// One resolved name: the PATH entry it was found at and that entry's canonical file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedExecutable {
    pub invoked_as: PathBuf,
    pub canonical: PathBuf,
}

#[derive(Default)]
struct Cache {
    names: Vec<String>,
    path: Option<OsString>,
    resolved: HashMap<String, ResolvedExecutable>,
}

/// Configured command names resolved lazily against PATH.
pub struct PathExecutables {
    names: NameSource,
    search_path: SearchPath,
    cache: Mutex<Cache>,
}

impl std::fmt::Debug for PathExecutables {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PathExecutables").finish_non_exhaustive()
    }
}

impl PathExecutables {
    pub fn new(names: NameSource, search_path: SearchPath) -> Self {
        Self {
            names,
            search_path,
            cache: Mutex::new(Cache::default()),
        }
    }

    /// The workspace's `tools.processExecutables`, resolved against this process's `PATH`.
    pub fn from_settings(store: Arc<dyn ApplicationStore>) -> Self {
        Self::new(
            Arc::new(move || {
                read_snapshot(&*store, &ReadOptions::default(), |snapshot| {
                    configured_names(snapshot.configuration())
                })
                .ok()
            }),
            Arc::new(|| std::env::var_os("PATH")),
        )
    }

    /// The executable `name` selects now, if it is configured and found on PATH.
    pub fn lookup(&self, name: &str) -> Option<ResolvedExecutable> {
        let path = (self.search_path)();
        let configured = (self.names)();
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        // A failed settings read keeps the names last read; none read yet selects nothing.
        let names = configured.unwrap_or_else(|| cache.names.clone());
        if cache.names != names || cache.path != path {
            *cache = Cache {
                names,
                path,
                resolved: HashMap::new(),
            };
        }
        if !cache.names.iter().any(|configured| configured == name) {
            return None;
        }
        if let Some(resolved) = cache.resolved.get(name) {
            return Some(resolved.clone());
        }
        // A miss is not kept, so a runtime installed later is found without a restart.
        let resolved = cache.path.as_ref().and_then(|path| resolve(path, name))?;
        cache.resolved.insert(name.to_owned(), resolved.clone());
        Some(resolved)
    }
}

/// Reads the configured names from the workspace layer, falling back to the catalog default.
pub fn configured_names(
    read: &dyn crate::ports::storage::ConfigurationRead,
) -> Result<Vec<String>, crate::ports::storage::StorageError> {
    let d = descriptor(PROCESS_EXECUTABLES_KEY).expect("catalog key");
    let layer = current_layer(read, &ConfigScope::Workspace)?;
    let value = chain(d, std::slice::from_ref(&layer))
        .map_err(|e| {
            crate::ports::storage::StorageError::IncompatibleSchema(format!(
                "{PROCESS_EXECUTABLES_KEY}: {e:?}"
            ))
        })?
        .into_iter()
        .next()
        .map(|e| e.value.value.0)
        .unwrap_or_else(|| d.default_value());
    Ok(value
        .as_array()
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default())
}

/// The first executable file called `name` in an absolute directory of `path`.
fn resolve(path: &std::ffi::OsStr, name: &str) -> Option<ResolvedExecutable> {
    #[cfg(windows)]
    let file = PathBuf::from(format!("{name}.exe"));
    #[cfg(not(windows))]
    let file = PathBuf::from(name);
    std::env::split_paths(path)
        .filter(|directory| directory.is_absolute())
        .find_map(|directory| {
            let invoked_as = directory.join(&file);
            let canonical = invoked_as.canonicalize().ok()?;
            executable_file(&canonical).then_some(ResolvedExecutable {
                invoked_as,
                canonical,
            })
        })
}

fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::services::process::tools::{BindingRefusal, RegisteredExecutable, ToolBindings};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("dw-executables-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn executable(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, b"#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn shared<T: Clone + Send + 'static>(
        value: &Arc<Mutex<T>>,
    ) -> Arc<dyn Fn() -> Option<T> + Send + Sync> {
        let value = value.clone();
        Arc::new(move || Some(value.lock().unwrap().clone()))
    }

    #[test]
    fn an_unlisted_executable_in_a_user_writable_path_directory_is_not_selectable() {
        // A directory this user can write, the only one on PATH, holding a planted program and
        // a link to an allowed one installed elsewhere.
        let writable = Dir::new("writable");
        let installed = Dir::new("installed");
        writable.executable("planted-tool");
        let real = installed.executable("allowed-tool-real");
        let link = writable.0.join("allowed-tool");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let names = Arc::new(Mutex::new(vec!["allowed-tool".to_owned()]));
        let path = Arc::new(Mutex::new(writable.0.clone().into_os_string()));
        let bindings = ToolBindings::new(std::iter::empty::<RegisteredExecutable>(), [])
            .unwrap()
            .with_path_executables(
                Arc::new(PathExecutables::new(shared(&names), shared(&path))),
                None,
            );
        assert_eq!(
            bindings.resolve_named_executable("planted-tool"),
            Err(BindingRefusal::UnknownExecutable)
        );
        // The allowed name is registered with its canonical file, which is what launches; the
        // child keeps the PATH entry as its argv[0].
        assert_eq!(
            bindings.resolve_launch("allowed-tool"),
            Ok((
                real.to_string_lossy().into_owned(),
                link.to_string_lossy().into_owned()
            ))
        );
        // Retargeting the link after it was resolved is refused, not followed.
        let other = installed.executable("other-tool");
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&other, &link).unwrap();
        assert_eq!(
            bindings.resolve_named_executable("allowed-tool"),
            Err(BindingRefusal::ChangedExecutable)
        );
    }

    #[test]
    fn names_and_path_are_read_on_first_use_and_follow_changes() {
        let first = Dir::new("first");
        let second = Dir::new("second");
        let tool = first.executable("tool");
        let moved = second.executable("tool");
        let reads = Arc::new(AtomicUsize::new(0));
        let names = Arc::new(Mutex::new(vec!["tool".to_owned()]));
        let path = Arc::new(Mutex::new(first.0.clone().into_os_string()));
        let counted: NameSource = {
            let (reads, names) = (reads.clone(), names.clone());
            Arc::new(move || {
                reads.fetch_add(1, Ordering::SeqCst);
                Some(names.lock().unwrap().clone())
            })
        };
        let registry = PathExecutables::new(counted, shared(&path));
        // Composing the registry reads neither the setting nor the filesystem.
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert_eq!(registry.lookup("tool").map(|r| r.canonical), Some(tool));
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        *path.lock().unwrap() = second.0.clone().into_os_string();
        assert_eq!(registry.lookup("tool").map(|r| r.canonical), Some(moved));
        // A configured name installed after a failed lookup is found on its next use.
        names.lock().unwrap().push("later".to_owned());
        assert_eq!(registry.lookup("later"), None);
        let later = second.executable("later");
        assert_eq!(registry.lookup("later").map(|r| r.canonical), Some(later));
        names.lock().unwrap().clear();
        assert_eq!(registry.lookup("tool"), None);
    }

    #[test]
    fn a_failed_settings_read_keeps_the_names_last_read() {
        let dir = Dir::new("unreadable");
        let tool = dir.executable("tool");
        let readable = Arc::new(AtomicBool::new(false));
        let path = Arc::new(Mutex::new(dir.0.clone().into_os_string()));
        let names: NameSource = {
            let readable = readable.clone();
            Arc::new(move || {
                readable
                    .load(Ordering::SeqCst)
                    .then(|| vec!["tool".to_owned()])
            })
        };
        let registry = PathExecutables::new(names, shared(&path));
        // Nothing read yet selects nothing.
        assert_eq!(registry.lookup("tool"), None);
        readable.store(true, Ordering::SeqCst);
        assert_eq!(
            registry.lookup("tool").map(|r| r.canonical),
            Some(tool.clone())
        );
        readable.store(false, Ordering::SeqCst);
        assert_eq!(registry.lookup("tool").map(|r| r.canonical), Some(tool));
    }
}
