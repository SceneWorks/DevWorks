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
];
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
