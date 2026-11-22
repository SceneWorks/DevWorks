//! Verified file custody for a manually initiated workspace backup and restore.
use crate::authority::destructive::FileIdentity;
use crate::ports::storage::{
    Cancellation, FileDigest, StorageError, WorkspaceArchive, WorkspaceCheckpoint,
};
use crate::protocol::Id;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Captured by the trusted native chooser; never accepted from command input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupFolder {
    pub path: PathBuf,
    pub identity: FileIdentity,
}

pub trait WorkspaceArchiveFiles: Send + Sync {
    fn bind_external_folder(&self, path: &Path) -> Result<BackupFolder, StorageError>;

    /// One exclusively created private stage, whose database has not been written yet.
    fn create_stage(&self, id: &Id) -> Result<PathBuf, StorageError>;
    /// Remove private, unreferenced preparation for this app-issued stage identity.
    fn discard_stage(&self, id: &Id) -> Result<(), StorageError>;
    fn staged_database(&self, id: &Id) -> Result<PathBuf, StorageError>;

    /// Freeze the exact captured package files while the owning package service holds collection.
    fn stage_packages(
        &self,
        id: &Id,
        checkpoint: &WorkspaceCheckpoint,
        cancellation: &Cancellation,
    ) -> Result<(), StorageError>;

    /// Freeze the exact artifact generations after the backup's durable blob pins are admitted.
    fn stage_blobs(
        &self,
        id: &Id,
        checkpoint: &WorkspaceCheckpoint,
        cancellation: &Cancellation,
    ) -> Result<(), StorageError>;

    fn seal_stage(
        &self,
        id: &Id,
        checkpoint: WorkspaceCheckpoint,
        app_build: &str,
        cancellation: &Cancellation,
    ) -> Result<WorkspaceArchive, StorageError>;

    /// Verify and seal transfer input under native custody. No SQLite connection opens here.
    fn stage_external_archive(
        &self,
        folder: &BackupFolder,
        id: &Id,
        cancellation: &Cancellation,
    ) -> Result<WorkspaceArchive, StorageError>;

    fn read_stage(&self, id: &Id) -> Result<WorkspaceArchive, StorageError>;
    fn staged_database_digest(
        &self,
        id: &Id,
        cancellation: &Cancellation,
    ) -> Result<FileDigest, StorageError>;

    /// Positive only when the complete original bundle is sealed and every member matches.
    fn observe_publication(
        &self,
        destination: &BackupFolder,
        id: &Id,
        manifest: &WorkspaceArchive,
        cancellation: &Cancellation,
    ) -> Result<bool, StorageError>;

    /// Publish only into a freshly created operation directory. An existing completed identical
    /// publication is an observation. An existing incomplete or different one stays untouched.
    fn publish(
        &self,
        destination: &BackupFolder,
        id: &Id,
        manifest: &WorkspaceArchive,
        cancellation: &Cancellation,
        permit: &mut dyn FnMut() -> Result<(), StorageError>,
    ) -> Result<(), StorageError>;
}
