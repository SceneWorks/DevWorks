//! Durable analysis intent, exact source bindings and independently settled phase records.
use super::{GraphUnavailableRepository, GraphViewWitness, StorageError};
use crate::ports::AuthorityFence;
use crate::protocol::data::{DataScope, ViewSelection};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{AnalysisLinkMode, AnalysisRefreshMode, AnalysisViewBinding};
use crate::protocol::{ActorId, AssignmentId, Id, OperationId, OperationState, ProjectId, RootId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnalysisCommand {
    LinkAndDetect,
    Community,
    Reanalyze,
    ProcessBatch,
    Synthesize,
    IndexPublished,
}

impl AnalysisCommand {
    pub const fn key(self) -> &'static str {
        match self {
            Self::LinkAndDetect => "link_and_detect",
            Self::Community => "community_analysis",
            Self::Reanalyze => "reanalyze",
            Self::ProcessBatch => "process_batch",
            Self::Synthesize => "synthesize",
            Self::IndexPublished => "index_published",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnalysisPhase {
    Links,
    Communities,
    Vitals,
    Security,
    SettleBatch,
    Projects,
    Synthesis,
}

impl AnalysisPhase {
    pub const fn key(self) -> &'static str {
        match self {
            Self::Links => "links",
            Self::Communities => "communities",
            Self::Vitals => "vitals",
            Self::Security => "security",
            Self::SettleBatch => "settle_batch",
            Self::Projects => "projects",
            Self::Synthesis => "synthesis",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisOwner {
    pub actor: ActorId,
    pub kind: String,
    pub invocation: Id,
    pub ingress: Id,
    pub project: Option<ProjectId>,
    pub root: Option<RootId>,
    pub assignment: Option<AssignmentId>,
    pub session: Option<Id>,
    pub policy_revision: i64,
    pub restore_epoch: i64,
    pub namespace: String,
    pub activation: Option<(Id, i64)>,
    pub worktree: Option<AnalysisWorktreeOwner>,
    pub fences: Vec<AuthorityFence>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisWorktreeOwner {
    pub id: Id,
    pub repository: Id,
    pub work: Id,
    pub ownership_revision: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisIntent {
    pub command: AnalysisCommand,
    pub scope: DataScope,
    pub selection: ViewSelection,
    pub views: Vec<AnalysisViewBinding>,
    /// Each exact view's repository/ref lineage, in the same order as views.
    pub repository_lineages: Vec<String>,
    pub unavailable: Vec<GraphUnavailableRepository>,
    pub witnesses: Vec<GraphViewWitness>,
    pub options_digest: String,
    pub configuration_revision: i64,
    pub node_ids: Vec<Id>,
    pub batch_limit: u16,
    pub link_mode: Option<AnalysisLinkMode>,
    pub refresh_mode: Option<AnalysisRefreshMode>,
    pub include_all_source: bool,
    pub prerequisite_run: Option<Id>,
}

/// One attributed model request and its settled, checked structured output.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredAnalysisSubject {
    pub id: Id,
    pub key: Id,
    pub repository: Id,
    pub view: Id,
    pub request: Id,
    pub prompt_digest: String,
    pub binding: serde_json::Value,
    pub state: OperationState,
    pub output: Option<serde_json::Value>,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredAnalysisPhase {
    pub phase: AnalysisPhase,
    pub state: OperationState,
    pub report: Option<ArtifactRef>,
    pub operation: Option<OperationId>,
    pub diagnostic: Option<String>,
    pub finished_at_ms: Option<i64>,
    #[serde(default)]
    pub subjects: Vec<StoredAnalysisSubject>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredAnalysisRun {
    pub id: Id,
    /// The original admission, preserved across resumes.
    pub operation: OperationId,
    /// The current dispatcher work, which may be a fenced successor.
    pub active_operation: OperationId,
    pub pin_id: Id,
    pub owner: AnalysisOwner,
    pub intent: AnalysisIntent,
    /// A digest of the exact ordered repository/view bindings, scope, configuration and algorithm.
    pub binding_key: String,
    /// A digest of the scoped repository and ref/worktree lineage without its changing revision.
    pub lineage_key: String,
    pub state: OperationState,
    pub revision: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub phases: Vec<StoredAnalysisPhase>,
}

impl StoredAnalysisRun {
    pub fn terminal(&self) -> bool {
        matches!(
            self.state,
            OperationState::ObservedComplete
                | OperationState::Failed
                | OperationState::CancelledBeforeDispatch
        )
    }
}

/// Query positions are scoped to the checked binding or lineage before traversing a page.
#[derive(Clone, Debug)]
pub struct AnalysisRunPosition {
    pub created_at_ms: i64,
    pub id: Id,
}

pub trait AnalysisRead {
    fn analysis_retention_page(
        &self,
        after: Option<&AnalysisRunPosition>,
        limit: u16,
    ) -> Result<Vec<StoredAnalysisRun>, StorageError>;
    fn analysis_active_reference(&self, scope: &str, run: &Id) -> Result<bool, StorageError>;
    fn active_model_runs(&self, scope_key: &str) -> Result<usize, StorageError>;
    fn analysis_repository_runs(
        &self,
        scope: Option<&str>,
        repositories: &[Id],
        oldest_ms: i64,
        limit: u16,
    ) -> Result<Vec<StoredAnalysisRun>, StorageError>;
    fn analysis_run(&self, id: &Id) -> Result<Option<StoredAnalysisRun>, StorageError>;
    fn analysis_runs(
        &self,
        digest: &str,
        lineage: bool,
        after: Option<&AnalysisRunPosition>,
        limit: u16,
    ) -> Result<Vec<StoredAnalysisRun>, StorageError>;
}

pub trait AnalysisRepository: AnalysisRead {
    fn remove_terminal_analysis_run(
        &mut self,
        id: &Id,
        expected_revision: i64,
    ) -> Result<(), StorageError>;
    fn insert_analysis_run(&mut self, run: &StoredAnalysisRun) -> Result<(), StorageError>;
    /// Update only this run revision. The original request, owner and view identity are immutable.
    fn update_analysis_run(
        &mut self,
        run: &StoredAnalysisRun,
        expected_revision: i64,
    ) -> Result<(), StorageError>;
}
