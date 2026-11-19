use devworks_core::authority::destructive::FileIdentity;
use devworks_core::locks::RwLockExt;
use devworks_core::ports::files::{ArtifactFiles, FileFault, PublishedBlob};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

/// The `ArtifactFiles` methods a hook can attach to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileOp {
    Publish,
    ManagedPath,
    Present,
    Inspect,
    ReadRange,
    Open,
    OpenControlled,
    ReadRangeControlled,
    Unlink,
    Quarantine,
    Staged,
    RemoveStaged,
    StoredFiles,
    WriteDerived,
    DerivedRoot,
    RemoveDerivedRemnants,
    ReadDerived,
    CollectDerived,
    Export,
}

/// What a hook sees of the call: the operation and the arguments it has (empty or zero for the
/// ones it does not). `name` is the staging name, derived key or staged file name.
#[derive(Clone, Copy, Debug)]
pub struct Call<'a> {
    pub op: FileOp,
    pub name: &'a str,
    pub digest: &'a str,
    pub generation: &'a str,
    pub bytes: u64,
}
impl<'a> Call<'a> {
    fn of(op: FileOp) -> Self {
        Self {
            op,
            name: "",
            digest: "",
            generation: "",
            bytes: 0,
        }
    }
    fn blob(op: FileOp, digest: &'a str, generation: &'a str, bytes: u64) -> Self {
        Self {
            digest,
            generation,
            bytes,
            ..Self::of(op)
        }
    }
    fn named(op: FileOp, name: &'a str) -> Self {
        Self {
            name,
            ..Self::of(op)
        }
    }
}

type Before = Arc<dyn Fn(&Call) -> Result<(), FileFault> + Send + Sync>;
type After = Arc<dyn Fn(&Call, bool) + Send + Sync>;
type Wrap = Arc<dyn Fn(&Call, Box<dyn Read + Send>) -> Box<dyn Read + Send> + Send + Sync>;
type Check = Arc<dyn Fn(&Call) + Send + Sync>;

#[derive(Default)]
struct Hooks {
    before: HashMap<FileOp, Vec<Before>>,
    after: HashMap<FileOp, Vec<After>>,
    wrap: HashMap<FileOp, Vec<Wrap>>,
    check: HashMap<FileOp, Vec<Check>>,
    chunk: Vec<Check>,
}

/// One configurable decorator of any `ArtifactFiles` for tests that need a file store to fail,
/// wait, count or observe at a chosen call. Every method forwards to the inner store unless a
/// hook says otherwise:
///
/// - [`before`](Self::before) runs ahead of the call and may refuse it with a fault (or block);
/// - [`after`](Self::after) runs once the inner call returned, told whether it succeeded;
/// - [`wrap_reader`](Self::wrap_reader) replaces the reader `open`/`open_controlled` return;
/// - [`on_stop_check`](Self::on_stop_check) runs every time the inner store polls the `stopped`
///   control of a controlled call.
///
/// Hooks may be added while the store is in use. Every method forwards by default, the
/// controlled reads and the project-cache custody included; [`required_only`](Self::required_only)
/// and [`refuse_defaults`](Self::refuse_defaults) make those refuse like an adapter that leaves
/// the trait defaults in place.
pub struct FaultFiles {
    inner: Arc<dyn ArtifactFiles>,
    hooks: RwLock<Hooks>,
    counts: RwLock<HashMap<FileOp, Arc<AtomicUsize>>>,
}

impl FaultFiles {
    pub fn over(inner: Arc<dyn ArtifactFiles>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            hooks: RwLock::default(),
            counts: RwLock::default(),
        })
    }
    /// Like [`over`](Self::over), refusing the four optional methods (the controlled reads and
    /// the project-cache custody) as an adapter that leaves the trait defaults in place does.
    pub fn required_only(inner: Arc<dyn ArtifactFiles>) -> Arc<Self> {
        let files = Self::over(inner);
        files.refuse_defaults(&[
            FileOp::OpenControlled,
            FileOp::ReadRangeControlled,
            FileOp::DerivedRoot,
            FileOp::RemoveDerivedRemnants,
        ]);
        files
    }
    /// Makes each of `ops` that the trait gives a default (the controlled reads and the
    /// project-cache custody) refuse with that default's fault instead of reaching the inner store.
    pub fn refuse_defaults(&self, ops: &[FileOp]) {
        for &op in ops {
            let what = match op {
                FileOp::OpenControlled => "controlled blob verification is unavailable",
                FileOp::ReadRangeControlled => "controlled blob ranges are unavailable",
                FileOp::DerivedRoot | FileOp::RemoveDerivedRemnants => {
                    "project cache custody unavailable"
                }
                other => panic!("{other:?} has no default to refuse with"),
            };
            self.before(op, move |_| Err(FileFault::Unsupported(what.into())));
        }
    }
    pub fn before(
        &self,
        op: FileOp,
        hook: impl Fn(&Call) -> Result<(), FileFault> + Send + Sync + 'static,
    ) {
        self.hooks
            .write_locked()
            .before
            .entry(op)
            .or_default()
            .push(Arc::new(hook));
    }
    pub fn after(&self, op: FileOp, hook: impl Fn(&Call, bool) + Send + Sync + 'static) {
        self.hooks
            .write_locked()
            .after
            .entry(op)
            .or_default()
            .push(Arc::new(hook));
    }
    pub fn wrap_reader(
        &self,
        op: FileOp,
        hook: impl Fn(&Call, Box<dyn Read + Send>) -> Box<dyn Read + Send> + Send + Sync + 'static,
    ) {
        self.hooks
            .write_locked()
            .wrap
            .entry(op)
            .or_default()
            .push(Arc::new(hook));
    }
    pub fn on_stop_check(&self, op: FileOp, hook: impl Fn(&Call) + Send + Sync + 'static) {
        self.hooks
            .write_locked()
            .check
            .entry(op)
            .or_default()
            .push(Arc::new(hook));
    }
    /// Reads the source of every publication in chunks of at most 4 bytes and runs `hook` after
    /// the first chunk of each: the staging file then exists, partly written.
    pub fn on_source_chunk(&self, hook: impl Fn(&Call) + Send + Sync + 'static) {
        self.hooks.write_locked().chunk.push(Arc::new(hook));
    }
    /// Fails the next `times` calls of `op` with `fault` (a full volume, say).
    pub fn fail_next(&self, op: FileOp, times: usize, fault: FileFault) {
        let left = AtomicUsize::new(times);
        self.before(op, move |_| {
            match left.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)) {
                Ok(_) => Err(fault.clone()),
                Err(_) => Ok(()),
            }
        });
    }
    /// Removes every hook.
    pub fn clear(&self) {
        *self.hooks.write_locked() = Hooks::default();
    }
    /// Calls of `op` made so far (before its hooks ran).
    pub fn calls(&self, op: FileOp) -> usize {
        self.counter(op).load(Ordering::SeqCst)
    }

    fn counter(&self, op: FileOp) -> Arc<AtomicUsize> {
        if let Some(c) = self.counts.read_locked().get(&op) {
            return c.clone();
        }
        self.counts.write_locked().entry(op).or_default().clone()
    }
    fn hooks_of<T: Clone>(&self, pick: impl Fn(&Hooks) -> Option<&Vec<T>>) -> Vec<T> {
        pick(&self.hooks.read_locked()).cloned().unwrap_or_default()
    }
    fn enter(&self, call: &Call) -> Result<(), FileFault> {
        self.counter(call.op).fetch_add(1, Ordering::SeqCst);
        for hook in self.hooks_of(|h| h.before.get(&call.op)) {
            hook(call)?;
        }
        Ok(())
    }
    fn leave(&self, call: &Call, ok: bool) {
        for hook in self.hooks_of(|h| h.after.get(&call.op)) {
            hook(call, ok);
        }
    }
    fn run<T>(&self, call: Call, f: impl FnOnce() -> Result<T, FileFault>) -> Result<T, FileFault> {
        self.enter(&call)?;
        let out = f();
        self.leave(&call, out.is_ok());
        out
    }
    fn wrapped(&self, call: &Call, reader: Box<dyn Read + Send>) -> Box<dyn Read + Send> {
        self.hooks_of(|h| h.wrap.get(&call.op))
            .into_iter()
            .fold(reader, |r, hook| hook(call, r))
    }
}

impl ArtifactFiles for FaultFiles {
    fn publish(
        &self,
        staging_name: &str,
        source: &mut dyn Read,
        expected_digest: &str,
        expected_bytes: u64,
        reuse: Option<&str>,
        generation: &str,
    ) -> Result<PublishedBlob, FileFault> {
        let call = Call {
            name: staging_name,
            ..Call::blob(FileOp::Publish, expected_digest, generation, expected_bytes)
        };
        let chunk = self.hooks_of(|h| Some(&h.chunk));
        self.run(call, || {
            let mut chunked = Chunked {
                inner: source,
                hooks: chunk,
                call,
                fired: false,
            };
            let source: &mut dyn Read = if chunked.hooks.is_empty() {
                chunked.inner
            } else {
                &mut chunked
            };
            self.inner.publish(
                staging_name,
                source,
                expected_digest,
                expected_bytes,
                reuse,
                generation,
            )
        })
    }
    fn managed_path(&self, digest: &str, generation: &str) -> Result<String, FileFault> {
        self.run(
            Call::blob(FileOp::ManagedPath, digest, generation, 0),
            || self.inner.managed_path(digest, generation),
        )
    }
    fn present(&self, digest: &str, generation: &str, byte_count: u64) -> Result<bool, FileFault> {
        self.run(
            Call::blob(FileOp::Present, digest, generation, byte_count),
            || self.inner.present(digest, generation, byte_count),
        )
    }
    fn inspect(&self, digest: &str, generation: &str) -> Result<Option<u64>, FileFault> {
        self.run(Call::blob(FileOp::Inspect, digest, generation, 0), || {
            self.inner.inspect(digest, generation)
        })
    }
    fn read_range(
        &self,
        digest: &str,
        generation: &str,
        byte_count: u64,
        offset: u64,
        max: usize,
    ) -> Result<Vec<u8>, FileFault> {
        self.run(
            Call::blob(FileOp::ReadRange, digest, generation, byte_count),
            || {
                self.inner
                    .read_range(digest, generation, byte_count, offset, max)
            },
        )
    }
    fn open(
        &self,
        digest: &str,
        generation: &str,
        byte_count: u64,
    ) -> Result<Box<dyn Read + Send>, FileFault> {
        let call = Call::blob(FileOp::Open, digest, generation, byte_count);
        let reader = self.run(call, || self.inner.open(digest, generation, byte_count))?;
        Ok(self.wrapped(&call, reader))
    }
    fn open_controlled(
        &self,
        digest: &str,
        generation: &str,
        byte_count: u64,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Box<dyn Read + Send>, FileFault> {
        let call = Call::blob(FileOp::OpenControlled, digest, generation, byte_count);
        let checks = self.hooks_of(|h| h.check.get(&call.op));
        let reader = self.run(call, || {
            self.inner
                .open_controlled(digest, generation, byte_count, &mut || {
                    checks.iter().for_each(|check| check(&call));
                    stopped()
                })
        })?;
        Ok(self.wrapped(&call, reader))
    }
    fn read_range_controlled(
        &self,
        digest: &str,
        generation: &str,
        byte_count: u64,
        offset: u64,
        max: usize,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Vec<u8>, FileFault> {
        let call = Call::blob(FileOp::ReadRangeControlled, digest, generation, byte_count);
        let checks = self.hooks_of(|h| h.check.get(&call.op));
        self.run(call, || {
            self.inner.read_range_controlled(
                digest,
                generation,
                byte_count,
                offset,
                max,
                &mut || {
                    checks.iter().for_each(|check| check(&call));
                    stopped()
                },
            )
        })
    }
    fn unlink(&self, digest: &str, generation: &str) -> Result<(), FileFault> {
        self.run(Call::blob(FileOp::Unlink, digest, generation, 0), || {
            self.inner.unlink(digest, generation)
        })
    }
    fn quarantine(&self, digest: &str, generation: &str) -> Result<(), FileFault> {
        self.run(
            Call::blob(FileOp::Quarantine, digest, generation, 0),
            || self.inner.quarantine(digest, generation),
        )
    }
    fn staged(&self) -> Result<Vec<String>, FileFault> {
        self.run(Call::of(FileOp::Staged), || self.inner.staged())
    }
    fn remove_staged(&self, name: &str) -> Result<(), FileFault> {
        self.run(Call::named(FileOp::RemoveStaged, name), || {
            self.inner.remove_staged(name)
        })
    }
    fn stored_files(&self) -> Result<Vec<(String, String)>, FileFault> {
        self.run(Call::of(FileOp::StoredFiles), || self.inner.stored_files())
    }
    fn write_derived(&self, key: &str, bytes: &[u8]) -> Result<(), FileFault> {
        self.run(Call::named(FileOp::WriteDerived, key), || {
            self.inner.write_derived(key, bytes)
        })
    }
    fn derived_root(&self) -> Result<FileIdentity, FileFault> {
        self.run(Call::of(FileOp::DerivedRoot), || self.inner.derived_root())
    }
    fn remove_derived_remnants(&self, key: &str, root: FileIdentity) -> Result<(), FileFault> {
        self.run(Call::named(FileOp::RemoveDerivedRemnants, key), || {
            self.inner.remove_derived_remnants(key, root)
        })
    }
    fn read_derived(&self, key: &str, max: usize) -> Result<Option<Vec<u8>>, FileFault> {
        self.run(Call::named(FileOp::ReadDerived, key), || {
            self.inner.read_derived(key, max)
        })
    }
    fn collect_derived(&self) -> Result<u64, FileFault> {
        self.run(Call::of(FileOp::CollectDerived), || {
            self.inner.collect_derived()
        })
    }
    fn export(
        &self,
        digest: &str,
        generation: &str,
        byte_count: u64,
        destination: &Path,
        overwrite: bool,
    ) -> Result<(), FileFault> {
        self.run(
            Call::blob(FileOp::Export, digest, generation, byte_count),
            || {
                self.inner
                    .export(digest, generation, byte_count, destination, overwrite)
            },
        )
    }
}

/// A publication source read in chunks of at most 4 bytes, running the chunk hooks after the first.
struct Chunked<'a, 'c> {
    inner: &'a mut dyn Read,
    hooks: Vec<Check>,
    call: Call<'c>,
    fired: bool,
}
impl Read for Chunked<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let cap = buf.len().min(4);
        let n = self.inner.read(&mut buf[..cap])?;
        if !self.fired {
            self.fired = true;
            self.hooks.iter().for_each(|hook| hook(&self.call));
        }
        Ok(n)
    }
}
