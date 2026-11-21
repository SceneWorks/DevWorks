use super::*;
use devworks_core::context::HostIngress;
use devworks_core::ports::provider_http::{SendPhase, TransportFailure};
use devworks_core::protocol::Counter;
use std::sync::mpsc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const LIMIT: Duration = Duration::from_secs(120);
const COMPLETED: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"noted\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
const INTERRUPTED: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial response\"},\"finish_reason\":null}]}\n\n";

struct Request {
    body: Value,
    respond: mpsc::Sender<Option<String>>,
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
        self.respond.send(Some(body.into())).unwrap();
    }

    fn crash(self) {
        self.respond.send(None).unwrap();
    }

    fn call_watch_inspect(self, watcher: &Id, call: &str) {
        self.call("watch_inspect_query", call, json!({"watcherId":watcher}));
    }

    fn call(self, name: &str, call: &str, arguments: Value) {
        assert!(
            self.body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["function"]["name"] == name)
        );
        let delta = json!({"choices":[{"index":0,"delta":{"tool_calls":[{
            "index":0,"id":call,"type":"function","function":{
                "name":name,"arguments":arguments.to_string()
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
            body.expect("the host stopped before the response arrived")
                .into_bytes(),
        ))))
    }
}

async fn scenario(fx: &Fx) -> (Arc<Runtime>, UnboundedReceiver<Request>) {
    let work = fx.h.root.join("watch-turn-work");
    std::fs::create_dir_all(work.join("work")).unwrap();
    let (mut composed, _) = watch_composition(fx, &work.canonicalize().unwrap());
    composed.application.sessions = Some(devworks_engine::SessionPorts::default());
    composed.process_host = Arc::new(crate::compaction::StoppingHost::default());
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

fn select_watch_tools(fx: &Fx, ctx: &InvocationContext) {
    select_tool_families(fx, ctx, &["router", "watch"]);
}

fn select_tool_families(fx: &Fx, ctx: &InvocationContext, families: &[&str]) {
    let families: Vec<String> = families.iter().map(|family| (*family).to_owned()).collect();
    let actor = ctx.principal().clone();
    write(fx.store.as_ref(), move |tx| {
        let principal = tx.authority().principal(&actor)?.unwrap();
        devworks_core::authority::fixture_writes(tx)?.bind_principal(
            NewPrincipalBinding {
                actor,
                kind: principal.kind,
                role: principal.role,
                project_id: principal.project_id,
                tool_families: Some(families),
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
        select_watch_tools(&fx, &ctx);
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
        select_watch_tools(&fx, &ctx);
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
fn a_queued_burst_of_watcher_events_reaches_the_same_provider_input() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    runtime().block_on(async {
        let (app, mut receive) = scenario(&fx).await;
        let (ctx, primary) = agent_context(&fx);
        select_watch_tools(&fx, &ctx);
        let session = ctx.session_id().unwrap();
        let timer = timer(&app, &ctx, 5000, "queued-burst");
        fx.h.advance(5000);
        let supervisor = app.watchers().unwrap();
        assert_eq!(supervisor.settle().unwrap(), 1);
        let original = event(&fx, &timer);
        let mut ids = vec![original.id.clone()];
        fx.tx({
            let mut additional = Vec::new();
            for n in 0..128 {
                let mut matched = original.clone();
                matched.id = id(&format!("queued-match-{n:03}"));
                matched.match_count = Some(1);
                matched.created_at_ms += i64::from(n) + 1;
                matched.payload["matchCount"] = json!("1");
                matched.payload["matches"] = json!((n + 1).to_string());
                ids.push(matched.id.clone());
                additional.push(matched);
            }
            move |_, tx| {
                for event in additional {
                    tx.watchers().insert_watch_event(&event)?;
                }
                Ok(())
            }
        });
        fx.h.advance(128);
        assert_eq!(supervisor.deliver().unwrap(), ids.len());
        let (_stop_tx, stop) = tokio::sync::watch::channel(false);
        let mut task = tokio::spawn(NativeAgentTurnExecutor::new(&app).run(ctx.clone(), primary, stop));
        let input = tokio::select! {
            input = request(&mut receive) => input,
            result = &mut task => panic!("provider turn ended before carrying the burst: {result:?}"),
        };
        let input = if input.is_compaction() {
            input.answer(COMPLETED);
            request(&mut receive).await
        } else {
            input
        };
        assert!(!input.is_compaction());
        let messages = input.body["messages"].to_string();
        let mut remaining = messages.as_str();
        for event in &ids {
            let marker = format!("Watcher event {}:", event.as_str());
            assert_eq!(messages.matches(&marker).count(), 1);
            remaining = remaining.split_once(&marker).expect("watcher event out of order").1;
        }
        input.answer(COMPLETED);
        assert_completed(finished(task).await);
        let acknowledged = read(fx.store.as_ref(), |snapshot| {
            snapshot.watchers().watch_events(session, &[WatchEventState::Acknowledged], 500)
        });
        assert_eq!(acknowledged.len(), ids.len());
        app.shutdown().await;
    });
}

#[test]
fn home_wakes_for_watchers_without_consuming_later_human_input() {
    use crate::workspace_reply::{open_home, send_workspace, wait_for_replies, workspace_ports};

    let _native_case = native_runtime_case();
    let fx = Fx::new();
    seed_local_route(&fx);
    let work = fx.h.root.join("home-watch-work");
    std::fs::create_dir_all(work.join("work")).unwrap();
    let (send, mut receive) = unbounded_channel();
    let mut ports = workspace_ports(&fx, Arc::new(ScriptedWire(send)));
    let (with_watch, _) = watch_composition(&fx, &work.canonicalize().unwrap());
    ports.config.process_tools = with_watch.config.process_tools;
    ports.config.watch_interval = Duration::from_secs(3600);
    runtime().block_on(async {
        let app = boot(ports).await.unwrap();
        let session = open_home(&fx, &app);
        let owner = read(fx.store.as_ref(), |snapshot| {
            snapshot.sessions().session(&session)
        })
        .unwrap();
        let ctx = HostIngress::new(fx.store.as_ref())
            .context(
                owner.agent_id,
                PrincipalKind::Agent,
                id("watch-home-test"),
                id("watch-home-test"),
                Counter::new(fx.store.info().restore_epoch).unwrap(),
                Revision::new(1).unwrap(),
            )
            .with_session(session.clone());
        select_watch_tools(&fx, &ctx);
        send_workspace(
            &fx,
            &app,
            &session,
            "home-create-watchers",
            "Start two timers",
            "watch-model",
        );
        request(&mut receive).await.call(
            "watch_create_command",
            "create-first-timer",
            json!({"timer":{"afterMs":"5000"}}),
        );
        request(&mut receive).await.call(
            "watch_create_command",
            "create-second-timer",
            json!({"timer":{"afterMs":"10000"}}),
        );
        let busy = request(&mut receive).await;
        let timers = read(fx.store.as_ref(), |snapshot| {
            snapshot
                .watchers()
                .watchers(&WatcherFilter::Session(session.clone()), &[], None, 10)
        });
        assert_eq!(timers.len(), 2);
        let first = &timers
            .iter()
            .min_by_key(|timer| timer.due_at_ms)
            .unwrap()
            .id;
        let second = &timers
            .iter()
            .max_by_key(|timer| timer.due_at_ms)
            .unwrap()
            .id;
        fx.h.advance(5000);
        assert_eq!(app.watchers().unwrap().tick().unwrap().delivered, 1);
        send_workspace(
            &fx,
            &app,
            &session,
            "home-later-human",
            "Later human input",
            "watch-model",
        );
        busy.call_watch_inspect(first, "inspect-first-timer");

        let mid_turn = request(&mut receive).await;
        let first_event = event(&fx, first);
        assert_eq!(mid_turn.event_count(&first_event.id), 1);
        assert!(
            !mid_turn.body["messages"]
                .to_string()
                .contains("Later human input")
        );
        assert_eq!(first_event.state, WatchEventState::Delivered);
        assert!(first_event.carried_turn_id.is_some());
        mid_turn.answer(COMPLETED);
        let later = request(&mut receive).await;
        assert!(
            later.body["messages"]
                .to_string()
                .contains("Later human input")
        );
        assert_eq!(event(&fx, first).state, WatchEventState::Acknowledged);
        later.answer(COMPLETED);
        wait_for_replies(&fx, 3).await;

        fx.h.advance(5000);
        assert_eq!(app.watchers().unwrap().tick().unwrap().delivered, 1);
        let wake = request(&mut receive).await;
        let second_event = event(&fx, second);
        assert_eq!(wake.event_count(&second_event.id), 1);
        assert_eq!(second_event.state, WatchEventState::Delivered);
        wake.answer(COMPLETED);
        wait_for_replies(&fx, 4).await;
        assert_eq!(event(&fx, second).state, WatchEventState::Acknowledged);
        assert_eq!(notices(&fx, &session).len(), 2);
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
        select_watch_tools(&fx, &ctx);
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

#[test]
fn a_restart_before_the_provider_terminal_frame_redelivers_the_watcher() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    let (session, timer, original) = runtime().block_on(async {
        let (app, mut receive) = scenario(&fx).await;
        let (ctx, primary) = agent_context(&fx);
        select_watch_tools(&fx, &ctx);
        let session = ctx.session_id().unwrap().clone();
        let timer = timer(&app, &ctx, 5000, "restart-wake");
        fx.h.advance(5000);
        assert_eq!(app.watchers().unwrap().tick().unwrap().delivered, 1);
        let original = event(&fx, &timer);
        let (_stop_tx, stop) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(NativeAgentTurnExecutor::new(&app).run(ctx, primary, stop));
        let sent = request(&mut receive).await;
        assert_eq!(sent.event_count(&original.id), 1);
        sent.crash();
        assert!(matches!(finished(task).await, LaunchEnd::Failed(_)));
        assert_eq!(event(&fx, &timer).state, WatchEventState::Delivered);
        assert!(
            !read(fx.store.as_ref(), |snapshot| snapshot
                .sessions()
                .entries(&session, 0, 500))
            .iter()
            .any(|entry| entry.kind == "AssistantAttempt")
        );
        app.shutdown().await;
        (session, timer, original)
    });
    let (harness, store) = fx.into_parts();
    drop(store);
    let store = harness.open();
    let fx = Fx::over(harness, store);
    runtime().block_on(async {
        let (app, mut receive) = scenario(&fx).await;
        tokio::time::timeout(LIMIT, async {
            while !read(fx.store.as_ref(), |snapshot| {
                snapshot.runs().inference_quiescent(&RootId(id("root-i")))
            }) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        applied(&fx.resume(&fx.user, "root-i"));
        let (ctx, primary) = bind(&fx, "watch-after-provider-restart");
        assert_eq!(ctx.session_id(), Some(&session));
        let (_stop_tx, stop) = tokio::sync::watch::channel(false);
        let mut task = tokio::spawn(NativeAgentTurnExecutor::new(&app).run(ctx, primary, stop));
        let replay = tokio::select! {
            input = request(&mut receive) => input,
            result = &mut task => panic!("provider did not restart with the event: {result:?}"),
        };
        assert_eq!(replay.event_count(&original.id), 1);
        replay.answer(COMPLETED);
        assert_completed(finished(task).await);
        let recorded = event(&fx, &timer);
        assert_eq!(recorded.id, original.id);
        assert_eq!(recorded.state, WatchEventState::Acknowledged);
        assert_eq!(notices(&fx, &session).len(), 1);
        app.shutdown().await;
    });
}

fn delegate(fx: &Fx, parent: &InvocationContext) -> StoredAssignment {
    let agents = AgentService::new(fx.h.ids.clone(), fx.service.clone());
    let request = devworks_core::operations::agents::DelegateInput {
        root_id: parent.root_id().unwrap().clone(),
        expected_control_revision: Revision::new(fx.control("root-i").revision).unwrap(),
        task_ids: devworks_core::protocol::support::BoundedList(Vec::new()),
        role_id: id("worker"),
        instruction: devworks_core::protocol::support::BoundedText("Scripted child".into()),
        source_checkpoint_id: None,
    };
    let ctx = parent.clone();
    fx.tx(move |_, tx| {
        let child = agents.delegate(tx, &ctx, &request).map_err(|error| {
            StorageError::InvalidRecord(format!("delegating scripted child: {error:?}"))
        })?;
        Ok(tx.runs().assignment(&child.assignment_id)?.unwrap())
    })
}

fn watch_assignment(
    app: &Runtime,
    parent: &InvocationContext,
    child: &StoredAssignment,
    key: &str,
) -> Id {
    let record = command_at(
        app,
        parent,
        "watch.create",
        json!({"assignmentId":child.id}),
        key,
        10,
    );
    assert_eq!(record["sourceKind"], "Assignment");
    assert_eq!(record["assignment"]["assignmentId"], json!(child.id));
    assert_eq!(record["assignment"]["sessionId"], json!(child.session_id));
    id(record["id"].as_str().unwrap())
}

#[test]
fn completed_delegated_assignment_wakes_its_idle_parent_with_session_and_evidence() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    runtime().block_on(async {
        let (app, mut receive) = scenario(&fx).await;
        let (parent, primary) = agent_context(&fx);
        select_tool_families(&fx, &parent, &["router", "watch", "agents", "sessions"]);
        let child = delegate(&fx, &parent);
        let watcher = watch_assignment(&app, &parent, &child, "watch-child-completion");
        let supervisor = app.watchers().unwrap();
        assert_eq!(supervisor.tick().unwrap().settled, 0);
        let session = parent.session_id().unwrap();

        let (_parent_stop, stop) = tokio::sync::watch::channel(false);
        let task =
            tokio::spawn(NativeAgentTurnExecutor::new(&app).run(parent.clone(), primary, stop));
        request(&mut receive).await.answer(COMPLETED);
        tokio::time::timeout(LIMIT, async {
            while turn_id(&fx, session).is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!task.is_finished());

        let launcher = AgentLauncher::new(
            fx.store.clone(),
            fx.h.ids.clone(),
            Arc::new(AgentService::new(fx.h.ids.clone(), fx.service.clone())),
            Arc::new(CompletedCoordinator::default()),
        );
        let (activation, epoch) = parent.activation().unwrap();
        let (_child_stop, stop) = tokio::sync::watch::channel(false);
        assert!(matches!(
            launcher
                .launch_child(child.id.clone(), activation.clone(), epoch, stop)
                .await,
            ChildLaunch::End(LaunchEnd::Finished {
                outcome: ActivationOutcome::Completed,
                ..
            })
        ));

        let pass = supervisor.tick().unwrap();
        assert_eq!((pass.settled, pass.delivered), (1, 1));
        let delivery = event(&fx, &watcher);
        assert_eq!(delivery.session_id, *session);
        assert_eq!(delivery.payload["outcome"]["kind"], "Succeeded");
        let wake = request(&mut receive).await;
        assert_eq!(wake.event_count(&delivery.id), 1);
        let messages = wake.body["messages"].to_string();
        assert!(messages.contains(&format!(
            "Child session: sessions.transcript for {}",
            child.session_id.as_str()
        )));
        let evidence = result(
            &app.query(
                &parent,
                "agents.inspect",
                &frame_at(
                    "Query",
                    "agents.inspect",
                    json!({"assignmentId":child.id}),
                    "child-evidence",
                    1,
                ),
            )
            .unwrap(),
        );
        assert_eq!(evidence["state"], "ObservedComplete");
        assert_eq!(evidence["assignmentId"], json!(child.id));
        assert!(messages.contains(&format!(
            "Evidence: agents.inspect for assignment {} at revision {}",
            child.id.0.as_str(),
            evidence["revision"].as_str().unwrap()
        )));
        let transcript = result(
            &app.query(
                &parent,
                "sessions.transcript",
                &serde_json::to_vec(&json!({
                    "protocol": PROTOCOL,
                    "queryId": "child-transcript",
                    "sessionId": child.session_id,
                    "limit": 10
                }))
                .unwrap(),
            )
            .unwrap(),
        );
        assert_eq!(transcript["sessionId"], json!(child.session_id));
        assert!(messages.contains("Child assignment Completed"));
        wake.answer(COMPLETED);
        assert_completed(finished(task).await);
        assert_eq!(event(&fx, &watcher).state, WatchEventState::Acknowledged);
        assert_eq!(supervisor.tick().unwrap().delivered, 0);
        assert!(notices(&fx, &child.session_id).is_empty());
        app.shutdown().await;
    });
}

#[test]
fn delegated_assignment_backup_reads_terminal_state_without_primary_observation() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    runtime().block_on(async {
        let (app, _) = scenario(&fx).await;
        let (parent, primary) = agent_context(&fx);
        select_watch_tools(&fx, &parent);
        let child = delegate(&fx, &parent);
        let watcher = watch_assignment(&app, &parent, &child, "watch-child-backup");

        // The parent cannot watch its own primary assignment or use a revision that cannot
        // represent an assignment source.
        let create = |target: &AssignmentId, revision, key| {
            app.command(
                &parent,
                "watch.create",
                &frame_at(
                    "Command",
                    "watch.create",
                    json!({"assignmentId":target}),
                    key,
                    revision,
                ),
            )
            .unwrap()
        };
        assert!(matches!(
            create(&primary.assignment_id, 10, "watch-nonchild"),
            CommandReply::Recorded { receipt, .. }
                if matches!(receipt.decision, DomainDecision::Refused { .. })
        ));
        assert!(matches!(
            create(&child.id, 9, "watch-legacy"),
            CommandReply::Rejected { .. }
        ));
        assert!(matches!(
            app.query(
                &parent,
                "watch.inspect",
                &frame_at(
                    "Query",
                    "watch.inspect",
                    json!({"watcherId":watcher}),
                    "inspect-legacy",
                    9
                )
            )
            .unwrap(),
            DomainQueryReply::Rejected { .. }
        ));

        let supervisor = app.watchers().unwrap();
        assert_eq!(supervisor.backup_if_due().unwrap(), None);
        fx.h.advance(900_000);
        let active = supervisor.backup_if_due().unwrap().unwrap();
        assert_eq!((active.caught, active.reminded), (0, 0));
        assert!(notices(&fx, parent.session_id().unwrap()).is_empty());

        let actor = fx.user.principal().clone();
        let target = child.id.clone();
        fx.tx(move |service, tx| {
            service
                .set_assignment_state(
                    tx,
                    &actor,
                    &target,
                    child.revision,
                    AssignmentState::Failed,
                    None,
                )?
                .map_err(|error| {
                    StorageError::InvalidRecord(format!("scripted failure: {error:?}"))
                })?;
            Ok(())
        });

        assert_eq!(
            inspect_at(&app, &parent, watcher.as_str(), 10)["state"],
            "Active"
        );
        assert!(
            read(fx.store.as_ref(), |snapshot| {
                snapshot.watchers().watch_event_for_watcher(&watcher)
            })
            .is_none()
        );
        fx.h.advance(900_000);
        let backup = supervisor.backup_if_due().unwrap().unwrap();
        assert_eq!((backup.caught, backup.reminded), (1, 0));
        let caught = event(&fx, &watcher);
        assert_eq!(caught.state, WatchEventState::Delivered);
        assert_eq!(caught.payload["outcome"]["kind"], "Failed");
        assert_eq!(
            caught.payload["assignment"]["sessionId"],
            json!(child.session_id)
        );
        assert!(
            caught.payload["outcome"]["detail"]
                .as_str()
                .unwrap()
                .contains(&format!(
                    "agents.inspect for assignment {}",
                    child.id.0.as_str()
                ))
        );
        assert_eq!(supervisor.tick().unwrap().delivered, 0);
        assert_eq!(event(&fx, &watcher).id, caught.id);
        assert_eq!(notices(&fx, parent.session_id().unwrap()).len(), 1);
        app.shutdown().await;
    });
}
