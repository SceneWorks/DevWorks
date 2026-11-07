//! `JobService` (owner: Scheduler): durable schedules and manual jobs (JOB-01..09) behind
//! `jobs.list`, `.create`, `.edit`, `.enable`, `.disable`, `.delete`, `.run_now`, `.inspect` and
//! `.cancel`, registered through [`crate::services::UseCases::with_jobs`]. The native Schedules
//! screen and agent tools reach the same use cases through the gateway; the engine's scheduler
//! reaches the same rules through [`runner`] (occurrence claims, run execution, cancellation,
//! boot and shutdown lifecycle) and the run's executors through [`discovery`] (checkpoints,
//! action identities, linked work and usage). No entry path has its own copy of these rules.
//!
//! # Jobs
//!
//! A job is app-level (`DataScope::Workspace`) or belongs to one project, has a title unique in
//! its scope (case-insensitive, as CodeGraph's schedule names were), a schedule (manual, a fixed
//! interval of at least [`occurrence::MIN_INTERVAL_SECONDS`], or a standard cron expression in an
//! IANA time zone, see [`occurrence`]), a definition (a native maintenance capability with
//! schema-bound arguments, or a scheduled inference task: saved prompt, route and effort, with an
//! optional target project and feature), an enabled state and its run history. Its revision
//! advances with its configuration only; the scheduler's next-occurrence bookkeeping does not
//! advance it.
//!
//! # Authorization (JOB-05, JOB-07, CONTROL-12)
//!
//! A schedule's existence is not authority. Creating an *enabled* job, enabling one, and editing
//! an enabled or authorized job are a person's decisions: any other principal (an agent, package,
//! MCP client or schedule) may create and edit disabled drafts, disable, cancel and run a job a
//! person authorized, and is refused `ApprovalRequired` otherwise. A draft edited by another
//! principal loses its authorization. Native runs execute their capability through the gateway
//! as the host (ordinary host authority: a person-only operation stays refused), so they can do
//! nothing a person did not authorize; scheduled inference runs act as a scheduled agent bound
//! by the host with the full configured tools of a coordinator in the job's scope, under every
//! ordinary policy, pause, plan-readiness and budget rule (see [`runner`]). A person's later
//! revocation or narrowing of that agent stands: a revoked agent's runs fail before dispatch.
//!
//! # Runs
//!
//! Every run has its own identity, its occurrence identity (`at-<utc ms>` for a scheduled
//! instant, `manual-<run>` for Run now), the job revision and definition snapshot it was claimed
//! with, a journal operation, status, times, reason, a preserved hold, a cancellation request,
//! a resumption count, linked work roots, its agent session and usage. A job has at most one
//! active run (`Queued`, `Running`, `Holding`), enforced by the store: an occurrence (or a Run
//! now) while one is active is recorded `Skipped` and nothing is queued (JOB-08). Missed
//! occurrences are never replayed or coalesced (JOB-03).
//!
//! # Retained CodeGraph behavior
//!
//! `SettingsController.ListSchedules`/`GetSchedule`/`CreateSchedule`/`UpdateSchedule`/
//! `DeleteSchedule`/`RunScheduleNow`/`EnableSchedule`/`DisableSchedule` map to `jobs.list`,
//! `.inspect`, `.create`, `.edit` (+ `.enable`/`.disable`, since DevWorks edits never change
//! enablement), `.delete`, `.run_now`, `.enable` and `.disable`. Kept: name uniqueness ("a
//! schedule named ... already exists" → `Conflict`, `job_title`), Cronos standard expressions,
//! unknown time zones and expressions without a future occurrence refused, enable recomputing
//! the next run from now, a manual run leaving the next scheduled occurrence in place, Run now
//! of a disabled job, an unknown or deleted schedule `NotFound`, and a durable execution identity
//! reused across a retried execution. Changed deliberately: Run now while a run is active records
//! a `Skipped` run (JOB-08) instead of `409 Conflict`; disabling clears the next run instead of
//! leaving a stale one; deletion keeps the run history as a tombstone and cancels the active run
//! instead of dropping the row.
pub mod discovery;
mod native_help;
pub mod occurrence;
pub mod runner;

use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, jobs as ops};
use crate::ports::storage::{
    JobRead, JobScope, NewEvent, NewOperation, OperationRead, ReadSnapshot, StorageError,
    StoredJob, StoredJobRun, StoredJobUsage, WriteTransaction, cancellable_before_dispatch,
};
use crate::ports::{CommandHandler, Decided, HandlerFailure, IdKind, QueryAnswer, QueryHandler};
use crate::protocol::data::DataScope;
use crate::protocol::provider::{Digest, QualifiedId, SchemaRef};
use crate::protocol::records::{
    JobDefinition, JobHold, JobInspection, JobRecord, JobRun, JobRunStatus, JobTrigger,
    PageRequest, RouteIntent, Schedule, UsageItem, UsageReport,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Page};
use crate::protocol::{
    CorrelationId, Counter, DomainErrorCode, DomainFault, EventId, Id, OperationId, OperationState,
    ProjectId, Revision, RevisionDimension,
};
use crate::registry::{self, Envelope, Mode, SchemaRevision};
use crate::services::UseCases;
use crate::services::cursor::CursorBinding;
use crate::services::sessions::{Failure, Refusal, not_found, project_visible, query_fault, rev};
use occurrence::Plan;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// The operations [`JobService`] is registered for.
pub const OPERATIONS: [&str; 9] = [
    "jobs.list",
    "jobs.create",
    "jobs.edit",
    "jobs.enable",
    "jobs.run_now",
    "jobs.inspect",
    "jobs.cancel",
    "jobs.delete",
    "jobs.disable",
];
/// History aggregates of jobs and their runs.
pub const JOB_AGGREGATE: &str = "job";
pub const RUN_AGGREGATE: &str = "job_run";
/// Journal capability of a run's operation.
pub const RUN_CAPABILITY: &str = "jobs.run";
/// The catalog commands a native job may run: the operation the frozen inventory maps each
/// retained CodeGraph job type to (`job:src/CodeGraph.Jobs/Jobs/*`). Only those with a registered
/// use case are offered and accepted ([`NativeCatalog::qualified`]).
pub const MAINTENANCE_OPERATIONS: [&str; 13] = [
    "maintenance.prune_diagnostics",
    "analysis.community_analysis",
    "indexing.discover",
    "dreaming.run",
    "maintenance.prune_index_runs",
    "conventions.index",
    "analysis.link_and_detect",
    "analysis.process_batch",
    "maintenance.prune_messages",
    "maintenance.prune_sessions",
    "indexing.reindex",
    "help.generate_capability_docs",
    "imports.apply",
];
/// Namespace of a native capability's argument schema (`<namespace>/<operation>`).
pub const ARGUMENT_SCHEMA_NAMESPACE: &str = "devworks.job-arguments";
/// Runs a run-history page returns by default, and usage items a run carries by default.
pub const HISTORY_PAGE: u16 = 50;
pub const USAGE_PAGE: u16 = 100;
/// Fault target kinds.
pub const JOB: &str = "job";
pub const RUN: &str = "job_run";

/// The capability ID of a catalog operation (`maintenance.prune_diagnostics` →
/// `maintenance-prune_diagnostics`; IDs cannot contain `.`).
pub fn capability_id(operation: &str) -> Id {
    Id::new(operation.replacen('.', "-", 1)).expect("catalog operation names are ID-safe")
}

/// The argument schema of a native capability at `revision`: its input schema's canonical
/// digest under a qualified ID. Arguments bound to another schema (or an older digest) are
/// refused.
pub fn argument_schema(operation: &str, revision: SchemaRevision) -> Option<SchemaRef> {
    let descriptor = registry::find(operation)?;
    let binding = descriptor.binding(Mode::Command)?;
    let schema = serde_json::to_value(binding.input_schema(revision)).ok()?;
    let canonical = crate::jcs::canonicalize(&schema).ok()?;
    Some(SchemaRef {
        schema_id: QualifiedId::try_from(format!("{ARGUMENT_SCHEMA_NAMESPACE}/{operation}"))
            .ok()?,
        revision: Revision::new(i64::from(revision.0)).ok()?,
        sha256: Digest(sha256_hex(canonical.as_bytes())),
    })
}

/// A native capability as offered to the Schedules screen and agents.
#[derive(Clone, Debug)]
pub struct NativeCapability {
    pub capability_id: Id,
    pub operation: &'static str,
    /// The latest registered schema revision and its argument schema.
    pub revision: SchemaRevision,
    pub schema: SchemaRef,
}

/// The native capabilities this composition can actually execute.
#[derive(Clone, Debug, Default)]
pub struct NativeCatalog {
    operations: Vec<&'static str>,
}
impl NativeCatalog {
    /// [`MAINTENANCE_OPERATIONS`] that are domain-envelope commands with a registered use case.
    pub fn qualified(use_cases: &UseCases) -> Self {
        Self::of(
            &MAINTENANCE_OPERATIONS
                .iter()
                .copied()
                .filter(|op| use_cases.has_command(op))
                .collect::<Vec<_>>(),
        )
    }
    /// Exactly `operations`, each a domain-envelope catalog command (others are dropped).
    pub fn of(operations: &[&'static str]) -> Self {
        Self {
            operations: operations
                .iter()
                .copied()
                .filter(|op| {
                    registry::find(op).is_some_and(|d| {
                        d.envelope == Envelope::Domain && d.binding(Mode::Command).is_some()
                    })
                })
                .collect(),
        }
    }
    pub fn capabilities(&self) -> Vec<NativeCapability> {
        self.operations
            .iter()
            .filter_map(|op| {
                let revision = *registry::find(op)?.revisions.last()?;
                Some(NativeCapability {
                    capability_id: capability_id(op),
                    operation: op,
                    revision,
                    schema: native_help::argument_schema(op, revision)
                        .or_else(|| argument_schema(op, revision))?,
                })
            })
            .collect()
    }
    fn resolve(&self, capability: &Id) -> Option<&'static str> {
        self.operations
            .iter()
            .copied()
            .find(|op| capability_id(op) == *capability)
    }
}

/// Scheduler policy (engineering defaults).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobConfig {
    /// An occurrence claimed more than this after its instant (the app was closed, asleep or
    /// busy) is missed, never run late.
    pub due_grace_ms: i64,
    /// Due jobs claimed per scheduler transaction.
    pub claim_batch: u16,
}
impl Default for JobConfig {
    fn default() -> Self {
        Self {
            due_grace_ms: 60_000,
            claim_batch: 50,
        }
    }
}

/// The job service.
pub struct JobService {
    catalog: NativeCatalog,
    config: JobConfig,
    approval_control: Option<std::sync::Arc<crate::services::runs::ControlService>>,
}

// ---------------------------------------------------------------------------------------------
// Helpers

fn wire<T: DeserializeOwned>(v: Value) -> Result<T, StorageError> {
    serde_json::from_value(v)
        .map_err(|e| StorageError::InvalidRecord(format!("stored job value: {e}")))
}
fn parse<T: DeserializeOwned>(s: &str) -> Result<T, StorageError> {
    serde_json::from_str(s).map_err(|e| {
        StorageError::unavailable(
            crate::ports::storage::Unavailable::Corrupt,
            format!("stored job record: {e}"),
        )
    })
}
fn canonical<T: serde::Serialize>(v: &T) -> String {
    let value = serde_json::to_value(v).expect("wire values serialize");
    crate::jcs::canonicalize(&value).expect("wire values canonicalize")
}
pub(crate) fn counter(n: i64) -> Counter {
    Counter::new(n.max(0)).expect("non-negative counter")
}
pub(crate) fn text(s: &str) -> BoundedText<1, 4096> {
    let t: String = s.chars().take(4096).collect();
    BoundedText(if t.is_empty() { "-".into() } else { t })
}
fn label(s: &str) -> BoundedText<1, 256> {
    let t: String = s.chars().take(256).collect();
    BoundedText(if t.is_empty() { "-".into() } else { t })
}
fn refuse(code: DomainErrorCode, kind: &'static str, id: Option<&Id>) -> Failure {
    Refusal::new(code, kind, id.cloned()).into()
}
fn invalid(kind: &'static str) -> Failure {
    refuse(DomainErrorCode::InvalidArgument, kind, None)
}
fn conflict(kind: &'static str, id: &Id, expected: i64, current: i64) -> Failure {
    Failure::Storage(StorageError::conflict(
        RevisionDimension::Schedule,
        kind,
        id.clone(),
        rev(expected),
        rev(current),
    ))
}
/// Whether the invocation is a person, whose decision alone authorizes a schedule.
pub fn authorizes(ctx: &InvocationContext) -> bool {
    ctx.principal_kind() == PrincipalKind::Human
        || ctx
            .reviewed_operation()
            .is_some_and(|r| crate::services::approvals::requests::reviewed_job(r.operation()))
}
/// Whether a project is closed to work (anything but `Active`: archived or deleted). DW-11's
/// `services::runs::closed_to_work` states the same rule; this keeps the jobs item independent
/// of it until both are merged.
pub fn project_closed(lifecycle: Option<&str>) -> bool {
    lifecycle.is_none_or(crate::services::runs::closed_to_work)
}
/// The project a job's runs act on: its scope's project, or the project an app-level inference
/// task targets (`None`: the workspace). Its lifecycle and archive epoch fence the job and its
/// runs; the listing scope alone does not name it.
pub(crate) fn target_project(scope: Option<&ProjectId>, definition: &str) -> Option<ProjectId> {
    scope
        .cloned()
        .or_else(|| match parse::<JobDefinition>(definition) {
            Ok(JobDefinition::Inference { project_id, .. }) => project_id,
            _ => None,
        })
}
/// The project a job's runs act on ([`target_project`]).
pub(crate) fn job_target(job: &StoredJob) -> Option<ProjectId> {
    target_project(job.project_id.as_ref(), &job.definition)
}
/// The project a run acts on: [`target_project`] of the run's own snapshot (the definition it
/// was queued with), never its job's current definition, which an edit may have retargeted.
pub(crate) fn run_target(run: &StoredJobRun) -> Option<ProjectId> {
    target_project(run.project_id.as_ref(), &run.definition)
}
/// A project's archive epoch (advanced by every archival); 0 for an app-level scope.
pub(crate) fn project_epoch(
    tx: &mut dyn WriteTransaction,
    project: Option<&ProjectId>,
) -> Result<i64, StorageError> {
    Ok(match project {
        Some(p) => tx.projects().project(p)?.map_or(0, |s| s.archive_epoch),
        None => 0,
    })
}
pub(crate) fn event(
    tx: &mut dyn WriteTransaction,
    actor: &crate::protocol::ActorId,
    project: Option<&ProjectId>,
    operation: Option<&OperationId>,
    aggregate: (&str, &str, i64),
    kind: &str,
    payload: Value,
) -> Result<(), StorageError> {
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id,
        actor: actor.clone(),
        project_id: project.cloned(),
        root_id: None,
        operation_id: operation.cloned(),
        aggregate_kind: aggregate.0.into(),
        aggregate_id: aggregate.1.into(),
        aggregate_revision: aggregate.2.max(1),
        kind: kind.into(),
        payload,
    })?;
    Ok(())
}

/// The live job `id` if the invocation may see it (otherwise `NotFound`).
fn visible_job(read: &dyn JobRead, ctx: &InvocationContext, id: &Id) -> Result<StoredJob, Failure> {
    match read.job(id)? {
        Some(j) if j.deleted_at_ms.is_none() && project_visible(ctx, j.project_id.as_ref()) => {
            Ok(j)
        }
        _ => Err(not_found(JOB, id).into()),
    }
}

/// The evaluable plan of a stored or requested schedule, with its zone name.
pub fn plan_of(schedule: &Schedule) -> Result<(Plan, String), Failure> {
    match schedule {
        Schedule::Manual {} => Ok((Plan::Manual, "UTC".into())),
        Schedule::Interval { seconds } => {
            let s: i64 = seconds.0.parse().map_err(|_| invalid("schedule"))?;
            if s < occurrence::MIN_INTERVAL_SECONDS {
                return Err(invalid("schedule"));
            }
            Ok((
                Plan::Interval {
                    period_ms: s.checked_mul(1000).ok_or_else(|| invalid("schedule"))?,
                },
                "UTC".into(),
            ))
        }
        Schedule::Calendar {
            expression,
            timezone,
        } => {
            let tz = occurrence::zone(timezone.0.trim()).map_err(|_| invalid("timezone"))?;
            let cron = occurrence::CronExpression::parse(&expression.0)
                .map_err(|_| invalid("schedule"))?;
            Ok((Plan::Calendar { cron, tz }, timezone.0.trim().to_owned()))
        }
    }
}
fn stored_plan(job: &StoredJob) -> Result<Plan, StorageError> {
    let schedule: Schedule = parse(&job.schedule)?;
    plan_of(&schedule).map(|(p, _)| p).map_err(|_| {
        StorageError::unavailable(
            crate::ports::storage::Unavailable::Corrupt,
            "stored schedule no longer evaluates",
        )
    })
}

fn status_of(s: &str) -> Result<JobRunStatus, StorageError> {
    wire(json!(s))
}
fn is_active(status: &str) -> bool {
    crate::ports::storage::ACTIVE_RUN_STATUSES.contains(&status)
}

// ---------------------------------------------------------------------------------------------
// Records

/// What a job record is built from besides the job row.
struct JobFacts {
    latest: Option<StoredJobRun>,
    active: Option<StoredJobRun>,
}
fn job_facts(read: &dyn JobRead, job: &StoredJob) -> Result<JobFacts, StorageError> {
    Ok(JobFacts {
        latest: read.job_runs(&job.id, None, 1)?.pop(),
        active: read.active_run(&job.id)?,
    })
}
fn build_job(job: &StoredJob, facts: JobFacts) -> Result<JobRecord, StorageError> {
    let latest = facts.latest.as_ref();
    let failed = latest.is_some_and(|r| matches!(r.status.as_str(), "Failed" | "OutcomeUnknown"));
    Ok(JobRecord {
        id: job.id.clone(),
        revision: rev(job.revision),
        title: label(&job.title),
        schedule: parse(&job.schedule)?,
        definition: parse(&job.definition)?,
        enabled: job.enabled,
        interrupted_run_id: job.interrupted_run_id.clone(),
        last_run_id: latest.map(|r| r.id.clone()),
        scope: Some(match &job.project_id {
            Some(p) => DataScope::Project {
                project_id: p.clone(),
            },
            None => DataScope::Workspace,
        }),
        next_run_at_ms: job.next_at_ms.map(counter),
        last_run_started_at_ms: latest.and_then(|r| r.started_at_ms).map(counter),
        last_run_finished_at_ms: latest.and_then(|r| r.finished_at_ms).map(counter),
        last_run_status: latest.map(|r| status_of(&r.status)).transpose()?,
        last_error: if failed {
            latest.and_then(|r| r.reason.as_deref()).map(text)
        } else {
            None
        },
        active_run_id: facts.active.map(|r| r.id),
        hold: job
            .hold
            .as_deref()
            .map(|h| wire::<JobHold>(json!(h)))
            .transpose()?,
        authorized: Some(job.authorized),
        created_at_ms: Some(counter(job.created_at_ms)),
        updated_at_ms: Some(counter(job.updated_at_ms)),
        deleted_at_ms: job.deleted_at_ms.map(counter),
    })
}
fn job_in_tx(tx: &mut dyn WriteTransaction, job: &StoredJob) -> Result<JobRecord, StorageError> {
    let facts = job_facts(tx.jobs(), job)?;
    build_job(job, facts)
}

/// What a run record is built from besides the run row.
struct RunFacts {
    state: OperationState,
    links: Vec<crate::protocol::RootId>,
    usage: UsageReport,
}
/// A page of a run's usage items with totals over all of them.
fn usage_report(
    read: &dyn JobRead,
    run: &Id,
    after: Option<i64>,
    limit: u16,
    binding: Option<&CursorBinding>,
) -> Result<UsageReport, StorageError> {
    let limit = limit.clamp(1, 500);
    let rows = read.run_usage(run, after, limit)?;
    let more = rows.len() == usize::from(limit)
        && rows
            .last()
            .map(|l| read.run_usage(run, Some(l.seq), 1))
            .transpose()?
            .is_some_and(|n| !n.is_empty());
    let totals = read.usage_totals(run)?;
    let next_cursor = match (more, binding, rows.last()) {
        (true, Some(b), Some(l)) => Some(BoundedText(b.encode(&l.seq.to_string()))),
        _ => None,
    };
    Ok(UsageReport {
        items: BoundedList(rows.iter().map(usage_item).collect::<Result<Vec<_>, _>>()?),
        total_tracked_usd: wire(json!(money(totals.tracked_pico_usd)))?,
        untracked_requests: counter(totals.untracked),
        incomplete_requests: counter(totals.incomplete),
        next_cursor,
    })
}
/// A non-negative pico-USD amount as a canonical decimal (`0`, `1.25`, ...).
pub fn money(pico: i128) -> String {
    let pico = pico.max(0);
    let whole = pico / 1_000_000_000_000;
    let frac = pico % 1_000_000_000_000;
    if frac == 0 {
        return whole.to_string();
    }
    let f = format!("{frac:012}");
    format!("{whole}.{}", f.trim_end_matches('0'))
}
fn usage_item(u: &StoredJobUsage) -> Result<UsageItem, StorageError> {
    Ok(UsageItem {
        request_id: u.request_id.clone(),
        purpose: label(&u.purpose),
        model_id: u.model_id.clone(),
        input_tokens: counter(u.input_tokens),
        output_tokens: counter(u.output_tokens),
        cache_read_tokens: counter(u.cache_read_tokens),
        cache_write_tokens: counter(u.cache_write_tokens),
        amount_usd: u
            .amount_usd
            .as_deref()
            .map(|a| wire(json!(a)))
            .transpose()?,
        price_kind: wire(json!(u.price_kind))?,
        price_origin: wire(json!(u.price_origin))?,
        price_revision: u.price_revision.map(rev),
        incomplete: u.incomplete,
    })
}
fn op_state(ops: &dyn OperationRead, run: &StoredJobRun) -> Result<OperationState, StorageError> {
    Ok(match &run.operation_id {
        Some(op) => ops
            .operation(op)?
            .map_or(OperationState::IntentRecorded, |o| o.state),
        None => OperationState::CancelledBeforeDispatch,
    })
}
fn build_run(run: &StoredJobRun, facts: RunFacts) -> Result<JobRun, StorageError> {
    Ok(JobRun {
        id: run.id.clone(),
        job_id: run.job_id.clone(),
        revision: rev(run.revision),
        occurrence_key: text(&run.occurrence_key),
        state: facts.state,
        operation_id: run.operation_id.clone(),
        usage: facts.usage,
        status: Some(status_of(&run.status)?),
        trigger: Some(wire::<JobTrigger>(json!(run.trigger))?),
        occurrence_at_ms: Some(counter(run.occurrence_at_ms)),
        started_at_ms: run.started_at_ms.map(counter),
        finished_at_ms: run.finished_at_ms.map(counter),
        reason: run.reason.as_deref().map(text),
        hold: run.hold.as_deref().map(label),
        cancel_requested: Some(run.cancel_requested),
        resumptions: Some(counter(run.resumptions)),
        session_id: run.session_id.clone(),
        linked_root_ids: Some(BoundedList(facts.links.into_iter().take(128).collect())),
        skipped_for_run_id: run.skipped_for.clone(),
    })
}
fn run_in_tx(tx: &mut dyn WriteTransaction, run: &StoredJobRun) -> Result<JobRun, StorageError> {
    let state = op_state(tx.operations(), run)?;
    let links = tx.jobs().run_links(&run.id)?;
    let usage = usage_report(tx.jobs(), &run.id, None, USAGE_PAGE, None)?;
    build_run(
        run,
        RunFacts {
            state,
            links,
            usage,
        },
    )
}
fn run_in_snapshot(
    s: &dyn ReadSnapshot,
    run: &StoredJobRun,
    usage: Option<(Option<i64>, u16)>,
    binding: Option<&CursorBinding>,
) -> Result<JobRun, StorageError> {
    let (after, limit) = usage.unwrap_or((None, USAGE_PAGE));
    build_run(
        run,
        RunFacts {
            state: op_state(s.operations(), run)?,
            links: s.jobs().run_links(&run.id)?,
            usage: usage_report(s.jobs(), &run.id, after, limit, binding)?,
        },
    )
}

// ---------------------------------------------------------------------------------------------
// Use cases

impl JobService {
    pub fn new(catalog: NativeCatalog, config: JobConfig) -> Self {
        Self {
            catalog,
            config,
            approval_control: None,
        }
    }
    pub fn with_approval_control(
        mut self,
        control: std::sync::Arc<crate::services::runs::ControlService>,
    ) -> Self {
        self.approval_control = Some(control);
        self
    }
    pub fn catalog(&self) -> &NativeCatalog {
        &self.catalog
    }
    pub fn config(&self) -> JobConfig {
        self.config
    }
    fn correlation(&self, ctx: &InvocationContext) -> CorrelationId {
        CorrelationId(ctx.invocation_id().clone())
    }

    /// Validates a definition for a job of `project` (`None`: app-level); returns its kind.
    fn check_definition(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        project: Option<&ProjectId>,
        definition: &JobDefinition,
    ) -> Result<&'static str, Failure> {
        match definition {
            JobDefinition::Native {
                capability_id,
                arguments,
            } => {
                let op = self.catalog.resolve(capability_id).ok_or_else(|| {
                    refuse(
                        DomainErrorCode::UnsupportedTarget,
                        "capability",
                        Some(capability_id),
                    )
                })?;
                let descriptor = registry::find(op).ok_or_else(|| invalid("capability"))?;
                let revision = descriptor
                    .revisions
                    .iter()
                    .copied()
                    .find(|r| i64::from(r.0) == arguments.schema.revision.get())
                    .ok_or_else(|| refuse(DomainErrorCode::SchemaMismatch, "arguments", None))?;
                let expected =
                    argument_schema(op, revision).ok_or_else(|| invalid("capability"))?;
                let adapter = native_help::argument_schema(op, revision).is_some_and(|schema| {
                    schema.schema_id == arguments.schema.schema_id
                        && schema.sha256 == arguments.schema.sha256
                });
                if !adapter
                    && (expected.schema_id.as_str() != arguments.schema.schema_id.as_str()
                        || expected.sha256 != arguments.schema.sha256)
                {
                    return Err(refuse(DomainErrorCode::SchemaMismatch, "arguments", None));
                }
                if adapter && native_help::is_empty(&arguments.value.0) {
                    return Ok("Native");
                }
                // The arguments are the operation's input body: decoded strictly by its own
                // reader, so a job can never hold arguments its capability would refuse.
                let mut body = arguments.value.0.clone();
                let Some(map) = body.as_object_mut() else {
                    return Err(invalid("arguments"));
                };
                if map.contains_key("kind") {
                    return Err(invalid("arguments"));
                }
                let binding = descriptor
                    .binding(Mode::Command)
                    .ok_or_else(|| invalid("capability"))?;
                (binding.decode)(body, revision).map_err(|_| invalid("arguments"))?;
                Ok("Native")
            }
            JobDefinition::Inference {
                prompt,
                route,
                project_id,
                feature_id,
                ..
            } => {
                // A project job's task stays in its project; an app-level task may target one.
                let target = match (project, project_id) {
                    (Some(p), Some(t)) if p != t => return Err(invalid("project")),
                    (Some(p), _) => Some(p.clone()),
                    (None, Some(t)) => {
                        if !project_visible(ctx, Some(t)) {
                            return Err(not_found("project", &t.0).into());
                        }
                        match tx.runs().project_lifecycle(t)? {
                            None => return Err(not_found("project", &t.0).into()),
                            Some(l) if project_closed(Some(&l)) => {
                                return Err(refuse(
                                    DomainErrorCode::Conflict,
                                    "project_closed",
                                    Some(&t.0),
                                ));
                            }
                            Some(_) => {}
                        }
                        Some(t.clone())
                    }
                    (None, None) => None,
                };
                if let Some(f) = feature_id {
                    let root = tx.sessions().work_root(f)?;
                    match (&root, &target) {
                        (Some(r), Some(t))
                            if &r.project_id == t
                                && crate::services::runs::scope::is_feature(r) => {}
                        (Some(_), None) => return Err(invalid("feature")),
                        _ => return Err(not_found("feature", &f.0).into()),
                    }
                }
                let version = tx.artifacts().version(&prompt.version_id)?;
                let artifact = tx.artifacts().artifact(&prompt.artifact_id)?;
                let visible = match (&version, &artifact) {
                    (Some(v), Some(a)) => {
                        v.artifact_id == a.id
                            && v.deleted_at_ms.is_none()
                            && v.blob_digest == prompt.sha256.0
                            && a.state == crate::ports::storage::ArtifactState::Live
                            && match a.scope.project() {
                                Some(p) => {
                                    target.as_ref() == Some(p)
                                        || project.is_none() && project_visible(ctx, Some(p))
                                }
                                None => true,
                            }
                    }
                    _ => false,
                };
                if !visible {
                    return Err(not_found("prompt", &prompt.artifact_id).into());
                }
                if let RouteIntent::Model { model_id } = route
                    && crate::services::router::latest::alias_family(model_id.as_str()).is_none()
                    && tx.router().route_model(model_id)?.is_none()
                {
                    return Err(not_found("route_model", model_id).into());
                }
                Ok("Inference")
            }
        }
    }

    /// The scope of a new job, checked against the invocation and the project's lifecycle.
    fn scope_of(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        scope: &DataScope,
    ) -> Result<Option<ProjectId>, Failure> {
        match scope {
            DataScope::Workspace => {
                if ctx.project_id().is_some() {
                    return Err(refuse(DomainErrorCode::PermissionDenied, "scope", None));
                }
                Ok(None)
            }
            DataScope::Project { project_id } => {
                crate::services::sessions::require_project(ctx, project_id)?;
                match tx.runs().project_lifecycle(project_id)? {
                    None => Err(not_found("project", &project_id.0).into()),
                    Some(l) if project_closed(Some(&l)) => Err(refuse(
                        DomainErrorCode::Conflict,
                        "project_closed",
                        Some(&project_id.0),
                    )),
                    Some(_) => Ok(Some(project_id.clone())),
                }
            }
        }
    }

    /// The next occurrence a schedule produces after `now`, refusing an enabled calendar
    /// schedule that produces none (CodeGraph: "does not produce a future occurrence").
    fn first_occurrence(plan: &Plan, anchor: i64, now: i64) -> Result<Option<i64>, Failure> {
        match (plan, plan.next_after(anchor, now)) {
            (Plan::Calendar { .. }, None) => Err(invalid("schedule")),
            (_, next) => Ok(next),
        }
    }

    fn create(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::CreateInput,
    ) -> Result<DomainDecision, Failure> {
        let project = self.scope_of(tx, ctx, &i.scope)?;
        let title = i.title.0.trim();
        if title.is_empty() {
            return Err(invalid("title"));
        }
        let (plan, zone) = plan_of(&i.schedule)?;
        let kind = self.check_definition(tx, ctx, project.as_ref(), &i.definition)?;
        let human = authorizes(ctx);
        if i.enabled && !human {
            return Err(refuse(
                DomainErrorCode::ApprovalRequired,
                "job_authorization",
                None,
            ));
        }
        let now = tx.now_ms();
        let first = Self::first_occurrence(&plan, now, now)?;
        let job = StoredJob {
            id: tx.new_id(IdKind::Job),
            revision: 1,
            project_id: project.clone(),
            title: title.to_owned(),
            kind: kind.into(),
            schedule: canonical(&i.schedule),
            timezone: zone,
            definition: canonical(&i.definition),
            enabled: i.enabled,
            authorized: human,
            authorized_by: human.then(|| authorizer(ctx)),
            hold: None,
            activated_epoch: tx.store().restore_epoch,
            project_epoch: project_epoch(
                tx,
                target_project(project.as_ref(), &canonical(&i.definition)).as_ref(),
            )?,
            anchor_at_ms: now,
            next_at_ms: if i.enabled { first } else { None },
            interrupted_run_id: None,
            created_at_ms: now,
            updated_at_ms: now,
            deleted_at_ms: None,
        };
        let job = match tx.jobs().insert_job(&job) {
            Err(StorageError::Duplicate {
                kind: "job_title", ..
            }) => {
                return Err(refuse(DomainErrorCode::Conflict, "job_title", None));
            }
            other => other?,
        };
        event(
            tx,
            ctx.principal(),
            job.project_id.as_ref(),
            None,
            (JOB_AGGREGATE, job.id.as_str(), job.revision),
            "JobCreated",
            json!({"title": job.title, "kind": job.kind, "enabled": job.enabled, "authorized": job.authorized, "nextAtMs": job.next_at_ms}),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::JobsCreate {
                value: job_in_tx(tx, &job)?,
            },
        })
    }

    fn edit(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::EditInput,
    ) -> Result<DomainDecision, Failure> {
        let before = visible_job(tx.jobs(), ctx, &i.job_id)?;
        if before.revision != i.expected_revision.get() {
            return Err(conflict(
                JOB,
                &before.id,
                i.expected_revision.get(),
                before.revision,
            ));
        }
        let human = authorizes(ctx);
        if !human && (before.enabled || before.authorized) {
            return Err(refuse(
                DomainErrorCode::ApprovalRequired,
                "job_authorization",
                Some(&before.id),
            ));
        }
        let title = i.title.0.trim();
        if title.is_empty() {
            return Err(invalid("title"));
        }
        let (plan, zone) = plan_of(&i.schedule)?;
        let kind = self.check_definition(tx, ctx, before.project_id.as_ref(), &i.definition)?;
        let schedule = canonical(&i.schedule);
        let definition = canonical(&i.definition);
        if title == before.title
            && schedule == before.schedule
            && definition == before.definition
            && before.authorized == human
        {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::JobsEdit {
                    value: job_in_tx(tx, &before)?,
                },
            });
        }
        let now = tx.now_ms();
        let first = Self::first_occurrence(&plan, now, now)?;
        let mut after = before.clone();
        after.title = title.to_owned();
        after.kind = kind.into();
        after.schedule = schedule;
        after.timezone = zone;
        after.definition = definition;
        let target = job_target(&after);
        if target != job_target(&before) {
            // Another target project: its own archive epoch is the one the edit authorizes.
            after.project_epoch = project_epoch(tx, target.as_ref())?;
        }
        after.authorized = human;
        after.authorized_by = human.then(|| authorizer(ctx));
        after.anchor_at_ms = now;
        after.next_at_ms = if after.enabled && after.hold.is_none() {
            first
        } else {
            None
        };
        after.updated_at_ms = now;
        let after = match tx.jobs().update_job(&after, before.revision) {
            Err(StorageError::Duplicate {
                kind: "job_title", ..
            }) => {
                return Err(refuse(
                    DomainErrorCode::Conflict,
                    "job_title",
                    Some(&before.id),
                ));
            }
            other => other?,
        };
        event(
            tx,
            ctx.principal(),
            after.project_id.as_ref(),
            None,
            (JOB_AGGREGATE, after.id.as_str(), after.revision),
            "JobEdited",
            json!({"title": after.title, "authorized": after.authorized, "nextAtMs": after.next_at_ms}),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::JobsEdit {
                value: job_in_tx(tx, &after)?,
            },
        })
    }

    fn enable(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::EnableInput,
    ) -> Result<DomainDecision, Failure> {
        let before = visible_job(tx.jobs(), ctx, &i.job_id)?;
        if before.revision != i.expected_revision.get() {
            return Err(conflict(
                JOB,
                &before.id,
                i.expected_revision.get(),
                before.revision,
            ));
        }
        if !authorizes(ctx) {
            return Err(refuse(
                DomainErrorCode::ApprovalRequired,
                "job_authorization",
                Some(&before.id),
            ));
        }
        let target = job_target(&before);
        if let Some(p) = &target
            && project_closed(tx.runs().project_lifecycle(p)?.as_deref())
        {
            return Err(refuse(
                DomainErrorCode::Conflict,
                "project_closed",
                Some(&p.0),
            ));
        }
        let epoch = tx.store().restore_epoch;
        if before.enabled
            && before.authorized
            && before.hold.is_none()
            && before.activated_epoch == epoch
            && before.project_epoch == project_epoch(tx, target.as_ref())?
        {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::JobsEnable {
                    value: job_in_tx(tx, &before)?,
                },
            });
        }
        let plan = stored_plan(&before)?;
        let now = tx.now_ms();
        let first = Self::first_occurrence(&plan, now, now)?;
        let reactivated = before.hold.clone();
        let mut after = before.clone();
        after.enabled = true;
        after.authorized = true;
        after.authorized_by = Some(authorizer(ctx));
        after.hold = None;
        after.activated_epoch = epoch;
        after.project_epoch = project_epoch(tx, target.as_ref())?;
        after.anchor_at_ms = now;
        after.next_at_ms = first;
        after.updated_at_ms = now;
        let after = tx.jobs().update_job(&after, before.revision)?;
        event(
            tx,
            ctx.principal(),
            after.project_id.as_ref(),
            None,
            (JOB_AGGREGATE, after.id.as_str(), after.revision),
            "JobEnabled",
            json!({"reactivatedFrom": reactivated, "nextAtMs": after.next_at_ms}),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::JobsEnable {
                value: job_in_tx(tx, &after)?,
            },
        })
    }

    fn disable(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::DisableInput,
    ) -> Result<DomainDecision, Failure> {
        let before = visible_job(tx.jobs(), ctx, &i.job_id)?;
        if before.revision != i.expected_revision.get() {
            return Err(conflict(
                JOB,
                &before.id,
                i.expected_revision.get(),
                before.revision,
            ));
        }
        if !before.enabled {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::JobsDisable {
                    value: job_in_tx(tx, &before)?,
                },
            });
        }
        let mut after = before.clone();
        after.enabled = false;
        after.next_at_ms = None;
        after.updated_at_ms = tx.now_ms();
        let after = tx.jobs().update_job(&after, before.revision)?;
        event(
            tx,
            ctx.principal(),
            after.project_id.as_ref(),
            None,
            (JOB_AGGREGATE, after.id.as_str(), after.revision),
            "JobDisabled",
            json!({}),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::JobsDisable {
                value: job_in_tx(tx, &after)?,
            },
        })
    }

    fn delete(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::DeleteInput,
    ) -> Result<(DomainDecision, Option<OperationId>), Failure> {
        let before = visible_job(tx.jobs(), ctx, &i.job_id)?;
        if before.revision != i.expected_revision.get() {
            return Err(conflict(
                JOB,
                &before.id,
                i.expected_revision.get(),
                before.revision,
            ));
        }
        let now = tx.now_ms();
        // Deletion fences new runs (the tombstone is never due) and cancels the active run in
        // the same unit of work: one never dispatched ends here, a dispatched one is asked to
        // stop and its outcome is recorded when known.
        let cancelled = match tx.jobs().active_run(&before.id)? {
            Some(run) => Some(runner::request_cancel(
                tx,
                ctx.principal(),
                &run,
                "the job was deleted",
            )?),
            None => None,
        };
        let mut after = before.clone();
        after.enabled = false;
        after.next_at_ms = None;
        after.hold = None;
        after.deleted_at_ms = Some(now);
        after.updated_at_ms = now;
        let after = tx.jobs().update_job(&after, before.revision)?;
        event(
            tx,
            ctx.principal(),
            after.project_id.as_ref(),
            None,
            (JOB_AGGREGATE, after.id.as_str(), after.revision),
            "JobDeleted",
            json!({"cancelledRunId": cancelled.as_ref().map(|r| r.id.as_str())}),
        )?;
        Ok((
            DomainDecision::Applied {
                result: OperationResult::JobsDelete {
                    value: job_in_tx(tx, &after)?,
                },
            },
            None,
        ))
    }

    fn run_now(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::RunNowInput,
    ) -> Result<(DomainDecision, Option<OperationId>), Failure> {
        let job = visible_job(tx.jobs(), ctx, &i.job_id)?;
        if job.revision != i.expected_revision.get() {
            return Err(conflict(
                JOB,
                &job.id,
                i.expected_revision.get(),
                job.revision,
            ));
        }
        if let Some(p) = &job_target(&job)
            && project_closed(tx.runs().project_lifecycle(p)?.as_deref())
        {
            return Err(refuse(
                DomainErrorCode::Conflict,
                "project_closed",
                Some(&p.0),
            ));
        }
        // Another principal may run only what a person authorized under the current epochs:
        // not while a hold keeps it stopped, and not when it was activated before a backup
        // restore or an archival of its project (a disabled job is never held, so its epochs
        // are checked here). A person's Run now is itself the authorization.
        if !authorizes(ctx)
            && (!job.authorized || job.hold.is_some() || runner::fence_of(tx, &job)?.is_some())
        {
            return Err(refuse(
                DomainErrorCode::ApprovalRequired,
                "job_authorization",
                Some(&job.id),
            ));
        }
        let now = tx.now_ms();
        let run = runner::queue_run(
            tx,
            ctx.principal(),
            &job,
            runner::Occurrence::Manual { at_ms: now },
            authorizes(ctx),
        )?;
        let operation = run.operation_id.clone();
        Ok((
            DomainDecision::Applied {
                result: OperationResult::JobsRunNow {
                    value: run_in_tx(tx, &run)?,
                },
            },
            operation,
        ))
    }

    fn cancel(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::CancelInput,
    ) -> Result<(DomainDecision, Option<OperationId>), Failure> {
        let job = visible_job(tx.jobs(), ctx, &i.job_id)?;
        let run = match tx.jobs().job_run(&i.run_id)? {
            Some(r) if r.job_id == job.id => r,
            _ => return Err(not_found(RUN, &i.run_id).into()),
        };
        if run.revision != i.expected_revision.get() {
            return Err(conflict(
                RUN,
                &run.id,
                i.expected_revision.get(),
                run.revision,
            ));
        }
        if !is_active(&run.status) || run.cancel_requested {
            let operation = run.operation_id.clone();
            return Ok((
                DomainDecision::NoChange {
                    result: OperationResult::JobsCancel {
                        value: run_in_tx(tx, &run)?,
                    },
                },
                operation,
            ));
        }
        let after = runner::request_cancel(tx, ctx.principal(), &run, "cancelled")?;
        let operation = after.operation_id.clone();
        if is_active(&after.status) {
            // Dispatched: the scheduler stops it through the process owner and records the
            // verified outcome; until then the cancellation is accepted, not done.
            let op = operation.clone().ok_or(StorageError::InvalidRecord(
                "an active run has its operation".into(),
            ))?;
            return Ok((DomainDecision::Accepted { operation_id: op }, operation));
        }
        Ok((
            DomainDecision::Applied {
                result: OperationResult::JobsCancel {
                    value: run_in_tx(tx, &after)?,
                },
            },
            operation,
        ))
    }

    // -----------------------------------------------------------------------------------------
    // Queries

    fn list(
        &self,
        s: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::ListInput,
    ) -> Result<OperationResult, Failure> {
        let (scope, key) = match &i.scope {
            DataScope::Workspace => {
                if ctx.project_id().is_some() {
                    return Err(refuse(DomainErrorCode::PermissionDenied, "scope", None));
                }
                (JobScope::App, "workspace".to_owned())
            }
            DataScope::Project { project_id } => {
                crate::services::sessions::require_project(ctx, project_id)?;
                (
                    JobScope::Project(project_id.clone()),
                    format!("project:{}", project_id.0.as_str()),
                )
            }
        };
        let filter = match i.enabled {
            None => "any",
            Some(true) => "enabled",
            Some(false) => "disabled",
        };
        let binding = CursorBinding::new(ctx, "jobs.list", &[&key, filter], 1);
        let after: Option<(String, String)> = match &i.page.cursor {
            None => None,
            Some(c) => {
                let at = binding.decode(&c.0)?;
                let (title, id) = parse_list_position(&at).ok_or(StorageError::CursorExpired)?;
                Some((title, id))
            }
        };
        let limit = i.page.limit.get() as u16;
        let rows = s.jobs().jobs(
            &scope,
            i.enabled,
            after.as_ref().map(|(t, id)| (t.as_str(), id.as_str())),
            limit,
        )?;
        let more = rows.len() == usize::from(limit)
            && rows.last().is_some_and(|l| {
                s.jobs()
                    .jobs(&scope, i.enabled, Some((&l.title, l.id.as_str())), 1)
                    .is_ok_and(|n| !n.is_empty())
            });
        let mut items = Vec::new();
        for j in &rows {
            items.push(build_job(j, job_facts(s.jobs(), j)?)?);
        }
        let next_cursor = match (more, rows.last()) {
            (true, Some(l)) => Some(BoundedText(binding.encode(&list_position(&l.title, &l.id)))),
            _ => None,
        };
        Ok(OperationResult::JobsList {
            value: Page {
                items: BoundedList(items),
                has_more: next_cursor.is_some(),
                next_cursor,
            },
        })
    }

    fn inspect(
        &self,
        s: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::InspectInput,
    ) -> Result<OperationResult, Failure> {
        let job = visible_job(s.jobs(), ctx, &i.job_id)?;
        let selected = match &i.run_id {
            Some(id) => match s.jobs().job_run(id)? {
                Some(r) if r.job_id == job.id => Some(r),
                _ => return Err(not_found(RUN, id).into()),
            },
            None => s.jobs().job_runs(&job.id, None, 1)?.pop(),
        };
        let usage_binding = selected.as_ref().map(|r| {
            CursorBinding::new(
                ctx,
                "jobs.inspect.usage",
                &[job.id.as_str(), r.id.as_str()],
                1,
            )
        });
        let usage = match (&i.usage, &usage_binding) {
            (Some(p), Some(b)) => Some((page_after(p, b)?, p.limit.get() as u16)),
            _ => None,
        };
        let run = selected
            .as_ref()
            .map(|r| run_in_snapshot(s, r, usage, usage_binding.as_ref()))
            .transpose()?;
        let (runs, runs_next_cursor) = match &i.history {
            None => (None, None),
            Some(p) => {
                let binding =
                    CursorBinding::new(ctx, "jobs.inspect.history", &[job.id.as_str()], 1);
                let before = page_after(p, &binding)?;
                let limit = p.limit.get() as u16;
                let rows = s.jobs().job_runs(&job.id, before, limit)?;
                let more = rows.len() == usize::from(limit)
                    && rows
                        .last()
                        .map(|l| s.jobs().job_runs(&job.id, Some(l.seq), 1))
                        .transpose()?
                        .is_some_and(|n| !n.is_empty());
                let mut items = Vec::new();
                for r in &rows {
                    items.push(run_in_snapshot(s, r, None, None)?);
                }
                let cursor = match (more, rows.last()) {
                    (true, Some(l)) => Some(BoundedText(binding.encode(&l.seq.to_string()))),
                    _ => None,
                };
                (Some(BoundedList(items)), cursor)
            }
        };
        Ok(OperationResult::JobsInspect {
            value: JobInspection {
                job: build_job(&job, job_facts(s.jobs(), &job)?)?,
                run,
                runs,
                runs_next_cursor,
            },
        })
    }
}

/// The position a job listing continues after: the last job's ID, then its title. An ID never
/// contains a newline, so the first newline always ends it, whatever the title holds.
fn list_position(title: &str, id: &Id) -> String {
    format!("{}\n{title}", id.as_str())
}
fn parse_list_position(at: &str) -> Option<(String, String)> {
    let (id, title) = at.split_once('\n')?;
    Id::new(id).ok()?;
    Some((title.to_owned(), id.to_owned()))
}

/// The sequence a page continues after (`None`: the first page).
fn page_after(p: &PageRequest, binding: &CursorBinding) -> Result<Option<i64>, StorageError> {
    match &p.cursor {
        None => Ok(None),
        Some(c) => binding
            .decode(&c.0)?
            .parse()
            .map(Some)
            .map_err(|_| StorageError::CursorExpired),
    }
}

impl QueryHandler for JobService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let correlation = self.correlation(ctx);
        let answered = match query.input() {
            OperationInput::Jobs(ops::Input::List(i)) => self.list(snapshot, ctx, i),
            OperationInput::Jobs(ops::Input::Inspect(i)) => self.inspect(snapshot, ctx, i),
            _ => {
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into())
            }
        };
        let result = answered.map_err(|f| query_fault(f, JOB, correlation))?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(1)),
            result,
        })
    }
}

impl CommandHandler for JobService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let OperationInput::Jobs(input) = command.input() else {
            return crate::services::sessions::decided(
                tx,
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into()),
            );
        };
        if ctx.reviewed_operation().is_some()
            && let Err(f) = self.validate_reviewed_consent(tx, ctx, command)
        {
            return crate::services::sessions::decided(tx, Err(f));
        }
        let outcome = match input {
            ops::Input::Create(i) => self.create(tx, ctx, i).map(|d| (d, None)),
            ops::Input::Edit(i) => self.edit(tx, ctx, i).map(|d| (d, None)),
            ops::Input::Enable(i) => self.enable(tx, ctx, i).map(|d| (d, None)),
            ops::Input::Disable(i) => self.disable(tx, ctx, i).map(|d| (d, None)),
            ops::Input::Delete(i) => self.delete(tx, ctx, i),
            ops::Input::RunNow(i) => self.run_now(tx, ctx, i),
            ops::Input::Cancel(i) => self.cancel(tx, ctx, i),
            _ => {
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into())
            }
        };
        match outcome {
            Ok((decision, operation_id)) => Ok(Decided {
                decision,
                operation_id,
            }),
            Err(f) => {
                let mut d = crate::services::sessions::decided(tx, Err(f))?;
                d.operation_id = None;
                Ok(d)
            }
        }
    }
}

/// The record of a job (tests and host code reading outside a gateway query).
pub fn record_of(s: &dyn ReadSnapshot, job: &StoredJob) -> Result<JobRecord, StorageError> {
    build_job(job, job_facts(s.jobs(), job)?)
}
/// The record of a run with its first usage page.
pub fn run_record_of(s: &dyn ReadSnapshot, run: &StoredJobRun) -> Result<JobRun, StorageError> {
    run_in_snapshot(s, run, None, None)
}
/// The operation journal entry a new run records before anything is dispatched.
pub(crate) fn run_operation(id: OperationId, job: &StoredJob, run_id: &Id) -> NewOperation {
    NewOperation {
        id,
        root_id: None,
        assignment_id: None,
        capability: RUN_CAPABILITY.into(),
        schema_version: 1,
        args_digest: sha256_hex(
            format!(
                "{}\n{}\n{}",
                job.id.as_str(),
                run_id.as_str(),
                job.definition
            )
            .as_bytes(),
        ),
        payload: json!({"jobId": job.id.as_str(), "runId": run_id.as_str(), "jobRevision": job.revision}),
    }
}
/// Whether a run's operation has certainly not dispatched.
pub(crate) fn undispatched(
    ops: &dyn OperationRead,
    run: &StoredJobRun,
) -> Result<bool, StorageError> {
    Ok(match &run.operation_id {
        None => true,
        Some(op) => ops
            .operation(op)?
            .is_none_or(|o| cancellable_before_dispatch(o.state)),
    })
}

fn authorizer(ctx: &InvocationContext) -> crate::protocol::ActorId {
    ctx.reviewed_operation()
        .map_or_else(|| ctx.principal().clone(), |r| r.approver().clone())
}
impl JobService {
    fn validate_reviewed_consent(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        c: &AdmittedCommand,
    ) -> Result<(), Failure> {
        let r = ctx
            .reviewed_operation()
            .ok_or_else(|| invalid("reviewed_consent"))?;
        if !crate::services::approvals::requests::reviewed_job(c.descriptor().name)
            || r.operation() != c.descriptor().name
            || r.request_digest() != c.fingerprint().as_str()
            || r.schema_revision() != c.schema_revision().0
        {
            return Err(refuse(
                DomainErrorCode::PermissionDenied,
                "reviewed_consent",
                None,
            ));
        }
        let p = tx
            .decisions()
            .approval(r.approval_id())?
            .ok_or_else(|| not_found("approval", r.approval_id()))?;
        let a = tx
            .decisions()
            .human_decision(
                crate::ports::storage::ApprovalSource::Generic,
                r.approval_id(),
            )?
            .ok_or_else(|| not_found("human_decision", r.approval_id()))?;
        let out = tx
            .decisions()
            .approval_resume(r.approval_id())?
            .ok_or_else(|| not_found("approval_resume", r.approval_id()))?;
        if p.state != "Approved"
            || a.decision != "Approved"
            || a.actor != *r.approver()
            || p.author != *ctx.principal()
            || p.author_kind != ctx.principal_kind()
            || p.decided_by.as_ref() != Some(r.approver())
            || p.content_digest != c.fingerprint().as_str()
            || p.payload["binding"] != *r.owner_binding()
            || out.request_digest != c.fingerprint().as_str()
            || out.principal != *ctx.principal()
            || out.principal_kind != ctx.principal_kind()
            || tx.actors().actor_kind(r.approver())? != Some(PrincipalKind::Human)
            || !out.grant_id.as_ref().is_some_and(|g| {
                c.authorization()
                    .authority_grants()
                    .iter()
                    .any(|a| &a.grant_id == g)
            })
        {
            return Err(refuse(
                DomainErrorCode::PermissionDenied,
                "reviewed_consent",
                None,
            ));
        }
        if let Some(id) = ctx.root_id() {
            let root = tx
                .sessions()
                .work_root(id)?
                .ok_or_else(|| not_found("root", &id.0))?;
            let target = ctx
                .assignment_id()
                .map_or(crate::services::runs::Target::Root, |a| {
                    crate::services::runs::Target::Assignment(a.clone())
                });
            let control = self.approval_control.as_ref().ok_or_else(|| {
                refuse(DomainErrorCode::PermissionDenied, "reviewed_control", None)
            })?;
            let eligibility = control.evaluate(
                tx.runs(),
                &root,
                &target,
                crate::ports::storage::ActivationPurpose::Implementation,
            )?;
            if !eligibility.eligible {
                return Err(StorageError::Busy.into());
            }
        }
        let job = match c.input() {
            OperationInput::Jobs(ops::Input::Edit(i)) => Some(i.job_id.clone()),
            OperationInput::Jobs(ops::Input::Enable(i)) => Some(i.job_id.clone()),
            OperationInput::Jobs(ops::Input::RunNow(i)) => Some(i.job_id.clone()),
            _ => None,
        };
        if let Some(id) = job {
            let job = tx.jobs().job(&id)?.ok_or_else(|| not_found(JOB, &id))?;
            if job.hold.is_some()
                || runner::fence_of(tx, &job)?
                    .is_some_and(|(kind, _)| kind != "AuthorizationRequired")
            {
                return Err(StorageError::Busy.into());
            }
        }
        if p.payload["authority"] != crate::services::approvals::authority_binding(tx, ctx)? {
            return Err(Refusal::new(
                DomainErrorCode::SourceChanged,
                "reviewed_job_authority",
                Some(p.id.clone()),
            )
            .retry(crate::protocol::RetryAdvice::RefreshAndNewRequest)
            .into());
        }
        let request = crate::services::approvals::requests::decode(
            c.descriptor().name,
            c.schema_revision().0,
            &out.input,
            true,
        )
        .map_err(|_| invalid("reviewed_job_request"))?;
        let saved = crate::services::approvals::OwnerPreview {
            scope: p.scope.clone(),
            target_id: p.target_id.clone(),
            target_revision: p.target_revision,
            summary: p.summary.clone(),
            reviewed: p.payload["reviewed"].clone(),
            binding: p.payload["binding"].clone(),
        };
        let raw = crate::services::approvals::review_evidence::expand(tx.artifacts(), &saved)?;
        self.validate_approval_preview(tx, ctx, &request, &raw)?;
        Ok(())
    }
}

// Owner-derived saved-operation review (DW-14). This deliberately shares check_definition,
// scope_of, plan_of and the occurrence validator with ordinary job commands.
struct JobApprovalCandidate {
    scope: crate::ports::storage::ConfigScope,
    target: Id,
    revision: i64,
    title: String,
    schedule: Schedule,
    definition: JobDefinition,
    enabled: bool,
    before: Option<StoredJob>,
}
fn job_approval_candidate(
    read: &dyn JobRead,
    ctx: &InvocationContext,
    input: &OperationInput,
) -> Result<JobApprovalCandidate, Failure> {
    use crate::ports::storage::ConfigScope;
    let (before, title, schedule, definition, enabled, scope) = match input {
        OperationInput::Jobs(ops::Input::Create(i)) => {
            let scope = match &i.scope {
                DataScope::Workspace => ConfigScope::Workspace,
                DataScope::Project { project_id } => ConfigScope::Project(project_id.clone()),
            };
            (
                None,
                i.title.0.clone(),
                i.schedule.clone(),
                i.definition.clone(),
                i.enabled,
                scope,
            )
        }
        OperationInput::Jobs(ops::Input::Edit(i)) => {
            let before = visible_job(read, ctx, &i.job_id)?;
            if before.revision != i.expected_revision.get() {
                return Err(conflict(
                    JOB,
                    &before.id,
                    i.expected_revision.get(),
                    before.revision,
                ));
            }
            let scope = before
                .project_id
                .clone()
                .map_or(ConfigScope::Workspace, ConfigScope::Project);
            let enabled = before.enabled;
            (
                Some(before),
                i.title.0.clone(),
                i.schedule.clone(),
                i.definition.clone(),
                enabled,
                scope,
            )
        }
        OperationInput::Jobs(ops::Input::Enable(i)) => {
            let before = visible_job(read, ctx, &i.job_id)?;
            if before.revision != i.expected_revision.get() {
                return Err(conflict(
                    JOB,
                    &before.id,
                    i.expected_revision.get(),
                    before.revision,
                ));
            }
            let scope = before
                .project_id
                .clone()
                .map_or(ConfigScope::Workspace, ConfigScope::Project);
            (
                Some(before.clone()),
                before.title.clone(),
                parse(&before.schedule)?,
                parse(&before.definition)?,
                true,
                scope,
            )
        }
        OperationInput::Jobs(ops::Input::RunNow(i)) => {
            let before = visible_job(read, ctx, &i.job_id)?;
            if before.revision != i.expected_revision.get() {
                return Err(conflict(
                    JOB,
                    &before.id,
                    i.expected_revision.get(),
                    before.revision,
                ));
            }
            let scope = before
                .project_id
                .clone()
                .map_or(ConfigScope::Workspace, ConfigScope::Project);
            (
                Some(before.clone()),
                before.title.clone(),
                parse(&before.schedule)?,
                parse(&before.definition)?,
                before.enabled,
                scope,
            )
        }
        _ => {
            return Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "saved_job_operation",
                None,
            ));
        }
    };
    let title = title.trim().to_owned();
    if title.is_empty() {
        return Err(invalid("title"));
    }
    let (plan, _) = plan_of(&schedule)?;
    // The actual command recomputes its future occurrence at dispatch time; absolute time is
    // intentionally not a review binding (reviewing a schedule must not make it stale by aging).
    let _ = plan;
    if let Some(other) = read.job_titled(scope.project_id(), &title)?
        && before.as_ref().is_none_or(|prior| prior.id != other.id)
    {
        return Err(refuse(
            DomainErrorCode::Conflict,
            "job_title",
            Some(&other.id),
        ));
    }
    let (target, revision) = before
        .as_ref()
        .map_or_else(|| (scope.target_id(), 1), |j| (j.id.clone(), j.revision));
    Ok(JobApprovalCandidate {
        scope,
        target,
        revision,
        title,
        schedule,
        definition,
        enabled,
        before,
    })
}
fn job_approval_config(job: &StoredJob) -> Value {
    // Scheduler bookkeeping does not advance configuration revision and is not authorized by
    // a saved operation. Independent holds and authorization generations DO belong here.
    json!({"id":job.id,"revision":job.revision,"projectId":job.project_id,"title":job.title,
        "kind":job.kind,"schedule":job.schedule,"definition":job.definition,"enabled":job.enabled,
        "authorized":job.authorized,"authorizedBy":job.authorized_by,"hold":job.hold,
        "activatedEpoch":job.activated_epoch,"projectEpoch":job.project_epoch,"deletedAtMs":job.deleted_at_ms})
}
fn job_approval_project(
    read: &dyn crate::ports::storage::ProjectRead,
    project: Option<&ProjectId>,
) -> Result<Value, Failure> {
    let Some(id) = project else {
        return Ok(Value::Null);
    };
    let p = read
        .project(id)?
        .ok_or_else(|| not_found("project", &id.0))?;
    if p.lifecycle != "Active" {
        return Err(refuse(
            DomainErrorCode::Conflict,
            "project_closed",
            Some(&id.0),
        ));
    }
    Ok(
        json!({"id":p.id,"revision":p.revision,"scopeRevision":p.scope_revision,"archiveEpoch":p.archive_epoch,"lifecycle":p.lifecycle}),
    )
}
fn job_approval_prompt(
    read: &dyn crate::ports::storage::ArtifactRead,
    definition: &JobDefinition,
) -> Result<Value, Failure> {
    match definition {
        JobDefinition::Inference { prompt, .. } => {
            crate::services::approvals::owner_preview::artifact_binding(read, prompt)
        }
        _ => Ok(Value::Null),
    }
}
fn job_approval_route(
    read: &dyn crate::ports::storage::RouterRead,
    definition: &JobDefinition,
) -> Result<Value, Failure> {
    match definition {
        JobDefinition::Inference {
            route: RouteIntent::Model { model_id },
            ..
        } => {
            if crate::services::router::latest::alias_family(model_id.as_str()).is_some() {
                return Ok(json!({"latestAlias":model_id}));
            }
            let model = read
                .route_model(model_id)?
                .ok_or_else(|| not_found("route_model", model_id))?;
            Ok(
                json!({"id":model.id,"revision":model.revision,"enabled":model.enabled,
                "upstreamModelId":model.upstream_model_id,"endpointProfile":model.endpoint_profile,
                "connectionId":model.connection_id,"efforts":model.efforts}),
            )
        }
        JobDefinition::Inference { route, .. } => Ok(json!({"route":route})),
        _ => Ok(Value::Null),
    }
}
fn job_approval_preview(
    candidate: JobApprovalCandidate,
    project: Value,
    prompt: Value,
    route: Value,
) -> crate::services::approvals::OwnerPreview {
    let before = candidate.before.as_ref().map(job_approval_config);
    let mut preview = crate::services::approvals::OwnerPreview {
        scope: candidate.scope,
        target_id: candidate.target,
        target_revision: candidate.revision,
        summary: format!("Review job {}", candidate.title),
        reviewed: json!({"title":candidate.title,"schedule":candidate.schedule,"definition":candidate.definition,"enabled":candidate.enabled,"prior":before,"artifact":match &candidate.definition {JobDefinition::Inference {prompt,..}=>Some(prompt),_=>None}}),
        binding: json!({"prior":before,"project":project,"prompt":prompt,"route":route}),
    };
    if preview.reviewed["artifact"].is_null() {
        preview
            .reviewed
            .as_object_mut()
            .expect("owner preview")
            .remove("artifact");
    }
    preview
}

impl JobService {
    pub(crate) fn approval_preview(
        &self,
        ctx: &InvocationContext,
        request: &crate::admission::DecodedRequest,
        env: &crate::ports::Preparation<'_>,
    ) -> Result<crate::services::approvals::OwnerPreview, Failure> {
        env.read(|snapshot| {
            let result = (|| {
                let candidate = job_approval_candidate(snapshot.jobs(), ctx, &request.input)?;
                match candidate.scope.project_id() {
                    Some(p) => crate::services::sessions::require_project(ctx, p)?,
                    None if ctx.project_id().is_some() => {
                        return Err(refuse(DomainErrorCode::PermissionDenied, "scope", None));
                    }
                    _ => {}
                }
                let target = target_project(
                    candidate.scope.project_id(),
                    &canonical(&candidate.definition),
                );
                let project = job_approval_project(snapshot.projects(), target.as_ref())?;
                let prompt = job_approval_prompt(snapshot.artifacts(), &candidate.definition)?;
                let route = job_approval_route(snapshot.router(), &candidate.definition)?;
                Ok(job_approval_preview(candidate, project, prompt, route))
            })();
            Ok(result)
        })?
    }
    pub(crate) fn validate_approval_preview(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        request: &crate::admission::DecodedRequest,
        preview: &crate::services::approvals::OwnerPreview,
    ) -> Result<(), Failure> {
        let candidate = job_approval_candidate(tx.jobs(), ctx, &request.input)?;
        let scope = match &candidate.scope {
            crate::ports::storage::ConfigScope::Workspace => DataScope::Workspace,
            crate::ports::storage::ConfigScope::Project(project_id) => DataScope::Project {
                project_id: project_id.clone(),
            },
        };
        let project = self.scope_of(tx, ctx, &scope)?;
        self.check_definition(tx, ctx, project.as_ref(), &candidate.definition)?;
        let (plan, _) = plan_of(&candidate.schedule)?;
        Self::first_occurrence(&plan, tx.now_ms(), tx.now_ms())?;
        let target = target_project(
            candidate.scope.project_id(),
            &canonical(&candidate.definition),
        );
        let project = job_approval_project(tx.projects(), target.as_ref())?;
        let prompt = job_approval_prompt(tx.artifacts(), &candidate.definition)?;
        let route = job_approval_route(tx.router(), &candidate.definition)?;
        let current = job_approval_preview(candidate, project, prompt, route);
        if &current != preview {
            return Err(refuse(
                DomainErrorCode::SourceChanged,
                "saved_job_operation",
                Some(&preview.target_id),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_position_round_trips_any_title() {
        for title in ["Docs", "two\nlines", "\n", "trailing\n", "a\nb\nc"] {
            let id = Id::new("job-1").unwrap();
            let at = list_position(title, &id);
            assert_eq!(
                parse_list_position(&at),
                Some((title.to_owned(), "job-1".to_owned())),
                "{title:?}"
            );
        }
        assert_eq!(parse_list_position("no newline"), None);
        assert_eq!(parse_list_position("bad id\ntitle"), None);
    }
}
