//! Migration 16 and the SQLite browser session repository: lifecycle compare-and-set, the
//! fields that never change, settled finality, observation sequences that never reuse a value
//! and do not change the session revision, and bounded records. No browser.
use super::{Harness, through};
use crate::storage::sqlite::{MIGRATION_0016_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::context::PrincipalKind;
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id, OperationId};
use serde_json::json;

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}

/// A store with an actor and an operation the session can reference.
fn seeded(h: &Harness) -> SqliteStore {
    let s = h.store();
    run_write(&s, WriteOptions::default(), |tx| {
        tx.actors()
            .register(&ActorId(id("user-a")), PrincipalKind::Human, "User")?;
        for op in ["op-create", "op-action"] {
            tx.operations().record_intent(NewOperation {
                id: OperationId(id(op)),
                root_id: None,
                assignment_id: None,
                capability: "browser.create".into(),
                schema_version: 1,
                args_digest: "0".repeat(64),
                payload: json!({}),
            })?;
        }
        Ok(())
    })
    .unwrap();
    s
}

fn session(s: &SqliteStore) -> StoredBrowserSession {
    StoredBrowserSession {
        id: id("bs-1"),
        revision: 1,
        phase: BrowserPhase::Starting,
        boot_id: s.info().boot_id.unwrap(),
        owner: ActorId(id("user-a")),
        owner_kind: "Human".into(),
        agent_session_id: None,
        project_id: None,
        root_id: None,
        assignment_id: None,
        asset_id: "chrome".into(),
        process_id: None,
        create_operation_id: OperationId(id("op-create")),
        uncertain_operation_id: None,
        last_action_operation_id: None,
        targets: vec![],
        observation_sequence: 0,
        detail: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    }
}

fn write<T: Send + 'static>(
    s: &SqliteStore,
    f: impl FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
) -> Result<T, StorageError> {
    run_write(s, WriteOptions::default(), f)
}
fn get(s: &SqliteStore) -> StoredBrowserSession {
    read_snapshot(s, &ReadOptions::default(), |r| {
        r.browser().browser_session(&id("bs-1"))
    })
    .unwrap()
    .unwrap()
}

#[test]
fn migration_16_adds_browser_sessions_on_top_of_version_12() {
    let m = MIGRATIONS
        .iter()
        .find(|m| m.version == 16)
        .expect("migration 16");
    assert_eq!(
        m.checksum(),
        MIGRATION_0016_SHA256,
        "an applied migration was edited"
    );
    // The writer floor keeps migration 14's: applying 16 must not lower it.
    assert_eq!((m.minimum_reader, m.minimum_writer), (8, 14));
    assert!(!m.rebuilds_tables);
    let h = Harness::new();
    drop(SqliteStore::open_with(h.config(), &through(12)).unwrap());
    let s = SqliteStore::open_with(h.config(), &through(16)).unwrap();
    let report = s.compatibility().unwrap();
    assert_eq!(
        (
            report.store_schema_version,
            report.minimum_reader,
            report.minimum_writer
        ),
        (16, 8, 14)
    );
}

#[test]
fn sessions_change_by_compare_and_set_and_settle_finally() {
    let h = Harness::new();
    let s = seeded(&h);
    let new = session(&s);
    write(&s, move |tx| tx.browser().insert_browser_session(&new)).unwrap();
    let dup = session(&s);
    assert!(matches!(
        write(&s, move |tx| tx.browser().insert_browser_session(&dup)),
        Err(StorageError::Duplicate { .. })
    ));
    // Ready needs a launched tree (schema check).
    let mut ready = get(&s);
    ready.phase = BrowserPhase::Ready;
    ready.revision = 2;
    assert!(write(&s, move |tx| tx.browser().update_browser_session(&ready, 1)).is_err());
    // A stale expected revision is a lost race, never a silent overwrite.
    let mut next = get(&s);
    next.targets = vec!["t1".into()];
    next.revision = 2;
    let stale = next.clone();
    write(&s, move |tx| tx.browser().update_browser_session(&next, 1)).unwrap();
    assert!(matches!(
        write(&s, move |tx| tx.browser().update_browser_session(&stale, 1)),
        Err(StorageError::LeaseLost { .. })
    ));
    // Identity, owner, scope and asset never change.
    let mut moved = get(&s);
    moved.asset_id = "edge".into();
    moved.revision = 3;
    assert!(matches!(
        write(&s, move |tx| tx.browser().update_browser_session(&moved, 2)),
        Err(StorageError::InvalidRecord(_))
    ));
    // Bounded records.
    let mut long = get(&s);
    long.detail = Some("x".repeat(2000));
    long.revision = 3;
    assert!(write(&s, move |tx| tx.browser().update_browser_session(&long, 2)).is_err());
    let mut targets = get(&s);
    targets.targets = (0..200).map(|i| format!("t{i}")).collect();
    targets.revision = 3;
    assert!(
        write(&s, move |tx| tx
            .browser()
            .update_browser_session(&targets, 2))
        .is_err()
    );
    // Settled is final.
    let mut closed = get(&s);
    closed.phase = BrowserPhase::Closed;
    closed.revision = 3;
    write(&s, move |tx| {
        tx.browser().update_browser_session(&closed, 2)
    })
    .unwrap();
    let mut reopened = get(&s);
    reopened.phase = BrowserPhase::Starting;
    reopened.revision = 4;
    assert!(matches!(
        write(&s, move |tx| tx
            .browser()
            .update_browser_session(&reopened, 3)),
        Err(StorageError::InvalidRecord(_))
    ));
    let listed = read_snapshot(&s, &ReadOptions::default(), |r| {
        Ok((
            r.browser()
                .browser_sessions(&BrowserFilter::Scope(None), None, 10)?,
            r.browser()
                .browser_sessions(&BrowserFilter::Unsettled, None, 10)?,
        ))
    })
    .unwrap();
    assert_eq!(listed.0.len(), 1);
    assert!(listed.1.is_empty(), "a closed session is settled");
}

#[test]
fn observation_sequences_are_allocated_by_the_session_without_changing_its_revision() {
    let h = Harness::new();
    let s = seeded(&h);
    let new = session(&s);
    write(&s, move |tx| tx.browser().insert_browser_session(&new)).unwrap();
    // An observation needs an allocated sequence and an existing artifact version.
    let orphan = StoredBrowserObservation {
        session_id: id("bs-1"),
        sequence: 1,
        channel: BrowserChannel::Console,
        artifact_id: id("a-1"),
        version_id: id("v-1"),
        truncated: false,
        recorded_at_ms: 1,
    };
    let o = orphan.clone();
    assert!(write(&s, move |tx| tx.browser().insert_browser_observation(&o)).is_err());
    let (a, b) = write(&s, |tx| {
        Ok((
            tx.browser().next_observation_sequence(&id("bs-1"))?,
            tx.browser().next_observation_sequence(&id("bs-1"))?,
        ))
    })
    .unwrap();
    assert_eq!((a, b), (1, 2));
    assert_eq!(
        get(&s).revision,
        1,
        "allocation is not a change of the session"
    );
    write(&s, |tx| {
        tx.browser()
            .set_last_action(&id("bs-1"), &OperationId(id("op-action")))
    })
    .unwrap();
    let got = get(&s);
    assert_eq!(got.revision, 1);
    assert_eq!(
        got.last_action_operation_id,
        Some(OperationId(id("op-action")))
    );
    // A later lifecycle update keeps the allocated sequence and the last action.
    let mut next = got.clone();
    next.observation_sequence = 0;
    next.revision = 2;
    assert!(
        matches!(
            write(&s, move |tx| tx.browser().update_browser_session(&next, 1)),
            Err(StorageError::InvalidRecord(_))
        ),
        "the sequence never decreases"
    );
    let mut next = got;
    next.targets = vec!["t1".into()];
    next.revision = 2;
    write(&s, move |tx| tx.browser().update_browser_session(&next, 1)).unwrap();
    let got = get(&s);
    assert_eq!(
        (
            got.observation_sequence,
            got.last_action_operation_id.is_some()
        ),
        (2, true)
    );
    assert!(matches!(
        write(&s, |tx| tx
            .browser()
            .next_observation_sequence(&id("missing"))),
        Err(StorageError::NotFound { .. })
    ));
}
