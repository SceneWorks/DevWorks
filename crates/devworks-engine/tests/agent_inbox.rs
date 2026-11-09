//! Retained inbox delivery crosses a write boundary; a lost reply must not lose a message.
mod common;

use common::{Harness, id, principal, seed_root, write};
use devworks_core::context::PrincipalKind;
use devworks_core::protocol::ProjectId;
use devworks_core::services::agents::retained::{
    AckSelection, MessageKind, PostSpec, Recipient, post_message,
};
use devworks_engine::agent_inbox::{InboxSelection, RetainedInboxDriver};

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
