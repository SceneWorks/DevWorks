//! Versioned schema migrations with a checksum ledger, checked before any write access (STORE-06).
//!
//! The ledger (`schema_migrations`) is authoritative; `user_version` is only an aid. Each
//! migration owns one `BEGIN IMMEDIATE` transaction that changes the schema and updates
//! `store_meta` versions and the ledger together, followed by foreign-key and quick integrity
//! checks. An applied migration whose checksum differs from this build's text is an
//! incompatibility, never permission to rerun edited SQL. Only transactional, non-destructive
//! migrations are applied automatically; no backup is ever taken automatically (STORE-07).
use super::conn::db;
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::storage::{AccessMode, AppliedMigration, StorageError};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Clone, Copy, Debug)]
pub struct Migration {
    pub version: i64,
    /// Oldest build version that can still read a store at this version.
    pub minimum_reader: i64,
    /// Oldest build version that can still write a store at this version.
    pub minimum_writer: i64,
    pub sql: &'static str,
    /// The migration rebuilds a table other tables reference (SQLite's documented table-rebuild
    /// procedure: create, copy, drop, rename). Foreign-key enforcement is switched off for its
    /// one transaction, because dropping a referenced table would otherwise fail, and the full
    /// foreign-key check must still pass before it commits. Enforcement is restored afterwards
    /// whatever the outcome.
    pub rebuilds_tables: bool,
}
impl Migration {
    pub fn checksum(&self) -> String {
        sha256_hex(self.sql.as_bytes())
    }
}

/// This build's migrations, in version order. The last version is the build's schema version.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        minimum_reader: 1,
        minimum_writer: 1,
        sql: include_str!("migrations/0001_initial.sql"),
        rebuilds_tables: false,
    },
    // Writers older than 2 would not bind claims to their boot epoch, so they may not write.
    Migration {
        version: 2,
        minimum_reader: 1,
        minimum_writer: 2,
        sql: include_str!("migrations/0002_claim_boot.sql"),
        rebuilds_tables: false,
    },
    // A version-2 build can still read deliveries, but writing them without partition/sequence
    // and revision bookkeeping would break ordered delivery, so writers must understand 3.
    Migration {
        version: 3,
        minimum_reader: 1,
        minimum_writer: 3,
        sql: include_str!("migrations/0003_durable_delivery.sql"),
        rebuilds_tables: false,
    },
    // Additive (a new table only). Applying a migration overwrites the store's writer floor, so
    // it keeps the floor of migration 3: it must never re-admit writers that cannot keep ordered
    // delivery bookkeeping or bind claims to their boot.
    Migration {
        version: 4,
        minimum_reader: 1,
        minimum_writer: 3,
        sql: include_str!("migrations/0004_settings.sql"),
        rebuilds_tables: false,
    },
    // Additive tables. Writers older than 5 would ignore principal revocation and authority
    // grants, so they may not write; the floor also stays at or above every earlier floor.
    // Readers older than 5 treat the `mcp_client` actor kind as corrupt, so they may not read
    // either: older builds open such a store read-refused instead of misreporting corruption.
    Migration {
        version: 5,
        minimum_reader: 5,
        minimum_writer: 5,
        sql: include_str!("migrations/0005_authority.sql"),
        rebuilds_tables: false,
    },
    // Additive columns, tables and indexes. Applying a migration overwrites both store floors,
    // so it keeps the highest floors so far (5/5). Nothing here needs a higher reader floor:
    // builds 5 and older never read artifact rows, and the added columns all have defaults, so
    // their existing queries see the same results. Raising the writer floor is not needed
    // either: no earlier build writes artifact tables. Lowering either floor would re-admit
    // builds that migration 5 refuses.
    Migration {
        version: 6,
        minimum_reader: 5,
        minimum_writer: 5,
        sql: include_str!("migrations/0006_artifacts.sql"),
        rebuilds_tables: false,
    },
    // Additive columns (all defaulted or nullable), tables and indexes. It keeps the highest
    // floors so far (5/5), because applying a migration overwrites both store floors and a lower
    // value would re-admit builds that migration 5 refuses. No higher floor is needed: builds 6
    // and older never read or write sessions, transcript entries or any of the new tables (no
    // earlier build has a session repository), and the defaulted columns leave their results
    // unchanged.
    Migration {
        version: 7,
        minimum_reader: 5,
        minimum_writer: 5,
        sql: include_str!("migrations/0007_sessions.sql"),
        rebuilds_tables: false,
    },
    // Rebuilds `artifacts` (nullable project columns for the workspace scope) and adds the
    // receipt reply records. Both floors rise to 8: a build 7 or older reads every artifact row
    // as project-owned and would report a workspace artifact as corrupt, and as a writer it
    // would delete receipt-retained artifacts and record receipts without their publication
    // intent. 8 is at least every earlier floor (5/5).
    Migration {
        version: 8,
        minimum_reader: 8,
        minimum_writer: 8,
        sql: include_str!("migrations/0008_receipt_replies.sql"),
        rebuilds_tables: true,
    },
    // Additive indexes and the tool_invocations table (backfilled from the recorded responses).
    // The reader floor stays at 8: no earlier build reads tool_invocations, and the indexes leave
    // every query's results unchanged. The writer floor rises to 9: a build 8 writer would append
    // tool responses without their invocation facts, so revision-2 tool usage would silently
    // miss them. Both are at least every earlier floor (8/8).
    Migration {
        version: 9,
        minimum_reader: 8,
        minimum_writer: 9,
        sql: include_str!("migrations/0009_history_feeds.sql"),
        rebuilds_tables: false,
    },
    // Additive columns (all defaulted or nullable), tables and indexes. The reader floor stays
    // at 8: no earlier build reads resources, leases or owned processes (the migration 1
    // resources table was never written), and the defaulted columns leave every existing query
    // unchanged. The writer floor stays at 9: no earlier build writes any of these records, so
    // it cannot break their fencing or queue invariants. Both are at least every earlier floor.
    Migration {
        version: 10,
        minimum_reader: 8,
        minimum_writer: 9,
        sql: include_str!("migrations/0010_resources_processes.sql"),
        rebuilds_tables: false,
    },
    // Additive columns (all defaulted or nullable), tables and indexes for run root controls.
    // The writer floor rises to 11: a build 10 or older has no run-control gate, so as a writer
    // it would claim and run the root-scoped deliveries of a paused, held or stopped root and
    // bypass the restart hold; it must open this store read-only. The reader floor stays at 8:
    // no earlier build reads root controls, activations, assignments, blockers, run instructions
    // or session turns (the migration 1 tables were never written by a build), and the defaulted
    // columns leave every query an earlier reader makes unchanged, so reading stays safe. Both
    // are at least every earlier floor.
    Migration {
        version: 11,
        minimum_reader: 8,
        minimum_writer: 11,
        sql: include_str!("migrations/0011_run_controls.sql"),
        rebuilds_tables: false,
    },
    // Additive: the retained diagnostic log, its retention boundary and export intents (three
    // tables and their indexes), independent of every table of migration 11. The writer floor is
    // 11, the floor migration 11 set: applying a migration overwrites the store's floors, so a
    // lower value would re-admit the build-10 writers that migration 11 refuses (they would
    // bypass run controls). It need not rise to 12: a build-11 writer never writes these tables,
    // so it cannot break the log's sequence, retention boundary or export claims, and every
    // other record it writes is unchanged here. The reader floor stays at 8: no earlier build
    // reads these tables, and they leave every query an earlier reader makes unchanged.
    Migration {
        version: 12,
        minimum_reader: 8,
        minimum_writer: 11,
        sql: include_str!("migrations/0012_diagnostics.sql"),
        rebuilds_tables: false,
    },
    // Additive: the router catalog (seeded once), tier cursors, session pins, cooldowns and
    // request bindings (five tables and their indexes), independent of every table of
    // migrations 11-12.
    // The writer floor stays at 11 (the highest so far): applying a migration overwrites the
    // store's floors, so a lower value would re-admit the build-10 writers migration 11 refuses.
    // It need not rise: no earlier build writes these tables, so it cannot break their cursor
    // and pin compare-and-set rules, and every record it writes is unchanged here. The reader
    // floor stays at 8: no earlier build reads these tables, and every query an earlier reader
    // makes is unchanged.
    Migration {
        version: 13,
        minimum_reader: 8,
        minimum_writer: 11,
        sql: include_str!("migrations/0013_router.sql"),
        rebuilds_tables: false,
    },
    // Additive: project and repository side tables, the index view registry and repository
    // texts, and four indexes; the migration 1 tables keep their shape. The writer floor rises
    // to 14: a build without this migration (up to and including one with migration 13) treats
    // only `Archived` as closed to work, so as a writer it would dispatch the work of a project
    // tombstoned `Deleted`, and it would accept configuration, sessions, artifacts and resources
    // for such a project. The reader floor stays at 8: no earlier build reads the new tables,
    // and the indexes leave every query an earlier reader makes unchanged. Both are at least
    // every earlier floor.
    Migration {
        version: 14,
        minimum_reader: 8,
        minimum_writer: 14,
        sql: include_str!("migrations/0014_projects.sql"),
        rebuilds_tables: false,
    },
    // Additive: provider connections and SystemOne decision models (two tables and their
    // indexes), one nullable column on router_models and a correction of still-unedited seed
    // rows; independent of every table of migration 14. The writer floor stays at 14 (the
    // highest so far): applying a migration overwrites the store's floors, so a lower value
    // would re-admit the writers migration 14 refuses (they would dispatch the work of a deleted
    // project). It need not rise: no earlier build writes the new tables, and the only earlier
    // writer of router_models names its columns and leaves connection_id untouched, so a binding
    // it does not know is kept, never cleared; a binding that no longer matches the model's
    // profile only makes that model ineligible. The reader floor stays at 8: earlier readers
    // select explicit columns, so the added column and tables leave every query they make
    // unchanged.
    Migration {
        version: 15,
        minimum_reader: 8,
        minimum_writer: 14,
        sql: include_str!("migrations/0015_router_connections.sql"),
        rebuilds_tables: false,
    },
    // Additive: the work detail, queue revision, plan-approval request, readiness and stop
    // coverage side tables and five indexes; the migration 1 work tables keep their shape. The
    // writer floor rises to 16: a build without this migration evaluates dispatch eligibility
    // without the work fences written here (unverified or open prerequisites, a card's own
    // unlink pause under its new feature, a card moved to another feature, the parent feature's
    // controls over a linked card), so as a writer it would dispatch work those records hold,
    // and it would settle a feature's stop without the linked cards it came to cover. The reader
    // floor stays at 8: no earlier build reads the new tables, and the indexes leave every query
    // an earlier reader makes unchanged. Both are at least every earlier floor.
    Migration {
        version: 16,
        minimum_reader: 8,
        minimum_writer: 16,
        sql: include_str!("migrations/0016_work.sql"),
        rebuilds_tables: false,
    },
    // Additive: columns (all defaulted or nullable) on `schedules` and `job_runs`, which no
    // earlier build ever wrote (migration 1 declared them for this item), their indexes, and four
    // new tables (run links, discovery checkpoints, action identities, per-run usage);
    // independent of every table of migrations 14-16. The writer floor is 16, the highest so far
    // (migration 16 raised it): applying a migration overwrites the store's floors, so a lower
    // value would re-admit the writers migrations 14 and 16 refuse (they would dispatch a
    // deleted project's work, or work the migration 16 fences hold). It need not rise to 17: no
    // earlier build reads or writes any of these records, so it cannot break the one-active-run
    // index, the title uniqueness or the occurrence identities, and every record it does write
    // is unchanged here; a project an earlier writer archives still fences its schedules, since
    // every claim, dispatch and resumption checks the project's lifecycle and archive epoch
    // itself. The reader floor stays at 8: no earlier build reads these tables, and every query
    // an earlier reader makes returns the same results.
    Migration {
        version: 17,
        minimum_reader: 8,
        minimum_writer: 16,
        sql: include_str!("migrations/0017_jobs.sql"),
        rebuilds_tables: false,
    },
    // Additive: the unified Knowledge tables, their indexes and two FTS5 projections, referencing
    // only actors, projects and sessions (migration 1/7 tables, unchanged).
    // The writer floor stays at 16 (the highest so far): applying a migration overwrites the
    // store's floors, so a lower value would re-admit the writers migrations 14 and 16 refuse. It
    // need not rise: no earlier build writes these tables, so none can break their
    // compare-and-set revisions, scope revisions or lexical rows, and every record an earlier
    // writer writes is unchanged here. The reader floor stays at 8: no earlier build reads these
    // tables, and every query an earlier reader makes returns the same results.
    Migration {
        version: 18,
        minimum_reader: 8,
        minimum_writer: 16,
        sql: include_str!("migrations/0018_knowledge.sql"),
        rebuilds_tables: false,
    },
    // Additive (DW-023): index source configuration, observed source identity, worktrees, index
    // runs and overlays (five tables), defaulted payload columns on the migration 1 unit and edge
    // tables, and six indexes. The writer floor rises to 19 (above every earlier
    // floor): an earlier writer's `repositories.remove_index` removes only the migration 14
    // registry rows, so it would leave this build's published pointer, views and graph payload
    // describing an index the registry no longer lists (a removed index that still reads as
    // current). The reader floor stays at 8: no earlier build reads the new tables or columns,
    // and every query an earlier reader makes returns the same rows (the added columns are
    // defaulted). Both are at least every earlier floor.
    Migration {
        version: 19,
        minimum_reader: 8,
        minimum_writer: 19,
        sql: include_str!("migrations/0019_indexing.sql"),
        rebuilds_tables: false,
    },
    // Additive: the package registry head, package records, verified versions, approval
    // proposals, retained state generations, index sources and availability observations (seven
    // tables and an index), independent of every table of migrations 11-19 (it references only
    // its own tables). The writer floor stays at 19, the highest so far: applying a migration
    // overwrites both floors, so a lower writer floor would re-admit the writers migrations 11, 14, 16 and 19 refuse. It need not rise: no earlier build reads or writes these tables, and the
    // package handler state rows (migration 1) a generation publication rewrites were never
    // written by an earlier build. The reader floor stays at 8: no earlier build reads these
    // tables, and every query an earlier reader makes is unchanged.
    Migration {
        version: 20,
        minimum_reader: 8,
        minimum_writer: 19,
        sql: include_str!("migrations/0020_packages.sql"),
        rebuilds_tables: false,
    },
    // Additive: owned browser sessions and their observation index (two tables and their
    // indexes; versions 1-20 are retained). The writer floor
    // stays at 19 (the highest so far): applying a migration overwrites the store's floors, so
    // a lower value would re-admit the writers migration 14 refuses. It need not rise: an
    // earlier writer never writes these tables, and every record it writes is unchanged here
    // (artifact, operation, process and actor rows are never deleted, so the references cannot
    // break). The reader floor stays at 8: no earlier build reads them.
    Migration {
        version: 21,
        minimum_reader: 8,
        minimum_writer: 19,
        sql: include_str!("migrations/0021_browser_sessions.sql"),
        rebuilds_tables: false,
    },
    // Additive owned Git identities; preserves all earlier reader/writer floors.
    Migration {
        version: 22,
        minimum_reader: 8,
        minimum_writer: 19,
        sql: include_str!("migrations/0022_git_worktrees.sql"),
        rebuilds_tables: false,
    },
    // Old writers cannot preserve invocation-bound provenance on new browser evidence.
    Migration {
        version: 23,
        minimum_reader: 8,
        minimum_writer: 23,
        sql: include_str!("migrations/0023_browser_source.sql"),
        rebuilds_tables: false,
    },
    // Additive immutable eval reports; previous readers and writers preserve these rows.
    Migration {
        version: 24,
        minimum_reader: 8,
        minimum_writer: 23,
        sql: include_str!("migrations/0024_eval_runs.sql"),
        rebuilds_tables: false,
    },
    // Additive concrete pin snapshots. Old writers would silently follow catalog identity
    // changes without replacing the pin, so only writers that preserve snapshots may write.
    Migration {
        version: 25,
        minimum_reader: 8,
        minimum_writer: 25,
        sql: include_str!("migrations/0025_router_pin_identity.sql"),
        rebuilds_tables: false,
    },
    // Earlier writers do not know allocation ownership or its membership fences.
    Migration {
        version: 26,
        minimum_reader: 8,
        minimum_writer: 26,
        sql: include_str!("migrations/0026_allocations.sql"),
        rebuilds_tables: false,
    },
    // Earlier writers cannot capture reviewed plan content or preserve decision wakeup bindings.
    Migration {
        version: 27,
        minimum_reader: 8,
        minimum_writer: 27,
        sql: include_str!("migrations/0027_decisions.sql"),
        rebuilds_tables: false,
    },
    // New writers preserve canonical attempt ownership, reservations and historical tariff identity.
    Migration {
        version: 28,
        minimum_reader: 8,
        minimum_writer: 28,
        sql: include_str!("migrations/0028_usage.sql"),
        rebuilds_tables: true,
    },
];
/// Recorded checksum of migration 17; a test fails if the shipped text changes.
pub const MIGRATION_0017_SHA256: &str =
    "c4c2ed1f2231f0cc56c9b55593ba481a39fb0b688fcef61579eb361972274fae";
/// Recorded checksum of migration 16; a test fails if the shipped text changes.
pub const MIGRATION_0016_SHA256: &str =
    "4868dd86ca4af8c5d4e77aa54061dcd0b1e17aac0bf5f5506fdbe231305f847c";

/// Recorded checksum of migration 18; a test fails if the shipped text changes.
pub const MIGRATION_0018_SHA256: &str =
    "0461f125bd4b1f5889d535749f33214d379cf7965dd5fb701d13e09648853127";

/// Recorded checksum of migration 19; a test fails if the shipped text changes.
pub const MIGRATION_0019_SHA256: &str =
    "ed77284397dee952eb7feccf17843669545a61f0c0f35797b0fc350f4a32121f";
/// Recorded checksum of migration 14; a test fails if the shipped text changes.
pub const MIGRATION_0014_SHA256: &str =
    "c15736262585b6da18c679eecb6d1263d9263c3b9272725c4d0fa1bc5cd14534";
/// Recorded checksum of migration 15; a test fails if the shipped text changes.
pub const MIGRATION_0015_SHA256: &str =
    "950ca955706f93e49bd4abafbaedbeac0f60342bfd1e5001fde47b5202985243";
/// Recorded checksum of migration 13; a test fails if the shipped text changes.
pub const MIGRATION_0013_SHA256: &str =
    "a666371fe419bf0b2a63e69762a8ca8d5fa9dfb123d4ca019fc2db98087c004b";
/// Recorded checksum of migration 12; a test fails if the shipped text changes.
pub const MIGRATION_0012_SHA256: &str =
    "dbcf28e8dc5a8eb6e4b801fb9f976021cd5e14cbf9e2b5b82af38f008c6cedba";
/// Recorded checksum of migration 11; a test fails if the shipped text changes.
pub const MIGRATION_0011_SHA256: &str =
    "8607df6d8f6fd0fe885e21595e7e5245422916b8bdf0d0679fd7f0ca5b0f19af";
/// Recorded checksum of migration 10; a test fails if the shipped text changes.
pub const MIGRATION_0010_SHA256: &str =
    "7fab1dd563202155578de4ffe4e14bf89647e97815685f0e58a886a051c20f01";
/// Recorded checksum of migration 9; a test fails if the shipped text changes.
pub const MIGRATION_0009_SHA256: &str =
    "0a893b2c15371f9d9d251bb45814eb9666ee883f94a5706a165184f5fd88c128";
/// Recorded checksum of migration 8; a test fails if the shipped text changes.
pub const MIGRATION_0008_SHA256: &str =
    "682af46e64f4ee7d6d854acdbec0862309a2e37262cf639124ccfb0c57a79cb9";
/// Recorded checksum of migration 7; a test fails if the shipped text changes.
pub const MIGRATION_0007_SHA256: &str =
    "2863f135528fc5a7c7499515872eb17a5acbddd1874fbd108d91ad2cd1253077";
/// Recorded checksum of migration 6; a test fails if the shipped text changes.
pub const MIGRATION_0006_SHA256: &str =
    "7a2ebf3518bed204be5592dcdee4ad203b1971efc421c26535a1a6478571e768";
/// Recorded checksum of migration 5; a test fails if the shipped text changes.
pub const MIGRATION_0005_SHA256: &str =
    "3b8c62996c4506f42eb45c0e76db77d4a7160135090c48f533eaec821e48372f";
/// Recorded checksum of migration 4; a test fails if the shipped text changes.
pub const MIGRATION_0004_SHA256: &str =
    "15188a10f9c8f49ffde96563e636ccda2116024a7e6bda1e3e01096e25d7bad9";
/// Recorded checksum of migration 2; a test fails if the shipped text changes.
pub const MIGRATION_0002_SHA256: &str =
    "a400cacbe5ef06f2045e6fe839c35c3adbe46dcc52a4aa110608498d88da1d37";
/// Recorded checksum of migration 1; a test fails if the shipped text changes.
pub const MIGRATION_0001_SHA256: &str =
    "c64cc140ad8de8c4c2ca7e7d3e3efac32966cea8bfc0bdf5e46e9c55206bd6a4";
/// Recorded checksum of migration 3.
pub const MIGRATION_0003_SHA256: &str =
    "b08289a2ef6eea452e7b7513c4c811564b90c2bd550445cb932212805be24ddc";

#[derive(Clone, Debug)]
pub(crate) struct Meta {
    pub store_id: String,
    pub schema_version: i64,
    pub minimum_reader: i64,
    pub minimum_writer: i64,
    pub restore_epoch: i64,
}

pub(crate) fn read_meta(conn: &Connection) -> Result<Option<Meta>, StorageError> {
    let has_meta: bool = db(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='store_meta')",
        [],
        |r| r.get(0),
    ))?;
    if !has_meta {
        return Ok(None);
    }
    db(conn
        .query_row(
            "SELECT store_id,schema_version,minimum_reader,minimum_writer,restore_epoch FROM store_meta WHERE singleton=1",
            [],
            |r| {
                Ok(Meta {
                    store_id: r.get(0)?,
                    schema_version: r.get(1)?,
                    minimum_reader: r.get(2)?,
                    minimum_writer: r.get(3)?,
                    restore_epoch: r.get(4)?,
                })
            },
        )
        .optional())
}

pub(crate) fn ledger(conn: &Connection) -> Result<Vec<AppliedMigration>, StorageError> {
    let mut stmt = db(conn.prepare(
        "SELECT version,checksum,applied_at,app_build FROM schema_migrations ORDER BY version",
    ))?;
    let rows = db(stmt.query_map([], |r| {
        Ok(AppliedMigration {
            version: r.get(0)?,
            checksum: r.get(1)?,
            applied_at_ms: r.get(2)?,
            app_build: r.get(3)?,
        })
    }))?;
    db(rows.collect())
}

fn user_tables(conn: &Connection) -> Result<i64, StorageError> {
    db(conn.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    ))
}

fn verify_after(conn: &Connection) -> Result<(), StorageError> {
    let fk: i64 = db(
        conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        }),
    )?;
    let quick: String = db(conn.query_row("PRAGMA quick_check", [], |r| r.get(0)))?;
    if fk != 0 || quick != "ok" {
        return Err(StorageError::IncompatibleSchema(format!(
            "post-migration check failed: {fk} foreign-key violations, quick_check={quick}"
        )));
    }
    Ok(())
}

/// Applies one migration in its own transaction; on any error nothing of it remains.
fn apply(
    conn: &Connection,
    m: &Migration,
    bootstrap_store_id: Option<&str>,
    now_ms: i64,
    app_build: &str,
) -> Result<(), StorageError> {
    if m.rebuilds_tables {
        // A no-op inside a transaction, so it is switched before BEGIN.
        db(conn.pragma_update(None, "foreign_keys", false))?;
    }
    let outcome = apply_in_transaction(conn, m, bootstrap_store_id, now_ms, app_build);
    if m.rebuilds_tables {
        let restored = db(conn.pragma_update(None, "foreign_keys", true));
        outcome?;
        return restored;
    }
    outcome
}

fn apply_in_transaction(
    conn: &Connection,
    m: &Migration,
    bootstrap_store_id: Option<&str>,
    now_ms: i64,
    app_build: &str,
) -> Result<(), StorageError> {
    db(conn.execute_batch("BEGIN IMMEDIATE"))?;
    let result = (|| {
        db(conn.execute_batch(m.sql))?;
        match bootstrap_store_id {
            Some(store_id) => db(conn.execute(
                "INSERT INTO store_meta(singleton,store_id,schema_version,minimum_reader,minimum_writer,restore_epoch) VALUES(1,?1,?2,?3,?4,0)",
                params![store_id, m.version, m.minimum_reader, m.minimum_writer],
            ))?,
            None => db(conn.execute(
                "UPDATE store_meta SET schema_version=?1,minimum_reader=?2,minimum_writer=?3 WHERE singleton=1",
                params![m.version, m.minimum_reader, m.minimum_writer],
            ))?,
        };
        db(conn.execute(
            "INSERT INTO schema_migrations(version,checksum,applied_at,app_build) VALUES(?1,?2,?3,?4)",
            params![m.version, m.checksum(), now_ms, app_build],
        ))?;
        db(conn.pragma_update(None, "user_version", m.version))?;
        // Validate inside the transaction: a migration that breaks foreign keys or integrity is
        // never committed.
        verify_after(conn)
    })();
    match result {
        Ok(()) => db(conn.execute_batch("COMMIT")),
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Checks store identity, minimum reader/writer versions and the migration ledger without
/// writing anything. `Ok(None)` means an empty file that may be bootstrapped.
pub(crate) fn check(
    conn: &Connection,
    migrations: &[Migration],
) -> Result<Option<(Meta, AccessMode)>, StorageError> {
    let build = migrations.last().map_or(0, |m| m.version);
    let meta = match read_meta(conn)? {
        Some(meta) => meta,
        None if user_tables(conn)? == 0 => return Ok(None),
        None => {
            return Err(StorageError::IncompatibleSchema(
                "database is not a DevWorks store (no store_meta)".into(),
            ));
        }
    };
    if meta.minimum_reader > build {
        return Err(StorageError::IncompatibleSchema(format!(
            "store schema {} requires reader version {}; this build is {build}",
            meta.schema_version, meta.minimum_reader
        )));
    }
    let applied = ledger(conn)?;
    for m in migrations
        .iter()
        .filter(|m| m.version <= meta.schema_version)
    {
        match applied.iter().find(|a| a.version == m.version) {
            Some(a) if a.checksum == m.checksum() => {}
            Some(a) => {
                return Err(StorageError::IncompatibleSchema(format!(
                    "migration {} checksum {} does not match this build ({})",
                    m.version,
                    a.checksum,
                    m.checksum()
                )));
            }
            None => {
                return Err(StorageError::IncompatibleSchema(format!(
                    "migration {} missing from the ledger",
                    m.version
                )));
            }
        }
    }
    let access = if meta.minimum_writer > build {
        AccessMode::ReadOnly {
            reason: format!(
                "store schema {} requires writer version {}; this build is {build} (diagnostics/export only)",
                meta.schema_version, meta.minimum_writer
            ),
        }
    } else {
        AccessMode::ReadWrite
    };
    Ok(Some((meta, access)))
}

/// Bootstraps an empty store or applies pending migrations, after [`check`]; only called when
/// this build may write. Returns the resulting metadata.
pub(crate) fn prepare(
    conn: &Connection,
    migrations: &[Migration],
    new_store_id: impl FnOnce() -> String,
    now_ms: i64,
    app_build: &str,
) -> Result<(Meta, AccessMode), StorageError> {
    let (meta, access) = match check(conn, migrations)? {
        Some(found) => found,
        None => {
            let first = migrations
                .first()
                .ok_or_else(|| StorageError::IncompatibleSchema("no migrations".into()))?;
            apply(conn, first, Some(&new_store_id()), now_ms, app_build)?;
            check(conn, migrations)?.ok_or_else(|| {
                StorageError::IncompatibleSchema("bootstrap lost store_meta".into())
            })?
        }
    };
    if let AccessMode::ReadOnly { .. } = access {
        return Ok((meta, access));
    }
    for m in migrations
        .iter()
        .filter(|m| m.version > meta.schema_version)
    {
        apply(conn, m, None, now_ms, app_build)?;
    }
    let meta = read_meta(conn)?
        .ok_or_else(|| StorageError::IncompatibleSchema("store_meta disappeared".into()))?;
    Ok((meta, AccessMode::ReadWrite))
}

/// Recorded checksum of migration 20; shipped SQL is immutable.
pub const MIGRATION_0020_SHA256: &str =
    "7cabf466f43dcc1677f62ad14ebb1e96ed9239f2cf505e9646c0a4e38287876a";

/// Recorded checksum of migration 21; shipped SQL is immutable.
pub const MIGRATION_0021_SHA256: &str =
    "27be7d33bb8b6ff768c4de0c2f688ba5442ef699a06130349a054913dcd5bc77";

/// Recorded checksum of migration 22.
pub const MIGRATION_0022_SHA256: &str =
    "2d0815af52a522e23870f9a2af56dc2a6f2f5227fbb842d1377cd49f84e02524";

/// Recorded checksum of migration 23.
pub const MIGRATION_0023_SHA256: &str =
    "bd92e57bf3aa579397301ccbb06deb45570712db11fa2ff229045e0c92c59c93";

/// Recorded checksum of migration 24.
pub const MIGRATION_0024_SHA256: &str =
    "7a648e33e052c1f0e8d2cd5d795f4a68a05f97a97399040b4be4631b7750f44b";

/// Recorded checksum of migration 25.
pub const MIGRATION_0025_SHA256: &str =
    "36b3e80b714419a911ac7e7dd74108b6bb17e4e5efedc3c24b8faf76b349d28f";

/// Recorded checksum of migration 26.
pub const MIGRATION_0026_SHA256: &str =
    "11c429872a1895385d75fd449994beb9bda95ccaf4e920dea63b944da3e2d478";

/// Recorded checksum of migration 27.
pub const MIGRATION_0027_SHA256: &str =
    "dafafed20991660c492ef38f23448111618cecb0b5f061793f266271a75c1c8f";

/// Recorded checksum of migration 28.
pub const MIGRATION_0028_SHA256: &str =
    "e7a0aa389a4f71a7e4bd0956bcd862fbddc55d0ee7ba3d3910478e7e44be3d96";
