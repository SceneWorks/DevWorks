//! Scoped surviving-reference evidence. Foreign keys retain identities; they do
//! not authorize removing a referring row or expanding the ownership selection.
use super::super::conn::db;
use super::candidates::{key_value, primary_keys, quote};
use devworks_core::ports::storage::{
    ProjectPurgeReference, ProjectPurgeReferenceKind, StorageError,
};
use devworks_core::protocol::ProjectId;
use rusqlite::Connection;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub(super) const ARTIFACT_SUBJECT_TARGETS: &[(&str, &str)] = &[
    ("root", "work_items"),
    ("assignment", "assignments"),
    ("operation", "operations"),
    ("resource", "resources"),
    ("blocker", "blockers"),
    ("session", "sessions"),
];

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

struct Selection<'a> {
    project: &'a ProjectId,
    scope_digest: &'a str,
    anchors: &'a str,
    predicates: &'a Map<String, Value>,
}
struct ReferenceSpec {
    kind: ProjectPurgeReferenceKind,
    from_table: String,
    target: String,
    source: String,
    join: String,
    from_columns: Vec<String>,
    to_columns: Vec<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProcessingIntent {
    action_key: String,
    operation_id: devworks_core::protocol::Id,
}
fn validate_processing_intents(conn: &Connection) -> Result<(), StorageError> {
    // A newer/legacy producer may have copied arguments here. Its ownership
    // cannot be inferred from an operationId while silently ignoring that body.
    let mut statement = db(conn.prepare("SELECT intents_json FROM package_handler_commits"))?;
    let mut rows = db(statement.query([]))?;
    while let Some(row) = db(rows.next())? {
        let encoded = db(row.get::<_, String>(0))?;
        let intents: Vec<ProcessingIntent> = serde_json::from_str(&encoded)
            .map_err(|_| invalid("unclassified package processing intent"))?;
        if intents.iter().any(|intent| {
            intent.action_key.trim().is_empty()
                || devworks_core::protocol::Id::new(intent.operation_id.as_str()).is_err()
        }) {
            return Err(invalid("unclassified package processing intent"));
        }
    }
    Ok(())
}
fn collect(
    conn: &Connection,
    selection: &Selection<'_>,
    spec: ReferenceSpec,
    result: &mut Vec<ProjectPurgeReference>,
) -> Result<(), StorageError> {
    let Some(predicate) = selection.predicates.get(&spec.target) else {
        return Ok(());
    };
    let predicate = predicate
        .as_str()
        .ok_or_else(|| invalid("purge reference predicate"))?;
    let from_keys = primary_keys(conn, &spec.from_table)?;
    let to_keys = primary_keys(conn, &spec.target)?;
    let source_projection = keys_projection("src", &from_keys, "rowid");
    let target_projection = keys_projection("dst", &to_keys, "__purge_rowid");
    let mut projection = source_projection.clone();
    projection.extend(target_projection.iter().cloned());
    let target_rows = if to_keys.is_empty() {
        "rowid AS __purge_rowid,*"
    } else {
        "*"
    };
    let sql = format!(
        "WITH {} SELECT {} FROM {} JOIN (SELECT {target_rows} FROM {} WHERE ({predicate})) AS dst ON {}",
        selection.anchors,
        projection.join(","),
        spec.source,
        quote(&spec.target),
        spec.join
    );
    let mut statement = db(conn.prepare(&sql))?;
    let mut rows = db(statement.query([selection.project.0.as_str(), selection.scope_digest]))?;
    while let Some(row) = db(rows.next())? {
        result.push(ProjectPurgeReference {
            kind: spec.kind,
            from_table: spec.from_table.clone(),
            from_key_digest: key_hash(row, 0, source_projection.len())?,
            from_columns: spec.from_columns.clone(),
            to_table: spec.target.clone(),
            to_key_digest: key_hash(row, source_projection.len(), target_projection.len())?,
            to_columns: spec.to_columns.clone(),
        });
    }
    Ok(())
}
fn typed_references(
    conn: &Connection,
    selection: &Selection<'_>,
    result: &mut Vec<ProjectPurgeReference>,
) -> Result<(), StorageError> {
    validate_processing_intents(conn)?;
    // These are the six closed SubjectKey variants used by native artifact links.
    // Their targets remain candidates: an audit link is not a grant to retain
    // executable/private state or to remove its independently owned source.
    for &(kind, target) in ARTIFACT_SUBJECT_TARGETS {
        collect(
            conn,
            selection,
            ReferenceSpec {
                kind: ProjectPurgeReferenceKind::ArtifactSubject,
                from_table: "artifact_links".into(),
                target: target.into(),
                source: "artifact_links AS src".into(),
                join: format!("src.subject_id=dst.id AND src.subject_kind='{kind}'"),
                from_columns: vec!["subject_kind".into(), "subject_id".into()],
                to_columns: vec!["id".into()],
            },
            result,
        )?;
    }
    // Native version provenance names the staging journal even after a version is moved
    // or shared outside its original project. Scope validation belongs to the classifier.
    collect(conn, selection, ReferenceSpec {
        kind: ProjectPurgeReferenceKind::ArtifactVersionOperation,
        from_table: "artifact_versions".into(), target: "operations".into(),
        source: "artifact_versions AS src".into(),
        join: "json_type(src.source_json,'$.operationId')='text' AND json_extract(src.source_json,'$.operationId')=dst.id AND dst.capability='artifacts.stage_file'".into(),
        from_columns: vec!["source_json".into()], to_columns: vec!["id".into()],
    }, result)?;
    for (kind, from, target, column) in [
        (
            ProjectPurgeReferenceKind::IndexOverlayRun,
            "index_overlays",
            "index_runs",
            "run_id",
        ),
        (
            ProjectPurgeReferenceKind::SearchChunk,
            "search_fts",
            "search_chunks",
            "chunk_id",
        ),
    ] {
        collect(
            conn,
            selection,
            ReferenceSpec {
                kind,
                from_table: from.into(),
                target: target.into(),
                source: format!("{} AS src", quote(from)),
                join: format!("src.{}=dst.id", quote(column)),
                from_columns: vec![column.into()],
                to_columns: vec!["id".into()],
            },
            result,
        )?;
    }
    collect(
        conn,
        selection,
        ReferenceSpec {
            kind: ProjectPurgeReferenceKind::PackageIntent,
            from_table: "package_handler_commits".into(),
            target: "operations".into(),
            source: "package_handler_commits AS src JOIN json_each(src.intents_json) AS intent"
                .into(),
            join: "json_extract(intent.value,'$.operationId')=dst.id".into(),
            from_columns: vec!["intents_json".into()],
            to_columns: vec!["id".into()],
        },
        result,
    )?;
    collect(
        conn,
        selection,
        ReferenceSpec {
            kind: ProjectPurgeReferenceKind::SchemaCertificate,
            from_table: "schema_sources".into(),
            target: "artifact_versions".into(),
            source: "schema_sources AS src".into(),
            join:
                "json_extract(src.body_json,'$.connection.config.caCertificate.versionId')=dst.id"
                    .into(),
            from_columns: vec!["body_json".into()],
            to_columns: vec!["id".into()],
        },
        result,
    )?;
    collect(
        conn,
        selection,
        ReferenceSpec {
            kind: ProjectPurgeReferenceKind::SchemaSourceOperation,
            from_table: "schema_sources".into(),
            target: "operations".into(),
            source: "schema_sources AS src".into(),
            join: "json_extract(src.body_json,'$.connection.activeOperationId')=dst.id".into(),
            from_columns: vec!["body_json".into()],
            to_columns: vec!["id".into()],
        },
        result,
    )?;
    Ok(())
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
    let selection = Selection {
        project,
        scope_digest,
        anchors,
        predicates,
    };
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
            if !predicates.contains_key(&foreign_key.target) {
                continue;
            }
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
            let join = foreign_key
                .columns
                .iter()
                .zip(&to_columns)
                .map(|((from, _), to)| format!("src.{}=dst.{}", quote(from), quote(to)))
                .collect::<Vec<_>>()
                .join(" AND ");
            collect(
                conn,
                &selection,
                ReferenceSpec {
                    kind: ProjectPurgeReferenceKind::ForeignKey,
                    from_table: from_table.clone(),
                    target: foreign_key.target,
                    source: format!("{} AS src", quote(&from_table)),
                    join,
                    from_columns: foreign_key
                        .columns
                        .into_iter()
                        .map(|(from, _)| from)
                        .collect(),
                    to_columns,
                },
                &mut result,
            )?;
        }
    }
    typed_references(conn, &selection, &mut result)?;
    result.sort();
    result.dedup();
    Ok(result)
}
