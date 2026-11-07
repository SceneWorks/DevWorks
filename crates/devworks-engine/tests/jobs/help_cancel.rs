use super::*;
use devworks_core::ports::files::{ArtifactFiles, FileFault, PublishedBlob};
use std::{io::Read, path::Path, sync::Condvar};

#[derive(Clone)]
struct HeldPublication {
    child: OperationId,
    generation: Id,
    staging: OperationId,
    run: Id,
}
struct PublishGate {
    inner: NativeArtifactFiles,
    armed: AtomicBool,
    entered: AtomicBool,
    released: Mutex<bool>,
    wake: Condvar,
    store: Mutex<Option<std::sync::Weak<dyn ApplicationStore>>>,
    job: Mutex<Option<Id>>,
    held: Mutex<Option<HeldPublication>>,
    last_observed: Mutex<Option<Value>>,
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
            job: Mutex::new(None),
            held: Mutex::new(None),
            last_observed: Mutex::new(None),
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
        let store = self
            .store
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|s| s.upgrade());
        let Some(store) = store else { return };
        let Some(job) = self.job.lock().unwrap().clone() else {
            return;
        };
        let staging = devworks_core::protocol::OperationId(Id::new(generation).unwrap());
        let owned = read(&*store, |s| {
            let Some(staged) = s.operations().operation(&staging)? else {
                return Ok(None);
            };
            let source = &staged.payload["source"];
            *self.last_observed.lock().unwrap() = Some(
                json!({"staging":staging,"sourceKind":source["kind"],"sourceGeneration":source["generationId"],"head":s.help().help_head()?.generation_operation_id,"jobRuns":s.jobs().job_runs(&job,None,3)?.iter().map(|r|json!({"id":r.id,"state":r.status,"inner":r.inner_operation_id})).collect::<Vec<_>>() }),
            );
            if source["kind"] != "HelpTopic" {
                return Ok(None);
            }
            let Some(inner) = s.help().help_head()?.generation_operation_id else {
                return Ok(None);
            };
            let Some(owner) = s.operations().operation(&inner)? else {
                return Ok(None);
            };
            if owner.state != OperationState::Dispatching
                || source["generationId"] != owner.payload["generation"]
            {
                return Ok(None);
            }
            let Some(run) = s
                .jobs()
                .job_runs(&job, None, 20)?
                .into_iter()
                .max_by_key(|run| run.seq)
            else {
                return Ok(None);
            };
            if run.status != "Running"
                || run
                    .inner_operation_id
                    .as_ref()
                    .is_some_and(|id| id != &inner)
            {
                return Ok(None);
            }
            let captured: Value = serde_json::from_str(run.checkpoint.as_ref().unwrap()).unwrap();
            let frame: Value = serde_json::from_str(captured["frame"].as_str().unwrap()).unwrap();
            assert_eq!(frame["requestKey"], json!(run.id));
            assert_eq!(
                frame["body"]["registryRevision"].as_str().unwrap(),
                owner.payload["registryRevision"].to_string()
            );
            assert_eq!(
                frame["body"]["expectedHelpRevision"].as_str().unwrap(),
                owner.payload["expectedHelpRevision"].to_string()
            );
            Ok(Some(HeldPublication {
                child: inner,
                generation: Id::new(source["generationId"].as_str().unwrap()).unwrap(),
                staging: staging.clone(),
                run: run.id,
            }))
        });
        if let Some(owned) = owned.filter(|_| self.armed.swap(false, Ordering::SeqCst)) {
            *self.held.lock().unwrap() = Some(owned);
            self.entered.store(true, Ordering::SeqCst);
            let guard = self.released.lock().unwrap();
            let (released_guard, wait) = self
                .wake
                .wait_timeout_while(guard, Duration::from_secs(20), |released| !*released)
                .unwrap();
            // Do not poison cleanup's mutex when the unchanged watchdog fails.
            drop(released_guard);
            assert!(!wait.timed_out(), "Help artifact gate timed out");
        }
    }
    fn release(&self) {
        *self.released.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.wake.notify_all();
    }
}
struct Release(Arc<PublishGate>);
impl Drop for Release {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "HELP_CANCEL_PUBLICATION_OBSERVATION {:?}",
                self.0
                    .last_observed
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
            );
        }
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
        // The input is this new staging intent; returned backing may be reused.
        self.pause(generation);
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
    let _native_case = crate::common::native_runtime_case();
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let gate = PublishGate::new(&h);
    let app = boot_help_with_files(&rt, &h, gate.clone());
    *gate.store.lock().unwrap() = Some(Arc::downgrade(&app.store));
    let _diagnostics =
        crate::common::help_startup_diagnostics(&*app.store, app.runtime.dispatcher());
    let (scheduler, _) = scheduler(&app, 0);
    let old_head = app.head();
    // Make one real generated source unavailable. Recovery must produce new bytes
    // (the explicit source-loss notice), so cancellation can hold THIS generation's
    // fresh publication instead of an arbitrary reader of shared old backing.
    let generated = id(&format!(
        "capability-{}",
        &devworks_core::fingerprint::sha256_hex(b"help.search\0Query")[..40]
    ));
    let topic = read(&*app.store, |s| {
        s.help()
            .help_topic(old_head.generation_id.as_ref().unwrap(), &generated)
    })
    .unwrap();
    let reference: devworks_core::protocol::provider::ArtifactRef =
        serde_json::from_value(topic.content_ref.clone().unwrap()).unwrap();
    let version = read(&*app.store, |s| {
        s.artifacts().version(&reference.version_id)
    })
    .unwrap();
    gate.inner
        .unlink(reference.sha256.as_str(), &version.blob_generation)
        .unwrap();
    // This scheduled generation owns the source-loss repair. Advancing the registry here
    // would also authorize background maintenance to repair it before this job is armed.
    let unrelated = enqueue(
        &*app.store,
        "unrelated-help-control",
        None,
        json!({"retained":true}),
    );
    let job = app.create(
        "Cancel accepted Help generation",
        json!({"kind":"Manual"}),
        app.definition(json!({}), true),
        true,
    );
    *gate.job.lock().unwrap() = Some(job.clone());
    gate.arm();
    let release = Release(gate.clone());
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
    let HeldPublication {
        child: held_child,
        generation: held_generation,
        staging: held_staging,
        run,
    } = gate.held.lock().unwrap().clone().unwrap();
    // Publication is proven to belong to the async child, so its gateway acknowledgment
    // can finish independently. Wait for this exact run's persisted Accepted link.
    let running = rt.block_on(within("Help job records its accepted child", async {
        loop {
            let run = read(&*app.store, |s| s.jobs().job_run(&run)).unwrap();
            if run.inner_operation_id.is_some() {
                break run;
            }
            tokio::task::yield_now().await;
        }
    }));
    let inner = running.inner_operation_id.clone().unwrap();
    assert_eq!(
        held_child, inner,
        "the held publication belongs to this accepted child"
    );
    let owner = read(&*app.store, |s| s.operations().operation(&inner)).unwrap();
    assert_eq!(owner.payload["generation"], json!(held_generation));
    let staged = read(&*app.store, |s| s.operations().operation(&held_staging)).unwrap();
    assert_eq!(
        staged.payload["source"]["generationId"],
        json!(held_generation)
    );
    assert_ne!(held_generation, old_head.generation_id.clone().unwrap());
    eprintln!(
        "HELP_CANCEL_BOUND child={held_child:?} generation={held_generation:?} staging={held_staging:?}"
    );
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
    assert_eq!(queue.entries[0].state, QueueState::Claimed);
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
    assert_eq!(
        read(&*app.store, |s| s
            .help()
            .help_topic(&topic.generation_id, &topic.id))
        .unwrap(),
        topic,
        "cancellation keeps the prior catalog/reference metadata; the fixture removed its physical body"
    );
    let operation = read(&*app.store, |s| s.operations().operation(&inner)).unwrap();
    assert!(matches!(
        operation.state,
        OperationState::Failed | OperationState::CancelledBeforeDispatch
    ));
    rt.block_on(app.runtime.shutdown());
}

#[path = "conventions_cancel.rs"]
mod conventions;
