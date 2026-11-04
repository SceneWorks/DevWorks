//! `ControlService`: run root Start, Pause, Continue, Redirect, Stop, Inspect and Eligibility
//! (CONTROL-01..13, AD17–20, AD79) behind `runs.start`, `.pause`, `.continue`, `.redirect`,
//! `.inspect`, `.stop` and `.eligibility`, registered through
//! [`crate::services::UseCases::with_runs`]. The native UI and agent tools reach the same use
//! cases through the gateway; the supervisor, tool broker, delegation and planning items reach the
//! same rules through the transaction-scoped substrate in [`substrate`] and [`recovery`], so no
//! entry path has its own copy of eligibility.
//!
//! # State (AD17)
//!
//! Desired root execution (`Enabled`/`Paused`, with its control revision), the activation phase
//! (`Pending → Claimed → Running → Stopping → Stopped`), assignment state and cause-specific
//! blockers are separate records. Pause and Stop advance the control revision and install a
//! `HumanPause` hold; a restart installs `RestartContinue`; only an authorized Continue that
//! names a hold (by ID and revision) clears it, and nothing clears another cause's blocker. A
//! root has at most one live activation (`executions` unique index) and at most one live
//! coordinator assignment; each session has at most one continuation turn in flight.
//!
//! # Commands
//!
//! * **Start** leaves paused or held work paused (`ContinueRequired` with the hold IDs), returns
//!   the existing live activation unchanged (`NoChange`), and otherwise records a `Pending`
//!   activation. An unapproved or unassessed feature is accepted for planning/assessment only;
//!   a standalone issue for implementation.
//! * **Pause** stops a not-yet-launched activation at once (`Prevented`) and asks a launched one
//!   to stop (`Stopping`, stop requested). The supervisor acknowledges at a safe boundary and
//!   reports `Stopped`: requested, acknowledged and stopped are distinct recorded stages.
//! * **Continue** clears exactly the covered manual-resume holds when every current one is
//!   covered (otherwise `ContinueRequired` naming the uncovered ones; nothing is cleared). A
//!   person may clear them directly; an agent, schedule, package or MCP client only with a
//!   user-minted resume grant covering them, spent with the receipt. Continue then records a
//!   new activation (or a reactivation once the stopping one stops); descendants keep their own
//!   blockers.
//! * **Redirect** saves the instruction (`Received`), advances the control and instruction
//!   revisions and appends it to the coordinator's transcript. The coordinator's next dispatch
//!   is fenced until it acknowledges at a safe boundary; incorporation records the disposition
//!   and supersedes only the assignments it names. It never clears a pause.
//! * **Stop** pauses the root, advances its stop epoch, cancels root-scoped deliveries and
//!   journals a stop operation (`Accepted`) whose outcome the engine records after stopping the
//!   root's owned process trees; nothing live means the stop is observed complete at once.
//!
//! # Retained CodeGraph behavior
//!
//! `AskController.CancelRun` maps to `runs.stop`: an invisible root is `NotFound`; stopping a
//! root that is already stopped answers the recorded stop unchanged (CodeGraph's `Ok(run)` for
//! a terminal run); otherwise the cancellation is requested and accepted while the work stops
//! (CodeGraph's `Accepted(run)`). The run identity, idempotency key reuse and status polling of
//! `StartRun`/`GetRun` map to the receipt replay of `runs.start` and to `runs.inspect`; chats,
//! transcripts, events and streams are the session and history operations (DW-008).
pub mod inspect;
pub mod recovery;
pub mod scope;
pub mod substrate;

use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, runs as ops};
use crate::ports::storage::{
    ActivationFilter, ActivationOutcome, ActivationPhase, ActivationPurpose, BlockerFilter,
    BlockerState, DesiredExecution, GrantRecord, HoldCause, InstructionStage, NewEvent,
    NewOperation, ProcessFilter, QueueState, ReadSnapshot, RunRead, SessionRead, StopCause,
    StorageError, StoredActivation, StoredAssignment, StoredBlocker, StoredInstruction,
    StoredRootControl, StoredStopCoverage, WorkRoot, WriteTransaction, cancellation_applies,
    is_terminal, possibly_dispatched,
};
use crate::ports::{
    CommandHandler, Decided, HandlerFailure, IdKind, IdSource, QueryAnswer, QueryHandler,
};
use crate::protocol::records::{EligibilitySnapshot, MutationSummary, OperationRef, RootSnapshot};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::BoundedList;
use crate::protocol::{
    Activation, ActorId, AssignmentId, Blocker, Blockers, Cause, CorrelationId, DesiredState,
    DomainErrorCode, DomainFault, EligibilityTarget, EventId, ExecutionId, HoldId, HoldRef, Id,
    OperationId, OperationState, Phase, Purpose, Query, RetryAdvice, RevisionDimension,
    RootCommand, RootControl, RootId, Subject,
};
use crate::services::sessions::{Failure, Refusal, not_found, query_fault, rev};
use scope::{ExecutionScope, PlanState, RecordedScope, is_feature};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

/// The operations [`ControlService`] is registered for.
pub const OPERATIONS: [&str; 7] = [
    "runs.start",
    "runs.pause",
    "runs.continue",
    "runs.redirect",
    "runs.inspect",
    "runs.stop",
    "runs.eligibility",
];
/// History aggregates of run control.
pub const CONTROL_AGGREGATE: &str = "root_control";
pub const ACTIVATION_AGGREGATE: &str = "activation";
pub const BLOCKER_AGGREGATE: &str = "blocker";
pub const ASSIGNMENT_AGGREGATE: &str = "assignment";
pub const INSTRUCTION_AGGREGATE: &str = "run_instruction";
/// Journal capability of a stop operation.
pub const STOP_CAPABILITY: &str = "runs.stop";
/// Most blockers a root snapshot lists (`hasMore` beyond).
pub const LISTED_BLOCKERS: usize = 100;
/// Project lifecycle label of an archived project: no work, queue allocation or dispatch.
pub const ARCHIVED: &str = "Archived";
/// Whether a project in `lifecycle` is closed to work: anything but `Active` (an archived
/// project, and a deleted one, whose tombstone follows its archival) allocates and dispatches
/// nothing.
pub fn closed_to_work(lifecycle: &str) -> bool {
    lifecycle != crate::ports::storage::ACTIVE
}
const PAGE: u16 = 500;

/// Bounded, categorized retry and lease policy of activations (CONTROL-06).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlConfig {
    /// Lease an activation claim holds; the owner renews it.
    pub lease_ms: i64,
    /// Transient failures (lost supervisor, connectivity) retried before one escalation.
    pub max_attempts: i64,
    pub base_backoff_ms: i64,
    pub max_backoff_ms: i64,
}
impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            lease_ms: 30_000,
            max_attempts: 5,
            base_backoff_ms: 1_000,
            max_backoff_ms: 300_000,
        }
    }
}
impl ControlConfig {
    /// Deterministic exponential backoff of the `attempt`-th retry (1-based).
    pub fn backoff_ms(&self, attempt: i64) -> i64 {
        let shift = u32::try_from((attempt - 1).clamp(0, 30)).unwrap_or(30);
        self.base_backoff_ms
            .saturating_mul(1_i64 << shift)
            .min(self.max_backoff_ms)
    }
}

/// The run control service: the one implementation of root controls and eligibility.
pub struct ControlService {
    ids: Arc<dyn IdSource>,
    scope: Arc<dyn ExecutionScope>,
    config: ControlConfig,
}

impl ControlService {
    /// A service over the production plan/readiness port ([`RecordedScope`]).
    pub fn new(ids: Arc<dyn IdSource>) -> Self {
        Self::with_scope(ids, Arc::new(RecordedScope))
    }
    pub fn with_scope(ids: Arc<dyn IdSource>, scope: Arc<dyn ExecutionScope>) -> Self {
        Self {
            ids,
            scope,
            config: ControlConfig::default(),
        }
    }
    pub fn with_config(mut self, config: ControlConfig) -> Self {
        self.config = config;
        self
    }
    pub fn config(&self) -> &ControlConfig {
        &self.config
    }
    pub fn scope(&self) -> &Arc<dyn ExecutionScope> {
        &self.scope
    }
    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }
}

// ---------------------------------------------------------------------------------------------
// Shared helpers

/// The root `id` as a run root this invocation may see: a feature or a standalone issue/task.
/// Work inside a feature has no manual control (root-only controls).
pub(crate) fn run_root(
    sessions: &dyn SessionRead,
    ctx: &InvocationContext,
    id: &RootId,
) -> Result<WorkRoot, Failure> {
    let root = crate::services::sessions::visible_root(sessions, ctx, id)?;
    if !is_run_root(&root) {
        return Err(Refusal::new(
            DomainErrorCode::UnsupportedTarget,
            "root",
            Some(root.id.0.clone()),
        )
        .into());
    }
    Ok(root)
}
/// The root `id` for a control of its own holds and activation: a run root, or a card linked
/// to a feature that has controls of its own (it ran as a root, or carries its unlink pause).
/// Such a card keeps its holds through relinking, and only its own Continue clears them; its
/// feature's coordinator runs its work, so Start stays root-only.
pub(crate) fn controlled_root(
    sessions: &dyn SessionRead,
    runs: &dyn RunRead,
    ctx: &InvocationContext,
    id: &RootId,
) -> Result<WorkRoot, Failure> {
    let root = crate::services::sessions::visible_root(sessions, ctx, id)?;
    if is_run_root(&root) || runs.root_control(&root.id)?.is_some() {
        return Ok(root);
    }
    Err(Refusal::new(
        DomainErrorCode::UnsupportedTarget,
        "root",
        Some(root.id.0.clone()),
    )
    .into())
}
pub(crate) fn controlled_root_tx(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    id: &RootId,
) -> Result<WorkRoot, Failure> {
    let root = crate::services::sessions::visible_root(tx.sessions(), ctx, id)?;
    if is_run_root(&root) || tx.runs().root_control(&root.id)?.is_some() {
        return Ok(root);
    }
    Err(Refusal::new(
        DomainErrorCode::UnsupportedTarget,
        "root",
        Some(root.id.0.clone()),
    )
    .into())
}
/// The cards linked to `feature` that keep run controls of their own: a retained activation (an
/// issue linked while its agent ran), its root-scoped deliveries and its process trees. The
/// feature's Pause and Stop reach them, and the feature's stop settles only once they quiesced.
pub(crate) fn linked_roots(
    tx: &mut dyn WriteTransaction,
    feature: &WorkRoot,
) -> Result<Vec<WorkRoot>, StorageError> {
    use crate::ports::storage::{WorkFilter, WorkPosition, WorkSelection};
    if feature.kind != "feature" {
        return Ok(Vec::new());
    }
    let filter = WorkFilter {
        project: feature.project_id.clone(),
        selection: WorkSelection::FeatureTasks(feature.id.0.clone()),
        status_ids: Vec::new(),
        repository_ids: Vec::new(),
        text: None,
    };
    let mut out = Vec::new();
    let mut after: Option<WorkPosition> = None;
    loop {
        let page = tx.work().works(&filter, after.as_ref(), 500)?;
        let more = page.len() == 500;
        after = page.last().map(WorkPosition::of);
        for w in &page {
            let id = RootId(w.id.clone());
            if tx.runs().root_control(&id)?.is_some()
                && let Some(r) = tx.sessions().work_root(&id)?
            {
                out.push(r);
            }
        }
        if !more {
            return Ok(out);
        }
    }
}
/// A linked card a feature's stop covers, fenced to what it covered when the stop was decided:
/// the card's activation generation then, and whether a reactivation was already pending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkedStop {
    pub root: RootId,
    pub generation: i64,
    pub reactivate: bool,
}
impl LinkedStop {
    fn of(control: &StoredRootControl) -> Self {
        Self {
            root: control.root_id.clone(),
            generation: control.generation,
            reactivate: control.reactivate,
        }
    }
    fn value(&self) -> Value {
        json!({"rootId": self.root.0.as_str(), "generation": self.generation,
            "reactivate": self.reactivate})
    }
    /// Whether the card moved past what the stop covered (the linked-card form of
    /// [`recovery::stop_superseded`]): a newer activation exists, or a Continue was accepted
    /// since (a reactivation is pending). The stop then observes only the generation it covered
    /// and never stops the card's work or process trees beyond those it holds for
    /// reconciliation.
    pub fn superseded(&self, control: &StoredRootControl) -> bool {
        control.generation > self.generation || (control.reactivate && !self.reactivate)
    }
}

/// The linked cards a feature's stop operation covers, each with the generation it covered: the
/// cards its journaled intent names (covered when the stop was decided), and those it came to
/// cover while it was pending (a card linked since, or reached again by a reapply at a newer
/// generation), whose recorded coverage takes the place of the intent's.
pub fn stop_linked_roots(
    tx: &mut dyn WriteTransaction,
    op: &OperationId,
) -> Result<Vec<LinkedStop>, StorageError> {
    let mut out: Vec<LinkedStop> = tx
        .operations()
        .operation(op)?
        .and_then(|o| {
            o.payload
                .get("linkedRoots")
                .and_then(Value::as_array)
                .cloned()
        })
        .unwrap_or_default()
        .iter()
        .filter_map(|v| {
            Some(LinkedStop {
                root: RootId(Id::new(v.get("rootId")?.as_str()?).ok()?),
                generation: v.get("generation")?.as_i64()?,
                reactivate: v.get("reactivate")?.as_bool()?,
            })
        })
        .collect();
    let mut after: Option<RootId> = None;
    loop {
        let page = tx.runs().stop_coverage(op, after.as_ref(), PAGE)?;
        let more = page.len() == usize::from(PAGE);
        after = page.last().map(|c| c.root_id.clone());
        for c in page {
            let card = LinkedStop {
                root: c.root_id,
                generation: c.generation,
                reactivate: c.reactivate,
            };
            match out.iter_mut().find(|l| l.root == card.root) {
                Some(l) => *l = card,
                None => out.push(card),
            }
        }
        if !more {
            return Ok(out);
        }
    }
}
/// Why a linked card's retained activation may not keep running: its feature is paused or holds
/// it at the root level, or the project is closed to work (the conditions its feature's
/// controls impose; plan and readiness gates hold new dispatch only).
pub(crate) fn inherited_hold(
    runs: &dyn RunRead,
    root: &WorkRoot,
) -> Result<Option<String>, StorageError> {
    if runs
        .project_lifecycle(&root.project_id)?
        .is_some_and(|l| closed_to_work(&l))
    {
        return Ok(Some("the project is archived or deleted".into()));
    }
    let (false, Some(f)) = (is_run_root(root), &root.parent_feature_id) else {
        return Ok(None);
    };
    if control_of(runs, f)?.desired == DesiredExecution::Paused {
        return Ok(Some("its feature is paused".into()));
    }
    Ok(active_blockers(runs, &BlockerFilter::RootLevel(f.clone()))?
        .first()
        .map(|b| format!("its feature is held: {}", b.reason)))
}
/// Whether a work item is a run root (a feature, or a task/issue without a feature).
pub fn is_run_root(w: &WorkRoot) -> bool {
    w.kind == "feature" || w.parent_feature_id.is_none()
}

/// The stored control of `root`, or the implicit initial one.
pub fn control_of(runs: &dyn RunRead, root: &RootId) -> Result<StoredRootControl, StorageError> {
    Ok(runs
        .root_control(root)?
        .unwrap_or_else(|| StoredRootControl::initial(root.clone())))
}
/// The stored control of `root`, materializing the initial record first.
pub(crate) fn ensure_control(
    tx: &mut dyn WriteTransaction,
    root: &RootId,
) -> Result<StoredRootControl, StorageError> {
    if let Some(c) = tx.runs().root_control(root)? {
        return Ok(c);
    }
    let mut c = StoredRootControl::initial(root.clone());
    c.updated_at_ms = tx.now_ms();
    tx.runs().insert_root_control(&c)?;
    Ok(c)
}
/// Writes `after` over the stored control at `before.revision` (stamping the update time).
pub(crate) fn save_control(
    tx: &mut dyn WriteTransaction,
    before: &StoredRootControl,
    after: &mut StoredRootControl,
) -> Result<(), StorageError> {
    after.updated_at_ms = tx.now_ms();
    tx.runs().update_root_control(after, before.revision)
}

/// Every active blocker matching `filter` (bounded pages).
pub fn active_blockers(
    runs: &dyn RunRead,
    filter: &BlockerFilter,
) -> Result<Vec<StoredBlocker>, StorageError> {
    let mut out: Vec<StoredBlocker> = Vec::new();
    loop {
        let page = runs.blockers(filter, out.last().map(|b| &b.id), PAGE)?;
        let more = page.len() == usize::from(PAGE);
        out.extend(page);
        if !more {
            return Ok(out);
        }
    }
}
/// Every assignment of `root` (only live ones when `live_only`).
pub fn root_assignments(
    runs: &dyn RunRead,
    root: &RootId,
    live_only: bool,
) -> Result<Vec<StoredAssignment>, StorageError> {
    let mut out: Vec<StoredAssignment> = Vec::new();
    loop {
        let page = runs.assignments(root, live_only, out.last().map(|a| &a.id), PAGE)?;
        let more = page.len() == usize::from(PAGE);
        out.extend(page);
        if !more {
            return Ok(out);
        }
    }
}
/// The root's live coordinator assignment.
pub fn coordinator(
    runs: &dyn RunRead,
    root: &RootId,
) -> Result<Option<StoredAssignment>, StorageError> {
    Ok(root_assignments(runs, root, true)?
        .into_iter()
        .find(|a| a.parent_id.is_none()))
}

/// Appends one run-control history event on the root's history.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    root: &WorkRoot,
    operation: Option<&OperationId>,
    aggregate_kind: &str,
    aggregate_id: &str,
    aggregate_revision: i64,
    kind: &str,
    payload: Value,
) -> Result<(), StorageError> {
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id,
        actor: actor.clone(),
        project_id: Some(root.project_id.clone()),
        root_id: Some(root.id.clone()),
        operation_id: operation.cloned(),
        aggregate_kind: aggregate_kind.into(),
        aggregate_id: aggregate_id.into(),
        aggregate_revision: aggregate_revision.max(1),
        kind: kind.into(),
        payload,
    })?;
    Ok(())
}

/// The subject a new blocker is raised on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HoldSubject {
    pub assignment: Option<AssignmentId>,
    pub operation: Option<OperationId>,
}

/// Raises a blocker (or returns the active one with the same subject, cause and origin, so a
/// repeated deterministic report creates one actionable blocker, not one per retry).
#[allow(clippy::too_many_arguments)]
pub fn raise_blocker(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    root: &WorkRoot,
    subject: HoldSubject,
    cause: HoldCause,
    origin_id: &str,
    origin_revision: i64,
    reason: &str,
    clearance: Value,
) -> Result<StoredBlocker, StorageError> {
    let filter = match (&subject.operation, &subject.assignment) {
        (Some(o), _) => BlockerFilter::Operation(o.clone()),
        (None, Some(a)) => BlockerFilter::Assignment(a.clone()),
        (None, None) => BlockerFilter::RootLevel(root.id.clone()),
    };
    if let Some(existing) = active_blockers(tx.runs(), &filter)?.into_iter().find(|b| {
        b.cause == cause && b.origin_id == origin_id && b.origin_revision == origin_revision
    }) {
        return Ok(existing);
    }
    let b = StoredBlocker {
        id: tx.new_id(IdKind::Link),
        root_id: Some(root.id.clone()),
        assignment_id: subject.assignment,
        operation_id: subject.operation,
        cause,
        origin_id: origin_id.to_owned(),
        origin_revision,
        state: BlockerState::Active,
        revision: 1,
        reason: reason.chars().take(4096).collect(),
        clearance: if clearance.is_object() {
            clearance
        } else {
            json!({})
        },
        created_at_ms: tx.now_ms(),
        cleared_at_ms: None,
        cleared_by: None,
    };
    tx.runs().insert_blocker(&b)?;
    record(
        tx,
        actor,
        root,
        b.operation_id.as_ref(),
        BLOCKER_AGGREGATE,
        b.id.as_str(),
        b.revision,
        "BlockerRaised",
        json!({
            "cause": cause.as_str(),
            "reason": b.reason,
            "assignmentId": b.assignment_id.as_ref().map(|a| a.0.as_str()),
            "operationId": b.operation_id.as_ref().map(|o| o.0.as_str()),
            "originId": origin_id,
            "originRevision": origin_revision,
        }),
    )?;
    Ok(b)
}

/// Clears an active blocker with its evidence (the owning service's clearance, or an authorized
/// Continue for a manual-resume hold).
pub fn clear_blocker(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    root: &WorkRoot,
    blocker: &StoredBlocker,
    evidence: Value,
) -> Result<StoredBlocker, StorageError> {
    let mut after = blocker.clone();
    after.state = BlockerState::Cleared;
    after.revision += 1;
    after.cleared_at_ms = Some(tx.now_ms());
    after.cleared_by = Some(actor.clone());
    if let Some(map) = after.clearance.as_object_mut() {
        map.insert("clearedWith".into(), evidence.clone());
    }
    tx.runs().update_blocker(&after, blocker.revision)?;
    record(
        tx,
        actor,
        root,
        after.operation_id.as_ref(),
        BLOCKER_AGGREGATE,
        after.id.as_str(),
        after.revision,
        "BlockerCleared",
        json!({"cause": after.cause.as_str(), "evidence": evidence}),
    )?;
    Ok(after)
}

// ---------------------------------------------------------------------------------------------
// Eligibility (AD17, AD79)

/// What eligibility is evaluated for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Root,
    Assignment(AssignmentId),
    /// One operation of an assignment (a tool call), with the operations it actually depends
    /// on: a sibling's failure or approval wait blocks only its dependents.
    Operation {
        assignment: Option<AssignmentId>,
        operation: OperationId,
        depends_on: Vec<OperationId>,
    },
}

/// One reason a target is not eligible: a stored blocker, or a condition derived from current
/// records (no approved plan, missing readiness, a superseded assignment, an archived project).
/// A derived condition has a stable derived ID; it is not a stored hold and Continue cannot
/// cover it.
#[derive(Clone, Debug)]
pub struct Condition {
    pub hold_id: Id,
    pub revision: i64,
    pub subject: Subject,
    pub cause: HoldCause,
    pub reason: String,
    pub stored: bool,
}

/// The current eligibility of a target for a purpose, with the revisions it was read at.
#[derive(Clone, Debug)]
pub struct Eligibility {
    pub eligible: bool,
    pub conditions: Vec<Condition>,
    pub control_revision: i64,
    pub generation: i64,
    pub plan_revision: Option<i64>,
    pub pending_plan_revision: Option<i64>,
    pub readiness_revision: Option<i64>,
    pub instruction_revision: i64,
    pub stop_epoch: i64,
}

fn subject_of(b: &StoredBlocker) -> Subject {
    match (&b.operation_id, &b.assignment_id, &b.root_id) {
        (Some(o), _, _) => Subject::Operation(o.clone()),
        (None, Some(a), _) => Subject::Assignment(a.clone()),
        (None, None, Some(r)) => Subject::Root(r.clone()),
        _ => Subject::Blocker(HoldId(b.id.clone())),
    }
}
fn stored(b: &StoredBlocker) -> Condition {
    Condition {
        hold_id: b.id.clone(),
        revision: b.revision,
        subject: subject_of(b),
        cause: b.cause,
        reason: b.reason.clone(),
        stored: true,
    }
}
/// Holds implementation of `work` while it has a prerequisite not completed with verified
/// acceptance.
fn prerequisite_gate(
    runs: &dyn RunRead,
    work: &RootId,
    subject: Subject,
    c: &mut Vec<Condition>,
) -> Result<(), StorageError> {
    let open = runs.open_prerequisites(work)?;
    if let Some(p) = open.first() {
        c.push(derived(
            &format!("prerequisites-{}", work.0.as_str()),
            subject,
            open.len() as i64,
            HoldCause::Dependency,
            &format!(
                "prerequisite {} is not complete with verified acceptance",
                p.0.as_str()
            ),
        ));
    }
    Ok(())
}
fn derived(
    kind: &str,
    subject: Subject,
    revision: i64,
    cause: HoldCause,
    reason: &str,
) -> Condition {
    let key = serde_json::to_string(&subject).unwrap_or_default();
    let digest = sha256_hex(format!("{kind}|{key}").as_bytes());
    Condition {
        hold_id: Id::new(format!("drv-{}", &digest[..40])).expect("derived identifier"),
        revision: revision.max(1),
        subject,
        cause,
        reason: reason.to_owned(),
        stored: false,
    }
}

impl ControlService {
    /// Evaluates current eligibility without changing anything (`EvaluateEligibility`). The same
    /// predicates are rechecked atomically by [`Self::begin_dispatch`]; a successful earlier
    /// evaluation is advisory.
    pub fn evaluate(
        &self,
        runs: &dyn RunRead,
        root: &WorkRoot,
        target: &Target,
        purpose: ActivationPurpose,
    ) -> Result<Eligibility, StorageError> {
        let control = control_of(runs, &root.id)?;
        let plan = self.scope.plan(runs, root)?;
        let mut c: Vec<Condition> = Vec::new();
        let root_subject = Subject::Root(root.id.clone());
        if runs
            .project_lifecycle(&root.project_id)?
            .is_some_and(|l| closed_to_work(&l))
        {
            c.push(derived(
                "archived",
                root_subject.clone(),
                1,
                HoldCause::Policy,
                "the project is archived or deleted",
            ));
        }
        let root_level = active_blockers(runs, &BlockerFilter::RootLevel(root.id.clone()))?;
        if control.desired == DesiredExecution::Paused
            && !root_level.iter().any(|b| b.cause.manual_resume())
        {
            c.push(derived(
                "paused",
                root_subject.clone(),
                control.revision,
                HoldCause::HumanPause,
                "the root is paused",
            ));
        }
        c.extend(root_level.iter().map(stored));
        // A card that keeps an activation of its own after it was linked to a feature (an
        // existing agent continues) is governed by its feature's controls as well.
        if !is_run_root(root)
            && let Some(f) = &root.parent_feature_id
        {
            let feature_control = control_of(runs, f)?;
            let feature_level = active_blockers(runs, &BlockerFilter::RootLevel(f.clone()))?;
            if feature_control.desired == DesiredExecution::Paused
                && !feature_level.iter().any(|b| b.cause.manual_resume())
            {
                c.push(derived(
                    "feature-paused",
                    Subject::Root(f.clone()),
                    feature_control.revision,
                    HoldCause::HumanPause,
                    "its feature is paused",
                ));
            }
            c.extend(feature_level.iter().map(stored));
            // Its implementation is the feature's: the feature's prerequisites, approved plan,
            // readiness and the findings that gate this card hold it as they hold the feature's
            // own assignments.
            if purpose == ActivationPurpose::Implementation {
                let feature = WorkRoot {
                    id: f.clone(),
                    project_id: root.project_id.clone(),
                    kind: "feature".into(),
                    title: String::new(),
                    revision: 1,
                    scope_revision: 1,
                    parent_feature_id: None,
                    created_at_ms: 0,
                };
                prerequisite_gate(runs, f, Subject::Root(f.clone()), &mut c)?;
                match self.scope.plan(runs, &feature)? {
                    PlanState::Approved { feature: unit, .. } if unit.readiness.is_current() => {}
                    PlanState::Approved { feature: unit, revision, .. } => c.push(derived(
                        "feature-readiness",
                        Subject::Root(f.clone()),
                        unit.readiness.revision().unwrap_or(revision),
                        HoldCause::Policy,
                        &unit.readiness.describe(),
                    )),
                    other => c.push(derived(
                        "feature-plan",
                        Subject::Root(f.clone()),
                        other.pending_revision().unwrap_or(1),
                        HoldCause::Approval,
                        "its feature has no approved plan; a pending revision cannot authorize implementation",
                    )),
                }
                if let Some((_, readiness)) = runs.feature_plan(f)?.and_then(|p| p.readiness)
                    && let Some(x) =
                        crate::services::work::readiness::gating_finding(&readiness, &root.id.0)
                {
                    c.push(derived(
                        "finding-gate",
                        root_subject.clone(),
                        control.revision,
                        HoldCause::Policy,
                        &format!("readiness finding {x} gates this work"),
                    ));
                }
            }
        }
        // A root's unsatisfied prerequisites (a feature's `features.dependencies`, a standalone
        // card's blockers) hold its implementation and, through the root, every assignment of
        // its work: a manual Done label is not verified completion.
        if purpose == ActivationPurpose::Implementation {
            prerequisite_gate(runs, &root.id, root_subject.clone(), &mut c)?;
        }
        let mut readiness_revision = None;
        let assignment = match target {
            Target::Root => None,
            Target::Assignment(a)
            | Target::Operation {
                assignment: Some(a),
                ..
            } => Some(a.clone()),
            Target::Operation {
                assignment: None, ..
            } => None,
        };
        let assignment = match assignment {
            None => None,
            Some(id) => match runs.assignment(&id)? {
                Some(a) if a.root_id == root.id => Some(a),
                _ => {
                    c.push(derived(
                        "assignment-missing",
                        Subject::Assignment(id.clone()),
                        1,
                        HoldCause::Policy,
                        "the assignment does not belong to this root",
                    ));
                    None
                }
            },
        };
        if let Some(a) = &assignment {
            let subject = Subject::Assignment(a.id.clone());
            if !a.state.is_live() {
                c.push(derived(
                    "assignment-ended",
                    subject.clone(),
                    a.revision,
                    HoldCause::Policy,
                    &format!("the assignment is {}", a.state.as_str()),
                ));
            }
            c.extend(
                active_blockers(runs, &BlockerFilter::Assignment(a.id.clone()))?
                    .iter()
                    .map(stored),
            );
            if a.work_id != root.id {
                // The card's own holds travel with it (an unlink pause survives relinking), a
                // card that left this root is fenced, and its unsatisfied prerequisites hold
                // its implementation.
                c.extend(
                    active_blockers(runs, &BlockerFilter::RootLevel(a.work_id.clone()))?
                        .iter()
                        .map(stored),
                );
                if let Some(parent) = runs.work_parent(&a.work_id)?
                    && parent.as_ref() != Some(&root.id)
                {
                    c.push(derived(
                        "membership",
                        subject.clone(),
                        a.revision,
                        HoldCause::Policy,
                        "the work is no longer part of this root",
                    ));
                }
                if purpose == ActivationPurpose::Implementation
                    && is_feature(root)
                    && let Some((_, readiness)) =
                        runs.feature_plan(&root.id)?.and_then(|p| p.readiness)
                    && let Some(f) =
                        crate::services::work::readiness::gating_finding(&readiness, &a.work_id.0)
                {
                    c.push(derived(
                        "finding-gate",
                        subject.clone(),
                        a.revision,
                        HoldCause::Policy,
                        &format!("readiness finding {f} gates this work"),
                    ));
                }
                if purpose == ActivationPurpose::Implementation {
                    let open = runs.open_prerequisites(&a.work_id)?;
                    if let Some(p) = open.first() {
                        c.push(derived(
                            "prerequisites",
                            subject.clone(),
                            open.len() as i64,
                            HoldCause::Dependency,
                            &format!(
                                "prerequisite {} is not complete with verified acceptance",
                                p.0.as_str()
                            ),
                        ));
                    }
                }
            }
            if a.parent_id.is_none() && a.instruction_revision < control.instruction_revision {
                c.push(derived(
                    "redirect",
                    subject.clone(),
                    control.instruction_revision,
                    HoldCause::Policy,
                    "a redirect awaits the coordinator's acknowledgment at its next safe boundary",
                ));
            }
        }
        if let Target::Operation {
            operation,
            depends_on,
            ..
        } = target
        {
            c.extend(
                active_blockers(runs, &BlockerFilter::Operation(operation.clone()))?
                    .iter()
                    .map(stored),
            );
            for dep in depends_on {
                let held = active_blockers(runs, &BlockerFilter::Operation(dep.clone()))?;
                if !held.is_empty() {
                    c.push(derived(
                        "dependency",
                        Subject::Operation(operation.clone()),
                        1,
                        HoldCause::Dependency,
                        &format!("it depends on held operation {}", dep.0.as_str()),
                    ));
                }
            }
        }
        if purpose == ActivationPurpose::Implementation && is_feature(root) {
            match &plan {
                PlanState::NotRequired => {}
                PlanState::Unapproved { pending_revision } => c.push(derived(
                    "plan",
                    root_subject.clone(),
                    pending_revision.unwrap_or(1),
                    HoldCause::Approval,
                    "the feature has no approved plan; a pending revision cannot authorize implementation",
                )),
                PlanState::Approved { revision, .. } => {
                    // No key: the coordinator's feature-wide scope.
                    let key = assignment.as_ref().and_then(|a| a.scope_key.as_deref());
                    let subject = assignment
                        .as_ref()
                        .map_or(root_subject.clone(), |a| Subject::Assignment(a.id.clone()));
                    match plan.unit(key) {
                        None => c.push(derived(
                            "scope",
                            subject,
                            *revision,
                            HoldCause::Policy,
                            "this scope is not part of the active approved plan",
                        )),
                        Some(unit) => {
                            if let Some(a) = &assignment
                                && a.scope_digest.as_ref().is_some_and(|d| d != &unit.digest)
                            {
                                c.push(derived(
                                    "scope-changed",
                                    subject.clone(),
                                    *revision,
                                    HoldCause::Policy,
                                    "the approved scope of this work changed; it is superseded at the next safe boundary",
                                ));
                            }
                            readiness_revision = unit.readiness.revision();
                            if !unit.readiness.is_current() {
                                c.push(derived(
                                    "readiness",
                                    subject,
                                    unit.readiness.revision().unwrap_or(*revision),
                                    HoldCause::Policy,
                                    &unit.readiness.describe(),
                                ));
                            }
                        }
                    }
                }
            }
        }
        Ok(Eligibility {
            eligible: c.is_empty(),
            conditions: c,
            control_revision: control.revision,
            generation: control.generation,
            plan_revision: plan.approved_revision(),
            pending_plan_revision: plan.pending_revision(),
            readiness_revision,
            instruction_revision: control.instruction_revision,
            stop_epoch: control.stop_epoch,
        })
    }

    /// The live assignments of `root` that are eligible for `purpose` now (CONTROL-13: Continue
    /// makes eligible descendants runnable; each keeps its own blockers).
    pub fn runnable_assignments(
        &self,
        runs: &dyn RunRead,
        root: &WorkRoot,
        purpose: ActivationPurpose,
    ) -> Result<Vec<StoredAssignment>, StorageError> {
        let mut out = Vec::new();
        for a in root_assignments(runs, &root.id, true)? {
            if self
                .evaluate(runs, root, &Target::Assignment(a.id.clone()), purpose)?
                .eligible
            {
                out.push(a);
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------------------------
// Wire projections

fn cause_of(c: HoldCause) -> Cause {
    match c {
        HoldCause::HumanPause => Cause::HumanPause,
        HoldCause::RestartContinue => Cause::RestartContinue,
        HoldCause::BudgetContinue => Cause::BudgetContinue,
        HoldCause::Approval => Cause::Approval,
        HoldCause::Dependency => Cause::Dependency,
        HoldCause::Reconciliation => Cause::Reconciliation,
        HoldCause::Credentials => Cause::Credentials,
        HoldCause::Capacity => Cause::Capacity,
        HoldCause::StorageUnavailable => Cause::StorageUnavailable,
        HoldCause::Policy => Cause::Policy,
    }
}
fn wire_blockers(conditions: &[Condition]) -> Blockers {
    Blockers {
        items: conditions
            .iter()
            .take(LISTED_BLOCKERS)
            .map(|c| Blocker {
                hold_id: HoldId(c.hold_id.clone()),
                revision: rev(c.revision),
                subject: c.subject.clone(),
                cause: cause_of(c.cause),
            })
            .collect(),
        has_more: conditions.len() > LISTED_BLOCKERS,
    }
}
fn phase_of(p: ActivationPhase) -> Phase {
    match p {
        ActivationPhase::Pending => Phase::Pending,
        ActivationPhase::Claimed => Phase::Claimed,
        ActivationPhase::Running => Phase::Running,
        ActivationPhase::Stopping => Phase::Stopping,
        ActivationPhase::Stopped => Phase::Stopped,
    }
}
fn wire_control(c: &StoredRootControl) -> RootControl {
    RootControl {
        root_id: c.root_id.clone(),
        revision: rev(c.revision),
        desired_state: match c.desired {
            DesiredExecution::Enabled => DesiredState::Enabled,
            DesiredExecution::Paused => DesiredState::Paused,
        },
    }
}
/// The root's current activation: the live one, else the latest.
pub fn current_activation(
    runs: &dyn RunRead,
    root: &RootId,
) -> Result<Option<StoredActivation>, StorageError> {
    if let Some(a) = runs.live_activation(root)? {
        return Ok(Some(a));
    }
    Ok(runs
        .activations(&ActivationFilter::Root(root.clone()), None, 1)?
        .into_iter()
        .next())
}
/// The wire root snapshot: control, current activation and every active blocker within the root
/// (root-level, assignments and operations), so child blockers stay visible after Continue.
pub fn root_snapshot(runs: &dyn RunRead, root: &RootId) -> Result<RootSnapshot, StorageError> {
    let control = control_of(runs, root)?;
    let activation = current_activation(runs, root)?.map(|a| Activation {
        execution_id: ExecutionId(a.id.clone()),
        generation: rev(a.generation),
        phase: phase_of(a.phase),
    });
    let blockers: Vec<Condition> = active_blockers(runs, &BlockerFilter::Root(root.clone()))?
        .iter()
        .map(stored)
        .collect();
    Ok(RootSnapshot {
        root: wire_control(&control),
        activation,
        blockers: wire_blockers(&blockers),
    })
}

// ---------------------------------------------------------------------------------------------
// Resume authority (CONTROL-12)

/// What a user-minted resume grant covers (its `binding`): exact holds (the default delegation:
/// the identified current pause only) or, for a standing grant, the causes it may clear, which
/// never include restart or budget holds.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResumeBinding {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holds: Vec<HoldRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<String>,
}
impl ResumeBinding {
    pub fn covers(&self, b: &StoredBlocker) -> bool {
        if self
            .holds
            .iter()
            .any(|h| h.hold_id.0 == b.id && h.revision.get() == b.revision)
        {
            return true;
        }
        b.cause == HoldCause::HumanPause && self.causes.iter().any(|c| c == "HumanPause")
    }
}
/// The resume grant (if any) among `grants` that covers `blocker` of `root`.
pub(crate) fn covering_grant<'a>(
    grants: &'a [GrantRecord],
    root: &RootId,
    blocker: &StoredBlocker,
) -> Option<&'a GrantRecord> {
    grants.iter().find(|g| {
        &g.root_id == root
            && serde_json::from_value::<ResumeBinding>(g.binding.clone())
                .is_ok_and(|b| b.covers(blocker))
    })
}

// ---------------------------------------------------------------------------------------------
// Commands

/// A use-case failure of run control: a typed refusal with the hold IDs it names
/// (`blockerIds`), or a storage error.
#[derive(Debug)]
pub enum Fail {
    Refused(Refusal, Vec<Id>),
    Storage(StorageError),
}
impl From<StorageError> for Fail {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}
impl From<Refusal> for Fail {
    fn from(r: Refusal) -> Self {
        Self::Refused(r, Vec::new())
    }
}
impl From<Failure> for Fail {
    fn from(f: Failure) -> Self {
        match f {
            Failure::Refused(r) => Self::Refused(r, Vec::new()),
            Failure::Storage(e) => Self::Storage(e),
        }
    }
}
fn refuse(code: DomainErrorCode, kind: &'static str, id: Option<Id>) -> Fail {
    Refusal::new(code, kind, id).into()
}
fn refuse_holds(
    code: DomainErrorCode,
    retry: RetryAdvice,
    kind: &'static str,
    root: &RootId,
    holds: &[&StoredBlocker],
) -> Fail {
    Fail::Refused(
        Refusal {
            code,
            retry,
            target_kind: kind,
            target_id: Some(root.0.clone()),
        },
        holds.iter().map(|b| b.id.clone()).collect(),
    )
}
/// The fault code of a non-manual root blocker that refuses a Start.
fn blocker_code(c: HoldCause) -> DomainErrorCode {
    match c {
        HoldCause::HumanPause | HoldCause::RestartContinue => DomainErrorCode::ContinueRequired,
        HoldCause::BudgetContinue => DomainErrorCode::BudgetHold,
        HoldCause::Approval => DomainErrorCode::ApprovalRequired,
        HoldCause::Dependency => DomainErrorCode::DependencyBlocked,
        HoldCause::Capacity => DomainErrorCode::CapacityWait,
        HoldCause::Credentials => DomainErrorCode::CredentialsRequired,
        HoldCause::StorageUnavailable => DomainErrorCode::StorageUnavailable,
        HoldCause::Reconciliation => DomainErrorCode::Conflict,
        HoldCause::Policy => DomainErrorCode::PermissionDenied,
    }
}
fn control_conflict(control: &StoredRootControl, expected: i64) -> Result<(), Fail> {
    if control.revision != expected {
        return Err(StorageError::conflict(
            RevisionDimension::Control,
            "root",
            control.root_id.0.clone(),
            rev(expected),
            rev(control.revision),
        )
        .into());
    }
    Ok(())
}
fn snapshot_result(
    runs: &dyn RunRead,
    root: &RootId,
    wrap: fn(RootSnapshot) -> OperationResult,
) -> Result<OperationResult, StorageError> {
    Ok(wrap(root_snapshot(runs, root)?))
}

impl ControlService {
    fn start(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        root_id: &RootId,
        expected_work: i64,
        expected_control: i64,
    ) -> Result<DomainDecision, Fail> {
        let root = run_root(tx.sessions(), ctx, root_id)?;
        if tx
            .runs()
            .project_lifecycle(&root.project_id)?
            .is_some_and(|l| closed_to_work(&l))
        {
            return Err(refuse(
                DomainErrorCode::Conflict,
                "archived_project",
                Some(root.project_id.0.clone()),
            ));
        }
        let control = ensure_control(tx, &root.id)?;
        let root_level = active_blockers(tx.runs(), &BlockerFilter::RootLevel(root.id.clone()))?;
        let manual: Vec<&StoredBlocker> = root_level
            .iter()
            .filter(|b| b.cause.manual_resume())
            .collect();
        // CONTROL-12: Start never clears a pause; it explains that Continue is required.
        if control.desired == DesiredExecution::Paused || !manual.is_empty() {
            return Err(refuse_holds(
                DomainErrorCode::ContinueRequired,
                RetryAdvice::AfterBlockerClears,
                "root",
                &root.id,
                &manual,
            ));
        }
        // Racing Starts converge on the one activation: the second returns it unchanged.
        if tx.runs().live_activation(&root.id)?.is_some() {
            return Ok(DomainDecision::NoChange {
                result: snapshot_result(tx.runs(), &root.id, |value| OperationResult::RunsStart {
                    value,
                })?,
            });
        }
        if root.revision != expected_work {
            return Err(StorageError::conflict(
                RevisionDimension::Work,
                "root",
                root.id.0.clone(),
                rev(expected_work),
                rev(root.revision),
            )
            .into());
        }
        control_conflict(&control, expected_control)?;
        if let Some(first) = root_level.first() {
            let all: Vec<&StoredBlocker> = root_level.iter().collect();
            return Err(refuse_holds(
                blocker_code(first.cause),
                RetryAdvice::AfterBlockerClears,
                "root",
                &root.id,
                &all,
            ));
        }
        let plan = self.scope.plan(tx.runs(), &root)?;
        let purpose = if !is_feature(&root) {
            ActivationPurpose::Implementation
        } else {
            match plan.unit(None) {
                Some(u) if u.readiness.is_current() => ActivationPurpose::Implementation,
                // Start permits planning and readiness assessment; implementation stays gated.
                _ => ActivationPurpose::Planning,
            }
        };
        let activation = self.new_activation(
            tx,
            ctx.principal(),
            &root,
            &control,
            purpose,
            plan.approved_revision(),
            "Start",
        )?;
        let _ = activation;
        Ok(DomainDecision::Applied {
            result: snapshot_result(tx.runs(), &root.id, |value| OperationResult::RunsStart {
                value,
            })?,
        })
    }

    /// Records a new `Pending` activation of the root's next generation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_activation(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        root: &WorkRoot,
        control: &StoredRootControl,
        purpose: ActivationPurpose,
        plan_revision: Option<i64>,
        why: &str,
    ) -> Result<StoredActivation, StorageError> {
        let boot = tx
            .store()
            .boot_id
            .clone()
            .ok_or_else(|| StorageError::IncompatibleSchema("store is read-only".into()))?;
        let now = tx.now_ms();
        let mut after = control.clone();
        after.generation += 1;
        after.reactivate = false;
        let a = StoredActivation {
            id: tx.new_id(IdKind::Operation),
            root_id: root.id.clone(),
            boot_id: boot,
            generation: after.generation,
            phase: ActivationPhase::Pending,
            purpose,
            owner: None,
            lease_until_ms: None,
            owner_epoch: 0,
            revision: 1,
            attempts: 0,
            ready_at_ms: now,
            created_at_ms: now,
            stop_cause: None,
            stop_requested_at_ms: None,
            stop_acknowledged_at_ms: None,
            stopped_at_ms: None,
            outcome: None,
            detail: None,
            plan_revision,
        };
        tx.runs().insert_activation(&a)?;
        save_control(tx, control, &mut after)?;
        record(
            tx,
            actor,
            root,
            None,
            ACTIVATION_AGGREGATE,
            a.id.as_str(),
            a.revision,
            "ActivationAccepted",
            json!({
                "generation": a.generation,
                "purpose": purpose.as_str(),
                "planRevision": plan_revision,
                "via": why,
            }),
        )?;
        Ok(a)
    }

    /// Asks the root's live activation to stop: one never launched stops at once (`Prevented`);
    /// a launched one becomes `Stopping` with the stop requested.
    pub(crate) fn request_stop(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        root: &WorkRoot,
        cause: StopCause,
    ) -> Result<Option<StoredActivation>, StorageError> {
        let Some(a) = tx.runs().live_activation(&root.id)? else {
            return Ok(None);
        };
        let now = tx.now_ms();
        let mut after = a.clone();
        after.revision += 1;
        let kind = match a.phase {
            ActivationPhase::Pending => {
                after.phase = ActivationPhase::Stopped;
                after.stop_cause = Some(cause);
                after.stop_requested_at_ms = Some(now);
                after.stopped_at_ms = Some(now);
                after.outcome = Some(ActivationOutcome::Prevented);
                "ActivationPrevented"
            }
            ActivationPhase::Claimed | ActivationPhase::Running => {
                after.phase = ActivationPhase::Stopping;
                after.stop_cause = Some(cause);
                after.stop_requested_at_ms = Some(now);
                "ActivationStopRequested"
            }
            ActivationPhase::Stopping | ActivationPhase::Stopped => return Ok(Some(a)),
        };
        tx.runs().update_activation(&after, a.revision)?;
        record(
            tx,
            actor,
            root,
            None,
            ACTIVATION_AGGREGATE,
            after.id.as_str(),
            after.revision,
            kind,
            json!({"cause": cause.as_str(), "phase": after.phase.as_str()}),
        )?;
        Ok(Some(after))
    }

    fn pause(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        root_id: &RootId,
        expected: i64,
    ) -> Result<DomainDecision, Fail> {
        let root = controlled_root_tx(tx, ctx, root_id)?;
        let control = ensure_control(tx, &root.id)?;
        control_conflict(&control, expected)?;
        let paused = active_blockers(tx.runs(), &BlockerFilter::RootLevel(root.id.clone()))?
            .iter()
            .any(|b| b.cause == HoldCause::HumanPause);
        if control.desired == DesiredExecution::Paused && paused {
            return Ok(DomainDecision::NoChange {
                result: snapshot_result(tx.runs(), &root.id, |value| OperationResult::RunsPause {
                    value,
                })?,
            });
        }
        self.pause_root(
            tx,
            ctx.principal(),
            &root,
            &control,
            StopCause::Pause,
            "Paused",
        )?;
        Ok(DomainDecision::Applied {
            result: snapshot_result(tx.runs(), &root.id, |value| OperationResult::RunsPause {
                value,
            })?,
        })
    }

    /// Pauses the root (control revision, `HumanPause` hold) and asks its activation to stop.
    pub(crate) fn pause_root(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        root: &WorkRoot,
        control: &StoredRootControl,
        cause: StopCause,
        reason: &str,
    ) -> Result<(StoredRootControl, StoredBlocker), StorageError> {
        let mut after = control.clone();
        after.revision += 1;
        after.desired = DesiredExecution::Paused;
        after.reactivate = false;
        if cause == StopCause::Stop || cause == StopCause::Archive {
            after.stop_epoch += 1;
            // The stop covers the activations recorded so far, never a later generation.
            after.stop_generation = after.generation;
        }
        save_control(tx, control, &mut after)?;
        let hold = raise_blocker(
            tx,
            actor,
            root,
            HoldSubject::default(),
            HoldCause::HumanPause,
            root.id.0.as_str(),
            after.revision,
            reason,
            json!({"clearsWith": "Continue", "stopCause": cause.as_str()}),
        )?;
        let activation = self.request_stop(tx, actor, root, cause)?;
        // A linked card's retained activation is run under its feature's controls: the
        // feature's Pause or Stop asks it to stop as well.
        let mut linked = Vec::new();
        for card in linked_roots(tx, root)? {
            if let Some(a) = self.request_stop(tx, actor, &card, cause)? {
                linked.push(
                    json!({"rootId": card.id.0.as_str(), "executionId": a.id.as_str(),
                    "phase": a.phase.as_str()}),
                );
            }
        }
        record(
            tx,
            actor,
            root,
            None,
            CONTROL_AGGREGATE,
            root.id.0.as_str(),
            after.revision,
            "RootPaused",
            json!({
                "holdId": hold.id.as_str(),
                "cause": cause.as_str(),
                "stopEpoch": after.stop_epoch,
                "activation": activation.as_ref().map(|a| json!({
                    "executionId": a.id.as_str(),
                    "phase": a.phase.as_str(),
                })),
                "linkedActivations": linked,
            }),
        )?;
        Ok((after, hold))
    }

    fn continue_(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        root_id: &RootId,
        expected: i64,
        covered: &[HoldRef],
    ) -> Result<DomainDecision, Fail> {
        let root = controlled_root_tx(tx, ctx, root_id)?;
        // An archived project allocates no work: Continue, like Start, is refused (its holds
        // and the archival stop stay in place).
        if tx
            .runs()
            .project_lifecycle(&root.project_id)?
            .is_some_and(|l| closed_to_work(&l))
        {
            return Err(refuse(
                DomainErrorCode::Conflict,
                "archived_project",
                Some(root.project_id.0.clone()),
            ));
        }
        let control = ensure_control(tx, &root.id)?;
        control_conflict(&control, expected)?;
        let manual: Vec<StoredBlocker> =
            active_blockers(tx.runs(), &BlockerFilter::RootLevel(root.id.clone()))?
                .into_iter()
                .filter(|b| b.cause.manual_resume())
                .collect();
        let mut clear: Vec<StoredBlocker> = Vec::new();
        for h in covered {
            let Some(b) = tx.runs().blocker(&h.hold_id.0)? else {
                return Err(not_found("hold", &h.hold_id.0).into());
            };
            let root_level = b.root_id.as_ref() == Some(&root.id)
                && b.assignment_id.is_none()
                && b.operation_id.is_none();
            if !root_level {
                return Err(refuse(
                    DomainErrorCode::NotFound,
                    "hold",
                    Some(b.id.clone()),
                ));
            }
            if b.state == BlockerState::Cleared {
                // Already satisfied (a concurrent Continue): nothing more to clear for it.
                continue;
            }
            if !b.cause.manual_resume() {
                // Continue never deletes an approval, dependency, capacity, reconciliation or
                // other cause-specific blocker; its owning service clears it.
                return Err(refuse(
                    DomainErrorCode::InvalidArgument,
                    "hold_cause",
                    Some(b.id.clone()),
                ));
            }
            if h.revision.get() != b.revision {
                return Err(StorageError::conflict(
                    RevisionDimension::Blocker,
                    "hold",
                    b.id.clone(),
                    h.revision.clone(),
                    rev(b.revision),
                )
                .into());
            }
            clear.push(b);
        }
        // A changed or new pause is not covered by an old command.
        let uncovered: Vec<&StoredBlocker> = manual
            .iter()
            .filter(|m| !clear.iter().any(|c| c.id == m.id))
            .collect();
        if !uncovered.is_empty() {
            return Err(refuse_holds(
                DomainErrorCode::ContinueRequired,
                RetryAdvice::RefreshAndNewRequest,
                "root",
                &root.id,
                &uncovered,
            ));
        }
        if clear.is_empty() && control.desired == DesiredExecution::Enabled {
            return Ok(DomainDecision::NoChange {
                result: snapshot_result(tx.runs(), &root.id, |value| {
                    OperationResult::RunsContinue { value }
                })?,
            });
        }
        // Agents and schedules clear a manual-resume hold only with explicit user authority.
        let grants: Vec<GrantRecord> = command
            .resume_grants()
            .iter()
            .filter_map(|g| tx.grants().grant(&g.grant_id).ok().flatten())
            .collect();
        let human = ctx.principal_kind() == PrincipalKind::Human;
        if !human {
            let unauthorized: Vec<&StoredBlocker> = clear
                .iter()
                .filter(|b| covering_grant(&grants, &root.id, b).is_none())
                .collect();
            if !unauthorized.is_empty() {
                return Err(refuse_holds(
                    DomainErrorCode::PermissionDenied,
                    RetryAdvice::Never,
                    "resume_authorization",
                    &root.id,
                    &unauthorized,
                ));
            }
        }
        let mut cleared = Vec::new();
        for b in &clear {
            let grant = if human {
                None
            } else {
                covering_grant(&grants, &root.id, b).map(|g| g.id.as_str().to_owned())
            };
            clear_blocker(
                tx,
                ctx.principal(),
                &root,
                b,
                json!({
                    "via": "runs.continue",
                    "principalKind": format!("{:?}", ctx.principal_kind()),
                    "resumeGrant": grant,
                }),
            )?;
            cleared.push(b.id.as_str().to_owned());
        }
        let mut after = control.clone();
        after.revision += 1;
        after.desired = DesiredExecution::Enabled;
        after.restart_boot_id = None;
        let live = tx.runs().live_activation(&root.id)?;
        let reactivate = match &live {
            Some(a) if a.phase == ActivationPhase::Stopping => {
                after.reactivate = true;
                true
            }
            _ => false,
        };
        save_control(tx, &control, &mut after)?;
        record(
            tx,
            ctx.principal(),
            &root,
            None,
            CONTROL_AGGREGATE,
            root.id.0.as_str(),
            after.revision,
            "RootContinued",
            json!({
                "clearedHolds": cleared,
                "authority": if human { "user" } else { "resumeGrant" },
                "reactivateAfterStop": reactivate,
            }),
        )?;
        // A linked card's work is run by its feature's coordinator: Continue clears its own
        // holds but records no activation of its own.
        if live.is_none() && is_run_root(&root) {
            let plan = self.scope.plan(tx.runs(), &root)?;
            self.new_activation(
                tx,
                ctx.principal(),
                &root,
                &after,
                ActivationPurpose::Resume,
                plan.approved_revision(),
                "Continue",
            )?;
        }
        Ok(DomainDecision::Applied {
            result: snapshot_result(tx.runs(), &root.id, |value| OperationResult::RunsContinue {
                value,
            })?,
        })
    }

    fn redirect(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        input: &ops::RedirectInput,
    ) -> Result<DomainDecision, Fail> {
        // A linked card's own running agent stays redirectable.
        let root = controlled_root_tx(tx, ctx, &input.root_id)?;
        let control = ensure_control(tx, &root.id)?;
        control_conflict(&control, input.expected_control_revision.get())?;
        for a in &input.attachments.0 {
            if !crate::services::sessions::transcript::reference_resolves(
                tx,
                ctx,
                Some(&root.project_id),
                a,
            )? {
                return Err(not_found("artifact", &a.artifact_id).into());
            }
        }
        if input.message.0.is_empty() && input.attachments.0.is_empty() {
            return Err(refuse(
                DomainErrorCode::InvalidArgument,
                "instruction",
                None,
            ));
        }
        let mut after = control.clone();
        after.revision += 1;
        after.instruction_revision += 1;
        save_control(tx, &control, &mut after)?;
        let id = tx.new_id(IdKind::Link);
        let (session_id, entry_id) = self.link_transcript(tx, ctx, &root, input)?;
        let instruction = StoredInstruction {
            id: id.clone(),
            root_id: root.id.clone(),
            seq: after.instruction_revision,
            actor: ctx.principal().clone(),
            message: input.message.0.clone(),
            attachments: serde_json::to_value(&input.attachments.0).unwrap_or(json!([])),
            control_revision: after.revision,
            instruction_revision: after.instruction_revision,
            stage: InstructionStage::Received,
            received_at_ms: tx.now_ms(),
            acknowledged_at_ms: None,
            acknowledged_by: None,
            incorporated_at_ms: None,
            disposition: None,
            session_id: session_id.clone(),
            transcript_entry_id: entry_id.clone(),
            revision: 1,
        };
        tx.runs().insert_instruction(&instruction)?;
        record(
            tx,
            ctx.principal(),
            &root,
            None,
            INSTRUCTION_AGGREGATE,
            id.as_str(),
            1,
            "InstructionReceived",
            json!({
                "seq": instruction.seq,
                "controlRevision": after.revision,
                "sessionId": session_id.as_ref().map(Id::as_str),
                "transcriptEntryId": entry_id.as_ref().map(Id::as_str),
            }),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::RunsRedirect {
                value: MutationSummary {
                    id,
                    revision: rev(after.revision),
                    changed: true,
                },
            },
        })
    }

    /// Appends the instruction to the root coordinator's open session as a user message.
    fn link_transcript(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        root: &WorkRoot,
        input: &ops::RedirectInput,
    ) -> Result<(Option<Id>, Option<Id>), StorageError> {
        use crate::protocol::provider::{TranscriptBody, UserContent};
        use crate::services::sessions::transcript::{
            AppendPath, BatchOutcome, SessionBatch, append_session_batch,
        };
        let Some(coordinator) = coordinator(tx.runs(), &root.id)? else {
            return Ok((None, None));
        };
        let Some(session) = tx.sessions().session(&coordinator.session_id)? else {
            return Ok((None, None));
        };
        if session.closed() {
            return Ok((Some(session.id), None));
        }
        let mut content = Vec::new();
        if !input.message.0.is_empty() {
            content.push(UserContent::Text {
                text: input.message.0.clone(),
            });
        }
        content.extend(input.attachments.0.iter().map(|a| UserContent::Attachment {
            artifact: a.clone(),
        }));
        let batch_id = tx.new_id(IdKind::TranscriptBatch);
        let entry_id = tx.new_id(IdKind::TranscriptEntry);
        let entry = entry_id.clone();
        let batch = crate::services::sessions::turn::batch_of(
            &session.id,
            batch_id,
            session.context_epoch,
            session.next_sequence,
            vec![TranscriptBody::UserMessage { content }],
            || entry.clone(),
        );
        let actor = ctx.principal().clone();
        Ok(
            match append_session_batch(
                tx,
                ctx,
                &actor,
                &SessionBatch::entries(batch),
                AppendPath::UserMessage,
            )? {
                BatchOutcome::Accepted(_) | BatchOutcome::Replayed(_) => {
                    (Some(session.id), Some(entry_id))
                }
                BatchOutcome::Refused(_) => (Some(session.id), None),
            },
        )
    }

    fn stop(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        input: &ops::StopInput,
    ) -> Result<Decided, Fail> {
        let root = controlled_root_tx(tx, ctx, &input.root_id)?;
        let control = ensure_control(tx, &root.id)?;
        control_conflict(&control, input.expected_control_revision.get())?;
        // A stop still in progress answers itself, but only while the root still carries its
        // intent (paused, a human pause hold, no reactivation pending). A Continue since then
        // is overridden: `stop_root` reapplies the stop under the same journaled operation.
        if let Some(op) = &control.stop_operation_id
            && let Some(rec) = tx.operations().operation(op)?
        {
            let paused_by_hold =
                active_blockers(tx.runs(), &BlockerFilter::RootLevel(root.id.clone()))?
                    .iter()
                    .any(|b| b.cause == HoldCause::HumanPause);
            let intent = control.desired == DesiredExecution::Paused
                && !control.reactivate
                && paused_by_hold;
            let live = tx.runs().live_activation(&root.id)?;
            let mut nothing_live = live.is_none() && !self.root_has_work(tx, &root.id)?;
            // A linked card counts only with what the stop covered: once it moved past that
            // (a newer generation, or a Continue since), its newer work is not this stop's.
            for card in stop_linked_roots(tx, op)? {
                let live = tx.runs().live_activation(&card.root)?;
                nothing_live &= if card.superseded(&control_of(tx.runs(), &card.root)?) {
                    !live.is_some_and(|a| a.generation <= card.generation)
                } else {
                    live.is_none() && !self.root_has_work(tx, &card.root)?
                };
            }
            if intent && (!is_terminal(rec.state) || nothing_live) {
                return Ok(Decided {
                    decision: DomainDecision::NoChange {
                        result: OperationResult::RunsStop {
                            value: OperationRef {
                                operation_id: op.clone(),
                                state: rec.state,
                            },
                        },
                    },
                    operation_id: Some(op.clone()),
                });
            }
        }
        let (op, completed) = self.stop_root(
            tx,
            ctx.principal(),
            &root,
            &control,
            StopCause::Stop,
            "Stopped",
        )?;
        let state = tx
            .operations()
            .operation(&op)?
            .map_or(OperationState::IntentRecorded, |o| o.state);
        Ok(Decided {
            decision: if completed {
                DomainDecision::Applied {
                    result: OperationResult::RunsStop {
                        value: OperationRef {
                            operation_id: op.clone(),
                            state,
                        },
                    },
                }
            } else {
                DomainDecision::Accepted {
                    operation_id: op.clone(),
                }
            },
            operation_id: Some(op),
        })
    }

    /// Whether anything of the root may still be running: an unsettled root-scoped delivery or
    /// an unsettled owned process.
    fn root_has_work(
        &self,
        tx: &mut dyn WriteTransaction,
        root: &RootId,
    ) -> Result<bool, StorageError> {
        if !tx.runs().root_deliveries(root, None, 1)?.is_empty() {
            return Ok(true);
        }
        let mut after: Option<Id> = None;
        loop {
            let page = tx
                .resources()
                .processes(&ProcessFilter::Unsettled, after.as_ref(), PAGE)?;
            if page.iter().any(|p| p.root_id.as_ref() == Some(root)) {
                return Ok(true);
            }
            if page.len() < usize::from(PAGE) {
                return Ok(false);
            }
            after = page.last().map(|p| p.id.clone());
        }
    }

    /// Stops the root: pause (stop epoch advanced), request the activation to stop, cancel its
    /// root-scoped deliveries and journal the stop. Returns the stop operation and whether it
    /// was observed complete at once (nothing was live).
    pub(crate) fn stop_root(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        root: &WorkRoot,
        control: &StoredRootControl,
        cause: StopCause,
        reason: &str,
    ) -> Result<(OperationId, bool), StorageError> {
        // One stop pointer per root, written only here: an unfinished stop (pending, or held
        // with an unknown outcome) is reused and widened to this cause — paused again under a
        // new stop epoch and generation, its root-scoped deliveries cancelled again — never
        // overwritten, so its settlement and its reconciliation hold are never orphaned.
        if let Some(op) = &control.stop_operation_id
            && tx
                .operations()
                .operation(op)?
                .is_some_and(|o| !is_terminal(o.state))
        {
            let op = op.clone();
            self.pause_root(tx, actor, root, control, cause, reason)?;
            let mut cancelled = self.cancel_root_deliveries(tx, &root.id)?;
            // A linked card that moved past what the stop covered keeps its newer work.
            for card in stop_linked_roots(tx, &op)? {
                if !card.superseded(&control_of(tx.runs(), &card.root)?) {
                    cancelled.extend(self.cancel_root_deliveries(tx, &card.root)?);
                }
            }
            // The reapplied stop reaches every card linked now: one linked since the stop was
            // decided, or one it covered at an older generation that is linked again, is covered
            // at its current generation (its trees are stopped and waited for as well).
            let mut extended = Vec::new();
            for card in linked_roots(tx, root)? {
                if self.extend_stop(tx, actor, root, &op, &card, cause)? {
                    extended.push(card.id.0.as_str().to_owned());
                }
            }
            let after = control_of(tx.runs(), &root.id)?;
            record(
                tx,
                actor,
                root,
                Some(&op),
                CONTROL_AGGREGATE,
                root.id.0.as_str(),
                after.revision,
                "RootStopReapplied",
                json!({
                    "cause": cause.as_str(),
                    "stopEpoch": after.stop_epoch,
                    "cancelledDeliveries": cancelled,
                    "extendedRoots": extended,
                }),
            )?;
            return Ok((op, false));
        }
        let (mut after, hold) = self.pause_root(tx, actor, root, control, cause, reason)?;
        let mut cancelled = self.cancel_root_deliveries(tx, &root.id)?;
        let live = tx.runs().live_activation(&root.id)?;
        // The linked cards' retained work is stopped with the feature: their deliveries are
        // cancelled, their trees stopped by the stop's execution, and the stop settles once
        // they quiesced.
        let linked = linked_roots(tx, root)?;
        let mut linked_work = false;
        // Each card is covered at its generation now: a later activation of it (once it is
        // unlinked and continued) is never stopped, waited for or held by this stop.
        let mut covered = Vec::new();
        for card in &linked {
            cancelled.extend(self.cancel_root_deliveries(tx, &card.id)?);
            linked_work |= tx.runs().live_activation(&card.id)?.is_some()
                || self.root_has_work(tx, &card.id)?;
            covered.push(LinkedStop::of(&control_of(tx.runs(), &card.id)?).value());
        }
        let op = OperationId(tx.new_id(IdKind::Operation));
        tx.operations().record_intent(NewOperation {
            id: op.clone(),
            root_id: Some(root.id.clone()),
            assignment_id: None,
            capability: STOP_CAPABILITY.into(),
            schema_version: 1,
            args_digest: sha256_hex(
                format!(
                    "{}|{}|{}",
                    root.id.0.as_str(),
                    after.stop_epoch,
                    cause.as_str()
                )
                .as_bytes(),
            ),
            payload: json!({
                "rootId": root.id.0.as_str(),
                "stopEpoch": after.stop_epoch,
                "cause": cause.as_str(),
                "holdId": hold.id.as_str(),
                "executionId": live.as_ref().map(|a| a.id.as_str()),
                "linkedRoots": covered,
            }),
        })?;
        let before = after.clone();
        after.stop_operation_id = Some(op.clone());
        save_control(tx, &before, &mut after)?;
        let nothing_live = live.is_none() && !linked_work && !self.root_has_work(tx, &root.id)?;
        if nothing_live {
            recovery::complete_stop_operation(tx, &op, &[], "nothing of the root was running")?;
        }
        record(
            tx,
            actor,
            root,
            Some(&op),
            CONTROL_AGGREGATE,
            root.id.0.as_str(),
            after.revision,
            "RootStopRequested",
            json!({
                "cause": cause.as_str(),
                "stopEpoch": after.stop_epoch,
                "cancelledDeliveries": cancelled,
                "observedComplete": nothing_live,
            }),
        )?;
        Ok((op, nothing_live))
    }

    /// Extends the pending stop `op` of `feature` to the linked card `card` unless it already
    /// covers the card's current generation: the coverage is recorded durably at the card's
    /// generation now (the same fence as the cards the stop's intent names), its activation is
    /// asked to stop and its root-scoped deliveries are cancelled. The stop's execution then
    /// signals the card's launches of that generation and stops its trees, and the stop settles
    /// only once the card quiesced; boot recovery reads the same coverage. A card that moves
    /// past it later (unlinked and continued) is superseded as for the intent's cards. Returns
    /// whether the coverage was extended.
    fn extend_stop(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        feature: &WorkRoot,
        op: &OperationId,
        card: &WorkRoot,
        cause: StopCause,
    ) -> Result<bool, StorageError> {
        let control = control_of(tx.runs(), &card.id)?;
        if stop_linked_roots(tx, op)?
            .iter()
            .any(|c| c.root == card.id && !c.superseded(&control))
        {
            return Ok(false);
        }
        let covered = LinkedStop::of(&control);
        tx.runs().cover_stop(&StoredStopCoverage {
            operation_id: op.clone(),
            root_id: covered.root.clone(),
            generation: covered.generation,
            reactivate: covered.reactivate,
        })?;
        let activation = self.request_stop(tx, actor, card, cause)?;
        let cancelled = self.cancel_root_deliveries(tx, &card.id)?;
        let revision = control_of(tx.runs(), &feature.id)?.revision;
        record(
            tx,
            actor,
            feature,
            Some(op),
            CONTROL_AGGREGATE,
            feature.id.0.as_str(),
            revision,
            "RootStopExtended",
            json!({
                "linkedRoot": covered.value(),
                "activation": activation.as_ref().map(|a| json!({
                    "executionId": a.id.as_str(),
                    "phase": a.phase.as_str(),
                })),
                "cancelledDeliveries": cancelled,
            }),
        )?;
        Ok(true)
    }

    /// A card was linked to `feature_id` (`tasks.set_feature`): while the feature's stop is still
    /// pending and the feature still carries it, the stop is extended to the card (when it keeps
    /// run controls of its own; one without any has nothing running).
    pub(crate) fn card_linked(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        feature_id: &RootId,
        card_id: &RootId,
    ) -> Result<bool, StorageError> {
        let control = control_of(tx.runs(), feature_id)?;
        let Some(op) = control.stop_operation_id.clone() else {
            return Ok(false);
        };
        let Some(rec) = tx.operations().operation(&op)? else {
            return Ok(false);
        };
        if is_terminal(rec.state)
            || recovery::stop_superseded(&control)
            || tx.runs().root_control(card_id)?.is_none()
        {
            return Ok(false);
        }
        let (Some(feature), Some(card)) = (
            tx.sessions().work_root(feature_id)?,
            tx.sessions().work_root(card_id)?,
        ) else {
            return Ok(false);
        };
        let cause = rec
            .payload
            .get("cause")
            .and_then(Value::as_str)
            .and_then(StopCause::parse)
            .unwrap_or(StopCause::Stop);
        self.extend_stop(tx, actor, &feature, &op, &card, cause)
    }

    /// Cancels the root's unsettled root-scoped deliveries: a claimed one gets the durable
    /// cancellation marker; a held one is cancelled unless its operation has an outcome.
    fn cancel_root_deliveries(
        &self,
        tx: &mut dyn WriteTransaction,
        root: &RootId,
    ) -> Result<Vec<String>, StorageError> {
        let mut cancelled = Vec::new();
        let mut cursor: Option<Id> = None;
        loop {
            let page = tx.runs().root_deliveries(root, cursor.as_ref(), PAGE)?;
            let more = page.len() == usize::from(PAGE);
            for id in &page {
                let Some(e) = tx.queue().entry(id)? else {
                    continue;
                };
                let dispatched = match &e.operation_id {
                    Some(op) => tx
                        .operations()
                        .operation(op)?
                        .is_some_and(|o| possibly_dispatched(o.state)),
                    None => false,
                };
                let applies = cancellation_applies(tx.operations(), &e)?;
                let cancel = match e.state {
                    QueueState::Claimed => true,
                    QueueState::Reconciling => dispatched || applies,
                    QueueState::Ready => applies,
                    _ => false,
                };
                if cancel {
                    tx.queue().cancel(id)?;
                    cancelled.push(id.as_str().to_owned());
                }
            }
            cursor = page.last().cloned();
            if !more {
                return Ok(cancelled);
            }
        }
    }

    fn inspect(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        root_id: &RootId,
    ) -> Result<OperationResult, Fail> {
        let root = controlled_root(snapshot.sessions(), snapshot.runs(), ctx, root_id)?;
        Ok(OperationResult::RunsInspect {
            value: root_snapshot(snapshot.runs(), &root.id)?,
        })
    }

    fn eligibility(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        target: &EligibilityTarget,
        purpose: &Purpose,
    ) -> Result<OperationResult, Fail> {
        let runs = snapshot.runs();
        let (root_id, t) = match target {
            EligibilityTarget::Root(r) => (r.clone(), Target::Root),
            EligibilityTarget::Assignment(a) => match runs.assignment(a)? {
                Some(x) => (x.root_id.clone(), Target::Assignment(a.clone())),
                None => return Err(not_found("assignment", &a.0).into()),
            },
        };
        let root = match controlled_root(snapshot.sessions(), snapshot.runs(), ctx, &root_id) {
            Ok(r) => r,
            // An assignment of an invisible root does not disclose the root.
            Err(_) if matches!(target, EligibilityTarget::Assignment(_)) => {
                let EligibilityTarget::Assignment(a) = target else {
                    unreachable!()
                };
                return Err(not_found("assignment", &a.0).into());
            }
            Err(e) => return Err(e.into()),
        };
        let purpose_kind = match purpose {
            Purpose::Planning => ActivationPurpose::Planning,
            Purpose::Implementation => ActivationPurpose::Implementation,
            Purpose::Resume => ActivationPurpose::Resume,
        };
        let e = self.evaluate(runs, &root, &t, purpose_kind)?;
        Ok(OperationResult::RunsEligibility {
            value: EligibilitySnapshot {
                target: target.clone(),
                purpose: purpose.clone(),
                eligible: e.eligible,
                blockers: wire_blockers(&e.conditions),
            },
        })
    }
}

impl QueryHandler for ControlService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let answered = match query.input() {
            OperationInput::Runs(ops::Input::Inspect(q)) => match &q.query {
                Query::Root { root_id } => self.inspect(snapshot, ctx, root_id),
                _ => Err(refuse(DomainErrorCode::InvalidArgument, "query", None)),
            },
            OperationInput::Runs(ops::Input::Eligibility(q)) => match &q.query {
                Query::Eligibility { target, purpose } => {
                    self.eligibility(snapshot, ctx, target, purpose)
                }
                _ => Err(refuse(DomainErrorCode::InvalidArgument, "query", None)),
            },
            _ => Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "operation",
                None,
            )),
        };
        let result = answered.map_err(|f| fault_of(f, self.correlation()))?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(1)),
            result,
        })
    }
}

/// A query or command fault of a failure, carrying the hold IDs it names.
fn fault_of(f: Fail, correlation: CorrelationId) -> DomainFault {
    match f {
        Fail::Refused(r, holds) => {
            let mut fault = r.fault(correlation);
            fault.blocker_ids = BoundedList(holds.into_iter().take(128).map(HoldId).collect());
            fault
        }
        Fail::Storage(e) => query_fault(Failure::Storage(e), "root", correlation),
    }
}

impl CommandHandler for ControlService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let outcome: Result<Decided, Fail> = match command.input() {
            OperationInput::Runs(ops::Input::Start(r)) => match &r.command {
                RootCommand::Start {
                    root_id,
                    expected_work_revision,
                    expected_control_revision,
                } => self
                    .start(
                        tx,
                        ctx,
                        root_id,
                        expected_work_revision.get(),
                        expected_control_revision.get(),
                    )
                    .map(plain),
                _ => Err(refuse(DomainErrorCode::InvalidArgument, "command", None)),
            },
            OperationInput::Runs(ops::Input::Pause(r)) => match &r.command {
                RootCommand::Pause {
                    root_id,
                    expected_control_revision,
                } => self
                    .pause(tx, ctx, root_id, expected_control_revision.get())
                    .map(plain),
                _ => Err(refuse(DomainErrorCode::InvalidArgument, "command", None)),
            },
            OperationInput::Runs(ops::Input::Continue(r)) => match &r.command {
                RootCommand::Continue {
                    root_id,
                    expected_control_revision,
                    covered_holds,
                } => self
                    .continue_(
                        tx,
                        ctx,
                        command,
                        root_id,
                        expected_control_revision.get(),
                        covered_holds,
                    )
                    .map(plain),
                _ => Err(refuse(DomainErrorCode::InvalidArgument, "command", None)),
            },
            OperationInput::Runs(ops::Input::Redirect(i)) => self.redirect(tx, ctx, i).map(plain),
            OperationInput::Runs(ops::Input::Stop(i)) => self.stop(tx, ctx, i),
            _ => Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "operation",
                None,
            )),
        };
        match outcome {
            Ok(d) => Ok(d),
            Err(Fail::Storage(e)) => Err(HandlerFailure::Storage(e)),
            Err(refused) => Ok(Decided {
                decision: DomainDecision::Refused {
                    fault: fault_of(refused, CorrelationId(tx.new_id(IdKind::Correlation))),
                },
                operation_id: None,
            }),
        }
    }
}
fn plain(decision: DomainDecision) -> Decided {
    Decided {
        decision,
        operation_id: None,
    }
}
