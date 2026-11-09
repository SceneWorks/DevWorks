//! Retained inbox delivery crosses a write boundary; a lost reply must not lose a message.
mod common;

use common::{Harness, id, principal, seed_root, write};
use devworks_core::authority::HostAuthority;
use devworks_core::context::PrincipalKind;
use devworks_core::gateway::Gateway;
use devworks_core::protocol::results::{DomainDecision, OperationResult};
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
    let stored = common::read(store.as_ref(), |snapshot| {
        snapshot.runs().agent_message(&message_id)
    });
    assert_eq!(stored.unwrap().body, "Gateway message");
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
    assert!(
        driver
            .wait_events(user, project, None, 0)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}
