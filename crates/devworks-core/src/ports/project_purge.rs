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

pub trait ProjectPurgeFiles: Send + Sync {
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
