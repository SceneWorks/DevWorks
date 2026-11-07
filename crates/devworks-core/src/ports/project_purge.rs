//! Exact managed-file inventory and recovery for a reviewed project purge.
//! Inputs are native owner identities, never caller paths. The adapter constructs
//! managed locations and refuses unknown identities, links escaping a managed root,
//! changed contents, unsupported identity guarantees, or incomplete enumeration.
use crate::authority::destructive::FileIdentity;
use crate::protocol::{Id, ProjectId, StoreId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum PurgeFileOwner {
    Blob { digest: String, generation: String },
    ArtifactStaging { operation_id: Id },
    Quarantine { digest: String, generation: String },
    Preview { version_id: Id },
    BrowserProfile { session_id: Id },
    DiagnosticExport { file_id: Id },
    ClientPaste { file_id: Id },
    ClientStaging { file_id: Id },
}

/// Exact owner set materialized from the same reviewed native project snapshot.
#[derive(Clone, Debug)]
pub struct PurgeFileSelection {
    pub project_id: ProjectId,
    pub store_id: StoreId,
    pub restore_epoch: i64,
    pub owners: Vec<PurgeFileOwner>,
}

/// A native adapter-produced witness, stored only until the physical effect settles.
/// Relative entry names are internal recovery state, not accepted by any wire input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PurgeFileWitness {
    pub project_id: ProjectId,
    pub store_id: StoreId,
    pub restore_epoch: i64,
    pub owner: PurgeFileOwner,
    pub root: FileIdentity,
    pub relative_entry: String,
    pub identity: FileIdentity,
    pub byte_count: u64,
    pub content_digest: String,
    pub directory: bool,
    pub link: bool,
}

#[derive(Clone, Debug)]
pub struct PurgeFileSnapshot {
    pub selection: PurgeFileSelection,
    pub files: Vec<PurgeFileWitness>,
    pub digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PurgeFileFault {
    /// An owned holder is still using the selected file class.
    InUse,
    /// Preview/confirmation or recovery observed a different root, file or content.
    Changed,
    /// Attribution/enumeration cannot prove a complete project-owned set.
    Unattributed,
    Unsupported,
    Cancelled,
    Io,
}

/// The artifact service's actual reader registry, shared with its ordinary collector.
/// Physical removal holds the registry lock through the effect so a new reader cannot
/// enter between the last in-use check and unlink.
pub trait ProjectPurgeReadFence: Send + Sync {
    fn busy(&self, owner: &PurgeFileOwner) -> bool;
    fn remove_if_idle(
        &self,
        owner: &PurgeFileOwner,
        work: &mut dyn FnMut() -> Result<(), PurgeFileFault>,
    ) -> Result<(), PurgeFileFault>;
}

pub trait ProjectPurgeFiles: Send + Sync {
    /// Native path keepers are local client references, never deletion inputs.
    fn client_keepers(
        &self,
        _selection: &PurgeFileSelection,
        paths: &[std::path::PathBuf],
    ) -> Result<Vec<PurgeFileOwner>, PurgeFileFault> {
        if paths.is_empty() {
            Ok(vec![])
        } else {
            Err(PurgeFileFault::Unsupported)
        }
    }

    /// Enumerate every file/directory of exactly the supplied native owners. A class
    /// that cannot be completely attributed refuses the whole preview.
    fn inventory(
        &self,
        selection: &PurgeFileSelection,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<PurgeFileSnapshot, PurgeFileFault>;
    /// Recheck one exact witness and remove only that entry. Missing is success;
    /// replacements/new entries never inherit the old deletion authority. Directories
    /// are removed only when empty, after their reviewed children have settled.
    fn remove_reviewed(
        &self,
        witness: &PurgeFileWitness,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<(), PurgeFileFault>;
}

/// Host-produced client ownership witness. Inspecting it never restarts pending commands.
#[derive(Clone, Debug)]
pub struct PurgeClientSnapshot {
    pub digest: String,
    pub surviving_artifacts: Vec<crate::protocol::provider::ArtifactRef>,
    /// Exact original commands, local producers or their recovery outcomes remain unsettled.
    pub pending: bool,
    pub removing_drafts: u64,
    pub confirmation_ready: bool,
    pub surviving_paths: Vec<std::path::PathBuf>,
}

pub trait ProjectPurgeClient: Send + Sync {
    fn inspect(
        &self,
        scope: &crate::ports::storage::ProjectPurgeClientScope,
    ) -> Result<PurgeClientSnapshot, PurgeFileFault>;

    /// Execute the exact durable client intent after logical commit. Surviving scopes and
    /// original receipt fences remain intact; failure leaves the original intent held.
    fn remove_reviewed(
        &self,
        _scope: &crate::ports::storage::ProjectPurgeClientScope,
        _digest: &str,
    ) -> Result<(), PurgeFileFault> {
        Err(PurgeFileFault::Unsupported)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum PurgeEffectIntent {
    File {
        witness: PurgeFileWitness,
    },
    Client {
        scope: crate::ports::storage::ProjectPurgeClientScope,
        digest: String,
    },
}
/// Recovery checks this against the immutable reviewed entry, beyond the mutable
/// effect row's own checksum. A recovered body cannot select an unreviewed effect.
pub fn reviewed_effect_entry(intent: &PurgeEffectIntent) -> String {
    let bytes = match intent {
        PurgeEffectIntent::File { witness } => serde_json::to_vec(&(0usize, witness)),
        PurgeEffectIntent::Client { digest, .. } => serde_json::to_vec(&("client_state", digest)),
    }
    .expect("typed purge entry");
    crate::fingerprint::sha256_hex(&bytes)
}
#[derive(Clone, Debug)]
pub struct PurgeEffect {
    pub manifest_id: Id,
    pub id: String,
    pub intent: PurgeEffectIntent,
    pub lease_owner: Option<Id>,
}
