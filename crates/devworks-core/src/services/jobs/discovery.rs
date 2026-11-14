//! What a run's executor records while it works (JOB-06, JOB-09): discovery checkpoints, action
//! identities, the work roots it started and its usage. A scheduled agent's tools (for example a
//! GitHub new-issue scan, DW-27/DW-28) and the inference pipeline (DW-16/DW-18) call these inside
//! their own units of work; nothing here performs an external effect.
//!
//! * **A scan** ([`scan`]) claims an identity for every item it found and advances its source's
//!   checkpoint in the same transaction, so the checkpoint never moves past an item whose action
//!   was not durably claimed, and a repeated scan of the same items (a retry, an overlapping
//!   cursor, a resumed run) finds their identities instead of acting again.
//! * **An action** is claimed before its effect ([`claim_action`]) and settled after it
//!   ([`settle_action`]). An identity already `Done` is never repeated; one still `Intent` (it may
//!   or may not have happened, for example the app closed in between) or `Unknown` must be
//!   reconciled against the external system before any retry; one that certainly `Failed` may be
//!   claimed again.
//! * **Usage** of the run's own discovery and orchestration requests is attributed to the run and
//!   reported without any job spending limit; feature work the run starts is attributed to its
//!   feature by the router and keeps that feature's budget (it is linked here, never charged
//!   twice).
//!
//! Every write is fenced by the run: it must be active and owned by this boot, so a late task of
//! a finished, cancelled or superseded run can never record into it or a later run.
use crate::ports::storage::{
    StorageError, StoredJobAction, StoredJobCheckpoint, StoredJobRun, StoredJobUsage,
    WriteTransaction,
};
use crate::protocol::{Id, RootId};
use serde_json::{Value, json};

/// What a claim found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionClaim {
    /// Newly claimed (or re-claimed after a certain failure): perform the action, then settle.
    New(StoredJobAction),
    /// Already applied: never repeat it.
    AlreadyDone(StoredJobAction),
    /// It may have been applied: reconcile against the external system first.
    NeedsReconciliation(StoredJobAction),
}

/// How an action ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionOutcome {
    Done,
    /// Certainly not applied (safe to claim again).
    Failed,
    /// Could not be established; it needs a person or a later reconciliation.
    Unknown,
}
impl ActionOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Done => "Done",
            Self::Failed => "Failed",
            Self::Unknown => "Unknown",
        }
    }
}

fn lost(run: &Id) -> StorageError {
    StorageError::LeaseLost {
        kind: "job_run",
        id: run.as_str().to_owned(),
    }
}

/// The run, if it is active and owned by this boot.
pub fn owned_run(tx: &mut dyn WriteTransaction, run: &Id) -> Result<StoredJobRun, StorageError> {
    let boot = tx.store().boot_id.clone();
    let r = tx.jobs().job_run(run)?.ok_or_else(|| lost(run))?;
    if !super::is_active(&r.status) || r.boot_id.is_none() || r.boot_id != boot {
        return Err(lost(run));
    }
    Ok(r)
}

/// Claims an action identity of the run's job (see the module documentation).
pub fn claim_action(
    tx: &mut dyn WriteTransaction,
    run: &Id,
    key: &str,
) -> Result<ActionClaim, StorageError> {
    let r = owned_run(tx, run)?;
    let now = tx.now_ms();
    match tx.jobs().action(&r.job_id, key)? {
        Some(a) if a.state == "Done" => Ok(ActionClaim::AlreadyDone(a)),
        Some(a) if a.state == "Failed" => {
            let revision = a.revision;
            let again = tx.jobs().update_action(
                &StoredJobAction {
                    run_id: r.id.clone(),
                    state: "Intent".into(),
                    effect: None,
                    settled_at_ms: None,
                    ..a
                },
                revision,
            )?;
            Ok(ActionClaim::New(again))
        }
        Some(a) => Ok(ActionClaim::NeedsReconciliation(a)),
        None => Ok(ActionClaim::New(tx.jobs().insert_action(
            &StoredJobAction {
                job_id: r.job_id.clone(),
                key: key.to_owned(),
                run_id: r.id.clone(),
                state: "Intent".into(),
                effect: None,
                revision: 1,
                created_at_ms: now,
                settled_at_ms: None,
            },
        )?)),
    }
}

/// Settles a claimed action at its `expected_revision` with what it produced (or how it was
/// reconciled). A `Done` action never changes again.
pub fn settle_action(
    tx: &mut dyn WriteTransaction,
    run: &Id,
    key: &str,
    expected_revision: i64,
    outcome: ActionOutcome,
    effect: Value,
) -> Result<StoredJobAction, StorageError> {
    let r = owned_run(tx, run)?;
    let now = tx.now_ms();
    let a = tx
        .jobs()
        .action(&r.job_id, key)?
        .ok_or_else(|| StorageError::NotFound {
            kind: "job_action",
            id: key.to_owned(),
        })?;
    tx.jobs().update_action(
        &StoredJobAction {
            run_id: r.id.clone(),
            state: outcome.as_str().into(),
            effect: Some(effect.to_string()),
            settled_at_ms: Some(now),
            ..a
        },
        expected_revision,
    )
}

/// One scan of a source: the items found claim their action identities and the source's
/// checkpoint advances to `cursor`, atomically. `expected_checkpoint` is the revision the scan
/// started from (`None`: the source's first scan); a checkpoint that moved meanwhile is a
/// conflict and nothing is claimed.
pub fn scan(
    tx: &mut dyn WriteTransaction,
    run: &Id,
    source: &str,
    expected_checkpoint: Option<i64>,
    cursor: Value,
    keys: &[String],
) -> Result<(Vec<(String, ActionClaim)>, StoredJobCheckpoint), StorageError> {
    let r = owned_run(tx, run)?;
    let mut claims = Vec::with_capacity(keys.len());
    for k in keys {
        claims.push((k.clone(), claim_action(tx, run, k)?));
    }
    let now = tx.now_ms();
    let checkpoint = tx.jobs().put_checkpoint(
        &StoredJobCheckpoint {
            job_id: r.job_id.clone(),
            source: source.to_owned(),
            cursor: cursor.to_string(),
            revision: expected_checkpoint.unwrap_or(0) + 1,
            run_id: r.id.clone(),
            updated_at_ms: now,
        },
        expected_checkpoint,
    )?;
    Ok((claims, checkpoint))
}

/// The run's actions that may have been applied but were never settled: what a resumed run
/// reconciles before it retries anything.
pub fn unsettled(
    tx: &mut dyn WriteTransaction,
    run: &Id,
) -> Result<Vec<StoredJobAction>, StorageError> {
    tx.jobs().unsettled_actions(run)
}

/// Links a work root the run started or dispatched (its own feature budget and approvals stay in
/// force; the link is provenance only). The root must be in the run's project when it has one.
pub fn link_root(
    tx: &mut dyn WriteTransaction,
    run: &Id,
    root: &RootId,
) -> Result<(), StorageError> {
    let r = owned_run(tx, run)?;
    let work = tx
        .sessions()
        .work_root(root)?
        .ok_or_else(|| StorageError::NotFound {
            kind: "root",
            id: root.0.as_str().to_owned(),
        })?;
    // Work of the project the run acts on (its job's project, or the one an app-level task
    // targets), never of another.
    if super::run_target(&r).is_some_and(|p| p != work.project_id) {
        return Err(StorageError::ScopeDenied);
    }
    tx.jobs().link_root(run, root)?;
    let actor = tx.store().host_actor.clone();
    super::event(
        tx,
        &actor,
        Some(&work.project_id),
        r.operation_id.as_ref(),
        (super::RUN_AGGREGATE, r.id.as_str(), r.revision),
        "JobRunLinkedWork",
        json!({"rootId": root.0.as_str()}),
    )
}

/// Records one attributed usage item of the run's own requests (usage only: there is no job
/// spending limit or monetary hold, JOB-09).
pub fn record_usage(
    tx: &mut dyn WriteTransaction,
    usage: &StoredJobUsage,
) -> Result<StoredJobUsage, StorageError> {
    owned_run(tx, &usage.run_id)?;
    let recorded = tx.jobs().prepare_usage(usage)?;
    crate::services::usage::ingest::job_usage(tx, &recorded)?;
    Ok(recorded)
}

/// Saves the run's progress checkpoint (an agent session checkpoint), fenced by the run. Progress
/// does not advance the run's revision, so a cancellation addressed to the run stays current.
pub fn save_progress(
    tx: &mut dyn WriteTransaction,
    run: &Id,
    checkpoint: Value,
) -> Result<(), StorageError> {
    owned_run(tx, run)?;
    tx.jobs().set_run_checkpoint(run, &checkpoint.to_string())
}
