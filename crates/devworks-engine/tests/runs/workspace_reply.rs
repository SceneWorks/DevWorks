use super::*;
use devworks_core::protocol::OperationState;
use devworks_core::services::sessions::workspace_reply::{CAPABILITY, ReplyIntent};

#[test]
fn a_legacy_home_send_adopts_the_owner_and_records_one_exact_ordered_reply() {
    let _native_case = native_runtime_case();
    let f = Fx::new();
    let legacy = id("legacy-workspace-session");
    let actor = f.user.principal().clone();
    let session_id = legacy.clone();
    f.tx(move |_, tx| {
        let session = StoredSession {
            id: session_id,
            conversation_id: id("legacy-workspace-conversation"),
            agent_id: actor.clone(),
            project_id: None,
            root_id: None,
            context_epoch: 0,
            revision: 1,
            title: "Existing Home chat".into(),
            created_by: actor,
            created_at_ms: tx.now_ms(),
            closed_at_ms: None,
            closed_by: None,
            next_sequence: 1,
            checkpoint_sequence: 0,
        };
        tx.sessions().insert_session(&session)
    });
    let owner = ActorId(id("workspace-coordinator"));
    let mut ports = composition(&f, None, Arc::new(FakePower::default()));
    ports.application.sessions = Some(devworks_engine::SessionPorts {
        coordinator: Some(owner.clone()),
    });
    runtime().block_on(async {
        let app = boot(ports).await.unwrap();
        let operation = "sessions.append_message";
        let frame = |key, revision| {
            json!({"protocol": "1.0-draft.1", "requestKey": key, "sessionId": legacy,
                "expectedSessionRevision": revision, "content":[{"kind":"Text","text":"First reply"}]})
        };
        let stale = app.command(
            &f.user, operation, frame("stale-legacy", "2").to_string().as_bytes(),
        ).unwrap();
        assert!(matches!(decision(&stale), DomainDecision::Refused { .. }));
        let still_human = read(&*f.store, |r| r.sessions().session(&legacy)).unwrap();
        assert_eq!(&still_human.agent_id, f.user.principal());

        let mut send = frame("first-legacy-reply", "1");
        send["replyRoute"] = json!({"intent":{"kind":"Tier","tier":"Mid"},"effort":"High"});
        send["schemaRevision"] = json!(2);
        assert!(matches!(
            app.command(&f.user, operation, send.to_string().as_bytes()).unwrap(),
            CommandReply::Rejected { .. }
        ));
        send["schemaRevision"] = json!(3);
        let exact = send.to_string();
        applied(&app.command(&f.user, operation, exact.as_bytes()).unwrap());
        assert!(replayed(&app.command(&f.user, operation, exact.as_bytes()).unwrap()));
        send["replyRoute"]["effort"] = json!("Low");
        assert!(matches!(
            app.command(&f.user, operation, send.to_string().as_bytes()).unwrap(),
            CommandReply::Rejected { code: devworks_core::protocol::Rejection::IdempotencyConflict, .. }
        ));
        read(&*f.store, |r| {
            let adopted = r.sessions().session(&legacy)?.unwrap();
            assert_eq!(adopted.agent_id, owner);
            assert_eq!(&adopted.created_by, f.user.principal());
            assert_eq!(adopted.revision, 2);
            let entries = r.sessions().entries(&legacy, 0, 10)?;
            let [entry] = entries.as_slice() else { panic!("one exact saved message") };
            let intents = r.operations().operations_for_capability(
                CAPABILITY, &[OperationState::IntentRecorded], None, 10,
            )?;
            let [intent] = intents.as_slice() else { panic!("one exact reply intent") };
            let intent: ReplyIntent = serde_json::from_value(intent.payload.clone()).unwrap();
            assert_eq!(intent.user_entry_id, entry.entry_id);
            assert_eq!(intent.user_sequence, entry.sequence);
            assert_eq!(intent.session_id, legacy);
            assert_eq!(intent.agent_id, owner);
            assert_eq!(
                serde_json::to_value(intent.route.unwrap()).unwrap(),
                json!({"intent":{"kind":"Tier","tier":"Mid"},"effort":"High"})
            );
            Ok(())
        });
        app.shutdown().await;
    });
}
