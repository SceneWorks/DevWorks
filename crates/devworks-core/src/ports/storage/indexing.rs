//! Repository indexing records (DW-023): per-scope source configuration, the observed source
//! identity of every registration (path, remote, HEAD) and its worktrees, index runs with their
//! claims and checkpoints, staged and published graph generations (whole source-unit
//! replacement, AD 23), immutable views and the overlay lifecycle.
//!
//! The repository enforces record invariants: state names, compare-and-set revisions, one active
//! run per repository and overlay, append-only immutable generations once sealed, and the
//! published pointer's compare-and-set. Scope, authority, which run may claim, what a stage may
//! publish and every lifecycle decision belong to `crate::services::indexing`, which writes
//! inside a unit of work.
//!
//! Graph payload lives in the migration 1 tables (`index_generations`, `source_units`,
//! `graph_nodes`, `graph_edges`, `index_views`, `published_views`, `index_pins`); nothing here
//! exposes SQL. A staged generation is never visible through a published view until
//! [`IndexRepository::publish`] moves the pointer.
use super::StorageError;
use crate::protocol::{ActorId, Id, ProjectId};
use serde_json::Value;

// ---------------------------------------------------------------------------------------------
// Names

/// Run kinds.
pub const RUN_INDEX: &str = "Index";
pub const RUN_REINDEX: &str = "Reindex";
pub const RUN_OVERLAY: &str = "Overlay";
pub const RUN_KINDS: [&str; 3] = [RUN_INDEX, RUN_REINDEX, RUN_OVERLAY];

/// Run states. `Queued` and `Running` are active; the others are terminal.
pub const RUN_QUEUED: &str = "Queued";
pub const RUN_RUNNING: &str = "Running";
pub const RUN_SUCCEEDED: &str = "Succeeded";
pub const RUN_FAILED: &str = "Failed";
pub const RUN_CANCELLED: &str = "Cancelled";
pub const RUN_STATES: [&str; 5] = [
    RUN_QUEUED,
    RUN_RUNNING,
    RUN_SUCCEEDED,
    RUN_FAILED,
    RUN_CANCELLED,
];
/// Whether a run state is terminal (nothing claims, cancels or publishes it again).
pub fn run_terminal(state: &str) -> bool {
    matches!(state, RUN_SUCCEEDED | RUN_FAILED | RUN_CANCELLED)
}

/// Generation layers (AD 23) and lifecycle states.
pub const LAYER_BASELINE: &str = "Baseline";
pub const LAYER_BRANCH: &str = "Branch";
pub const LAYER_WORKTREE: &str = "Worktree";
pub const LAYERS: [&str; 3] = [LAYER_BASELINE, LAYER_BRANCH, LAYER_WORKTREE];
/// Being written in bounded batches; never readable through a view.
pub const GEN_STAGING: &str = "Staging";
/// Complete, verified and immutable; referenced by a view.
pub const GEN_SEALED: &str = "Sealed";
/// No view references it any more; its payload is being reclaimed.
pub const GEN_RETIRED: &str = "Retired";
pub const GEN_STATES: [&str; 3] = [GEN_STAGING, GEN_SEALED, GEN_RETIRED];

/// What a source unit records in its generation.
pub const UNIT_REPLACE: &str = "Replace";
pub const UNIT_DELETE: &str = "Delete";

/// Overlay states (CODE-11: distinct queued, indexing, ready, failed, stale, expired and
/// unavailable states).
pub const OVERLAY_QUEUED: &str = "Queued";
pub const OVERLAY_INDEXING: &str = "Indexing";
pub const OVERLAY_READY: &str = "Ready";
pub const OVERLAY_FAILED: &str = "Failed";
pub const OVERLAY_STALE: &str = "Stale";
pub const OVERLAY_EXPIRED: &str = "Expired";
pub const OVERLAY_UNAVAILABLE: &str = "Unavailable";
pub const OVERLAY_STATES: [&str; 7] = [
    OVERLAY_QUEUED,
    OVERLAY_INDEXING,
    OVERLAY_READY,
    OVERLAY_FAILED,
    OVERLAY_STALE,
    OVERLAY_EXPIRED,
    OVERLAY_UNAVAILABLE,
];

/// Observed source states of a registration.
pub const SOURCE_PRESENT: &str = "Present";
pub const SOURCE_MISSING: &str = "Missing";
pub const SOURCE_NOT_REPOSITORY: &str = "NotARepository";

/// The workspace configuration scope key (a project scope is keyed by its project ID).
pub const WORKSPACE_SCOPE_KEY: &str = "workspace";

// ---------------------------------------------------------------------------------------------
// Records

/// The index source configuration of one section: the workspace or one project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSourceConfig {
    /// [`WORKSPACE_SCOPE_KEY`] or the project ID.
    pub scope_key: String,
    pub project_id: Option<ProjectId>,
    /// 1 for a section never written (the defaults); advances with every change.
    pub revision: i64,
    /// Structural languages to extract (lowercase IDs); empty means every supported language.
    pub structural_languages: Vec<String>,
    /// Semantic language packs to apply when installed (item 24–27 IDs).
    pub semantic_packages: Vec<String>,
    /// Exclusion patterns (see `crate::services::indexing::config`).
    pub exclude_patterns: Vec<String>,
    pub max_file_bytes: i64,
    pub updated_at_ms: i64,
}

/// The observed source identity of a registered repository (discovery keeps it current).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredRepositorySource {
    pub repository_id: Id,
    /// The section it was discovered under ([`WORKSPACE_SCOPE_KEY`] or a project ID).
    pub section: String,
    /// The base directory it was found under (its group is the path between the two).
    pub section_root: Option<String>,
    /// The exact location it was found at (the registration's canonical location).
    pub location: String,
    /// [`SOURCE_PRESENT`], [`SOURCE_MISSING`] or [`SOURCE_NOT_REPOSITORY`].
    pub state: String,
    /// The commit `HEAD` named when observed (lowercase hex), absent for an unborn branch.
    pub head_oid: Option<String>,
    /// The branch `HEAD` points at (`refs/heads/...`), absent when detached.
    pub head_ref: Option<String>,
    /// `Sha1` or `Sha256`.
    pub object_format: String,
    /// The `origin` remote as configured (never stored when it embeds credentials).
    pub remote_url: Option<String>,
    /// Advances whenever the observed identity changes.
    pub revision: i64,
    pub observed_at_ms: i64,
}

/// One worktree of a registered repository: its primary checkout or a linked worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredWorktree {
    pub repository_id: Id,
    pub worktree_id: Id,
    pub path: String,
    pub head_oid: Option<String>,
    pub head_ref: Option<String>,
    /// The repository's primary checkout.
    pub main: bool,
    pub observed_at_ms: i64,
}

/// A warning a run reports with its record (bounded; the full log is the diagnostics channel).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunWarning {
    pub code: String,
    pub message: String,
}

/// One index run: a baseline index, a manual rebuild or an overlay build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredIndexRun {
    /// Also the run's operation ID (`indexing.cancel` names it).
    pub id: Id,
    /// The request that recorded it (the first run of that request): a request naming several
    /// repositories records one run each, and cancelling the batch cancels them together.
    pub batch_id: Id,
    /// [`RUN_INDEX`], [`RUN_REINDEX`] or [`RUN_OVERLAY`].
    pub kind: String,
    pub repository_id: Id,
    /// The project of the invocation that requested it (its archival fences the run).
    pub project_id: Option<ProjectId>,
    pub overlay_id: Option<Id>,
    pub state: String,
    pub stage: String,
    pub revision: i64,
    /// Claims so far; every claim advances it, so a stale owner's writes are refused.
    pub attempt: i64,
    pub max_attempts: i64,
    /// The claiming owner (boot owner identity) and its lease.
    pub owner: Option<String>,
    pub lease_until_ms: Option<i64>,
    pub cancel_requested_at_ms: Option<i64>,
    /// The source configuration the run was requested under.
    pub config_revision: i64,
    pub config_digest: String,
    /// The commit the run indexes, once resolved.
    pub source_oid: Option<String>,
    /// The staging generation (the checkpoint a resumed attempt continues).
    pub generation_id: Option<Id>,
    pub completed_units: i64,
    pub total_units: Option<i64>,
    pub output_view_id: Option<Id>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    /// The failure would repeat on every attempt: never retried.
    pub deterministic: bool,
    pub warnings: Vec<RunWarning>,
    pub requested_by: ActorId,
    pub created_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    /// Earliest time a queued run may be claimed (backoff after a transient failure).
    pub next_attempt_at_ms: i64,
}

/// Which runs a listing covers (newest first, then by ID).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunFilter {
    /// Only runs of this repository.
    pub repository: Option<Id>,
    /// Only runs of these repositories (a project's members); `None` means every repository.
    pub repositories: Option<Vec<Id>>,
}

/// Position after which a run listing continues.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunPosition {
    pub created_at_ms: i64,
    pub id: Id,
}

/// One graph generation (a baseline, branch or captured-worktree layer).
#[derive(Clone, Debug, PartialEq)]
pub struct StoredGeneration {
    pub id: Id,
    pub repository_id: Id,
    /// [`LAYER_BASELINE`], [`LAYER_BRANCH`] or [`LAYER_WORKTREE`].
    pub layer: String,
    pub state: String,
    /// The commit (baseline/branch) or capture digest (worktree) it was built from.
    pub source_digest: String,
    /// Units recorded (the seal checks it).
    pub watermark: i64,
    /// Parent generation, run, configuration digest, coverage and counts.
    pub manifest: Value,
}

/// One source unit (a file path) of a generation.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredUnit {
    pub generation_id: Id,
    /// The repository-relative path with `/` separators (exact case).
    pub unit_key: String,
    pub content_digest: String,
    /// [`UNIT_REPLACE`] or [`UNIT_DELETE`] (a delete masks lower layers).
    pub action: String,
    /// Language, extraction mode, size, parse errors, skip reason, previous path.
    pub payload: Value,
}

/// One node a unit contributes.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredNode {
    pub unit_key: String,
    /// Stable across generations for the same unit, kind and qualified name.
    pub node_id: String,
    pub kind: String,
    pub name: String,
    /// Qualified name, span, language, properties, confidence and provenance.
    pub payload: Value,
}

/// One relationship a unit contributes. Its target is a node ID (resolved or inferred) or a
/// symbolic `sym-` key (unresolved); the payload says which.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredEdge {
    pub unit_key: String,
    pub edge_id: String,
    pub source_node_id: String,
    pub target_node_id: String,
    pub kind: String,
    /// Resolution (`Resolved`, `Inferred`, `Unresolved`), confidence, target name, provenance.
    pub payload: Value,
}

/// A resolved target for one stored edge.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeTargetUpdate {
    pub unit_key: String,
    pub edge_id: String,
    pub target_node_id: String,
    pub payload: Value,
}

/// An immutable view: one repository's baseline plus optional branch and worktree layers.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredView {
    pub view_id: Id,
    pub repository_id: Id,
    /// Digest of the layer identities and snapshot (unique).
    pub view_digest: String,
    pub baseline_id: Id,
    pub branch_id: Option<Id>,
    pub worktree_id: Option<Id>,
    /// Coverage, snapshot provenance and the configuration digest.
    pub coverage: Value,
}

/// The published baseline pointer of a repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredPublished {
    pub view_id: Id,
    /// Advances with every publication (compare-and-set).
    pub revision: i64,
}

/// One overlay: the derived view of a worktree over its repository's baseline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredOverlay {
    pub id: Id,
    pub repository_id: Id,
    pub worktree_id: Id,
    pub worktree_path: String,
    pub state: String,
    pub revision: i64,
    /// The baseline generation and commit it composes over.
    pub base_generation_id: Option<Id>,
    pub base_oid: Option<String>,
    /// The worktree `HEAD` commit and branch.
    pub head_oid: Option<String>,
    pub head_ref: Option<String>,
    /// Digest of the captured staged, unstaged and indexable untracked changes.
    pub capture_digest: Option<String>,
    pub branch_generation_id: Option<Id>,
    pub worktree_generation_id: Option<Id>,
    pub view_id: Option<Id>,
    pub run_id: Option<Id>,
    /// Why it failed, went stale or became unavailable.
    pub reason: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub expires_at_ms: i64,
}

// ---------------------------------------------------------------------------------------------
// Ports

pub trait IndexRead {
    /// The stored configuration of a section (`None`: never written, the defaults apply).
    fn source_config(&self, scope_key: &str) -> Result<Option<StoredSourceConfig>, StorageError>;
    fn repository_source(
        &self,
        repository: &Id,
    ) -> Result<Option<StoredRepositorySource>, StorageError>;
    /// The registration at exactly `location`, if any (any lifecycle).
    fn repository_at(&self, location: &str) -> Result<Option<Id>, StorageError>;
    /// The repository's worktrees (primary checkout first, then by path).
    fn worktrees(&self, repository: &Id) -> Result<Vec<StoredWorktree>, StorageError>;
    fn run(&self, id: &Id) -> Result<Option<StoredIndexRun>, StorageError>;
    /// At most `limit` runs after `after`, newest first.
    fn runs(
        &self,
        filter: &RunFilter,
        after: Option<&RunPosition>,
        limit: u16,
    ) -> Result<Vec<StoredIndexRun>, StorageError>;
    /// The active (queued or running) run of a repository's baseline (`overlay` `None`) or of
    /// one overlay.
    fn active_run(
        &self,
        repository: &Id,
        overlay: Option<&Id>,
    ) -> Result<Option<StoredIndexRun>, StorageError>;
    /// The runs one request recorded, by ID.
    fn batch_runs(&self, batch: &Id) -> Result<Vec<StoredIndexRun>, StorageError>;
    /// The newest run of a repository's baseline (`overlay` `None`) or of one overlay.
    fn latest_run(
        &self,
        repository: &Id,
        overlay: Option<&Id>,
    ) -> Result<Option<StoredIndexRun>, StorageError>;
    /// Runs a claim may take at `now_ms`, oldest first: queued and due, or running with an
    /// expired lease, of repositories with no run under a live claim (so a busy repository never
    /// fills the listing).
    fn claimable_runs(&self, now_ms: i64, limit: u16) -> Result<Vec<StoredIndexRun>, StorageError>;
    /// Every run currently `Running`, by ID.
    fn running_runs(&self, limit: u16) -> Result<Vec<StoredIndexRun>, StorageError>;
    fn generation(&self, id: &Id) -> Result<Option<StoredGeneration>, StorageError>;
    /// The repository's generations (any layer and state), by ID.
    fn generations(&self, repository: &Id) -> Result<Vec<StoredGeneration>, StorageError>;
    /// Whether any view names `generation` as one of its layers.
    fn generation_in_view(&self, generation: &Id) -> Result<bool, StorageError>;
    /// At most `limit` generations in `state` (any repository), by ID.
    fn generations_in_state(
        &self,
        state: &str,
        limit: u16,
    ) -> Result<Vec<StoredGeneration>, StorageError>;
    /// At most `limit` units of `generation` after `after`, by unit key.
    fn units(
        &self,
        generation: &Id,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<StoredUnit>, StorageError>;
    fn unit(&self, generation: &Id, unit_key: &str) -> Result<Option<StoredUnit>, StorageError>;
    fn nodes(&self, generation: &Id, unit_key: &str) -> Result<Vec<StoredNode>, StorageError>;
    fn edges(&self, generation: &Id, unit_key: &str) -> Result<Vec<StoredEdge>, StorageError>;
    /// At most `limit` nodes of `generation` named exactly `name`, by unit and node ID.
    fn nodes_named(
        &self,
        generation: &Id,
        name: &str,
        limit: u16,
    ) -> Result<Vec<StoredNode>, StorageError>;
    fn published(&self, repository: &Id) -> Result<Option<StoredPublished>, StorageError>;
    fn view(&self, view: &Id) -> Result<Option<StoredView>, StorageError>;
    /// Read pins of `view` whose lease runs past `now_ms`.
    fn live_pins(&self, view: &Id, now_ms: i64) -> Result<i64, StorageError>;
    fn overlay(&self, id: &Id) -> Result<Option<StoredOverlay>, StorageError>;
    fn overlay_for(
        &self,
        repository: &Id,
        worktree: &Id,
    ) -> Result<Option<StoredOverlay>, StorageError>;
    /// The repository's overlays, by worktree ID.
    fn overlays(&self, repository: &Id) -> Result<Vec<StoredOverlay>, StorageError>;
    /// Overlays (not expired or unavailable) whose expiry is at or before `now_ms`.
    fn overlays_due(&self, now_ms: i64, limit: u16) -> Result<Vec<StoredOverlay>, StorageError>;
}

pub trait IndexRepository: IndexRead {
    /// Replaces a section's configuration at `expected_revision` (1 for a section never
    /// written); a stale revision is a `Configuration` revision conflict.
    fn put_source_config(
        &mut self,
        config: &StoredSourceConfig,
        expected_revision: i64,
    ) -> Result<(), StorageError>;
    /// Inserts or replaces the observed source identity of a registration.
    fn put_repository_source(
        &mut self,
        source: &StoredRepositorySource,
    ) -> Result<(), StorageError>;
    /// Replaces the repository's worktree set with exactly `worktrees`.
    fn put_worktrees(
        &mut self,
        repository: &Id,
        worktrees: &[StoredWorktree],
    ) -> Result<(), StorageError>;
    /// Records a new run; a second active run of the same repository baseline or overlay is
    /// `Duplicate`.
    fn insert_run(&mut self, run: &StoredIndexRun) -> Result<(), StorageError>;
    /// Replaces a run at `expected_revision` (a stale one is an `Operation` revision conflict).
    /// A terminal run is never changed again.
    fn update_run(
        &mut self,
        run: &StoredIndexRun,
        expected_revision: i64,
    ) -> Result<(), StorageError>;
    /// Records a new `Staging` generation.
    fn insert_generation(&mut self, generation: &StoredGeneration) -> Result<(), StorageError>;
    /// Replaces a generation's state, watermark and manifest. A sealed generation's content is
    /// immutable: only `Sealed` → `Retired` is allowed after sealing.
    fn update_generation(&mut self, generation: &StoredGeneration) -> Result<(), StorageError>;
    /// Stages one unit with exactly these rows (replacing any earlier staging of the same unit,
    /// so a retried batch is idempotent). Refused unless the generation is `Staging`.
    fn put_unit(
        &mut self,
        unit: &StoredUnit,
        nodes: &[StoredNode],
        edges: &[StoredEdge],
    ) -> Result<(), StorageError>;
    /// Copies the named units (with their rows) of sealed generation `from` into staging
    /// generation `to`; returns how many were copied.
    fn inherit_units(
        &mut self,
        from: &Id,
        to: &Id,
        unit_keys: &[String],
    ) -> Result<u64, StorageError>;
    /// Sets resolved targets of staged edges.
    fn set_edge_targets(
        &mut self,
        generation: &Id,
        updates: &[EdgeTargetUpdate],
    ) -> Result<(), StorageError>;
    /// Removes at most `limit` payload rows of a retired or staging generation; once nothing
    /// remains it removes the generation itself. Returns the rows removed.
    fn reclaim_generation(&mut self, generation: &Id, limit: u32) -> Result<u64, StorageError>;
    /// Records an immutable view.
    fn insert_view(&mut self, view: &StoredView) -> Result<(), StorageError>;
    /// Moves the repository's published pointer to `view` (already inserted) when the pointer
    /// is still at `expected_revision` (0: none published); returns the new revision. A stale
    /// revision is a `Source` revision conflict: a slower builder never overwrites a newer
    /// pointer.
    fn publish(
        &mut self,
        repository: &Id,
        view: &Id,
        expected_revision: i64,
    ) -> Result<i64, StorageError>;
    /// Removes a view that no published pointer names, with its registry row and texts.
    fn remove_view(&mut self, view: &Id) -> Result<(), StorageError>;
    /// Inserts an overlay (`expected_revision` `None`) or replaces it at that revision.
    fn put_overlay(
        &mut self,
        overlay: &StoredOverlay,
        expected_revision: Option<i64>,
    ) -> Result<(), StorageError>;
}
