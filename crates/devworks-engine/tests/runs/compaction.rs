//! Automatic context compaction at `agents.compactionThresholdPercent`. An agent turn whose next
//! request reaches the configured share of its model's context window first sends a compaction
//! request and continues from the summary in a new context epoch; below the threshold it does
//! not. Provider bytes are scripted; no model runs.
use super::approval_wait::{Body, Entropy, pinned_agent_session};
use super::*;
use devworks_core::protocol::provider::{ContextChangeReason, TranscriptBody};

/// Text of the host's compaction request, as the scripted provider recognizes it.
const COMPACTION_REQUEST: &str = "will be compacted";
const SUMMARY: &str = "SUMMARY-OF-THE-WORK-SO-FAR";
/// Each generation answers with this marker padded to about 64,000 estimated tokens.
const LONG_ANSWER: &str = "LONG-ANSWER";

fn long_answer() -> String {
    format!("{LONG_ANSWER} {}", "x".repeat(256 * 1024))
}

/// Answers a compaction request with the next of `summaries`, then [`SUMMARY`], and every other
/// request with the next of `answers`, then a long answer. With `crash`, a compaction request
/// panics the worker that sends it, as a process that stops mid-request would leave it. Every
/// request body is kept.
#[derive(Default)]
struct CompactionWire {
    bodies: Mutex<Vec<String>>,
    summaries: Mutex<std::collections::VecDeque<String>>,
    answers: Mutex<std::collections::VecDeque<String>>,
    crash: AtomicBool,
}
impl StreamingHttp for CompactionWire {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        let body = String::from_utf8(request.body.clone().unwrap_or_default()).unwrap();
        let compaction = body.contains(COMPACTION_REQUEST);
        self.bodies.lock().unwrap().push(body);
        if compaction && self.crash.load(Ordering::SeqCst) {
            panic!("the process stops while the compaction request runs");
        }
        let text = if compaction {
            self.summaries.lock().unwrap().pop_front()
        } else {
            self.answers.lock().unwrap().pop_front()
        }
        .unwrap_or_else(|| {
            if compaction {
                SUMMARY.to_owned()
            } else {
                long_answer()
            }
        });
        let delta =
            json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":"stop"}]});
        Ok(Box::new(Body(Some(
            format!("data: {delta}\n\ndata: [DONE]\n\n").into_bytes(),
        ))))
    }
}

async fn compaction_runtime(fx: &Fx, wire: Arc<CompactionWire>) -> Arc<devworks_engine::Runtime> {
    runtime_on(fx, wire, Arc::new(FakeHost::default())).await
}

/// A host process with an identity, so that a later boot can tell it has stopped.
#[derive(Default)]
struct StoppingHost(FakeHost);
impl ProcessHost for StoppingHost {
    fn platform(&self) -> &'static str {
        self.0.platform()
    }
    fn environment(&self) -> Vec<(String, String)> {
        self.0.environment()
    }
    fn spawn(&self, spec: &ProcessSpec) -> Result<Box<dyn ProcessTree>, ProcessError> {
        self.0.spawn(spec)
    }
    fn current_identity(&self) -> Option<NativeIdentity> {
        Some(NativeIdentity {
            pid: 4343,
            birth: "stopping-host".into(),
            group: None,
        })
    }
    /// Every earlier boot's process has stopped.
    fn probe(&self, identity: &NativeIdentity) -> Probe {
        self.0.probe(identity)
    }
    fn terminate(&self, identity: &NativeIdentity) -> Probe {
        self.0.terminate(identity)
    }
}

async fn runtime_on(
    fx: &Fx,
    wire: Arc<CompactionWire>,
    host: Arc<dyn ProcessHost>,
) -> Arc<devworks_engine::Runtime> {
    let mut composed = composition_with(
        fx,
        None,
        Arc::new(FakePower::default()),
        Arc::new(Authority),
        host,
    );
    composed.application = devworks_engine::ApplicationComposition {
        connections: Some(devworks_engine::ConnectionPorts {
            http: Arc::new(devworks_core::ports::http::NoNetwork),
            secrets: Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
            entropy: Arc::new(Entropy),
            endpoints: None,
        }),
        router: Some(Default::default()),
        ..Default::default()
    };
    composed.config.providers =
        Some(devworks_engine::providers::driver::ProviderComposition::new(wire));
    Arc::new(boot(composed).await.unwrap())
}

#[test]
fn workspace_conversation_compacts_before_reply_at_the_routed_threshold() {
    exercise_workspace_compaction(false);
}

#[test]
fn workspace_compaction_refused_before_transport_leaves_the_conversation_available() {
    exercise_workspace_compaction(true);
}

fn exercise_workspace_compaction(refuse_once: bool) {
    use super::workspace_reply::{open_home, send_workspace, wait_for_replies, workspace_ports};
    use devworks_core::context::{HostIngress, WorkspaceReply};
    use devworks_core::protocol::Counter;
    let _native_case = native_runtime_case();
    let f = Fx::new();
    seed_local_route(&f);
    let set_model_window = |size| {
        f.tx(move |_, tx| {
            let mut model = tx.router().route_model(&id("watch-model"))?.unwrap();
            let previous = model.revision;
            model.context_tokens = Some(size);
            model.revision += 1;
            tx.router().put_route_model(&model, Some(previous))
        })
    };
    set_model_window(1_000_000);
    let wire = Arc::new(CompactionWire::default());
    wire.answers
        .lock()
        .unwrap()
        .extend([long_answer(), "Reply after summary".into()]);
    runtime().block_on(async {
        let mut ports = workspace_ports(&f, wire.clone());
        if refuse_once {
            let files = FaultFiles::over(Arc::new(NativeArtifactFiles::new(
                f.h.root.clone(),
                f.h.root.with_extension("cache"),
            )));
            let store = Arc::downgrade(&f.store);
            let reject = AtomicBool::new(true);
            files.before(FileOp::Publish, move |call| {
                let Some(store) = store.upgrade() else {
                    return Ok(());
                };
                let Some(staging) = call
                    .name
                    .strip_suffix(".part")
                    .and_then(|id| Id::new(id).ok())
                else {
                    return Ok(());
                };
                let staged = read(&*store, |r| r.operations().operation(&OperationId(staging)));
                if staged
                    .is_some_and(|op| op.payload["source"]["kind"] == "ProviderCompactionInput")
                    && reject.swap(false, Ordering::SeqCst)
                {
                    write(&*store, |tx| {
                        let now = tx.now_ms();
                        tx.router()
                            .record_cooldown(&StoredCooldown {
                                model_id: id("watch-model"),
                                until_ms: now + 60_000,
                                reason: "RateLimited".into(),
                                observed_at_ms: now,
                            })
                            .map(|_| ())
                    })
                    .unwrap();
                }
                Ok(())
            });
            ports.artifacts = ArtifactComposition::new(
                files,
                Arc::new(HostArtifactSources::new(f.h.clock(), f.h.ids.clone())),
                Arc::new(NativeFilesystem::new()),
            );
        }
        let app = boot(ports).await.unwrap();
        let session = open_home(&f, &app);
        send_workspace(
            &f,
            &app,
            &session,
            "home-before-compaction",
            "Keep the original brief",
            "watch-model",
        );
        wait_for_replies(&f, 1).await;
        let session_record = read(&*f.store, |r| r.sessions().session(&session)).unwrap();
        let info = f.store.info();
        let ctx = HostIngress::new(&*f.store)
            .context(
                session_record.agent_id.clone(),
                PrincipalKind::Agent,
                id("measure-home-compaction"),
                id("workspace-compaction-test"),
                Counter::new(info.restore_epoch).unwrap(),
                Revision::new(1).unwrap(),
            )
            .with_session(session.clone())
            .with_workspace_reply(WorkspaceReply {
                user_sequence: session_record.last_durable_sequence(),
                preference: None,
            });
        let engine = app.turn_engine();
        let (head, _, _) = engine
            .recover_native_context_head(&ctx, &session)
            .unwrap()
            .unwrap();
        let measured =
            devworks_engine::agent_turns::context_tokens(engine, &ctx, &head, 0).unwrap();
        let saved = read(&*f.store, |r| r.sessions().entries(&session, 0, 30));
        let response_bytes = saved
            .iter()
            .find_map(|entry| {
                entry.body["visibleText"]
                    .as_str()
                    .filter(|text| text.starts_with(LONG_ANSWER))
                    .map(str::len)
            })
            .expect("the long response was durably recorded");
        // Choose a window whose 80% boundary falls between the measured context and
        // its fixed prefix. The set of composed tool definitions can grow independently.
        set_model_window((measured - response_bytes as i64 / 8) * 100 / 80);

        send_workspace(
            &f,
            &app,
            &session,
            "home-after-compaction",
            "Continue the same conversation",
            "watch-model",
        );
        if refuse_once {
            within(
                "a known compaction refusal settles without opening a transport",
                async {
                    loop {
                        let status = app.workspace_reply_status(&f.user, &session).unwrap();
                        if status == Some(devworks_engine::WorkspaceReplyStatus::Unavailable) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                },
            )
            .await;
            assert_eq!(wire.bodies.lock().unwrap().len(), 1);
            f.tx(|_, tx| tx.router().clear_cooldown(&id("watch-model")).map(|_| ()));
            send_workspace(
                &f,
                &app,
                &session,
                "home-retry-compaction",
                "Continue the same conversation",
                "watch-model",
            );
            // The earlier known refusal stays visible in its own completed operation.
            within(
                "the next workspace reply settles after availability returns",
                async {
                    loop {
                        let status = read(&*f.store, |r| {
                            r.queue().status(
                                devworks_core::services::sessions::workspace_reply::CONSUMER,
                            )
                        });
                        if status.completed == 3 {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                },
            )
            .await;
        } else {
            wait_for_replies(&f, 2).await;
        }
        {
            let requests = wire.bodies.lock().unwrap();
            let [initial, compaction, reply] = requests.as_slice() else {
                panic!(
                    "expected one compaction between two replies, got {}",
                    requests.len()
                );
            };
            assert!(
                initial.contains("Keep the original brief")
                    && !initial.contains(COMPACTION_REQUEST)
            );
            assert!(compaction.contains(COMPACTION_REQUEST) && compaction.contains(LONG_ANSWER));
            assert!(reply.contains(SUMMARY) && !reply.contains(LONG_ANSWER));
            assert!(reply.contains("Continue the same conversation"));
            assert!(requests.iter().all(|r| r.contains("fixture-model")));
        }
        let entries = read(&*f.store, |r| r.sessions().entries(&session, 0, 30));
        assert!(
            entries
                .iter()
                .any(|e| e.kind == "ContextEpochChanged" && e.body["reason"] == "Compaction")
        );
        assert!(entries.iter().any(|e| {
            e.kind == "AssistantAttempt"
                && e.body["visibleText"]
                    .as_str()
                    .is_some_and(|text| text.starts_with(LONG_ANSWER))
        }));
        app.shutdown().await;
    });
}

/// Sets the pinned fixture model's context window.
fn set_context_window(fx: &Fx, tokens: i64) {
    fx.tx(move |_, tx| {
        let mut model = tx
            .router()
            .route_model(&id("approval-wait-model"))?
            .ok_or(StorageError::ScopeDenied)?;
        let expected = model.revision;
        model.context_tokens = Some(tokens);
        model.revision += 1;
        tx.router().put_route_model(&model, Some(expected))?;
        Ok(())
    });
}

fn set_threshold(fx: &Fx, percent: i64) {
    let schema = devworks_core::settings::catalog::descriptor("agents.compactionThresholdPercent")
        .unwrap()
        .schema_ref();
    fx.tx(move |_, tx| {
        // Keeps the workspace's other settings.
        let current = tx
            .configuration()
            .current(&ConfigScope::Workspace, "settings")?;
        let mut body = current
            .as_ref()
            .map_or_else(|| json!({"values": {}}), |current| current.body.clone());
        body["values"]["agents.compactionThresholdPercent"] =
            json!({"schema": schema, "value": percent});
        tx.configuration().append_revision(
            &ConfigScope::Workspace,
            "settings",
            current.map_or(1, |current| current.revision),
            1,
            body,
        )?;
        Ok(())
    });
}

async fn completed_turn(
    app: &Arc<devworks_engine::Runtime>,
    context: &InvocationContext,
    primary: &devworks_core::protocol::records::AgentRecord,
) {
    let (_stop_tx, stop) = tokio::sync::watch::channel(false);
    let end = NativeAgentTurnExecutor::new(app)
        .run(context.clone(), primary.clone(), stop)
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
}

struct SecondTurn {
    bodies: Vec<String>,
    epochs: Vec<(i64, ContextChangeReason)>,
    attempts: Vec<StoredInferenceAttempt>,
    tokens_before: i64,
    tokens_after: i64,
    /// Estimated tokens of a request from the first compacted context, if one was published.
    compacted_tokens: Option<i64>,
    window: i64,
}

/// One completed turn with a long answer under a large window; then a window at which the next
/// request is 60% full, a threshold of `percent` (unset: the default) and a second turn; and
/// with `third`, a third turn.
fn second_turn(percent: Option<i64>, third: bool) -> SecondTurn {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let wire = Arc::new(CompactionWire::default());
            let app = compaction_runtime(&fx, wire.clone()).await;
            let (context, primary) = pinned_agent_session(&fx);
            let session_id = context.session_id().unwrap().clone();
            set_context_window(&fx, 10_000_000);
            completed_turn(&app, &context, &primary).await;
            assert_eq!(wire.bodies.lock().unwrap().len(), 1, "one tool window");
            let (head, _, _) = app
                .recover_native_context_head(&context, &session_id)
                .unwrap()
                .unwrap();
            let tokens_before =
                devworks_engine::agent_turns::context_tokens(app.turn_engine(), &context, &head, 0)
                    .unwrap();
            // The next request is at 60% of this window: over a 50% threshold, under 80%.
            let window = tokens_before * 100 / 60;
            set_context_window(&fx, window);
            if let Some(percent) = percent {
                set_threshold(&fx, percent);
            }
            completed_turn(&app, &context, &primary).await;
            let (head, _, _) = app
                .recover_native_context_head(&context, &session_id)
                .unwrap()
                .unwrap();
            let tokens_after =
                devworks_engine::agent_turns::context_tokens(app.turn_engine(), &context, &head, 0)
                    .unwrap();
            if third {
                completed_turn(&app, &context, &primary).await;
            }
            let compacted_tokens =
                read(&*fx.store, |s| s.sessions().epoch(&session_id, 1)).map(|epoch| {
                    let root = serde_json::from_value(epoch.context).unwrap();
                    devworks_engine::agent_turns::context_tokens(
                        app.turn_engine(),
                        &context,
                        &root,
                        0,
                    )
                    .unwrap()
                });
            let epochs = read(&*fx.store, |s| s.sessions().entries(&session_id, 0, 500))
                .into_iter()
                .filter_map(|entry| {
                    match serde_json::from_value::<TranscriptBody>(entry.body).unwrap() {
                        TranscriptBody::ContextEpochChanged {
                            next_epoch, reason, ..
                        } => Some((next_epoch.get(), reason)),
                        _ => None,
                    }
                })
                .collect();
            let attempts = read(&*fx.store, |s| {
                s.usage().inference_attempts(
                    &UsageAttemptFilter {
                        session_id: Some(session_id.clone()),
                        ..Default::default()
                    },
                    None,
                    100,
                )
            });
            let bodies = wire.bodies.lock().unwrap().clone();
            Arc::try_unwrap(app)
                .ok()
                .expect("runtime has one owner")
                .shutdown()
                .await;
            SecondTurn {
                bodies,
                epochs,
                attempts,
                tokens_before,
                tokens_after,
                compacted_tokens,
                window,
            }
        })
}

#[test]
fn a_context_over_the_threshold_is_compacted_before_the_next_request() {
    let turn = second_turn(Some(50), true);
    // The first turn's request, a compaction request, the second turn's request (its long
    // answer brings the context back to 60%), a compaction request in the compacted epoch, then
    // the third turn's request.
    assert_eq!(
        turn.bodies.len(),
        5,
        "{:?} before {} window {} compacted {:?}",
        turn.epochs,
        turn.tokens_before,
        turn.window,
        turn.compacted_tokens
    );
    let compaction = &turn.bodies[1];
    assert!(compaction.contains(COMPACTION_REQUEST));
    assert!(
        compaction.contains(LONG_ANSWER),
        "the summary covers the long answer"
    );
    assert!(
        !compaction.contains("\"tools\""),
        "a compaction request offers no tools"
    );
    let next = &turn.bodies[2];
    assert!(next.contains(SUMMARY));
    assert!(
        !next.contains(LONG_ANSWER),
        "the summary replaced the long answer"
    );
    assert!(!next.contains(COMPACTION_REQUEST));
    // From the second epoch: the earlier summary is compacted again and replaced, not kept.
    let again = &turn.bodies[3];
    assert!(again.contains(COMPACTION_REQUEST));
    assert!(again.contains(SUMMARY) && again.contains(LONG_ANSWER));
    let last = &turn.bodies[4];
    assert_eq!(last.matches(SUMMARY).count(), 1, "{last}");
    assert!(!last.contains(LONG_ANSWER));
    assert!(
        matches!(
            turn.epochs.as_slice(),
            [
                (1, ContextChangeReason::Compaction),
                (2, ContextChangeReason::Compaction)
            ]
        ),
        "{:?}",
        turn.epochs
    );
    let compacted = turn.compacted_tokens.expect("a compacted context");
    assert!(
        compacted * 2 < turn.window,
        "the compacted context is under 50%"
    );
    let purposes: Vec<_> = turn
        .attempts
        .iter()
        .map(|attempt| attempt.purpose.as_str())
        .collect();
    assert_eq!(
        purposes.iter().filter(|p| **p == "Compaction").count(),
        2,
        "{purposes:?}"
    );
    assert!(
        turn.attempts
            .iter()
            .any(|attempt| attempt.request_id.as_str().starts_with("agent-compaction-")),
    );
}

#[test]
fn a_context_under_the_threshold_is_not_compacted() {
    // The same 60% context under the default 80% threshold.
    let turn = second_turn(None, false);
    assert_eq!(turn.bodies.len(), 2);
    assert!(
        turn.bodies
            .iter()
            .all(|body| !body.contains(COMPACTION_REQUEST))
    );
    assert!(turn.bodies[1].contains(LONG_ANSWER));
    assert!(turn.epochs.is_empty(), "{:?}", turn.epochs);
    assert!(turn.compacted_tokens.is_none());
    assert!(turn.tokens_after > turn.tokens_before);
}

fn in_runtime(test: impl AsyncFnOnce()) {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(test());
}

async fn shut_down(app: Arc<devworks_engine::Runtime>) {
    Arc::try_unwrap(app)
        .ok()
        .expect("runtime has one owner")
        .shutdown()
        .await;
}

async fn run_turn(
    app: &Arc<devworks_engine::Runtime>,
    context: &InvocationContext,
    primary: &devworks_core::protocol::records::AgentRecord,
) -> LaunchEnd {
    let (_stop_tx, stop) = tokio::sync::watch::channel(false);
    tokio::time::timeout(
        Duration::from_secs(180),
        NativeAgentTurnExecutor::new(app).run(context.clone(), primary.clone(), stop),
    )
    .await
    .expect("the agent turn did not end within the bound")
}

/// The context epochs the session's transcript records.
fn epochs(fx: &Fx, session_id: &Id) -> Vec<(i64, ContextChangeReason)> {
    read(&*fx.store, |s| s.sessions().entries(session_id, 0, 500))
        .into_iter()
        .filter_map(
            |entry| match serde_json::from_value::<TranscriptBody>(entry.body).unwrap() {
                TranscriptBody::ContextEpochChanged {
                    next_epoch, reason, ..
                } => Some((next_epoch.get(), reason)),
                _ => None,
            },
        )
        .collect()
}

/// The compaction requests the host recorded, by ID.
fn compaction_requests(fx: &Fx) -> Vec<Id> {
    read(&*fx.store, |s| s.router().route_requests(None, 100))
        .into_iter()
        .map(|request| request.request_id)
        .filter(|id| id.as_str().starts_with("agent-compaction-"))
        .collect()
}

/// After one completed turn under a large window: a window at which the next request is 60%
/// full and a 50% threshold.
async fn over_the_threshold(
    fx: &Fx,
    app: &Arc<devworks_engine::Runtime>,
    context: &InvocationContext,
    primary: &devworks_core::protocol::records::AgentRecord,
) {
    set_context_window(fx, 10_000_000);
    completed_turn(app, context, primary).await;
    let (head, _, _) = app
        .recover_native_context_head(context, context.session_id().unwrap())
        .unwrap()
        .unwrap();
    let tokens =
        devworks_engine::agent_turns::context_tokens(app.turn_engine(), context, &head, 0).unwrap();
    set_context_window(fx, tokens * 100 / 60);
    set_threshold(fx, 50);
}

/// Claims the root's current activation after a restart and binds the primary agent's context
/// to it: the earlier boot's activation no longer runs turns.
fn bind_after_restart(
    fx: &Fx,
) -> (
    InvocationContext,
    devworks_core::protocol::records::AgentRecord,
) {
    applied(&fx.resume(&fx.user, "root-i"));
    let activation = fx.launch("compaction-after-restart");
    let host = principal(&*fx.store, "compaction-host", PrincipalKind::Host);
    let agents = AgentService::new(fx.h.ids.clone(), fx.service.clone());
    let (execution_id, epoch) = (activation.id.clone(), activation.owner_epoch);
    let primary = fx.tx(move |_, tx| {
        agents
            .materialize_primary(tx, &host, &execution_id, epoch)
            .map_err(|error| StorageError::InvalidRecord(format!("primary: {error:?}")))
    });
    let assignment = read(&*fx.store, |s| s.runs().assignment(&primary.assignment_id)).unwrap();
    let session = read(&*fx.store, |s| s.sessions().session(&assignment.session_id)).unwrap();
    let context = devworks_core::context::HostIngress::new(&*fx.store)
        .context(
            session.agent_id.clone(),
            PrincipalKind::Agent,
            id("compaction-invocation"),
            id("compaction-ingress"),
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

#[test]
fn a_restart_between_compaction_admission_and_epoch_publication_compacts_after_the_restart() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    let (session_id, admitted) = {
        let fx = &fx;
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let wire = Arc::new(CompactionWire::default());
                let app = runtime_on(fx, wire.clone(), Arc::new(StoppingHost::default())).await;
                let (context, primary) = pinned_agent_session(fx);
                let session_id = context.session_id().unwrap().clone();
                over_the_threshold(fx, &app, &context, &primary).await;
                // The compaction request is admitted and sent; the process stops before its
                // summary arrives, so no epoch is published.
                wire.crash.store(true, Ordering::SeqCst);
                let end = run_turn(&app, &context, &primary).await;
                assert!(
                    !matches!(
                        end,
                        LaunchEnd::Finished {
                            outcome: ActivationOutcome::Completed,
                            ..
                        }
                    ),
                    "{end:?}"
                );
                let admitted = compaction_requests(fx);
                assert_eq!(admitted.len(), 1, "{admitted:?}");
                assert!(epochs(fx, &session_id).is_empty());
                shut_down(app).await;
                (session_id, admitted)
            })
    };
    // The next boot opens the store again.
    let (h, store) = fx.into_parts();
    drop(store);
    let store = h.open();
    let fx = Fx::over(h, store);
    in_runtime(async || {
        let wire = Arc::new(CompactionWire::default());
        let app = runtime_on(&fx, wire.clone(), Arc::new(StoppingHost::default())).await;
        // Boot recovery settles the stopped boot's attempt as an unknown tail.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !read(&*fx.store, |s| {
            s.runs().inference_quiescent(&RootId(id("root-i")))
        }) {
            assert!(
                std::time::Instant::now() < deadline,
                "{:?}",
                read(&*fx.store, |s| s.usage().inference_attempts(
                    &UsageAttemptFilter::default(),
                    None,
                    100
                ))
                .iter()
                .map(|attempt| (&attempt.request_id, &attempt.state))
                .collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let (context, primary) = bind_after_restart(&fx);
        let end = run_turn(&app, &context, &primary).await;
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
        assert!(
            matches!(
                epochs(&fx, &session_id).as_slice(),
                [(1, ContextChangeReason::Compaction)]
            ),
            "{:?}",
            epochs(&fx, &session_id)
        );
        let bodies = wire.bodies.lock().unwrap().clone();
        assert_eq!(
            bodies.len(),
            2,
            "a compaction request, then the turn's request"
        );
        assert!(bodies[0].contains(COMPACTION_REQUEST));
        assert!(bodies[1].contains(SUMMARY) && !bodies[1].contains(LONG_ANSWER));
        // The restarted compaction is its own request: the stopped one ended with an unknown
        // tail, which a replay would return again instead of a summary.
        let requests = compaction_requests(&fx);
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert!(requests.contains(&admitted[0]));
        shut_down(app).await;
    });
}

#[test]
fn a_context_whose_summary_alone_reaches_the_threshold_is_compacted_again() {
    const LONG_SUMMARY: &str = "LONG-SUMMARY";
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    in_runtime(async || {
        let wire = Arc::new(CompactionWire::default());
        let app = compaction_runtime(&fx, wire.clone()).await;
        let (context, primary) = pinned_agent_session(&fx);
        let session_id = context.session_id().unwrap().clone();
        over_the_threshold(&fx, &app, &context, &primary).await;
        // The first summary alone is about 60% of the window; later answers are short.
        wire.summaries
            .lock()
            .unwrap()
            .push_back(format!("{LONG_SUMMARY} {}", "y".repeat(256 * 1024)));
        wire.answers
            .lock()
            .unwrap()
            .extend(["short".to_owned(), "short".to_owned()]);
        completed_turn(&app, &context, &primary).await;
        let (head, _, _) = app
            .recover_native_context_head(&context, &session_id)
            .unwrap()
            .unwrap();
        let tokens =
            devworks_engine::agent_turns::context_tokens(app.turn_engine(), &context, &head, 0)
                .unwrap();
        let window = read(&*fx.store, |s| {
            s.router().route_model(&id("approval-wait-model"))
        })
        .unwrap()
        .context_tokens
        .unwrap();
        assert!(tokens * 2 >= window, "{tokens} of {window}");
        completed_turn(&app, &context, &primary).await;
        let bodies = wire.bodies.lock().unwrap().clone();
        // The first turn's request; the second turn's compaction request and its request from
        // the long summary; the third turn's compaction request and its request.
        assert_eq!(bodies.len(), 5, "{:?}", epochs(&fx, &session_id));
        assert!(bodies[3].contains(COMPACTION_REQUEST) && bodies[3].contains(LONG_SUMMARY));
        assert!(bodies[4].contains(SUMMARY) && !bodies[4].contains(LONG_SUMMARY));
        assert!(
            matches!(
                epochs(&fx, &session_id).as_slice(),
                [
                    (1, ContextChangeReason::Compaction),
                    (2, ContextChangeReason::Compaction)
                ]
            ),
            "{:?}",
            epochs(&fx, &session_id)
        );
        shut_down(app).await;
    });
}

/// The first request gets one call to the desktop takeover tool, which needs approval. The
/// first compaction request moves the call's operation to a terminal state, as a person's
/// rejection would, without publishing its final update. A compaction request gets
/// [`SUMMARY`]. Every other
/// request gets a long answer. Every request body is kept.
struct SettlingWire {
    store: Arc<dyn ApplicationStore>,
    arguments: Mutex<String>,
    operation: Mutex<Option<OperationId>>,
    bodies: Mutex<Vec<String>>,
}
impl StreamingHttp for SettlingWire {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        let body = String::from_utf8(request.body.clone().unwrap_or_default()).unwrap();
        let compaction = body.contains(COMPACTION_REQUEST);
        let first = {
            let mut bodies = self.bodies.lock().unwrap();
            bodies.push(body);
            bodies.len() == 1
        };
        let delta = if first {
            let arguments = self.arguments.lock().unwrap().clone();
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-held-0",
                "type":"function","function":{"name":"desktop_request_control_command",
                "arguments":arguments}}]},"finish_reason":"tool_calls"}]})
        } else {
            let text = if compaction {
                if let Some(operation) = self.operation.lock().unwrap().take() {
                    write(self.store.as_ref(), move |tx| {
                        let current = tx.operations().operation(&operation)?.unwrap();
                        tx.operations().transition(
                            &operation,
                            current.revision,
                            None,
                            devworks_core::protocol::OperationState::CancelledBeforeDispatch,
                            None,
                        )?;
                        Ok(())
                    })
                    .unwrap();
                }
                SUMMARY.to_owned()
            } else {
                long_answer()
            };
            json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":"stop"}]})
        };
        Ok(Box::new(Body(Some(
            format!("data: {delta}\n\ndata: [DONE]\n\n").into_bytes(),
        ))))
    }
}

#[test]
fn a_call_whose_operation_turns_terminal_during_the_summary_request_keeps_its_final_result() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    in_runtime(async || {
        let wire = Arc::new(SettlingWire {
            store: fx.store.clone(),
            arguments: Mutex::new("{}".into()),
            operation: Mutex::new(None),
            bodies: Mutex::new(Vec::new()),
        });
        let (app, context, primary, arguments) =
            super::approval_wait::takeover_with(&fx, wire.clone()).await;
        *wire.arguments.lock().unwrap() = arguments.to_string();
        let session_id = context.session_id().unwrap().clone();
        // The call is answered ApprovalRequired; its operation awaits a person.
        over_the_threshold(&fx, &app, &context, &primary).await;
        let (_, operation) = super::approval_wait::answered(&fx, &context, "call-held-0");
        *wire.operation.lock().unwrap() = Some(operation);

        let end = run_turn(&app, &context, &primary).await;
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
        assert!(
            matches!(
                epochs(&fx, &session_id).as_slice(),
                [(1, ContextChangeReason::Compaction)]
            ),
            "{:?}",
            epochs(&fx, &session_id)
        );
        let bodies = wire.bodies.lock().unwrap().clone();
        assert_eq!(
            bodies.len(),
            4,
            "two requests, a compaction request, a request"
        );
        assert!(bodies[2].contains(COMPACTION_REQUEST));
        let next: Value = serde_json::from_str(&bodies[3]).unwrap();
        let messages = next["messages"].as_array().unwrap();
        assert!(
            messages.iter().any(|message| message["tool_calls"]
                .as_array()
                .is_some_and(|calls| calls.iter().any(|call| call["id"] == "call-held-0"))),
            "{messages:?}"
        );
        let result = messages
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == "call-held-0")
            .unwrap_or_else(|| panic!("no result for the call: {messages:?}"))["content"]
            .to_string();
        assert!(result.contains("ApprovalRequired"), "{result}");
        assert!(result.contains("CancelledBeforeDispatch"), "{result}");
        assert!(bodies[3].contains(SUMMARY) && !bodies[3].contains(LONG_ANSWER));

        // The next compaction summarizes the settled call with the rest: its update was
        // published before that summary request, so the call is not kept again.
        let end = run_turn(&app, &context, &primary).await;
        let bodies = wire.bodies.lock().unwrap().clone();
        assert!(
            matches!(
                end,
                LaunchEnd::Finished {
                    outcome: ActivationOutcome::Completed,
                    ..
                }
            ),
            "{end:?} after {} requests",
            bodies.len()
        );
        assert_eq!(bodies.len(), 6, "a compaction request, then a request");
        assert!(bodies[4].contains(COMPACTION_REQUEST) && bodies[4].contains("call-held-0"));
        assert!(!bodies[5].contains("call-held-0"), "{}", bodies[5]);
        shut_down(app).await;
    });
}

/// Appends `count` one-character user messages to the agent's session and folds them into its
/// context head, at most 100 per context revision.
fn append_messages(
    fx: &Fx,
    app: &devworks_engine::Runtime,
    context: &InvocationContext,
    count: usize,
) {
    use devworks_core::services::sessions::{
        transcript::{BatchOutcome, SessionBatch, SessionWriter},
        turn,
    };
    let session_id = context.session_id().unwrap().clone();
    for start in (0..count).step_by(100) {
        let n = (count - start).min(100);
        let (append_context, append_session) = (context.clone(), session_id.clone());
        fx.tx(move |_, tx| {
            let session = tx.sessions().session(&append_session)?.unwrap();
            let bodies = (0..n)
                .map(|_| TranscriptBody::UserMessage {
                    content: vec![devworks_core::protocol::provider::UserContent::Text {
                        text: ".".into(),
                    }],
                })
                .collect();
            let batch = turn::batch_of(
                &append_session,
                tx.new_id(devworks_core::ports::IdKind::TranscriptBatch),
                session.context_epoch,
                session.next_sequence,
                bodies,
                || tx.new_id(devworks_core::ports::IdKind::TranscriptEntry),
            );
            assert!(matches!(
                SessionWriter::append_in(tx, &append_context, &SessionBatch::entries(batch))?,
                BatchOutcome::Accepted(_)
            ));
            Ok(())
        });
        let (head, _, _) = app
            .recover_native_context_head(context, &session_id)
            .unwrap()
            .unwrap();
        app.refresh_native_tool_context(context, &session_id, &head)
            .unwrap();
    }
}

/// After `count` more user messages: a window at which the next request is 60% full, and one
/// turn, which compacts. Returns the epoch it published.
async fn compact_after_messages(
    fx: &Fx,
    app: &Arc<devworks_engine::Runtime>,
    context: &InvocationContext,
    primary: &devworks_core::protocol::records::AgentRecord,
    count: usize,
) -> i64 {
    let session_id = context.session_id().unwrap();
    append_messages(fx, app, context, count);
    let (head, binding, _) = app
        .recover_native_context_head(context, session_id)
        .unwrap()
        .unwrap();
    let tokens =
        devworks_engine::agent_turns::context_tokens(app.turn_engine(), context, &head, 0).unwrap();
    set_context_window(fx, tokens * 100 / 60);
    let end = run_turn(app, context, primary).await;
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
    let next = binding.context_epoch.get() + 1;
    let epoch = read(&*fx.store, |s| s.sessions().epoch(session_id, next)).unwrap();
    assert_eq!(epoch.reason, "Compaction");
    next
}

#[test]
fn a_call_summarized_long_ago_does_not_stop_compaction() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    in_runtime(async || {
        let wire = Arc::new(SettlingWire {
            store: fx.store.clone(),
            arguments: Mutex::new("{}".into()),
            operation: Mutex::new(None),
            bodies: Mutex::new(Vec::new()),
        });
        let (app, context, primary, arguments) =
            super::approval_wait::takeover_with(&fx, wire.clone()).await;
        *wire.arguments.lock().unwrap() = arguments.to_string();
        let session_id = context.session_id().unwrap().clone();
        set_context_window(&fx, 10_000_000);
        set_threshold(&fx, 50);
        completed_turn(&app, &context, &primary).await;
        // The call's operation settles and its final update is published at once.
        let (_, operation) = super::approval_wait::answered(&fx, &context, "call-held-0");
        let settled = operation.clone();
        write(fx.store.as_ref(), move |tx| {
            let current = tx.operations().operation(&settled)?.unwrap();
            tx.operations().transition(
                &settled,
                current.revision,
                None,
                devworks_core::protocol::OperationState::CancelledBeforeDispatch,
                None,
            )?;
            Ok(())
        })
        .unwrap();
        assert!(
            app.record_tool_operation_update(&context, &session_id, &operation)
                .unwrap()
                .is_some()
        );
        let (head, _, _) = app
            .recover_native_context_head(&context, &session_id)
            .unwrap()
            .unwrap();
        app.refresh_native_tool_context(&context, &session_id, &head)
            .unwrap();

        // The first compaction summarizes the settled call with the rest.
        assert_eq!(
            compact_after_messages(&fx, &app, &context, &primary, 4_000).await,
            1
        );
        let first = wire.bodies.lock().unwrap()[3].clone();
        assert!(first.contains(SUMMARY) && !first.contains("call-held-0"));
        // A context holds at most 4,096 messages, so more entries than one update scan reads
        // follow the call's response across further compactions.
        assert_eq!(
            compact_after_messages(&fx, &app, &context, &primary, 4_000).await,
            2
        );
        assert_eq!(
            compact_after_messages(&fx, &app, &context, &primary, 2_100).await,
            3
        );
        let last = read(&*fx.store, |s| s.sessions().session(&session_id))
            .unwrap()
            .last_durable_sequence();
        let response = read(&*fx.store, |s| {
            s.sessions().linked_operations(&session_id, 500)
        })[0]
            .response_sequence
            .unwrap();
        assert!(last - response > 10_000, "{response} to {last}");
        assert_eq!(wire.bodies.lock().unwrap().len(), 8);
        shut_down(app).await;
    });
}
