//! Reject unreviewed persistent owning shapes before preparing any deletion authority.
use super::super::conn::db;
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::storage::StorageError;
use rusqlite::Connection;

type Definitions = Vec<(String, String, String, Option<String>)>;

/// SHA-256 of the persistent schema (every `main.sqlite_schema` row) whose ownership the project
/// purge policy documents under `docs/architecture/validation/project-purge/` were reviewed
/// against. A test rebuilds that schema from `MIGRATIONS` and fails when it no longer matches, so
/// any schema change, including a new column on an existing table, needs a deliberate policy
/// review before purge accepts it.
///
/// Review notes for changes without their own policy document: migration 48 rebuilds
/// session_context_epochs to add MembershipHandoff to its reason constraint; it adds no table or
/// ownership edge, and its rows keep their session owner. Migration 50 adds watchers and
/// watcher_events, both owned by their project (schema50 extension); a watcher outside any
/// project is never a candidate. Migration 51 rebuilds watchers to add probe and timer sources;
/// it adds no table or ownership edge.
pub(super) const REVIEWED_SCHEMA_SHA256: &str =
    "93f9fa4ee3871dd8676afa86f76fcbc28caff1833bf2e8b84b0d34deb1fa22f9";

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

fn schema_digest(conn: &Connection) -> Result<String, StorageError> {
    let encoded = serde_json::to_string(&definitions(conn)?)
        .map_err(|_| StorageError::IncompatibleSchema("cannot encode the store schema".into()))?;
    Ok(sha256_hex(encoded.as_bytes()))
}

pub(super) fn verify(conn: &Connection) -> Result<(), StorageError> {
    // A later build must extend the ownership policy deliberately, rather than blessing a newly
    // migrated owning table or column merely because it is in MIGRATIONS.
    if schema_digest(conn)? != REVIEWED_SCHEMA_SHA256 {
        return Err(StorageError::IncompatibleSchema(
            "project purge requires the exact reviewed persistent schema".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::super::migrations::MIGRATIONS;
    use super::*;

    #[test]
    fn reviewed_schema_digest_matches_the_migrated_reference_schema() {
        // An empty reference, never a user store. Rebuild migrations can drop their empty parent
        // tables; the live purge keeps foreign keys on.
        let reference = Connection::open_in_memory().unwrap();
        super::super::guard::register(&reference, false).unwrap();
        for migration in MIGRATIONS {
            reference.execute_batch(migration.sql).unwrap();
        }
        let actual = schema_digest(&reference).unwrap();
        assert_eq!(
            actual, REVIEWED_SCHEMA_SHA256,
            "the schema built from MIGRATIONS changed, so project purge refuses every store. \
             Review each new or changed table, column, index and trigger against the purge \
             ownership policy (docs/architecture/validation/project-purge/ and the policy \
             documents loaded in project_purge/classification.rs), add a policy document for any \
             new owning shape, then set REVIEWED_SCHEMA_SHA256 in \
             crates/devworks-adapters/src/storage/sqlite/project_purge/schema.rs to {actual}."
        );
    }
}
