use super::*;
use devworks_core::ports::files::{ArtifactFiles, FileFault, PublishedBlob};
use std::{io::Read, path::Path, sync::Condvar};

struct PublishGate {
    inner: NativeArtifactFiles,
    armed: AtomicBool,
    entered: AtomicBool,
    released: Mutex<bool>,
    wake: Condvar,
    store: Mutex<Option<std::sync::Weak<dyn ApplicationStore>>>,
}
impl PublishGate {
    fn new(h: &Harness) -> Arc<Self> {
        Arc::new(Self {
            inner: NativeArtifactFiles::new(h.root.clone(), h.root.with_extension("cache")),
            armed: AtomicBool::new(false),
            entered: AtomicBool::new(false),
            released: Mutex::new(false),
            wake: Condvar::new(),
            store: Mutex::new(None),
        })
    }
    fn arm(&self) {
        *self.released.lock().unwrap() = false;
        self.entered.store(false, Ordering::SeqCst);
        self.armed.store(true, Ordering::SeqCst);
    }
    fn pause(&self, generation: &str) {
        if !self.armed.load(Ordering::SeqCst) {
            return;
        }
        // Gateway receipts share this file port. Hold only actual Help-topic work,
        // whose physical generation is the persisted host staging operation.
        let store = self
            .store
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|s| s.upgrade());
        let Some(store) = store else { return };
        let operation = devworks_core::protocol::OperationId(Id::new(generation).unwrap());
        let help_topic = read(&*store, |s| s.operations().operation(&operation))
            .is_some_and(|op| op.payload["source"]["kind"] == "HelpTopic");
        if help_topic && self.armed.swap(false, Ordering::SeqCst) {
            self.entered.store(true, Ordering::SeqCst);
            let guard = self.released.lock().unwrap();
            let (_released_guard, wait) = self
                .wake
                .wait_timeout_while(guard, Duration::from_secs(20), |released| !*released)
                .unwrap();
            assert!(!wait.timed_out(), "Help artifact gate timed out");
        }
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}
struct Release(Arc<PublishGate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
impl ArtifactFiles for PublishGate {
    fn publish(
        &self,
        name: &str,
        source: &mut dyn Read,
        digest: &str,
        bytes: u64,
        reuse: Option<&str>,
        generation: &str,
    ) -> Result<PublishedBlob, FileFault> {
        let published = self
            .inner
            .publish(name, source, digest, bytes, reuse, generation)?;
        self.pause(&published.generation);
        Ok(published)
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
        let reader = self.inner.open_controlled(d, g, n, stopped)?;
        self.pause(g);
        Ok(reader)
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
fn cancelling_an_accepted_help_job_cancels_only_its_delivery_and_prevents_catalog_publication() {
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let gate = PublishGate::new(&h);
    let app = boot_help_with_files(&rt, &h, gate.clone());
    *gate.store.lock().unwrap() = Some(Arc::downgrade(&app.store));
    let (scheduler, _) = scheduler(&app, 0);
    let old_head = app.head();
    let registry = read(&*app.store, |s| s.packages().registry_revision());
    write(&*app.store, move |tx| {
        tx.packages().advance_registry(registry)
    })
    .unwrap();
    let unrelated = enqueue(
        &*app.store,
        "unrelated-help-control",
        None,
        json!({"retained":true}),
    );
    gate.arm();
    let release = Release(gate.clone());
    let job = app.create(
        "Cancel accepted Help generation",
        json!({"kind":"Manual"}),
        app.definition(json!({}), true),
        true,
    );
    value(app.command(
        "jobs.run_now",
        json!({"jobId":job,"expectedRevision":"1"}),
        2,
    ));
    app.tick(&rt, &scheduler);
    rt.block_on(within("Help generation reached file publication", async {
        while !gate.entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    }));
    let running = rt.block_on(within("Help job records its accepted child", async {
        loop {
            let run = app.runs(&job).into_iter().max_by_key(|r| r.seq).unwrap();
            if run.inner_operation_id.is_some() {
                break run;
            }
            tokio::task::yield_now().await;
        }
    }));
    let inner = running
        .inner_operation_id
        .clone()
        .expect("actual Accepted Help operation");
    let queue = read(&*app.store, |s| {
        s.queue().deliveries(
            &DeliveryFilter {
                consumer: None,
                operation_id: Some(inner.clone()),
            },
            None,
            10,
        )
    });
    assert_eq!(queue.entries.len(), 1);
    let delivery = queue.entries[0].id.clone();
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
        read(&*app.store, |s| s.queue().entry(&delivery))
            .unwrap()
            .cancel_requested,
        "job cancellation forwards to the actual accepted delivery"
    );
    let unrelated_after = entry(&*app.store, &unrelated.id);
    assert_eq!(
        unrelated_after, unrelated,
        "cancellation cannot reach unrelated deliveries"
    );
    gate.release();
    drop(release);
    let cancelled = app.settle(&rt, &scheduler, &job);
    assert_eq!(cancelled.status, "Cancelled");
    let after = app.head();
    assert_eq!(after.revision, old_head.revision);
    assert_eq!(after.generation_id, old_head.generation_id);
    assert_eq!(after.source_digest, old_head.source_digest);
    let operation = read(&*app.store, |s| s.operations().operation(&inner)).unwrap();
    assert!(matches!(
        operation.state,
        OperationState::Failed | OperationState::CancelledBeforeDispatch
    ));
    rt.block_on(app.runtime.shutdown());
}

#[path = "conventions_cancel.rs"]
mod conventions;
