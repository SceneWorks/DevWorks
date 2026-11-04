//! The scheduler's transactional steps (JOB-02..09, CONTROL-05/08/09/10/11): occurrence claims,
//! run execution, cancellation, interrupted-run reconciliation and the boot/shutdown and project
//! lifecycle hooks. The engine's scheduler drives these on blocking threads; each step is one
//! unit of work on the write owner, so every limit and overlap check commits with its effect.
//!
//! # Claims
//!
//! [`JobService::claim_due`] takes the due jobs (enabled, unheld, live, next occurrence at or
//! before now). For each, in one transaction: a job of a restored store epoch is held
//! `ReactivationRequired` and a job of a closed project `ProjectClosed` (nothing runs); an
//! occurrence older than [`super::JobConfig::due_grace_ms`] is **missed** (recorded once with its
//! count, never run late); otherwise the occurrence is recorded — `Queued` with its journal intent,
//! or `Skipped` while another run is active (JOB-08) — and every later occurrence already past is
//! missed too (no coalescing). The next occurrence is always strictly in the future.
//!
//! # Execution
//!
//! [`JobService::begin`] starts a queued run (`Running`, attempt `Preflighted`, the agent
//! principal of an inference run bound), [`JobService::dispatching`] moves the attempt to
//! `Dispatching` immediately before the effect, and the settle steps record the observed outcome.
//! A failure before the effect is recorded as such (`Failed`, never dispatched); a failure after
//! it is `OutcomeUnknown` unless the effect's own receipt resolves it. Native runs submit one
//! command whose request key is the run's identity, so a resubmission after a lost acknowledgment
//! or a restart is resolved by the gateway's receipt (it never applies twice). A start whose
//! executor is gone (its task ended after the start committed) is reclaimed from what is known of
//! its effect ([`JobService::reclaim`]), never left running with nothing executing it.
//!
//! # Interruption, restart and sleep
//!
//! At shutdown ([`JobLifecycle::quiesce`]) active runs are marked interrupted on their job. At the
//! next boot ([`JobLifecycle::recover`], after run-root recovery) each run left active by an
//! earlier boot — interrupted or crashed — is reconciled before anything retries: a run never
//! dispatched resumes; a dispatched native run is settled from its command's receipt when there is
//! one and resumes only when there is none; a dispatched inference run resumes its session and
//! checkpoint, its actions guarded by their identities ([`super::discovery`]). The run keeps its
//! identity, progress, history and any separate hold, and its resumption count advances; when its
//! job, project or restore epoch no longer permits it, it ends instead. A run that was holding
//! keeps its hold and is reattached by its next start: its session is handed to the pipeline
//! again, still held, so clearing the hold continues it. Then every enabled job's
//! next occurrence is recomputed from now with missed ones recorded, never caught up. Nothing
//! here touches a feature or issue root: their restart holds stay until a manual Continue
//! (CONTROL-10). Sleep and wake need nothing: runs are not leased, so they continue, and an
//! occurrence slept through is missed by the grace rule (CONTROL-11).
//!
//! # Projects and restore
//!
//! `projects.archive` calls [`project_archived`] in its unit of work (after
//! `ControlService::stop_project`): every enabled job whose current definition acts on the
//! project is held `ProjectClosed`, and every active run whose own snapshot acts on it is asked
//! to stop, at once (a run keeps the target it was queued with when its job is retargeted). `projects.restore` calls [`project_restored`]: those
//! holds become `ReactivationRequired`, so the restored project's schedules stay stopped until a
//! person enables each. Independently of both calls, a job records the project's archive epoch it
//! was activated under and a run the epochs it was authorized under (a person's Run now: the
//! current ones; anything else: its job's): a claim, start or dispatch under a
//! later epoch (the project was archived since, even if already restored) holds the job and never
//! dispatches the run, so no stale run of an archived epoch can act after a restore. A restored backup (a later store restore epoch)
//! holds every job activated under an earlier epoch at boot and at claim (STORE-13).
use super::{
    JOB_AGGREGATE, JobService, RUN_AGGREGATE, counter, event, job_target, parse, project_closed,
    run_operation, run_target, stored_plan, undispatched,
};
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::GatewayFailure;
use crate::ports::IdKind;
use crate::ports::storage::{
    AttemptBinding, AttemptFence, JobScope, LedgerEntry, NewPrincipalBinding, OperationRecord,
    RunFilter, StorageError, StoredJob, StoredJobRun, WriteTransaction, is_terminal,
};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{EffortName, JobDefinition, RouteIntent};
use crate::protocol::results::{DomainCommandReply, DomainDecision};
use crate::protocol::{
    ActorId, CommandReply, Counter, Id, OperationId, OperationState, ProjectId, Rejection,
    RequestKey, Revision, RootId,
};
use crate::services::runs::recovery::LifecycleHooks;
use crate::settings::catalog::AgentRole;
use serde_json::{Value, json};
use std::sync::Arc;

const PAGE: u16 = 200;
/// Pages of due jobs one claim transaction processes at most.
const MAX_CLAIM_PAGES: usize = 20;

/// What an occurrence is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Occurrence {
    /// A scheduled instant (UTC ms).
    Scheduled { at_ms: i64 },
    /// Run now, requested at `at_ms`.
    Manual { at_ms: i64 },
}

fn host(tx: &mut dyn WriteTransaction) -> ActorId {
    tx.store().host_actor.clone()
}
fn reason_text(s: &str) -> String {
    s.chars().take(4096).collect()
}

/// Records an occurrence of `job`: `Queued` with its journal intent, or `Skipped` (nothing
/// queued, no operation) while another run of the job is active.
///
/// The run is authorized under the epochs it records: a person's Run now (`by_person`)
/// authorizes it in the current restore and project-archive epochs; a scheduled occurrence or
/// another principal's Run now inherits the epochs its job was activated under, so a run of a job
/// activated before a backup restore or an archival never dispatches ([`dispatch_fence`]).
pub fn queue_run(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    job: &StoredJob,
    occurrence: Occurrence,
    by_person: bool,
) -> Result<StoredJobRun, StorageError> {
    let now = tx.now_ms();
    let (restore_epoch, project_epoch) = if by_person {
        (
            tx.store().restore_epoch,
            super::project_epoch(tx, job_target(job).as_ref())?,
        )
    } else {
        (job.activated_epoch, job.project_epoch)
    };
    let run_id = tx.new_id(IdKind::JobRun);
    let (key, at_ms, trigger) = match occurrence {
        Occurrence::Scheduled { at_ms } => {
            (super::occurrence::occurrence_key(at_ms), at_ms, "Scheduled")
        }
        Occurrence::Manual { at_ms } => (format!("manual-{}", run_id.as_str()), at_ms, "Manual"),
    };
    let base = StoredJobRun {
        id: run_id.clone(),
        job_id: job.id.clone(),
        seq: 0,
        project_id: job.project_id.clone(),
        job_revision: job.revision,
        project_epoch,
        restore_epoch,
        definition: job.definition.clone(),
        occurrence_key: key,
        occurrence_at_ms: at_ms,
        trigger: trigger.into(),
        status: "Queued".into(),
        revision: 1,
        operation_id: None,
        inner_operation_id: None,
        boot_id: None,
        started_at_ms: None,
        finished_at_ms: None,
        reason: None,
        hold: None,
        cancel_requested: false,
        resumptions: 0,
        session_id: None,
        agent_actor: None,
        skipped_for: None,
        checkpoint: None,
        created_at_ms: now,
    };
    if let Some(active) = tx.jobs().active_run(&job.id)? {
        let skipped = tx.jobs().insert_run(&StoredJobRun {
            status: "Skipped".into(),
            finished_at_ms: Some(now),
            reason: Some(format!(
                "The previous run ({}) is still active",
                active.id.as_str()
            )),
            skipped_for: Some(active.id.clone()),
            ..base
        })?;
        event(
            tx,
            actor,
            job.project_id.as_ref(),
            None,
            (RUN_AGGREGATE, skipped.id.as_str(), skipped.revision),
            "JobOccurrenceSkipped",
            json!({"jobId": job.id.as_str(), "occurrenceAtMs": at_ms, "trigger": trigger, "activeRunId": active.id.as_str()}),
        )?;
        return Ok(skipped);
    }
    let op = OperationId(tx.new_id(IdKind::Operation));
    tx.operations()
        .record_intent(run_operation(op.clone(), job, &run_id))?;
    let run = tx.jobs().insert_run(&StoredJobRun {
        operation_id: Some(op.clone()),
        ..base
    })?;
    event(
        tx,
        actor,
        job.project_id.as_ref(),
        Some(&op),
        (RUN_AGGREGATE, run.id.as_str(), run.revision),
        "JobRunQueued",
        json!({"jobId": job.id.as_str(), "occurrenceAtMs": at_ms, "trigger": trigger}),
    )?;
    Ok(run)
}

/// Moves `op` to `to` from wherever it is, presenting its latest attempt's fence; `Ok(false)`
/// when the journal does not allow the transition from its current state.
fn transition(
    tx: &mut dyn WriteTransaction,
    op: &OperationId,
    to: OperationState,
    observation: Value,
) -> Result<bool, StorageError> {
    let Some(rec) = tx.operations().operation(op)? else {
        return Ok(false);
    };
    if rec.state == to {
        return Ok(true);
    }
    if !crate::ports::storage::transition_allowed(rec.state, to) {
        return Ok(false);
    }
    let fence = rec.latest_attempt().map(|a| a.fence());
    // An observation is attributed to an attempt; an operation that never started one (cancelled
    // or failed before any attempt) transitions without it.
    let observation = fence.as_ref().map(|_| observation);
    tx.operations()
        .transition(op, rec.revision, fence.as_ref(), to, observation)?;
    Ok(true)
}

/// Ends a run with `status` (and its operation with `op_state` when one is given and allowed).
fn finish(
    tx: &mut dyn WriteTransaction,
    run: &StoredJobRun,
    status: &str,
    op_state: Option<OperationState>,
    reason: Option<&str>,
    observation: Value,
) -> Result<StoredJobRun, StorageError> {
    if let (Some(op), Some(to)) = (&run.operation_id, op_state) {
        transition(tx, op, to, observation)?;
    }
    let now = tx.now_ms();
    let after = tx.jobs().update_run(
        &StoredJobRun {
            status: status.into(),
            finished_at_ms: Some(now),
            reason: reason.map(reason_text).or_else(|| run.reason.clone()),
            ..run.clone()
        },
        run.revision,
    )?;
    if let Some(job) = tx.jobs().job(&run.job_id)?
        && job.interrupted_run_id.as_ref() == Some(&run.id)
    {
        tx.jobs()
            .set_job_schedule(&job.id, job.revision, job.next_at_ms, None)?;
    }
    let actor = host(tx);
    event(
        tx,
        &actor,
        run.project_id.as_ref(),
        run.operation_id.as_ref(),
        (RUN_AGGREGATE, after.id.as_str(), after.revision),
        "JobRunFinished",
        json!({"jobId": run.job_id.as_str(), "status": status, "reason": after.reason}),
    )?;
    Ok(after)
}

/// Asks `run` to stop: one never dispatched is cancelled at once (its operation
/// `CancelledBeforeDispatch`); a dispatched one keeps running with its cancellation requested
/// until the scheduler records the verified outcome. Bound to the run's identity: it never
/// reaches a later run of the job.
pub fn request_cancel(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    run: &StoredJobRun,
    reason: &str,
) -> Result<StoredJobRun, StorageError> {
    if undispatched(tx.operations(), run)? && run.inner_operation_id.is_none() {
        let after = finish(
            tx,
            &StoredJobRun {
                cancel_requested: true,
                ..run.clone()
            },
            "Cancelled",
            Some(OperationState::CancelledBeforeDispatch),
            Some(reason),
            json!({"cancelled": true, "dispatched": false}),
        )?;
        return Ok(after);
    }
    let after = tx.jobs().update_run(
        &StoredJobRun {
            cancel_requested: true,
            ..run.clone()
        },
        run.revision,
    )?;
    event(
        tx,
        actor,
        run.project_id.as_ref(),
        run.operation_id.as_ref(),
        (RUN_AGGREGATE, after.id.as_str(), after.revision),
        "JobRunCancelRequested",
        json!({"jobId": run.job_id.as_str(), "reason": reason}),
    )?;
    Ok(after)
}

/// Holds an enabled job (nothing is scheduled until a person enables it again).
fn hold_job(
    tx: &mut dyn WriteTransaction,
    job: &StoredJob,
    hold: &str,
    why: &str,
) -> Result<StoredJob, StorageError> {
    if job.hold.as_deref() == Some(hold) && job.next_at_ms.is_none() {
        return Ok(job.clone());
    }
    let now = tx.now_ms();
    let after = tx.jobs().update_job(
        &StoredJob {
            hold: Some(hold.into()),
            next_at_ms: None,
            updated_at_ms: now,
            ..job.clone()
        },
        job.revision,
    )?;
    let actor = host(tx);
    event(
        tx,
        &actor,
        job.project_id.as_ref(),
        None,
        (JOB_AGGREGATE, after.id.as_str(), after.revision),
        "JobHeld",
        json!({"hold": hold, "reason": why}),
    )?;
    Ok(after)
}

/// Whether a job's stored schedule still evaluates in this build (its time zone may have left
/// the platform's database). One that does not stops scheduling, visibly, instead of failing
/// every claim and boot: its next occurrence is cleared and the reason recorded; editing or
/// enabling it again recomputes it.
fn evaluable(tx: &mut dyn WriteTransaction, job: &StoredJob) -> Result<bool, StorageError> {
    let Err(e) = stored_plan(job) else {
        return Ok(true);
    };
    if job.next_at_ms.is_some() {
        tx.jobs()
            .set_job_schedule(&job.id, job.revision, None, job.interrupted_run_id.as_ref())?;
        let actor = host(tx);
        event(
            tx,
            &actor,
            job.project_id.as_ref(),
            None,
            (JOB_AGGREGATE, job.id.as_str(), job.revision),
            "JobScheduleUnavailable",
            json!({"reason": format!("{e:?}"), "timezone": job.timezone}),
        )?;
    }
    Ok(false)
}

/// Records missed occurrences in `[first, now]` (once, with their count) and returns the next
/// future occurrence.
fn miss(
    tx: &mut dyn WriteTransaction,
    job: &StoredJob,
    first: i64,
    now: i64,
) -> Result<Option<i64>, StorageError> {
    let plan = stored_plan(job)?;
    let (count, exact) = plan.count_between(job.anchor_at_ms, Some(first), first - 1, now);
    let next = plan.next_after(job.anchor_at_ms, now);
    if count > 0 {
        let actor = host(tx);
        event(
            tx,
            &actor,
            job.project_id.as_ref(),
            None,
            (JOB_AGGREGATE, job.id.as_str(), job.revision),
            "JobOccurrencesMissed",
            json!({"count": count, "exact": exact, "firstAtMs": first, "nextAtMs": next}),
        )?;
    }
    Ok(next)
}

/// Why a job may not dispatch now, as the hold it gets (`None`: it may).
pub(super) fn fence_of(
    tx: &mut dyn WriteTransaction,
    job: &StoredJob,
) -> Result<Option<(&'static str, &'static str)>, StorageError> {
    if job.activated_epoch < tx.store().restore_epoch {
        return Ok(Some((
            "ReactivationRequired",
            "the store was restored from a backup; enable the schedule to reactivate it",
        )));
    }
    if let Some(p) = &job_target(job) {
        if project_closed(tx.runs().project_lifecycle(p)?.as_deref()) {
            return Ok(Some((
                "ProjectClosed",
                "the project is archived or deleted",
            )));
        }
        // Archived since it was activated (and restored meanwhile): restore leaves schedules
        // stopped until a person reactivates them.
        if super::project_epoch(tx, Some(p))? > job.project_epoch {
            return Ok(Some((
                "ReactivationRequired",
                "the project was archived and restored; enable the schedule to reactivate it",
            )));
        }
    }
    if !job.authorized {
        return Ok(Some((
            "AuthorizationRequired",
            "a person has not authorized this schedule's current definition",
        )));
    }
    Ok(None)
}

/// Whether a run's project is still open under the archive epoch the run was authorized in (an
/// archival since, even one already restored, stopped it).
fn run_project_current(
    tx: &mut dyn WriteTransaction,
    run: &StoredJobRun,
) -> Result<bool, StorageError> {
    let Some(p) = &run_target(run) else {
        return Ok(true);
    };
    Ok(!project_closed(tx.runs().project_lifecycle(p)?.as_deref())
        && super::project_epoch(tx, Some(p))? == run.project_epoch)
}

/// Whether a run was authorized under an earlier restore epoch (the store was restored from a
/// backup since): its receipts and effects belong to the earlier epoch.
fn run_restored(tx: &mut dyn WriteTransaction, run: &StoredJobRun) -> Result<bool, StorageError> {
    Ok(run.restore_epoch < tx.store().restore_epoch)
}

/// Why a run may not start or dispatch now (`None`: it may): its project was archived since
/// the epoch it was authorized under, or the store was restored from a backup since.
fn dispatch_fence(
    tx: &mut dyn WriteTransaction,
    run: &StoredJobRun,
) -> Result<Option<&'static str>, StorageError> {
    if !run_project_current(tx, run)? {
        return Ok(Some(
            "the project was archived since the run was authorized",
        ));
    }
    if run_restored(tx, run)? {
        return Ok(Some(
            "the store was restored from a backup since the run was authorized",
        ));
    }
    Ok(None)
}

/// Submits a native run's command through the application gateway (the engine's runtime, behind
/// its admission fence). Called outside any transaction.
pub trait NativeJobDispatch: Send + Sync {
    fn command(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
    ) -> crate::gateway::CommandOutcome;
}

/// The executable step of a started run.
#[derive(Clone, Debug)]
pub enum Step {
    /// Submit `frame` for `operation` through the gateway with `ctx` (the host, in the job's
    /// scope; the request key is the run's identity).
    Native {
        ctx: InvocationContext,
        operation: &'static str,
        frame: Vec<u8>,
    },
    /// Hand the run to the inference pipeline.
    Inference(InferenceStart),
}

/// A started run, ready to dispatch.
#[derive(Clone, Debug)]
pub struct Begun {
    pub run: StoredJobRun,
    pub operation: OperationId,
    pub fence: AttemptFence,
    pub step: Step,
}

/// What a scheduled inference run hands the pipeline (DW-16 coordinator turns, DW-18 provider
/// transports). The agent acts through `ctx`: its own principal, bound by the host with the
/// `Scheduled` role and the full configured tool families of the job's scope, so ordinary
/// authority, plan readiness, pauses and feature budgets apply to everything it starts.
#[derive(Clone, Debug)]
pub struct InferenceStart {
    pub run_id: Id,
    pub job_id: Id,
    pub project_id: Option<ProjectId>,
    pub feature_id: Option<RootId>,
    pub prompt: ArtifactRef,
    pub route: RouteIntent,
    pub effort: EffortName,
    pub agent: ActorId,
    pub ctx: InvocationContext,
    /// A resumption of an interrupted run: continue `session_id` from `checkpoint`, reconciling
    /// the run's unsettled actions before any retry.
    pub resume: bool,
    pub session_id: Option<Id>,
    pub checkpoint: Option<String>,
    /// A separate hold the resumed run still waits on (a user pause, budget hold or approval
    /// wait): the session is reattached held, and continues once the pipeline reports the hold
    /// cleared ([`InferenceOutcome::Resumed`]).
    pub hold: Option<String>,
}

/// Why the inference pipeline did not take a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InferenceRefusal {
    /// Whether anything may already have been sent (a model request, a tool call).
    pub dispatched: bool,
    pub reason: String,
}

/// The scheduled-inference dispatch port. The engine's scheduler calls it outside any
/// transaction; it never runs inference itself.
pub trait InferenceJobs: Send + Sync {
    /// Whether a run can be handed over now; checked before the attempt is marked dispatched, so
    /// an unavailable pipeline is a failure *before* dispatch.
    fn availability(&self) -> Result<(), String>;
    /// Starts (or resumes) the run's agent once it has taken the run: `Ok` is that acceptance,
    /// with the session when the pipeline already has one (`Ok(None)`: taken, no session yet).
    /// Acceptance itself is what the run records as owned, never the session. The pipeline
    /// reports the outcome later through [`JobService::finish_inference`].
    fn start(&self, run: &InferenceStart) -> Result<Option<Id>, InferenceRefusal>;
    /// Asks a started run to stop; `Ok(true)` when it is verified stopped, `Ok(false)` while it is
    /// still stopping (the scheduler asks again; the run stays active until then).
    fn cancel(&self, run: &Id) -> Result<bool, String>;
}

/// Scheduled inference is not connected in this build: no coordinator turn pipeline (DW-16) and
/// no provider connection (DW-18). A run fails truthfully before dispatch; nothing is sent.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoInferencePipeline;
/// The visible reason of a scheduled inference run in a build without the pipeline.
pub const NO_PIPELINE: &str = "Scheduled inference is not available in this build: the coordinator turn pipeline and provider connections are not installed. Nothing was sent to a model.";
impl InferenceJobs for NoInferencePipeline {
    fn availability(&self) -> Result<(), String> {
        Err(NO_PIPELINE.into())
    }
    fn start(&self, _: &InferenceStart) -> Result<Option<Id>, InferenceRefusal> {
        Err(InferenceRefusal {
            dispatched: false,
            reason: NO_PIPELINE.into(),
        })
    }
    fn cancel(&self, _: &Id) -> Result<bool, String> {
        Ok(true)
    }
}

/// How a scheduled inference run ended, as its pipeline observed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InferenceOutcome {
    Succeeded,
    Failed(String),
    /// Stopped after its cancellation was requested.
    Cancelled,
    /// Waiting on a separate hold (a user pause, budget hold or approval wait).
    Holding(String),
    /// The hold cleared; the run continues.
    Resumed,
    /// What was applied cannot be established.
    Unknown(String),
}

/// How a cancellation's native part ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopVerdict {
    /// Everything of the run is verified stopped.
    Stopped,
    /// Something could not be verified stopped.
    Uncertain(String),
}

/// The ingress namespace of a job's native commands.
pub fn native_namespace(job: &Id) -> Id {
    Id::new(
        format!("job-{}", job.as_str())
            .chars()
            .take(128)
            .collect::<String>(),
    )
    .expect("job IDs are ID-safe")
}
/// The scheduled agent principal of an inference job.
pub fn agent_actor(job: &Id) -> ActorId {
    ActorId(
        Id::new(
            format!("job-agent-{}", job.as_str())
                .chars()
                .take(128)
                .collect::<String>(),
        )
        .expect("job IDs are ID-safe"),
    )
}
fn host_ctx(
    tx: &mut dyn WriteTransaction,
    namespace: Id,
    invocation: Id,
    project: Option<&ProjectId>,
) -> InvocationContext {
    let info = tx.store().clone();
    InvocationContext::new(
        info.host_actor,
        PrincipalKind::Host,
        invocation,
        namespace,
        info.store_id,
        counter(info.restore_epoch),
        Revision::new(1).expect("positive"),
    )
    .with_lineage(project.cloned(), None, None)
}

impl JobService {
    /// Claims the due occurrences of up to [`super::JobConfig::claim_batch`] jobs (see the module
    /// documentation). Returns the runs queued.
    pub fn claim_due(
        &self,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Vec<StoredJobRun>, StorageError> {
        let now = tx.now_ms();
        let actor = host(tx);
        let mut queued = Vec::new();
        // Every processed job's next occurrence moves past now (or it is held), so each page
        // holds new jobs; the page count bounds the transaction regardless.
        for _ in 0..MAX_CLAIM_PAGES {
            let page = tx.jobs().due_jobs(now, self.config.claim_batch)?;
            if page.is_empty() {
                break;
            }
            self.claim_page(tx, &actor, now, page, &mut queued)?;
        }
        Ok(queued)
    }

    fn claim_page(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        now: i64,
        page: Vec<StoredJob>,
        queued: &mut Vec<StoredJobRun>,
    ) -> Result<(), StorageError> {
        for job in page {
            if let Some((hold, why)) = fence_of(tx, &job)? {
                hold_job(tx, &job, hold, why)?;
                continue;
            }
            let Some(at) = job.next_at_ms else { continue };
            if !evaluable(tx, &job)? {
                continue;
            }
            let plan = stored_plan(&job)?;
            if now - at > self.config.due_grace_ms {
                let next = miss(tx, &job, at, now)?;
                tx.jobs().set_job_schedule(
                    &job.id,
                    job.revision,
                    next,
                    job.interrupted_run_id.as_ref(),
                )?;
                continue;
            }
            match queue_run(tx, actor, &job, Occurrence::Scheduled { at_ms: at }, false) {
                Ok(run) if run.status == "Queued" => queued.push(run),
                Ok(_) => {}
                // Already recorded (it cannot be claimed twice): only advance past it.
                Err(StorageError::Duplicate {
                    kind: "job_occurrence",
                    ..
                }) => {}
                Err(e) => return Err(e),
            }
            // Every later occurrence already past is missed: never coalesced into a catch-up.
            let mut next = plan.next_after(job.anchor_at_ms, at);
            if let Some(n) = next
                && n <= now
            {
                next = miss(tx, &job, n, now)?;
            }
            tx.jobs().set_job_schedule(
                &job.id,
                job.revision,
                next,
                job.interrupted_run_id.as_ref(),
            )?;
        }
        Ok(())
    }

    /// Runs ready to start, oldest first by ID: queued and unheld, or holding with no owner (an
    /// earlier boot's session to reattach, its hold kept); none with a cancellation requested.
    pub fn runnable(
        &self,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Vec<StoredJobRun>, StorageError> {
        let mut out = Vec::new();
        let mut after: Option<Id> = None;
        loop {
            let page = tx
                .jobs()
                .runs_where(&RunFilter::Active, after.as_ref(), PAGE)?;
            let more = page.len() == usize::from(PAGE);
            after = page.last().map(|r| r.id.clone());
            out.extend(
                page.into_iter()
                    .filter(|r| startable(r) && !r.cancel_requested),
            );
            if !more {
                return Ok(out);
            }
        }
    }

    /// Runs whose cancellation is requested and not yet settled.
    pub fn cancellations(
        &self,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Vec<StoredJobRun>, StorageError> {
        let mut out = Vec::new();
        let mut after: Option<Id> = None;
        loop {
            let page = tx
                .jobs()
                .runs_where(&RunFilter::CancelRequested, after.as_ref(), PAGE)?;
            let more = page.len() == usize::from(PAGE);
            after = page.last().map(|r| r.id.clone());
            out.extend(page);
            if !more {
                return Ok(out);
            }
        }
    }

    /// Starts a queued run (see the module documentation). `Ok(None)` when it is not startable
    /// (no longer queued, held, cancelled meanwhile) or ended here before dispatch.
    pub fn begin(
        &self,
        tx: &mut dyn WriteTransaction,
        run_id: &Id,
        inference: &dyn InferenceJobs,
    ) -> Result<Option<Begun>, StorageError> {
        let boot = tx
            .store()
            .boot_id
            .clone()
            .ok_or_else(|| StorageError::IncompatibleSchema("store is read-only".into()))?;
        let Some(run) = tx.jobs().job_run(run_id)? else {
            return Ok(None);
        };
        if !startable(&run) {
            return Ok(None);
        }
        // A holding run reattaches its interrupted session and keeps holding.
        let reattach = run.status == "Holding";
        let Some(op_id) = run.operation_id.clone() else {
            return Ok(None);
        };
        if run.cancel_requested {
            let actor = host(tx);
            request_cancel(tx, &actor, &run, "cancelled before it started")?;
            return Ok(None);
        }
        if let Some(why) = dispatch_fence(tx, &run)? {
            finish(
                tx,
                &run,
                "Cancelled",
                Some(OperationState::CancelledBeforeDispatch),
                Some(why),
                json!({"dispatched": false}),
            )?;
            return Ok(None);
        }
        let Some(rec) = tx.operations().operation(&op_id)? else {
            return Err(StorageError::InvalidRecord(
                "a queued run's operation is missing".into(),
            ));
        };
        if !matches!(
            rec.state,
            OperationState::IntentRecorded | OperationState::RetryPending
        ) {
            return Ok(None);
        }
        let attempt =
            tx.operations()
                .begin_attempt(&op_id, rec.revision, AttemptBinding::default())?;
        let fence = attempt.fence();
        let definition: JobDefinition = parse(&run.definition)?;
        let resume = run.resumptions > 0;
        let now = tx.now_ms();
        let started = tx.jobs().update_run(
            &StoredJobRun {
                status: if reattach { "Holding" } else { "Running" }.into(),
                started_at_ms: run.started_at_ms.or(Some(now)),
                boot_id: Some(boot),
                ..run.clone()
            },
            run.revision,
        )?;
        let fail_before = |tx: &mut dyn WriteTransaction, started: &StoredJobRun, why: &str| {
            finish(
                tx,
                started,
                "Failed",
                Some(OperationState::Failed),
                Some(why),
                json!({"dispatched": false, "reason": why}),
            )
        };
        let step = match definition {
            JobDefinition::Native {
                capability_id,
                arguments,
            } => {
                let Some(operation) = self.catalog.resolve(&capability_id) else {
                    fail_before(
                        tx,
                        &started,
                        "this capability is not available in this build; nothing ran",
                    )?;
                    return Ok(None);
                };
                let mut body = arguments.value.0.clone();
                if let Some(map) = body.as_object_mut() {
                    map.insert("kind".into(), json!(operation));
                }
                let frame = json!({
                    "protocol": crate::protocol::CONTRACT,
                    "mode": "Command",
                    "requestKey": run.id.as_str(),
                    "schemaRevision": arguments.schema.revision.get(),
                    "body": body,
                });
                let invocation = tx.new_id(IdKind::Correlation);
                Step::Native {
                    ctx: host_ctx(
                        tx,
                        native_namespace(&run.job_id),
                        invocation,
                        run.project_id.as_ref(),
                    ),
                    operation,
                    frame: frame.to_string().into_bytes(),
                }
            }
            JobDefinition::Inference {
                prompt,
                route,
                effort,
                project_id,
                feature_id,
            } => {
                if let Err(why) = inference.availability() {
                    fail_before(tx, &started, &why)?;
                    return Ok(None);
                }
                let scope = run.project_id.clone().or(project_id);
                let Some(agent) = self.bind_agent(tx, &run.job_id, scope.as_ref())? else {
                    fail_before(
                        tx,
                        &started,
                        "this job's scheduled agent is revoked; a person must rebind it before it \
                         runs again; nothing ran",
                    )?;
                    return Ok(None);
                };
                let info = tx.store().clone();
                let ctx = InvocationContext::new(
                    agent.clone(),
                    PrincipalKind::Agent,
                    run.id.clone(),
                    Id::new(
                        format!("job-run-{}", run.id.as_str())
                            .chars()
                            .take(128)
                            .collect::<String>(),
                    )
                    .expect("run IDs are ID-safe"),
                    info.store_id,
                    counter(info.restore_epoch),
                    Revision::new(1).expect("positive"),
                )
                .with_lineage(scope.clone(), None, None);
                let started = tx.jobs().update_run(
                    &StoredJobRun {
                        agent_actor: Some(agent.clone()),
                        ..started.clone()
                    },
                    started.revision,
                )?;
                return Ok(Some(Begun {
                    operation: op_id,
                    fence,
                    step: Step::Inference(InferenceStart {
                        run_id: started.id.clone(),
                        job_id: started.job_id.clone(),
                        project_id: scope,
                        feature_id,
                        prompt,
                        route,
                        effort,
                        agent,
                        ctx,
                        resume,
                        session_id: started.session_id.clone(),
                        checkpoint: started.checkpoint.clone(),
                        hold: started.hold.clone(),
                    }),
                    run: started,
                }));
            }
        };
        Ok(Some(Begun {
            run: started,
            operation: op_id,
            fence,
            step,
        }))
    }

    /// Binds a job's scheduled agent principal on its first run: `Scheduled` role, confined to the
    /// job's target project (an app-level job's agent covers the workspace), every configured
    /// tool family. Host-bound, so it is attributable and revocable like any agent.
    ///
    /// An existing binding is reused as it stands: a person's revocation, role or narrowed tool
    /// families are never undone here (scheduling grants no bypass, JOB-07). The only field a
    /// re-bind sets is the project, when the job's target moved; a revoked binding is never
    /// re-bound. `Ok(None)`: the agent is revoked and the run must not start.
    fn bind_agent(
        &self,
        tx: &mut dyn WriteTransaction,
        job: &Id,
        project: Option<&ProjectId>,
    ) -> Result<Option<ActorId>, StorageError> {
        let actor = agent_actor(job);
        tx.actors()
            .register(&actor, PrincipalKind::Agent, "Scheduled agent")?;
        let current = tx.authority().principal(&actor)?;
        let (role, tool_families, expected) = match &current {
            None => (AgentRole::Scheduled, None, None),
            Some(b) if !b.active => return Ok(None),
            Some(b) if b.project_id.as_ref() == project => return Ok(Some(actor)),
            Some(b) => (
                b.role.unwrap_or(AgentRole::Scheduled),
                b.tool_families.clone(),
                Some(b.revision),
            ),
        };
        let invocation = tx.new_id(IdKind::Correlation);
        let ctx = host_ctx(tx, native_namespace(job), invocation, None);
        crate::authority::service::bind_principal(
            tx,
            &ctx,
            NewPrincipalBinding {
                actor: actor.clone(),
                kind: PrincipalKind::Agent,
                role: Some(role),
                project_id: project.cloned(),
                tool_families,
                bound_by: ctx.principal().clone(),
            },
            expected,
        )
        .map_err(|e| match e {
            crate::authority::service::AdminError::Storage(e) => e,
            other => {
                StorageError::InvalidRecord(format!("scheduled agent binding refused: {other:?}"))
            }
        })?;
        Ok(Some(actor))
    }

    /// Marks a started run's attempt `Dispatching`, immediately before its effect. `Ok(false)`
    /// when the attempt is no longer current (cancelled or superseded meanwhile): nothing may be
    /// dispatched.
    pub fn dispatching(
        &self,
        tx: &mut dyn WriteTransaction,
        begun: &Begun,
    ) -> Result<bool, StorageError> {
        let Some(run) = tx.jobs().job_run(&begun.run.id)? else {
            return Ok(false);
        };
        if run.status != begun.run.status
            || run.cancel_requested
            || run.revision != begun.run.revision
        {
            return Ok(false);
        }
        if let Some(why) = dispatch_fence(tx, &run)? {
            finish(
                tx,
                &run,
                "Cancelled",
                Some(OperationState::CancelledBeforeDispatch),
                Some(why),
                json!({"dispatched": false}),
            )?;
            return Ok(false);
        }
        let Some(rec) = tx.operations().operation(&begun.operation)? else {
            return Ok(false);
        };
        let current = rec.latest_attempt().map(|a| a.fence()) == Some(begun.fence.clone());
        // A retry of this step after its commit's acknowledgment was lost: the attempt is
        // already dispatching under this fence, and its executor has dispatched nothing yet (it
        // dispatches only once this step returns).
        if rec.state == OperationState::Dispatching && current {
            return Ok(true);
        }
        if rec.state != OperationState::Preflighted
            || rec.latest_attempt().map(|a| a.fence()) != Some(begun.fence.clone())
        {
            return Ok(false);
        }
        match tx.operations().transition(
            &begun.operation,
            rec.revision,
            Some(&begun.fence),
            OperationState::Dispatching,
            None,
        ) {
            Ok(_) => Ok(true),
            Err(StorageError::LeaseLost { .. } | StorageError::RevisionConflict(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// A started run whose dispatch did not happen (its attempt was no longer current): it ends
    /// cancelled when its cancellation was requested, and is queued again otherwise.
    pub fn undispatched(
        &self,
        tx: &mut dyn WriteTransaction,
        begun: &Begun,
    ) -> Result<(), StorageError> {
        let Some(run) = tx.jobs().job_run(&begun.run.id)? else {
            return Ok(());
        };
        if !matches!(run.status.as_str(), "Running" | "Holding") {
            return Ok(());
        }
        if run.cancel_requested {
            finish(
                tx,
                &run,
                "Cancelled",
                Some(OperationState::CancelledBeforeDispatch),
                Some("cancelled before dispatch"),
                json!({"dispatched": false}),
            )?;
            return Ok(());
        }
        if let Some(rec) = tx.operations().operation(&begun.operation)?
            && rec.state == OperationState::Preflighted
        {
            transition(
                tx,
                &begun.operation,
                OperationState::RetryPending,
                json!({"dispatched": false}),
            )?;
        }
        tx.jobs().update_run(
            &StoredJobRun {
                status: requeued(&run),
                boot_id: None,
                ..run.clone()
            },
            run.revision,
        )?;
        Ok(())
    }

    /// Started runs of this boot whose attempt is not yet handed to anything that settles it
    /// (no accepted operation, no session, no handoff the pipeline took), oldest first by ID. The engine reclaims those it is
    /// not executing ([`Self::reclaim`]): their executor ended after the start committed.
    pub fn unsettled_starts(
        &self,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Vec<StoredJobRun>, StorageError> {
        let boot = tx.store().boot_id.clone();
        let mut out = Vec::new();
        let mut after: Option<Id> = None;
        loop {
            let page = tx
                .jobs()
                .runs_where(&RunFilter::Active, after.as_ref(), PAGE)?;
            let more = page.len() == usize::from(PAGE);
            after = page.last().map(|r| r.id.clone());
            for r in page {
                if matches!(r.status.as_str(), "Running" | "Holding")
                    && r.boot_id.is_some()
                    && r.boot_id == boot
                    && r.inner_operation_id.is_none()
                    && r.session_id.is_none()
                    && !operation_of(tx, &r)?.is_some_and(|o| handed_off(&o))
                {
                    out.push(r);
                }
            }
            if !more {
                return Ok(out);
            }
        }
    }

    /// Reclaims a started run whose executor is gone (see [`Self::unsettled_starts`]). Never
    /// dispatched, it is queued again for the same identity (or cancelled, when that was asked).
    /// Possibly dispatched, a native run is settled from its command's receipt, or queued again
    /// for the same identity when there is none (the command never committed, and its receipt
    /// makes a resubmission apply at most once); an inference handoff whose answer was lost ends
    /// `OutcomeUnknown`. `Ok(None)` when the run is not such a start (any more).
    pub fn reclaim(
        &self,
        tx: &mut dyn WriteTransaction,
        run_id: &Id,
    ) -> Result<Option<StoredJobRun>, StorageError> {
        let boot = tx.store().boot_id.clone();
        let Some(run) = tx.jobs().job_run(run_id)? else {
            return Ok(None);
        };
        if !matches!(run.status.as_str(), "Running" | "Holding")
            || run.boot_id.is_none()
            || run.boot_id != boot
            || run.inner_operation_id.is_some()
            || run.session_id.is_some()
        {
            return Ok(None);
        }
        let Some(op) = run.operation_id.clone() else {
            return Ok(None);
        };
        let Some(rec) = tx.operations().operation(&op)? else {
            return Ok(None);
        };
        if handed_off(&rec) {
            return Ok(None);
        }
        match rec.state {
            OperationState::Preflighted if run.cancel_requested => {
                finish(
                    tx,
                    &run,
                    "Cancelled",
                    Some(OperationState::CancelledBeforeDispatch),
                    Some("cancelled before dispatch"),
                    json!({"dispatched": false}),
                )?;
            }
            OperationState::Preflighted => {
                transition(
                    tx,
                    &op,
                    OperationState::RetryPending,
                    json!({"reclaimed": true, "dispatched": false}),
                )?;
                tx.jobs().update_run(
                    &StoredJobRun {
                        status: requeued(&run),
                        boot_id: None,
                        ..run.clone()
                    },
                    run.revision,
                )?;
            }
            OperationState::Dispatching => {
                let native = matches!(
                    parse::<JobDefinition>(&run.definition)?,
                    JobDefinition::Native { .. }
                );
                if !native {
                    finish(
                        tx,
                        &run,
                        "OutcomeUnknown",
                        Some(OperationState::OutcomeUnknown),
                        Some(
                            "the pipeline's answer to the handoff was lost; what the run did is unknown",
                        ),
                        json!({"reclaimed": true}),
                    )?;
                } else if !self.settle_from_receipt(tx, &run)? {
                    transition(
                        tx,
                        &op,
                        OperationState::OutcomeUnknown,
                        json!({"reclaimed": true}),
                    )?;
                    transition(
                        tx,
                        &op,
                        OperationState::RetryPending,
                        json!({"reconciled": "no receipt"}),
                    )?;
                    let current = tx.jobs().job_run(run_id)?.unwrap_or(run);
                    tx.jobs().update_run(
                        &StoredJobRun {
                            status: requeued(&current),
                            boot_id: None,
                            ..current.clone()
                        },
                        current.revision,
                    )?;
                }
            }
            _ => return Ok(None),
        }
        tx.jobs().job_run(run_id)
    }

    /// Records a native run's command outcome. A committed decision settles it; an `Accepted`
    /// one leaves it running on that operation; a command refused before admission because the
    /// store was unavailable (shutting down) is queued again for the same identity; an unknown
    /// outcome stays unknown (the receipt resolves it at the next reconciliation).
    pub fn settle_native(
        &self,
        tx: &mut dyn WriteTransaction,
        begun: &Begun,
        outcome: &Result<DomainCommandReply, GatewayFailure>,
    ) -> Result<Option<StoredJobRun>, StorageError> {
        let Some(run) = tx.jobs().job_run(&begun.run.id)? else {
            return Ok(None);
        };
        if run.status != "Running" || run.operation_id.as_ref() != Some(&begun.operation) {
            return Ok(None);
        }
        let late = if run.cancel_requested {
            " (the command committed before the cancellation took effect)"
        } else {
            ""
        };
        Ok(Some(match outcome {
            Ok(CommandReply::Recorded { receipt, .. }) => {
                self.settle_decision(tx, &run, &receipt.decision, late)?
            }
            Ok(CommandReply::Rejected { code, .. }) => {
                if matches!(code, Rejection::StorageUnavailable) {
                    // Refused before admission: nothing was recorded. The same identity is
                    // submitted again (now, or after the next boot).
                    transition(
                        tx,
                        &begun.operation,
                        OperationState::RetryPending,
                        json!({"rejected": format!("{code:?}")}),
                    )?;
                    tx.jobs().update_run(
                        &StoredJobRun {
                            status: "Queued".into(),
                            boot_id: None,
                            ..run.clone()
                        },
                        run.revision,
                    )?
                } else {
                    let why = format!(
                        "the command was rejected before admission ({code:?}); nothing was applied"
                    );
                    finish(
                        tx,
                        &run,
                        "Failed",
                        Some(OperationState::Failed),
                        Some(&why),
                        json!({"rejected": format!("{code:?}")}),
                    )?
                }
            }
            // The acknowledgment was lost even after identical resubmissions: the receipt says
            // whether it applied; without one it never committed and is submitted again.
            Ok(CommandReply::CommandStatusUnknown { .. }) => self.resolve_dispatch(tx, &run)?,
            Err(e) => {
                let why = format!("the command was not recorded: {e}");
                finish(
                    tx,
                    &run,
                    "Failed",
                    Some(OperationState::Failed),
                    Some(&why),
                    json!({"recorded": false}),
                )?
            }
        }))
    }

    /// A native run whose dispatch outcome was not observed (a lost acknowledgment, a dispatch
    /// task that failed mid-call): settled from its command's receipt when there is one, queued
    /// again for the same identity when there is none (it never committed).
    pub fn reconcile_dispatch(
        &self,
        tx: &mut dyn WriteTransaction,
        begun: &Begun,
    ) -> Result<Option<StoredJobRun>, StorageError> {
        let Some(run) = tx.jobs().job_run(&begun.run.id)? else {
            return Ok(None);
        };
        if run.status != "Running" || run.operation_id.as_ref() != Some(&begun.operation) {
            return Ok(None);
        }
        self.resolve_dispatch(tx, &run).map(Some)
    }
    fn resolve_dispatch(
        &self,
        tx: &mut dyn WriteTransaction,
        run: &StoredJobRun,
    ) -> Result<StoredJobRun, StorageError> {
        if self.settle_from_receipt(tx, run)? {
            return tx
                .jobs()
                .job_run(&run.id)?
                .ok_or_else(|| StorageError::NotFound {
                    kind: "job_run",
                    id: run.id.as_str().to_owned(),
                });
        }
        if let Some(op) = &run.operation_id {
            transition(
                tx,
                op,
                OperationState::OutcomeUnknown,
                json!({"acknowledgment": "lost"}),
            )?;
            transition(
                tx,
                op,
                OperationState::RetryPending,
                json!({"reconciled": "no receipt"}),
            )?;
        }
        tx.jobs().update_run(
            &StoredJobRun {
                status: "Queued".into(),
                boot_id: None,
                ..run.clone()
            },
            run.revision,
        )
    }

    fn settle_decision(
        &self,
        tx: &mut dyn WriteTransaction,
        run: &StoredJobRun,
        decision: &DomainDecision,
        late: &str,
    ) -> Result<StoredJobRun, StorageError> {
        Ok(match decision {
            DomainDecision::Applied { .. } | DomainDecision::NoChange { .. } => {
                let reason = (!late.is_empty()).then(|| format!("succeeded{late}"));
                finish(
                    tx,
                    run,
                    "Succeeded",
                    Some(OperationState::ObservedComplete),
                    reason.as_deref(),
                    json!({"decision": "applied"}),
                )?
            }
            DomainDecision::Accepted { operation_id } => tx.jobs().update_run(
                &StoredJobRun {
                    inner_operation_id: Some(operation_id.clone()),
                    ..run.clone()
                },
                run.revision,
            )?,
            DomainDecision::Refused { fault } => {
                let target = fault
                    .target_kind
                    .as_ref()
                    .map(|k| format!(" ({})", k.0))
                    .unwrap_or_default();
                let why = format!("refused: {:?}{target}", fault.code);
                finish(
                    tx,
                    run,
                    "Failed",
                    Some(OperationState::Failed),
                    Some(&why),
                    json!({"decision": "refused"}),
                )?
            }
        })
    }

    /// Settles running native runs whose accepted operation reached a terminal state.
    pub fn poll_inner(&self, tx: &mut dyn WriteTransaction) -> Result<usize, StorageError> {
        let mut settled = 0;
        let mut after: Option<Id> = None;
        loop {
            let page = tx
                .jobs()
                .runs_where(&RunFilter::Active, after.as_ref(), PAGE)?;
            let more = page.len() == usize::from(PAGE);
            after = page.last().map(|r| r.id.clone());
            for run in page {
                let Some(inner) = run.inner_operation_id.clone() else {
                    continue;
                };
                if run.status != "Running" {
                    continue;
                }
                let Some(rec) = tx.operations().operation(&inner)? else {
                    continue;
                };
                let late = if run.cancel_requested {
                    " (the operation finished before the cancellation took effect)"
                } else {
                    ""
                };
                match rec.state {
                    OperationState::ObservedComplete => {
                        let reason = (!late.is_empty()).then(|| format!("succeeded{late}"));
                        finish(
                            tx,
                            &run,
                            "Succeeded",
                            Some(OperationState::ObservedComplete),
                            reason.as_deref(),
                            json!({"inner": inner.0.as_str()}),
                        )?;
                        settled += 1;
                    }
                    OperationState::Failed | OperationState::CancelledBeforeDispatch => {
                        // A failed operation does not establish that its owned processes
                        // stopped. Leave requested cancellations for the scheduler's
                        // verified-stop path, including a previously uncertain stop.
                        if run.cancel_requested {
                            continue;
                        }
                        finish(
                            tx,
                            &run,
                            "Failed",
                            Some(OperationState::Failed),
                            Some(&format!(
                                "operation {} ended {:?}",
                                inner.0.as_str(),
                                rec.state
                            )),
                            json!({"inner": inner.0.as_str()}),
                        )?;
                        settled += 1;
                    }
                    _ => {}
                }
            }
            if !more {
                return Ok(settled);
            }
        }
    }

    /// Records the inference pipeline's answer to [`InferenceJobs::start`].
    pub fn settle_inference_start(
        &self,
        tx: &mut dyn WriteTransaction,
        begun: &Begun,
        started: &Result<Option<Id>, InferenceRefusal>,
    ) -> Result<Option<StoredJobRun>, StorageError> {
        let Some(run) = tx.jobs().job_run(&begun.run.id)? else {
            return Ok(None);
        };
        if !matches!(run.status.as_str(), "Running" | "Holding")
            || run.operation_id.as_ref() != Some(&begun.operation)
        {
            return Ok(None);
        }
        Ok(Some(match started {
            Ok(session) => {
                // The pipeline owns the run now, with or without a session to name: recorded
                // on the attempt, so supervision never reclaims an accepted handoff.
                if let Some(rec) = tx.operations().operation(&begun.operation)?
                    && !handed_off(&rec)
                {
                    tx.operations().observe(
                        &begun.operation,
                        begun.fence.attempt_no,
                        json!({"handedOff": true, "sessionId": session}),
                    )?;
                }
                tx.jobs().update_run(
                    &StoredJobRun {
                        session_id: session.clone().or(run.session_id.clone()),
                        ..run.clone()
                    },
                    run.revision,
                )?
            }
            Err(InferenceRefusal {
                dispatched: false,
                reason,
            }) => finish(
                tx,
                &run,
                "Failed",
                Some(OperationState::Failed),
                Some(reason),
                json!({"dispatched": false}),
            )?,
            Err(InferenceRefusal { reason, .. }) => finish(
                tx,
                &run,
                "OutcomeUnknown",
                Some(OperationState::OutcomeUnknown),
                Some(reason),
                json!({"dispatched": true}),
            )?,
        }))
    }

    /// The pipeline's report of a scheduled inference run (fenced by run identity and revision:
    /// a late report of a run that ended or changed is refused, never applied to a later run).
    pub fn finish_inference(
        &self,
        tx: &mut dyn WriteTransaction,
        run_id: &Id,
        expected_revision: i64,
        outcome: &InferenceOutcome,
    ) -> Result<StoredJobRun, StorageError> {
        let run = tx
            .jobs()
            .job_run(run_id)?
            .ok_or_else(|| StorageError::NotFound {
                kind: "job_run",
                id: run_id.as_str().to_owned(),
            })?;
        if run.revision != expected_revision
            || !matches!(run.status.as_str(), "Running" | "Holding")
        {
            return Err(StorageError::LeaseLost {
                kind: "job_run",
                id: run_id.as_str().to_owned(),
            });
        }
        match outcome {
            InferenceOutcome::Succeeded => finish(
                tx,
                &run,
                "Succeeded",
                Some(OperationState::ObservedComplete),
                None,
                json!({}),
            ),
            InferenceOutcome::Failed(why) => finish(
                tx,
                &run,
                "Failed",
                Some(OperationState::Failed),
                Some(why),
                json!({}),
            ),
            InferenceOutcome::Cancelled => finish(
                tx,
                &run,
                "Cancelled",
                Some(OperationState::Failed),
                Some("cancelled"),
                json!({"cancelled": true}),
            ),
            InferenceOutcome::Unknown(why) => finish(
                tx,
                &run,
                "OutcomeUnknown",
                Some(OperationState::OutcomeUnknown),
                Some(why),
                json!({}),
            ),
            InferenceOutcome::Holding(hold) => tx.jobs().update_run(
                &StoredJobRun {
                    status: "Holding".into(),
                    hold: Some(hold.chars().take(256).collect()),
                    ..run.clone()
                },
                run.revision,
            ),
            InferenceOutcome::Resumed => tx.jobs().update_run(
                &StoredJobRun {
                    status: "Running".into(),
                    hold: None,
                    ..run.clone()
                },
                run.revision,
            ),
        }
    }

    /// The receipt of a native run's command, looked up by the run's identity in its job's
    /// namespace under the current restore epoch.
    fn native_receipt(
        &self,
        tx: &mut dyn WriteTransaction,
        run: &StoredJobRun,
    ) -> Result<Option<LedgerEntry>, StorageError> {
        let ctx = host_ctx(
            tx,
            native_namespace(&run.job_id),
            run.id.clone(),
            run.project_id.as_ref(),
        );
        let identity = crate::context::ReceiptIdentity::bind(&ctx, RequestKey(run.id.clone()));
        tx.receipts().lookup(&identity)
    }

    /// Settles a possibly dispatched native run from its command's receipt. `Ok(true)` when the
    /// receipt decided it (or showed it cannot be read); `Ok(false)` when there is none: the
    /// command never committed and the same identity may be submitted again.
    fn settle_from_receipt(
        &self,
        tx: &mut dyn WriteTransaction,
        run: &StoredJobRun,
    ) -> Result<bool, StorageError> {
        let Some(op) = run.operation_id.clone() else {
            return Ok(false);
        };
        match self.native_receipt(tx, run)? {
            Some(LedgerEntry::Current(stored)) => {
                transition(
                    tx,
                    &op,
                    OperationState::OutcomeUnknown,
                    json!({"reconciled": "receipt"}),
                )?;
                self.settle_decision(tx, run, &stored.receipt.decision, "")?;
                Ok(true)
            }
            Some(LedgerEntry::Incompatible { reason, .. }) => {
                let why = format!("its command's receipt cannot be read: {reason}");
                finish(
                    tx,
                    run,
                    "OutcomeUnknown",
                    Some(OperationState::OutcomeUnknown),
                    Some(&why),
                    json!({}),
                )?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Records the outcome of a requested cancellation once its native part ran (the scheduler
    /// calls it only for a run it is not executing). A native run whose accepted operation is
    /// still going keeps running (its outcome settles it); a native command whose receipt exists
    /// is settled from it (it committed before the cancellation); a run that could not be
    /// verified stopped is `OutcomeUnknown`, never shown as cancelled. Bound to the run's identity
    /// and the revision the scheduler read: it never settles a later run.
    pub fn settle_cancel(
        &self,
        tx: &mut dyn WriteTransaction,
        run_id: &Id,
        expected_revision: i64,
        verdict: &StopVerdict,
    ) -> Result<Option<StoredJobRun>, StorageError> {
        let Some(run) = tx.jobs().job_run(run_id)? else {
            return Ok(None);
        };
        if run.revision != expected_revision
            || !run.cancel_requested
            || !super::is_active(&run.status)
        {
            return Ok(None);
        }
        if undispatched(tx.operations(), &run)? && run.inner_operation_id.is_none() {
            return finish(
                tx,
                &run,
                "Cancelled",
                Some(OperationState::CancelledBeforeDispatch),
                Some("cancelled"),
                json!({"dispatched": false}),
            )
            .map(Some);
        }
        if let Some(inner) = &run.inner_operation_id {
            if tx
                .operations()
                .operation(inner)?
                .is_some_and(|o| !is_terminal(o.state))
            {
                return Ok(None);
            }
        } else if matches!(
            parse::<JobDefinition>(&run.definition)?,
            JobDefinition::Native { .. }
        ) && self.settle_from_receipt(tx, &run)?
        {
            return tx.jobs().job_run(run_id);
        }
        Ok(Some(match verdict {
            StopVerdict::Stopped => finish(
                tx,
                &run,
                "Cancelled",
                Some(OperationState::Failed),
                Some("cancelled; the run was verified stopped"),
                json!({"cancelled": true}),
            )?,
            StopVerdict::Uncertain(why) => finish(
                tx,
                &run,
                "OutcomeUnknown",
                Some(OperationState::OutcomeUnknown),
                Some(&format!("the run could not be verified stopped: {why}")),
                json!({"cancelled": "uncertain"}),
            )?,
        }))
    }

    /// Reconciles one run an earlier boot left active (see the module documentation).
    fn reconcile_interrupted(
        &self,
        tx: &mut dyn WriteTransaction,
        run: StoredJobRun,
    ) -> Result<(), StorageError> {
        let Some(op) = run.operation_id.clone() else {
            return Ok(());
        };
        let Some(rec) = tx.operations().operation(&op)? else {
            return Ok(());
        };
        // Already settled in the journal: the run follows it.
        match rec.state {
            OperationState::ObservedComplete => {
                finish(tx, &run, "Succeeded", None, None, json!({}))?;
                return Ok(());
            }
            OperationState::Failed => {
                finish(
                    tx,
                    &run,
                    "Failed",
                    None,
                    Some("the run's operation failed"),
                    json!({}),
                )?;
                return Ok(());
            }
            OperationState::CancelledBeforeDispatch => {
                finish(
                    tx,
                    &run,
                    "Cancelled",
                    None,
                    Some("cancelled before dispatch"),
                    json!({}),
                )?;
                return Ok(());
            }
            _ => {}
        }
        let restored = run_restored(tx, &run)?;
        let job = tx.jobs().job(&run.job_id)?;
        let permit: Result<(), String> = match &job {
            None => Err("the job no longer exists".into()),
            Some(j) if j.deleted_at_ms.is_some() => Err("the job was deleted".into()),
            Some(j) if !j.enabled && run.trigger == "Scheduled" => {
                Err("the schedule was disabled".into())
            }
            Some(_) if restored => Err("the store was restored from a backup".into()),
            Some(_) if !run_project_current(tx, &run)? => Err("the project was archived".into()),
            Some(j) if run.trigger == "Scheduled" && !j.authorized => {
                Err("a person has not authorized this schedule".into())
            }
            Some(_) => Ok(()),
        };
        let possibly = !matches!(
            rec.state,
            OperationState::IntentRecorded
                | OperationState::RetryPending
                | OperationState::Preflighted
        );
        let native = matches!(
            parse::<JobDefinition>(&run.definition)?,
            JobDefinition::Native { .. }
        );
        if possibly && run.inner_operation_id.is_none() {
            if native && !restored {
                // The command's receipt establishes whether it applied.
                if self.settle_from_receipt(tx, &run)? {
                    return Ok(());
                }
                // No receipt: it never committed, so the same identity may be submitted again.
                transition(
                    tx,
                    &op,
                    OperationState::OutcomeUnknown,
                    json!({"interrupted": true}),
                )?;
                transition(
                    tx,
                    &op,
                    OperationState::RetryPending,
                    json!({"reconciled": "no receipt"}),
                )?;
            } else if permit.is_ok() {
                // An inference run resumes its session from its checkpoint; its actions carry
                // identities that are reconciled before any retry ([`super::discovery`]).
                transition(
                    tx,
                    &op,
                    OperationState::OutcomeUnknown,
                    json!({"interrupted": true}),
                )?;
                transition(
                    tx,
                    &op,
                    OperationState::RetryPending,
                    json!({"resume": "checkpoint"}),
                )?;
            } else {
                // Possibly applied and not resumable (or only knowable from a receipt of an earlier
                // restore epoch): it stays visibly unknown, never shown as cancelled.
                let why = format!(
                    "not resumed ({}); what it already did is unknown",
                    permit.unwrap_err()
                );
                finish(
                    tx,
                    &run,
                    "OutcomeUnknown",
                    Some(OperationState::OutcomeUnknown),
                    Some(&why),
                    json!({}),
                )?;
                return Ok(());
            }
        } else if rec.state == OperationState::Preflighted {
            transition(
                tx,
                &op,
                OperationState::RetryPending,
                json!({"interrupted": true}),
            )?;
        }
        let still = tx.jobs().job_run(&run.id)?.unwrap_or(run);
        if !super::is_active(&still.status) {
            return Ok(());
        }
        if let Err(why) = permit {
            let why = format!("not resumed: {why}");
            let to = if still.inner_operation_id.is_some() {
                // The accepted operation belongs to its own service; only the run ends.
                None
            } else {
                Some(OperationState::Failed)
            };
            finish(
                tx,
                &still,
                "Cancelled",
                to,
                Some(&why),
                json!({"resumed": false}),
            )?;
            return Ok(());
        }
        // Same identity, progress and history; a separate hold stays (a holding run stays
        // holding, unowned until the scheduler reattaches its session), a running one is queued
        // to continue.
        let this_boot = tx.store().boot_id.clone();
        let polling = still.inner_operation_id.is_some();
        let resumed = tx.jobs().update_run(
            &StoredJobRun {
                status: if still.status == "Holding" {
                    "Holding".into()
                } else if polling {
                    "Running".into()
                } else {
                    "Queued".into()
                },
                boot_id: if polling { this_boot } else { None },
                resumptions: still.resumptions + 1,
                ..still.clone()
            },
            still.revision,
        )?;
        if let Some(j) = tx.jobs().job(&resumed.job_id)? {
            tx.jobs()
                .set_job_schedule(&j.id, j.revision, j.next_at_ms, Some(&resumed.id))?;
        }
        let actor = host(tx);
        event(
            tx,
            &actor,
            resumed.project_id.as_ref(),
            resumed.operation_id.as_ref(),
            (RUN_AGGREGATE, resumed.id.as_str(), resumed.revision),
            "JobRunResumed",
            json!({"jobId": resumed.job_id.as_str(), "resumptions": resumed.resumptions, "hold": resumed.hold}),
        )?;
        Ok(())
    }

    /// Boot recovery of schedules (see the module documentation).
    pub fn recover(&self, tx: &mut dyn WriteTransaction) -> Result<(), StorageError> {
        let boot = tx
            .store()
            .boot_id
            .clone()
            .ok_or_else(|| StorageError::IncompatibleSchema("store is read-only".into()))?;
        let mut after: Option<Id> = None;
        loop {
            let page = tx.jobs().runs_where(
                &RunFilter::ActiveNotOfBoot(boot.clone()),
                after.as_ref(),
                PAGE,
            )?;
            let more = page.len() == usize::from(PAGE);
            after = page.last().map(|r| r.id.clone());
            for run in page {
                self.reconcile_interrupted(tx, run)?;
            }
            if !more {
                break;
            }
        }
        let now = tx.now_ms();
        let mut after: Option<Id> = None;
        loop {
            let page = tx.jobs().all_jobs(after.as_ref(), PAGE)?;
            let more = page.len() == usize::from(PAGE);
            after = page.last().map(|j| j.id.clone());
            for job in page {
                if !job.enabled || job.hold.is_some() {
                    continue;
                }
                if let Some((hold, why)) = fence_of(tx, &job)? {
                    hold_job(tx, &job, hold, why)?;
                    continue;
                }
                if !evaluable(tx, &job)? {
                    continue;
                }
                let next = match job.next_at_ms {
                    Some(at) if at <= now => miss(tx, &job, at, now)?,
                    Some(at) => Some(at),
                    None => stored_plan(&job)?.next_after(job.anchor_at_ms, now),
                };
                if next != job.next_at_ms {
                    tx.jobs().set_job_schedule(
                        &job.id,
                        job.revision,
                        next,
                        job.interrupted_run_id.as_ref(),
                    )?;
                }
            }
            if !more {
                return Ok(());
            }
        }
    }

    /// Shutdown: every active run of this boot is marked interrupted on its job (it resumes at
    /// the next boot). Schedule admission itself is frozen by the engine before this runs.
    pub fn quiesce(&self, tx: &mut dyn WriteTransaction) -> Result<usize, StorageError> {
        let boot = tx.store().boot_id.clone();
        let mut n = 0;
        let mut after: Option<Id> = None;
        loop {
            let page = tx
                .jobs()
                .runs_where(&RunFilter::Active, after.as_ref(), PAGE)?;
            let more = page.len() == usize::from(PAGE);
            after = page.last().map(|r| r.id.clone());
            for run in page {
                if run.boot_id.is_some() && run.boot_id != boot {
                    continue;
                }
                if let Some(j) = tx.jobs().job(&run.job_id)? {
                    tx.jobs()
                        .set_job_schedule(&j.id, j.revision, j.next_at_ms, Some(&run.id))?;
                }
                let actor = host(tx);
                event(
                    tx,
                    &actor,
                    run.project_id.as_ref(),
                    run.operation_id.as_ref(),
                    (RUN_AGGREGATE, run.id.as_str(), run.revision),
                    "JobRunInterrupted",
                    json!({"jobId": run.job_id.as_str(), "status": run.status}),
                )?;
                n += 1;
            }
            if !more {
                return Ok(n);
            }
        }
    }
}

/// The schedules' run-control lifecycle hooks ([`LifecycleHooks`]), composed into the engine's
/// run supervision: shutdown marks active runs interrupted, boot reconciles and resumes them and
/// recomputes every next occurrence without catch-up.
pub struct JobLifecycle(pub Arc<JobService>);
impl LifecycleHooks for JobLifecycle {
    fn quiesce(&self, tx: &mut dyn WriteTransaction) -> Result<(), StorageError> {
        self.0.quiesce(tx).map(|_| ())
    }
    fn recover(&self, tx: &mut dyn WriteTransaction) -> Result<(), StorageError> {
        self.0.recover(tx)
    }
}

/// The jobs whose schedules act on `project` under their current definition: those scoped to
/// it, and the app-level inference tasks that now target it. A run acts on its own snapshot's
/// target instead ([`project_runs`]).
fn project_jobs(
    tx: &mut dyn WriteTransaction,
    project: &ProjectId,
) -> Result<Vec<StoredJob>, StorageError> {
    let mut out = Vec::new();
    for scope in [JobScope::Project(project.clone()), JobScope::App] {
        let mut after: Option<(String, String)> = None;
        loop {
            let page = tx.jobs().jobs(
                &scope,
                None,
                after.as_ref().map(|(t, i)| (t.as_str(), i.as_str())),
                PAGE,
            )?;
            let more = page.len() == usize::from(PAGE);
            after = page
                .last()
                .map(|j| (j.title.clone(), j.id.as_str().to_owned()));
            out.extend(
                page.into_iter()
                    .filter(|j| job_target(j).as_ref() == Some(project)),
            );
            if !more {
                break;
            }
        }
    }
    Ok(out)
}

/// The active runs that act on `project` by their own snapshot ([`run_target`]), whatever their
/// job's current definition targets: an edit may retarget a job while its run is active.
fn project_runs(
    tx: &mut dyn WriteTransaction,
    project: &ProjectId,
) -> Result<Vec<StoredJobRun>, StorageError> {
    let mut out = Vec::new();
    let mut after: Option<Id> = None;
    loop {
        let page = tx
            .jobs()
            .runs_where(&RunFilter::Active, after.as_ref(), PAGE)?;
        let more = page.len() == usize::from(PAGE);
        after = page.last().map(|r| r.id.clone());
        out.extend(
            page.into_iter()
                .filter(|r| run_target(r).as_ref() == Some(project)),
        );
        if !more {
            return Ok(out);
        }
    }
}

/// `projects.archive` (DW-11) calls this in the archival's unit of work (after
/// `ControlService::stop_project`): every enabled job whose schedule acts on `project` (scoped
/// to it, or an app-level task now targeting it) is held `ProjectClosed`, and every active run
/// whose own snapshot acts on it is asked to stop. Returns the runs asked to stop.
pub fn project_archived(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    project: &ProjectId,
) -> Result<Vec<Id>, StorageError> {
    for job in project_jobs(tx, project)? {
        if job.enabled {
            hold_job(tx, &job, "ProjectClosed", "the project was archived")?;
        }
    }
    let mut stopped = Vec::new();
    for run in project_runs(tx, project)? {
        if !run.cancel_requested {
            request_cancel(tx, actor, &run, "the project was archived")?;
            stopped.push(run.id);
        }
    }
    Ok(stopped)
}

/// `projects.restore` (DW-11) calls this in the restore's unit of work: the restored
/// project's held schedules stay stopped, now as `ReactivationRequired` (each needs a person's
/// `jobs.enable`; keep-working likewise stays stopped). Returns the jobs that stay held.
pub fn project_restored(
    tx: &mut dyn WriteTransaction,
    project: &ProjectId,
) -> Result<Vec<Id>, StorageError> {
    let mut held = Vec::new();
    for job in project_jobs(tx, project)? {
        if job.enabled {
            hold_job(
                tx,
                &job,
                "ReactivationRequired",
                "the project was restored; enable the schedule to reactivate it",
            )?;
            held.push(job.id);
        }
    }
    Ok(held)
}

/// Whether the pipeline took a run under its operation's latest attempt
/// ([`JobService::settle_inference_start`]).
fn handed_off(rec: &OperationRecord) -> bool {
    rec.latest_attempt().is_some_and(|a| {
        a.observations
            .iter()
            .any(|o| o.get("handedOff") == Some(&json!(true)))
    })
}

/// Whether a run may be started: queued and unheld, or holding with no owner (its session to
/// reattach, still holding).
fn startable(run: &StoredJobRun) -> bool {
    match run.status.as_str() {
        "Queued" => run.hold.is_none(),
        "Holding" => run.boot_id.is_none(),
        _ => false,
    }
}

/// The status a started run returns to when it is queued again: a holding run keeps holding.
fn requeued(run: &StoredJobRun) -> String {
    if run.status == "Holding" {
        "Holding".into()
    } else {
        "Queued".into()
    }
}

/// The counter value of a stored integer (re-exported for the engine's reports).
pub fn as_counter(n: i64) -> Counter {
    counter(n)
}

/// A run's operation record (for the engine's process stops).
pub fn operation_of(
    tx: &mut dyn WriteTransaction,
    run: &StoredJobRun,
) -> Result<Option<OperationRecord>, StorageError> {
    match &run.operation_id {
        Some(op) => tx.operations().operation(op),
        None => Ok(None),
    }
}
