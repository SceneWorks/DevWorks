//! Native admission, durable analysis receipts and exact-scope resume.
use super::selection::{self, Selection};
use crate::context::InvocationContext;
use crate::gateway::AdmittedCommand;
use crate::operations::{OperationInput, analysis as ops};
use crate::ports::storage::*;
use crate::ports::{CommandHandler, Decided, HandlerFailure, IdKind, Preparation, Prepared};
use crate::protocol::data::{Coverage, CoverageState, DataScope, ViewSelection};
use crate::protocol::records::{
    AnalysisAvailability, AnalysisLinkMode, AnalysisObservation, AnalysisRecord,
    AnalysisRefreshMode,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText};
use crate::protocol::{
    DomainErrorCode, EventId, Id, OperationId, OperationState, RevisionDimension,
};
use crate::services::kernel::{Failure, Refusal, decided, rev};
use serde_json::json;

pub const CONSUMER: &str = "RepositoryAnalysis";
pub(crate) const AGGREGATE: &str = "analysis_run";
const MAX_ACTIVE_MODEL_RUNS: usize = 32;

pub(super) struct Commands;

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
        let Some(operation_id) = &entry.operation_id else {
            return Ok(());
        };
        let operation = tx
            .operations()
            .operation(operation_id)?
            .ok_or(StorageError::Busy)?;
        let work = super::work::Work::from_operation(&operation).map_err(Failure::into_storage)?;
        let run = tx
            .analysis()
            .analysis_run(&work.run_id)?
            .ok_or(StorageError::Busy)?;
        if run.active_operation == *operation_id
            && operation.state == OperationState::CancelledBeforeDispatch
        {
            cancelled(tx, run, "CancelledBeforeDispatch")?;
        }
        Ok(())
    }
}

pub(super) fn cancelled(
    tx: &mut dyn WriteTransaction,
    mut run: StoredAnalysisRun,
    reason: &str,
) -> Result<(), StorageError> {
    if run.terminal() {
        return Ok(());
    }
    let revision = run.revision;
    run.revision += 1;
    run.state = OperationState::CancelledBeforeDispatch;
    run.updated_at_ms = tx.now_ms();
    for phase in &mut run.phases {
        if phase.state != OperationState::ObservedComplete {
            phase.state = OperationState::CancelledBeforeDispatch;
            phase.diagnostic = Some(reason.into());
            phase.finished_at_ms = Some(run.updated_at_ms);
        }
    }
    if let Some(pin) = tx.graph().graph_pin(&run.pin_id)? {
        tx.graph()
            .retire_graph_pin(&pin.id, &pin.owner_namespace, pin.revision)?;
    }
    tx.analysis().update_analysis_run(&run, revision)
}

struct Request<'a> {
    kind: AnalysisCommand,
    scope: &'a DataScope,
    views: &'a ViewSelection,
    expected_revision: i64,
    nodes: &'a [Id],
    limit: u16,
    resume: Option<&'a Id>,
    link: Option<AnalysisLinkMode>,
    refresh: Option<AnalysisRefreshMode>,
    all_source: bool,
}

impl<'a> Request<'a> {
    fn from(input: &'a OperationInput) -> Result<Self, Failure> {
        macro_rules! common {
            ($input:ident, $kind:ident) => {
                Request {
                    kind: AnalysisCommand::$kind,
                    scope: &$input.scope,
                    views: &$input.views,
                    expected_revision: $input.expected_source_revision.get(),
                    nodes: &$input.node_ids.0,
                    limit: $input
                        .batch_limit
                        .get()
                        .try_into()
                        .map_err(|_| invalid("batch_limit"))?,
                    resume: $input.resume_run_id.as_ref(),
                    link: None,
                    refresh: None,
                    all_source: false,
                }
            };
        }
        let OperationInput::Analysis(input) = input else {
            return Err(unsupported());
        };
        let request = match input {
            ops::Input::LinkAndDetect(i) => Self {
                link: Some(
                    i.link_mode
                        .clone()
                        .unwrap_or(AnalysisLinkMode::LinksAndCommunities),
                ),
                ..common!(i, LinkAndDetect)
            },
            ops::Input::CommunityAnalysis(i) => common!(i, Community),
            ops::Input::Reanalyze(i) => Self {
                refresh: Some(
                    i.refresh_mode
                        .clone()
                        .unwrap_or(AnalysisRefreshMode::WithSynthesis),
                ),
                all_source: i.include_all_source.unwrap_or(false),
                ..common!(i, Reanalyze)
            },
            ops::Input::ProcessBatch(i) => common!(i, ProcessBatch),
            ops::Input::Synthesize(i) => common!(i, Synthesize),
            _ => return Err(unsupported()),
        };
        if request.all_source && matches!(request.refresh, Some(AnalysisRefreshMode::VitalsOnly)) {
            return Err(invalid("include_all_source"));
        }
        Ok(request)
    }

    fn phases(&self) -> &'static [AnalysisPhase] {
        use AnalysisPhase as P;
        match self.kind {
            AnalysisCommand::LinkAndDetect
                if matches!(self.link, Some(AnalysisLinkMode::LinksOnly)) =>
            {
                &[P::Links]
            }
            AnalysisCommand::LinkAndDetect | AnalysisCommand::Community => {
                &[P::Links, P::Communities]
            }
            AnalysisCommand::Reanalyze => match self.refresh {
                Some(AnalysisRefreshMode::VitalsOnly) => &[P::Links, P::Vitals, P::Security],
                Some(AnalysisRefreshMode::WithProjectAnalysis) => {
                    &[P::Links, P::Vitals, P::Security, P::Projects]
                }
                _ => &[P::Links, P::Vitals, P::Security, P::Projects, P::Synthesis],
            },
            AnalysisCommand::ProcessBatch => &[P::SettleBatch],
            AnalysisCommand::Synthesize => &[P::Synthesis],
            AnalysisCommand::IndexPublished => {
                unreachable!("host admission selects its own phases")
            }
        }
    }

    fn options_digest(&self) -> String {
        selection::digest(&json!({
            "kind": self.kind,
            "nodeIds": self.nodes,
            "batchLimit": self.limit,
            "link": self.link,
            "refresh": self.refresh,
            "includeAllSource": self.all_source,
        }))
    }
}

struct PreparedRequest {
    selection: Selection,
}

impl CommandHandler for Commands {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        let prepared = env.read(|read| {
            let result = Request::from(command.input()).and_then(|request| {
                let selection = Selection::resolve(
                    read,
                    ctx,
                    request.scope,
                    request.views,
                    read.stamp().now_ms,
                )?;
                if selection.views.is_empty() {
                    return Err(selection::unavailable());
                }
                if request.kind == AnalysisCommand::Synthesize
                    && (selection.views.len() != 1 || !selection.unavailable.is_empty())
                {
                    return Err(invalid("synthesis_repository"));
                }
                if selection.views.len() + selection.unavailable.len()
                    > selection::MAX_COMMAND_REPOSITORIES
                {
                    return Err(invalid("analysis_repository_limit"));
                }
                let scope = selection.graph_scope();
                let mut seen = std::collections::BTreeSet::new();
                for id in request.nodes {
                    if !seen.insert(id.as_str()) {
                        return Err(invalid("duplicate_analysis_node"));
                    }
                    let page = read.graph().graph_nodes(
                        &scope,
                        &GraphNodeFilter {
                            node_id: Some(id.as_str().into()),
                            ..Default::default()
                        },
                        None,
                        2,
                    )?;
                    if page.items.len() != 1 || page.has_more {
                        return Err(selection::unavailable());
                    }
                }
                Ok(PreparedRequest { selection })
            });
            Ok(result)
        })?;
        Ok(Some(Prepared::new(prepared)))
    }

    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let outcome = admit(tx, ctx, command);
        if let Ok(DomainDecision::Accepted { operation_id }) = &outcome {
            return Ok(Decided {
                operation_id: Some(operation_id.clone()),
                decision: DomainDecision::Accepted {
                    operation_id: operation_id.clone(),
                },
            });
        }
        decided(tx, outcome)
    }
}

fn admit(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    command: &AdmittedCommand,
) -> Result<DomainDecision, Failure> {
    let request = Request::from(command.input())?;
    let prepared = command
        .prepared::<Result<PreparedRequest, Failure>>()
        .ok_or_else(selection::unavailable)?
        .as_ref()
        .map_err(Clone::clone)?;
    let selected = &prepared.selection;
    let revision = selection::configuration_revision(tx.indexing(), &selected.scope)?;
    if revision != request.expected_revision || revision != selected.configuration_revision {
        return Err(StorageError::conflict(
            RevisionDimension::Configuration,
            "analysis_source_configuration",
            Id::new(scope_key(&selected.scope)).map_err(|_| invalid("analysis_scope"))?,
            rev(request.expected_revision),
            rev(revision),
        )
        .into());
    }
    selected.validate_in(tx, ctx)?;

    if let Some(id) = request.resume
        && request.kind != AnalysisCommand::ProcessBatch
    {
        return resume(tx, ctx, command, &request, selected, id);
    }
    let previous = tx
        .analysis()
        .analysis_runs(&selected.binding_key, false, None, 128)?;
    if request.kind == AnalysisCommand::ProcessBatch {
        let mut has_pending = false;
        for run in previous.iter().filter(|run| {
            run.intent.command != AnalysisCommand::ProcessBatch
                && request.resume.is_none_or(|id| run.id == *id)
                && !run.terminal()
        }) {
            if tx
                .operations()
                .operation(&run.active_operation)?
                .is_some_and(|operation| operation.state == OperationState::OutcomeUnknown)
            {
                has_pending = true;
                break;
            }
        }
        if !has_pending {
            if request
                .resume
                .is_some_and(|id| !previous.iter().any(|run| run.id == *id))
            {
                return Err(selection::unavailable());
            }
            return Ok(DomainDecision::NoChange {
                result: OperationResult::AnalysisProcessBatch {
                    value: no_pending(selected, request.views.clone())?,
                },
            });
        }
    }

    if ctx.principal_kind() == crate::context::PrincipalKind::Host
        && ctx.principal() == &tx.store().host_actor
        && matches!(
            request.kind,
            AnalysisCommand::Community | AnalysisCommand::LinkAndDetect
        )
        && let Some(prior) = previous.iter().find(|run| {
            run.intent.node_ids == request.nodes
                && !matches!(
                    run.state,
                    OperationState::Failed | OperationState::CancelledBeforeDispatch
                )
                && request.phases().iter().all(|required| {
                    run.phases.iter().any(|phase| {
                        phase.phase == *required
                            && !matches!(
                                phase.state,
                                OperationState::Failed | OperationState::CancelledBeforeDispatch
                            )
                    })
                })
        })
    {
        let phase = request
            .phases()
            .last()
            .and_then(|wanted| prior.phases.iter().find(|phase| phase.phase == *wanted));
        let observation = super::observation::Observation {
            selected,
            run: Some(prior),
            phase,
            gaps: &[],
        };
        let report = phase.and_then(|phase| phase.report.clone());
        let value = observation.record(request.kind.key(), report, observation.wire())?;
        return Ok(DomainDecision::NoChange {
            result: super::service::operation_result(request.kind, value),
        });
    }

    let prerequisite = if request.kind == AnalysisCommand::Synthesize {
        Some(
            previous
                .iter()
                .find(|run| {
                    run.owner.namespace == super::super::lease::owner(ctx)
                        && run.owner.restore_epoch == ctx.restore_epoch().get()
                        && run.intent.node_ids == request.nodes
                        && run.phases.iter().any(|phase| {
                            phase.phase == AnalysisPhase::Projects
                                && phase.state == OperationState::ObservedComplete
                                && phase.report.is_some()
                        })
                })
                .ok_or_else(|| {
                    Refusal::new(
                        DomainErrorCode::ViewUnavailable,
                        "analysis_project_results",
                        None,
                    )
                })?
                .id
                .clone(),
        )
    } else {
        request.resume.cloned()
    };
    let phases = request.phases();
    if phases.iter().copied().any(model_phase)
        && tx
            .analysis()
            .active_model_runs(scope_key(&selected.scope))?
            >= MAX_ACTIVE_MODEL_RUNS
    {
        return Err(StorageError::Busy.into());
    }
    let pin = selected.retain_in(tx, ctx)?;
    let operation = OperationId(tx.new_id(IdKind::Operation));
    let id = tx.new_id(IdKind::Operation);
    let now = tx.now_ms();
    let run = StoredAnalysisRun {
        id,
        operation: operation.clone(),
        active_operation: operation.clone(),
        pin_id: pin,
        owner: owner(ctx, command),
        intent: AnalysisIntent {
            command: request.kind,
            scope: request.scope.clone(),
            selection: request.views.clone(),
            views: selected.views.clone(),
            unavailable: selected.unavailable.clone(),
            witnesses: selected.witnesses().to_vec(),
            repository_lineages: selected.repository_lineages.clone(),
            options_digest: request.options_digest(),
            configuration_revision: revision,
            node_ids: request.nodes.to_vec(),
            batch_limit: request.limit,
            link_mode: request.link,
            refresh_mode: request.refresh,
            include_all_source: request.all_source,
            prerequisite_run: prerequisite,
        },
        binding_key: selected.binding_key.clone(),
        lineage_key: selected.lineage_key.clone(),
        state: OperationState::IntentRecorded,
        revision: 1,
        created_at_ms: now,
        updated_at_ms: now,
        phases: phases
            .iter()
            .map(|phase| StoredAnalysisPhase {
                phase: *phase,
                state: OperationState::IntentRecorded,
                report: None,
                operation: None,
                diagnostic: None,
                finished_at_ms: None,
                subjects: Vec::new(),
            })
            .collect(),
    };
    queue(tx, ctx, command, &run)?;
    tx.analysis().insert_analysis_run(&run)?;
    Ok(DomainDecision::Accepted {
        operation_id: operation,
    })
}

fn resume(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    command: &AdmittedCommand,
    request: &Request<'_>,
    selected: &Selection,
    id: &Id,
) -> Result<DomainDecision, Failure> {
    let mut run = tx
        .analysis()
        .analysis_run(id)?
        .ok_or_else(selection::unavailable)?;
    if run.owner.namespace != super::super::lease::owner(ctx)
        || run.owner.restore_epoch != ctx.restore_epoch().get()
        || run.binding_key != selected.binding_key
        || run.intent.options_digest != request.options_digest()
    {
        return Err(selection::unavailable());
    }
    if !run.terminal()
        && let Some(op) = tx.operations().operation(&run.active_operation)?
    {
        match op.state {
            OperationState::CancelledBeforeDispatch => {
                cancelled(tx, run.clone(), "CancelledBeforeDispatch")?;
                run = tx
                    .analysis()
                    .analysis_run(id)?
                    .ok_or_else(selection::unavailable)?;
            }
            OperationState::Failed => {
                let revision = run.revision;
                run.revision += 1;
                run.state = OperationState::Failed;
                run.updated_at_ms = tx.now_ms();
                tx.analysis().update_analysis_run(&run, revision)?;
            }
            _ => {}
        }
    }
    // Existing dispatcher work remains authoritative for in-flight or uncertain effects.
    if !matches!(
        run.state,
        OperationState::Failed | OperationState::CancelledBeforeDispatch
    ) {
        return Ok(DomainDecision::Accepted {
            operation_id: run.active_operation,
        });
    }
    if run
        .phases
        .iter()
        .any(|phase| phase.state == OperationState::OutcomeUnknown)
    {
        return Err(Refusal::new(
            DomainErrorCode::ViewUnavailable,
            "analysis_outcome_unknown",
            None,
        )
        .into());
    }
    if run
        .phases
        .iter()
        .any(|phase| model_phase(phase.phase) && phase.state != OperationState::ObservedComplete)
        && tx
            .analysis()
            .active_model_runs(scope_key(&selected.scope))?
            >= MAX_ACTIVE_MODEL_RUNS
    {
        return Err(StorageError::Busy.into());
    }
    let previous = run.revision;
    run.pin_id = selected.retain_in(tx, ctx)?;
    run.active_operation = OperationId(tx.new_id(IdKind::Operation));
    run.updated_at_ms = tx.now_ms();
    run.revision += 1;
    run.state = OperationState::IntentRecorded;
    for phase in &mut run.phases {
        if phase.state != OperationState::ObservedComplete {
            phase.state = OperationState::IntentRecorded;
            phase.diagnostic = None;
            phase.finished_at_ms = None;
            phase
                .subjects
                .retain(|subject| subject.state == OperationState::ObservedComplete);
        }
    }
    queue(tx, ctx, command, &run)?;
    tx.analysis().update_analysis_run(&run, previous)?;
    Ok(DomainDecision::Accepted {
        operation_id: run.active_operation,
    })
}

fn queue(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    command: &AdmittedCommand,
    run: &StoredAnalysisRun,
) -> Result<(), Failure> {
    let event = EventId(tx.new_id(IdKind::Event));
    let delivery = tx.new_id(IdKind::QueueEntry);
    let active_authority = owner(ctx, command);
    tx.operations().record_intent(NewOperation {
        id: run.active_operation.clone(),
        root_id: run.owner.root.clone(),
        assignment_id: run.owner.assignment.clone(),
        capability: command.descriptor().name.into(),
        schema_version: i64::from(command.schema_revision().0),
        args_digest: command.fingerprint().as_str().to_owned(),
        payload: json!({"runId": run.id, "deliveryId": delivery, "authority": active_authority}),
    })?;
    let project = match &run.intent.scope {
        DataScope::Workspace => None,
        DataScope::Project { project_id } => Some(project_id.clone()),
    };
    tx.history().append(NewEvent {
        id: event.clone(),
        actor: run.owner.actor.clone(),
        project_id: project,
        root_id: run.owner.root.clone(),
        operation_id: Some(run.active_operation.clone()),
        aggregate_kind: AGGREGATE.into(),
        aggregate_id: run.id.as_str().into(),
        aggregate_revision: run.revision,
        kind: "RepositoryAnalysisRequested".into(),
        payload: json!({"runId": run.id, "kind": run.intent.command.key(), "binding": run.binding_key}),
    })?;
    let now = tx.now_ms();
    tx.queue().enqueue(NewQueueEntry {
        id: delivery,
        consumer: CONSUMER.into(),
        event_id: event,
        root_id: run.owner.root.clone(),
        operation_id: Some(run.active_operation.clone()),
        partition: None,
        priority: 0,
        ready_at_ms: now,
        payload: json!({"runId": run.id, "operationId": run.active_operation}),
    })?;
    Ok(())
}

fn owner(ctx: &InvocationContext, command: &AdmittedCommand) -> AnalysisOwner {
    AnalysisOwner {
        actor: ctx.principal().clone(),
        kind: ctx.principal_kind().as_str().into(),
        invocation: ctx.invocation_id().clone(),
        ingress: ctx.ingress_namespace().clone(),
        project: ctx.project_id().cloned(),
        root: ctx.root_id().cloned(),
        assignment: ctx.assignment_id().cloned(),
        session: ctx.session_id().cloned(),
        policy_revision: ctx.policy_revision().get(),
        restore_epoch: ctx.restore_epoch().get(),
        namespace: super::super::lease::owner(ctx),
        activation: ctx.activation().map(|(id, epoch)| (id.clone(), epoch)),
        worktree: ctx.worktree().map(|tree| AnalysisWorktreeOwner {
            id: tree.id().clone(),
            repository: tree.repository_id().clone(),
            work: tree.work_id().clone(),
            ownership_revision: tree.ownership_revision(),
        }),
        fences: command.authorization().fences().to_vec(),
    }
}

fn no_pending(selected: &Selection, views: ViewSelection) -> Result<AnalysisRecord, Failure> {
    // This gives receipt replays a stable identity even when there was no queued run.
    let id =
        Id::new(format!("batch_{}", selected.binding_key)).map_err(|_| selection::unavailable())?;
    let coverage = Coverage {
        state: CoverageState::Unavailable,
        gaps: Vec::new(),
        more_gaps: false,
    };
    Ok(AnalysisRecord {
        id,
        revision: rev(selected.configuration_revision),
        views,
        kind: BoundedText("process_batch".to_owned()),
        state: OperationState::ObservedComplete,
        report: None,
        coverage: coverage.clone(),
        observation: Some(AnalysisObservation {
            availability: AnalysisAvailability::NoData,
            bindings: BoundedList(selected.views.clone()),
            computed_at_ms: None,
            coverage,
            diagnostics: BoundedList(Vec::new()),
            phases: None,
            base_score: None,
            adjusted_score: None,
            trends: None,
            related_work_ids: None,
            top_hotspots: None,
            health: None,
            security: None,
            next_cursor: None,
            has_more: None,
            missing_repositories: None,
            indexed_bindings: None,
            narratives: None,
        }),
    })
}

pub(super) fn scope_key(scope: &DataScope) -> &str {
    match scope {
        DataScope::Workspace => WORKSPACE_SCOPE_KEY,
        DataScope::Project { project_id } => project_id.0.as_str(),
    }
}

pub(super) fn model_phase(phase: AnalysisPhase) -> bool {
    matches!(phase, AnalysisPhase::Projects | AnalysisPhase::Synthesis)
}

fn invalid(field: &'static str) -> Failure {
    Refusal::new(DomainErrorCode::InvalidArgument, field, None).into()
}

fn unsupported() -> Failure {
    Refusal::new(DomainErrorCode::UnsupportedCapability, "analysis", None).into()
}
