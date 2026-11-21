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
/// it adds no table or ownership edge. Migration 52 drops thirty tables no build wrote, with
/// their policy rows and purge branches; rebuilds feature_plans and transcript_chunks without the
/// two always-NULL columns that referenced dropped tables, adding no ownership edge; and adds the
/// operations_capability_state index. Planner statistics tables (`sqlite_stat*`, created by the
/// `PRAGMA optimize` that clean shutdown runs) hold no project rows and are not part of the digest.
/// Migration 53 rebuilds watchers (match and file columns) and watcher_events (several events per
/// watcher, match_count); both keep their project owner and add no table or ownership edge, and a
/// project's purge still removes all of their rows. Migration 54 adds read indexes (usage
/// attempts, origins and router requests by time, open attempt balances, live allocation
/// claims, work items by status); four tally columns on knowledge_claims with the triggers that
/// keep them current from knowledge_deliveries, knowledge_lesson_events and
/// knowledge_challenges, which only update the claim those rows already belong to; and moves
/// each outgoing MCP server's catalog into its own configuration_revisions namespace in the same
/// scope. It adds no table or ownership edge: the tallies are columns of their claim, and the
/// catalogs are configuration rows of the scope that already owns the server. Migration 55 adds
/// two partial indexes, owned_process_root_unsettled and owned_containers_root_agent_work; it
/// adds no table, column or ownership edge, and both tables keep their purge policy. Migration 56
/// adds the nullable default_ref column to repository_sources; the row keeps its owner, and the
/// migration adds no table or ownership edge. Migration 57 drops session_usage and
/// job_run_usage, with their policy rows, after copying their items into usage_origins; adds
/// usage_origins.committed_seq, four usage_origins indexes and the recreated update trigger.
/// usage_origins keeps its project owner (`project_id`), so a project's purge removes the copied
/// rows with it, and adds no table or ownership edge. Migration 58 adds graph_view_links and
/// graph_view_link_sets (schema58 extension): derived rows of an overlay index view, preserved
/// with every other repository index row; they hold no project rows and add no ownership edge.
/// Migration 59 adds the partial index graph_edges_target_qualified on graph_edges; it adds no
/// table, column or ownership edge, and graph_edges keeps its purge policy. Migration 60 adds the
/// graph_nodes_by_id index and the partial graph_nodes_project_directory index on graph_nodes; it
/// adds no table, column or ownership edge, and graph_nodes keeps its purge policy. Migration 61
/// adds graph_view_unresolved (schema61 extension), a derived count per one-layer index view,
/// preserved with every other repository index row, and the partial graph_edges_named_repository
/// index; they hold no project rows and add no ownership edge. Migration 62 adds
/// instruction_revision_lessons (schema62 extension), the links from a skill or role revision to
/// the lessons it came from. A link follows both of its ends: it is deleted with a project when
/// the revision is the project's (a project skill, or a draft confined to the project) or the
/// lesson is (a claim of the project), and it is never a candidate otherwise. It adds one
/// foreign key to instruction_revisions and one to knowledge_claims, and no other table changes.
/// Migration 63 adds file scan progress to watchers. The watcher retains its existing project
/// and session ownership. The new column adds no reference to another row.
/// Migration 64 adds analysis_runs and its four indexes. Project runs follow their recorded
/// scope key. Workspace runs and ordinary repository index views keep their independent ownership.
/// Migration 65 adds database_sensitive_policies. Rules follow the workspace or shared schema
/// source. Project purge removes only the project's existing access to that shared source.
pub(super) const REVIEWED_SCHEMA_SHA256: &str =
    "be1704e7d4244eef2f771fe2bd5c1f9cdb9d8bd8d8649283d47bea31fef0be77";

fn definitions(conn: &Connection) -> Result<Definitions, StorageError> {
    let mut statement = db(conn.prepare(
        "SELECT type,name,tbl_name,sql FROM main.sqlite_schema WHERE name NOT LIKE 'sqlite_stat%' \
         ORDER BY type,name,tbl_name",
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

    #[test]
    fn schema_baseline_lists_only_tables_columns_foreign_keys_and_triggers_the_migrated_schema_has()
    {
        // schema-baseline.json is the frozen schema 35 inventory that every later policy
        // document extends. Later migrations add to the schema, so the baseline may list fewer
        // entries than the migrated schema, but it must not list a table, column, foreign key or
        // trigger a migration dropped.
        let reference = Connection::open_in_memory().unwrap();
        super::super::guard::register(&reference, false).unwrap();
        for migration in MIGRATIONS {
            reference.execute_batch(migration.sql).unwrap();
        }
        let baseline: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/architecture/validation/project-purge/schema-baseline.json"
        )))
        .unwrap();
        let mut stale = Vec::new();
        for table in baseline["tables"].as_array().unwrap() {
            let name = table["name"].as_str().unwrap();
            let live: std::collections::BTreeSet<String> = reference
                .prepare(&format!("SELECT name FROM pragma_table_xinfo('{name}')"))
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            if live.is_empty() {
                stale.push(format!("table {name}"));
                continue;
            }
            for column in table["columns"].as_array().unwrap() {
                let column = column.as_str().unwrap();
                if !live.contains(column) {
                    stale.push(format!("column {name}.{column}"));
                }
            }
            for key in table["foreignKeys"].as_array().unwrap() {
                let from = key["from"].as_str().unwrap();
                let target = key["target"].as_str().unwrap();
                let target_exists: i64 = reference
                    .query_row(
                        "SELECT count(*) FROM main.sqlite_schema WHERE type='table' AND name=?1",
                        [target],
                        |row| row.get(0),
                    )
                    .unwrap();
                if target_exists == 0 || !live.contains(from) {
                    stale.push(format!("foreign key {name}.{from} -> {target}"));
                }
            }
        }
        for trigger in baseline["triggers"].as_array().unwrap() {
            let name = trigger["name"].as_str().unwrap();
            let exists: i64 = reference
                .query_row(
                    "SELECT count(*) FROM main.sqlite_schema WHERE type='trigger' AND name=?1",
                    [name],
                    |row| row.get(0),
                )
                .unwrap();
            if exists == 0 {
                stale.push(format!("trigger {name}"));
            }
        }
        assert!(
            stale.is_empty(),
            "schema-baseline.json lists schema objects the migrations dropped; remove them from \
             docs/architecture/validation/project-purge/schema-baseline.json: {stale:#?}"
        );
    }
}
