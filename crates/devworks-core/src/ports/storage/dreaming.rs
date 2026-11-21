//! Durable state of a Dreaming sweep. Knowledge, provider usage and operation receipts keep
//! their existing owners. The sweep records the exact identities it can recover from them.
use super::{KnowledgeScope, StorageError};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::EffortName;
use crate::protocol::{ActorId, AttemptId, Id, OperationId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DreamPolicy {
    pub enabled: bool,
    /// The exact route catalog model, or the shared router's configured Dreaming route.
    pub model_id: Option<Id>,
    pub effort: Option<EffortName>,
    pub schedule_id: Option<Id>,
    pub minimum_interval_ms: u64,
    pub max_turns: u16,
    pub max_tokens: u32,
    pub max_elapsed_ms: u32,
    pub max_micro_usd: Option<u64>,
    pub batch_limit: u16,
}

impl Default for DreamPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            model_id: None,
            effort: None,
            schedule_id: None,
            minimum_interval_ms: 3_600_000,
            max_turns: 12,
            max_tokens: 128_000,
            max_elapsed_ms: 1_200_000,
            max_micro_usd: None,
            batch_limit: 20,
        }
    }
}

impl DreamPolicy {
    pub fn validate(&self) -> Result<(), StorageError> {
        if !(1..=100).contains(&self.max_turns)
            || !(1..=1_000_000).contains(&self.max_tokens)
            || !(1_000..=3_600_000).contains(&self.max_elapsed_ms)
            || self.minimum_interval_ms > 31_536_000_000
            || !(1..=500).contains(&self.batch_limit)
            || self.max_micro_usd.is_some_and(|cap| cap > i64::MAX as u64)
        {
            return Err(StorageError::InvalidRecord(
                "invalid Dreaming policy bounds".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DreamCutoffs {
    pub knowledge: i64,
    pub reviews: i64,
    pub sources: i64,
    pub workspace_knowledge: i64,
    pub workspace_sources: i64,
    pub base_instruction_revision: i64,
    pub project_instruction_revision: i64,
}

#[derive(Clone, Copy, Debug)]
pub enum DreamStream {
    Knowledge,
    Reviews,
    UnlearnedReviews,
    Sources,
}

#[derive(Clone, Debug)]
pub struct DreamSignal {
    pub sequence: i64,
    pub reference: String,
    pub observed_at_ms: i64,
    /// For source observations, the exact observed source identity (not a read capability).
    pub source_revision: Option<String>,
}

impl DreamCutoffs {
    pub fn does_not_precede(&self, prior: &Self) -> bool {
        [
            (self.knowledge, prior.knowledge),
            (self.reviews, prior.reviews),
            (self.sources, prior.sources),
            (self.workspace_knowledge, prior.workspace_knowledge),
            (self.workspace_sources, prior.workspace_sources),
            (
                self.base_instruction_revision,
                prior.base_instruction_revision,
            ),
            (
                self.project_instruction_revision,
                prior.project_instruction_revision,
            ),
        ]
        .into_iter()
        .all(|(actual, minimum)| actual >= minimum)
    }
}

#[derive(Clone, Debug)]
pub struct DreamScopeState {
    pub revision: i64,
    pub policy: DreamPolicy,
    pub completed: Option<(DreamCutoffs, i64)>,
}

impl Default for DreamScopeState {
    fn default() -> Self {
        Self {
            revision: 1,
            policy: DreamPolicy::default(),
            completed: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DreamInstruction {
    pub revision: i64,
    pub editor: ActorId,
    pub created_at_ms: i64,
    pub artifact: ArtifactRef,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DreamTrigger {
    Manual,
    Scheduled,
    Resume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DreamPhase {
    Queued,
    Running,
    Disabled,
    Busy,
    TooSoon,
    NoSignal,
    Blocked,
    BudgetExhausted,
    Partial,
    Failed,
    Cancelled,
    OutcomeUnknown,
    ScheduledFanout,
    Completed,
}

impl DreamPhase {
    pub fn owns_scope(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::OutcomeUnknown)
    }

    pub fn ended(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }

    pub fn resumable(self) -> bool {
        matches!(
            self,
            Self::Blocked | Self::BudgetExhausted | Self::Partial | Self::Cancelled
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub enum DreamRequestStage {
    Prepared,
    /// Committed before network admission. After restart only the original journal/ledger can
    /// prove whether a send may be repeated or a response was already observed.
    PossiblySent,
    Observed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DreamRequest {
    pub logical_id: Id,
    /// The accounting owner allocates it. It must be attached before transport admission.
    pub attempt_id: Option<AttemptId>,
    pub frame: ArtifactRef,
    pub stage: DreamRequestStage,
    pub result: Option<ArtifactRef>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DreamCheckpoint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_key: Option<[u8; 32]>,
    pub turn: u16,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub priced_micro_usd: u64,
    #[serde(default)]
    pub usage_uncertain: bool,
    pub request: Option<DreamRequest>,
    /// Immutable transcript/window and pending action identities published as one artifact.
    pub continuation: Option<ArtifactRef>,
}

#[derive(Clone, Debug)]
pub struct DreamRunRow {
    pub id: Id,
    pub scope: KnowledgeScope,
    pub continued_from: Option<Id>,
    pub operation_id: Option<OperationId>,
    pub initiator: ActorId,
    pub trigger: DreamTrigger,
    pub revision: i64,
    pub generation: i64,
    pub phase: DreamPhase,
    pub settings_revision: i64,
    pub policy: DreamPolicy,
    pub cutoffs: DreamCutoffs,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub checkpoint: Option<DreamCheckpoint>,
    pub report: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct DreamProgress {
    pub phase: DreamPhase,
    pub checkpoint: Option<DreamCheckpoint>,
    pub report: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct DreamActionRow {
    pub id: Id,
    pub position: i64,
    pub committed_at_ms: i64,
    /// The frozen operation, evidence, reply and guarded inverse metadata.
    pub action: Value,
}

pub struct DreamClaimWindow<'a> {
    pub scope: &'a KnowledgeScope,
    pub through: i64,
    pub after: Option<&'a Id>,
    pub exact: Option<&'a Id>,
    pub kind: Option<&'a str>,
    pub status: Option<&'a str>,
    pub possibly_stale: Option<bool>,
    pub lessons: Option<&'a str>,
    pub delivered_since_ms: i64,
    pub captured_at_ms: i64,
    pub limit: u16,
}

#[derive(Clone, Debug)]
pub struct DreamClaimRef {
    pub id: Id,
    pub revision: i64,
    pub current_revision: i64,
    pub lifecycle: String,
    pub entry_kind: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DreamTrailPosition {
    pub at_ms: i64,
    pub id: String,
    pub kind: String,
}

pub struct DreamTrailQuery<'a> {
    pub scope: &'a KnowledgeScope,
    pub through_ms: i64,
    pub knowledge_through: i64,
    pub reviews_through: i64,
    pub since_ms: Option<i64>,
    pub kind: Option<&'a str>,
    pub id: Option<&'a str>,
    pub item_id: Option<&'a str>,
    pub before: Option<&'a DreamTrailPosition>,
    pub offset: u64,
    pub limit: u16,
}

#[derive(Clone, Debug)]
pub struct DreamTrailRow {
    pub position: DreamTrailPosition,
    pub item_id: Option<String>,
    pub content: Value,
}

pub trait DreamingRead {
    fn dream_trail(&self, query: &DreamTrailQuery<'_>) -> Result<Vec<DreamTrailRow>, StorageError>;
    fn dream_scope(&self, scope: &KnowledgeScope) -> Result<DreamScopeState, StorageError>;
    fn dream_cutoffs(&self, scope: &KnowledgeScope) -> Result<DreamCutoffs, StorageError>;
    fn dream_signals(
        &self,
        scope: &KnowledgeScope,
        stream: DreamStream,
        after: i64,
        through: i64,
        limit: u16,
    ) -> Result<Vec<DreamSignal>, StorageError>;
    fn dream_claim_refs(
        &self,
        window: &DreamClaimWindow<'_>,
    ) -> Result<Vec<DreamClaimRef>, StorageError>;
    fn dream_prior_proposal(
        &self,
        scope: &KnowledgeScope,
        target: &Id,
        action: &str,
    ) -> Result<Option<Id>, StorageError>;
    fn dream_lesson_recurred_while_served(&self, claim: &Id) -> Result<bool, StorageError>;
    fn dream_instructions(
        &self,
        scope: &KnowledgeScope,
        before_revision: Option<i64>,
        limit: u16,
    ) -> Result<Vec<DreamInstruction>, StorageError>;
    fn dream_run(&self, id: &Id) -> Result<Option<DreamRunRow>, StorageError>;
    fn dream_live_run(&self, scope: &KnowledgeScope) -> Result<Option<DreamRunRow>, StorageError>;
    fn dream_resumable_run(
        &self,
        scope: &KnowledgeScope,
    ) -> Result<Option<DreamRunRow>, StorageError>;
    fn dream_continuation_used(&self, run: &Id) -> Result<bool, StorageError>;
    fn dream_runs(
        &self,
        scope: &KnowledgeScope,
        before: Option<(i64, &Id)>,
        limit: u16,
    ) -> Result<Vec<DreamRunRow>, StorageError>;
    fn dream_action(&self, run: &Id, action: &Id) -> Result<Option<DreamActionRow>, StorageError>;
    fn dream_reversion(&self, run: &Id, action: &Id) -> Result<Option<Value>, StorageError>;
    fn dream_actions(
        &self,
        run: &Id,
        before: Option<i64>,
        limit: u16,
    ) -> Result<Vec<DreamActionRow>, StorageError>;
}

pub trait DreamingRepository: DreamingRead {
    fn set_dream_policy(
        &mut self,
        scope: &KnowledgeScope,
        expected: i64,
        policy: &DreamPolicy,
    ) -> Result<DreamScopeState, StorageError>;
    fn add_dream_instruction(
        &mut self,
        scope: &KnowledgeScope,
        expected: i64,
        artifact: &ArtifactRef,
        editor: &ActorId,
    ) -> Result<DreamInstruction, StorageError>;
    fn insert_dream_run(&mut self, run: &DreamRunRow) -> Result<(), StorageError>;
    fn advance_dream_run(
        &mut self,
        run: &Id,
        expected_revision: i64,
        generation: i64,
        progress: &DreamProgress,
    ) -> Result<DreamRunRow, StorageError>;
    fn put_dream_action(
        &mut self,
        run: &Id,
        generation: i64,
        action: &Id,
        outcome: &Value,
    ) -> Result<DreamActionRow, StorageError>;
    fn put_dream_reversion(
        &mut self,
        run: &Id,
        action: &Id,
        actor: &ActorId,
        receipt: &Value,
    ) -> Result<(), StorageError>;
}
