//! SQLite artifact records: `artifacts`, `artifact_versions`, `blobs`, `artifact_links`,
//! `artifact_shares`, `artifact_tags` (migration 1, extended by migration 6) and
//! `artifact_blob_pins`.
use super::conn::db;
use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id, OperationId, ProjectId, RootId};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;

fn invalid(m: impl Into<String>) -> StorageError {
    StorageError::InvalidRecord(m.into())
}
fn corrupt(m: impl Into<String>) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, m)
}
fn id_of(s: String) -> Result<Id, StorageError> {
    Id::new(s).map_err(|e| corrupt(format!("stored identifier: {e}")))
}
fn json_of(s: &str) -> Result<Value, StorageError> {
    serde_json::from_str(s).map_err(|e| corrupt(format!("stored JSON: {e}")))
}
fn bounded(limit: u16) -> Result<i64, StorageError> {
    if limit == 0 || limit > MAX_PAGE {
        return Err(invalid("listing limit must be 1..=500"));
    }
    Ok(limit as i64)
}

const ARTIFACT_COLUMNS: &str = "id,origin_project_id,origin_work_id,title,revision,current_project_id,scope_revision,reference_revision,current_version_id,state,created_at,provenance_json,scope_kind,owner_actor_id,retention";
struct ArtifactRaw {
    id: String,
    origin: Option<String>,
    work: Option<String>,
    title: String,
    revision: i64,
    current: Option<String>,
    scope_revision: i64,
    reference_revision: i64,
    version: Option<String>,
    state: String,
    created_at: i64,
    provenance: String,
    scope_kind: String,
    owner: Option<String>,
    retention: String,
}
fn artifact_row(r: &Row<'_>) -> rusqlite::Result<ArtifactRaw> {
    Ok(ArtifactRaw {
        id: r.get(0)?,
        origin: r.get(1)?,
        work: r.get(2)?,
        title: r.get(3)?,
        revision: r.get(4)?,
        current: r.get(5)?,
        scope_revision: r.get(6)?,
        reference_revision: r.get(7)?,
        version: r.get(8)?,
        state: r.get(9)?,
        created_at: r.get(10)?,
        provenance: r.get(11)?,
        scope_kind: r.get(12)?,
        owner: r.get(13)?,
        retention: r.get(14)?,
    })
}
fn artifact_of(raw: ArtifactRaw) -> Result<StoredArtifact, StorageError> {
    let scope = match (raw.scope_kind.as_str(), raw.current) {
        ("Project", Some(p)) => ArtifactScope::Project(ProjectId(id_of(p)?)),
        ("Workspace", None) => ArtifactScope::Workspace,
        (kind, _) => return Err(corrupt(format!("stored artifact scope {kind}"))),
    };
    Ok(StoredArtifact {
        id: id_of(raw.id)?,
        origin_project_id: raw.origin.map(id_of).transpose()?.map(ProjectId),
        origin_work_id: raw.work.map(id_of).transpose()?.map(RootId),
        title: raw.title,
        revision: raw.revision,
        scope,
        owner: raw.owner.map(id_of).transpose()?.map(ActorId),
        retention: ArtifactRetention::parse(&raw.retention)
            .ok_or_else(|| corrupt(format!("stored artifact retention {}", raw.retention)))?,
        scope_revision: raw.scope_revision,
        reference_revision: raw.reference_revision,
        current_version_id: raw.version.map(id_of).transpose()?,
        state: ArtifactState::parse(&raw.state)
            .ok_or_else(|| corrupt(format!("stored artifact state {}", raw.state)))?,
        created_at_ms: raw.created_at,
        provenance: json_of(&raw.provenance)?,
    })
}
fn scope_columns(scope: &ArtifactScope) -> (&'static str, Option<&str>) {
    match scope {
        ArtifactScope::Project(p) => ("Project", Some(p.0.as_str())),
        ArtifactScope::Workspace => ("Workspace", None),
    }
}

const VERSION_COLUMNS: &str = "v.id,v.artifact_id,v.version_no,v.blob_digest,b.byte_count,v.media_type,v.created_at,v.deleted_at,v.source_json,b.generation";
type VersionRaw = (
    String,
    String,
    i64,
    String,
    i64,
    String,
    i64,
    Option<i64>,
    String,
    String,
);
fn version_row(r: &Row<'_>) -> rusqlite::Result<VersionRaw> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
    ))
}
fn version_of(raw: VersionRaw) -> Result<StoredVersion, StorageError> {
    let (id, artifact, no, digest, bytes, media, at, deleted, source, generation) = raw;
    Ok(StoredVersion {
        id: id_of(id)?,
        artifact_id: id_of(artifact)?,
        version_no: no,
        blob_digest: digest,
        byte_count: bytes,
        blob_generation: generation,
        media_type: media,
        created_at_ms: at,
        deleted_at_ms: deleted,
        source: json_of(&source)?,
    })
}

const BLOB_COLUMNS: &str =
    "digest,byte_count,generation,relative_path,state,claim_owner,last_error";
type BlobRaw = (
    String,
    i64,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
);
fn blob_row(r: &Row<'_>) -> rusqlite::Result<BlobRaw> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
    ))
}
fn blob_of(raw: BlobRaw) -> Result<BlobRecord, StorageError> {
    let (digest, byte_count, generation, relative_path, state, claim_owner, last_error) = raw;
    Ok(BlobRecord {
        digest,
        byte_count,
        generation,
        relative_path,
        state: BlobState::parse(&state)
            .ok_or_else(|| corrupt(format!("stored blob state {state}")))?,
        claim_owner,
        last_error,
    })
}

fn link_row(r: &Row<'_>) -> rusqlite::Result<(String, String, String, String)> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
}
fn link_of(raw: (String, String, String, String)) -> Result<ArtifactLink, StorageError> {
    let (version, kind, subject, purpose) = raw;
    Ok(ArtifactLink {
        version_id: id_of(version)?,
        subject: SubjectKey {
            kind: SubjectKey::parse_kind(&kind)
                .ok_or_else(|| corrupt(format!("stored subject kind {kind}")))?,
            id: id_of(subject)?,
        },
        purpose: LinkPurpose::parse(&purpose)
            .ok_or_else(|| corrupt(format!("stored link purpose {purpose}")))?,
    })
}

fn exists(conn: &Connection, sql: &str, args: impl rusqlite::Params) -> Result<bool, StorageError> {
    db(db(conn.prepare_cached(sql))?
        .query_row(args, |r| r.get::<_, i64>(0))
        .optional())
    .map(|n| n.is_some())
}

fn artifact(conn: &Connection, id: &Id) -> Result<Option<StoredArtifact>, StorageError> {
    db(db(conn.prepare_cached(&format!(
        "SELECT {ARTIFACT_COLUMNS} FROM artifacts WHERE id=?1"
    )))?
    .query_row([id.as_str()], artifact_row)
    .optional())?
    .map(artifact_of)
    .transpose()
}
fn version(conn: &Connection, id: &Id) -> Result<Option<StoredVersion>, StorageError> {
    db(db(conn.prepare_cached(&format!(
        "SELECT {VERSION_COLUMNS} FROM artifact_versions v JOIN blobs b ON b.digest=v.blob_digest WHERE v.id=?1"
    )))?
    .query_row([id.as_str()], version_row)
    .optional())?
    .map(version_of)
    .transpose()
}
fn versions(conn: &Connection, artifact: &Id) -> Result<Vec<StoredVersion>, StorageError> {
    let mut s = db(conn.prepare_cached(&format!(
        "SELECT {VERSION_COLUMNS} FROM artifact_versions v JOIN blobs b ON b.digest=v.blob_digest WHERE v.artifact_id=?1 ORDER BY v.version_no"
    )))?;
    let rows =
        db(db(s.query_map([artifact.as_str()], version_row))?
            .collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter().map(version_of).collect()
}
fn version_by_operation(
    conn: &Connection,
    op: &OperationId,
) -> Result<Option<StoredVersion>, StorageError> {
    db(db(conn.prepare_cached(&format!(
        "SELECT {VERSION_COLUMNS} FROM artifact_versions v JOIN blobs b ON b.digest=v.blob_digest WHERE json_extract(v.source_json,'$.operationId')=?1"
    )))?
    .query_row([op.0.as_str()], version_row)
    .optional())?
    .map(version_of)
    .transpose()
}
fn list(
    conn: &Connection,
    host: &ActorId,
    project: &ProjectId,
    tags: &[String],
    after: Option<&ListPosition>,
    limit: u16,
) -> Result<Vec<StoredArtifact>, StorageError> {
    let limit = bounded(limit)?;
    let (at, id) = match after {
        Some(p) => (p.created_at_ms, p.id.as_str().to_owned()),
        None => (i64::MAX, String::new()),
    };
    // All-of tag filter: the artifact carries as many of the (distinct) requested tags as there
    // are requested tags.
    let wanted = Value::from(tags.to_vec()).to_string();
    let mut s = db(conn.prepare_cached(&format!(
        "SELECT {ARTIFACT_COLUMNS} FROM artifacts a
         WHERE a.state='Live' AND a.current_version_id IS NOT NULL AND a.retention='Explicit'
           AND (a.current_project_id=?1 OR EXISTS(SELECT 1 FROM artifact_shares s WHERE s.artifact_id=a.id AND s.project_id=?1))
           AND NOT (json_extract(a.provenance_json,'$.principalKind') IS 'Host'
                    AND json_extract(a.provenance_json,'$.producedBy') IS ?7
                    AND json_extract(a.provenance_json,'$.derived.kind') IS 'HelpTopic')
           AND (?2=9223372036854775807 OR a.created_at<?2 OR (a.created_at=?2 AND a.id<?3))
           AND (?6=0 OR (SELECT COUNT(*) FROM artifact_tags t WHERE t.artifact_id=a.id
                         AND t.tag IN (SELECT value FROM json_each(?5)))=?6)
         ORDER BY a.created_at DESC, a.id DESC LIMIT ?4"
    )))?;
    let rows = db(db(s.query_map(
        params![
            project.0.as_str(),
            at,
            id,
            limit,
            wanted,
            tags.len() as i64,
            host.0.as_str()
        ],
        artifact_row,
    ))?
    .collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter().map(artifact_of).collect()
}
fn list_workspace(
    conn: &Connection,
    host: &ActorId,
    owner: Option<&ActorId>,
    tags: &[String],
    after: Option<&ListPosition>,
    limit: u16,
) -> Result<Vec<StoredArtifact>, StorageError> {
    let limit = bounded(limit)?;
    let (at, id) = match after {
        Some(p) => (p.created_at_ms, p.id.as_str().to_owned()),
        None => (i64::MAX, String::new()),
    };
    let wanted = Value::from(tags.to_vec()).to_string();
    let mut s = db(conn.prepare_cached(&format!(
        "SELECT {ARTIFACT_COLUMNS} FROM artifacts a
         WHERE a.scope_kind='Workspace' AND a.state='Live' AND a.current_version_id IS NOT NULL
           AND a.retention='Explicit' AND (?1 IS NULL OR a.owner_actor_id=?1)
           AND NOT (json_extract(a.provenance_json,'$.principalKind') IS 'Host'
                    AND json_extract(a.provenance_json,'$.producedBy') IS ?7
                    AND json_extract(a.provenance_json,'$.derived.kind') IS 'HelpTopic')
           AND (?2=9223372036854775807 OR a.created_at<?2 OR (a.created_at=?2 AND a.id<?3))
           AND (?6=0 OR (SELECT COUNT(*) FROM artifact_tags t WHERE t.artifact_id=a.id
                         AND t.tag IN (SELECT value FROM json_each(?5)))=?6)
         ORDER BY a.created_at DESC, a.id DESC LIMIT ?4"
    )))?;
    let rows = db(db(s.query_map(
        params![
            owner.map(|o| o.0.as_str()),
            at,
            id,
            limit,
            wanted,
            tags.len() as i64,
            host.0.as_str()
        ],
        artifact_row,
    ))?
    .collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter().map(artifact_of).collect()
}
fn tags(conn: &Connection, artifact: &Id) -> Result<Vec<String>, StorageError> {
    let mut s =
        db(conn.prepare_cached("SELECT tag FROM artifact_tags WHERE artifact_id=?1 ORDER BY tag"))?;
    db(
        db(s.query_map([artifact.as_str()], |r| r.get::<_, String>(0)))?
            .collect::<rusqlite::Result<Vec<_>>>(),
    )
}
fn shares(conn: &Connection, artifact: &Id) -> Result<Vec<ProjectId>, StorageError> {
    let mut s = db(conn.prepare_cached(
        "SELECT project_id FROM artifact_shares WHERE artifact_id=?1 ORDER BY project_id",
    ))?;
    let rows = db(
        db(s.query_map([artifact.as_str()], |r| r.get::<_, String>(0)))?
            .collect::<rusqlite::Result<Vec<_>>>(),
    )?;
    rows.into_iter().map(|p| id_of(p).map(ProjectId)).collect()
}
fn links(conn: &Connection, artifact: &Id) -> Result<Vec<ArtifactLink>, StorageError> {
    let mut s = db(conn.prepare_cached(
        "SELECT l.artifact_version_id,l.subject_kind,l.subject_id,l.purpose FROM artifact_links l
         JOIN artifact_versions v ON v.id=l.artifact_version_id WHERE v.artifact_id=?1
         ORDER BY v.version_no,l.subject_kind,l.subject_id,l.purpose",
    ))?;
    let rows =
        db(db(s.query_map([artifact.as_str()], link_row))?.collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter().map(link_of).collect()
}
fn linked_to(conn: &Connection, subject: &SubjectKey) -> Result<Vec<ArtifactLink>, StorageError> {
    let mut s = db(conn.prepare_cached(
        "SELECT artifact_version_id,subject_kind,subject_id,purpose FROM artifact_links
         WHERE subject_kind=?1 AND subject_id=?2 ORDER BY artifact_version_id,purpose LIMIT 500",
    ))?;
    let rows = db(
        db(s.query_map(params![subject.kind, subject.id.as_str()], link_row))?
            .collect::<rusqlite::Result<Vec<_>>>(),
    )?;
    rows.into_iter().map(link_of).collect()
}
fn blob(conn: &Connection, digest: &str) -> Result<Option<BlobRecord>, StorageError> {
    db(
        db(conn.prepare_cached(&format!("SELECT {BLOB_COLUMNS} FROM blobs WHERE digest=?1")))?
            .query_row([digest], blob_row)
            .optional(),
    )?
    .map(blob_of)
    .transpose()
}
fn blobs_in(
    conn: &Connection,
    state: BlobState,
    after: Option<&str>,
    limit: u16,
) -> Result<Vec<BlobRecord>, StorageError> {
    let limit = bounded(limit)?;
    let mut s = db(conn.prepare_cached(&format!(
        "SELECT {BLOB_COLUMNS} FROM blobs WHERE state=?1 AND digest>?2 ORDER BY digest LIMIT ?3"
    )))?;
    let rows = db(db(s.query_map(
        params![state.as_str(), after.unwrap_or(""), limit],
        blob_row,
    ))?
    .collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter().map(blob_of).collect()
}
fn count(conn: &Connection, sql: &str, args: impl rusqlite::Params) -> Result<i64, StorageError> {
    db(db(conn.prepare_cached(sql))?.query_row(args, |r| r.get(0)))
}
fn subject_exists(
    conn: &Connection,
    project: &ProjectId,
    subject: &SubjectKey,
) -> Result<bool, StorageError> {
    let (p, id) = (project.0.as_str(), subject.id.as_str());
    match subject.kind {
        "root" => exists(
            conn,
            "SELECT 1 FROM work_items WHERE id=?1 AND project_id=?2",
            [id, p],
        ),
        "assignment" => exists(
            conn,
            "SELECT 1 FROM assignments a JOIN work_items w ON w.id=a.root_id WHERE a.id=?1 AND w.project_id=?2",
            [id, p],
        ),
        "operation" => exists(
            conn,
            "SELECT 1 FROM operations o LEFT JOIN work_items w ON w.id=o.root_id WHERE o.id=?1 AND (o.root_id IS NULL OR w.project_id=?2)",
            [id, p],
        ),
        "blocker" => exists(
            conn,
            "SELECT 1 FROM blockers b LEFT JOIN work_items w ON w.id=b.root_id WHERE b.id=?1 AND (b.root_id IS NULL OR w.project_id=?2)",
            [id, p],
        ),
        "resource" => exists(conn, "SELECT 1 FROM resources WHERE id=?1", [id]),
        other => Err(invalid(format!("unknown subject kind {other}"))),
    }
}

// Help backing bodies remain point-readable and retained, but are not user/work outputs.
// Bind the SQL classification to the actual store host, not a title/tag/caller-selected value.
impl Snapshot<'_> {
    fn artifact_host_actor(&self) -> &ActorId {
        self.host_actor
    }
}
impl Unit<'_> {
    fn artifact_host_actor(&self) -> &ActorId {
        &self.info.host_actor
    }
}

macro_rules! artifact_reads {
    ($t:ty) => {
        impl ArtifactRead for $t {
            fn project_exists(&self, project: &ProjectId) -> Result<bool, StorageError> {
                exists(self.conn, "SELECT 1 FROM projects WHERE id=?1 AND lifecycle<>'Deleted'", [project.0.as_str()])
            }
            fn subject_exists(
                &self,
                project: &ProjectId,
                subject: &SubjectKey,
            ) -> Result<bool, StorageError> {
                subject_exists(self.conn, project, subject)
            }
            fn artifact(&self, id: &Id) -> Result<Option<StoredArtifact>, StorageError> {
                artifact(self.conn, id)
            }
            fn version(&self, id: &Id) -> Result<Option<StoredVersion>, StorageError> {
                version(self.conn, id)
            }
            fn versions(&self, artifact: &Id) -> Result<Vec<StoredVersion>, StorageError> {
                versions(self.conn, artifact)
            }
            fn version_by_operation(
                &self,
                op: &OperationId,
            ) -> Result<Option<StoredVersion>, StorageError> {
                version_by_operation(self.conn, op)
            }
            fn list(
                &self,
                project: &ProjectId,
                tags: &[String],
                after: Option<&ListPosition>,
                limit: u16,
            ) -> Result<Vec<StoredArtifact>, StorageError> {
                list(self.conn, self.artifact_host_actor(), project, tags, after, limit)
            }
            fn list_workspace(
                &self,
                owner: Option<&ActorId>,
                tags: &[String],
                after: Option<&ListPosition>,
                limit: u16,
            ) -> Result<Vec<StoredArtifact>, StorageError> {
                list_workspace(self.conn, self.artifact_host_actor(), owner, tags, after, limit)
            }
            fn tags(&self, artifact: &Id) -> Result<Vec<String>, StorageError> {
                tags(self.conn, artifact)
            }
            fn shares(&self, artifact: &Id) -> Result<Vec<ProjectId>, StorageError> {
                shares(self.conn, artifact)
            }
            fn repository_text_readers(
                &self,
                artifact: &Id,
            ) -> Result<Vec<ProjectId>, StorageError> {
                let mut s = db(self.conn.prepare_cached(
                    "SELECT DISTINCT pr.project_id FROM repository_texts t
                     JOIN project_repositories pr ON pr.repository_id=t.repository_id
                     LEFT JOIN repository_details d ON d.repository_id=t.repository_id
                     WHERE t.artifact_id=?1 AND COALESCE(d.lifecycle,'Active')<>'Deleted'
                     ORDER BY pr.project_id",
                ))?;
                let rows = db(db(s.query_map([artifact.as_str()], |r| r.get::<_, String>(0)))?
                    .collect::<rusqlite::Result<Vec<_>>>())?;
                rows.into_iter().map(|p| id_of(p).map(ProjectId)).collect()
            }
            fn links(&self, artifact: &Id) -> Result<Vec<ArtifactLink>, StorageError> {
                links(self.conn, artifact)
            }
            fn linked_to(&self, subject: &SubjectKey) -> Result<Vec<ArtifactLink>, StorageError> {
                linked_to(self.conn, subject)
            }
            fn blob(&self, digest: &str) -> Result<Option<BlobRecord>, StorageError> {
                blob(self.conn, digest)
            }
            fn live_references(&self, digest: &str) -> Result<i64, StorageError> {
                count(
                    self.conn,
                    "SELECT (SELECT COUNT(*) FROM artifact_versions WHERE blob_digest=?1 AND deleted_at IS NULL) + (SELECT COUNT(*) FROM graph_source_captures WHERE blob_digest=?1) + (SELECT COUNT(*) FROM convention_import_artifacts a JOIN convention_imports d ON d.operation_id=a.operation_id WHERE a.blob_digest=?1 AND d.phase!='Published')",
                    [digest],
                )
            }
            fn pinned(&self, digest: &str) -> Result<bool, StorageError> {
                exists(
                    self.conn,
                    "SELECT 1 FROM artifact_blob_pins WHERE blob_digest=?1 LIMIT 1",
                    [digest],
                )
            }
            fn blobs_in(
                &self,
                state: BlobState,
                after: Option<&str>,
                limit: u16,
            ) -> Result<Vec<BlobRecord>, StorageError> {
                blobs_in(self.conn, state, after, limit)
            }
            fn project_usage(&self, project: &ProjectId) -> Result<i64, StorageError> {
                count(
                    self.conn,
                    "SELECT COALESCE(SUM(b.byte_count),0) FROM blobs b WHERE b.digest IN (
                       SELECT v.blob_digest FROM artifact_versions v JOIN artifacts a ON a.id=v.artifact_id
                       WHERE a.current_project_id=?1 AND a.retention='Explicit' AND v.deleted_at IS NULL)",
                    [project.0.as_str()],
                )
            }
            fn workspace_usage(&self) -> Result<i64, StorageError> {
                count(
                    self.conn,
                    "SELECT COALESCE(SUM(b.byte_count),0) FROM blobs b WHERE b.digest IN (
                       SELECT v.blob_digest FROM artifact_versions v JOIN artifacts a ON a.id=v.artifact_id
                       WHERE a.scope_kind='Workspace' AND a.retention='Explicit' AND v.deleted_at IS NULL)",
                    [],
                )
            }
            fn receipt_usage(&self, project: Option<&ProjectId>) -> Result<i64, StorageError> {
                count(
                    self.conn,
                    "SELECT COALESCE(SUM(b.byte_count),0) FROM blobs b WHERE b.digest IN (
                       SELECT v.blob_digest FROM artifact_versions v JOIN artifacts a ON a.id=v.artifact_id
                       WHERE a.retention='Receipt' AND v.deleted_at IS NULL
                         AND a.current_project_id IS ?1)",
                    [project.map(|p| p.0.as_str())],
                )
            }
            fn unresolved_staging(
                &self,
                after: Option<&OperationId>,
                limit: u16,
            ) -> Result<Vec<OperationId>, StorageError> {
                let limit = bounded(limit)?;
                let mut s = db(self.conn.prepare_cached(&format!(
                    "SELECT id FROM operations WHERE capability='{STAGE_CAPABILITY}'
                     AND state NOT IN ({TERMINAL}) AND id>?1 ORDER BY id LIMIT ?2"
                )))?;
                let rows = db(db(s.query_map(
                    params![after.map_or("", |a| a.0.as_str()), limit],
                    |r| r.get::<_, String>(0),
                ))?
                .collect::<rusqlite::Result<Vec<_>>>())?;
                rows.into_iter()
                    .map(|i| id_of(i).map(OperationId))
                    .collect()
            }
        }
    };
}

/// Terminal operation states (an unresolved staging operation is in none of them).
const TERMINAL: &str = "'ObservedComplete','Failed','CancelledBeforeDispatch'";

artifact_reads!(Snapshot<'_>);
artifact_reads!(Unit<'_>);

impl ArtifactRepository for Unit<'_> {
    fn put_blob(
        &mut self,
        digest: &str,
        byte_count: i64,
        generation: &str,
        relative_path: &str,
    ) -> Result<BlobRecord, StorageError> {
        if Id::new(generation).is_err() {
            return Err(invalid("blob generation must be an opaque identifier"));
        }
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(invalid("blob digest must be 64 lower-case hex characters"));
        }
        if byte_count < 0 {
            return Err(invalid("blob byte count must be non-negative"));
        }
        match blob(self.conn, digest)? {
            Some(b) if b.byte_count != byte_count => {
                return Err(invalid("blob byte count differs for the same digest"));
            }
            Some(_) => {
                db(db(self.conn.prepare_cached(
                    "UPDATE blobs SET state='Present',generation=?2,relative_path=?3,claim_owner=NULL,last_error=NULL WHERE digest=?1",
                ))?
                .execute(params![digest, generation, relative_path]))?;
            }
            None => {
                db(db(self.conn.prepare_cached(
                    "INSERT INTO blobs(digest,byte_count,generation,relative_path,state) VALUES(?1,?2,?3,?4,'Present')",
                ))?
                .execute(params![digest, byte_count, generation, relative_path]))?;
            }
        }
        blob(self.conn, digest)?.ok_or_else(|| corrupt("blob vanished inside its transaction"))
    }

    fn set_blob_state(
        &mut self,
        digest: &str,
        expected: BlobState,
        to: BlobState,
        error: Option<&str>,
    ) -> Result<BlobRecord, StorageError> {
        let n = db(db(self.conn.prepare_cached(
            "UPDATE blobs SET state=?1,last_error=?2 WHERE digest=?3 AND state=?4",
        ))?
        .execute(params![to.as_str(), error, digest, expected.as_str()]))?;
        if n != 1 {
            return Err(invalid(format!(
                "blob {digest} is not {}",
                expected.as_str()
            )));
        }
        blob(self.conn, digest)?.ok_or_else(|| corrupt("blob vanished inside its transaction"))
    }

    fn insert_artifact(&mut self, a: &StoredArtifact) -> Result<(), StorageError> {
        if !a.provenance.is_object() {
            return Err(invalid("artifact provenance must be a JSON object"));
        }
        if a.owner.is_none()
            && (a.scope == ArtifactScope::Workspace || a.retention == ArtifactRetention::Receipt)
        {
            return Err(invalid(
                "a workspace artifact or a receipt reply needs an owning principal",
            ));
        }
        for project in [a.origin_project_id.as_ref(), a.scope.project()]
            .into_iter()
            .flatten()
        {
            if !exists(
                self.conn,
                "SELECT 1 FROM projects WHERE id=?1 AND lifecycle<>'Deleted'",
                [project.0.as_str()],
            )? {
                return Err(StorageError::NotFound {
                    kind: "project",
                    id: project.0.as_str().to_owned(),
                });
            }
        }
        if let Some(owner) = &a.owner
            && !exists(
                self.conn,
                "SELECT 1 FROM actors WHERE id=?1",
                [owner.0.as_str()],
            )?
        {
            return Err(StorageError::NotFound {
                kind: "actor",
                id: owner.0.as_str().to_owned(),
            });
        }
        if let Some(w) = &a.origin_work_id {
            let Some(origin) = &a.origin_project_id else {
                return Err(invalid("an origin work item needs its origin project"));
            };
            if !exists(
                self.conn,
                "SELECT 1 FROM work_items WHERE id=?1 AND project_id=?2",
                [w.0.as_str(), origin.0.as_str()],
            )? {
                return Err(StorageError::NotFound {
                    kind: "work_item",
                    id: w.0.as_str().to_owned(),
                });
            }
        }
        if artifact(self.conn, &a.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "artifact",
                id: a.id.as_str().to_owned(),
            });
        }
        let (scope_kind, current) = scope_columns(&a.scope);
        db(db(self.conn.prepare_cached(&format!(
            "INSERT INTO artifacts({ARTIFACT_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)"
        )))?
        .execute(params![
            a.id.as_str(),
            a.origin_project_id.as_ref().map(|p| p.0.as_str()),
            a.origin_work_id.as_ref().map(|w| w.0.as_str()),
            a.title,
            a.revision,
            current,
            a.scope_revision,
            a.reference_revision,
            a.current_version_id.as_ref().map(Id::as_str),
            a.state.as_str(),
            a.created_at_ms,
            a.provenance.to_string(),
            scope_kind,
            a.owner.as_ref().map(|o| o.0.as_str()),
            a.retention.as_str(),
        ]))?;
        Ok(())
    }

    fn insert_version(&mut self, v: &StoredVersion) -> Result<(), StorageError> {
        if !v.source.is_object() {
            return Err(invalid("version source must be a JSON object"));
        }
        match blob(self.conn, &v.blob_digest)? {
            Some(b) if b.byte_count == v.byte_count => {}
            Some(_) => return Err(invalid("version byte count differs from its blob")),
            None => {
                return Err(StorageError::NotFound {
                    kind: "blob",
                    id: v.blob_digest.clone(),
                });
            }
        }
        if version(self.conn, &v.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "artifact_version",
                id: v.id.as_str().to_owned(),
            });
        }
        db(db(self.conn.prepare_cached(
            "INSERT INTO artifact_versions(id,artifact_id,version_no,blob_digest,media_type,created_at,deleted_at,source_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        ))?
        .execute(params![
            v.id.as_str(),
            v.artifact_id.as_str(),
            v.version_no,
            v.blob_digest,
            v.media_type,
            v.created_at_ms,
            v.deleted_at_ms,
            v.source.to_string(),
        ]))?;
        Ok(())
    }

    fn update_artifact(&mut self, a: &StoredArtifact) -> Result<(), StorageError> {
        if let Some(p) = a.scope.project()
            && !exists(
                self.conn,
                "SELECT 1 FROM projects WHERE id=?1",
                [p.0.as_str()],
            )?
        {
            return Err(StorageError::NotFound {
                kind: "project",
                id: p.0.as_str().to_owned(),
            });
        }
        let (scope_kind, current) = scope_columns(&a.scope);
        let n = db(db(self.conn.prepare_cached(
            "UPDATE artifacts SET title=?1,revision=?2,current_project_id=?3,scope_revision=?4,reference_revision=?5,current_version_id=?6,state=?7,scope_kind=?9 WHERE id=?8",
        ))?
        .execute(params![
            a.title,
            a.revision,
            current,
            a.scope_revision,
            a.reference_revision,
            a.current_version_id.as_ref().map(Id::as_str),
            a.state.as_str(),
            a.id.as_str(),
            scope_kind,
        ]))?;
        if n != 1 {
            return Err(StorageError::NotFound {
                kind: "artifact",
                id: a.id.as_str().to_owned(),
            });
        }
        Ok(())
    }

    fn tombstone_version(&mut self, id: &Id, at_ms: i64) -> Result<(), StorageError> {
        let n = db(db(self.conn.prepare_cached(
            "UPDATE artifact_versions SET deleted_at=?1 WHERE id=?2 AND deleted_at IS NULL",
        ))?
        .execute(params![at_ms, id.as_str()]))?;
        if n != 1 {
            return Err(invalid(format!("version {} is not live", id.as_str())));
        }
        Ok(())
    }

    fn add_link(&mut self, link: &ArtifactLink) -> Result<bool, StorageError> {
        let n = db(db(self.conn.prepare_cached(
            "INSERT OR IGNORE INTO artifact_links(artifact_version_id,subject_kind,subject_id,purpose) VALUES(?1,?2,?3,?4)",
        ))?
        .execute(params![
            link.version_id.as_str(),
            link.subject.kind,
            link.subject.id.as_str(),
            link.purpose.as_str(),
        ]))?;
        Ok(n == 1)
    }

    fn set_tags(&mut self, artifact: &Id, tags: &[String]) -> Result<(), StorageError> {
        if !exists(
            self.conn,
            "SELECT 1 FROM artifacts WHERE id=?1",
            [artifact.as_str()],
        )? {
            return Err(StorageError::NotFound {
                kind: "artifact",
                id: artifact.as_str().to_owned(),
            });
        }
        db(db(self
            .conn
            .prepare_cached("DELETE FROM artifact_tags WHERE artifact_id=?1"))?
        .execute([artifact.as_str()]))?;
        let mut insert = db(self
            .conn
            .prepare_cached("INSERT INTO artifact_tags(artifact_id,tag) VALUES(?1,?2)"))?;
        for tag in tags {
            db(insert.execute([artifact.as_str(), tag.as_str()]))?;
        }
        Ok(())
    }

    fn add_share(&mut self, artifact: &Id, project: &ProjectId) -> Result<bool, StorageError> {
        let n = db(db(self.conn.prepare_cached(
            "INSERT OR IGNORE INTO artifact_shares(artifact_id,project_id) VALUES(?1,?2)",
        ))?
        .execute([artifact.as_str(), project.0.as_str()]))?;
        Ok(n == 1)
    }

    fn remove_share(&mut self, artifact: &Id, project: &ProjectId) -> Result<bool, StorageError> {
        let n = db(db(self.conn.prepare_cached(
            "DELETE FROM artifact_shares WHERE artifact_id=?1 AND project_id=?2",
        ))?
        .execute([artifact.as_str(), project.0.as_str()]))?;
        Ok(n == 1)
    }

    fn pin(&mut self, holder: &Id, digest: &str) -> Result<(), StorageError> {
        if blob(self.conn, digest)?.is_none() {
            return Err(StorageError::NotFound {
                kind: "blob",
                id: digest.to_owned(),
            });
        }
        db(db(self.conn.prepare_cached(
            "INSERT OR IGNORE INTO artifact_blob_pins(holder_id,blob_digest,created_at) VALUES(?1,?2,?3)",
        ))?
        .execute(params![holder.as_str(), digest, self.now]))?;
        Ok(())
    }

    fn claim_blob(
        &mut self,
        digest: &str,
        generation: &str,
        owner: &str,
    ) -> Result<bool, StorageError> {
        let n = db(db(self.conn.prepare_cached(
            "UPDATE blobs SET state='Unlinking',claim_owner=?3 WHERE digest=?1 AND generation=?2 AND state IN ('UnlinkPending','Retired')",
        ))?
        .execute(params![digest, generation, owner]))?;
        Ok(n == 1)
    }

    fn adopt_claim(
        &mut self,
        digest: &str,
        generation: &str,
        from_owner: &str,
        to_owner: &str,
    ) -> Result<bool, StorageError> {
        let n = db(db(self.conn.prepare_cached(
            "UPDATE blobs SET claim_owner=?4 WHERE digest=?1 AND generation=?2 AND state='Unlinking' AND claim_owner=?3",
        ))?
        .execute(params![digest, generation, from_owner, to_owner]))?;
        Ok(n == 1)
    }

    fn finish_claim(
        &mut self,
        digest: &str,
        generation: &str,
        owner: &str,
        error: Option<&str>,
    ) -> Result<bool, StorageError> {
        let to = if error.is_some() {
            "Retired"
        } else {
            "Unlinked"
        };
        let n = db(db(self.conn.prepare_cached(
            "UPDATE blobs SET state=?4,claim_owner=NULL,last_error=?5 WHERE digest=?1 AND generation=?2 AND state='Unlinking' AND claim_owner=?3",
        ))?
        .execute(params![digest, generation, owner, to, error]))?;
        Ok(n == 1)
    }

    fn release_pins(&mut self, holder: &Id) -> Result<i64, StorageError> {
        let n = db(db(self
            .conn
            .prepare_cached("DELETE FROM artifact_blob_pins WHERE holder_id=?1"))?
        .execute([holder.as_str()]))?;
        Ok(n as i64)
    }
}
