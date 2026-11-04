//! Real-adapter tests: the shared contract suite plus SQLite-specific durability, schema,
//! migration, exclusivity, scheduling and fault cases. Temporary data roots, no inference.
#![allow(clippy::result_large_err)]
use super::writer::TxGuard;
use super::*;
use devworks_core::context::InvocationContext;
use devworks_core::gateway::{AdmittedCommand, Gateway};
use devworks_core::ports::storage::contract::StoreHarness;
use devworks_core::ports::storage::*;
use devworks_core::ports::{
    AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization, CommandHandler, Decided,
    GrantUse, HandlerFailure,
};
use devworks_core::protocol::results::DomainDecision;
use devworks_core::protocol::{
    CommandReply, Counter, EventId, OperationId, Rejection, Revision, RootId,
};
use serde_json::json;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::task::{Context, Poll, Wake, Waker};

const T0: i64 = 1_700_000_000_000;

struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn now_ms(&self) -> Counter {
        Counter::new(self.0.load(Ordering::SeqCst)).unwrap()
    }
}
struct TestIds(AtomicU64);
impl IdSource for TestIds {
    fn next_id(&self, kind: IdKind) -> Id {
        let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        Id::new(format!("{kind:?}-{n}").to_lowercase()).unwrap()
    }
}

/// A uniquely named temporary data root, removed on drop.
struct TempRoot(PathBuf);
impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "devworks-store-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }
}
impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Harness {
    root: TempRoot,
    clock: Arc<TestClock>,
    ids: Arc<TestIds>,
    tune: fn(&mut SqliteConfig),
}
impl Harness {
    fn new() -> Self {
        Self::tuned(|_| {})
    }
    fn tuned(tune: fn(&mut SqliteConfig)) -> Self {
        Self {
            root: TempRoot::new(),
            clock: Arc::new(TestClock(AtomicI64::new(T0))),
            ids: Arc::new(TestIds(AtomicU64::new(0))),
            tune,
        }
    }
    fn config(&self) -> SqliteConfig {
        let mut c = SqliteConfig::new(
            self.root.0.clone(),
            self.clock.clone(),
            self.ids.clone(),
            ActorId(Id::new("host").unwrap()),
        );
        (self.tune)(&mut c);
        c
    }
    fn store(&self) -> SqliteStore {
        SqliteStore::open(self.config()).expect("open store")
    }
    fn raw(&self) -> Connection {
        let c = Connection::open(self.root.0.join(DATABASE_FILE)).unwrap();
        c.pragma_update(None, "foreign_keys", true).unwrap();
        c
    }
}
impl StoreHarness for Harness {
    fn open(&mut self) -> Arc<dyn ApplicationStore> {
        Arc::new(self.store())
    }
    fn set_now(&self, ms: i64) {
        self.clock.0.store(ms, Ordering::SeqCst);
    }
    fn now(&self) -> i64 {
        self.clock.0.load(Ordering::SeqCst)
    }
}

mod artifacts;
mod authority;
mod diagnostics;
mod packages;
mod projects;
mod replies;
mod resources;
mod router;
mod runs;
mod sessions;
mod settings;

mod contract {
    devworks_core::store_contract_tests!(super::Harness::new());
}

fn write<T: Send + 'static>(
    s: &dyn ApplicationStore,
    f: impl FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
) -> Result<T, StorageError> {
    run_write(s, WriteOptions::default(), f)
}
fn append(
    tx: &mut dyn WriteTransaction,
    kind: &str,
    payload: serde_json::Value,
) -> Result<i64, StorageError> {
    let id = EventId(tx.new_id(IdKind::Event));
    let actor = tx.store().host_actor.clone();
    tx.history().append(NewEvent {
        id,
        actor,
        project_id: None,
        root_id: None,
        operation_id: None,
        aggregate_kind: "test".into(),
        aggregate_id: "t".into(),
        aggregate_revision: 1,
        kind: kind.into(),
        payload,
    })
}
fn sequence(s: &dyn ApplicationStore) -> i64 {
    read_snapshot(s, &ReadOptions::default(), |r| {
        Ok(r.stamp().committed_sequence)
    })
    .unwrap()
}

// --- durability settings and runtime identity ---------------------------------------------------

#[test]
fn effective_durability_settings_are_verified() {
    let h = Harness::new();
    let s = h.store();
    let w = s.writer_settings().unwrap();
    assert_eq!(w.journal_mode.to_ascii_lowercase(), "wal");
    assert!(w.foreign_keys);
    assert_eq!(w.synchronous, 2, "synchronous=FULL on the writer");
    assert!(!w.query_only);
    if cfg!(target_os = "macos") {
        assert!(w.fullfsync, "macOS full-fsync requested and honoured");
    }
    let r = s.reader_settings().unwrap();
    assert!(r.foreign_keys && r.query_only);
    // A reader cannot write even if asked to.
    let lease = s.lease(None).unwrap();
    let conn = lease.conn.as_ref().unwrap();
    assert!(conn.execute("DELETE FROM actors", []).is_err());
}

#[test]
fn runtime_identity_meets_the_baseline() {
    let h = Harness::new();
    let s = h.store();
    let id = s.runtime_identity();
    let v: Vec<i32> = id
        .engine_version
        .split('.')
        .map(|p| p.parse().unwrap())
        .collect();
    assert!(
        v[0] * 1_000_000 + v[1] * 1_000 + v[2] >= MIN_SQLITE_VERSION,
        "{}",
        id.engine_version
    );
    assert!(rusqlite::version_number() >= MIN_SQLITE_VERSION);
    assert!(id.compile_options.iter().any(|o| o == "ENABLE_FTS5"));
    assert_eq!(
        id.extensions,
        [("sqlite-vec".to_owned(), EXPECTED_VEC_VERSION.to_owned())]
    );
    // The bundled vector functions and FTS5 work on the store's own connections.
    let d: f64 = s
        .with_writer_connection(|c| {
            db(c.query_row(
                "SELECT vec_distance_L2(vec_f32('[0,0]'), vec_f32('[3,4]'))",
                [],
                |r| r.get(0),
            ))
        })
        .unwrap();
    assert!((d - 5.0).abs() < 1e-9);
    let fts: i64 = s
        .with_writer_connection(|c| {
            db(c.query_row(
                "SELECT count(*) FROM search_fts WHERE search_fts MATCH 'x'",
                [],
                |r| r.get(0),
            ))
        })
        .unwrap();
    assert_eq!(fts, 0);
}

// --- data root, boots, schema ---------------------------------------------------------------------

#[test]
fn one_write_owner_per_data_root() {
    let h = Harness::new();
    let s = h.store();
    match SqliteStore::open(h.config()) {
        Err(StorageError::StorageUnavailable {
            cause: Unavailable::DataRootInUse,
            ..
        }) => {}
        other => panic!("second owner admitted: {:?}", other.map(|_| ())),
    }
    drop(s);
    assert!(SqliteStore::open(h.config()).is_ok());
}

#[test]
fn boot_epochs_record_clean_and_interrupted_shutdowns() {
    let h = Harness::new();
    let s = h.store();
    assert_eq!(s.previous_shutdown(), None);
    let first = s.info().boot_id.unwrap();
    drop(s);
    let s = h.store();
    assert_eq!(s.previous_shutdown(), Some("Clean"));
    let second = s.info().boot_id.unwrap();
    assert_ne!(first, second);
    drop(s);
    // Simulate a crash of the second boot (no clean stop recorded).
    h.raw()
        .execute(
            "UPDATE app_boots SET stopped_at=NULL,shutdown_state='Open' WHERE id=?1",
            [second.as_str()],
        )
        .unwrap();
    let s = h.store();
    assert_eq!(s.previous_shutdown(), Some("Interrupted"));
    drop(s);
    let state: String = h
        .raw()
        .query_row(
            "SELECT shutdown_state FROM app_boots WHERE id=?1",
            [second.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "Interrupted");
}

fn schema(conn: &Connection) -> Vec<(String, String, String, Option<String>)> {
    let mut s = conn
        .prepare("SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name")
        .unwrap();
    s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn migration_one_is_exactly_the_adopted_schema_fixture() {
    let fixture = Connection::open_in_memory().unwrap();
    fixture
        .execute_batch(include_str!(
            "../../../../../docs/architecture/persistence-schema.sql"
        ))
        .unwrap();
    let h = Harness::new();
    let s = SqliteStore::open_with(h.config(), &MIGRATIONS[..1]).unwrap();
    drop(s);
    assert_eq!(schema(&h.raw()), schema(&fixture));
    assert_eq!(
        MIGRATIONS[0].checksum(),
        MIGRATION_0001_SHA256,
        "an applied migration was edited"
    );
    assert_eq!(
        MIGRATIONS[1].checksum(),
        MIGRATION_0002_SHA256,
        "an applied migration was edited"
    );
    // Migration 2 upgrades a version-1 store in place: claims gain their boot binding.
    let s = SqliteStore::open_with(h.config(), &through(2)).unwrap();
    let report = s.compatibility().unwrap();
    assert_eq!(
        (
            report.store_schema_version,
            report.minimum_writer,
            report.migrations.len()
        ),
        (2, 2, 2)
    );
    drop(s);
    let has_column: bool = h
        .raw()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('queue_entries') WHERE name='claim_boot_id')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(has_column);
}

/// A version-1 store with pending deliveries upgrades in place through migrations 2 and 3: their
/// event sequences are backfilled, they stay unordered and claimable, and older writers are then
/// refused write access.
#[test]
fn migration_three_upgrades_existing_deliveries() {
    let delivery = MIGRATIONS
        .iter()
        .find(|m| m.version == 3)
        .expect("durable delivery migration");
    assert_eq!(
        delivery.checksum(),
        MIGRATION_0003_SHA256,
        "an applied migration was edited"
    );
    let h = Harness::new();
    let s = SqliteStore::open_with(h.config(), &through(1)).unwrap();
    drop(s);
    // Seed a v1 delivery row directly (v1 has no partition/sequence columns).
    let c = h.raw();
    c.execute_batch(
        "INSERT INTO events(id,actor_id,aggregate_kind,aggregate_id,aggregate_revision,kind,created_at,payload_json) VALUES('ev-old','host','fixture','a',1,'Old',0,'{}');
         INSERT INTO events(id,actor_id,aggregate_kind,aggregate_id,aggregate_revision,kind,created_at,payload_json) VALUES('ev-v1','host','fixture','a',1,'Changed',0,'{}');
         INSERT INTO queue_entries(id,consumer,event_id,priority,ready_at,state,payload_json) VALUES('q-v1','work','ev-v1',0,0,'Ready','{}');
         INSERT INTO queue_entries(id,consumer,event_id,priority,ready_at,state,payload_json) VALUES('q-failed','work','ev-old',0,7,'Failed','{}');",
    )
    .unwrap();
    drop(c);
    let s = h.store();
    let e = read_snapshot(&s, &ReadOptions::default(), |r| {
        r.queue().entry(&Id::new("q-v1").unwrap())
    })
    .unwrap()
    .unwrap();
    assert_eq!(
        (
            e.event_sequence,
            e.revision,
            e.partition.clone(),
            e.last_error.clone()
        ),
        (2, 1, None, None)
    );
    assert_eq!(
        (e.cancel_requested, e.awaiting_eligibility, e.failed_at_ms),
        (false, false, None)
    );
    // A failed delivery is backfilled with a failure time so it keeps its inspection position.
    let failed = read_snapshot(&s, &ReadOptions::default(), |r| {
        r.queue().entry(&Id::new("q-failed").unwrap())
    })
    .unwrap()
    .unwrap();
    assert_eq!(
        (failed.state, failed.failed_at_ms),
        (QueueState::Failed, Some(7))
    );
    let claimed = write(&s, |tx| {
        tx.queue().claim_ready(&ClaimRequest {
            consumer: "work".into(),
            owner: Id::new("w").unwrap(),
            lease_ms: 1_000,
            limit: 10,
        })
    })
    .unwrap();
    assert_eq!(claimed.len(), 1);
    let report = s.compatibility().unwrap();
    assert!(report.store_schema_version >= 3 && report.minimum_writer >= 3);
    drop(s);
    // A build older than the store's reader floor may not even read it: from migration 5 an
    // older reader would misreport the `mcp_client` actor kind as corruption. (A build between
    // the reader and writer floors reads but does not write; see the floor test.)
    match SqliteStore::open_with(h.config(), &through(2)) {
        Err(StorageError::IncompatibleSchema(_)) => {}
        Ok(old) => panic!("an older build opened the store: {:?}", old.info().access),
        Err(other) => panic!("{other}"),
    }
}

#[test]
fn every_shipped_table_is_strict_and_its_foreign_keys_resolve() {
    let h = Harness::new();
    drop(h.store());
    let c = h.raw();
    let tables: Vec<(String, bool)> = c
        .prepare("SELECT name,strict FROM pragma_table_list WHERE schema='main' AND type='table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'search_fts_%' ORDER BY name")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? == 1)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(tables.len() > 70, "{}", tables.len());
    let mut foreign_keys = 0;
    for (table, strict) in &tables {
        assert!(*strict, "{table} is not STRICT");
        let fks: Vec<(i64, String, String, Option<String>)> = c
            .prepare(&format!("SELECT id,\"table\",\"from\",\"to\" FROM pragma_foreign_key_list('{table}') ORDER BY id,seq"))
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let mut by_id: std::collections::BTreeMap<i64, (String, Vec<Option<String>>)> =
            Default::default();
        for (fid, parent, _, to) in fks {
            let e = by_id.entry(fid).or_insert((parent, Vec::new()));
            e.1.push(to);
        }
        for (parent, to) in by_id.values() {
            foreign_keys += 1;
            // Parent key: named columns, or the parent's primary key when omitted.
            let pk: Vec<String> = c
                .prepare(&format!(
                    "SELECT name FROM pragma_table_info('{parent}') WHERE pk>0 ORDER BY pk"
                ))
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert!(
                !pk.is_empty() || to.iter().all(Option::is_some),
                "{table} -> missing parent {parent}"
            );
            let cols: Vec<String> = to
                .iter()
                .zip(pk.iter().map(Some).chain(std::iter::repeat(None)))
                .map(|(t, p)| t.clone().or_else(|| p.cloned()).expect("parent column"))
                .collect();
            let mut want = cols.clone();
            want.sort();
            // The parent key must be the primary key or exactly one unique index, otherwise
            // SQLite raises "foreign key mismatch" at the first write.
            let mut sorted_pk = pk.clone();
            sorted_pk.sort();
            let unique = sorted_pk == want
                || c.prepare(&format!(
                    "SELECT name FROM pragma_index_list('{parent}') WHERE \"unique\"=1"
                ))
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|i| i.unwrap())
                .any(|index| {
                    let mut ic: Vec<String> = c
                        .prepare(&format!("SELECT name FROM pragma_index_info('{index}')"))
                        .unwrap()
                        .query_map([], |r| r.get(0))
                        .unwrap()
                        .collect::<Result<_, _>>()
                        .unwrap();
                    ic.sort();
                    ic == want
                });
            assert!(
                unique,
                "{table} -> {parent}({cols:?}) is not a unique parent key"
            );
        }
    }
    assert!(foreign_keys > 80, "{foreign_keys}");
    let s = h.store();
    assert!(s.integrity_check().unwrap().ok());
    // Foreign keys are enforced on store connections: an orphan receipt is refused.
    let orphan = s.with_writer_connection(|c| {
        db(c.execute(
            "INSERT INTO command_receipts VALUES('nobody','s','k','x','d',NULL,'{}',1)",
            [],
        ))
    });
    assert!(
        matches!(orphan, Err(StorageError::InvalidRecord(_))),
        "{orphan:?}"
    );
}

/// This build's migrations up to and including `version` (versions may have gaps).
fn through(version: i64) -> Vec<Migration> {
    MIGRATIONS
        .iter()
        .copied()
        .filter(|m| m.version <= version)
        .collect()
}

#[test]
fn shipped_migrations_increase_and_settings_apply_on_top() {
    assert!(MIGRATIONS.windows(2).all(|w| w[0].version < w[1].version));
    let settings = MIGRATIONS
        .iter()
        .find(|m| m.version == 4)
        .expect("settings migration");
    assert_eq!(
        settings.checksum(),
        MIGRATION_0004_SHA256,
        "an applied migration was edited"
    );
    // Applying a migration overwrites the store's writer floor, so declared floors may never
    // decrease: a later migration would otherwise re-admit writers an earlier one refused.
    assert!(
        MIGRATIONS
            .windows(2)
            .all(|w| w[0].minimum_writer <= w[1].minimum_writer),
        "a shipped migration lowers the writer floor"
    );
    let highest_floor = MIGRATIONS.iter().map(|m| m.minimum_writer).max().unwrap();
    let h = Harness::new();
    drop(SqliteStore::open_with(h.config(), &through(2)).unwrap());
    // Upgrade one shipped migration at a time: the store floor tracks the highest floor declared
    // so far and never decreases.
    let mut floor = 0;
    for m in MIGRATIONS.iter().filter(|m| m.version > 2) {
        let s = SqliteStore::open_with(h.config(), &through(m.version)).unwrap();
        let report = s.compatibility().unwrap();
        let declared = through(m.version)
            .iter()
            .map(|applied| applied.minimum_writer)
            .max()
            .unwrap();
        assert_eq!(
            (report.store_schema_version, report.minimum_writer),
            (m.version, declared),
            "after migration {}",
            m.version
        );
        assert!(
            report.minimum_writer >= floor,
            "after migration {}",
            m.version
        );
        floor = report.minimum_writer;
    }
    let s = h.store();
    let report = s.compatibility().unwrap();
    assert_eq!(
        report.store_schema_version,
        MIGRATIONS.last().unwrap().version
    );
    assert_eq!(
        report.minimum_writer, highest_floor,
        "the store keeps the highest declared writer floor"
    );
    let authority = MIGRATIONS
        .iter()
        .find(|m| m.version == 5)
        .expect("authority migration");
    assert_eq!(
        authority.checksum(),
        MIGRATION_0005_SHA256,
        "an applied migration was edited"
    );
    assert_eq!(authority.minimum_writer, 5);
    assert!(report.migrations.iter().any(|m| m.version == 4));
    drop(s);
    assert_eq!(version_and_ledger(&h, "configuration_proposals").0, 1);
}

const EXTRA: Migration = Migration {
    version: 3,
    minimum_reader: 1,
    minimum_writer: 3,
    sql: "CREATE TABLE fixture_extra(id TEXT PRIMARY KEY) STRICT;",
    rebuilds_tables: false,
};
const BROKEN: Migration = Migration {
    version: 4,
    minimum_reader: 1,
    minimum_writer: 4,
    sql: "CREATE TABLE fixture_partial(id TEXT PRIMARY KEY) STRICT; INSERT INTO no_such_table VALUES(1);",
    rebuilds_tables: false,
};
/// Leaves a (deferred) foreign-key violation behind: the in-transaction check must refuse it.
const DANGLING: Migration = Migration {
    version: 4,
    minimum_reader: 1,
    minimum_writer: 4,
    sql: "CREATE TABLE fixture_dangling(id TEXT PRIMARY KEY) STRICT; PRAGMA defer_foreign_keys=ON; INSERT INTO command_receipts VALUES('nobody','s','k','x','d',NULL,'{}',1);",
    rebuilds_tables: false,
};

fn version_and_ledger(h: &Harness, table: &str) -> (i64, i64, i64) {
    h.raw()
        .query_row(
            "SELECT (SELECT count(*) FROM sqlite_schema WHERE name=?1),(SELECT schema_version FROM store_meta),(SELECT count(*) FROM schema_migrations)",
            [table],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

#[test]
fn migrations_apply_in_order_and_an_interrupted_one_leaves_nothing() {
    let h = Harness::new();
    drop(SqliteStore::open_with(h.config(), &MIGRATIONS[..1]).unwrap());
    let s = SqliteStore::open_with(h.config(), &[MIGRATIONS[0], MIGRATIONS[1], EXTRA]).unwrap();
    let report = s.compatibility().unwrap();
    assert_eq!(
        (
            report.store_schema_version,
            report.build_schema_version,
            report.minimum_writer
        ),
        (3, 3, 3)
    );
    assert_eq!(
        report
            .migrations
            .iter()
            .map(|m| m.version)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    drop(s);
    for (bad, table) in [(BROKEN, "fixture_partial"), (DANGLING, "fixture_dangling")] {
        let failed =
            SqliteStore::open_with(h.config(), &[MIGRATIONS[0], MIGRATIONS[1], EXTRA, bad]);
        assert!(failed.is_err(), "{table}");
        assert_eq!(
            version_and_ledger(&h, table),
            (0, 3, 3),
            "{table} left residue"
        );
    }
    let orphans: i64 = h
        .raw()
        .query_row("SELECT count(*) FROM command_receipts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(orphans, 0);
    assert!(SqliteStore::open_with(h.config(), &[MIGRATIONS[0], MIGRATIONS[1], EXTRA]).is_ok());
}

fn file_digest(h: &Harness) -> String {
    devworks_core::fingerprint::sha256_hex(&std::fs::read(h.root.0.join(DATABASE_FILE)).unwrap())
}

fn boots(h: &Harness) -> i64 {
    h.raw()
        .query_row("SELECT count(*) FROM app_boots", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn incompatible_stores_are_refused_before_any_write() {
    // Checksum mismatch.
    let h = Harness::new();
    drop(h.store());
    h.raw()
        .execute(
            "UPDATE schema_migrations SET checksum='edited' WHERE version=1",
            [],
        )
        .unwrap();
    let before = boots(&h);
    assert!(matches!(
        SqliteStore::open(h.config()),
        Err(StorageError::IncompatibleSchema(_))
    ));
    assert_eq!(boots(&h), before, "an incompatible store was written");

    // Newer reader requirement: not even opened.
    let h = Harness::new();
    drop(h.store());
    // One past this build's schema version, whatever migrations it ships.
    let newer = MIGRATIONS.last().unwrap().version + 1;
    h.raw()
        .execute(
            "UPDATE store_meta SET schema_version=?1,minimum_reader=?1,minimum_writer=?1",
            [newer],
        )
        .unwrap();
    let digest = file_digest(&h);
    assert!(matches!(
        SqliteStore::open(h.config()),
        Err(StorageError::IncompatibleSchema(_))
    ));
    assert_eq!(file_digest(&h), digest, "a refused store was rewritten");

    // Newer writer requirement: diagnostics/read-only mode, no writes at all.
    let h = Harness::new();
    let s = h.store();
    write(&s, |tx| append(tx, "Kept", json!({}))).unwrap();
    drop(s);
    h.raw()
        .execute(
            "UPDATE store_meta SET schema_version=?1,minimum_writer=?1",
            [newer],
        )
        .unwrap();
    let before = boots(&h);
    let s = h.store();
    assert!(matches!(s.info().access, AccessMode::ReadOnly { .. }));
    assert!(s.info().boot_id.is_none());
    assert!(matches!(
        write(&s, |tx| append(tx, "Refused", json!({}))),
        Err(StorageError::IncompatibleSchema(_))
    ));
    assert_eq!(sequence(&s), 1, "committed data stays readable");
    assert!(matches!(
        s.compatibility().unwrap().access,
        AccessMode::ReadOnly { .. }
    ));
    drop(s);
    assert_eq!(boots(&h), before);

    // A newer but writer-compatible (additive) schema stays writable and is not migrated.
    let h = Harness::new();
    drop(h.store());
    h.raw()
        .execute("UPDATE store_meta SET schema_version=?1", [newer])
        .unwrap();
    let s = h.store();
    assert_eq!(s.info().access, AccessMode::ReadWrite);
    assert_eq!(
        s.compatibility().unwrap().migrations.len(),
        MIGRATIONS.len()
    );

    // Not a DevWorks store.
    let h = Harness::new();
    std::fs::create_dir_all(&h.root.0).unwrap();
    h.raw().execute_batch("CREATE TABLE unrelated(x)").unwrap();
    let digest = file_digest(&h);
    assert!(matches!(
        SqliteStore::open(h.config()),
        Err(StorageError::IncompatibleSchema(_))
    ));
    // Refused before any writer PRAGMA: still a rollback-journal file, byte for byte.
    assert_eq!(file_digest(&h), digest, "a foreign file was rewritten");
    let mode: String = h
        .raw()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "delete");
}

// --- writer behavior ----------------------------------------------------------------------------

#[test]
fn drop_without_commit_rolls_back() {
    let h = Harness::new();
    drop(h.store());
    let c = h.raw();
    {
        let _tx = TxGuard::begin(&c).unwrap();
        c.execute("INSERT INTO actors VALUES('dropped','human','Dropped')", [])
            .unwrap();
    }
    let n: i64 = c
        .query_row("SELECT count(*) FROM actors WHERE id='dropped'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 0);
    assert!(c.is_autocommit());
}

#[test]
fn disk_exhaustion_is_visible_and_preserves_committed_data() {
    let h = Harness::new();
    let s = h.store();
    let kept = write(&s, |tx| append(tx, "Kept", json!({}))).unwrap();
    s.with_writer_connection(|c| {
        let pages: i64 = db(c.query_row("PRAGMA page_count", [], |r| r.get(0)))?;
        db(
            c.query_row(&format!("PRAGMA max_page_count={pages}"), [], |r| {
                r.get::<_, i64>(0)
            }),
        )
    })
    .unwrap();
    let big = "x".repeat(256 * 1024);
    let full = write(&s, move |tx| append(tx, "TooBig", json!({ "blob": big })));
    assert!(
        matches!(
            full,
            Err(StorageError::StorageUnavailable {
                cause: Unavailable::DiskFull,
                ..
            })
        ),
        "{full:?}"
    );
    assert_eq!(sequence(&s), kept);
    assert!(s.integrity_check().unwrap().ok());
    s.with_writer_connection(|c| {
        db(c.query_row("PRAGMA max_page_count=1073741823", [], |r| {
            r.get::<_, i64>(0)
        }))
    })
    .unwrap();
    assert!(write(&s, |tx| append(tx, "AfterSpace", json!({}))).is_ok());
}

fn spawn_blocker(s: &SqliteStore) -> (mpsc::Sender<()>, mpsc::Receiver<()>) {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    s.submit_write(
        WriteOptions::default(),
        write_job(
            move |_| {
                started_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
                Ok(())
            },
            |_| {},
        ),
    )
    .unwrap();
    started_rx.recv().unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let _ = done_tx;
    (release_tx, done_rx)
}

#[test]
fn authoritative_work_has_precedence_with_a_reserved_bulk_share_and_bounded_mailboxes() {
    let h = Harness::tuned(|c| {
        c.mailbox_capacity = 4;
        c.bulk_share = 2;
    });
    let s = h.store();
    let (release, _) = spawn_blocker(&s);
    let order = Arc::new(Mutex::new(Vec::new()));
    let (done_tx, done_rx) = mpsc::channel();
    let submit = |label: &'static str, priority: WritePriority| {
        let (order, done) = (order.clone(), done_tx.clone());
        s.submit_write(
            WriteOptions {
                priority,
                ..Default::default()
            },
            write_job(
                move |_| {
                    order.lock().unwrap().push(label);
                    Ok(())
                },
                move |_| {
                    let _ = done.send(());
                },
            ),
        )
    };
    for label in ["A1", "A2", "A3", "A4"] {
        submit(label, WritePriority::Authoritative).unwrap();
    }
    // The authoritative mailbox is full: visible backpressure, nothing buffered beyond it.
    assert!(matches!(
        submit("A5", WritePriority::Authoritative),
        Err(StorageError::Busy)
    ));
    submit("B1", WritePriority::Bulk).unwrap();
    submit("B2", WritePriority::Bulk).unwrap();
    release.send(()).unwrap();
    for _ in 0..6 {
        done_rx.recv().unwrap();
    }
    // The blocker counted as one authoritative job.
    assert_eq!(*order.lock().unwrap(), ["A1", "B1", "A2", "A3", "B2", "A4"]);
}

#[test]
fn bounded_read_pool_reports_busy() {
    let h = Harness::tuned(|c| {
        c.read_connections = 1;
        c.read_wait = Duration::from_millis(20);
    });
    let s = Arc::new(h.store());
    let other = s.clone();
    read_snapshot(&*s, &ReadOptions::default(), move |_| {
        let busy = std::thread::spawn(move || {
            read_snapshot(&*other, &ReadOptions::default(), |_| Ok(())).map(|_| ())
        })
        .join()
        .unwrap();
        assert!(matches!(busy, Err(StorageError::Busy)), "{busy:?}");
        Ok(())
    })
    .unwrap();
    assert!(read_snapshot(&*s, &ReadOptions::default(), |_| Ok(())).is_ok());
}

struct ThreadWaker(std::thread::Thread);
impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}
fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
}

#[test]
fn async_facade_resolves_after_commit_and_dropping_it_never_rolls_back() {
    let h = Harness::new();
    let s = h.store();
    let seq = block_on(write_async(&s, WriteOptions::default(), |tx| {
        append(tx, "Async", json!({}))
    }))
    .unwrap();
    assert_eq!(sequence(&s), seq);
    // The caller stops waiting immediately; the admitted job still commits.
    drop(write_async(&s, WriteOptions::default(), |tx| {
        append(tx, "Abandoned", json!({}))
    }));
    write(&s, |_| Ok(())).unwrap(); // FIFO barrier on the same mailbox
    assert_eq!(sequence(&s), seq + 1);
}

// --- command path on the real store -------------------------------------------------------------

struct Granting(Mutex<Authorization>);
impl AuthorityPort for Granting {
    fn authorize(
        &self,
        _: &InvocationContext,
        _: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        Ok(self.0.lock().unwrap().clone())
    }
}
struct Seq(AtomicU64);
impl IdSource for Seq {
    fn next_id(&self, _: IdKind) -> Id {
        Id::new(format!("corr-{}", self.0.fetch_add(1, Ordering::SeqCst))).unwrap()
    }
}
struct Accept {
    calls: AtomicUsize,
    fail: bool,
}
impl CommandHandler for Accept {
    fn decide(
        &self,
        _: &InvocationContext,
        _: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let op = OperationId(tx.new_id(IdKind::Operation));
        tx.operations().record_intent(NewOperation {
            id: op.clone(),
            root_id: Some(RootId(Id::new("root-a").unwrap())),
            assignment_id: None,
            capability: "runs.start".into(),
            schema_version: 1,
            args_digest: "d".into(),
            payload: json!({}),
        })?;
        if self.fail {
            return Err(HandlerFailure::Storage(StorageError::unavailable(
                Unavailable::Io,
                "injected",
            )));
        }
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: op.clone(),
            },
            operation_id: Some(op),
        })
    }
}
fn start(key: &str) -> String {
    format!(
        r#"{{"protocol":"1.0-draft.1","requestKey":"{key}","command":{{"kind":"runs.start","rootId":"root-a","expectedWorkRevision":"1","expectedControlRevision":"1"}}}}"#
    )
}
fn ctx(s: &SqliteStore) -> InvocationContext {
    let info = s.info();
    devworks_core::context::HostIngress::new(s).context(
        ActorId(Id::new("user-a").unwrap()),
        PrincipalKind::Human,
        Id::new("inv").unwrap(),
        Id::new("ui-main").unwrap(),
        Counter::new(info.restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    )
}
/// Host fixture rows the command path references (owned by later work repositories).
fn seed(s: &SqliteStore) {
    s.with_writer_connection(|c| {
        db(c.execute_batch(
            "INSERT INTO actors VALUES('user-a','human','User A');
             INSERT INTO projects VALUES('project-a','Fixture','Active',1,1,1);
             INSERT INTO work_items VALUES('root-a','project-a','feature',NULL,NULL,NULL,'Fixture',1,0,1,1,1,'{}');",
        ))
    })
    .unwrap();
}
fn grant_state(s: &SqliteStore) -> GrantRecord {
    read_snapshot(s, &ReadOptions::default(), |r| {
        r.grants().grant(&Id::new("grant-1").unwrap())
    })
    .unwrap()
    .unwrap()
}

#[test]
fn a_lost_acknowledgment_is_resolved_by_the_receipt() {
    let h = Harness::new();
    let s = h.store();
    seed(&s);
    let ids = Seq(AtomicU64::new(0));
    let authority = Granting(Mutex::new(Authorization::direct()));
    let g = Gateway {
        authority: &authority,
        store: &s,
        ids: &ids,
    };
    let handler = Arc::new(Accept {
        calls: AtomicUsize::new(0),
        fail: false,
    });
    s.lose_next_ack();
    let reply = g
        .command(
            &ctx(&s),
            "runs.start",
            start("k-ack").as_bytes(),
            handler.clone(),
        )
        .unwrap();
    assert!(matches!(reply, CommandReply::CommandStatusUnknown { .. }));
    let reply = g
        .command(
            &ctx(&s),
            "runs.start",
            start("k-ack").as_bytes(),
            handler.clone(),
        )
        .unwrap();
    assert!(matches!(
        reply,
        CommandReply::Recorded { replayed: true, .. }
    ));
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn grant_use_is_consumed_atomically_with_the_receipt() {
    let h = Harness::new();
    let s = h.store();
    seed(&s);
    write(&s, |tx| {
        tx.grants().issue(NewGrant {
            id: Id::new("grant-1").unwrap(),
            actor: ActorId(Id::new("user-a").unwrap()),
            root_id: RootId(Id::new("root-a").unwrap()),
            authority_source: "explicit-user-instruction".into(),
            binding: json!({"rootId": "root-a"}),
            expires_at_ms: Some(T0 + 60_000),
            uses: Some(1),
        })
    })
    .unwrap();
    let ids = Seq(AtomicU64::new(0));
    let authority = Granting(Mutex::new(Authorization::with_grants(vec![GrantUse {
        grant_id: Id::new("grant-1").unwrap(),
        expected_revision: 1,
    }])));
    let g = Gateway {
        authority: &authority,
        store: &s,
        ids: &ids,
    };
    // A failure after the domain update leaves the grant unspent.
    let failing = Arc::new(Accept {
        calls: AtomicUsize::new(0),
        fail: true,
    });
    let reply = g
        .command(&ctx(&s), "runs.start", start("k-1").as_bytes(), failing)
        .unwrap();
    assert!(matches!(
        reply,
        CommandReply::Rejected {
            code: Rejection::StorageUnavailable,
            ..
        }
    ));
    assert_eq!(grant_state(&s).uses_remaining, Some(1));

    let ok = Arc::new(Accept {
        calls: AtomicUsize::new(0),
        fail: false,
    });
    let reply = g
        .command(&ctx(&s), "runs.start", start("k-1").as_bytes(), ok.clone())
        .unwrap();
    assert!(matches!(
        reply,
        CommandReply::Recorded {
            replayed: false,
            ..
        }
    ));
    let spent = grant_state(&s);
    assert_eq!(
        (spent.uses_remaining, spent.active, spent.revision),
        (Some(0), false, 2)
    );
    // A replay does not spend it again.
    let reply = g
        .command(&ctx(&s), "runs.start", start("k-1").as_bytes(), ok.clone())
        .unwrap();
    assert!(matches!(
        reply,
        CommandReply::Recorded { replayed: true, .. }
    ));
    assert_eq!(grant_state(&s).revision, 2);
    // A new command relying on the spent grant is denied with nothing recorded.
    let before = sequence(&s);
    let reply = g
        .command(&ctx(&s), "runs.start", start("k-2").as_bytes(), ok.clone())
        .unwrap();
    assert!(matches!(
        reply,
        CommandReply::Rejected {
            code: Rejection::PermissionDenied,
            ..
        }
    ));
    assert_eq!(sequence(&s), before);
    assert_eq!(
        ok.calls.load(Ordering::SeqCst),
        2,
        "decided, then rolled back"
    );
}

#[test]
fn driver_types_stay_inside_the_adapter() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    for krate in ["devworks-core", "devworks-extension-protocol"] {
        let manifest =
            std::fs::read_to_string(root.join("crates").join(krate).join("Cargo.toml")).unwrap();
        for driver in ["rusqlite", "libsqlite3", "sqlite-vec", "sqlx", "diesel"] {
            assert!(!manifest.contains(driver), "{krate} depends on {driver}");
        }
    }
    fn scan(dir: &Path) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                scan(&p);
            } else if p.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&p).unwrap();
                assert!(!text.contains("rusqlite"), "{} names rusqlite", p.display());
            }
        }
    }
    scan(&root.join("crates/devworks-core/src"));
}

#[test]
fn a_panicking_responder_never_stops_the_write_owner() {
    let h = Harness::new();
    let s = h.store();
    s.submit_write(
        WriteOptions::default(),
        write_job(
            |tx| append(tx, "Committed", json!({})),
            |_| panic!("responder panicked"),
        ),
    )
    .unwrap();
    // The owner keeps serving; the panicking job's commit stands.
    write(&s, |tx| append(tx, "After", json!({}))).unwrap();
    assert_eq!(sequence(&s), 2);
}

#[test]
fn queued_jobs_are_finished_when_the_owner_stops() {
    let h = Harness::new();
    let s = h.store();
    let (release, _) = spawn_blocker(&s);
    let (tx, rx) = mpsc::channel();
    s.submit_write(
        WriteOptions::default(),
        write_job(
            |tx| append(tx, "Queued", json!({})),
            move |r| {
                let _ = tx.send(r.map(|_| ()));
            },
        ),
    )
    .unwrap();
    let stopper = std::thread::spawn(move || drop(s));
    // Let the stop request land before the blocker finishes, then release it.
    std::thread::sleep(Duration::from_millis(20));
    release.send(()).unwrap();
    stopper.join().unwrap();
    // Finished either way (dispatched before the stop was seen, or reported not dispatched).
    let outcome = rx
        .recv()
        .expect("queued job was dropped without being finished");
    assert!(
        matches!(
            outcome,
            Ok(())
                | Err(StorageError::StorageUnavailable {
                    cause: Unavailable::WriterStopped,
                    ..
                })
        ),
        "{outcome:?}"
    );
}

#[test]
fn a_cancelled_read_stops_waiting_promptly() {
    let h = Harness::tuned(|c| {
        c.read_connections = 1;
        c.read_wait = Duration::from_secs(30);
    });
    let s = Arc::new(h.store());
    let other = s.clone();
    read_snapshot(&*s, &ReadOptions::default(), move |_| {
        let token = Cancellation::new();
        let waiter_token = token.clone();
        let waiter = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let r = read_snapshot(
                &*other,
                &ReadOptions {
                    cancellation: Some(waiter_token),
                    deadline_ms: None,
                },
                |_| Ok(()),
            );
            (r.map(|_| ()), started.elapsed())
        });
        std::thread::sleep(Duration::from_millis(20));
        token.cancel();
        let (r, waited) = waiter.join().unwrap();
        assert!(matches!(r, Err(StorageError::Cancelled)), "{r:?}");
        assert!(waited < Duration::from_secs(5), "{waited:?}");
        Ok(())
    })
    .unwrap();
}
