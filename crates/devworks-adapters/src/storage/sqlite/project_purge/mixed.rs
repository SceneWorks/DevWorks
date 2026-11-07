//! Explicit mixed-owner metadata; source text and opaque names never grant scope.
use super::super::conn::db;
use super::classification::{Key, assign};
use devworks_core::ports::storage::{
    ProjectPurgeCandidates, ProjectPurgeRowAction as Action, ProjectPurgeRowReason as Reason,
    ProjectPurgeRowWitness, ProjectPurgeUnresolved, StorageError,
};
use devworks_core::protocol::ProjectId;
use rusqlite::Connection;
use std::collections::BTreeMap;

pub(super) fn source_access(
    conn: &Connection,
    project: &ProjectId,
    snapshot: &ProjectPurgeCandidates,
    rows: &mut BTreeMap<Key, ProjectPurgeRowWitness>,
    unresolved: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> Result<(), StorageError> {
    use devworks_core::ports::storage::StoredSchemaSource;
    use devworks_core::protocol::Check;
    let selection = super::candidates::selection(project)?;
    let sql = selection.sql(
        "schema_sources",
        "id,owner_id,revision,configuration_revision,deleted,body_json",
        "1=1",
    )?;
    let mut statement = db(conn.prepare(&sql))?;
    let mut records = db(statement.query(selection.parameters()))?;
    while let Some(record) = db(records.next())? {
        let (id, owner, revision, configuration, deleted, encoded) = (
            db(record.get::<_, String>(0))?,
            db(record.get::<_, String>(1))?,
            db(record.get::<_, i64>(2))?,
            db(record.get::<_, i64>(3))?,
            db(record.get::<_, bool>(4))?,
            db(record.get::<_, String>(5))?,
        );
        let Ok(raw) = serde_json::from_str::<serde_json::Value>(&encoded) else {
            continue;
        };
        let Ok(source) = serde_json::from_value::<StoredSchemaSource>(raw.clone()) else {
            continue;
        };
        if source.connection.check().is_err()
            || source.connection.id.as_str() != id
            || source.owner_id.0.as_str() != owner
            || source.connection.owner_id.as_ref() != Some(&source.owner_id)
            || source.connection.revision.get() != revision
            || source.configuration_revision != configuration
            || source.deleted != deleted
            || serde_json::to_value(&source).ok() != Some(raw)
            || !source.connection.config.as_ref().is_some_and(|c| {
                c.project_access
                    .as_ref()
                    .is_some_and(|a| a.0.iter().any(|a| a.project_id == project.0))
            })
        {
            continue;
        }
        let key =
            super::guard::key_digest(&serde_json::to_string(&[id]).expect("source access key"))?;
        if let Some(candidate) = snapshot
            .rows
            .iter()
            .find(|r| r.table == "schema_sources" && r.key_digest == key)
        {
            assign(
                candidate,
                Action::Scrub,
                Reason::ProjectMembership,
                rows,
                unresolved,
            );
        }
    }
    Ok(())
}

pub(super) fn managed_files(
    conn: &Connection,
    project: &ProjectId,
    snapshot: &ProjectPurgeCandidates,
    rows: &mut BTreeMap<Key, ProjectPurgeRowWitness>,
    unresolved: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> Result<(), StorageError> {
    use devworks_core::ports::project_purge::{PurgeFileOwner, PurgeFileWitness};
    let selection = super::candidates::selection(project)?;
    let sql = selection.sql(
        "project_managed_files",
        "id,kind,relative_path,identity_json,byte_count,state",
        "1=1",
    )?;
    let mut statement = db(conn.prepare(&sql))?;
    let mut records = db(statement.query(selection.parameters()))?;
    while let Some(record) = db(records.next())? {
        let (id, kind, path, encoded, bytes, state) = (
            db(record.get::<_, String>(0))?,
            db(record.get::<_, String>(1))?,
            db(record.get::<_, String>(2))?,
            db(record.get::<_, String>(3))?,
            db(record.get::<_, i64>(4))?,
            db(record.get::<_, String>(5))?,
        );
        if !matches!(
            kind.as_str(),
            "DiagnosticExport" | "ClientPaste" | "Preview"
        ) {
            continue;
        }
        let Ok(raw) = serde_json::from_str::<serde_json::Value>(&encoded) else {
            continue;
        };
        let action = if state == "Removed" && path.is_empty() && raw == serde_json::json!({}) {
            Action::Delete
        } else if matches!(state.as_str(), "Live" | "Removing") {
            let Ok(witness) = serde_json::from_value::<PurgeFileWitness>(raw.clone()) else {
                continue;
            };
            let owner = matches!((&*kind,&witness.owner),("DiagnosticExport",PurgeFileOwner::DiagnosticExport{file_id})|("ClientPaste",PurgeFileOwner::ClientPaste{file_id}) if file_id.as_str()==id);
            let normal = std::path::Path::new(&path);
            if !owner
                || witness.project_id != *project
                || witness.relative_entry != path
                || i64::try_from(witness.byte_count).ok() != Some(bytes)
                || witness.restore_epoch < 0
                || witness.directory
                || witness.link
                || serde_json::to_value(&witness).ok() != Some(raw)
                || devworks_core::fingerprint::Fingerprint::from_hex(&witness.content_digest)
                    .is_none()
                || path.contains('\\')
                || normal.components().count() != 1
                || !matches!(
                    normal.components().next(),
                    Some(std::path::Component::Normal(_))
                )
            {
                continue;
            }
            // This resolves only database attribution. Actual roots, contents,
            // store epoch and byte eligibility still require native inventory.
            Action::Scrub
        } else {
            continue;
        };
        let key =
            super::guard::key_digest(&serde_json::to_string(&[id]).expect("managed file key"))?;
        if let Some(candidate) = snapshot
            .rows
            .iter()
            .find(|r| r.table == "project_managed_files" && r.key_digest == key)
        {
            assign(candidate, action, Reason::DerivedContent, rows, unresolved);
        }
    }
    Ok(())
}

pub(super) fn intake(
    conn: &Connection,
    project: &ProjectId,
    snapshot: &ProjectPurgeCandidates,
    rows: &mut BTreeMap<Key, ProjectPurgeRowWitness>,
    unresolved: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> Result<(), StorageError> {
    use devworks_core::protocol::records::GithubIssueIntakeState;
    let selection = super::candidates::selection(project)?;
    let sql = selection.sql(
        "github_issue_intake",
        "repository_id,issue_number,state,project_id,work_id",
        "1=1",
    )?;
    let mut statement = db(conn.prepare(&sql))?;
    let mut records = db(statement.query(selection.parameters()))?;
    while let Some(record) = db(records.next())? {
        let (repo, number, state, owner, work) = (
            db(record.get::<_, String>(0))?,
            db(record.get::<_, i64>(1))?,
            db(record.get::<_, String>(2))?,
            db(record.get::<_, Option<String>>(3))?,
            db(record.get::<_, Option<String>>(4))?,
        );
        if serde_json::from_value::<GithubIssueIntakeState>(serde_json::Value::String(state))
            .is_err()
            || owner.as_deref().is_some_and(|p| p != project.0.as_str())
        {
            continue;
        }
        if let Some(work) = work {
            let owned: bool = db(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM work_items WHERE id=?1 AND project_id=?2)",
                [work.as_str(), project.0.as_str()],
                |r| r.get(0),
            ))?;
            if !owned {
                continue;
            }
        }
        let key = super::guard::key_digest(
            &serde_json::to_string(&serde_json::json!([repo, number])).expect("intake key"),
        )?;
        if let Some(candidate) = snapshot
            .rows
            .iter()
            .find(|r| r.table == "github_issue_intake" && r.key_digest == key)
        {
            assign(
                candidate,
                Action::Scrub,
                Reason::MinimalDeletedIdentity,
                rows,
                unresolved,
            );
        }
    }
    // These are association edges with no independent body. The frozen policy
    // removes an edge when its explicitly owned work or integration record goes.
    for candidate in selection.rows_where(conn, "integration_links", "1=1")? {
        assign(
            &candidate,
            Action::Delete,
            Reason::OwnedLineage,
            rows,
            unresolved,
        );
    }
    Ok(())
}

pub(super) fn grants(
    conn: &Connection,
    project: &ProjectId,
    snapshot: &ProjectPurgeCandidates,
    rows: &mut BTreeMap<Key, ProjectPurgeRowWitness>,
    unresolved: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> Result<(), StorageError> {
    use devworks_core::authority::service::GrantBinding;
    use devworks_core::fingerprint::Fingerprint;
    use devworks_core::registry::Mode;
    let selection = super::candidates::selection(project)?;
    let sql = selection.sql("authority_grants", "id,kind,binding_json", "1=1")?;
    let mut statement = db(conn.prepare(&sql))?;
    let mut records = db(statement.query(selection.parameters()))?;
    while let Some(record) = db(records.next())? {
        let (id, kind, encoded) = (
            db(record.get::<_, String>(0))?,
            db(record.get::<_, String>(1))?,
            db(record.get::<_, String>(2))?,
        );
        if !matches!(kind.as_str(), "Operation" | "NativeExecution") {
            continue;
        }
        let Ok(binding) = serde_json::from_str::<GrantBinding>(&encoded) else {
            continue;
        };
        if binding.project_id.as_ref().is_some_and(|p| p != project)
            || !devworks_core::registry::find(&binding.operation)
                .is_some_and(|d| d.binding(Mode::Command).is_some())
            || [&binding.request_digest, &binding.policy_digest]
                .into_iter()
                .flatten()
                .any(|d| Fingerprint::from_hex(d).is_none())
        {
            continue;
        }
        if let Some(root) = &binding.root_id {
            let owned: bool = db(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM work_items WHERE id=?1 AND project_id=?2)",
                [root.0.as_str(), project.0.as_str()],
                |r| r.get(0),
            ))?;
            if !owned {
                continue;
            }
        } else if binding.project_id.as_ref() != Some(project) {
            continue;
        }
        let key = super::guard::key_digest(&serde_json::to_string(&[id]).expect("grant key"))?;
        if let Some(candidate) = snapshot
            .rows
            .iter()
            .find(|r| r.table == "authority_grants" && r.key_digest == key)
        {
            assign(
                candidate,
                Action::Scrub,
                Reason::RevokedAuthority,
                rows,
                unresolved,
            );
        }
    }
    Ok(())
}

pub(super) fn source_heads(
    conn: &Connection,
    project: &ProjectId,
    snapshot: &ProjectPurgeCandidates,
    rows: &mut BTreeMap<Key, ProjectPurgeRowWitness>,
    unresolved: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> Result<(), StorageError> {
    let selection = super::candidates::selection(project)?;
    let sql = selection.sql("knowledge_source_heads", "source_ref", "1=1")?;
    let mut statement = db(conn.prepare(&sql))?;
    let mut records = db(statement.query(selection.parameters()))?;
    while let Some(record) = db(records.next())? {
        let source = db(record.get::<_, String>(0))?;
        let key_digest =
            super::guard::key_digest(&serde_json::to_string(&[&source]).expect("source head key"))?;
        let Some(candidate) = snapshot
            .rows
            .iter()
            .find(|r| r.table == "knowledge_source_heads" && r.key_digest == key_digest)
        else {
            continue;
        };
        let mut keeper = false;
        let mut known = false;
        for (table, projection) in [
            ("knowledge_claim_basis", "claim_id,source_ref"),
            ("knowledge_evidence", "id"),
        ] {
            let mut references = db(conn.prepare(&format!(
                "SELECT {projection} FROM {table} WHERE source_ref=?1"
            )))?;
            let mut references = db(references.query([&source]))?;
            while let Some(reference) = db(references.next())? {
                known = true;
                let mut tuple = vec![db(reference.get::<_, String>(0))?];
                if table == "knowledge_claim_basis" {
                    tuple.push(db(reference.get::<_, String>(1))?);
                }
                let key = (
                    table.into(),
                    super::guard::key_digest(
                        &serde_json::to_string(&tuple).expect("source reference key"),
                    )?,
                );
                keeper |= rows
                    .get(&key)
                    .is_none_or(|row| row.action != Action::Delete)
                    || unresolved.contains_key(&key);
            }
        }
        if known {
            assign(
                candidate,
                if keeper { Action::Keep } else { Action::Delete },
                if keeper {
                    Reason::SharedContent
                } else {
                    Reason::OwnedLineage
                },
                rows,
                unresolved,
            );
        }
    }
    Ok(())
}
