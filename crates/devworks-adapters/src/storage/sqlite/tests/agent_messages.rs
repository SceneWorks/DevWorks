use super::{Harness, through};
use crate::storage::sqlite::{MIGRATIONS, SqliteStore};
use devworks_core::context::{HostIngress, PrincipalKind};
use devworks_core::operations::agents as ops;
use devworks_core::ports::storage::contract::StoreHarness;
use devworks_core::ports::storage::*;
use devworks_core::protocol::records::MessageClass;
use devworks_core::protocol::support::{BoundedList, BoundedText};
use devworks_core::protocol::{ActorId, AssignmentId, Counter, Id, ProjectId, Revision, RootId};
use serde_json::json;
use std::sync::Arc;

fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}
fn assignment(value: &str) -> AssignmentId {
    AssignmentId(id(value))
}
fn project(value: &str) -> ProjectId {
    ProjectId(id(value))
}

#[test]
fn itemless_direct_messages_pass_once_to_same_repository_successor() {
    use devworks_core::services::agents::retained::{
        AckSelection, MessageKind, PostSpec, Recipient, acknowledge, deliver_inbox,
        handover_direct, post_message,
    };
    let h = Harness::new();
    drop(SqliteStore::open_with(h.config(), &through(40)).unwrap());
    h.raw().execute_batch(
        "INSERT INTO actors VALUES('agent-a','agent','Agent'),('human-a','human','Human');
         INSERT INTO projects VALUES('project-a','A','Active',1,1,1);
         INSERT INTO repositories VALUES('repo-a','/a',1),('repo-b','/b',1);
         INSERT INTO project_repositories VALUES('project-a','repo-a'),('project-a','repo-b');
         INSERT INTO work_items VALUES('root-a','project-a','issue',NULL,NULL,NULL,'A',1,0,1,1,1,'{}'),
                                       ('root-b','project-a','issue',NULL,NULL,NULL,'B',1,0,1,1,1,'{}');
         INSERT INTO work_repositories VALUES('root-a','repo-a'),('root-b','repo-b');
         INSERT INTO conversations VALUES('conv-a','root-a',0),('conv-b','root-b',0);
         INSERT INTO sessions(id,conversation_id,agent_id,root_id,context_epoch,revision,project_id,created_at)
           VALUES('session-old','conv-a','agent-a','root-a',0,1,'project-a',1700000000000);"
    ).unwrap();
    let s = SqliteStore::open_with(h.config(), MIGRATIONS).unwrap();
    let ingress = HostIngress::new(&s);
    let human = ingress.context(
        ActorId(id("human-a")),
        PrincipalKind::Human,
        id("post-direct"),
        id("test-ingress"),
        Counter::new(s.info().restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    );
    let (unread, read) = run_write(&s, WriteOptions::default(), move |tx| {
        let spec = |body: &str| PostSpec {
            project: project("project-a"),
            body: body.into(),
            kind: MessageKind::Info,
            item: None,
            feature: None,
            resource: None,
            to_session: Some(id("session-old")),
            to_human: false,
            blocking: false,
            in_reply_to: None,
            resolves: None,
            idempotency_key: None,
        };
        Ok((
            post_message(tx, &human, &spec("unread"))?.message.id,
            post_message(tx, &human, &spec("already read"))?.message.id,
        ))
    })
    .unwrap();
    let agent = ingress
        .context(
            ActorId(id("agent-a")),
            PrincipalKind::Agent,
            id("ack-direct"),
            id("test-ingress"),
            Counter::new(s.info().restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
        .with_session(id("session-old"));
    run_write(&s, WriteOptions::default(), move |tx| {
        acknowledge(
            tx,
            &agent,
            &project("project-a"),
            &Recipient::Session(id("session-old")),
            AckSelection::Messages(vec![read]),
            None,
        )?;
        Ok(())
    })
    .unwrap();
    h.raw().execute_batch(
        r#"UPDATE sessions SET closed_at=1700000000000,closed_by='human-a' WHERE id='session-old';
         INSERT INTO sessions(id,conversation_id,agent_id,root_id,context_epoch,revision,project_id,created_at,repository_ids_json)
           VALUES('session-other','conv-b','agent-a','root-b',0,1,'project-a',1700000000000,'["repo-b"]'),
                 ('session-next','conv-a','agent-a','root-a',0,1,'project-a',1700000000000,'["repo-a"]');"#
    ).unwrap();
    h.raw()
        .execute_batch(
            "DELETE FROM work_repositories WHERE work_id='root-a';
         INSERT INTO work_repositories VALUES('root-a','repo-b');",
        )
        .unwrap();
    let inherited_id = unread.clone();
    let result = run_write(&s, WriteOptions::default(), move |tx| {
        assert_eq!(
            tx.sessions().session_repository_scope(&id("session-old"))?,
            vec![id("repo-a")]
        );
        assert_eq!(
            tx.sessions()
                .session_repository_scope(&id("session-next"))?,
            vec![id("repo-a")]
        );
        assert_eq!(handover_direct(tx, &id("session-other"))?, 0);
        assert_eq!(handover_direct(tx, &id("session-next"))?, 1);
        assert_eq!(handover_direct(tx, &id("session-next"))?, 0);
        let original = tx
            .runs()
            .agent_session_delivery(&unread, &id("session-old"))?
            .unwrap();
        assert_eq!(original.handed_to_session_id, Some(id("session-next")));
        assert!(!original.handed_over);
        let successor = tx
            .runs()
            .agent_session_delivery(&unread, &id("session-next"))?
            .unwrap();
        assert!(successor.handed_over);
        assert_eq!(successor.state, "Pending");
        Ok(())
    });
    result.unwrap();
    h.set_now(1_700_000_000_000 + 16 * 60 * 1_000);
    let active = ingress
        .context(
            ActorId(id("agent-a")),
            PrincipalKind::Agent,
            id("still-active"),
            id("test-ingress"),
            Counter::new(s.info().restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
        .with_session(id("session-next"));
    run_write(&s, WriteOptions::default(), move |tx| {
        post_message(
            tx,
            &active,
            &PostSpec {
                project: project("project-a"),
                body: "Still here".into(),
                kind: MessageKind::Info,
                item: None,
                feature: None,
                resource: None,
                to_session: None,
                to_human: true,
                blocking: false,
                in_reply_to: None,
                resolves: None,
                idempotency_key: None,
            },
        )?;
        Ok(())
    })
    .unwrap();
    h.raw().execute_batch(
        r#"INSERT INTO sessions(id,conversation_id,agent_id,root_id,context_epoch,revision,project_id,created_at,repository_ids_json)
           VALUES('session-later','conv-a','agent-a','root-a',0,1,'project-a',1700000960000,'["repo-a"]');"#
    ).unwrap();
    run_write(&s, WriteOptions::default(), |tx| {
        assert_eq!(handover_direct(tx, &id("session-later"))?, 0);
        Ok(())
    })
    .unwrap();
    h.set_now(1_700_000_000_000 + 31 * 60 * 1_000);
    let polling = ingress
        .context(
            ActorId(id("agent-a")),
            PrincipalKind::Agent,
            id("empty-poll"),
            id("test-ingress"),
            Counter::new(s.info().restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
        .with_session(id("session-next"));
    run_write(&s, WriteOptions::default(), move |tx| {
        let head = tx.runs().agent_delivery_head()?;
        let page = deliver_inbox(
            tx,
            &polling,
            &project("project-a"),
            &Recipient::Session(id("session-next")),
            head,
            false,
            10,
        )?;
        assert!(page.items.is_empty());
        assert_eq!(
            tx.sessions().session_activity_at_ms(&id("session-next"))?,
            Some(1_700_000_000_000 + 31 * 60 * 1_000)
        );
        Ok(())
    })
    .unwrap();
    h.set_now(1_700_000_000_000 + 32 * 60 * 1_000);
    h.raw().execute_batch(
        r#"INSERT INTO sessions(id,conversation_id,agent_id,root_id,context_epoch,revision,project_id,created_at,repository_ids_json)
           VALUES('session-after-silence','conv-a','agent-a','root-a',0,1,'project-a',1700001920000,'["repo-a"]');"#
    ).unwrap();
    run_write(&s, WriteOptions::default(), move |tx| {
        assert_eq!(handover_direct(tx, &id("session-after-silence"))?, 0);
        Ok(())
    })
    .unwrap();
    h.set_now(1_700_000_000_000 + 47 * 60 * 1_000);
    h.raw().execute_batch(
        r#"INSERT INTO sessions(id,conversation_id,agent_id,root_id,context_epoch,revision,project_id,created_at,repository_ids_json)
           VALUES('session-inherits-silent','conv-a','agent-a','root-a',0,1,'project-a',1700002820000,'["repo-a"]');"#
    ).unwrap();
    run_write(&s, WriteOptions::default(), move |tx| {
        assert_eq!(handover_direct(tx, &id("session-inherits-silent"))?, 1);
        assert_eq!(
            tx.runs()
                .agent_session_delivery(&inherited_id, &id("session-next"))?
                .unwrap()
                .handed_to_session_id,
            Some(id("session-inherits-silent"))
        );
        Ok(())
    })
    .unwrap();
    let human = ingress.context(
        ActorId(id("human-a")),
        PrincipalKind::Human,
        id("post-expiring"),
        id("test-ingress"),
        Counter::new(s.info().restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    );
    run_write(&s, WriteOptions::default(), move |tx| {
        post_message(
            tx,
            &human,
            &PostSpec {
                project: project("project-a"),
                body: "expires".into(),
                kind: MessageKind::Info,
                item: None,
                feature: None,
                resource: None,
                to_session: Some(id("session-old")),
                to_human: false,
                blocking: false,
                in_reply_to: None,
                resolves: None,
                idempotency_key: None,
            },
        )?;
        Ok(())
    })
    .unwrap();
    h.set_now(1_700_000_000_000 + 8 * 24 * 60 * 60 * 1_000);
    h.raw().execute_batch(
        r#"INSERT INTO sessions(id,conversation_id,agent_id,root_id,context_epoch,revision,project_id,created_at,repository_ids_json)
           VALUES('session-expired','conv-a','agent-a','root-a',0,1,'project-a',1700691200000,'["repo-a"]');"#
    ).unwrap();
    run_write(&s, WriteOptions::default(), |tx| {
        assert_eq!(handover_direct(tx, &id("session-expired"))?, 0);
        Ok(())
    })
    .unwrap();
}

#[test]
fn message_delivery_replays_after_reopen_and_rejects_cross_project_links() {
    let h = Harness::new();
    drop(SqliteStore::open_with(h.config(), &through(40)).unwrap());
    h.raw().execute_batch(
        "INSERT INTO actors VALUES('agent-a','agent','Agent'),('human-a','human','Human');
         INSERT INTO projects VALUES('project-a','A','Active',1,1,1),('project-b','B','Active',1,1,1);
         INSERT INTO work_items VALUES('root-a','project-a','issue',NULL,NULL,NULL,'A',1,0,1,1,1,'{}'),
                                       ('root-b','project-b','issue',NULL,NULL,NULL,'B',1,0,1,1,1,'{}'),
                                       ('root-c','project-a','issue',NULL,NULL,NULL,'C',1,0,1,1,1,'{}');
         INSERT INTO conversations VALUES('conv-a','root-a',0),('conv-b','root-b',0),('conv-home',NULL,0);
         INSERT INTO sessions(id,conversation_id,agent_id,root_id,context_epoch,revision)
           VALUES('session-a','conv-a','agent-a','root-a',0,1),
                 ('session-child','conv-a','agent-a','root-a',0,1),
                 ('session-b','conv-b','agent-a','root-b',0,1),
                 ('session-home','conv-home','agent-a',NULL,0,1);
         UPDATE sessions SET title='Home coordinator',project_id='project-a' WHERE id='session-home';
         INSERT INTO assignments(id,root_id,work_id,parent_id,session_id,state,generation,slot_held,revision,created_at,purpose)
           VALUES('sender','root-a','root-a',NULL,'session-a','Ready',0,0,1,0,'Primary'),
                 ('recipient','root-a','root-a','sender','session-child','Ready',0,0,1,0,'Child'),
                 ('foreign-recipient','root-b','root-b',NULL,'session-b','Ready',0,0,1,0,'Primary');"
    ).unwrap();
    let s = SqliteStore::open_with(h.config(), MIGRATIONS).unwrap();
    let context = StoredAgentAssignmentContext {
        assignment_id: assignment("recipient"),
        project_id: project("project-a"),
        role_id: id("worker"),
        role_instructions: "Approved worker instructions".into(),
        role_revisions: json!({"workspace": 3, "project": 2}),
        brief: "Investigate the root issue".into(),
        task_ids: json!([]),
        source_checkpoint_id: None,
        created_at_ms: 100,
    };
    let saved_context = context.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.runs().insert_agent_assignment_context(&saved_context)?;
        let mut foreign = context.clone();
        foreign.project_id = project("project-b");
        foreign.assignment_id = assignment("sender");
        assert!(matches!(
            tx.runs().insert_agent_assignment_context(&foreign),
            Err(StorageError::ScopeDenied)
        ));
        Ok(())
    })
    .unwrap();
    let message = StoredAgentMessage {
        id: id("message-a"),
        project_id: project("project-a"),
        root_id: Some(RootId(id("root-a"))),
        channel: "Assignment".into(),
        from_assignment_id: Some(assignment("sender")),
        to_assignment_id: Some(assignment("recipient")),
        message_class: "Question".into(),
        body: "Can you check this?".into(),
        attachments: json!([]),
        in_reply_to_id: None,
        state: Some("Open".into()),
        blocking: false,
        idempotency_key: Some("request-1".into()),
        revision: 1,
        created_at_ms: 100,
        resolved_at_ms: None,
        from_actor_id: ActorId(id("agent-a")),
        target_item_id: None,
        target_feature_id: None,
        resource_name: None,
        target_session_id: None,
        to_human: false,
        sender_session_id: Some(id("session-a")),
        resolved_by_actor_id: None,
        resolved_by_session_id: None,
    };
    let m = message.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.runs().insert_agent_message(&m)?;
        let delivery_sequence = tx.runs().agent_delivery_head()? + 1;
        tx.runs().insert_agent_delivery(&StoredAgentDelivery {
            sequence: delivery_sequence,
            message_id: m.id.clone(),
            project_id: m.project_id.clone(),
            recipient_assignment_id: m.to_assignment_id.clone(),
            recipient_human: false,
            state: "Pending".into(),
            revision: 1,
            delivered_at_ms: None,
            read_at_ms: None,
            recipient_session_id: None,
            handed_over: false,
            handed_to_session_id: None,
            resolved_at_ms: None,
        })?;
        let event_sequence = tx.runs().agent_event_head()? + 1;
        tx.runs()
            .insert_agent_message_event(&StoredAgentMessageEvent {
                sequence: event_sequence,
                project_id: m.project_id.clone(),
                message_id: m.id.clone(),
                kind: "Posted".into(),
                actor_id: ActorId(id("agent-a")),
                recipient_assignment_id: m.to_assignment_id.clone(),
                created_at_ms: 100,
                recipient_session_id: None,
                recipient_human: false,
            })?;
        let mut bad = m.clone();
        bad.id = id("message-bad");
        bad.to_assignment_id = Some(assignment("foreign-recipient"));
        assert!(matches!(
            tx.runs().insert_agent_message(&bad),
            Err(StorageError::ScopeDenied)
        ));
        let mut direct = m.clone();
        direct.id = id("message-direct");
        direct.channel = "Session".into();
        direct.to_assignment_id = None;
        direct.target_session_id = Some(id("session-child"));
        direct.message_class = "Info".into();
        direct.state = None;
        direct.body = "Direct session notice".into();
        direct.idempotency_key = Some("request-2".into());
        tx.runs().insert_agent_message(&direct)?;
        let direct_sequence = tx.runs().agent_delivery_head()? + 1;
        tx.runs().insert_agent_delivery(&StoredAgentDelivery {
            sequence: direct_sequence,
            message_id: direct.id,
            project_id: direct.project_id,
            recipient_assignment_id: None,
            recipient_session_id: Some(id("session-child")),
            recipient_human: false,
            state: "Pending".into(),
            revision: 1,
            delivered_at_ms: None,
            read_at_ms: None,
            resolved_at_ms: None,
            handed_over: false,
            handed_to_session_id: None,
        })?;
        let mut rootless = m.clone();
        rootless.id = id("message-rootless");
        rootless.channel = "Session".into();
        rootless.to_assignment_id = None;
        rootless.target_session_id = Some(id("session-home"));
        rootless.message_class = "Question".into();
        rootless.state = Some("Open".into());
        rootless.body = "Rootless project session".into();
        rootless.idempotency_key = Some("request-3".into());
        tx.runs().insert_agent_message(&rootless)?;
        let rootless_sequence = tx.runs().agent_delivery_head()? + 1;
        tx.runs().insert_agent_delivery(&StoredAgentDelivery {
            sequence: rootless_sequence,
            message_id: rootless.id,
            project_id: rootless.project_id,
            recipient_assignment_id: None,
            recipient_session_id: Some(id("session-home")),
            recipient_human: false,
            state: "Pending".into(),
            revision: 1,
            delivered_at_ms: None,
            read_at_ms: None,
            resolved_at_ms: None,
            handed_over: false,
            handed_to_session_id: None,
        })?;
        let mut oversized = m.clone();
        oversized.id = id("message-oversized");
        oversized.channel = "Session".into();
        oversized.to_assignment_id = None;
        oversized.target_session_id = Some(id("session-home"));
        oversized.body = "a".repeat(8001);
        assert!(matches!(
            tx.runs().insert_agent_message(&oversized),
            Err(StorageError::InvalidRecord(_))
        ));
        oversized.id = id("message-bad-kind");
        oversized.body = "valid length".into();
        oversized.message_class = "Evidence".into();
        assert!(matches!(
            tx.runs().insert_agent_message(&oversized),
            Err(StorageError::InvalidRecord(_))
        ));
        oversized.message_class = "Question".into();
        oversized.state = None;
        assert!(matches!(
            tx.runs().insert_agent_message(&oversized),
            Err(StorageError::InvalidRecord(_))
        ));
        Ok(())
    })
    .unwrap();
    drop(s);
    let s = SqliteStore::open_with(h.config(), MIGRATIONS).unwrap();
    read_snapshot(&s, &ReadOptions::default(), |snap| {
        let context = snap
            .runs()
            .agent_assignment_context(&assignment("recipient"))?
            .unwrap();
        assert_eq!(context.role_id.as_str(), "worker");
        assert_eq!(context.role_revisions, json!({"workspace":3,"project":2}));
        let inbox = snap.runs().agent_inbox(
            &project("project-a"),
            Some(&assignment("recipient")),
            0,
            false,
            10,
        )?;
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].0.sequence, 1);
        assert_eq!(inbox[0].1.body, "Can you check this?");
        let direct = snap.runs().agent_session_inbox(
            &project("project-a"),
            &id("session-child"),
            0,
            false,
            10,
        )?;
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].1.body, "Direct session notice");
        let rootless = snap.runs().agent_session_inbox(
            &project("project-a"),
            &id("session-home"),
            0,
            false,
            10,
        )?;
        assert_eq!(rootless.len(), 1);
        assert_eq!(rootless[0].1.body, "Rootless project session");
        assert_eq!(
            snap.runs()
                .agent_session_delivery(&id("message-direct"), &id("session-child"))?
                .unwrap()
                .sequence,
            2
        );
        assert!(
            snap.runs()
                .agent_inbox(
                    &project("project-b"),
                    Some(&assignment("recipient")),
                    0,
                    false,
                    10
                )?
                .is_empty()
        );
        assert_eq!(
            snap.runs()
                .agent_message_events(&project("project-a"), 0, 10)?
                .len(),
            1
        );
        let candidates = snap
            .projects()
            .purge_candidates(&project("project-a"))?
            .unwrap();
        for table in [
            "agent_messages",
            "agent_message_deliveries",
            "agent_message_events",
            "agent_assignment_context",
        ] {
            assert!(candidates.rows.iter().any(|row| row.table == table));
        }
        let foreign = snap
            .projects()
            .purge_candidates(&project("project-b"))?
            .unwrap();
        for table in [
            "agent_messages",
            "agent_message_deliveries",
            "agent_message_events",
            "agent_assignment_context",
        ] {
            assert!(!foreign.rows.iter().any(|row| row.table == table));
        }
        Ok(())
    })
    .unwrap();
    let ingress = HostIngress::new(&s);
    let child_context = ingress
        .context(
            ActorId(id("agent-a")),
            PrincipalKind::Agent,
            id("delivery-invocation"),
            id("delivery-ingress"),
            Counter::new(s.info().restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
        .with_session(id("session-child"))
        .with_lineage(
            Some(project("project-a")),
            Some(RootId(id("root-a"))),
            Some(assignment("recipient")),
        );
    for _ in 0..2 {
        let context = child_context.clone();
        run_write(&s, WriteOptions::default(), move |tx| {
            let page = devworks_core::services::agents::retained::deliver_inbox(
                tx,
                &context,
                &project("project-a"),
                &devworks_core::services::agents::retained::Recipient::Session(id("session-child")),
                0,
                false,
                10,
            )?;
            assert_eq!(page.items.len(), 1);
            assert_eq!(page.next_sequence, 2);
            assert_eq!(page.items[0].0.state, "Delivered");
            Ok(())
        })
        .unwrap();
    }
    read_snapshot(&s, &ReadOptions::default(), |snap| {
        assert_eq!(
            snap.runs()
                .agent_session_delivery(&id("message-direct"), &id("session-child"))?
                .unwrap()
                .revision,
            2
        );
        Ok(())
    })
    .unwrap();
    for expected in [1, 0] {
        let context = child_context.clone();
        run_write(&s, WriteOptions::default(), move |tx| {
            let result = devworks_core::services::agents::retained::acknowledge(
                tx,
                &context,
                &project("project-a"),
                &devworks_core::services::agents::retained::Recipient::Session(id("session-child")),
                devworks_core::services::agents::retained::AckSelection::Messages(vec![id(
                    "message-direct",
                )]),
                None,
            )?;
            assert_eq!(result.changed.len(), expected);
            Ok(())
        })
        .unwrap();
    }
    read_snapshot(&s, &ReadOptions::default(), |snap| {
        assert_eq!(
            snap.runs()
                .agent_session_delivery(&id("message-direct"), &id("session-child"))?
                .unwrap()
                .state,
            "Read"
        );
        Ok(())
    })
    .unwrap();
    let home_context = ingress
        .context(
            ActorId(id("agent-a")),
            PrincipalKind::Agent,
            id("home-delivery-invocation"),
            id("home-delivery-ingress"),
            Counter::new(s.info().restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
        .with_session(id("session-home"))
        .with_lineage(Some(project("project-a")), None, None);
    let context = home_context.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        let result = devworks_core::services::agents::retained::acknowledge(
            tx,
            &context,
            &project("project-a"),
            &devworks_core::services::agents::retained::Recipient::Session(id("session-home")),
            devworks_core::services::agents::retained::AckSelection::Through(3),
            None,
        )?;
        assert_eq!(result.changed.len(), 1);
        assert_eq!(result.changed[0].1.id, id("message-rootless"));
        Ok(())
    })
    .unwrap();
    for expected in [1, 0] {
        let context = home_context.clone();
        run_write(&s, WriteOptions::default(), move |tx| {
            let result = devworks_core::services::agents::retained::acknowledge(
                tx,
                &context,
                &project("project-a"),
                &devworks_core::services::agents::retained::Recipient::Session(id("session-home")),
                devworks_core::services::agents::retained::AckSelection::Messages(vec![id(
                    "message-rootless",
                )]),
                Some(devworks_core::services::agents::retained::Resolution::Answered),
            )?;
            assert_eq!(result.changed.len(), expected);
            if expected == 1 {
                assert_eq!(result.changed[0].1.state.as_deref(), Some("Answered"));
            }
            Ok(())
        })
        .unwrap();
    }
    let spec = devworks_core::services::agents::retained::PostSpec {
        project: project("project-a"),
        body: "Please review this item".into(),
        kind: devworks_core::services::agents::retained::MessageKind::Question,
        item: Some(RootId(id("root-a"))),
        feature: None,
        resource: None,
        to_session: None,
        to_human: true,
        blocking: true,
        in_reply_to: None,
        resolves: None,
        idempotency_key: Some("review-request".into()),
    };
    let context = child_context.clone();
    let (new_message, replay) = run_write(&s, WriteOptions::default(), move |tx| {
        let created = devworks_core::services::agents::retained::post_message(tx, &context, &spec)?;
        let repeated =
            devworks_core::services::agents::retained::post_message(tx, &context, &spec)?;
        Ok((created, repeated))
    })
    .unwrap();
    assert!(new_message.created);
    assert!(!replay.created);
    assert_eq!(new_message.message.id, replay.message.id);
    assert_eq!(new_message.recipients, 1);
    read_snapshot(&s, &ReadOptions::default(), |snap| {
        assert!(snap.runs().open_blocking_question(&RootId(id("root-a")))?);
        Ok(())
    })
    .unwrap();
    let human_context = ingress.context(
        ActorId(id("human-a")),
        PrincipalKind::Human,
        id("human-invocation"),
        id("human-ingress"),
        Counter::new(s.info().restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    );
    let parent_id = new_message.message.id.clone();
    let reply = run_write(&s, WriteOptions::default(), move |tx| {
        let reply = devworks_core::services::agents::retained::post_message(
            tx,
            &human_context,
            &devworks_core::services::agents::retained::PostSpec {
                project: project("project-a"),
                body: "Approved".into(),
                kind: devworks_core::services::agents::retained::MessageKind::Info,
                item: None,
                feature: None,
                resource: None,
                to_session: None,
                to_human: false,
                blocking: false,
                in_reply_to: Some(parent_id),
                resolves: Some(devworks_core::services::agents::retained::Resolution::Answered),
                idempotency_key: None,
            },
        )?;
        assert_eq!(reply.message.channel, "Session");
        assert_eq!(reply.recipients, 1);
        assert_eq!(
            reply.resolved.as_ref().and_then(|m| m.state.as_deref()),
            Some("Answered")
        );
        Ok(reply)
    })
    .unwrap();
    read_snapshot(&s, &ReadOptions::default(), |snap| {
        assert!(!snap.runs().open_blocking_question(&RootId(id("root-a")))?);
        Ok(())
    })
    .unwrap();
    read_snapshot(&s, &ReadOptions::default(), |snap| {
        let delivery = snap
            .runs()
            .agent_session_delivery(&reply.message.id, &id("session-child"))?
            .unwrap();
        assert_eq!(delivery.state, "Pending");
        Ok(())
    })
    .unwrap();
    let human_context = ingress.context(
        ActorId(id("human-a")),
        PrincipalKind::Human,
        id("human-handover-invocation"),
        id("human-ingress"),
        Counter::new(s.info().restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    );
    let direct = run_write(&s, WriteOptions::default(), move |tx| {
        let posted = devworks_core::services::agents::retained::post_message(
            tx,
            &human_context,
            &devworks_core::services::agents::retained::PostSpec {
                project: project("project-a"),
                body: "Open item question".into(),
                kind: devworks_core::services::agents::retained::MessageKind::Question,
                item: Some(RootId(id("root-a"))),
                feature: None,
                resource: None,
                to_session: Some(id("session-a")),
                to_human: false,
                blocking: false,
                in_reply_to: None,
                resolves: None,
                idempotency_key: None,
            },
        )?;
        assert_eq!(
            devworks_core::services::agents::retained::handover_item(tx, &assignment("recipient"))?,
            1
        );
        assert_eq!(
            devworks_core::services::agents::retained::handover_item(tx, &assignment("recipient"))?,
            0
        );
        let broadcast = devworks_core::services::agents::retained::post_message(
            tx,
            &human_context,
            &devworks_core::services::agents::retained::PostSpec {
                project: project("project-a"),
                body: "Project update".into(),
                kind: devworks_core::services::agents::retained::MessageKind::Announcement,
                item: None,
                feature: None,
                resource: None,
                to_session: None,
                to_human: false,
                blocking: false,
                in_reply_to: None,
                resolves: None,
                idempotency_key: None,
            },
        )?;
        assert_eq!(broadcast.recipients, 0);
        Ok(posted)
    })
    .unwrap();
    read_snapshot(&s, &ReadOptions::default(), |snap| {
        let delivery = snap
            .runs()
            .agent_session_delivery(&direct.message.id, &id("session-child"))?
            .unwrap();
        assert_eq!(delivery.state, "Pending");
        assert!(delivery.handed_over);
        Ok(())
    })
    .unwrap();
    let history_context = ingress.context(
        ActorId(id("human-a")),
        PrincipalKind::Human,
        id("history-invocation"),
        id("history-ingress"),
        Counter::new(s.info().restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    );
    read_snapshot(&s, &ReadOptions::default(), |snap| {
        let mut query = devworks_core::services::agents::retained::MessageHistoryQuery {
            project: project("project-a"),
            channel: Some("Session".into()),
            item: Some(RootId(id("root-a"))),
            feature: None,
            resource: None,
            open_only: false,
            to_human: false,
            before: None,
            limit: 1,
        };
        let first =
            devworks_core::services::agents::retained::history(snap, &history_context, &query)?;
        assert_eq!(first.items.len(), 1);
        query.before = first.next_before;
        assert!(query.before.is_some());
        let second =
            devworks_core::services::agents::retained::history(snap, &history_context, &query)?;
        assert_eq!(second.items.len(), 1);
        assert_ne!(first.items[0].id, second.items[0].id);
        query.open_only = true;
        query.before = None;
        let open =
            devworks_core::services::agents::retained::history(snap, &history_context, &query)?;
        assert_eq!(open.items[0].id, direct.message.id);
        let fresh = devworks_core::services::agents::retained::events(
            snap,
            &history_context,
            &project("project-a"),
            None,
            100,
        )?;
        assert!(fresh.items.is_empty());
        let replay = devworks_core::services::agents::retained::events(
            snap,
            &history_context,
            &project("project-a"),
            Some(0),
            100,
        )?;
        assert!(replay.items.iter().any(|event| event.kind == "HandedOver"));
        assert_eq!(replay.next_sequence, fresh.next_sequence);
        Ok(())
    })
    .unwrap();
    run_write(&s, WriteOptions::default(), move |tx| {
        let mut delivery = tx
            .runs()
            .agent_delivery(&id("message-a"), Some(&assignment("recipient")))?
            .unwrap();
        delivery.state = "Read".into();
        delivery.revision += 1;
        delivery.delivered_at_ms = Some(110);
        delivery.read_at_ms = Some(110);
        tx.runs().update_agent_delivery(&delivery, 1)?;
        let mut resolved = tx.runs().agent_message(&id("message-a"))?.unwrap();
        resolved.state = Some("Answered".into());
        resolved.revision += 1;
        resolved.resolved_at_ms = Some(111);
        resolved.resolved_by_actor_id = Some(ActorId(id("agent-a")));
        tx.runs().resolve_agent_message(&resolved, 1)?;
        assert!(matches!(
            tx.runs().resolve_agent_message(&resolved, 1),
            Err(StorageError::InvalidRecord(_))
        ));
        Ok(())
    })
    .unwrap();
    read_snapshot(&s, &ReadOptions::default(), |snap| {
        assert!(
            snap.runs()
                .agent_inbox(
                    &project("project-a"),
                    Some(&assignment("recipient")),
                    0,
                    false,
                    10
                )?
                .is_empty()
        );
        assert_eq!(
            snap.runs().agent_inbox(
                &project("project-a"),
                Some(&assignment("recipient")),
                0,
                true,
                10
            )?[0]
                .0
                .state,
            "Read"
        );
        assert_eq!(
            snap.runs()
                .agent_message_by_key(&project("project-a"), &ActorId(id("agent-a")), "request-1")?
                .unwrap()
                .state
                .as_deref(),
            Some("Answered")
        );
        Ok(())
    })
    .unwrap();

    let ingress = HostIngress::new(&s);
    let context = |assignment_name: &str, session_name: &str, root_name: &str| {
        ingress
            .context(
                ActorId(id("agent-a")),
                PrincipalKind::Agent,
                id("invocation"),
                id("test-ingress"),
                Counter::new(s.info().restore_epoch).unwrap(),
                Revision::new(1).unwrap(),
            )
            .with_session(id(session_name))
            .with_lineage(
                Some(project("project-a")),
                Some(RootId(id(root_name))),
                Some(assignment(assignment_name)),
            )
            .with_activation(id("sender-execution"), 1)
    };
    let stale_sender = context("sender", "session-a", "root-a");
    assert!(matches!(
        run_write(&s, WriteOptions::default(), move |tx| {
            devworks_core::services::agents::post_assignment(
                tx,
                &stale_sender,
                &ops::MessagesInput {
                    recipient_assignment_id: assignment("recipient"),
                    message_class: MessageClass::Status,
                    content: BoundedText("Too early".into()),
                    attachments: BoundedList(vec![]),
                },
            )
        }),
        Err(StorageError::ScopeDenied)
    ));
    let boot = s.info().boot_id.clone().unwrap();
    run_write(&s, WriteOptions::default(), move |tx| {
        let stale = tx.runs().assignment(&assignment("sender"))?.unwrap();
        assert_eq!(stale.state, AssignmentState::Ready);
        tx.runs().insert_activation(&StoredActivation {
            id: id("sender-execution"),
            root_id: RootId(id("root-a")),
            boot_id: boot,
            generation: 1,
            phase: ActivationPhase::Running,
            purpose: ActivationPurpose::Implementation,
            owner: Some(id("test-owner")),
            lease_until_ms: Some(i64::MAX),
            owner_epoch: 1,
            revision: 1,
            attempts: 1,
            ready_at_ms: 0,
            created_at_ms: 0,
            stop_cause: None,
            stop_requested_at_ms: None,
            stop_acknowledged_at_ms: None,
            stopped_at_ms: None,
            outcome: None,
            detail: None,
            plan_revision: None,
        })?;
        let mut running = stale.clone();
        running.state = AssignmentState::Active;
        running.execution_id = Some(id("sender-execution"));
        running.revision += 1;
        tx.runs().update_assignment(&running, stale.revision)
    })
    .unwrap();
    let stale_epoch_sender =
        context("sender", "session-a", "root-a").with_activation(id("sender-execution"), 0);
    assert!(matches!(
        run_write(&s, WriteOptions::default(), move |tx| {
            devworks_core::services::agents::post_assignment(
                tx,
                &stale_epoch_sender,
                &ops::MessagesInput {
                    recipient_assignment_id: assignment("recipient"),
                    message_class: MessageClass::Status,
                    content: BoundedText("Old owner".into()),
                    attachments: BoundedList(vec![]),
                },
            )
        }),
        Err(StorageError::ScopeDenied)
    ));
    let sender_context = context("sender", "session-a", "root-a");
    let recipient_context = context("recipient", "session-child", "root-a");
    let input = ops::MessagesInput {
        recipient_assignment_id: assignment("recipient"),
        message_class: MessageClass::Status,
        content: BoundedText("Native service message".into()),
        attachments: BoundedList(vec![]),
    };
    let posted = run_write(&s, WriteOptions::default(), move |tx| {
        let sender = tx.runs().assignment(&assignment("sender"))?.unwrap();
        let activation = tx.runs().activation(&id("sender-execution"))?.unwrap();
        assert_eq!(sender.state, AssignmentState::Active);
        assert_eq!(activation.phase, ActivationPhase::Running);
        assert_eq!(tx.store().boot_id.as_ref(), Some(&activation.boot_id));
        assert!(activation.lease_until_ms.unwrap() > tx.now_ms());
        devworks_core::services::agents::post_assignment(tx, &sender_context, &input)
    })
    .unwrap();
    assert_eq!(posted.to_assignment_id, assignment("recipient"));
    assert!(!posted.acknowledged);
    read_snapshot(&s, &ReadOptions::default(), |snap| {
        let inbox = snap.runs().agent_inbox(
            &project("project-a"),
            Some(&assignment("recipient")),
            0,
            false,
            10,
        )?;
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].1.id, posted.id);
        let thread = snap.runs().agent_assignment_thread(
            &project("project-a"),
            &assignment("sender"),
            "Outbound",
            false,
            None,
            10,
        )?;
        assert!(thread.iter().any(|(message, _)| message.id == posted.id));
        let unread = snap.runs().agent_assignment_thread(
            &project("project-a"),
            &assignment("recipient"),
            "Inbound",
            true,
            None,
            10,
        )?;
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].0.id, posted.id);
        Ok(())
    })
    .unwrap();
    let ack = ops::AcknowledgeInput {
        assignment_id: assignment("recipient"),
        message_id: posted.id.clone(),
        expected_revision: Revision::new(1).unwrap(),
    };
    let acknowledged = run_write(&s, WriteOptions::default(), move |tx| {
        devworks_core::services::agents::acknowledge_assignment(tx, &recipient_context, &ack)
    })
    .unwrap();
    assert!(acknowledged.acknowledged);
    assert_eq!(acknowledged.revision.get(), 2);
    read_snapshot(&s, &ReadOptions::default(), |snap| {
        assert!(
            snap.runs()
                .agent_assignment_thread(
                    &project("project-a"),
                    &assignment("recipient"),
                    "Inbound",
                    true,
                    None,
                    10
                )?
                .is_empty()
        );
        Ok(())
    })
    .unwrap();
    run_write(&s, WriteOptions::default(), |tx| {
        let mut assignment = tx.runs().assignment(&assignment("recipient"))?.unwrap();
        let expected = assignment.revision;
        assignment.state = AssignmentState::Completed;
        assignment.revision += 1;
        tx.runs().update_assignment(&assignment, expected)?;
        Ok(())
    })
    .unwrap();
    let closed_recipient = context("recipient", "session-child", "root-a");
    let repeated_ack = ops::AcknowledgeInput {
        assignment_id: assignment("recipient"),
        message_id: posted.id.clone(),
        expected_revision: Revision::new(2).unwrap(),
    };
    assert!(matches!(
        run_write(&s, WriteOptions::default(), move |tx| {
            devworks_core::services::agents::acknowledge_assignment(
                tx,
                &closed_recipient,
                &repeated_ack,
            )
        }),
        Err(StorageError::ScopeDenied)
    ));

    // A root issue without a feature can record a child intent with a brief and no cards.
    let coordinator = context("sender", "session-a", "root-a");
    let service = devworks_core::services::agents::AgentService::new(
        h.ids.clone(),
        Arc::new(devworks_core::services::runs::ControlService::new(
            h.ids.clone(),
        )),
    );
    let foreign_task = ops::DelegateInput {
        root_id: RootId(id("root-a")),
        expected_control_revision: Revision::new(1).unwrap(),
        task_ids: BoundedList(vec![id("root-c")]),
        role_id: id("worker"),
        instruction: BoundedText("Investigate the root issue".into()),
        source_checkpoint_id: None,
    };
    let rejected_service = devworks_core::services::agents::AgentService::new(
        h.ids.clone(),
        Arc::new(devworks_core::services::runs::ControlService::new(
            h.ids.clone(),
        )),
    );
    let rejected_context = coordinator.clone();
    let rejected = run_write(&s, WriteOptions::default(), move |tx| {
        rejected_service
            .delegate(tx, &rejected_context, &foreign_task)
            .map(|_| ())
            .map_err(|error| StorageError::InvalidRecord(format!("delegation refused: {error:?}")))
    });
    assert!(
        rejected.is_err(),
        "another issue in the project is outside this root"
    );
    let request = ops::DelegateInput {
        root_id: RootId(id("root-a")),
        expected_control_revision: Revision::new(1).unwrap(),
        task_ids: BoundedList(vec![]),
        role_id: id("worker"),
        instruction: BoundedText("Investigate the root issue".into()),
        source_checkpoint_id: None,
    };
    let child = run_write(&s, WriteOptions::default(), move |tx| {
        service
            .delegate(tx, &coordinator, &request)
            .map_err(|error| StorageError::InvalidRecord(format!("delegation refused: {error:?}")))
    })
    .unwrap();
    assert_eq!(child.root_id, RootId(id("root-a")));
    assert_eq!(child.role_id.as_str(), "worker");
    read_snapshot(&s, &ReadOptions::default(), |snapshot| {
        let child_assignment = snapshot.runs().assignment(&child.assignment_id)?.unwrap();
        assert_eq!(child_assignment.parent_id, Some(assignment("sender")));
        assert_eq!(child_assignment.purpose, AssignmentPurpose::Child);
        let child_session = snapshot
            .sessions()
            .session(&child_assignment.session_id)?
            .unwrap();
        assert!(
            snapshot
                .authority()
                .principal(&child_session.agent_id)?
                .is_none()
        );
        let profile = snapshot
            .runs()
            .agent_assignment_context(&child.assignment_id)?
            .unwrap();
        assert_eq!(profile.brief, "Investigate the root issue");
        assert_eq!(profile.task_ids, json!([]));
        assert_eq!(
            snapshot
                .runs()
                .live_assignment_for_work(&RootId(id("root-a")))?
                .unwrap()
                .id,
            assignment("sender")
        );
        Ok(())
    })
    .unwrap();
    let host = ingress
        .context(
            ActorId(id("host")),
            PrincipalKind::Host,
            id("host-invocation"),
            id("host-namespace"),
            Counter::new(s.info().restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
        .with_lineage(Some(project("project-a")), None, None);
    let child_id = child.assignment_id.clone();
    let actor = run_write(&s, WriteOptions::default(), move |tx| {
        devworks_core::services::agents::bind_child_authority(tx, &host, &child_id).map_err(
            |error| StorageError::InvalidRecord(format!("host binding refused: {error:?}")),
        )
    })
    .unwrap();
    read_snapshot(&s, &ReadOptions::default(), |snapshot| {
        let binding = snapshot.authority().principal(&actor)?.unwrap();
        assert!(binding.active);
        assert_eq!(
            binding.role,
            Some(devworks_core::settings::catalog::AgentRole::Worker)
        );
        assert_eq!(binding.project_id, Some(project("project-a")));
        Ok(())
    })
    .unwrap();
    run_write(&s, WriteOptions::default(), |tx| {
        let mut activation = tx.runs().activation(&id("sender-execution"))?.unwrap();
        let expected = activation.revision;
        activation.lease_until_ms = Some(tx.now_ms());
        activation.revision += 1;
        tx.runs().update_activation(&activation, expected)
    })
    .unwrap();
    let expired_sender = context("sender", "session-a", "root-a");
    assert!(matches!(
        run_write(&s, WriteOptions::default(), move |tx| {
            devworks_core::services::agents::post_assignment(
                tx,
                &expired_sender,
                &ops::MessagesInput {
                    recipient_assignment_id: assignment("recipient"),
                    message_class: MessageClass::Status,
                    content: BoundedText("Expired lease".into()),
                    attachments: BoundedList(vec![]),
                },
            )
        }),
        Err(StorageError::ScopeDenied)
    ));

    let artifacts = Arc::new(devworks_core::services::artifacts::ArtifactService::new(
        Arc::new(crate::fs::artifacts::NativeArtifactFiles::new(
            h.root.0.clone(),
            h.root.0.join("cache"),
        )),
        Arc::new(crate::fs::sources::HostArtifactSources::new(
            h.clock.clone(),
            h.ids.clone(),
        )),
        Arc::new(crate::fs::NativeFilesystem::new()),
        h.clock.clone(),
        h.ids.clone(),
    ));
    let host = ingress.context(
        ActorId(id("host")),
        PrincipalKind::Host,
        id("notice-invocation"),
        id("notice-namespace"),
        Counter::new(s.info().restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    );
    for (screen, context) in [
        ("features", "Viewing feature board"),
        ("issues", "Viewing project issues"),
    ] {
        devworks_core::services::sessions::notice::append_screen_context(
            &s,
            &artifacts,
            &host,
            &id("session-home"),
            screen,
            context,
        )
        .unwrap();
    }
    read_snapshot(&s, &ReadOptions::default(), |snapshot| {
        let entries = snapshot.sessions().entries(&id("session-home"), 0, 10)?;
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry.kind == "HostNotice"));
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            snapshot
                .sessions()
                .session(&id("session-home"))?
                .unwrap()
                .next_sequence,
            3
        );
        Ok(())
    })
    .unwrap();
    let notice = read_snapshot(&s, &ReadOptions::default(), |snapshot| {
        let entry = snapshot
            .sessions()
            .entries(&id("session-home"), 0, 1)?
            .remove(0);
        serde_json::from_value::<devworks_core::protocol::provider::TranscriptBody>(entry.body)
            .map_err(|_| StorageError::InvalidRecord("notice fixture".into()))
    })
    .unwrap();
    let agent = ingress
        .context(
            ActorId(id("agent-a")),
            PrincipalKind::Agent,
            id("notice-spoof"),
            id("notice-spoof-namespace"),
            Counter::new(s.info().restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
        .with_session(id("session-home"));
    let outcome = run_write(&s, WriteOptions::default(), move |tx| {
        let batch = devworks_core::services::sessions::turn::batch_of(
            &id("session-home"),
            tx.new_id(devworks_core::ports::IdKind::TranscriptBatch),
            0,
            3,
            vec![notice],
            || tx.new_id(devworks_core::ports::IdKind::TranscriptEntry),
        );
        devworks_core::services::sessions::transcript::append_session_batch(
            tx,
            &agent,
            agent.principal(),
            &devworks_core::services::sessions::transcript::SessionBatch::entries(batch),
            devworks_core::services::sessions::transcript::AppendPath::Runtime,
        )
    })
    .unwrap();
    assert!(matches!(
        outcome,
        devworks_core::services::sessions::transcript::BatchOutcome::Refused(
            devworks_core::services::sessions::transcript::BatchRefusal::Unauthorized(
                "HostNoticeRequiresHost"
            )
        )
    ));
}
