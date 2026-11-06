//! Paid native maintenance over the same runtime, with recorded vectors and in-memory secrets.
use super::conventions::imported_body;
use super::*;
use devworks_core::{
    ports::{
        http::{Entropy, NoNetwork},
        provider_http::*,
        secrets::{Secret, SecretKey, SecretStore, SessionSecrets},
    },
    services::router::{
        InferenceRouter,
        connections::{ConnectionService, auth::Credential},
    },
};
use std::io::{Cursor, Read};
struct EntropyFixture;
impl Entropy for EntropyFixture {
    fn fill(&self, b: &mut [u8]) {
        b.fill(1);
    }
}
struct Wire {
    calls: AtomicUsize,
}
struct Body {
    head: ResponseHead,
    bytes: Cursor<Vec<u8>>,
    closed: Arc<AtomicBool>,
}
struct Closer(Arc<AtomicBool>);
impl StreamCloser for Closer {
    fn close(&self) -> Result<(), TransportError> {
        self.0.store(true, Ordering::SeqCst);
        Ok(())
    }
}
impl HttpStream for Body {
    fn head(&self) -> &ResponseHead {
        &self.head
    }
    fn closer(&self) -> Arc<dyn StreamCloser> {
        Arc::new(Closer(self.closed.clone()))
    }
    fn read(&mut self, b: &mut [u8], c: &Cancellation) -> Result<usize, TransportError> {
        if c.is_cancelled() {
            return Err(TransportError {
                phase: SendPhase::AfterSend,
                kind: TransportFailure::Cancelled,
            });
        }
        self.bytes.read(b).map_err(|_| TransportError {
            phase: SendPhase::AfterSend,
            kind: TransportFailure::Protocol,
        })
    }
    fn close(&mut self) -> Result<(), TransportError> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}
impl StreamingHttp for Wire {
    fn open(
        &self,
        r: &devworks_core::ports::http::HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let body: Value = serde_json::from_slice(r.body.as_ref().unwrap()).unwrap();
        Ok(Box::new(Body{head:ResponseHead{status:200,headers:vec![("content-type".into(),"application/json".into())]},bytes:Cursor::new(serde_json::to_vec(&json!({"model":body["model"],"data":[{"index":0,"embedding":[0.25,0.75]}],"usage":{"prompt_tokens":7,"total_tokens":7}})).unwrap()),closed:Arc::new(AtomicBool::new(false))}))
    }
}
fn boot_paid(rt: &tokio::runtime::Runtime, h: &Harness, wire: Arc<Wire>) -> HelpApp {
    let store = h.open();
    let secrets = Arc::new(SessionSecrets::new());
    secrets
        .put(
            &SecretKey::new(store.info().store_id.0.as_str(), id("paid-key")),
            &Credential::ApiKey(Secret::new("in-memory-fixture-key")).to_secret(),
        )
        .unwrap();
    write(store.as_ref(), |tx| {
        if tx.router().connection(&id("paid-embedding"))?.is_some() {
            return Ok(());
        }
        let now = tx.now_ms();
        let epoch = tx.store().restore_epoch;
        tx.router()
            .put_connection(
                &StoredConnection {
                    id: id("paid-embedding"),
                    name: "Explicit recorded paid API".into(),
                    profile: "Embedding".into(),
                    endpoint: "https://example.test/v1".into(),
                    enabled: true,
                    local_free: false,
                    open_router: None,
                    credential_ref: Some(id("paid-key")),
                    credential_revision: 1,
                    credential_epoch: Some(epoch),
                    lifecycle: "Connected".into(),
                    state_reason: None,
                    account_revision: 1,
                    refresh_operation_id: None,
                    account_digest: None,
                    access_expires_at_ms: None,
                    risk: None,
                    discovered: Some(vec![DiscoveredModel {
                        id: "recorded-embedding".into(),
                        context_tokens: None,
                    }]),
                    discovered_at_ms: Some(now),
                    healthy: Some(true),
                    checked_at_ms: Some(now),
                    revision: 1,
                    created_at_ms: now,
                    updated_at_ms: now,
                },
                None,
            )
            .map(|_| ())
    })
    .unwrap();
    let connections = Arc::new(ConnectionService::new(
        h.ids.clone(),
        h.clock(),
        store.clone(),
        Arc::new(NoNetwork),
        secrets,
        Arc::new(EntropyFixture),
    ));
    let router = Arc::new(InferenceRouter::new(
        h.ids.clone(),
        h.clock(),
        connections.clone(),
    ));
    let mut config = RuntimeConfig::new(id("paid-convention-dispatcher"));
    config.scheduler_interval = Duration::from_secs(3600);
    config.collect_interval = Duration::from_secs(3600);
    config.housekeeping_interval = Duration::from_secs(3600);
    config.shutdown_grace = Duration::from_millis(50);
    config.shutdown_bound = Duration::from_secs(5);
    config.providers = Some(devworks_engine::providers::driver::ProviderComposition::new(wire));
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
        use_cases: UseCases::new()
            .with_queue()
            .with_connections(connections)
            .with_router(router),
        consumers: vec![],
        indexing: None,
        config,
    };
    let runtime = Arc::new(rt.block_on(boot(composition)).unwrap());
    runtime.scheduler().stop();
    let app = HelpApp {
        store,
        runtime,
        n: AtomicUsize::new(0),
    };
    rt.block_on(within("paid runtime Help startup", async {
        loop {
            app.runtime.dispatcher().run_until_idle().await.unwrap();
            if app.head().generation_id.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    }));
    app
}
fn configure(app: &HelpApp, rt: &tokio::runtime::Runtime) -> Id {
    let create=value(app.command("router.configure",json!({"action":{"kind":"CreateEmbeddingSpace","name":"Recorded paid space","connectionId":"paid-embedding","expectedConnectionRevision":"1","upstreamModelId":"recorded-embedding","dimensions":"2","dimensionMode":"Native","metric":"Cosine"}}),3));
    let candidate = id(create["id"].as_str().unwrap());
    let model = devworks_core::services::router::connections::discovered_model_id(
        "Embedding",
        &id("paid-embedding"),
        "recorded-embedding",
    );
    value(app.command("router.price_override",json!({"modelId":model,"expectedRevision":"1","price":{"inputUsdPerMillion":"1","outputUsdPerMillion":"0"}}),3));
    let result=app.command("router.configure",json!({"action":{"kind":"QualifyEmbeddingSpace","candidateId":candidate,"expectedRevision":"1"}}),3);
    assert!(
        matches!(
            result,
            CommandReply::Recorded {
                receipt: devworks_core::protocol::Receipt {
                    decision: DomainDecision::Applied { .. },
                    ..
                },
                ..
            }
        ),
        "{result:?}"
    );
    rt.block_on(within("real native paid qualification", async {
        loop {
            app.runtime.dispatcher().run_until_idle().await.unwrap();
            if read(app.store.as_ref(), |s| {
                s.router().embedding_qualification(&candidate)
            })
            .is_some_and(|q| q.state == EmbeddingQualificationState::Qualified)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    }));
    let q = read(app.store.as_ref(), |s| {
        s.router().embedding_qualification(&candidate)
    })
    .unwrap();
    value(app.command("router.configure",json!({"action":{"kind":"SelectEmbeddingSpace","candidateId":candidate,"expectedRevision":q.revision.to_string(),"expectedSelectionRevision":"1"}}),3));
    candidate
}
struct AckGate {
    runtime: Arc<Runtime>,
    entered: AtomicBool,
    release: Mutex<bool>,
    changed: std::sync::Condvar,
    inner: Mutex<Option<OperationId>>,
}
impl AckGate {
    fn release(&self) {
        *self.release.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct ReleaseAck(Arc<AckGate>);
impl Drop for ReleaseAck {
    fn drop(&mut self) {
        self.0.release();
    }
}
impl NativeJobDispatch for AckGate {
    fn command(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
    ) -> devworks_core::gateway::CommandOutcome {
        let reply = self.runtime.command(ctx, operation, raw)?;
        if let CommandReply::Recorded { receipt, .. } = &reply {
            *self.inner.lock().unwrap() = receipt.operation_id.clone();
        }
        self.entered.store(true, Ordering::SeqCst);
        let open = self.release.lock().unwrap();
        let (_guard, wait) = self
            .changed
            .wait_timeout_while(open, Duration::from_secs(20), |open| !*open)
            .unwrap();
        assert!(!wait.timed_out(), "owner ACK gate timeout");
        Ok(reply)
    }
}
#[test]
fn paid_native_index_waits_for_actual_accepted_job_link_and_accounts_seven_tokens_once() {
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let wire = Arc::new(Wire {
        calls: AtomicUsize::new(0),
    });
    let app = boot_paid(&rt, &h, wire.clone());
    let owner = imported_body(&app, &rt);
    configure(&app, &rt);
    rt.block_on(within("selected paid body publication", async {
        loop {
            app.runtime.dispatcher().run_until_idle().await.unwrap();
            if read(app.store.as_ref(), |s| {
                s.conventions().convention_index_head(&owner)
            })
            .is_some_and(|i| i.status == "Ready")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    }));
    let baseline = wire.calls.load(Ordering::SeqCst);
    let native = app
        .runtime
        .jobs()
        .catalog()
        .capabilities()
        .into_iter()
        .find(|c| c.operation == "conventions.index")
        .unwrap();
    let job=app.create("Actual paid native index",json!({"kind":"Manual"}),json!({"kind":"Native","capabilityId":native.capability_id,"arguments":{"schema":native.schema,"value":{}}}),true);
    let gate = Arc::new(AckGate {
        runtime: app.runtime.clone(),
        entered: AtomicBool::new(false),
        release: Mutex::new(false),
        changed: std::sync::Condvar::new(),
        inner: Mutex::new(None),
    });
    let release = ReleaseAck(gate.clone());
    let scheduler = devworks_engine::Scheduler::new(
        app.store.clone(),
        app.runtime.jobs().clone(),
        gate.clone(),
        Arc::new(NoInferencePipeline),
        app.runtime.processes().clone(),
    );
    value(app.command(
        "jobs.run_now",
        json!({"jobId":job,"expectedRevision":"1"}),
        2,
    ));
    rt.block_on(within("accepted child awaiting actual owner ACK", async {
        scheduler.tick().await.unwrap();
        loop {
            app.runtime.dispatcher().tick().await.unwrap();
            if gate.entered.load(Ordering::SeqCst) {
                let inner = gate.inner.lock().unwrap().clone().unwrap();
                let entries = read(app.store.as_ref(), |s| {
                    s.queue().deliveries(
                        &DeliveryFilter {
                            consumer: None,
                            operation_id: Some(inner.clone()),
                        },
                        None,
                        10,
                    )
                });
                if entries
                    .entries
                    .iter()
                    .any(|e| e.state == QueueState::Claimed)
                {
                    // Keep the real owner ACK blocked while the claimed delivery gets
                    // multiple executor turns; a claim alone precedes transaction A.
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    assert_eq!(
                        read(app.store.as_ref(), |s| s.operations().operation(&inner))
                            .unwrap()
                            .state,
                        OperationState::IntentRecorded
                    );
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
    }));
    assert_eq!(
        wire.calls.load(Ordering::SeqCst),
        baseline,
        "no native preparation/admission before actual parent ACK link"
    );
    gate.release();
    drop(release);
    let run = app.settle(&rt, &scheduler, &job);
    assert_eq!(run.status, "Succeeded", "{run:?}");
    assert_eq!(wire.calls.load(Ordering::SeqCst), baseline + 1);
    let attempts = read(app.store.as_ref(), |s| {
        s.usage().inference_attempts(
            &UsageAttemptFilter {
                job_run_id: Some(run.id.clone()),
                ..Default::default()
            },
            None,
            10,
        )
    });
    assert_eq!(
        attempts.len(),
        1,
        "one actual paid native attempt owned by this run"
    );
    let attempt = &attempts[0];
    assert_eq!(attempt.actor_id, app.store.info().host_actor);
    let balance = read(app.store.as_ref(), |s| {
        s.usage().attempt_balance(&attempt.attempt_id)
    })
    .unwrap();
    assert!(balance.local_quiescent && !balance.unknown_tail);
    assert_eq!(
        devworks_core::services::usage::accounting::summary(&balance)
            .unwrap()
            .meter
            .quantities
            .input_tokens,
        Some(7)
    );
    assert_eq!(balance.priced_micro_usd, Some(7));
    let attempt_id = attempt.attempt_id.clone();
    rt.block_on(app.runtime.shutdown());
    drop(scheduler);
    drop(gate);
    drop(app);
    // Open a real new boot. Completed native custody and its owning job survive;
    // recovery must not turn a completed paid child into a new request.
    let reopened = boot_paid(&rt, &h, wire.clone());
    assert_eq!(reopened.runs(&job).len(), 1);
    assert_eq!(reopened.runs(&job)[0].status, "Succeeded");
    assert_eq!(wire.calls.load(Ordering::SeqCst), baseline + 1);
    assert_eq!(
        read(reopened.store.as_ref(), |s| s
            .usage()
            .attempt_balance(&attempt_id))
        .unwrap(),
        balance
    );
    rt.block_on(reopened.runtime.shutdown());
}
