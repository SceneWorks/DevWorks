//! Typed checkpoints for whole-workspace backup and offline migration.
use super::{Cancellation, StorageError};
use crate::fingerprint::is_sha256_hex;
use crate::protocol::{Id, StoreId};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const ARCHIVE_FORMAT: &str = "DevWorksWorkspaceBackup";
pub const ARCHIVE_REVISION: u16 = 1;
pub const DATABASE_MEMBER: &str = "workspace.sqlite";
pub const MANIFEST_MEMBER: &str = "manifest.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileDigest {
    pub sha256: String,
    pub bytes: u64,
}

impl FileDigest {
    fn valid(&self) -> bool {
        is_sha256_hex(&self.sha256)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationWitness {
    pub version: i64,
    pub checksum: String,
    pub minimum_reader: i64,
    pub minimum_writer: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BlobWitness {
    pub generation: Id,
    pub content: FileDigest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageFileWitness {
    pub path: String,
    pub content: FileDigest,
    pub executable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageWitness {
    pub package: Id,
    pub manifest_digest: String,
    pub files: Vec<PackageFileWitness>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceCheckpoint {
    pub store_id: StoreId,
    pub restore_epoch: i64,
    pub sequence: i64,
    pub captured_at_ms: i64,
    pub schema_version: i64,
    pub minimum_reader: i64,
    pub minimum_writer: i64,
    pub migrations: Vec<MigrationWitness>,
    pub schema_digest: String,
    pub blobs: Vec<BlobWitness>,
    pub packages: Vec<PackageWitness>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceArchive {
    pub format: String,
    pub revision: u16,
    pub operation_id: Id,
    pub app_build: String,
    pub checkpoint: WorkspaceCheckpoint,
    pub database: FileDigest,
}

fn ordered<T, K: Ord>(rows: &[T], key: impl Fn(&T) -> K) -> bool {
    rows.windows(2).all(|pair| key(&pair[0]) < key(&pair[1]))
}

fn relative_member(path: &str) -> bool {
    path.len() <= 4096
        && path.split('/').all(|name| {
            !name.is_empty()
                && name != "."
                && name != ".."
                && !name.ends_with([' ', '.'])
                && !name
                    .bytes()
                    .any(|b| matches!(b, b'\\' | b':' | b'\0') || b.is_ascii_control())
        })
}

impl WorkspaceArchive {
    pub fn validate(&self) -> Result<(), StorageError> {
        let checkpoint = &self.checkpoint;
        let migrations = &checkpoint.migrations;
        let valid_migrations = migrations.last().is_some_and(|last| {
            last.version == checkpoint.schema_version
                && last.minimum_reader == checkpoint.minimum_reader
                && last.minimum_writer == checkpoint.minimum_writer
        }) && migrations.iter().enumerate().all(|(index, migration)| {
            migration.version == index as i64 + 1
                && is_sha256_hex(&migration.checksum)
                && (1..=migration.version).contains(&migration.minimum_reader)
                && (1..=migration.version).contains(&migration.minimum_writer)
        });
        if self.format != ARCHIVE_FORMAT
            || self.revision != ARCHIVE_REVISION
            || self.app_build.is_empty()
            || self.app_build.len() > 128
            || self.app_build.chars().any(char::is_control)
            || !self.database.valid()
            || self.database.bytes == 0
            || checkpoint.restore_epoch < 0
            || checkpoint.sequence < 0
            || checkpoint.captured_at_ms < 0
            || !is_sha256_hex(&checkpoint.schema_digest)
            || !valid_migrations
        {
            return Err(StorageError::InvalidRecord(
                "invalid workspace backup manifest".into(),
            ));
        }
        if !ordered(&checkpoint.blobs, |blob| blob.content.sha256.clone())
            || checkpoint.blobs.iter().any(|blob| !blob.content.valid())
            || !ordered(&checkpoint.packages, |package| {
                (
                    package.package.as_str().to_owned(),
                    package.manifest_digest.clone(),
                )
            })
        {
            return Err(StorageError::InvalidRecord(
                "invalid workspace file inventory".into(),
            ));
        }
        for package in &checkpoint.packages {
            let mut case_folded = std::collections::BTreeSet::new();
            if !is_sha256_hex(&package.manifest_digest)
                || !ordered(&package.files, |file| file.path.clone())
                || package.files.iter().any(|file| {
                    !file.content.valid()
                        || !relative_member(&file.path)
                        || !case_folded.insert(file.path.to_lowercase())
                })
            {
                return Err(StorageError::InvalidRecord(
                    "invalid workspace package inventory".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Native storage owns each SQLite connection. Paths are private files already obtained from
/// verified host custody, never caller-supplied command text.
pub trait WorkspaceArchiveStorage: Send + Sync {
    /// Capture the exact reviewed committed sequence in a sealed, standalone SQLite image.
    /// No write owner transaction spans the copy.
    fn capture_checkpoint(
        &self,
        expected_sequence: i64,
        destination: &Path,
        cancellation: &Cancellation,
    ) -> Result<Option<WorkspaceCheckpoint>, StorageError>;

    /// Independently attest the copied database before it can enter any migration path.
    fn inspect_checkpoint(
        &self,
        source: &Path,
        captured_at_ms: i64,
    ) -> Result<WorkspaceCheckpoint, StorageError>;

    /// Upgrade a previously reviewed private checkpoint to a distinct image using only the
    /// shipped migration program. The source and the active store are left in place.
    fn migrate_checkpoint(
        &self,
        source: &Path,
        manifest: &WorkspaceArchive,
        destination: &Path,
        app_build: &str,
        cancellation: &Cancellation,
    ) -> Result<WorkspaceCheckpoint, StorageError>;

    fn active_checkpoint(&self) -> Result<WorkspaceCheckpoint, StorageError>;

    /// The checksums accepted at a distinct candidate's migration boundary come from this build.
    fn shipped_migrations(&self) -> Result<Vec<MigrationWitness>, StorageError>;
}

use crate::ports::workspace_archive::BackupFolder;
use crate::protocol::provider::ArtifactRef;
use crate::protocol::{ActorId, OperationId};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredBackupFolder {
    pub id: Id,
    pub owner: ActorId,
    pub store: StoreId,
    pub epoch: i64,
    pub folder: BackupFolder,
    pub expires_at_ms: i64,
    pub redeemed_by: Option<OperationId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchivePhase {
    Prepared,
    Queued,
    AwaitingRestart,
    Published,
    Migrated,
    Restored,
    Cancelled,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigratedWorkspaceImage {
    pub stage_id: Id,
    pub checkpoint: WorkspaceCheckpoint,
    pub database: FileDigest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum ArchiveAction {
    Backup {
        checkpoint: WorkspaceCheckpoint,
        destination: BackupFolder,
    },
    Restore {
        archive: WorkspaceArchive,
        migrated: Option<MigratedWorkspaceImage>,
    },
    VerifyMigration {
        checkpoint: WorkspaceCheckpoint,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredWorkspaceArchive {
    pub id: Id,
    pub stage_id: Id,
    pub owner: ActorId,
    pub store: StoreId,
    pub epoch: i64,
    pub created_at_ms: i64,
    pub expires_at_ms: Option<i64>,
    pub revision: i64,
    pub phase: ArchivePhase,
    pub action: ArchiveAction,
    pub operation: Option<OperationId>,
    pub manifest: Option<ArtifactRef>,
    pub migration_manifest: Option<ArtifactRef>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RestoredWorkspace {
    pub epoch: i64,
    pub owner: ActorId,
    pub proposal: Id,
    pub operation: OperationId,
    pub source: WorkspaceArchive,
    pub prior: WorkspaceArchive,
    pub unresolved_effects: i64,
    pub completion: Option<crate::protocol::results::OperationCompletion>,
}

pub trait WorkspaceArchiveRead {
    fn backup_folder(&self, id: &Id) -> Result<Option<StoredBackupFolder>, StorageError>;
    fn workspace_archive(&self, id: &Id) -> Result<Option<StoredWorkspaceArchive>, StorageError>;
    fn workspace_archive_for_operation(
        &self,
        operation: &OperationId,
    ) -> Result<Option<StoredWorkspaceArchive>, StorageError>;
    fn workspace_migration_plan(
        &self,
        artifact: &Id,
    ) -> Result<Option<StoredWorkspaceArchive>, StorageError>;
    fn latest_workspace_archive(
        &self,
        owner: &ActorId,
        epoch: i64,
    ) -> Result<Option<StoredWorkspaceArchive>, StorageError>;
    fn latest_workspace_restore(
        &self,
        epoch: i64,
    ) -> Result<Option<RestoredWorkspace>, StorageError>;
    fn restore_effect_held(
        &self,
        root: Option<&crate::protocol::RootId>,
        capability: Option<&str>,
    ) -> Result<bool, StorageError>;
    /// The exact accepted restore waiting for the native stopped-store boundary, if any.
    fn restore_awaiting_restart(&self) -> Result<Option<StoredWorkspaceArchive>, StorageError>;
}

pub trait WorkspaceArchiveRepository: WorkspaceArchiveRead {
    fn record_restored_completion(
        &mut self,
        epoch: i64,
        operation: &OperationId,
        completion: &crate::protocol::results::OperationCompletion,
    ) -> Result<(), StorageError>;
    fn register_backup_folder(&mut self, grant: &StoredBackupFolder) -> Result<(), StorageError>;
    fn redeem_backup_folder(
        &mut self,
        id: &Id,
        operation: &OperationId,
    ) -> Result<(), StorageError>;
    fn put_workspace_archive(
        &mut self,
        record: &StoredWorkspaceArchive,
        expected_revision: Option<i64>,
    ) -> Result<(), StorageError>;
}
