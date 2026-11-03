//! Shared envelope, scalar, receipt, read and event DTOs adopted from the recorded
//! `1.0-draft.1` candidates (tools/contracts-eval/src/lib.rs). Structural decoding and the
//! cross-field checks here are not authorization; the gateway and host ports own that.
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, collections::HashSet};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Id(pub(crate) String);
impl TryFrom<String> for Id {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        if s.is_empty()
            || s.len() > 128
            || !s
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        {
            return Err("invalid opaque ID/key");
        }
        Ok(Self(s))
    }
}
impl From<Id> for String {
    fn from(v: Id) -> Self {
        v.0
    }
}
impl JsonSchema for Id {
    fn schema_name() -> Cow<'static, str> {
        "Id".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_-]+$"})
    }
}
macro_rules! ids { ($($name:ident),*) => {$ (
    #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    #[serde(transparent)] pub struct $name(pub Id);
)*}; }
ids!(
    RootId,
    AssignmentId,
    OperationId,
    ResourceId,
    HoldId,
    ExecutionId,
    CommandId,
    EventId,
    ActorId,
    StoreId,
    ProjectId,
    AttemptId,
    QueryId,
    RequestKey,
    CorrelationId
);

/// Canonical decimal string, 0..i64::MAX. Range is checked by typed decoding, not a schema format annotation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Counter(pub(crate) String);
impl TryFrom<String> for Counter {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        if s.is_empty()
            || s.len() > 19
            || !s.bytes().all(|c| c.is_ascii_digit())
            || (s.len() > 1 && s.starts_with('0'))
            || s.parse::<i64>().is_err()
        {
            return Err("invalid counter");
        }
        Ok(Self(s))
    }
}
impl From<Counter> for String {
    fn from(v: Counter) -> Self {
        v.0
    }
}
impl JsonSchema for Counter {
    fn schema_name() -> Cow<'static, str> {
        "Counter".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","pattern":"^(0|[1-9][0-9]{0,18})$","description":"Canonical decimal 0..9223372036854775807. Typed range validation required."})
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Revision(pub(crate) String);
impl TryFrom<String> for Revision {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        let c = Counter::try_from(s)?;
        if c.0 == "0" {
            return Err("revision must be positive");
        }
        Ok(Self(c.0))
    }
}
impl From<Revision> for String {
    fn from(v: Revision) -> Self {
        v.0
    }
}
impl JsonSchema for Revision {
    fn schema_name() -> Cow<'static, str> {
        "Revision".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","pattern":"^[1-9][0-9]{0,18}$","description":"Positive signed-64-bit counter; typed upper-bound validation required."})
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HoldRef {
    pub hold_id: HoldId,
    pub revision: Revision,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum Protocol {
    #[serde(rename = "1.0-draft.1")]
    Draft1,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandRequest {
    pub protocol: Protocol,
    pub request_key: RequestKey,
    pub command: RootCommand,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum RootCommand {
    #[serde(rename = "runs.start")]
    Start {
        root_id: RootId,
        expected_work_revision: Revision,
        expected_control_revision: Revision,
    },
    #[serde(rename = "runs.pause")]
    Pause {
        root_id: RootId,
        expected_control_revision: Revision,
    },
    #[serde(rename = "runs.continue")]
    Continue {
        root_id: RootId,
        expected_control_revision: Revision,
        #[schemars(length(min = 1, max = 128))]
        covered_holds: Vec<HoldRef>,
    },
}
impl CommandRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if let RootCommand::Continue { covered_holds, .. } = &self.command {
            if covered_holds.is_empty() || covered_holds.len() > 128 {
                return Err("covered holds out of bounds");
            }
            let mut ids = HashSet::new();
            if covered_holds.iter().any(|h| !ids.insert(&h.hold_id.0.0)) {
                return Err("duplicate hold ID");
            }
        }
        Ok(())
    }
    pub fn normalize(&mut self) {
        if let RootCommand::Continue { covered_holds, .. } = &mut self.command {
            covered_holds.sort_by(|a, b| a.hold_id.0.0.cmp(&b.hold_id.0.0));
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "id",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum Subject {
    Root(RootId),
    Assignment(AssignmentId),
    Operation(OperationId),
    Resource(ResourceId),
    Blocker(HoldId),
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "id",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum EligibilityTarget {
    Root(RootId),
    Assignment(AssignmentId),
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum Purpose {
    Planning,
    Implementation,
    Resume,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum DesiredState {
    Enabled,
    Paused,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum Phase {
    Pending,
    Claimed,
    Running,
    Stopping,
    Stopped,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum Cause {
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum RevisionKind {
    Work,
    Control,
    Blocker,
    Operation,
    Policy,
    Scope,
    Plan,
    Authority,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum RetryAdvice {
    Never,
    SameCommand,
    AfterBlockerClears,
    ResolveReceipt,
    ReconcileOperation,
    RefreshAndNewRequest,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootControl {
    pub root_id: RootId,
    pub revision: Revision,
    pub desired_state: DesiredState,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Activation {
    pub execution_id: ExecutionId,
    pub generation: Revision,
    pub phase: Phase,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Blocker {
    pub hold_id: HoldId,
    pub revision: Revision,
    pub subject: Subject,
    pub cause: Cause,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Blockers {
    #[schemars(length(max = 100))]
    pub items: Vec<Blocker>,
    pub has_more: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum Decision {
    Accepted {
        root: RootControl,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        activation: Option<Activation>,
        #[schemars(length(max = 128))]
        cleared_holds: Vec<HoldRef>,
        remaining: Blockers,
    },
    AlreadyActive {
        root: RootControl,
        activation: Activation,
    },
    ContinueRequired {
        root_id: RootId,
        #[schemars(length(min = 1, max = 128))]
        holds: Vec<HoldRef>,
    },
    Blocked {
        root_id: RootId,
        blockers: Blockers,
    },
    RevisionConflict {
        subject: Subject,
        revision_kind: RevisionKind,
        expected: Revision,
        current: Revision,
    },
    Archived {
        root_id: RootId,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Receipt<D = Decision> {
    pub command_id: CommandId,
    pub committed_sequence: Revision,
    pub accepted_at_ms: Counter,
    #[schemars(regex(pattern = "^[a-f0-9]{64}$"))]
    pub request_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
    pub decision: D,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Rejection {
    InvalidRequest,
    UnsupportedSchema,
    PermissionDenied,
    InvalidTarget,
    IdempotencyConflict,
    StorageUnavailable,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum CommandReply<D = Decision> {
    Recorded {
        protocol: Protocol,
        receipt: Receipt<D>,
        replayed: bool,
    },
    Rejected {
        protocol: Protocol,
        request_key: RequestKey,
        code: Rejection,
        retry: RetryAdvice,
        correlation_id: CorrelationId,
    },
    CommandStatusUnknown {
        protocol: Protocol,
        request_key: RequestKey,
        correlation_id: CorrelationId,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueryRequest {
    pub protocol: Protocol,
    pub query_id: QueryId,
    pub query: Query,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum Query {
    #[serde(rename = "runs.inspect")]
    Root { root_id: RootId },
    #[serde(rename = "runs.eligibility")]
    Eligibility {
        target: EligibilityTarget,
        purpose: Purpose,
    },
    #[serde(rename = "history.operation_status")]
    Operation { operation_id: OperationId },
    #[serde(rename = "history.command_receipt")]
    Receipt { request_key: RequestKey },
    #[serde(rename = "history.events")]
    Events {
        root_id: RootId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(length(min = 1, max = 4096))]
        after: Option<String>,
        #[schemars(range(min = 1, max = 500))]
        limit: u16,
    },
}
impl QueryRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if let Query::Events { after, limit, .. } = &self.query
            && (*limit == 0
                || *limit > 500
                || after
                    .as_ref()
                    .is_some_and(|s| s.is_empty() || s.len() > 4096))
        {
            return Err("invalid bounded event page");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum OperationState {
    IntentRecorded,
    Preflighted,
    Dispatching,
    ObservedComplete,
    Failed,
    OutcomeUnknown,
    NeedsInput,
    RetryPending,
    CancelledBeforeDispatch,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadStamp {
    pub store_id: StoreId,
    pub restore_epoch: Counter,
    pub sequence: Counter,
    pub policy_revision: Revision,
    pub scope_revision: Revision,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum QueryError {
    PermissionDenied,
    NotFound,
    InvalidRequest,
    CursorExpired,
    ViewUnavailable,
    UnsupportedSchema,
    StorageUnavailable,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum QueryReply {
    Result {
        protocol: Protocol,
        query_id: QueryId,
        stamp: ReadStamp,
        result: QueryResult,
    },
    Rejected {
        protocol: Protocol,
        query_id: QueryId,
        code: QueryError,
        retry: RetryAdvice,
        correlation_id: CorrelationId,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum QueryResult {
    Root {
        root: RootControl,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        activation: Option<Activation>,
        blockers: Blockers,
    },
    Eligibility {
        target: EligibilityTarget,
        purpose: Purpose,
        eligible: bool,
        blockers: Blockers,
    },
    Operation {
        operation_id: OperationId,
        revision: Revision,
        state: OperationState,
        retry: RetryAdvice,
    },
    Receipt {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        receipt: Option<Receipt>,
    },
    Events {
        #[schemars(length(max = 500))]
        events: Vec<Event>,
        #[schemars(length(min = 1, max = 4096))]
        next_cursor: String,
        has_more: bool,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Event {
    pub protocol: Protocol,
    pub event_id: EventId,
    pub sequence: Revision,
    pub aggregate: Subject,
    pub aggregate_revision: Revision,
    pub aggregate_revision_kind: RevisionKind,
    pub actor_id: ActorId,
    pub occurred_at_ms: Counter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<CommandId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_id: Option<RootId>,
    pub event: EventBody,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum EventBody {
    CommandDecided {
        command_id: CommandId,
    },
    RootControlChanged {
        root: RootControl,
        reason: ControlReason,
    },
    ActivationRequested {
        root_id: RootId,
        execution_id: ExecutionId,
        generation: Revision,
    },
    BlockerChanged {
        blocker: Blocker,
        change: BlockerChange,
    },
    OperationObserved {
        operation_id: OperationId,
        attempt_id: AttemptId,
        owner_generation: Revision,
        observation: ObservationDisposition,
        state: OperationState,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ControlReason {
    Paused,
    Continued,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum BlockerChange {
    Raised,
    Cleared,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ObservationDisposition {
    CurrentOwner,
    StaleOwnerEvidenceOnly,
}

// Cross-field checks follow structural decoding; these are not authorization checks.
fn holds_valid(holds: &[HoldRef], minimum: usize) -> Result<(), &'static str> {
    let mut ids = HashSet::new();
    if holds.len() < minimum
        || holds.len() > 128
        || holds.iter().any(|h| !ids.insert(&h.hold_id.0.0))
    {
        return Err("invalid hold set");
    }
    Ok(())
}
impl Blockers {
    fn validate(&self) -> Result<(), &'static str> {
        let mut ids = HashSet::new();
        if self.items.len() > 100
            || self
                .items
                .iter()
                .any(|b| !ids.insert(&b.hold_id.0.0) || matches!(b.subject, Subject::Blocker(_)))
        {
            return Err("invalid blocker summary");
        }
        Ok(())
    }
}
impl Receipt {
    fn validate(&self) -> Result<(), &'static str> {
        if self.request_digest.len() != 64
            || !self
                .request_digest
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err("invalid request digest");
        }
        match &self.decision {
            Decision::Accepted {
                cleared_holds,
                remaining,
                ..
            } => {
                holds_valid(cleared_holds, 0)?;
                remaining.validate()?;
            }
            Decision::ContinueRequired { holds, .. } => holds_valid(holds, 1)?,
            Decision::Blocked { blockers, .. } => blockers.validate()?,
            Decision::AlreadyActive { root, activation }
                if (!matches!(root.desired_state, DesiredState::Enabled)
                    || !matches!(
                        activation.phase,
                        Phase::Pending | Phase::Claimed | Phase::Running
                    )) =>
            {
                return Err("not active");
            }
            _ => {}
        }
        Ok(())
    }
}
impl CommandReply {
    pub fn validate(&self) -> Result<(), &'static str> {
        if let Self::Recorded { receipt, .. } = self {
            receipt.validate()?;
        }
        Ok(())
    }
}
impl Event {
    pub fn validate(&self) -> Result<(), &'static str> {
        match &self.event {
            EventBody::CommandDecided { command_id } => {
                if self.command_id.as_ref() != Some(command_id) {
                    return Err("command causation mismatch");
                }
            }
            EventBody::RootControlChanged { root, .. } => {
                if !matches!(&self.aggregate,Subject::Root(id) if id==&root.root_id)
                    || self.root_id.as_ref() != Some(&root.root_id)
                    || !matches!(self.aggregate_revision_kind, RevisionKind::Control)
                    || self.aggregate_revision != root.revision
                {
                    return Err("root event mismatch");
                }
            }
            EventBody::ActivationRequested { root_id, .. } => {
                if !matches!(&self.aggregate,Subject::Root(id) if id==root_id)
                    || self.root_id.as_ref() != Some(root_id)
                {
                    return Err("activation scope mismatch");
                }
            }
            EventBody::BlockerChanged { blocker, .. } => {
                if !matches!(&self.aggregate,Subject::Blocker(id) if id==&blocker.hold_id)
                    || !matches!(self.aggregate_revision_kind, RevisionKind::Blocker)
                    || self.aggregate_revision != blocker.revision
                {
                    return Err("blocker event mismatch");
                }
            }
            EventBody::OperationObserved { operation_id, .. } => {
                if !matches!(&self.aggregate,Subject::Operation(id) if id==operation_id) {
                    return Err("operation observation mismatch");
                }
            }
        }
        Ok(())
    }
}
impl QueryReply {
    pub fn validate(&self) -> Result<(), &'static str> {
        if let Self::Result { stamp, result, .. } = self {
            match result {
                QueryResult::Root { blockers, .. } => blockers.validate()?,
                QueryResult::Eligibility {
                    eligible, blockers, ..
                } => {
                    blockers.validate()?;
                    if *eligible != (blockers.items.is_empty() && !blockers.has_more) {
                        return Err("eligibility and blockers disagree");
                    }
                }
                QueryResult::Operation { state, retry, .. } => {
                    if matches!(state, OperationState::OutcomeUnknown)
                        && !matches!(retry, RetryAdvice::ReconcileOperation)
                    {
                        return Err("unknown outcome requires reconciliation");
                    }
                }
                QueryResult::Receipt { receipt } => {
                    if let Some(r) = receipt {
                        r.validate()?;
                    }
                }
                QueryResult::Events {
                    events,
                    next_cursor,
                    ..
                } => {
                    if events.len() > 500 || next_cursor.is_empty() || next_cursor.len() > 4096 {
                        return Err("invalid event page");
                    }
                    let watermark = stamp.sequence.get();
                    let mut prior = 0;
                    for event in events {
                        event.validate()?;
                        let seq = event.sequence.get();
                        if seq <= prior || seq > watermark {
                            return Err("event page order/watermark mismatch");
                        }
                        prior = seq;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Common receipt header checks reused by domain-specific reply candidates.
pub fn validate_domain_reply<D>(reply: &CommandReply<D>) -> Result<(), &'static str> {
    if let CommandReply::Recorded { receipt, .. } = reply
        && (receipt.request_digest.len() != 64
            || !receipt
                .request_digest
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
    {
        return Err("invalid request digest");
    }
    Ok(())
}

impl Id {
    /// Validates an opaque identifier (1–128 ASCII `[A-Za-z0-9_-]`).
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        Self::try_from(value.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Counter {
    /// Canonical decimal for a nonnegative signed-64 value.
    pub fn new(value: i64) -> Result<Self, &'static str> {
        if value < 0 {
            return Err("invalid counter");
        }
        Ok(Self(value.to_string()))
    }
    pub fn get(&self) -> i64 {
        // Construction guarantees a canonical decimal within 0..=i64::MAX.
        self.0.parse().unwrap_or(i64::MAX)
    }
    /// Checked increment: `None` at `i64::MAX` instead of wrapping.
    pub fn checked_next(&self) -> Option<Self> {
        self.get().checked_add(1).map(|v| Self(v.to_string()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Revision {
    /// Canonical decimal for a positive signed-64 revision.
    pub fn new(value: i64) -> Result<Self, &'static str> {
        if value <= 0 {
            return Err("revision must be positive");
        }
        Ok(Self(value.to_string()))
    }
    pub fn get(&self) -> i64 {
        // Construction guarantees a canonical decimal within 1..=i64::MAX.
        self.0.parse().unwrap_or(i64::MAX)
    }
    /// Checked increment: `None` at `i64::MAX` instead of wrapping.
    pub fn checked_next(&self) -> Option<Self> {
        self.get().checked_add(1).map(|v| Self(v.to_string()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
