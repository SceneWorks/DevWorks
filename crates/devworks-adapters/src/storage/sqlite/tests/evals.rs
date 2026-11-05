//! Eval storage invariants and migration compatibility; no inference or model execution.
use super::{Harness, through};
use crate::storage::sqlite::{MIGRATION_0024_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{Id, ProjectId};
use serde_json::json;

fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}
fn seed(store: &SqliteStore) {
    let actor = store.info().host_actor;
    run_write(store, WriteOptions::default(), move |tx| {
        tx.projects().insert_project(&StoredProject {
            id: ProjectId(id("project-a")),
            name: "Project".into(),
            description: String::new(),
            lifecycle: "Active".into(),
            revision: 1,
            scope_revision: 1,
            created_at_ms: 1,
            updated_at_ms: 1,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 0,
            archive_stop_pending: false,
        })?;
        for name in ["source-session", "other-session", "project-session"] {
            tx.sessions().insert_session(&StoredSession {
                id: id(name),
                conversation_id: id(&format!("conversation-{name}")),
                agent_id: actor.clone(),
                project_id: (name == "project-session").then(|| ProjectId(id("project-a"))),
                root_id: None,
                title: "Source".into(),
                context_epoch: 0,
                revision: 1,
                created_by: actor.clone(),
                created_at_ms: 1,
                closed_at_ms: None,
                closed_by: None,
                next_sequence: 1,
                checkpoint_sequence: 0,
            })?;
        }
        Ok(())
    })
    .unwrap();
}
fn report(name: &str) -> StoredEvalRun {
    StoredEvalRun {
        id: id(name),
        suite_id: id("suite"),
        suite_revision: 2,
        project_id: None,
        session_id: id("source-session"),
        session_revision: 1,
        measurement_digest: "a".repeat(64),
        created_at_ms: 2,
        source_watermark: 0,
        report: json!({"cases":[{"source":{"sessionId":"source-session","entrySequence":1},"grade":"pass"}], "measurementVersion":2}),
    }
}
fn insert(store: &SqliteStore, run: StoredEvalRun) -> Result<(), StorageError> {
    run_write(store, WriteOptions::default(), move |tx| {
        tx.evals().insert_eval_run(&run)
    })
}
fn get(store: &SqliteStore, name: &str) -> Option<StoredEvalRun> {
    read_snapshot(store, &ReadOptions::default(), |snapshot| {
        snapshot.evals().eval_run(&id(name))
    })
    .unwrap()
}

#[test]
fn eval_migration_is_additive_checksummed_and_preserves_compatibility_floors() {
    let migration = MIGRATIONS
        .iter()
        .find(|migration| migration.version == 24)
        .unwrap();
    assert_eq!(migration.checksum(), MIGRATION_0024_SHA256);
    assert_eq!(
        (migration.minimum_reader, migration.minimum_writer),
        (8, 23)
    );
    assert!(!migration.rebuilds_tables);
    let harness = Harness::new();
    let old = SqliteStore::open_with(harness.config(), &through(23)).unwrap();
    seed(&old);
    drop(old);
    let store = harness.store();
    assert_eq!(store.compatibility().unwrap().store_schema_version, 24);
    insert(&store, report("run-a")).unwrap();
    drop(store);
    // The previous writer can coexist: it neither edits nor deletes eval reports.
    let old = SqliteStore::open_with(harness.config(), &through(23)).unwrap();
    assert_eq!(old.compatibility().unwrap().access, AccessMode::ReadWrite);
    drop(old);
    let reopened = harness.store();
    assert_eq!(get(&reopened, "run-a"), Some(report("run-a")));
    assert!(reopened.integrity_check().unwrap().problems.is_empty());
}

#[test]
fn eval_reports_preserve_source_identity_without_changing_sources() {
    let harness = Harness::new();
    let store = harness.store();
    seed(&store);
    let before = read_snapshot(&store, &ReadOptions::default(), |snapshot| {
        snapshot.sessions().session(&id("source-session"))
    })
    .unwrap();
    insert(&store, report("run-a")).unwrap();
    let after = read_snapshot(&store, &ReadOptions::default(), |snapshot| {
        snapshot.sessions().session(&id("source-session"))
    })
    .unwrap();
    assert_eq!(before, after);
    assert_eq!(get(&store, "run-a"), Some(report("run-a")));
    let mut duplicate = report("run-a");
    duplicate.report = json!({"rewritten":true});
    assert!(matches!(
        insert(&store, duplicate),
        Err(StorageError::Duplicate { .. })
    ));
    assert_eq!(get(&store, "run-a"), Some(report("run-a")));
    store
        .with_writer_connection(|connection| {
            assert!(
                connection
                    .execute("UPDATE eval_runs SET report_json='{}' WHERE id='run-a'", [])
                    .is_err()
            );
            assert!(
                connection
                    .execute("DELETE FROM eval_runs WHERE id='run-a'", [])
                    .is_err()
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn eval_records_reject_invalid_scope_source_revisions_and_bounds() {
    let harness = Harness::new();
    let store = harness.store();
    seed(&store);
    let mut missing = report("missing");
    missing.session_id = id("absent");
    assert!(matches!(
        insert(&store, missing),
        Err(StorageError::NotFound {
            kind: "session",
            ..
        })
    ));
    let mut scoped = report("scope");
    scoped.project_id = Some(ProjectId(id("another-project")));
    assert!(matches!(
        insert(&store, scoped),
        Err(StorageError::ScopeDenied)
    ));
    let mut stale = report("stale");
    stale.session_revision = 0;
    assert!(matches!(
        insert(&store, stale),
        Err(StorageError::InvalidRecord(_))
    ));
    for field in 0..6 {
        let mut invalid = report(&format!("invalid-{field}"));
        match field {
            0 => invalid.suite_revision = -1,
            1 => invalid.session_revision = -1,
            2 => invalid.source_watermark = -1,
            3 => invalid.created_at_ms = -1,
            4 => invalid.source_watermark = i64::MAX,
            _ => invalid.measurement_digest = "g".repeat(64),
        }
        assert!(matches!(
            insert(&store, invalid),
            Err(StorageError::InvalidRecord(_))
        ));
    }
    let mut oversized = report("oversized");
    oversized.report = json!("x".repeat(MAX_EVAL_REPORT_BYTES));
    assert!(matches!(
        insert(&store, oversized),
        Err(StorageError::InvalidRecord(_))
    ));
    assert!(get(&store, "scope").is_none());
}

#[test]
fn eval_pages_are_bounded_exclusive_and_session_scoped_in_id_order() {
    let harness = Harness::new();
    let store = harness.store();
    seed(&store);
    for name in ["run-c", "run-a", "run-b"] {
        insert(&store, report(name)).unwrap();
    }
    let mut other = report("run-ab");
    other.session_id = id("other-session");
    insert(&store, other).unwrap();
    read_snapshot(&store, &ReadOptions::default(), |snapshot| {
        let first = snapshot.evals().eval_runs(&id("source-session"), None, 2)?;
        assert_eq!(
            first.iter().map(|run| run.id.as_str()).collect::<Vec<_>>(),
            ["run-a", "run-b"]
        );
        let next = snapshot
            .evals()
            .eval_runs(&id("source-session"), Some(&first[1].id), 2)?;
        assert_eq!(
            next.iter().map(|run| run.id.as_str()).collect::<Vec<_>>(),
            ["run-c"]
        );
        for limit in [0, MAX_PAGE + 1] {
            assert!(matches!(
                snapshot
                    .evals()
                    .eval_runs(&id("source-session"), None, limit),
                Err(StorageError::InvalidRecord(_))
            ));
        }
        Ok(())
    })
    .unwrap();
}

#[test]
fn project_eval_reports_require_the_source_project() {
    let harness = Harness::new();
    let store = harness.store();
    seed(&store);
    let mut run = report("project-run");
    run.session_id = id("project-session");
    assert!(matches!(
        insert(&store, run.clone()),
        Err(StorageError::ScopeDenied)
    ));
    run.project_id = Some(ProjectId(id("project-a")));
    insert(&store, run.clone()).unwrap();
    assert_eq!(get(&store, "project-run"), Some(run));
}
