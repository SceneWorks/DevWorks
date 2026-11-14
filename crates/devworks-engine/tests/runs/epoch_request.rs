//! Two context epochs whose roots have identical bytes. An empty provider answer adds nothing to
//! the context, and a provider epoch change on an unchanged route copies it, so the next epoch's
//! root repeats the bytes an earlier epoch already sent. Provider bytes are scripted; no model
//! runs.
use super::approval_wait::{Body, pinned_agent_session, runtime_with};
use super::*;
use devworks_core::ports::provider_http::{
    HttpStream, StreamLimits, StreamingHttp, TransportError,
};
use devworks_core::protocol::provider::{ArtifactRef, NativeBinding};
use devworks_core::protocol::records::AgentRecord;

/// Answers every request with an empty completion and counts the requests.
struct EmptyWire(Arc<AtomicUsize>);
impl StreamingHttp for EmptyWire {
    fn open(
        &self,
        _: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Body(Some(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
              data: [DONE]\n\n"
                .to_vec(),
        ))))
    }
}

/// Generous for a debug build; it only turns a spinning turn into a failure.
const TURN_BOUND: Duration = Duration::from_secs(120);

async fn run_turn(
    app: &Arc<devworks_engine::Runtime>,
    context: &InvocationContext,
    agent: &AgentRecord,
) -> LaunchEnd {
    let (_stop_tx, stop) = tokio::sync::watch::channel(false);
    tokio::time::timeout(
        TURN_BOUND,
        NativeAgentTurnExecutor::new(app).run(context.clone(), agent.clone(), stop),
    )
    .await
    .expect("the agent turn did not end within the bound")
}

fn head(
    app: &devworks_engine::Runtime,
    context: &InvocationContext,
) -> (ArtifactRef, NativeBinding) {
    let session = context.session_id().unwrap();
    let (head, binding, _) = app
        .recover_native_context_head(context, session)
        .unwrap()
        .unwrap();
    (head, binding)
}

/// Starts a new provider epoch on the same route. The converted root has the same bytes as the
/// head it converts once the bootstrap output cap is gone.
fn next_epoch(app: &devworks_engine::Runtime, context: &InvocationContext) -> ArtifactRef {
    let (head, binding) = head(app, context);
    let next = NativeBinding {
        context_epoch: devworks_core::protocol::Counter::new(binding.context_epoch.get() + 1)
            .unwrap(),
        ..binding.clone()
    };
    app.change_provider_epoch(
        context,
        context.session_id().unwrap(),
        &head,
        &binding,
        &next,
    )
    .unwrap()
}

struct Repeated {
    app: Arc<devworks_engine::Runtime>,
    context: InvocationContext,
    agent: AgentRecord,
    requests: Arc<AtomicUsize>,
}

/// The provider requests the host recorded, by ID.
fn recorded_requests(fx: &Fx) -> Vec<Id> {
    read(&*fx.store, |s| s.router().route_requests(None, 100))
        .into_iter()
        .map(|request| request.request_id)
        .collect()
}

/// One completed turn in epoch 1, then epoch 2 whose root has that turn's context bytes.
async fn repeated_root(fx: &Fx) -> Repeated {
    let requests = Arc::new(AtomicUsize::new(0));
    let app = runtime_with(
        fx,
        Arc::new(Authority),
        Arc::new(EmptyWire(requests.clone())),
        |_| {},
    )
    .await;
    let (context, agent) = pinned_agent_session(fx);
    let pin = read(&*fx.store, |s| {
        s.router().session_pin(context.session_id().unwrap())
    })
    .unwrap();
    let route = devworks_core::services::router::choice_of(&pin).unwrap();
    devworks_engine::agent_turns::publish_first_context(&app, &context, &agent, &route).unwrap();
    // Epoch 1 drops the bootstrap output cap, so later conversions keep the bytes unchanged.
    let epoch_one = next_epoch(&app, &context);

    let end = run_turn(&app, &context, &agent).await;
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
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(recorded_requests(fx).len(), 1);
    let (answered, _) = head(&app, &context);
    assert_eq!(
        answered.sha256, epoch_one.sha256,
        "the empty answer added nothing"
    );

    let epoch_two = next_epoch(&app, &context);
    assert_eq!(epoch_two.sha256, epoch_one.sha256);
    assert_ne!(epoch_two.version_id, epoch_one.version_id);
    Repeated {
        app,
        context,
        agent,
        requests,
    }
}

#[test]
fn a_new_epoch_with_the_same_root_bytes_sends_its_own_request() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let t = repeated_root(&fx).await;
            assert_eq!(head(&t.app, &t.context).1.context_epoch.get(), 2);
            let end = run_turn(&t.app, &t.context, &t.agent).await;
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
            assert_eq!(t.requests.load(Ordering::SeqCst), 2);
            assert_eq!(recorded_requests(&fx).len(), 2);
            Arc::try_unwrap(t.app)
                .ok()
                .expect("runtime has one owner")
                .shutdown()
                .await;
        });
}

/// The request the agent turn would send next for the current head.
fn planned_request(fx: &Fx, app: &devworks_engine::Runtime, context: &InvocationContext) -> Id {
    let session = context.session_id().unwrap().clone();
    let assignment = context.assignment_id().unwrap().clone();
    let (execution, epoch) = context.activation().unwrap();
    let execution = execution.clone();
    let turn_session = session.clone();
    let turn = fx.tx(move |service, tx| {
        service
            .begin_turn(tx, &turn_session, &assignment, &execution, epoch)?
            .map_err(|fence| StorageError::InvalidRecord(format!("turn: {fence:?}")))
    });
    let pin = read(&*fx.store, |s| s.router().session_pin(&session)).unwrap();
    let route = devworks_core::services::router::choice_of(&pin).unwrap();
    let (head, binding) = head(app, context);
    let request =
        devworks_engine::agent_turns::plan_generation(app, context, &route, &binding, &head)
            .unwrap()
            .request
            .request_id;
    let turn_id = turn.turn_id;
    fx.tx(move |service, tx| {
        service
            .end_turn(tx, &session, &turn_id, true)?
            .map_err(|fence| StorageError::InvalidRecord(format!("turn end: {fence:?}")))
    });
    request
}

#[test]
fn a_request_refused_again_against_the_same_head_ends_the_turn() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let t = repeated_root(&fx).await;
            // Give the epoch-2 request the identity of the epoch-1 request already sent, as two
            // epochs sharing request bytes did before the epoch was part of the request ID.
            // The driver finds that request already dispatched for another epoch and refuses
            // the new one while the stored head still lags the epoch, every time.
            let next = planned_request(&fx, &t.app, &t.context);
            let [sent] = <[Id; 1]>::try_from(recorded_requests(&fx)).unwrap();
            let db = rusqlite::Connection::open(fx.h.root.join("workspace.sqlite")).unwrap();
            // The sent attempt keeps its own request ID.
            db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
            assert_eq!(
                db.execute(
                    "UPDATE router_requests SET request_id=?1 WHERE request_id=?2",
                    [next.as_str(), sent.as_str()],
                )
                .unwrap(),
                1
            );

            let end = run_turn(&t.app, &t.context, &t.agent).await;
            assert!(
                matches!(
                    &end,
                    LaunchEnd::Failed(ActivationFailure::Deterministic(detail))
                        if detail.contains("made no progress")
                ),
                "{end:?}"
            );
            assert_eq!(t.requests.load(Ordering::SeqCst), 1);
            Arc::try_unwrap(t.app)
                .ok()
                .expect("runtime has one owner")
                .shutdown()
                .await;
        });
}
