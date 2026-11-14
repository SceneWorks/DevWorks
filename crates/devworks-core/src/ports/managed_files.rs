//! Filesystem side of host-created project files. Custody of those files (reservation, owner
//! scope check, publication and recovery of interrupted writes) belongs to
//! `services::projects::managed_files`; an implementation of these ports performs only the
//! filesystem I/O and holds no application store.
use crate::authority::destructive::FileIdentity;
use crate::ports::project_purge::{
    PurgeFileFault, PurgeFileOwner, PurgeFileSelection, PurgeFileWitness,
};
use crate::ports::storage::ProjectManagedFileRecovery;
use crate::protocol::ProjectId;
use std::path::PathBuf;

/// One native directory of client staging files (pasted images and editor text).
pub trait StagingDirectory: Send + Sync {
    /// Create the directory if needed and hold it for one write of `name`, a single path
    /// component. A name that is not a valid file name is refused here.
    fn open(&self, name: &str) -> Result<Box<dyn StagingWrite + '_>, String>;
    /// Remove the file an interrupted write left, only while the directory and the file are
    /// still the ones recorded for it. A missing file is success.
    fn remove_abandoned(
        &self,
        file: &ProjectManagedFileRecovery,
        identity: FileIdentity,
        binding: &PurgeFileSelection,
        owner: PurgeFileOwner,
    ) -> Result<(), String>;
}

/// One write held open on a staging directory.
pub trait StagingWrite {
    /// Identity of the held directory.
    fn root(&self) -> FileIdentity;
    /// Create the file new; an existing entry, including a link, is refused.
    fn create(&mut self) -> Result<(), String>;
    /// Identity of the created file.
    fn created(&self) -> Result<FileIdentity, String>;
    /// Write `bytes`, flush and close the created file, then flush the directory.
    fn write(&mut self, bytes: &[u8]) -> Result<(), String>;
    /// Witness the written file through the no-follow, identity-checked reader.
    fn witness(
        &self,
        binding: &PurgeFileSelection,
        owner: PurgeFileOwner,
    ) -> Result<PurgeFileWitness, PurgeFileFault>;
    /// The path handed to the client.
    fn path(&self) -> PathBuf;
}

/// Store-side facts a native purge file adapter checks its entries against. Implemented by
/// core over the application store.
pub trait PurgeFileRecords {
    /// The store and restore epoch native files of `project_id` are bound to now.
    fn binding(&self, project_id: ProjectId) -> PurgeFileSelection;
    /// The published witness of a registered transient owner (diagnostic export, client paste
    /// or client staging); `None` for other owners. A missing or differently bound
    /// registration is `Unattributed`.
    fn registered(
        &self,
        owner: &PurgeFileOwner,
        binding: &PurgeFileSelection,
    ) -> Result<Option<PurgeFileWitness>, PurgeFileFault>;
    /// Whether this witness is an original reviewed purge effect.
    fn effect_reviewed(&self, witness: &PurgeFileWitness) -> Result<bool, PurgeFileFault>;
}

/// Whether a project still accepts new host-issued file handles.
pub trait ProjectFileFence: Send + Sync {
    /// False once a purge fences the project, or when that cannot be read.
    fn open(&self, project: &ProjectId) -> bool;
}
