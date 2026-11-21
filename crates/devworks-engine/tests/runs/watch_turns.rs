use super::*;
use devworks_core::ports::provider_http::{SendPhase, TransportFailure};
use std::sync::mpsc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const LIMIT: Duration = Duration::from_secs(120);
const COMPLETED: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"noted\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
const INTERRUPTED: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial response\"},\"finish_reason\":null}]}\n\n";

struct Request {
    body: Value,
    respond: mpsc::Sender<String>,
}

impl Request {
    fn event_count(&self, event: &Id) -> usize {
        self.body["messages"]
            .to_string()
            .matches(&format!("Watcher event {}:", event.as_str()))
            .count()
    }

    fn is_compaction(&self) -> bool {
        self.body["messages"]
            .to_string()
            .contains("Your context is nearly full and will be compacted")
    }

    fn answer(self, body: impl Into<String>) {
        self.respond.send(body.into()).unwrap();
    }

    fn call_watch_inspect(self, watcher: &Id, call: &str) {
        let name = "watch_inspect_query";
        assert!(
            self.body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["function"]["name"] == name)
        );
        let delta = json!({"choices":[{"index":0,"delta":{"tool_calls":[{
            "index":0,"id":call,"type":"function","function":{
                "name":name,"arguments":json!({"watcherId":watcher}).to_string()
            }
        }]},"finish_reason":null}]});
        self.answer(format!(
            "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\
             \"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n",
        ));
    }
}

struct ScriptedWire(UnboundedSender<Request>);
impl StreamingHttp for ScriptedWire {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        let disconnected = |_| TransportError {
            phase: SendPhase::AfterSend,
            kind: TransportFailure::Protocol,
        };
        let (respond, response) = mpsc::channel();
        self.0
            .send(Request {
                body: serde_json::from_slice(request.body.as_deref().unwrap()).unwrap(),
                respond,
            })
            .map_err(disconnected)?;
        let body = response.recv_timeout(LIMIT).map_err(|_| TransportError {
            phase: SendPhase::AfterSend,
            kind: TransportFailure::IdleTimeout,
        })?;
        Ok(Box::new(crate::approval_wait::Body(Some(
            body.into_bytes(),
        ))))
    }
}

async fn scenario(fx: &Fx) -> (Arc<Runtime>, UnboundedReceiver<Request>) {
    let work = fx.h.root.join("watch-turn-work");
    std::fs::create_dir_all(work.join("work")).unwrap();
    let (mut composed, _) = watch_composition(fx, &work.canonicalize().unwrap());
    let (send, receive) = unbounded_channel();
    composed.config.providers = Some(
        devworks_engine::providers::driver::ProviderComposition::new(Arc::new(ScriptedWire(send))),
    );
    (Arc::new(boot(composed).await.unwrap()), receive)
}

fn timer(app: &Runtime, ctx: &InvocationContext, after: i64, key: &str) -> Id {
    let result = command(
        app,
        ctx,
        "watch.create",
        json!({"timer":{"afterMs":after.to_string()}}),
        key,
    );
    id(result["id"].as_str().unwrap())
}

fn event(fx: &Fx, watcher: &Id) -> StoredWatchEvent {
    read(fx.store.as_ref(), |snapshot| {
        snapshot.watchers().watch_event_for_watcher(watcher)
    })
    .unwrap()
}

fn turn_id(fx: &Fx, session: &Id) -> Option<Id> {
    read(fx.store.as_ref(), |snapshot| {
        snapshot.runs().session_turn(session)
    })
    .map(|turn| turn.turn_id)
}

async fn request(receive: &mut UnboundedReceiver<Request>) -> Request {
    tokio::time::timeout(LIMIT, receive.recv())
        .await
        .expect("provider did not receive the next request")
        .unwrap()
}

async fn finished(task: tokio::task::JoinHandle<LaunchEnd>) -> LaunchEnd {
    tokio::time::timeout(LIMIT, task)
        .await
        .expect("provider turn did not finish")
        .unwrap()
}

fn assert_completed(end: LaunchEnd) {
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

fn only_watch_tools(fx: &Fx, ctx: &InvocationContext) {
    let actor = ctx.principal().clone();
    write(fx.store.as_ref(), move |tx| {
        let principal = tx.authority().principal(&actor)?.unwrap();
        devworks_core::authority::fixture_writes(tx)?.bind_principal(
            NewPrincipalBinding {
                actor,
                kind: principal.kind,
                role: principal.role,
                project_id: principal.project_id,
                tool_families: Some(vec!["watch".into()]),
                bound_by: principal.bound_by,
            },
            Some(principal.revision),
        )?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn delivered_watcher_events_start_a_new_turn_for_an_idle_agent() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    runtime().block_on(async {
        let (app, mut receive) = scenario(&fx).await;
        let (ctx, primary) = agent_context(&fx);
        only_watch_tools(&fx, &ctx);
        let session = ctx.session_id().unwrap();
        let timer = timer(&app, &ctx, 5000, "idle-wake");
        let (_stop_tx, stop) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(NativeAgentTurnExecutor::new(&app).run(ctx.clone(), primary, stop));
        let first = request(&mut receive).await;
        let first_turn = turn_id(&fx, session).unwrap();
        let answer = json!({"choices":[{"index":0,
            "delta":{"content":"earlier work ".repeat(4500)},"finish_reason":"stop"}]});
        first.answer(format!("data: {answer}\n\ndata: [DONE]\n\n"));
        tokio::time::timeout(LIMIT, async {
            while turn_id(&fx, session).is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            !task.is_finished(),
            "assignment ended while its timer was active"
        );
        let (head, _, _) = app
            .recover_native_context_head(&ctx, session)
            .unwrap()
            .unwrap();
        let tokens =
            devworks_engine::agent_turns::context_tokens(app.turn_engine(), &ctx, &head, 0)
                .unwrap();
        fx.tx(move |_, tx| {
            let mut model = tx.router().route_model(&id("watch-model"))?.unwrap();
            let expected = model.revision;
            model.context_tokens = Some(tokens * 100 / 95);
            model.revision += 1;
            tx.router().put_route_model(&model, Some(expected))?;
            Ok(())
        });
        fx.h.advance(5000);
        assert_eq!(app.watchers().unwrap().tick().unwrap().delivered, 1);
        let summary = request(&mut receive).await;
        let delivered = event(&fx, &timer);
        assert!(summary.is_compaction());
        assert_eq!(
            delivered.carried_turn_id, None,
            "a summary does not carry the event"
        );
        summary.answer(COMPLETED);
        let wake = request(&mut receive).await;
        let delivered = event(&fx, &timer);
        assert!(!wake.is_compaction());
        assert_eq!(wake.event_count(&delivered.id), 1);
        assert_eq!(delivered.state, WatchEventState::Delivered);
        let wake_turn = turn_id(&fx, session).unwrap();
        assert_ne!(wake_turn, first_turn);
        assert_eq!(delivered.carried_turn_id, Some(wake_turn));
        wake.answer(COMPLETED);
        assert_completed(finished(task).await);
        assert_eq!(event(&fx, &timer).state, WatchEventState::Acknowledged);
        app.shutdown().await;
    });
}

#[test]
fn watcher_notices_follow_tool_results_in_order_and_acknowledge_at_turn_end() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    runtime().block_on(async {
        let (app, mut receive) = scenario(&fx).await;
        let (ctx, primary) = agent_context(&fx);
        only_watch_tools(&fx, &ctx);
        let session = ctx.session_id().unwrap();
        let first_timer = timer(&app, &ctx, 5000, "first-boundary");
        let second_timer = timer(&app, &ctx, 10000, "second-boundary");
        let (_stop_tx, stop) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(NativeAgentTurnExecutor::new(&app).run(ctx.clone(), primary, stop));
        let first = request(&mut receive).await;
        let turn = turn_id(&fx, session).unwrap();
        fx.h.advance(5000);
        assert_eq!(app.watchers().unwrap().tick().unwrap().delivered, 1);
        let first_event = event(&fx, &first_timer);
        assert_eq!(first.event_count(&first_event.id), 0);
        assert_eq!(first_event.carried_turn_id, None);
        first.call_watch_inspect(&first_timer, "watch-after-first-request");

        let second = request(&mut receive).await;
        assert_eq!(second.event_count(&first_event.id), 1);
        assert_eq!(event(&fx, &first_timer).carried_turn_id, Some(turn.clone()));
        assert!(event(&fx, &first_timer).acknowledged_at_ms.is_none());
        fx.h.advance(5000);
        assert_eq!(app.watchers().unwrap().tick().unwrap().delivered, 1);
        let second_event = event(&fx, &second_timer);
        assert_eq!(second.event_count(&second_event.id), 0);
        assert_eq!(second_event.carried_turn_id, None);
        second.call_watch_inspect(&second_timer, "watch-after-second-request");

        let last = request(&mut receive).await;
        assert_eq!(last.event_count(&first_event.id), 1);
        assert_eq!(last.event_count(&second_event.id), 1);
        let messages = last.body["messages"].as_array().unwrap();
        for (event, call) in [
            (&first_event, "watch-after-first-request"),
            (&second_event, "watch-after-second-request"),
        ] {
            let result = messages
                .iter()
                .position(|message| message["role"] == "tool" && message["tool_call_id"] == call)
                .expect("tool call did not receive a result");
            assert!(messages[result + 1].to_string().contains(event.id.as_str()));
        }
        for timer in [&first_timer, &second_timer] {
            let carried = event(&fx, timer);
            assert_eq!(carried.carried_turn_id.as_ref(), Some(&turn));
            assert_eq!(carried.state, WatchEventState::Delivered);
            assert!(carried.acknowledged_at_ms.is_none());
        }
        last.answer(COMPLETED);
        assert_completed(finished(task).await);
        for timer in [&first_timer, &second_timer] {
            let ack = event(&fx, timer);
            assert_eq!(ack.state, WatchEventState::Acknowledged);
            assert!(ack.acknowledged_at_ms.is_some());
            assert_eq!(ack.carried_turn_id, None);
        }
        assert_eq!(notices(&fx, session).len(), 2);
        assert_eq!(app.watchers().unwrap().tick().unwrap().delivered, 0);
        app.shutdown().await;
    });
}

#[test]
fn a_failed_provider_turn_redelivers_the_same_watcher_event() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    runtime().block_on(async {
        let (app, mut receive) = scenario(&fx).await;
        let (ctx, primary) = agent_context(&fx);
        only_watch_tools(&fx, &ctx);
        let session = ctx.session_id().unwrap();
        let timer = timer(&app, &ctx, 5000, "failed-turn");
        fx.h.advance(5000);
        assert_eq!(app.watchers().unwrap().tick().unwrap().delivered, 1);
        let original = event(&fx, &timer);
        let (_stop_tx, stop) = tokio::sync::watch::channel(false);
        let native = NativeAgentTurnExecutor::new(&app);
        let failed = tokio::spawn(native.run(ctx.clone(), primary.clone(), stop.clone()));
        let first = request(&mut receive).await;
        assert_eq!(first.event_count(&original.id), 1);
        let failed_turn = turn_id(&fx, session).unwrap();
        assert_eq!(
            event(&fx, &timer).carried_turn_id,
            Some(failed_turn.clone())
        );
        first.answer(INTERRUPTED);
        assert!(matches!(finished(failed).await, LaunchEnd::Failed(_)));
        let pending = event(&fx, &timer);
        assert_eq!(pending, original);
        assert_eq!(pending.state, WatchEventState::Delivered);

        let retried = tokio::spawn(native.run(ctx.clone(), primary, stop));
        let replay = request(&mut receive).await;
        assert_eq!(replay.event_count(&original.id), 1);
        let retry_turn = turn_id(&fx, session).unwrap();
        assert_ne!(retry_turn, failed_turn);
        assert_eq!(event(&fx, &timer).carried_turn_id, Some(retry_turn));
        replay.answer(COMPLETED);
        assert_completed(finished(retried).await);
        let ack = event(&fx, &timer);
        assert_eq!(ack.id, original.id);
        assert_eq!(ack.state, WatchEventState::Acknowledged);
        assert_eq!(notices(&fx, session).len(), 1);
        app.shutdown().await;
    });
}
