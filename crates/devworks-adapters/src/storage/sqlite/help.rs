//! Offline Help: metadata-only pages, bounded Unicode passages and atomic generation publication.
use super::conn::db;
use super::repos::{Snapshot, Unit};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::packages::MAX_BUNDLE_BYTES;
use devworks_core::ports::storage::*;
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::HelpProvenance;
use devworks_core::protocol::{Check, Id, OperationId, Revision, RevisionDimension};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;
use std::collections::HashSet;

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
fn corrupt(message: &str) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, message)
}
fn id(value: String) -> Result<Id, StorageError> {
    Id::new(value).map_err(|_| corrupt("invalid Help identifier"))
}
fn json(value: &str) -> Result<Value, StorageError> {
    serde_json::from_str(value).map_err(|_| corrupt("invalid Help JSON"))
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn text(value: &str, min: usize, max: usize) -> bool {
    !value.contains('\0') && (min..=max).contains(&value.chars().count())
}
fn page(limit: u16) -> Result<(), StorageError> {
    if limit == 0 || limit > MAX_PAGE {
        Err(invalid("Help page limit out of bounds"))
    } else {
        Ok(())
    }
}
fn conflict(
    kind: &'static str,
    expected: i64,
    current: i64,
    dimension: RevisionDimension,
) -> StorageError {
    StorageError::conflict(
        dimension,
        kind,
        Id::new("help").expect("static"),
        Revision::new(expected.max(1)).expect("positive"),
        Revision::new(current.max(1)).expect("positive"),
    )
}
fn head(conn: &Connection) -> Result<StoredHelpHead, StorageError> {
    let row = db(conn.query_row("SELECT revision,app_version,registry_revision,source_digest,generation_id,generation_operation_id,updated_at FROM help_head WHERE singleton=1",[],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<String>>(5)?,r.get(6)?))))?;
    Ok(StoredHelpHead {
        revision: row.0,
        app_version: row.1,
        registry_revision: row.2,
        source_digest: row.3,
        generation_id: row.4.map(id).transpose()?,
        generation_operation_id: row.5.map(id).transpose()?.map(OperationId),
        updated_at_ms: row.6,
    })
}
fn check_heads(
    conn: &Connection,
    help: i64,
    registry: i64,
) -> Result<StoredHelpHead, StorageError> {
    if help < 1 || registry < 1 {
        return Err(invalid("Help revisions must be positive"));
    }
    let current = head(conn)?;
    if current.revision != help {
        return Err(conflict(
            "help",
            help,
            current.revision,
            RevisionDimension::Source,
        ));
    }
    let actual: i64 = db(conn.query_row(
        "SELECT revision FROM package_registry WHERE singleton=1",
        [],
        |r| r.get(0),
    ))?;
    if actual != registry {
        return Err(conflict(
            "package_registry",
            registry,
            actual,
            RevisionDimension::Registry,
        ));
    }
    Ok(current)
}
fn operation(conn: &Connection, operation: &OperationId) -> Result<String, StorageError> {
    let value: Option<(String, String)> = db(conn
        .query_row(
            "SELECT capability,state FROM operations WHERE id=?1",
            [operation.0.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional())?;
    match value {
        Some((cap, state)) if cap == "help.generate" => Ok(state),
        Some(_) => Err(invalid(
            "Help generation requires a real help.generate operation",
        )),
        None => Err(StorageError::NotFound {
            kind: "operation",
            id: operation.0.as_str().into(),
        }),
    }
}
const TOPIC_COLUMNS: &str = "t.generation_id,t.id,t.version,t.title,t.content_digest,t.content_ref_json,t.provenance_json,t.related_operations_json,t.screen_ids_json,t.source_kind,t.package_id,t.manifest_digest,t.character_count,t.topic_revision,t.availability,t.reason";
struct RawTopic {
    generation_id: String,
    id: String,
    version: String,
    title: String,
    content_digest: String,
    content_ref: Option<String>,
    provenance: String,
    related_operations: String,
    screen_ids: String,
    source_kind: String,
    package_id: Option<String>,
    manifest_digest: Option<String>,
    character_count: i64,
    topic_revision: i64,
    availability: String,
    reason: Option<String>,
}
fn topic_row(r: &Row<'_>) -> rusqlite::Result<RawTopic> {
    Ok(RawTopic {
        generation_id: r.get(0)?,
        id: r.get(1)?,
        version: r.get(2)?,
        title: r.get(3)?,
        content_digest: r.get(4)?,
        content_ref: r.get(5)?,
        provenance: r.get(6)?,
        related_operations: r.get(7)?,
        screen_ids: r.get(8)?,
        source_kind: r.get(9)?,
        package_id: r.get(10)?,
        manifest_digest: r.get(11)?,
        character_count: r.get(12)?,
        topic_revision: r.get(13)?,
        availability: r.get(14)?,
        reason: r.get(15)?,
    })
}
fn topics<P: rusqlite::Params>(
    conn: &Connection,
    tail: &str,
    p: P,
) -> Result<Vec<StoredHelpTopic>, StorageError> {
    let mut stmt=db(conn.prepare_cached(&format!("SELECT {TOPIC_COLUMNS} FROM help_topics t JOIN help_generations g ON g.id=t.generation_id WHERE {tail}")))?;
    let rows = db(db(stmt.query_map(p, topic_row))?.collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter()
        .map(|r| {
            Ok(StoredHelpTopic {
                generation_id: id(r.generation_id)?,
                id: id(r.id)?,
                version: r.version,
                title: r.title,
                content_digest: r.content_digest,
                content_ref: r.content_ref.as_deref().map(json).transpose()?,
                provenance: json(&r.provenance)?,
                related_operations: serde_json::from_str(&r.related_operations)
                    .map_err(|_| corrupt("invalid Help operation list"))?,
                screen_ids: serde_json::from_str(&r.screen_ids)
                    .map_err(|_| corrupt("invalid Help screen list"))?,
                source_kind: r.source_kind,
                package_id: r.package_id.map(id).transpose()?,
                manifest_digest: r.manifest_digest,
                character_count: r.character_count,
                topic_revision: r.topic_revision,
                availability: r.availability,
                reason: r.reason,
            })
        })
        .collect()
}
fn staged_topics(
    conn: &Connection,
    generation: &Id,
    after: Option<&Id>,
    limit: u16,
) -> Result<Vec<StoredHelpTopic>, StorageError> {
    page(limit)?;
    topics(
        conn,
        "t.generation_id=?1 AND t.id>?2 ORDER BY t.id LIMIT ?3",
        params![
            generation.as_str(),
            after.map_or("", Id::as_str),
            i64::from(limit)
        ],
    )
}
macro_rules! help_reads { ($ty:ty) => { impl HelpRead for $ty {
    fn help_head(&self)->Result<StoredHelpHead,StorageError>{head(self.conn)}
    fn help_drafts(&self,after:Option<&Id>,limit:u16)->Result<Vec<StoredHelpDraft>,StorageError>{
        page(limit)?; let mut stmt=db(self.conn.prepare_cached("SELECT id,operation_id FROM help_generations WHERE state='Draft' AND id>?1 ORDER BY id LIMIT ?2"))?;
        let rows=db(db(stmt.query_map(params![after.map_or("",Id::as_str),i64::from(limit)],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))))?.collect::<rusqlite::Result<Vec<_>>>())?;
        rows.into_iter().map(|(g,o)|Ok(StoredHelpDraft{generation_id:id(g)?,operation_id:OperationId(id(o)?)})).collect()
    }
    fn help_topic(&self,generation:&Id,id:&Id)->Result<Option<StoredHelpTopic>,StorageError>{Ok(topics(self.conn,"g.state='Published' AND t.generation_id=?1 AND t.id=?2",params![generation.as_str(),id.as_str()])?.into_iter().next())}
    fn help_topics(&self,generation:&Id,filter:&HelpFilter,after:Option<&Id>,limit:u16)->Result<Vec<StoredHelpTopic>,StorageError>{
        page(limit)?;
        if filter.query.as_ref().is_some_and(|q| !text(q,1,4096) || *q!=q.to_lowercase()) || filter.screen_id.as_ref().is_some_and(|s|!text(s,1,256)){return Err(invalid("invalid Help filter"));}
        topics(self.conn,"g.state='Published' AND t.generation_id=?1 AND t.id>?2 AND (?3 IS NULL OR instr(t.search_text,?3)>0) AND (?4 IS NULL OR EXISTS(SELECT 1 FROM help_topic_screens s WHERE s.generation_id=t.generation_id AND s.topic_id=t.id AND s.screen_id=?4)) AND (?5 IS NULL OR t.package_id=?5) ORDER BY t.id LIMIT ?6",params![generation.as_str(),after.map_or("",Id::as_str),filter.query,filter.screen_id,filter.package_id.as_ref().map(Id::as_str),i64::from(limit)])
    }
    fn help_passage(&self,generation:&Id,id:&Id,offset:i64,chars:u32)->Result<Option<String>,StorageError>{
        if offset<0 || chars==0 || chars>MAX_HELP_PASSAGE_CHARS {return Err(invalid("Help passage bounds invalid"));}
        let start=offset.checked_add(1).ok_or_else(||invalid("Help passage offset overflow"))?;
        db(self.conn.query_row("SELECT substr(t.body,?3,?4) FROM help_topics t JOIN help_generations g ON g.id=t.generation_id WHERE g.state='Published' AND t.generation_id=?1 AND t.id=?2 AND t.availability='Available'",params![generation.as_str(),id.as_str(),start,i64::from(chars)],|r|r.get(0)).optional())
    }
} }; }
help_reads!(Snapshot<'_>);
help_reads!(Unit<'_>);

fn validate_document(
    conn: &Connection,
    document: &StoredHelpDocument,
) -> Result<(Option<ArtifactRef>, String, String, String, String), StorageError> {
    let t = &document.topic;
    if !text(&t.version, 1, 256)
        || !text(&t.title, 1, 256)
        || t.topic_revision < 1
        || t.character_count < 0
        || document.body.contains('\0')
        || document.search_text.contains('\0')
        || document.body.len() as u64 > MAX_BUNDLE_BYTES
        || t.reason.as_ref().is_some_and(|r| !text(r, 0, 65536))
    {
        return Err(invalid("invalid Help document bounds"));
    }
    if !["Bundled", "Package", "Generated"].contains(&t.source_kind.as_str())
        || !["Available", "Missing", "Stale", "Unavailable"].contains(&t.availability.as_str())
        || (t.source_kind == "Package") != t.package_id.is_some()
        || (t.source_kind == "Package") != t.manifest_digest.is_some()
        || t.manifest_digest.as_ref().is_some_and(|d| !digest(d))
    {
        return Err(invalid("invalid Help source metadata"));
    }
    for values in [&t.related_operations, &t.screen_ids] {
        let mut seen = HashSet::new();
        for value in values {
            if !text(value, 1, 256) || !seen.insert(value) {
                return Err(invalid("invalid or duplicate Help association"));
            }
        }
    }
    let reference = if t.availability == "Available" {
        if !digest(&t.content_digest)
            || sha256_hex(document.body.as_bytes()) != t.content_digest
            || i64::try_from(document.body.chars().count()).ok() != Some(t.character_count)
        {
            return Err(invalid(
                "Help body differs from its captured digest or Unicode length",
            ));
        }
        let reference: ArtifactRef = serde_json::from_value(
            t.content_ref
                .clone()
                .ok_or_else(|| invalid("available Help requires a real artifact"))?,
        )
        .map_err(|_| invalid("invalid Help artifact reference"))?;
        reference.validate().map_err(invalid)?;
        if reference.sha256.as_str() != t.content_digest
            || reference.byte_length.get() != document.body.len() as i64
        {
            return Err(invalid("Help artifact reference differs from its body"));
        }
        let real:bool=db(conn.query_row("SELECT EXISTS(SELECT 1 FROM artifact_versions v JOIN artifacts a ON a.id=v.artifact_id JOIN blobs b ON b.digest=v.blob_digest WHERE v.id=?1 AND v.artifact_id=?2 AND v.blob_digest=?3 AND v.media_type=?4 AND v.deleted_at IS NULL AND a.state='Live' AND b.byte_count=?5 AND b.state='Present')",params![reference.version_id.as_str(),reference.artifact_id.as_str(),reference.sha256.as_str(),reference.media_type,reference.byte_length.get()],|r|r.get(0)))?;
        if !real {
            return Err(invalid(
                "Help artifact reference is not a committed live version",
            ));
        }
        let provenance: HelpProvenance = serde_json::from_value(t.provenance.clone())
            .map_err(|_| invalid("invalid Help provenance"))?;
        provenance
            .check()
            .map_err(|_| invalid("invalid Help provenance"))?;
        if t.provenance["kind"].as_str() != Some(t.source_kind.as_str())
            || t.provenance["contentDigest"].as_str() != Some(t.content_digest.as_str())
            || (t.source_kind == "Package"
                && (t.provenance["packageId"].as_str() != t.package_id.as_ref().map(Id::as_str)
                    || t.provenance["manifestDigest"].as_str() != t.manifest_digest.as_deref()))
        {
            return Err(invalid("Help provenance differs from its source"));
        }
        Some(reference)
    } else {
        if t.content_ref.is_some()
            || !t.provenance.is_null()
            || !document.body.is_empty()
            || t.character_count != 0
            || (!t.content_digest.is_empty() && !digest(&t.content_digest))
        {
            return Err(invalid("unavailable Help must not claim verified content"));
        }
        None
    };
    Ok((
        reference,
        serde_json::to_string(&t.content_ref)
            .map_err(|_| invalid("invalid Help reference JSON"))?,
        t.provenance.to_string(),
        serde_json::to_string(&t.related_operations)
            .map_err(|_| invalid("invalid Help operation list"))?,
        serde_json::to_string(&t.screen_ids).map_err(|_| invalid("invalid Help screen list"))?,
    ))
}
fn generation(conn: &Connection, id: &Id) -> Result<Option<(String, String)>, StorageError> {
    db(conn
        .query_row(
            "SELECT state,operation_id FROM help_generations WHERE id=?1",
            [id.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional())
}
fn equal_topics(conn: &Connection, a: &Id, b: &Id) -> Result<bool, StorageError> {
    let fields = "id,version,title,content_digest,provenance_json,related_operations_json,screen_ids_json,source_kind,package_id,manifest_digest,availability,reason,character_count,topic_revision";
    db(conn.query_row(&format!("SELECT NOT EXISTS(SELECT {fields} FROM help_topics WHERE generation_id=?1 EXCEPT SELECT {fields} FROM help_topics WHERE generation_id=?2) AND NOT EXISTS(SELECT {fields} FROM help_topics WHERE generation_id=?2 EXCEPT SELECT {fields} FROM help_topics WHERE generation_id=?1)"),params![a.as_str(),b.as_str()],|r|r.get(0)))
}
impl HelpRepository for Unit<'_> {
    fn set_help_generation_operation(
        &mut self,
        expected_help_revision: i64,
        expected_registry_revision: i64,
        op: &OperationId,
    ) -> Result<StoredHelpHead, StorageError> {
        check_heads(
            self.conn,
            expected_help_revision,
            expected_registry_revision,
        )?;
        operation(self.conn, op)?;
        db(self.conn.execute(
            "UPDATE help_head SET generation_operation_id=?1,updated_at=?2 WHERE singleton=1",
            params![op.0.as_str(), self.now],
        ))?;
        head(self.conn)
    }
    fn staged_help_topic(
        &self,
        generation: &Id,
        id: &Id,
    ) -> Result<Option<StoredHelpTopic>, StorageError> {
        Ok(topics(
            self.conn,
            "t.generation_id=?1 AND t.id=?2",
            params![generation.as_str(), id.as_str()],
        )?
        .into_iter()
        .next())
    }
    fn staged_help_topics(
        &self,
        generation: &Id,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredHelpTopic>, StorageError> {
        staged_topics(self.conn, generation, after, limit)
    }
    fn stage_help_document(
        &mut self,
        document: &StoredHelpDocument,
    ) -> Result<StoredHelpTopic, StorageError> {
        let (reference, content, provenance, operations, screens) =
            validate_document(self.conn, document)?;
        let t = &document.topic;
        let current = head(self.conn)?;
        let op = current
            .generation_operation_id
            .ok_or_else(|| invalid("Help staging requires a generation operation claim"))?;
        operation(self.conn, &op)?;
        if let Some((state, owner)) = generation(self.conn, &t.generation_id)? {
            if state != "Draft" || owner != op.0.as_str() {
                return Err(invalid(
                    "Help generation is published or belongs to another operation",
                ));
            }
        }
        if let Some(old) = self.staged_help_topic(&t.generation_id, &t.id)? {
            let identical:bool=db(self.conn.query_row("SELECT body=?3 AND search_text=?4 FROM help_topics WHERE generation_id=?1 AND id=?2",params![t.generation_id.as_str(),t.id.as_str(),document.body,document.search_text],|r|r.get(0)))?;
            return if old == *t && identical {
                Ok(old)
            } else {
                Err(StorageError::Duplicate {
                    kind: "help_topic",
                    id: t.id.as_str().into(),
                })
            };
        }
        db(self.conn.execute("INSERT INTO help_generations(id,operation_id,state) VALUES(?1,?2,'Draft') ON CONFLICT(id) DO NOTHING",params![t.generation_id.as_str(),op.0.as_str()]))?;
        db(self.conn.execute("INSERT INTO help_topics(generation_id,id,version,title,content_digest,content_ref_json,artifact_id,version_id,provenance_json,related_operations_json,screen_ids_json,source_kind,package_id,manifest_digest,availability,reason,character_count,topic_revision,body,search_text) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",params![t.generation_id.as_str(),t.id.as_str(),t.version,t.title,t.content_digest,t.content_ref.as_ref().map(|_|content.as_str()),reference.as_ref().map(|r|r.artifact_id.as_str()),reference.as_ref().map(|r|r.version_id.as_str()),provenance,operations,screens,t.source_kind,t.package_id.as_ref().map(Id::as_str),t.manifest_digest,t.availability,t.reason,t.character_count,t.topic_revision,document.body,document.search_text]))?;
        for screen in &t.screen_ids {
            db(self.conn.execute(
                "INSERT INTO help_topic_screens(generation_id,topic_id,screen_id) VALUES(?1,?2,?3)",
                params![t.generation_id.as_str(), t.id.as_str(), screen],
            ))?;
        }
        Ok(t.clone())
    }
    fn publish_help_generation(
        &mut self,
        expected_help_revision: i64,
        expected_registry_revision: i64,
        expected_operation: Option<&OperationId>,
        new: &StoredHelpHead,
        expected_topic_count: i64,
    ) -> Result<StoredHelpHead, StorageError> {
        let current = check_heads(
            self.conn,
            expected_help_revision,
            expected_registry_revision,
        )?;
        if expected_operation.is_none()
            || current.generation_operation_id.as_ref() != expected_operation
            || new.generation_operation_id.as_ref() != expected_operation
        {
            return Err(invalid("Help generation operation claim changed"));
        }
        let op = expected_operation.expect("checked");
        operation(self.conn, op)?;
        if !text(&new.app_version, 1, 256)
            || !digest(&new.source_digest)
            || new.registry_revision != expected_registry_revision
            || new.updated_at_ms < 0
            || expected_topic_count < 0
        {
            return Err(invalid("invalid Help publication head"));
        }
        let gen_id = new
            .generation_id
            .as_ref()
            .ok_or_else(|| invalid("Help publication requires a generation"))?;
        if let Some((_, owner)) = generation(self.conn, gen_id)? {
            if owner != op.0.as_str() && current.generation_id.as_ref() != Some(gen_id) {
                return Err(invalid("Help draft belongs to another operation"));
            }
        }
        let count: i64 = db(self.conn.query_row(
            "SELECT count(*) FROM help_topics WHERE generation_id=?1",
            [gen_id.as_str()],
            |r| r.get(0),
        ))?;
        if count != expected_topic_count {
            return Err(invalid("Help generation is incomplete"));
        }
        if current.generation_id.is_some()
            && current.app_version == new.app_version
            && current.registry_revision == new.registry_revision
            && current.source_digest == new.source_digest
        {
            if !equal_topics(
                self.conn,
                current.generation_id.as_ref().expect("checked"),
                gen_id,
            )? {
                return Err(invalid(
                    "unchanged Help source identity has different content",
                ));
            }
            return Ok(current);
        }
        if expected_help_revision.checked_add(1) != Some(new.revision) {
            return Err(invalid("Help content revision must advance by one"));
        }
        if generation(self.conn, gen_id)?.is_some_and(|(state, _)| state != "Draft") {
            return Err(invalid("published Help generations cannot be replaced"));
        }
        db(self.conn.execute("INSERT INTO help_generations(id,operation_id,state) VALUES(?1,?2,'Draft') ON CONFLICT(id) DO NOTHING",params![gen_id.as_str(),op.0.as_str()]))?;
        db(self.conn.execute("UPDATE help_generations SET state='Published',app_version=?2,registry_revision=?3,source_digest=?4,help_revision=?5,published_at=?6 WHERE id=?1",params![gen_id.as_str(),new.app_version,new.registry_revision,new.source_digest,new.revision,new.updated_at_ms]))?;
        db(self.conn.execute("UPDATE help_head SET revision=?1,app_version=?2,registry_revision=?3,source_digest=?4,generation_id=?5,generation_operation_id=?6,updated_at=?7 WHERE singleton=1",params![new.revision,new.app_version,new.registry_revision,new.source_digest,gen_id.as_str(),op.0.as_str(),new.updated_at_ms]))?;
        head(self.conn)
    }
    fn discard_help_draft(
        &mut self,
        gen_id: &Id,
        op: &OperationId,
        limit: u16,
    ) -> Result<bool, StorageError> {
        page(limit)?;
        let Some((state, owner)) = generation(self.conn, gen_id)? else {
            return Ok(true);
        };
        if state != "Draft" || owner != op.0.as_str() {
            return Err(invalid(
                "only the owning operation's Help draft can be discarded",
            ));
        }
        if !["ObservedComplete", "Failed", "CancelledBeforeDispatch"]
            .contains(&operation(self.conn, op)?.as_str())
        {
            return Err(StorageError::Busy);
        }
        db(self.conn.execute("DELETE FROM help_topics WHERE generation_id=?1 AND id IN (SELECT id FROM help_topics WHERE generation_id=?1 ORDER BY id LIMIT ?2)",params![gen_id.as_str(),i64::from(limit)]))?;
        let remains: bool = db(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM help_topics WHERE generation_id=?1)",
            [gen_id.as_str()],
            |r| r.get(0),
        ))?;
        if !remains {
            db(self.conn.execute(
                "DELETE FROM help_generations WHERE id=?1",
                [gen_id.as_str()],
            ))?;
        }
        Ok(!remains)
    }
}
