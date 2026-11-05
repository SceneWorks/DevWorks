//! Artifact store records: artifact identity and scope, immutable versions over content-addressed
//! blobs, origin/reference links, project shares and backup pins.
//!
//! The repository only enforces record invariants (identity, foreign keys, bounded listings).
//! Every lifecycle rule — who may publish, reference, share or delete, expected revisions,
//! tombstone-before-unlink and the surviving-reference check — belongs to
//! `crate::services::artifacts`, which runs inside the gateway's unit of work.
//!
//! Bytes never live here: a [`BlobRecord`] names a managed file the artifact file port
//! (`crate::ports::files::ArtifactFiles`) published beneath the data root. A blob row is written
//! only after its file was flushed and atomically published, so committed metadata never
//! advertises a file that was not durably in place; a file without a row is invisible and
//! collectible.
use super::StorageError;
use crate::protocol::shared::Subject;
use crate::protocol::{ActorId, Id, OperationId, ProjectId, RootId};
use serde_json::Value;

/// Journal capability of a staged artifact file publication.
pub const STAGE_CAPABILITY: &str = "artifacts.stage_file";

/// Lifecycle of one managed blob file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlobState {
    /// Published and readable through every live version that names it.
    Present,
    /// No live version names it any more; physical removal waits for readers and backup pins.
    UnlinkPending,
    /// The collector claimed it and is removing the file (crash recovery finishes the removal).
    /// A generation that was ever claimed is permanently dead: it is never reused or kept.
    Unlinking,
    /// A claimed generation whose removal failed: dead (never reused or kept), still named so
    /// a later collection or recovery removes its file.
    Retired,
    /// The file was removed; the row keeps tombstoned versions' digest provenance.
    Unlinked,
}
impl BlobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Present => "Present",
            Self::UnlinkPending => "UnlinkPending",
            Self::Unlinking => "Unlinking",
            Self::Retired => "Retired",
            Self::Unlinked => "Unlinked",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::Present,
            Self::UnlinkPending,
            Self::Unlinking,
            Self::Retired,
            Self::Unlinked,
        ]
        .into_iter()
        .find(|b| b.as_str() == s)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobRecord {
    /// Lower-case hex SHA-256 of the content.
    pub digest: String,
    pub byte_count: i64,
    /// The file incarnation this row names. Every publication of new bytes uses a generation
    /// never used before (the publishing staging operation's ID), so the managed file
    /// `<digest>.<generation>` is written once and removing it can never remove another
    /// incarnation of the same digest.
    pub generation: String,
    /// Data-root-relative managed path of that generation, constructed by the file adapter.
    pub relative_path: String,
    pub state: BlobState,
    /// Holder of the `Unlinking` claim on `generation`, when there is one.
    pub claim_owner: Option<String>,
    /// Most recent physical removal failure; visible and retryable.
    pub last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArtifactState {
    Live,
    /// Explicitly deleted: every version is tombstoned and no new read can acquire it.
    Deleted,
}
impl ArtifactState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "Live",
            Self::Deleted => "Deleted",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Live" => Some(Self::Live),
            "Deleted" => Some(Self::Deleted),
            _ => None,
        }
    }
}

/// Who owns an artifact: a project, or the workspace (no project). A workspace artifact always
/// has an owning principal ([`StoredArtifact::owner`]); it is visible to that principal, the
/// user and the host, and the workspace coordinator, and to a project only once shared with it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ArtifactScope {
    Project(ProjectId),
    Workspace,
}
impl ArtifactScope {
    pub fn project(&self) -> Option<&ProjectId> {
        match self {
            Self::Project(p) => Some(p),
            Self::Workspace => None,
        }
    }
}

/// How long an artifact is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArtifactRetention {
    /// Kept until explicitly deleted (ART-05).
    Explicit,
    /// The durable typed reply of a command receipt: system-owned, immutable and retained while
    /// its receipt is retained. It is never deleted, shared, moved or replaced through the
    /// artifact use cases, and only the receipt's principal (with the user, the host and the
    /// workspace coordinator) sees it.
    Receipt,
}
impl ArtifactRetention {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "Explicit",
            Self::Receipt => "Receipt",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Explicit" => Some(Self::Explicit),
            "Receipt" => Some(Self::Receipt),
            _ => None,
        }
    }
}

/// One artifact: stable identity, origin and scope. Its versions are separate immutable rows.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredArtifact {
    pub id: Id,
    /// Project the artifact was created in (`None` for one created in the workspace scope);
    /// never changes (later reuse preserves it).
    pub origin_project_id: Option<ProjectId>,
    /// Work item (feature, issue or task root) that produced it, when the host lineage named one.
    pub origin_work_id: Option<RootId>,
    pub title: String,
    /// Positive; advances on every version publication or deletion.
    pub revision: i64,
    /// The scope that currently owns it: a project (the origin unless the documents service moved
    /// it) or the workspace.
    pub scope: ArtifactScope,
    /// The owning principal: required for the workspace scope and for receipt replies, `None`
    /// for ordinary project artifacts (the project owns them).
    pub owner: Option<ActorId>,
    pub retention: ArtifactRetention,
    /// Advances whenever the set of projects that can see it changes.
    pub scope_revision: i64,
    /// Advances whenever its reference set (links, shares) changes.
    pub reference_revision: i64,
    /// The version new readers see by default; `None` after the current version was deleted (an
    /// older retained version is never silently reactivated).
    pub current_version_id: Option<Id>,
    pub state: ArtifactState,
    pub created_at_ms: i64,
    /// Attributed creation provenance (producing principal and kind, session, assignment,
    /// invocation, source kind and handle, repository revision). Versioned JSON object.
    pub provenance: Value,
}

/// One immutable version.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredVersion {
    pub id: Id,
    pub artifact_id: Id,
    /// 1, 2, …: publication order within the artifact.
    pub version_no: i64,
    pub blob_digest: String,
    /// Byte length of the blob (read from the blob row).
    pub byte_count: i64,
    /// The file generation the blob row currently names (read from the blob row).
    pub blob_generation: String,
    pub media_type: String,
    pub created_at_ms: i64,
    /// Set once by explicit deletion (tombstone); the row and its provenance stay.
    pub deleted_at_ms: Option<i64>,
    /// Versioned JSON object: source kind and handle, the staging operation that published the
    /// bytes, producing principal and session.
    pub source: Value,
}
impl StoredVersion {
    pub fn live(&self) -> bool {
        self.deleted_at_ms.is_none()
    }
    /// The staging operation recorded in [`Self::source`].
    pub fn operation_id(&self) -> Option<&str> {
        self.source.get("operationId").and_then(Value::as_str)
    }
}

/// The work subject a link points at, in storage form.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SubjectKey {
    /// `root`, `assignment`, `operation`, `resource`, `blocker`, or `session` (a committed
    /// transcript entry of that session names the version).
    pub kind: &'static str,
    pub id: Id,
}
impl SubjectKey {
    pub fn of(subject: &Subject) -> Self {
        let (kind, id) = match subject {
            Subject::Root(r) => ("root", &r.0),
            Subject::Assignment(a) => ("assignment", &a.0),
            Subject::Operation(o) => ("operation", &o.0),
            Subject::Resource(r) => ("resource", &r.0),
            Subject::Blocker(b) => ("blocker", &b.0),
        };
        Self {
            kind,
            id: id.clone(),
        }
    }
    pub fn root(root: &RootId) -> Self {
        Self {
            kind: "root",
            id: root.0.clone(),
        }
    }
    /// The session whose committed transcript references a version.
    pub fn session(session: &Id) -> Self {
        Self {
            kind: "session",
            id: session.clone(),
        }
    }
    pub fn parse_kind(kind: &str) -> Option<&'static str> {
        [
            "root",
            "assignment",
            "operation",
            "resource",
            "blocker",
            "session",
        ]
        .into_iter()
        .find(|k| *k == kind)
    }
}

/// Why a version is linked to a subject.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LinkPurpose {
    /// Recorded at publication from the host lineage: the work that produced the version.
    Origin,
    /// Added later by `artifacts.reference`; reuse never rewrites the origin.
    Reference,
}
impl LinkPurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Origin => "origin",
            Self::Reference => "reference",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "origin" => Some(Self::Origin),
            "reference" => Some(Self::Reference),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactLink {
    pub version_id: Id,
    pub subject: SubjectKey,
    pub purpose: LinkPurpose,
}

/// Keyset position of the project listing (newest first, then ID descending).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListPosition {
    pub created_at_ms: i64,
    pub id: Id,
}

pub trait ArtifactRead {
    fn project_exists(&self, project: &ProjectId) -> Result<bool, StorageError>;
    /// Whether `subject` exists and belongs to `project` (roots, assignments, operations,
    /// resources and blockers through their owning work item where they have one).
    fn subject_exists(
        &self,
        project: &ProjectId,
        subject: &SubjectKey,
    ) -> Result<bool, StorageError>;
    fn artifact(&self, id: &Id) -> Result<Option<StoredArtifact>, StorageError>;
    fn version(&self, id: &Id) -> Result<Option<StoredVersion>, StorageError>;
    /// Every version of `artifact`, tombstoned ones included, in publication order.
    fn versions(&self, artifact: &Id) -> Result<Vec<StoredVersion>, StorageError>;
    /// The version a staging operation published, if its metadata committed.
    fn version_by_operation(
        &self,
        operation: &OperationId,
    ) -> Result<Option<StoredVersion>, StorageError>;
    /// Live artifacts with a current version that `project` owns or is shared with and that carry
    /// every tag in `tags` (all-of; empty means no tag filter), newest first after `after`, at
    /// most `limit` (1..=500). `tags` holds distinct values. Receipt replies are not listed: they
    /// are reached from their receipt.
    fn list(
        &self,
        project: &ProjectId,
        tags: &[String],
        after: Option<&ListPosition>,
        limit: u16,
    ) -> Result<Vec<StoredArtifact>, StorageError>;
    /// Live, explicitly retained workspace-scoped artifacts with a current version, owned by
    /// `owner` (`None`: every owner) and carrying every tag in `tags`, newest first after
    /// `after`, at most `limit` (1..=500). Receipt replies are not listed.
    fn list_workspace(
        &self,
        owner: Option<&ActorId>,
        tags: &[String],
        after: Option<&ListPosition>,
        limit: u16,
    ) -> Result<Vec<StoredArtifact>, StorageError>;
    /// The artifact's tags in byte order (empty when it has none).
    fn tags(&self, artifact: &Id) -> Result<Vec<String>, StorageError>;
    /// Projects the artifact is shared with (its owner is not listed), in ID order.
    fn shares(&self, artifact: &Id) -> Result<Vec<ProjectId>, StorageError>;
    /// Projects that read the artifact through repository membership: those with a member
    /// repository (not deleted) one of whose published views binds the artifact as its README
    /// or structural summary text, in ID order.
    fn repository_text_readers(&self, artifact: &Id) -> Result<Vec<ProjectId>, StorageError>;
    /// Links of every version of `artifact`.
    fn links(&self, artifact: &Id) -> Result<Vec<ArtifactLink>, StorageError>;
    /// Links pointing at `subject` (navigation from work to its artifacts), at most 500.
    fn linked_to(&self, subject: &SubjectKey) -> Result<Vec<ArtifactLink>, StorageError>;
    fn blob(&self, digest: &str) -> Result<Option<BlobRecord>, StorageError>;
    /// Live (not tombstoned) versions of any artifact that name `digest`.
    fn live_references(&self, digest: &str) -> Result<i64, StorageError>;
    /// Whether a manual backup (or another holder) pins `digest` against physical removal.
    fn pinned(&self, digest: &str) -> Result<bool, StorageError>;
    /// Blobs in `state` in digest order after `after`, at most `limit` (1..=500).
    fn blobs_in(
        &self,
        state: BlobState,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<BlobRecord>, StorageError>;
    /// Bytes of distinct blobs named by live versions of explicitly retained artifacts `project`
    /// owns (what its storage limit measures; receipt replies are not counted).
    fn project_usage(&self, project: &ProjectId) -> Result<i64, StorageError>;
    /// Bytes of distinct blobs named by live versions of explicitly retained workspace-scoped
    /// artifacts (receipt replies are not counted).
    fn workspace_usage(&self) -> Result<i64, StorageError>;
    /// Bytes of distinct blobs named by live versions of receipt replies owned by `project`
    /// (`None`: the workspace scope), reported separately from the storage limit.
    fn receipt_usage(&self, project: Option<&ProjectId>) -> Result<i64, StorageError>;
    /// Unresolved staging operations in ID order after `after`, at most `limit` (1..=500).
    fn unresolved_staging(
        &self,
        after: Option<&OperationId>,
        limit: u16,
    ) -> Result<Vec<OperationId>, StorageError>;
}

pub trait ArtifactRepository: ArtifactRead {
    /// Makes the row `Present` naming `generation` (inserting it when absent, clearing any
    /// claim). The caller decided, inside the same transaction, that `generation`'s file is the
    /// one to name. A different byte count for the same digest is
    /// [`StorageError::InvalidRecord`].
    fn put_blob(
        &mut self,
        digest: &str,
        byte_count: i64,
        generation: &str,
        relative_path: &str,
    ) -> Result<BlobRecord, StorageError>;
    /// Compare-and-set of a blob's state; a different current state is
    /// [`StorageError::InvalidRecord`]. `error` replaces the recorded failure.
    fn set_blob_state(
        &mut self,
        digest: &str,
        expected: BlobState,
        to: BlobState,
        error: Option<&str>,
    ) -> Result<BlobRecord, StorageError>;
    /// Inserts a new artifact (its current version is set by [`Self::update_artifact`] after the
    /// first version exists). Its projects, origin work item and owner must exist; a workspace
    /// artifact and a receipt reply need an owner.
    fn insert_artifact(&mut self, artifact: &StoredArtifact) -> Result<(), StorageError>;
    fn insert_version(&mut self, version: &StoredVersion) -> Result<(), StorageError>;
    /// Replaces the mutable columns (title, revisions, scope, current version, state). The
    /// identity, origin, owner, retention, creation time and provenance never change.
    fn update_artifact(&mut self, artifact: &StoredArtifact) -> Result<(), StorageError>;
    /// Tombstones a live version at `at_ms`; an already tombstoned version is
    /// [`StorageError::InvalidRecord`].
    fn tombstone_version(&mut self, version: &Id, at_ms: i64) -> Result<(), StorageError>;
    /// Adds a link; `false` when the identical link already exists.
    fn add_link(&mut self, link: &ArtifactLink) -> Result<bool, StorageError>;
    /// Replaces the artifact's tag set with `tags` (distinct values). The artifact must exist.
    fn set_tags(&mut self, artifact: &Id, tags: &[String]) -> Result<(), StorageError>;
    /// Adds a project share; `false` when it already exists.
    fn add_share(&mut self, artifact: &Id, project: &ProjectId) -> Result<bool, StorageError>;
    /// Removes a project share; `false` when there was none.
    fn remove_share(&mut self, artifact: &Id, project: &ProjectId) -> Result<bool, StorageError>;
    /// Pins `digest` for `holder` (idempotent). The blob row must exist.
    fn pin(&mut self, holder: &Id, digest: &str) -> Result<(), StorageError>;
    /// Releases every pin `holder` holds; returns how many were released.
    fn release_pins(&mut self, holder: &Id) -> Result<i64, StorageError>;
    /// Claims `generation` of an `UnlinkPending` (or already `Retired`) blob for removal:
    /// compare-and-set on (that state, `generation`) to `Unlinking` held by `owner`. `false`
    /// when the row is in any other state or names another generation.
    fn claim_blob(
        &mut self,
        digest: &str,
        generation: &str,
        owner: &str,
    ) -> Result<bool, StorageError>;
    /// Adopts a claim another holder left: compare-and-set on (`Unlinking`, `generation`,
    /// `from_owner`) to `to_owner`. `false` when the claim changed since it was read.
    fn adopt_claim(
        &mut self,
        digest: &str,
        generation: &str,
        from_owner: &str,
        to_owner: &str,
    ) -> Result<bool, StorageError>;
    /// Finishes a held claim: compare-and-set on (`Unlinking`, `generation`, `owner`) to
    /// `Unlinked`, or with `error` to `Retired` with the visible error (the generation stays
    /// dead; never back to a reusable state). `false` when the holder no longer owns it.
    fn finish_claim(
        &mut self,
        digest: &str,
        generation: &str,
        owner: &str,
        error: Option<&str>,
    ) -> Result<bool, StorageError>;
}
