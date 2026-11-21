use super::{CONSUMER, SweepDelivery};
use crate::context::InvocationContext;
use crate::ports::AuthorityFence;
use crate::ports::storage::{
    DreamCheckpoint, DreamCutoffs, DreamPhase, DreamProgress, DreamRequest, DreamRequestStage,
    DreamRunRow, DreamSignal, DreamStream, KnowledgeScope, OperationRecord, QueueState,
    StorageError, WriteTransaction,
};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::{AttemptId, Id};
use crate::services::effect_job;
use serde_json::Value;

/// Immutable operation and delivery coordinates. Every state change rechecks the live lease.
#[derive(Clone, Debug)]
pub struct SweepLease {
    operation: OperationRecord,
    delivery: SweepDelivery,
    generation: i64,
}

impl SweepLease {
    pub fn claim(
        tx: &mut dyn WriteTransaction,
        operation: &OperationRecord,
    ) -> Result<Self, StorageError> {
        let delivery: SweepDelivery = serde_json::from_value(operation.payload.clone())
            .map_err(|_| invalid("invalid Dreaming delivery"))?;
        let entry = tx
            .queue()
            .entry(&delivery.run)?
            .ok_or_else(|| lease_lost(&delivery.run))?;
        let lease = Self {
            operation: operation.clone(),
            delivery,
            generation: entry.generation,
        };
        lease.read(tx, false)?;
        Ok(lease)
    }

    pub fn run_id(&self) -> &Id {
        &self.delivery.run
    }

    pub fn consolidate(
        &self,
        tx: &mut dyn WriteTransaction,
        tool: &str,
        arguments: &Value,
        checked_sources: &std::collections::BTreeSet<String>,
    ) -> Result<Option<Value>, StorageError> {
        use crate::services::kernel::Failure;
        use crate::services::knowledge::dream_actions;
        if !dream_actions::is_write(tool) {
            return Ok(None);
        }
        self.context(tx)?;
        let run = self.read(tx, true)?;
        let point = tx.savepoint()?;
        match dream_actions::apply(tx, &run, tool, arguments, checked_sources) {
            Ok(result) => {
                tx.release(point)?;
                Ok(Some(result))
            }
            Err(failure) => {
                tx.rollback_to(point)?;
                match failure {
                    Failure::Refused(_) | Failure::Storage(StorageError::ScopeDenied) => Ok(Some(
                        serde_json::json!({"error":"DreamingKnowledgePolicyRefused"}),
                    )),
                    Failure::Storage(error) => Err(error),
                }
            }
        }
    }

    pub fn stage_proposal(
        &self,
        tx: &mut dyn WriteTransaction,
        arguments: &Value,
        artifact: crate::protocol::provider::ArtifactRef,
    ) -> Result<Option<Value>, StorageError> {
        self.context(tx)?;
        let run = self.read(tx, true)?;
        match super::proposals::stage_from_tool(tx, &run, arguments, artifact) {
            Ok(result) => Ok(Some(result)),
            Err(
                crate::services::kernel::Failure::Refused(_)
                | crate::services::kernel::Failure::Storage(StorageError::ScopeDenied),
            ) => Ok(None),
            Err(crate::services::kernel::Failure::Storage(error)) => Err(error),
        }
    }

    pub fn operation(&self) -> &OperationRecord {
        &self.operation
    }

    pub fn context(
        &self,
        tx: &mut dyn WriteTransaction,
    ) -> Result<InvocationContext, StorageError> {
        self.read(tx, true)?;
        self.delivery.restore(tx, &self.operation.id)
    }

    pub fn recovery_context(
        &self,
        tx: &mut dyn WriteTransaction,
    ) -> Result<InvocationContext, StorageError> {
        self.read(tx, false)?;
        self.delivery.restore(tx, &self.operation.id)
    }

    pub fn send_fences(&self, request: &Id) -> Vec<AuthorityFence> {
        let mut fences = self.delivery.fences.clone();
        fences.push(AuthorityFence::DreamingSweep {
            run: self.delivery.run.clone(),
            generation: self.generation,
            request: request.clone(),
        });
        fences
    }

    pub fn read(
        &self,
        tx: &mut dyn WriteTransaction,
        dispatch: bool,
    ) -> Result<DreamRunRow, StorageError> {
        effect_job::attempt_holds(tx, &self.operation, CONSUMER)?;
        let entry = tx
            .queue()
            .entry(self.run_id())?
            .ok_or_else(|| lease_lost(self.run_id()))?;
        if entry.generation != self.generation
            || entry.operation_id.as_ref() != Some(&self.operation.id)
            || entry.consumer != CONSUMER
            || !matches!(entry.state, QueueState::Claimed | QueueState::Reconciling)
        {
            return Err(lease_lost(self.run_id()));
        }
        if dispatch && (entry.cancel_requested || entry.state != QueueState::Claimed) {
            return Err(StorageError::Cancelled);
        }
        tx.dreaming()?
            .dream_run(self.run_id())?
            .ok_or_else(|| lease_lost(self.run_id()))
    }

    pub fn start(&self, tx: &mut dyn WriteTransaction) -> Result<DreamRunRow, StorageError> {
        self.context(tx)?;
        let row = self.read(tx, true)?;
        match row.phase {
            DreamPhase::Queued => {
                let checkpoint = row.checkpoint.clone();
                self.advance(tx, &row, DreamPhase::Running, checkpoint, None)
            }
            DreamPhase::Running => Ok(row),
            _ => Err(lease_lost(self.run_id())),
        }
    }

    /// Metadata-only read of the exact bounded current sweep. Payloads are opened one by one
    /// by the private read tools, never carried into the initial model prompt.
    pub fn signals(
        &self,
        tx: &mut dyn WriteTransaction,
        watermark: &DreamCutoffs,
        limit_per_stream: u16,
    ) -> Result<Vec<(DreamStream, KnowledgeScope, Vec<DreamSignal>)>, StorageError> {
        let row = self.read(tx, true)?;
        let repo = tx.dreaming()?;
        let prior = repo
            .dream_scope(&row.scope)?
            .completed
            .map(|(cutoffs, _)| cutoffs)
            .unwrap_or_default();
        let mut streams = vec![
            (
                DreamStream::Knowledge,
                row.scope.clone(),
                prior.knowledge.max(watermark.knowledge),
                row.cutoffs.knowledge,
            ),
            (
                DreamStream::Reviews,
                row.scope.clone(),
                prior.reviews.max(watermark.reviews),
                row.cutoffs.reviews,
            ),
            (
                DreamStream::Sources,
                row.scope.clone(),
                prior.sources.max(watermark.sources),
                row.cutoffs.sources,
            ),
        ];
        if row.scope.project().is_some() {
            streams.extend([
                (
                    DreamStream::Knowledge,
                    KnowledgeScope::Workspace,
                    prior.workspace_knowledge.max(watermark.workspace_knowledge),
                    row.cutoffs.workspace_knowledge,
                ),
                (
                    DreamStream::Sources,
                    KnowledgeScope::Workspace,
                    prior.workspace_sources.max(watermark.workspace_sources),
                    row.cutoffs.workspace_sources,
                ),
            ]);
        }
        streams
            .into_iter()
            .map(|(kind, scope, start, end)| {
                let signals = repo.dream_signals(&scope, kind, start, end, limit_per_stream)?;
                Ok((kind, scope, signals))
            })
            .collect()
    }

    pub fn frozen_cutoffs(
        &self,
        tx: &mut dyn WriteTransaction,
    ) -> Result<DreamCutoffs, StorageError> {
        self.read(tx, false).map(|row| row.cutoffs)
    }

    /// Keep the original logical identity and frame before submitting anything to the router.
    pub fn prepare_request(
        &self,
        tx: &mut dyn WriteTransaction,
        logical: &Id,
        frame: &ArtifactRef,
    ) -> Result<DreamRunRow, StorageError> {
        self.context(tx)?;
        let row = self.read(tx, true)?;
        let mut checkpoint = row.checkpoint.clone().unwrap_or_default();
        if let Some(existing) = &checkpoint.request
            && (existing.stage == DreamRequestStage::PossiblySent
                || (existing.stage == DreamRequestStage::Prepared
                    && existing.logical_id != *logical))
        {
            return Err(invalid("unsettled Dreaming provider request"));
        }
        // Preserve the last known exact frame even when admission refuses before a send.
        checkpoint.continuation = Some(frame.clone());
        checkpoint.request = Some(DreamRequest {
            logical_id: logical.clone(),
            attempt_id: None,
            frame: frame.clone(),
            stage: DreamRequestStage::Prepared,
            result: None,
        });
        self.advance(
            tx,
            &row,
            DreamPhase::Running,
            Some(checkpoint),
            row.report.clone(),
        )
    }

    /// Called by the router's owned source once the attempt exists, before physical transport.
    pub fn before_send(
        &self,
        tx: &mut dyn WriteTransaction,
        logical: &Id,
        attempt: &AttemptId,
    ) -> Result<DreamRunRow, StorageError> {
        self.context(tx)?;
        let row = self.read(tx, true)?;
        let mut checkpoint = row
            .checkpoint
            .clone()
            .ok_or_else(|| invalid("missing Dreaming checkpoint"))?;
        let request = checkpoint
            .request
            .as_mut()
            .filter(|request| {
                request.logical_id == *logical && request.stage == DreamRequestStage::Prepared
            })
            .ok_or_else(|| invalid("Dreaming provider request already crossed its send fence"))?;
        if let Some(cost_cap) = row.policy.max_micro_usd {
            let balance = tx
                .usage()
                .attempt_balance(&attempt.0)?
                .ok_or(StorageError::ScopeDenied)?;
            let reserved = balance
                .reserved_micro_usd
                .or(balance.priced_micro_usd)
                .and_then(|cost| u64::try_from(cost).ok());
            if reserved
                .is_none_or(|cost| checkpoint.priced_micro_usd.saturating_add(cost) > cost_cap)
            {
                return Err(StorageError::ScopeDenied);
            }
        }
        request.attempt_id = Some(attempt.clone());
        request.stage = DreamRequestStage::PossiblySent;
        self.advance(
            tx,
            &row,
            DreamPhase::Running,
            Some(checkpoint),
            row.report.clone(),
        )
    }

    pub fn observe(
        &self,
        tx: &mut dyn WriteTransaction,
        logical: &Id,
        attempt: &AttemptId,
        result: &ArtifactRef,
        mut checkpoint: DreamCheckpoint,
    ) -> Result<DreamRunRow, StorageError> {
        let row = self.read(tx, false)?;
        let mut request = row
            .checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint.request.clone())
            .filter(|request| request.logical_id == *logical)
            .ok_or_else(|| invalid("Dreaming observation has no matching request"))?;
        request.attempt_id = Some(attempt.clone());
        request.stage = DreamRequestStage::Observed;
        request.result = Some(result.clone());
        checkpoint.request = Some(request);
        self.advance(
            tx,
            &row,
            DreamPhase::Running,
            Some(checkpoint),
            row.report.clone(),
        )
    }

    pub fn finish(
        &self,
        tx: &mut dyn WriteTransaction,
        phase: DreamPhase,
        report: Value,
    ) -> Result<DreamRunRow, StorageError> {
        if phase == DreamPhase::Completed {
            self.recovery_context(tx)?;
        }
        let row = self.read(tx, false)?;
        let checkpoint = row.checkpoint.clone();
        self.advance(tx, &row, phase, checkpoint, Some(report))
    }

    /// The exact round's text and action receipts are now retained as the next material.
    pub fn continue_with(
        &self,
        tx: &mut dyn WriteTransaction,
        material: ArtifactRef,
    ) -> Result<DreamRunRow, StorageError> {
        self.context(tx)?;
        let row = self.read(tx, true)?;
        let mut checkpoint = row
            .checkpoint
            .clone()
            .ok_or_else(|| invalid("missing Dreaming checkpoint"))?;
        if checkpoint
            .request
            .as_ref()
            .is_none_or(|request| request.stage != DreamRequestStage::Observed)
        {
            return Err(invalid("Dreaming continuation lacks an observed round"));
        }
        checkpoint.request = None;
        checkpoint.continuation = Some(material);
        self.advance(
            tx,
            &row,
            DreamPhase::Running,
            Some(checkpoint),
            row.report.clone(),
        )
    }

    fn advance(
        &self,
        tx: &mut dyn WriteTransaction,
        row: &DreamRunRow,
        phase: DreamPhase,
        checkpoint: Option<DreamCheckpoint>,
        report: Option<Value>,
    ) -> Result<DreamRunRow, StorageError> {
        tx.dreaming()?.advance_dream_run(
            &row.id,
            row.revision,
            row.generation,
            &DreamProgress {
                phase,
                checkpoint,
                report,
            },
        )
    }
}

pub fn inference_fence_in(
    tx: &mut dyn WriteTransaction,
    run: &Id,
    generation: i64,
    request: &Id,
) -> Result<bool, StorageError> {
    let Some(entry) = tx.queue().entry(run)? else {
        return Ok(false);
    };
    if entry.consumer != CONSUMER
        || entry.generation != generation
        || entry.state != QueueState::Claimed
        || entry.cancel_requested
    {
        return Ok(false);
    }
    let repo = tx.dreaming()?;
    let Some(row) = repo.dream_run(run)? else {
        return Ok(false);
    };
    let settings = repo.dream_scope(&row.scope)?;
    Ok(row.phase == DreamPhase::Running
        && settings.revision == row.settings_revision
        && settings.policy.enabled
        && row.operation_id == entry.operation_id
        && row
            .checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint.request.as_ref())
            .is_some_and(|round| {
                round.logical_id == *request && round.stage != DreamRequestStage::Observed
            }))
}

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}

fn lease_lost(run: &Id) -> StorageError {
    StorageError::LeaseLost {
        kind: CONSUMER,
        id: run.as_str().to_owned(),
    }
}
