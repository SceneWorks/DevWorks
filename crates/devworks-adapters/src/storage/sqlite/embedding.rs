//! DW-119 metadata only. Provider calls and vector validation happen outside the writer.
use super::conn::db;
use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::*;
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::{Id, OperationId, OperationState, Revision, RevisionDimension};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
fn corrupt(message: &str) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, message)
}
fn conflict(id: &Id, expected: i64, current: i64) -> StorageError {
    StorageError::conflict(
        RevisionDimension::Configuration,
        "embedding_space",
        id.clone(),
        Revision::new(expected.max(1)).expect("positive"),
        Revision::new(current).expect("stored positive"),
    )
}
fn encode(value: &impl Serialize) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|_| invalid("embedding metadata serialization"))
}
fn decode<T: DeserializeOwned>(value: String) -> Result<T, StorageError> {
    serde_json::from_str(&value).map_err(|_| corrupt("embedding metadata shape"))
}
const SUBJECT_COLUMNS: &str =
    "s.model_id,s.connection_id,s.upstream_model_id,s.created_connection_revision,s.created_at";
// This is current persisted identity, not eligibility: offline/disabled pricing authoring
// remains possible. Transport/qualification and the invocation's authority are separate owners.
const CANDIDATE_IDENTITY: &str = "json_extract(c.body_json,'$.accountRevision')=e.account_revision AND json_extract(c.body_json,'$.accountDigest') IS e.account_digest AND json_extract(c.body_json,'$.endpoint')=e.endpoint";
const KNOWN_EMBEDDING: &str = "e.profile='Embedding' AND EXISTS(SELECT 1 FROM json_each(e.discovered_json) d WHERE json_extract(d.value,'$.id')=s.upstream_model_id)";
fn subject_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(String, String, String, i64, i64, Option<String>)> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}
fn subject_of(
    raw: (String, String, String, i64, i64, Option<String>),
) -> Result<StoredEmbeddingPriceSubject, StorageError> {
    let id = |v: String| Id::new(v).map_err(|_| corrupt("embedding price subject identity"));
    Ok(StoredEmbeddingPriceSubject {
        model_id: id(raw.0)?,
        connection_id: id(raw.1)?,
        upstream_model_id: raw.2,
        created_connection_revision: raw.3,
        created_at_ms: raw.4,
        current_candidate_id: raw.5.map(id).transpose()?,
    })
}
fn subject_selection() -> String {
    format!(
        "SELECT {SUBJECT_COLUMNS},CASE WHEN {KNOWN_EMBEDDING} THEN (SELECT c.id FROM router_embedding_candidates c WHERE c.pricing_model_id=s.model_id AND {CANDIDATE_IDENTITY} ORDER BY c.id LIMIT 1) END FROM router_embedding_price_subjects s LEFT JOIN router_connections e ON e.id=s.connection_id"
    )
}
pub(super) fn price_subject(
    conn: &Connection,
    id: &Id,
) -> Result<Option<StoredEmbeddingPriceSubject>, StorageError> {
    db(conn
        .query_row(
            &(subject_selection() + " WHERE s.model_id=?1"),
            [id.as_str()],
            subject_row,
        )
        .optional())?
    .map(subject_of)
    .transpose()
}
pub(super) fn has_price_subject(conn: &Connection, id: &Id) -> Result<bool, StorageError> {
    db(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM router_embedding_price_subjects WHERE model_id=?1)",
        [id.as_str()],
        |r| r.get(0),
    ))
}
fn candidate_price_subject(
    conn: &Connection,
    id: &Id,
) -> Result<Option<StoredEmbeddingPriceSubject>, StorageError> {
    let sql = format!(
        "SELECT {SUBJECT_COLUMNS},c.id FROM router_embedding_candidates c JOIN router_embedding_price_subjects s ON s.model_id=c.pricing_model_id JOIN router_connections e ON e.id=s.connection_id WHERE c.id=?1 AND {KNOWN_EMBEDDING} AND {CANDIDATE_IDENTITY}"
    );
    db(conn.query_row(&sql, [id.as_str()], subject_row).optional())?
        .map(subject_of)
        .transpose()
}
fn price_subjects(
    conn: &Connection,
    after: Option<&Id>,
    limit: u16,
) -> Result<Vec<StoredEmbeddingPriceSubject>, StorageError> {
    if limit == 0 || limit > MAX_PAGE {
        return Err(invalid(
            "embedding price subject page limit must be 1..=MAX_PAGE",
        ));
    }
    let mut stmt = db(conn.prepare_cached(
        &(subject_selection() + " WHERE s.model_id>?1 ORDER BY s.model_id LIMIT ?2"),
    ))?;
    let rows = db(
        db(stmt.query_map(params![after.map_or("", Id::as_str), limit], subject_row))?
            .collect::<rusqlite::Result<Vec<_>>>(),
    )?;
    rows.into_iter().map(subject_of).collect()
}

fn candidate(conn: &Connection, id: &Id) -> Result<Option<StoredEmbeddingCandidate>, StorageError> {
    db(conn
        .query_row(
            "SELECT body_json FROM router_embedding_candidates WHERE id=?1",
            [id.as_str()],
            |r| r.get::<_, String>(0),
        )
        .optional())?
    .map(decode)
    .transpose()
}
fn qualification(
    conn: &Connection,
    id: &Id,
) -> Result<Option<StoredEmbeddingQualification>, StorageError> {
    db(conn.query_row("SELECT body_json FROM router_embedding_qualifications WHERE candidate_id=?1 ORDER BY revision DESC LIMIT 1", [id.as_str()], |r| r.get::<_, String>(0)).optional())?.map(decode).transpose()
}
fn selection(conn: &Connection) -> Result<StoredEmbeddingSelection, StorageError> {
    let (revision, candidate_id, space_id): (i64, Option<String>, Option<String>) = db(conn.query_row("SELECT revision,candidate_id,space_id FROM router_embedding_selection ORDER BY revision DESC LIMIT 1", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))))?;
    let id = |s: String| Id::new(s).map_err(|_| corrupt("embedding selection ID"));
    Ok(StoredEmbeddingSelection {
        revision,
        candidate_id: candidate_id.map(id).transpose()?,
        space_id: space_id.map(id).transpose()?,
    })
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn validate(c: &StoredEmbeddingCandidate) -> Result<(), StorageError> {
    if c.name.is_empty()
        || c.name.chars().count() > 256
        || c.name.contains('\0')
        || c.upstream_model_id.is_empty()
        || c.upstream_model_id.chars().count() > 4096
        || c.upstream_model_id.contains('\0')
        || !(1..=65536).contains(&c.dimensions)
        || c.account_revision < 1
        || c.created_at_ms < 0
        || !["Native", "Parameter"].contains(&c.dimension_mode.as_str())
        || !["L2", "Cosine"].contains(&c.metric.as_str())
        || c.element_type != "Float32"
        || !digest(&c.configuration_digest)
        || !digest(&c.model_digest)
        || !digest(&c.endpoint_digest)
        || !c.encoder_config.is_object()
    {
        return Err(invalid("invalid embedding candidate identity"));
    }
    let model_identity = serde_json::json!({"version":1,"connectionId":c.connection_id,"endpointProfile":"Embedding","endpoint":c.endpoint,"accountRevision":c.account_revision.to_string(),"accountDigest":c.account_digest,"upstreamModelId":c.upstream_model_id});
    let model = devworks_core::jcs::canonicalize(&model_identity)
        .map_err(|_| invalid("embedding model identity"))?;
    let expected_encoder = serde_json::json!({"version":1,"modelDigest":c.model_digest,"dimensions":c.dimensions,"dimensionMode":c.dimension_mode,"metric":c.metric,"elementType":c.element_type,"encodingFormat":"float"});
    if devworks_core::fingerprint::sha256_hex(model.as_bytes()) != c.model_digest
        || c.encoder_config != expected_encoder
        || c.space_id.as_str() != format!("embspace-{}", c.configuration_digest)
    {
        return Err(invalid("embedding semantic identity mismatch"));
    }
    let canonical = devworks_core::jcs::canonicalize(&c.encoder_config)
        .map_err(|_| invalid("embedding encoder configuration"))?;
    if devworks_core::fingerprint::sha256_hex(canonical.as_bytes()) != c.configuration_digest {
        return Err(invalid("embedding configuration digest mismatch"));
    }
    if devworks_core::fingerprint::sha256_hex(c.endpoint.as_bytes()) != c.endpoint_digest {
        return Err(invalid("embedding endpoint digest mismatch"));
    }
    Ok(())
}
/// Connection revisions fence an individual attempt, while immutable account and endpoint
/// identity determine whether a previously qualified space is still the same semantic source.
fn current_source(
    conn: &Connection,
    c: &StoredEmbeddingCandidate,
) -> Result<StoredConnection, StorageError> {
    let source = super::connections::connection(conn, &c.connection_id)?
        .ok_or_else(|| invalid("embedding connection absent"))?;
    if source.profile != "Embedding"
        || source.account_revision != c.account_revision
        || source.account_digest != c.account_digest
        || source.endpoint != c.endpoint
        || !source.enabled
        || source.lifecycle != "Connected"
        || !source
            .discovered
            .as_ref()
            .is_some_and(|models| models.iter().any(|m| m.id == c.upstream_model_id))
    {
        return Err(invalid("embedding source changed or unavailable"));
    }
    Ok(source)
}
fn append(conn: &Connection, q: &StoredEmbeddingQualification) -> Result<(), StorageError> {
    db(conn.execute("INSERT INTO router_embedding_qualifications(candidate_id,revision,state,operation_id,body_json,updated_at) VALUES(?1,?2,?3,?4,?5,?6)",params![q.candidate_id.as_str(),q.revision,serde_json::to_value(q.state).map_err(|_|invalid("qualification state"))?.as_str().ok_or_else(||invalid("qualification state"))?,q.probe.as_ref().map(|p|p.operation_id.0.as_str()),encode(q)?,q.updated_at_ms]))?;
    Ok(())
}
fn checked_qualification(
    conn: &Connection,
    id: &Id,
    expected: i64,
) -> Result<StoredEmbeddingQualification, StorageError> {
    let q = qualification(conn, id)?.ok_or_else(|| StorageError::NotFound {
        kind: "embedding_space",
        id: id.as_str().into(),
    })?;
    if expected != q.revision {
        return Err(conflict(id, expected, q.revision));
    }
    Ok(q)
}
fn next(q: &mut StoredEmbeddingQualification, now: i64) -> Result<(), StorageError> {
    q.revision = q
        .revision
        .checked_add(1)
        .ok_or_else(|| invalid("embedding revision overflow"))?;
    q.updated_at_ms = now;
    Ok(())
}
macro_rules! reads { ($t:ty) => { impl EmbeddingRead for $t {
    fn embedding_price_subject(&self,id:&Id)->Result<Option<StoredEmbeddingPriceSubject>,StorageError>{price_subject(self.conn,id)}
    fn embedding_price_subject_for_candidate(&self,id:&Id)->Result<Option<StoredEmbeddingPriceSubject>,StorageError>{candidate_price_subject(self.conn,id)}
    fn embedding_price_subjects(&self,after:Option<&Id>,limit:u16)->Result<Vec<StoredEmbeddingPriceSubject>,StorageError>{price_subjects(self.conn,after,limit)}
    fn embedding_inventory_revision(&self)->Result<i64,StorageError>{db(self.conn.query_row("SELECT revision FROM router_embedding_inventory WHERE singleton=1",[],|r|r.get(0)))}
    fn embedding_candidate(&self,id:&Id)->Result<Option<StoredEmbeddingCandidate>,StorageError>{candidate(self.conn,id)}
    fn embedding_candidates(&self,after:Option<&Id>,limit:u16)->Result<Vec<StoredEmbeddingCandidate>,StorageError>{
        if limit==0 || limit>MAX_PAGE{return Err(invalid("embedding page limit must be 1..=MAX_PAGE"));}
        let mut stmt=db(self.conn.prepare_cached("SELECT body_json FROM router_embedding_candidates WHERE id>?1 ORDER BY id LIMIT ?2"))?;
        let rows=db(db(stmt.query_map(params![after.map_or("",Id::as_str),limit],|r|r.get::<_,String>(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;
        rows.into_iter().map(decode).collect()
    }
    fn embedding_qualification(&self,id:&Id)->Result<Option<StoredEmbeddingQualification>,StorageError>{qualification(self.conn,id)}
    fn embedding_selection(&self)->Result<StoredEmbeddingSelection,StorageError>{selection(self.conn)}
}}; }
reads!(Snapshot<'_>);
reads!(Unit<'_>);

impl EmbeddingRepository for Unit<'_> {
    fn insert_embedding_candidate(
        &mut self,
        c: &StoredEmbeddingCandidate,
        expected_connection_revision: i64,
    ) -> Result<(), StorageError> {
        validate(c)?;
        if candidate(self.conn, &c.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "embedding_space",
                id: c.id.as_str().into(),
            });
        }
        let source = current_source(self.conn, c)?;
        if source.revision != expected_connection_revision {
            return Err(conflict(
                &c.connection_id,
                expected_connection_revision,
                source.revision,
            ));
        }
        // A canonical space ID can never name two semantic identities, even before qualification.
        let previous: Option<String> = db(self
            .conn
            .query_row(
                "SELECT body_json FROM router_embedding_candidates WHERE space_id=?1 LIMIT 1",
                [c.space_id.as_str()],
                |r| r.get(0),
            )
            .optional())?;
        if let Some(previous) = previous {
            let mut previous: StoredEmbeddingCandidate = decode(previous)?;
            previous.id = c.id.clone();
            previous.name = c.name.clone();
            previous.created_by = c.created_by.clone();
            previous.created_at_ms = c.created_at_ms;
            if previous != *c {
                return Err(invalid("embedding space identity collision"));
            }
        }
        let pricing_id = devworks_core::services::router::connections::discovered_model_id(
            "Embedding",
            &c.connection_id,
            &c.upstream_model_id,
        );
        if self.route_model(&pricing_id)?.is_some() || self.decision_model(&pricing_id)?.is_some() {
            return Err(invalid(
                "embedding price identity already belongs to another model kind",
            ));
        }
        if let Some(previous) = price_subject(self.conn, &pricing_id)?
            && (previous.connection_id != c.connection_id
                || previous.upstream_model_id != c.upstream_model_id)
        {
            return Err(invalid("embedding price subject identity collision"));
        }
        // A caller may catch a rejected candidate write and still commit its own unit. The
        // private map and candidate/qualification therefore share this existing savepoint.
        let savepoint = WriteTransaction::savepoint(self)?;
        let outcome = (|| {
            db(self.conn.execute("INSERT OR IGNORE INTO router_embedding_price_subjects(model_id,connection_id,upstream_model_id,created_connection_revision,created_at) VALUES(?1,?2,?3,?4,?5)",params![pricing_id.as_str(),c.connection_id.as_str(),c.upstream_model_id,source.revision,c.created_at_ms]))?;
            db(self.conn.execute("INSERT INTO router_embedding_candidates(id,space_id,configuration_digest,pricing_model_id,body_json,created_at) VALUES(?1,?2,?3,?4,?5,?6)",params![c.id.as_str(),c.space_id.as_str(),c.configuration_digest,pricing_id.as_str(),encode(c)?,c.created_at_ms]))?;
            append(
                self.conn,
                &StoredEmbeddingQualification {
                    candidate_id: c.id.clone(),
                    revision: 1,
                    state: EmbeddingQualificationState::Unqualified,
                    reason: None,
                    probe: None,
                    evidence: None,
                    updated_at_ms: self.now,
                },
            )
        })();
        match outcome {
            Ok(()) => {
                WriteTransaction::release(self, savepoint)?;
                Ok(())
            }
            Err(error) => {
                WriteTransaction::rollback_to(self, savepoint)?;
                Err(error)
            }
        }
    }

    fn start_embedding_qualification(
        &mut self,
        id: &Id,
        expected: i64,
        p: &StoredEmbeddingProbe,
    ) -> Result<StoredEmbeddingQualification, StorageError> {
        let mut q = checked_qualification(self.conn, id, expected)?;
        let c = candidate(self.conn, id)?.ok_or_else(|| corrupt("embedding candidate absent"))?;
        let source = current_source(self.conn, &c)?;
        if p.candidate_revision != expected
            || source.revision != p.connection_revision
            || p.store_id != self.info.store_id
            || p.restore_epoch != self.info.restore_epoch
            || !p.source_context.is_object()
            || !devworks_core::authority::service::fences_hold(self, &p.authority_fences)?
        {
            return Err(StorageError::ScopeDenied);
        }
        let reused:bool=db(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM router_embedding_qualifications WHERE operation_id=?1 AND candidate_id<>?2) OR EXISTS(SELECT 1 FROM router_embedding_qualifications WHERE operation_id IS NOT NULL AND json_extract(body_json,'$.probe.requestId')=?3 AND operation_id<>?1) OR EXISTS(SELECT 1 FROM router_embedding_qualifications WHERE operation_id IS NOT NULL AND json_extract(body_json,'$.probe.admissionKey')=?4 AND operation_id<>?1)",
            params![p.operation_id.0.as_str(),id.as_str(),p.request_id.as_str(),p.admission_key.as_str()],|r|r.get(0)))?;
        if reused {
            return Err(invalid(
                "embedding probe identity already belongs to another intent",
            ));
        }
        if let Some(previous) = &q.probe {
            let previous = self
                .operation(&previous.operation_id)?
                .ok_or_else(|| corrupt("embedding probe operation absent"))?;
            if !is_terminal(previous.state) {
                return Err(invalid("previous embedding probe unresolved"));
            }
            if previous.id == p.operation_id {
                return Err(invalid("embedding probe intent cannot be reused"));
            }
        }
        let operation = self
            .operation(&p.operation_id)?
            .ok_or_else(|| invalid("embedding probe requires durable operation"))?;
        let delivery = self
            .entry(&p.delivery_id)?
            .ok_or_else(|| invalid("embedding probe requires durable delivery"))?;
        if operation.state != OperationState::IntentRecorded
            || delivery.operation_id.as_ref() != Some(&p.operation_id)
        {
            return Err(invalid("embedding probe intent binding"));
        }
        q.state = EmbeddingQualificationState::Qualifying;
        q.reason = None;
        q.probe = Some(p.clone());
        q.evidence = None;
        next(&mut q, self.now)?;
        append(self.conn, &q)?;
        Ok(q)
    }
    fn stage_embedding_probe_evidence(
        &mut self,
        id: &Id,
        expected: i64,
        operation_id: &OperationId,
        fence: &AttemptFence,
        evidence: &StoredEmbeddingEvidence,
    ) -> Result<StoredEmbeddingQualification, StorageError> {
        let mut q = checked_qualification(self.conn, id, expected)?;
        if !matches!(
            q.state,
            EmbeddingQualificationState::Qualifying | EmbeddingQualificationState::Unknown
        ) {
            return Err(invalid("embedding probe not qualifying"));
        }
        let p = q
            .probe
            .clone()
            .ok_or_else(|| invalid("embedding probe absent"))?;
        if &p.operation_id != operation_id
            || p.store_id != self.info.store_id
            || p.restore_epoch != self.info.restore_epoch
            || !devworks_core::authority::service::fences_hold(self, &p.authority_fences)?
        {
            return Err(StorageError::ScopeDenied);
        }
        let operation = self
            .operation(operation_id)?
            .ok_or_else(|| invalid("embedding operation absent"))?;
        if !matches!(
            operation.state,
            OperationState::Dispatching
                | OperationState::OutcomeUnknown
                | OperationState::NeedsInput
        ) || operation
            .latest_attempt()
            .map(AttemptRecord::fence)
            .as_ref()
            != Some(fence)
        {
            return Err(StorageError::LeaseLost {
                kind: "embedding_probe",
                id: id.as_str().into(),
            });
        }
        if probe_cancelled(self, &p)? {
            return Err(StorageError::Cancelled);
        }
        check_evidence(self, id, &p, evidence, true)?;
        if let Some(previous) = q.evidence.as_ref() {
            if encode(previous)? != encode(evidence)? {
                return Err(invalid("embedding evidence immutable"));
            }
            return Ok(q);
        }
        q.evidence = Some(evidence.clone());
        next(&mut q, self.now)?;
        append(self.conn, &q)?;
        Ok(q)
    }
    fn finish_embedding_qualification(
        &mut self,
        id: &Id,
        expected: i64,
        operation_id: &OperationId,
        fence: Option<&AttemptFence>,
        state: EmbeddingQualificationState,
        reason: Option<&str>,
    ) -> Result<StoredEmbeddingQualification, StorageError> {
        let mut q = checked_qualification(self.conn, id, expected)?;
        let p = q
            .probe
            .clone()
            .ok_or_else(|| invalid("embedding probe absent"))?;
        if q.state != EmbeddingQualificationState::Qualifying
            && q.state != EmbeddingQualificationState::Unknown
        {
            return Err(invalid("embedding probe already settled"));
        }
        if &p.operation_id != operation_id
            || p.store_id != self.info.store_id
            || p.restore_epoch != self.info.restore_epoch
        {
            return Err(StorageError::ScopeDenied);
        }
        // Recording an exact negative journal fact cannot activate a space or change proof.
        // Revoking the original proposer must prevent publication, not erase failure history.
        if state == EmbeddingQualificationState::Qualified
            && !devworks_core::authority::service::fences_hold(self, &p.authority_fences)?
        {
            return Err(StorageError::ScopeDenied);
        }
        let operation = self
            .operation(operation_id)?
            .ok_or_else(|| invalid("embedding operation absent"))?;
        let cancelled = probe_cancelled(self, &p)?;
        let no_attempt_cancel = state == EmbeddingQualificationState::Cancelled
            && operation.state == OperationState::CancelledBeforeDispatch
            && cancelled;
        if operation
            .latest_attempt()
            .map(AttemptRecord::fence)
            .as_ref()
            != fence
            || (fence.is_none() && !no_attempt_cancel)
        {
            return Err(StorageError::LeaseLost {
                kind: "embedding_probe",
                id: id.as_str().into(),
            });
        }
        if state == EmbeddingQualificationState::Qualified && cancelled {
            return Err(StorageError::Cancelled);
        }
        let valid_state = match state {
            EmbeddingQualificationState::Qualified => {
                operation.state == OperationState::ObservedComplete
            }
            EmbeddingQualificationState::Failed => operation.state == OperationState::Failed,
            EmbeddingQualificationState::Unknown => matches!(
                operation.state,
                OperationState::OutcomeUnknown | OperationState::NeedsInput
            ),
            EmbeddingQualificationState::Cancelled => {
                operation.state == OperationState::CancelledBeforeDispatch
                    || (matches!(
                        operation.state,
                        OperationState::ObservedComplete | OperationState::Failed
                    ) && cancelled)
            }
            _ => false,
        };
        if !valid_state || (state == EmbeddingQualificationState::Qualified && q.evidence.is_none())
        {
            return Err(invalid("embedding qualification journal outcome mismatch"));
        }
        let c = candidate(self.conn, id)?.ok_or_else(|| corrupt("embedding candidate absent"))?;
        if state == EmbeddingQualificationState::Qualified {
            let e = q
                .evidence
                .as_ref()
                .ok_or_else(|| invalid("embedding evidence absent"))?;
            check_evidence(self, id, &p, e, true)?;
            let registered:Option<(String,i64,String,String)>=db(self.conn.query_row("SELECT model_digest,dimensions,metric,element_type FROM embedding_spaces WHERE id=?1",[c.space_id.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional())?;
            let identity = (
                c.model_digest.clone(),
                c.dimensions,
                c.metric.clone(),
                c.element_type.clone(),
            );
            if registered.as_ref().is_some_and(|r| r != &identity) {
                return Err(invalid("registered embedding space identity collision"));
            }
            db(self.conn.execute("INSERT OR IGNORE INTO embedding_spaces(id,model_digest,dimensions,metric,element_type) VALUES(?1,?2,?3,?4,?5)",params![c.space_id.as_str(),c.model_digest,c.dimensions,c.metric,c.element_type]))?;
        }
        if reason.is_some_and(|r| r.chars().count() > 65536 || r.contains('\0')) {
            return Err(invalid("embedding reason bound"));
        }
        q.state = state;
        q.reason = reason.map(str::to_owned);
        next(&mut q, self.now)?;
        append(self.conn, &q)?;
        Ok(q)
    }
    fn select_embedding_candidate(
        &mut self,
        expected: i64,
        selected: Option<(&Id, i64)>,
    ) -> Result<StoredEmbeddingSelection, StorageError> {
        let current = selection(self.conn)?;
        if expected != current.revision {
            return Err(conflict(
                &Id::new("embedding_selection").expect("static ID"),
                expected,
                current.revision,
            ));
        }
        let candidate = if let Some((id, revision)) = selected {
            let q = checked_qualification(self.conn, id, revision)?;
            if q.state != EmbeddingQualificationState::Qualified || q.evidence.is_none() {
                return Err(invalid("embedding space not qualified"));
            }
            let c =
                candidate(self.conn, id)?.ok_or_else(|| corrupt("embedding candidate absent"))?;
            current_source(self.conn, &c)?;
            let p = q
                .probe
                .as_ref()
                .ok_or_else(|| corrupt("qualified embedding probe absent"))?;
            if p.store_id != self.info.store_id
                || p.restore_epoch != self.info.restore_epoch
                || !devworks_core::authority::service::fences_hold(self, &p.authority_fences)?
            {
                return Err(StorageError::ScopeDenied);
            }
            check_evidence(
                self,
                id,
                p,
                q.evidence
                    .as_ref()
                    .ok_or_else(|| corrupt("qualified evidence absent"))?,
                false,
            )?;
            if self
                .operation(&p.operation_id)?
                .is_none_or(|o| o.state != OperationState::ObservedComplete)
            {
                return Err(invalid("embedding probe not complete"));
            }
            Some(c)
        } else {
            None
        };
        let next = current
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("selection revision overflow"))?;
        db(self.conn.execute("INSERT INTO router_embedding_selection(revision,candidate_id,space_id) VALUES(?1,?2,?3)",params![next,candidate.as_ref().map(|c|c.id.as_str()),candidate.as_ref().map(|c|c.space_id.as_str())]))?;
        selection(self.conn)
    }
}
fn check_artifact(unit: &Unit<'_>, reference: &ArtifactRef) -> Result<(), StorageError> {
    reference.validate().map_err(invalid)?;
    let version = unit
        .version(&reference.version_id)?
        .ok_or_else(|| invalid("embedding artifact version absent"))?;
    if !version.live()
        || version.artifact_id != reference.artifact_id
        || version.blob_digest != reference.sha256.as_str()
        || version.byte_count != reference.byte_length.get()
        || version.media_type != reference.media_type
    {
        return Err(invalid("embedding artifact identity mismatch"));
    }
    Ok(())
}

fn check_evidence(
    unit: &Unit<'_>,
    id: &Id,
    p: &StoredEmbeddingProbe,
    e: &StoredEmbeddingEvidence,
    exact_connection_revision: bool,
) -> Result<(), StorageError> {
    let c = candidate(unit.conn, id)?.ok_or_else(|| corrupt("embedding candidate absent"))?;
    let source = current_source(unit.conn, &c)?;
    if e.request_id != p.request_id
        || (exact_connection_revision && e.connection_revision != source.revision)
        || !digest(&e.registration_digest)
    {
        return Err(invalid("embedding evidence source mismatch"));
    }
    let attempt = unit
        .inference_attempt(&e.attempt_id)?
        .ok_or_else(|| invalid("native embedding attempt absent"))?;
    let route = unit
        .route_request(&p.request_id)?
        .ok_or_else(|| invalid("native embedding request absent"))?;
    if route.operation_id != e.native_operation_id
        || route.connection_id != c.connection_id
        || route.purpose != "Embedding"
    {
        return Err(invalid("native embedding request binding mismatch"));
    }
    let native = unit
        .operation(&e.native_operation_id)?
        .ok_or_else(|| invalid("native embedding operation absent"))?;
    if native.state != OperationState::ObservedComplete
        || native
            .latest_attempt()
            .is_none_or(|last| last.id.0 != e.attempt_id)
    {
        return Err(invalid(
            "native embedding operation not completed by exact attempt",
        ));
    }
    let balance = unit
        .attempt_balance(&e.attempt_id)?
        .ok_or_else(|| invalid("native embedding balance absent"))?;
    let source_scope_matches = match &p.scope {
        devworks_core::protocol::data::DataScope::Workspace => attempt.project_id.is_none(),
        devworks_core::protocol::data::DataScope::Project { project_id } => {
            attempt.project_id.as_ref() == Some(project_id)
        }
    };
    if attempt.request_id != p.request_id
        || attempt.operation_id != e.native_operation_id
        || attempt.connection_id != c.connection_id
        || attempt.actor_id != p.actor_id
        || !source_scope_matches
        || attempt.provider_model_id != e.reported_model
        || attempt
            .pricing_snapshot
            .get("admission_key")
            .and_then(serde_json::Value::as_str)
            != Some(p.admission_key.as_str())
        || attempt
            .pricing_snapshot
            .get("quote")
            .and_then(|v| v.get("connection_revision"))
            .and_then(serde_json::Value::as_i64)
            != Some(e.connection_revision)
        || attempt.endpoint_profile != "Embedding"
        || attempt.purpose != "Embedding"
        || attempt.state != "Settled"
        || !balance.local_quiescent
        || balance.unknown_tail
        || balance.incomplete
        || balance
            .summary
            .get("outcome")
            .and_then(serde_json::Value::as_str)
            != Some("Completed")
    {
        return Err(invalid("native embedding attempt incomplete or mismatched"));
    }
    for reference in [&e.result_artifact, &e.vector_artifact] {
        check_artifact(unit, reference)?;
    }
    if e.vector_artifact.byte_length.get() != c.dimensions * 4 {
        return Err(invalid("embedding vector byte length mismatch"));
    }
    Ok(())
}

fn probe_cancelled(unit: &Unit<'_>, p: &StoredEmbeddingProbe) -> Result<bool, StorageError> {
    let delivery = unit
        .entry(&p.delivery_id)?
        .ok_or_else(|| invalid("embedding delivery absent"))?;
    if delivery.operation_id.as_ref() != Some(&p.operation_id) {
        return Err(invalid("embedding delivery operation mismatch"));
    }
    Ok(delivery.cancel_requested || delivery.state == QueueState::Cancelled)
}
