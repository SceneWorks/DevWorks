//! Native GitHub facts and paced intake. Remote payloads are evidence, never authority.
//! External effect truth remains in OperationJournal; this facet stores exact source bindings.
use super::{OperationRecord, StorageError};
use crate::protocol::{Id, OperationId, ProjectId, RootId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubIssueKey {
    pub repository_id: Id,
    pub number: i64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredGithubAccount {
    pub id: Id,
    pub revision: i64,
    pub state: String,
    pub remote_user_id: Option<String>,
    pub login: Option<String>,
    pub credential_revision: i64,
    pub permission_revision: i64,
    pub granted_scopes: Vec<String>,
    pub observed_at_ms: i64,
    pub last_error: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredGithubRepository {
    pub id: Id,
    /// Absent until explicit registration/association; discovery itself creates no local checkout.
    pub repository_id: Option<Id>,
    pub account_id: Id,
    pub revision: i64,
    pub remote_repository_id: String,
    pub owner: String,
    pub name: String,
    pub owner_kind: String,
    pub url: String,
    pub default_branch: Option<String>,
    pub permissions: Value,
    pub permission_revision: i64,
    pub observed_at_ms: i64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredGithubObservation {
    pub id: Id,
    pub repository_id: Id,
    pub kind: String,
    pub subject_id: String,
    pub remote_revision: String,
    pub url: String,
    pub head: Option<String>,
    pub payload: Value,
    pub source_updated_at_ms: Option<i64>,
    pub observed_at_ms: i64,
}
/// The caller supplies actual authorized repositories. A project restriction also filters
/// registered membership before paging. Project-owned work links have a separate restriction.
#[derive(Clone, Debug, Default)]
pub struct GithubScope {
    pub repositories: Vec<Id>,
    pub project_id: Option<ProjectId>,
}
#[derive(Clone, Debug, Default)]
pub struct GithubObservationFilter {
    pub scope: GithubScope,
    pub kinds: Vec<String>,
    pub subject_id: Option<String>,
    pub head: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredGithubPollCursor {
    pub repository_id: Id,
    pub endpoint: String,
    pub revision: i64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub next_page: Option<String>,
    pub watermark: Option<Value>,
    pub bootstrap_complete: bool,
    pub next_poll_at_ms: i64,
    pub last_polled_at_ms: Option<i64>,
    pub failure_count: i64,
    pub last_error: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredGithubIssueIntake {
    pub key: GithubIssueKey,
    pub revision: i64,
    pub state: String,
    pub observation_id: Id,
    /// Exact immutable observation ID, never a substituted/missing ETag.
    pub remote_revision: String,
    pub project_id: Option<ProjectId>,
    pub work_id: Option<RootId>,
    pub operation_id: Option<OperationId>,
    pub last_triaged_revision: Option<String>,
}
#[derive(Clone, Debug, Default)]
pub struct GithubIntakeFilter {
    pub scope: GithubScope,
    pub states: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubIntakeClock {
    pub revision: i64,
    pub interval_seconds: i64,
    pub last_admitted_at_ms: Option<i64>,
    pub next_admission_at_ms: i64,
}
#[derive(Clone, Debug)]
pub struct GithubAdmission {
    pub issue: StoredGithubIssueIntake,
    pub clock: GithubIntakeClock,
    pub operation: OperationRecord,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubWorkLink {
    pub work_id: RootId,
    pub project_id: ProjectId,
}
/// Immutable accepted-completion binding. Draft/Sealed describe snapshot completeness only;
/// comment and close outcomes are read from the actual two journal operations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredGithubResolution {
    pub id: Id,
    pub key: GithubIssueKey,
    pub state: String,
    pub completion_digest: String,
    pub source_revision: String,
    pub obligation_revision: i64,
    pub summary_digest: String,
    pub comment_operation_id: OperationId,
    pub close_operation_id: OperationId,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredGithubResolutionWitness {
    pub resolution_id: Id,
    pub work_id: RootId,
    pub project_id: ProjectId,
    pub work_revision: i64,
    pub evidence: Value,
}
pub trait GithubRead {
    fn github_discovered_repository(
        &self,
        id: &Id,
    ) -> Result<Option<StoredGithubRepository>, StorageError>;
    /// Trusted account discovery view; public callers still require actual account/scope authority.
    fn github_account_repositories(
        &self,
        account: &Id,
        owner: Option<&str>,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredGithubRepository>, StorageError>;
    fn github_account(&self) -> Result<Option<StoredGithubAccount>, StorageError>;
    fn github_repository(
        &self,
        repository: &Id,
    ) -> Result<Option<StoredGithubRepository>, StorageError>;
    fn github_repositories(
        &self,
        scope: &GithubScope,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredGithubRepository>, StorageError>;
    fn github_observation(
        &self,
        id: &Id,
        scope: &GithubScope,
    ) -> Result<Option<StoredGithubObservation>, StorageError>;
    fn github_observations(
        &self,
        filter: &GithubObservationFilter,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredGithubObservation>, StorageError>;
    fn github_poll_cursor(
        &self,
        repository: &Id,
        endpoint: &str,
    ) -> Result<Option<StoredGithubPollCursor>, StorageError>;
    fn github_issue(
        &self,
        key: &GithubIssueKey,
        scope: &GithubScope,
    ) -> Result<Option<StoredGithubIssueIntake>, StorageError>;
    fn github_issues(
        &self,
        filter: &GithubIntakeFilter,
        after: Option<&GithubIssueKey>,
        limit: u16,
    ) -> Result<Vec<StoredGithubIssueIntake>, StorageError>;
    /// Implicit initial revision one, interval1800, no accumulated credits.
    fn github_intake_clock(&self) -> Result<GithubIntakeClock, StorageError>;
    fn github_obligation_revision(&self, key: &GithubIssueKey) -> Result<i64, StorageError>;
    fn github_work_links(
        &self,
        key: &GithubIssueKey,
        project: Option<&ProjectId>,
        after: Option<&RootId>,
        limit: u16,
    ) -> Result<Vec<GithubWorkLink>, StorageError>;
    fn github_resolution(&self, id: &Id) -> Result<Option<StoredGithubResolution>, StorageError>;
    fn github_resolutions(
        &self,
        key: &GithubIssueKey,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredGithubResolution>, StorageError>;
    fn github_resolution_witnesses(
        &self,
        resolution: &Id,
        project: Option<&ProjectId>,
        after: Option<&RootId>,
        limit: u16,
    ) -> Result<Vec<StoredGithubResolutionWitness>, StorageError>;
}
pub trait GithubRepository: GithubRead {
    fn put_github_account(
        &mut self,
        row: &StoredGithubAccount,
        expected: Option<i64>,
    ) -> Result<(), StorageError>;
    fn put_github_repository(
        &mut self,
        row: &StoredGithubRepository,
        expected: Option<i64>,
    ) -> Result<(), StorageError>;
    /// Idempotent immutable source facts; duplicate polls return the original winning observation.
    fn append_github_observation(
        &mut self,
        row: &StoredGithubObservation,
    ) -> Result<StoredGithubObservation, StorageError>;
    fn put_github_poll_cursor(
        &mut self,
        row: &StoredGithubPollCursor,
        expected: Option<i64>,
    ) -> Result<(), StorageError>;
    fn put_github_issue(
        &mut self,
        row: &StoredGithubIssueIntake,
        expected: Option<i64>,
    ) -> Result<(), StorageError>;
    fn set_github_intake_interval(
        &mut self,
        expected: i64,
        seconds: i64,
    ) -> Result<GithubIntakeClock, StorageError>;
    /// Atomically binds one real github.triage intent (payload repositoryId/issueNumber/sourceRevision)
    /// and advances the global clock from NOW. Lost-ack replay of that same operation spends no slot.
    fn admit_github_issue(
        &mut self,
        key: &GithubIssueKey,
        expected_issue: i64,
        expected_clock: i64,
        operation: &OperationId,
    ) -> Result<Option<GithubAdmission>, StorageError>;
    fn link_github_work(
        &mut self,
        key: &GithubIssueKey,
        work: &RootId,
        expected_work_revision: i64,
    ) -> Result<(), StorageError>;
    fn insert_github_resolution(
        &mut self,
        row: &StoredGithubResolution,
    ) -> Result<StoredGithubResolution, StorageError>;
    fn insert_github_resolution_witness(
        &mut self,
        row: &StoredGithubResolutionWitness,
    ) -> Result<(), StorageError>;
    fn seal_github_resolution(
        &mut self,
        id: &Id,
        expected_witness_count: i64,
    ) -> Result<StoredGithubResolution, StorageError>;
}
