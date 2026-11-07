//! Scoped surviving-reference evidence. Foreign keys retain identities; they do
//! not authorize removing a referring row or expanding the ownership selection.
use super::super::conn::db;
use super::candidates::{key_value, primary_keys, quote};
use devworks_core::ports::storage::{ProjectPurgeReference, StorageError};
use devworks_core::protocol::ProjectId;
use rusqlite::Connection;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
struct ForeignKey {
    target: String,
    columns: Vec<(String, Option<String>)>,
}
fn keys_projection(alias: &str, keys: &[String], rowid: &str) -> Vec<String> {
    if keys.is_empty() {
        vec![format!("{alias}.{rowid}")]
    } else {
        keys.iter()
            .map(|key| format!("{alias}.{}", quote(key)))
            .collect()
    }
}
fn key_hash(row: &rusqlite::Row<'_>, start: usize, count: usize) -> Result<String, StorageError> {
    let values = (start..start + count)
        .map(|index| key_value(db(row.get_ref(index))?))
        .collect::<Result<Vec<_>, _>>()?;
    super::guard::key_digest(
        &serde_json::to_string(&values).map_err(|_| invalid("purge reference key encoding"))?,
    )
}

pub(super) fn snapshot(
    conn: &Connection,
    project: &ProjectId,
    scope_digest: &str,
    anchors: &str,
    predicates: &Map<String, Value>,
) -> Result<Vec<ProjectPurgeReference>, StorageError> {
    let mut tables =
        db(conn.prepare("SELECT name FROM main.sqlite_schema WHERE type='table' ORDER BY name"))?;
    let tables = db(tables.query_map([], |row| row.get::<_, String>(0)))?
        .map(db)
        .collect::<Result<Vec<_>, _>>()?;
    let mut result = Vec::new();
    for from_table in tables {
        let mut statement = db(conn.prepare(
            "SELECT id,\"table\",\"from\",\"to\" FROM pragma_foreign_key_list(?1) ORDER BY id,seq",
        ))?;
        let mut foreign_keys = BTreeMap::<i64, ForeignKey>::new();
        let mut rows = db(statement.query([&from_table]))?;
        while let Some(row) = db(rows.next())? {
            let id = db(row.get::<_, i64>(0))?;
            let target = db(row.get::<_, String>(1))?;
            let from = db(row.get::<_, String>(2))?;
            let to = db(row.get::<_, Option<String>>(3))?;
            foreign_keys
                .entry(id)
                .or_insert_with(|| ForeignKey {
                    target,
                    columns: Vec::new(),
                })
                .columns
                .push((from, to));
        }
        for foreign_key in foreign_keys.into_values() {
            let Some(predicate) = predicates.get(&foreign_key.target) else {
                continue;
            };
            let predicate = predicate
                .as_str()
                .ok_or_else(|| invalid("purge reference predicate"))?;
            let from_keys = primary_keys(conn, &from_table)?;
            let to_keys = primary_keys(conn, &foreign_key.target)?;
            let mut to_columns = Vec::new();
            for (index, (_, column)) in foreign_key.columns.iter().enumerate() {
                to_columns.push(match column {
                    Some(column) => column.clone(),
                    None => to_keys
                        .get(index)
                        .cloned()
                        .ok_or_else(|| invalid("unclassified implicit purge reference"))?,
                });
            }
            let source_projection = keys_projection("src", &from_keys, "rowid");
            let target_projection = keys_projection("dst", &to_keys, "__purge_rowid");
            let mut projection = source_projection.clone();
            projection.extend(target_projection.iter().cloned());
            let join = foreign_key
                .columns
                .iter()
                .zip(&to_columns)
                .map(|((from, _), to)| format!("src.{}=dst.{}", quote(from), quote(to)))
                .collect::<Vec<_>>()
                .join(" AND ");
            let target_rows = if to_keys.is_empty() {
                "rowid AS __purge_rowid,*"
            } else {
                "*"
            };
            let sql = format!(
                "WITH {anchors} SELECT {} FROM {} AS src JOIN (SELECT {target_rows} FROM {} WHERE ({predicate})) AS dst ON {join}",
                projection.join(","),
                quote(&from_table),
                quote(&foreign_key.target)
            );
            let mut statement = db(conn.prepare(&sql))?;
            let mut rows = db(statement.query([project.0.as_str(), scope_digest]))?;
            while let Some(row) = db(rows.next())? {
                result.push(ProjectPurgeReference {
                    from_table: from_table.clone(),
                    from_key_digest: key_hash(row, 0, source_projection.len())?,
                    from_columns: foreign_key
                        .columns
                        .iter()
                        .map(|(from, _)| from.clone())
                        .collect(),
                    to_table: foreign_key.target.clone(),
                    to_key_digest: key_hash(row, source_projection.len(), target_projection.len())?,
                    to_columns: to_columns.clone(),
                });
            }
        }
    }
    result.sort();
    result.dedup();
    Ok(result)
}
