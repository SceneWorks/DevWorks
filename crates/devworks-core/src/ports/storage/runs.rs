//! Run root control records (CONTROL-01..13): the root control (desired execution and its
//! revision), activations (the `executions` table: one dispatch-capable activation per root),
//! assignments (root and assignment identity), cause-specific blockers, redirect instructions
//! with their stages, and the one in-flight continuation turn of each session. The control
//! service (`crate::services::runs`) is the only writer; every update is a compare-and-set on the
//! record's revision, so a stale writer gets a [`StorageError::RevisionConflict`] and changes
//! nothing.
use super::{
    AgentMessageHistoryFilter, MAX_PAGE, StorageError, StoredAgentAssignmentContext,
    StoredAgentDelivery, StoredAgentMessage, StoredAgentMessageEvent,
};
use crate::protocol::{ActorId, AssignmentId, Id, OperationId, ProjectId, RootId};
use serde_json::Value;

/// Desired root execution: the only state root controls change (AD17).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesiredExecution {
    Enabled,
    Paused,
}
impl DesiredExecution {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "Enabled",
            Self::Paused => "Paused",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Enabled" => Some(Self::Enabled),
            "Paused" => Some(Self::Paused),
            _ => None,
        }
    }
}

/// A root's control record. A root without a stored record is `Enabled` at revision 1 with no
/// activation generation yet ([`StoredRootControl::initial`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredRootControl {
    pub root_id: RootId,
    pub desired: DesiredExecution,
    /// The control revision every root control command expects.
    pub revision: i64,
    /// Generation of the root's latest activation (0 before the first).
    pub generation: i64,
    /// The boot whose restart materialized a manual-Continue hold for this root.
    pub restart_boot_id: Option<Id>,
    /// Advances with every redirect: the instruction context revision the coordinator must bind.
    pub instruction_revision: i64,
    /// The active approved plan revision the root's assignments were last adopted under.
    pub adopted_plan_revision: Option<i64>,
    /// Advances with every stop (user stop, archival): work admitted under an older epoch is
    /// cancelled.
    pub stop_epoch: i64,
    /// The journaled stop whose outcome is still to be observed, if any.
    pub stop_operation_id: Option<OperationId>,
    /// The activation generation the latest stop covers: its signals, process stops and
    /// quiescence check never reach a newer generation (one a later Continue created).
    pub stop_generation: i64,
    /// Continue was accepted while the previous activation was still stopping: a new activation
    /// is created when it stops.
    pub reactivate: bool,
    pub updated_at_ms: i64,
}
impl StoredRootControl {
    pub fn initial(root_id: RootId) -> Self {
        Self {
            root_id,
            desired: DesiredExecution::Enabled,
            revision: 1,
            generation: 0,
            restart_boot_id: None,
            instruction_revision: 0,
            adopted_plan_revision: None,
            stop_epoch: 0,
            stop_operation_id: None,
            stop_generation: 0,
            reactivate: false,
            updated_at_ms: 0,
        }
    }
}

/// Activation phases (AD17): `Pending → Claimed → Running → Stopping → Stopped`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivationPhase {
    Pending,
    Claimed,
    Running,
    Stopping,
    Stopped,
}
impl ActivationPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Claimed => "Claimed",
            Self::Running => "Running",
            Self::Stopping => "Stopping",
            Self::Stopped => "Stopped",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "Pending" => Self::Pending,
            "Claimed" => Self::Claimed,
            "Running" => Self::Running,
            "Stopping" => Self::Stopping,
            "Stopped" => Self::Stopped,
            _ => return None,
        })
    }
    /// Every phase but `Stopped`: at most one live activation per root.
    pub fn is_live(self) -> bool {
        self != Self::Stopped
    }
}

/// What an activation was accepted for. Planning (and readiness assessment) needs no approved
/// plan; implementation dispatch needs the active approved scope and current readiness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivationPurpose {
    Planning,
    Implementation,
    Resume,
}
impl ActivationPurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planning => "Planning",
            Self::Implementation => "Implementation",
            Self::Resume => "Resume",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "Planning" => Self::Planning,
            "Implementation" => Self::Implementation,
            "Resume" => Self::Resume,
            _ => return None,
        })
    }
}

/// Why an activation was asked to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopCause {
    Pause,
    Stop,
    Shutdown,
    Restart,
    Archive,
    Failure,
    Superseded,
}
impl StopCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pause => "Pause",
            Self::Stop => "Stop",
            Self::Shutdown => "Shutdown",
            Self::Restart => "Restart",
            Self::Archive => "Archive",
            Self::Failure => "Failure",
            Self::Superseded => "Superseded",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "Pause" => Self::Pause,
            "Stop" => Self::Stop,
            "Shutdown" => Self::Shutdown,
            "Restart" => Self::Restart,
            "Archive" => Self::Archive,
            "Failure" => Self::Failure,
            "Superseded" => Self::Superseded,
            _ => return None,
        })
    }
}

/// The attributed terminal outcome of an activation. Ending an activation is never evidence
/// that its work is complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivationOutcome {
    Completed,
    Failed,
    Cancelled,
    /// The process ended (crash, quit, restart) while it was live.
    Interrupted,
    /// Stopped before it was ever launched (a pause or stop won before dispatch admission).
    Prevented,
}
impl ActivationOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "Completed",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
            Self::Interrupted => "Interrupted",
            Self::Prevented => "Prevented",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "Completed" => Self::Completed,
            "Failed" => Self::Failed,
            "Cancelled" => Self::Cancelled,
            "Interrupted" => Self::Interrupted,
            "Prevented" => Self::Prevented,
            _ => return None,
        })
    }
}

/// One activation (an `ExecutionId` attempt) of a root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredActivation {
    pub id: Id,
    pub root_id: RootId,
    /// The boot that accepted it; dispatch requires the current boot.
    pub boot_id: Id,
    pub generation: i64,
    pub phase: ActivationPhase,
    pub purpose: ActivationPurpose,
    pub owner: Option<Id>,
    pub lease_until_ms: Option<i64>,
    /// Advances on every claim: the fence a supervisor presents to renew, dispatch and finish.
    pub owner_epoch: i64,
    pub revision: i64,
    /// Claims after a transient failure (bounded, categorized retries).
    pub attempts: i64,
    /// Earliest time a pending activation may be claimed (retry backoff).
    pub ready_at_ms: i64,
    pub created_at_ms: i64,
    pub stop_cause: Option<StopCause>,
    pub stop_requested_at_ms: Option<i64>,
    pub stop_acknowledged_at_ms: Option<i64>,
    pub stopped_at_ms: Option<i64>,
    pub outcome: Option<ActivationOutcome>,
    /// Inspectable reason (failure detail, escalation).
    pub detail: Option<String>,
    /// The active approved plan revision it was accepted under (features only).
    pub plan_revision: Option<i64>,
}

/// Which activations a listing covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivationFilter {
    /// Every activation of a root, newest generation first.
    Root(RootId),
    /// Pending activations of `boot` ready by `now_ms`, oldest first.
    Claimable { boot: Id, now_ms: i64 },
    /// Live activations of any boot but `boot`.
    LivePriorBoots(Id),
    /// Live activations of `boot`.
    LiveOfBoot(Id),
}

/// Blocker causes (AD17), the storage spelling of the wire `Cause`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum HoldCause {
    HumanPause,
    RestartContinue,
    BudgetContinue,
    Approval,
    Dependency,
    Reconciliation,
    Credentials,
    Capacity,
    StorageUnavailable,
    Policy,
}
impl HoldCause {
    pub const ALL: [HoldCause; 10] = [
        Self::HumanPause,
        Self::RestartContinue,
        Self::BudgetContinue,
        Self::Approval,
        Self::Dependency,
        Self::Reconciliation,
        Self::Credentials,
        Self::Capacity,
        Self::StorageUnavailable,
        Self::Policy,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HumanPause => "HumanPause",
            Self::RestartContinue => "RestartContinue",
            Self::BudgetContinue => "BudgetContinue",
            Self::Approval => "Approval",
            Self::Dependency => "Dependency",
            Self::Reconciliation => "Reconciliation",
            Self::Credentials => "Credentials",
            Self::Capacity => "Capacity",
            Self::StorageUnavailable => "StorageUnavailable",
            Self::Policy => "Policy",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }
    /// Only an authorized Continue clears these (manual-resume conditions).
    pub fn manual_resume(self) -> bool {
        matches!(
            self,
            Self::HumanPause | Self::RestartContinue | Self::BudgetContinue
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockerState {
    Active,
    Cleared,
}

/// A cause-specific blocker on a root, an assignment or an operation. Its owning service clears
/// it when evidence satisfies its clearance condition; Continue clears only covered
/// manual-resume conditions.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredBlocker {
    pub id: Id,
    pub root_id: Option<RootId>,
    pub assignment_id: Option<AssignmentId>,
    pub operation_id: Option<OperationId>,
    pub cause: HoldCause,
    /// The record that raised it (a control revision, boot, proposal, operation, ...).
    pub origin_id: String,
    pub origin_revision: i64,
    pub state: BlockerState,
    pub revision: i64,
    /// Inspectable reason shown with the blocked work.
    pub reason: String,
    /// The clearance condition and evidence (cause-specific JSON object).
    pub clearance: Value,
    pub created_at_ms: i64,
    pub cleared_at_ms: Option<i64>,
    pub cleared_by: Option<ActorId>,
}

/// Which active blockers a listing covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockerFilter {
    /// Every active blocker within the root (root-level, its assignments and operations).
    Root(RootId),
    /// Active blockers on the root itself (no assignment or operation subject).
    RootLevel(RootId),
    Assignment(AssignmentId),
    Operation(OperationId),
    /// Active root-level blockers of one cause, across every root (the supervisor's sweeps).
    RootLevelCause(HoldCause),
}

/// Assignment states (AD17). Waiting is not a manual pause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignmentState {
    Ready,
    Active,
    Waiting,
    Completed,
    Failed,
    Superseded,
}

/// A primary work owner and its delegated children share a root but have different custody.
/// Only primary assignments participate in the one-live-owner constraint for a work item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignmentPurpose {
    Primary,
    Child,
}
impl AssignmentPurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "Primary",
            Self::Child => "Child",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "Primary" => Some(Self::Primary),
            "Child" => Some(Self::Child),
            _ => None,
        }
    }
}
impl AssignmentState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "Ready",
            Self::Active => "Active",
            Self::Waiting => "Waiting",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
            Self::Superseded => "Superseded",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "Ready" => Self::Ready,
            "Active" => Self::Active,
            "Waiting" => Self::Waiting,
            "Completed" => Self::Completed,
            "Failed" => Self::Failed,
            "Superseded" => Self::Superseded,
            _ => return None,
        })
    }
    pub fn is_live(self) -> bool {
        matches!(self, Self::Ready | Self::Active | Self::Waiting)
    }
}

/// A coordinator (no parent) or worker assignment of one root.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredAssignment {
    pub id: AssignmentId,
    pub purpose: AssignmentPurpose,
    pub root_id: RootId,
    /// The work item it is for: the root itself or work in the root's hierarchy.
    pub work_id: RootId,
    pub parent_id: Option<AssignmentId>,
    pub session_id: Id,
    /// The activation it currently runs under.
    pub execution_id: Option<Id>,
    pub state: AssignmentState,
    /// Owner generation: advances whenever it is rebound to a new activation.
    pub generation: i64,
    pub slot_held: bool,
    pub revision: i64,
    pub checkpoint: Option<Value>,
    /// The approved scope unit it implements (none: the coordinator's feature-wide scope).
    pub scope_key: Option<String>,
    /// Digest of that unit in the plan revision it was bound under.
    pub scope_digest: Option<String>,
    pub plan_revision: Option<i64>,
    /// The redirect context revision it last bound.
    pub instruction_revision: i64,
    pub created_at_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstructionStage {
    Received,
    Acknowledged,
    Incorporated,
}
impl InstructionStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Received => "Received",
            Self::Acknowledged => "Acknowledged",
            Self::Incorporated => "Incorporated",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "Received" => Self::Received,
            "Acknowledged" => Self::Acknowledged,
            "Incorporated" => Self::Incorporated,
            _ => return None,
        })
    }
}

/// One redirect instruction and its durable stages.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredInstruction {
    pub id: Id,
    pub root_id: RootId,
    /// Per-root order of instructions (1, 2, ...).
    pub seq: i64,
    pub actor: ActorId,
    pub message: String,
    /// The attachments' wire references (JSON array).
    pub attachments: Value,
    pub control_revision: i64,
    pub instruction_revision: i64,
    pub stage: InstructionStage,
    pub received_at_ms: i64,
    pub acknowledged_at_ms: Option<i64>,
    /// The activation that bound it at a safe boundary.
    pub acknowledged_by: Option<Id>,
    pub incorporated_at_ms: Option<i64>,
    /// The recorded plan/assignment disposition.
    pub disposition: Option<Value>,
    /// The coordinator session and transcript entry it was linked to.
    pub session_id: Option<Id>,
    pub transcript_entry_id: Option<Id>,
    pub revision: i64,
}

/// The one inference/continuation turn in flight for a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredTurn {
    pub session_id: Id,
    pub turn_id: Id,
    pub assignment_id: AssignmentId,
    pub execution_id: Id,
    pub owner_epoch: i64,
    pub boot_id: Id,
    pub started_at_ms: i64,
}

/// A feature's plan slot as stored by the planning service (`feature_plans`): only the active
/// approved plan authorizes implementation; a pending revision never does.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredPlanState {
    pub revision: i64,
    pub approved: bool,
    pub pending_revision: Option<i64>,
    /// The active approved plan as the planning service stored it.
    pub approved_plan: Option<Value>,
    /// The feature's readiness assessment (its revision and body), if one was recorded.
    pub readiness: Option<(i64, Value)>,
}

/// A linked card a pending feature stop came to cover after it was decided (the card was
/// linked while the stop was pending, or a reapply reached it at a newer generation), fenced
/// like the cards the stop's journaled intent names: the card's activation generation then, and
/// whether a reactivation was already pending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredStopCoverage {
    pub operation_id: OperationId,
    pub root_id: RootId,
    pub generation: i64,
    pub reactivate: bool,
}

pub trait RunRead {
    fn agent_assignment_context(
        &self,
        assignment: &AssignmentId,
    ) -> Result<Option<StoredAgentAssignmentContext>, StorageError>;
    fn agent_message(&self, id: &Id) -> Result<Option<StoredAgentMessage>, StorageError>;
    fn agent_message_by_key(
        &self,
        project: &ProjectId,
        sender: &ActorId,
        key: &str,
    ) -> Result<Option<StoredAgentMessage>, StorageError>;
    /// Project history, newest first. `before` is the last message ID of the prior page.
    fn agent_messages(
        &self,
        project: &ProjectId,
        before: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredAgentMessage>, StorageError>;
    /// Direct session deliveries still eligible for one successor, in delivery order.
    fn unhanded_direct_deliveries(
        &self,
        project: &ProjectId,
        after: i64,
        limit: u16,
    ) -> Result<Vec<(StoredAgentDelivery, StoredAgentMessage)>, StorageError>;
    fn agent_message_history(
        &self,
        filter: &AgentMessageHistoryFilter<'_>,
    ) -> Result<Vec<(i64, StoredAgentMessage)>, StorageError>;
    /// Assignment's inbound/outbound message thread, newest first; `before` is the prior page's
    /// final ID. An unacknowledged filter only selects inbound pending/delivered copies.
    fn agent_assignment_thread(
        &self,
        project: &ProjectId,
        assignment: &AssignmentId,
        direction: &str,
        unacknowledged_only: bool,
        before: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<(StoredAgentMessage, Option<StoredAgentDelivery>)>, StorageError>;
    fn agent_delivery(
        &self,
        message: &Id,
        recipient: Option<&AssignmentId>,
    ) -> Result<Option<StoredAgentDelivery>, StorageError>;
    fn agent_session_delivery(
        &self,
        message: &Id,
        session: &Id,
    ) -> Result<Option<StoredAgentDelivery>, StorageError>;
    fn agent_recipient_count(&self, message: &Id) -> Result<usize, StorageError>;
    /// Latest agent message post, delivery, or acknowledgement attributable to this session.
    fn agent_session_activity_at(&self, session: &Id) -> Result<Option<i64>, StorageError>;
    /// Open questions to a human that hold implementation of exactly this work item.
    fn open_blocking_question(&self, item: &RootId) -> Result<bool, StorageError>;
    /// Delivery cursor is global, so a session can resume after a crash without reordering.
    fn agent_inbox(
        &self,
        project: &ProjectId,
        recipient: Option<&AssignmentId>,
        after: i64,
        include_read: bool,
        limit: u16,
    ) -> Result<Vec<(StoredAgentDelivery, StoredAgentMessage)>, StorageError>;
    fn agent_session_inbox(
        &self,
        project: &ProjectId,
        session: &Id,
        after: i64,
        include_read: bool,
        limit: u16,
    ) -> Result<Vec<(StoredAgentDelivery, StoredAgentMessage)>, StorageError>;
    fn agent_delivery_head(&self) -> Result<i64, StorageError>;
    fn agent_message_events(
        &self,
        project: &ProjectId,
        after: i64,
        limit: u16,
    ) -> Result<Vec<StoredAgentMessageEvent>, StorageError>;
    fn agent_event_head(&self) -> Result<i64, StorageError>;
    fn root_control(&self, root: &RootId) -> Result<Option<StoredRootControl>, StorageError>;
    /// Roots with a journaled stop whose outcome is still to be observed, in root order after
    /// `after`, at most `limit` (1..=500).
    fn pending_stops(
        &self,
        after: Option<&RootId>,
        limit: u16,
    ) -> Result<Vec<StoredRootControl>, StorageError>;
    fn activation(&self, id: &Id) -> Result<Option<StoredActivation>, StorageError>;
    /// The root's live (not `Stopped`) activation, if any.
    fn live_activation(&self, root: &RootId) -> Result<Option<StoredActivation>, StorageError>;
    /// Activations matching `filter` after `after` (by ID), at most `limit` (1..=500).
    fn activations(
        &self,
        filter: &ActivationFilter,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredActivation>, StorageError>;
    fn blocker(&self, id: &Id) -> Result<Option<StoredBlocker>, StorageError>;
    /// Active blockers matching `filter` in ID order after `after`, at most `limit` (1..=500).
    fn blockers(
        &self,
        filter: &BlockerFilter,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredBlocker>, StorageError>;
    fn assignment(&self, id: &AssignmentId) -> Result<Option<StoredAssignment>, StorageError>;
    /// Global work identity lookup; membership may have changed since its worker was assigned.
    fn live_assignment_for_work(
        &self,
        work: &RootId,
    ) -> Result<Option<StoredAssignment>, StorageError>;
    /// No locally unclosed canonical attempts captured under this managed execution root.
    fn inference_quiescent(&self, root: &RootId) -> Result<bool, StorageError>;
    /// Shared safe-boundary check, including assignment-owned effects without queue entries.
    fn assignment_handoff_quiescent(&self, assignment: &AssignmentId)
    -> Result<bool, StorageError>;
    /// Current task inheritance; dispatch checks every referenced requirement, not only one unit.
    fn work_inherited_requirements(&self, work: &RootId) -> Result<Vec<Id>, StorageError>;
    /// The root's assignments in ID order after `after` (only live ones when `live_only`).
    fn assignments(
        &self,
        root: &RootId,
        live_only: bool,
        after: Option<&AssignmentId>,
        limit: u16,
    ) -> Result<Vec<StoredAssignment>, StorageError>;
    fn instruction(&self, id: &Id) -> Result<Option<StoredInstruction>, StorageError>;
    /// The root's instructions with sequence greater than `after_seq`, in order.
    fn instructions(
        &self,
        root: &RootId,
        after_seq: i64,
        limit: u16,
    ) -> Result<Vec<StoredInstruction>, StorageError>;
    fn session_turn(&self, session: &Id) -> Result<Option<StoredTurn>, StorageError>;
    /// Turns recorded by any boot but `boot`.
    fn turns_prior_boots(&self, boot: &Id, limit: u16) -> Result<Vec<StoredTurn>, StorageError>;
    fn feature_plan(&self, feature: &RootId) -> Result<Option<StoredPlanState>, StorageError>;
    /// The work item's current feature membership (`None`: no such item; `Some(None)`: it has
    /// no feature).
    fn work_parent(&self, work: &RootId) -> Result<Option<Option<RootId>>, StorageError>;
    /// The repositories currently named by a work item (checked against active plan scope).
    fn work_repository_ids(&self, work: &RootId) -> Result<Vec<Id>, StorageError>;
    /// The work item's prerequisites that are not completed with verified acceptance.
    fn open_prerequisites(&self, work: &RootId) -> Result<Vec<RootId>, StorageError>;
    /// The project's lifecycle label (`None` for an unknown project).
    fn project_lifecycle(&self, project: &ProjectId) -> Result<Option<String>, StorageError>;
    /// Root work items of a project in ID order: features, standalone issues/tasks, and cards
    /// linked to a feature that keep run controls of their own (an archival stops them too).
    fn project_roots(
        &self,
        project: &ProjectId,
        after: Option<&RootId>,
        limit: u16,
    ) -> Result<Vec<RootId>, StorageError>;
    /// Unsettled (ready, claimed, held or failed) queue deliveries scoped to the root, by ID.
    fn root_deliveries(
        &self,
        root: &RootId,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<Id>, StorageError>;
    /// The cards the stop `op` came to cover after it was decided, in root order after `after`,
    /// at most `limit` (1..=500).
    fn stop_coverage(
        &self,
        op: &OperationId,
        after: Option<&RootId>,
        limit: u16,
    ) -> Result<Vec<StoredStopCoverage>, StorageError>;
}

pub trait RunRepository: RunRead {
    fn insert_agent_assignment_context(
        &mut self,
        context: &StoredAgentAssignmentContext,
    ) -> Result<(), StorageError>;
    fn insert_agent_message(&mut self, message: &StoredAgentMessage) -> Result<(), StorageError>;
    fn resolve_agent_message(
        &mut self,
        message: &StoredAgentMessage,
        expected_revision: i64,
    ) -> Result<(), StorageError>;
    fn insert_agent_delivery(&mut self, delivery: &StoredAgentDelivery)
    -> Result<(), StorageError>;
    fn update_agent_delivery(
        &mut self,
        delivery: &StoredAgentDelivery,
        expected_revision: i64,
    ) -> Result<(), StorageError>;
    fn insert_agent_message_event(
        &mut self,
        event: &StoredAgentMessageEvent,
    ) -> Result<(), StorageError>;
    /// Inserts the root's first control record (its revision is the record's).
    fn insert_root_control(&mut self, control: &StoredRootControl) -> Result<(), StorageError>;
    /// Replaces the record when its stored revision is `expected` (a `Control` conflict
    /// otherwise).
    fn update_root_control(
        &mut self,
        control: &StoredRootControl,
        expected: i64,
    ) -> Result<(), StorageError>;
    /// Inserts an activation. A second live activation of one root is
    /// [`StorageError::Duplicate`].
    fn insert_activation(&mut self, activation: &StoredActivation) -> Result<(), StorageError>;
    fn update_activation(
        &mut self,
        activation: &StoredActivation,
        expected: i64,
    ) -> Result<(), StorageError>;
    /// Inserts a blocker. An active blocker with the same subject, cause and origin is
    /// [`StorageError::Duplicate`].
    fn insert_blocker(&mut self, blocker: &StoredBlocker) -> Result<(), StorageError>;
    fn update_blocker(
        &mut self,
        blocker: &StoredBlocker,
        expected: i64,
    ) -> Result<(), StorageError>;
    /// Inserts an assignment. A second live primary owner of one work item is
    /// [`StorageError::Duplicate`]; child assignments retain their own identity.
    fn insert_assignment(&mut self, assignment: &StoredAssignment) -> Result<(), StorageError>;
    fn update_assignment(
        &mut self,
        assignment: &StoredAssignment,
        expected: i64,
    ) -> Result<(), StorageError>;
    /// Moves one superseded, quiescent task worker to its current membership, preserving its
    /// identity/session and checkpoint. Also advances the session's context epoch/revision and
    /// relocates assignment-local holds. Ordinary updates cannot change these roots.
    fn reparent_assignment(
        &mut self,
        assignment: &StoredAssignment,
        expected_assignment: i64,
        expected_session_revision: i64,
    ) -> Result<(), StorageError>;
    fn insert_instruction(&mut self, instruction: &StoredInstruction) -> Result<(), StorageError>;
    fn update_instruction(
        &mut self,
        instruction: &StoredInstruction,
        expected: i64,
    ) -> Result<(), StorageError>;
    /// Starts a session's turn. A session with a turn in flight is [`StorageError::Duplicate`].
    fn insert_turn(&mut self, turn: &StoredTurn) -> Result<(), StorageError>;
    /// Ends the session's turn `turn`; another (or no) turn is [`StorageError::LeaseLost`].
    fn delete_turn(&mut self, session: &Id, turn: &Id) -> Result<(), StorageError>;
    /// Records (or moves forward) the stop's coverage of one card. A coverage never moves to an
    /// older generation ([`StorageError::InvalidRecord`]).
    fn cover_stop(&mut self, coverage: &StoredStopCoverage) -> Result<(), StorageError>;
}

/// The page bound every listing of this repository honours.
pub const RUN_PAGE: u16 = MAX_PAGE;
