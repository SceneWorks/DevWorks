//! Versioned skills and role instructions (DW-034). An instruction item names one skill or role
//! in one scope; its revisions are immutable and their contents are managed artifacts. Only an
//! approved revision (human-saved, observed from a user-owned file, or approved by a human
//! decision) can become active, and the active revision is what agents adopt at their next safe
//! step. Drafts and pending proposals never change the active revision.
use super::{ConfigScope, StorageError};
use crate::protocol::{ActorId, Id, ProjectId};
use serde_json::Value;

/// What an instruction item is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InstructionKind {
    Skill,
    Role,
}
impl InstructionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skill => "Skill",
            Self::Role => "Role",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Skill" => Some(Self::Skill),
            "Role" => Some(Self::Role),
            _ => None,
        }
    }
}

/// Where an instruction item's content is owned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InstructionOrigin {
    /// A user-owned shared skill read in place (`~/.agents/skills`). Never written by the app.
    Shared,
    /// A harness-specific skill under the app's managed skill root (local skill or override).
    Harness,
    /// A default role instruction; settings hold its effective text.
    Role,
}
impl InstructionOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "Shared",
            Self::Harness => "Harness",
            Self::Role => "Role",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Shared" => Some(Self::Shared),
            "Harness" => Some(Self::Harness),
            "Role" => Some(Self::Role),
            _ => None,
        }
    }
}

/// Lifecycle of one immutable revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InstructionRevisionState {
    /// Agent-authored content not yet proposed; never effective.
    Draft,
    /// Bound to a pending human decision; never effective.
    Proposed,
    /// Approved content (human-saved, observed from a user-owned file or approved). Eligible to
    /// be the active revision.
    Approved,
    /// A human rejected the proposal; never effective.
    Rejected,
}
impl InstructionRevisionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "Draft",
            Self::Proposed => "Proposed",
            Self::Approved => "Approved",
            Self::Rejected => "Rejected",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Draft" => Some(Self::Draft),
            "Proposed" => Some(Self::Proposed),
            "Approved" => Some(Self::Approved),
            "Rejected" => Some(Self::Rejected),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredInstructionItem {
    pub id: Id,
    pub kind: InstructionKind,
    /// Skill name (its directory name) or role key (`worker`, `codeReviewer`, ...).
    pub name: String,
    pub scope: ConfigScope,
    pub origin: InstructionOrigin,
    /// Canonical skill root the directory was discovered in (file-backed skills only).
    pub root_path: Option<String>,
    /// Canonical skill directory (file-backed skills only).
    pub source_path: Option<String>,
    /// The latest revision number (every revision, any state). Commands bind to it.
    pub latest_revision: i64,
    /// The effective revision, always an `Approved` one; `None` until one is approved.
    pub active_revision: Option<i64>,
    pub enabled: bool,
    /// Digest of the files last written to the harness directory (harness skills only).
    pub materialized_digest: Option<String>,
    /// Digest of the content last observed in the source directory (file-backed skills only).
    pub observed_digest: Option<String>,
    /// Whether discovery still finds the source directory.
    pub present: bool,
    /// Compare-and-set version of the item row; every change increments it.
    pub version: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// The one project that may see a workspace item created by a project-confined agent's
    /// draft, until a revision of it becomes active (`None` for every other item).
    pub confined_project: Option<ProjectId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredInstructionRevision {
    pub instruction_id: Id,
    pub revision: i64,
    /// The `ArtifactRef` of the instruction text (SKILL.md or the role text).
    pub content: Value,
    /// Lowercase SHA-256 of the whole revision: instruction bytes plus every resource.
    pub content_digest: String,
    pub state: InstructionRevisionState,
    pub author: ActorId,
    /// Provenance: `{kind: Observed|HumanSaved|AgentDraft|Rollback|Default, sessionId,
    /// assignmentId, rootId, rationale, proposalId, approvalId, baseRevision, validation}`.
    pub origin: Value,
    /// `[{path, artifact: ArtifactRef, sha256, byteCount, executable}]`, sorted by path.
    pub resources: Value,
    pub applicability: Vec<String>,
    /// Pending human decision bound to this revision (`Proposed` only).
    pub approval_id: Option<Id>,
    pub created_at_ms: i64,
    pub decided_at_ms: Option<i64>,
    /// The one project that may see an unapproved revision a project-confined agent drafted
    /// (its content artifact is scoped to that project); cleared when it is approved.
    pub confined_project: Option<ProjectId>,
}

/// A filter over instruction items.
#[derive(Clone, Debug, Default)]
pub struct InstructionFilter {
    pub kind: Option<InstructionKind>,
    /// Exactly these scopes (empty means every scope).
    pub scopes: Vec<ConfigScope>,
    /// Only items with this name.
    pub name: Option<String>,
}

pub trait InstructionRead {
    fn instruction_item(&self, id: &Id) -> Result<Option<StoredInstructionItem>, StorageError>;
    /// Items in `(kind, scope key, name, origin, id)` order after `after`.
    fn instruction_items(
        &self,
        filter: &InstructionFilter,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredInstructionItem>, StorageError>;
    fn instruction_revision(
        &self,
        id: &Id,
        revision: i64,
    ) -> Result<Option<StoredInstructionRevision>, StorageError>;
    /// Revisions newest first, strictly below `before` when given.
    fn instruction_revisions(
        &self,
        id: &Id,
        before: Option<i64>,
        limit: u16,
    ) -> Result<Vec<StoredInstructionRevision>, StorageError>;
    /// The revision bound to a human decision.
    fn instruction_revision_by_approval(
        &self,
        approval_id: &Id,
    ) -> Result<Option<StoredInstructionRevision>, StorageError>;
}

pub trait InstructionRepository: InstructionRead {
    /// Inserts a new item. Its `(kind, scope, name, origin)` must be unique.
    fn insert_instruction_item(&mut self, item: &StoredInstructionItem)
    -> Result<(), StorageError>;
    /// Replaces an item whose stored version is `item.version - 1` (compare-and-set); a
    /// mismatch is a revision conflict.
    fn update_instruction_item(&mut self, item: &StoredInstructionItem)
    -> Result<(), StorageError>;
    /// Appends revision `item.latest_revision + 1`; the caller updates the item in the same
    /// unit of work.
    fn insert_instruction_revision(
        &mut self,
        revision: &StoredInstructionRevision,
    ) -> Result<(), StorageError>;
    /// Changes only the state, decision time and approval binding of a revision whose current
    /// state is `from`.
    fn set_instruction_revision_state(
        &mut self,
        id: &Id,
        revision: i64,
        from: InstructionRevisionState,
        to: InstructionRevisionState,
        approval_id: Option<&Id>,
        decided_at_ms: Option<i64>,
    ) -> Result<(), StorageError>;
    /// Releases a confined revision to its item's scope: replaces its content with `content`,
    /// a copy of the same bytes (same `sha256`) published at that scope, and clears the
    /// confinement. Refused unless the revision is confined and the bytes are the same.
    fn release_instruction_revision(
        &mut self,
        id: &Id,
        revision: i64,
        content: &Value,
    ) -> Result<(), StorageError>;
}
