//! Admission persists the exact reviewed diff and the durable request together.
use super::{
    EFFECT_CONSUMER, ReviewService, ReviewStateRecord, ReviewTarget, decode, not_implemented,
};
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::{canonical_digest, sha256_hex};
use crate::gateway::AdmittedCommand;
use crate::operations::{OperationInput, reviews as ops};
use crate::ports::storage::{
    ArtifactScope, FEATURE, NewEvent, NewOperation, NewQueueEntry, StorageError, StoredCodeReview,
    StoredGitWorktree, WriteTransaction,
};
use crate::ports::{CommandHandler, Decided, HandlerFailure, IdKind, Preparation, Prepared};
use crate::protocol::provider::Digest;
use crate::protocol::records::{
    LocalReviewEvidence, LocalReviewTarget, ReviewObservationState, ReviewRecord, ReviewState,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::BoundedList;
use crate::protocol::{DomainErrorCode, EventId, Id, OperationId, RootId};
use crate::services::artifacts::{StagedFile, host::HostArtifactPublication, version_ref};
use crate::services::kernel::{Failure, Refusal, counter, decided, not_found, rev};
use serde_json::json;

const MAX_REVIEW_BYTES: usize = 4 * 1024 * 1024;

pub(super) struct LocalPatch {
    pub worktree: StoredGitWorktree,
    pub repository_revision: i64,
    pub staged: StagedFile,
}

pub(super) struct LocalInput {
    digest: Digest,
    pub patches: Vec<LocalPatch>,
}

impl ReviewService {
    fn prepare_local(
        &self,
        ctx: &InvocationContext,
        root: &RootId,
        local: &LocalReviewTarget,
        env: &Preparation<'_>,
    ) -> Result<LocalInput, Failure> {
        let (feature, task) = env.read(|snapshot| {
            Ok((
                crate::services::work::visible_work(snapshot.work(), ctx, &root.0),
                crate::services::work::visible_work(snapshot.work(), ctx, &local.task_id),
            ))
        })?;
        let (feature, task) = (feature?, task?);
        if feature.kind != FEATURE
            || task.parent_feature_id.as_ref() != Some(&feature.id)
            || task.project_id != feature.project_id
        {
            return Err(not_found("feature_task", &local.task_id).into());
        }
        let diffs = local
            .diffs
            .0
            .iter()
            .map(|diff| (local.task_id.clone(), diff.clone()))
            .collect();
        self.capture_diffs(ctx, env, diffs)
    }

    pub(super) fn capture_diffs(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        mut diffs: Vec<(Id, crate::protocol::records::LocalReviewDiff)>,
    ) -> Result<LocalInput, Failure> {
        diffs.sort_unstable_by(|(_, a), (_, b)| {
            a.repository_id.as_str().cmp(b.repository_id.as_str())
        });
        let mut captured = Vec::with_capacity(diffs.len());
        let mut witnesses = Vec::with_capacity(diffs.len());
        let mut size: usize = 0;
        for (work, diff) in diffs {
            let (worktree, repository_revision, patch) = self
                .git
                .capture_local_review_patch(ctx, env, &work, &diff)?;
            size = size.saturating_add(patch.len());
            if patch.is_empty() || size > MAX_REVIEW_BYTES {
                return Err(refused(
                    DomainErrorCode::InvalidArgument,
                    "review_patch_size",
                    &diff.repository_id,
                ));
            }
            let binding = json!({
                "source": diff, "workId": work, "sha256": sha256_hex(&patch)
            });
            witnesses.push(binding.clone());
            captured.push((worktree, repository_revision, patch, binding));
        }
        let digest = canonical_digest(&json!(witnesses))
            .map_err(|_| StorageError::InvalidRecord("invalid review diff identity".into()))?;
        let mut patches = Vec::with_capacity(captured.len());
        for (worktree, repository_revision, patch, binding) in captured {
            if env.stopped(self.clock.now_ms().get()) {
                return Err(StorageError::Cancelled.into());
            }
            let staged = self
                .artifacts
                .stage_host_bytes(
                    self.store.as_ref(),
                    &patch,
                    json!({"kind":"CodeReviewDiff","binding":binding}),
                )?
                .map_err(|_| {
                    refused(
                        DomainErrorCode::ExternalFailure,
                        "review_patch",
                        &worktree.id,
                    )
                })?;
            patches.push(LocalPatch {
                worktree,
                repository_revision,
                staged,
            });
        }
        Ok(LocalInput {
            digest: Digest(digest),
            patches,
        })
    }

    fn request_local(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        input: &ops::RequestInput,
        local: &LocalReviewTarget,
    ) -> Result<DomainDecision, Failure> {
        let feature = crate::services::work::visible_work(tx.work(), ctx, &input.root_id.0)?;
        let task = crate::services::work::visible_work(tx.work(), ctx, &local.task_id)?;
        if feature.kind != FEATURE
            || task.parent_feature_id.as_ref() != Some(&feature.id)
            || task.project_id != feature.project_id
            || ctx.root_id().is_some_and(|root| root != &input.root_id)
            || (ctx.principal_kind() == PrincipalKind::Agent
                && ctx.root_id() != Some(&input.root_id))
        {
            return Err(not_found("feature_task", &local.task_id).into());
        }
        crate::services::work::open_project(tx, ctx, &feature.project_id)?;
        if input.expected_revision.get() != feature.revision {
            return Err(StorageError::conflict(
                crate::protocol::RevisionDimension::Work,
                "feature",
                feature.id.clone(),
                input.expected_revision.clone(),
                rev(feature.revision),
            )
            .into());
        }
        if &input.policy_revision != ctx.policy_revision() {
            return Err(refused(
                DomainErrorCode::SourceChanged,
                "review_policy",
                &feature.id,
            ));
        }
        let prepared = command
            .prepared::<Result<LocalInput, Failure>>()
            .ok_or_else(|| refused(DomainErrorCode::Conflict, "review_preparation", &task.id))?
            .as_ref()
            .map_err(Clone::clone)?;
        let mut after = None;
        loop {
            let rows = tx.git().code_reviews(
                Some(&feature.project_id),
                Some(&input.root_id),
                after.as_ref(),
                128,
            )?;
            for row in &rows {
                let saved: ReviewStateRecord = decode(&row.body)?;
                if let ReviewTarget::Local { target } = &saved.target
                    && target.task_id == task.id
                {
                    if saved.value.head_digest.0 == prepared.digest.0
                        && saved.revision_of_policy == input.policy_revision.get()
                    {
                        return Ok(DomainDecision::NoChange {
                            result: OperationResult::ReviewsRequest { value: saved.value },
                        });
                    }
                    return Err(refused(
                        DomainErrorCode::Conflict,
                        "local_review_already_requested",
                        &task.id,
                    ));
                }
            }
            if rows.len() < 128 {
                break;
            }
            after = rows.last().map(|row| row.id.clone());
        }
        let selected = tx.work().work_repositories(&task.id)?;
        for patch in &prepared.patches {
            if tx.git().git_worktree(&patch.worktree.id)?.as_ref() != Some(&patch.worktree)
                || !selected.contains(&patch.worktree.repository_id)
                || !tx
                    .projects()
                    .repository_projects(&patch.worktree.repository_id)?
                    .contains(&feature.project_id)
                || tx
                    .projects()
                    .repository(&patch.worktree.repository_id)?
                    .is_none_or(|repository| {
                        repository.revision != patch.repository_revision
                            || repository.lifecycle != "Active"
                    })
            {
                return Err(refused(
                    DomainErrorCode::SourceChanged,
                    "review_worktree",
                    &patch.worktree.id,
                ));
            }
        }

        let id = tx.new_id(IdKind::Link);
        let operation = OperationId(tx.new_id(IdKind::Operation));
        let mut sources = Vec::with_capacity(prepared.patches.len());
        for patch in &prepared.patches {
            let version = self.artifacts.commit_host_bytes_in_scope(
                tx, &patch.staged, HostArtifactPublication {
                    title: "Exact code review diff",
                    media_type: "text/x-diff",
                    provenance: json!({"kind":"CodeReviewDiff","reviewId":id,
                        "worktreeId":patch.worktree.id,"repositoryId":patch.worktree.repository_id}),
                    scope: ArtifactScope::Project(feature.project_id.clone()),
                    owner: ctx.principal().clone(),
                }
            )?;
            sources.push(super::ReviewDiffArtifact {
                repository_id: patch.worktree.repository_id.clone(),
                reference: version_ref(&version),
            });
        }
        let saved = ReviewStateRecord {
            value: ReviewRecord {
                id: id.clone(),
                revision: rev(1),
                root_id: input.root_id.clone(),
                state: ReviewState::Pending,
                barrier: None,
                finding_ids: BoundedList(Vec::new()),
                head_digest: prepared.digest.clone(),
                sources: BoundedList(Vec::new()),
                local_review: Some(LocalReviewEvidence {
                    target: local.clone(),
                    diff_digest: prepared.digest.clone(),
                    state: ReviewObservationState::Requested,
                    reviewer_actor_id: None,
                    reviewer_session_id: None,
                    completed_at: None,
                    finding_count: counter(0),
                    report: None,
                }),
                candidate_repair: None,
            },
            target: ReviewTarget::Local {
                target: local.clone(),
            },
            requested_by: ctx.principal().clone(),
            revision_of_policy: input.policy_revision.get(),
            source_artifacts: sources,
            local_binding: None,
            operation: operation.clone(),
            restore_epoch: tx.store().restore_epoch,
            failure: None,
            final_rounds: Vec::new(),
            verified_candidates: Vec::new(),
        };
        persist_request(tx, ctx, command, &feature.project_id, saved)
    }
}

pub(super) fn persist_request(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    command: &AdmittedCommand,
    project: &crate::protocol::ProjectId,
    saved: ReviewStateRecord,
) -> Result<DomainDecision, Failure> {
    let root = saved.value.root_id.clone();
    let id = saved.value.id.clone();
    let operation = saved.operation.clone();
    tx.operations().record_intent(NewOperation {
        id: operation.clone(),
        root_id: Some(root.clone()),
        assignment_id: ctx.assignment_id().cloned(),
        capability: "reviews.request".into(),
        schema_version: 1,
        args_digest: command.fingerprint().as_str().into(),
        payload: json!({"reviewId":id}),
    })?;
    tx.git().put_code_review(
        &StoredCodeReview {
            id: id.clone(),
            root_id: root.clone(),
            project_id: project.clone(),
            revision: 1,
            state: "Pending".into(),
            body: serde_json::to_value(&saved)
                .map_err(|_| StorageError::InvalidRecord("code review state".into()))?,
        },
        None,
    )?;
    let event = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: event.clone(),
        actor: ctx.principal().clone(),
        project_id: Some(project.clone()),
        root_id: Some(root.clone()),
        operation_id: Some(operation.clone()),
        aggregate_kind: "code_review".into(),
        aggregate_id: id.as_str().into(),
        aggregate_revision: 1,
        kind: "CodeReviewRequested".into(),
        payload: json!({"reviewId":id,"headDigest":saved.value.head_digest}),
    })?;
    let queue = tx.new_id(IdKind::QueueEntry);
    let now = tx.now_ms();
    tx.queue().enqueue(NewQueueEntry {
        id: queue,
        consumer: EFFECT_CONSUMER.into(),
        event_id: event,
        root_id: Some(root.clone()),
        operation_id: Some(operation.clone()),
        partition: None,
        priority: 0,
        ready_at_ms: now,
        payload: json!({"reviewId":id}),
    })?;
    Ok(DomainDecision::Accepted {
        operation_id: operation,
    })
}

pub(super) fn refused(code: DomainErrorCode, target_kind: &'static str, target: &Id) -> Failure {
    Refusal::new(code, target_kind, Some(target.clone())).into()
}

impl CommandHandler for ReviewService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        let OperationInput::Reviews(ops::Input::Request(input)) = command.input() else {
            return Ok(None);
        };
        if let Some(proof) = input.candidate_repair.as_ref() {
            return Ok(Some(Prepared::new(self.prepare_candidate(ctx, proof, env))));
        }
        let prepared = if let Some(local) = input.local_target.as_ref() {
            self.prepare_local(ctx, &input.root_id, local, env)
        } else if let Some(barrier) = input.barrier.as_ref() {
            self.prepare_final(ctx, barrier, env)
        } else {
            return Ok(None);
        };
        Ok(Some(Prepared::new(prepared)))
    }

    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let outcome = match command.input() {
            OperationInput::Reviews(ops::Input::Request(input)) if input.local_target.is_some() => {
                self.request_local(
                    tx,
                    ctx,
                    command,
                    input,
                    input.local_target.as_ref().expect("matched local target"),
                )
            }
            OperationInput::Reviews(ops::Input::Request(input)) if input.barrier.is_some() => self
                .request_final(
                    tx,
                    ctx,
                    command,
                    input,
                    input.barrier.as_ref().expect("matched final barrier"),
                ),
            OperationInput::Reviews(ops::Input::Request(input))
                if input.candidate_repair.is_some() =>
            {
                self.verify_candidate(
                    tx,
                    ctx,
                    command,
                    input,
                    input
                        .candidate_repair
                        .as_ref()
                        .expect("matched candidate proof"),
                )
            }
            OperationInput::Reviews(ops::Input::Respond(input)) => self.respond(tx, ctx, input),
            OperationInput::Reviews(ops::Input::Adjudicate(input)) => {
                self.adjudicate(tx, ctx, input)
            }
            _ => Err(not_implemented()),
        };
        match outcome {
            Ok(DomainDecision::Accepted { operation_id }) => Ok(Decided {
                operation_id: Some(operation_id.clone()),
                decision: DomainDecision::Accepted { operation_id },
            }),
            other => decided(tx, other),
        }
    }
}
