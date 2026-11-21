use super::*;
use devworks_core::protocol::{
    AttemptId,
    provider::{ArtifactRef, Digest},
};
use serde_json::Value;

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}

fn artifact(s: &str) -> ArtifactRef {
    ArtifactRef {
        artifact_id: id(s),
        version_id: id("test-version"),
        sha256: Digest::try_from("a".repeat(64)).unwrap(),
        byte_length: Counter::new(1).unwrap(),
        media_type: "application/json".into(),
    }
}

fn run(name: &str, cutoffs: DreamCutoffs) -> DreamRunRow {
    DreamRunRow {
        id: id(name),
        scope: KnowledgeScope::Workspace,
        continued_from: None,
        operation_id: None,
        initiator: ActorId(id("host")),
        trigger: DreamTrigger::Manual,
        revision: 1,
        generation: 1,
        phase: DreamPhase::Queued,
        settings_revision: 1,
        policy: DreamPolicy::default(),
        cutoffs,
        started_at_ms: T0,
        ended_at_ms: None,
        checkpoint: None,
        report: None,
    }
}

fn progress(phase: DreamPhase, checkpoint: Option<DreamCheckpoint>) -> DreamProgress {
    DreamProgress {
        phase,
        checkpoint,
        report: phase.ended().then(|| json!({"summary":"test report"})),
    }
}

#[test]
fn scoped_settings_are_compare_and_set_and_keep_completed_watermarks() {
    let h = Harness::new();
    let s = h.store();
    let policy = DreamPolicy {
        enabled: false,
        max_tokens: 32000,
        ..Default::default()
    };
    let saved = write(&s, move |tx| {
        tx.dreaming()?
            .set_dream_policy(&KnowledgeScope::Workspace, 1, &policy)
    })
    .unwrap();
    assert_eq!(saved.revision, 2);
    assert!(!saved.policy.enabled);
    assert!(matches!(
        write(&s, |tx| tx.dreaming()?.set_dream_policy(
            &KnowledgeScope::Workspace,
            1,
            &DreamPolicy::default()
        )),
        Err(StorageError::RevisionConflict(_)),
    ));
    drop(s);
    let reopened = h.store();
    let saved = read_snapshot(&reopened, &ReadOptions::default(), |snapshot| {
        snapshot.dreaming()?.dream_scope(&KnowledgeScope::Workspace)
    })
    .unwrap();
    assert_eq!((saved.revision, saved.policy.max_tokens), (2, 32000));
    assert!(!saved.policy.enabled);
}

#[test]
fn completion_promotes_only_the_frozen_window_and_atomically_retires_the_checkpoint() {
    let h = Harness::new();
    let s = h.store();
    let first_cutoff = DreamCutoffs {
        sweep_started_at_ms: T0,
        knowledge: 4,
        reviews: 9,
        sources: 6,
        ..Default::default()
    };
    let first = run("first", first_cutoff.clone());
    write(&s, move |tx| {
        let store = tx.dreaming()?;
        store.insert_dream_run(&first)?;
        assert!(matches!(
            store.insert_dream_run(&run("busy", DreamCutoffs::default())),
            Err(StorageError::Busy)
        ));
        let active =
            store.advance_dream_run(&first.id, 1, 1, &progress(DreamPhase::Running, None))?;
        store.advance_dream_run(
            &first.id,
            active.revision,
            active.generation,
            &progress(
                DreamPhase::Partial,
                Some(DreamCheckpoint {
                    turn: 1,
                    ..Default::default()
                }),
            ),
        )?;
        assert!(
            store
                .dream_scope(&KnowledgeScope::Workspace)?
                .completed
                .is_none()
        );
        Ok(())
    })
    .unwrap();

    drop(s);
    let s = h.store();
    let published = first_cutoff.clone();
    let resumed = write(&s, move |tx| {
        let store = tx.dreaming()?;
        let parent = store.dream_run(&id("first"))?.unwrap();
        let mut resumed = run("resumed", parent.cutoffs);
        resumed.continued_from = Some(parent.id);
        resumed.trigger = DreamTrigger::Resume;
        resumed.checkpoint = parent.checkpoint;
        let mut drifted = resumed.clone();
        drifted.cutoffs.reviews += 1;
        assert!(matches!(
            store.insert_dream_run(&drifted),
            Err(StorageError::InvalidRecord(_))
        ));
        store.insert_dream_run(&resumed)?;
        store.advance_dream_run(
            &resumed.id,
            1,
            1,
            &progress(DreamPhase::Running, resumed.checkpoint),
        )
    })
    .unwrap();

    let attempt = resumed.clone();
    assert!(matches!(
        write::<()>(&s, move |tx| {
            tx.dreaming()?.advance_dream_run(
                &attempt.id,
                attempt.revision,
                attempt.generation,
                &progress(DreamPhase::Completed, attempt.checkpoint),
            )?;
            Err(StorageError::InvalidRecord("fixture rollback".into()))
        }),
        Err(StorageError::InvalidRecord(_))
    ));
    write(&s, move |tx| {
        let store = tx.dreaming()?;
        assert!(
            store
                .dream_scope(&KnowledgeScope::Workspace)?
                .completed
                .is_none()
        );
        let saved = store.advance_dream_run(
            &resumed.id,
            resumed.revision,
            resumed.generation,
            &progress(DreamPhase::Completed, resumed.checkpoint),
        )?;
        assert!(saved.checkpoint.is_none());
        assert_eq!(
            store
                .dream_scope(&KnowledgeScope::Workspace)?
                .completed
                .unwrap()
                .0,
            published
        );
        let new_work = DreamCutoffs {
            knowledge: first_cutoff.knowledge + 2,
            sources: first_cutoff.sources + 3,
            ..first_cutoff
        };
        store.insert_dream_run(&run("new-window", new_work))?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn possibly_sent_identity_survives_reopen_and_requires_its_exact_observation() {
    let h = Harness::new();
    let s = h.store();
    write(&s, |tx| {
        let store = tx.dreaming()?;
        let run = run("request-run", DreamCutoffs::default());
        store.insert_dream_run(&run)?;
        let mut cp = DreamCheckpoint {
            request: Some(DreamRequest {
                logical_id: id("logical-request"),
                attempt_id: Some(AttemptId(id("attempt"))),
                frame: artifact("frame"),
                stage: DreamRequestStage::Prepared,
                result: None,
            }),
            ..Default::default()
        };
        let r = store.advance_dream_run(
            &run.id,
            1,
            1,
            &progress(DreamPhase::Running, Some(cp.clone())),
        )?;
        cp.request.as_mut().unwrap().stage = DreamRequestStage::PossiblySent;
        let r = store.advance_dream_run(
            &r.id,
            r.revision,
            r.generation,
            &progress(DreamPhase::Running, Some(cp.clone())),
        )?;
        store.advance_dream_run(
            &r.id,
            r.revision,
            r.generation,
            &progress(DreamPhase::OutcomeUnknown, Some(cp)),
        )?;
        Ok(())
    })
    .unwrap();

    drop(s);
    let s = h.store();
    write(&s, |tx| {
        let store = tx.dreaming()?;
        let r = store.dream_live_run(&KnowledgeScope::Workspace)?.unwrap();
        assert_eq!(r.phase, DreamPhase::OutcomeUnknown);
        assert!(matches!(
            store.insert_dream_run(&run("delayed", DreamCutoffs::default())),
            Err(StorageError::Busy)
        ));
        assert!(matches!(
            store.put_dream_action(&r.id, r.generation, &id("action"), &json!({})),
            Err(StorageError::LeaseLost { .. })
        ));
        let mut cp = r.checkpoint.unwrap();
        let request = cp.request.as_mut().unwrap();
        request.stage = DreamRequestStage::Observed;
        request.result = Some(artifact("result"));
        let mut wrong = cp.clone();
        wrong.request.as_mut().unwrap().attempt_id = Some(AttemptId(id("different-attempt")));
        assert!(matches!(
            store.advance_dream_run(
                &r.id,
                r.revision,
                r.generation,
                &progress(DreamPhase::Running, Some(wrong))
            ),
            Err(StorageError::InvalidRecord(_))
        ));
        let observed = store.advance_dream_run(
            &r.id,
            r.revision,
            r.generation,
            &progress(DreamPhase::Running, Some(cp)),
        )?;
        let outcome: Value = json!({"writeId":"committed-write"});
        let first =
            store.put_dream_action(&observed.id, observed.generation, &id("action"), &outcome)?;
        let replay =
            store.put_dream_action(&observed.id, observed.generation, &id("action"), &outcome)?;
        assert_eq!(first.position, replay.position);
        assert_eq!(first.action, replay.action);
        assert_eq!(store.dream_actions(&observed.id, None, 10)?.len(), 1);
        Ok(())
    })
    .unwrap();
}
