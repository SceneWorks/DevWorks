//! Observe exact request evidence without opening inference on the recovery path.
use super::{
    commands::CONSUMER,
    modeling::{self, AnalysisModel},
    paid::ModelBinding,
    phase_store::{self, PhaseEvidence, PhaseStore},
    selection::{self, Selection},
    service::{AnalysisService, Execution},
    work::{Claim, Work},
};
use crate::ports::Authorization;
use crate::ports::storage::*;
use crate::protocol::{Id, OperationState};
use crate::services::kernel::Failure;
use serde_json::{Value, json};
use std::sync::Arc;

enum Settled {
    Applied(Value),
    Failed,
    NotApplied,
}

struct Resolution {
    phase: AnalysisPhase,
    subject: Id,
    end: Settled,
}

pub(super) fn observe(
    service: &AnalysisService,
    store: &PhaseStore,
    model: Option<&dyn AnalysisModel>,
    operation: &OperationRecord,
) -> Execution {
    recover(service, store, model, operation).unwrap_or(Execution::Unknown(
        "original analysis authority or durable evidence is unavailable",
    ))
}

fn recover(
    service: &AnalysisService,
    store: &PhaseStore,
    model: Option<&dyn AnalysisModel>,
    operation: &OperationRecord,
) -> Result<Execution, Failure> {
    let work = Work::from_operation(operation)?;
    let ctx = work.context(&store.store.info().store_id)?;
    let run = read_snapshot(store.store.as_ref(), &ReadOptions::default(), |read| {
        Ok((|| -> Result<_, Failure> {
            work.authorize(read, &ctx)?;
            let run = read
                .analysis()
                .analysis_run(&work.run_id)?
                .filter(|run| run.active_operation == operation.id)
                .ok_or_else(selection::unavailable)?;
            Selection::check_saved_authority(read, &ctx, &run.intent.scope, &run)?;
            Ok(run)
        })())
    })??;
    if run.terminal() {
        return service.outcome(&work, &ctx, run);
    }

    let mut resolved = Vec::new();
    let mut remaining = false;
    for phase in &run.phases {
        for subject in phase.subjects.iter().filter(|subject| {
            matches!(
                subject.state,
                OperationState::Dispatching | OperationState::OutcomeUnknown
            )
        }) {
            let observation = model.map_or(modeling::Observation::Unknown, |model| {
                model.observe(&ctx, &subject.request)
            });
            let end = match observation {
                modeling::Observation::Completed(terminal)
                    if terminal.request == subject.request =>
                {
                    let binding: ModelBinding = serde_json::from_value(subject.binding.clone())
                        .map_err(|_| selection::unavailable())?;
                    binding
                        .recover(terminal, subject.id.clone())
                        .map_or(Settled::Failed, Settled::Applied)
                }
                modeling::Observation::Failed | modeling::Observation::Refused(_) => {
                    Settled::Failed
                }
                modeling::Observation::NotApplied => Settled::NotApplied,
                _ => {
                    remaining = true;
                    continue;
                }
            };
            resolved.push(Resolution {
                phase: phase.phase,
                subject: subject.id.clone(),
                end,
            });
        }
    }
    if remaining && resolved.is_empty() {
        return Ok(Execution::Unknown("provider outcome is still unknown"));
    }
    // Observing a prior provider request uses its original durable identity. The
    // computation lease only authorizes restarting analysis against the live exact source.
    let selected =
        read_snapshot(
            store.store.as_ref(),
            &ReadOptions::default(),
            |read| match Selection::resolve(
                read,
                &ctx,
                &run.intent.scope,
                &crate::protocol::data::ViewSelection::Pinned {
                    pin_id: run.pin_id.clone(),
                },
                read.stamp().now_ms,
            ) {
                Ok(selected) => Ok((selected.binding_key == run.binding_key).then_some(selected)),
                Err(Failure::Refused(_)) | Err(Failure::Storage(StorageError::ViewUnavailable)) => {
                    Ok(None)
                }
                Err(error) => Err(error.into_storage()),
            },
        )?;
    let operation_id = operation.id.clone();
    let operation_rev = operation.revision;
    let can_retry = run_write(
        store.store.as_ref(),
        WriteOptions::settlement(),
        move |tx| {
            let current = tx
                .operations()
                .operation(&operation_id)?
                .ok_or(StorageError::ScopeDenied)?;
            if current.revision != operation_rev || current.state != OperationState::OutcomeUnknown
            {
                return Err(StorageError::Busy);
            }
            let gate = Authorization::direct().fenced_by(work.authority.fences.clone());
            if !crate::services::fences::authorization_holds_in(tx, &ctx, &gate)?
                || tx.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
            {
                return Err(StorageError::ScopeDenied);
            }
            Selection::validate_saved_authority_in(tx, &ctx, &run)
                .map_err(Failure::into_storage)?;
            let now = tx.now_ms();
            let pin = tx.graph().graph_pin(&run.pin_id)?;
            let can_retry = if let (Some(selected), Some(pin)) = (&selected, &pin) {
                match super::super::lease::check_pin(pin, &ctx, &run.intent.scope, now)
                    .and_then(|_| selected.validate_in(tx, &ctx))
                {
                    Ok(()) => true,
                    Err(Failure::Refused(_))
                    | Err(Failure::Storage(StorageError::ViewUnavailable)) => false,
                    Err(error) => return Err(error.into_storage()),
                }
            } else {
                false
            };
            let mut next = run.clone();
            for result in resolved {
                let phase = phase_store::phase_mut(&mut next, result.phase)?;
                let Some(index) = phase.subjects.iter().position(|subject| {
                    subject.id == result.subject
                        && matches!(
                            subject.state,
                            OperationState::Dispatching | OperationState::OutcomeUnknown
                        )
                }) else {
                    return Err(StorageError::Busy);
                };
                match result.end {
                    Settled::Applied(output) => {
                        phase.subjects[index].output = Some(output);
                        phase.subjects[index].state = OperationState::ObservedComplete;
                        phase.subjects[index].diagnostic = None;
                    }
                    Settled::Failed => {
                        phase.subjects[index].state = OperationState::Failed;
                        phase.subjects[index].diagnostic = Some("ObservedModelFailure".into());
                    }
                    Settled::NotApplied => {
                        phase.subjects.remove(index);
                    }
                }
            }
            if !remaining {
                next.state = if can_retry {
                    OperationState::RetryPending
                } else {
                    OperationState::Failed
                };
                for phase in next.phases.iter_mut().filter(|phase| {
                    matches!(
                        phase.state,
                        OperationState::OutcomeUnknown | OperationState::Dispatching
                    )
                }) {
                    phase.state = next.state;
                    phase.diagnostic = (!can_retry).then(|| "AnalysisCaptureExpired".into());
                    phase.finished_at_ms = (!can_retry).then_some(now);
                }
                if !can_retry && let Some(pin) = pin {
                    tx.graph()
                        .retire_graph_pin(&pin.id, &pin.owner_namespace, pin.revision)?;
                }
            }
            phase_store::advance(tx, &run, &mut next)?;
            Ok(can_retry)
        },
    )?;
    Ok(if remaining {
        Execution::Unknown("some provider effects still require observation")
    } else if can_retry {
        Execution::NotApplied("remaining phases may resume from observed checkpoints")
    } else {
        Execution::Failed("analysis capture expired after provider outcome was observed")
    })
}

pub(super) fn process_batch(
    service: &AnalysisService,
    store: &PhaseStore,
    claim: &Arc<Claim>,
) -> Result<StoredAnalysisRun, Failure> {
    let phase = AnalysisPhase::SettleBatch;
    let run = store.begin(claim, phase, None)?;
    let candidates = read_snapshot(store.store.as_ref(), &ReadOptions::default(), |read| {
        Ok((|| -> Result<_, Failure> {
            claim.run(read)?;
            let mut candidates = Vec::new();
            for candidate in read
                .analysis()
                .analysis_runs(&run.binding_key, false, None, 64)?
            {
                if candidate.id == run.id
                    || run
                        .intent
                        .prerequisite_run
                        .as_ref()
                        .is_some_and(|id| candidate.id != *id)
                {
                    continue;
                }
                if let Some(op) = read.operations().operation(&candidate.active_operation)?
                    && op.state == OperationState::OutcomeUnknown
                    && op.payload["deliveryId"].as_str().is_some()
                {
                    let delivery_id = op.payload["deliveryId"]
                        .as_str()
                        .and_then(|id| Id::new(id).ok())
                        .ok_or_else(selection::unavailable)?;
                    if read.queue().entry(&delivery_id)?.is_some_and(|entry| {
                        entry.consumer == CONSUMER && entry.state == QueueState::Reconciling
                    }) {
                        candidates.push(op);
                    }
                }
            }
            Ok(candidates)
        })())
    })??;
    let mut observed = 0;
    let mut unknown = 0;
    for operation in candidates.into_iter().take(run.intent.batch_limit as usize) {
        let outcome = service.observe(&operation);
        let (state, evidence) = match outcome {
            Execution::Completed(evidence) => (OperationState::ObservedComplete, evidence),
            Execution::Failed(cause) => (OperationState::Failed, json!({"cause": cause})),
            Execution::NotApplied(cause) => (OperationState::RetryPending, json!({"cause": cause})),
            Execution::Unknown(_) => {
                unknown += 1;
                continue;
            }
        };
        let updated = run_write(
            store.store.as_ref(),
            WriteOptions::settlement(),
            move |tx| {
                let Some(current) = tx.operations().operation(&operation.id)? else {
                    return Ok(false);
                };
                if current.revision != operation.revision
                    || current.state != OperationState::OutcomeUnknown
                {
                    return Ok(false);
                }
                tx.operations().transition(
                    &operation.id,
                    current.revision,
                    None,
                    state,
                    Some(json!({"analysisObservation": evidence})),
                )?;
                Ok(true)
            },
        )?;
        observed += usize::from(updated);
    }
    let artifact = store.stage(&run, phase, &json!({
        "observed": observed, "unknown": unknown, "scope": run.intent.scope, "binding": run.binding_key,
    }))?;
    store.settle(
        claim,
        &run,
        phase,
        PhaseEvidence {
            state: OperationState::ObservedComplete,
            attempt: None,
            diagnostic: (unknown > 0).then_some("ModelOutcomeUnknown"),
        },
        Some(&artifact),
        Vec::new(),
    )
}
