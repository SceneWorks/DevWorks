//! Owned browser session records (DW-047) and the observation index of their evidence.
//!
//! The repository enforces record invariants only (identity, compare-and-set revisions, a
//! settled session is final, bounded text, monotonic observation sequences). Lifecycle, scope,
//! ownership and revision rules belong to `crate::services::browser`.
//!
//! Live CDP state (targets' frame and document revisions, accessibility snapshots and their
//! node maps, buffered console/network notifications) is ephemeral: it belongs to the running
//! browser process and is invalidated by its exit. Only the session lifecycle and the evidence
//! the session published (artifact references) are durable.
use super::StorageError;
use crate::protocol::{ActorId, AssignmentId, Id, OperationId, ProjectId, RootId};

/// Durable lifecycle of an owned browser session (`Starting → Ready → Stopping → Closed`, with
/// `Failed` and `Uncertain` recorded separately).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BrowserPhase {
    /// Recorded by Create; the browser is being launched and connected.
    Starting,
    /// The browser runs and accepts commands.
    Ready,
    /// Close was accepted; the browser is being stopped.
    Stopping,
    /// The browser tree was verified gone and its profile removed.
    Closed,
    /// The launch failed, or the browser exited or its transport was lost (a crash).
    Failed,
    /// The browser tree could not be verified gone; its profile is kept.
    Uncertain,
}
impl BrowserPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "Starting",
            Self::Ready => "Ready",
            Self::Stopping => "Stopping",
            Self::Closed => "Closed",
            Self::Failed => "Failed",
            Self::Uncertain => "Uncertain",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::Starting,
            Self::Ready,
            Self::Stopping,
            Self::Closed,
            Self::Failed,
            Self::Uncertain,
        ]
        .into_iter()
        .find(|p| p.as_str() == s)
    }
    /// Whether the session may still own a running browser.
    pub fn live(self) -> bool {
        matches!(self, Self::Starting | Self::Ready | Self::Stopping)
    }
    /// A settled session never changes again.
    pub fn settled(self) -> bool {
        matches!(self, Self::Closed | Self::Failed)
    }
}

/// One owned browser session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredBrowserSession {
    pub id: Id,
    /// Positive; advances on every lifecycle change and every change of the registered targets.
    pub revision: i64,
    pub phase: BrowserPhase,
    /// The boot that launched (or will launch) the browser.
    pub boot_id: Id,
    /// The principal that created the session (only it, a person or the host may drive it).
    pub owner: ActorId,
    /// `PrincipalKind` of the owner, as recorded (`Human`, `Agent`, ...).
    pub owner_kind: String,
    /// The agent/conversation session of the creating invocation.
    pub agent_session_id: Option<Id>,
    /// Owning project; `None` for a workspace session.
    pub project_id: Option<ProjectId>,
    pub root_id: Option<RootId>,
    pub assignment_id: Option<AssignmentId>,
    /// The browser asset the session runs (`chrome`, `chromium`, `edge`, `configured`).
    pub asset_id: String,
    /// The owned process tree, once launched.
    pub process_id: Option<Id>,
    /// The journaled Create operation.
    pub create_operation_id: OperationId,
    /// A mutating operation of this session whose outcome is unknown: new navigation, input and
    /// upload are refused until it is reconciled.
    pub uncertain_operation_id: Option<OperationId>,
    /// The latest admitted navigation, input or upload; the next one runs only after it
    /// settled. Maintained by [`BrowserRepository::set_last_action`] only.
    pub last_action_operation_id: Option<OperationId>,
    /// The latest accepted Close. Never cleared, and never on a `Ready` session: a session
    /// that is still starting when its Close is accepted keeps `Starting` with this request
    /// (it never settles while its launch may still produce a tree), never becomes `Ready`,
    /// and settles `Closed` once its tree is verified gone (or `Uncertain`, profile kept).
    pub close_operation_id: Option<OperationId>,
    /// Host target IDs registered under the session, in registration order.
    pub targets: Vec<String>,
    /// Last observation sequence allocated (0: none).
    pub observation_sequence: i64,
    /// Bounded, redacted reason of the latest failure or limitation.
    pub detail: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Which notification stream an observation belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BrowserChannel {
    /// Console messages, exceptions and page notices (dialogs, downloads, popups, crashes).
    Console,
    /// Network request/response summaries (no headers or bodies).
    Network,
    /// What an input or upload did and observed.
    Action,
}
impl BrowserChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Console => "Console",
            Self::Network => "Network",
            Self::Action => "Action",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [Self::Console, Self::Network, Self::Action]
            .into_iter()
            .find(|c| c.as_str() == s)
    }
}

/// One published observation: an immutable evidence artifact version of a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredBrowserObservation {
    pub session_id: Id,
    /// Positive, unique and increasing within the session (across channels).
    pub sequence: i64,
    pub channel: BrowserChannel,
    pub artifact_id: Id,
    pub version_id: Id,
    /// Notifications were dropped (bounded buffer) or the evidence was cut to its bound.
    pub truncated: bool,
    pub recorded_at_ms: i64,
}

/// Which sessions a listing returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrowserFilter {
    /// Sessions owned by this project (`None`: workspace sessions).
    Scope(Option<ProjectId>),
    /// Sessions that may still own a browser (`Starting`, `Ready`, `Stopping`, `Uncertain`).
    Unsettled,
}

pub trait BrowserRead {
    fn browser_session(&self, id: &Id) -> Result<Option<StoredBrowserSession>, StorageError>;
    /// Sessions matching `filter` in ID order after `after`, at most `limit` (1..=500).
    fn browser_sessions(
        &self,
        filter: &BrowserFilter,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredBrowserSession>, StorageError>;
    /// Observations of `session` on `channel` with a sequence above `after`, in sequence order,
    /// at most `limit` (1..=500).
    fn browser_observations(
        &self,
        session: &Id,
        channel: BrowserChannel,
        after: i64,
        limit: u16,
    ) -> Result<Vec<StoredBrowserObservation>, StorageError>;
}

pub trait BrowserRepository: BrowserRead {
    /// Inserts a new session at revision 1 (its create operation, owner and project must exist).
    fn insert_browser_session(
        &mut self,
        session: &StoredBrowserSession,
    ) -> Result<(), StorageError>;
    /// Compare-and-set on `expected` (the stored revision); `session.revision` must be
    /// `expected + 1`. Identity, owner, boot, scope, lineage, asset and create operation never
    /// change; a settled session is final; the observation sequence never decreases; a close
    /// request is never withdrawn, and a `Ready` session has none.
    fn update_browser_session(
        &mut self,
        session: &StoredBrowserSession,
        expected: i64,
    ) -> Result<(), StorageError>;
    /// Allocates the session's next observation sequence (without changing its revision: an
    /// observation is evidence about the session, not a change of it). Settled sessions still
    /// allocate, so the notifications they buffered before settling stay indexed.
    fn next_observation_sequence(&mut self, session: &Id) -> Result<i64, StorageError>;
    /// Records the session's latest admitted action (without changing its revision: admitting
    /// an action is not a change of the session).
    fn set_last_action(
        &mut self,
        session: &Id,
        operation: &OperationId,
    ) -> Result<(), StorageError>;
    /// Records one observation; its sequence must not exceed the session's allocated sequence
    /// and must be new for the session.
    fn insert_browser_observation(
        &mut self,
        observation: &StoredBrowserObservation,
    ) -> Result<(), StorageError>;
}
