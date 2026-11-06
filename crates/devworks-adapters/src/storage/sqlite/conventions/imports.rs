//! Private per-entry captures over the existing artifact staging/blob lifecycle, never public
//! artifact rows until the entire convention import publishes atomically.
use super::*;
use devworks_core::protocol::{OperationId, ProjectId, RootId};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactData {
    id: Id,
    origin_project_id: Option<ProjectId>,
    origin_work_id: Option<RootId>,
    title: String,
    revision: i64,
    scope: String,
    owner: Option<ActorId>,
    retention: String,
    scope_revision: i64,
    reference_revision: i64,
    current_version_id: Option<Id>,
    state: String,
    created_at_ms: i64,
    provenance: Value,
}
impl From<&StoredArtifact> for ArtifactData {
    fn from(a: &StoredArtifact) -> Self {
        Self {
            id: a.id.clone(),
            origin_project_id: a.origin_project_id.clone(),
            origin_work_id: a.origin_work_id.clone(),
            title: a.title.clone(),
            revision: a.revision,
            scope: a
                .scope
                .project()
                .cloned()
                .map_or(KnowledgeScope::Workspace, KnowledgeScope::Project)
                .key(),
            owner: a.owner.clone(),
            retention: a.retention.as_str().into(),
            scope_revision: a.scope_revision,
            reference_revision: a.reference_revision,
            current_version_id: a.current_version_id.clone(),
            state: a.state.as_str().into(),
            created_at_ms: a.created_at_ms,
            provenance: a.provenance.clone(),
        }
    }
}
impl ArtifactData {
    fn stored(self) -> Result<StoredArtifact, StorageError> {
        let scope = KnowledgeScope::parse(&self.scope)?;
        Ok(StoredArtifact {
            id: self.id,
            origin_project_id: self.origin_project_id,
            origin_work_id: self.origin_work_id,
            title: self.title,
            revision: self.revision,
            scope: scope
                .project()
                .cloned()
                .map_or(ArtifactScope::Workspace, ArtifactScope::Project),
            owner: self.owner,
            retention: ArtifactRetention::parse(&self.retention)
                .ok_or_else(|| invalid("invalid draft artifact retention"))?,
            scope_revision: self.scope_revision,
            reference_revision: self.reference_revision,
            current_version_id: self.current_version_id,
            state: ArtifactState::parse(&self.state)
                .ok_or_else(|| invalid("invalid draft artifact state"))?,
            created_at_ms: self.created_at_ms,
            provenance: self.provenance,
        })
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionData {
    id: Id,
    artifact_id: Id,
    version_no: i64,
    blob_digest: String,
    byte_count: i64,
    blob_generation: String,
    media_type: String,
    created_at_ms: i64,
    deleted_at_ms: Option<i64>,
    source: Value,
}
impl From<&StoredVersion> for VersionData {
    fn from(v: &StoredVersion) -> Self {
        Self {
            id: v.id.clone(),
            artifact_id: v.artifact_id.clone(),
            version_no: v.version_no,
            blob_digest: v.blob_digest.clone(),
            byte_count: v.byte_count,
            blob_generation: v.blob_generation.clone(),
            media_type: v.media_type.clone(),
            created_at_ms: v.created_at_ms,
            deleted_at_ms: v.deleted_at_ms,
            source: v.source.clone(),
        }
    }
}
impl From<VersionData> for StoredVersion {
    fn from(v: VersionData) -> Self {
        Self {
            id: v.id,
            artifact_id: v.artifact_id,
            version_no: v.version_no,
            blob_digest: v.blob_digest,
            byte_count: v.byte_count,
            blob_generation: v.blob_generation,
            media_type: v.media_type,
            created_at_ms: v.created_at_ms,
            deleted_at_ms: v.deleted_at_ms,
            source: v.source,
        }
    }
}
fn encoded(value: &impl Serialize) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|_| invalid("invalid private import metadata"))
}
fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, StorageError> {
    serde_json::from_str(value).map_err(|_| invalid("invalid stored private import metadata"))
}
fn import_record(r: &Row<'_>) -> rusqlite::Result<StoredConventionImport> {
    Ok(StoredConventionImport {
        operation_id: OperationId(identifier(r, 0)?),
        convention_id: identifier(r, 1)?,
        scope: KnowledgeScope::parse(&r.get::<_, String>(2)?).map_err(|e| conversion(2, e))?,
        author_id: ActorId(identifier(r, 3)?),
        source_ref: value(r, 4)?,
        expected_scope_revision: r.get(5)?,
        revision: r.get(6)?,
        phase: r.get(7)?,
        checkpoint: value(r, 8)?,
        processed_resources: r.get(9)?,
        total_resources: r.get(10)?,
        error: r.get(11)?,
        created_at_ms: r.get(12)?,
        updated_at_ms: r.get(13)?,
    })
}
const IMPORT_COLUMNS: &str = "operation_id,convention_id,scope_key,author_id,source_ref_json,expected_scope_revision,revision,phase,checkpoint_json,processed_resources,total_resources,error,created_at,updated_at";
fn get(
    conn: &Connection,
    op: &OperationId,
) -> Result<Option<StoredConventionImport>, StorageError> {
    db(conn
        .query_row(
            &format!("SELECT {IMPORT_COLUMNS} FROM convention_imports WHERE operation_id=?1"),
            [op.0.as_str()],
            import_record,
        )
        .optional())
}
fn required(conn: &Connection, op: &OperationId) -> Result<StoredConventionImport, StorageError> {
    get(conn, op)?.ok_or_else(|| invalid("convention import missing"))
}
fn metadata(conn: &Connection, op: &OperationId) -> Result<Option<StoredConvention>, StorageError> {
    db(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM convention_import_metadata v WHERE v.operation_id=?1"),
            [op.0.as_str()],
            record,
        )
        .optional())
}
fn captures(
    conn: &Connection,
    op: &OperationId,
    after: Option<i64>,
    limit: u16,
) -> Result<Vec<StoredConventionImportArtifact>, StorageError> {
    if after.is_some_and(|n| n < 0) {
        return Err(invalid("negative import ordinal"));
    }
    let limit = page(limit)?;
    let mut stmt=db(conn.prepare_cached("SELECT ordinal,kind,resource_id,resource_key,resource_name,resource_lifecycle,resource_media_type,artifact_json,version_json,staging_operation_id,staging_attempt,staging_generation,managed_path FROM convention_import_artifacts WHERE operation_id=?1 AND ordinal>coalesce(?2,-1) ORDER BY ordinal LIMIT ?3"))?;
    let rows = db(
        db(stmt.query_map(params![op.0.as_str(), after, limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
                identifier(r, 9)?,
                r.get::<_, i64>(10)?,
                r.get::<_, Option<i64>>(11)?,
                r.get::<_, String>(12)?,
            ))
        }))?
        .collect::<rusqlite::Result<Vec<_>>>(),
    )?;
    rows.into_iter()
        .map(|r| {
            Ok(StoredConventionImportArtifact {
                operation_id: op.clone(),
                ordinal: r.0,
                kind: r.1,
                resource_id: r
                    .2
                    .map(Id::new)
                    .transpose()
                    .map_err(|_| invalid("invalid private resource identity"))?,
                resource_key: r.3,
                resource_name: r.4,
                resource_lifecycle: r.5,
                resource_media_type: r.6,
                artifact: decode::<ArtifactData>(&r.7)?.stored()?,
                version: decode::<VersionData>(&r.8)?.into(),
                staging_operation_id: OperationId(r.9),
                staging_fence: AttemptFence {
                    attempt_no: r.10,
                    owner_generation: r.11,
                },
                managed_path: r.12,
            })
        })
        .collect()
}
fn operation(conn: &Connection, op: &OperationId) -> Result<String, StorageError> {
    let r: Option<(String, String)> = db(conn
        .query_row(
            "SELECT capability,state FROM operations WHERE id=?1",
            [op.0.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional())?;
    match r {
        Some((cap, state)) if cap == "conventions.import" => Ok(state),
        _ => Err(invalid(
            "import requires actual conventions.import operation",
        )),
    }
}
fn active(conn: &Connection, op: &OperationId) -> Result<(), StorageError> {
    if matches!(
        operation(conn, op)?.as_str(),
        "Failed" | "NeedsInput" | "ObservedComplete" | "CancelledBeforeDispatch"
    ) {
        Err(invalid("terminal import cannot publish"))
    } else {
        Ok(())
    }
}
fn actual_source(conn: &Connection, reference: &Value) -> Result<ArtifactRef, StorageError> {
    let reference: ArtifactRef = serde_json::from_value(reference.clone())
        .map_err(|_| invalid("invalid import source reference"))?;
    let project: Option<Option<String>> = db(conn
        .query_row(
            "SELECT current_project_id FROM artifacts WHERE id=?1",
            [reference.artifact_id.as_str()],
            |r| r.get(0),
        )
        .optional())?;
    let Some(project) = project else {
        return Err(invalid("import source artifact missing"));
    };
    let scope = project
        .map(Id::new)
        .transpose()
        .map_err(|_| invalid("invalid source scope"))?
        .map(ProjectId)
        .map_or(KnowledgeScope::Workspace, KnowledgeScope::Project);
    backing(
        conn,
        &scope,
        &serde_json::to_value(reference).map_err(|_| invalid("invalid import source"))?,
    )
}
fn checkpoint(value: &Value) -> Result<(), StorageError> {
    if !value.is_object() || value.to_string().len() > 512 * 1024 {
        Err(invalid("invalid bounded import parser checkpoint"))
    } else {
        Ok(())
    }
}
fn target_exists(conn: &Connection, scope: &KnowledgeScope) -> Result<(), StorageError> {
    if let Some(project) = scope.project() {
        let exists: bool = db(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE id=?1 AND lifecycle!='Deleted')",
            [project.0.as_str()],
            |r| r.get(0),
        ))?;
        if !exists {
            return Err(invalid("import target project missing/deleted"));
        }
    }
    Ok(())
}
fn stage_proof(
    conn: &Connection,
    row: &StoredConventionImportArtifact,
    published: bool,
) -> Result<(), StorageError> {
    let draft = required(conn, &row.operation_id)?;
    let (cap, state, payload): (String, String, String) = db(conn.query_row(
        "SELECT capability,state,payload_json FROM operations WHERE id=?1",
        [row.staging_operation_id.0.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    ))?;
    let payload: Value = decode(&payload)?;
    let source = &payload["source"];
    if cap != STAGE_CAPABILITY
        || !(state == "ObservedComplete" || (!published && state == "Dispatching"))
        || payload["digest"].as_str() != Some(row.version.blob_digest.as_str())
        || payload["byteCount"].as_i64() != Some(row.version.byte_count)
        || source["kind"] != "ConventionImportSource"
        || source["operationId"].as_str() != Some(row.operation_id.0.as_str())
        || source["conventionId"].as_str() != Some(draft.convention_id.as_str())
        || source["ordinal"].as_i64() != Some(row.ordinal)
        || source["entryKind"] != row.kind
        || source["resourceKey"].as_str() != row.resource_key.as_deref()
    {
        return Err(invalid(
            "private import capture lacks exact verified staging association",
        ));
    }
    let fence:Option<(i64,Option<i64>)>=db(conn.query_row("SELECT attempt_no,owner_generation FROM operation_attempts WHERE operation_id=?1 ORDER BY attempt_no DESC LIMIT 1",[row.staging_operation_id.0.as_str()],|r|Ok((r.get(0)?,r.get(1)?))).optional())?;
    if fence
        != Some((
            row.staging_fence.attempt_no,
            row.staging_fence.owner_generation,
        ))
    {
        return Err(StorageError::LeaseLost {
            kind: "artifact_stage",
            id: row.staging_operation_id.0.as_str().into(),
        });
    }
    let blob:bool=db(conn.query_row("SELECT EXISTS(SELECT 1 FROM blobs WHERE digest=?1 AND generation=?2 AND byte_count=?3 AND relative_path=?4 AND state='Present')",params![row.version.blob_digest,row.version.blob_generation,row.version.byte_count,row.managed_path],|r|r.get(0)))?;
    if !blob {
        return Err(StorageError::Busy);
    }
    Ok(())
}
fn capture_valid(
    conn: &Connection,
    r: &StoredConventionImportArtifact,
    d: &StoredConventionImport,
    host: &ActorId,
) -> Result<(), StorageError> {
    let a = &r.artifact;
    let v = &r.version;
    let actual_scope = a
        .scope
        .project()
        .cloned()
        .map_or(KnowledgeScope::Workspace, KnowledgeScope::Project);
    if r.ordinal < 0
        || !matches!(r.kind.as_str(), "Markdown" | "Raw" | "Resource")
        || a.id != v.artifact_id
        || a.revision != 1
        || a.scope_revision != 1
        || a.reference_revision != 1
        || a.state != ArtifactState::Live
        || a.retention != ArtifactRetention::Explicit
        || a.owner.as_ref() != Some(&d.author_id)
        || a.origin_project_id.as_ref() != d.scope.project()
        || actual_scope != d.scope
        || a.current_version_id.as_ref().is_some_and(|id| id != &v.id)
        || v.version_no != 1
        || v.deleted_at_ms.is_some()
        || a.created_at_ms < 0
        || v.created_at_ms < a.created_at_ms
        || v.byte_count < 0
        || !text(&v.media_type, 1, 128)
        || a.provenance["principalKind"] != "Host"
        || a.provenance["producedBy"].as_str() != Some(host.0.as_str())
        || v.source["operationId"].as_str() != Some(r.staging_operation_id.0.as_str())
    {
        return Err(invalid(
            "invalid prospective import artifact/version identity",
        ));
    }
    if let Some(work) = &a.origin_work_id {
        let project = a
            .origin_project_id
            .as_ref()
            .ok_or_else(|| invalid("import work origin requires a project"))?;
        let matches: bool = db(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_items WHERE id=?1 AND project_id=?2)",
            params![work.0.as_str(), project.0.as_str()],
            |r| r.get(0),
        ))?;
        if !matches {
            return Err(invalid("import work origin differs from actual project"));
        }
    }
    if r.kind == "Resource" {
        if r.resource_id.is_none()
            || r.resource_key.as_ref().is_none_or(|k| !text(k, 1, 200))
            || r.resource_name.as_ref().is_none_or(|n| !text(n, 1, 255))
            || r.resource_media_type
                .as_ref()
                .is_none_or(|m| !(0..=200).contains(&m.chars().count()))
            || r.resource_lifecycle
                .as_ref()
                .is_none_or(|l| !matches!(l.as_str(), "Active" | "Retired" | "Deleted"))
        {
            return Err(invalid("invalid imported resource metadata"));
        }
    } else if r.resource_id.is_some()
        || r.resource_key.is_some()
        || r.resource_name.is_some()
        || r.resource_lifecycle.is_some()
        || r.resource_media_type.is_some()
    {
        return Err(invalid(
            "document capture cannot claim a resource association",
        ));
    }
    let collision:bool=db(conn.query_row("SELECT EXISTS(SELECT 1 FROM artifacts WHERE id=?1) OR EXISTS(SELECT 1 FROM artifact_versions WHERE id=?2) OR EXISTS(SELECT 1 FROM convention_resource_heads WHERE id=?3)",params![a.id.as_str(),v.id.as_str(),r.resource_id.as_ref().map(Id::as_str)],|r|r.get(0)))?;
    if collision {
        return Err(invalid(
            "import prospective identity already exists publicly",
        ));
    }
    stage_proof(conn, r, false)
}
fn ref_of(row: &StoredConventionImportArtifact) -> Value {
    json!({"artifactId":row.artifact.id,"versionId":row.version.id,"sha256":row.version.blob_digest,"byteLength":row.version.byte_count.to_string(),"mediaType":row.version.media_type})
}

macro_rules! reads {($ty:ty)=>{impl ConventionImportRead for $ty {
    fn convention_import_storage_usage(&self,op:&OperationId)->Result<ConventionImportStorageUsage,StorageError>{let d=required(self.conn,op)?;db(self.conn.query_row("SELECT coalesce(max(byte_count),0),coalesce((SELECT sum(b.byte_count) FROM blobs b WHERE b.digest IN(SELECT blob_digest FROM convention_import_artifacts WHERE operation_id=?1) AND NOT EXISTS(SELECT 1 FROM artifact_versions v JOIN artifacts a ON a.id=v.artifact_id WHERE v.blob_digest=b.digest AND a.retention='Explicit' AND v.deleted_at IS NULL AND a.current_project_id IS ?2)),0) FROM convention_import_artifacts WHERE operation_id=?1",params![op.0.as_str(),d.scope.project().map(|p|p.0.as_str())],|r|Ok(ConventionImportStorageUsage{max_entry_bytes:r.get(0)?,additional_scope_bytes:r.get(1)?})))}
    fn convention_import(&self,op:&OperationId)->Result<Option<StoredConventionImport>,StorageError>{get(self.conn,op)}
    fn convention_imports_pending(&self,after:Option<&OperationId>,limit:u16)->Result<Vec<StoredConventionImport>,StorageError>{let limit=page(limit)?;let mut stmt=db(self.conn.prepare_cached(&format!("SELECT {IMPORT_COLUMNS} FROM convention_imports WHERE phase!='Published' AND (phase IN('Queued','Parsing','Ready') OR EXISTS(SELECT 1 FROM convention_import_artifacts a WHERE a.operation_id=convention_imports.operation_id)) AND (?1 IS NULL OR operation_id>?1) ORDER BY operation_id LIMIT ?2")))?;db(db(stmt.query_map(params![after.map(|o|o.0.as_str()),limit],import_record))?.collect())}
    fn convention_import_metadata(&self,op:&OperationId)->Result<Option<StoredConvention>,StorageError>{metadata(self.conn,op)}
    fn convention_import_artifacts(&self,op:&OperationId,after:Option<i64>,limit:u16)->Result<Vec<StoredConventionImportArtifact>,StorageError>{captures(self.conn,op,after,limit)}
}};}
reads!(Snapshot<'_>);
reads!(Unit<'_>);
impl ConventionImportRepository for Unit<'_> {
    fn begin_convention_import(
        &mut self,
        d: &StoredConventionImport,
    ) -> Result<StoredConventionImport, StorageError> {
        if d.revision != 1
            || d.phase != "Queued"
            || d.processed_resources != 0
            || d.total_resources.is_some()
            || d.error.is_some()
            || d.expected_scope_revision < 1
            || d.created_at_ms < 0
            || d.updated_at_ms != d.created_at_ms
        {
            return Err(invalid("invalid initial convention import"));
        }
        checkpoint(&d.checkpoint)?;
        if let Some(old) = get(self.conn, &d.operation_id)? {
            return if old.convention_id == d.convention_id
                && old.scope == d.scope
                && old.author_id == d.author_id
                && old.source_ref == d.source_ref
                && old.expected_scope_revision == d.expected_scope_revision
            {
                Ok(old)
            } else {
                Err(invalid("immutable import binding changed"))
            };
        }
        active(self.conn, &d.operation_id)?;
        target_exists(self.conn, &d.scope)?;
        let source = actual_source(self.conn, &d.source_ref)?;
        if current(self.conn, &d.convention_id)?.is_some() {
            return Err(invalid("import identity must be new"));
        }
        let actual = scope_revision(self.conn, &d.scope)?;
        if actual != d.expected_scope_revision {
            return Err(conflict(
                &d.convention_id,
                d.expected_scope_revision,
                actual,
            ));
        }
        db(self.conn.execute("INSERT INTO convention_imports(operation_id,convention_id,scope_key,project_id,author_id,source_ref_json,source_version_id,expected_scope_revision,revision,phase,checkpoint_json,processed_resources,total_resources,error,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,1,'Queued',?9,0,NULL,NULL,?10,?10)",params![d.operation_id.0.as_str(),d.convention_id.as_str(),d.scope.key(),d.scope.project().map(|p|p.0.as_str()),d.author_id.0.as_str(),d.source_ref.to_string(),source.version_id.as_str(),d.expected_scope_revision,d.checkpoint.to_string(),d.created_at_ms]))?;
        Ok(d.clone())
    }
    fn stage_convention_import_artifact(
        &mut self,
        r: &StoredConventionImportArtifact,
    ) -> Result<StoredConventionImport, StorageError> {
        let d = required(self.conn, &r.operation_id)?;
        if !matches!(d.phase.as_str(), "Queued" | "Parsing") {
            return Err(invalid("import is not parsing"));
        }
        active(self.conn, &r.operation_id)?;
        let old = captures(
            self.conn,
            &r.operation_id,
            r.ordinal.checked_sub(1).filter(|v| *v >= 0),
            1,
        )?
        .into_iter()
        .find(|old| old.ordinal == r.ordinal);
        if let Some(old) = old {
            return if old == *r {
                Ok(d)
            } else {
                Err(invalid("private import capture is immutable"))
            };
        }
        capture_valid(self.conn, r, &d, &self.info.host_actor)?;
        let revision = d
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("import revision overflow"))?;
        let processed = d
            .processed_resources
            .checked_add(i64::from(r.kind == "Resource"))
            .ok_or_else(|| invalid("import resource count overflow"))?;
        atomic(self.conn, || {
            db(self.conn.execute("INSERT INTO convention_import_artifacts(operation_id,ordinal,kind,resource_id,resource_key,resource_name,resource_lifecycle,resource_media_type,artifact_id,version_id,artifact_json,version_json,blob_digest,blob_generation,byte_count,staging_operation_id,staging_attempt,staging_generation,managed_path) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",params![r.operation_id.0.as_str(),r.ordinal,r.kind,r.resource_id.as_ref().map(Id::as_str),r.resource_key,r.resource_name,r.resource_lifecycle,r.resource_media_type,r.artifact.id.as_str(),r.version.id.as_str(),encoded(&ArtifactData::from(&r.artifact))?,encoded(&VersionData::from(&r.version))?,r.version.blob_digest,r.version.blob_generation,r.version.byte_count,r.staging_operation_id.0.as_str(),r.staging_fence.attempt_no,r.staging_fence.owner_generation,r.managed_path]))?;
            db(self.conn.execute("UPDATE convention_imports SET revision=?2,processed_resources=?3,phase='Parsing',updated_at=max(updated_at,?4) WHERE operation_id=?1",params![r.operation_id.0.as_str(),revision,processed,self.now]))?;
            required(self.conn, &r.operation_id)
        })
    }
    fn set_convention_import_metadata(
        &mut self,
        op: &OperationId,
        r: &StoredConvention,
        parent_revision: Option<i64>,
    ) -> Result<StoredConventionImport, StorageError> {
        let d = required(self.conn, op)?;
        if !matches!(d.phase.as_str(), "Queued" | "Parsing") {
            return Err(invalid("import metadata is sealed"));
        }
        active(self.conn, op)?;
        if let Some(old) = metadata(self.conn, op)? {
            let prior:Option<i64>=db(self.conn.query_row("SELECT expected_parent_revision FROM convention_import_metadata WHERE operation_id=?1",[op.0.as_str()],|r|r.get(0)))?;
            return if old == *r && prior == parent_revision {
                Ok(d)
            } else {
                Err(invalid("prospective convention metadata is immutable"))
            };
        }
        if r.id != d.convention_id
            || r.scope != d.scope
            || r.author_id != d.author_id
            || r.created_at_ms != d.created_at_ms
            || r.revision != 1
            || r.parent_id.is_some() != parent_revision.is_some()
            || parent_revision.is_some_and(|v| v < 1)
            || r.provenance["kind"] != "Imported"
            || r.provenance["source"] != d.source_ref
        {
            return Err(invalid(
                "prospective convention differs from actual import binding",
            ));
        }
        validate(self.conn, r)?;
        if let Some(parent) = &r.parent_id {
            let row =
                current(self.conn, parent)?.ok_or_else(|| invalid("import parent missing"))?;
            if Some(row.revision) != parent_revision {
                return Err(conflict(parent, parent_revision.unwrap_or(1), row.revision));
            }
        }
        let mut body = None;
        let mut raw = None;
        let mut after = None;
        loop {
            let rows = captures(self.conn, op, after, MAX_PAGE)?;
            if rows.is_empty() {
                break;
            }
            after = rows.last().map(|r| r.ordinal);
            for row in rows {
                match row.kind.as_str() {
                    "Markdown" => body = Some(ref_of(&row)),
                    "Raw" => raw = Some(ref_of(&row)),
                    _ => {}
                }
            }
        }
        if body.is_none() || r.content_ref != body || r.raw_content_ref != raw {
            return Err(invalid(
                "convention body refs must name its exact private document captures",
            ));
        }
        let revision = d
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("import revision overflow"))?;
        atomic(self.conn, || {
            db(self.conn.execute("INSERT INTO convention_import_metadata(operation_id,convention_id,scope_key,project_id,revision,slug,title,title_folded,slug_folded,parent_id,expected_parent_revision,sort_order,lifecycle,content_ref_json,raw_content_ref_json,applicability_json,provenance_json,author_id,created_at,updated_at,legacy_approved) VALUES(?1,?2,?3,?4,1,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",params![op.0.as_str(),r.id.as_str(),r.scope.key(),r.scope.project().map(|p|p.0.as_str()),r.slug,r.title,r.title.to_lowercase(),r.slug.to_lowercase(),r.parent_id.as_ref().map(Id::as_str),parent_revision,r.sort_order,r.lifecycle,r.content_ref.as_ref().map(Value::to_string),r.raw_content_ref.as_ref().map(Value::to_string),json!(r.applicability).to_string(),r.provenance.to_string(),r.author_id.0.as_str(),r.created_at_ms,r.updated_at_ms,r.legacy_approved]))?;
            db(self.conn.execute("UPDATE convention_imports SET revision=?2,updated_at=max(updated_at,?3) WHERE operation_id=?1",params![op.0.as_str(),revision,self.now]))?;
            required(self.conn, op)
        })
    }
    fn set_convention_import_phase(
        &mut self,
        op: &OperationId,
        expected: i64,
        phase: &str,
        total: Option<i64>,
        error: Option<&str>,
        saved: Option<&Value>,
    ) -> Result<StoredConventionImport, StorageError> {
        let d = required(self.conn, op)?;
        if d.revision != expected {
            return Err(conflict(&d.convention_id, expected, d.revision));
        }
        let transition = d.phase == phase
            || match d.phase.as_str() {
                "Queued" => matches!(phase, "Parsing" | "Failed" | "Cancelled"),
                "Parsing" => matches!(phase, "Ready" | "Failed" | "Cancelled"),
                "Ready" => matches!(phase, "Failed" | "Cancelled"),
                _ => false,
            };
        if !transition
            || phase == "Published"
            || total.is_some_and(|n| n < 0)
            || error.is_some_and(|e| e.chars().count() > 4096)
            || d.total_resources
                .is_some_and(|old| total.is_some_and(|new| old != new))
        {
            return Err(invalid("invalid import phase transition"));
        }
        let saved = saved.unwrap_or(&d.checkpoint);
        checkpoint(saved)?;
        if phase != "Failed" && phase != "Cancelled" {
            active(self.conn, op)?;
        }
        let total = total.or(d.total_resources);
        if phase == "Ready"
            && (total != Some(d.processed_resources) || metadata(self.conn, op)?.is_none())
        {
            return Err(invalid("incomplete import cannot become ready"));
        }
        let revision = expected
            .checked_add(1)
            .ok_or_else(|| invalid("import revision overflow"))?;
        db(self.conn.execute("UPDATE convention_imports SET revision=?2,phase=?3,total_resources=?4,error=?5,checkpoint_json=?6,updated_at=max(updated_at,?7) WHERE operation_id=?1",params![op.0.as_str(),revision,phase,total,error,saved.to_string(),self.now]))?;
        required(self.conn, op)
    }
    fn publish_convention_import(
        &mut self,
        op: &OperationId,
        expected: i64,
        expected_scope: i64,
        count: i64,
    ) -> Result<StoredConvention, StorageError> {
        let d = required(self.conn, op)?;
        if d.phase != "Ready"
            || d.revision != expected
            || d.expected_scope_revision != expected_scope
            || d.total_resources != Some(count)
            || d.processed_resources != count
            || count < 0
        {
            return Err(invalid("import publication CAS/count refused"));
        }
        active(self.conn, op)?;
        actual_source(self.conn, &d.source_ref)?;
        target_exists(self.conn, &d.scope)?;
        let actual = scope_revision(self.conn, &d.scope)?;
        if actual != expected_scope {
            return Err(conflict(&d.convention_id, expected_scope, actual));
        }
        let mut r = metadata(self.conn, op)?.ok_or_else(|| invalid("import metadata missing"))?;
        validate(self.conn, &r)?;
        if current(self.conn, &r.id)?.is_some() {
            return Err(invalid("import cannot overwrite an existing convention"));
        }
        let parent_expected: Option<i64> = db(self.conn.query_row(
            "SELECT expected_parent_revision FROM convention_import_metadata WHERE operation_id=?1",
            [op.0.as_str()],
            |r| r.get(0),
        ))?;
        if let Some(parent) = &r.parent_id {
            let parent =
                current(self.conn, parent)?.ok_or_else(|| invalid("import parent missing"))?;
            if Some(parent.revision) != parent_expected {
                return Err(invalid("import parent revision changed"));
            }
        }
        let actual_count:i64=db(self.conn.query_row("SELECT count(*) FROM convention_import_artifacts WHERE operation_id=?1 AND kind='Resource'",[op.0.as_str()],|r|r.get(0)))?;
        if actual_count != count {
            return Err(invalid("import resources are incomplete"));
        }
        // Bounded pages validate the real journal and blob generation, without collecting all
        // resource metadata or opening any file inside the writer.
        let mut after = None;
        loop {
            let rows = captures(self.conn, op, after, MAX_PAGE)?;
            if rows.is_empty() {
                break;
            }
            after = rows.last().map(|r| r.ordinal);
            for row in rows {
                capture_valid(self.conn, &row, &d, &self.info.host_actor)?;
                stage_proof(self.conn, &row, true)?;
            }
        }
        r.updated_at_ms = self.now.max(r.updated_at_ms);
        let final_revision = d
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("import revision overflow"))?;
        atomic(self.conn, || {
            db(self.conn.execute("INSERT INTO artifacts(id,origin_project_id,origin_work_id,title,revision,current_project_id,scope_revision,reference_revision,current_version_id,state,created_at,provenance_json,scope_kind,owner_actor_id,retention) SELECT artifact_id,json_extract(artifact_json,'$.origin_project_id'),json_extract(artifact_json,'$.origin_work_id'),json_extract(artifact_json,'$.title'),1,?2,1,1,NULL,'Live',json_extract(artifact_json,'$.created_at_ms'),json_extract(artifact_json,'$.provenance'),?3,json_extract(artifact_json,'$.owner'),'Explicit' FROM convention_import_artifacts WHERE operation_id=?1",params![op.0.as_str(),d.scope.project().map(|p|p.0.as_str()),if d.scope.project().is_some(){"Project"}else{"Workspace"}]))?;
            db(self.conn.execute("INSERT INTO artifact_versions(id,artifact_id,version_no,blob_digest,media_type,created_at,deleted_at,source_json) SELECT version_id,artifact_id,1,blob_digest,json_extract(version_json,'$.media_type'),json_extract(version_json,'$.created_at_ms'),NULL,json_extract(version_json,'$.source') FROM convention_import_artifacts WHERE operation_id=?1",[op.0.as_str()]))?;
            db(self.conn.execute("UPDATE artifacts SET current_version_id=(SELECT version_id FROM convention_import_artifacts d WHERE d.artifact_id=artifacts.id AND d.operation_id=?1) WHERE id IN(SELECT artifact_id FROM convention_import_artifacts WHERE operation_id=?1)",[op.0.as_str()]))?;
            ConventionRepository::create_convention(self, &r)?;
            db(self.conn.execute("INSERT INTO convention_resource_heads(id,convention_id,revision) SELECT resource_id,?2,1 FROM convention_import_artifacts WHERE operation_id=?1 AND kind='Resource'",params![op.0.as_str(),r.id.as_str()]))?;
            db(self.conn.execute("INSERT INTO convention_resource_versions(resource_id,revision,convention_id,convention_revision,name,media_type,content_ref_json,content_version_id,author_id,created_at,deleted_at,lifecycle) SELECT resource_id,1,?2,1,resource_name,resource_media_type,json_object('artifactId',artifact_id,'versionId',version_id,'sha256',blob_digest,'byteLength',cast(byte_count AS TEXT),'mediaType',json_extract(version_json,'$.media_type')),version_id,?3,?4,CASE WHEN resource_lifecycle='Deleted' THEN ?5 ELSE NULL END,resource_lifecycle FROM convention_import_artifacts WHERE operation_id=?1 AND kind='Resource'",params![op.0.as_str(),r.id.as_str(),d.author_id.0.as_str(),d.created_at_ms,self.now]))?;
            ConventionRepository::bump_convention_scope(self, &d.scope)?;
            db(self.conn.execute("UPDATE convention_imports SET phase='Published',revision=?2,error=NULL,updated_at=max(updated_at,?3) WHERE operation_id=?1",params![op.0.as_str(),final_revision,self.now]))?;
            Ok(r)
        })
    }
    fn discard_convention_import_artifacts(
        &mut self,
        op: &OperationId,
        limit: u16,
    ) -> Result<bool, StorageError> {
        let limit = page(limit)?;
        let d = required(self.conn, op)?;
        let state = operation(self.conn, op)?;
        if !matches!(d.phase.as_str(), "Failed" | "Cancelled")
            || !matches!(
                state.as_str(),
                "Failed" | "NeedsInput" | "CancelledBeforeDispatch" | "ObservedComplete"
            )
        {
            return Err(invalid(
                "only terminal unpublished imports may release private captures",
            ));
        }
        db(self.conn.execute("DELETE FROM convention_import_artifacts WHERE operation_id=?1 AND ordinal IN(SELECT ordinal FROM convention_import_artifacts WHERE operation_id=?1 ORDER BY ordinal LIMIT ?2)",params![op.0.as_str(),limit]))?;
        let remaining: bool = db(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM convention_import_artifacts WHERE operation_id=?1)",
            [op.0.as_str()],
            |r| r.get(0),
        ))?;
        Ok(!remaining)
    }
}
