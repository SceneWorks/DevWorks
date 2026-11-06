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
            if tx
                .conventions()
                .convention(convention_id, None, &scoped.readable)?
                .is_none_or(|r| r.revision != *revision)
            {
                return Err(invalid("convention source changed"));
            }
        }
        IndexTarget::All {
            scope: _,
            scope_revision,
        } => {
            if tx.conventions().convention_scope_revision(&scoped.exact)? != *scope_revision {
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
    ) -> Result<Result<(), String>, StorageError> {
        let (op, job, cancel, deadline) = (run.op, run.job, run.cancel, run.deadline);
        self.index_bounded(cancel, deadline)?;
        if owner.lifecycle != "Active" {
            return Ok(Ok(()));
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
            return Ok(Ok(()));
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
        let revision = owner.revision;
        let lexical = space.is_none() || !dense;
        let operation_id = op.id.clone();
        self.index_write(run, move |tx| {
            if lexical {tx.conventions().set_convention_index_status(&generation,&operation_id,"Indexing","Indexing",Some("Compatible native vectors are unavailable; the full lexical index is published."))?;}
            tx.conventions().publish_convention_index(&generation, revision, head.as_ref(), count)
        })?;
        Ok(Ok(()))
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
        if let Some(result) = recorded(&actual) {
            return Ok(IndexOutcome::Completed(result));
        }
        if operation.capability != "conventions.index" || operation.payload["kind"] != JOB_KIND {
            return Err(invalid("unsupported index journal"));
        }
        let job: IndexJob = serde_json::from_value(operation.payload.clone())
            .map_err(|_| invalid("invalid index journal"))?;
        let run = Publish {
            op: operation,
            job: &job,
            cancel: cancellation,
            deadline: deadline_ms,
        };
        let (op, j, available) = (operation.clone(), job.clone(), self.native_available());
        let ctx = self.index_write(&run, move |tx| current(tx, &op, &j, available))?;
        let mut after = None;
        let mut processed = 0;
        loop {
            self.index_bounded(cancellation, deadline_ms)?;
            let owners = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                let scope = access::scope(read, &ctx, Some(&job.scope), false)
                    .map_err(|_| StorageError::ScopeDenied)?;
                match &job.target {
                    IndexTarget::One {
                        convention_id,
                        revision,
                    } => Ok(read
                        .conventions()
                        .convention(convention_id, Some(*revision), &scope.readable)?
                        .into_iter()
                        .collect::<Vec<_>>()),
                    IndexTarget::All { .. } => read.conventions().conventions(
                        &ConventionQuery {
                            scopes: vec![scope.exact],
                            lifecycles: vec!["Active".into()],
                            ..Default::default()
                        },
                        after.as_ref(),
                        32,
                    ),
                }
            })?;
            if owners.is_empty() {
                break;
            }
            for owner in &owners {
                if let Err(reason) = self.index_owner(
                    &Publish {
                        op: operation,
                        job: &job,
                        cancel: cancellation,
                        deadline: deadline_ms,
                    },
                    &ctx,
                    owner,
                )? {
                    return Ok(IndexOutcome::Unknown(reason));
                }
                processed += 1;
            }
            if matches!(job.target, IndexTarget::One { .. }) {
                break;
            }
            after = owners.last().map(StoredConvention::position);
        }
        let result = json!({"operationId":operation.id,"indexed":processed});
        let value = result.clone();
        let op = operation.clone();
        self.index_write(&run, move |tx| {
            let attempt = op
                .latest_attempt()
                .ok_or_else(|| invalid("index attempt missing"))?;
            tx.operations().observe(
                &op.id,
                attempt.attempt_no,
                json!({"conventionPublication":value}),
            )
        })?;
        Ok(IndexOutcome::Completed(result))
    }
}
