//! The actual import owner retains one journal while a same-epoch execution control waits.
use super::*;
use devworks_core::{
    protocol::{ProjectId, RootId},
    services::conventions::importing::ImportOutcome,
};
#[test]
fn temporary_control_wait_is_unknown_without_retry_exhaustion_then_same_import_completes() {
    let mut f = Fx::new();
    common::seed_root(&f.h, "waiting-root");
    f.ctx = f.ctx.clone().with_lineage(
        Some(ProjectId(id("project-a"))),
        Some(RootId(id("waiting-root"))),
        None,
    );
    let source = f.source(&serde_json::to_vec(&manifest(0)).unwrap());
    let op = f.admit(&source, "held-import");
    let key = op.clone();
    let operation = write(&*f.store, move |tx| {
        let row = tx.operations().operation(&key)?.unwrap();
        let attempt =
            tx.operations()
                .begin_attempt(&key, row.revision, AttemptBinding::default())?;
        let row = tx.operations().operation(&key)?.unwrap();
        tx.operations().transition(
            &key,
            row.revision,
            Some(&attempt.fence()),
            OperationState::Dispatching,
            None,
        )
    })
    .unwrap();
    write(&*f.store, |tx| {
        let mut paused = StoredRootControl::initial(RootId(id("waiting-root")));
        paused.desired = DesiredExecution::Paused;
        tx.runs().insert_root_control(&paused)
    })
    .unwrap();
    for _ in 0..8 {
        assert!(
            matches!(f.service.execute(&operation), ImportOutcome::Unknown(_)),
            "a temporary hold must not consume finite NotApplied retry attempts"
        );
    }
    let actual = read(&*f.store, |r| r.operations().operation(&op)).unwrap();
    assert_eq!(actual.attempts.len(), 1);
    assert_eq!(
        actual.attempts[0]
            .observations
            .iter()
            .filter(|v| v.get("conventionImportWaitingControls").is_some())
            .count(),
        1
    );
    let draft = read(&*f.store, |r| r.conventions().convention_import(&op)).unwrap();
    assert_eq!(draft.phase, "Queued");
    assert!(
        read(&*f.store, |r| r.conventions().convention(
            &draft.convention_id,
            None,
            &[KnowledgeScope::Project(ProjectId(id("project-a")))]
        ))
        .is_none()
    );
    assert_eq!(
        read(&*f.store, |r| r
            .runs()
            .root_control(&RootId(id("waiting-root"))))
        .unwrap()
        .desired,
        DesiredExecution::Paused
    );
    let key = op.clone();
    let unknown = write(&*f.store, move |tx| {
        let row = tx.operations().operation(&key)?.unwrap();
        tx.operations().transition(
            &key,
            row.revision,
            row.latest_attempt().map(|a| a.fence()).as_ref(),
            OperationState::OutcomeUnknown,
            None,
        )
    })
    .unwrap();
    write(&*f.store, |tx| {
        let mut control = tx
            .runs()
            .root_control(&RootId(id("waiting-root")))?
            .unwrap();
        let revision = control.revision;
        control.desired = DesiredExecution::Enabled;
        control.revision += 1;
        tx.runs().update_root_control(&control, revision)
    })
    .unwrap();
    assert!(matches!(
        f.service.observe(&unknown),
        ImportOutcome::Completed(_)
    ));
    assert!(matches!(
        f.service.observe(&unknown),
        ImportOutcome::Completed(_)
    ));
    let result = read(&*f.store, |r| r.conventions().convention_import(&op)).unwrap();
    assert_eq!(result.phase, "Published");
    assert!(
        read(&*f.store, |r| r.conventions().convention(
            &result.convention_id,
            None,
            &[KnowledgeScope::Project(ProjectId(id("project-a")))]
        ))
        .is_some()
    );
    assert_eq!(
        read(&*f.store, |r| r.operations().operation(&op))
            .unwrap()
            .attempts
            .len(),
        1
    );
}
