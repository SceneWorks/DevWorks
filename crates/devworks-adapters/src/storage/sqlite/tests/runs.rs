//! Migration 11 and the run root control records on the real SQLite store: a version-10 store
//! is upgraded in place (its rows untouched, the new columns defaulted, the new tables empty),
//! and the store-level invariants hold under it — one live activation per root, one live
//! coordinator per root, one live assignment per work item, one in-flight turn per session,
//! compare-and-set on every record and final ended records. No inference.
use super::{Harness, through};
use crate::storage::sqlite::{MIGRATION_0011_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, AssignmentId, Id, RootId};
use serde_json::json;

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn root(s: &str) -> RootId {
    RootId(id(s))
}
fn write<T: Send + 'static>(
    s: &SqliteStore,
    f: impl FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
) -> Result<T, StorageError> {
    run_write(s, WriteOptions::default(), f)
}
fn activation(s: &SqliteStore, a: &str, r: &str, generation: i64) -> StoredActivation {
    StoredActivation {
        id: id(a),
        root_id: root(r),
        boot_id: s.info().boot_id.unwrap(),
        generation,
        phase: ActivationPhase::Pending,
        purpose: ActivationPurpose::Planning,
        owner: None,
        lease_until_ms: None,
        owner_epoch: 0,
        revision: 1,
        attempts: 0,
        ready_at_ms: 0,
        created_at_ms: 0,
        stop_cause: None,
        stop_requested_at_ms: None,
        stop_acknowledged_at_ms: None,
        stopped_at_ms: None,
        outcome: None,
        detail: None,
        plan_revision: None,
    }
}
fn assignment(a: &str, work: &str, parent: Option<&str>, session: &str) -> StoredAssignment {
    StoredAssignment {
        id: AssignmentId(id(a)),
        root_id: root("root-a"),
        work_id: root(work),
        parent_id: parent.map(|p| AssignmentId(id(p))),
        session_id: id(session),
        execution_id: None,
        state: AssignmentState::Ready,
        generation: 0,
        slot_held: false,
        revision: 1,
        checkpoint: None,
        scope_key: None,
        scope_digest: None,
        plan_revision: None,
        instruction_revision: 0,
        created_at_ms: 0,
    }
}

#[test]
fn migration_11_adds_run_controls_to_a_version_10_store() {
    let m = MIGRATIONS
        .iter()
        .find(|m| m.version == 11)
        .expect("migration 11");
    assert_eq!(
        m.checksum(),
        MIGRATION_0011_SHA256,
        "an applied migration was edited"
    );
    assert_eq!((m.minimum_reader, m.minimum_writer), (8, 11));
    assert!(
        MIGRATIONS
            .iter()
            .filter(|e| e.version < 11)
            .all(|e| e.minimum_reader <= m.minimum_reader && e.minimum_writer <= m.minimum_writer)
    );
    assert!(!m.rebuilds_tables);
    let h = Harness::new();
    drop(SqliteStore::open_with(h.config(), &through(10)).unwrap());
    // Version-10 rows of the tables migration 11 extends (never written by a build, but the
    // upgrade must keep whatever is there).
    let c = h.raw();
    c.execute_batch(
        "INSERT INTO actors VALUES('agent-a','agent','Agent');
         INSERT INTO projects VALUES('project-a','A','Active',1,1,1);
         INSERT INTO work_items VALUES('root-a','project-a','feature',NULL,NULL,NULL,'A',1,0,1,1,1,'{}');
         INSERT INTO work_items VALUES('task-a','project-a','task','root-a',NULL,NULL,'T',1,0,1,1,1,'{}');
         INSERT INTO work_items VALUES('task-b','project-a','task','root-a',NULL,NULL,'U',1,0,1,1,1,'{}');
         INSERT INTO root_controls(root_id,desired_state,revision,current_generation) VALUES('root-a','Paused',3,2);
         INSERT INTO blockers(id,root_id,cause,origin_id,origin_revision,state,revision,clearance_json) VALUES('hold-a','root-a','HumanPause','root-a',3,'Active',1,'{}');
         INSERT INTO conversations VALUES('conv-a','root-a',0);
         INSERT INTO sessions(id,conversation_id,agent_id,root_id,context_epoch,revision) VALUES('s-a','conv-a','agent-a','root-a',0,1);
         INSERT INTO sessions(id,conversation_id,agent_id,root_id,context_epoch,revision) VALUES('s-b','conv-a','agent-a','root-a',0,1);",
    )
    .unwrap();
    drop(c);
    let s = SqliteStore::open_with(h.config(), &through(11)).unwrap();
    let report = s.compatibility().unwrap();
    assert_eq!(
        (
            report.store_schema_version,
            report.minimum_reader,
            report.minimum_writer
        ),
        (11, 8, 11),
        "pre-gate builds may read this store but never write it"
    );
    let control = read_snapshot(&s, &ReadOptions::default(), |r| {
        r.runs().root_control(&root("root-a"))
    })
    .unwrap()
    .unwrap();
    assert_eq!(
        (control.desired, control.revision, control.generation),
        (DesiredExecution::Paused, 3, 2)
    );
    assert_eq!((control.instruction_revision, control.stop_epoch), (0, 0));
    assert!(!control.reactivate && control.stop_operation_id.is_none());
    let hold = read_snapshot(&s, &ReadOptions::default(), |r| {
        r.runs().blocker(&id("hold-a"))
    })
    .unwrap()
    .unwrap();
    assert_eq!(
        (hold.cause, hold.reason.as_str()),
        (HoldCause::HumanPause, "")
    );

    // One live activation per root; a stopped one is final.
    let a1 = activation(&s, "act-1", "root-a", 3);
    let a = a1.clone();
    write(&s, move |tx| tx.runs().insert_activation(&a)).unwrap();
    let a = activation(&s, "act-2", "root-a", 4);
    assert!(matches!(
        write(&s, move |tx| tx.runs().insert_activation(&a)),
        Err(StorageError::Duplicate { .. })
    ));
    let mut stale = a1.clone();
    stale.revision = 2;
    stale.phase = ActivationPhase::Claimed;
    stale.owner = Some(id("sup"));
    stale.lease_until_ms = Some(10);
    let x = stale.clone();
    assert!(matches!(
        write(&s, move |tx| tx.runs().update_activation(&x, 7)),
        Err(StorageError::RevisionConflict(_))
    ));
    let mut stopped = a1.clone();
    stopped.revision = 2;
    stopped.phase = ActivationPhase::Stopped;
    stopped.outcome = Some(ActivationOutcome::Prevented);
    let x = stopped.clone();
    write(&s, move |tx| tx.runs().update_activation(&x, 1)).unwrap();
    let mut again = stopped.clone();
    again.revision = 3;
    assert!(matches!(
        write(&s, move |tx| tx.runs().update_activation(&again, 2)),
        Err(StorageError::InvalidRecord(_))
    ));
    let a = activation(&s, "act-2", "root-a", 4);
    write(&s, move |tx| tx.runs().insert_activation(&a)).unwrap();

    // One live coordinator per root; one live assignment per work item.
    let coord = assignment("asg-1", "root-a", None, "s-a");
    let x = coord.clone();
    write(&s, move |tx| tx.runs().insert_assignment(&x)).unwrap();
    let x = assignment("asg-2", "task-a", None, "s-b");
    assert!(matches!(
        write(&s, move |tx| tx.runs().insert_assignment(&x)),
        Err(StorageError::Duplicate { .. })
    ));
    let x = assignment("asg-3", "task-a", Some("asg-1"), "s-b");
    write(&s, move |tx| tx.runs().insert_assignment(&x)).unwrap();
    let x = assignment("asg-4", "task-a", Some("asg-1"), "s-b");
    assert!(matches!(
        write(&s, move |tx| tx.runs().insert_assignment(&x)),
        Err(StorageError::Duplicate { .. })
    ));

    // A blocker's active origin is unique; a cleared one is final.
    let b = StoredBlocker {
        id: id("hold-b"),
        root_id: Some(root("root-a")),
        assignment_id: None,
        operation_id: None,
        cause: HoldCause::HumanPause,
        origin_id: "root-a".into(),
        origin_revision: 3,
        state: BlockerState::Active,
        revision: 1,
        reason: "dup".into(),
        clearance: json!({}),
        created_at_ms: 0,
        cleared_at_ms: None,
        cleared_by: None,
    };
    let x = b.clone();
    assert!(matches!(
        write(&s, move |tx| tx.runs().insert_blocker(&x)),
        Err(StorageError::Duplicate { .. })
    ));
    let mut cleared = hold.clone();
    cleared.state = BlockerState::Cleared;
    cleared.revision = 2;
    cleared.cleared_by = Some(ActorId(id("agent-a")));
    let x = cleared.clone();
    write(&s, move |tx| tx.runs().update_blocker(&x, 1)).unwrap();
    let mut reopen = cleared.clone();
    reopen.state = BlockerState::Active;
    reopen.revision = 3;
    assert!(matches!(
        write(&s, move |tx| tx.runs().update_blocker(&reopen, 2)),
        Err(StorageError::InvalidRecord(_))
    ));

    // One turn in flight per session; ending another turn is refused.
    let turn = StoredTurn {
        session_id: id("s-a"),
        turn_id: id("turn-1"),
        assignment_id: AssignmentId(id("asg-1")),
        execution_id: id("act-2"),
        owner_epoch: 1,
        boot_id: s.info().boot_id.unwrap(),
        started_at_ms: 0,
    };
    let x = turn.clone();
    write(&s, move |tx| tx.runs().insert_turn(&x)).unwrap();
    let mut second = turn.clone();
    second.turn_id = id("turn-2");
    assert!(matches!(
        write(&s, move |tx| tx.runs().insert_turn(&second)),
        Err(StorageError::Duplicate { .. })
    ));
    assert!(matches!(
        write(&s, |tx| tx.runs().delete_turn(&id("s-a"), &id("turn-2"))),
        Err(StorageError::LeaseLost { .. })
    ));
    write(&s, |tx| tx.runs().delete_turn(&id("s-a"), &id("turn-1"))).unwrap();

    // Redirect stages only advance.
    let i = StoredInstruction {
        id: id("ins-1"),
        root_id: root("root-a"),
        seq: 1,
        actor: ActorId(id("agent-a")),
        message: "change course".into(),
        attachments: json!([]),
        control_revision: 4,
        instruction_revision: 1,
        stage: InstructionStage::Acknowledged,
        received_at_ms: 0,
        acknowledged_at_ms: Some(1),
        acknowledged_by: Some(id("act-2")),
        incorporated_at_ms: None,
        disposition: None,
        session_id: Some(id("s-a")),
        transcript_entry_id: None,
        revision: 1,
    };
    let x = i.clone();
    write(&s, move |tx| tx.runs().insert_instruction(&x)).unwrap();
    let mut back = i.clone();
    back.stage = InstructionStage::Received;
    back.acknowledged_at_ms = None;
    back.revision = 2;
    assert!(write(&s, move |tx| tx.runs().update_instruction(&back, 1)).is_err());
    // The plan slot and project reads run control uses.
    let roots = read_snapshot(&s, &ReadOptions::default(), |r| {
        r.runs().project_roots(
            &devworks_core::protocol::ProjectId(id("project-a")),
            None,
            10,
        )
    })
    .unwrap();
    assert_eq!(roots, vec![root("root-a")]);
}
