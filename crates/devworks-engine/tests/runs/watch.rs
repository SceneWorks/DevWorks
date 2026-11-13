//! DW-94 watchers over the real runtime: `watch.*` through the admission gateway with real child
//! processes (exit 0, non-zero, signal, timeout, stop), a restart between an event's commit and
//! its delivery, and a native agent turn that carries the event and acknowledges it only when it
//! completes. This fixture runs `/bin/sh`; Windows compiles the service and runs its process
//! supervisor checks in the platform CI lane.
#![cfg(unix)]
use super::*;
use devworks_core::protocol::results::OperationResult;
use devworks_core::services::process::tools::{
    RegisteredExecutable, RegisteredWorkingRoot, ToolBindings,
};
use devworks_engine::runtime::{ProcessToolComposition, Runtime};

/// Provider wire that records each request body and, at each request, whether the watcher
/// event is carried by the in-flight turn and still unacknowledged.
struct RecordingWire {
    store: Arc<dyn ApplicationStore>,
    session: Mutex<Option<Id>>,
    bodies: Arc<Mutex<Vec<String>>>,
    carried: Arc<Mutex<Vec<(bool, bool)>>>,
}
impl StreamingHttp for RecordingWire {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        self.bodies.lock().unwrap().push(
            String::from_utf8_lossy(request.body.as_deref().unwrap_or_default()).into_owned(),
        );
        if let Some(session) = self.session.lock().unwrap().clone() {
            let events = read(&*self.store, |s| {
                s.watchers().watch_events(
                    &session,
                    &[WatchEventState::Delivered, WatchEventState::Acknowledged],
                    10,
                )
            });
            let turn = read(&*self.store, |s| s.runs().session_turn(&session));
            for event in events {
                self.carried.lock().unwrap().push((
                    event.carried_turn_id.is_some()
                        && event.carried_turn_id.as_ref() == turn.as_ref().map(|t| &t.turn_id),
                    event.state == WatchEventState::Acknowledged,
                ));
            }
        }
        Ok(Box::new(AgentStream {
            head: ResponseHead {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
            },
            body: Some(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"noted\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_vec()),
        }))
    }
}

struct Wired {
    session: Arc<Mutex<Option<Id>>>,
    bodies: Arc<Mutex<Vec<String>>>,
    carried: Arc<Mutex<Vec<(bool, bool)>>>,
}

fn watch_composition(fx: &Fx, work: &std::path::Path) -> (Composition, Wired) {
    let mut composed = composition_with(
        fx,
        None,
        Arc::new(FakePower::default()),
        Arc::new(Authority),
        Arc::new(devworks_adapters::process::NativeProcessHost),
    );
    let connections = Arc::new(
        devworks_core::services::router::connections::ConnectionService::new(
            fx.h.ids.clone(),
            fx.h.clock(),
            fx.store.clone(),
            Arc::new(devworks_core::ports::http::NoNetwork),
            Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
            Arc::new(WatchEntropy),
        ),
    );
    composed.use_cases = UseCases::new()
        .with_connections(connections.clone())
        .with_router(Arc::new(
            devworks_core::services::router::InferenceRouter::new(
                fx.h.ids.clone(),
                fx.h.clock(),
                connections,
            ),
        ));
    let wired = Wired {
        session: Arc::new(Mutex::new(None)),
        bodies: Arc::new(Mutex::new(Vec::new())),
        carried: Arc::new(Mutex::new(Vec::new())),
    };
    let wire = SharedWire {
        inner: Arc::new(RecordingWire {
            store: fx.store.clone(),
            session: Mutex::new(None),
            bodies: wired.bodies.clone(),
            carried: wired.carried.clone(),
        }),
        session: wired.session.clone(),
    };
    composed.config.providers =
        Some(devworks_engine::providers::driver::ProviderComposition::new(Arc::new(wire)));
    composed.config.process_tools = Some(ProcessToolComposition {
        bindings: ToolBindings::new(
            [RegisteredExecutable {
                id: id("sh"),
                path: "/bin/sh".into(),
            }],
            [RegisteredWorkingRoot {
                id: id("watch-root"),
                project: Some(ProjectId(id("project-a"))),
                path: work.to_path_buf(),
            }],
        )
        .unwrap(),
        filesystem: Arc::new(NativeFilesystem::new()),
        protected_roots: ProtectedRoots::new(Vec::new(), Vec::new(), None),
        secrets: Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
    });
    // Passes run only when the test asks, so a restart can fall between commit and delivery.
    composed.config.watch_interval = Duration::from_secs(3600);
    (composed, wired)
}

struct WatchEntropy;
impl devworks_core::ports::http::Entropy for WatchEntropy {
    fn fill(&self, bytes: &mut [u8]) {
        bytes.fill(9);
    }
}

/// Forwards to the recording wire, which learns the session once the test knows it.
struct SharedWire {
    inner: Arc<RecordingWire>,
    session: Arc<Mutex<Option<Id>>>,
}
impl StreamingHttp for SharedWire {
    fn open(
        &self,
        request: &HttpRequest,
        limits: &StreamLimits,
        cancellation: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        *self.inner.session.lock().unwrap() = self.session.lock().unwrap().clone();
        self.inner.open(request, limits, cancellation)
    }
}

/// The primary agent's session context, with a routed pin, as the first-context test builds it.
fn agent_context(
    fx: &Fx,
) -> (
    InvocationContext,
    devworks_core::protocol::records::AgentRecord,
) {
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let (context, primary) = bind(fx, "coordinator-watch");
    route(fx, context.session_id().unwrap());
    (context, primary)
}

/// Claims the root's current activation and binds the primary agent's context to it (after a
/// restart the earlier boot's activation is no longer the one that may run a turn).
fn bind(
    fx: &Fx,
    owner: &str,
) -> (
    InvocationContext,
    devworks_core::protocol::records::AgentRecord,
) {
    let activation = fx.launch(owner);
    let host = principal(&*fx.store, "watch-host", PrincipalKind::Host);
    let agents = AgentService::new(fx.h.ids.clone(), fx.service.clone());
    let execution_id = activation.id.clone();
    let epoch = activation.owner_epoch;
    let primary = fx.tx(move |_, tx| {
        agents
            .materialize_primary(tx, &host, &execution_id, epoch)
            .map_err(|error| StorageError::InvalidRecord(format!("primary: {error:?}")))
    });
    let assignment = read(&*fx.store, |s| s.runs().assignment(&primary.assignment_id)).unwrap();
    let session = read(&*fx.store, |s| s.sessions().session(&assignment.session_id)).unwrap();
    assert_eq!(session.project_id, Some(ProjectId(id("project-a"))));
    let context = devworks_core::context::HostIngress::new(&*fx.store)
        .context(
            session.agent_id.clone(),
            PrincipalKind::Agent,
            id("watch-invocation"),
            id("watch-ingress"),
            devworks_core::protocol::Counter::new(fx.store.info().restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
        .with_session(session.id.clone())
        .with_lineage(
            session.project_id.clone(),
            session.root_id.clone(),
            Some(primary.assignment_id.clone()),
        )
        .with_activation(activation.id.clone(), epoch);
    (context, primary)
}

fn route(fx: &Fx, session: &Id) {
    let pin = StoredPin {
        session_id: session.clone(),
        pin_id: id("watch-pin"),
        revision: 1,
        model_id: id("watch-model"),
        connection_id: id("watch-connection"),
        upstream_model_id: Some("fixture-model".into()),
        endpoint_profile: Some("CompatibleChat".into()),
        identity_origin: "Resolved".into(),
        intent: serde_json::to_string(&devworks_core::protocol::records::RouteIntent::Model {
            model_id: id("watch-model"),
        })
        .unwrap(),
        requested_effort: "Default".into(),
        effective_effort: "Default".into(),
        required_capabilities: vec!["tools".into()],
        replaced_pin_id: None,
        reason: "Initial".into(),
        updated_at_ms: 0,
    };
    fx.tx(move |_, tx| {
        tx.router().put_connection(
            &StoredConnection {
                id: id("watch-connection"),
                name: "Fixture".into(),
                profile: "CompatibleChat".into(),
                endpoint: "http://localhost:11434/v1".into(),
                enabled: true,
                local_free: true,
                open_router: None,
                credential_ref: None,
                credential_revision: 0,
                credential_epoch: None,
                lifecycle: "Connected".into(),
                state_reason: None,
                account_revision: 1,
                refresh_operation_id: None,
                account_digest: None,
                access_expires_at_ms: None,
                risk: None,
                discovered: None,
                discovered_at_ms: None,
                healthy: Some(true),
                checked_at_ms: Some(0),
                revision: 1,
                created_at_ms: 0,
                updated_at_ms: 0,
            },
            None,
        )?;
        tx.router().put_route_model(
            &StoredRouteModel {
                id: id("watch-model"),
                display_name: "Fixture model".into(),
                endpoint_profile: "CompatibleChat".into(),
                upstream_model_id: "fixture-model".into(),
                enabled: true,
                efforts: vec![],
                image_input: false,
                tools: true,
                context_tokens: Some(16_000),
                connection_id: Some(id("watch-connection")),
                position: 1,
                revision: 1,
                updated_at_ms: 0,
            },
            None,
        )?;
        tx.router().put_session_pin(&pin, None)?;
        Ok(())
    });
}

fn frame(mode: &str, op: &str, mut body: Value, key: &str) -> Vec<u8> {
    body["kind"] = json!(op);
    let mut envelope = json!({"mode":mode,"protocol":"1.0-draft.1","schemaRevision":8,"body":body});
    if mode == "Command" {
        envelope["requestKey"] = json!(key);
    } else {
        envelope["queryId"] = json!(key);
    }
    serde_json::to_vec(&envelope).unwrap()
}

fn command(app: &Runtime, ctx: &InvocationContext, op: &str, body: Value, key: &str) -> Value {
    match app
        .command(ctx, op, &frame("Command", op, body, key))
        .unwrap()
    {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied { result } => {
                serde_json::to_value(result).unwrap()["value"].clone()
            }
            other => panic!("{op}: {other:?}"),
        },
        other => panic!("{op}: {other:?}"),
    }
}

fn inspect(app: &Runtime, ctx: &InvocationContext, watcher: &str) -> Value {
    match app
        .query(
            ctx,
            "watch.inspect",
            &frame(
                "Query",
                "watch.inspect",
                json!({"watcherId": watcher}),
                "inspect",
            ),
        )
        .unwrap()
    {
        DomainQueryReply::Result { result, .. } => match result {
            OperationResult::WatchInspect { value } => serde_json::to_value(value).unwrap(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn output(app: &Runtime, ctx: &InvocationContext, process: &str) -> (String, Value) {
    match app
        .query(
            ctx,
            "process.output",
            // process.output is carried at the default schema revision.
            &serde_json::to_vec(&json!({"mode": "Query", "protocol": "1.0-draft.1",
                "queryId": "output", "body": {"kind": "process.output",
                "processId": process, "afterBytes": "0", "maxBytes": 4096}}))
            .unwrap(),
        )
        .unwrap()
    {
        DomainQueryReply::Result { result, .. } => match result {
            OperationResult::ProcessOutput { value } => {
                let value = serde_json::to_value(value).unwrap();
                let bytes = devworks_core::services::browser::cdp::decode_base64(
                    value["content"].as_str().unwrap(),
                )
                .unwrap();
                (String::from_utf8(bytes).unwrap(), value)
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn create(
    app: &Runtime,
    ctx: &InvocationContext,
    script: &str,
    timeout_ms: u64,
    key: &str,
) -> Value {
    command(
        app,
        ctx,
        "watch.create",
        json!({"process": {
            "executableRef":"sh", "args":["-c", script],
            "workingDirectory":{"rootId":"watch-root","relative":"work"},
            "environment":[], "profile":"Full",
            "timeoutMs": timeout_ms.to_string(), "outputLimitBytes":"4096"
        }, "tailBytes": 64}),
        key,
    )
}

/// Ticks settlement (not delivery) until every watcher in `ids` is settled.
async fn settle_all(app: &Runtime, ctx: &InvocationContext, ids: &[&str]) {
    let watchers = app.watchers().expect("watchers composed");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        watchers.settle().unwrap();
        if ids
            .iter()
            .all(|w| inspect(app, ctx, w)["state"] == json!("Settled"))
        {
            return;
        }
        if std::time::Instant::now() >= deadline {
            let detail: Vec<String> = ids
                .iter()
                .map(|w| {
                    let record = inspect(app, ctx, w);
                    let op = record["process"]["processId"].as_str().unwrap().to_owned();
                    let operation = read(&**app.store(), |s| {
                        s.operations().operation(&OperationId(id(&op)))
                    });
                    let queued = read(&**app.store(), |s| {
                        s.queue().deliveries(
                            &DeliveryFilter {
                                consumer: None,
                                operation_id: Some(OperationId(id(&op))),
                            },
                            None,
                            5,
                        )
                    });
                    format!(
                        "{w}: {} queued={:?} op={:?} attempts={:?}",
                        record["state"],
                        queued
                            .entries
                            .iter()
                            .map(|e| (e.consumer.clone(), e.state, e.last_error.clone()))
                            .collect::<Vec<_>>(),
                        operation.as_ref().map(|o| o.state),
                        operation.as_ref().map(|o| o
                            .attempts
                            .iter()
                            .map(|a| a.observations.clone())
                            .collect::<Vec<_>>())
                    )
                })
                .collect();
            panic!("watchers never settled: {detail:#?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn notices(fx: &Fx, session: &Id) -> Vec<Value> {
    read(&*fx.store, |s| s.sessions().entries(session, 0, 500))
        .into_iter()
        .filter(|entry| entry.kind == "HostNotice")
        .map(|entry| entry.body)
        .collect()
}

#[test]
fn a_watched_process_wakes_its_session_once_and_a_completed_turn_acknowledges_it() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    let work = fx.h.root.with_extension("watch-work");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(work.join("work")).unwrap();
    let work = work.canonicalize().unwrap();
    let rt = runtime();
    rt.block_on(async {
        let (composed, wired) = watch_composition(&fx, &work);
        let app = Arc::new(boot(composed).await.unwrap());
        let (ctx, _) = agent_context(&fx);
        let session = ctx.session_id().unwrap().clone();

        // watch.create answers before the process exits.
        let slow = create(&app, &ctx, "echo started; sleep 1; echo finishing; exit 3", 10_000, "w-fail");
        assert_eq!(slow["state"], json!("Active"));
        assert!(slow.get("outcome").is_none());
        let failed = slow["id"].as_str().unwrap().to_owned();
        let ok = create(&app, &ctx, "printf 'all good'", 10_000, "w-ok")["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let secret = command(
            &app,
            &ctx,
            "watch.create",
            json!({"process": {
                "executableRef":"sh", "args":["-c", "printf 'token=%s end' \"$WATCH_SECRET\""],
                "workingDirectory":{"rootId":"watch-root","relative":"work"},
                "environment":[{"name":"WATCH_SECRET","value":"watch-secret-value"}], "profile":"Full",
                "timeoutMs":"10000", "outputLimitBytes":"4096"
            }}),
            "w-secret",
        )["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let signalled = create(&app, &ctx, "kill -9 $$", 10_000, "w-signal")["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let timed_out = create(&app, &ctx, "sleep 30", 300, "w-timeout")["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let stopped = create(&app, &ctx, "sleep 30", 60_000, "w-stop");
        let unobserved = create(&app, &ctx, "sleep 2", 60_000, "w-cancel");

        // watch.list answers the session's watchers; inspect shows a running watcher's state.
        let listed = match app
            .query(&ctx, "watch.list", &frame("Query", "watch.list", json!({"page":{"limit":50}}), "list"))
            .unwrap()
        {
            DomainQueryReply::Result {
                result: OperationResult::WatchList { value },
                ..
            } => value,
            other => panic!("{other:?}"),
        };
        assert_eq!(listed.items.0.len(), 7);
        assert_eq!(inspect(&app, &ctx, &failed)["state"], json!("Active"));

        // Cancel with stop_process stops the launched process; without it the watcher stops
        // observing and delivers nothing.
        let stop = command(
            &app,
            &ctx,
            "watch.cancel",
            json!({"watcherId": stopped["id"], "expectedRevision": stopped["revision"], "stopProcess": true}),
            "cancel-stop",
        );
        assert_eq!(stop["state"], json!("Stopping"));
        let quiet = command(
            &app,
            &ctx,
            "watch.cancel",
            json!({"watcherId": unobserved["id"], "expectedRevision": unobserved["revision"], "stopProcess": false}),
            "cancel-quiet",
        );
        assert_eq!(quiet["state"], json!("Cancelled"));

        let stopped = stopped["id"].as_str().unwrap().to_owned();
        settle_all(
            &app,
            &ctx,
            &[&failed, &ok, &secret, &signalled, &timed_out, &stopped],
        )
        .await;
        let outcome = |w: &str| inspect(&app, &ctx, w)["outcome"].clone();
        assert_eq!(outcome(&ok)["kind"], json!("Succeeded"));
        assert_eq!(outcome(&ok)["exitCode"], json!(0));
        assert_eq!(inspect(&app, &ctx, &ok)["outputTail"], json!("all good"));
        let secret_tail = inspect(&app, &ctx, &secret)["outputTail"].as_str().unwrap().to_owned();
        assert!(secret_tail.starts_with("token=") && secret_tail.ends_with(" end"), "{secret_tail}");
        assert!(!secret_tail.contains("watch-secret-value"), "{secret_tail}");
        assert_eq!(outcome(&failed)["kind"], json!("Failed"));
        assert_eq!(outcome(&failed)["exitCode"], json!(3));
        let failed_record = inspect(&app, &ctx, &failed);
        assert_eq!(failed_record["outputTail"], json!("started\nfinishing\n"));
        assert!(failed_record["durationMs"].as_str().unwrap().parse::<u64>().unwrap() >= 900, "{failed_record:#}");
        assert_eq!(outcome(&signalled)["kind"], json!("Failed"));
        assert_eq!(outcome(&signalled)["signal"], json!(9));
        assert!(outcome(&signalled).get("exitCode").is_none());
        assert_eq!(outcome(&timed_out)["kind"], json!("TimedOut"));
        assert_eq!(outcome(&stopped)["kind"], json!("Cancelled"));
        assert_eq!(inspect(&app, &ctx, unobserved["id"].as_str().unwrap())["state"], json!("Cancelled"));

        // Settled events are recorded but not yet delivered when the app stops.
        let event_of = |w: &str| inspect(&app, &ctx, w)["event"].clone();
        assert_eq!(event_of(&failed)["state"], json!("Pending"));
        assert!(inspect(&app, &ctx, unobserved["id"].as_str().unwrap()).get("event").is_none());
        assert!(notices(&fx, &session).is_empty());
        let event_id = event_of(&failed)["id"].as_str().unwrap().to_owned();
        app.shutdown().await;
        drop(app);

        // After the restart the same events are delivered once, each as one host notice whose
        // source is its event ID.
        let (composed, wired_again) = watch_composition(&fx, &work);
        let app = Arc::new(boot(composed).await.unwrap());
        // A restart holds the root's work until a person continues it.
        applied(&fx.resume(&fx.user, "root-i"));
        let (ctx, primary) = bind(&fx, "coordinator-watch-after-restart");
        let watchers = app.watchers().unwrap();
        assert_eq!(watchers.deliver().unwrap(), 6);
        assert_eq!(watchers.tick().unwrap().delivered, 0);
        let delivered = notices(&fx, &session);
        assert_eq!(delivered.len(), 6);
        assert_eq!(
            delivered
                .iter()
                .filter(|body| body["source"] == json!(event_id))
                .count(),
            1
        );
        assert_eq!(inspect(&app, &ctx, &failed)["event"]["state"], json!("Delivered"));
        // The agent can read each delivered notice (host-written, in its session's scope).
        for body in &delivered {
            let artifact: devworks_core::protocol::provider::ArtifactRef =
                serde_json::from_value(body["artifact"].clone()).unwrap();
            let read_back = app.artifacts().read_exact(app.store().as_ref(), &ctx, &artifact, 8_000);
            assert!(read_back.is_ok(), "{:?}", read_back.err());
        }
        drop(wired);

        // The next turn carries every delivered event; it stays unacknowledged while the turn
        // runs and is acknowledged once the turn completes.
        *wired_again.session.lock().unwrap() = Some(session.clone());
        let (_stop_tx, stop) = tokio::sync::watch::channel(false);
        let end = NativeAgentTurnExecutor::new(&app)
            .run(ctx.clone(), primary.clone(), stop)
            .await;
        assert!(
            matches!(
                end,
                LaunchEnd::Finished {
                    outcome: ActivationOutcome::Completed,
                    ..
                }
            ),
            "{end:?}"
        );
        let bodies = wired_again.bodies.lock().unwrap().clone();
        assert!(!bodies.is_empty());
        let last = bodies.last().unwrap();
        assert!(
            last.contains(&format!("Watcher event {event_id}"))
                && last.contains("Failed (exit code 3)")
                && last.contains(" Duration: ")
                && last.contains("started\\nfinishing"),
            "{last}"
        );
        assert_eq!(last.matches(&format!("Watcher event {event_id}")).count(), 1);
        let carried = wired_again.carried.lock().unwrap().clone();
        assert!(!carried.is_empty() && carried.iter().all(|(carried, acked)| *carried && !acked), "{carried:?}");
        for watcher in [&failed, &ok, &secret, &signalled, &timed_out, &stopped] {
            assert_eq!(
                inspect(&app, &ctx, watcher)["event"]["state"],
                json!("Acknowledged"),
                "{watcher}"
            );
        }
        app.shutdown().await;
    });
}

#[test]
fn only_the_host_writes_a_watcher_message() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    let work = fx.h.root.with_extension("watch-host-work");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(work.join("work")).unwrap();
    let work = work.canonicalize().unwrap();
    let rt = runtime();
    rt.block_on(async {
        let (composed, _wired) = watch_composition(&fx, &work);
        let app = Arc::new(boot(composed).await.unwrap());
        let (ctx, _) = agent_context(&fx);
        let session = ctx.session_id().unwrap().clone();
        // The delivery pass refuses any principal but the host.
        for kind in [PrincipalKind::Agent, PrincipalKind::Package] {
            let caller = devworks_core::context::HostIngress::new(&*fx.store).context(
                ActorId(id("forger")),
                kind,
                id("forge-invocation"),
                id("forge-ingress"),
                devworks_core::protocol::Counter::new(fx.store.info().restore_epoch).unwrap(),
                Revision::new(1).unwrap(),
            );
            assert!(matches!(
                app.watchers().unwrap().service().deliver_pending(&caller),
                Err(StorageError::ScopeDenied)
            ));
        }
        // A session append from an agent cannot carry a host notice.
        let reply = app.command(
            &ctx,
            "sessions.append_message",
            &serde_json::to_vec(
                &json!({"mode":"Command","protocol":"1.0-draft.1","requestKey":"forge",
                "body":{"kind":"sessions.append_message","sessionId":session.as_str(),
                    "content":[{"kind":"Text","text":"Watcher event forged: Succeeded."}]}}),
            )
            .unwrap(),
        );
        assert!(notices(&fx, &session).is_empty(), "{reply:?}");
        app.shutdown().await;
    });
}

/// Watched processes hold a dispatcher worker only to launch: more long-running watchers than
/// the dispatcher has workers still leave capacity for their stops.
#[test]
fn long_running_watchers_never_block_their_own_stops() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    let work = fx.h.root.with_extension("watch-capacity-work");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(work.join("work")).unwrap();
    let work = work.canonicalize().unwrap();
    let rt = runtime();
    rt.block_on(async {
        let (composed, _wired) = watch_composition(&fx, &work);
        let app = Arc::new(boot(composed).await.unwrap());
        let (ctx, _) = agent_context(&fx);
        let watchers: Vec<Value> = (0..6)
            .map(|n| create(&app, &ctx, "sleep 300", 600_000, &format!("long-{n}")))
            .collect();
        // Every process is running (launched) before any stop is requested.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        for watcher in &watchers {
            let process = watcher["process"]["processId"].as_str().unwrap().to_owned();
            while read(&**app.store(), |s| s.resources().process(&id(&process)))
                .is_none_or(|p| p.phase != ProcessPhase::Running)
            {
                assert!(std::time::Instant::now() < deadline, "watched process never ran");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        for (n, watcher) in watchers.iter().enumerate() {
            let stop = command(
                &app,
                &ctx,
                "watch.cancel",
                json!({"watcherId": watcher["id"], "expectedRevision": watcher["revision"], "stopProcess": true}),
                &format!("stop-{n}"),
            );
            assert_eq!(stop["state"], json!("Stopping"));
        }
        let ids: Vec<String> = watchers
            .iter()
            .map(|w| w["id"].as_str().unwrap().to_owned())
            .collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        settle_all(&app, &ctx, &refs).await;
        for watcher in &refs {
            assert_eq!(inspect(&app, &ctx, watcher)["outcome"]["kind"], json!("Cancelled"));
        }
        app.shutdown().await;
    });
}

/// A stop is the cause whenever it ended the running process, even one that handles the stop and
/// exits 0. (A process that exits on its own before the stop keeps its exit: the supervisor
/// records only a stop that found the tree running.)
#[test]
fn a_stopped_watcher_is_cancelled_even_when_its_process_exits_cleanly() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    let work = fx.h.root.with_extension("watch-trap-work");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(work.join("work")).unwrap();
    let work = work.canonicalize().unwrap();
    let rt = runtime();
    rt.block_on(async {
        let (composed, _wired) = watch_composition(&fx, &work);
        let app = Arc::new(boot(composed).await.unwrap());
        let (ctx, _) = agent_context(&fx);
        let trapping = create(
            &app,
            &ctx,
            "trap 'exit 0' TERM; while :; do sleep 0.05; done",
            60_000,
            "w-trap",
        );
        let process = trapping["process"]["processId"].as_str().unwrap().to_owned();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while read(&**app.store(), |s| s.resources().process(&id(&process)))
            .is_none_or(|p| p.phase != ProcessPhase::Running)
        {
            assert!(std::time::Instant::now() < deadline, "watched process never ran");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // The shell installs its trap before it loops.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let stop = command(
            &app,
            &ctx,
            "watch.cancel",
            json!({"watcherId": trapping["id"], "expectedRevision": trapping["revision"], "stopProcess": true}),
            "stop-trap",
        );
        assert_eq!(stop["state"], json!("Stopping"));
        let trapping_id = trapping["id"].as_str().unwrap();
        settle_all(&app, &ctx, &[trapping_id]).await;
        let outcome = inspect(&app, &ctx, trapping_id)["outcome"].clone();
        assert_eq!(outcome["kind"], json!("Cancelled"), "{outcome}");
        app.shutdown().await;
    });
}

/// `process.output` reads a watched process: its live output while it runs, then the output its
/// monitor captured once it ends (its effect completing at launch is not the end of its output).
#[test]
fn process_output_reads_a_watched_process_while_it_runs_and_after_it_ends() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    let work = fx.h.root.with_extension("watch-output-work");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(work.join("work")).unwrap();
    let work = work.canonicalize().unwrap();
    let rt = runtime();
    rt.block_on(async {
        let (composed, _wired) = watch_composition(&fx, &work);
        let app = Arc::new(boot(composed).await.unwrap());
        let (ctx, _) = agent_context(&fx);
        let watcher = create(
            &app,
            &ctx,
            "echo started; while [ ! -f done ]; do sleep 0.05; done; echo finished",
            60_000,
            "w-output",
        );
        let process = watcher["process"]["processId"].as_str().unwrap().to_owned();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let live = loop {
            let running = read(&**app.store(), |s| s.resources().process(&id(&process)))
                .is_some_and(|p| p.phase == ProcessPhase::Running);
            if running {
                let (text, value) = output(&app, &ctx, &process);
                if text.contains("started") {
                    break value;
                }
            }
            assert!(std::time::Instant::now() < deadline, "no live output");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(live["complete"], json!(false), "{live}");
        assert_eq!(live["state"], json!("Dispatching"), "{live}");
        std::fs::write(work.join("work").join("done"), b"").unwrap();
        let watcher_id = watcher["id"].as_str().unwrap();
        settle_all(&app, &ctx, &[watcher_id]).await;
        let (text, value) = output(&app, &ctx, &process);
        assert_eq!(text, "started\nfinished\n", "{value}");
        assert_eq!(value["complete"], json!(true), "{value}");
        assert_eq!(value["state"], json!("ObservedComplete"), "{value}");
        app.shutdown().await;
    });
}

fn decision(app: &Runtime, ctx: &InvocationContext, body: Value, key: &str) -> DomainDecision {
    match app
        .command(
            ctx,
            "watch.create",
            &frame("Command", "watch.create", body, key),
        )
        .unwrap()
    {
        CommandReply::Recorded { receipt, .. } => receipt.decision,
        other => panic!("watch.create: {other:?}"),
    }
}

fn probe(script: &str, interval_ms: i64, extra: Value) -> Value {
    let mut body = json!({"probe": {
        "process": {
            "executableRef":"sh", "args":["-c", script],
            "workingDirectory":{"rootId":"watch-root","relative":"work"},
            "environment":[], "profile":"Full",
            "timeoutMs":"10000", "outputLimitBytes":"4096"
        },
        "intervalMs": interval_ms.to_string(),
        "condition": {}
    }});
    if let Value::Object(extra) = extra {
        for (key, value) in extra {
            body[key] = value;
        }
    }
    body
}

fn counter_at(record: &Value, pointer: &str) -> Option<i64> {
    record
        .pointer(pointer)
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
}

/// Steps a probe watcher on the test clock until it settles: each pass settles what is observed
/// and, while the probe waits for its next run, moves the clock to that run. Returns the record
/// and every wait the probe scheduled.
async fn step_probe(
    fx: &Fx,
    app: &Runtime,
    ctx: &InvocationContext,
    watcher: &str,
) -> (Value, Vec<i64>) {
    let watchers = app.watchers().expect("watchers composed");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut waits = Vec::new();
    loop {
        watchers.settle().unwrap();
        let record = inspect(app, ctx, watcher);
        if record["state"] == json!("Settled") {
            return (record, waits);
        }
        if let Some(next) = counter_at(&record, "/probe/nextRunAtMs") {
            let wait = next - fx.h.now();
            if wait > 0 {
                waits.push(wait);
                fx.h.advance(wait);
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "probe never settled: {record}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A probe reruns its command, backing off, until the condition holds: here on the third run.
#[test]
fn a_probe_succeeds_on_the_run_that_meets_its_condition_with_backoff_between_runs() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    let work = fx.h.root.with_extension("watch-probe-work");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(work.join("work")).unwrap();
    let work = work.canonicalize().unwrap();
    let rt = runtime();
    rt.block_on(async {
        let (composed, _wired) = watch_composition(&fx, &work);
        let app = Arc::new(boot(composed).await.unwrap());
        let (ctx, _) = agent_context(&fx);
        let script = "n=$(cat count 2>/dev/null || echo 0); n=$((n+1)); echo $n > count; \
                      echo run $n; [ $n -ge 3 ]";
        let watcher = command(
            &app,
            &ctx,
            "watch.create",
            probe(script, 1000, json!({})),
            "w-probe",
        );
        assert_eq!(watcher["sourceKind"], json!("Probe"));
        let id = watcher["id"].as_str().unwrap();
        let (record, waits) = step_probe(&fx, &app, &ctx, id).await;
        assert_eq!(record["outcome"]["kind"], json!("Succeeded"), "{record}");
        assert_eq!(record["outcome"]["exitCode"], json!(0), "{record}");
        assert_eq!(counter_at(&record, "/probe/runs"), Some(3), "{record}");
        assert_eq!(waits, vec![1000, 2000], "one interval, then doubled");
        assert_eq!(
            std::fs::read_to_string(work.join("work").join("count"))
                .unwrap()
                .trim(),
            "3",
            "exactly three runs"
        );
        assert!(
            record["outputTail"].as_str().unwrap().contains("run 3"),
            "{record}"
        );
        app.shutdown().await;
    });
}

/// A probe whose condition never holds ends TimedOut at its maximum lifetime.
#[test]
fn a_polling_watcher_past_its_maximum_lifetime_ends_timed_out() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    let work = fx.h.root.with_extension("watch-lifetime-work");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(work.join("work")).unwrap();
    let work = work.canonicalize().unwrap();
    let rt = runtime();
    rt.block_on(async {
        let (composed, _wired) = watch_composition(&fx, &work);
        let app = Arc::new(boot(composed).await.unwrap());
        let (ctx, _) = agent_context(&fx);
        let watcher = command(
            &app,
            &ctx,
            "watch.create",
            probe("exit 1", 1000, json!({"maxLifetimeMs": "2500"})),
            "w-lifetime",
        );
        let created = counter_at(&watcher, "/createdAtMs").unwrap();
        assert_eq!(counter_at(&watcher, "/expiresAtMs"), Some(created + 2500));
        let (record, _) = step_probe(&fx, &app, &ctx, watcher["id"].as_str().unwrap()).await;
        assert_eq!(record["outcome"]["kind"], json!("TimedOut"), "{record}");
        assert_eq!(counter_at(&record, "/probe/runs"), Some(2), "{record}");
        // Without its own lifetime a probe takes the 24-hour default.
        let defaulted = command(
            &app,
            &ctx,
            "watch.create",
            probe("exit 1", 1000, json!({})),
            "w-default",
        );
        assert_eq!(
            counter_at(&defaulted, "/expiresAtMs"),
            Some(counter_at(&defaulted, "/createdAtMs").unwrap() + 86_400_000)
        );
        app.shutdown().await;
    });
}

/// A timer fires at its due time, not before, and wakes its session like any watcher.
#[test]
fn a_timer_fires_at_its_due_time() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    let work = fx.h.root.with_extension("watch-timer-work");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(work.join("work")).unwrap();
    let work = work.canonicalize().unwrap();
    let rt = runtime();
    rt.block_on(async {
        let (composed, _wired) = watch_composition(&fx, &work);
        let app = Arc::new(boot(composed).await.unwrap());
        let (ctx, _) = agent_context(&fx);
        let session = ctx.session_id().unwrap().clone();
        let watchers = app.watchers().expect("watchers composed");
        let timer = command(
            &app,
            &ctx,
            "watch.create",
            json!({"timer": {"afterMs": "5000"}}),
            "w-timer",
        );
        assert_eq!(timer["sourceKind"], json!("Timer"));
        let id = timer["id"].as_str().unwrap();
        assert_eq!(counter_at(&timer, "/dueAtMs"), Some(fx.h.now() + 5000));
        watchers.settle().unwrap();
        fx.h.advance(4999);
        watchers.settle().unwrap();
        assert_eq!(
            inspect(&app, &ctx, id)["state"],
            json!("Active"),
            "not before its due time"
        );
        fx.h.advance(1);
        assert_eq!(watchers.tick().unwrap().settled, 1);
        let record = inspect(&app, &ctx, id);
        assert_eq!(record["outcome"]["kind"], json!("Succeeded"), "{record}");
        assert_eq!(
            record["outcome"]["detail"],
            json!("the timer fired"),
            "{record}"
        );
        let delivered = notices(&fx, &session);
        assert_eq!(delivered.len(), 1, "{delivered:?}");
        assert_eq!(
            delivered[0]["source"], record["event"]["id"],
            "{delivered:?}"
        );
        app.shutdown().await;
    });
}

/// A probe command the shell preflight blocks is refused when the watcher is created.
#[test]
fn a_probe_command_the_shell_preflight_blocks_is_refused_at_creation() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    let work = fx.h.root.with_extension("watch-preflight-work");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(work.join("work")).unwrap();
    let work = work.canonicalize().unwrap();
    let rt = runtime();
    rt.block_on(async {
        let (composed, _wired) = watch_composition(&fx, &work);
        let app = Arc::new(boot(composed).await.unwrap());
        let (ctx, _) = agent_context(&fx);
        let refused = decision(
            &app,
            &ctx,
            probe("git push --force origin main", 1000, json!({})),
            "w-blocked",
        );
        let DomainDecision::Refused { fault } = refused else {
            panic!("a blocked probe was admitted: {refused:?}");
        };
        assert_eq!(fault.code, DomainErrorCode::PermissionDenied, "{fault:?}");
        let session = ctx.session_id().unwrap().clone();
        let watchers = read(&**app.store(), |s| {
            s.watchers()
                .watchers(&WatcherFilter::Session(session.clone()), &[], None, 10)
        });
        assert!(watchers.is_empty(), "nothing was recorded: {watchers:?}");
        app.shutdown().await;
    });
}

/// A process that exits before its launch report is consumed still settles with its real exit,
/// never Lost: the outcome is recorded once the launch was reported, whatever the timing.
#[test]
fn an_immediately_exiting_watched_process_keeps_its_exit_status() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    let work = fx.h.root.with_extension("watch-exit-work");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(work.join("work")).unwrap();
    let work = work.canonicalize().unwrap();
    let rt = runtime();
    rt.block_on(async {
        let (composed, _wired) = watch_composition(&fx, &work);
        let app = Arc::new(boot(composed).await.unwrap());
        let (ctx, _) = agent_context(&fx);
        let ids: Vec<String> = (0..24)
            .map(|n| {
                create(&app, &ctx, "exit 7", 10_000, &format!("exit-{n}"))["id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        settle_all(&app, &ctx, &refs).await;
        for watcher in &refs {
            let outcome = inspect(&app, &ctx, watcher)["outcome"].clone();
            assert_eq!(outcome["kind"], json!("Failed"), "{watcher}: {outcome}");
            assert_eq!(outcome["exitCode"], json!(7), "{watcher}: {outcome}");
        }
        app.shutdown().await;
    });
}
