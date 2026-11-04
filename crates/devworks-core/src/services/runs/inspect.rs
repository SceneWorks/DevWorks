//! The run inspection read model (CONTROL-01, CONTROL-07): everything the work detail view and
//! the coordinator's tools need to see why a root is where it is, read from one snapshot. The
//! wire `runs.inspect` result ([`super::root_snapshot`]) is its contract projection; transcripts
//! and tool calls are read with `sessions.transcript` and the history operations for the listed
//! sessions, and owned process output through the process owner for the listed trees.
use super::{ControlService, active_blockers, control_of, root_assignments};
use crate::context::InvocationContext;
use crate::ports::storage::{
    ActivationFilter, ActivationOutcome, ActivationPhase, BlockerFilter, DesiredExecution,
    HoldCause, OperationRecord, ProcessFilter, ReadSnapshot, StoredActivation, StoredAssignment,
    StoredBlocker, StoredInstruction, StoredProcess, StoredRootControl, WorkRoot,
};
use crate::protocol::{Id, RootId};
use crate::services::sessions::Failure;
use serde_json::Value;

/// Most activations an inspection lists (newest first).
pub const LISTED_ACTIVATIONS: u16 = 20;

/// Why the root is where it is, in the terms the user sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunCondition {
    /// Paused by the user, a stop, a restart or a budget latch: Continue is required.
    Paused,
    /// A cause-specific blocker (approval, dependency, reconciliation, credentials, policy or an
    /// escalation) prevents progress; Continue does not clear it.
    Blocked,
    /// Waiting for capacity until its ready time.
    CapacityWait,
    /// Waiting for a provider rate limit until its ready time.
    RateLimited,
    /// Accepted, not launched yet (a retry may be scheduled).
    Starting,
    Running,
    /// A stop was requested and the work has not stopped yet.
    Stopping,
    /// The last activation failed; its reason is kept.
    Failed,
    /// The last activation completed. Not evidence that the work itself is complete.
    Completed,
    /// Nothing is active.
    Idle,
}

#[derive(Clone, Debug)]
pub struct RunInspection {
    pub root: WorkRoot,
    pub control: StoredRootControl,
    pub condition: RunCondition,
    /// The inspectable reason of `condition`, when there is one.
    pub reason: Option<String>,
    /// Newest first.
    pub activations: Vec<StoredActivation>,
    /// Every assignment of the root (coordinator and descendants) with its session.
    pub assignments: Vec<StoredAssignment>,
    /// Every active blocker within the root.
    pub blockers: Vec<StoredBlocker>,
    /// Every redirect instruction with its stage, in order.
    pub instructions: Vec<StoredInstruction>,
    /// The root's owned process trees not yet verified quiesced.
    pub processes: Vec<StoredProcess>,
    /// The latest journaled stop.
    pub stop: Option<OperationRecord>,
}
impl RunInspection {
    /// The sessions whose transcripts and tool calls belong to this run.
    pub fn sessions(&self) -> Vec<Id> {
        let mut out: Vec<Id> = self
            .assignments
            .iter()
            .map(|a| a.session_id.clone())
            .collect();
        out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        out.dedup();
        out
    }
}

fn wait_kind(b: &StoredBlocker) -> Option<&str> {
    b.clearance.get("kind").and_then(Value::as_str)
}

/// Classifies the root's condition from its records.
pub fn condition_of(
    control: &StoredRootControl,
    root_level: &[StoredBlocker],
    activations: &[StoredActivation],
    now_ms: i64,
) -> (RunCondition, Option<String>) {
    let reasons = |pick: &dyn Fn(&StoredBlocker) -> bool| -> Option<String> {
        let r: Vec<&str> = root_level
            .iter()
            .filter(|b| pick(b))
            .map(|b| b.reason.as_str())
            .collect();
        (!r.is_empty()).then(|| r.join("; "))
    };
    if control.desired == DesiredExecution::Paused
        || root_level.iter().any(|b| b.cause.manual_resume())
    {
        return (
            RunCondition::Paused,
            reasons(&|b| b.cause.manual_resume()).or(Some("paused".into())),
        );
    }
    if let Some(b) = root_level.iter().find(|b| b.cause == HoldCause::Capacity) {
        let kind = if wait_kind(b) == Some("RateLimited") {
            RunCondition::RateLimited
        } else {
            RunCondition::CapacityWait
        };
        return (kind, Some(b.reason.clone()));
    }
    if !root_level.is_empty() {
        return (RunCondition::Blocked, reasons(&|_| true));
    }
    let latest = activations.first();
    match latest {
        Some(a) if a.phase == ActivationPhase::Pending => (
            RunCondition::Starting,
            (a.ready_at_ms > now_ms)
                .then(|| {
                    format!(
                        "retry {} scheduled{}",
                        a.attempts,
                        a.detail
                            .as_ref()
                            .map_or(String::new(), |d| format!(": {d}"))
                    )
                })
                .or(Some("accepted, waiting to launch".into())),
        ),
        Some(a) if matches!(a.phase, ActivationPhase::Claimed | ActivationPhase::Running) => {
            (RunCondition::Running, None)
        }
        Some(a) if a.phase == ActivationPhase::Stopping => (
            RunCondition::Stopping,
            Some(match a.stop_acknowledged_at_ms {
                Some(_) => "stop acknowledged; the work has not stopped yet".into(),
                None => "stop requested; not yet acknowledged".into(),
            }),
        ),
        Some(a) if a.outcome == Some(ActivationOutcome::Failed) => {
            (RunCondition::Failed, a.detail.clone())
        }
        Some(a) if a.outcome == Some(ActivationOutcome::Completed) => {
            (RunCondition::Completed, a.detail.clone())
        }
        _ => (RunCondition::Idle, None),
    }
}

impl ControlService {
    /// The full inspection of a visible run root (see the module documentation).
    pub fn inspection(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        root_id: &RootId,
        now_ms: i64,
    ) -> Result<RunInspection, Failure> {
        let root = super::controlled_root(snapshot.sessions(), snapshot.runs(), ctx, root_id)?;
        let runs = snapshot.runs();
        let control = control_of(runs, &root.id)?;
        let activations = runs.activations(
            &ActivationFilter::Root(root.id.clone()),
            None,
            LISTED_ACTIVATIONS,
        )?;
        let blockers = active_blockers(runs, &BlockerFilter::Root(root.id.clone()))?;
        let root_level: Vec<StoredBlocker> = blockers
            .iter()
            .filter(|b| b.assignment_id.is_none() && b.operation_id.is_none())
            .cloned()
            .collect();
        let (condition, reason) = condition_of(&control, &root_level, &activations, now_ms);
        let mut instructions = Vec::new();
        loop {
            let after = instructions.last().map_or(0, |i: &StoredInstruction| i.seq);
            let page = runs.instructions(&root.id, after, 500)?;
            let more = page.len() == 500;
            instructions.extend(page);
            if !more {
                break;
            }
        }
        let mut processes = Vec::new();
        let mut after: Option<Id> = None;
        loop {
            let page =
                snapshot
                    .resources()
                    .processes(&ProcessFilter::Unsettled, after.as_ref(), 500)?;
            let more = page.len() == 500;
            after = page.last().map(|p| p.id.clone());
            processes.extend(
                page.into_iter()
                    .filter(|p| p.root_id.as_ref() == Some(&root.id)),
            );
            if !more {
                break;
            }
        }
        let stop = match &control.stop_operation_id {
            Some(op) => snapshot.operations().operation(op)?,
            None => None,
        };
        Ok(RunInspection {
            assignments: root_assignments(runs, &root.id, false)?,
            root,
            control,
            condition,
            reason,
            activations,
            blockers,
            instructions,
            processes,
            stop,
        })
    }
}
