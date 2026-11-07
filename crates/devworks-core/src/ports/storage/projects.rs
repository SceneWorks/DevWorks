//! Project and repository registration records (DW-011): project boundaries and their revision,
//! repository membership, registered repositories and the app index views published for them.
//!
//! The repository enforces record invariants (lifecycle names, compare-and-set revisions,
//! membership referencing registered repositories, bounded listings). Visibility, authority,
//! archive fencing, confirmation and what each command may change belong to
//! `crate::services::projects`, which writes inside a unit of work.
//!
//! A project is never deleted as a row: deletion tombstones it (`Deleted`) so retained
//! cross-scope references (history, receipts, artifacts) keep resolving to an honest record.
//! Registered repositories are app records of a source location; nothing here touches the
//! source itself. Removing an index removes only the published index views and their texts.
use super::StorageError;
use crate::protocol::{Id, ProjectId};

/// The stored lifecycle names (`LifeState` wire names).
pub const ACTIVE: &str = "Active";
pub const ARCHIVED: &str = "Archived";
pub const DELETED: &str = "Deleted";
/// Every stored lifecycle name.
pub const LIFECYCLES: [&str; 3] = [ACTIVE, ARCHIVED, DELETED];

/// One project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredProject {
    pub id: ProjectId,
    pub name: String,
    pub description: String,
    /// `Active`, `Archived` or `Deleted`.
    pub lifecycle: String,
    pub revision: i64,
    /// Advances whenever the project's boundary (its repository membership) changes.
    pub scope_revision: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub archived_at_ms: Option<i64>,
    pub deleted_at_ms: Option<i64>,
    /// Advances with every archival: a launch whose intent was recorded under an earlier epoch
    /// belongs to work an archival stopped.
    pub archive_epoch: i64,
    /// The archival's immediate stop (the project-wide kill of owned trees) has not run yet.
    /// Restore and delete wait for it; the engine runs it and clears the flag.
    pub archive_stop_pending: bool,
}

/// One registered repository: the app record of a source location (never the source itself).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredRepository {
    pub id: Id,
    /// The canonical local location it was registered at (exact; never rewritten).
    pub location: String,
    pub name: String,
    /// The remote it tracks, as registered (credentials are stripped before it is shown).
    pub remote_url: Option<String>,
    /// A primary local checkout (as opposed to a remote-only registration).
    pub primary: bool,
    pub lifecycle: String,
    /// Language identifiers discovery recorded for it (lowercase).
    pub languages: Vec<String>,
    pub revision: i64,
    pub registered_at_ms: i64,
}

/// Coverage of a published view or text: `Complete`, `Partial` or `Unavailable`.
pub const COVERAGE: [&str; 3] = ["Complete", "Partial", "Unavailable"];

/// One app index view published for a repository (the indexing item publishes them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredIndexView {
    pub view_id: Id,
    pub repository_id: Id,
    pub source_revision: String,
    pub coverage: String,
    pub published_at_ms: i64,
}

/// What a repository text holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextKind {
    Readme,
    Summary,
}
impl TextKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Readme => "Readme",
            Self::Summary => "Summary",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Readme" => Some(Self::Readme),
            "Summary" => Some(Self::Summary),
            _ => None,
        }
    }
}

/// A repository text of one view: its README or generated summary, as an artifact version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredRepositoryText {
    pub view_id: Id,
    pub repository_id: Id,
    pub kind: TextKind,
    pub artifact_id: Id,
    pub version_id: Id,
    pub sha256: String,
    pub byte_length: i64,
    pub media_type: String,
    pub coverage: String,
}

/// Which projects a listing covers (ordered by creation time, then ID).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectFilter {
    /// Only this project (a project invocation's own).
    pub only: Option<ProjectId>,
    /// Lifecycle names to include (never empty).
    pub lifecycles: Vec<String>,
}

/// Which repositories a listing covers (ordered by ID).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepositoryFilter {
    /// Only members of this project.
    pub project: Option<ProjectId>,
    /// Case-insensitive substring of the name, location or remote.
    pub text: Option<String>,
    /// Every one of these languages (lowercase).
    pub languages: Vec<String>,
    /// Include deleted registrations.
    pub include_deleted: bool,
}

/// Position after which a project listing continues.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectPosition {
    pub created_at_ms: i64,
    pub id: ProjectId,
}

/// The exact native action covered by a reviewed row witness.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProjectPurgeRowAction {
    Delete,
    Scrub,
    Keep,
}

/// Fixed audit vocabulary: no project-authored text is accepted as a row reason.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProjectPurgeRowReason {
    OwnedProjectRecords,
    OwnedLineage,
    ProjectMembership,
    MinimalDeletedIdentity,
    SharedContent,
    IndependentPublication,
    RevokedAuthority,
    ReceiptReplayFence,
    PurgeControlAudit,
    ScopedReadLease,
    DerivedContent,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProjectPurgeRowWitness {
    pub table: String,
    pub key_digest: String,
    pub row_digest: String,
    pub action: ProjectPurgeRowAction,
    pub reason: ProjectPurgeRowReason,
}

/// A scoped planning candidate, deliberately without a deletion action or permit.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ProjectPurgeCandidate {
    pub table: String,
    pub key_digest: String,
    pub row_digest: String,
}
#[derive(Clone, Debug)]
pub struct ProjectPurgeCandidates {
    pub project_id: ProjectId,
    pub policy_digest: String,
    pub digest: String,
    pub rows: Vec<ProjectPurgeCandidate>,
}

#[derive(Clone, Debug)]
/// Durable reviewed purge intent. Private source keys are never copied here.
pub struct StoredProjectPurge {
    pub manifest: crate::protocol::records::ProjectPurgeManifest,
    pub actor_id: crate::protocol::ActorId,
    pub store_id: crate::protocol::StoreId,
    pub restore_epoch: i64,
    pub scope_revision: i64,
    pub policy_digest: String,
    pub witness_digest: String,
    pub entries: Vec<crate::protocol::records::ProjectPurgeEntry>,
    pub rows: Vec<ProjectPurgeRowWitness>,
}

/// Content identity remains stable while execution status advances. The native producer
/// and storage fence use this same encoding; no secret or caller-minted authority is involved.
pub fn project_purge_digest(p: &StoredProjectPurge) -> String {
    let m = &p.manifest;
    let mut rows = p.rows.iter().collect::<Vec<_>>();
    rows.sort_by(|a, b| (&a.table, &a.key_digest).cmp(&(&b.table, &b.key_digest)));
    crate::fingerprint::sha256_hex(
        serde_json::to_string(&serde_json::json!({
            "manifestId":m.id,"projectId":m.project_id,"projectRevision":m.project_revision,
            "scopeRevision":p.scope_revision,"storeId":p.store_id,"restoreEpoch":p.restore_epoch,
            "actorId":p.actor_id,"policyDigest":p.policy_digest,"witnessDigest":p.witness_digest,
            "recordCount":m.record_count,"managedByteCount":m.managed_byte_count,
            "keptRecordCount":m.kept_record_count,"keptByteCount":m.kept_byte_count,
            "createdAtMs":m.created_at_ms,"entries":p.entries,"rows":rows
        }))
        .expect("purge digest values are JSON")
        .as_bytes(),
    )
}

/// Reserved before external creation; a live write prevents purge preparation.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectManagedFileReservation {
    pub id: Id,
    pub project_id: ProjectId,
    pub kind: String,
    pub relative_path: String,
    pub root: crate::authority::destructive::FileIdentity,
    pub store_id: crate::protocol::StoreId,
    pub restore_epoch: i64,
    pub boot_id: Option<Id>,
    pub created_at_ms: i64,
}

/// A host-created transient file, attributed durably before its source handle escapes.
#[derive(Clone, Debug)]
pub struct StoredProjectManagedFile {
    pub id: Id,
    pub project_id: ProjectId,
    pub kind: String,
    pub relative_path: String,
    pub witness: crate::ports::project_purge::PurgeFileWitness,
    pub created_at_ms: i64,
}

/// Recovery view for one normal staging owner, including incomplete writes.
#[derive(Clone, Debug)]
pub struct ProjectManagedFileRecovery {
    pub id: Id,
    pub project_id: ProjectId,
    pub relative_path: String,
    pub root: crate::authority::destructive::FileIdentity,
    pub writing: bool,
    pub boot_id: Option<Id>,
}

pub trait ProjectRead {
    /// Snapshot exact candidate rows for the ownership classifier. This neither
    /// classifies mixed owners/keepers nor grants permission to delete any row.
    fn purge_candidates(
        &self,
        project: &ProjectId,
    ) -> Result<Option<ProjectPurgeCandidates>, StorageError>;
    fn managed_file_recovery(
        &self,
        kind: &str,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<ProjectManagedFileRecovery>, StorageError>;

    fn pending_project_managed_writes(&self, project: &ProjectId) -> Result<bool, StorageError>;

    fn project_managed_file(
        &self,
        project: &ProjectId,
        id: &Id,
    ) -> Result<Option<StoredProjectManagedFile>, StorageError>;
    fn project_managed_files(
        &self,
        project: &ProjectId,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredProjectManagedFile>, StorageError>;

    /// Exact project binding is mandatory even when the manifest ID is known.
    fn purge_manifest(
        &self,
        project: &ProjectId,
        manifest: &Id,
    ) -> Result<Option<StoredProjectPurge>, StorageError>;

    fn project(&self, id: &ProjectId) -> Result<Option<StoredProject>, StorageError>;
    /// At most `limit` projects after `after`, by creation time then ID.
    fn projects(
        &self,
        filter: &ProjectFilter,
        after: Option<&ProjectPosition>,
        limit: u16,
    ) -> Result<Vec<StoredProject>, StorageError>;
    /// The project's member repositories, by ID.
    fn members(&self, project: &ProjectId) -> Result<Vec<Id>, StorageError>;
    fn repository(&self, id: &Id) -> Result<Option<StoredRepository>, StorageError>;
    /// At most `limit` repositories after `after`, by ID.
    fn repositories(
        &self,
        filter: &RepositoryFilter,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredRepository>, StorageError>;
    /// The projects a repository is a member of (any lifecycle but `Deleted`), by ID.
    fn repository_projects(&self, repository: &Id) -> Result<Vec<ProjectId>, StorageError>;
    /// The repository's published index views, newest first.
    fn index_views(&self, repository: &Id) -> Result<Vec<StoredIndexView>, StorageError>;
    fn index_view(&self, view: &Id) -> Result<Option<StoredIndexView>, StorageError>;
    fn repository_text(
        &self,
        view: &Id,
        kind: TextKind,
    ) -> Result<Option<StoredRepositoryText>, StorageError>;
    /// Work items of `project` that name `repository`: in their repository context, in an
    /// expected touch, or in their feature's pending plan draft.
    fn work_naming_repository(
        &self,
        project: &ProjectId,
        repository: &Id,
    ) -> Result<i64, StorageError>;
    /// Projects whose archival stop has not run yet, by ID.
    fn pending_archive_stops(&self, limit: u16) -> Result<Vec<ProjectId>, StorageError>;
}

pub trait ProjectRepository: ProjectRead {
    fn reserve_project_managed_file(
        &mut self,
        file: &ProjectManagedFileReservation,
    ) -> Result<(), StorageError>;

    fn register_project_managed_file(
        &mut self,
        file: &StoredProjectManagedFile,
    ) -> Result<(), StorageError>;
    /// A normal staging owner records physical settlement and forgets its private path.
    fn finish_project_managed_file(
        &mut self,
        project: &ProjectId,
        id: &Id,
    ) -> Result<(), StorageError>;

    /// Records an immutable preparation under its current project/store fences.
    fn insert_purge_manifest(&mut self, manifest: &StoredProjectPurge) -> Result<(), StorageError>;

    /// Creates a project (revision 1). An existing ID is `Duplicate`.
    fn insert_project(&mut self, project: &StoredProject) -> Result<(), StorageError>;
    /// Replaces the project at `expected_revision` (a stale one is a `Scope` revision conflict).
    fn update_project(
        &mut self,
        project: &StoredProject,
        expected_revision: i64,
    ) -> Result<(), StorageError>;
    /// Replaces the project's membership with exactly `repositories` (each registered).
    fn set_members(&mut self, project: &ProjectId, repositories: &[Id])
    -> Result<(), StorageError>;
    /// Registers a repository (revision 1); the repository discovery item calls this.
    fn insert_repository(&mut self, repository: &StoredRepository) -> Result<(), StorageError>;
    /// Bumps the repository's revision from `expected_revision` (a stale one is a `Registry`
    /// revision conflict); returns the new revision.
    fn touch_repository(
        &mut self,
        repository: &Id,
        expected_revision: i64,
    ) -> Result<i64, StorageError>;
    /// Publishes an index view (the indexing item calls this).
    fn publish_index_view(&mut self, view: &StoredIndexView) -> Result<(), StorageError>;
    /// Records a text of a published view (the indexing item calls this).
    fn put_repository_text(&mut self, text: &StoredRepositoryText) -> Result<(), StorageError>;
    /// Removes every published index view of the repository and their texts; returns how many
    /// views were removed. The registration, membership and work history are untouched.
    fn remove_index(&mut self, repository: &Id) -> Result<u64, StorageError>;
    /// Records that the archival stop of `epoch` ran (no project revision change). Returns
    /// whether that stop was still pending.
    fn finish_archive_stop(
        &mut self,
        project: &ProjectId,
        epoch: i64,
    ) -> Result<bool, StorageError>;
}
