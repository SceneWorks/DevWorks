use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A uniquely named directory that is removed, with its contents, when the guard drops (also
/// when the test panics). Creating the guard removes anything a crashed earlier run left at the
/// same path; it does not create the directory unless [`TempRoot::created`] is called.
pub struct TempRoot {
    path: PathBuf,
    extra: Vec<PathBuf>,
}
impl TempRoot {
    /// `{system temp}/{prefix}-{pid}-{n}`.
    pub fn new(prefix: &str) -> Self {
        Self::under(&std::env::temp_dir(), prefix)
    }
    /// `{base}/{prefix}-{pid}-{n}`, for a base other than the system temp directory (a test
    /// binary's `CARGO_TARGET_TMPDIR`, say).
    pub fn under(base: &Path, prefix: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self::at(base.join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        )))
    }
    /// Takes over a chosen path.
    pub fn at(path: PathBuf) -> Self {
        let _ = std::fs::remove_dir_all(&path);
        Self {
            path,
            extra: Vec::new(),
        }
    }
    /// Takes over a directory that exists already (made by something else): removed on drop
    /// like the others, but not cleared first.
    pub fn adopt(path: PathBuf) -> Self {
        Self {
            path,
            extra: Vec::new(),
        }
    }
    /// Also creates the directory.
    pub fn created(self) -> Self {
        std::fs::create_dir_all(&self.path).expect("create the temporary directory");
        self
    }
    /// Removes `path` too when the guard drops (a sibling the test derives from the root).
    pub fn and_removes(mut self, path: PathBuf) -> Self {
        self.extra.push(path);
        self
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// A path beneath the root, as text.
    pub fn text(&self, relative: &str) -> String {
        self.path.join(relative).to_string_lossy().into_owned()
    }
}
impl std::ops::Deref for TempRoot {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}
impl AsRef<Path> for TempRoot {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}
impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        for extra in &self.extra {
            let _ = std::fs::remove_dir_all(extra);
        }
    }
}
