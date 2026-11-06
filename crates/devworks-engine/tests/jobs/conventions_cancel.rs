//! Cancellation reaches the real accepted body-index source, without touching other work.
use super::super::conventions::{DirectNative, imported_body};
use super::super::*;
use devworks_core::{
    ports::files::{ArtifactFiles, FileFault, PublishedBlob},
    protocol::provider::ArtifactRef,
};
use std::{
    io::Read,
    path::Path,
    sync::{Condvar, Weak},
};

#[derive(Clone)]
struct SourceBinding {
    store: Weak<dyn ApplicationStore>,
    job: Id,
    convention: Id,
    reference: ArtifactRef,
    blob_generation: String,
}
struct HeldSource {
    run: Id,
    child: OperationId,
    index_generation: Id,
}
/// Pauses only the verified imported body while its real Accepted index child owns it.
struct SourceVerificationGate {
    inner: NativeArtifactFiles,
    binding: Mutex<Option<SourceBinding>>,
    held: Mutex<Option<HeldSource>>,
    entered: AtomicBool,
    released: Mutex<bool>,
    wake: Condvar,
}
impl SourceVerificationGate {
    fn new(h: &Harness) -> Arc<Self> {
        Arc::new(Self {
            inner: NativeArtifactFiles::new(h.root.clone(), h.root.with_extension("cache")),
            binding: Mutex::new(None),
            held: Mutex::new(None),
            entered: AtomicBool::new(false),
            released: Mutex::new(false),
            wake: Condvar::new(),
        })
    }
    fn arm(&self, store: &Arc<dyn ApplicationStore>, job: &Id, index: &StoredConventionIndex) {
        let reference: ArtifactRef = serde_json::from_value(index.source_ref.clone()).unwrap();
        let version = read(store.as_ref(), |s| {
            s.artifacts().version(&reference.version_id)
        })
        .unwrap();
        assert_eq!(version.artifact_id, reference.artifact_id);
        assert_eq!(version.blob_digest, reference.sha256.as_str());
        assert_eq!(
            version.byte_count.to_string(),
            reference.byte_length.as_str()
        );
        assert!(version.live());
        *self.binding.lock().unwrap() = Some(SourceBinding {
            store: Arc::downgrade(store),
            job: job.clone(),
            convention: index.convention_id.clone(),
            reference,
            blob_generation: version.blob_generation,
        });
    }
    fn pause(&self, digest: &str, generation: &str, bytes: u64) {
        let Some(binding) = self.binding.lock().unwrap().clone() else {
            return;
        };
        if binding.reference.sha256.as_str() != digest
            || binding.blob_generation != generation
            || binding.reference.byte_length.as_str() != bytes.to_string()
        {
            return;
        }
        let Some(store) = binding.store.upgrade() else {
            return;
        };
        let owned = read(store.as_ref(), |s| {
            let Some(run) = s
                .jobs()
                .job_runs(&binding.job, None, 20)?
                .into_iter()
                .max_by_key(|r| r.seq)
            else {
                return Ok(None);
            };
            let Some(inner) = run.inner_operation_id.as_ref() else {
                return Ok(None);
            };
            if run.status != "Running" || run.cancel_requested || run.hold.is_some() {
                return Ok(None);
            }
            let Some(child) = s.operations().operation(inner)? else {
                return Ok(None);
            };
            if child.capability != "conventions.index"
                || child.state != OperationState::Dispatching
                || child
                    .payload
                    .pointer("/sourceContext/invocationContext/jobRun")
                    != Some(&json!(run.id))
            {
                return Ok(None);
            }
            let Some(index) = s
                .conventions()
                .convention_index_generations(Some(&binding.convention), Some(inner), None, 10)?
                .into_iter()
                .find(|i| {
                    i.state == "Draft"
                        && i.status == "Indexing"
                        && i.source_ref == json!(binding.reference)
                })
            else {
                return Ok(None);
            };
            Ok(Some(HeldSource {
                run: run.id,
                child: inner.clone(),
                index_generation: index.generation_id,
            }))
        });
        let Some(owned) = owned else { return };
        // One exact verified handle is held; other reads and later children cannot consume it.
        let mut held = self.held.lock().unwrap();
        if held.is_some() {
            return;
        }
        *held = Some(owned);
        drop(held);
        self.entered.store(true, Ordering::SeqCst);
        let guard = self.released.lock().unwrap();
        let (guard, wait) = self
            .wake
            .wait_timeout_while(guard, Duration::from_secs(20), |released| !*released)
            .unwrap();
        drop(guard);
        assert!(
            !wait.timed_out(),
            "Conventions source verification gate timed out"
        );
    }
    fn release(&self) {
        *self.released.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.wake.notify_all();
    }
}
struct ReleaseSource(Arc<SourceVerificationGate>);
impl Drop for ReleaseSource {
    fn drop(&mut self) {
        self.0.release();
    }
}
impl ArtifactFiles for SourceVerificationGate {
    fn publish(
        &self,
        n: &str,
        r: &mut dyn Read,
        d: &str,
        b: u64,
        reuse: Option<&str>,
        g: &str,
    ) -> Result<PublishedBlob, FileFault> {
        self.inner.publish(n, r, d, b, reuse, g)
    }
    fn managed_path(&self, d: &str, g: &str) -> Result<String, FileFault> {
        self.inner.managed_path(d, g)
    }
    fn present(&self, d: &str, g: &str, n: u64) -> Result<bool, FileFault> {
        self.inner.present(d, g, n)
    }
    fn inspect(&self, d: &str, g: &str) -> Result<Option<u64>, FileFault> {
        self.inner.inspect(d, g)
    }
    fn read_range(&self, d: &str, g: &str, n: u64, o: u64, m: usize) -> Result<Vec<u8>, FileFault> {
        self.inner.read_range(d, g, n, o, m)
    }
    fn open(&self, d: &str, g: &str, n: u64) -> Result<Box<dyn Read + Send>, FileFault> {
        self.inner.open(d, g, n)
    }
    fn open_controlled(
        &self,
        d: &str,
        g: &str,
        n: u64,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Box<dyn Read + Send>, FileFault> {
        let verified = self.inner.open_controlled(d, g, n, stopped)?;
        self.pause(d, g, n);
        Ok(verified)
    }
    fn read_range_controlled(
        &self,
        d: &str,
        g: &str,
        n: u64,
        o: u64,
        m: usize,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Vec<u8>, FileFault> {
        self.inner.read_range_controlled(d, g, n, o, m, stopped)
    }
    fn unlink(&self, d: &str, g: &str) -> Result<(), FileFault> {
        self.inner.unlink(d, g)
    }
    fn quarantine(&self, d: &str, g: &str) -> Result<(), FileFault> {
        self.inner.quarantine(d, g)
    }
    fn staged(&self) -> Result<Vec<String>, FileFault> {
        self.inner.staged()
    }
    fn remove_staged(&self, n: &str) -> Result<(), FileFault> {
        self.inner.remove_staged(n)
    }
    fn stored_files(&self) -> Result<Vec<(String, String)>, FileFault> {
        self.inner.stored_files()
    }
    fn write_derived(&self, k: &str, b: &[u8]) -> Result<(), FileFault> {
        self.inner.write_derived(k, b)
    }
    fn read_derived(&self, k: &str, m: usize) -> Result<Option<Vec<u8>>, FileFault> {
        self.inner.read_derived(k, m)
    }
    fn collect_derived(&self) -> Result<u64, FileFault> {
        self.inner.collect_derived()
    }
    fn export(&self, d: &str, g: &str, n: u64, p: &Path, o: bool) -> Result<(), FileFault> {
        self.inner.export(d, g, n, p, o)
    }
}

#[test]
fn cancelling_an_accepted_convention_job_preserves_previous_head_and_cancels_its_source() {
    let _native_case = crate::common::native_runtime_case();
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let gate = SourceVerificationGate::new(&h);
    let app = boot_help_with_files(&rt, &h, gate.clone());
    let owner = imported_body(&app, &rt);
    let old = read(app.store.as_ref(), |s| {
        s.conventions().convention_index_head(&owner)
    })
    .unwrap();
    let native = app
        .runtime
        .jobs()
        .catalog()
        .capabilities()
        .into_iter()
        .find(|c| c.operation == "conventions.index")
        .unwrap();
    let job = app.create("Cancel actual convention index", json!({"kind":"Manual"}), json!({"kind":"Native","capabilityId":native.capability_id,"arguments":{"schema":native.schema,"value":{}}}), true);
    let scheduler = devworks_engine::Scheduler::new(
        app.store.clone(),
        app.runtime.jobs().clone(),
        Arc::new(DirectNative(app.runtime.clone())),
        Arc::new(NoInferencePipeline),
        app.runtime.processes().clone(),
    );
    let unrelated = enqueue(
        app.store.as_ref(),
        "unrelated-index-control",
        None,
        json!({"retained":true}),
    );
    gate.arm(&app.store, &job, &old);
    let release = ReleaseSource(gate.clone());
    value(app.command(
        "jobs.run_now",
        json!({"jobId":job,"expectedRevision":"1"}),
        2,
    ));
    app.tick(&rt, &scheduler);
    rt.block_on(within("actual convention source verification", async {
        while !gate.entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    }));
    let running = app.runs(&job).pop().unwrap();
    assert_eq!(running.status, "Running");
    let inner = running.inner_operation_id.as_ref().unwrap();
    let operation = read(app.store.as_ref(), |s| s.operations().operation(inner)).unwrap();
    let held = gate.held.lock().unwrap();
    let captured = held
        .as_ref()
        .expect("the actual child owns the held source");
    assert_eq!(captured.run, running.id);
    assert_eq!(&captured.child, inner);
    assert_ne!(captured.index_generation, old.generation_id);
    assert_eq!(
        read(app.store.as_ref(), |s| s
            .conventions()
            .convention_index_generation(&captured.index_generation))
        .unwrap()
        .operation_id,
        *inner
    );
    drop(held);
    let delivery: Id = serde_json::from_value(operation.payload["deliveryId"].clone()).unwrap();
    let cancel = app.command(
        "jobs.cancel",
        json!({"jobId":job,"runId":running.id,"expectedRevision":running.revision.to_string()}),
        2,
    );
    assert!(
        matches!(
            cancel,
            CommandReply::Recorded {
                receipt: devworks_core::protocol::Receipt {
                    decision: DomainDecision::Accepted { .. },
                    ..
                },
                ..
            }
        ),
        "{cancel:?}"
    );
    assert!(
        read(app.store.as_ref(), |s| s.queue().entry(&delivery))
            .unwrap()
            .cancel_requested
    );
    assert_eq!(entry(app.store.as_ref(), &unrelated.id), unrelated);
    gate.release();
    drop(release);
    let cancelled = app.settle(&rt, &scheduler, &job);
    assert_eq!(cancelled.status, "Cancelled", "{cancelled:?}");
    let after = read(app.store.as_ref(), |s| {
        s.conventions().convention_index_head(&owner)
    })
    .unwrap();
    assert_eq!(old.generation_id, after.generation_id);
    assert_eq!(old.source_digest, after.source_digest);
    rt.block_on(app.runtime.shutdown());
}
