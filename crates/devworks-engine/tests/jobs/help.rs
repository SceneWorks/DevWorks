//! Native Help scheduling over real SQLite/runtime/gateway, with no inference or network.
use super::*;
use devworks_core::services::jobs::runner::{NativeJobDispatch, NoInferencePipeline};
use std::sync::Mutex;

struct HelpApp {
    store: Arc<dyn ApplicationStore>,
    runtime: Arc<Runtime>,
    n: AtomicUsize,
}
fn boot_help(rt: &tokio::runtime::Runtime, h: &Harness) -> HelpApp {
    boot_help_with_files(
        rt,
        h,
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.with_extension("cache"),
        )),
    )
}
fn boot_help_with_files(
    rt: &tokio::runtime::Runtime,
    h: &Harness,
    files: Arc<dyn devworks_core::ports::files::ArtifactFiles>,
) -> HelpApp {
    let store = h.open();
    let mut config = RuntimeConfig::new(id("help-job-dispatcher"));
    config.scheduler_interval = Duration::from_secs(3600);
    config.collect_interval = Duration::from_secs(3600);
    config.housekeeping_interval = Duration::from_secs(3600);
    config.shutdown_grace = Duration::from_millis(50);
    config.shutdown_bound = Duration::from_secs(5);
    let artifacts = Arc::new(ArtifactService::new(
        files,
        Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
        Arc::new(NativeFilesystem::new()),
        h.clock(),
        h.ids.clone(),
    ));
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
        process_host: Arc::new(FakeHost::default()),
        runs: RunComposition::new(Arc::new(ControlService::new(h.ids.clone()))),
        power: Arc::new(NoPower),
        diagnostics: Arc::new(
            devworks_core::services::diagnostics::DiagnosticService::new(
                store.clone(),
                h.ids.clone(),
                h.clock(),
                None,
            ),
        ),
        use_cases: UseCases::new().with_queue(),
        consumers: vec![],
        indexing: None,
        config,
    };
    let runtime = Arc::new(
        rt.block_on(boot(composition))
            .expect("native Help composition"),
    );
    runtime.scheduler().stop();
    rt.block_on(async {
        runtime.scheduler().tick().await.unwrap();
        runtime
            .scheduler()
            .drain(tokio::time::Instant::now() + Duration::from_secs(10))
            .await;
        let _diagnostics = crate::common::help_startup_diagnostics(&*store, runtime.dispatcher());
        within("native Help startup generation", async {
            loop {
                runtime.dispatcher().run_until_idle().await.unwrap();
                let ready = read(&*store, |s| {
                    let head = s.help().help_head()?;
                    let ended = match head.generation_operation_id.as_ref() {
                        Some(op) => s
                            .operations()
                            .operation(op)?
                            .is_some_and(|o| is_terminal(o.state)),
                        None => false,
                    };
                    Ok(head.generation_id.is_some() && ended)
                });
                if ready {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
    });
    HelpApp {
        store,
        runtime,
        n: AtomicUsize::new(0),
    }
}
impl HelpApp {
    fn user(&self) -> InvocationContext {
        HostIngress::new(&*self.store).context(
            ActorId(id("user-a")),
            PrincipalKind::Human,
            id("help-job-human"),
            id("help-job-ui"),
            Counter::new(self.store.info().restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
    }
    fn command(&self, operation: &str, mut body: Value, revision: u32) -> DomainCommandReply {
        body["kind"] = json!(operation);
        let frame = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":revision,
            "requestKey":format!("help-job-{}",self.n.fetch_add(1,Ordering::SeqCst)),"body":body});
        self.runtime
            .command(
                &self.user(),
                operation,
                &serde_json::to_vec(&frame).unwrap(),
            )
            .unwrap()
    }
    fn definition(&self, args: Value, empty_adapter: bool) -> Value {
        let schema = if empty_adapter {
            self.runtime
                .jobs()
                .catalog()
                .capabilities()
                .into_iter()
                .find(|c| c.operation == DOCS)
                .expect("native Help qualified")
                .schema
        } else {
            argument_schema(DOCS, SchemaRevision(1)).unwrap()
        };
        json!({"kind":"Native","capabilityId":"help-generate_capability_docs","arguments":{"schema":schema,"value":args}})
    }
    fn create(&self, title: &str, schedule: Value, definition: Value, enabled: bool) -> Id {
        let job=value(self.command("jobs.create",json!({"scope":{"kind":"Workspace"},"title":title,"schedule":schedule,"definition":definition,"enabled":enabled}),2));
        id(job["id"].as_str().unwrap())
    }
    fn runs(&self, job: &Id) -> Vec<StoredJobRun> {
        read(&*self.store, |s| s.jobs().job_runs(job, None, 20))
    }
    fn head(&self) -> StoredHelpHead {
        read(&*self.store, |s| s.help().help_head())
    }
    fn tick(&self, rt: &tokio::runtime::Runtime, scheduler: &Arc<devworks_engine::Scheduler>) {
        rt.block_on(within("Help job tick", async {
            scheduler.tick().await.unwrap();
            scheduler
                .drain(tokio::time::Instant::now() + Duration::from_secs(10))
                .await;
        }));
    }
    fn settle(
        &self,
        rt: &tokio::runtime::Runtime,
        scheduler: &Arc<devworks_engine::Scheduler>,
        job: &Id,
    ) -> StoredJobRun {
        rt.block_on(within("Help generation and job settlement", async {
            loop {
                self.runtime.dispatcher().run_until_idle().await.unwrap();
                scheduler.tick().await.unwrap();
                scheduler
                    .drain(tokio::time::Instant::now() + Duration::from_secs(10))
                    .await;
                let run = self
                    .runs(job)
                    .into_iter()
                    .max_by_key(|run| run.seq)
                    .expect("scheduled run");
                if !matches!(run.status.as_str(), "Queued" | "Running" | "Holding") {
                    return run;
                }
                tokio::task::yield_now().await;
            }
        }))
    }
}
struct Native {
    runtime: Arc<Runtime>,
    store: Arc<dyn ApplicationStore>,
    unknown: AtomicUsize,
    lose_committed: AtomicBool,
    receipts: Mutex<Vec<(Option<OperationId>, bool)>>,
    frames: Mutex<Vec<(Vec<u8>, Id)>>,
}
impl NativeJobDispatch for Native {
    fn command(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
    ) -> devworks_core::gateway::CommandOutcome {
        assert_eq!(operation, DOCS);
        let frame: Value = serde_json::from_slice(raw).unwrap();
        let run_id = id(frame["requestKey"].as_str().unwrap());
        let run = read(&*self.store, |s| s.jobs().job_run(&run_id)).unwrap();
        if frame["body"].get("registryRevision").is_some()
            && let Some(checkpoint) = &run.checkpoint
        {
            let saved: Value = serde_json::from_str(checkpoint).unwrap();
            assert_eq!(
                saved["frame"].as_str().unwrap().as_bytes(),
                raw,
                "capture commits before gateway admission"
            );
            assert_eq!(saved["invocationId"], json!(ctx.invocation_id()));
        }
        self.frames
            .lock()
            .unwrap()
            .push((raw.into(), ctx.invocation_id().clone()));
        if self
            .unknown
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Ok(serde_json::from_value(json!({"kind":"CommandStatusUnknown","protocol":PROTOCOL,"requestKey":run_id,"correlationId":"lost-help-job-ack"})).unwrap());
        }
        let reply = self.runtime.command(ctx, operation, raw).unwrap();
        if let CommandReply::Recorded {
            receipt, replayed, ..
        } = &reply
        {
            self.receipts
                .lock()
                .unwrap()
                .push((receipt.operation_id.clone(), *replayed));
        }
        if self.lose_committed.swap(false, Ordering::SeqCst) {
            return Ok(serde_json::from_value(json!({"kind":"CommandStatusUnknown","protocol":PROTOCOL,"requestKey":run_id,"correlationId":"lost-committed-help-ack"})).unwrap());
        }
        Ok(reply)
    }
}
fn scheduler(app: &HelpApp, unknown: usize) -> (Arc<devworks_engine::Scheduler>, Arc<Native>) {
    let native = Arc::new(Native {
        runtime: app.runtime.clone(),
        store: app.store.clone(),
        unknown: AtomicUsize::new(unknown),
        lose_committed: AtomicBool::new(false),
        receipts: Mutex::new(vec![]),
        frames: Mutex::new(vec![]),
    });
    let scheduler = devworks_engine::Scheduler::new(
        app.store.clone(),
        app.runtime.jobs().clone(),
        native.clone(),
        Arc::new(NoInferencePipeline),
        app.runtime.processes().clone(),
    );
    (scheduler, native)
}
fn captured(run: &StoredJobRun) -> Value {
    let saved: Value =
        serde_json::from_str(run.checkpoint.as_ref().expect("durable native invocation")).unwrap();
    serde_json::from_str(saved["frame"].as_str().unwrap()).unwrap()
}

#[test]
fn empty_scheduled_help_jobs_capture_current_revisions_for_each_run_and_explicit_stale_stays_stale()
{
    let _native_case = crate::common::native_runtime_case();
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let app = boot_help(&rt, &h);
    let (scheduler, native) = scheduler(&app, 0);
    let first_head = app.head();
    let registry = read(&*app.store, |s| s.packages().registry_revision());
    let job = app.create(
        "Current Help",
        json!({"kind":"Interval","seconds":"3600"}),
        app.definition(json!({}), true),
        true,
    );
    h.advance(3_601_000);
    app.tick(&rt, &scheduler);
    let first = app.settle(&rt, &scheduler, &job);
    assert_eq!(first.status, "Succeeded");
    let frame1 = captured(&first);
    assert_eq!(frame1["body"]["registryRevision"], registry.to_string());
    assert_eq!(
        frame1["body"]["expectedHelpRevision"],
        first_head.revision.to_string()
    );
    let next_registry = write(&*app.store, move |tx| {
        tx.packages().advance_registry(registry)
    })
    .unwrap();
    // Publish a real changed Help generation before the next occurrence, so both CAS
    // dimensions change independently of the immutable empty schedule definition.
    let args = read(
        &*app.store,
        devworks_core::services::help::current_generation_args,
    );
    let regenerate = app.command(DOCS, serde_json::to_value(args).unwrap(), 1);
    assert!(
        matches!(
            regenerate,
            CommandReply::Recorded {
                receipt: devworks_core::protocol::Receipt {
                    decision: DomainDecision::Accepted { .. },
                    ..
                },
                ..
            }
        ),
        "{regenerate:?}"
    );
    rt.block_on(within("changed Help generation", async {
        loop {
            app.runtime.dispatcher().run_until_idle().await.unwrap();
            if app.head().registry_revision == next_registry {
                break;
            }
            tokio::task::yield_now().await;
        }
    }));
    let current_head = app.head();
    assert!(current_head.revision > first_head.revision);
    h.advance(3_600_000);
    app.tick(&rt, &scheduler);
    let second = app.settle(&rt, &scheduler, &job);
    assert_eq!(second.status, "Succeeded");
    let frame2 = captured(&second);
    assert_ne!(first.id, second.id);
    assert_ne!(first.operation_id, second.operation_id);
    assert_eq!(
        frame2["body"]["registryRevision"],
        next_registry.to_string()
    );
    assert_eq!(
        frame2["body"]["expectedHelpRevision"],
        current_head.revision.to_string()
    );
    assert_eq!(first.definition, second.definition);
    assert!(
        serde_json::from_str::<Value>(&first.definition).unwrap()["arguments"]["value"]
            .as_object()
            .unwrap()
            .is_empty()
    );
    let empty_manual = app.command(DOCS, json!({}), 1);
    assert!(
        matches!(
            empty_manual,
            CommandReply::Rejected {
                code: devworks_core::protocol::Rejection::InvalidRequest,
                ..
            }
        ),
        "the job adapter must not change the public Help command: {empty_manual:?}"
    );
    let mut stale = frame1["body"].clone();
    stale.as_object_mut().unwrap().remove("kind");
    let reply = app.command(DOCS, stale.clone(), 1);
    assert!(
        matches!(
            reply,
            CommandReply::Recorded {
                receipt: devworks_core::protocol::Receipt {
                    decision: DomainDecision::Refused { .. },
                    ..
                },
                ..
            }
        ),
        "{reply:?}"
    );
    for adapter in [false, true] {
        let exact = app.create(
            &format!("Explicit stale Help ({adapter})"),
            json!({"kind":"Manual"}),
            app.definition(stale.clone(), adapter),
            true,
        );
        value(app.command(
            "jobs.run_now",
            json!({"jobId":exact,"expectedRevision":"1"}),
            2,
        ));
        app.tick(&rt, &scheduler);
        let run = app.settle(&rt, &scheduler, &exact);
        assert_eq!(run.status, "Failed");
        assert!(
            run.checkpoint.is_none(),
            "authored args are not replaced by current metadata under either schema"
        );
    }
    assert_eq!(native.frames.lock().unwrap().len(), 4);
    rt.block_on(app.runtime.shutdown());
}

#[test]
fn unknown_ack_requeue_reuses_persisted_help_frame_and_invocation_after_metadata_changes() {
    let _native_case = crate::common::native_runtime_case();
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let app = boot_help(&rt, &h);
    let (scheduler, native) = scheduler(&app, devworks_engine::jobs::UNKNOWN_RESUBMISSIONS + 1);
    let job = app.create(
        "Unknown Help acknowledgment",
        json!({"kind":"Manual"}),
        app.definition(json!({}), true),
        true,
    );
    let run = value(app.command(
        "jobs.run_now",
        json!({"jobId":job,"expectedRevision":"1"}),
        2,
    ));
    let run_id = id(run["id"].as_str().unwrap());
    app.tick(&rt, &scheduler);
    let pending = read(&*app.store, |s| s.jobs().job_run(&run_id)).unwrap();
    assert_eq!(pending.status, "Queued");
    let exact = pending.checkpoint.clone();
    let registry = read(&*app.store, |s| s.packages().registry_revision());
    write(&*app.store, move |tx| {
        tx.packages().advance_registry(registry)
    })
    .unwrap();
    app.tick(&rt, &scheduler);
    let refused = app.settle(&rt, &scheduler, &job);
    assert_eq!(refused.id, pending.id);
    assert_eq!(refused.operation_id, pending.operation_id);
    assert_eq!(refused.checkpoint, exact);
    assert_eq!(
        refused.status, "Failed",
        "the original stale capture is refused, never silently refreshed under the old identity"
    );
    let frames = native.frames.lock().unwrap();
    assert_eq!(
        frames.len(),
        devworks_engine::jobs::UNKNOWN_RESUBMISSIONS + 2
    );
    assert!(frames.iter().all(|frame| frame == &frames[0]));
    drop(frames);
    rt.block_on(app.runtime.shutdown());
}

#[test]
fn empty_help_adapter_keeps_disabled_cancelled_held_and_stopped_runs_undispatched() {
    let _native_case = crate::common::native_runtime_case();
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let app = boot_help(&rt, &h);
    let (scheduler, native) = scheduler(&app, 0);
    let disabled = app.create(
        "Disabled current Help",
        json!({"kind":"Interval","seconds":"3600"}),
        app.definition(json!({}), true),
        false,
    );
    h.advance(3_601_000);
    app.tick(&rt, &scheduler);
    assert!(app.runs(&disabled).is_empty());
    let cancelled = app.create(
        "Cancelled current Help",
        json!({"kind":"Manual"}),
        app.definition(json!({}), true),
        true,
    );
    let run = value(app.command(
        "jobs.run_now",
        json!({"jobId":cancelled,"expectedRevision":"1"}),
        2,
    ));
    let run_id = id(run["id"].as_str().unwrap());
    value(app.command(
        "jobs.cancel",
        json!({"jobId":cancelled,"runId":run_id,"expectedRevision":"1"}),
        2,
    ));
    app.tick(&rt, &scheduler);
    let cancelled = read(&*app.store, |s| s.jobs().job_run(&run_id)).unwrap();
    assert_eq!(cancelled.status, "Cancelled");
    assert!(cancelled.checkpoint.is_none());
    let held = app.create(
        "Held current Help",
        json!({"kind":"Manual"}),
        app.definition(json!({}), true),
        true,
    );
    let run = value(app.command(
        "jobs.run_now",
        json!({"jobId":held,"expectedRevision":"1"}),
        2,
    ));
    let held_id = id(run["id"].as_str().unwrap());
    let key = held_id.clone();
    write(&*app.store, move |tx| {
        let mut run = tx.jobs().job_run(&key)?.unwrap();
        let revision = run.revision;
        run.hold = Some("HumanPause".into());
        tx.jobs().update_run(&run, revision)
    })
    .unwrap();
    app.tick(&rt, &scheduler);
    let held = read(&*app.store, |s| s.jobs().job_run(&held_id)).unwrap();
    assert_eq!(held.hold.as_deref(), Some("HumanPause"));
    assert!(held.checkpoint.is_none());
    let stopped = app.create(
        "Stopped current Help",
        json!({"kind":"Manual"}),
        app.definition(json!({}), true),
        true,
    );
    let run = value(app.command(
        "jobs.run_now",
        json!({"jobId":stopped,"expectedRevision":"1"}),
        2,
    ));
    let stopped_id = id(run["id"].as_str().unwrap());
    scheduler.stop();
    app.tick(&rt, &scheduler);
    let stopped = read(&*app.store, |s| s.jobs().job_run(&stopped_id)).unwrap();
    assert_eq!(stopped.status, "Queued");
    assert!(stopped.checkpoint.is_none());
    assert!(native.frames.lock().unwrap().is_empty());
    rt.block_on(app.runtime.shutdown());
}

#[test]
fn a_lost_committed_help_ack_replays_one_exact_generation_operation() {
    let _native_case = crate::common::native_runtime_case();
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let app = boot_help(&rt, &h);
    let (scheduler, native) = scheduler(&app, 0);
    native.lose_committed.store(true, Ordering::SeqCst);
    let job = app.create(
        "Lost committed Help reply",
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
    let run = app.settle(&rt, &scheduler, &job);
    assert_eq!(run.status, "Succeeded");
    let frames = native.frames.lock().unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0], frames[1]);
    drop(frames);
    let receipts = native.receipts.lock().unwrap();
    assert_eq!(receipts.len(), 2);
    assert!(!receipts[0].1 && receipts[1].1);
    assert!(receipts[0].0.is_some());
    assert_eq!(receipts[0].0, receipts[1].0);
    assert_eq!(run.inner_operation_id, receipts[0].0);
    drop(receipts);
    rt.block_on(app.runtime.shutdown());
}

#[path = "help_cancel.rs"]
mod cancellation;

#[path = "help_service.rs"]
mod service;
