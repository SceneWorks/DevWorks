//! Durable package registry records (architecture "Durable package state and atomic
//! activation"): installed versions and their verified identity, desired enablement and the
//! active pointer of every package, the one revisioned registry head (the `ActivePackageSet`
//! revision), exact-version approval proposals and retained package state generations.
//!
//! The repository enforces record invariants (bounded identities, one version per
//! `(package, manifest digest)`, a monotonic registry head, compare-and-set on every package
//! record and proposal, generations that only grow). Trust, approval, compatibility and
//! publication rules belong to `crate::services::packages`, which writes inside one unit of work,
//! so a registry pointer, its state generation, the approval it consumed and its history commit
//! together or not at all.
use super::StorageError;
use crate::protocol::{ActorId, Id};
use serde_json::Value;

/// Lifecycle of one installed version's files and identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionPhase {
    /// Verified and published on disk, waiting for approval, trust, compatibility or quiescence.
    Staged,
    /// Verified, published and approved (it is, was or may again become active).
    Installed,
    /// Its files were removed after retirement; the identity and provenance are kept.
    Removed,
    /// A person declined it or a newer proposal superseded it; it never activates.
    Rejected,
}
impl VersionPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Staged => "Staged",
            Self::Installed => "Installed",
            Self::Removed => "Removed",
            Self::Rejected => "Rejected",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "Staged" => Self::Staged,
            "Installed" => Self::Installed,
            "Removed" => Self::Removed,
            "Rejected" => Self::Rejected,
            _ => return None,
        })
    }
}

/// One immutable package version: its verified identity never changes after insertion; only its
/// phase, waits and approval move. Reusing a version string with different contents is a
/// different record (different manifest digest).
#[derive(Clone, Debug, PartialEq)]
pub struct StoredPackageVersion {
    pub package_id: String,
    /// SHA-256 of the exact signed manifest bytes (the version's identity).
    pub manifest_digest: String,
    /// SHA-256 of the whole bundle as received.
    pub bundle_digest: String,
    pub version: String,
    pub publisher_id: String,
    /// Fingerprint (SHA-256 of the public key) of a valid signature; `None` for unsigned content.
    pub signer_key: Option<String>,
    /// The verified inspection (`crate::ports::packages::Inspection` as JSON): manifest facts,
    /// normalized access envelope, targets, prerequisites and contributions.
    pub inspection: Value,
    /// Who installed or proposed it, from which artifact, and whether its contents are
    /// agent-authored (a proposal exists for them).
    pub provenance: Value,
    pub phase: VersionPhase,
    /// Wire names of the waits that keep a staged version from activating.
    pub waits: Vec<String>,
    /// The exact-version approval: `{"by", "kind", "atMs", "proposalId"?}` once a person approved
    /// these exact contents (or a host-recorded trusted update admitted them).
    pub approval: Option<Value>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// A package's desired and published state. `revision` is the package record's own revision
/// (configuration, enablement, trust and pointers all advance it).
#[derive(Clone, Debug, PartialEq)]
pub struct StoredPackage {
    pub package_id: String,
    pub revision: i64,
    /// Configured enablement (desired), independent of effective availability.
    pub enabled: bool,
    /// `false` once uninstalled; the record, state and history stay.
    pub installed: bool,
    /// Manifest digest of the active version (the registry pointer).
    pub active_digest: Option<String>,
    /// The immediate predecessor kept for rollback.
    pub predecessor_digest: Option<String>,
    /// The newest staged version waiting to activate.
    pub pending_digest: Option<String>,
    /// The active package state generation (0: none yet).
    pub state_generation: i64,
    /// `{"schema": SchemaRef, "value": ...}` validated against the active configuration schema.
    pub config: Option<Value>,
    pub config_revision: i64,
    /// The publisher trust receipt for automatic updates (`None`: not trusted).
    pub trust: Option<Value>,
    /// The last recorded failure: `{"code", "message", "atMs", "attempts", "digest"}`.
    pub failure: Option<Value>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// One exact-version approval proposal (EXT-02). Its decision is recorded once; a pending
/// proposal leaves the active version unchanged.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredPackageProposal {
    pub id: Id,
    pub revision: i64,
    pub package_id: String,
    pub manifest_digest: String,
    pub bundle_digest: String,
    pub version: String,
    /// `Pending`, `Approved`, `Rejected`, `Superseded` or `Expired` (the wire `ApprovalState`).
    pub state: String,
    pub base_package_id: Option<String>,
    pub originator: ActorId,
    /// The originator's principal kind and lineage: `{"kind", "sessionId"?, "projectId"?,
    /// "rootId"?, "assignmentId"?}`.
    pub origin: Value,
    /// Rationale, requested capabilities, dependencies, changes, verification evidence and the
    /// source artifact, as recorded for the reviewer.
    pub summary: Value,
    pub decided_by: Option<ActorId>,
    pub decided_at_ms: Option<i64>,
    pub decision: Option<Value>,
    pub created_at_ms: i64,
}

/// One retained package state generation (the host-owned package namespace).
#[derive(Clone, Debug, PartialEq)]
pub struct StoredStateGeneration {
    pub package_id: String,
    pub generation: i64,
    /// The manifest digest of the version this generation was published for.
    pub manifest_digest: String,
    /// SHA-256 of the state schema reference it is written against.
    pub schema_digest: String,
    /// The generation it was seeded from (its state was carried forward), if any.
    pub seeded_from: Option<i64>,
    /// The state revision when it was published (unchanged since means no writes since).
    pub seed_revision: i64,
    pub created_at_ms: i64,
}

/// The last fetch of one configured publisher index source.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredIndex {
    /// SHA-256 of the source's location and key (its identity).
    pub source_key: String,
    pub location: String,
    pub public_key: String,
    /// `Verified` or `Failed`.
    pub status: String,
    /// The verified index (`crate::ports::packages::VerifiedIndex` as JSON) of the last
    /// successful fetch; kept when a later fetch fails.
    pub index: Option<Value>,
    pub error: Option<String>,
    pub fetched_at_ms: i64,
}

/// The effective availability and active version last published for a package (so a change
/// is announced exactly once).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredObservation {
    pub package_id: String,
    pub availability: String,
    pub version: Option<String>,
    pub digest: Option<String>,
    pub revision: i64,
}

pub trait PackageRegistryRead {
    /// The registry head revision (1 for an empty registry).
    fn registry_revision(&self) -> Result<i64, StorageError>;
    fn package(&self, package_id: &str) -> Result<Option<StoredPackage>, StorageError>;
    /// Packages ordered by ID, strictly after `after`, at most `limit`.
    fn packages(&self, after: Option<&str>, limit: u16)
    -> Result<Vec<StoredPackage>, StorageError>;
    fn package_version(
        &self,
        package_id: &str,
        manifest_digest: &str,
    ) -> Result<Option<StoredPackageVersion>, StorageError>;
    /// The newest versions of a package (at most 512; the newest is always included), oldest
    /// first.
    fn package_versions(&self, package_id: &str)
    -> Result<Vec<StoredPackageVersion>, StorageError>;
    fn proposal(&self, id: &Id) -> Result<Option<StoredPackageProposal>, StorageError>;
    /// The newest proposal for exactly these contents, if any.
    fn proposal_for(
        &self,
        package_id: &str,
        manifest_digest: &str,
    ) -> Result<Option<StoredPackageProposal>, StorageError>;
    fn state_generation(
        &self,
        package_id: &str,
        generation: i64,
    ) -> Result<Option<StoredStateGeneration>, StorageError>;
    /// The retained state value of a generation (`None` when it holds no state yet).
    fn generation_state(
        &self,
        package_id: &str,
        generation: i64,
    ) -> Result<Option<(i64, Value)>, StorageError>;
    /// Every recorded index source (bounded by the repository).
    fn indexes(&self) -> Result<Vec<StoredIndex>, StorageError>;
    fn observation(&self, package_id: &str) -> Result<Option<StoredObservation>, StorageError>;
}

pub trait PackageRegistry: PackageRegistryRead {
    /// Compare-and-set of the registry head: `expected` must be current; returns the new head.
    fn advance_registry(&mut self, expected: i64) -> Result<i64, StorageError>;
    /// Inserts a new package record at revision 1 ([`StorageError::Duplicate`] if it exists).
    fn insert_package(&mut self, package: &StoredPackage) -> Result<StoredPackage, StorageError>;
    /// Compare-and-set update of a package record on `expected_revision`; the stored revision
    /// becomes `expected_revision + 1`.
    fn update_package(
        &mut self,
        package: &StoredPackage,
        expected_revision: i64,
    ) -> Result<StoredPackage, StorageError>;
    /// Inserts a verified version ([`StorageError::Duplicate`] for the same manifest digest).
    fn insert_version(&mut self, version: &StoredPackageVersion) -> Result<(), StorageError>;
    /// Moves a version's phase, waits and approval. Identity fields never change.
    fn update_version(
        &mut self,
        package_id: &str,
        manifest_digest: &str,
        phase: VersionPhase,
        waits: &[String],
        approval: Option<&Value>,
    ) -> Result<(), StorageError>;
    fn insert_proposal(&mut self, proposal: &StoredPackageProposal) -> Result<(), StorageError>;
    /// Records a proposal's state on `expected_revision` (the revision becomes one higher).
    fn update_proposal(
        &mut self,
        proposal: &StoredPackageProposal,
        expected_revision: i64,
    ) -> Result<StoredPackageProposal, StorageError>;
    /// Publishes a new state generation (strictly above every earlier one of the package) with
    /// `state` at revision `revision` (0: empty), and points the package handler state at it, so
    /// a handler pinned to an older generation is fenced (`broker.commit_event`).
    fn publish_generation(
        &mut self,
        generation: &StoredStateGeneration,
        state: Option<&Value>,
        revision: i64,
    ) -> Result<(), StorageError>;
    /// Records (inserts or replaces) an index source's last fetch.
    fn put_index(&mut self, index: &StoredIndex) -> Result<(), StorageError>;
    /// Forgets index sources that are no longer configured.
    fn retain_indexes(&mut self, source_keys: &[String]) -> Result<(), StorageError>;
    /// Records the published availability of a package (revision advances by one).
    fn put_observation(&mut self, observation: &StoredObservation) -> Result<(), StorageError>;
}
