//! Durable settings, runs and source mappings owned by the selected work importer.
use super::StorageError;
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::ImportConfig;
use crate::protocol::{ActorId, Id, OperationState, ProjectId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkImportConfig {
    pub id: Id,
    pub owner: ActorId,
    pub revision: i64,
    pub source_namespace: Id,
    pub root_identity: Option<crate::authority::destructive::FileIdentity>,
    pub settings: ImportConfig,
    pub all_projects: bool,
    pub approved: bool,
    pub allow_scheduled_delta: bool,
    pub updated_at_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkImportMode {
    DryRun,
    Full,
    Delta,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkImportCounts {
    pub added: i64,
    pub changed: i64,
    pub unchanged: i64,
    pub skipped: i64,
    pub failures: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkImportRun {
    pub id: Id,
    pub owner: ActorId,
    pub revision: i64,
    pub source_namespace: Id,
    pub source_snapshot: String,
    pub captured_through_ms: i64,
    pub scope_digest: String,
    pub mode: WorkImportMode,
    pub requested_mode: WorkImportMode,
    pub updated_since_ms: Option<i64>,
    pub config_id: Option<Id>,
    pub config_revision: Option<i64>,
    pub preview_digest: String,
    pub binding_digest: String,
    pub root_identity: Option<crate::authority::destructive::FileIdentity>,
    pub state: OperationState,
    pub report: ArtifactRef,
    pub input: Option<ArtifactRef>,
    pub cursor: usize,
    pub imported_ids: Vec<Id>,
    pub retry_issues: Vec<WorkImportIssue>,
    pub issues: Vec<WorkImportIssue>,
    pub rows: i64,
    pub counts: WorkImportCounts,
    pub started_at_ms: i64,
    pub completed_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkImportIssue {
    pub source_id: Id,
    pub kind: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkImportMapping {
    pub source_namespace: Id,
    pub source_kind: String,
    pub source_id: Id,
    pub source_digest: String,
    pub observed_through_ms: i64,
    pub destination_id: Id,
    pub destination_revision: Option<i64>,
    pub project_id: Option<ProjectId>,
}

pub trait WorkImportRead {
    fn import_config(&self, id: &Id) -> Result<Option<WorkImportConfig>, StorageError>;
    fn import_settings(
        &self,
        owner: &ActorId,
        namespace: &Id,
    ) -> Result<Option<WorkImportConfig>, StorageError>;
    fn import_run(&self, id: &Id) -> Result<Option<WorkImportRun>, StorageError>;
    fn import_preview(
        &self,
        owner: &ActorId,
        digest: &str,
    ) -> Result<Option<WorkImportRun>, StorageError>;
    fn matching_preview(
        &self,
        owner: &ActorId,
        namespace: &Id,
        scope: &str,
        snapshot: &str,
    ) -> Result<Option<WorkImportRun>, StorageError>;
    fn import_runs(
        &self,
        owner: &ActorId,
        namespace: Option<&Id>,
        before: Option<i64>,
        limit: u32,
    ) -> Result<Vec<(i64, WorkImportRun)>, StorageError>;
    fn active_import(&self, namespace: &Id) -> Result<Option<WorkImportRun>, StorageError>;
    fn latest_import(
        &self,
        owner: &ActorId,
        namespace: &Id,
        scope_digest: &str,
        config_revision: Option<i64>,
    ) -> Result<Option<WorkImportRun>, StorageError>;
    fn latest_import_issues(
        &self,
        owner: &ActorId,
        namespace: &Id,
        scope_digest: &str,
        config_revision: Option<i64>,
    ) -> Result<Option<WorkImportRun>, StorageError>;
    fn import_mapping(
        &self,
        namespace: &Id,
        kind: &str,
        source_id: &Id,
    ) -> Result<Option<WorkImportMapping>, StorageError>;
}

pub trait WorkImportRepository: WorkImportRead {
    fn put_import_config(
        &mut self,
        record: &WorkImportConfig,
        expected_revision: Option<i64>,
    ) -> Result<(), StorageError>;
    fn put_import_run(
        &mut self,
        record: &WorkImportRun,
        expected_revision: Option<i64>,
    ) -> Result<(), StorageError>;
    fn put_import_mapping(&mut self, record: &WorkImportMapping) -> Result<(), StorageError>;
}
