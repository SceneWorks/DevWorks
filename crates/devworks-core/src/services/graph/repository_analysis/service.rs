//! One native analysis service for the gateway and its durable consumer.
use super::{
    commands::Commands,
    deterministic::Deterministic,
    modeling::AnalysisModel,
    observation::Observation,
    paid::Paid,
    phase_store::{PhaseEvidence, PhaseStore},
    queries::Queries,
    security::Feed,
    selection,
    work::{Claim, Work},
};
use crate::context::InvocationContext;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::kernel::analysis::dispatch_binding_snapshot;
use crate::ports::storage::*;
use crate::ports::web::{HopTransport, Resolver};
use crate::ports::{
    CommandHandler, Decided, HandlerFailure, IdSource, Preparation, Prepared, QueryAnswer,
    QueryHandler,
};
use crate::protocol::records::AnalysisRecord;
use crate::protocol::results::OperationResult;
use crate::protocol::{DomainFault, OperationState};
use crate::services::{artifacts::ArtifactService, indexing::IndexEnv, kernel::Failure};
use serde_json::Value;
use std::sync::{Arc, Mutex, OnceLock};

pub const COMMANDS: &[&str] = &[
    "analysis.community_analysis",
    "analysis.link_and_detect",
    "analysis.process_batch",
    "analysis.reanalyze",
    "analysis.synthesize",
];
pub const QUERIES: &[&str] = &[
    "analysis.health",
    "analysis.cross_repo_links",
    "analysis.status",
    "analysis.clusters",
    "analysis.cluster_detail",
    "analysis.security",
    "analysis.metrics",
    "analysis.hotspots",
    "analysis.fleet_health",
    "analysis.archival_candidates",
    "analysis.file_hotspot",
];

pub enum Execution {
    Completed(Value),
    Failed(&'static str),
    NotApplied(&'static str),
    Unknown(&'static str),
}

pub struct AnalysisService {
    commands: Commands,
    queries: Queries,
    phases: Arc<PhaseStore>,
    indexing: Arc<OnceLock<Arc<IndexEnv>>>,
    advisory: Option<Feed>,
    model: OnceLock<Arc<dyn AnalysisModel>>,
    retention_cursor: Mutex<Option<AnalysisRunPosition>>,
    history_gate: super::vitals::HistoryGate,
}

impl AnalysisService {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        artifacts: Arc<ArtifactService>,
        ids: Arc<dyn IdSource>,
        network: Option<(Arc<dyn Resolver>, Arc<dyn HopTransport>)>,
    ) -> Self {
        let phases = Arc::new(PhaseStore { store, artifacts });
        let indexing = Arc::new(OnceLock::new());
        Self {
            commands: Commands,
            queries: Queries {
                reports: phases.clone(),
                ids,
                indexing: indexing.clone(),
            },
            phases,
            indexing,
            advisory: network.map(|(resolver, transport)| Feed::new(resolver, transport)),
            model: OnceLock::new(),
            retention_cursor: Mutex::new(None),
            history_gate: super::vitals::HistoryGate::default(),
        }
    }

    pub fn install_indexing(&self, env: Arc<IndexEnv>) -> bool {
        self.indexing.set(env).is_ok()
    }

    pub fn install_model(&self, model: Arc<dyn AnalysisModel>) -> bool {
        self.model.set(model).is_ok()
    }

    pub fn execute(&self, operation: &OperationRecord, cancellation: &Cancellation) -> Execution {
        let result = self.run(operation, cancellation);
        match result {
            Ok(execution) => execution,
            Err(error) => self.failure(operation, error),
        }
    }

    fn run(
        &self,
        operation: &OperationRecord,
        cancellation: &Cancellation,
    ) -> Result<Execution, Failure> {
        let options = ReadOptions {
            cancellation: Some(cancellation.clone()),
            ..Default::default()
        };
        let claim = Arc::new(read_snapshot(
            self.phases.store.as_ref(),
            &options,
            |read| Ok(Claim::load(read, operation)),
        )??);
        let run = read_snapshot(self.phases.store.as_ref(), &options, |read| {
            Ok(claim.run(read))
        })??
        .0;
        let env = self.indexing.get().ok_or_else(selection::unavailable)?;
        let deterministic = Deterministic {
            phases: &self.phases,
            index: env,
            advisory: self.advisory.as_ref(),
            history_gate: &self.history_gate,
            cancellation,
        };

        for phase in run.phases.iter().map(|record| record.phase) {
            if cancellation.is_cancelled() {
                return Err(StorageError::Cancelled.into());
            }
            let next = match phase {
                AnalysisPhase::Links
                | AnalysisPhase::Communities
                | AnalysisPhase::Vitals
                | AnalysisPhase::Security => deterministic.phase(&claim, phase),
                AnalysisPhase::Projects | AnalysisPhase::Synthesis => {
                    if let Some(model) = self.model.get() {
                        Paid {
                            phases: &self.phases,
                            model: model.as_ref(),
                            cancellation,
                        }
                        .phase(&claim, phase)
                    } else {
                        self.phase_failed(&claim, phase, "AnalysisModelUnavailable")
                    }
                }
                AnalysisPhase::SettleBatch => self.settle_batch(&claim),
            }?;
            if next.terminal() || next.state == OperationState::OutcomeUnknown {
                return self.outcome(&claim.work, &claim.context, next);
            }
        }
        Err(StorageError::InvalidRecord("analysis reached no terminal phase".into()).into())
    }

    fn phase_failed(
        &self,
        claim: &Arc<Claim>,
        phase: AnalysisPhase,
        code: &'static str,
    ) -> Result<StoredAnalysisRun, Failure> {
        let run = self.phases.begin(claim, phase, None)?;
        self.phases.settle(
            claim,
            &run,
            phase,
            PhaseEvidence {
                state: OperationState::Failed,
                attempt: None,
                diagnostic: Some(code),
            },
            None,
            Vec::new(),
        )
    }

    pub(super) fn outcome(
        &self,
        work: &Work,
        ctx: &InvocationContext,
        run: StoredAnalysisRun,
    ) -> Result<Execution, Failure> {
        match run.state {
            OperationState::ObservedComplete => {
                let report = run
                    .phases
                    .iter()
                    .rev()
                    .find_map(|phase| phase.report.clone());
                let result = read_snapshot(
                    self.phases.store.as_ref(),
                    &ReadOptions::default(),
                    |read| {
                        Ok((|| {
                            work.authorize(read, ctx)?;
                            selection::Selection::check_saved(
                                read,
                                ctx,
                                &run.intent.scope,
                                &run,
                                read.stamp().now_ms,
                            )?;
                            let selected = selection::Selection::resolve(
                                read,
                                ctx,
                                &run.intent.scope,
                                &crate::protocol::data::ViewSelection::Exact {
                                    view_ids: run
                                        .intent
                                        .views
                                        .iter()
                                        .map(|binding| binding.view_id.clone())
                                        .collect(),
                                },
                                read.stamp().now_ms,
                            )?;
                            let observation = Observation {
                                selected: &selected,
                                run: Some(&run),
                                phase: None,
                                gaps: &[],
                            };
                            let value = observation.record(
                                run.intent.command.key(),
                                report,
                                observation.wire(),
                            )?;
                            completion(run.intent.command, value)
                        })())
                    },
                )??;
                Ok(Execution::Completed(result))
            }
            OperationState::OutcomeUnknown => Ok(Execution::Unknown(
                "a recorded analysis effect has no terminal observation",
            )),
            OperationState::Failed => Ok(Execution::Failed("one or more analysis phases failed")),
            OperationState::CancelledBeforeDispatch => {
                Ok(Execution::Failed("analysis was cancelled"))
            }
            _ => Ok(Execution::NotApplied("analysis has pending phases")),
        }
    }

    fn failure(&self, operation: &OperationRecord, error: Failure) -> Execution {
        let uncertain = Work::from_operation(operation)
            .ok()
            .and_then(|work| {
                read_snapshot(
                    self.phases.store.as_ref(),
                    &ReadOptions::default(),
                    |read| read.analysis().analysis_run(&work.run_id),
                )
                .ok()
                .flatten()
            })
            .is_none_or(|run| {
                run.phases.iter().any(|phase| {
                    phase.subjects.iter().any(|subject| {
                        matches!(
                            subject.state,
                            OperationState::Dispatching | OperationState::OutcomeUnknown
                        )
                    })
                })
            });
        if uncertain {
            return Execution::Unknown("a recorded provider request requires observation");
        }
        match error {
            Failure::Storage(StorageError::Busy | StorageError::CommitUnknown) => {
                Execution::NotApplied("analysis can restart at its durable checkpoint")
            }
            Failure::Storage(StorageError::Cancelled) => {
                Execution::NotApplied("analysis stopped at its durable checkpoint")
            }
            _ => Execution::Failed("analysis source, report or owner became unavailable"),
        }
    }

    pub fn completion_current(
        &self,
        tx: &mut dyn WriteTransaction,
        operation: &OperationRecord,
    ) -> Result<bool, StorageError> {
        let work = Work::from_operation(operation).map_err(Failure::into_storage)?;
        let ctx = work
            .context(&tx.store().store_id)
            .map_err(Failure::into_storage)?;
        let authority = crate::ports::Authorization::direct().fenced_by(work.authority.fences);
        let Some(run) = tx.analysis().analysis_run(&work.run_id)? else {
            return Ok(false);
        };
        if run.state != OperationState::ObservedComplete
            || run.active_operation != operation.id
            || !crate::services::fences::authorization_holds_in(tx, &ctx, &authority)?
        {
            return Ok(false);
        }
        if let crate::protocol::data::DataScope::Project { project_id } = &run.intent.scope
            && crate::services::projects::visible_project(tx.projects(), &ctx, project_id).is_err()
        {
            return Ok(false);
        }
        for binding in &run.intent.views {
            if crate::services::projects::visible_repository(
                tx.projects(),
                &ctx,
                &binding.repository_id,
            )
            .is_err()
            {
                return Ok(false);
            }
            if let crate::protocol::data::DataScope::Project { project_id } = &run.intent.scope
                && !tx
                    .projects()
                    .members(project_id)?
                    .contains(&binding.repository_id)
            {
                return Ok(false);
            }
        }
        let now = tx.now_ms();
        if run
            .intent
            .witnesses
            .iter()
            .any(|proof| proof.expires_at_ms.is_some_and(|expiry| expiry <= now))
        {
            return Ok(false);
        }
        for report in run.phases.iter().filter_map(|phase| phase.report.as_ref()) {
            if !crate::services::artifacts::exact_reference_readable_in(tx, &ctx, report)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn settle_batch(&self, claim: &Arc<Claim>) -> Result<StoredAnalysisRun, Failure> {
        super::recovery::process_batch(self, &self.phases, claim)
    }

    pub fn maintain(&self, stopped: &dyn Fn() -> bool) -> Result<(), StorageError> {
        let mut cursor = self.retention_cursor.lock().map_err(|_| {
            StorageError::unavailable(
                Unavailable::HandlerPanicked,
                "analysis retention cursor unavailable",
            )
        })?;
        *cursor = super::retention::run(&self.phases, cursor.as_ref(), stopped)?;
        Ok(())
    }

    pub fn index_published(
        &self,
        entry: &QueueEntry,
        event: &EventRecord,
        cancellation: &Cancellation,
    ) -> Result<(), StorageError> {
        super::auto::index_published(self.phases.store.as_ref(), entry, event, cancellation)
            .map_err(Failure::into_storage)
    }

    pub fn observe(&self, operation: &OperationRecord) -> Execution {
        super::recovery::observe(
            self,
            &self.phases,
            self.model.get().map(Arc::as_ref),
            operation,
        )
    }
}

pub fn authorize_model_request(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    operation: &crate::protocol::OperationId,
    request: &crate::protocol::Id,
) -> Result<bool, StorageError> {
    let Some(op) = read.operations().operation(operation)? else {
        return Ok(false);
    };
    let Ok(work) = Work::from_operation(&op) else {
        return Ok(false);
    };
    let Some(run) = read.analysis().analysis_run(&work.run_id)? else {
        return Ok(false);
    };
    let original = &work.authority;
    let expected_project = match &run.intent.scope {
        crate::protocol::data::DataScope::Project { project_id } => Some(project_id),
        crate::protocol::data::DataScope::Workspace => None,
    };
    Ok(dispatch_binding_snapshot(read, operation, request)?
        && original.actor == *ctx.principal()
        && original.kind == ctx.principal_kind().as_str()
        && original.restore_epoch == ctx.restore_epoch().get()
        && original.root.as_ref() == ctx.root_id()
        && original.assignment.as_ref() == ctx.assignment_id()
        && expected_project == ctx.project_id()
        && (ctx.principal_kind() == crate::context::PrincipalKind::Human
            || original.session.as_ref() == ctx.session_id()))
}

pub(super) fn operation_result(command: AnalysisCommand, value: AnalysisRecord) -> OperationResult {
    match command {
        AnalysisCommand::LinkAndDetect | AnalysisCommand::IndexPublished => {
            OperationResult::AnalysisLinkAndDetect { value }
        }
        AnalysisCommand::Community => OperationResult::AnalysisCommunityAnalysis { value },
        AnalysisCommand::Reanalyze => OperationResult::AnalysisReanalyze { value },
        AnalysisCommand::ProcessBatch => OperationResult::AnalysisProcessBatch { value },
        AnalysisCommand::Synthesize => OperationResult::AnalysisSynthesize { value },
    }
}

fn completion(command: AnalysisCommand, value: AnalysisRecord) -> Result<Value, Failure> {
    serde_json::to_value(operation_result(command, value))
        .map_err(|_| StorageError::InvalidRecord("analysis completion is invalid".into()).into())
}

impl CommandHandler for AnalysisService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        self.commands.prepare(ctx, command, env)
    }
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        self.commands.decide(ctx, command, tx)
    }
}

impl QueryHandler for AnalysisService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        self.queries.prepare(ctx, query, env)
    }
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        read: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        self.queries.query(ctx, query, read)
    }
}
