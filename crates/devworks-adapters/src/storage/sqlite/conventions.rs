//! Scoped immutable convention revisions, exact resource history and generation-fenced retrieval.
mod imports;
mod index;
use super::conn::db;
use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::*;
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::{ActorId, Check, Id, Revision, RevisionDimension};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};
use std::collections::HashSet;

fn invalid(s: &str) -> StorageError {
    StorageError::InvalidRecord(s.into())
}
fn conversion(
    index: usize,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(error))
}
fn identifier(row: &Row<'_>, index: usize) -> rusqlite::Result<Id> {
    Id::new(row.get::<_, String>(index)?)
        .map_err(|e| conversion(index, std::io::Error::other(e.to_string())))
}
fn value(row: &Row<'_>, index: usize) -> rusqlite::Result<Value> {
    serde_json::from_str(&row.get::<_, String>(index)?).map_err(|e| conversion(index, e))
}
fn optional_value(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<Value>> {
    row.get::<_, Option<String>>(index)?
        .map(|v| serde_json::from_str(&v).map_err(|e| conversion(index, e)))
        .transpose()
}
fn record(row: &Row<'_>) -> rusqlite::Result<StoredConvention> {
    Ok(StoredConvention {
        id: identifier(row, 0)?,
        scope: KnowledgeScope::parse(&row.get::<_, String>(1)?).map_err(|e| conversion(1, e))?,
        revision: row.get(2)?,
        slug: row.get(3)?,
        title: row.get(4)?,
        parent_id: row
            .get::<_, Option<String>>(5)?
            .map(|s| Id::new(s).map_err(|e| conversion(5, std::io::Error::other(e.to_string()))))
            .transpose()?,
        sort_order: row.get(6)?,
        lifecycle: row.get(7)?,
        content_ref: optional_value(row, 8)?,
        raw_content_ref: optional_value(row, 9)?,
        applicability: serde_json::from_value(value(row, 10)?).map_err(|e| conversion(10, e))?,
        provenance: value(row, 11)?,
        author_id: ActorId(identifier(row, 12)?),
        created_at_ms: row.get(13)?,
        updated_at_ms: row.get(14)?,
        legacy_approved: row.get(15)?,
    })
}
const COLUMNS: &str = "v.convention_id,v.scope_key,v.revision,v.slug,v.title,v.parent_id,v.sort_order,v.lifecycle,v.content_ref_json,v.raw_content_ref_json,v.applicability_json,v.provenance_json,v.author_id,v.created_at,v.updated_at,v.legacy_approved";
fn page(limit: u16) -> Result<i64, StorageError> {
    if limit == 0 || limit > MAX_PAGE {
        Err(invalid("convention page limit must be 1..=500"))
    } else {
        Ok(i64::from(limit))
    }
}
fn scopes(scopes: &[KnowledgeScope]) -> Result<String, StorageError> {
    if scopes.len() > usize::from(MAX_PAGE) {
        return Err(invalid("too many convention scopes"));
    }
    Ok(json!(scopes.iter().map(KnowledgeScope::key).collect::<Vec<_>>()).to_string())
}
fn current(conn: &Connection, id: &Id) -> Result<Option<StoredConvention>, StorageError> {
    db(conn.query_row(&format!("SELECT {COLUMNS} FROM convention_heads h JOIN convention_versions v ON v.convention_id=h.id AND v.revision=h.revision WHERE h.id=?1"),[id.as_str()],record).optional())
}
fn convention(
    conn: &Connection,
    id: &Id,
    revision: Option<i64>,
    allowed: &[KnowledgeScope],
) -> Result<Option<StoredConvention>, StorageError> {
    if revision.is_some_and(|r| r < 1) {
        return Err(invalid("convention revision must be positive"));
    }
    let allowed = scopes(allowed)?;
    db(conn.query_row(&format!("SELECT {COLUMNS} FROM convention_heads h JOIN convention_versions c ON c.convention_id=h.id AND c.revision=h.revision JOIN convention_versions v ON v.convention_id=h.id AND v.revision=coalesce(?2,h.revision) WHERE h.id=?1 AND c.scope_key IN(SELECT value FROM json_each(?3)) AND v.scope_key IN(SELECT value FROM json_each(?3))"),params![id.as_str(),revision,allowed],record).optional())
}
fn conventions(
    conn: &Connection,
    q: &ConventionQuery,
    after: Option<&ConventionPosition>,
    limit: u16,
) -> Result<Vec<StoredConvention>, StorageError> {
    let limit = page(limit)?;
    states(&q.lifecycles)?;
    let scope = scopes(&q.scopes)?;
    let filter=json!({"lifecycles":q.lifecycles,"applicability":q.applicability,"filterParent":q.parent_id.is_some(),"parent":q.parent_id.as_ref().and_then(|p|p.as_ref()).map(Id::as_str),"slug":q.slug,"title":q.title.as_ref().map(|s|s.to_lowercase()),"name":q.name_pattern.as_ref().map(|s|s.to_lowercase())}).to_string();
    let (position, order) = match q.order {
        ConventionOrder::Id => ("h.id>?3", "h.id"),
        ConventionOrder::Title => (
            "(v.title COLLATE BINARY,h.id)>(?6,?3)",
            "v.title COLLATE BINARY,h.id",
        ),
        ConventionOrder::Organization => (
            "(v.sort_order,v.title COLLATE BINARY,h.id)>(?5,?6,?3)",
            "v.sort_order,v.title COLLATE BINARY,h.id",
        ),
    };
    let sql = format!(
        "SELECT {COLUMNS} FROM convention_heads h JOIN convention_versions v ON v.convention_id=h.id AND v.revision=h.revision WHERE v.scope_key IN(SELECT value FROM json_each(?1)) AND (json_array_length(?2,'$.lifecycles')=0 OR v.lifecycle IN(SELECT value FROM json_each(?2,'$.lifecycles'))) AND NOT EXISTS(SELECT 1 FROM json_each(?2,'$.applicability') a WHERE a.value NOT IN(SELECT value FROM json_each(v.applicability_json))) AND (NOT json_extract(?2,'$.filterParent') OR v.parent_id IS json_extract(?2,'$.parent')) AND (json_extract(?2,'$.slug') IS NULL OR v.slug=json_extract(?2,'$.slug')) AND (json_extract(?2,'$.title') IS NULL OR instr(v.title_folded,json_extract(?2,'$.title'))>0) AND (json_extract(?2,'$.name') IS NULL OR convention_like(json_extract(?2,'$.name'),v.slug_folded) OR convention_like(json_extract(?2,'$.name'),v.title_folded)) AND (?3 IS NULL OR {position}) AND (?5 IS ?5) AND (?6 IS ?6) ORDER BY {order} LIMIT ?4"
    );
    let mut stmt = db(conn.prepare_cached(&sql))?;
    db(db(stmt.query_map(
        params![
            scope,
            filter,
            after.map(|p| p.id.as_str()),
            limit,
            after.map(|p| p.sort_order),
            after.map(|p| p.title.as_str())
        ],
        record,
    ))?
    .collect())
}

fn versions(
    conn: &Connection,
    id: &Id,
    allowed: &[KnowledgeScope],
    after: Option<i64>,
    limit: u16,
) -> Result<Vec<StoredConvention>, StorageError> {
    if after.is_some_and(|r| r < 0) {
        return Err(invalid("negative convention revision cursor"));
    }
    let limit = page(limit)?;
    let allowed = scopes(allowed)?;
    let mut stmt=db(conn.prepare_cached(&format!("SELECT {COLUMNS} FROM convention_heads h JOIN convention_versions c ON c.convention_id=h.id AND c.revision=h.revision JOIN convention_versions v ON v.convention_id=h.id WHERE h.id=?1 AND c.scope_key IN(SELECT value FROM json_each(?2)) AND v.scope_key IN(SELECT value FROM json_each(?2)) AND (?3 IS NULL OR v.revision<?3) ORDER BY v.revision DESC LIMIT ?4")))?;
    db(db(stmt.query_map(params![id.as_str(), allowed, after, limit], record))?.collect())
}
fn resource(row: &Row<'_>) -> rusqlite::Result<StoredConventionResource> {
    Ok(StoredConventionResource {
        id: identifier(row, 0)?,
        convention_id: identifier(row, 1)?,
        revision: row.get(2)?,
        convention_revision: row.get(3)?,
        name: row.get(4)?,
        content_ref: value(row, 5)?,
        author_id: ActorId(identifier(row, 6)?),
        created_at_ms: row.get(7)?,
        deleted_at_ms: row.get(8)?,
        lifecycle: row.get(9)?,
        media_type: row.get(10)?,
    })
}
fn resources(
    conn: &Connection,
    q: &ConventionResourceQuery,
    after: Option<&Id>,
    limit: u16,
    resource_id: Option<&Id>,
) -> Result<Vec<StoredConventionResource>, StorageError> {
    if q.convention_revision.is_some_and(|r| r < 1)
        || (q.convention_revision.is_some() && q.convention_id.is_none())
    {
        return Err(invalid(
            "resource history requires exact owner and positive revision",
        ));
    }
    states(&q.lifecycles)?;
    let limit = page(limit)?;
    let allowed = scopes(&q.scopes)?;
    let mut stmt=db(conn.prepare_cached("SELECT r.resource_id,r.convention_id,r.revision,r.convention_revision,r.name,r.content_ref_json,r.author_id,r.created_at,r.deleted_at,r.lifecycle,r.media_type FROM convention_resource_heads rh JOIN convention_heads h ON h.id=rh.convention_id JOIN convention_versions c ON c.convention_id=h.id AND c.revision=h.revision JOIN convention_versions requested ON requested.convention_id=h.id AND requested.revision=coalesce(?2,h.revision) JOIN convention_resource_versions r ON r.resource_id=rh.id AND r.revision=(SELECT max(p.revision) FROM convention_resource_versions p WHERE p.resource_id=rh.id AND p.convention_revision<=requested.revision) JOIN convention_versions origin ON origin.convention_id=r.convention_id AND origin.revision=r.convention_revision WHERE (?1 IS NULL OR h.id=?1) AND c.scope_key IN(SELECT value FROM json_each(?3)) AND requested.scope_key IN(SELECT value FROM json_each(?3)) AND origin.scope_key IN(SELECT value FROM json_each(?3)) AND (json_array_length(?4)=0 OR r.lifecycle IN(SELECT value FROM json_each(?4))) AND (?5 IS NULL OR rh.id>?5) AND (?7 IS NULL OR rh.id=?7) ORDER BY rh.id LIMIT ?6"))?;
    db(db(stmt.query_map(
        params![
            q.convention_id.as_ref().map(Id::as_str),
            q.convention_revision,
            allowed,
            json!(q.lifecycles).to_string(),
            after.map(Id::as_str),
            limit,
            resource_id.map(Id::as_str)
        ],
        resource,
    ))?
    .collect())
}
fn scope_revision(conn: &Connection, scope: &KnowledgeScope) -> Result<i64, StorageError> {
    Ok(db(conn
        .query_row(
            "SELECT revision FROM convention_scope_revisions WHERE scope_key=?1",
            [scope.key()],
            |r| r.get(0),
        )
        .optional())?
    .unwrap_or(1))
}
fn states(states: &[String]) -> Result<(), StorageError> {
    if states
        .iter()
        .any(|s| !matches!(s.as_str(), "Active" | "Retired" | "Deleted"))
    {
        Err(invalid("invalid convention lifecycle"))
    } else {
        Ok(())
    }
}
fn text(s: &str, min: usize, max: usize) -> bool {
    !s.contains('\0') && (min..=max).contains(&s.chars().count())
}
fn conflict(id: &Id, expected: i64, current: i64) -> StorageError {
    StorageError::conflict(
        RevisionDimension::Source,
        "convention",
        id.clone(),
        Revision::new(expected.max(1)).expect("positive"),
        Revision::new(current.max(1)).expect("positive"),
    )
}
fn atomic<T>(
    conn: &Connection,
    f: impl FnOnce() -> Result<T, StorageError>,
) -> Result<T, StorageError> {
    db(conn.execute_batch("SAVEPOINT convention_mutation"))?;
    match f() {
        Ok(v) => {
            db(conn.execute_batch("RELEASE convention_mutation"))?;
            Ok(v)
        }
        Err(e) => {
            db(conn.execute_batch("ROLLBACK TO convention_mutation; RELEASE convention_mutation"))?;
            Err(e)
        }
    }
}
fn backing(
    conn: &Connection,
    scope: &KnowledgeScope,
    reference: &Value,
) -> Result<ArtifactRef, StorageError> {
    let reference: ArtifactRef = serde_json::from_value(reference.clone())
        .map_err(|_| invalid("invalid convention artifact reference"))?;
    reference
        .check()
        .map_err(|_| invalid("invalid convention artifact reference"))?;
    let real:bool=db(conn.query_row("SELECT EXISTS(SELECT 1 FROM artifact_versions v JOIN artifacts a ON a.id=v.artifact_id JOIN blobs b ON b.digest=v.blob_digest WHERE v.id=?1 AND v.artifact_id=?2 AND v.blob_digest=?3 AND v.media_type=?4 AND b.byte_count=?5 AND v.deleted_at IS NULL AND a.state='Live' AND a.current_project_id IS ?6 AND a.scope_kind=CASE WHEN ?6 IS NULL THEN 'Workspace' ELSE 'Project' END AND b.state='Present')",params![reference.version_id.as_str(),reference.artifact_id.as_str(),reference.sha256.as_str(),reference.media_type,reference.byte_length.get(),scope.project().map(|p|p.0.as_str())],|r|r.get(0)))?;
    if !real {
        return Err(invalid(
            "convention artifact must be a live committed version in its exact scope",
        ));
    }
    Ok(reference)
}
fn validate(conn: &Connection, r: &StoredConvention) -> Result<(), StorageError> {
    states(std::slice::from_ref(&r.lifecycle))?;
    validate_provenance(r)?;
    if r.revision < 1
        || !(1..=200).contains(&r.slug.chars().count())
        || !(1..=500).contains(&r.title.chars().count())
        || r.created_at_ms < 0
        || r.updated_at_ms < r.created_at_ms
        || r.applicability.len() > 128
        || r.applicability.iter().any(|s| !text(s, 1, 256))
        || r.applicability.iter().collect::<HashSet<_>>().len() != r.applicability.len()
        || !r.provenance.is_object()
        || r.provenance.to_string().len() > 512 * 1024
    {
        return Err(invalid("invalid convention metadata"));
    }
    let collision:bool=db(conn.query_row("SELECT EXISTS(SELECT 1 FROM convention_heads h JOIN convention_versions v ON v.convention_id=h.id AND v.revision=h.revision WHERE h.id!=?1 AND v.scope_key=?2 AND v.slug=?3 AND v.parent_id IS ?4 AND v.lifecycle!='Deleted')",params![r.id.as_str(),r.scope.key(),r.slug,r.parent_id.as_ref().map(Id::as_str)],|v|v.get(0)))?;
    if r.lifecycle != "Deleted" && collision {
        return Err(invalid(
            "convention slug already belongs to a live scoped identity",
        ));
    }
    if let Some(parent) = &r.parent_id {
        let p = current(conn, parent)?.ok_or_else(|| invalid("convention parent is missing"))?;
        if parent == &r.id || p.scope != r.scope || p.lifecycle == "Deleted" {
            return Err(invalid("convention parent must be live in the same scope"));
        }
        let cycle:bool=db(conn.query_row("WITH RECURSIVE ancestry(id) AS(SELECT ?1 UNION SELECT v.parent_id FROM ancestry a JOIN convention_heads h ON h.id=a.id JOIN convention_versions v ON v.convention_id=h.id AND v.revision=h.revision WHERE v.parent_id IS NOT NULL) SELECT EXISTS(SELECT 1 FROM ancestry WHERE id=?2)",params![parent.as_str(),r.id.as_str()],|r|r.get(0)))?;
        if cycle {
            return Err(invalid("convention parent cycle"));
        }
    }
    Ok(())
}
fn insert_snapshot(
    conn: &Connection,
    r: &StoredConvention,
    boot: &Option<Id>,
    transaction: u64,
) -> Result<(), StorageError> {
    let boot = boot
        .as_ref()
        .ok_or_else(|| invalid("convention writer boot missing"))?;
    db(conn.execute("INSERT INTO convention_versions(convention_id,revision,scope_key,project_id,slug,title,title_folded,parent_id,sort_order,lifecycle,content_ref_json,raw_content_ref_json,applicability_json,provenance_json,author_id,created_at,updated_at,legacy_approved,content_version_id,raw_content_version_id,write_boot_id,write_transaction,slug_folded) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23)",params![r.id.as_str(),r.revision,r.scope.key(),r.scope.project().map(|p|p.0.as_str()),r.slug,r.title,r.title.to_lowercase(),r.parent_id.as_ref().map(Id::as_str),r.sort_order,r.lifecycle,r.content_ref.as_ref().map(Value::to_string),r.raw_content_ref.as_ref().map(Value::to_string),json!(r.applicability).to_string(),r.provenance.to_string(),r.author_id.0.as_str(),r.created_at_ms,r.updated_at_ms,r.legacy_approved,r.content_ref.as_ref().and_then(|v|v["versionId"].as_str()),r.raw_content_ref.as_ref().and_then(|v|v["versionId"].as_str()),boot.as_str(),transaction.to_string(),r.slug.to_lowercase()]))?;
    Ok(())
}
fn insert_resource(conn: &Connection, r: &StoredConventionResource) -> Result<(), StorageError> {
    db(conn.execute("INSERT INTO convention_resource_versions(resource_id,revision,convention_id,convention_revision,name,content_ref_json,author_id,created_at,deleted_at,lifecycle,content_version_id,media_type) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",params![r.id.as_str(),r.revision,r.convention_id.as_str(),r.convention_revision,r.name,r.content_ref.to_string(),r.author_id.0.as_str(),r.created_at_ms,r.deleted_at_ms,r.lifecycle,r.content_ref["versionId"].as_str(),r.media_type]))?;
    Ok(())
}
fn validate_resource(
    conn: &Connection,
    r: &StoredConventionResource,
    expected: i64,
    boot: &Option<Id>,
    transaction: u64,
) -> Result<(), StorageError> {
    let owner =
        current(conn, &r.convention_id)?.ok_or_else(|| invalid("resource owner missing"))?;
    if owner.revision != expected {
        return Err(conflict(&owner.id, expected, owner.revision));
    }
    if r.convention_revision != expected
        || (owner.lifecycle == "Deleted" && r.lifecycle != "Deleted")
        || !text(&r.name, 1, 255)
        || !(0..=200).contains(&r.media_type.chars().count())
        || r.revision < 1
        || r.created_at_ms < 0
        || r.deleted_at_ms.is_some_and(|t| t < r.created_at_ms)
        || (r.lifecycle == "Deleted") != r.deleted_at_ms.is_some()
    {
        return Err(invalid("invalid convention resource identity"));
    }
    states(std::slice::from_ref(&r.lifecycle))?;
    let boot = boot
        .as_ref()
        .ok_or_else(|| invalid("convention writer boot missing"))?;
    let same_transaction:bool=db(conn.query_row("SELECT write_boot_id=?3 AND write_transaction=?4 FROM convention_versions WHERE convention_id=?1 AND revision=?2",params![r.convention_id.as_str(),expected,boot.as_str(),transaction.to_string()],|r|r.get(0)))?;
    if !same_transaction {
        return Err(invalid(
            "resource change must follow a new owner revision in the same transaction",
        ));
    }
    Ok(())
}
macro_rules! reads {
    ($ty:ty) => {
        impl ConventionRead for $ty {
            fn convention(
                &self,
                id: &Id,
                revision: Option<i64>,
                scopes: &[KnowledgeScope],
            ) -> Result<Option<StoredConvention>, StorageError> {
                convention(self.conn, id, revision, scopes)
            }
            fn conventions(
                &self,
                q: &ConventionQuery,
                after: Option<&ConventionPosition>,
                limit: u16,
            ) -> Result<Vec<StoredConvention>, StorageError> {
                conventions(self.conn, q, after, limit)
            }
            fn convention_versions(
                &self,
                id: &Id,
                scopes: &[KnowledgeScope],
                after: Option<i64>,
                limit: u16,
            ) -> Result<Vec<StoredConvention>, StorageError> {
                versions(self.conn, id, scopes, after, limit)
            }
            fn convention_resource(
                &self,
                owner: &Id,
                id: &Id,
                revision: Option<i64>,
                scopes: &[KnowledgeScope],
            ) -> Result<Option<StoredConventionResource>, StorageError> {
                let q = ConventionResourceQuery {
                    scopes: scopes.to_vec(),
                    convention_id: Some(owner.clone()),
                    convention_revision: revision,
                    ..Default::default()
                };
                Ok(resources(self.conn, &q, None, 1, Some(id))?
                    .into_iter()
                    .next())
            }
            fn convention_resources(
                &self,
                q: &ConventionResourceQuery,
                after: Option<&Id>,
                limit: u16,
            ) -> Result<Vec<StoredConventionResource>, StorageError> {
                resources(self.conn, q, after, limit, None)
            }
            fn convention_scope_revision(
                &self,
                scope: &KnowledgeScope,
            ) -> Result<i64, StorageError> {
                scope_revision(self.conn, scope)
            }
        }
    };
}
reads!(Snapshot<'_>);
reads!(Unit<'_>);
impl ConventionRepository for Unit<'_> {
    fn create_convention(&mut self, r: &StoredConvention) -> Result<(), StorageError> {
        if r.revision != 1 || current(self.conn, &r.id)?.is_some() {
            return Err(invalid(
                "new convention requires unused identity and revision one",
            ));
        }
        validate(self.conn, r)?;
        for reference in [&r.content_ref, &r.raw_content_ref].into_iter().flatten() {
            backing(self.conn, &r.scope, reference)?;
        }
        atomic(self.conn, || {
            db(self.conn.execute(
                "INSERT INTO convention_heads(id,revision) VALUES(?1,1)",
                [r.id.as_str()],
            ))?;
            insert_snapshot(self.conn, r, &self.info.boot_id, self.serial)
        })
    }
    fn revise_convention(
        &mut self,
        r: &StoredConvention,
        expected: i64,
    ) -> Result<(), StorageError> {
        let old = current(self.conn, &r.id)?.ok_or_else(|| invalid("convention missing"))?;
        if old.revision != expected {
            return Err(conflict(&r.id, expected, old.revision));
        }
        if Some(r.revision) != expected.checked_add(1)
            || r.created_at_ms != old.created_at_ms
            || r.updated_at_ms < old.updated_at_ms
            || r.provenance != old.provenance
        {
            return Err(invalid("convention revision/creation identity changed"));
        }
        validate(self.conn, r)?;
        for (reference, previous) in [
            (&r.content_ref, &old.content_ref),
            (&r.raw_content_ref, &old.raw_content_ref),
        ] {
            if (r.scope != old.scope || reference != previous)
                && let Some(reference) = reference
            {
                backing(self.conn, &r.scope, reference)?;
            }
        }
        atomic(self.conn, || {
            insert_snapshot(self.conn, r, &self.info.boot_id, self.serial)?;
            if r.content_ref != old.content_ref
                || r.scope != old.scope
                || r.title != old.title
                || r.lifecycle != old.lifecycle
            {
                index::invalidate(self.conn, &r.id, r.updated_at_ms)?;
            }
            db(self.conn.execute(
                "UPDATE convention_heads SET revision=?2 WHERE id=?1 AND revision=?3",
                params![r.id.as_str(), r.revision, expected],
            ))?;
            Ok(())
        })
    }
    fn add_convention_resource(
        &mut self,
        r: &StoredConventionResource,
        expected: i64,
    ) -> Result<(), StorageError> {
        validate_resource(self.conn, r, expected, &self.info.boot_id, self.serial)?;
        if r.revision != 1 {
            return Err(invalid("new convention resource requires revision one"));
        }
        let owner = current(self.conn, &r.convention_id)?.expect("validated owner");
        backing(self.conn, &owner.scope, &r.content_ref)?;
        atomic(self.conn, || {
            db(self.conn.execute(
                "INSERT INTO convention_resource_heads(id,convention_id,revision) VALUES(?1,?2,1)",
                params![r.id.as_str(), r.convention_id.as_str()],
            ))?;
            insert_resource(self.conn, r)
        })
    }
    fn revise_convention_resource(
        &mut self,
        r: &StoredConventionResource,
        expected_resource: i64,
        expected: i64,
    ) -> Result<(), StorageError> {
        validate_resource(self.conn, r, expected, &self.info.boot_id, self.serial)?;
        let old:Option<(String,i64,i64,i64,String)>=db(self.conn.query_row("SELECT h.convention_id,h.revision,v.convention_revision,v.created_at,v.content_ref_json FROM convention_resource_heads h JOIN convention_resource_versions v ON v.resource_id=h.id AND v.revision=h.revision WHERE h.id=?1",[r.id.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional())?;
        let Some((owner, revision, owner_revision, created, reference)) = old else {
            return Err(invalid("convention resource missing"));
        };
        if revision != expected_resource {
            return Err(conflict(&r.id, expected_resource, revision));
        }
        if owner != r.convention_id.as_str()
            || r.created_at_ms != created
            || Some(r.revision) != revision.checked_add(1)
            || r.convention_revision <= owner_revision
        {
            return Err(invalid(
                "resource identity or immutable owner revision changed",
            ));
        }
        let owner = current(self.conn, &r.convention_id)?.expect("validated owner");
        let previous_scope: String = db(self.conn.query_row(
            "SELECT scope_key FROM convention_versions WHERE convention_id=?1 AND revision=?2",
            params![r.convention_id.as_str(), owner_revision],
            |r| r.get(0),
        ))?;
        if serde_json::from_str::<Value>(&reference)
            .map_err(|_| invalid("invalid old resource reference"))?
            != r.content_ref
            || previous_scope != owner.scope.key()
        {
            backing(self.conn, &owner.scope, &r.content_ref)?;
        }
        atomic(self.conn, || {
            insert_resource(self.conn, r)?;
            db(self.conn.execute(
                "UPDATE convention_resource_heads SET revision=?2 WHERE id=?1 AND revision=?3",
                params![r.id.as_str(), r.revision, expected_resource],
            ))?;
            Ok(())
        })
    }
    fn bump_convention_scope(&mut self, scope: &KnowledgeScope) -> Result<i64, StorageError> {
        let revision = scope_revision(self.conn, scope)?
            .checked_add(1)
            .ok_or_else(|| invalid("convention scope revision overflow"))?;
        db(self.conn.execute("INSERT INTO convention_scope_revisions(scope_key,project_id,revision) VALUES(?1,?2,?3) ON CONFLICT(scope_key) DO UPDATE SET revision=excluded.revision",params![scope.key(),scope.project().map(|p|p.0.as_str()),revision]))?;
        Ok(revision)
    }
}

/// SQLite's built-in LIKE treats NUL as the end of a string. Retained source metadata can
/// contain it, so this deterministic SQL predicate matches complete Unicode scalar strings.
pub(super) fn register_functions(conn: &Connection) -> Result<(), StorageError> {
    use rusqlite::functions::FunctionFlags;
    db(conn.create_scalar_function(
        "convention_like",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let pattern = ctx.get::<String>(0)?;
            let value = ctx.get::<String>(1)?;
            Ok(like_pattern(&pattern, &value))
        },
    ))
}
fn like_pattern(pattern: &str, value: &str) -> bool {
    #[derive(Clone, Copy)]
    enum Token {
        Many,
        One,
        Literal(char),
    }
    let mut chars = pattern.chars();
    let mut tokens = Vec::new();
    while let Some(c) = chars.next() {
        tokens.push(match c {
            '%' => Token::Many,
            '_' => Token::One,
            '\\' => Token::Literal(chars.next().unwrap_or('\\')),
            c => Token::Literal(c),
        });
    }
    let value = value.chars().collect::<Vec<_>>();
    let (mut p, mut v) = (0, 0);
    let mut star = None;
    while v < value.len() {
        match tokens.get(p) {
            Some(Token::Literal(c)) if *c == value[v] => {
                p += 1;
                v += 1;
            }
            Some(Token::One) => {
                p += 1;
                v += 1;
            }
            Some(Token::Many) => {
                p += 1;
                star = Some((p, v));
            }
            _ => {
                if let Some((position, matched)) = star {
                    if matched >= value.len() {
                        return false;
                    }
                    v = matched + 1;
                    p = position;
                    star = Some((position, v));
                } else {
                    return false;
                }
            }
        }
    }
    while matches!(tokens.get(p), Some(Token::Many)) {
        p += 1;
    }
    p == tokens.len()
}

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
enum Provenance {
    Authored {
        actor_id: ActorId,
    },
    Imported {
        actor_id: ActorId,
        source: Box<ArtifactRef>,
        schema: devworks_core::protocol::provider::SchemaRef,
        imported_at_ms: devworks_core::protocol::Counter,
    },
}
fn validate_provenance(row: &StoredConvention) -> Result<(), StorageError> {
    let value: Provenance = serde_json::from_value(row.provenance.clone())
        .map_err(|_| invalid("invalid closed convention provenance"))?;
    let actor = match value {
        Provenance::Authored { actor_id } => actor_id,
        Provenance::Imported {
            actor_id,
            source,
            schema,
            imported_at_ms,
        } => {
            source
                .check()
                .map_err(|_| invalid("invalid import source reference"))?;
            schema
                .check()
                .map_err(|_| invalid("invalid import schema reference"))?;
            if imported_at_ms.get() < 0 {
                return Err(invalid("invalid import timestamp"));
            }
            actor_id
        }
    };
    if row.revision == 1 && actor != row.author_id {
        return Err(invalid(
            "convention provenance actor differs from native author",
        ));
    }
    Ok(())
}
