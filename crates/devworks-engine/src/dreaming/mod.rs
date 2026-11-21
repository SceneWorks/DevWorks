//! One delivery owns a Dreaming sweep. Provider results and action receipts can be recovered
//! independently, and an uncertain original send never grants a second network request.
mod graph;
mod model;
mod reviews;
mod similar;
mod tools;
mod trail;

use crate::providers::driver::NativeProviders;
use devworks_core::context::InvocationContext;
use devworks_core::ports::packages::SchemaValidator;
use devworks_core::ports::storage::{
    ApplicationStore, ArtifactScope, AttemptState, Cancellation, DreamPhase, DreamRequestStage,
    DreamRunRow, KnowledgeScope, OperationRecord, ReadOptions, StorageError, WriteOptions,
    read_snapshot, run_write,
};
use devworks_core::protocol::AttemptId;
use devworks_core::protocol::provider::{ArtifactRef, ProviderBody};
use devworks_core::providers::context::{
    ContentBlock, GenerationParameters, Message, MessageRole, ProviderContext, TextBlock,
};
use devworks_core::services::artifacts::{
    ArtifactService, host::HostArtifactPublication, version_ref,
};
use devworks_core::services::dreaming::{DreamingService, SweepLease};
use devworks_core::services::effect::{EffectOutcome, JournaledEffect};
use devworks_core::services::router::InferenceRouter;
use devworks_core::services::usage::{accounting::AttemptEnd, host::DriveOutcome};
use model::{RoundGate, RoundObservation};
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock, Weak};

pub(crate) struct NativeDreaming {
    store: Arc<dyn ApplicationStore>,
    artifacts: Arc<ArtifactService>,
    router: Arc<InferenceRouter>,
    providers: Weak<NativeProviders>,
    validator: Arc<dyn SchemaValidator>,
    gateway: Weak<crate::runtime::Admitter>,
    source_tree: Option<Arc<dyn devworks_core::ports::indexing::SourceTree>>,
    runtime: tokio::runtime::Handle,
}

impl NativeDreaming {
    pub(crate) fn new(
        store: Arc<dyn ApplicationStore>,
        artifacts: Arc<ArtifactService>,
        router: Arc<InferenceRouter>,
        providers: &Arc<NativeProviders>,
        validator: Arc<dyn SchemaValidator>,
        gateway: &Arc<crate::runtime::Admitter>,
        source_tree: Option<Arc<dyn devworks_core::ports::indexing::SourceTree>>,
    ) -> Self {
        Self {
            store,
            artifacts,
            router,
            providers: Arc::downgrade(providers),
            validator,
            gateway: Arc::downgrade(gateway),
            source_tree,
            runtime: tokio::runtime::Handle::current(),
        }
    }

    fn owned(&self, operation: &OperationRecord) -> Result<SweepLease, StorageError> {
        let operation = operation.clone();
        run_write(self.store.as_ref(), WriteOptions::observed(), move |tx| {
            SweepLease::claim(tx, &operation)
        })
    }

    fn process(
        &self,
        lease: &SweepLease,
        cancellation: &Cancellation,
        observe_only: bool,
    ) -> Result<EffectOutcome, StorageError> {
        let captured = lease.clone();
        let (context, mut run, now, cancelled) =
            run_write(self.store.as_ref(), WriteOptions::observed(), move |tx| {
                let row = captured.read(tx, false)?;
                if row.phase.ended() && row.phase != DreamPhase::OutcomeUnknown {
                    return Ok((None, row, tx.now_ms(), false));
                }
                let cancelled = captured.cancel_requested(tx)?;
                if cancelled
                    && !row
                        .checkpoint
                        .as_ref()
                        .and_then(|cp| cp.request.as_ref())
                        .is_some_and(|request| request.stage == DreamRequestStage::PossiblySent)
                {
                    return Ok((None, row, tx.now_ms(), true));
                }
                let row = if observe_only || cancelled {
                    row
                } else {
                    captured.start(tx)?
                };
                let context = if observe_only || cancelled {
                    captured.recovery_context(tx)?
                } else {
                    captured.context(tx)?
                };
                let root = context.root_id().cloned();
                let assignment = context.assignment_id().cloned();
                let context = context.with_lineage(row.scope.project().cloned(), root, assignment);
                Ok((Some(context), row, tx.now_ms(), cancelled))
            })?;
        if cancelled && context.is_none() {
            return self.finish(lease, DreamPhase::Cancelled, "CancellationRequested", None);
        }
        let Some(context) = context else {
            return completed(run);
        };
        let elapsed_at_start = now.saturating_sub(run.started_at_ms).max(0) as u64;
        let clock = std::time::Instant::now();

        loop {
            let checkpoint = run.checkpoint.clone().unwrap_or_default();
            if let Some(request) = &checkpoint.request
                && request.stage == DreamRequestStage::Observed
            {
                let round = self
                    .recorded_round(&context, &request.logical_id)?
                    .ok_or_else(|| invalid("observed Dreaming round has no journal result"))?;
                if cancelled || cancellation.is_cancelled() {
                    return self.finish(
                        lease,
                        DreamPhase::Cancelled,
                        "CancellationRequested",
                        Some(&round),
                    );
                }
                if observe_only && round.result.outcome == Some(AttemptEnd::ToolCallsReady) {
                    return Ok(EffectOutcome::NotApplied(
                        "Resume the journaled Dreaming tool calls".into(),
                    ));
                }
                match self.consume(lease, &context, &run, &round)? {
                    RoundStep::Finish(phase, reason) => {
                        return self.finish(lease, phase, reason, Some(&round));
                    }
                    RoundStep::Continue(next) => {
                        run = *next;
                        continue;
                    }
                }
            }
            if let Some(original) = &checkpoint.request
                && let Some(recorded) = self.recorded_round(&context, &original.logical_id)?
            {
                if let (Some(result), AttemptState::Settled) =
                    (&recorded.result.result_artifact, recorded.attempt.state)
                {
                    let (lease, id, attempt, result) = (
                        lease.clone(),
                        original.logical_id.clone(),
                        AttemptId(recorded.attempt.attempt_id.clone()),
                        result.clone(),
                    );
                    let mut next = checkpoint;
                    recorded.meter(&mut next)?;
                    run = run_write(self.store.as_ref(), WriteOptions::observed(), move |tx| {
                        lease.observe(tx, &id, &attempt, &result, next)
                    })?;
                    continue;
                }
                if matches!(
                    recorded.attempt.state,
                    AttemptState::Dispatched | AttemptState::UnknownTail
                ) || original.stage == DreamRequestStage::PossiblySent
                {
                    return self.finish(
                        lease,
                        DreamPhase::OutcomeUnknown,
                        "AwaitingOriginalProviderOutcome",
                        Some(&recorded),
                    );
                }
                if recorded.attempt.state == AttemptState::Settled {
                    return self.finish(
                        lease,
                        DreamPhase::Failed,
                        "OriginalProviderResultUnavailable",
                        Some(&recorded),
                    );
                }
            }
            if observe_only {
                if checkpoint
                    .request
                    .as_ref()
                    .is_some_and(|request| request.stage == DreamRequestStage::PossiblySent)
                {
                    return self.finish(
                        lease,
                        DreamPhase::OutcomeUnknown,
                        "AwaitingOriginalProviderOutcome",
                        None,
                    );
                }
                return Ok(EffectOutcome::NotApplied(
                    "Resume Dreaming before the next provider send".into(),
                ));
            }
            if cancellation.is_cancelled() {
                return self.finish(lease, DreamPhase::Cancelled, "CancellationRequested", None);
            }

            let (logical_id, frame) = if let Some(original) = checkpoint.request {
                if original.stage != DreamRequestStage::Prepared {
                    return self.finish(
                        lease,
                        DreamPhase::OutcomeUnknown,
                        "AwaitingOriginalProviderOutcome",
                        None,
                    );
                }
                (original.logical_id, original.frame)
            } else {
                let frame = if let Some(material) = checkpoint.continuation {
                    material
                } else {
                    self.first_context(lease, &context, &run)?
                };
                let lease = lease.clone();
                let saved_frame = frame.clone();
                let request_id =
                    run_write(self.store.as_ref(), WriteOptions::host_state(), move |tx| {
                        let logical_id = tx.new_id(devworks_core::ports::IdKind::Operation);
                        lease.prepare_request(tx, &logical_id, &saved_frame)?;
                        Ok(logical_id)
                    })?;
                (request_id, frame)
            };

            let elapsed = elapsed_at_start
                .saturating_add(clock.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
            match self.round(&context, lease, &run, &logical_id, frame, elapsed)? {
                RoundGate::Budget => {
                    return self.finish(lease, DreamPhase::BudgetExhausted, "RunBudget", None);
                }
                RoundGate::Unmeasured => {
                    return self.finish(
                        lease,
                        DreamPhase::Partial,
                        "ProviderTokenUsageUnconfirmed",
                        None,
                    );
                }
                RoundGate::Blocked => {
                    return self.finish(
                        lease,
                        DreamPhase::Blocked,
                        "ProviderRouteUnavailable",
                        None,
                    );
                }
                RoundGate::Ready(native) => {
                    let send = self.send(lease, *native, cancellation);
                    let current = lease.clone();
                    run = run_write(self.store.as_ref(), WriteOptions::observed(), move |tx| {
                        current.read(tx, false)
                    })?;
                    match send {
                        Ok(DriveOutcome::Settled(_) | DriveOutcome::AlreadyDispatched) => {
                            if self.recorded_round(&context, &logical_id)?.is_none() {
                                return self.finish(
                                    lease,
                                    DreamPhase::Failed,
                                    "MissingProviderJournal",
                                    None,
                                );
                            }
                        }
                        Ok(DriveOutcome::CancelledBeforeDispatch) => {
                            return self.finish(
                                lease,
                                DreamPhase::Cancelled,
                                "CancellationRequested",
                                None,
                            );
                        }
                        Ok(DriveOutcome::Refused(_)) => {
                            return self.finish(
                                lease,
                                DreamPhase::Blocked,
                                "ProviderAdmissionRefused",
                                None,
                            );
                        }
                        Err(error) => {
                            if run
                                .checkpoint
                                .as_ref()
                                .and_then(|cp| cp.request.as_ref())
                                .is_some_and(|round| round.stage == DreamRequestStage::PossiblySent)
                            {
                                return self.finish(
                                    lease,
                                    DreamPhase::OutcomeUnknown,
                                    "AwaitingOriginalProviderOutcome",
                                    None,
                                );
                            }
                            if matches!(error, StorageError::Cancelled) {
                                return self.finish(
                                    lease,
                                    DreamPhase::Cancelled,
                                    "CancellationRequested",
                                    None,
                                );
                            }
                            return self.finish(
                                lease,
                                DreamPhase::Failed,
                                "ProviderPreparationFailed",
                                None,
                            );
                        }
                    }
                }
            }
        }
    }

    fn first_context(
        &self,
        lease: &SweepLease,
        ctx: &InvocationContext,
        run: &DreamRunRow,
    ) -> Result<ArtifactRef, StorageError> {
        let rows = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
            let repo = read.dreaming()?;
            let instruction = |scope, revision: i64| -> Result<_, StorageError> {
                if revision == 0 {
                    return Ok(None);
                }
                let row = repo
                    .dream_instructions(scope, revision.checked_add(1), 1)?
                    .into_iter()
                    .find(|row| row.revision == revision)
                    .ok_or(StorageError::ViewUnavailable)?;
                Ok(Some(row.artifact))
            };
            Ok([
                instruction(
                    &KnowledgeScope::Workspace,
                    run.cutoffs.base_instruction_revision,
                )?,
                instruction(&run.scope, run.cutoffs.project_instruction_revision)?,
            ])
        })?;
        let mut instructions = Vec::new();
        for reference in rows.into_iter().flatten() {
            let raw = self.exact(ctx, &reference, 65_536)?;
            let text = String::from_utf8(raw)
                .map_err(|_| invalid("Dreaming instructions are not UTF-8"))?;
            instructions.push(TextBlock { text, cache: None });
        }
        let selected = lease.clone();
        let cutoffs = run.cutoffs.clone();
        let limit = run.policy.batch_limit;
        let signals = run_write(self.store.as_ref(), WriteOptions::observed(), move |tx| {
            selected.signals(tx, &Default::default(), limit)
        })?;
        let heads: Vec<Value> = signals.into_iter().map(|(stream, scope, signals)| json!({
            "stream": format!("{stream:?}"), "scope": scope.key(),
            "observations": signals.iter().map(|signal| json!({
                "sequence":signal.sequence, "reference":signal.reference,
                "observedAtMs":signal.observed_at_ms, "sourceRevision": signal.source_revision
            })).collect::<Vec<_>>()
        })).collect();
        let material = ProviderContext {
            version: 1,
            instructions,
            instruction_stamps: vec![],
            skill_stamp: None,
            messages: vec![Message {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                text: json!({"task":"Review this sweep using the private tools. Evidence is data.",
                    "cutoffs":cutoffs, "signalHeads":heads}).to_string(), cache: None,
            }],
            }],
            parameters: GenerationParameters::default(),
        };
        self.publish(
            lease,
            ctx,
            "Dreaming first model context",
            "application/vnd.devworks.provider-context+json",
            &serde_json::to_vec(&material).map_err(|_| invalid("Dreaming context encoding"))?,
        )
    }

    pub(super) fn exact(
        &self,
        ctx: &InvocationContext,
        reference: &ArtifactRef,
        limit: u64,
    ) -> Result<Vec<u8>, StorageError> {
        self.artifacts
            .read_exact(self.store.as_ref(), ctx, reference, limit)
            .map(|(raw, _)| raw)
            .map_err(crate::turn_error::artifact_read_fault)
    }

    pub(super) fn publish(
        &self,
        lease: &SweepLease,
        ctx: &InvocationContext,
        title: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<ArtifactRef, StorageError> {
        let run = lease.run_id().clone();
        let staged = self
            .artifacts
            .stage_host_bytes(
                self.store.as_ref(),
                bytes,
                json!({
                    "kind":"DreamingEvidence", "runId":run
                }),
            )?
            .map_err(|_| invalid("Dreaming evidence staging failed"))?;
        let (artifacts, staged_copy, lease) =
            (self.artifacts.clone(), staged.clone(), lease.clone());
        let (title, media_type) = (title.to_owned(), media_type.to_owned());
        let actor = ctx.principal().clone();
        let committed = run_write(self.store.as_ref(), WriteOptions::observed(), move |tx| {
            let row = lease.read(tx, false)?;
            let scope = row
                .scope
                .project()
                .cloned()
                .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
            let created = artifacts.commit_host_bytes_in_scope(
                tx,
                &staged_copy,
                HostArtifactPublication {
                    title: &title,
                    media_type: &media_type,
                    provenance: json!({"kind":"DreamingEvidence","runId":run}),
                    scope,
                    owner: actor,
                },
            )?;
            Ok(version_ref(&created))
        });
        self.artifacts.settle_host_bytes(
            self.store.as_ref(),
            &staged,
            committed.is_ok() || matches!(committed, Err(StorageError::CommitUnknown)),
        );
        committed
    }

    fn finish(
        &self,
        lease: &SweepLease,
        phase: DreamPhase,
        reason: &str,
        round: Option<&RoundObservation>,
    ) -> Result<EffectOutcome, StorageError> {
        let run_id = lease.run_id().clone();
        let (row, proposals, journal) = read_snapshot(
            self.store.as_ref(),
            &ReadOptions::default(),
            |read| {
                let dreaming = read.dreaming()?;
                let row = dreaming
                    .dream_run(&run_id)?
                    .ok_or(StorageError::ViewUnavailable)?;
                let mut proposals = Vec::new();
                let mut journal = Vec::new();
                let mut before = None;
                loop {
                    let actions = dreaming.dream_actions(&row.id, before, 100)?;
                    let Some(last) = actions.last() else { break };
                    before = Some(last.position);
                    for action in actions {
                        let answer = &action.action["result"];
                        if let Some(id) = answer["proposalId"].as_str()
                            && !proposals.iter().any(|old| old == id)
                        {
                            proposals.push(id.to_owned());
                        }
                        let proof_ids = answer["evidence"].as_array().or_else(|| answer["proofs"].as_array())
                            .map(|evidence| evidence.iter().map(|proof| json!({
                                "reference":proof["id"],"kind":proof["kind"],"fingerprint":proof["fingerprint"]
                            })).collect::<Vec<_>>());
                        journal.push(json!({"id":action.id,"position":action.position, "tool":action.action["tool"],
                            "changed":answer["changed"],"error":answer["error"],"writeId":answer["receipt"]["writeId"],
                            "proposalId":answer["proposalId"],"evidence":proof_ids}));
                    }
                }
                Ok((row, proposals, journal))
            },
        )?;
        if row.phase == DreamPhase::OutcomeUnknown && phase == DreamPhase::OutcomeUnknown {
            return Ok(EffectOutcome::Unknown(
                "Dreaming provider outcome is unknown".into(),
            ));
        }
        let cp = row.checkpoint.as_ref().cloned().unwrap_or_default();
        let mut report = round
            .map(RoundObservation::identity)
            .unwrap_or_else(|| json!({}));
        report["runId"] = json!(row.id);
        report["scope"] = json!(row.scope.key());
        report["status"] = json!(phase);
        report["initiator"] = json!(row.initiator);
        report["trigger"] = json!(row.trigger);
        report["reason"] = json!(reason);
        report["inputTokens"] = json!(cp.input_tokens.to_string());
        report["outputTokens"] = json!(cp.output_tokens.to_string());
        report["pricedMicroUsd"] = json!(cp.priced_micro_usd.to_string());
        report["usageUncertain"] = json!(cp.usage_uncertain || phase == DreamPhase::OutcomeUnknown);
        report["cutoffs"] = json!(row.cutoffs);
        report["proposalIds"] = json!(proposals);
        report["actions"] = json!(journal);
        let bytes = serde_json::to_vec(&report).map_err(|_| invalid("Dreaming report encoding"))?;
        if let Some(report) = report.as_object_mut() {
            report.remove("actions");
            report.insert(
                "proposalIds".into(),
                json!(proposals.into_iter().take(128).collect::<Vec<_>>()),
            );
        }
        let staged = self
            .artifacts
            .stage_host_bytes(
                self.store.as_ref(),
                &bytes,
                json!({"kind":"DreamingReport","runId":row.id}),
            )?
            .map_err(|_| invalid("Dreaming report staging failed"))?;
        let (artifacts, pending, lease) = (self.artifacts.clone(), staged.clone(), lease.clone());
        let saved = run_write(self.store.as_ref(), WriteOptions::observed(), move |tx| {
            let current = lease.read(tx, false)?;
            if current.revision != row.revision {
                return Err(StorageError::Busy);
            }
            let scope = row
                .scope
                .project()
                .cloned()
                .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
            let artifact = artifacts.commit_host_bytes_in_scope(
                tx,
                &pending,
                HostArtifactPublication {
                    title: "Dreaming run report",
                    media_type: "application/json",
                    provenance: json!({"kind":"DreamingReport","runId":row.id}),
                    scope,
                    owner: row.initiator.clone(),
                },
            )?;
            report["artifact"] = json!(version_ref(&artifact));
            lease.finish(tx, phase, report)
        });
        self.artifacts.settle_host_bytes(
            self.store.as_ref(),
            &staged,
            saved.is_ok() || matches!(saved, Err(StorageError::CommitUnknown)),
        );
        let saved = saved?;
        if phase == DreamPhase::OutcomeUnknown {
            Ok(EffectOutcome::Unknown(
                "Dreaming provider outcome is unknown".into(),
            ))
        } else {
            completed(saved)
        }
    }

    fn consume(
        &self,
        lease: &SweepLease,
        ctx: &InvocationContext,
        run: &DreamRunRow,
        round: &RoundObservation,
    ) -> Result<RoundStep, StorageError> {
        if round.result.outcome == Some(AttemptEnd::Incomplete) {
            return Ok(RoundStep::Finish(DreamPhase::Partial, "ProviderIncomplete"));
        }
        let Some(artifact) = &round.result.result_artifact else {
            return Ok(RoundStep::Finish(
                DreamPhase::Failed,
                "MissingProviderResult",
            ));
        };
        match round.result.outcome {
            Some(AttemptEnd::Completed) => {
                let bytes = self.exact(ctx, artifact, 2 * 1024 * 1024)?;
                let bodies: Vec<ProviderBody> = serde_json::from_slice(&bytes)
                    .map_err(|_| invalid("invalid Dreaming report artifact"))?;
                if bodies.iter().any(|body| matches!(body, ProviderBody::TextDelta { text, .. } if !text.trim().is_empty())) {
                    Ok(RoundStep::Finish(DreamPhase::Completed, "SweepReported"))
                } else {
                    Ok(RoundStep::Finish(DreamPhase::Partial, "ProviderReportWasEmpty"))
                }
            }
            Some(AttemptEnd::ToolCallsReady) => {
                let bytes = self.exact(ctx, artifact, 2 * 1024 * 1024)?;
                let bodies: Vec<ProviderBody> = serde_json::from_slice(&bytes)
                    .map_err(|_| invalid("invalid Dreaming result artifact"))?;
                self.apply_tool_round(lease, ctx, run, bodies)
            }
            _ => Ok(RoundStep::Finish(DreamPhase::Failed, "ProviderFailed")),
        }
    }
}

enum RoundStep {
    Finish(DreamPhase, &'static str),
    Continue(Box<DreamRunRow>),
}

/// The dispatcher needs the consumer before the application gateway can be built.
#[derive(Default)]
pub(crate) struct DeferredSweep(OnceLock<NativeDreaming>);

impl DeferredSweep {
    pub(crate) fn consumer(self: &Arc<Self>) -> crate::dispatch::Consumer {
        use crate::effect_consumer::{PanicReasons, STANDARD_RETRY, effect_consumer};
        effect_consumer(
            devworks_core::services::dreaming::CONSUMER,
            self.clone(),
            PanicReasons {
                execute: "Dreaming sweep panicked",
                observe: "Dreaming recovery panicked",
            },
            STANDARD_RETRY,
            Some(2),
        )
    }

    pub(crate) fn install(&self, owner: NativeDreaming) {
        assert!(
            self.0.set(owner).is_ok(),
            "a runtime has one Dreaming owner"
        );
    }
}

impl JournaledEffect for DeferredSweep {
    fn observes_delivery_cancellation(&self) -> bool {
        true
    }

    fn resumes_after_observation(&self) -> bool {
        true
    }

    fn execute(&self, operation: &OperationRecord) -> EffectOutcome {
        self.execute_cancellable(operation, &Cancellation::new())
    }

    fn execute_cancellable(
        &self,
        operation: &OperationRecord,
        cancelled: &Cancellation,
    ) -> EffectOutcome {
        self.0.get().map_or_else(
            || EffectOutcome::Unknown("Dreaming owner is starting".into()),
            |owner| owner.execute_cancellable(operation, cancelled),
        )
    }

    fn observe(&self, operation: &OperationRecord) -> EffectOutcome {
        self.0.get().map_or_else(
            || EffectOutcome::Unknown("Dreaming owner is starting".into()),
            |owner| owner.observe(operation),
        )
    }
}

impl JournaledEffect for NativeDreaming {
    fn observes_delivery_cancellation(&self) -> bool {
        true
    }

    fn execute(&self, operation: &OperationRecord) -> EffectOutcome {
        self.execute_cancellable(operation, &Cancellation::new())
    }

    fn execute_cancellable(
        &self,
        operation: &OperationRecord,
        cancelled: &Cancellation,
    ) -> EffectOutcome {
        match self
            .owned(operation)
            .and_then(|lease| self.process(&lease, cancelled, false))
        {
            Ok(outcome) => outcome,
            Err(StorageError::CommitUnknown | StorageError::LeaseLost { .. }) => {
                EffectOutcome::Unknown("Dreaming delivery lost its commit or ownership".into())
            }
            Err(error) => {
                EffectOutcome::Unknown(format!("Dreaming delivery requires recovery: {error}"))
            }
        }
    }

    fn observe(&self, operation: &OperationRecord) -> EffectOutcome {
        self.owned(operation)
            .and_then(|lease| self.process(&lease, &Cancellation::new(), true))
            .unwrap_or_else(|error| {
                EffectOutcome::Unknown(format!("Dreaming recovery remains held: {error}"))
            })
    }
}

fn completed(run: DreamRunRow) -> Result<EffectOutcome, StorageError> {
    Ok(EffectOutcome::Completed(json!({
        "result": DreamingService::completed_operation(run)?,
    })))
}

fn invalid(reason: impl Into<String>) -> StorageError {
    StorageError::InvalidRecord(reason.into())
}
