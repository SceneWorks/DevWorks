//! Retained inbox delivery crosses a write boundary; a lost reply must not lose a message.
mod common;

use common::{Harness, id, principal, seed_root, write};
use devworks_core::authority::HostAuthority;
use devworks_core::context::PrincipalKind;
use devworks_core::gateway::Gateway;
use devworks_core::ports::storage::{Cancellation, StorageError, WriteOptions};
use devworks_core::protocol::results::{DomainDecision, DomainQueryReply, OperationResult};
use devworks_core::protocol::support::DomainErrorCode;
use devworks_core::protocol::{CommandReply, ProjectId};
use devworks_core::services::UseCases;
use devworks_core::services::agents::AgentService;
use devworks_core::services::agents::retained::{
    AckSelection, MessageKind, PostSpec, Recipient, post_message,
};
use devworks_core::services::runs::ControlService;
use devworks_core::services::runs::scope::ScriptedScope;
use devworks_engine::agent_inbox::{InboxSelection, RetainedInboxDriver};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn waiting_inbox_cancels_without_a_held_writer() {
    let harness = Harness::new();
    let store = harness.open();
    seed_root(&harness, "root-a");
    let user = principal(store.as_ref(), "waiting-human", PrincipalKind::Human);
    let driver = RetainedInboxDriver::new(store.clone());
    let cancellation = Cancellation::new();
    let stop = cancellation.clone();
    let waiting = tokio::spawn(async move {
        driver
            .receive_cancellable(
                user,
                ProjectId(id("project-a")),
                InboxSelection {
                    recipient: Recipient::Human,
                    after: 0,
                    include_read: false,
                    limit: 10,
                    wait_seconds: 55,
                },
                Some(stop),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let peer = store.clone();
    let writer = tokio::task::spawn_blocking(move || {
        write(peer.as_ref(), |tx| tx.runs().agent_delivery_head())
    });
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), writer)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        0
    );
    cancellation.cancel();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap(),
        Err(StorageError::Cancelled)
    ));
    let head = common::read(store.as_ref(), |snapshot| {
        snapshot.runs().agent_delivery_head()
    });
    assert_eq!(head, 0);
}

#[test]
fn selected_retained_post_uses_the_native_gateway_and_replays_its_receipt() {
    let harness = Harness::new();
    let store = harness.open();
    seed_root(&harness, "root-a");
    rusqlite::Connection::open(harness.root.join("workspace.sqlite"))
        .unwrap()
        .execute_batch("INSERT INTO work_items VALUES('issue-a','project-a','issue',NULL,NULL,NULL,'Issue',1,0,1,1,1,'{}');")
        .unwrap();
    let user = principal(store.as_ref(), "post-user", PrincipalKind::Human);
    let runs = Arc::new(ControlService::with_scope(
        harness.ids.clone(),
        Arc::new(ScriptedScope::new()),
    ));
    let cases = UseCases::new().with_agents(Arc::new(AgentService::new(harness.ids.clone(), runs)));
    let authority = HostAuthority {
        store: store.as_ref(),
        clock: harness.clock.as_ref(),
        protected_branches: &[],
    };
    let gateway = Gateway {
        authority: &authority,
        store: store.as_ref(),
        ids: harness.ids.as_ref(),
    };
    let frame = json!({
        "protocol":"1.0-draft.1", "mode":"Command", "schemaRevision":7,
        "requestKey":"retained-post-key",
        "body":{"kind":"agents.post","projectId":"project-a","body":"Gateway message",
            "toHuman":true,"idempotencyKey":"source-post-key"}
    });
    let raw = serde_json::to_vec(&frame).unwrap();
    let first = cases.command(&gateway, &user, "agents.post", &raw).unwrap();
    let message_id = match &first {
        CommandReply::Recorded { receipt, .. } => match &receipt.decision {
            DomainDecision::Applied {
                result: OperationResult::AgentsPost { value },
            } => {
                assert!(value.created);
                assert_eq!(value.message.body.0, "Gateway message");
                assert_eq!(value.message.from.name.0, "fixture");
                assert_eq!(value.message.from.kind.0, "user");
                assert_eq!(value.recipients.get(), 1);
                value.message.id.clone()
            }
            other => panic!("unexpected post decision: {other:?}"),
        },
        other => panic!("unexpected post reply: {other:?}"),
    };
    let replay = cases.command(&gateway, &user, "agents.post", &raw).unwrap();
    match replay {
        CommandReply::Recorded {
            receipt,
            replayed: true,
            ..
        } => match receipt.decision {
            DomainDecision::Applied {
                result: OperationResult::AgentsPost { value },
            } => assert_eq!(value.message.id, message_id),
            other => panic!("unexpected replay decision: {other:?}"),
        },
        other => panic!("unexpected replay reply: {other:?}"),
    }
    let mut second_frame = frame.clone();
    second_frame["requestKey"] = json!("retained-post-new-request");
    let second_raw = serde_json::to_vec(&second_frame).unwrap();
    let duplicate = cases
        .command(&gateway, &user, "agents.post", &second_raw)
        .unwrap();
    match duplicate {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::NoChange {
                result: OperationResult::AgentsPost { value },
            } => {
                assert!(!value.created);
                assert_eq!(value.message.id, message_id);
            }
            other => panic!("unexpected idempotent decision: {other:?}"),
        },
        other => panic!("unexpected idempotent reply: {other:?}"),
    }
    let mut inferred = frame.clone();
    inferred["requestKey"] = json!("retained-post-inferred-project");
    inferred["body"]
        .as_object_mut()
        .unwrap()
        .remove("projectId");
    inferred["body"]["itemId"] = json!("issue-a");
    inferred["body"]["idempotencyKey"] = json!("inferred-post-key");
    let inferred_reply = cases
        .command(
            &gateway,
            &user,
            "agents.post",
            &serde_json::to_vec(&inferred).unwrap(),
        )
        .unwrap();
    assert!(
        matches!(inferred_reply, CommandReply::Recorded { receipt, .. }
        if matches!(&receipt.decision, DomainDecision::Applied {
            result: OperationResult::AgentsPost { value }
        } if value.message.project_id == ProjectId(id("project-a")) && value.message.item_id.as_ref().is_some_and(|item| item.0 == id("issue-a"))))
    );
    let mut forged = frame;
    forged["requestKey"] = json!("retained-post-forged-session");
    forged["body"]["sessionId"] = json!("another-session");
    let refused = cases
        .command(
            &gateway,
            &user,
            "agents.post",
            &serde_json::to_vec(&forged).unwrap(),
        )
        .unwrap();
    assert!(matches!(refused, CommandReply::Recorded { receipt, .. }
        if matches!(receipt.decision, DomainDecision::Refused { .. })));
    let blocked = |key: &str, body: serde_json::Value| {
        let raw = serde_json::to_vec(&json!({
            "protocol":"1.0-draft.1", "mode":"Command", "schemaRevision":7,
            "requestKey":key, "body":body
        }))
        .unwrap();
        let reply = cases.command(&gateway, &user, "agents.post", &raw).unwrap();
        match reply {
            CommandReply::Recorded { receipt, .. } => match receipt.decision {
                DomainDecision::Applied {
                    result: OperationResult::AgentsPost { value },
                } => value,
                other => panic!("unexpected blocking post decision: {other:?}"),
            },
            other => panic!("unexpected blocking post reply: {other:?}"),
        }
    };
    let question = |key: &str| {
        blocked(
            key,
            json!({
                "kind":"agents.post", "projectId":"project-a", "body":"Human decision needed",
                "messageKind":"Question", "itemId":"issue-a", "toHuman":true, "blocking":true
            }),
        )
    };
    let first_question = question("blocking-first");
    assert_eq!(
        first_question.item_versions.as_ref().unwrap().0[0]
            .version
            .get(),
        2
    );
    let second_question = question("blocking-second");
    assert!(second_question.item_versions.is_none());
    let answer = |key: &str, message: &devworks_core::protocol::Id| {
        blocked(
            key,
            json!({
                "kind":"agents.post", "projectId":"project-a", "body":"Answered",
                "inReplyToId":message, "resolves":"Answered"
            }),
        )
    };
    let first_answer = answer("answer-first", &first_question.message.id);
    assert!(first_answer.item_versions.is_none());
    let last_answer = answer("answer-second", &second_question.message.id);
    assert_eq!(
        last_answer.item_versions.as_ref().unwrap().0[0]
            .version
            .get(),
        3
    );
    assert_eq!(
        last_answer
            .resolved
            .as_ref()
            .unwrap()
            .resolved_by
            .as_ref()
            .unwrap()
            .name
            .0,
        "fixture"
    );
    assert!(!common::read(store.as_ref(), |snapshot| {
        snapshot
            .runs()
            .open_blocking_question(&devworks_core::protocol::RootId(id("issue-a")))
    }));
    let stored = common::read(store.as_ref(), |snapshot| {
        snapshot.runs().agent_message(&message_id)
    });
    assert_eq!(stored.unwrap().body, "Gateway message");
    let query = |kind: &str, fields: serde_json::Value| {
        let mut body = fields.as_object().unwrap().clone();
        body.insert("kind".into(), json!(kind));
        let raw = serde_json::to_vec(&json!({
            "protocol":"1.0-draft.1", "mode":"Query", "schemaRevision":7,
            "queryId":"retained-query", "body":body
        }))
        .unwrap();
        cases.query(&gateway, &user, kind, &raw).unwrap()
    };
    match query(
        "agents.history",
        json!({"projectId":"project-a","itemId":"issue-a"}),
    ) {
        DomainQueryReply::Result {
            result: OperationResult::AgentsHistory { value },
            ..
        } => {
            assert!(
                value
                    .items
                    .0
                    .iter()
                    .any(|row| row.id == first_question.message.id)
            );
            assert!(
                value
                    .items
                    .0
                    .iter()
                    .any(|row| row.id == second_question.message.id)
            );
            assert!(value.items.0.iter().all(|row| {
                row.item_id
                    .as_ref()
                    .is_some_and(|item| item.0 == id("issue-a"))
            }));
        }
        other => panic!("unexpected history reply: {other:?}"),
    }
    match query("agents.events", json!({"projectId":"project-a"})) {
        DomainQueryReply::Result {
            result: OperationResult::AgentsEvents { value },
            ..
        } => assert!(value.items.0.is_empty()),
        other => panic!("unexpected live event reply: {other:?}"),
    }
    match query(
        "agents.events",
        json!({"projectId":"project-a","after":"0"}),
    ) {
        DomainQueryReply::Result {
            result: OperationResult::AgentsEvents { value },
            ..
        } => {
            assert!(
                value
                    .items
                    .0
                    .iter()
                    .any(|event| event.message_id == message_id)
            );
            assert!(
                value
                    .items
                    .0
                    .iter()
                    .all(|event| event.project_id == ProjectId(id("project-a")))
            );
        }
        other => panic!("unexpected replay event reply: {other:?}"),
    }
    assert!(matches!(
        query(
            "agents.events",
            json!({"projectId":"project-b","after":"0"})
        ),
        DomainQueryReply::Rejected { .. }
    ));
    assert!(matches!(
        query(
            "agents.events",
            json!({"projectId":"project-a","after":"99999999"})
        ),
        DomainQueryReply::Rejected { .. }
    ));
    let agent = principal(store.as_ref(), "history-agent", PrincipalKind::Agent);
    let raw = serde_json::to_vec(&json!({
        "protocol":"1.0-draft.1", "mode":"Query", "schemaRevision":7,
        "queryId":"agent-history", "body":{"kind":"agents.history","projectId":"project-a"}
    }))
    .unwrap();
    assert!(matches!(
        cases.query(&gateway, &agent, "agents.history", &raw).unwrap(),
        DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::PermissionDenied
    ));
    let ack = |key: &str, body: serde_json::Value| {
        let raw = serde_json::to_vec(&json!({
            "protocol":"1.0-draft.1", "mode":"Command", "schemaRevision":7,
            "requestKey":key, "body":body
        }))
        .unwrap();
        cases.command(&gateway, &user, "agents.ack", &raw).unwrap()
    };
    let third_question = question("blocking-for-ack");
    let named = json!({
        "kind":"agents.ack", "projectId":"project-a",
        "messages":[third_question.message.id], "resolution":"Answered"
    });
    match ack("ack-named", named.clone()) {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied {
                result: OperationResult::AgentsAck { value },
            } => {
                assert_eq!(value.acked.get(), 1);
                assert_eq!(value.messages.0.len(), 1);
                assert_eq!(value.item_versions.unwrap().0[0].version.get(), 5);
            }
            other => panic!("unexpected named ack decision: {other:?}"),
        },
        other => panic!("unexpected named ack reply: {other:?}"),
    }
    assert!(
        matches!(ack("ack-named-repeat", named), CommandReply::Recorded { receipt, .. }
        if matches!(&receipt.decision, DomainDecision::NoChange {
            result: OperationResult::AgentsAck { value }
        } if value.acked.get() == 0))
    );
    let later = blocked(
        "post-for-cursor",
        json!({"kind":"agents.post","projectId":"project-a","body":"Read through cursor\nhidden details","toHuman":true}),
    );
    let head = common::read(store.as_ref(), |snapshot| {
        snapshot.runs().agent_delivery_head()
    });
    let cursor = devworks_core::services::agents::retained::inbox_cursor(head).unwrap();
    assert!(matches!(ack("ack-through", json!({
        "kind":"agents.ack", "projectId":"project-a", "upTo":cursor
    })), CommandReply::Recorded { receipt, .. }
        if matches!(&receipt.decision, DomainDecision::Applied {
            result: OperationResult::AgentsAck { value }
        } if value.acked.get() > 0 && value.messages.0.iter().any(|line|
            line.id == later.message.id && line.summary.0 == "Read through cursor"))));
    for (key, body) in [
        (
            "ack-neither",
            json!({"kind":"agents.ack","projectId":"project-a"}),
        ),
        (
            "ack-both",
            json!({"kind":"agents.ack","projectId":"project-a","messages":[message_id],"upTo":"aWI6MA"}),
        ),
        (
            "ack-forged-session",
            json!({"kind":"agents.ack","projectId":"project-a","sessionId":"another-session","messages":[message_id]}),
        ),
        (
            "ack-wrong-cursor",
            json!({"kind":"agents.ack","projectId":"project-a","upTo":"Zm9v"}),
        ),
        (
            "ack-ahead-cursor",
            json!({"kind":"agents.ack","projectId":"project-a","upTo":devworks_core::services::agents::retained::inbox_cursor(head + 1).unwrap()}),
        ),
    ] {
        let reply = ack(key, body);
        assert!(
            matches!(&reply, CommandReply::Recorded { receipt, .. }
            if matches!(receipt.decision, DomainDecision::Refused { .. })),
            "{key}: {reply:?}"
        );
    }
}

#[test]
fn selected_receive_waits_replays_and_acknowledges_through_the_native_gateway() {
    let harness = Harness::new();
    let store = harness.open();
    seed_root(&harness, "root-a");
    let user = principal(store.as_ref(), "receive-user", PrincipalKind::Human);
    let runs = Arc::new(ControlService::with_scope(
        harness.ids.clone(),
        Arc::new(ScriptedScope::new()),
    ));
    let cases = UseCases::new().with_agents(Arc::new(AgentService::new(harness.ids.clone(), runs)));
    let authority = HostAuthority {
        store: store.as_ref(),
        clock: harness.clock.as_ref(),
        protected_branches: &[],
    };
    let gateway = Gateway {
        authority: &authority,
        store: store.as_ref(),
        ids: harness.ids.as_ref(),
    };
    let command = |operation: &str, key: &str, body: serde_json::Value| {
        let raw = serde_json::to_vec(&json!({
            "protocol":"1.0-draft.1", "mode":"Command", "schemaRevision":7,
            "requestKey":key, "body":body
        }))
        .unwrap();
        cases.command(&gateway, &user, operation, &raw).unwrap()
    };
    let empty = command(
        "agents.receive",
        "receive-empty",
        json!({"kind":"agents.receive","projectId":"project-a","waitSeconds":1}),
    );
    assert!(
        matches!(&empty, CommandReply::Recorded { receipt, .. }
        if matches!(&receipt.decision, DomainDecision::NoChange {
            result: OperationResult::AgentsReceive { value }
        } if value.items.0.is_empty() && value.next_cursor.0 == "aWI6MA")),
        "{empty:?}"
    );
    let receive = json!({"kind":"agents.receive","projectId":"project-a","waitSeconds":2});
    let (posted, first) = std::thread::scope(|scope| {
        let waiting = scope.spawn(|| command("agents.receive", "receive-first", receive.clone()));
        std::thread::sleep(Duration::from_millis(100));
        let posted = command(
            "agents.post",
            "receive-post",
            json!({"kind":"agents.post","projectId":"project-a","body":"Durable inbox body","toHuman":true}),
        );
        (posted, waiting.join().unwrap())
    });
    let message_id = match posted {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied {
                result: OperationResult::AgentsPost { value },
            } => value.message.id,
            other => panic!("unexpected post decision: {other:?}"),
        },
        other => panic!("unexpected post reply: {other:?}"),
    };
    let cursor = match &first {
        CommandReply::Recorded { receipt, .. } => match &receipt.decision {
            DomainDecision::Applied {
                result: OperationResult::AgentsReceive { value },
            } => {
                assert_eq!(value.items.0.len(), 1);
                assert_eq!(value.items.0[0].id, message_id);
                assert_eq!(value.items.0[0].body.0, "Durable inbox body");
                assert!(matches!(
                    value.items.0[0].delivery,
                    Some(devworks_core::protocol::records::RetainedDeliveryState::Delivered)
                ));
                value.next_cursor.0.clone()
            }
            other => panic!("unexpected receive decision: {other:?}"),
        },
        other => panic!("unexpected receive reply: {other:?}"),
    };
    let replay_start = std::time::Instant::now();
    assert!(matches!(
        command("agents.receive", "receive-first", receive.clone()),
        CommandReply::Recorded { replayed: true, .. }
    ));
    assert!(replay_start.elapsed() < Duration::from_secs(1));
    assert!(
        matches!(command("agents.receive", "receive-again", receive),
        CommandReply::Recorded { receipt, .. }
        if matches!(&receipt.decision, DomainDecision::Applied {
            result: OperationResult::AgentsReceive { value }
        } if value.items.0.len() == 1 && value.items.0[0].id == message_id))
    );
    assert!(matches!(command("agents.ack", "receive-ack", json!({
        "kind":"agents.ack","projectId":"project-a","upTo":cursor
    })), CommandReply::Recorded { receipt, .. }
        if matches!(&receipt.decision, DomainDecision::Applied {
            result: OperationResult::AgentsAck { value }
        } if value.acked.get() == 1)));
    assert!(
        matches!(command("agents.receive", "receive-after-ack", json!({
        "kind":"agents.receive","projectId":"project-a"
    })), CommandReply::Recorded { receipt, .. }
        if matches!(&receipt.decision, DomainDecision::NoChange {
            result: OperationResult::AgentsReceive { value }
        } if value.items.0.is_empty()))
    );
    for (key, body) in [
        (
            "receive-bad-wait",
            json!({"kind":"agents.receive","projectId":"project-a","waitSeconds":56}),
        ),
        (
            "receive-bad-cursor",
            json!({"kind":"agents.receive","projectId":"project-a","cursor":"Zm9v","waitSeconds":1}),
        ),
        (
            "receive-forged-session",
            json!({"kind":"agents.receive","projectId":"project-a","sessionId":"another-session","waitSeconds":1}),
        ),
    ] {
        assert!(
            matches!(command("agents.receive", key, body), CommandReply::Recorded { receipt, .. }
            if matches!(receipt.decision, DomainDecision::Refused { .. })),
            "{key}"
        );
    }
}

#[test]
fn selected_receive_cancellation_keeps_the_request_key_retryable() {
    let harness = Harness::new();
    let store = harness.open();
    seed_root(&harness, "root-a");
    let user = principal(store.as_ref(), "receive-cancel-user", PrincipalKind::Human);
    let runs = Arc::new(ControlService::with_scope(
        harness.ids.clone(),
        Arc::new(ScriptedScope::new()),
    ));
    let cases = UseCases::new().with_agents(Arc::new(AgentService::new(harness.ids.clone(), runs)));
    let authority = HostAuthority {
        store: store.as_ref(),
        clock: harness.clock.as_ref(),
        protected_branches: &[],
    };
    let gateway = Gateway {
        authority: &authority,
        store: store.as_ref(),
        ids: harness.ids.as_ref(),
    };
    let raw = serde_json::to_vec(&json!({
        "protocol":"1.0-draft.1", "mode":"Command", "schemaRevision":7,
        "requestKey":"receive-cancel-retry",
        "body":{"kind":"agents.receive","projectId":"project-a","waitSeconds":55}
    }))
    .unwrap();
    let cancellation = Cancellation::new();
    std::thread::scope(|scope| {
        let stop = cancellation.clone();
        let cases_ref = &cases;
        let gateway_ref = &gateway;
        let user_ref = &user;
        let raw_ref = &raw;
        let started = std::time::Instant::now();
        let waiting = scope.spawn(move || {
            cases_ref
                .command_with(
                    gateway_ref,
                    user_ref,
                    "agents.receive",
                    raw_ref,
                    WriteOptions {
                        cancellation: Some(stop),
                        ..WriteOptions::default()
                    },
                )
                .unwrap()
        });
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            write(store.as_ref(), |tx| tx.runs().agent_delivery_head()).unwrap(),
            0
        );
        cancellation.cancel();
        assert!(matches!(
            waiting.join().unwrap(),
            CommandReply::Rejected { .. }
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    });
    let deadline_start = std::time::Instant::now();
    assert!(matches!(
        cases
            .command_with(
                &gateway,
                &user,
                "agents.receive",
                &raw,
                WriteOptions {
                    deadline_ms: Some(harness.now() - 1),
                    ..WriteOptions::default()
                },
            )
            .unwrap(),
        CommandReply::Rejected { .. }
    ));
    assert!(deadline_start.elapsed() < Duration::from_secs(1));
    let post = serde_json::to_vec(&json!({
        "protocol":"1.0-draft.1", "mode":"Command", "schemaRevision":7,
        "requestKey":"receive-cancel-post",
        "body":{"kind":"agents.post","projectId":"project-a","body":"After cancellation","toHuman":true}
    })).unwrap();
    assert!(matches!(
        cases
            .command(&gateway, &user, "agents.post", &post)
            .unwrap(),
        CommandReply::Recorded { .. }
    ));
    assert!(
        matches!(cases.command(&gateway, &user, "agents.receive", &raw).unwrap(),
        CommandReply::Recorded { receipt, replayed: false, .. }
        if matches!(&receipt.decision, DomainDecision::Applied {
            result: OperationResult::AgentsReceive { value }
        } if value.items.0.len() == 1 && value.items.0[0].body.0 == "After cancellation"))
    );
}

#[tokio::test]
async fn delivered_message_replays_until_read_acknowledgment() {
    let harness = Harness::new();
    let store = harness.open();
    seed_root(&harness, "root-a");
    let user = principal(store.as_ref(), "inbox-user", PrincipalKind::Human);
    let project = ProjectId(id("project-a"));
    let send_context = user.clone();
    let send_project = project.clone();
    let posted = write(store.as_ref(), move |tx| {
        post_message(
            tx,
            &send_context,
            &PostSpec {
                project: send_project,
                body: "A durable question".into(),
                kind: MessageKind::Info,
                item: None,
                feature: None,
                resource: None,
                to_session: None,
                to_human: true,
                blocking: false,
                in_reply_to: None,
                resolves: None,
                idempotency_key: Some("inbox-replay".into()),
            },
        )
    })
    .unwrap();
    assert!(posted.created);

    let driver = RetainedInboxDriver::new(store.clone());
    let receive = |include_read| InboxSelection {
        recipient: Recipient::Human,
        after: 0,
        include_read,
        limit: 10,
        wait_seconds: 0,
    };
    let first = driver
        .receive(user.clone(), project.clone(), receive(false))
        .await
        .unwrap();
    assert_eq!(first.items.len(), 1);
    assert_eq!(first.items[0].0.state, "Delivered");
    assert_eq!(first.items[0].1.id, posted.message.id);
    let delivered_revision = first.items[0].0.revision;

    let replay = driver
        .receive(user.clone(), project.clone(), receive(false))
        .await
        .unwrap();
    assert_eq!(replay.items.len(), 1);
    assert_eq!(replay.items[0].0.state, "Delivered");
    assert_eq!(replay.items[0].0.revision, delivered_revision);

    let acknowledged = driver
        .acknowledge(
            user.clone(),
            project.clone(),
            Recipient::Human,
            AckSelection::Messages(vec![posted.message.id.clone()]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(acknowledged.changed.len(), 1);
    assert_eq!(acknowledged.changed[0].0.state, "Read");
    assert!(
        driver
            .receive(user.clone(), project.clone(), receive(false))
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let read = driver
        .receive(user.clone(), project.clone(), receive(true))
        .await
        .unwrap();
    assert_eq!(read.items.len(), 1);
    assert_eq!(read.items[0].0.state, "Read");

    let resumed = driver
        .wait_events(user.clone(), project.clone(), Some(0), 0)
        .await
        .unwrap();
    assert!(resumed.items.iter().any(|event| event.kind == "Posted"));
    assert!(resumed.items.iter().any(|event| event.kind == "Read"));
    assert_eq!(
        resumed.next_sequence,
        resumed.items.last().unwrap().sequence
    );
    assert!(
        driver
            .wait_events(
                user.clone(),
                project.clone(),
                Some(resumed.next_sequence),
                0
            )
            .await
            .unwrap()
            .items
            .is_empty()
    );
    rusqlite::Connection::open(harness.root.join("workspace.sqlite"))
        .unwrap()
        .execute_batch("INSERT INTO work_items VALUES('issue-b','project-a','issue',NULL,NULL,NULL,'Issue',1,0,1,1,1,'{}');")
        .unwrap();
    let blocking_context = user.clone();
    let blocking_project = project.clone();
    let question = write(store.as_ref(), move |tx| {
        post_message(
            tx,
            &blocking_context,
            &PostSpec {
                project: blocking_project,
                body: "Need human answer".into(),
                kind: MessageKind::Question,
                item: Some(devworks_core::protocol::RootId(id("issue-b"))),
                feature: None,
                resource: None,
                to_session: None,
                to_human: true,
                blocking: true,
                in_reply_to: None,
                resolves: None,
                idempotency_key: None,
            },
        )
    })
    .unwrap();
    assert_eq!(question.item_versions[0].version, 2);
    let resolved = driver
        .acknowledge(
            user.clone(),
            project.clone(),
            Recipient::Human,
            AckSelection::Messages(vec![question.message.id]),
            Some(devworks_core::services::agents::retained::Resolution::Answered),
        )
        .await
        .unwrap();
    assert_eq!(resolved.item_versions[0].version, 3);
    assert!(!common::read(store.as_ref(), |snapshot| {
        snapshot
            .runs()
            .open_blocking_question(&devworks_core::protocol::RootId(id("issue-b")))
    }));
    assert!(
        driver
            .wait_events(user, project, None, 0)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}
