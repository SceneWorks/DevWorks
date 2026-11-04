//! Inference router records (ROUTE-14, ROUTE-15, ROUTE-19): the conversational model catalog,
//! the persisted round-robin cursor of each tier, each session's route pin, router-reported
//! cooldowns and the binding of a logical inference request to its operation journal entry.
//!
//! * **Catalog models** are editable configuration, not a provider inventory: the initial rows
//!   (the default tier members and Open Router Auto) are seeded once by the migration and kept
//!   across restarts as edited. A SystemOne decision model is never stored here.
//! * **Tier cursors** count the round-robin turns a tier has handed out (`next`, 0 before the
//!   first). A turn is taken only when a new or replacement pin is allocated from the tier.
//! * **Pins** are one row per session: the route the session is pinned to, its intent, requested
//!   and effective effort, the capabilities its latest request required and why it was set.
//!   The revision advances with every change; the pin ID changes only when the pinned model or
//!   connection changes (an effort-only change keeps it).
//! * **Cooldowns** are per model, with a validated absolute expiry. Recording keeps the later
//!   of an existing and a new expiry; a cooldown ends early only when it is explicitly cleared.
//! * **Requests** bind a logical inference request ID to its operation and attribution, so its
//!   status is read from the one operation journal.
//!
//! The repository enforces record invariants (bounded identifiers, wire enumerations, compare-
//! and-set revisions and cursors, effort preservation). Eligibility, allocation order, failover
//! and authority belong to `crate::services::router`, which runs inside one unit of work.
use super::StorageError;
use crate::protocol::{Id, OperationId, ProjectId};

/// A catalog model. Enumerated members are stored by their wire names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredRouteModel {
    pub id: Id,
    pub display_name: String,
    /// `EndpointKind` wire name; never `SystemOne`.
    pub endpoint_profile: String,
    pub upstream_model_id: String,
    pub enabled: bool,
    /// Declared explicit efforts (`EffortName` wire names other than `Default`), lowest first.
    pub efforts: Vec<String>,
    pub image_input: bool,
    pub tools: bool,
    /// The declared context window in tokens; `None` when it is unknown.
    pub context_tokens: Option<i64>,
    /// Display order of the named-model group.
    pub position: i64,
    /// Positive; advances on every change.
    pub revision: i64,
    pub updated_at_ms: i64,
}

/// A session's route pin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredPin {
    pub session_id: Id,
    pub pin_id: Id,
    /// Positive; advances on every change of the row.
    pub revision: i64,
    pub model_id: Id,
    pub connection_id: Id,
    /// Canonical JSON of the wire `RouteIntent`.
    pub intent: String,
    /// `EffortName` wire names; an explicit request is never changed (`Default` may resolve).
    pub requested_effort: String,
    pub effective_effort: String,
    /// The capability labels the latest routed request required, sorted and unique.
    pub required_capabilities: Vec<String>,
    /// The pin this one replaced when the model or connection changed.
    pub replaced_pin_id: Option<Id>,
    /// Why the row was last written: one of [`PIN_REASONS`].
    pub reason: String,
    pub updated_at_ms: i64,
}

/// Why a pin was set.
pub const PIN_REASONS: [&str; 5] = [
    "Initial",
    "Failover",
    "SelectionChanged",
    "EffortChanged",
    "ConnectionChanged",
];

/// A router-reported cooldown of one model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredCooldown {
    pub model_id: Id,
    /// Validated absolute expiry (UTC ms).
    pub until_ms: i64,
    /// One of [`COOLDOWN_REASONS`].
    pub reason: String,
    pub observed_at_ms: i64,
}

/// Why a model is cooling.
pub const COOLDOWN_REASONS: [&str; 5] = [
    "RateLimited",
    "Overloaded",
    "Unreachable",
    "AuthRejected",
    "PlanRejected",
];

/// The binding of a logical inference request to its operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredRouteRequest {
    pub request_id: Id,
    pub operation_id: OperationId,
    /// One of the router's purpose names (`Conversation`, `Title`, `Compaction`, ...).
    pub purpose: String,
    pub session_id: Option<Id>,
    pub project_id: Option<ProjectId>,
    pub model_id: Id,
    pub connection_id: Id,
    /// The session pin the request was routed by (`None` for an auxiliary request).
    pub pin_id: Option<Id>,
    pub created_at_ms: i64,
}

pub trait RouterRead {
    fn route_model(&self, id: &Id) -> Result<Option<StoredRouteModel>, StorageError>;
    /// Models in display order (`position`, then ID), strictly after `after`, at most `limit`
    /// (1..=500).
    fn route_models(
        &self,
        after: Option<(i64, &str)>,
        limit: u16,
    ) -> Result<Vec<StoredRouteModel>, StorageError>;
    /// The turns `tier` (a `TierName` wire name) has handed out; 0 before the first.
    fn tier_cursor(&self, tier: &str) -> Result<i64, StorageError>;
    fn session_pin(&self, session: &Id) -> Result<Option<StoredPin>, StorageError>;
    /// Every recorded cooldown, expired or not, in model ID order (one per catalog model).
    fn cooldowns(&self) -> Result<Vec<StoredCooldown>, StorageError>;
    fn route_request(&self, id: &Id) -> Result<Option<StoredRouteRequest>, StorageError>;
}

pub trait RouterRepository: RouterRead {
    /// Inserts `model` at revision 1 (`expected_revision` `None`; an existing ID is
    /// [`StorageError::Duplicate`]) or replaces it at `expected_revision`, storing
    /// `expected_revision + 1`. A stale revision is [`StorageError::RevisionConflict`].
    fn put_route_model(
        &mut self,
        model: &StoredRouteModel,
        expected_revision: Option<i64>,
    ) -> Result<StoredRouteModel, StorageError>;
    /// Takes one round-robin turn of `tier` whose cursor is at `expected`, returning the new
    /// value; a cursor that moved is [`StorageError::RevisionConflict`].
    fn advance_tier_cursor(&mut self, tier: &str, expected: i64) -> Result<i64, StorageError>;
    /// Inserts a session's first pin (`expected_revision` `None`, stored at revision 1; an
    /// existing pin is [`StorageError::Duplicate`]) or replaces it at `expected_revision`,
    /// storing `expected_revision + 1`. A stale revision is [`StorageError::RevisionConflict`].
    fn put_session_pin(
        &mut self,
        pin: &StoredPin,
        expected_revision: Option<i64>,
    ) -> Result<StoredPin, StorageError>;
    /// Records a cooldown, keeping the later expiry when the model is already cooling; returns
    /// the stored cooldown.
    fn record_cooldown(
        &mut self,
        cooldown: &StoredCooldown,
    ) -> Result<StoredCooldown, StorageError>;
    /// Ends a model's cooldown (meaningful success or a configuration change); `false` when it
    /// had none.
    fn clear_cooldown(&mut self, model: &Id) -> Result<bool, StorageError>;
    /// Binds a logical request; a duplicate request or operation is
    /// [`StorageError::Duplicate`].
    fn record_route_request(&mut self, request: &StoredRouteRequest) -> Result<(), StorageError>;
}
