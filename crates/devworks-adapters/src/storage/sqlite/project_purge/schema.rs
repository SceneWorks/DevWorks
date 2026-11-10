//! Reject unreviewed persistent owning shapes before preparing any deletion authority.
use super::super::{conn::db, migrations::MIGRATIONS};
use devworks_core::ports::storage::StorageError;
use rusqlite::Connection;
use std::sync::OnceLock;

type Definitions = Vec<(String, String, String, Option<String>)>;

fn definitions(conn: &Connection) -> Result<Definitions, StorageError> {
    let mut statement = db(conn.prepare(
        "SELECT type,name,tbl_name,sql FROM main.sqlite_schema ORDER BY type,name,tbl_name",
    ))?;
    db(statement.query_map([], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    }))?
    .map(db)
    .collect()
}

pub(super) fn verify(conn: &Connection) -> Result<(), StorageError> {
    // A later build must extend the ownership policy deliberately, rather than
    // blessing a newly migrated owning table merely because it is in MIGRATIONS.
    // Migration 48 rebuilds session_context_epochs to add MembershipHandoff to its reason
    // constraint; it adds no table or ownership edge. Its rows keep their session owner.
    if MIGRATIONS.last().map(|migration| migration.version) != Some(48) {
        return Err(StorageError::IncompatibleSchema(
            "project purge ownership policy requires schema 48".into(),
        ));
    }
    static EXPECTED: OnceLock<Result<Definitions, String>> = OnceLock::new();
    let expected = EXPECTED.get_or_init(|| {
        let build = || -> Result<Definitions, StorageError> {
            // This is an empty reference, never a user store. Rebuild migrations can
            // drop their empty parent tables; the live purge keeps foreign keys on.
            let reference = db(Connection::open_in_memory())?;
            super::guard::register(&reference, false)?;
            for migration in MIGRATIONS {
                db(reference.execute_batch(migration.sql))?;
            }
            definitions(&reference)
        };
        build().map_err(|_| "cannot establish the reviewed purge schema".into())
    });
    let expected = expected
        .as_ref()
        .map_err(|message| StorageError::IncompatibleSchema(message.clone()))?;
    if definitions(conn)? != *expected {
        return Err(StorageError::IncompatibleSchema(
            "project purge requires the exact reviewed persistent schema".into(),
        ));
    }
    Ok(())
}
