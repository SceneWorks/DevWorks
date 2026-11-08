use super::{Harness, through};
use crate::storage::sqlite::{MIGRATIONS, SqliteStore};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, AssignmentId, Id, ProjectId, RootId};
use serde_json::json;

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
fn message_delivery_replays_after_reopen_and_rejects_cross_project_links() {
    let h = Harness::new();
    drop(SqliteStore::open_with(h.config(), &through(40)).unwrap());
    h.raw().execute_batch(
        "INSERT INTO actors VALUES('agent-a','agent','Agent');
         INSERT INTO projects VALUES('project-a','A','Active',1,1,1),('project-b','B','Active',1,1,1);
         INSERT INTO work_items VALUES('root-a','project-a','issue',NULL,NULL,NULL,'A',1,0,1,1,1,'{}'),
                                       ('root-b','project-b','issue',NULL,NULL,NULL,'B',1,0,1,1,1,'{}');
         INSERT INTO conversations VALUES('conv-a','root-a',0),('conv-b','root-b',0);
         INSERT INTO sessions(id,conversation_id,agent_id,root_id,context_epoch,revision)
           VALUES('session-a','conv-a','agent-a','root-a',0,1),
                 ('session-child','conv-a','agent-a','root-a',0,1),
                 ('session-b','conv-b','agent-a','root-b',0,1);
         INSERT INTO assignments(id,root_id,work_id,parent_id,session_id,state,generation,slot_held,revision,created_at,purpose)
           VALUES('sender','root-a','root-a',NULL,'session-a','Ready',0,0,1,0,'Primary'),
                 ('recipient','root-a','root-a','sender','session-child','Ready',0,0,1,0,'Child'),
                 ('foreign-recipient','root-b','root-b',NULL,'session-b','Ready',0,0,1,0,'Primary');"
    ).unwrap();
    let s = SqliteStore::open_with(h.config(), MIGRATIONS).unwrap();
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
        })?;
        Ok(())
    })
    .unwrap();
    drop(s);
    let s = SqliteStore::open_with(h.config(), MIGRATIONS).unwrap();
    read_snapshot(&s, &ReadOptions::default(), |snap| {
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
        ] {
            assert!(!foreign.rows.iter().any(|row| row.table == table));
        }
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
}
