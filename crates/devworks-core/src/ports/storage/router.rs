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
    /// The one connection that serves the model (migration 15); `None` lets any connection of
    /// its endpoint profile serve it.
    pub connection_id: Option<Id>,
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
    /// Concrete identity captured when routed. A catalog edit cannot silently change it.
    /// Nullable only for a legacy pin whose catalog row was absent during migration 25.
    pub upstream_model_id: Option<String>,
    pub endpoint_profile: Option<String>,
    /// `Resolved`, `Migration25CurrentCatalog`, or `Unknown`; migration never invents the
    /// historical upstream of a pin that did not record one.
    pub identity_origin: String,
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

// ---------------------------------------------------------------------------------------------
// Provider connections and decision models (migration 15, DW-18)

/// The stored authentication lifecycle of a connection. `Authorizing` and `Refreshing` exist
/// only while a sign-in or refresh is running in this process, and `Disabled` is derived (an
/// unaccepted or revoked subscription risk notice), so none of them is ever stored.
pub const STORED_LIFECYCLES: [&str; 4] = [
    "Disconnected",
    "Connected",
    "ReauthenticationRequired",
    "Unavailable",
];

/// One model a connection's last successful discovery listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub id: String,
    /// The context window the provider reported, when it reports one.
    pub context_tokens: Option<i64>,
}

/// A human's acceptance of the subscription risk notice, bound to what it was accepted for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredRiskAcceptance {
    /// SHA-256 of the exact notice text accepted.
    pub notice_sha256: String,
    /// Digest of the profile, endpoint and account the acceptance covers.
    pub binding: String,
    /// The store restore epoch it was accepted in (a restore revokes it).
    pub restore_epoch: i64,
    pub accepted_by: String,
    pub accepted_at_ms: i64,
}

/// A provider connection. Never holds a secret: `credential_ref` names a `SecretStore` handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredConnection {
    pub id: Id,
    pub name: String,
    /// `EndpointKind` wire name.
    pub profile: String,
    /// The inference base URL.
    pub endpoint: String,
    /// Turned on by the user; a disabled connection keeps its credential and is never routed.
    pub enabled: bool,
    /// Explicit human-authored billing policy; never inferred from endpoint hostname.
    pub local_free: bool,
    /// Canonical JSON of the wire `OpenRouterConstraints` (`OpenRouterChat` connections only).
    pub open_router: Option<String>,
    pub credential_ref: Option<Id>,
    /// Advances with every credential publish, rotation or removal; 0 before the first.
    pub credential_revision: i64,
    /// The restore epoch the credential was published in (a restored reference needs a new
    /// sign-in: the secret is not part of a backup).
    pub credential_epoch: Option<i64>,
    /// One of [`STORED_LIFECYCLES`].
    pub lifecycle: String,
    /// A safe, secret-free reason for the lifecycle or the last failure.
    pub state_reason: Option<String>,
    /// Positive native account identity generation, independent of credential refreshes.
    /// The owning service preserves it or advances it by one with the connection CAS.
    pub account_revision: i64,
    /// Digest of the provider account the credential acts for.
    pub account_digest: Option<String>,
    pub access_expires_at_ms: Option<i64>,
    pub risk: Option<StoredRiskAcceptance>,
    /// The last successful discovery (`None` before the first).
    pub discovered: Option<Vec<DiscoveredModel>>,
    pub discovered_at_ms: Option<i64>,
    /// The latest test (or discovery) outcome; `None` before the first.
    pub healthy: Option<bool>,
    pub checked_at_ms: Option<i64>,
    /// Positive; advances on every change.
    pub revision: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// A SystemOne decision model (ROUTE-18): its own model ID, never a conversational catalog
/// model or tier member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredDecisionModel {
    pub id: Id,
    pub display_name: String,
    pub connection_id: Id,
    pub upstream_model_id: String,
    /// `DecisionQuestionType` wire names.
    pub question_types: Vec<String>,
    pub accepts_images: bool,
    pub accepts_videos: bool,
    pub enabled: bool,
    /// Canonical JSON array of the extension question `SchemaRef`s.
    pub extension_schemas: String,
    /// Canonical JSON of the result `SchemaRef`.
    pub result_schema: String,
    pub revision: i64,
    pub updated_at_ms: i64,
}

pub trait RouterRead {
    fn route_model(&self, id: &Id) -> Result<Option<StoredRouteModel>, StorageError>;
    fn connection(&self, id: &Id) -> Result<Option<StoredConnection>, StorageError>;
    /// Last native account identity generation, retained even after a connection is removed.
    fn connection_account_revision(&self, id: &Id) -> Result<Option<i64>, StorageError>;
    /// Connections in ID order strictly after `after`, at most `limit` (1..=500).
    fn connections(
        &self,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<StoredConnection>, StorageError>;
    fn decision_model(&self, id: &Id) -> Result<Option<StoredDecisionModel>, StorageError>;
    /// Decision models in ID order strictly after `after`, at most `limit` (1..=500).
    fn decision_models(
        &self,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<StoredDecisionModel>, StorageError>;
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
    /// Host bootstrap logical request records in exclusive request-ID order.
    fn route_requests(
        &self,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredRouteRequest>, StorageError>;
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
    /// Inserts a connection at revision 1 (`expected_revision` `None`; an existing ID is
    /// [`StorageError::Duplicate`]) or replaces it at `expected_revision`, storing
    /// `expected_revision + 1`. A stale revision is [`StorageError::RevisionConflict`]. Two
    /// connections never share a credential handle. Account identity stays unchanged or advances
    /// by one; insertion uses the retained account generation plus one (one for a new ID).
    fn put_connection(
        &mut self,
        connection: &StoredConnection,
        expected_revision: Option<i64>,
    ) -> Result<StoredConnection, StorageError>;
    /// Removes a connection at `expected_revision` (a stale revision is
    /// [`StorageError::RevisionConflict`]). A catalog or decision model still bound to it is
    /// [`StorageError::InvalidRecord`]: it must be unbound first, never silently orphaned.
    /// The account identity generation is retained after removal.
    fn delete_connection(&mut self, id: &Id, expected_revision: i64) -> Result<(), StorageError>;
    /// Inserts or replaces (compare-and-set, as [`Self::put_connection`]) a decision model. Its ID
    /// is never a conversational catalog model's, and its connection is a `SystemOne` one.
    fn put_decision_model(
        &mut self,
        model: &StoredDecisionModel,
        expected_revision: Option<i64>,
    ) -> Result<StoredDecisionModel, StorageError>;
}
