//! Native A→B/unlink handoff boundary, with no inference or filesystem execution.
use super::{activate, id, project, seeded, work};
use crate::storage::sqlite::SqliteStore;
use crate::storage::sqlite::tests::{Harness, T0};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, AssignmentId, EventId, OperationId, RootId};
use serde_json::{Value, json};

fn assignment(name: &str, work: &str, root: &str, parent: Option<&str>) -> StoredAssignment {
    StoredAssignment {
        id: AssignmentId(id(name)),
        root_id: RootId(id(root)),
        work_id: RootId(id(work)),
        parent_id: parent.map(|p| AssignmentId(id(p))),
        session_id: id(&format!("session-{name}")),
        execution_id: None,
        state: AssignmentState::Active,
        generation: 1,
        slot_held: false,
        revision: 1,
        checkpoint: None,
        scope_key: None,
        scope_digest: None,
        plan_revision: None,
        instruction_revision: 0,
        created_at_ms: T0,
    }
}
fn session(a: &StoredAssignment, actor: ActorId) -> StoredSession {
    StoredSession {
        id: a.session_id.clone(),
        conversation_id: id(&format!("conversation-{}", a.id.0.as_str())),
        agent_id: actor.clone(),
        project_id: Some(project()),
        root_id: Some(a.root_id.clone()),
        title: "Worker retained".into(),
        context_epoch: 0,
        revision: 1,
        created_by: actor,
        created_at_ms: T0,
        closed_at_ms: None,
        closed_by: None,
        next_sequence: 1,
        checkpoint_sequence: 0,
    }
}
fn marker(previous: Option<Value>) -> Value {
    json!({"kind":"DevWorksMembershipHandoff","version":1,"membershipHandoff":{"hadCheckpoint":previous.is_some(),"previousCheckpoint":previous,"subtree":["worker","descendant"]}})
}
fn fixture(h: &Harness, previous: Option<Value>) -> (SqliteStore, StoredAssignment) {
    let s = seeded(h);
    let old_execution = activate(&s, "feature");
    let actor = s.info().host_actor;
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.work().insert_work(&work("feature-b", "feature", None))?;
        for a in [
            assignment("coord-a", "feature", "feature", None),
            assignment("coord-b", "feature-b", "feature-b", None),
        ] {
            tx.sessions().insert_session(&session(&a, actor.clone()))?;
            tx.runs().insert_assignment(&a)?;
        }
        let mut worker = assignment("worker", "task-a", "feature", Some("coord-a"));
        worker.state = AssignmentState::Superseded;
        worker.generation = 2;
        worker.checkpoint = Some(marker(previous));
        worker.execution_id = Some(old_execution.id);
        tx.sessions()
            .insert_session(&session(&worker, actor.clone()))?;
        tx.runs().insert_assignment(&worker)?;
        let mut child = assignment("descendant", "task-b", "feature", Some("worker"));
        child.state = AssignmentState::Superseded;
        tx.sessions()
            .insert_session(&session(&child, actor.clone()))?;
        tx.runs().insert_assignment(&child)?;
        tx.sessions().insert_entry(&StoredEntry {
            session_id: worker.session_id.clone(),
            sequence: 1,
            entry_id: id("prior-entry"),
            batch_id: id("prior-batch"),
            context_epoch: 0,
            actor: actor.clone(),
            kind: "UserMessage".into(),
            created_at_ms: T0,
            body: json!({"text":"retained transcript"}),
        })?;
        let mut persisted = tx.sessions().session(&worker.session_id)?.unwrap();
        persisted.next_sequence = 2;
        persisted.checkpoint_sequence = 1;
        tx.sessions().update_session(&persisted)?;
        let mut changed = tx.work().work(&id("task-a"))?.unwrap();
        changed.parent_feature_id = Some(id("feature-b"));
        changed.revision = 2;
        tx.work().update_work(&changed, 1)?;
        tx.history().append(NewEvent {
            id: EventId(id("prior-event")),
            actor,
            project_id: Some(project()),
            root_id: Some(RootId(id("feature"))),
            operation_id: None,
            aggregate_kind: "work".into(),
            aggregate_id: "task-a".into(),
            aggregate_revision: 1,
            kind: "PriorWork".into(),
            payload: json!({"root":"feature","cost":12}),
        })?;
        Ok(())
    })
    .unwrap();
    let moved = read_snapshot(&s, &ReadOptions::default(), |r| {
        let mut moved = r.runs().assignment(&AssignmentId(id("worker")))?.unwrap();
        moved.root_id = RootId(id("feature-b"));
        moved.parent_id = Some(AssignmentId(id("coord-b")));
        moved.execution_id = None;
        moved.state = AssignmentState::Waiting;
        moved.generation += 1;
        moved.revision += 1;
        moved.checkpoint = moved
            .checkpoint
            .as_ref()
            .and_then(|c| c.get("membershipHandoff"))
            .and_then(|m| {
                m.get("hadCheckpoint")
                    .and_then(Value::as_bool)
                    .filter(|v| *v)
                    .and_then(|_| m.get("previousCheckpoint").cloned())
            });
        Ok(moved)
    })
    .unwrap();
    (s, moved)
}
fn reparent(
    s: &SqliteStore,
    moved: StoredAssignment,
    assignment: i64,
    session: i64,
) -> Result<(), StorageError> {
    run_write(s, WriteOptions::default(), move |tx| {
        tx.runs().reparent_assignment(&moved, assignment, session)
    })
}
fn hold(
    name: &str,
    assignment: Option<&str>,
    operation: Option<&str>,
    root: &str,
) -> StoredBlocker {
    StoredBlocker {
        id: id(name),
        root_id: Some(RootId(id(root))),
        assignment_id: assignment.map(|s| AssignmentId(id(s))),
        operation_id: operation.map(|s| OperationId(id(s))),
        cause: HoldCause::HumanPause,
        origin_id: name.into(),
        origin_revision: 1,
        state: BlockerState::Active,
        revision: 1,
        reason: name.into(),
        clearance: json!({"manualContinue":true}),
        created_at_ms: T0,
        cleared_at_ms: None,
        cleared_by: None,
    }
}

#[test]
fn handoff_moves_same_worker_session_and_only_local_holds_preserving_history() {
    let h = Harness::new();
    let checkpoint = Some(json!({"checkpoint":"exact","revision":7}));
    let (s, moved) = fixture(&h, checkpoint.clone());
    run_write(&s, WriteOptions::default(), |tx| {
        tx.operations().record_intent(NewOperation {
            id: OperationId(id("operation")),
            root_id: Some(RootId(id("feature"))),
            assignment_id: Some(AssignmentId(id("worker"))),
            capability: "test".into(),
            schema_version: 1,
            args_digest: "a".repeat(64),
            payload: json!({}),
        })?;
        for b in [
            hold("local", Some("worker"), None, "feature"),
            hold("op-local", None, Some("operation"), "feature"),
            hold("global", None, None, "feature"),
            hold("child-local", Some("descendant"), None, "feature"),
            hold("task-pause", None, None, "task-a"),
        ] {
            tx.runs().insert_blocker(&b)?;
        }
        Ok(())
    })
    .unwrap();
    let entries = read_snapshot(&s, &ReadOptions::default(), |r| {
        r.sessions().entries(&id("session-worker"), 0, 500)
    })
    .unwrap();
    reparent(&s, moved.clone(), 1, 1).unwrap();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        assert_eq!(r.runs().assignment(&moved.id)?, Some(moved.clone()));
        assert_eq!(
            r.runs().live_assignment_for_work(&RootId(id("task-a")))?,
            Some(moved.clone())
        );
        let session = r.sessions().session(&moved.session_id)?.unwrap();
        assert_eq!(
            (
                session.context_epoch,
                session.revision,
                session.next_sequence,
                session.checkpoint_sequence
            ),
            (1, 2, 2, 1)
        );
        assert_eq!(session.root_id, Some(RootId(id("feature-b"))));
        assert_eq!(session.agent_id, s.info().host_actor);
        assert_eq!(r.sessions().entries(&moved.session_id, 0, 500)?, entries);
        for name in ["local", "op-local"] {
            let b = r.runs().blocker(&id(name))?.unwrap();
            assert_eq!(b.root_id, Some(RootId(id("feature-b"))));
            assert_eq!(b.revision, 2);
            assert_eq!(b.clearance, json!({"manualContinue":true}));
        }
        for (name, root) in [
            ("global", "feature"),
            ("child-local", "feature"),
            ("task-pause", "task-a"),
        ] {
            let b = r.runs().blocker(&id(name))?.unwrap();
            assert_eq!(b.root_id, Some(RootId(id(root))));
            assert_eq!(b.revision, 1);
        }
        let prior = r.history().event(&EventId(id("prior-event")))?.unwrap();
        assert_eq!(prior.root_id, Some(RootId(id("feature"))));
        assert_eq!(prior.payload, json!({"root":"feature","cost":12}));
        let child = r
            .runs()
            .assignment(&AssignmentId(id("descendant")))?
            .unwrap();
        assert_eq!(child.root_id, RootId(id("feature")));
        assert_eq!(child.state, AssignmentState::Superseded);
        assert_eq!(child.parent_id, Some(moved.id.clone()));
        Ok(())
    })
    .unwrap();
    s.with_writer_connection(|c| {
        let root: String = super::super::super::conn::db(c.query_row(
            "SELECT root_id FROM conversations WHERE id='conversation-worker'",
            [],
            |r| r.get(0),
        ))?;
        assert_eq!(root, "feature-b");
        Ok(())
    })
    .unwrap();
}

#[test]
fn handoff_checks_both_revisions_scope_parent_and_ordinary_immutability() {
    let h = Harness::new();
    let (s, moved) = fixture(&h, Some(Value::Null));
    assert!(matches!(
        reparent(&s, moved.clone(), 2, 1),
        Err(StorageError::RevisionConflict(_))
    ));
    assert!(matches!(
        reparent(&s, moved.clone(), 1, 2),
        Err(StorageError::RevisionConflict(_))
    ));
    let mut wrong = moved.clone();
    wrong.parent_id = Some(AssignmentId(id("coord-a")));
    assert!(matches!(
        reparent(&s, wrong, 1, 1),
        Err(StorageError::InvalidRecord(_))
    ));
    let mut wrong = moved.clone();
    wrong.checkpoint = None;
    assert!(matches!(
        reparent(&s, wrong, 1, 1),
        Err(StorageError::InvalidRecord(_))
    ));
    let mut missing_parent = moved.clone();
    missing_parent.parent_id = None;
    assert!(matches!(
        reparent(&s, missing_parent, 1, 1),
        Err(StorageError::InvalidRecord(_))
    ));
    let valid = moved.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        assert!(matches!(
            tx.runs().update_assignment(&moved, 1),
            Err(StorageError::InvalidRecord(_))
        ));
        let mut session = tx.sessions().session(&moved.session_id)?.unwrap();
        session.root_id = Some(moved.root_id.clone());
        tx.sessions().update_session(&session)?;
        assert_eq!(
            tx.sessions().session(&moved.session_id)?.unwrap().root_id,
            Some(RootId(id("feature")))
        );
        Ok(())
    })
    .unwrap();
    reparent(&s, valid.clone(), 1, 1).unwrap();
    assert_eq!(valid.checkpoint, Some(Value::Null));
}

#[test]
fn handoff_unlink_preserves_absent_checkpoint_and_requires_no_parent() {
    let h = Harness::new();
    let (s, mut moved) = fixture(&h, None);
    run_write(&s, WriteOptions::default(), |tx| {
        let mut w = tx.work().work(&id("task-a"))?.unwrap();
        w.parent_feature_id = None;
        w.revision += 1;
        tx.work().update_work(&w, 2)
    })
    .unwrap();
    moved.root_id = moved.work_id.clone();
    moved.parent_id = None;
    reparent(&s, moved.clone(), 1, 1).unwrap();
    assert!(moved.checkpoint.is_none());
    read_snapshot(&s, &ReadOptions::default(), |r| {
        let session = r.sessions().session(&moved.session_id)?.unwrap();
        assert_eq!(session.root_id, Some(moved.work_id.clone()));
        assert_eq!(session.context_epoch, 1);
        Ok(())
    })
    .unwrap();
}

#[test]
fn handoff_savepoint_rolls_back_assignment_and_epoch_when_hold_move_fails() {
    let h = Harness::new();
    let (s, moved) = fixture(&h, None);
    run_write(&s, WriteOptions::default(), |tx| {
        let a = hold("collision", Some("worker"), None, "feature");
        let mut b = a.clone();
        b.id = id("existing-target");
        b.root_id = Some(RootId(id("feature-b")));
        tx.runs().insert_blocker(&a)?;
        tx.runs().insert_blocker(&b)
    })
    .unwrap();
    run_write(&s, WriteOptions::default(), move |tx| {
        assert!(tx.runs().reparent_assignment(&moved, 1, 1).is_err());
        Ok(())
    })
    .unwrap();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        let a = r.runs().assignment(&AssignmentId(id("worker")))?.unwrap();
        assert_eq!(a.root_id, RootId(id("feature")));
        assert_eq!(a.state, AssignmentState::Superseded);
        let session = r.sessions().session(&a.session_id)?.unwrap();
        assert_eq!((session.context_epoch, session.revision), (0, 1));
        assert_eq!(
            r.runs().blocker(&id("collision"))?.unwrap().root_id,
            Some(RootId(id("feature")))
        );
        Ok(())
    })
    .unwrap();
}

#[test]
fn handoff_refuses_live_descendants_turns_uncertain_effects_and_processes() {
    for hazard in 0..6 {
        let h = Harness::new();
        let (s, moved) = fixture(&h, None);
        let info = s.info();
        s.with_writer_connection(move|c|{
            let db=super::super::super::conn::db;
            match hazard {
                0=>{db(c.execute("UPDATE assignments SET state='Waiting' WHERE id='descendant'",[]))?;},
                1=>{db(c.execute("INSERT INTO session_turns(session_id,turn_id,assignment_id,execution_id,owner_epoch,boot_id,started_at) VALUES('session-descendant','turn','descendant','execution-feature',0,?1,?2)",rusqlite::params![info.boot_id.unwrap().as_str(),T0]))?;},
                2=>{db(c.execute("INSERT INTO operations(id,root_id,assignment_id,capability,schema_version,args_digest,payload_json,state,revision,created_at) VALUES('unsafe','feature','descendant','test',1,'digest','{}','OutcomeUnknown',1,?1)",[T0]))?;},
                3=>{db(c.execute("INSERT INTO owned_processes(id,boot_id,state,platform,program,argv_sha256,actor_id,session_id,project_id,root_id,assignment_id,created_at,updated_at,revision) VALUES('process',?1,'Uncertain','unix','worker',?2,?3,'session-descendant','project','feature','descendant',?4,?4,1)",rusqlite::params![info.boot_id.unwrap().as_str(),"a".repeat(64),info.host_actor.0.as_str(),T0]))?;},
                4=>{
                    db(c.execute("INSERT INTO operations(id,root_id,assignment_id,capability,schema_version,args_digest,payload_json,state,revision,created_at) VALUES('unsafe','feature','worker','test',1,'digest','{}','IntentRecorded',1,?1)",[T0]))?;
                    db(c.execute("INSERT INTO queue_entries(id,consumer,event_id,root_id,operation_id,priority,ready_at,state,payload_json) VALUES('queued','test','prior-event','feature','unsafe',0,?1,'Ready','{}')",[T0]))?;
                }
                _=>{db(c.execute("INSERT INTO operations(id,root_id,assignment_id,capability,schema_version,args_digest,payload_json,state,revision,created_at) VALUES('unsafe','feature','worker','test',1,'digest','{}','NeedsInput',1,?1)",[T0]))?;}
            }
            Ok(())
        }).unwrap();
        assert!(
            matches!(reparent(&s, moved.clone(), 1, 1), Err(StorageError::Busy)),
            "hazard {hazard}"
        );
        read_snapshot(&s, &ReadOptions::default(), |r| {
            assert_eq!(
                r.sessions()
                    .session(&moved.session_id)?
                    .unwrap()
                    .context_epoch,
                0
            );
            assert_eq!(
                r.runs().assignment(&moved.id)?.unwrap().root_id,
                RootId(id("feature"))
            );
            Ok(())
        })
        .unwrap();
        s.with_writer_connection(move |c| {
            let db = super::super::super::conn::db;
            match hazard {
                0 => {
                    db(c.execute(
                        "UPDATE assignments SET state='Superseded' WHERE id='descendant'",
                        [],
                    ))?;
                }
                1 => {
                    db(c.execute("DELETE FROM session_turns WHERE turn_id='turn'", []))?;
                }
                2 | 5 => {
                    db(c.execute(
                        "UPDATE operations SET state='ObservedComplete' WHERE id='unsafe'",
                        [],
                    ))?;
                }
                3 => {
                    db(c.execute(
                        "UPDATE owned_processes SET state='Reaped' WHERE id='process'",
                        [],
                    ))?;
                }
                _ => {
                    db(c.execute(
                        "UPDATE queue_entries SET state='Cancelled' WHERE id='queued'",
                        [],
                    ))?;
                }
            }
            Ok(())
        })
        .unwrap();
        reparent(&s, moved, 1, 1).unwrap();
    }
}

#[test]
fn global_assignment_lookup_finds_worker_under_old_membership_using_existing_index() {
    let h = Harness::new();
    let (s, moved) = fixture(&h, None);
    s.with_writer_connection(|c|{
        let db=super::super::super::conn::db;
        db(c.execute("UPDATE assignments SET state='Waiting' WHERE id='worker'",[]))?;
        let detail:String=super::super::super::conn::db(c.query_row("EXPLAIN QUERY PLAN SELECT id FROM assignments WHERE work_id='task-a' AND state IN ('Ready','Active','Waiting') LIMIT 1",[],|row|row.get(3)))?;
        assert!(detail.contains("one_live_assignment"),"{detail}");Ok(())
    }).unwrap();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        let worker = r
            .runs()
            .live_assignment_for_work(&RootId(id("task-a")))?
            .unwrap();
        assert_eq!(worker.root_id, RootId(id("feature")));
        assert_eq!(
            r.work().work(&id("task-a"))?.unwrap().parent_feature_id,
            Some(id("feature-b"))
        );
        Ok(())
    })
    .unwrap();
    assert!(
        matches!(
            reparent(&s, moved, 1, 1),
            Err(StorageError::InvalidRecord(_))
        ),
        "a live source must first be fenced"
    );
}

#[test]
fn aba_reassociation_restores_marked_worker_only_after_quiescence_and_both_cas() {
    let h = Harness::new();
    let (s, mut restored) = fixture(&h, Some(json!({"saved":"checkpoint"})));
    run_write(&s, WriteOptions::default(), |tx| {
        let mut task = tx.work().work(&id("task-a"))?.unwrap();
        task.parent_feature_id = Some(id("feature"));
        task.revision += 1;
        tx.work().update_work(&task, 2)
    })
    .unwrap();
    restored.root_id = RootId(id("feature"));
    restored.parent_id = Some(AssignmentId(id("coord-a")));
    let boot = s.info().boot_id.unwrap();
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.runs().insert_turn(&StoredTurn {
            session_id: id("session-worker"),
            turn_id: id("aba-turn"),
            assignment_id: AssignmentId(id("worker")),
            execution_id: id("execution-feature"),
            owner_epoch: 0,
            boot_id: boot,
            started_at_ms: T0,
        })
    })
    .unwrap();
    assert!(
        !read_snapshot(&s, &ReadOptions::default(), |r| r
            .runs()
            .assignment_handoff_quiescent(&restored.id))
        .unwrap()
    );
    assert!(matches!(
        reparent(&s, restored.clone(), 1, 1),
        Err(StorageError::Busy)
    ));
    run_write(&s, WriteOptions::default(), |tx| {
        tx.runs()
            .delete_turn(&id("session-worker"), &id("aba-turn"))
    })
    .unwrap();
    assert!(matches!(
        reparent(&s, restored.clone(), 2, 1),
        Err(StorageError::RevisionConflict(_))
    ));
    assert!(matches!(
        reparent(&s, restored.clone(), 1, 2),
        Err(StorageError::RevisionConflict(_))
    ));
    reparent(&s, restored.clone(), 1, 1).unwrap();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        let assignment = r.runs().assignment(&restored.id)?.unwrap();
        assert_eq!(assignment, restored);
        assert_eq!(assignment.generation, 3);
        assert_eq!(assignment.checkpoint, Some(json!({"saved":"checkpoint"})));
        let session = r.sessions().session(&assignment.session_id)?.unwrap();
        assert_eq!(
            (session.root_id, session.context_epoch, session.revision),
            (Some(RootId(id("feature"))), 1, 2)
        );
        Ok(())
    })
    .unwrap();
    assert!(matches!(
        reparent(&s, restored, 1, 1),
        Err(StorageError::RevisionConflict(_))
    ));
}

#[test]
fn operation_handoff_read_fact_uses_canonical_uncertainty_without_queue_entries() {
    for state in [
        "Dispatching",
        "OutcomeUnknown",
        "NeedsInput",
        "RetryPending",
    ] {
        let h = Harness::new();
        let (s, moved) = fixture(&h, None);
        s.with_writer_connection(move|c|{super::super::super::conn::db(c.execute("INSERT INTO operations(id,root_id,assignment_id,capability,schema_version,args_digest,payload_json,state,revision,created_at) VALUES('direct','feature','worker','test',1,'digest','{}',?1,1,?2)",rusqlite::params![state,T0]))?;Ok(())}).unwrap();
        let quiescent = read_snapshot(&s, &ReadOptions::default(), |r| {
            r.runs().assignment_handoff_quiescent(&moved.id)
        })
        .unwrap();
        if state == "RetryPending" {
            assert!(quiescent);
            reparent(&s, moved, 1, 1).unwrap();
        } else {
            assert!(!quiescent, "{state}");
            assert!(
                matches!(reparent(&s, moved, 1, 1), Err(StorageError::Busy)),
                "{state}"
            );
        }
    }
}

#[test]
fn inherited_requirement_read_fact_keeps_all_ids_and_refuses_invalid_stored_shapes() {
    let h = Harness::new();
    let (s, _) = fixture(&h, None);
    read_snapshot(&s, &ReadOptions::default(), |r| {
        assert!(
            r.runs()
                .work_inherited_requirements(&RootId(id("task-a")))?
                .is_empty()
        );
        Ok(())
    })
    .unwrap();
    for (references, valid) in [
        (json!(["E1", "E2"]), true),
        (json!(["E1", "E1"]), false),
        (json!(["E1", 2]), false),
        (
            json!((0..129).map(|n| format!("E{n}")).collect::<Vec<_>>()),
            false,
        ),
    ] {
        run_write(&s, WriteOptions::default(), move |tx| {
            let mut w = tx.work().work(&id("task-a"))?.unwrap();
            let expected = w.revision;
            w.revision += 1;
            w.brief["inherits"] = references;
            tx.work().update_work(&w, expected)
        })
        .unwrap();
        let result = read_snapshot(&s, &ReadOptions::default(), |r| {
            r.runs().work_inherited_requirements(&RootId(id("task-a")))
        });
        if valid {
            assert_eq!(result.unwrap(), vec![id("E1"), id("E2")]);
        } else {
            assert!(result.is_err());
        }
    }
}

#[test]
fn same_root_restoration_never_revives_an_unmarked_ended_assignment() {
    let h = Harness::new();
    let (s, mut moved) = fixture(&h, None);
    run_write(&s, WriteOptions::default(), |tx| {
        let mut task = tx.work().work(&id("task-a"))?.unwrap();
        task.parent_feature_id = Some(id("feature"));
        task.revision += 1;
        tx.work().update_work(&task, 2)
    })
    .unwrap();
    moved.root_id = RootId(id("feature"));
    moved.parent_id = Some(AssignmentId(id("coord-a")));
    moved.checkpoint = None;
    s.with_writer_connection(|c| {
        super::super::super::conn::db(c.execute(
            "UPDATE assignments SET checkpoint_json=NULL WHERE id='worker'",
            [],
        ))?;
        Ok(())
    })
    .unwrap();
    assert!(matches!(
        reparent(&s, moved, 1, 1),
        Err(StorageError::InvalidRecord(_))
    ));
    read_snapshot(&s, &ReadOptions::default(), |r| {
        assert_eq!(
            r.runs()
                .assignment(&AssignmentId(id("worker")))?
                .unwrap()
                .state,
            AssignmentState::Superseded
        );
        assert_eq!(
            r.sessions()
                .session(&id("session-worker"))?
                .unwrap()
                .context_epoch,
            0
        );
        Ok(())
    })
    .unwrap();
}
