//! Finite native minimal projections. Unknown scrub shapes never get an inferred default.
use super::super::conn::db;
use super::invalid;
use devworks_core::ports::storage::{StorageError, StoredSchemaSource};
use devworks_core::protocol::{ActorId, Id, ProjectId, Revision};
use rusqlite::{Connection, params_from_iter, types::Value};

pub(super) fn scrub(
    conn: &Connection,
    target: &super::logical::Target,
    project: &ProjectId,
    human: &ActorId,
    now: i64,
    kept_version: Option<&Id>,
) -> Result<(), StorageError> {
    let table = target.row.table.as_str();
    let predicate = &target.predicate;
    let keys = &target.keys;
    let mut bindings = keys.to_vec();
    let mut bind = |v: Value| {
        bindings.push(v);
        format!("?{}", bindings.len())
    };
    let now = bind(Value::Integer(now));
    let human = bind(Value::Text(human.0.as_str().into()));
    let zero_digest = "0".repeat(64);
    let assignments = match table {
        "projects" => "title='Deleted project',lifecycle='Deleted',revision=revision+1,scope_revision=scope_revision+1".into(),
        "project_details" => format!("description='',updated_at={now},archive_stop_pending=0"),
        "work_items" => "title='Deleted work',brief_json='{}',priority=0,manual_order=0,parent_feature_id=NULL,associated_feature_id=NULL,revision=revision+1,scope_revision=scope_revision+1".into(),
        "workflow_statuses" => "name='Deleted status',sort_order=0,revision=revision+1".into(),
        "events" => "root_id=NULL,operation_id=NULL,aggregate_kind='Purged',aggregate_id='purged',kind='ProjectContentPurged',payload_json='{}'".into(),
        "operations" => format!("root_id=NULL,assignment_id=NULL,args_digest='{zero_digest}',payload_json='{{}}',revision=revision+1"),
        "sessions" => format!("root_id=NULL,title='Deleted session',pin_json=NULL,context_epoch=context_epoch+1,revision=revision+1,closed_at=COALESCE(closed_at,{now}),closed_by=COALESCE(closed_by,{human}),checkpoint_seq=0"),
        "conversations" => "root_id=NULL".into(),
        "assignments" => "parent_id=NULL,execution_id=NULL,state='Stopped',slot_held=0,generation=generation+1,revision=revision+1,checkpoint_json=NULL,scope_key=NULL,scope_digest=NULL,plan_revision=NULL,instruction_revision=0".into(),
        "resources" => format!("name='Deleted resource',canonical_identity='purged:'||id,labels_json='[]',revision=revision+1,fence_seq=fence_seq+1,queue_seq=queue_seq+1,updated_at={now}"),
        "blockers" => format!("root_id=NULL,assignment_id=NULL,operation_id=NULL,cause='Purged',origin_id='purged',origin_revision=origin_revision+1,state='Cleared',revision=revision+1,clearance_json='{{}}',reason='',cleared_at=COALESCE(cleared_at,{now}),cleared_by=COALESCE(cleared_by,{human})"),
        "artifacts" => {
            let head = kept_version.ok_or_else(||invalid("purged artifact has no exact kept version"))?;
            let head = bind(Value::Text(head.as_str().into()));
            format!("title='Deleted artifact',provenance_json='{{}}',current_version_id={head},state='Live',revision=revision+1,scope_revision=scope_revision+1,reference_revision=reference_revision+1")
        }
        "index_runs" => format!("stage='purged',owner=NULL,lease_until=NULL,config_digest='{zero_digest}',source_oid=NULL,generation_id=NULL,output_view_id=NULL,error_code=NULL,error_message=NULL,warnings_json='[]',revision=revision+1"),
        "authority_grants" => format!("binding_json='{{\"operation\":\"projects.purge\"}}',state='Revoked',revision=revision+1,uses_remaining=0,expires_at={now},revoked_by={human}"),
        "principal_authority" => format!("role=NULL,tool_families_json=NULL,state='Revoked',revision=revision+1,updated_at={now}"),
        "github_issue_intake" => "work_id=NULL,operation_id=NULL,remote_json='{}',revision=revision+1".into(),
        "project_managed_files" => "state='Removed',relative_path='',identity_json='{}',byte_count=0".into(),
        "schema_sources" => {
            let raw: String = db(conn.query_row(&format!("SELECT body_json FROM schema_sources WHERE {predicate}"),params_from_iter(keys.iter()), |r|r.get(0)))?;
            let mut source: StoredSchemaSource = serde_json::from_str(&raw).map_err(|_|invalid("purge source membership body"))?;
            let access = source.connection.config.as_mut().and_then(|c|c.project_access.as_mut()).ok_or_else(||invalid("purge source membership"))?;
            access.0.retain(|a|a.project_id != project.0);
            source.connection.revision = Revision::new(source.connection.revision.get().checked_add(1).ok_or_else(||invalid("purge source revision overflow"))?).map_err(|_|invalid("purge source revision"))?;
            let body = bind(Value::Text(serde_json::to_string(&source).map_err(|_|invalid("purge source body"))?));
            format!("revision=revision+1,body_json={body}")
        }
        _ => return Err(invalid("unmapped native purge scrub projection")),
    };
    // Numbered parameters include two optional static-projection arguments. SQLite permits
    // unused numbered slots, keeping predicates and projections on one explicit binding list.
    let sql = format!(
        "UPDATE {} SET {assignments} WHERE {predicate}",
        super::candidates::quote(table)
    );
    let mut statement = db(conn.prepare(&sql))?;
    for (index, value) in bindings.iter().enumerate() {
        if index < statement.parameter_count() {
            db(statement.raw_bind_parameter(index + 1, value))?;
        }
    }
    if db(statement.raw_execute())? != 1 {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}
