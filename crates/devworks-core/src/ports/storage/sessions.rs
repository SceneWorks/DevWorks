//! Session, transcript and lineage records: sessions with their per-session append position,
//! immutable transcript entries, batch receipts, durable tool calls and their single initial
//! response, context epochs with their recorded context sources, checkpoints, attributed usage
//! observations and the typed relationship graph (`history_links`) that history navigates.
//!
//! The repository enforces record invariants (identity, uniqueness, foreign keys, bounded
//! listings). Every append rule — batch replay, sequence CAS, explicit epoch boundaries, one
//! response per durable call, reference resolution and attribution — belongs to
//! `crate::services::sessions::transcript`, which runs inside one unit of work. Rows are never
//! rewritten except the session row's lifecycle/append bookkeeping and a call's response slot,
//! which is written once.
use super::{EventRecord, StorageError};
use crate::protocol::{ActorId, Id, OperationId, ProjectId, RootId};
use serde_json::Value;

/// One work item as history and briefings need it (owned by the work item; read-only here).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkRoot {
    pub id: RootId,
    pub project_id: ProjectId,
    /// `feature`, `issue` or `task`.
    pub kind: String,
    pub title: String,
    pub revision: i64,
    /// Advances whenever who can see the work changes; history cursors bind it.
    pub scope_revision: i64,
    pub parent_feature_id: Option<RootId>,
    /// When the work item was created (UTC ms). Work items record no creator.
    pub created_at_ms: i64,
}

/// A session: identity, lineage, lifecycle and its append position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSession {
    pub id: Id,
    /// The durable conversation that owns the transcript (created with the session).
    pub conversation_id: Id,
    /// The logical agent or principal conducting the session.
    pub agent_id: ActorId,
    /// `None` for a workspace (Home) session.
    pub project_id: Option<ProjectId>,
    pub root_id: Option<RootId>,
    pub title: String,
    pub context_epoch: i64,
    /// Positive; advances on start, user messages, context epoch changes and end.
    pub revision: i64,
    pub created_by: ActorId,
    pub created_at_ms: i64,
    pub closed_at_ms: Option<i64>,
    pub closed_by: Option<ActorId>,
    /// The next per-session transcript sequence (1 for an empty transcript).
    pub next_sequence: i64,
    /// Sequence of the last durable checkpoint (0 before the first).
    pub checkpoint_sequence: i64,
}
impl StoredSession {
    pub fn closed(&self) -> bool {
        self.closed_at_ms.is_some()
    }
    /// The last durable transcript position (0 for an empty transcript).
    pub fn last_durable_sequence(&self) -> i64 {
        self.next_sequence - 1
    }
}

/// One immutable transcript entry.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredEntry {
    pub session_id: Id,
    pub sequence: i64,
    pub entry_id: Id,
    pub batch_id: Id,
    pub context_epoch: i64,
    /// The principal the batch was attributed to.
    pub actor: ActorId,
    /// Variant name of the body (`UserMessage`, `ToolCallRecorded`, ...).
    pub kind: String,
    pub created_at_ms: i64,
    /// The validated `TranscriptBody` JSON, exactly as appended.
    pub body: Value,
}

/// The durable receipt of one appended batch. Equal replays return it unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredBatch {
    pub batch_id: Id,
    pub session_id: Id,
    /// SHA-256 of the canonical batch envelope (entries, checkpoint, sources, usage, actor).
    pub payload_digest: String,
    pub actor: ActorId,
    pub context_epoch: i64,
    pub first_sequence: i64,
    pub last_sequence: i64,
    /// The history event recording the append and its committed (global) sequence.
    pub event_id: Id,
    pub committed_sequence: i64,
    pub accepted_at_ms: i64,
}

/// A durable, complete tool call and its single initial response slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredToolCall {
    pub session_id: Id,
    pub attempt_id: Id,
    pub native_call_id: String,
    pub ordinal: i64,
    pub capability_id: String,
    pub call_sequence: i64,
    pub response_sequence: Option<i64>,
    /// Outcome variant of the initial response (`Completed`, `ApprovalRequired`, ...).
    pub outcome: Option<String>,
    /// The operation an `ApprovalRequired`/`OutcomeUnknown` response is linked to.
    pub operation_id: Option<OperationId>,
}

/// One context epoch boundary. The original transcript before it is never replaced.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredEpoch {
    pub session_id: Id,
    pub epoch: i64,
    pub previous_epoch: i64,
    /// `Compaction`, `ProviderChange` or `InstructionAdoption`.
    pub reason: String,
    /// The immutable context snapshot (`ArtifactRef` JSON).
    pub context: Value,
    /// Identity of the epoch's context: SHA-256 over the session, epoch and the immutable
    /// snapshot's digest and length. It identifies what the epoch's requests start from; the
    /// provider cache key (instruction/tool-schema revisions, binding) is the router's.
    pub identity: String,
    /// Transcript sequence of the boundary entry.
    pub sequence: i64,
    pub created_at_ms: i64,
}

/// The latest immutable provider context published for a session. The transcript remains the
/// authority for conversation facts; this pointer makes exact native aliases and opaque items
/// recoverable after a host restart without reconstructing them from a changed tool registry.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredProviderContextHead {
    pub session_id: Id,
    pub context_epoch: i64,
    pub observed_sequence: i64,
    /// `ProviderTurnContinuation` or `ProviderOperationUpdateContext`.
    pub kind: String,
    pub attempt_id: Option<Id>,
    pub binding: Value,
    pub artifact: Value,
    pub revision: i64,
}

/// A context source the host recorded for one epoch (instruction revisions, retrieved sources,
/// omissions/truncation).
#[derive(Clone, Debug, PartialEq)]
pub struct StoredContextSource {
    pub session_id: Id,
    pub epoch: i64,
    /// 1, 2, ... within the epoch, in recording order.
    pub ordinal: i64,
    pub source_id: Id,
    pub kind: String,
    pub source_revision: i64,
    pub artifact: Option<Value>,
    pub token_estimate: i64,
    /// `Included`, `Truncated` or `Omitted`.
    pub disposition: String,
    /// Transcript position at which it was recorded.
    pub sequence: i64,
}

/// A durable checkpoint at a transcript position.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredCheckpoint {
    pub session_id: Id,
    pub sequence: i64,
    pub batch_id: Id,
    pub artifact: Value,
    pub created_at_ms: i64,
}

/// One attributed, host-classified usage observation (`UsageItem` JSON).
#[derive(Clone, Debug, PartialEq)]
pub struct StoredUsage {
    pub session_id: Id,
    pub batch_id: Id,
    /// 1, 2, ... within the batch.
    pub ordinal: i64,
    pub project_id: Option<ProjectId>,
    pub root_id: Option<RootId>,
    pub committed_sequence: i64,
    pub item: Value,
}

/// A typed edge of the history relationship graph, anchored at a work root. Corrections and
/// reversals are new links that name the link they supersede; the prior one stays.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredLink {
    pub id: Id,
    pub project_id: ProjectId,
    pub root_id: RootId,
    /// `session`, `tool_call`, `tool_response`, `context_epoch`, `checkpoint`,
    /// `operation_update`, `attachment`, `git`, `pull_request`, `review`, `decision`,
    /// `outcome`, `evidence` or `correction`.
    pub kind: String,
    pub target_kind: String,
    pub target_id: String,
    pub parent_id: Option<Id>,
    pub supersedes_id: Option<Id>,
    pub actor: ActorId,
    pub event_id: Id,
    /// Committed sequence of the fact that created the link (paging key with `id`).
    pub sequence: i64,
    pub repository_id: Option<Id>,
    pub revision: Option<String>,
    pub summary: String,
    pub body: Value,
    pub created_at_ms: i64,
}

/// Which sessions a listing covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionScope {
    /// Every session (workspace visibility).
    All,
    /// Sessions of one project.
    Project(ProjectId),
    /// Workspace (Home) sessions without a project.
    Workspace,
}

/// Keyset position of the session listing (newest first, then ID descending).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPosition {
    pub created_at_ms: i64,
    pub id: Id,
}

/// Keyset position in committed-sequence order: `(sequence, source, id)`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SequenceKey {
    pub sequence: i64,
    /// 0 = work hierarchy, 1 = history link, 2 = event, 3 = usage.
    pub source: u8,
    pub id: String,
}

/// Which committed history a feed covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryScope {
    /// The facts attached to one work root.
    Root(RootId),
    /// Every fact of one project.
    Project(ProjectId),
    /// The facts of the workspace itself (no project): Home sessions and workspace commands.
    Workspace,
}

/// One completed tool invocation: a durable call and its single initial response, recorded with
/// the response for every session (with or without a project or work root).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredInvocation {
    pub session_id: Id,
    /// Transcript sequence of the response entry (unique per session).
    pub response_sequence: i64,
    pub call_sequence: i64,
    /// Committed sequence of the batch that recorded the response.
    pub committed_sequence: i64,
    pub project_id: Option<ProjectId>,
    pub root_id: Option<RootId>,
    /// Transcript entry ID of the call.
    pub call_entry_id: Id,
    pub capability_id: String,
    /// Outcome variant of the response (`Completed`, `Failed`, ...).
    pub outcome: String,
    /// The `Failed` response's code, as recorded (redacted when served).
    pub error_code: Option<String>,
    pub operation_id: Option<OperationId>,
    /// The principal the response batch was attributed to.
    pub actor: ActorId,
    pub called_at_ms: i64,
    pub responded_at_ms: i64,
}

/// One usage fact of a scope in committed-sequence order: an attributed inference observation
/// (key source 3, ID the zero-padded ordinal) or a completed tool invocation (key source 1, ID the
/// zero-padded response sequence).
#[derive(Clone, Debug, PartialEq)]
pub enum UsageFact {
    Inference {
        key: SequenceKey,
        usage: StoredUsage,
    },
    Tool {
        key: SequenceKey,
        invocation: StoredInvocation,
    },
}
impl UsageFact {
    pub fn key(&self) -> &SequenceKey {
        match self {
            Self::Inference { key, .. } | Self::Tool { key, .. } => key,
        }
    }
}

/// One row of a work root's lineage, in committed-sequence order.
#[derive(Clone, Debug, PartialEq)]
pub enum LineageRow {
    /// A parent feature or child task/issue of the root, at the fixed key `(0, 0, id)` that never
    /// changes once visible (a hierarchy change expires lineage cursors instead; see
    /// [`SessionRead::work_membership`]).
    Work {
        key: SequenceKey,
        work: WorkRoot,
    },
    Link {
        key: SequenceKey,
        link: StoredLink,
    },
    /// A history fact attached to the root (commands, artifacts).
    Event {
        key: SequenceKey,
        event: EventRecord,
    },
}
impl LineageRow {
    pub fn key(&self) -> &SequenceKey {
        match self {
            Self::Work { key, .. } | Self::Link { key, .. } | Self::Event { key, .. } => key,
        }
    }
}

pub trait SessionRead {
    fn work_root(&self, root: &RootId) -> Result<Option<WorkRoot>, StorageError>;
    fn session(&self, id: &Id) -> Result<Option<StoredSession>, StorageError>;
    /// Immutable repository IDs captured when the session was created, in ID order.
    fn session_repository_scope(&self, id: &Id) -> Result<Vec<Id>, StorageError>;
    /// Last authenticated session activity, including an empty inbox poll.
    fn session_activity_at_ms(&self, id: &Id) -> Result<Option<i64>, StorageError>;
    /// Sessions in `scope` (optionally only open or closed ones), newest first after `after`, at
    /// most `limit` (1..=500).
    fn sessions(
        &self,
        scope: &SessionScope,
        closed: Option<bool>,
        after: Option<&SessionPosition>,
        limit: u16,
    ) -> Result<Vec<StoredSession>, StorageError>;
    /// Entries of `session` with sequence greater than `after`, at most `limit` (1..=500).
    fn entries(
        &self,
        session: &Id,
        after: i64,
        limit: u16,
    ) -> Result<Vec<StoredEntry>, StorageError>;
    fn entry_exists(&self, session: &Id, entry_id: &Id) -> Result<bool, StorageError>;
    /// Commit time (store clock, UTC ms) of the session's latest transcript entry; `None` for
    /// an empty transcript.
    fn last_entry_at_ms(&self, session: &Id) -> Result<Option<i64>, StorageError>;
    /// The receipt of `batch_id` in `session` (batch identities are scoped per session).
    fn batch(&self, session: &Id, batch_id: &Id) -> Result<Option<StoredBatch>, StorageError>;
    fn project_exists(&self, project: &ProjectId) -> Result<bool, StorageError>;
    /// The project's scope revision (it advances whenever who can see the project changes;
    /// project feed cursors bind it), or `None` for an unknown project.
    fn project_scope_revision(&self, project: &ProjectId) -> Result<Option<i64>, StorageError>;
    /// Digest of the root's work hierarchy (its parent feature and child work items); it
    /// changes whenever work is added to, moved into or out of the root's hierarchy.
    fn work_membership(&self, root: &RootId) -> Result<String, StorageError>;
    /// Whether `repository` is a member of `project`.
    fn project_has_repository(
        &self,
        project: &ProjectId,
        repository: &Id,
    ) -> Result<bool, StorageError>;
    fn tool_call(
        &self,
        session: &Id,
        attempt_id: &Id,
        native_call_id: &str,
    ) -> Result<Option<StoredToolCall>, StorageError>;
    /// Whether `attempt_id` already recorded a call with `ordinal` in `session`.
    fn ordinal_taken(
        &self,
        session: &Id,
        attempt_id: &Id,
        ordinal: i64,
    ) -> Result<bool, StorageError>;
    /// Calls of `session` without an initial response, in call order, at most `limit`.
    fn pending_calls(&self, session: &Id, limit: u16) -> Result<Vec<StoredToolCall>, StorageError>;
    /// Responses of `session` linked to an operation (waits and unknown outcomes), at most
    /// `limit`, in response order.
    fn linked_operations(
        &self,
        session: &Id,
        limit: u16,
    ) -> Result<Vec<StoredToolCall>, StorageError>;
    fn epoch(&self, session: &Id, epoch: i64) -> Result<Option<StoredEpoch>, StorageError>;
    fn provider_context_head(
        &self,
        session: &Id,
    ) -> Result<Option<StoredProviderContextHead>, StorageError>;
    /// Recorded sources of one epoch with ordinal greater than `after`, at most `limit`.
    fn context_sources(
        &self,
        session: &Id,
        epoch: i64,
        after: i64,
        limit: u16,
    ) -> Result<Vec<StoredContextSource>, StorageError>;
    fn latest_checkpoint(&self, session: &Id) -> Result<Option<StoredCheckpoint>, StorageError>;
    /// Usage observations attributed to `root` after `after`, at most `limit`.
    fn usage(
        &self,
        root: &RootId,
        after: Option<&SequenceKey>,
        limit: u16,
    ) -> Result<Vec<(SequenceKey, StoredUsage)>, StorageError>;
    /// The usage facts of `scope` — its usage observations and completed tool invocations — in
    /// committed-sequence order `(sequence, source, id)` after `after`, at most `limit`.
    /// Host accounting bootstrap across all project scopes, preserving historical attribution.
    fn all_usage_facts(
        &self,
        after: Option<&SequenceKey>,
        limit: u16,
    ) -> Result<Vec<UsageFact>, StorageError>;
    fn usage_facts(
        &self,
        scope: &HistoryScope,
        after: Option<&SequenceKey>,
        limit: u16,
    ) -> Result<Vec<UsageFact>, StorageError>;
    fn link(&self, id: &Id) -> Result<Option<StoredLink>, StorageError>;
    /// Links of `root` whose kind is in `kinds` after `after`, at most `limit`.
    fn links(
        &self,
        root: &RootId,
        kinds: &[&str],
        after: Option<&SequenceKey>,
        limit: u16,
    ) -> Result<Vec<StoredLink>, StorageError>;
    /// Links pointing at a target (navigation back from a session, call or commit to work).
    fn links_to(
        &self,
        target_kind: &str,
        target_id: &str,
        limit: u16,
    ) -> Result<Vec<StoredLink>, StorageError>;
    /// The root's lineage: its parent feature and child work (see [`LineageRow::Work`]), its links,
    /// and its command and artifact history facts, in committed-sequence order after `after`.
    fn lineage(
        &self,
        root: &RootId,
        after: Option<&SequenceKey>,
        limit: u16,
    ) -> Result<Vec<LineageRow>, StorageError>;
    /// History events attached to `root` whose kind is in `kinds` and that name their subject
    /// (`payload.subject`), sequence greater than `after`, at most `limit`.
    fn root_events(
        &self,
        root: &RootId,
        kinds: &[&str],
        after: i64,
        limit: u16,
    ) -> Result<Vec<EventRecord>, StorageError>;
    /// The `CommandDecided` events of `scope`, whether or not they name a subject, sequence
    /// greater than `after`, at most `limit`.
    fn decisions(
        &self,
        scope: &HistoryScope,
        after: i64,
        limit: u16,
    ) -> Result<Vec<EventRecord>, StorageError>;
    /// Newest-first decided commands, strictly before the optional sequence boundary.
    /// None starts at the newest committed decision; scope filtering precedes the limit.
    fn decisions_before(
        &self,
        scope: &HistoryScope,
        before: Option<i64>,
        limit: u16,
    ) -> Result<Vec<EventRecord>, StorageError>;
}

pub trait SessionRepository: SessionRead {
    /// Advances the durable activity clock without changing transcript or authority revisions.
    fn touch_session_activity(&mut self, id: &Id, at_ms: i64) -> Result<(), StorageError>;
    /// Inserts the session and its conversation. The project, root and actors must exist.
    fn insert_session(&mut self, session: &StoredSession) -> Result<(), StorageError>;
    /// Replaces the mutable columns (title, epoch, revision, close, append bookkeeping). The
    /// identity, lineage, creator and creation time never change.
    fn update_session(&mut self, session: &StoredSession) -> Result<(), StorageError>;
    /// Appends an entry; its `(session, sequence)` and `(session, entry_id)` are unique.
    fn insert_entry(&mut self, entry: &StoredEntry) -> Result<(), StorageError>;
    /// Records a batch receipt; its batch ID is unique within its session.
    fn insert_batch(&mut self, batch: &StoredBatch) -> Result<(), StorageError>;
    /// Records a durable call; `(session, attempt, native call)` and `(session, attempt,
    /// ordinal)` are unique.
    fn insert_tool_call(&mut self, call: &StoredToolCall) -> Result<(), StorageError>;
    /// Writes the call's single initial response; a second response is
    /// [`StorageError::Duplicate`].
    fn record_response(&mut self, call: &StoredToolCall) -> Result<(), StorageError>;
    fn insert_epoch(&mut self, epoch: &StoredEpoch) -> Result<(), StorageError>;
    /// Replace the session's provider context pointer after the new artifact is committed in
    /// this transaction. `expected_revision` is None only for its first publication.
    fn put_provider_context_head(
        &mut self,
        head: &StoredProviderContextHead,
        expected_revision: Option<i64>,
    ) -> Result<(), StorageError>;
    fn insert_context_source(&mut self, source: &StoredContextSource) -> Result<(), StorageError>;
    fn insert_checkpoint(&mut self, checkpoint: &StoredCheckpoint) -> Result<(), StorageError>;
    fn insert_usage(&mut self, usage: &StoredUsage) -> Result<(), StorageError>;
    /// Records a completed invocation; `(session, response sequence)` is unique.
    fn insert_invocation(&mut self, invocation: &StoredInvocation) -> Result<(), StorageError>;
    /// Adds a link; its ID is unique and a superseded or parent link must exist.
    fn insert_link(&mut self, link: &StoredLink) -> Result<(), StorageError>;
}
