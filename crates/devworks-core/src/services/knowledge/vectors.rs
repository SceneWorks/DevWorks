//! Vector matches for `knowledge.search` (RAG-05, RAG-07), through the shared qualified
//! embedding route of document and convention retrieval ([`IndexService`]).
//!
//! # Production
//!
//! A committed write that changed a claim, while a qualified cosine space is selected, queues
//! one delivery for [`VECTOR_CONSUMER`] with the writer's captured context and authority
//! fences. The delivery embeds, in batches of 32, every claim of the write's scope that is not
//! deleted and has no vector in the selected space (the write's own claims, and any written
//! before the space was selected), each batch under its own captured input and journal. The
//! store keeps one vector per claim and space of the claim's exact text: a text change or a
//! deletion removes it in the same write, so the next delivery embeds the new text. A claim's
//! scope is read from the claim at query time, so a scope move takes its vector along, and a
//! project purge removes the vectors with the project's claims.
//!
//! # Search
//!
//! Query preparation embeds the query text (one request per page) when the read scopes have
//! vectors in the selected space. Text matches (every query word) come first, by BM25; then
//! the vector matches: of the claims passing every other filter that are not text matches, the
//! [`VECTOR_NEIGHBORS`] nearest with positive cosine similarity, by similarity descending, then
//! id descending. The store applies the read scopes before it computes any similarity. A
//! position is `r<score>|<id>` among text matches and `v<similarity>|<id>` among vector
//! matches; the cursor binds the space, so a page never resumes under another space or without
//! vectors. Storing a vector does not advance the scope's revision, so a claim embedded between
//! two pages can join a later page; no claim is listed twice. Fusing the two orders (with graph
//! neighbours) is later work.
use super::{Scoped, knowledge_read, model, ops, resolve_scope, writable};
use crate::context::InvocationContext;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::ports::storage::{
    Cancellation, KnowledgeScope, NewEvent, NewQueueEntry, QueueEntry, QueueState, ReadSnapshot,
    StorageError, WriteOptions, WriteTransaction, run_write,
};
use crate::ports::{AuthorityFence, IdKind, Preparation};
use crate::protocol::results::OperationResult;
use crate::protocol::{ActorId, EventId, Id, OperationId, RootId, StoreId};
use crate::services::conventions::{
    control,
    indexing::{
        IndexService,
        embedding::{EmbeddingFailure, EmbeddingOrigin, EmbeddingRequest},
        preparation::InputPreparation,
    },
};
use crate::services::{approvals, effect_job, kernel::Failure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

/// The durable consumer that embeds claims.
pub const VECTOR_CONSUMER: &str = "knowledge-claim-vectors";
/// The most vector matches one search lists after its text matches.
pub const VECTOR_NEIGHBORS: u16 = 20;
const JOB_SCHEMA: u32 = 1;
const BATCH: u16 = 32;
const AGGREGATE: &str = "knowledge_claim_vectors";

fn invalid(what: &str) -> StorageError {
    StorageError::InvalidRecord(format!("knowledge claim vectors: {what}"))
}

/// The selected space's identity as the store names it.
fn space_id(space: &str) -> Result<Id, StorageError> {
    Id::new(space).map_err(|_| invalid("space identity"))
}

// ---------------------------------------------------------------------------------------------
// Production

/// One queued embedding of a scope's pending claims, with the identity of the write that
/// queued it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Job {
    schema: u32,
    scope: String,
    write_id: Id,
    actor: ActorId,
    actor_kind: String,
    store_id: StoreId,
    restore_epoch: i64,
    fences: Vec<AuthorityFence>,
    source_context: Value,
    control_roots: Vec<(RootId, i64)>,
    assignment_generation: Option<i64>,
}

impl Job {
    fn from_delivery(entry: &QueueEntry) -> Result<Self, StorageError> {
        let job: Self =
            serde_json::from_value(entry.payload.clone()).map_err(|_| invalid("delivery"))?;
        if entry.consumer != VECTOR_CONSUMER || job.schema != JOB_SCHEMA {
            return Err(invalid("delivery"));
        }
        Ok(job)
    }

    fn scope(&self) -> Result<KnowledgeScope, StorageError> {
        KnowledgeScope::parse(&self.scope)
    }

    /// The writer's context, while the delivery still holds its claim, the writer's authority
    /// still holds and the writer may still write the scope.
    fn restore(
        &self,
        tx: &mut dyn WriteTransaction,
        entry: &QueueEntry,
    ) -> Result<InvocationContext, StorageError> {
        let boot = tx.store().boot_id.clone();
        let now = tx.now_ms();
        tx.queue()
            .entry(&entry.id)?
            .filter(|actual| {
                actual.state == QueueState::Claimed
                    && !actual.cancel_requested
                    && actual.payload == entry.payload
                    && actual.claim_boot_id == boot
                    && actual.fence() == entry.fence()
                    && actual.lease_until_ms.is_some_and(|expiry| expiry > now)
            })
            .ok_or_else(|| StorageError::LeaseLost {
                kind: VECTOR_CONSUMER,
                id: entry.id.as_str().into(),
            })?;
        let (_, ctx) = effect_job::restore(
            tx,
            &effect_job::JobBinding {
                store_id: &self.store_id,
                restore_epoch: self.restore_epoch,
                fences: &self.fences,
                actor: &self.actor,
                actor_kind: &self.actor_kind,
                source_context: &self.source_context,
                control_roots: &self.control_roots,
                assignment_generation: self.assignment_generation,
            },
            &OperationId(entry.id.clone()),
        )?;
        let reach = crate::authority::reach::reach(tx.authority(), &ctx)?
            .ok_or(StorageError::ScopeDenied)?;
        if !writable(&reach, &self.scope()?) {
            return Err(StorageError::ScopeDenied);
        }
        Ok(ctx)
    }
}

/// The receipt of a Knowledge write result.
fn receipt(result: &OperationResult) -> Option<&Id> {
    match result {
        OperationResult::KnowledgeRecord { value }
        | OperationResult::KnowledgeRevise { value }
        | OperationResult::KnowledgeRelate { value }
        | OperationResult::KnowledgeTag { value }
        | OperationResult::KnowledgeVerify { value }
        | OperationResult::KnowledgeRetract { value }
        | OperationResult::KnowledgeScope { value }
        | OperationResult::KnowledgeImportUserData { value }
        | OperationResult::KnowledgePromote { value }
        | OperationResult::KnowledgeChallenge { value } => Some(&value.write_id),
        _ => None,
    }
}

/// Queues the embedding of the scope of the write `result` committed, in its unit of work,
/// when the write changed a claim and a qualified space is selected for the writer.
pub(super) fn queue_after_write(
    runtime: &IndexService,
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    command: &AdmittedCommand,
    result: &OperationResult,
) -> Result<(), StorageError> {
    let Some(write_id) = receipt(result) else {
        return Ok(());
    };
    let Some(write) = tx.knowledge().knowledge_write(write_id)? else {
        return Ok(());
    };
    if !write.changes.iter().any(|c| c.target_kind == "Claim")
        || runtime.configuration_in(tx, ctx)?.space.is_none()
    {
        return Ok(());
    }
    let controls = control::capture(tx, ctx)?;
    let job = Job {
        schema: JOB_SCHEMA,
        scope: write.scope.key(),
        write_id: write.write_id.clone(),
        actor: ctx.principal().clone(),
        actor_kind: ctx.principal_kind().as_str().into(),
        store_id: ctx.store_id().clone(),
        restore_epoch: ctx.restore_epoch().get(),
        fences: command.authorization().fences().to_vec(),
        source_context: approvals::save_source_context(ctx),
        control_roots: controls.roots,
        assignment_generation: controls.assignment_generation,
    };
    let event = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: event.clone(),
        actor: job.actor.clone(),
        project_id: write.scope.project().cloned(),
        root_id: ctx.root_id().cloned(),
        operation_id: None,
        aggregate_kind: AGGREGATE.into(),
        aggregate_id: write.write_id.as_str().to_owned(),
        aggregate_revision: 1,
        kind: "KnowledgeClaimVectorsQueued".into(),
        payload: json!({"writeId": write.write_id.as_str(), "scope": job.scope}),
    })?;
    let entry = tx.new_id(IdKind::QueueEntry);
    let now = tx.now_ms();
    // Unordered: one delivery at a time (the consumer's limit), and a failed delivery never
    // blocks a later write's.
    tx.queue().enqueue(NewQueueEntry {
        id: entry,
        consumer: VECTOR_CONSUMER.into(),
        event_id: event,
        root_id: ctx.root_id().cloned(),
        operation_id: None,
        partition: None,
        priority: 0,
        ready_at_ms: now,
        payload: serde_json::to_value(&job).map_err(|_| invalid("delivery"))?,
    })?;
    Ok(())
}

/// Embeds the delivery's scope's pending claims, batch by batch, each stored in its own write.
/// Returns when none is pending or no qualified space is selected (or its route is
/// unavailable: those claims stay pending for the next write of the scope).
pub(super) fn run(
    runtime: &Arc<IndexService>,
    entry: &QueueEntry,
    cancellation: &Cancellation,
) -> Result<(), StorageError> {
    let job = Job::from_delivery(entry)?;
    let scope = job.scope()?;
    loop {
        if cancellation.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        let (saved, claimed, index, at) =
            (job.clone(), entry.clone(), runtime.clone(), scope.clone());
        let step = run_write(
            runtime.store.as_ref(),
            WriteOptions::host_state(),
            move |tx| {
                let ctx = saved.restore(tx, &claimed)?;
                let selection = index.configuration_in(tx, &ctx)?;
                let Some(space) = selection.space else {
                    return Ok(None);
                };
                let id = space_id(&space.id)?;
                let pending = tx
                    .knowledge()
                    .pending_knowledge_claim_vectors(&at, &id, BATCH)?;
                if pending.is_empty() {
                    return Ok(None);
                }
                tx.conventions().put_convention_embedding_space(&space)?;
                Ok(Some((ctx, selection.source_digest, space, id, pending)))
            },
        );
        let (ctx, digest, space, id, pending) = match step {
            Ok(Some(step)) => step,
            // Nothing pending, no qualified space, or the writer may no longer act for the
            // scope: its claims stay pending for the scope's next write.
            Ok(None) | Err(StorageError::ScopeDenied) => return Ok(()),
            Err(error) => return Err(error),
        };
        let origin = EmbeddingOrigin::KnowledgeIndex {
            delivery_id: entry.id.clone(),
            scope: job.scope.clone(),
            first_claim: pending[0].0.clone(),
        };
        let texts: Vec<&str> = pending.iter().map(|(_, text)| text.as_str()).collect();
        let prepared = runtime.prepare_embedding_inputs(InputPreparation {
            context: &ctx,
            origin: &origin,
            embedding_space: &space.id,
            configuration_source_digest: &digest,
            texts: &texts,
            fences: &job.fences,
            cancellation,
            deadline_ms: None,
        })?;
        let request = EmbeddingRequest {
            request_id: prepared.request_id.clone(),
            origin,
            space: space.clone(),
            configuration_source_digest: digest,
            preparation_operation: prepared.operation_id,
            input_refs: prepared.input_refs,
            fences: job.fences.clone(),
            cancellation: cancellation.clone(),
            deadline_ms: None,
        };
        let outcome = match runtime.embed_batch(&prepared.context, request.clone()) {
            // The journal can close just before its acknowledgement is lost: observe it.
            Err(EmbeddingFailure::Unknown { .. }) => {
                runtime.observe_batch(&prepared.context, request)
            }
            outcome => outcome,
        };
        let vectors = match outcome {
            Ok(batch)
                if batch.request_id == prepared.request_id
                    && batch.space == space
                    && batch.vectors.len() == pending.len() =>
            {
                batch.vectors
            }
            Ok(_) => return Err(invalid("embedding population")),
            Err(EmbeddingFailure::Cancelled) => return Err(StorageError::Cancelled),
            Err(EmbeddingFailure::Storage(error)) => return Err(error),
            Err(EmbeddingFailure::Unavailable(_)) => return Ok(()),
            // Not established: the delivery retries and observes the same captured request.
            Err(EmbeddingFailure::Unknown { .. } | EmbeddingFailure::NotStarted { .. }) => {
                return Err(StorageError::Busy);
            }
        };
        let rows: Vec<(Id, String, Vec<f32>)> = pending
            .into_iter()
            .zip(vectors)
            .map(|((claim, text), vector)| (claim, text, vector))
            .collect();
        let (saved, claimed) = (job.clone(), entry.clone());
        run_write(
            runtime.store.as_ref(),
            WriteOptions::host_state(),
            move |tx| {
                saved.restore(tx, &claimed)?;
                tx.knowledge().insert_knowledge_claim_vectors(&id, &rows)
            },
        )?;
    }
}

// ---------------------------------------------------------------------------------------------
// Search

/// A prepared query vector, meaningful only under the same selection.
pub(super) struct QueryVector {
    pub space: Id,
    pub vector: Vec<f32>,
    configuration_digest: String,
}

impl QueryVector {
    /// Whether the selection this vector was embedded under is still the current one.
    pub fn current(
        &self,
        runtime: &IndexService,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
    ) -> Result<bool, StorageError> {
        Ok(runtime.configuration_read(read, ctx)?.source_digest == self.configuration_digest)
    }
}

/// Embeds `knowledge.search`'s text before the gateway opens its snapshot, when the query has
/// words, a qualified space is selected and the read scopes have vectors in it. `None`:
/// lexical results only.
pub(super) fn prepare_search(
    runtime: &IndexService,
    ctx: &InvocationContext,
    query: &AdmittedQuery,
    i: &ops::SearchInput,
    env: &Preparation<'_>,
) -> Result<Option<QueryVector>, Failure> {
    let words = model::words(&i.text.0);
    if words.is_empty() || words.len() > model::MAX_WORDS || runtime.is_stopped() {
        return Ok(None);
    }
    let env = Preparation::new(runtime.store.as_ref(), env.cancellation, env.deadline_ms);
    let origin = EmbeddingOrigin::Query {
        query_id: query.query_id().0.clone(),
        fingerprint: query.fingerprint().as_str().into(),
        scope_digest: query.fingerprint().as_str().into(),
    };
    let searchable = env.read(|read| {
        Ok((|| {
            if !crate::services::fences::authorization_holds(read, query.authorization())? {
                return Err(StorageError::ScopeDenied.into());
            }
            let scoped: Scoped =
                resolve_scope(read.artifacts(), read.authority(), ctx, &i.scope, false)?;
            let configuration = runtime.configuration_read(read, ctx)?;
            runtime.validate_query_custody_in(read, ctx, &origin, &configuration.source_digest)?;
            let Some(space) = configuration.space else {
                return Ok(None);
            };
            let id = space_id(&space.id)?;
            let (embedded, _) =
                knowledge_read(read).knowledge_vector_coverage(&scoped.keys(), &id)?;
            Ok::<_, Failure>((embedded > 0).then_some((configuration.source_digest, space, id)))
        })())
    })??;
    let Some((digest, space, id)) = searchable else {
        return Ok(None);
    };
    let vector = runtime.embed_query_text(
        ctx,
        query,
        origin,
        &space,
        &digest,
        &i.text.0,
        env.cancellation.cloned().unwrap_or_default(),
        env.deadline_ms,
    )?;
    Ok(vector
        .filter(|v| {
            v.len() == space.dimensions as usize
                && v.iter().all(|x| x.is_finite())
                && v.iter().any(|x| *x != 0.0)
        })
        .map(|vector| QueryVector {
            space: id,
            vector,
            configuration_digest: digest,
        }))
}

// ---------------------------------------------------------------------------------------------
// Diagnostics

/// The retrieval diagnostic's code and message for the read scopes `scoped`.
pub(super) fn retrieval_mode(
    runtime: Option<&IndexService>,
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    scoped: &Scoped,
) -> Result<(&'static str, String), StorageError> {
    let lexical = |reason: &str| {
        (
            "LexicalOnly",
            format!(
                "Retrieval is lexical only ({reason}): no qualified embedding space is \
                 available, so vector search is not run."
            ),
        )
    };
    let Some(runtime) = runtime else {
        return Ok(lexical("NoEmbeddingRoute"));
    };
    let configuration = runtime.configuration_read(read, ctx)?;
    let Some(space) = configuration.space else {
        return Ok(lexical(
            configuration
                .reason
                .as_deref()
                .unwrap_or("NoEmbeddingRoute"),
        ));
    };
    let (embedded, live) =
        knowledge_read(read).knowledge_vector_coverage(&scoped.keys(), &space_id(&space.id)?)?;
    Ok((
        "Hybrid",
        format!(
            "Retrieval is hybrid in embedding space {} ({}, {} dimensions): text matches, then \
             up to {VECTOR_NEIGHBORS} vector matches. {embedded} of {live} claims have vectors.",
            space.id, space.model, space.dimensions
        ),
    ))
}
