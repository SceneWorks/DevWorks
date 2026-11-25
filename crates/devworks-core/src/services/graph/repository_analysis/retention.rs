//! Bounded retention of report snapshots, their active references and their artifacts.
use super::{commands::scope_key, phase_store::PhaseStore, selection};
use crate::ports::storage::*;
use crate::protocol::OperationState;
use serde_json::json;
use std::collections::BTreeSet;

const PAGE: u16 = 16;
const WINDOW: i64 = 90 * 86_400_000;
const SUPERSEDED: i64 = 7 * 86_400_000;
const TREND_POINTS: usize = 12;
const FAILED_RUNS: usize = 12;

pub(super) fn run(
    store: &PhaseStore,
    after: Option<&AnalysisRunPosition>,
    stopped: &dyn Fn() -> bool,
) -> Result<Option<AnalysisRunPosition>, StorageError> {
    let page = read_snapshot(store.store.as_ref(), &ReadOptions::default(), |read| {
        read.analysis().analysis_retention_page(after, PAGE)
    })?;
    let next = if page.len() == usize::from(PAGE) {
        page.last().map(|run| AnalysisRunPosition {
            created_at_ms: run.created_at_ms,
            id: run.id.clone(),
        })
    } else {
        None
    };
    for prior in page {
        if stopped() {
            return Err(StorageError::Cancelled);
        }
        let artifacts = store.artifacts.clone();
        let result = run_write(
            store.store.as_ref(),
            WriteOptions::host_state(),
            move |tx| {
                let Some(mut run) = tx.analysis().analysis_run(&prior.id)? else {
                    return Ok(());
                };
                if run.revision != prior.revision {
                    return Ok(());
                }
                if !run.terminal() {
                    sync_terminal(tx, &mut run)?;
                    return Ok(());
                }
                if !prunable(tx, &run)? {
                    return Ok(());
                }
                let reports: Vec<_> = run
                    .phases
                    .iter()
                    .filter_map(|phase| phase.report.as_ref())
                    .collect();
                for report in &reports {
                    if !artifacts.analysis_report_removable(tx, report, &run.id)? {
                        return Ok(());
                    }
                }
                for report in reports {
                    artifacts.retire_analysis_report(tx, report, &run.id)?;
                }
                tx.analysis()
                    .remove_terminal_analysis_run(&run.id, run.revision)
            },
        );
        match result {
            Ok(()) | Err(StorageError::Busy) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(next)
}

fn sync_terminal(
    tx: &mut dyn WriteTransaction,
    run: &mut StoredAnalysisRun,
) -> Result<(), StorageError> {
    let Some(operation) = tx.operations().operation(&run.active_operation)? else {
        return Ok(());
    };
    if !matches!(
        operation.state,
        OperationState::Failed | OperationState::CancelledBeforeDispatch
    ) || run.phases.iter().any(|phase| {
        phase.subjects.iter().any(|subject| {
            matches!(
                subject.state,
                OperationState::Dispatching | OperationState::OutcomeUnknown
            )
        })
    }) {
        return Ok(());
    }
    let previous = run.clone();
    run.state = operation.state;
    for phase in run
        .phases
        .iter_mut()
        .filter(|phase| phase.state != OperationState::ObservedComplete)
    {
        phase.state = operation.state;
        phase
            .diagnostic
            .get_or_insert("AnalysisExecutionStopped".into());
    }
    super::phase_store::advance(tx, &previous, run)
}

fn prunable(tx: &mut dyn WriteTransaction, run: &StoredAnalysisRun) -> Result<bool, StorageError> {
    let Some(operation) = tx.operations().operation(&run.active_operation)? else {
        return Ok(false);
    };
    if !is_terminal(operation.state)
        || tx
            .analysis()
            .analysis_active_reference(scope_key(&run.intent.scope), &run.id)?
        || tx
            .graph()
            .graph_pin(&run.pin_id)?
            .is_some_and(|pin| pin.expires_at_ms > tx.now_ms())
    {
        return Ok(false);
    }
    let now = tx.now_ms();
    if run
        .phases
        .iter()
        .any(|phase| phase.diagnostic.as_deref() == Some("SupersededByIndexPublication"))
    {
        return Ok(now.saturating_sub(run.updated_at_ms) >= SUPERSEDED);
    }

    let reports: BTreeSet<_> = run
        .phases
        .iter()
        .filter(|phase| phase.state == OperationState::ObservedComplete && phase.report.is_some())
        .map(|phase| phase.phase.key())
        .collect();
    if !reports.is_empty() && is_current(tx, run)? {
        let newer = tx
            .analysis()
            .analysis_runs(&run.binding_key, false, None, 64)?;
        if reports.iter().any(|kind| {
            !newer
                .iter()
                .take_while(|candidate| candidate.id != run.id)
                .any(|candidate| replacement(candidate, run, kind))
        }) {
            return Ok(false);
        }
    }
    if now.saturating_sub(run.updated_at_ms) >= WINDOW {
        return Ok(true);
    }
    let prior = tx
        .analysis()
        .analysis_runs(&run.lineage_key, true, None, 128)?;
    if reports.is_empty() {
        return Ok(prior
            .iter()
            .take_while(|row| row.id != run.id)
            .filter(|row| row.terminal() && !row.phases.iter().any(|phase| phase.report.is_some()))
            .take(FAILED_RUNS)
            .count()
            >= FAILED_RUNS);
    }
    let point = snapshot_key(run);
    for kind in reports {
        let mut points = BTreeSet::new();
        let mut retained = false;
        for candidate in &prior {
            if now.saturating_sub(candidate.updated_at_ms) >= WINDOW
                || !replacement(candidate, run, kind)
            {
                continue;
            }
            let snapshot = snapshot_key(candidate);
            if points.insert(snapshot.clone()) && snapshot == point && candidate.id == run.id {
                retained = true;
                break;
            }
            if points.len() == TREND_POINTS {
                break;
            }
        }
        if retained {
            return Ok(false);
        }
    }
    Ok(true)
}

fn replacement(candidate: &StoredAnalysisRun, original: &StoredAnalysisRun, kind: &str) -> bool {
    (!matches!(kind, "links" | "communities")
        || candidate.intent.node_ids == original.intent.node_ids)
        && candidate.phases.iter().any(|phase| {
            phase.phase.key() == kind
                && phase.state == OperationState::ObservedComplete
                && phase.report.is_some()
        })
}

fn snapshot_key(run: &StoredAnalysisRun) -> String {
    selection::digest(&json!(
        run.intent
            .views
            .iter()
            .map(|binding| &binding.snapshot)
            .collect::<Vec<_>>()
    ))
}

fn is_current(
    tx: &mut dyn WriteTransaction,
    run: &StoredAnalysisRun,
) -> Result<bool, StorageError> {
    let now = tx.now_ms();
    for view in &run.intent.views {
        let Some(saved) = tx.indexing().view(&view.view_id)? else {
            return Ok(false);
        };
        if saved.view_digest != view.view_digest.as_str() {
            return Ok(false);
        }
        if let Some(witness) = run
            .intent
            .witnesses
            .iter()
            .find(|proof| proof.view_id == view.view_id)
            && let Some(overlay_id) = &witness.overlay_id
        {
            if witness.expires_at_ms.is_none_or(|until| until <= now)
                || tx
                    .indexing()
                    .overlay(overlay_id)?
                    .is_none_or(|row| row.view_id.as_ref() != Some(&view.view_id))
            {
                return Ok(false);
            }
        } else if tx
            .indexing()
            .published(&view.repository_id)?
            .is_none_or(|published| published.view_id != view.view_id)
        {
            return Ok(false);
        }
    }
    Ok(true)
}
