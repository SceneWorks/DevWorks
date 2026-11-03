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
}
impl Migration {
    pub fn checksum(&self) -> String {
        sha256_hex(self.sql.as_bytes())
    }
}

/// This build's migrations, in version order. The last version is the build's schema version.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    minimum_reader: 1,
    minimum_writer: 1,
    sql: include_str!("migrations/0001_initial.sql"),
}];
/// Recorded checksum of migration 1; a test fails if the shipped text changes.
pub const MIGRATION_0001_SHA256: &str =
    "c64cc140ad8de8c4c2ca7e7d3e3efac32966cea8bfc0bdf5e46e9c55206bd6a4";

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
        Ok(())
    })();
    match result {
        Ok(()) => db(conn.execute_batch("COMMIT"))?,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
    }
    verify_after(conn)
}

/// Checks identity, compatibility versions and the ledger, applying pending migrations only when
/// this build may write. Returns the resulting metadata and access mode.
pub(crate) fn prepare(
    conn: &Connection,
    migrations: &[Migration],
    new_store_id: impl FnOnce() -> String,
    now_ms: i64,
    app_build: &str,
) -> Result<(Meta, AccessMode), StorageError> {
    let build = migrations.last().map_or(0, |m| m.version);
    let meta = match read_meta(conn)? {
        Some(meta) => meta,
        None if user_tables(conn)? == 0 => {
            let first = migrations
                .first()
                .ok_or_else(|| StorageError::IncompatibleSchema("no migrations".into()))?;
            apply(conn, first, Some(&new_store_id()), now_ms, app_build)?;
            read_meta(conn)?.ok_or_else(|| {
                StorageError::IncompatibleSchema("bootstrap lost store_meta".into())
            })?
        }
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
    if meta.minimum_writer > build {
        let reason = format!(
            "store schema {} requires writer version {}; this build is {build} (diagnostics/export only)",
            meta.schema_version, meta.minimum_writer
        );
        return Ok((meta, AccessMode::ReadOnly { reason }));
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
