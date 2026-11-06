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
        db(self.conn.execute("INSERT INTO router_embedding_candidates(id,space_id,configuration_digest,body_json,created_at) VALUES(?1,?2,?3,?4,?5)",params![c.id.as_str(),c.space_id.as_str(),c.configuration_digest,encode(c)?,c.created_at_ms]))?;
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
        fence: &AttemptFence,
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
            || !devworks_core::authority::service::fences_hold(self, &p.authority_fences)?
        {
            return Err(StorageError::ScopeDenied);
        }
        let operation = self
            .operation(operation_id)?
            .ok_or_else(|| invalid("embedding operation absent"))?;
        if operation
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
