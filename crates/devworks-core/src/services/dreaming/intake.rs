use super::{CONSUMER, DreamingService, active, authorize, invalid, queries, rev};
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::AdmittedCommand;
use crate::operations::dreaming::RunInput;
use crate::ports::storage::{
    DreamCheckpoint, DreamCutoffs, DreamPhase, DreamProgress, DreamRunRow, DreamStream,
    DreamTrigger, DreamingRead, KnowledgeScope, NewEvent, NewOperation, NewQueueEntry,
    ProjectFilter, ProjectPosition, StorageError, WriteTransaction,
};
use crate::ports::{AuthorityFence, Decided, IdKind};
use crate::protocol::data::DataScope;
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::{
    ActorId, EventId, Id, OperationId, OperationState, RevisionDimension, RootId, StoreId,
};
use crate::services::{approvals, conventions::control, effect_job};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Failure;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SweepDelivery {
    pub run: Id,
    pub scope: DataScope,
    pub actor: ActorId,
    pub actor_kind: String,
    pub store_id: StoreId,
    pub restore_epoch: i64,
    pub source_context: Value,
    pub fences: Vec<AuthorityFence>,
    pub control_roots: Vec<(RootId, i64)>,
    pub assignment_generation: Option<i64>,
}

impl SweepDelivery {
    pub fn restore(
        &self,
        tx: &mut dyn WriteTransaction,
        operation: &OperationId,
        needs_current_settings: bool,
    ) -> Result<InvocationContext, StorageError> {
        let (_, context) = effect_job::restore(
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
            operation,
        )?;
        let scope = KnowledgeScope::from(&self.scope);
        authorize(&context, &scope)?;
        active(tx.projects(), &scope)?;
        let repo = tx.dreaming()?;
        let row = repo
            .dream_run(&self.run)?
            .ok_or_else(|| StorageError::LeaseLost {
                kind: "dreaming_run",
                id: self.run.as_str().into(),
            })?;
        if row.scope != scope
            || row.operation_id.as_ref() != Some(operation)
            || !row.phase.owns_scope()
        {
            return Err(StorageError::ScopeDenied);
        }
        if needs_current_settings && !settings_current(repo, &row)? {
            return Err(StorageError::ScopeDenied);
        }
        Ok(context)
    }
}

pub(super) fn settings_current(
    repo: &dyn DreamingRead,
    row: &DreamRunRow,
) -> Result<bool, StorageError> {
    let settings = repo.dream_scope(&row.scope)?;
    Ok(settings.revision == row.settings_revision && settings.policy.enabled)
}

fn resume_checkpoint(mut prior: DreamCheckpoint) -> DreamCheckpoint {
    // A new run has a fresh budget. A prepared request retains its immutable frame but must
    // not reuse the retired delivery's request identity and authority.
    if prior
        .request
        .as_ref()
        .is_some_and(|request| request.stage == crate::ports::storage::DreamRequestStage::Prepared)
    {
        prior.continuation = prior.request.take().map(|request| request.frame);
    }
    DreamCheckpoint {
        cursor_key: prior.cursor_key,
        request: prior.request,
        continuation: prior.continuation,
        ..Default::default()
    }
}

impl DreamingService {
    pub(super) fn run(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        input: &RunInput,
    ) -> Result<Decided, Failure> {
        let scope = KnowledgeScope::from(&input.scope);
        authorize(ctx, &scope)?;
        active(tx.projects(), &scope)?;
        let state = tx.dreaming()?.dream_scope(&scope)?;
        let scheduled = if let Some(id) = ctx.job_run_id() {
            let run = tx.jobs().job_run(id)?.ok_or(StorageError::ScopeDenied)?;
            if ctx.principal_kind() != PrincipalKind::Host
                || run.project_id.as_ref() != scope.project()
                || run.status != "Running"
                || run.restore_epoch != tx.store().restore_epoch
                || state
                    .policy
                    .schedule_id
                    .as_ref()
                    .is_some_and(|job| job != &run.job_id)
                || input.resume_run_id.is_some()
                || input.cancel_run_id.is_some()
            {
                return Err(StorageError::ScopeDenied.into());
            }
            if run.job_id.as_str() == super::NIGHTLY_JOB_ID && scope == KnowledgeScope::Workspace {
                return self.fan_out(tx, ctx, command, input);
            }
            true
        } else {
            ctx.principal_kind() == PrincipalKind::Schedule
        };
        self.run_scope(tx, ctx, command, input, scheduled)
    }

    fn run_scope(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        input: &RunInput,
        scheduled: bool,
    ) -> Result<Decided, Failure> {
        let scope = KnowledgeScope::from(&input.scope);
        let now = tx.now_ms();
        let state = tx.dreaming()?.dream_scope(&scope)?;
        if !scheduled && state.revision != input.expected_settings_revision.get() {
            return Err(StorageError::conflict(
                RevisionDimension::Configuration,
                "dreaming_settings",
                Id::new(scope.key()).unwrap_or_else(|_| Id::new("dreaming").expect("literal id")),
                input.expected_settings_revision.clone(),
                rev(state.revision),
            )
            .into());
        }
        if let Some(requested) = &input.cancel_run_id {
            return cancel(tx, ctx, &scope, requested);
        }
        let cutoffs = tx.dreaming()?.dream_cutoffs(&scope)?;
        let prior = state
            .completed
            .as_ref()
            .map_or_else(DreamCutoffs::default, |(c, _)| c.clone());
        let parent = if let Some(id) = &input.resume_run_id {
            let run = visible_run(tx, &scope, id)?;
            if tx.dreaming()?.dream_continuation_used(id)? {
                return Err(invalid("dreaming_resume_consumed").into());
            }
            Some(run)
        } else {
            tx.dreaming()?
                .dream_resumable_run(&scope)?
                .filter(|run| run.cutoffs.does_not_precede(&prior))
        };
        if let Some(parent) = &parent
            && (!parent.phase.resumable()
                || parent.checkpoint.is_none()
                || !parent.cutoffs.does_not_precede(&prior))
        {
            return Err(invalid("dreaming_resume").into());
        }
        let busy = tx.dreaming()?.dream_live_run(&scope)?.is_some();
        let too_soon = state.completed.as_ref().is_some_and(|(_, at)| {
            now.saturating_sub(*at) < state.policy.minimum_interval_ms as i64
        });
        let phase = if !state.policy.enabled {
            DreamPhase::Disabled
        } else if busy {
            DreamPhase::Busy
        } else if parent.is_some() {
            DreamPhase::Queued
        } else if too_soon {
            DreamPhase::TooSoon
        } else if !has_signal(
            tx.dreaming()?,
            &scope,
            &prior,
            &cutoffs,
            state.completed.is_some(),
        )? {
            DreamPhase::NoSignal
        } else {
            DreamPhase::Queued
        };
        let trigger = if parent.is_some() {
            DreamTrigger::Resume
        } else if scheduled {
            DreamTrigger::Scheduled
        } else {
            DreamTrigger::Manual
        };
        let operation_id =
            (phase == DreamPhase::Queued).then(|| OperationId(tx.new_id(IdKind::Operation)));
        let frozen = parent.as_ref().map_or_else(
            || DreamCutoffs {
                sweep_started_at_ms: now,
                ..cutoffs.clone()
            },
            |parent| DreamCutoffs {
                sweep_started_at_ms: parent.sweep_started_at_ms(),
                base_instruction_revision: cutoffs.base_instruction_revision,
                project_instruction_revision: cutoffs.project_instruction_revision,
                ..parent.cutoffs.clone()
            },
        );
        let mut row = DreamRunRow {
            id: tx.new_id(IdKind::JobRun),
            scope,
            continued_from: parent.as_ref().map(|parent| parent.id.clone()),
            operation_id: operation_id.clone(),
            initiator: ctx.principal().clone(),
            trigger,
            revision: 1,
            generation: 1,
            phase,
            settings_revision: state.revision,
            policy: state.policy,
            cutoffs: frozen,
            started_at_ms: now,
            ended_at_ms: phase.ended().then_some(now),
            checkpoint: parent
                .and_then(|parent| parent.checkpoint)
                .filter(|_| operation_id.is_some())
                .map(resume_checkpoint),
            report: phase.ended().then(|| json!({"reason":phase})),
        };
        if let Some(operation) = &operation_id {
            let controls = control::capture(tx, ctx)?;
            let delivery = SweepDelivery {
                run: row.id.clone(),
                scope: (&row.scope).into(),
                actor: ctx.principal().clone(),
                actor_kind: ctx.principal_kind().as_str().into(),
                store_id: ctx.store_id().clone(),
                restore_epoch: ctx.restore_epoch().get(),
                source_context: approvals::save_source_context(ctx),
                fences: command.authorization().fences().to_vec(),
                control_roots: controls.roots,
                assignment_generation: controls.assignment_generation,
            };
            let payload =
                serde_json::to_value(delivery).map_err(|_| invalid("dreaming_delivery"))?;
            tx.operations().record_intent(NewOperation {
                id: operation.clone(),
                root_id: ctx.root_id().cloned(),
                assignment_id: ctx.assignment_id().cloned(),
                capability: "dreaming.run".into(),
                schema_version: i64::from(command.schema_revision().0),
                args_digest: command.fingerprint().as_str().to_owned(),
                payload: payload.clone(),
            })?;
            let checkpoint = row.checkpoint.get_or_insert_with(DreamCheckpoint::default);
            if checkpoint.cursor_key.is_none() {
                checkpoint.cursor_key = Some(super::windows::cursor_key()?);
            }
            tx.dreaming()?.insert_dream_run(&row)?;
            let event_id = EventId(tx.new_id(IdKind::Event));
            tx.history().append(NewEvent {
                id: event_id.clone(),
                actor: row.initiator.clone(),
                project_id: row.scope.project().cloned(),
                root_id: ctx.root_id().cloned(),
                operation_id: Some(operation.clone()),
                aggregate_kind: "dreaming_run".into(),
                aggregate_id: row.id.as_str().into(),
                aggregate_revision: 1,
                kind: "DreamingQueued".into(),
                payload: json!({"runId":row.id,"trigger":row.trigger}),
            })?;
            tx.queue().enqueue(NewQueueEntry {
                id: row.id,
                consumer: CONSUMER.into(),
                event_id,
                root_id: ctx.root_id().cloned(),
                operation_id: Some(operation.clone()),
                partition: None,
                priority: 0,
                ready_at_ms: now,
                payload,
            })?;
            return Ok(Decided {
                operation_id: Some(operation.clone()),
                decision: DomainDecision::Accepted {
                    operation_id: operation.clone(),
                },
            });
        }
        // A disabled/busy resume is a new observation, not a consumption of the saved parent.
        row.continued_from = None;
        if row.trigger == DreamTrigger::Resume {
            row.trigger = DreamTrigger::Manual;
        }
        tx.dreaming()?.insert_dream_run(&row)?;
        Ok(Decided {
            operation_id: None,
            decision: DomainDecision::Applied {
                result: OperationResult::DreamingRun {
                    value: queries::render_run(row)?,
                },
            },
        })
    }

    /// The recurring workspace job records submission separately from the consolidation results.
    /// Its one gateway receipt covers the scope selection and the project deliveries atomically.
    fn fan_out(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        input: &RunInput,
    ) -> Result<Decided, Failure> {
        let scope = KnowledgeScope::Workspace;
        let state = tx.dreaming()?.dream_scope(&scope)?;
        if !state.policy.enabled {
            return self.run_scope(tx, ctx, command, input, true);
        }
        let filter = ProjectFilter {
            only: None,
            lifecycles: vec!["Active".into()],
        };
        let now = tx.now_ms();
        let mut project_position = None;
        let mut queued = Vec::new();
        // The job owns submission. Each queued operation takes its own subsequent inference
        // custody and ledger lineage, and cannot keep the scheduling run artificially open.
        let child_source = ctx.clone().without_job_run();
        loop {
            let projects = tx
                .projects()
                .projects(&filter, project_position.as_ref(), 100)?;
            let Some(last) = projects.last() else { break };
            project_position = Some(ProjectPosition {
                created_at_ms: last.created_at_ms,
                id: last.id.clone(),
            });
            for project in projects {
                let scope = KnowledgeScope::Project(project.id.clone());
                let state = tx.dreaming()?.dream_scope(&scope)?;
                if !state.policy.enabled
                    || state.policy.schedule_id.is_some()
                    || tx.dreaming()?.dream_live_run(&scope)?.is_some()
                {
                    continue;
                }
                let (prior, completed_at) = state
                    .completed
                    .map_or_else(|| (DreamCutoffs::default(), None), |(c, at)| (c, Some(at)));
                if completed_at.is_some_and(|at| {
                    now.saturating_sub(at) < state.policy.minimum_interval_ms as i64
                }) {
                    continue;
                }
                let current = tx.dreaming()?.dream_cutoffs(&scope)?;
                let saved = tx
                    .dreaming()?
                    .dream_resumable_run(&scope)?
                    .is_some_and(|run| run.cutoffs.does_not_precede(&prior));
                if !saved
                    && !has_signal(
                        tx.dreaming()?,
                        &scope,
                        &prior,
                        &current,
                        completed_at.is_some(),
                    )?
                {
                    continue;
                }
                let scoped = RunInput {
                    scope: DataScope::Project {
                        project_id: project.id.clone(),
                    },
                    expected_settings_revision: rev(state.revision),
                    resume_run_id: None,
                    cancel_run_id: None,
                };
                let receipt = self.run_scope(tx, &child_source, command, &scoped, true)?;
                if let Some(operation) = receipt.operation_id {
                    queued.push(json!({"projectId":project.id,"operationId":operation}));
                }
            }
        }
        let cutoffs = tx.dreaming()?.dream_cutoffs(&scope)?;
        let id = tx.new_id(IdKind::JobRun);
        let row = DreamRunRow {
            id,
            scope,
            continued_from: None,
            operation_id: None,
            initiator: ctx.principal().clone(),
            trigger: DreamTrigger::Scheduled,
            revision: 1,
            generation: 1,
            phase: DreamPhase::ScheduledFanout,
            settings_revision: state.revision,
            policy: state.policy,
            cutoffs,
            started_at_ms: now,
            ended_at_ms: Some(now),
            checkpoint: None,
            report: Some(
                json!({"reason":"ScheduledProjectsQueued","scheduleRunId":ctx.job_run_id(),"projects":queued}),
            ),
        };
        tx.dreaming()?.insert_dream_run(&row)?;
        Ok(Decided {
            operation_id: None,
            decision: DomainDecision::Applied {
                result: OperationResult::DreamingRun {
                    value: queries::render_run(row)?,
                },
            },
        })
    }
}

fn has_signal(
    repo: &dyn DreamingRead,
    scope: &KnowledgeScope,
    prior: &DreamCutoffs,
    current: &DreamCutoffs,
    has_completed: bool,
) -> Result<bool, StorageError> {
    let knowledge_changed =
        |scope: &KnowledgeScope, previous, latest| -> Result<bool, StorageError> {
            if latest <= previous {
                return Ok(false);
            }
            Ok(!repo
                .dream_signals(scope, DreamStream::Knowledge, previous, latest, 1)?
                .is_empty())
        };
    if current.reviews > prior.reviews
        || current.sources > prior.sources
        || knowledge_changed(scope, prior.knowledge, current.knowledge)?
    {
        return Ok(true);
    }
    let useful_scope = current.knowledge != 0 || current.reviews != 0 || current.sources != 0;
    if has_completed || useful_scope {
        Ok(current.workspace_sources > prior.workspace_sources
            || current.base_instruction_revision > prior.base_instruction_revision
            || current.project_instruction_revision > prior.project_instruction_revision
            || knowledge_changed(
                &KnowledgeScope::Workspace,
                prior.workspace_knowledge,
                current.workspace_knowledge,
            )?)
    } else {
        Ok(false)
    }
}

fn visible_run(
    tx: &mut dyn WriteTransaction,
    scope: &KnowledgeScope,
    run: &Id,
) -> Result<DreamRunRow, Failure> {
    tx.dreaming()?
        .dream_run(run)?
        .filter(|row| &row.scope == scope)
        .ok_or_else(|| super::super::kernel::not_found("dreaming_run", run).into())
}

fn cancel(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    scope: &KnowledgeScope,
    requested: &Id,
) -> Result<Decided, Failure> {
    let mut run = visible_run(tx, scope, requested)?;
    if run.initiator != *ctx.principal() {
        super::human_or_host(ctx)?;
    }
    let mut changed = false;
    if let Some(operation) = &run.operation_id
        && run.phase.owns_scope()
    {
        tx.queue().cancel(&run.id)?;
        changed = true;
        if run.phase == DreamPhase::Queued {
            if let Some(current) = tx.operations().operation(operation)?
                && current.state == OperationState::IntentRecorded
            {
                tx.operations().transition(
                    operation,
                    current.revision,
                    None,
                    OperationState::CancelledBeforeDispatch,
                    None,
                )?;
            }
            run = tx.dreaming()?.advance_dream_run(
                &run.id,
                run.revision,
                run.generation,
                &DreamProgress {
                    phase: DreamPhase::Cancelled,
                    checkpoint: None,
                    report: Some(json!({"reason":"CancelledBeforeDispatch"})),
                },
            )?;
        }
    }
    let result = OperationResult::DreamingRun {
        value: queries::render_run(run)?,
    };
    Ok(Decided {
        operation_id: None,
        decision: if changed {
            DomainDecision::Applied { result }
        } else {
            DomainDecision::NoChange { result }
        },
    })
}
