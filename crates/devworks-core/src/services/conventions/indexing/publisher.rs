//! Durable source-fenced publication. Provider work and source bytes are always outside SQL.
use super::{
    IndexService,
    chunker::{MarkdownChunks, MarkdownSection},
    commands::{EFFECT_CONSUMER, IndexJob, IndexTarget, JOB_KIND},
    configuration,
    embedding::{EmbeddingFailure, EmbeddingOrigin, EmbeddingRequest},
    preparation::InputPreparation,
    rank,
};
use crate::{
    context::InvocationContext,
    fingerprint::sha256_hex,
    ports::storage::*,
    protocol::{Id, OperationState, provider::ArtifactRef},
    services::{
        approvals,
        artifacts::ConventionSourceSelection,
        conventions::{access, model},
        router::embedding_spaces,
    },
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, io::Read, sync::Arc};

#[derive(Debug, Clone, PartialEq)]
pub enum IndexOutcome {
    Completed(Value),
    Failed(String),
    Unknown(String),
}
fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
pub(super) fn identity(prefix: &str, value: &str) -> Id {
    Id::new(format!("{prefix}{}", &sha256_hex(value.as_bytes())[..40]))
        .expect("host digest identifier")
}
pub(super) fn header(
    operation: &crate::protocol::OperationId,
    job: &IndexJob,
    owner: &StoredConvention,
    now: i64,
) -> Result<StoredConventionIndex, StorageError> {
    let source_ref = owner
        .content_ref
        .clone()
        .ok_or_else(|| invalid("missing convention body"))?;
    let reference: ArtifactRef = serde_json::from_value(source_ref.clone())
        .map_err(|_| invalid("invalid convention body"))?;
    Ok(StoredConventionIndex {
        generation_id: identity(
            "cidx_",
            &format!(
                "{}:{}:{}:{}",
                operation.0.as_str(),
                owner.id.as_str(),
                owner.revision,
                reference.sha256.as_str()
            ),
        ),
        convention_id: owner.id.clone(),
        convention_revision: owner.revision,
        source_ref,
        operation_id: operation.clone(),
        state: "Draft".into(),
        status: "Queued".into(),
        error: None,
        embedding_space: job.embedding_space.clone(),
        source_digest: reference.sha256.as_str().into(),
        created_at_ms: now,
        updated_at_ms: now,
    })
}
fn current(
    tx: &mut dyn WriteTransaction,
    operation: &OperationRecord,
    job: &IndexJob,
    available: bool,
) -> Result<InvocationContext, StorageError> {
    if tx.store().store_id != job.store_id
        || tx.store().restore_epoch != job.restore_epoch
        || !crate::authority::fences_hold(tx, &job.fences)?
    {
        return Err(StorageError::ScopeDenied);
    }
    let kind = tx
        .actors()
        .actor_kind(&job.actor)?
        .ok_or(StorageError::ScopeDenied)?;
    if format!("{kind:?}") != job.actor_kind {
        return Err(StorageError::ScopeDenied);
    }
    let context =
        approvals::restore_source_context(tx.store(), &job.actor, kind, &job.source_context)?;
    crate::services::conventions::control::check(
        tx,
        &context,
        &operation.id,
        &job.control_roots,
        job.assignment_generation,
    )?;
    super::selection::validate_child(tx, job, &context)?;
    let scoped = access::scope_in(tx, &context, Some(&job.scope), true)
        .map_err(|_| StorageError::ScopeDenied)?;
    let configuration = configuration::resolved_configuration(
        &embedding_spaces::resolve_selected_in(tx, &context)?,
        available,
    );
    if configuration.source_digest != job.configuration_source_digest
        || configuration.space.as_ref().map(|s| &s.id) != job.embedding_space.as_ref()
    {
        return Err(invalid("convention embedding configuration changed"));
    }
    match &job.target {
        IndexTarget::One {
            convention_id,
            revision,
        } => {
            let captured =
                tx.conventions()
                    .convention(convention_id, Some(*revision), &scoped.readable)?;
            let latest = tx
                .conventions()
                .convention(convention_id, None, &scoped.readable)?;
            if !captured.zip(latest).is_some_and(|(captured, latest)| {
                captured.lifecycle == "Active"
                    && latest.lifecycle == captured.lifecycle
                    && latest.scope == captured.scope
                    && latest.title == captured.title
                    && captured.content_ref.is_some()
                    && latest.content_ref == captured.content_ref
            }) {
                return Err(invalid("convention source changed"));
            }
        }
        IndexTarget::All {
            scope: _,
            scope_revision,
        } => {
            let source_holds = match job.index_source_revision {
                Some(revision) => {
                    tx.conventions()
                        .convention_index_source_revision(&scoped.exact)?
                        == revision
                }
                None => {
                    tx.conventions().convention_scope_revision(&scoped.exact)? == *scope_revision
                }
            };
            if !source_holds {
                return Err(invalid("convention scope changed"));
            }
        }
    }
    let actual = tx
        .operations()
        .operation(&operation.id)?
        .ok_or_else(|| invalid("index journal missing"))?;
    if actual.payload != operation.payload
        || actual.latest_attempt().map(|a| a.fence())
            != operation.latest_attempt().map(|a| a.fence())
        || !matches!(
            actual.state,
            OperationState::Dispatching | OperationState::OutcomeUnknown
        )
    {
        return Err(StorageError::LeaseLost {
            kind: "convention_index",
            id: operation.id.0.as_str().into(),
        });
    }
    let entry = tx
        .queue()
        .entry(&job.delivery_id)?
        .ok_or_else(|| invalid("index delivery missing"))?;
    if entry.operation_id.as_ref() != Some(&operation.id) || entry.consumer != EFFECT_CONSUMER {
        return Err(invalid("index delivery changed"));
    }
    if entry.cancel_requested {
        return Err(StorageError::Cancelled);
    }
    Ok(context)
}
struct Publish<'a> {
    op: &'a OperationRecord,
    job: &'a IndexJob,
    cancel: &'a Cancellation,
    deadline: Option<i64>,
}
fn path_bytes(section: &MarkdownSection) -> usize {
    section.heading.len() + section.parent.as_ref().map_or(0, |p| 3 + path_bytes(p))
}
fn recorded(operation: &OperationRecord) -> Option<Value> {
    operation
        .attempts
        .iter()
        .rev()
        .flat_map(|a| a.observations.iter().rev())
        .find_map(|v| v.get("conventionPublication").cloned())
}
fn completion(op: &crate::protocol::OperationId, chunks: u64, conventions: u64) -> Value {
    json!({"operationId":op,"indexed":chunks,"conventions":conventions})
}
fn captured_count(op: &OperationRecord, job: &IndexJob) -> Option<u64> {
    op.attempts
        .iter()
        .flat_map(|a| &a.observations)
        .find_map(|v| {
            let value = v.get("conventionCapture")?;
            if value["configurationSourceDigest"] != job.configuration_source_digest
                || value["target"] != json!(job.target)
            {
                return None;
            }
            value.get("count")?.as_u64()
        })
}
fn completed_headers(
    read: &dyn ConventionIndexRead,
    op: &OperationRecord,
    job: &IndexJob,
    stopped: &mut dyn FnMut() -> bool,
) -> Result<Option<Value>, StorageError> {
    let expected = match &job.target {
        IndexTarget::One { .. } => 1,
        IndexTarget::All { .. } => match captured_count(op, job) {
            Some(count) => count,
            None => return Ok(None),
        },
    };
    let mut after = None;
    let mut count = 0u64;
    let mut chunks = 0u64;
    loop {
        if stopped() {
            return Err(StorageError::Cancelled);
        }
        let rows = read.convention_index_generations(None, Some(&op.id), after.as_ref(), 32)?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            if row.state != "Published" {
                return Ok(None);
            }
            if let IndexTarget::One {
                convention_id,
                revision,
            } = &job.target
                && (&row.convention_id != convention_id || row.convention_revision != *revision)
            {
                return Err(invalid(
                    "published target differs from original index intent",
                ));
            }
            count = count
                .checked_add(1)
                .ok_or_else(|| invalid("index target count overflow"))?;
            chunks = chunks
                .checked_add(read.convention_chunk_count(&row.generation_id)?)
                .ok_or_else(|| invalid("index chunk count overflow"))?;
        }
        after = rows.last().map(|r| r.generation_id.clone());
    }
    Ok((count == expected).then(|| completion(&op.id, chunks, count)))
}
impl IndexService {
    fn index_write<T: Send + 'static>(
        &self,
        run: &Publish<'_>,
        work: impl FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    ) -> Result<T, StorageError> {
        self.index_bounded(run.cancel, run.deadline)?;
        let _phase = self.closed.enter()?;
        let (op, job, available, closed, clock, cancel, deadline) = (
            run.op.clone(),
            run.job.clone(),
            self.native_available(),
            self.closed.clone(),
            self.clock.clone(),
            run.cancel.clone(),
            run.deadline,
        );
        run_write(
            &*self.store,
            WriteOptions {
                cancellation: Some(cancel.clone()),
                deadline_ms: deadline,
                ..Default::default()
            },
            move |tx| {
                if closed.is_closed()
                    || cancel.is_cancelled()
                    || deadline.is_some_and(|d| clock.now_ms().get() >= d)
                {
                    return Err(StorageError::Cancelled);
                }
                current(tx, &op, &job, available)?;
                work(tx)
            },
        )
    }
    fn index_bounded(
        &self,
        cancel: &Cancellation,
        deadline: Option<i64>,
    ) -> Result<(), StorageError> {
        if self.is_stopped() || cancel.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        if deadline.is_some_and(|d| self.clock.now_ms().get() >= d) {
            return Err(StorageError::Cancelled);
        }
        Ok(())
    }
    fn index_section(
        &self,
        run: &Publish<'_>,
        generation: &Id,
        section: &Arc<MarkdownSection>,
        seen: &mut BTreeSet<u64>,
        source: &str,
    ) -> Result<(), StorageError> {
        self.index_bounded(run.cancel, run.deadline)?;
        if seen.contains(&section.index) {
            return Ok(());
        }
        if let Some(parent) = &section.parent {
            self.index_section(run, generation, parent, seen, source)?;
        }
        let id = identity(
            "csec_",
            &format!("{}:{}", generation.as_str(), section.index),
        );
        let row = StoredConventionSection {
            generation_id: generation.clone(),
            id: id.clone(),
            parent_id: section
                .parent
                .as_ref()
                .map(|p| identity("csec_", &format!("{}:{}", generation.as_str(), p.index))),
            heading: if section.parent.is_none() {
                Some(section.heading.to_string())
            } else {
                section
                    .source_span
                    .as_ref()
                    .and_then(|span| {
                        source.get(
                            span.first_byte as usize..(span.first_byte + span.byte_length) as usize,
                        )
                    })
                    .filter(|value| {
                        value.len() <= 65536
                            && serde_json::from_value::<crate::protocol::support::Content>(json!(
                                value
                            ))
                            .is_ok()
                    })
                    .map(str::to_owned)
            },
            level: section.source_span.as_ref().map_or(0, |s| s.level),
            source_offset: section.source_span.as_ref().map(|s| s.first_byte as i64),
            source_length: section.source_span.as_ref().map(|s| s.byte_length as i64),
            token_count: rank::tokens(&section.heading).count() as i64,
        };
        self.index_write(run, move |tx| {
            tx.conventions().stage_convention_section(&row)
        })?;
        self.index_terms(run, generation, &id, &section.heading, true)?;
        seen.insert(section.index);
        Ok(())
    }
    fn index_terms(
        &self,
        run: &Publish<'_>,
        generation: &Id,
        id: &Id,
        text: &str,
        section: bool,
    ) -> Result<(), StorageError> {
        for (n, terms) in rank::term_batches(text).enumerate() {
            self.index_bounded(run.cancel, run.deadline)?;
            let terms = terms
                .into_iter()
                .map(|(term, frequency)| ConventionTerm { term, frequency })
                .collect::<Vec<_>>();
            let digest = sha256_hex(
                crate::jcs::canonicalize(
                    &serde_json::to_value(&terms).map_err(|_| invalid("index terms"))?,
                )
                .map_err(|_| invalid("index terms"))?
                .as_bytes(),
            );
            let (g, id) = (generation.clone(), id.clone());
            self.index_write(run, move |tx| {
                if section {
                    tx.conventions()
                        .stage_convention_section_terms(&g, &id, n as i64, &digest, &terms)
                } else {
                    tx.conventions()
                        .stage_convention_chunk_terms(&g, &id, n as i64, &digest, &terms)
                }
            })?;
        }
        Ok(())
    }
    fn index_owner(
        &self,
        run: &Publish<'_>,
        ctx: &InvocationContext,
        owner: &StoredConvention,
    ) -> Result<Result<u64, String>, StorageError> {
        let (op, job, cancel, deadline) = (run.op, run.job, run.cancel, run.deadline);
        self.index_bounded(cancel, deadline)?;
        if owner.lifecycle != "Active" {
            return Ok(Ok(0));
        }
        let mut candidate = header(&op.id, job, owner, self.clock.now_ms().get())?;
        let (existing, head) = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            Ok((
                read.conventions()
                    .convention_index_generation(&candidate.generation_id)?,
                read.conventions()
                    .convention_index_head(&owner.id)?
                    .map(|g| g.generation_id),
            ))
        })?;
        if existing.as_ref().is_some_and(|g| g.state == "Published") {
            return Ok(Ok(read_snapshot(
                &*self.store,
                &ReadOptions::default(),
                |read| {
                    read.conventions()
                        .convention_chunk_count(&candidate.generation_id)
                },
            )?));
        }
        if let Some(existing) = existing {
            candidate = existing;
        }
        let g = candidate.clone();
        self.index_write(run, move |tx| {
            if tx
                .conventions()
                .convention_index_generation(&g.generation_id)?
                .is_none()
            {
                tx.conventions().stage_convention_index(&g)?;
            }
            if g.status == "Queued" {
                tx.conventions().set_convention_index_status(
                    &g.generation_id,
                    &g.operation_id,
                    "Queued",
                    "Indexing",
                    None,
                )?;
            }
            Ok(())
        })?;
        let lease = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            self.artifacts
                .resolve_convention_source(
                    read,
                    ctx,
                    Some(&job.scope),
                    owner,
                    ConventionSourceSelection::Markdown,
                )
                .map_err(|_| StorageError::ScopeDenied)
        })?;
        let mut stopped = || self.index_bounded(cancel, deadline).is_err();
        let mut reader = lease.open_controlled(&mut stopped).map_err(|_| {
            if stopped() {
                StorageError::Cancelled
            } else {
                invalid("convention body unreadable")
            }
        })?;
        let mut bytes = Vec::new();
        let mut buffer = [0; 65536];
        loop {
            self.index_bounded(cancel, deadline)?;
            let n = reader
                .read(&mut buffer)
                .map_err(|_| invalid("convention body unreadable"))?;
            if n == 0 {
                break;
            }
            if (bytes.len() + n) as u64 > model::MAX_MARKDOWN_BYTES {
                return Err(invalid("convention body exceeds retained source bound"));
            }
            bytes.extend_from_slice(&buffer[..n]);
        }
        let body = String::from_utf8(bytes).map_err(|_| invalid("convention body not UTF-8"))?;
        if sha256_hex(body.as_bytes()) != candidate.source_digest {
            return Err(invalid("convention source bytes changed"));
        }
        let root = Arc::new(MarkdownSection {
            index: 0,
            heading: owner.title.clone().into(),
            source_span: None,
            parent: None,
        });
        let mut sections = BTreeSet::new();
        self.index_section(run, &candidate.generation_id, &root, &mut sections, &body)?;
        let mut chunks = MarkdownChunks::new(&body, &owner.title).peekable();
        let mut count = 0i64;
        let mut dense = true;
        let space = job
            .embedding_space
            .as_ref()
            .map(|id| {
                read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                    read.conventions().convention_embedding_space(id)
                })
            })
            .transpose()?
            .flatten();
        while chunks.peek().is_some() {
            self.index_bounded(cancel, deadline)?;
            let mut batch = Vec::new();
            let mut input_bytes = 0usize;
            while let Some(chunk) = chunks.peek() {
                let size = path_bytes(&chunk.section)
                    .saturating_add(chunk.text.len())
                    .saturating_add(2);
                if !batch.is_empty() && input_bytes.saturating_add(size) > 8 * 1024 * 1024 {
                    break;
                }
                input_bytes = input_bytes.saturating_add(size);
                batch.push(chunks.next().expect("peeked chunk"));
                if batch.len() == 32 {
                    break;
                }
            }
            let mut rows = Vec::new();
            let mut pending = Vec::new();
            let mut old_rows = 0;
            for c in &batch {
                self.index_section(
                    run,
                    &candidate.generation_id,
                    &c.section,
                    &mut sections,
                    &body,
                )?;
                let id = identity(
                    "cchunk_",
                    &format!("{}:{}", candidate.generation_id.as_str(), c.index),
                );
                let old = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                    read.conventions()
                        .convention_chunk(&candidate.generation_id, &id)
                })?;
                let old_vector = old.as_ref().and_then(|r| r.vector.clone());
                if let Some(old) = old {
                    if old.text != c.text || old.index != c.index as i64 {
                        return Err(invalid("immutable chunk source changed"));
                    }
                    old_rows += 1;
                }
                pending.push(c.embedding_text());
                rows.push(StoredConventionChunk {
                    generation_id: candidate.generation_id.clone(),
                    id,
                    convention_id: owner.id.clone(),
                    convention_revision: owner.revision,
                    index: c.index as i64,
                    section_id: identity(
                        "csec_",
                        &format!("{}:{}", candidate.generation_id.as_str(), c.section.index),
                    ),
                    text: c.text.clone(),
                    token_count: rank::tokens(&c.text).count() as i64,
                    vector: old_vector,
                });
            }
            if old_rows != 0 && old_rows != rows.len() {
                return Err(invalid("partial immutable embedding batch"));
            }
            if !rows.is_empty()
                && old_rows == 0
                && dense
                && let Some(space) = &space
            {
                let origin = EmbeddingOrigin::Index {
                    operation_id: op.id.clone(),
                    generation_id: candidate.generation_id.clone(),
                    source_digest: candidate.source_digest.clone(),
                    first_chunk: rows[0].index,
                };
                let texts = pending.iter().map(String::as_str).collect::<Vec<_>>();
                let prepared = self.prepare_embedding_inputs(InputPreparation {
                    context: ctx,
                    origin: &origin,
                    embedding_space: &space.id,
                    configuration_source_digest: &job.configuration_source_digest,
                    texts: &texts,
                    fences: &job.fences,
                    cancellation: cancel,
                    deadline_ms: deadline,
                })?;
                let request = EmbeddingRequest {
                    request_id: prepared.request_id.clone(),
                    origin,
                    space: space.clone(),
                    configuration_source_digest: job.configuration_source_digest.clone(),
                    preparation_operation: prepared.operation_id,
                    input_refs: prepared.input_refs,
                    fences: job.fences.clone(),
                    cancellation: cancel.clone(),
                    deadline_ms: deadline,
                };
                let result = if op.state == OperationState::OutcomeUnknown {
                    match self.observe_batch(&prepared.context, request.clone()) {
                        Err(EmbeddingFailure::NotStarted { request_id })
                            if request_id == prepared.request_id =>
                        {
                            self.embed_batch(&prepared.context, request)
                        }
                        result => result,
                    }
                } else {
                    self.embed_batch(&prepared.context, request)
                };
                match result {
                    Ok(result)
                        if result.request_id == prepared.request_id
                            && result.space == *space
                            && result.vectors.len() == rows.len()
                            && result.vectors.iter().all(|v| {
                                v.len() == space.dimensions as usize
                                    && v.iter().all(|x| x.is_finite())
                            }) =>
                    {
                        for (row, vector) in rows.iter_mut().zip(result.vectors) {
                            row.vector = Some(vector);
                        }
                    }
                    Ok(_) => return Err(invalid("native embedding population/binding mismatch")),
                    Err(EmbeddingFailure::Unknown { reason, .. }) => return Ok(Err(reason)),
                    Err(EmbeddingFailure::NotStarted { .. }) => {
                        return Err(invalid("native send returned observer-only absence"));
                    }
                    Err(EmbeddingFailure::Unavailable(_)) => dense = false,
                    Err(EmbeddingFailure::Cancelled) => return Err(StorageError::Cancelled),
                    Err(EmbeddingFailure::Storage(error)) => return Err(error),
                }
            }
            let generation = candidate.generation_id.clone();
            let stage = rows.clone();
            self.index_write(run, move |tx| {
                tx.conventions()
                    .stage_convention_chunks(&generation, &stage)
            })?;
            for row in rows {
                self.index_terms(run, &candidate.generation_id, &row.id, &row.text, false)?;
            }
            count += batch.len() as i64;
        }
        self.index_bounded(cancel, deadline)?;
        let generation = candidate.generation_id.clone();
        let (owner_id, owner_scope) = (owner.id.clone(), owner.scope.clone());
        let lexical = space.is_none() || !dense;
        let operation_id = op.id.clone();
        let actual = op.clone();
        let one = matches!(job.target, IndexTarget::One { .. });
        self.index_write(run, move |tx| {
            if lexical {tx.conventions().set_convention_index_status(&generation,&operation_id,"Indexing","Indexing",Some("Compatible native vectors are unavailable; the full lexical index is published."))?;}
            // Keep the captured generation and input identity. The repository still checks
            // its exact immutable body/title/scope against this current owner under one UoW.
            let revision = tx.conventions().convention(&owner_id,None,std::slice::from_ref(&owner_scope))?
                .ok_or_else(||invalid("convention source changed"))?.revision;
            tx.conventions().publish_convention_index(&generation, revision, head.as_ref(), count)?;
            if one {let attempt=actual.latest_attempt().ok_or_else(||invalid("index attempt missing"))?;tx.operations().observe(&actual.id,attempt.attempt_no,json!({"conventionPublication":completion(&actual.id,count as u64,1)}))?;}
            Ok(())
        })?;
        Ok(Ok(count as u64))
    }
    fn capture_all(&self, run: &Publish<'_>, ctx: &InvocationContext) -> Result<u64, StorageError> {
        let mut after = None;
        let mut count = 0u64;
        loop {
            self.index_bounded(run.cancel, run.deadline)?;
            let owners = read_snapshot(
                &*self.store,
                &ReadOptions {
                    cancellation: Some(run.cancel.clone()),
                    deadline_ms: run.deadline,
                },
                |read| {
                    let scope = access::scope(read, ctx, Some(&run.job.scope), false)
                        .map_err(|_| StorageError::ScopeDenied)?;
                    read.conventions().conventions(
                        &ConventionQuery {
                            scopes: vec![scope.exact],
                            lifecycles: vec!["Active".into()],
                            ..Default::default()
                        },
                        after.as_ref(),
                        32,
                    )
                },
            )?;
            if owners.is_empty() {
                break;
            }
            let headers = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                owners
                    .iter()
                    .map(|owner| {
                        // A metadata edit during partial capture must retain the original
                        // generation/input identities already captured for this owner.
                        if let Some(existing) = read
                            .conventions()
                            .convention_index_generations(
                                Some(&owner.id),
                                Some(&run.op.id),
                                None,
                                1,
                            )?
                            .into_iter()
                            .next()
                        {
                            let captured = read.conventions().convention(
                                &owner.id,
                                Some(existing.convention_revision),
                                std::slice::from_ref(&owner.scope),
                            )?;
                            if owner.content_ref.as_ref() != Some(&existing.source_ref)
                                || captured.is_none_or(|old| {
                                    old.title != owner.title
                                        || old.scope != owner.scope
                                        || old.content_ref.as_ref() != Some(&existing.source_ref)
                                        || old.lifecycle != "Active"
                                })
                            {
                                return Err(invalid("captured index source changed"));
                            }
                            Ok(existing)
                        } else {
                            header(&run.op.id, run.job, owner, self.clock.now_ms().get())
                        }
                    })
                    .collect::<Result<Vec<_>, StorageError>>()
            })?;
            self.index_write(run, move |tx| {
                for header in headers {
                    if tx
                        .conventions()
                        .convention_index_generation(&header.generation_id)?
                        .is_none()
                    {
                        tx.conventions().stage_convention_index(&header)?;
                    }
                }
                Ok(())
            })?;
            count = count
                .checked_add(owners.len() as u64)
                .ok_or_else(|| invalid("index capture count overflow"))?;
            after = owners.last().map(StoredConvention::position);
        }
        let (op, job) = (run.op.clone(), run.job.clone());
        self.index_write(run,move|tx|{let attempt=op.latest_attempt().ok_or_else(||invalid("index attempt missing"))?;tx.operations().observe(&op.id,attempt.attempt_no,json!({"conventionCapture":{"count":count,"target":job.target,"configurationSourceDigest":job.configuration_source_digest}}))})?;
        Ok(count)
    }
    pub fn run_index(
        &self,
        operation: &OperationRecord,
        cancellation: &Cancellation,
        deadline_ms: Option<i64>,
    ) -> Result<IndexOutcome, StorageError> {
        let actual = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            read.operations().operation(&operation.id)
        })?
        .ok_or_else(|| invalid("index operation missing"))?;
        if actual.payload["kind"] == super::selection::SELECTION_KIND {
            return self.run_selection(operation, cancellation, deadline_ms);
        }
        if let Some(result) = recorded(&actual) {
            return Ok(IndexOutcome::Completed(result));
        }
        if actual.capability != "conventions.index"
            || actual.schema_version != 1
            || actual.payload["kind"] != JOB_KIND
        {
            return Err(invalid("unsupported index journal"));
        }
        let job: IndexJob = serde_json::from_value(actual.payload.clone())
            .map_err(|_| invalid("invalid index journal"))?;
        // Observe immutable publication before fresh source/config admission. A later edit or
        // selection cannot erase a completed effect, and this path performs no provider work.
        if let Some(result) = read_snapshot(
            &*self.store,
            &ReadOptions {
                cancellation: Some(cancellation.clone()),
                deadline_ms,
            },
            |read| {
                completed_headers(read.conventions(), &actual, &job, &mut || {
                    self.is_stopped() || cancellation.is_cancelled()
                })
            },
        )? {
            return Ok(IndexOutcome::Completed(result));
        }
        let run = Publish {
            op: operation,
            job: &job,
            cancel: cancellation,
            deadline: deadline_ms,
        };
        let (op, j, available) = (operation.clone(), job.clone(), self.native_available());
        let ctx = self.index_write(&run, move |tx| current(tx, &op, &j, available))?;
        let expected = match job.target {
            IndexTarget::One { .. } => None,
            IndexTarget::All { .. } => Some(match captured_count(&actual, &job) {
                Some(count) => count,
                None => self.capture_all(&run, &ctx)?,
            }),
        };
        let mut after = None;
        let mut processed = 0u64;
        let mut chunks = 0u64;
        loop {
            self.index_bounded(cancellation, deadline_ms)?;
            let headers = read_snapshot(
                &*self.store,
                &ReadOptions {
                    cancellation: Some(cancellation.clone()),
                    deadline_ms,
                },
                |read| {
                    read.conventions().convention_index_generations(
                        None,
                        Some(&operation.id),
                        after.as_ref(),
                        32,
                    )
                },
            )?;
            if headers.is_empty() {
                break;
            }
            for header in &headers {
                let count = if header.state == "Published" {
                    read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                        read.conventions()
                            .convention_chunk_count(&header.generation_id)
                    })?
                } else {
                    let owner = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                        let scope = access::scope(read, &ctx, Some(&job.scope), false)
                            .map_err(|_| StorageError::ScopeDenied)?;
                        read.conventions()
                            .convention(
                                &header.convention_id,
                                Some(header.convention_revision),
                                &scope.readable,
                            )?
                            .ok_or(StorageError::ScopeDenied)
                    })?;
                    match self.index_owner(&run, &ctx, &owner)? {
                        Ok(count) => count,
                        Err(reason) => return Ok(IndexOutcome::Unknown(reason)),
                    }
                };
                chunks = chunks
                    .checked_add(count)
                    .ok_or_else(|| invalid("index chunk count overflow"))?;
                processed = processed
                    .checked_add(1)
                    .ok_or_else(|| invalid("index target count overflow"))?;
            }
            after = headers.last().map(|h| h.generation_id.clone());
        }
        if expected.is_some_and(|count| count != processed) {
            return Err(invalid("captured index target population changed"));
        }
        let result = completion(&operation.id, chunks, processed);
        if matches!(job.target, IndexTarget::One { .. }) && processed == 1 {
            return Ok(IndexOutcome::Completed(result));
        }
        // All target headers were captured before any provider work. Published headers and
        // their chunks are immutable while this operation is active. Record the observed
        // completion under its owner fence, without re-admitting the already applied effect.
        let (op, value) = (operation.clone(), result.clone());
        self.write(move |tx| {
            let current = tx
                .operations()
                .operation(&op.id)?
                .ok_or_else(|| invalid("index operation missing"))?;
            if current.payload != op.payload
                || current.latest_attempt().map(|a| a.fence())
                    != op.latest_attempt().map(|a| a.fence())
                || !matches!(
                    current.state,
                    OperationState::Dispatching | OperationState::OutcomeUnknown
                )
            {
                return Err(StorageError::LeaseLost {
                    kind: "convention_index",
                    id: op.id.0.as_str().into(),
                });
            }
            if recorded(&current).is_none() {
                let attempt = op
                    .latest_attempt()
                    .ok_or_else(|| invalid("index attempt missing"))?;
                tx.operations().observe(
                    &op.id,
                    attempt.attempt_no,
                    json!({"conventionPublication":value}),
                )?;
            }
            Ok(())
        })?;
        Ok(IndexOutcome::Completed(result))
    }
}
