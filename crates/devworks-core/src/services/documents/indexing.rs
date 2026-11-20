//! Index work is a replayable local delivery. The receipt, exact source and delivery are
//! captured together. An immutable extraction checkpoint survives a stopped writer. A later
//! fenced transaction publishes the per-version pointer and settles the delivery together.
use super::extraction::{ExtractedBlock, ExtractionControl, ExtractionInput};
use super::{checked_document, version_ref};
use crate::{
    context::InvocationContext,
    fingerprint::{canonical_digest, sha256_hex},
    gateway::AdmittedCommand,
    ports::{AuthorityFence, IdKind, storage::*},
    protocol::{
        ActorId, Check, DomainErrorCode, EventId, Id, OperationId, OperationState, Revision,
        RevisionDimension, RootId, StoreId,
        data::{CitationSource, Coverage, CoverageState},
        provider::ArtifactRef,
        records::{Diagnostic, IndexRunRecord, Severity},
        results::{DomainDecision, OperationResult},
        support::{BoundedList, BoundedText},
    },
    services::{
        approvals,
        artifacts::{ArtifactService, lifecycle::ExactReadRefusal},
        conventions::{
            control,
            indexing::{
                IndexService,
                embedding::{EmbeddingFailure, EmbeddingOrigin, EmbeddingRequest},
                preparation::InputPreparation,
            },
        },
        effect_job,
        kernel::{Failure, Refusal, counter, rev},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub const CONSUMER: &str = "document_index";
const JOB_SCHEMA: u32 = 1;
const SOURCE_LIMIT: u64 = 32 * 1024 * 1024;
const NORMALIZED_LIMIT: usize = 16 * 1024 * 1024;
const COUNT_LIMIT: usize = 16_384;

pub struct DocumentExtraction {
    pub blocks: Vec<ExtractedBlock>,
    pub coverage: Coverage,
}

/// A configured native extractor. Its revision is part of every durable job and citation.
/// Implementations must observe the control and return only stable, bounded diagnostic IDs.
pub trait DocumentProcessor: Send + Sync {
    fn revision(&self) -> Revision;
    fn extract(
        &self,
        source: ExtractionInput<'_>,
        control: &ExtractionControl<'_>,
    ) -> Result<DocumentExtraction, Id>;
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Job {
    schema: u32,
    source: ArtifactRef,
    artifact_revision: i64,
    scope_revision: i64,
    reference_revision: i64,
    current_version: Option<Id>,
    extraction_revision: Revision,
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
        let job: Self = serde_json::from_value(entry.payload.clone()).map_err(|_| invalid())?;
        if entry.consumer != CONSUMER || job.schema != JOB_SCHEMA || entry.operation_id.is_some() {
            return Err(invalid());
        }
        Ok(job)
    }

    fn restore(
        &self,
        tx: &mut dyn WriteTransaction,
        entry: &QueueEntry,
        revision: Revision,
    ) -> Result<InvocationContext, StorageError> {
        if self.extraction_revision != revision {
            return Err(StorageError::ViewUnavailable);
        }
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
        let artifact =
            checked_document(tx, &ctx, &self.source.artifact_id).map_err(|error| match error {
                Failure::Storage(error) => error,
                _ => StorageError::ScopeDenied,
            })?;
        if artifact.revision != self.artifact_revision
            || artifact.scope_revision != self.scope_revision
            || artifact.reference_revision != self.reference_revision
            || artifact.current_version_id != self.current_version
        {
            return Err(StorageError::ViewUnavailable);
        }
        if let Some(project) = artifact.scope.project() {
            active(tx, project)?;
        }
        let source = version(tx, &artifact.id, &self.source.version_id)?;
        if !same_source(&source, &self.source) {
            return Err(StorageError::ViewUnavailable);
        }
        Ok(ctx)
    }
}

fn active(
    tx: &mut dyn WriteTransaction,
    project: &crate::protocol::ProjectId,
) -> Result<(), StorageError> {
    if tx
        .projects()
        .project(project)?
        .is_some_and(|value| value.lifecycle == "Active")
    {
        Ok(())
    } else {
        Err(StorageError::ViewUnavailable)
    }
}

fn version(
    tx: &mut dyn WriteTransaction,
    document: &Id,
    id: &Id,
) -> Result<ArtifactRef, StorageError> {
    tx.artifacts()
        .version(id)?
        .filter(|v| &v.artifact_id == document && v.live())
        .map(|v| version_ref(&v))
        .ok_or(StorageError::ViewUnavailable)
}

pub(super) fn same_source(left: &ArtifactRef, right: &ArtifactRef) -> bool {
    left.artifact_id == right.artifact_id
        && left.version_id == right.version_id
        && left.sha256 == right.sha256
        && left.byte_length == right.byte_length
        && left.media_type == right.media_type
}

pub(super) fn check_revision(
    artifact: &StoredArtifact,
    expected: &Revision,
) -> Result<(), StorageError> {
    if artifact.revision != expected.get() {
        return Err(StorageError::conflict(
            RevisionDimension::Artifact,
            "document",
            artifact.id.clone(),
            expected.clone(),
            rev(artifact.revision),
        ));
    }
    Ok(())
}

/// Only the owning scope can mutate document-wide retrieval settings. The managed library
/// supplies the visibility check first, so this never grants authority to an unknown ID.
pub(super) fn writable_document(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    doc: &Id,
) -> Result<StoredArtifact, Failure> {
    let artifact = checked_document(tx, ctx, doc)?;
    if ctx
        .project_id()
        .is_some_and(|project| artifact.scope.project() != Some(project))
    {
        return Err(Refusal::new(
            DomainErrorCode::PermissionDenied,
            "document",
            Some(doc.clone()),
        )
        .into());
    }
    if let Some(project) = artifact.scope.project() {
        active(tx, project)?;
    }
    Ok(artifact)
}

/// Repository and document indexing share the existing cancel command and result contract.
/// An ordinary repository run ID is left to the repository coordinator.
pub(crate) fn cancel_if_document(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    run: &Id,
) -> Result<Option<DomainDecision>, Failure> {
    let Some(entry) = tx
        .queue()
        .entry(run)?
        .filter(|entry| entry.consumer == CONSUMER)
    else {
        return Ok(None);
    };
    let job = Job::from_delivery(&entry)?;
    writable_document(tx, ctx, &job.source.artifact_id)?;
    let changed = matches!(
        entry.state,
        QueueState::Ready | QueueState::Claimed | QueueState::Reconciling
    ) && !entry.cancel_requested;
    let after = if changed {
        let after = tx.queue().cancel(run)?;
        if after.state == QueueState::Cancelled {
            mark_cancelled(tx, &after, &job)?;
        }
        record_run(
            tx,
            &after,
            &job,
            ctx.principal(),
            "DocumentIndexCancelRequested",
            None,
        )?;
        after
    } else {
        entry
    };
    let latest = tx
        .documents()
        .document_index(&job.source.version_id)?
        .filter(|index| index.latest == *run);
    let value = cancel_record(&after, latest.as_ref());
    let result = OperationResult::IndexingCancel { value };
    Ok(Some(if changed {
        DomainDecision::Applied { result }
    } else {
        DomainDecision::NoChange { result }
    }))
}

fn cancel_record(entry: &QueueEntry, index: Option<&DocumentIndexVersion>) -> IndexRunRecord {
    let state = super::super::queue::delivery_state(entry, None);
    let stage = match entry.state {
        QueueState::Claimed if entry.cancel_requested => "Cancelling",
        QueueState::Cancelled => "Cancelled",
        QueueState::Failed | QueueState::Dismissed => "Failed",
        QueueState::Completed
            if index.is_some_and(|index| index.stage == DocumentIndexStage::Partial) =>
        {
            "Partial"
        }
        QueueState::Completed => "Published",
        QueueState::Claimed => "Extracting",
        QueueState::Reconciling => "Held",
        QueueState::Ready => "Queued",
    };
    let gap = index
        .and_then(|index| index.diagnostic.clone())
        .or_else(|| {
            matches!(entry.state, QueueState::Failed | QueueState::Dismissed).then(|| {
                entry
                    .last_error
                    .as_deref()
                    .and_then(|error| Id::new(error).ok())
                    .unwrap_or_else(|| code("IndexFailed"))
            })
        });
    let diagnostics = gap
        .into_iter()
        .map(|id| Diagnostic {
            code: BoundedText(id.as_str().into()),
            id,
            severity: Severity::Warning,
            component: BoundedText("DocumentIndex".into()),
            message: BoundedText("The document index did not publish.".into()),
            correlation_id: None,
            evidence: None,
        })
        .collect();
    IndexRunRecord {
        id: entry.id.clone(),
        revision: rev(entry.revision),
        repository_ids: BoundedList(vec![]),
        stage: BoundedText(stage.into()),
        state,
        completed_units: counter(i64::from(entry.state == QueueState::Completed)),
        total_units: None,
        output_views: BoundedList(vec![]),
        diagnostics: BoundedList(diagnostics),
    }
}

fn mark_cancelled(
    tx: &mut dyn WriteTransaction,
    entry: &QueueEntry,
    job: &Job,
) -> Result<(), StorageError> {
    if let Some(mut index) = tx.documents().document_index(&job.source.version_id)?
        && index.latest == entry.id
        && index.stage != DocumentIndexStage::Removed
    {
        index.stage = DocumentIndexStage::Cancelled;
        index.diagnostic = Some(code("IndexCancelled"));
        tx.documents().put_document_index(&index)?;
    }
    Ok(())
}

/// Let generic before-dispatch cancellation update the same exact version.
pub struct QueueHook;

impl crate::services::queue::ConsumerHook for QueueHook {
    fn consumer(&self) -> &'static str {
        CONSUMER
    }
    fn cancelled(
        &self,
        tx: &mut dyn WriteTransaction,
        entry: &QueueEntry,
    ) -> Result<(), StorageError> {
        if entry.state == QueueState::Cancelled {
            mark_cancelled(tx, entry, &Job::from_delivery(entry)?)?;
        }
        Ok(())
    }
}

/// The shared job service owns admission and processing. An explicit host composition supplies
/// the native extractor before the feature is made available to product UI and tools.
pub struct DocumentIndexer {
    store: Arc<dyn ApplicationStore>,
    artifacts: Arc<ArtifactService>,
    extractor: Arc<dyn DocumentProcessor>,
    embeddings: Option<Arc<IndexService>>,
}

enum VectorPublication {
    Absent,
    Complete(DocumentEmbeddingRun),
    Partial(Id),
}

impl DocumentIndexer {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        artifacts: Arc<ArtifactService>,
        extractor: Arc<dyn DocumentProcessor>,
    ) -> Self {
        Self {
            store,
            artifacts,
            extractor,
            embeddings: None,
        }
    }

    pub fn with_embeddings(mut self, embeddings: Arc<IndexService>) -> Self {
        self.embeddings = Some(embeddings);
        self
    }

    pub(super) fn embeddings(&self) -> Option<&IndexService> {
        self.embeddings.as_deref()
    }

    pub(super) fn admit(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        document: &Id,
        id: &Id,
        expected: &Revision,
    ) -> Result<IndexRunRecord, Failure> {
        let artifact = writable_document(tx, ctx, document)?;
        check_revision(&artifact, expected)?;
        let source = version(tx, document, id)?;
        let prior = tx.documents().document_index(id)?;
        if let Some(prior) = &prior
            && matches!(
                prior.stage,
                DocumentIndexStage::Queued | DocumentIndexStage::Extracted
            )
            && let Some(entry) = tx.queue().entry(&prior.latest)?
            && matches!(
                entry.state,
                QueueState::Ready | QueueState::Claimed | QueueState::Reconciling
            )
            && Job::from_delivery(&entry)?.artifact_revision == artifact.revision
        {
            return Err(StorageError::Busy.into());
        }
        let controls = control::capture(tx, ctx)?;
        let job = Job {
            schema: JOB_SCHEMA,
            source,
            artifact_revision: artifact.revision,
            scope_revision: artifact.scope_revision,
            reference_revision: artifact.reference_revision,
            current_version: artifact.current_version_id,
            extraction_revision: self.extractor.revision(),
            actor: ctx.principal().clone(),
            actor_kind: ctx.principal_kind().as_str().into(),
            store_id: ctx.store_id().clone(),
            restore_epoch: ctx.restore_epoch().get(),
            fences: command.authorization().fences().to_vec(),
            source_context: approvals::save_source_context(ctx),
            control_roots: controls.roots,
            assignment_generation: controls.assignment_generation,
        };
        let run = tx.new_id(IdKind::QueueEntry);
        let event = EventId(tx.new_id(IdKind::Event));
        let ready_at_ms = tx.now_ms();
        tx.history().append(NewEvent {
            id: event.clone(),
            actor: job.actor.clone(),
            project_id: artifact.scope.project().cloned(),
            root_id: ctx.root_id().cloned(),
            operation_id: None,
            aggregate_kind: "document_index_run".into(),
            aggregate_id: run.as_str().into(),
            aggregate_revision: 1,
            kind: "DocumentIndexQueued".into(),
            payload: json!({"documentId":document,"versionId":id}),
        })?;
        tx.queue().enqueue(NewQueueEntry {
            id: run.clone(),
            consumer: CONSUMER.into(),
            event_id: event,
            root_id: ctx.root_id().cloned(),
            operation_id: None,
            partition: None,
            priority: 0,
            ready_at_ms,
            payload: serde_json::to_value(&job).map_err(|_| invalid())?,
        })?;
        tx.documents().put_document_index(&DocumentIndexVersion {
            version: id.clone(),
            latest: run.clone(),
            stage: DocumentIndexStage::Queued,
            diagnostic: None,
            published: prior.and_then(|view| view.published),
        })?;
        Ok(IndexRunRecord {
            id: run,
            revision: rev(1),
            repository_ids: BoundedList(vec![]),
            stage: BoundedText("Queued".into()),
            state: OperationState::IntentRecorded,
            completed_units: counter(0),
            total_units: None,
            output_views: BoundedList(vec![]),
            diagnostics: BoundedList(vec![]),
        })
    }

    /// The caller runs this off the dispatcher runtime. The lease and authoritative source
    /// are checked before opening bytes, before checkpointing, and before publication.
    pub fn run(&self, entry: &QueueEntry, cancellation: &Cancellation) -> Result<(), StorageError> {
        let job = Job::from_delivery(entry)?;
        match self.process(entry, &job, cancellation) {
            Err(StorageError::Cancelled) => self.finish(entry, &job, None, true),
            Err(StorageError::ViewUnavailable | StorageError::ScopeDenied) => {
                self.finish(entry, &job, Some(code("SourceChanged")), false)
            }
            outcome => outcome,
        }
    }

    fn process(
        &self,
        entry: &QueueEntry,
        job: &Job,
        cancellation: &Cancellation,
    ) -> Result<(), StorageError> {
        let (version, revision) = (
            job.source.version_id.clone(),
            job.extraction_revision.clone(),
        );
        let (context, checkpoint) = self.fenced(entry, job, move |tx, ctx, mut index| {
            let checkpoint = tx.documents().document_checkpoint(&version, &revision)?;
            if checkpoint.is_some() && index.stage == DocumentIndexStage::Queued {
                index.stage = DocumentIndexStage::Extracted;
                tx.documents().put_document_index(&index)?;
            }
            Ok((ctx, checkpoint))
        })?;
        if cancellation.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        if checkpoint.is_none() {
            let source =
                match self
                    .artifacts
                    .read_exact(&*self.store, &context, &job.source, SOURCE_LIMIT)
                {
                    Ok((bytes, _)) => bytes,
                    Err(ExactReadRefusal::Storage(error)) => return Err(error),
                    Err(ExactReadRefusal::NotFound) => return Err(StorageError::ViewUnavailable),
                    Err(ExactReadRefusal::TooLarge { .. }) => {
                        return self.finish(entry, job, Some(code("SourceTooLarge")), false);
                    }
                    Err(ExactReadRefusal::Unreadable) => {
                        return self.finish(entry, job, Some(code("SourceUnreadable")), false);
                    }
                };
            let control = ExtractionControl {
                cancelled: &|| cancellation.is_cancelled(),
                deadline: Instant::now() + Duration::from_secs(30),
            };
            let output = self.extractor.extract(
                ExtractionInput {
                    version: &job.source,
                    revision: &job.extraction_revision,
                    bytes: &source,
                },
                &control,
            );
            if cancellation.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            match output {
                Ok(value) => self.save_checkpoint(entry, job, value)?,
                Err(reason) => return self.finish(entry, job, Some(reason), false),
            }
        }
        if cancellation.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        let vectors = self.embed_document(entry, job, cancellation)?;
        self.publish(entry, job, vectors)
    }

    fn fenced<T: Send + 'static>(
        &self,
        entry: &QueueEntry,
        job: &Job,
        action: impl FnOnce(
            &mut dyn WriteTransaction,
            InvocationContext,
            DocumentIndexVersion,
        ) -> Result<T, StorageError>
        + Send
        + 'static,
    ) -> Result<T, StorageError> {
        let (entry, job, processor) = (entry.clone(), job.clone(), self.extractor.clone());
        run_write(&*self.store, WriteOptions::host_state(), move |tx| {
            let actual = current_claim(tx, &entry)?;
            if actual.cancel_requested || actual.cancellation_epoch != entry.cancellation_epoch {
                return Err(StorageError::Cancelled);
            }
            let ctx = job.restore(tx, &entry, processor.revision())?;
            let index = tx
                .documents()
                .document_index(&job.source.version_id)?
                .filter(|v| v.latest == entry.id && v.stage != DocumentIndexStage::Removed)
                .ok_or(StorageError::ViewUnavailable)?;
            action(tx, ctx, index)
        })
    }

    fn save_checkpoint(
        &self,
        entry: &QueueEntry,
        job: &Job,
        extraction: DocumentExtraction,
    ) -> Result<(), StorageError> {
        let id = canonical_digest(&json!([job.source, job.extraction_revision]))
            .map_err(|_| invalid())?;
        let checkpoint = DocumentCheckpoint {
            id: Id::new(id).map_err(|_| invalid())?,
            version: job.source.version_id.clone(),
            revision: job.extraction_revision.clone(),
            source_digest: job.source.sha256.as_str().into(),
            coverage: extraction.coverage,
            blocks: extraction.blocks.len().try_into().map_err(|_| invalid())?,
        };
        validate(&checkpoint, &job.source, &extraction.blocks)?;
        self.fenced(entry, job, move |tx, _, mut index| {
            if tx
                .documents()
                .document_checkpoint(&checkpoint.version, &checkpoint.revision)?
                .is_none()
            {
                tx.documents()
                    .insert_document_checkpoint(&checkpoint, &extraction.blocks)?;
            }
            index.stage = DocumentIndexStage::Extracted;
            tx.documents().put_document_index(&index)
        })
    }

    /// Checkpoints every completed provider batch under the same lease and original routing
    /// witness. The producer observes its exact existing journal before sending.
    fn embed_document(
        &self,
        entry: &QueueEntry,
        job: &Job,
        cancellation: &Cancellation,
    ) -> Result<VectorPublication, StorageError> {
        let Some(embeddings) = &self.embeddings else {
            return Ok(VectorPublication::Absent);
        };
        let (runtime, run, version, revision) = (
            embeddings.clone(),
            entry.id.clone(),
            job.source.version_id.clone(),
            job.extraction_revision.clone(),
        );
        let (plan, context, selection) = self.fenced(entry, job, move |tx, ctx, _| {
            let selection = runtime.configuration_in(tx, &ctx)?;
            let checkpoint = tx
                .documents()
                .document_checkpoint(&version, &revision)?
                .ok_or(StorageError::ViewUnavailable)?;
            let plan = match tx.documents().document_embedding_run(&run)? {
                Some(plan) if plan.extraction == checkpoint.id => plan,
                Some(_) => return Err(invalid()),
                None => {
                    let space = if let Some(space) = &selection.space {
                        tx.conventions().put_convention_embedding_space(space)?;
                        Some(Id::new(space.id.clone()).map_err(|_| invalid())?)
                    } else {
                        None
                    };
                    let plan = DocumentEmbeddingRun {
                        run,
                        extraction: checkpoint.id,
                        space,
                        configuration_digest: selection.source_digest.clone(),
                        complete: false,
                    };
                    tx.documents().insert_document_embedding_run(&plan)?;
                    plan
                }
            };
            Ok((plan, ctx, selection))
        })?;
        if plan.space.is_none() {
            return Ok(VectorPublication::Absent);
        }
        let Some(space) = selection.space.filter(|space| {
            plan.space
                .as_ref()
                .is_some_and(|id| id.as_str() == space.id)
                && plan.configuration_digest == selection.source_digest
        }) else {
            return Ok(VectorPublication::Partial(code("EmbeddingRouteChanged")));
        };
        let mut fences = job.fences.clone();
        fences.push(AuthorityFence::DocumentIndex {
            run: entry.id.clone(),
            generation: entry.generation,
        });
        loop {
            if cancellation.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            let snapshot = plan.clone();
            let blocks = self.fenced(entry, job, move |tx, _, _| {
                tx.documents().pending_document_vectors(&snapshot, 32)
            })?;
            let Some((first_block, _)) = blocks.first() else {
                return Ok(VectorPublication::Complete(plan));
            };
            let origin = EmbeddingOrigin::DocumentIndex {
                run_id: plan.run.clone(),
                extraction_id: plan.extraction.clone(),
                source: job.source.clone(),
                first_block: first_block.clone(),
            };
            let texts = blocks
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>();
            let prepared = embeddings.prepare_embedding_inputs(InputPreparation {
                context: &context,
                origin: &origin,
                embedding_space: &space.id,
                configuration_source_digest: &plan.configuration_digest,
                texts: &texts,
                fences: &fences,
                cancellation,
                deadline_ms: None,
            })?;
            let request_id = prepared.request_id.clone();
            let request = EmbeddingRequest {
                request_id: request_id.clone(),
                origin,
                space: space.clone(),
                configuration_source_digest: plan.configuration_digest.clone(),
                preparation_operation: prepared.operation_id,
                input_refs: prepared.input_refs,
                fences: fences.clone(),
                cancellation: cancellation.clone(),
                deadline_ms: None,
            };
            let outcome = match embeddings.embed_batch(&prepared.context, request.clone()) {
                Err(EmbeddingFailure::Unknown { .. }) => {
                    // The producer may have closed its original journal just before the
                    // local acknowledgement was lost. Observation cannot send another request.
                    embeddings.observe_batch(&prepared.context, request)
                }
                outcome => outcome,
            };
            let vectors = match outcome {
                Ok(outcome)
                    if outcome.request_id == request_id
                        && outcome.space == space
                        && outcome.vectors.len() == blocks.len() =>
                {
                    outcome.vectors
                }
                Ok(_) => return Err(invalid()),
                Err(EmbeddingFailure::Cancelled) => return Err(StorageError::Cancelled),
                Err(EmbeddingFailure::Storage(error)) => return Err(error),
                Err(EmbeddingFailure::Unknown { .. }) => {
                    return Ok(VectorPublication::Partial(code("EmbeddingOutcomeUnknown")));
                }
                Err(EmbeddingFailure::Unavailable(_) | EmbeddingFailure::NotStarted { .. }) => {
                    return Ok(VectorPublication::Partial(code(
                        "EmbeddingRouteUnavailable",
                    )));
                }
            };
            let vectors = blocks
                .into_iter()
                .map(|(id, _)| id)
                .zip(vectors)
                .collect::<Vec<_>>();
            let snapshot = plan.clone();
            self.fenced(entry, job, move |tx, _, _| {
                tx.documents().insert_document_vectors(&snapshot, &vectors)
            })?;
        }
    }

    fn publish(
        &self,
        entry: &QueueEntry,
        job: &Job,
        vectors: VectorPublication,
    ) -> Result<(), StorageError> {
        let (claimed, saved, runtime) = (entry.clone(), job.clone(), self.embeddings.clone());
        self.fenced(entry, job, move |tx, ctx, mut index| {
            let extraction = tx
                .documents()
                .document_checkpoint(&saved.source.version_id, &saved.extraction_revision)?
                .filter(|value| value.source_digest == saved.source.sha256.as_str())
                .ok_or_else(invalid)?;
            let diagnostic = match vectors {
                VectorPublication::Absent => None,
                VectorPublication::Partial(reason) => Some(reason),
                VectorPublication::Complete(run) => {
                    let selection = runtime
                        .as_ref()
                        .ok_or_else(invalid)?
                        .configuration_in(tx, &ctx)?;
                    if selection.source_digest == run.configuration_digest {
                        tx.documents().complete_document_vectors(&run)?;
                        None
                    } else {
                        Some(code("EmbeddingRouteChanged"))
                    }
                }
            };
            index.stage = if diagnostic.is_none()
                && matches!(extraction.coverage.state, CoverageState::Complete)
            {
                DocumentIndexStage::Ready
            } else {
                DocumentIndexStage::Partial
            };
            index.published = Some((claimed.id.clone(), extraction.revision));
            index.diagnostic = diagnostic.clone();
            tx.documents().put_document_index(&index)?;
            if saved.current_version.as_ref() == Some(&saved.source.version_id) {
                tx.documents()
                    .set_document_retrieval(&saved.source.artifact_id, true)?;
            }
            let event = if index.stage == DocumentIndexStage::Partial {
                "DocumentIndexPartiallyPublished"
            } else {
                "DocumentIndexPublished"
            };
            record_run(
                tx,
                &claimed,
                &saved,
                &saved.actor,
                event,
                diagnostic.as_ref(),
            )?;
            tx.queue().settle(
                &claimed.id,
                &claimed.fence().ok_or_else(invalid)?,
                Settlement::Completed,
            )?;
            Ok(())
        })
    }

    fn finish(
        &self,
        entry: &QueueEntry,
        job: &Job,
        reason: Option<Id>,
        interrupted: bool,
    ) -> Result<(), StorageError> {
        let (entry, job) = (entry.clone(), job.clone());
        run_write(&*self.store, WriteOptions::settlement(), move |tx| {
            let actual = current_claim(tx, &entry)?;
            let cancelled =
                actual.cancel_requested || actual.cancellation_epoch != entry.cancellation_epoch;
            if interrupted && !cancelled {
                return Err(StorageError::Cancelled); // owner shutdown; preserve the checkpoint
            }
            let fence = entry.fence().ok_or_else(invalid)?;
            if cancelled {
                mark_cancelled(tx, &entry, &job)?;
                record_run(
                    tx,
                    &actual,
                    &job,
                    &job.actor,
                    "DocumentIndexCancelled",
                    None,
                )?;
                tx.queue()
                    .settle(&entry.id, &fence, Settlement::Cancelled)?;
                return Ok(());
            }
            let reason = reason.unwrap_or_else(|| code("IndexFailed"));
            if let Some(mut index) = tx.documents().document_index(&job.source.version_id)?
                && index.latest == entry.id
                && index.stage != DocumentIndexStage::Removed
            {
                index.stage = DocumentIndexStage::Failed;
                index.diagnostic = Some(reason.clone());
                tx.documents().put_document_index(&index)?;
            }
            record_run(
                tx,
                &actual,
                &job,
                &job.actor,
                "DocumentIndexFailed",
                Some(&reason),
            )?;
            tx.queue().fail(
                &entry.id,
                &fence,
                reason.as_str(),
                FailureDisposition::Terminal,
            )?;
            Ok(())
        })
    }
}

fn active_embedding_claim(
    entry: &QueueEntry,
    generation: i64,
    boot: Option<&Id>,
    now: i64,
) -> bool {
    entry.consumer == CONSUMER
        && entry.state == QueueState::Claimed
        && entry.generation == generation
        && entry.claim_boot_id.as_ref() == boot
        && entry.lease_until_ms.is_some_and(|expiry| expiry > now)
        && !entry.cancel_requested
}

pub(crate) fn embedding_fence_in(
    tx: &mut dyn WriteTransaction,
    run: &Id,
    generation: i64,
) -> Result<bool, StorageError> {
    let boot = tx.store().boot_id.clone();
    let now = tx.now_ms();
    let Some(entry) = tx
        .queue()
        .entry(run)?
        .filter(|entry| active_embedding_claim(entry, generation, boot.as_ref(), now))
    else {
        return Ok(false);
    };
    let job = Job::from_delivery(&entry)?;
    match job.restore(tx, &entry, job.extraction_revision.clone()) {
        Ok(_) => Ok(tx
            .documents()
            .document_index(&job.source.version_id)?
            .is_some_and(|index| {
                index.latest == entry.id && index.stage == DocumentIndexStage::Extracted
            })),
        Err(
            StorageError::ScopeDenied | StorageError::ViewUnavailable | StorageError::Cancelled,
        ) => Ok(false),
        Err(error) => Err(error),
    }
}

pub(crate) fn embedding_fence_read(
    read: &dyn ReadSnapshot,
    run: &Id,
    generation: i64,
) -> Result<bool, StorageError> {
    let Some(entry) = read.queue().entry(run)?.filter(|entry| {
        active_embedding_claim(
            entry,
            generation,
            read.stamp().boot_id.as_ref(),
            read.stamp().now_ms,
        )
    }) else {
        return Ok(false);
    };
    let job = Job::from_delivery(&entry)?;
    let artifacts = read.artifacts();
    let current = artifacts.artifact(&job.source.artifact_id)?;
    let source = artifacts.version(&job.source.version_id)?;
    Ok(job.store_id == read.stamp().store_id
        && job.restore_epoch == read.stamp().restore_epoch
        && current.is_some_and(|artifact| {
            matches!(artifact.state, ArtifactState::Live)
                && artifact.revision == job.artifact_revision
                && artifact.scope_revision == job.scope_revision
                && artifact.reference_revision == job.reference_revision
                && artifact.current_version_id == job.current_version
        })
        && source.is_some_and(|version| {
            version.live() && same_source(&job.source, &version_ref(&version))
        })
        && read
            .documents()
            .document_index(&job.source.version_id)?
            .is_some_and(|index| {
                index.latest == entry.id && index.stage == DocumentIndexStage::Extracted
            }))
}

fn current_claim(
    tx: &mut dyn WriteTransaction,
    entry: &QueueEntry,
) -> Result<QueueEntry, StorageError> {
    let boot = tx.store().boot_id.clone();
    let now = tx.now_ms();
    tx.queue()
        .entry(&entry.id)?
        .filter(|actual| {
            actual.state == QueueState::Claimed
                && actual.consumer == CONSUMER
                && actual.event_id == entry.event_id
                && actual.payload == entry.payload
                && actual.claim_boot_id == boot
                && actual.fence() == entry.fence()
                && actual.lease_until_ms.is_some_and(|expiry| expiry > now)
        })
        .ok_or_else(|| StorageError::LeaseLost {
            kind: CONSUMER,
            id: entry.id.as_str().into(),
        })
}

fn record_run(
    tx: &mut dyn WriteTransaction,
    entry: &QueueEntry,
    job: &Job,
    actor: &ActorId,
    event: &str,
    diagnostic: Option<&Id>,
) -> Result<(), StorageError> {
    let origin = tx.history().event(&entry.event_id)?.ok_or_else(invalid)?;
    let event_id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: event_id,
        actor: actor.clone(),
        project_id: origin.project_id,
        root_id: entry.root_id.clone(),
        operation_id: None,
        aggregate_kind: "document_index_run".into(),
        aggregate_id: entry.id.as_str().into(),
        aggregate_revision: entry.revision,
        kind: event.into(),
        payload: json!({"documentId":job.source.artifact_id,
            "versionId":job.source.version_id,"diagnostic":diagnostic}),
    })?;
    Ok(())
}

fn validate(
    checkpoint: &DocumentCheckpoint,
    source: &ArtifactRef,
    blocks: &[ExtractedBlock],
) -> Result<(), StorageError> {
    checkpoint.coverage.check().map_err(|_| invalid())?;
    if blocks.len() > COUNT_LIMIT
        || blocks.iter().map(|block| block.text.len()).sum::<usize>() > NORMALIZED_LIMIT
    {
        return Err(invalid());
    }
    for block in blocks {
        let bound = matches!(&block.citation.source, CitationSource::Managed { version, extraction_revision }
            if same_source(version, source) && extraction_revision == &checkpoint.revision);
        if !bound
            || block.text.is_empty()
            || block.text.len() > 4096
            || block.citation.content_digest.as_str() != sha256_hex(block.text.as_bytes())
        {
            return Err(invalid());
        }
        block.citation.check().map_err(|_| invalid())?;
    }
    Ok(())
}

pub(super) fn code(id: &str) -> Id {
    Id::new(id).expect("static document diagnostic")
}

fn invalid() -> StorageError {
    StorageError::InvalidRecord("document index checkpoint".into())
}
