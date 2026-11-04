//! Unified Knowledge records (KNOW-01..08): entities, atomic claims and their tags, evidence and
//! verification basis, typed relations, revision history and the write journal, lesson
//! recurrence events, challenges, local addresses, per-session rule delivery, observed source
//! revisions and the user import identity map.
//!
//! * **One store.** Memory, knowledge and lessons are claims told apart by tags and entry kind;
//!   there is no second catalog. Scope ([`KnowledgeScope`]) is a separate property of every
//!   record: tags never widen it.
//! * **Scope revisions.** Every change of a scope's membership or content advances that scope's
//!   revision ([`KnowledgeRepository::bump_scope`]) in the same unit of work, so cursors and
//!   previews read under an older revision expire.
//! * **Revisions.** Entities and claims are compare-and-set on their revision; every revision is
//!   kept as an immutable attributed snapshot ([`StoredHistory`]) tied to its write.
//! * **Filters are applied by the store** ([`ClaimQuery`]), before rows are returned, so a page
//!   is never filtered after its limit and no out-of-scope row is materialized.
//!
//! Folding (tags, entity keys, search words) is done by `crate::services::knowledge` in Rust;
//! the lexical index folds Unicode case itself. Authority, scope eligibility, lifecycle rules and
//! lesson policy belong to that service, which runs inside one unit of work.
use super::StorageError;
use crate::protocol::{ActorId, Id, ProjectId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where a Knowledge record lives: general (every project sees it) or one project.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum KnowledgeScope {
    Workspace,
    Project(ProjectId),
}
impl KnowledgeScope {
    /// The stored key: `workspace` or `project:<id>`.
    pub fn key(&self) -> String {
        match self {
            Self::Workspace => "workspace".into(),
            Self::Project(p) => format!("project:{}", p.0.as_str()),
        }
    }
    pub fn project(&self) -> Option<&ProjectId> {
        match self {
            Self::Workspace => None,
            Self::Project(p) => Some(p),
        }
    }
    /// The scope of a stored key.
    pub fn parse(key: &str) -> Result<Self, StorageError> {
        if key == "workspace" {
            return Ok(Self::Workspace);
        }
        key.strip_prefix("project:")
            .and_then(|p| Id::new(p).ok())
            .map(|p| Self::Project(ProjectId(p)))
            .ok_or_else(|| StorageError::InvalidRecord(format!("knowledge scope {key}")))
    }
}

/// An entity: the subject or object of claims.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredEntity {
    pub id: Id,
    pub scope: KnowledgeScope,
    /// Positive; advances on every change.
    pub revision: i64,
    pub label: String,
    pub entity_type: String,
    pub aliases: Vec<String>,
    /// Case-folded `type|name`; unique among live entities of a scope (reuse key).
    pub canonical_key: String,
    pub canonical_name: Option<String>,
    pub external_id: Option<String>,
    pub source: Option<String>,
    pub author: ActorId,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub deleted_at_ms: Option<i64>,
}

/// A lesson's stored rule fields (indexed for lesson filters).
#[derive(Clone, Debug, PartialEq)]
pub struct StoredLesson {
    /// `RuleState` wire name as stored (a recurrence-derived transition is applied on read).
    pub rule_state: String,
    /// `LessonSource` wire name of the first recorded event.
    pub source: String,
    /// Wire `Applicability` JSON.
    pub applicability: Value,
    pub effective_at_ms: Option<i64>,
}

/// Claim content and provenance that no filter reads (stored as JSON).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_json: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized_text: Option<String>,
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<Id>,
    /// `KnowledgeOrigin` wire name: who made the decision or stated the claim.
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_ref: Option<String>,
    /// An agent's decision made without the user.
    pub autonomous: bool,
    /// The principal kind that recorded the first revision (`Human`, `Agent`, ...).
    pub author_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_source_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_by: Option<ActorId>,
    pub evidence_ids: Vec<Id>,
    /// Lesson fields no filter reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lesson_source_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lesson_quoted_words: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lesson_effect: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lesson_reason: Option<String>,
    /// The last lifecycle reason (retraction, deletion).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A claim at its current revision.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredClaim {
    pub id: Id,
    pub scope: KnowledgeScope,
    /// Positive; advances on every change.
    pub revision: i64,
    /// `KnowledgeKind` wire name (the revision-1 kind).
    pub kind: String,
    /// `KnowledgeEntryKind` wire name.
    pub entry_kind: String,
    /// `KnowledgeLifecycle` wire name.
    pub lifecycle: String,
    pub subject_id: Id,
    pub predicate: String,
    pub object_id: Option<Id>,
    pub source: Option<String>,
    pub author: ActorId,
    pub session_id: Option<Id>,
    /// Case-folded, sorted and unique.
    pub tags: Vec<String>,
    /// The `(source_ref, source_revision)` pairs it was recorded or verified against.
    pub basis: Vec<(String, String)>,
    pub lesson: Option<StoredLesson>,
    pub body: ClaimBody,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub deleted_at_ms: Option<i64>,
}

/// One evidence record.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredEvidence {
    pub id: Id,
    pub scope: KnowledgeScope,
    /// `EvidenceKind` wire name.
    pub kind: String,
    pub source_ref: String,
    /// Wire `ArtifactRef` JSON.
    pub artifact: Option<Value>,
    pub locator: Option<String>,
    pub snippet: String,
    pub metadata: Option<Value>,
    pub source_revision: Option<String>,
    pub author: ActorId,
    pub recorded_at_ms: i64,
    pub deleted_at_ms: Option<i64>,
}

/// A typed relation between two entities or claims.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredRelation {
    pub id: Id,
    pub scope: KnowledgeScope,
    pub subject_id: Id,
    pub predicate: String,
    pub object_id: Id,
    pub evidence_ids: Vec<Id>,
    pub author: ActorId,
    pub created_at_ms: i64,
    pub removed_at_ms: Option<i64>,
    pub write_id: Id,
}

/// One immutable revision snapshot of an entity or claim.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredHistory {
    pub target_id: Id,
    pub revision: i64,
    /// `Entity` or `Claim`.
    pub target_kind: String,
    /// What the revision changed (`Recorded`, `Revised`, `Tagged`, ...).
    pub change: String,
    pub author: ActorId,
    pub at_ms: i64,
    pub write_id: Id,
    pub lifecycle: String,
    /// The record at that revision (wire JSON; content is purged by a deletion).
    pub snapshot: Value,
}

/// One target a write changed, with its revision before (`None`: created) and after.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteChange {
    /// `Entity`, `Claim`, `Evidence` or `Relation`.
    pub target_kind: String,
    pub id: Id,
    pub before: Option<i64>,
    pub after: Option<i64>,
}

/// The durable receipt of a committed Knowledge write.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredWrite {
    pub write_id: Id,
    pub operation: String,
    pub scope: KnowledgeScope,
    pub actor: ActorId,
    pub session_id: Option<Id>,
    pub at_ms: i64,
    /// The scope revision after the write.
    pub scope_revision: i64,
    pub entity_ids: Vec<Id>,
    pub claim_ids: Vec<Id>,
    pub changes: Vec<WriteChange>,
    /// Wire `Diagnostic` JSON objects.
    pub diagnostics: Vec<Value>,
}

/// One distinct source event a lesson recurred in.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredLessonEvent {
    pub claim_id: Id,
    /// Distinct per source event: `<source>|<source_ref>`, or the write for an event without a
    /// reference.
    pub event_key: String,
    pub source: String,
    pub source_ref: Option<String>,
    pub session_id: Option<Id>,
    pub actor: ActorId,
    pub at_ms: i64,
    pub write_id: Id,
    /// The lessons this event was merged from, oldest first (a merge pushes, an unmerge pops).
    pub merge_path: Vec<Id>,
}

/// A challenge of a claim or rule.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredChallenge {
    pub id: Id,
    pub claim_id: Id,
    /// `ChallengeState` wire name.
    pub state: String,
    pub reason: String,
    pub evidence_ids: Vec<Id>,
    pub author: ActorId,
    pub opened_at_ms: i64,
    pub resolved_at_ms: Option<i64>,
    pub resolved_by: Option<ActorId>,
    pub resolution: Option<String>,
}

/// Where a lesson is addressed outside Knowledge.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredAddress {
    pub claim_id: Id,
    /// `LessonAddressKind` wire name.
    pub kind: String,
    pub name: String,
    pub reference: Option<String>,
    pub actor: ActorId,
    pub at_ms: i64,
}

/// How often a rule was supplied to sessions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeliverySummary {
    /// Distinct sessions it was supplied to (any revision).
    pub sessions: i64,
    pub last_at_ms: Option<i64>,
}

/// The lesson filters of a [`ClaimQuery`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LessonFilter {
    /// Effective rule states (`RuleState` wire names) after the recurrence rule: a stored
    /// `Candidate` or `Proposed` lesson whose distinct events reach its scope's threshold reads
    /// `Effective`. Empty: any.
    pub rule_states: Vec<String>,
    /// The recurrence threshold of each scope read, by scope key.
    pub thresholds: Vec<(String, i64)>,
    /// Applicability activity the lesson must list.
    pub activity: Option<String>,
    /// `LessonView` wire name (`All` when `None`).
    pub view: Option<String>,
}

/// A claim listing filter, applied by the store before paging.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClaimQuery {
    /// Scope keys read (never empty).
    pub scopes: Vec<String>,
    /// `KnowledgeLifecycle` wire names; empty: any.
    pub lifecycles: Vec<String>,
    /// `KnowledgeKind` wire names; empty: any.
    pub kinds: Vec<String>,
    /// `KnowledgeEntryKind` wire names; empty: any.
    pub entry_kinds: Vec<String>,
    /// Folded tags the claim must all carry.
    pub tags: Vec<String>,
    /// Folded words that must all appear in the claim's text or its subject entity's.
    pub words: Vec<String>,
    /// At most this many matching subject entities contribute (with `words`).
    pub entity_limit: Option<u16>,
    pub source: Option<String>,
    pub stale: Option<bool>,
    pub subject_id: Option<Id>,
    pub lesson: Option<LessonFilter>,
}

/// Counts of one scope set (diagnostics).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KnowledgeCounts {
    /// `(lifecycle, count)` of claims.
    pub claims: Vec<(String, i64)>,
    /// `(rule_state as stored, count)` of live lessons.
    pub lessons: Vec<(String, i64)>,
    pub entities: i64,
    pub evidence: i64,
    pub stale: i64,
    pub open_challenges: i64,
}

/// Most rows one repository page returns.
pub const KNOWLEDGE_PAGE_MAX: u16 = 500;

pub trait KnowledgeRead {
    /// The scope's revision; 0 before its first write.
    fn knowledge_scope_revision(&self, scope: &KnowledgeScope) -> Result<i64, StorageError>;
    fn knowledge_entity(&self, id: &Id) -> Result<Option<StoredEntity>, StorageError>;
    /// The live entity of `scope` with `canonical_key`.
    fn knowledge_entity_by_key(
        &self,
        scope: &KnowledgeScope,
        canonical_key: &str,
    ) -> Result<Option<StoredEntity>, StorageError>;
    fn knowledge_claim(&self, id: &Id) -> Result<Option<StoredClaim>, StorageError>;
    /// Claims matching `query`, ordered by `(sort time, id)` descending, strictly after `after`,
    /// at most `limit` (1..=[`KNOWLEDGE_PAGE_MAX`]). The sort time is the update time, or for the
    /// `RecentlyLearned` view the time a rule took effect.
    fn knowledge_claims(
        &self,
        query: &ClaimQuery,
        after: Option<(i64, &str)>,
        limit: u16,
    ) -> Result<Vec<StoredClaim>, StorageError>;
    /// Live claims of `scopes` about `subject` with `predicate`, at most `limit`.
    fn knowledge_claims_about(
        &self,
        scopes: &[String],
        subject: &Id,
        predicate: &str,
        limit: u16,
    ) -> Result<Vec<StoredClaim>, StorageError>;
    /// Non-deleted claims of exactly `scope` that reference `entity` as subject or object.
    fn knowledge_entity_users(
        &self,
        entity: &Id,
        limit: u16,
    ) -> Result<Vec<StoredClaim>, StorageError>;
    fn knowledge_evidence(&self, id: &Id) -> Result<Option<StoredEvidence>, StorageError>;
    /// The claims that cite `evidence`, at most `limit`.
    fn knowledge_evidence_users(&self, evidence: &Id, limit: u16) -> Result<Vec<Id>, StorageError>;
    /// Live relations whose subject or object is `id`, at most `limit`.
    fn knowledge_relations(&self, id: &Id, limit: u16)
    -> Result<Vec<StoredRelation>, StorageError>;
    /// The revision snapshots of `target`, newest first, at most `limit`.
    fn knowledge_history(
        &self,
        target: &Id,
        limit: u16,
    ) -> Result<Vec<StoredHistory>, StorageError>;
    fn knowledge_history_at(
        &self,
        target: &Id,
        revision: i64,
    ) -> Result<Option<StoredHistory>, StorageError>;
    fn knowledge_write(&self, id: &Id) -> Result<Option<StoredWrite>, StorageError>;
    /// The newest writes of `scope`, at most `limit`.
    fn knowledge_recent_writes(
        &self,
        scope: &KnowledgeScope,
        limit: u16,
    ) -> Result<Vec<StoredWrite>, StorageError>;
    /// Distinct source events of a lesson.
    fn knowledge_lesson_event_count(&self, claim: &Id) -> Result<i64, StorageError>;
    /// The events of a lesson, oldest first, at most `limit`.
    fn knowledge_lesson_events(
        &self,
        claim: &Id,
        limit: u16,
    ) -> Result<Vec<StoredLessonEvent>, StorageError>;
    /// Events whose latest merge moved them off `source` (what unmerging `source` restores).
    fn knowledge_merged_events(
        &self,
        source: &Id,
        limit: u16,
    ) -> Result<Vec<StoredLessonEvent>, StorageError>;
    /// Challenges of a claim, oldest first, at most `limit`.
    fn knowledge_challenges(
        &self,
        claim: &Id,
        limit: u16,
    ) -> Result<Vec<StoredChallenge>, StorageError>;
    fn knowledge_challenge(&self, id: &Id) -> Result<Option<StoredChallenge>, StorageError>;
    /// Open challenges of a claim (all of them, not a page).
    fn knowledge_open_challenges(&self, claim: &Id) -> Result<i64, StorageError>;
    fn knowledge_addresses(&self, claim: &Id) -> Result<Vec<StoredAddress>, StorageError>;
    fn knowledge_deliveries(&self, claim: &Id) -> Result<DeliverySummary, StorageError>;
    /// Whether a later revision of one of the claim's basis sources was observed.
    fn knowledge_claim_stale(&self, claim: &Id) -> Result<bool, StorageError>;
    fn knowledge_source_head(&self, source_ref: &str) -> Result<Option<String>, StorageError>;
    /// Non-deleted claims of exactly `scope` whose source is `source`, by id, strictly after
    /// `after`, at most `limit`.
    fn knowledge_source_claims(
        &self,
        scope: &KnowledgeScope,
        source: &str,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<StoredClaim>, StorageError>;
    /// Live entities of exactly `scope` whose source is `source`, by id, strictly after
    /// `after`, at most `limit`.
    fn knowledge_source_entities(
        &self,
        scope: &KnowledgeScope,
        source: &str,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<StoredEntity>, StorageError>;
    fn knowledge_counts(&self, scopes: &[String]) -> Result<KnowledgeCounts, StorageError>;
    /// The claim an import candidate became.
    fn knowledge_imported(
        &self,
        artifact_version: &Id,
        candidate: i64,
    ) -> Result<Option<Id>, StorageError>;
}

pub trait KnowledgeRepository: KnowledgeRead {
    /// Advances the scope's revision, returning the new value (1 for its first write).
    fn bump_scope(&mut self, scope: &KnowledgeScope) -> Result<i64, StorageError>;
    /// Inserts `entity` at revision 1 (`expected` `None`; an existing ID is
    /// [`StorageError::Duplicate`]) or replaces it at `expected`, storing `expected + 1`.
    fn put_knowledge_entity(
        &mut self,
        entity: &StoredEntity,
        expected: Option<i64>,
    ) -> Result<StoredEntity, StorageError>;
    /// Inserts `claim` at revision 1 or replaces it at `expected`, storing `expected + 1`, with
    /// its tags, basis, evidence links and lexical row (none once deleted).
    fn put_knowledge_claim(
        &mut self,
        claim: &StoredClaim,
        expected: Option<i64>,
    ) -> Result<StoredClaim, StorageError>;
    /// Inserts evidence (`replace` false; an existing ID is [`StorageError::Duplicate`]) or
    /// replaces an existing one (a purge).
    fn put_knowledge_evidence(
        &mut self,
        evidence: &StoredEvidence,
        replace: bool,
    ) -> Result<(), StorageError>;
    /// Inserts a relation; a live relation with the same subject, predicate and object is
    /// [`StorageError::Duplicate`].
    fn put_knowledge_relation(&mut self, relation: &StoredRelation) -> Result<(), StorageError>;
    /// Marks the live relation of `scope` with this subject, predicate and object removed,
    /// returning its ID (`None` when there is none).
    fn remove_live_knowledge_relation(
        &mut self,
        scope: &KnowledgeScope,
        subject: &Id,
        predicate: &str,
        object: &Id,
        at_ms: i64,
    ) -> Result<Option<Id>, StorageError>;
    /// Marks a live relation removed; `false` when it was not live.
    fn remove_knowledge_relation(&mut self, id: &Id, at_ms: i64) -> Result<bool, StorageError>;
    /// Appends a revision snapshot; an existing `(target, revision)` is
    /// [`StorageError::Duplicate`].
    fn put_knowledge_history(&mut self, history: &StoredHistory) -> Result<(), StorageError>;
    /// Purges the content of every snapshot of `target` (explicit deletion), keeping each
    /// snapshot's attribution, time, change and lifecycle.
    fn purge_knowledge_history(
        &mut self,
        target: &Id,
        retained: &dyn Fn(&Value) -> Value,
    ) -> Result<(), StorageError>;
    fn put_knowledge_write(&mut self, write: &StoredWrite) -> Result<(), StorageError>;
    /// Records a lesson event; `false` when the lesson already has that event.
    fn add_knowledge_lesson_event(
        &mut self,
        event: &StoredLessonEvent,
    ) -> Result<bool, StorageError>;
    /// Moves every event of `from` onto `to` (a same_as merge), pushing `from` onto each
    /// event's merge path (earlier merges stay below it); an event `to` already has stays with
    /// `from`, counted once. Returns how many moved.
    fn merge_knowledge_lesson_events(&mut self, from: &Id, to: &Id) -> Result<i64, StorageError>;
    /// Moves the events whose latest merge left `from` back onto it, popping it from their
    /// merge path (unmerge; nested merges unwind one level at a time). Returns how many moved.
    fn unmerge_knowledge_lesson_events(&mut self, from: &Id) -> Result<i64, StorageError>;
    /// Inserts a challenge, or replaces one that is `Open` (a resolution); a resolved one is
    /// [`StorageError::InvalidRecord`].
    fn put_knowledge_challenge(&mut self, challenge: &StoredChallenge) -> Result<(), StorageError>;
    /// Inserts or updates the address of the same claim, kind and name.
    fn put_knowledge_address(&mut self, address: &StoredAddress) -> Result<(), StorageError>;
    /// Records that `session` was supplied `claim` at `revision`; `false` when it already was.
    fn record_knowledge_delivery(
        &mut self,
        session: &Id,
        claim: &Id,
        revision: i64,
        at_ms: i64,
    ) -> Result<bool, StorageError>;
    /// Marks every live relation whose subject or object is `id` removed (its endpoint was
    /// deleted), returning how many.
    fn remove_knowledge_relations_of(&mut self, id: &Id, at_ms: i64) -> Result<i64, StorageError>;
    /// Records the current revision of a source; returns the claims whose stale status the
    /// observation changed (became stale, or current again), with their scopes.
    fn observe_knowledge_source(
        &mut self,
        source_ref: &str,
        revision: &str,
        at_ms: i64,
    ) -> Result<Vec<(Id, KnowledgeScope)>, StorageError>;
    /// Scrubs the lesson side records of a deleted claim: challenge reasons, resolutions and
    /// evidence are emptied (identity, state, author and times kept), addresses are removed,
    /// and event references become opaque digests (the distinct-event count is kept).
    fn purge_knowledge_lesson_records(&mut self, claim: &Id) -> Result<(), StorageError>;
    fn map_knowledge_import(
        &mut self,
        artifact_version: &Id,
        candidate: i64,
        claim: &Id,
        write: &Id,
    ) -> Result<(), StorageError>;
}
