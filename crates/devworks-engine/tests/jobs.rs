//! The schedule runner inside the booted runtime (DW-50): due occurrences claimed and executed
//! through the runtime's one command path, a run active at close resumed after reopen with its
//! identity while feature/issue holds stay, and a cancellation that stops the run's owned trees
//! through the process owner before its outcome is recorded. Real SQLite store, deterministic
//! wall clock, a scripted native capability and a fake process host; scheduler ticks are driven
//! explicitly and in-flight runs joined, never slept on. No inference.
mod common;

use common::*;
use devworks_adapters::fs::NativeFilesystem;
use devworks_adapters::fs::artifacts::NativeArtifactFiles;
use devworks_adapters::fs::sources::HostArtifactSources;
use devworks_core::authority::HostAuthority;
use devworks_core::authority::destructive::ProtectedRoots;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::gateway::AdmittedCommand;
use devworks_core::ports::process::{
    NativeIdentity, PowerAssertion, PowerPort, PowerRequest, Probe, ProcessError, ProcessHost,
    ProcessSpec, ProcessTree, TreeStatus,
};
use devworks_core::ports::storage::*;
use devworks_core::ports::{
    AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization, Clock, CommandHandler,
    Decided, HandlerFailure, IdKind,
};
use devworks_core::protocol::records::OperationRef;
use devworks_core::protocol::results::{DomainCommandReply, DomainDecision, OperationResult};
use devworks_core::protocol::{
    ActorId, CommandReply, Counter, Id, OperationId, OperationState, Revision, RootId,
};
use devworks_core::registry::SchemaRevision;
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::jobs::argument_schema;
use devworks_core::services::process::{LaunchPolicy, LaunchRequest};
use devworks_core::services::resources::ResourceService;
use devworks_core::services::runs::ControlService;
use devworks_engine::{Composition, RunComposition, Runtime, RuntimeConfig, boot};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

const DOCS: &str = "help.generate_capability_docs";
const PROTOCOL: &str = "1.0-draft.1";

/// Current host authority over the one store (as the desktop composes it).
struct HostAuth {
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn Clock>,
}
impl AuthorityPort for HostAuth {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.clock,
            protected_branches: &[],
        }
        .authorize(ctx, request)
    }
}

/// The scripted native capability: applied, or accepted as an asynchronous operation.
struct Docs {
    accept: AtomicBool,
    calls: AtomicUsize,
    /// Once called, the store refuses this many further writes.
    then_fail: std::sync::Mutex<Option<(Arc<Flaky>, usize)>>,
}

/// A store whose next `fail` writes are refused before admission (`Busy`, as from a full
/// writer mailbox).
struct Flaky {
    inner: Arc<dyn ApplicationStore>,
    fail: AtomicUsize,
    refused: AtomicUsize,
}
impl ApplicationStore for Flaky {
    fn info(&self) -> StoreInfo {
        self.inner.info()
    }
    fn submit_write(
        &self,
        options: WriteOptions,
        job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError> {
        if self
            .fail
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            self.refused.fetch_add(1, Ordering::SeqCst);
            return Err(StorageError::Busy);
        }
        self.inner.submit_write(options, job)
    }
    fn read(
        &self,
        options: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.inner.read(options, work)
    }
}
impl CommandHandler for Docs {
    fn decide(
        &self,
        _: &InvocationContext,
        _: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some((store, n)) = self.then_fail.lock().unwrap().take() {
            store.fail.store(n, Ordering::SeqCst);
        }
        let op = OperationId(tx.new_id(IdKind::Operation));
        if self.accept.load(Ordering::SeqCst) {
            tx.operations().record_intent(NewOperation {
                id: op.clone(),
                root_id: None,
                assignment_id: None,
                capability: DOCS.into(),
                schema_version: 1,
                args_digest: "d".repeat(64),
                payload: json!({}),
            })?;
            return Ok(Decided {
                decision: DomainDecision::Accepted {
                    operation_id: op.clone(),
                },
                operation_id: Some(op),
            });
        }
        Ok(Decided {
            decision: DomainDecision::Applied {
                result: OperationResult::HelpGenerateCapabilityDocs {
                    value: OperationRef {
                        operation_id: op,
                        state: OperationState::ObservedComplete,
                    },
                },
            },
            operation_id: None,
        })
    }
}

#[derive(Default)]
struct FakeHost {
    stopped: Arc<AtomicUsize>,
    stop_uncertain: Arc<AtomicBool>,
}
struct FakeTree {
    identity: NativeIdentity,
    gone: bool,
    stopped: Arc<AtomicUsize>,
    stop_uncertain: Arc<AtomicBool>,
}
impl ProcessTree for FakeTree {
    fn identity(&self) -> &NativeIdentity {
        &self.identity
    }
    fn request_stop(&mut self) -> Result<bool, ProcessError> {
        self.gone = !self.stop_uncertain.load(Ordering::SeqCst);
        self.stopped.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }
    fn kill(&mut self) -> Result<(), ProcessError> {
        self.gone = !self.stop_uncertain.load(Ordering::SeqCst);
        Ok(())
    }
    fn poll(&mut self) -> Result<TreeStatus, ProcessError> {
        Ok(TreeStatus {
            empty: self.gone,
            exit_code: self.gone.then_some(0),
        })
    }
    fn take_output(
        &mut self,
    ) -> Option<(Box<dyn std::io::Read + Send>, Box<dyn std::io::Read + Send>)> {
        None
    }
}
impl ProcessHost for FakeHost {
    fn platform(&self) -> &'static str {
        "unix"
    }
    fn environment(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn spawn(&self, _: &ProcessSpec) -> Result<Box<dyn ProcessTree>, ProcessError> {
        Ok(Box::new(FakeTree {
            identity: NativeIdentity {
                pid: 4242,
                birth: "b".into(),
                group: Some(4242),
            },
            gone: false,
            stopped: self.stopped.clone(),
            stop_uncertain: self.stop_uncertain.clone(),
        }))
    }
    fn probe(&self, _: &NativeIdentity) -> Probe {
        Probe::Gone
    }
    fn terminate(&self, _: &NativeIdentity) -> Probe {
        Probe::Gone
    }
}
struct NoPower;
struct Held;
impl PowerAssertion for Held {
    fn status(&mut self) -> PowerRequest {
        PowerRequest::Effective { mechanism: "fake" }
    }
}
impl PowerPort for NoPower {
    fn prevent_idle_sleep(&self) -> Result<Box<dyn PowerAssertion>, String> {
        Ok(Box::new(Held))
    }
}

fn seed(h: &Harness) {
    let c = rusqlite::Connection::open(h.root.join("workspace.sqlite")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute_batch(
        "INSERT INTO projects(id,title,lifecycle,revision,scope_revision,created_at) VALUES('project-a','A','Active',1,1,0);
         INSERT INTO work_items VALUES('root-i','project-a','issue',NULL,NULL,NULL,'root-i',1,0,1,1,1,'{}');
         INSERT INTO actors VALUES('user-a','human','User');",
    )
    .unwrap();
}

struct App {
    store: Arc<dyn ApplicationStore>,
    docs: Arc<Docs>,
    host: Arc<FakeHost>,
    runtime: Runtime,
    n: AtomicUsize,
}

fn boot_app(rt: &tokio::runtime::Runtime, h: &Harness, accept: bool) -> App {
    boot_over(
        rt,
        h,
        accept,
        h.open(),
        Arc::new(devworks_core::services::jobs::runner::NoInferencePipeline),
    )
}
fn boot_over(
    rt: &tokio::runtime::Runtime,
    h: &Harness,
    accept: bool,
    store: Arc<dyn ApplicationStore>,
    inference: Arc<dyn devworks_core::services::jobs::runner::InferenceJobs>,
) -> App {
    let docs = Arc::new(Docs {
        accept: AtomicBool::new(accept),
        calls: AtomicUsize::new(0),
        then_fail: std::sync::Mutex::new(None),
    });
    let host = Arc::new(FakeHost::default());
    let mut config = RuntimeConfig::new(id("dispatcher-1"));
    // Ticks are driven by the test; the loop's own pacing never fires within it.
    config.scheduler_interval = Duration::from_secs(3600);
    config.collect_interval = Duration::from_secs(3600);
    config.housekeeping_interval = Duration::from_secs(3600);
    config.shutdown_grace = Duration::from_millis(50);
    config.shutdown_bound = Duration::from_secs(5);
    let artifacts = Arc::new(ArtifactService::new(
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.with_extension("cache"),
        )),
        Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
        Arc::new(NativeFilesystem::new()),
        h.clock(),
        h.ids.clone(),
    ));
    let mut use_cases = UseCases::new().with_queue();
    use_cases.register(DOCS, None, Some(docs.clone()));
    let composition = Composition {
        git: None,
        browser: None,
        packages: None,
        store: store.clone(),
        clock: h.clock(),
        ids: h.ids.clone(),
        authority: Arc::new(HostAuth {
            store: store.clone(),
            clock: h.clock(),
        }),
        artifacts,
        resources: Arc::new(ResourceService::new(h.ids.clone(), h.clock())),
        process_host: host.clone(),
        runs: RunComposition {
            inference,
            ..RunComposition::new(Arc::new(ControlService::new(h.ids.clone())))
        },
        power: Arc::new(NoPower),
        diagnostics: Arc::new(
            devworks_core::services::diagnostics::DiagnosticService::new(
                store.clone(),
                h.ids.clone(),
                h.clock(),
                None,
            ),
        ),
        use_cases,
        consumers: Vec::new(),
        indexing: None,
        config,
    };
    let runtime = rt
        .block_on(boot(composition))
        .unwrap_or_else(|e| panic!("boot: {e:?}"));
    App {
        store,
        docs,
        host,
        runtime,
        n: AtomicUsize::new(0),
    }
}

impl App {
    fn user(&self) -> InvocationContext {
        let info = self.store.info();
        HostIngress::new(&*self.store).context(
            ActorId(id("user-a")),
            PrincipalKind::Human,
            id("invocation-1"),
            id("ui-main"),
            Counter::new(info.restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
    }
    fn command(&self, op: &str, mut body: Value) -> DomainCommandReply {
        body["kind"] = json!(op);
        let key = format!("key-{}", self.n.fetch_add(1, Ordering::SeqCst));
        let frame = json!({"mode": "Command", "protocol": PROTOCOL, "requestKey": key,
            "schemaRevision": 2, "body": body});
        self.runtime
            .command(&self.user(), op, frame.to_string().as_bytes())
            .expect("addressable")
    }
    fn specialized(&self, op: &str, command: Value) -> DomainCommandReply {
        let key = format!("key-{}", self.n.fetch_add(1, Ordering::SeqCst));
        let frame = json!({"protocol": PROTOCOL, "requestKey": key, "command": command});
        self.runtime
            .command(&self.user(), op, frame.to_string().as_bytes())
            .expect("addressable")
    }
    fn run(&self, run: &Id) -> StoredJobRun {
        let r = run.clone();
        read(&*self.store, move |s| s.jobs().job_run(&r)).unwrap()
    }
    fn job(&self, job: &Id) -> StoredJob {
        let j = job.clone();
        read(&*self.store, move |s| s.jobs().job(&j)).unwrap()
    }
    /// One scheduler tick, then every run it started is joined.
    fn tick(&self, rt: &tokio::runtime::Runtime) -> devworks_engine::SchedulerTick {
        let s = self.runtime.scheduler().clone();
        rt.block_on(within("a scheduler tick and its runs", async move {
            let report = s.tick().await.expect("tick");
            s.drain(tokio::time::Instant::now() + Duration::from_secs(20))
                .await;
            report
        }))
    }
}

fn value(reply: DomainCommandReply) -> Value {
    match reply {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied { result } | DomainDecision::NoChange { result } => {
                serde_json::to_value(result).unwrap()["value"].clone()
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// Quiesce the booted scheduler before driving exact cancellation steps. A command wakes its
/// loop, and stop alone does not wait for a tick that already passed its stop check.
fn cancellation_scheduler(
    rt: &tokio::runtime::Runtime,
    app: &App,
    inference: Arc<dyn devworks_core::services::jobs::runner::InferenceJobs>,
) -> Arc<devworks_engine::Scheduler> {
    let background = app.runtime.scheduler().clone();
    background.stop();
    rt.block_on(within(
        "the background scheduler stop barrier",
        async move {
            background.tick().await.expect("stopped tick barrier");
            background
                .drain(tokio::time::Instant::now() + Duration::from_secs(20))
                .await;
        },
    ));
    struct NoDispatch;
    impl devworks_core::services::jobs::runner::NativeJobDispatch for NoDispatch {
        fn command(
            &self,
            _: &InvocationContext,
            _: &str,
            _: &[u8],
        ) -> devworks_core::gateway::CommandOutcome {
            panic!("cancellation cannot dispatch a new operation")
        }
    }
    let mut scheduler = devworks_engine::Scheduler::new(
        app.store.clone(),
        app.runtime.jobs().clone(),
        Arc::new(NoDispatch),
        inference,
        app.runtime.processes().clone(),
    );
    let config = Arc::get_mut(&mut scheduler).unwrap();
    config.stop_grace = Duration::ZERO;
    config.stop_bound = Duration::from_millis(50);
    scheduler
}

fn native() -> Value {
    let schema = argument_schema(DOCS, SchemaRevision(1)).unwrap();
    json!({"kind": "Native", "capabilityId": "help-generate_capability_docs", "arguments": {
        "schema": serde_json::to_value(schema).unwrap(),
        "value": {"registryRevision": "1", "expectedHelpRevision": "1"}}})
}
fn job_body(schedule: Value, enabled: bool) -> Value {
    json!({"scope": {"kind": "Workspace"}, "title": "Capability docs", "schedule": schedule,
        "definition": native(), "enabled": enabled})
}
fn finish_inner(store: &Arc<dyn ApplicationStore>, inner: OperationId, to: OperationState) {
    write(&**store, move |tx| {
        let rec = tx.operations().operation(&inner)?.unwrap();
        if to != OperationState::ObservedComplete {
            return tx
                .operations()
                .transition(&inner, rec.revision, None, to, None);
        }
        // Completion walks the journal: attempt, dispatch, observed outcome.
        let attempt =
            tx.operations()
                .begin_attempt(&inner, rec.revision, AttemptBinding::default())?;
        let fence = attempt.fence();
        let rec = tx.operations().operation(&inner)?.unwrap();
        let rec = tx.operations().transition(
            &inner,
            rec.revision,
            Some(&fence),
            OperationState::Dispatching,
            None,
        )?;
        tx.operations()
            .transition(&inner, rec.revision, Some(&fence), to, None)
    })
    .unwrap();
}

#[test]
fn a_due_occurrence_is_claimed_and_run_once_through_the_runtime() {
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let app = boot_app(&rt, &h, false);
    let job = value(app.command(
        "jobs.create",
        job_body(json!({"kind": "Interval", "seconds": "3600"}), true),
    ));
    let jid = id(job["id"].as_str().unwrap());
    // Nothing is due yet.
    assert!(app.tick(&rt).queued.is_empty());
    h.advance(3_600_000 + 1_000);
    // The background loop may claim it first (a command woke it); either way, after a tick and
    // a join exactly one run exists and it ran once.
    app.tick(&rt);
    let runs = {
        let j = jid.clone();
        read(&*app.store, move |s| s.jobs().job_runs(&j, None, 10))
    };
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].status, "Succeeded");
    assert_eq!(runs[0].trigger, "Scheduled");
    assert_eq!(app.docs.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        app.job(&jid).next_at_ms,
        Some(runs[0].occurrence_at_ms + 3_600_000)
    );
    // Claiming again at the same instant records nothing more; it never runs twice.
    let again = app.tick(&rt);
    assert!(again.queued.is_empty() && again.started.is_empty());
    assert_eq!(app.docs.calls.load(Ordering::SeqCst), 1);
    rt.block_on(app.runtime.shutdown());
    // After shutdown began, schedule admission is frozen.
    let frozen = rt.block_on(app.runtime.scheduler().tick()).unwrap();
    assert!(frozen.queued.is_empty());
}

#[test]
fn a_run_active_at_close_resumes_after_reopen_and_feature_holds_stay() {
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let app = boot_app(&rt, &h, true);
    let job = value(app.command("jobs.create", job_body(json!({"kind": "Manual"}), true)));
    let jid = id(job["id"].as_str().unwrap());
    let run = value(app.command(
        "jobs.run_now",
        json!({"jobId": jid.as_str(), "expectedRevision": "1"}),
    ));
    let rid = id(run["id"].as_str().unwrap());
    app.tick(&rt);
    let running = app.run(&rid);
    assert_eq!(running.status, "Running");
    let inner = running
        .inner_operation_id
        .clone()
        .expect("accepted operation");
    // A person paused an issue root: its hold must survive the job's resumption.
    let paused = app.specialized(
        "runs.pause",
        json!({"kind": "runs.pause", "rootId": "root-i", "expectedControlRevision": "1"}),
    );
    assert!(
        matches!(paused, CommandReply::Recorded { .. }),
        "{paused:?}"
    );
    rt.block_on(app.runtime.shutdown());
    assert_eq!(app.job(&jid).interrupted_run_id.as_ref(), Some(&rid));
    let App { runtime, store, .. } = app;
    drop(runtime);
    drop(store);
    h.advance(86_400_000);
    let app = boot_app(&rt, &h, true);
    // Same identity, one resumption, still running on its accepted operation.
    let resumed = app.run(&rid);
    assert_eq!(
        (resumed.status.as_str(), resumed.resumptions),
        ("Running", 1)
    );
    assert_eq!(resumed.inner_operation_id.as_ref(), Some(&inner));
    assert_eq!(
        app.docs.calls.load(Ordering::SeqCst),
        0,
        "nothing is dispatched again"
    );
    let holds = read(&*app.store, |s| {
        s.runs()
            .blockers(&BlockerFilter::RootLevel(RootId(id("root-i"))), None, 10)
    });
    assert!(
        holds
            .iter()
            .any(|b| b.cause == HoldCause::HumanPause && b.state == BlockerState::Active),
        "{holds:?}"
    );
    // The operation it waits on completes: the run settles.
    finish_inner(&app.store, inner, OperationState::ObservedComplete);
    app.tick(&rt);
    assert_eq!(app.run(&rid).status, "Succeeded");
    assert!(app.job(&jid).interrupted_run_id.is_none());
    rt.block_on(app.runtime.shutdown());
}

#[test]
fn cancelling_a_dispatched_run_stops_its_trees_through_the_process_owner() {
    cancel_failed_inner(true, false);
}

#[test]
fn an_inner_failure_before_the_first_cancel_tick_still_verifies_the_process_stop() {
    cancel_failed_inner(false, false);
}

#[test]
fn an_inner_failure_after_an_uncertain_stop_never_claims_cancellation() {
    cancel_failed_inner(true, true);
}

fn cancel_failed_inner(prior_tick: bool, uncertain: bool) {
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let app = boot_app(&rt, &h, true);
    app.host.stop_uncertain.store(uncertain, Ordering::SeqCst);
    let job = value(app.command("jobs.create", job_body(json!({"kind": "Manual"}), true)));
    let jid = id(job["id"].as_str().unwrap());
    let run = value(app.command(
        "jobs.run_now",
        json!({"jobId": jid.as_str(), "expectedRevision": "1"}),
    ));
    let rid = id(run["id"].as_str().unwrap());
    app.tick(&rt);
    let scheduler = cancellation_scheduler(
        &rt,
        &app,
        Arc::new(devworks_core::services::jobs::runner::NoInferencePipeline),
    );
    let running = app.run(&rid);
    let inner = running.inner_operation_id.clone().unwrap();
    // The accepted operation launched an owned tree.
    let fs = NativeFilesystem::default();
    let roots = ProtectedRoots::new(vec![], vec![], None);
    app.runtime
        .processes()
        .launch(
            &app.user(),
            LaunchRequest {
                spec: ProcessSpec::new(&["fake-tool".to_owned()]).unwrap(),
                operation_id: Some(inner.clone()),
                lease: None,
                approved_hold: false,
            },
            &LaunchPolicy {
                fs: &fs,
                roots: &roots,
                protected_branches: &[],
            },
        )
        .expect("launched");
    let reply = app.command(
        "jobs.cancel",
        json!({"jobId": jid.as_str(), "runId": rid.as_str(), "expectedRevision": running.revision.to_string()}),
    );
    let CommandReply::Recorded { receipt, .. } = &reply else {
        panic!("{reply:?}")
    };
    assert!(matches!(receipt.decision, DomainDecision::Accepted { .. }));
    if prior_tick {
        let report = rt.block_on(scheduler.tick()).unwrap();
        // Even an uncertain stop keeps the run active until its operation ends.
        assert!(app.host.stopped.load(Ordering::SeqCst) > 0);
        assert!(report.cancelled.is_empty());
        let asked = app.run(&rid);
        assert!(asked.cancel_requested && asked.status == "Running");
    }
    finish_inner(&app.store, inner, OperationState::Failed);
    let report = rt.block_on(scheduler.tick()).unwrap();
    assert_eq!(
        report.settled, 0,
        "polling cannot bypass verified cancellation"
    );
    assert_eq!(report.cancelled, vec![rid.clone()]);
    assert!(app.host.stopped.load(Ordering::SeqCst) > 0);
    let ended = app.run(&rid);
    assert_eq!(
        ended.status,
        if uncertain {
            "OutcomeUnknown"
        } else {
            "Cancelled"
        }
    );
    assert!(ended.reason.unwrap().contains(if uncertain {
        "could not be verified stopped"
    } else {
        "verified stopped"
    }));
    app.host.stop_uncertain.store(false, Ordering::SeqCst);
    rt.block_on(app.runtime.shutdown());
}

#[test]
fn scheduled_inference_without_a_pipeline_fails_before_dispatch_in_the_runtime() {
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let app = boot_app(&rt, &h, false);
    // A workspace prompt artifact.
    write(&*app.store, |tx| {
        let digest = "a".repeat(64);
        tx.artifacts()
            .put_blob(&digest, 12, "gen-1", "blobs/aa/prompt")?;
        tx.artifacts().insert_artifact(&StoredArtifact {
            id: id("prompt-1"),
            origin_project_id: None,
            origin_work_id: None,
            title: "Scan".into(),
            revision: 1,
            scope: ArtifactScope::Workspace,
            owner: Some(ActorId(id("user-a"))),
            retention: ArtifactRetention::Explicit,
            scope_revision: 1,
            reference_revision: 1,
            current_version_id: None,
            state: ArtifactState::Live,
            created_at_ms: T0,
            provenance: json!({}),
        })?;
        tx.artifacts().insert_version(&StoredVersion {
            id: id("prompt-v1"),
            artifact_id: id("prompt-1"),
            version_no: 1,
            blob_digest: digest,
            byte_count: 12,
            blob_generation: "gen-1".into(),
            media_type: "text/markdown".into(),
            created_at_ms: T0,
            deleted_at_ms: None,
            source: json!({}),
        })
    })
    .unwrap();
    let def = json!({"kind": "Inference", "prompt": {"artifactId": "prompt-1", "versionId": "prompt-v1",
        "sha256": "a".repeat(64), "byteLength": "12", "mediaType": "text/markdown"},
        "route": {"kind": "Tier", "tier": "High"}, "effort": "Default"});
    let job = value(app.command(
        "jobs.create",
        json!({"scope": {"kind": "Workspace"}, "title": "Scan",
        "schedule": {"kind": "Manual"}, "definition": def, "enabled": true}),
    ));
    let run = value(app.command(
        "jobs.run_now",
        json!({"jobId": job["id"], "expectedRevision": "1"}),
    ));
    app.tick(&rt);
    let failed = app.run(&id(run["id"].as_str().unwrap()));
    assert_eq!(failed.status, "Failed");
    assert!(
        failed
            .reason
            .unwrap()
            .contains("Nothing was sent to a model")
    );
    assert!(failed.session_id.is_none() && failed.agent_actor.is_none());
    rt.block_on(app.runtime.shutdown());
}

/// A step of a started run that fails (the writer refusing it: `Busy`) is retried, never
/// abandoned: the run's outcome is settled within the same tick and its command ran once.
#[test]
fn a_failed_step_after_the_start_is_retried_until_it_settles() {
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let flaky = Arc::new(Flaky {
        inner: h.open(),
        fail: AtomicUsize::new(0),
        refused: AtomicUsize::new(0),
    });
    let app = boot_over(
        &rt,
        &h,
        false,
        flaky.clone(),
        Arc::new(devworks_core::services::jobs::runner::NoInferencePipeline),
    );
    let job = value(app.command("jobs.create", job_body(json!({"kind": "Manual"}), true)));
    let jid = id(job["id"].as_str().unwrap());
    // Once its command committed, the settle of its outcome is refused three times.
    *app.docs.then_fail.lock().unwrap() = Some((flaky.clone(), 3));
    let run = value(app.command(
        "jobs.run_now",
        json!({"jobId": jid.as_str(), "expectedRevision": "1"}),
    ));
    let rid = id(run["id"].as_str().unwrap());
    app.tick(&rt);
    assert_eq!(flaky.refused.load(Ordering::SeqCst), 3);
    let settled = app.run(&rid);
    assert_eq!(settled.status, "Succeeded");
    assert_eq!(app.docs.calls.load(Ordering::SeqCst), 1);
    // Settled by its own task's retried step, not reclaimed later from the command's receipt.
    let op = settled.operation_id.unwrap();
    let rec = read(&*app.store, move |s| s.operations().operation(&op)).unwrap();
    assert_eq!(rec.state, OperationState::ObservedComplete);
    let observed: Vec<Value> = rec
        .attempts
        .iter()
        .flat_map(|a| a.observations.clone())
        .collect();
    assert!(
        observed.iter().all(|o| o.get("reconciled").is_none()),
        "{observed:?}"
    );
    rt.block_on(app.runtime.shutdown());
}

/// A run whose start committed but whose task is gone (here: started outside any task) is
/// picked up by supervision: reclaimed and run, once.
#[test]
fn supervision_picks_up_a_started_run_no_task_executes() {
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let app = boot_app(&rt, &h, false);
    let job = value(app.command("jobs.create", job_body(json!({"kind": "Manual"}), true)));
    let jid = id(job["id"].as_str().unwrap());
    let run = value(app.command(
        "jobs.run_now",
        json!({"jobId": jid.as_str(), "expectedRevision": "1"}),
    ));
    let rid = id(run["id"].as_str().unwrap());
    // The scheduler has not run yet (nothing drives the runtime): start it here, with no task.
    let (svc, r) = (app.runtime.scheduler().service().clone(), rid.clone());
    let begun = write(&*app.store, move |tx| {
        svc.begin(
            tx,
            &r,
            &devworks_core::services::jobs::runner::NoInferencePipeline,
        )
    })
    .unwrap();
    assert!(begun.is_some());
    assert_eq!(app.run(&rid).status, "Running");
    app.tick(&rt);
    assert_eq!(app.run(&rid).status, "Succeeded");
    assert_eq!(app.docs.calls.load(Ordering::SeqCst), 1);
    rt.block_on(app.runtime.shutdown());
}

/// A workspace prompt artifact, and an inference definition that runs it.
fn seed_prompt(store: &Arc<dyn ApplicationStore>) {
    write(&**store, |tx| {
        let digest = "a".repeat(64);
        tx.artifacts()
            .put_blob(&digest, 12, "gen-1", "blobs/aa/prompt")?;
        tx.artifacts().insert_artifact(&StoredArtifact {
            id: id("prompt-1"),
            origin_project_id: None,
            origin_work_id: None,
            title: "Scan".into(),
            revision: 1,
            scope: ArtifactScope::Workspace,
            owner: Some(ActorId(id("user-a"))),
            retention: ArtifactRetention::Explicit,
            scope_revision: 1,
            reference_revision: 1,
            current_version_id: None,
            state: ArtifactState::Live,
            created_at_ms: T0,
            provenance: json!({}),
        })?;
        tx.artifacts().insert_version(&StoredVersion {
            id: id("prompt-v1"),
            artifact_id: id("prompt-1"),
            version_no: 1,
            blob_digest: digest,
            byte_count: 12,
            blob_generation: "gen-1".into(),
            media_type: "text/markdown".into(),
            created_at_ms: T0,
            deleted_at_ms: None,
            source: json!({}),
        })
    })
    .unwrap();
}
fn prompt_definition() -> Value {
    json!({"kind": "Inference", "prompt": {"artifactId": "prompt-1", "versionId": "prompt-v1",
        "sha256": "a".repeat(64), "byteLength": "12", "mediaType": "text/markdown"},
        "route": {"kind": "Tier", "tier": "High"}, "effort": "Default"})
}

/// A pipeline whose handoff waits until released.
#[derive(Default)]
struct SlowPipeline {
    entered: AtomicBool,
    open: std::sync::Mutex<bool>,
    opened: std::sync::Condvar,
}
impl SlowPipeline {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.opened.notify_all();
    }
}
impl devworks_core::services::jobs::runner::InferenceJobs for SlowPipeline {
    fn availability(&self) -> Result<(), String> {
        Ok(())
    }
    fn start(
        &self,
        run: &devworks_core::services::jobs::runner::InferenceStart,
    ) -> Result<Option<Id>, devworks_core::services::jobs::runner::InferenceRefusal> {
        self.entered.store(true, Ordering::SeqCst);
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.opened.wait(open).unwrap();
        }
        Ok(Some(id(&format!("session-{}", run.run_id.as_str()))))
    }
    fn cancel(&self, _: &Id) -> Result<bool, String> {
        Ok(true)
    }
}

/// While its runs are drained (their join handles taken), a tick never treats a run its task
/// still executes as abandoned: an inference handoff in progress is not reclaimed, and settles
/// once the pipeline answers.
#[test]
fn a_tick_while_runs_are_drained_never_reclaims_a_run_its_task_still_executes() {
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let pipeline = Arc::new(SlowPipeline::default());
    let app = boot_over(&rt, &h, false, h.open(), pipeline.clone());
    seed_prompt(&app.store);
    let job = value(app.command(
        "jobs.create",
        json!({"scope": {"kind": "Workspace"}, "title": "Scan", "schedule": {"kind": "Manual"},
        "definition": prompt_definition(), "enabled": true}),
    ));
    let run = value(app.command(
        "jobs.run_now",
        json!({"jobId": job["id"], "expectedRevision": "1"}),
    ));
    let rid = id(run["id"].as_str().unwrap());
    let s = app.runtime.scheduler().clone();
    let released = Arc::new(AtomicBool::new(false));
    let reclaimed = rt.block_on(within("the handoff and a tick during the drain", {
        let (pipeline, released) = (pipeline.clone(), released.clone());
        async move {
            s.tick().await.expect("tick");
            while !pipeline.entered.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let draining = tokio::spawn({
                let s = s.clone();
                async move {
                    s.drain(tokio::time::Instant::now() + Duration::from_secs(20))
                        .await
                }
            });
            // The drain has taken the run's handle and waits for it.
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            let report = s.tick().await.expect("tick");
            released.store(true, Ordering::SeqCst);
            pipeline.release();
            draining.await.unwrap();
            report.reclaimed
        }
    }));
    assert!(released.load(Ordering::SeqCst));
    assert!(reclaimed.is_empty(), "{reclaimed:?}");
    let after = app.run(&rid);
    assert_eq!(after.status, "Running");
    assert_eq!(
        after.session_id.as_ref().map(Id::as_str),
        Some(format!("session-{}", rid.as_str()).as_str())
    );
    rt.block_on(app.runtime.shutdown());
}

/// A pipeline that takes runs with a session and stops a run only when asked a second time.
#[derive(Default)]
struct SlowToStop {
    asked: AtomicUsize,
}
impl devworks_core::services::jobs::runner::InferenceJobs for SlowToStop {
    fn availability(&self) -> Result<(), String> {
        Ok(())
    }
    fn start(
        &self,
        run: &devworks_core::services::jobs::runner::InferenceStart,
    ) -> Result<Option<Id>, devworks_core::services::jobs::runner::InferenceRefusal> {
        Ok(Some(id(&format!("session-{}", run.run_id.as_str()))))
    }
    fn cancel(&self, _: &Id) -> Result<bool, String> {
        Ok(self.asked.fetch_add(1, Ordering::SeqCst) > 0)
    }
}

/// A cancellation whose agent is still stopping stays requested: the run keeps its active slot
/// (never settled as lost) until the pipeline verifies the stop, then it is cancelled.
#[test]
fn a_cancellation_still_stopping_is_asked_again_never_settled_as_lost() {
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let pipeline = Arc::new(SlowToStop::default());
    let app = boot_over(&rt, &h, false, h.open(), pipeline.clone());
    seed_prompt(&app.store);
    let job = value(app.command(
        "jobs.create",
        json!({"scope": {"kind": "Workspace"}, "title": "Scan", "schedule": {"kind": "Manual"},
        "definition": prompt_definition(), "enabled": true}),
    ));
    let jid = id(job["id"].as_str().unwrap());
    let run = value(app.command(
        "jobs.run_now",
        json!({"jobId": jid.as_str(), "expectedRevision": "1"}),
    ));
    let rid = id(run["id"].as_str().unwrap());
    app.tick(&rt);
    let scheduler = cancellation_scheduler(&rt, &app, pipeline.clone());
    let running = app.run(&rid);
    assert!(running.status == "Running" && running.session_id.is_some());
    let reply = app.command(
        "jobs.cancel",
        json!({"jobId": jid.as_str(), "runId": rid.as_str(), "expectedRevision": running.revision.to_string()}),
    );
    assert!(matches!(reply, CommandReply::Recorded { .. }), "{reply:?}");
    let report = rt.block_on(scheduler.tick()).unwrap();
    assert!(report.cancelled.is_empty());
    let stopping = app.run(&rid);
    assert_eq!(
        (stopping.status.as_str(), stopping.cancel_requested),
        ("Running", true),
        "still stopping: not settled"
    );
    let report = rt.block_on(scheduler.tick()).unwrap();
    assert_eq!(report.cancelled, vec![rid.clone()]);
    assert_eq!(app.run(&rid).status, "Cancelled");
    assert_eq!(pipeline.asked.load(Ordering::SeqCst), 2);
    rt.block_on(app.runtime.shutdown());
}

#[path = "jobs/help.rs"]
mod help;
