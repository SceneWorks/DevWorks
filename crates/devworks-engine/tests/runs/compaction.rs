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
/// Each generation answers with this marker padded to about 16,000 estimated tokens.
const LONG_ANSWER: &str = "LONG-ANSWER";

/// Answers a compaction request with [`SUMMARY`] and every other request with a long answer.
/// Every request body is kept.
#[derive(Default)]
struct CompactionWire {
    bodies: Mutex<Vec<String>>,
}
impl StreamingHttp for CompactionWire {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        let body = String::from_utf8(request.body.clone().unwrap_or_default()).unwrap();
        let text = if body.contains(COMPACTION_REQUEST) {
            SUMMARY.to_owned()
        } else {
            format!("{LONG_ANSWER} {}", "x".repeat(64 * 1024))
        };
        self.bodies.lock().unwrap().push(body);
        let delta =
            json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":"stop"}]});
        Ok(Box::new(Body(Some(
            format!("data: {delta}\n\ndata: [DONE]\n\n").into_bytes(),
        ))))
    }
}

async fn compaction_runtime(fx: &Fx, wire: Arc<CompactionWire>) -> Arc<devworks_engine::Runtime> {
    let mut composed = composition(fx, None, Arc::new(FakePower::default()));
    let connections = Arc::new(
        devworks_core::services::router::connections::ConnectionService::new(
            fx.h.ids.clone(),
            fx.h.clock(),
            fx.store.clone(),
            Arc::new(devworks_core::ports::http::NoNetwork),
            Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
            Arc::new(Entropy),
        ),
    );
    composed.use_cases = UseCases::new()
        .with_runs(fx.service.clone())
        .with_connections(connections.clone())
        .with_router(Arc::new(
            devworks_core::services::router::InferenceRouter::new(
                fx.h.ids.clone(),
                fx.h.clock(),
                connections,
            ),
        ));
    composed.config.providers =
        Some(devworks_engine::providers::driver::ProviderComposition::new(wire));
    Arc::new(boot(composed).await.unwrap())
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
        tx.configuration().append_revision(
            &ConfigScope::Workspace,
            "settings",
            1,
            1,
            json!({"values": {"agents.compactionThresholdPercent": {"schema": schema,
                "value": percent}}}),
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
                devworks_engine::agent_turns::context_tokens(&app, &context, &head, 0).unwrap();
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
                devworks_engine::agent_turns::context_tokens(&app, &context, &head, 0).unwrap();
            if third {
                completed_turn(&app, &context, &primary).await;
            }
            let compacted_tokens =
                read(&*fx.store, |s| s.sessions().epoch(&session_id, 1)).map(|epoch| {
                    let root = serde_json::from_value(epoch.context).unwrap();
                    devworks_engine::agent_turns::context_tokens(&app, &context, &root, 0).unwrap()
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
    assert_eq!(turn.bodies.len(), 5, "{:?}", turn.epochs);
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
