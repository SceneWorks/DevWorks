//! Durable schedules and their runs (JOB-01..09): the `schedules` and `job_runs` records of the
//! initial schema, extended by migration 17 with discovery checkpoints, action identities, run
//! links and per-run usage.
//!
//! * **Jobs** are app-level (`project_id` `None`) or project schedules. The repository enforces
//!   compare-and-set revisions, one live title per scope (case-insensitive) and the wire
//!   enumerations; occurrence computation, authority and lifecycle belong to
//!   `crate::services::jobs`, which runs inside one unit of work.
//! * **Runs** have a per-job sequence (newest-first paging), an occurrence identity unique per
//!   job, and at most one *active* run (`Queued`, `Running` or `Holding`) per job: overlap is
//!   impossible even for two writers racing the same occurrence.
//! * **Checkpoints** are per job and source (`github:owner/repo/issues`, ...), compare-and-set.
//! * **Actions** are per job and action identity: a repeated scan or a retry finds the identity it
//!   already recorded and never repeats the action.
//! * **Usage** items are attributed to one run; totals cover every item.
use super::StorageError;
use crate::protocol::{ActorId, Id, OperationId, ProjectId, RootId};

/// Run statuses that hold the job's one active slot.
pub const ACTIVE_RUN_STATUSES: [&str; 3] = ["Queued", "Running", "Holding"];
/// Every run status (`JobRunStatus` wire names).
pub const RUN_STATUSES: [&str; 8] = [
    "Queued",
    "Running",
    "Holding",
    "Succeeded",
    "Failed",
    "Skipped",
    "Cancelled",
    "OutcomeUnknown",
];
/// Why an enabled job does not dispatch (`JobHold` wire names).
pub const JOB_HOLDS: [&str; 3] = [
    "ReactivationRequired",
    "AuthorizationRequired",
    "ProjectClosed",
];
/// Action states: intent recorded (the effect may or may not have happened), done, failed
/// (certainly not applied), or unknown after reconciliation could not establish it.
pub const ACTION_STATES: [&str; 4] = ["Intent", "Done", "Failed", "Unknown"];

/// One schedule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredJob {
    pub id: Id,
    /// Positive; advances on every change of the job's configuration (title, schedule,
    /// definition, enablement, authorization, hold, deletion). Scheduler bookkeeping (the next
    /// occurrence, the interrupted run) changes without advancing it, so a person's edit is not
    /// refused merely because an occurrence ran.
    pub revision: i64,
    /// `None`: an app-level job.
    pub project_id: Option<ProjectId>,
    pub title: String,
    /// `JobKind` wire name of the definition (`Native` or `Inference`).
    pub kind: String,
    /// Canonical JSON of the wire `Schedule`.
    pub schedule: String,
    /// IANA zone of a calendar schedule; `UTC` otherwise.
    pub timezone: String,
    /// Canonical JSON of the wire `JobDefinition`.
    pub definition: String,
    pub enabled: bool,
    /// A person authorized the current definition (create/edit/enable by the user).
    pub authorized: bool,
    pub authorized_by: Option<ActorId>,
    /// One of [`JOB_HOLDS`].
    pub hold: Option<String>,
    /// The store restore epoch the job was last activated under; a later epoch (a restored
    /// backup) holds it for explicit reactivation.
    pub activated_epoch: i64,
    /// The project's archive epoch the job was last activated under; a later epoch (the project
    /// was archived since, even if restored) holds it for explicit reactivation. 0 for app jobs.
    pub project_epoch: i64,
    /// Anchor of an interval schedule (UTC ms).
    pub anchor_at_ms: i64,
    /// The next occurrence the scheduler will claim; `None` while nothing is due (manual,
    /// disabled, held or deleted).
    pub next_at_ms: Option<i64>,
    /// A run that was active when the app closed, resumed at the next boot.
    pub interrupted_run_id: Option<Id>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub deleted_at_ms: Option<i64>,
}

/// One run of a job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredJobRun {
    pub id: Id,
    pub job_id: Id,
    /// Per-job sequence, 1 for the first run (assigned by the repository on insert).
    pub seq: i64,
    pub project_id: Option<ProjectId>,
    /// The job revision and definition the run was claimed with: a later edit never changes
    /// what an already claimed run executes.
    pub job_revision: i64,
    pub definition: String,
    /// The project's archive epoch the run was authorized under: a run of an earlier epoch never
    /// dispatches (an archival stopped it).
    pub project_epoch: i64,
    /// The store's restore epoch the run was authorized under: a run of an earlier epoch never
    /// dispatches (the store was restored from a backup since).
    pub restore_epoch: i64,
    pub occurrence_key: String,
    pub occurrence_at_ms: i64,
    /// `JobTrigger` wire name.
    pub trigger: String,
    /// One of [`RUN_STATUSES`].
    pub status: String,
    /// Positive; advances on lifecycle changes (claim, start, hold, cancel request, resume,
    /// finish), never on progress alone.
    pub revision: i64,
    /// The run's journal operation (absent for a skipped occurrence).
    pub operation_id: Option<OperationId>,
    /// An asynchronous operation a native run's command was accepted as.
    pub inner_operation_id: Option<OperationId>,
    /// The boot that last owned the run.
    pub boot_id: Option<Id>,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub reason: Option<String>,
    /// A separate hold the run waits on (a user pause, a budget hold, an approval wait); kept
    /// across resumption.
    pub hold: Option<String>,
    pub cancel_requested: bool,
    pub resumptions: i64,
    pub session_id: Option<Id>,
    /// The scheduled agent principal of an inference run.
    pub agent_actor: Option<ActorId>,
    /// The active run an overlapping occurrence was skipped for.
    pub skipped_for: Option<Id>,
    /// Durable progress (an agent checkpoint), JSON.
    pub checkpoint: Option<String>,
    pub created_at_ms: i64,
}

/// Which jobs a listing covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobScope {
    /// App-level jobs.
    App,
    Project(ProjectId),
    /// Every job, any scope.
    All,
}

/// Which runs a listing covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunFilter {
    /// Active runs (any boot).
    Active,
    /// Active runs a boot other than this one owned (or none owned).
    ActiveNotOfBoot(Id),
    /// Active runs with a cancellation request.
    CancelRequested,
}

/// A discovery checkpoint (a source cursor) of one job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredJobCheckpoint {
    pub job_id: Id,
    pub source: String,
    /// JSON cursor.
    pub cursor: String,
    pub revision: i64,
    pub run_id: Id,
    pub updated_at_ms: i64,
}

/// An action identity of one job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredJobAction {
    pub job_id: Id,
    pub key: String,
    pub run_id: Id,
    /// One of [`ACTION_STATES`].
    pub state: String,
    /// What the effect produced or how it was reconciled (JSON).
    pub effect: Option<String>,
    pub revision: i64,
    pub created_at_ms: i64,
    pub settled_at_ms: Option<i64>,
}

/// One attributed usage item of a run (ROUTE-17 price classification).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredJobUsage {
    pub run_id: Id,
    /// Per-run sequence (assigned by `prepare_usage`).
    pub seq: i64,
    pub request_id: Id,
    pub purpose: String,
    pub model_id: Id,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// Canonical decimal USD; `None` for untracked usage.
    pub amount_usd: Option<String>,
    /// `UsagePriceKind` wire name.
    pub price_kind: String,
    /// `PriceOrigin` wire name.
    pub price_origin: String,
    pub price_revision: Option<i64>,
    pub incomplete: bool,
    pub recorded_at_ms: i64,
}

/// Totals over every usage item of a run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JobUsageTotals {
    pub items: i64,
    /// Exact sum of tracked amounts in pico-USD (amounts carry at most 12 decimals).
    pub tracked_pico_usd: i128,
    pub untracked: i64,
    pub incomplete: i64,
}

pub trait JobRead {
    fn job(&self, id: &Id) -> Result<Option<StoredJob>, StorageError>;
    /// Live (not deleted) jobs of `scope` in case-insensitive title order, then ID, strictly
    /// after `after` (title, ID), optionally only `enabled` ones, at most `limit` (1..=500).
    fn jobs(
        &self,
        scope: &JobScope,
        enabled: Option<bool>,
        after: Option<(&str, &str)>,
        limit: u16,
    ) -> Result<Vec<StoredJob>, StorageError>;
    /// The live job of `project` (`None`: app-level) titled `title`, case-insensitively.
    fn job_titled(
        &self,
        project: Option<&ProjectId>,
        title: &str,
    ) -> Result<Option<StoredJob>, StorageError>;
    /// Enabled, unheld, live jobs whose next occurrence is at or before `now_ms`, earliest first.
    fn due_jobs(&self, now_ms: i64, limit: u16) -> Result<Vec<StoredJob>, StorageError>;
    /// Every live job (any state) in ID order strictly after `after`, at most `limit`.
    fn all_jobs(&self, after: Option<&Id>, limit: u16) -> Result<Vec<StoredJob>, StorageError>;
    fn job_run(&self, id: &Id) -> Result<Option<StoredJobRun>, StorageError>;
    /// A job's runs newest first (by sequence) strictly before `before_seq`, at most `limit`.
    fn job_runs(
        &self,
        job: &Id,
        before_seq: Option<i64>,
        limit: u16,
    ) -> Result<Vec<StoredJobRun>, StorageError>;
    /// The job's one active run, if any.
    fn active_run(&self, job: &Id) -> Result<Option<StoredJobRun>, StorageError>;
    /// Runs matching `filter` in ID order strictly after `after`, at most `limit`.
    fn runs_where(
        &self,
        filter: &RunFilter,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredJobRun>, StorageError>;
    /// Work roots a run started or dispatched, in ID order.
    fn run_links(&self, run: &Id) -> Result<Vec<RootId>, StorageError>;
    fn checkpoint(
        &self,
        job: &Id,
        source: &str,
    ) -> Result<Option<StoredJobCheckpoint>, StorageError>;
    fn action(&self, job: &Id, key: &str) -> Result<Option<StoredJobAction>, StorageError>;
    /// A run's actions still in `Intent` (possibly applied, never settled), in key order.
    fn unsettled_actions(&self, run: &Id) -> Result<Vec<StoredJobAction>, StorageError>;
    /// A run's usage items in sequence order strictly after `after_seq`, at most `limit`.
    fn run_usage(
        &self,
        run: &Id,
        after_seq: Option<i64>,
        limit: u16,
    ) -> Result<Vec<StoredJobUsage>, StorageError>;
    fn usage_totals(&self, run: &Id) -> Result<JobUsageTotals, StorageError>;
}

pub trait JobRepository: JobRead {
    /// Inserts a job at revision 1. An existing ID is [`StorageError::Duplicate`] (`job`), a live
    /// title already used in its scope is [`StorageError::Duplicate`] (`job_title`).
    fn insert_job(&mut self, job: &StoredJob) -> Result<StoredJob, StorageError>;
    /// Replaces a job at `expected_revision`, storing `expected_revision + 1`. A stale revision
    /// is [`StorageError::RevisionConflict`]; a taken title is `Duplicate` (`job_title`).
    fn update_job(
        &mut self,
        job: &StoredJob,
        expected_revision: i64,
    ) -> Result<StoredJob, StorageError>;
    /// Sets a job's scheduler bookkeeping (next occurrence, interrupted run) without advancing
    /// its revision, provided its configuration is still at `expected_revision`.
    fn set_job_schedule(
        &mut self,
        job: &Id,
        expected_revision: i64,
        next_at_ms: Option<i64>,
        interrupted_run_id: Option<&Id>,
    ) -> Result<StoredJob, StorageError>;
    /// Inserts a run at revision 1 with the job's next sequence. A repeated occurrence of the
    /// job is `Duplicate` (`job_occurrence`); a second active run is `Duplicate` (`active_run`).
    fn insert_run(&mut self, run: &StoredJobRun) -> Result<StoredJobRun, StorageError>;
    /// Replaces a run at `expected_revision`, storing `expected_revision + 1`.
    fn update_run(
        &mut self,
        run: &StoredJobRun,
        expected_revision: i64,
    ) -> Result<StoredJobRun, StorageError>;
    /// Saves an active run's progress checkpoint (JSON) without advancing its revision.
    fn set_run_checkpoint(&mut self, run: &Id, checkpoint: &str) -> Result<(), StorageError>;
    /// Links a work root to a run (idempotent).
    fn link_root(&mut self, run: &Id, root: &RootId) -> Result<(), StorageError>;
    /// Inserts (`expected_revision` `None`; an existing one is `Duplicate`) or replaces at
    /// `expected_revision` a job's checkpoint for one source.
    fn put_checkpoint(
        &mut self,
        checkpoint: &StoredJobCheckpoint,
        expected_revision: Option<i64>,
    ) -> Result<StoredJobCheckpoint, StorageError>;
    /// Records a new action identity (`Intent`, revision 1); an existing one is `Duplicate`.
    fn insert_action(&mut self, action: &StoredJobAction) -> Result<StoredJobAction, StorageError>;
    /// Replaces an action at `expected_revision`.
    fn update_action(
        &mut self,
        action: &StoredJobAction,
        expected_revision: i64,
    ) -> Result<StoredJobAction, StorageError>;
    /// Validates a usage item of an existing run and returns it with the run's next usage
    /// sequence; a request the run already recorded is `Duplicate` (`job_usage`). It writes
    /// nothing: the caller records the item as a `JobUsage` usage origin.
    fn prepare_usage(&self, usage: &StoredJobUsage) -> Result<StoredJobUsage, StorageError>;
}
