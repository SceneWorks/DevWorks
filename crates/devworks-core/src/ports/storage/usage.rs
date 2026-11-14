//! Canonical inference accounting and immutable price provenance (DW-21). An attempt retains
//! host-resolved ownership at admission; later association edits never rewrite incurred usage.
use super::{SequenceKey, StorageError};
use crate::ports::usage::PublishedTariff;
use crate::protocol::{ActorId, AssignmentId, Id, OperationId, ProjectId, RootId};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct StoredInferenceAttempt {
    pub attempt_id: Id,
    pub request_id: Id,
    pub operation_id: OperationId,
    pub attempt_number: i64,
    pub project_id: Option<ProjectId>,
    pub feature_id: Option<Id>,
    pub work_id: Option<Id>,
    pub assignment_id: Option<AssignmentId>,
    /// Positive managed-execution ownership captured at admission, never inferred from membership.
    pub execution_root_id: Option<RootId>,
    pub execution_id: Option<Id>,
    pub session_id: Option<Id>,
    pub job_run_id: Option<Id>,
    pub actor_id: ActorId,
    pub role: String,
    pub purpose: String,
    pub catalog_model_id: Id,
    pub connection_id: Id,
    pub endpoint_profile: String,
    pub access_mode: String,
    pub provider_model_id: Option<String>,
    pub pricing_snapshot: Value,
    /// Admitted, Dispatched, Settled, UnknownTail or CancelledBeforeDispatch.
    pub state: String,
    pub revision: i64,
    pub created_at_ms: i64,
}
#[derive(Clone, Debug, PartialEq)]
pub struct StoredUsageObservation {
    pub attempt_id: Id,
    pub observation_key: String,
    pub sequence: i64,
    pub digest: String,
    pub payload: Value,
    pub recorded_at_ms: i64,
}
#[derive(Clone, Debug, PartialEq)]
pub struct StoredAttemptBalance {
    pub attempt_id: Id,
    pub revision: i64,
    pub priced_micro_usd: Option<i64>,
    pub reserved_micro_usd: Option<i64>,
    pub classification: String,
    pub monetary_complete: bool,
    pub incomplete: bool,
    pub unknown_tail: bool,
    /// Host transport closed/reconciled; independent of remote completion or monetary coverage.
    pub local_quiescent: bool,
    /// Core-owned merged metrics, field provenance and cumulative/delta semantics.
    pub summary: Value,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredFeatureBudget {
    pub feature_id: Id,
    pub revision: i64,
    pub limit_micro_usd: Option<i64>,
    pub warning_revision: Option<i64>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoredFeatureUsageTotals {
    pub attempts: i64,
    pub spent_micro_usd: i64,
    pub reserved_micro_usd: i64,
    pub reported_micro_usd: i64,
    pub estimated_micro_usd: i64,
    pub local_free_attempts: i64,
    pub untracked_attempts: i64,
    pub monetary_incomplete_attempts: i64,
    pub unknown_tail_attempts: i64,
    pub incomplete_attempts: i64,
    /// Legacy facts have no proven attempt identity. Their subtotal is not incurred spending.
    pub legacy_usage_facts: i64,
    pub legacy_untracked_facts: i64,
    pub legacy_observation_micro_usd: i64,
}
#[derive(Clone, Debug, PartialEq)]
pub struct StoredUsageChange {
    pub event_id: Id,
    pub attempt_id: Option<Id>,
    pub feature_id: Option<Id>,
    pub kind: String,
    pub delta_micro_usd: Option<i64>,
    pub payload: Value,
    pub created_at_ms: i64,
}
#[derive(Clone, Debug, PartialEq)]
pub struct StoredUsageOrigin {
    pub source_kind: String,
    pub source_id: String,
    pub attempt_id: Option<Id>,
    pub request_id: Option<Id>,
    pub project_id: Option<ProjectId>,
    pub feature_id: Option<Id>,
    pub work_id: Option<Id>,
    pub session_id: Option<Id>,
    pub job_run_id: Option<Id>,
    pub payload: Value,
    pub recorded_at_ms: i64,
    pub priced_micro_usd: Option<i64>,
    pub classification: String,
    pub monetary_complete: bool,
    pub incomplete: bool,
    pub unknown_tail: bool,
    /// Committed sequence of the transcript batch that recorded a `SessionUsage` item (the
    /// order session usage is paged in); `None` for every other kind.
    pub committed_sequence: Option<i64>,
}
impl StoredUsageOrigin {
    /// Opaque tuple key for exclusive source-kind/source-ID pagination.
    pub fn cursor(&self) -> String {
        serde_json::json!([self.source_kind, self.source_id]).to_string()
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageAttemptFilter {
    pub project_id: Option<ProjectId>,
    pub feature_id: Option<Id>,
    pub work_id: Option<Id>,
    pub assignment_id: Option<AssignmentId>,
    pub session_id: Option<Id>,
    pub job_run_id: Option<Id>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub actor_id: Option<ActorId>,
    pub model_id: Option<Id>,
    pub connection_id: Option<Id>,
    pub purpose: Option<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageOriginFilter {
    pub project_id: Option<ProjectId>,
    pub feature_id: Option<Id>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub kind: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageJobPosition {
    pub run_id: Id,
    pub seq: i64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredUsageBootstrap {
    pub revision: i64,
    pub phase: String,
    pub session_after: Option<SequenceKey>,
    pub job_after: Option<UsageJobPosition>,
    pub router_after: Option<Id>,
    pub source_sequence: i64,
    pub processed: i64,
    pub completed_at_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredPriceSnapshot {
    pub id: Id,
    pub provider: String,
    pub revision: i64,
    pub source_url: String,
    pub source_digest: String,
    pub parser_version: String,
    pub fetched_at_ms: i64,
    pub verified_at_ms: i64,
    pub effective_date: Option<String>,
    pub tariffs: Vec<PublishedTariff>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredPriceStatus {
    pub provider: String,
    pub revision: i64,
    pub current_snapshot_id: Option<Id>,
    pub last_attempt_ms: Option<i64>,
    pub last_success_ms: Option<i64>,
    pub refresh_pending: bool,
    /// Most recent owning refresh journal operation, retained after terminal publication/failure.
    pub refresh_operation_id: Option<OperationId>,
    pub last_error: Option<String>,
    pub next_refresh_ms: Option<i64>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredPriceOverride {
    pub model_id: Id,
    pub revision: i64,
    pub mode: String,
    pub tariffs: Vec<PublishedTariff>,
    pub updated_by: ActorId,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceOverridePosition {
    pub model_id: Id,
    pub revision: i64,
}

pub trait UsageRead {
    fn inference_attempt(&self, id: &Id) -> Result<Option<StoredInferenceAttempt>, StorageError>;
    /// ID-ordered, exclusive cursor, limit 1..=MAX_PAGE. Time range is [start_ms,end_ms).
    fn inference_attempts(
        &self,
        filter: &UsageAttemptFilter,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredInferenceAttempt>, StorageError>;
    /// Attempts created in `[filter.start_ms, filter.end_ms)` (both required), in exclusive
    /// `(created_at_ms, attempt_id)` order, through a time index: attempts outside the range
    /// are never visited.
    fn inference_attempts_between(
        &self,
        filter: &UsageAttemptFilter,
        after: Option<&(i64, Id)>,
        limit: u16,
    ) -> Result<Vec<StoredInferenceAttempt>, StorageError>;
    /// Attempts created at or after `filter.end_ms` whose logical request was created in
    /// `[filter.start_ms, filter.end_ms)`, each with its request's creation time, in exclusive
    /// `(request created_at_ms, request_id, attempt_id)` order. That order follows the request
    /// time index, so each page starts after the last request it visited.
    fn late_inference_attempts(
        &self,
        filter: &UsageAttemptFilter,
        after: Option<&(i64, Id, Id)>,
        limit: u16,
    ) -> Result<Vec<(i64, StoredInferenceAttempt)>, StorageError>;
    /// Origins recorded in `[filter.start_ms, filter.end_ms)` (both required), in exclusive
    /// `(recorded_at_ms, source_kind, source_id)` order, through a time index.
    fn usage_origins_between(
        &self,
        filter: &UsageOriginFilter,
        after: Option<&(i64, String, String)>,
        limit: u16,
    ) -> Result<Vec<StoredUsageOrigin>, StorageError>;
    /// Attempts whose balance is not yet locally quiescent, in exclusive ID order. Settled
    /// attempts are never visited, so the cost follows open custody rather than history.
    fn unquiesced_attempts(
        &self,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredInferenceAttempt>, StorageError>;
    fn usage_observation(
        &self,
        attempt: &Id,
        key: &str,
    ) -> Result<Option<StoredUsageObservation>, StorageError>;
    fn usage_observations(
        &self,
        attempt: &Id,
        after_sequence: Option<i64>,
        limit: u16,
    ) -> Result<Vec<StoredUsageObservation>, StorageError>;
    fn attempt_balance(&self, attempt: &Id) -> Result<Option<StoredAttemptBalance>, StorageError>;
    fn feature_budget(&self, feature: &Id) -> Result<Option<StoredFeatureBudget>, StorageError>;
    fn feature_usage_totals(&self, feature: &Id) -> Result<StoredFeatureUsageTotals, StorageError>;
    fn usage_origins(
        &self,
        filter: &UsageOriginFilter,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<StoredUsageOrigin>, StorageError>;
    fn usage_origin(&self, kind: &str, id: &str)
    -> Result<Option<StoredUsageOrigin>, StorageError>;
    fn usage_changes(
        &self,
        feature: Option<&Id>,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredUsageChange>, StorageError>;
    /// Durable bounded bootstrap starts at revision1/Sessions, never fabricates attempts.
    fn usage_bootstrap(&self) -> Result<StoredUsageBootstrap, StorageError>;
    /// Workspace generation has authoritative implicit initial revision1.
    fn price_revision(&self) -> Result<i64, StorageError>;
    /// Immutable publication history in exclusive ID order, optionally one provider.
    fn price_snapshots(
        &self,
        provider: Option<&str>,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredPriceSnapshot>, StorageError>;
    fn price_snapshot(&self, id: &Id) -> Result<Option<StoredPriceSnapshot>, StorageError>;
    fn current_price_snapshot(
        &self,
        provider: &str,
    ) -> Result<Option<StoredPriceSnapshot>, StorageError>;
    fn price_status(&self, provider: &str) -> Result<Option<StoredPriceStatus>, StorageError>;
    /// Retained Set/Clear revisions in exclusive (model_id,revision) order, limit1..=MAX_PAGE.
    fn price_override_history(
        &self,
        model: Option<&Id>,
        after: Option<&PriceOverridePosition>,
        limit: u16,
    ) -> Result<Vec<StoredPriceOverride>, StorageError>;
    fn price_override(&self, model: &Id) -> Result<Option<StoredPriceOverride>, StorageError>;
    /// Registered models have implicit override revision1 before any Set/Clear record.
    fn price_override_revision(&self, model: &Id) -> Result<i64, StorageError>;
}
pub trait UsageRepository: UsageRead {
    fn insert_inference_attempt(
        &mut self,
        attempt: &StoredInferenceAttempt,
    ) -> Result<(), StorageError>;
    fn update_inference_attempt(
        &mut self,
        attempt: &StoredInferenceAttempt,
        expected: i64,
    ) -> Result<(), StorageError>;
    fn insert_usage_observation(
        &mut self,
        observation: &StoredUsageObservation,
    ) -> Result<(), StorageError>;
    fn put_attempt_balance(
        &mut self,
        balance: &StoredAttemptBalance,
        expected: Option<i64>,
    ) -> Result<(), StorageError>;
    fn put_feature_budget(
        &mut self,
        budget: &StoredFeatureBudget,
        expected: Option<i64>,
    ) -> Result<(), StorageError>;
    fn insert_usage_origin(&mut self, origin: &StoredUsageOrigin) -> Result<(), StorageError>;
    fn bind_usage_origin(&mut self, kind: &str, id: &str, attempt: &Id)
    -> Result<(), StorageError>;
    fn append_usage_change(&mut self, change: &StoredUsageChange) -> Result<(), StorageError>;
    fn update_usage_bootstrap(
        &mut self,
        bootstrap: &StoredUsageBootstrap,
        expected: i64,
    ) -> Result<(), StorageError>;
    fn advance_price_revision(&mut self, expected: i64) -> Result<i64, StorageError>;
    /// A successful status/publication/override mutation also advances the workspace generation.
    fn put_price_status(
        &mut self,
        status: &StoredPriceStatus,
        expected: Option<i64>,
    ) -> Result<(), StorageError>;
    fn publish_price_snapshot(
        &mut self,
        snapshot: &StoredPriceSnapshot,
        expected_status_revision: i64,
    ) -> Result<StoredPriceStatus, StorageError>;
    /// First Set/Clear uses expected1 and revision2. Every stored revision is retained.
    fn put_price_override(
        &mut self,
        override_: &StoredPriceOverride,
        expected: i64,
    ) -> Result<(), StorageError>;
}
