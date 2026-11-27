//! `KnowledgeService`: one Knowledge system for memory, knowledge and lessons (KNOW-01..08),
//! registered through [`crate::services::UseCases::with_knowledge`]. The native Knowledge
//! workspace and agent tools reach the same use cases through the gateway.
//!
//! # Model
//!
//! Entities and atomic claims (predicate, value, scope, evidence, author/agent provenance,
//! confidence, timestamps and revision history) are told apart by tags and entry kind, never by
//! catalog: a lesson is a claim of entry kind `Lesson` tagged `lesson`; goals, decisions,
//! constraints, gotchas, attempts and architecture notes are tagged `knowledge`; facts, findings
//! and proposals `memory`. Tags never widen scope (KNOW-08).
//!
//! # Scope (KNOW-07, RAG-05)
//!
//! Every record is general (`Workspace`, available to every project automatically) or belongs to
//! one project. A project read sees that project and general Knowledge; a workspace read sees
//! only general Knowledge. Scope is applied by the store before any row is materialized, and
//! every identifier a request names is checked against it (an invisible one is `NotFound`).
//! Writing general Knowledge needs a principal whose authorized scope is the workspace (the user,
//! the host, an unconfined agent); a project-confined principal writes only its project.
//!
//! # Lifecycle (KNOW-02/03)
//!
//! A claim is `Active`, `Superseded` (by a later claim), `Retracted` (wrong, still readable) or
//! `Deleted` (explicit permissioned deletion: identity, attribution and times kept, content
//! purged). Verification records its evidence and source revision; a later observed source
//! revision ([`KnowledgeService::observe_source`]) makes the claim read stale until verified
//! again. Conflicting active claims (same subject and predicate, another value, or an explicit
//! `contradicts` relation) are kept and shown as open conflicts. Each decision names who made
//! it: a decision the user made or approved (an agent records the approval with `originRef`)
//! has origin `User`; only a decision an agent made alone is `autonomous`, and it never asks for
//! acknowledgment.
//!
//! # Lessons (KNOW-04/05/06)
//!
//! A rule from the user's own words (`Prompt`, `Review`, or relayed by an agent with the user's
//! words attached) takes effect at once. A lesson an agent, subagent or automated reviewer
//! inferred is candidate evidence until it recurs across the configured number of distinct
//! source events (`knowledge.lessonRecurrenceThreshold`), then takes effect automatically. No
//! rule asks for approval; the user may promote a candidate early, challenge or retire any rule,
//! or reject a proposal. A `same_as` merge of a rule the user stated or promoted keeps the
//! lesson it merges into in effect; an unmerge restores that lesson's prior rule once no
//! remaining merge carries the user's effect, and keeps every event a remaining merge brought.
//! Delivery of rules to sessions is recorded
//! ([`KnowledgeService::record_delivery`]); addresses record where a lesson is handled in code,
//! instructions, a skill, documentation or a test.
//!
//! # Retained CodeGraph behavior
//!
//! `record`/`memory_store`/`store_memory_v2` map to `knowledge.record` (with revise, retract,
//! verify and relate as their own operations; `relate` keeps `same_as`, `contradicts` and
//! `unmerge`), `recall`/`search_memory`/`GetKnowledge` to `knowledge.search` and `.read`,
//! `promote` to `knowledge.promote` (approve is `Effective`), `challenge` to
//! `knowledge.challenge`, `lessons` to `knowledge.tagged_lessons` views, memory diagnostics and
//! write status to `knowledge.diagnostics` and `.write_status`, cleanup by source, by ids and of
//! test data to `knowledge.delete_by_source` and `.delete_selected`, and `import_knowledge` to
//! `knowledge.import_user_data`. CodeGraph's `acknowledge` is superseded by KNOW-03 (autonomous
//! decisions never ask to be acknowledged).
pub(crate) mod dream_actions;
mod fusion;
mod graph;
pub mod import;
pub mod model;
mod vectors;
mod writes;
pub(crate) use writes::{reverse_dream_changes, review_dream_change};

pub use fusion::{CANDIDATES, FUSION_K};
pub use graph::NO_MEMORIES;
pub use vectors::VECTOR_CONSUMER;

use crate::context::InvocationContext;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::locks::RwLockExt;
use crate::operations::{OperationInput, knowledge as ops};
use crate::ports::files::ArtifactFiles;
use crate::ports::storage::ClaimVectorQuery;
use crate::ports::storage::{
    ArtifactRead, AuthorityRead, ClaimQuery, ClaimRank, ConfigScope, ConfigurationRead,
    KNOWLEDGE_PAGE_MAX, KnowledgeRead, KnowledgeScope, LessonFilter, RankedClaim, ReadSnapshot,
    StorageError, StoredClaim, StoredEntity, StoredEvidence, StoredHistory, WriteTransaction,
};
use crate::ports::{
    Clock, CommandHandler, Decided, HandlerFailure, IdKind, IdSource, Preparation, Prepared,
    QueryAnswer, QueryHandler,
};
use crate::protocol::data::DataScope;
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{
    Applicability, ChallengeState, Diagnostic, EvidenceKind, EvidenceLocator, KnowledgeBundle,
    KnowledgeClaim, KnowledgeConflict, KnowledgeEntity, KnowledgeEntryKind, KnowledgeKind,
    KnowledgeLifecycle, KnowledgeOrigin, KnowledgeRelation, KnowledgeRetrievalMode,
    KnowledgeRevisionEntry, KnowledgeSearchPage, KnowledgeSearchResult, KnowledgeState,
    KnowledgeWriteReceipt, LessonAddress, LessonAddressKind, LessonChallenge, LessonRecord,
    LessonSource, RuleEffect, RuleState, SchemaValue, Severity, SourceEvidence,
};
use crate::protocol::results::OperationResult;
use crate::protocol::support::{
    BoundedList, BoundedText, DynamicJson, Label, Limit, Page, Probability, RelativePath,
    TokenBudget,
};
use crate::protocol::{CorrelationId, DomainErrorCode, DomainFault, Id, OperationState, ProjectId};
use crate::registry::SchemaRevision;
use crate::services::conventions::indexing::IndexService;
use crate::services::cursor::{self, CursorBinding};
use crate::services::kernel::{
    Failure, Refusal, content, counter, decided, label, not_found, query_fault, require_project,
    rev,
};
use crate::settings::catalog::descriptor;
use crate::settings::model::{chain, current_chain};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

pub use model::{TAG_KNOWLEDGE, TAG_LESSON, TAG_MEMORY};

fn cleanup_source<'a>(
    id: Option<&'a Id>,
    label: Option<&'a Label>,
) -> Result<Option<&'a str>, Failure> {
    match (id, label) {
        (Some(_), Some(_)) => {
            Err(Refusal::new(DomainErrorCode::InvalidArgument, "source", None).into())
        }
        (Some(id), None) => Ok(Some(id.as_str())),
        (None, Some(label)) => Ok(Some(&label.0)),
        (None, None) => Ok(None),
    }
}

/// The operations [`KnowledgeService`] is registered for.
pub const OPERATIONS: [&str; 23] = [
    "knowledge.search",
    "knowledge.read",
    "knowledge.record",
    "knowledge.revise",
    "knowledge.relate",
    "knowledge.tag",
    "knowledge.verify",
    "knowledge.retract",
    "knowledge.scope",
    "knowledge.diagnostics",
    "knowledge.delete_by_source",
    "knowledge.delete_selected",
    "knowledge.write_status",
    "knowledge.import_user_data",
    "knowledge.promote",
    "knowledge.challenge",
    "knowledge.tagged_lessons",
    "knowledge.subgraph",
    "knowledge.entity_bundle",
    "knowledge.entity_graph",
    "knowledge.claim_bundle",
    "knowledge.expand_frontier",
    "knowledge.query",
];
/// Registered only when summary publication is composed ([`KnowledgeService::with_summaries`]).
pub const SUMMARY_OPERATION: &str = "knowledge.render_summary";

/// The setting that configures how many distinct source events make an inferred lesson a rule.
pub const THRESHOLD_KEY: &str = "knowledge.lessonRecurrenceThreshold";
/// Setting: the approximate tokens of claim text a `knowledge.search` page holds for an agent
/// caller that passes no `budgetTokens` (inherited; a project may override).
pub const SEARCH_BUDGET_KEY: &str = "knowledge.searchTokenBudget";
/// Setting: the largest `budgetTokens` a `knowledge.search` caller may pass (inherited).
pub const SEARCH_BUDGET_MAX_KEY: &str = "knowledge.searchTokenBudgetMax";
/// The smallest `budgetTokens` a `knowledge.search` caller may pass.
pub const MIN_SEARCH_BUDGET: u32 = 1_000;
/// History aggregate of a committed Knowledge write (its ID is the write ID).
pub const WRITE_AGGREGATE: &str = "knowledge_write";
/// History event of a committed Knowledge write.
pub const WRITE_EVENT: &str = "KnowledgeWritten";
/// Fault target kinds.
pub const CLAIM: &str = "knowledge_claim";
pub const ENTITY: &str = "knowledge_entity";
pub const EVIDENCE: &str = "knowledge_evidence";
pub const SCOPE: &str = "knowledge_scope";
pub const PREVIEW: &str = "knowledge_preview";
pub const LIFECYCLE: &str = "knowledge_lifecycle";
pub const LESSON: &str = "knowledge_lesson";
pub const CHALLENGE: &str = "knowledge_challenge";
pub const WRITE: &str = "knowledge_write";
/// Diagnostic components.
pub const RETRIEVAL_COMPONENT: &str = "knowledge.retrieval";
pub const COUNTS_COMPONENT: &str = "knowledge.counts";
pub const WRITES_COMPONENT: &str = "knowledge.writes";
pub const PREVIEW_COMPONENT: &str = "knowledge.preview";
pub const IMPORT_COMPONENT: &str = "knowledge.import";
/// A lesson a previewed deletion restores (one diagnostic each).
pub const RESTORE_COMPONENT: &str = "knowledge.restore";
/// Most claims (then entities) one delete-by-source transaction tombstones. A larger source is
/// deleted whole, as a durable job of such batches (each one checkpoint, the source fenced
/// read-only meanwhile), because one transaction for it would hold the single writer too long:
/// measured on the development machine (release build, load average ~35), 5,000 claims took
/// 1.7 s and 50,000 took 39 s, past the dispatcher's 30 s delivery lease and the 5 s busy
/// timeout and shutdown grace. In batches of 500 (load average ~47) the same 50,000 claims and
/// 5,000 entities took 111 transactions, the longest 0.29 s.
pub const DELETION_BATCH: usize = 500;
/// The durable consumer that continues a delete-by-source job, one batch per delivery.
pub const SOURCE_DELETION_CONSUMER: &str = "knowledge-source-deletion";
/// History aggregate of a delete-by-source job's continuations.
pub const DELETION_AGGREGATE: &str = "knowledge_source_deletion";
/// Fault target kind naming a running deletion's write (a fenced source).
pub const DELETION: &str = "knowledge_source_deletion";
/// Most challenges, addresses, tags, cited evidence records and aliases one record carries:
/// the record's own wire lists' capacity. A write that would exceed one is refused, so no list
/// is ever cut short. (A receipt is not bound by it: it pages, see `receipt_of`.)
pub const MAX_CHALLENGES: usize = 128;
pub const MAX_LIST: usize = 128;
/// Schema revision 2 of the Knowledge operations.
pub const REVISION_2: SchemaRevision = SchemaRevision(2);

/// The service.
pub struct KnowledgeService {
    ids: Arc<dyn IdSource>,
    clock: Arc<dyn Clock>,
    /// Reads the bytes of an import's immutable artifact version (`None`: imports refused).
    files: Option<Arc<dyn ArtifactFiles>>,
    /// Publishes rendered summaries as artifacts (`None`: `knowledge.render_summary` is not
    /// offered).
    summaries: Option<(
        Arc<dyn crate::ports::storage::ApplicationStore>,
        Arc<crate::services::artifacts::ArtifactService>,
    )>,
    /// The shared qualified embedding runtime: claim vectors and their search ([`vectors`]).
    /// `None`: retrieval is lexical only.
    embeddings: RwLock<Option<Arc<IndexService>>>,
}

// ---------------------------------------------------------------------------------------------
// Scope and authority

pub(crate) use crate::authority::reach::Reach;
/// [`crate::authority::reach::reach`], refusing a principal that has no reach.
pub(crate) fn reach(
    authority: &dyn AuthorityRead,
    ctx: &InvocationContext,
) -> Result<Reach, Failure> {
    crate::authority::reach::reach(authority, ctx)?
        .ok_or_else(|| Refusal::new(DomainErrorCode::PermissionDenied, "principal", None).into())
}

/// The scope a request names, and every scope it may read.
#[derive(Clone, Debug)]
pub(crate) struct Scoped {
    pub exact: KnowledgeScope,
    pub readable: Vec<KnowledgeScope>,
}
impl Scoped {
    pub fn keys(&self) -> Vec<String> {
        self.readable.iter().map(KnowledgeScope::key).collect()
    }
    pub fn sees(&self, scope: &KnowledgeScope) -> bool {
        self.readable.contains(scope)
    }
}

/// Resolves a requested scope: a project must match the invocation's project (when it has one),
/// be within the principal's reach and exist; general Knowledge is readable by every principal
/// with a reach and writable (`write`) only with workspace reach.
pub(crate) fn resolve_scope(
    artifacts: &dyn ArtifactRead,
    authority: &dyn AuthorityRead,
    ctx: &InvocationContext,
    requested: &DataScope,
    write: bool,
) -> Result<Scoped, Failure> {
    let reach = reach(authority, ctx)?;
    resolve_scope_with(&reach, ctx, requested, write, |p| {
        artifacts.project_exists(p)
    })
}
/// [`resolve_scope`] with the principal's reach already read and project existence asked of
/// `project_exists` (inside a unit of work, the two reads are separate borrows).
pub(crate) fn resolve_scope_with(
    reach: &Reach,
    ctx: &InvocationContext,
    requested: &DataScope,
    write: bool,
    project_exists: impl FnOnce(&ProjectId) -> Result<bool, StorageError>,
) -> Result<Scoped, Failure> {
    match requested {
        DataScope::Workspace => {
            if write && *reach != Reach::Workspace {
                return Err(Refusal::new(DomainErrorCode::PermissionDenied, SCOPE, None).into());
            }
            Ok(Scoped {
                exact: KnowledgeScope::Workspace,
                readable: vec![KnowledgeScope::Workspace],
            })
        }
        DataScope::Project { project_id } => {
            require_project(ctx, project_id)?;
            if let Reach::Project(p) = reach
                && p != project_id
            {
                return Err(Refusal::new(
                    DomainErrorCode::PermissionDenied,
                    "project",
                    Some(project_id.0.clone()),
                )
                .into());
            }
            if !project_exists(project_id)? {
                return Err(not_found("project", &project_id.0).into());
            }
            Ok(Scoped {
                exact: KnowledgeScope::Project(project_id.clone()),
                readable: vec![
                    KnowledgeScope::Project(project_id.clone()),
                    KnowledgeScope::Workspace,
                ],
            })
        }
    }
}

/// Whether the principal may change records of `scope`.
pub(crate) fn writable(reach: &Reach, scope: &KnowledgeScope) -> bool {
    match (reach, scope) {
        (Reach::Workspace, _) => true,
        (Reach::Project(p), KnowledgeScope::Project(q)) => p == q,
        (Reach::Project(_), KnowledgeScope::Workspace) => false,
    }
}

/// Whether a record of `inner` may be referenced from a record of `outer` without exposing it
/// beyond its scope: the same scope, or general Knowledge from a project.
pub(crate) fn nests(inner: &KnowledgeScope, outer: &KnowledgeScope) -> bool {
    inner == outer || *inner == KnowledgeScope::Workspace
}

/// The recurrence threshold of each scope (inherited setting; a project may override).
pub(crate) fn thresholds(
    config: &dyn ConfigurationRead,
    scopes: &[KnowledgeScope],
) -> Result<BTreeMap<String, i64>, StorageError> {
    let mut out = BTreeMap::new();
    for s in scopes {
        let value = scope_setting(config, THRESHOLD_KEY, s)?;
        out.insert(s.key(), value.as_i64().unwrap_or(3).max(1));
    }
    Ok(out)
}

/// The effective value of the inherited Knowledge setting `key` for `scope`: the project's own
/// value, else the workspace's, else the catalog default.
fn scope_setting(
    config: &dyn ConfigurationRead,
    key: &str,
    scope: &KnowledgeScope,
) -> Result<Value, StorageError> {
    let d = descriptor(key).expect("catalog setting");
    let layers = current_chain(config, &ConfigScope::from(scope))?;
    let entries = chain(d, &layers).map_err(|m| {
        StorageError::InvalidRecord(format!("{} stored under another schema", m.key))
    })?;
    Ok(entries
        .first()
        .map(|e| e.value.value.0.clone())
        .unwrap_or_else(|| d.default_value()))
}

/// The token budget of a `knowledge.search` page of `scope`: the caller's `budgetTokens`, which
/// must lie between [`MIN_SEARCH_BUDGET`] and the scope's [`SEARCH_BUDGET_MAX_KEY`] (otherwise
/// `InvalidArgument` on `budgetTokens`, its target id the allowed range `<min>-<max>`), else
/// the scope's [`SEARCH_BUDGET_KEY`] for an agent caller (at most the maximum), else none: a
/// person, or the host acting for them, pages by count.
fn search_budget(
    config: &dyn ConfigurationRead,
    ctx: &InvocationContext,
    scope: &KnowledgeScope,
    requested: Option<u32>,
) -> Result<Option<u32>, Failure> {
    let setting = |key| -> Result<u32, StorageError> {
        let value = scope_setting(config, key, scope)?;
        value
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| StorageError::InvalidRecord(format!("{key} is not a token count")))
    };
    let max = setting(SEARCH_BUDGET_MAX_KEY)?;
    match requested {
        Some(b) if (MIN_SEARCH_BUDGET..=max).contains(&b) => Ok(Some(b)),
        Some(_) => Err(Refusal::new(
            DomainErrorCode::InvalidArgument,
            "budgetTokens",
            Id::new(format!("{MIN_SEARCH_BUDGET}-{max}")).ok(),
        )
        .into()),
        None if writes::is_user(ctx) => Ok(None),
        // Two settings saved apart can disagree; the maximum bounds the default too.
        None => Ok(Some(setting(SEARCH_BUDGET_KEY)?.min(max))),
    }
}

/// A claim's approximate tokens: the characters of its text ([`StoredClaim::text`]) divided by
/// four, rounded up. No tokenizer: the count is an estimate, and the reply says so.
pub fn approximate_tokens(c: &StoredClaim) -> u64 {
    (c.text().chars().count() as u64).div_ceil(4)
}

/// `rows` cut to `budget` approximate tokens of their claims' text, always keeping the first
/// ([`cursor::Paged::clip_to_budget`]); no budget keeps the page as the pager read it. Every
/// `knowledge.search` page, with words or without, is cut here.
fn budgeted<T>(
    rows: cursor::Paged<T>,
    binding: &CursorBinding,
    budget: Option<u32>,
    claim: impl Fn(&T) -> &StoredClaim,
    position_of: impl Fn(&T) -> String,
) -> cursor::Paged<T> {
    match budget {
        Some(b) => rows.clip_to_budget(
            binding,
            u64::from(b),
            |r| approximate_tokens(claim(r)),
            position_of,
        ),
        None => rows,
    }
}

// ---------------------------------------------------------------------------------------------
// Wire records

fn text_of(s: &str) -> BoundedText<1, 4096> {
    let t: String = s.chars().take(4096).collect();
    BoundedText(if t.is_empty() { "-".into() } else { t })
}
fn id_list(ids: &[Id]) -> BoundedList<Id, 0, 128> {
    BoundedList(ids.iter().take(128).cloned().collect())
}
fn labels(items: &[String]) -> BoundedList<BoundedText<1, 256>, 0, 128> {
    BoundedList(items.iter().take(128).map(|s| label(s, "-")).collect())
}

pub(crate) fn entity_wire(e: &StoredEntity) -> KnowledgeEntity {
    KnowledgeEntity {
        id: e.id.clone(),
        revision: rev(e.revision),
        label: label(&e.label, "-"),
        entity_type: label(&e.entity_type, "-"),
        aliases: labels(&e.aliases),
        scope: DataScope::from(&e.scope),
        canonical_name: e.canonical_name.as_deref().map(|s| label(s, "-")),
        external_id: e.external_id.as_deref().map(text_of),
        source: e.source.as_deref().map(|s| label(s, "-")),
        author_id: Some(e.author.clone()),
        created_at_ms: Some(counter(e.created_at_ms)),
        updated_at_ms: Some(counter(e.updated_at_ms)),
        deleted: Some(e.deleted_at_ms.is_some()),
    }
}

pub(crate) fn evidence_wire(
    e: &StoredEvidence,
    claim: Option<&Id>,
) -> Result<SourceEvidence, StorageError> {
    let corrupt = |what: &str| StorageError::InvalidRecord(format!("stored evidence {what}"));
    Ok(SourceEvidence {
        id: e.id.clone(),
        claim_id: claim.cloned(),
        source: EvidenceLocator {
            kind: model::parse::<EvidenceKind>(&e.kind).ok_or_else(|| corrupt("kind"))?,
            source_ref: text_of(&e.source_ref),
            artifact: e
                .artifact
                .clone()
                .map(serde_json::from_value::<ArtifactRef>)
                .transpose()
                .map_err(|_| corrupt("artifact"))?,
            locator: e.locator.as_deref().map(text_of),
        },
        snippet: content(&e.snippet),
        metadata: e
            .metadata
            .clone()
            .map(serde_json::from_value::<SchemaValue>)
            .transpose()
            .map_err(|_| corrupt("metadata"))?,
        source_revision: e.source_revision.as_deref().map(text_of),
    })
}

/// The wire claim (schema revision 2 shape; the gateway projects it for revision 1). Lesson
/// state is read as of now: recurrence against the scope's current threshold.
pub(crate) fn claim_wire(
    read: &dyn KnowledgeRead,
    c: &StoredClaim,
    thresholds: &BTreeMap<String, i64>,
) -> Result<KnowledgeClaim, StorageError> {
    let corrupt = |what: &str| StorageError::InvalidRecord(format!("stored claim {what}"));
    let lifecycle: KnowledgeLifecycle =
        model::parse(&c.lifecycle).ok_or_else(|| corrupt("lifecycle"))?;
    // One read of the claim's maintained tallies; none of them counts history.
    let tally = read.knowledge_claim_tally(&c.id)?;
    let stale = c.lifecycle == "Active" && tally.stale;
    // A deleted claim shows no challenge or address content: only its identity remains.
    let deleted = c.lifecycle == "Deleted";
    let challenges = if deleted {
        Vec::new()
    } else {
        read.knowledge_challenges(&c.id, MAX_CHALLENGES as u16)?
    };
    let open_challenge = !deleted && tally.open_challenges > 0;
    let state = match c.lifecycle.as_str() {
        "Retracted" | "Deleted" => KnowledgeState::Retracted,
        _ if open_challenge => KnowledgeState::Challenged,
        _ if c.body.verified_at_ms.is_some() && !stale => KnowledgeState::Verified,
        _ => KnowledgeState::Active,
    };
    let challenge_records = challenges
        .iter()
        .take(128)
        .map(|ch| {
            Ok(LessonChallenge {
                id: ch.id.clone(),
                state: model::parse::<ChallengeState>(&ch.state)
                    .ok_or_else(|| corrupt("challenge state"))?,
                reason: content(&ch.reason),
                evidence_ids: id_list(&ch.evidence_ids),
                author_id: ch.author.clone(),
                opened_at_ms: counter(ch.opened_at_ms),
                resolved_at_ms: ch.resolved_at_ms.map(counter),
                resolution: ch.resolution.as_deref().map(content),
            })
        })
        .collect::<Result<Vec<_>, StorageError>>()?;
    let lesson = match &c.lesson {
        None => None,
        Some(l) => {
            let events = tally.lesson_events;
            let threshold = thresholds.get(&c.scope.key()).copied().unwrap_or(3);
            let (rule_state, effect) = model::effective_rule_state(
                &l.rule_state,
                c.body.lesson_effect.as_deref(),
                events,
                threshold,
            );
            let applicability: Applicability = serde_json::from_value(l.applicability.clone())
                .map_err(|_| corrupt("applicability"))?;
            let deliveries = &tally.deliveries;
            let addresses = if deleted {
                Vec::new()
            } else {
                read.knowledge_addresses(&c.id)?
            };
            Some(LessonRecord {
                rule_state: model::parse::<RuleState>(&rule_state)
                    .ok_or_else(|| corrupt("rule state"))?,
                effect: effect
                    .as_deref()
                    .map(|e| model::parse::<RuleEffect>(e).ok_or_else(|| corrupt("rule effect")))
                    .transpose()?,
                effective_at_ms: l.effective_at_ms.map(counter),
                source: model::parse::<LessonSource>(&l.source)
                    .ok_or_else(|| corrupt("lesson source"))?,
                source_ref: c.body.lesson_source_ref.as_deref().map(text_of),
                quoted_words: c.body.lesson_quoted_words.as_deref().map(content),
                applicability,
                recurrence: counter(events),
                recurrence_threshold: counter(threshold),
                challenges: BoundedList(challenge_records.clone()),
                addressed_by: BoundedList(
                    addresses
                        .iter()
                        .take(128)
                        .map(|a| {
                            Ok(LessonAddress {
                                kind: model::parse::<LessonAddressKind>(&a.kind)
                                    .ok_or_else(|| corrupt("address kind"))?,
                                name: label(&a.name, "-"),
                                reference: a.reference.as_deref().map(text_of),
                            })
                        })
                        .collect::<Result<_, StorageError>>()?,
                ),
                delivered_sessions: counter(deliveries.sessions),
                last_delivered_at_ms: deliveries.last_at_ms.map(counter),
                reason: c.body.lesson_reason.as_deref().map(content),
                // Set by `with_skills` where the viewer is known.
                skills: None,
            })
        }
    };
    Ok(KnowledgeClaim {
        id: c.id.clone(),
        revision: rev(c.revision),
        kind: model::parse::<KnowledgeKind>(&c.kind).ok_or_else(|| corrupt("kind"))?,
        subject_id: c.subject_id.clone(),
        predicate: label(&c.predicate, "-"),
        object_id: c.object_id.clone(),
        value: c.body.value.as_deref().map(content),
        confidence: Probability::new(c.body.confidence).ok_or_else(|| corrupt("confidence"))?,
        state,
        evidence_ids: id_list(&c.body.evidence_ids),
        tags: labels(&c.tags),
        scope: DataScope::from(&c.scope),
        value_json: c.body.value_json.clone().map(DynamicJson),
        normalized_text: c.body.normalized_text.as_deref().map(content),
        effective_at_ms: c.body.effective_at_ms.map(counter),
        supersedes: c.body.supersedes.clone(),
        source: c.source.as_deref().map(|s| label(s, "-")),
        entry_kind: Some(
            model::parse::<KnowledgeEntryKind>(&c.entry_kind)
                .ok_or_else(|| corrupt("entry kind"))?,
        ),
        lifecycle: Some(lifecycle),
        origin: Some(
            model::parse::<KnowledgeOrigin>(&c.body.origin).ok_or_else(|| corrupt("origin"))?,
        ),
        origin_ref: c.body.origin_ref.as_deref().map(text_of),
        autonomous: Some(c.body.autonomous),
        author_id: Some(c.author.clone()),
        session_id: c.session_id.clone(),
        created_at_ms: Some(counter(c.created_at_ms)),
        updated_at_ms: Some(counter(c.updated_at_ms)),
        verified_at_ms: c.body.verified_at_ms.map(counter),
        verified_source_revision: c.body.verified_source_revision.as_deref().map(text_of),
        stale: Some(stale),
        superseded_by: c.body.superseded_by.clone(),
        lesson,
        challenges: Some(BoundedList(challenge_records)),
    })
}

fn history_wire(h: &StoredHistory) -> Result<KnowledgeRevisionEntry, StorageError> {
    Ok(KnowledgeRevisionEntry {
        revision: rev(h.revision),
        change: label(&h.change, "-"),
        author_id: h.author.clone(),
        at_ms: counter(h.at_ms),
        lifecycle: model::parse(&h.lifecycle)
            .ok_or_else(|| StorageError::InvalidRecord("stored history lifecycle".into()))?,
        write_id: Some(h.write_id.clone()),
        value: h.snapshot.get("value").and_then(Value::as_str).map(content),
    })
}

fn diagnostic(id: &str, severity: Severity, component: &str, code: &str, msg: &str) -> Diagnostic {
    Diagnostic {
        id: Id::new(id.chars().take(128).collect::<String>())
            .unwrap_or_else(|_| Id::new("diagnostic").expect("valid")),
        severity,
        component: label(component, "knowledge"),
        code: label(code, "-"),
        message: content(msg),
        correlation_id: None,
        evidence: None,
    }
}

/// How many IDs of each list one receipt names (the wire `Items` bound).
pub const RECEIPT_WINDOW: usize = 128;

/// What a receipt cursor is bound to: the write (immutable once committed) and the reader.
fn receipt_binding(ctx: &InvocationContext, write: &Id) -> CursorBinding {
    CursorBinding::new(ctx, "knowledge.write_status", &[write.as_str()], 0)
}

/// The receipt of a write, naming window `window` of its claim IDs, entity IDs and notes (128
/// of each). It carries both ID totals, and a cursor while any list has more, so a bulk write
/// is named completely across write_status pages instead of being cut or refused.
fn receipt_of(
    ctx: &InvocationContext,
    w: &crate::ports::storage::StoredWrite,
    window: usize,
) -> KnowledgeWriteReceipt {
    let start = window.saturating_mul(RECEIPT_WINDOW);
    let slice = |ids: &[Id]| -> Vec<Id> {
        ids.iter()
            .skip(start)
            .take(RECEIPT_WINDOW)
            .cloned()
            .collect()
    };
    let end = start.saturating_add(RECEIPT_WINDOW);
    let more = w.claim_ids.len() > end || w.entity_ids.len() > end || w.diagnostics.len() > end;
    KnowledgeWriteReceipt {
        write_id: w.write_id.clone(),
        state: OperationState::ObservedComplete,
        entity_ids: BoundedList(slice(&w.entity_ids)),
        claim_ids: BoundedList(slice(&w.claim_ids)),
        revision: rev(w.scope_revision),
        // The write's notes page with its IDs: a write with many notes loses none.
        diagnostics: BoundedList(
            w.diagnostics
                .iter()
                .skip(start)
                .take(RECEIPT_WINDOW)
                .filter_map(|d| serde_json::from_value(d.clone()).ok())
                .collect(),
        ),
        entity_count: Some(counter(w.entity_ids.len() as i64)),
        claim_count: Some(counter(w.claim_ids.len() as i64)),
        next_cursor: more.then(|| {
            BoundedText(receipt_binding(ctx, &w.write_id).encode(&(window + 1).to_string()))
        }),
    }
}

// ---------------------------------------------------------------------------------------------
// Paging

/// Claims in `query`'s order strictly after the position `after`, at most `n`: the read the
/// shared pager pages.
fn claims(
    read: &dyn KnowledgeRead,
    query: &ClaimQuery,
    after: Option<&str>,
    n: u16,
) -> Result<Vec<RankedClaim>, StorageError> {
    let after = after.map(|at| parse_position(query, at)).transpose()?;
    read.knowledge_claims(query, after.as_ref().map(|(r, id)| (*r, id.as_str())), n)
}

/// A claim's position in `query`'s order: its sort key as the store ranked it, and its id. A
/// relevance key is `r` and the exact score (Rust's shortest round-tripping decimal), so the
/// next page resumes strictly after the last row as it was read; the prefix keeps a time
/// position issued before relevance ranking from resuming a relevance listing.
fn claim_position(query: &ClaimQuery, c: &RankedClaim) -> String {
    debug_assert_eq!(
        query.words.is_empty(),
        matches!(c.rank, ClaimRank::Time(_)),
        "the store ranks by relevance exactly when the query has words"
    );
    match c.rank {
        ClaimRank::Time(t) => format!("{t}|{}", c.id.as_str()),
        ClaimRank::Relevance(s) => format!("r{s}|{}", c.id.as_str()),
    }
}

/// Parses a position of `query`'s order.
fn parse_position(query: &ClaimQuery, at: &str) -> Result<(ClaimRank, String), StorageError> {
    let (key, id) = at.split_once('|').ok_or(StorageError::CursorExpired)?;
    let rank = if query.words.is_empty() {
        ClaimRank::Time(key.parse().map_err(|_| StorageError::CursorExpired)?)
    } else {
        ClaimRank::Relevance(
            key.strip_prefix('r')
                .and_then(|s| s.parse().ok())
                .filter(|s: &f64| s.is_finite())
                .ok_or(StorageError::CursorExpired)?,
        )
    };
    Ok((rank, id.to_owned()))
}

/// The `knowledge.search` reply: one page with its retrieval mode, whether a candidate cap
/// limited it ([`fusion`]), the approximate tokens of its claims' text and the token budget
/// that bounded it.
fn search_result(
    claims: cursor::Paged<StoredClaim>,
    wire: impl Fn(&StoredClaim) -> Result<KnowledgeClaim, StorageError>,
    mode: KnowledgeRetrievalMode,
    work_limited: bool,
    budget: Option<u32>,
) -> Result<OperationResult, Failure> {
    let tokens: u64 = claims.items.iter().map(approximate_tokens).sum();
    let page: Page<KnowledgeClaim> = claims.try_into_page(|c| wire(&c))?;
    Ok(OperationResult::KnowledgeSearch {
        value: KnowledgeSearchResult::Current(KnowledgeSearchPage {
            items: page.items,
            next_cursor: page.next_cursor,
            has_more: page.has_more,
            mode,
            work_limited,
            approximate_tokens: Some(counter(i64::try_from(tokens).unwrap_or(i64::MAX))),
            budget_tokens: budget.and_then(TokenBudget::new),
        }),
    })
}

// ---------------------------------------------------------------------------------------------
// Service

impl KnowledgeService {
    pub fn new(
        ids: Arc<dyn IdSource>,
        clock: Arc<dyn Clock>,
        files: Option<Arc<dyn ArtifactFiles>>,
    ) -> Self {
        Self {
            ids,
            clock,
            files,
            summaries: None,
            embeddings: RwLock::new(None),
        }
    }
    /// Embeds claims and adds vector matches to `knowledge.search` through the shared
    /// qualified embedding runtime. The composition that sets it also registers
    /// [`VECTOR_CONSUMER`], which runs [`KnowledgeService::embed_claims`].
    pub fn set_embeddings(&self, runtime: Option<Arc<IndexService>>) {
        *self.embeddings.write_locked() = runtime;
    }
    fn embeddings(&self) -> Option<Arc<IndexService>> {
        self.embeddings.read_locked().clone()
    }
    /// Runs one [`VECTOR_CONSUMER`] delivery: embeds its scope's claims that have no vector
    /// in the selected space. Without an embedding runtime there is nothing to do.
    pub fn embed_claims(
        &self,
        entry: &crate::ports::storage::QueueEntry,
        cancellation: &crate::ports::storage::Cancellation,
    ) -> Result<(), StorageError> {
        match self.embeddings() {
            Some(runtime) => vectors::run(&runtime, entry, cancellation),
            None => Ok(()),
        }
    }
    /// Offers `knowledge.render_summary`: summaries are published as artifacts of the
    /// invocation's scope through the artifact store's observed-content publication.
    pub fn with_summaries(
        mut self,
        store: Arc<dyn crate::ports::storage::ApplicationStore>,
        artifacts: Arc<crate::services::artifacts::ArtifactService>,
    ) -> Self {
        self.summaries = Some((store, artifacts));
        self
    }
    pub fn offers_summaries(&self) -> bool {
        self.summaries.is_some()
    }
    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }

    /// The scope revisions a listing read, as cursor binding parts.
    fn revision_parts(
        read: &dyn KnowledgeRead,
        scoped: &Scoped,
    ) -> Result<(Vec<String>, i64), StorageError> {
        let mut parts = Vec::new();
        for s in &scoped.readable {
            parts.push(format!("{}={}", s.key(), read.knowledge_scope_revision(s)?));
        }
        Ok((parts, read.knowledge_scope_revision(&scoped.exact)?))
    }

    // -----------------------------------------------------------------------------------------
    // Queries

    fn search(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::SearchInput,
        prepared: Option<&vectors::QueryVector>,
    ) -> Result<(OperationResult, i64), Failure> {
        let scoped = resolve_scope(
            snapshot.artifacts(),
            snapshot.authority(),
            ctx,
            &i.scope,
            false,
        )?;
        let words = model::words(&i.text.0);
        if words.len() > model::MAX_WORDS {
            return Err(Refusal::new(DomainErrorCode::InvalidArgument, "text", None).into());
        }
        // The budget ends a page early; it never changes the order, so the cursor does not
        // bind it.
        let budget = search_budget(
            snapshot.configuration(),
            ctx,
            &scoped.exact,
            i.budget_tokens.as_ref().map(TokenBudget::get),
        )?;
        let lifecycles: Vec<String> = match &i.states {
            Some(s) if !s.0.is_empty() => s.0.iter().map(model::wire).collect(),
            _ => vec!["Active".into()],
        };
        let query = ClaimQuery {
            scopes: scoped.keys(),
            lifecycles,
            kinds: i.kinds.0.iter().map(model::wire).collect(),
            entry_kinds: i
                .entry_kinds
                .as_ref()
                .map(|k| k.0.iter().map(model::wire).collect())
                .unwrap_or_default(),
            tags: model::fold_tags(i.tags.0.iter().map(|t| t.0.as_str())),
            words: words.clone(),
            entity_limit: (!words.is_empty()).then_some(i.entity_limit.get() as u16),
            source: i.source.as_ref().map(|s| s.0.clone()),
            stale: i.stale,
            subject_id: None,
            lesson: None,
        };
        let limit = i.page.limit.get().min(i.claim_limit.get()) as usize;
        let k = knowledge_read(snapshot);
        let mut request = i.page.clone();
        request.limit = Limit::new(u32::try_from(limit).expect("page size")).expect("page size");
        let (mut parts, exact_revision) = Self::revision_parts(k, &scoped)?;
        parts.push(format!("{query:?}"));
        parts.push(limit.to_string());
        let th = thresholds(snapshot.configuration(), &scoped.readable)?;
        let wire = |c: &StoredClaim| with_skills(snapshot, ctx, claim_wire(k, c, &th)?);
        if words.is_empty() {
            // Without words: newest first, one store order.
            parts.push("lexical".into());
            let binding =
                CursorBinding::new(ctx, "knowledge.search", &refs(&parts), exact_revision);
            let rows = cursor::page(
                &binding,
                &request,
                |after, n| claims(k, &query, after, n),
                |c| claim_position(&query, c),
            )?;
            let rows = budgeted(
                rows,
                &binding,
                budget,
                |c| &c.claim,
                |c| claim_position(&query, c),
            )
            .try_map(|c| Ok::<_, StorageError>(c.claim))?;
            return Ok((
                search_result(
                    rows,
                    wire,
                    KnowledgeRetrievalMode::LexicalOnly,
                    false,
                    budget,
                )?,
                exact_revision,
            ));
        }
        // A continued listing keeps its first page's mode ([`fusion`]). A first page fuses
        // meaning matches when its query vector was embedded under the current selection.
        let continued = fusion::continued_mode(&request)?;
        let vector = match (self.embeddings(), prepared) {
            (Some(runtime), Some(v))
                if continued != Some(fusion::Mode::Lexical)
                    && runtime
                        .configuration_read(snapshot, ctx)?
                        .space
                        .is_some_and(|space| space.id == v.space.as_str())
                    && v.current(&runtime, snapshot, ctx)? =>
            {
                Some(v)
            }
            _ => None,
        };
        if continued == Some(fusion::Mode::Hybrid) && vector.is_none() {
            // Without a query vector the order is another one: the client starts again.
            return Err(StorageError::CursorExpired.into());
        }
        let near = vector.map(|v| ClaimVectorQuery {
            space: &v.space,
            vector: &v.vector,
        });
        let fused = fusion::rank(k, &query, near)?;
        let mode = if near.is_some() {
            fusion::Mode::Hybrid
        } else {
            fusion::Mode::Lexical
        };
        parts.extend(fusion::binding_parts(
            mode,
            vector.map(|v| (&v.space, v.embedded, v.vector.as_slice())),
        ));
        let binding = CursorBinding::new(ctx, "knowledge.search", &refs(&parts), exact_revision);
        let rows = fusion::page(k, &query, near, &binding, &request, &fused, budget)?;
        let mode = match mode {
            fusion::Mode::Hybrid => KnowledgeRetrievalMode::Hybrid,
            fusion::Mode::Lexical => KnowledgeRetrievalMode::LexicalOnly,
        };
        Ok((
            search_result(rows, wire, mode, fused.work_limited, budget)?,
            exact_revision,
        ))
    }

    fn tagged_lessons(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::TaggedLessonsInput,
    ) -> Result<(OperationResult, i64), Failure> {
        let scoped = resolve_scope(
            snapshot.artifacts(),
            snapshot.authority(),
            ctx,
            &i.scope,
            false,
        )?;
        let th = thresholds(snapshot.configuration(), &scoped.readable)?;
        let query = ClaimQuery {
            scopes: scoped.keys(),
            lifecycles: vec!["Active".into()],
            kinds: vec!["Lesson".into()],
            tags: model::fold_tags(i.tags.0.iter().map(|t| t.0.as_str())),
            lesson: Some(LessonFilter {
                rule_states: i
                    .rule_states
                    .as_ref()
                    .map(|s| s.0.iter().map(model::wire).collect())
                    .unwrap_or_default(),
                thresholds: th.iter().map(|(k, v)| (k.clone(), *v)).collect(),
                activity: i.activity.as_ref().map(|a| model::fold(&a.0)),
                view: i.view.as_ref().map(model::wire),
            }),
            ..ClaimQuery::default()
        };
        let limit = i.page.limit.get() as usize;
        let k = knowledge_read(snapshot);
        let (mut parts, exact_revision) = Self::revision_parts(k, &scoped)?;
        parts.push(format!("{query:?}"));
        parts.push(limit.to_string());
        let binding = CursorBinding::new(
            ctx,
            "knowledge.tagged_lessons",
            &refs(&parts),
            exact_revision,
        );
        let mut request = i.page.clone();
        request.limit = Limit::new(u32::try_from(limit).expect("page size")).expect("page size");
        let rows = cursor::page(
            &binding,
            &request,
            |after, n| claims(k, &query, after, n),
            |c| claim_position(&query, c),
        )?;
        Ok((
            OperationResult::KnowledgeTaggedLessons {
                value: rows
                    .try_into_page(|c| with_skills(snapshot, ctx, claim_wire(k, &c, &th)?))?,
            },
            exact_revision,
        ))
    }

    fn read(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::ReadInput,
    ) -> Result<(OperationResult, i64), Failure> {
        let scoped = resolve_scope(
            snapshot.artifacts(),
            snapshot.authority(),
            ctx,
            &i.scope,
            false,
        )?;
        let k = knowledge_read(snapshot);
        let claim = match k.knowledge_claim(&i.claim_id)? {
            Some(c) if scoped.sees(&c.scope) => c,
            _ => return Err(not_found(CLAIM, &i.claim_id).into()),
        };
        let th = thresholds(snapshot.configuration(), &scoped.readable)?;
        let (wire, at) = match &i.revision {
            Some(r) if r.get() != claim.revision => {
                let Some(h) = k.knowledge_history_at(&claim.id, r.get())? else {
                    return Err(not_found("knowledge_revision", &claim.id).into());
                };
                let (wire, entities) = snapshot_claim(&h.snapshot)?;
                (wire, Some(entities))
            }
            _ => (claim_wire(k, &claim, &th)?, None),
        };
        let mut bundle = bundle_of(k, &scoped, &claim, wire, &th, at)?;
        bundle.claims.0 = with_skills_all(snapshot, ctx, bundle.claims.0)?;
        Ok((
            OperationResult::KnowledgeRead { value: bundle },
            k.knowledge_scope_revision(&scoped.exact)?,
        ))
    }

    fn write_status(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::WriteStatusInput,
    ) -> Result<(OperationResult, i64), Failure> {
        let k = knowledge_read(snapshot);
        let reach = reach(snapshot.authority(), ctx)?;
        let write = match k.knowledge_write(&i.write_id)? {
            Some(w) if visible_from(ctx, &reach, &w.scope) => w,
            _ => return Err(not_found(WRITE, &i.write_id).into()),
        };
        let revision = k.knowledge_scope_revision(&write.scope)?;
        let window = match &i.cursor {
            None => 0,
            Some(c) => {
                let window: usize = receipt_binding(ctx, &write.write_id)
                    .decode(&c.0)?
                    .parse()
                    .map_err(|_| StorageError::CursorExpired)?;
                // Only a window that names something is a continuation.
                let start = window.saturating_mul(RECEIPT_WINDOW);
                if window == 0
                    || (start >= write.claim_ids.len()
                        && start >= write.entity_ids.len()
                        && start >= write.diagnostics.len())
                {
                    return Err(StorageError::CursorExpired.into());
                }
                window
            }
        };
        let mut value = receipt_of(ctx, &write, window);
        // A delete by source still running as a job: its receipt names what is done so far.
        if k.knowledge_source_deletion(&write.write_id)?
            .is_some_and(|j| j.state == "Running")
        {
            value.state = OperationState::Dispatching;
        }
        Ok((OperationResult::KnowledgeWriteStatus { value }, revision))
    }

    fn diagnostics(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::DiagnosticsInput,
    ) -> Result<(OperationResult, i64), Failure> {
        let scoped = resolve_scope(
            snapshot.artifacts(),
            snapshot.authority(),
            ctx,
            &i.scope,
            false,
        )?;
        let k = knowledge_read(snapshot);
        let exact_revision = k.knowledge_scope_revision(&scoped.exact)?;
        let mut items = Vec::new();
        // Text matches are lexical (SQLite FTS5); vector matches are added exactly when a
        // qualified embedding space is selected (Hybrid), and this says which.
        let (mode, message) =
            vectors::retrieval_mode(self.embeddings().as_deref(), snapshot, ctx, &scoped)?;
        items.push(diagnostic(
            "knowledge-retrieval",
            Severity::Info,
            RETRIEVAL_COMPONENT,
            mode,
            &message,
        ));
        let counts = k.knowledge_counts(&scoped.keys())?;
        for (lifecycle, n) in &counts.claims {
            items.push(diagnostic(
                &format!("count-claims-{}", lifecycle.to_lowercase()),
                Severity::Info,
                COUNTS_COMPONENT,
                &format!("claims.{lifecycle}"),
                &format!("{n} {} claims", lifecycle.to_lowercase()),
            ));
        }
        for (state, n) in &counts.lessons {
            items.push(diagnostic(
                &format!("count-lessons-{}", state.to_lowercase()),
                Severity::Info,
                COUNTS_COMPONENT,
                &format!("lessons.{state}"),
                &format!("{n} lessons stored as {}", state.to_lowercase()),
            ));
        }
        for (code, n, what) in [
            ("entities", counts.entities, "live entities"),
            ("evidence", counts.evidence, "live evidence records"),
            (
                "stale",
                counts.stale,
                "active claims whose sources changed since verification",
            ),
            ("challenges.Open", counts.open_challenges, "open challenges"),
        ] {
            items.push(diagnostic(
                &format!("count-{}", code.to_lowercase().replace('.', "-")),
                if code == "stale" && n > 0 {
                    Severity::Warning
                } else {
                    Severity::Info
                },
                COUNTS_COMPONENT,
                code,
                &format!("{n} {what}"),
            ));
        }
        for w in k.knowledge_recent_writes(&scoped.exact, 20)? {
            items.push(diagnostic(
                w.write_id.as_str(),
                Severity::Info,
                WRITES_COMPONENT,
                &w.operation,
                &format!(
                    "Committed by {} at {} ms: {} claims, {} entities, scope revision {}.",
                    w.actor.0.as_str(),
                    w.at_ms,
                    w.claim_ids.len(),
                    w.entity_ids.len(),
                    w.scope_revision
                ),
            ));
        }
        let source = cleanup_source(i.source_id.as_ref(), i.source_label.as_ref())?;
        if source.is_some() && i.import_source.is_some() {
            return Err(Refusal::new(DomainErrorCode::InvalidArgument, PREVIEW, None).into());
        }
        if let Some(source) = source {
            let plan = writes::deletion_plan(k, &scoped.exact, source, exact_revision)?;
            items.push(diagnostic(
                &plan.digest,
                Severity::Info,
                PREVIEW_COMPONENT,
                "DeleteBySource",
                &format!(
                    "Deleting source {} at scope revision {exact_revision} tombstones {} claims, \
                     {} entities and {} evidence records{}{}. Confirm with this diagnostic's id \
                     as previewDigest.",
                    source,
                    plan.claims,
                    plan.entities,
                    plan.evidence,
                    if plan.restored.is_empty() {
                        String::new()
                    } else {
                        format!(
                            ", and restores {} lessons merged into a deleted lesson as \
                             independent lessons (each listed in {RESTORE_COMPONENT})",
                            plan.restored.len()
                        )
                    },
                    if plan.claims > DELETION_BATCH {
                        " in batches (the source is read-only until the last one; \
                         knowledge.write_status reports progress)"
                    } else {
                        ""
                    }
                ),
            ));
            for (lesson, from) in &plan.restored {
                items.push(diagnostic(
                    &format!("restore-{}", lesson.as_str()),
                    Severity::Info,
                    RESTORE_COMPONENT,
                    "RestoresLesson",
                    &format!(
                        "Lesson {} is merged into {}, which the deletion removes: it becomes an \
                         independent lesson again with the source events its merge carried.",
                        lesson.as_str(),
                        from.as_str()
                    ),
                ));
            }
        }
        if let Some(source) = &i.import_source {
            let Some(schema) = &i.import_schema else {
                return Err(Refusal::new(DomainErrorCode::InvalidArgument, "schema", None).into());
            };
            writes::check_import_schema(schema)?;
            let candidates = self.read_import(snapshot, ctx, source)?;
            let (outcomes, digest) =
                writes::import_outcomes(k, &scoped.exact, source, &candidates)?;
            for (c, o) in candidates.iter().zip(&outcomes) {
                items.push(diagnostic(
                    &format!("import-candidate-{}", c.index),
                    if *o == import::Outcome::WouldStore {
                        Severity::Info
                    } else {
                        Severity::Warning
                    },
                    IMPORT_COMPONENT,
                    o.as_str(),
                    &format!(
                        "Candidate {} ({}, from {}): {}",
                        c.index, c.entry_kind, c.source_file, c.summary
                    ),
                ));
            }
            let stored = outcomes
                .iter()
                .filter(|o| **o == import::Outcome::WouldStore)
                .count();
            items.push(diagnostic(
                &digest,
                Severity::Info,
                PREVIEW_COMPONENT,
                "Import",
                &format!(
                    "Importing version {} stores {stored} of {} candidates. Confirm with this \
                     diagnostic's id as previewDigest.",
                    source.version_id.as_str(),
                    candidates.len()
                ),
            ));
        }
        items.truncate(500);
        Ok((
            OperationResult::KnowledgeDiagnostics {
                value: Page {
                    items: BoundedList(items),
                    next_cursor: None,
                    has_more: false,
                },
            },
            exact_revision,
        ))
    }

    /// Reads and parses an import source the invocation may see: its live version, with the
    /// exact digest and length the reference names, at most [`import::MAX_IMPORT_BYTES`].
    fn read_import(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        source: &ArtifactRef,
    ) -> Result<Vec<import::Candidate>, Failure> {
        let Some(files) = &self.files else {
            return Err(
                Refusal::new(DomainErrorCode::UnsupportedCapability, "import", None).into(),
            );
        };
        let viewer = crate::services::artifacts::Viewer::resolve(snapshot.authority(), ctx)?;
        let version = writes::import_version(snapshot.artifacts(), &viewer, source)?;
        if version.byte_count as u64 > import::MAX_IMPORT_BYTES {
            return Err(Refusal::new(DomainErrorCode::InvalidArgument, "import_size", None).into());
        }
        let bytes = files
            .read_range(
                &version.blob_digest,
                &version.blob_generation,
                version.byte_count as u64,
                0,
                version.byte_count as usize,
            )
            .map_err(|_| {
                Refusal::new(
                    DomainErrorCode::SourceChanged,
                    "artifact",
                    Some(source.artifact_id.clone()),
                )
            })?;
        import::parse(&bytes).map_err(|_| {
            Refusal::new(DomainErrorCode::InvalidArgument, "import_format", None).into()
        })
    }

    // -----------------------------------------------------------------------------------------
    // Host pipeline API (context assembly, indexing, durable jobs)

    /// Runs the next batch of the delete-by-source job of `write_id` (the delivery of its
    /// durable consumer, [`SOURCE_DELETION_CONSUMER`]) and queues the one after, returning
    /// whether another follows. A finished or unknown job is a no-op, so a repeated delivery
    /// after a restart is harmless. A batch cannot be refused by its own fence; any refusal is
    /// reported as an invalid record (a defect, never a partial batch: the unit rolls back).
    pub fn continue_source_deletion(
        &self,
        tx: &mut dyn WriteTransaction,
        write_id: &Id,
    ) -> Result<bool, StorageError> {
        writes::continue_source_deletion(tx, write_id).map_err(|f| match f {
            Failure::Storage(e) => e,
            Failure::Refused(r) => StorageError::InvalidRecord(format!(
                "source deletion batch refused: {:?} {}",
                r.code, r.target_kind
            )),
        })
    }

    /// Records that `session` was supplied `rules` (claim and revision) in the same unit of
    /// work as the context that carried them (KNOW-06). Unknown sessions and unknown or deleted
    /// claims are refused.
    pub fn record_delivery(
        &self,
        tx: &mut dyn WriteTransaction,
        session: &Id,
        rules: &[(Id, i64)],
    ) -> Result<(), StorageError> {
        record_delivery(tx, session, rules)
    }

    /// Records the current revision of a source (a repository path, a document version); claims
    /// recorded or verified against an earlier revision read stale until verified again.
    /// Returns the claims whose stale status changed (stale now, or current again).
    pub fn observe_source(
        &self,
        tx: &mut dyn WriteTransaction,
        source_ref: &str,
        revision: &str,
    ) -> Result<Vec<Id>, StorageError> {
        observe_source(tx, source_ref, revision)
    }

    /// Every effective rule a session of `project` (`None`: a workspace session) may be given:
    /// the project's and general rules, read in full (never only a first page), filtered by
    /// applicability `activity` when given, in newest-first order (CONTEXT seam).
    pub fn rules_for(
        &self,
        snapshot: &dyn ReadSnapshot,
        project: Option<&ProjectId>,
        activity: Option<&str>,
    ) -> Result<Vec<KnowledgeClaim>, StorageError> {
        rules_for(snapshot, project, activity)
    }
}

/// See [`KnowledgeService::record_delivery`].
pub fn record_delivery(
    tx: &mut dyn WriteTransaction,
    session: &Id,
    rules: &[(Id, i64)],
) -> Result<(), StorageError> {
    let Some(s) = tx.sessions().session(session)? else {
        return Err(StorageError::NotFound {
            kind: "session",
            id: session.as_str().to_owned(),
        });
    };
    let now = tx.now_ms();
    let mut touched: Vec<KnowledgeScope> = Vec::new();
    for (claim, revision) in rules {
        // Only a live rule the session may be given: general, or of the session's own project.
        let scope = match tx.knowledge().knowledge_claim(claim)? {
            Some(c)
                if c.revision >= *revision
                    && *revision >= 1
                    && c.deleted_at_ms.is_none()
                    && c.scope
                        .project()
                        .is_none_or(|p| Some(p) == s.project_id.as_ref()) =>
            {
                c.scope
            }
            _ => {
                return Err(StorageError::NotFound {
                    kind: CLAIM,
                    id: claim.as_str().to_owned(),
                });
            }
        };
        if tx
            .knowledge()
            .record_knowledge_delivery(session, claim, *revision, now)?
            && !touched.contains(&scope)
        {
            touched.push(scope);
        }
    }
    // A delivery changes the Undelivered view's membership: its listings' cursors expire.
    for scope in &touched {
        tx.knowledge().bump_scope(scope)?;
    }
    Ok(())
}

/// See [`KnowledgeService::observe_source`].
pub fn observe_source(
    tx: &mut dyn WriteTransaction,
    source_ref: &str,
    revision: &str,
) -> Result<Vec<Id>, StorageError> {
    if source_ref.is_empty() || revision.is_empty() {
        return Err(StorageError::InvalidRecord(
            "source and revision are required".into(),
        ));
    }
    let now = tx.now_ms();
    let changed = tx
        .knowledge()
        .observe_knowledge_source(source_ref, revision, now)?;
    // A changed stale status changes what a stale-filtered listing returns: every scope it
    // touched advances, so a cursor read before it expires instead of skipping a claim.
    let mut scopes: Vec<KnowledgeScope> = Vec::new();
    for (_, scope) in &changed {
        if !scopes.contains(scope) {
            scopes.push(scope.clone());
        }
    }
    for scope in &scopes {
        tx.knowledge().bump_scope(scope)?;
    }
    Ok(changed.into_iter().map(|(id, _)| id).collect())
}

/// The query of every effective rule of `project` and general knowledge (newest first), with
/// the recurrence thresholds it applies.
fn effective_rules_query(
    snapshot: &dyn ReadSnapshot,
    project: Option<&ProjectId>,
    activity: Option<&str>,
) -> Result<(ClaimQuery, BTreeMap<String, i64>), StorageError> {
    let mut scopes = vec![KnowledgeScope::Workspace];
    if let Some(p) = project {
        scopes.insert(0, KnowledgeScope::Project(p.clone()));
    }
    let th = thresholds(snapshot.configuration(), &scopes)?;
    let query = ClaimQuery {
        scopes: scopes.iter().map(KnowledgeScope::key).collect(),
        lifecycles: vec!["Active".into()],
        kinds: vec!["Lesson".into()],
        lesson: Some(LessonFilter {
            rule_states: vec!["Effective".into()],
            thresholds: th.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            activity: activity.map(model::fold),
            view: None,
        }),
        ..ClaimQuery::default()
    };
    Ok((query, th))
}

/// Every effective rule a session of `project` may be given (see [`KnowledgeService::rules_for`]).
pub fn rules_for(
    snapshot: &dyn ReadSnapshot,
    project: Option<&ProjectId>,
    activity: Option<&str>,
) -> Result<Vec<KnowledgeClaim>, StorageError> {
    let (query, th) = effective_rules_query(snapshot, project, activity)?;
    let k = knowledge_read(snapshot);
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let rows = claims(k, &query, after.as_deref(), KNOWLEDGE_PAGE_MAX)?;
        for c in &rows {
            out.push(claim_wire(k, c, &th)?);
        }
        match rows.last() {
            Some(last) if rows.len() == usize::from(KNOWLEDGE_PAGE_MAX) => {
                after = Some(claim_position(&query, last))
            }
            _ => return Ok(out),
        }
    }
}

/// The knowledge source a repository file is cited as: `repo:<repository-id>/<path>`, the
/// path repository-relative with `/` separators. Its revision is the content digest of the
/// file's unit in the repository's published index baseline (KNOW-03).
pub fn repository_source_ref(repository: &Id, path: &str) -> String {
    format!("{}{path}", repository_source_prefix(repository))
}

/// The prefix every [`repository_source_ref`] of `repository` starts with.
pub fn repository_source_prefix(repository: &Id) -> String {
    format!("repo:{}/", repository.as_str())
}

/// The prefix of every repository file citation.
pub const REPOSITORY_SOURCE_SCHEME: &str = "repo:";

/// The repository a `repo:<repository-id>/<path>` citation names, with its path normalized
/// (empty and `.` segments and a leading `/` dropped), or `None` for a path that cannot be
/// normalized (empty, a `..` segment, a backslash or a control character). `None` overall
/// when `source_ref` has no such shape.
pub fn repository_source(source_ref: &str) -> Option<(Id, Option<String>)> {
    let rest = source_ref.strip_prefix(REPOSITORY_SOURCE_SCHEME)?;
    let (repository, path) = rest.split_once('/')?;
    let repository = Id::new(repository).ok()?;
    let normalized = (|| {
        if path.contains('\\') || path.chars().any(char::is_control) {
            return None;
        }
        let mut parts = Vec::new();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => return None,
                part => parts.push(part),
            }
        }
        (!parts.is_empty()).then(|| parts.join("/"))
    })();
    Some((repository, normalized))
}

/// The content digest of `path` in the published index baseline of `repository`; `None` when
/// the repository has no publication or its baseline holds no such file.
pub fn published_source_revision(
    indexing: &dyn crate::ports::storage::IndexRead,
    repository: &Id,
    path: &str,
) -> Result<Option<String>, StorageError> {
    let Some(published) = indexing.published(repository)? else {
        return Ok(None);
    };
    let Some(view) = indexing.view(&published.view_id)? else {
        return Ok(None);
    };
    Ok(indexing
        .unit(&view.baseline_id, path)?
        .map(|unit| unit.content_digest))
}

/// Whether a rule restricted to repositories applies to a session whose captured repository
/// scope is `repositories` (an unrestricted rule always does).
pub(crate) fn applies_to_repositories(applicability: &Applicability, repositories: &[Id]) -> bool {
    applicability.repository_ids.0.is_empty()
        || applicability
            .repository_ids
            .0
            .iter()
            .any(|r| repositories.contains(r))
}

/// The harness a native DevWorks session runs in: a rule whose applicability names another
/// harness is never bound into its context.
pub const NATIVE_HARNESS: &str = "devworks";

/// The lesson rules a session's provider context carries, and whether more applied.
#[derive(Debug, Default)]
pub struct LessonSelection {
    pub rules: Vec<crate::providers::instructions::LessonRule>,
    /// More relevant rules exist than [`MAX_STAMPED_LESSONS`] or [`MAX_LESSON_TEXT_BYTES`]
    /// allow; the context says so.
    ///
    /// [`MAX_STAMPED_LESSONS`]: crate::providers::instructions::MAX_STAMPED_LESSONS
    /// [`MAX_LESSON_TEXT_BYTES`]: crate::providers::instructions::MAX_LESSON_TEXT_BYTES
    pub truncated: bool,
}

/// The effective lesson rules a session's provider context carries (KNOW-06): its project's
/// and general rules in newest-first order, without those restricted to repositories outside
/// the session's captured scope, to another harness than [`NATIVE_HARNESS`], or to tools none
/// of `offered_tools` (wire names or capability ids) names. Activity and path conditions are
/// rendered with the rule. The read stops once the bounds are filled and reads only the stored
/// claims (no tallies or challenges).
pub fn session_lesson_rules(
    snapshot: &dyn ReadSnapshot,
    session: &Id,
    offered_tools: &[String],
) -> Result<LessonSelection, StorageError> {
    use crate::providers::instructions::{LessonRule, MAX_LESSON_TEXT_BYTES, MAX_STAMPED_LESSONS};
    let Some(stored) = snapshot.sessions().session(session)? else {
        return Ok(LessonSelection::default());
    };
    let repositories = snapshot.sessions().session_repository_scope(session)?;
    let tools: std::collections::BTreeSet<String> =
        offered_tools.iter().map(|t| model::fold(t)).collect();
    let (query, _) = effective_rules_query(snapshot, stored.project_id.as_ref(), None)?;
    let k = knowledge_read(snapshot);
    let mut out = LessonSelection::default();
    let mut bytes = 0usize;
    let mut after: Option<String> = None;
    loop {
        let rows = claims(k, &query, after.as_deref(), KNOWLEDGE_PAGE_MAX)?;
        for c in &rows {
            let (Some(lesson), Some(text)) = (&c.lesson, &c.body.value) else {
                continue;
            };
            let a: Applicability = serde_json::from_value(lesson.applicability.clone())
                .map_err(|_| StorageError::InvalidRecord("stored applicability".into()))?;
            let a = model::fold_applicability(&a);
            if !applies_to_repositories(&a, &repositories)
                || a.harness.as_ref().is_some_and(|h| h.0 != NATIVE_HARNESS)
                || (!a.tools.0.is_empty() && !a.tools.0.iter().any(|t| tools.contains(&t.0)))
            {
                continue;
            }
            let rule = LessonRule {
                claim_id: c.id.clone(),
                revision: c.revision,
                text: text.clone(),
                activities: a.activities.0.iter().map(|v| v.0.clone()).collect(),
                paths: a.paths.0.iter().map(|v| v.0.clone()).collect(),
            };
            let line = rule.line().len();
            if out.rules.len() == MAX_STAMPED_LESSONS || bytes + line > MAX_LESSON_TEXT_BYTES {
                out.truncated = true;
                return Ok(out);
            }
            bytes += line;
            out.rules.push(rule);
        }
        match rows.last() {
            Some(last) if rows.len() == usize::from(KNOWLEDGE_PAGE_MAX) => {
                after = Some(claim_position(&query, last))
            }
            _ => return Ok(out),
        }
    }
}

/// Whether a write of `scope` is visible from this invocation.
pub(crate) fn visible_from(ctx: &InvocationContext, reach: &Reach, scope: &KnowledgeScope) -> bool {
    match scope {
        KnowledgeScope::Workspace => true,
        KnowledgeScope::Project(p) => {
            ctx.project_id().is_none_or(|q| q == p)
                && match reach {
                    Reach::Workspace => true,
                    Reach::Project(q) => q == p,
                }
        }
    }
}

/// A lesson with the skills whose revisions name it that the invocation may see (the lesson's
/// `skills`, absent when there are none). Only skills are listed; a role revision that names the
/// lesson shows it in its own history. A skill or revision hidden from the invocation (another
/// project's skill, a draft confined to another project) is left out.
pub(crate) fn with_skills(
    snapshot: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    mut claim: KnowledgeClaim,
) -> Result<KnowledgeClaim, StorageError> {
    // A deleted claim shows no lesson content: only its identity remains.
    if matches!(claim.lifecycle, Some(KnowledgeLifecycle::Deleted)) {
        return Ok(claim);
    }
    let Some(lesson) = claim.lesson.as_mut() else {
        return Ok(claim);
    };
    let links = crate::services::instructions::lesson_skill_links(
        snapshot.instructions(),
        ctx,
        &claim.id,
        crate::services::instructions::LESSON_LINK_PAGE,
    )?;
    if !links.is_empty() {
        lesson.skills = Some(BoundedList(links));
    }
    Ok(claim)
}

/// [`with_skills`] over every claim of a result.
pub(crate) fn with_skills_all(
    snapshot: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    claims: Vec<KnowledgeClaim>,
) -> Result<Vec<KnowledgeClaim>, StorageError> {
    claims
        .into_iter()
        .map(|c| with_skills(snapshot, ctx, c))
        .collect()
}

/// Whether a claim is a lesson the invocation may name or list: it exists (a deleted claim keeps
/// only its identity) and its scope is visible from the invocation.
pub(crate) fn lesson_visible(ctx: &InvocationContext, reach: &Reach, claim: &StoredClaim) -> bool {
    claim.lesson.is_some() && claim.lifecycle != "Deleted" && visible_from(ctx, reach, &claim.scope)
}

fn refs(parts: &[String]) -> Vec<&str> {
    parts.iter().map(String::as_str).collect()
}

/// The Knowledge view of a snapshot.
fn knowledge_read(snapshot: &dyn ReadSnapshot) -> &dyn KnowledgeRead {
    snapshot.knowledge()
}

/// A summary published during preparation.
struct PublishedSummary {
    reference: ArtifactRef,
    revision: i64,
}

impl KnowledgeService {
    fn graph_bundle(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        operation: &'static str,
        scope: &DataScope,
        query: &crate::protocol::records::KnowledgeQuery,
    ) -> Result<(OperationResult, i64), Failure> {
        let (read, revision) = self.graph_read(snapshot, ctx, operation, scope, query)?;
        let graph::Read::Bundle(value) = read else {
            return Err(Refusal::new(
                DomainErrorCode::InvalidArgument,
                "knowledge_operation",
                None,
            )
            .into());
        };
        Ok((
            match operation {
                "knowledge.subgraph" => OperationResult::KnowledgeSubgraph { value },
                "knowledge.entity_bundle" => OperationResult::KnowledgeEntityBundle { value },
                "knowledge.entity_graph" => OperationResult::KnowledgeEntityGraph { value },
                "knowledge.claim_bundle" => OperationResult::KnowledgeClaimBundle { value },
                "knowledge.expand_frontier" => OperationResult::KnowledgeExpandFrontier { value },
                _ => OperationResult::KnowledgeQuery { value },
            },
            revision,
        ))
    }
}

/// The most supersession links a bundle lists: the wire list's bound. A chain is walked in both
/// directions from a claim and each link is a claim the reader can see, so the only reason to stop
/// early is that the list cannot carry more; a shorter bound would drop links silently and make
/// two reads of one claim disagree.
pub(crate) const MAX_SUPERSESSION: usize = 128;

/// Appends to `chain` the claims `start` supersedes and the claims that supersede it, following
/// `supersedes` and `superseded_by` (the links of the revision being shown) through the stored
/// claims the reader can see. Claims already in `chain` are skipped. Returns whether a visible
/// link remained past `MAX_SUPERSESSION`.
pub(crate) fn supersession_chain(
    k: &dyn KnowledgeRead,
    scoped: &Scoped,
    start: &Id,
    supersedes: Option<Id>,
    superseded_by: Option<Id>,
    chain: &mut Vec<Id>,
) -> Result<bool, StorageError> {
    let mut cut = false;
    for backward in [true, false] {
        let mut next = if backward {
            supersedes.clone()
        } else {
            superseded_by.clone()
        };
        let mut walked: std::collections::HashSet<Id> = std::collections::HashSet::new();
        while let Some(id) = next.take() {
            if &id == start || chain.contains(&id) || !walked.insert(id.clone()) {
                break;
            }
            let Some(found) = k.knowledge_claim(&id)?.filter(|c| scoped.sees(&c.scope)) else {
                break;
            };
            // A deleted claim is not listed, but the chain goes on through it.
            let listed = found.lifecycle != "Deleted";
            if listed && chain.len() >= MAX_SUPERSESSION {
                cut = true;
                break;
            }
            next = if backward {
                found.body.supersedes.clone()
            } else {
                found.body.superseded_by.clone()
            };
            if listed {
                chain.push(id);
            }
        }
    }
    Ok(cut)
}

/// A conflict and the claims it names besides the claim it was built for.
pub(crate) type ValueConflict = Option<(KnowledgeConflict, Vec<StoredClaim>)>;

/// What one build of conflicts has already looked up: the stored claims about each subject and
/// predicate, and whether each was admitted, so a group of claims costs one lookup and one
/// admission per claim however many of its claims are asked about.
#[derive(Default)]
pub(crate) struct ConflictLookups {
    about: std::collections::HashMap<(Id, String), (Vec<StoredClaim>, bool)>,
    admitted: std::collections::HashMap<Id, bool>,
}

/// The open conflict a group of claims makes: the active claims about one subject and predicate
/// (the claim asked about, and at most 127 others that `admit` accepts) when any of the others
/// states it differently. Every claim of the group asks for the same conflict, so a group yields
/// one conflict (named for its lowest claim ID). Returns the conflict with the other claims and
/// whether the read was cut.
pub(crate) fn value_conflict(
    k: &dyn KnowledgeRead,
    scoped: &Scoped,
    claim: &StoredClaim,
    lookups: &mut ConflictLookups,
    admit: &mut dyn FnMut(&StoredClaim) -> Result<bool, StorageError>,
) -> Result<(ValueConflict, bool), StorageError> {
    let key = (claim.subject_id.clone(), claim.predicate.clone());
    if !lookups.about.contains_key(&key) {
        let found =
            k.knowledge_claims_about(&scoped.keys(), &claim.subject_id, &claim.predicate, 129)?;
        let cut = found.len() > 128;
        lookups
            .about
            .insert(key.clone(), (found.into_iter().take(128).collect(), cut));
    }
    let (others, cut) = &lookups.about[&key];
    let cut = *cut;
    let mut group: Vec<StoredClaim> = Vec::new();
    let mut differs = false;
    for o in others.iter().filter(|o| o.id != claim.id) {
        let accepted = match lookups.admitted.get(&o.id) {
            Some(known) => *known,
            None => {
                let decided = admit(o)?;
                lookups.admitted.insert(o.id.clone(), decided);
                decided
            }
        };
        if accepted {
            differs |= o.body.value != claim.body.value
                || o.body.value_json != claim.body.value_json
                || o.object_id != claim.object_id;
            group.push(o.clone());
        }
    }
    if !differs {
        return Ok((None, cut));
    }
    group.truncate(127);
    let mut ids: Vec<Id> = std::iter::once(claim.id.clone())
        .chain(group.iter().map(|o| o.id.clone()))
        .collect();
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let conflict = KnowledgeConflict {
        id: Id::new(
            format!("conflict-{}", ids[0].as_str())
                .chars()
                .take(128)
                .collect::<String>(),
        )
        .map_err(|_| StorageError::InvalidRecord("conflict id".into()))?,
        state: label("Open", "Open"),
        explanation: content(&format!(
            "{} active claims about the same subject state {} differently.",
            ids.len(),
            claim.predicate
        )),
        claim_ids: BoundedList(ids),
    };
    Ok((Some((conflict, group)), cut))
}

/// The open conflict a recorded `contradicts` relation of `claim` makes, with the other claim. It
/// is open only while both claims are active and the other is visible.
pub(crate) fn contradiction(
    k: &dyn KnowledgeRead,
    scoped: &Scoped,
    claim: &StoredClaim,
    r: &crate::ports::storage::StoredRelation,
) -> Result<Option<(KnowledgeConflict, StoredClaim)>, StorageError> {
    if r.predicate != model::CONTRADICTS || claim.lifecycle != "Active" {
        return Ok(None);
    }
    let other = if r.subject_id == claim.id {
        &r.object_id
    } else {
        &r.subject_id
    };
    Ok(match k.knowledge_claim(other)? {
        Some(o) if scoped.sees(&o.scope) && o.lifecycle == "Active" => Some((
            KnowledgeConflict {
                id: r.id.clone(),
                claim_ids: BoundedList(vec![claim.id.clone(), o.id.clone()]),
                state: label("Open", "Open"),
                explanation: content("Recorded as contradicting each other."),
            },
            o,
        )),
        _ => None,
    })
}

/// A claim with its entities, evidence, conflicts, supersession chain, relations and history,
/// restricted to what the invocation may see.
pub(crate) fn bundle_of(
    k: &dyn KnowledgeRead,
    scoped: &Scoped,
    claim: &StoredClaim,
    wire: KnowledgeClaim,
    th: &BTreeMap<String, i64>,
    at: Option<BTreeMap<String, i64>>,
) -> Result<KnowledgeBundle, StorageError> {
    // Entities and evidence are the ones the shown revision names (`wire`), not the current
    // revision's; a past revision shows each entity as it was at that revision's time.
    let mut entities = Vec::new();
    for id in std::iter::once(&wire.subject_id).chain(wire.object_id.iter()) {
        let Some(e) = k.knowledge_entity(id)? else {
            continue;
        };
        if !scoped.sees(&e.scope) || entities.iter().any(|x: &KnowledgeEntity| x.id == e.id) {
            continue;
        }
        entities.push(match &at {
            Some(revisions) => entity_at(k, &e, revisions)?,
            None => entity_wire(&e),
        });
    }
    let mut evidence = Vec::new();
    let mut cited = wire.evidence_ids.0.clone();
    for challenge in wire.challenges.iter().flat_map(|list| &list.0) {
        for id in &challenge.evidence_ids.0 {
            if !cited.contains(id) {
                cited.push(id.clone());
            }
        }
    }
    let historical = at.is_some();
    let mut claims = vec![wire.clone()];
    let mut conflicts = Vec::new();
    let mut truncated = false;
    if claim.lifecycle == "Active" && !historical {
        let (found, cut) = value_conflict(
            k,
            scoped,
            claim,
            &mut ConflictLookups::default(),
            &mut |_| Ok(true),
        )?;
        truncated |= cut;
        if let Some((conflict, different)) = found {
            conflicts.push(conflict);
            // Every conflicting claim the conflict names is in the bundle (at most 127).
            for o in &different {
                claims.push(claim_wire(k, o, th)?);
            }
        }
    }
    // Relations and contradictions are current facts: a past revision is shown without them.
    let relations = if historical {
        Vec::new()
    } else {
        k.knowledge_relations(&claim.id, &scoped.keys(), 129)?
    };
    truncated |= relations.len() > 128;
    let visible_relations: Vec<_> = relations
        .iter()
        .take(128)
        .filter(|r| scoped.sees(&r.scope))
        .collect();
    for r in &visible_relations {
        if let Some((conflict, o)) = contradiction(k, scoped, claim, r)? {
            conflicts.push(conflict);
            if !claims.iter().any(|c| c.id == o.id) {
                claims.push(claim_wire(k, &o, th)?);
            }
        }
    }
    for relation in &visible_relations {
        for id in &relation.evidence_ids {
            if !cited.contains(id) {
                cited.push(id.clone());
            }
        }
    }
    for id in &cited {
        if let Some(e) = k.knowledge_evidence(id)?
            && e.deleted_at_ms.is_none()
            && scoped.sees(&e.scope)
        {
            evidence.push(evidence_wire(&e, Some(&claim.id))?);
        }
    }
    truncated |= evidence.len() > 500;
    let mut supersession = Vec::new();
    truncated |= supersession_chain(
        k,
        scoped,
        &claim.id,
        wire.supersedes.clone(),
        wire.superseded_by.clone(),
        &mut supersession,
    )?;
    let history = k.knowledge_history(&claim.id, KNOWLEDGE_PAGE_MAX)?;
    truncated |= history.len() >= KNOWLEDGE_PAGE_MAX as usize;
    Ok(KnowledgeBundle {
        entities: BoundedList(entities),
        claims: BoundedList(claims),
        evidence: BoundedList(evidence.into_iter().take(500).collect()),
        frontier: BoundedList(Vec::new()),
        next_cursor: None,
        truncated,
        conflicts: BoundedList(conflicts.into_iter().take(500).collect()),
        supersession_ids: BoundedList(supersession.into_iter().take(128).collect()),
        summary: None,
        relations: (!historical).then(|| {
            BoundedList(
                visible_relations
                    .iter()
                    .map(|r| KnowledgeRelation {
                        id: r.id.clone(),
                        subject_id: r.subject_id.clone(),
                        predicate: label(&r.predicate, "-"),
                        object_id: r.object_id.clone(),
                        evidence_ids: id_list(&r.evidence_ids),
                        author_id: r.author.clone(),
                        created_at_ms: counter(r.created_at_ms),
                    })
                    .collect(),
            )
        }),
        revisions: Some(BoundedList(
            history
                .iter()
                .take(500)
                .map(history_wire)
                .collect::<Result<_, _>>()?,
        )),
        scope_revision: Some(rev(k.knowledge_scope_revision(&scoped.exact)?)),
    })
}

/// The member of a claim snapshot naming the revisions of its subject and object entities.
pub(crate) const SNAPSHOT_ENTITIES: &str = "$entities";

/// A claim revision snapshot: the wire claim and the entity revisions it named.
fn snapshot_claim(
    snapshot: &Value,
) -> Result<(KnowledgeClaim, BTreeMap<String, i64>), StorageError> {
    let mut v = snapshot.clone();
    let entities = v
        .as_object_mut()
        .and_then(|m| m.remove(SNAPSHOT_ENTITIES))
        .and_then(|e| serde_json::from_value::<BTreeMap<String, i64>>(e).ok())
        .unwrap_or_default();
    let wire = serde_json::from_value::<KnowledgeClaim>(v)
        .map_err(|_| StorageError::InvalidRecord("stored claim revision snapshot".into()))?;
    Ok((wire, entities))
}

/// An entity at the revision a past claim revision named. A deleted entity, or a revision
/// that is not recorded (or was purged), shows the entity as it is now (for a deleted entity,
/// its tombstone).
fn entity_at(
    k: &dyn KnowledgeRead,
    current: &StoredEntity,
    revisions: &BTreeMap<String, i64>,
) -> Result<KnowledgeEntity, StorageError> {
    if current.deleted_at_ms.is_none()
        && let Some(r) = revisions.get(current.id.as_str())
        && *r != current.revision
        && let Some(h) = k.knowledge_history_at(&current.id, *r)?
        && let Ok(e) = serde_json::from_value::<KnowledgeEntity>(h.snapshot)
    {
        return Ok(e);
    }
    Ok(entity_wire(current))
}

/// Applicability with every list empty (a lesson recorded without one).
pub(crate) fn no_applicability() -> Applicability {
    Applicability {
        activities: BoundedList(Vec::new()),
        paths: BoundedList(Vec::<RelativePath>::new()),
        tools: BoundedList(Vec::new()),
        harness: None,
        repository_ids: BoundedList(Vec::new()),
    }
}

impl crate::services::conventions::indexing::SelectionListener for KnowledgeService {
    /// A newly selected space queues the embedding of every scope with claims it has no
    /// vector for, so claims written before it was selected need no other write.
    fn selection_changed(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        fences: &[crate::ports::AuthorityFence],
    ) -> Result<(), StorageError> {
        if self.embeddings().is_none() {
            return Ok(());
        }
        vectors::queue_selection(tx, ctx, fences)
    }
}

/// Queues the embedding of `scope`'s pending claims as `ctx` after a Knowledge change made
/// outside the Knowledge commands (Dreaming), in its unit of work.
pub(crate) fn queue_claim_vectors(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    scope: &KnowledgeScope,
) -> Result<(), StorageError> {
    vectors::queue_scope(tx, ctx, &[], scope, None)
}

impl QueryHandler for KnowledgeService {
    /// `knowledge.render_summary` renders from one snapshot and publishes the text as an
    /// artifact of the invocation's scope before the answer (bounded, recoverable publication).
    fn prepare(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        // `knowledge.search` embeds its text before the snapshot, through the shared route.
        if let OperationInput::Knowledge(ops::Input::Search(i)) = query.input() {
            let Some(runtime) = self.embeddings() else {
                return Ok(None);
            };
            return vectors::prepare_search(&runtime, ctx, query, i, env)
                .map(|v| v.map(Prepared::new))
                .map_err(|f| query_fault(f, CLAIM, self.correlation()));
        }
        let OperationInput::Knowledge(ops::Input::RenderSummary(i)) = query.input() else {
            return Ok(None);
        };
        let Some((store, artifacts)) = &self.summaries else {
            return Err(Refusal::new(
                DomainErrorCode::UnsupportedCapability,
                "knowledge_summary",
                None,
            )
            .fault(self.correlation()));
        };
        let rendered = env
            .read(|snapshot| {
                Ok(self.graph_read(
                    snapshot,
                    ctx,
                    "knowledge.render_summary",
                    &i.scope,
                    &i.query,
                ))
            })
            .map_err(|e| query_fault(e.into(), CLAIM, self.correlation()))?
            .map_err(|f| query_fault(f, CLAIM, self.correlation()))?;
        let (graph::Read::Summary(text), revision) = rendered else {
            return Err(
                Refusal::new(DomainErrorCode::InvalidArgument, "knowledge_summary", None)
                    .fault(self.correlation()),
            );
        };
        let project = match &i.scope {
            DataScope::Workspace => None,
            DataScope::Project { project_id } => Some(project_id.clone()),
        };
        let published = artifacts.publish_observed(
            &**store,
            ctx,
            query.authorization(),
            crate::services::artifacts::observed::Observed {
                project,
                title: "Knowledge summary".into(),
                media_type: "text/markdown; charset=utf-8".into(),
                tags: vec!["knowledge-summary".into()],
                source: serde_json::json!({
                    "kind": "KnowledgeSummary",
                    "entityIds": i.query.entity_ids,
                    "claimIds": i.query.claim_ids,
                    "scopeRevision": revision,
                }),
                actor_audience: true,
            },
            text.as_bytes(),
            |_, _| Ok(()),
        );
        match published {
            Ok((reference, ())) => Ok(Some(Prepared::new(PublishedSummary {
                reference,
                revision,
            }))),
            Err(crate::services::artifacts::observed::ObservedFailure::Refused(r)) => {
                Err(r.fault(self.correlation()))
            }
            Err(crate::services::artifacts::observed::ObservedFailure::Storage(e)) => {
                Err(query_fault(e.into(), CLAIM, self.correlation()))
            }
        }
    }
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let OperationInput::Knowledge(input) = query.input() else {
            return Err(
                Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None)
                    .fault(self.correlation()),
            );
        };
        let answered = match input {
            ops::Input::Search(i) => {
                self.search(snapshot, ctx, i, query.prepared::<vectors::QueryVector>())
            }
            ops::Input::Read(i) => self.read(snapshot, ctx, i),
            ops::Input::Diagnostics(i) => self.diagnostics(snapshot, ctx, i),
            ops::Input::WriteStatus(i) => self.write_status(snapshot, ctx, i),
            ops::Input::TaggedLessons(i) => self.tagged_lessons(snapshot, ctx, i),
            ops::Input::Subgraph(i) => {
                self.graph_bundle(snapshot, ctx, "knowledge.subgraph", &i.scope, &i.query)
            }
            ops::Input::EntityBundle(i) => {
                self.graph_bundle(snapshot, ctx, "knowledge.entity_bundle", &i.scope, &i.query)
            }
            ops::Input::EntityGraph(i) => {
                self.graph_bundle(snapshot, ctx, "knowledge.entity_graph", &i.scope, &i.query)
            }
            ops::Input::ClaimBundle(i) => {
                self.graph_bundle(snapshot, ctx, "knowledge.claim_bundle", &i.scope, &i.query)
            }
            ops::Input::ExpandFrontier(i) => self.graph_bundle(
                snapshot,
                ctx,
                "knowledge.expand_frontier",
                &i.scope,
                &i.query,
            ),
            ops::Input::Query(i) => {
                self.graph_bundle(snapshot, ctx, "knowledge.query", &i.scope, &i.query)
            }
            ops::Input::RenderSummary(_) => match query.prepared::<PublishedSummary>() {
                Some(p) => Ok((
                    OperationResult::KnowledgeRenderSummary {
                        value: p.reference.clone(),
                    },
                    p.revision,
                )),
                None => Err(Refusal::new(
                    DomainErrorCode::UnsupportedCapability,
                    "knowledge_summary",
                    None,
                )
                .into()),
            },
            _ => {
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into())
            }
        };
        let (result, scope_revision) =
            answered.map_err(|f| query_fault(f, CLAIM, self.correlation()))?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(scope_revision)),
            result,
        })
    }
}

impl CommandHandler for KnowledgeService {
    /// An import reads and parses its immutable source before the unit of work (bounded file
    /// I/O never runs inside the write transaction). Nothing visible is staged.
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        // A delete by source plans its whole set before the unit of work: for a large source
        // that is a long read, which never runs inside the write transaction. It is used only
        // at the same scope revision, after the unit of work authorizes the request.
        if let OperationInput::Knowledge(ops::Input::DeleteBySource(i)) = command.input() {
            let scope = KnowledgeScope::from(&i.scope);
            let Some(source) = cleanup_source(i.source_id.as_ref(), i.source_label.as_ref())
                .ok()
                .flatten()
            else {
                // Admission records the precise semantic refusal; no planning is needed.
                return Ok(None);
            };
            let prepared = env.read(|snapshot| {
                let k = knowledge_read(snapshot);
                let scope_revision = k.knowledge_scope_revision(&scope)?.max(1);
                let plan = writes::deletion_plan(k, &scope, source, scope_revision)?;
                Ok(writes::PreparedDeletion {
                    scope_key: scope.key(),
                    source: source.to_owned(),
                    scope_revision,
                    plan,
                })
            })?;
            return Ok(Some(Prepared::new(prepared)));
        }
        let OperationInput::Knowledge(ops::Input::ImportUserData(i)) = command.input() else {
            return Ok(None);
        };
        if env.stopped(self.clock.now_ms().get()) {
            return Err(StorageError::Cancelled);
        }
        let parsed = env.read(|snapshot| {
            Ok(match self.read_import(snapshot, ctx, &i.source) {
                Ok(c) => Ok(c),
                Err(Failure::Refused(r)) => Err(r),
                Err(Failure::Storage(e)) => return Err(e),
            })
        })?;
        Ok(Some(Prepared::new(parsed)))
    }

    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let OperationInput::Knowledge(input) = command.input() else {
            let refused =
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into());
            return decided(tx, refused);
        };
        let mut outcome = writes::decide(ctx, command, input, tx);
        if let (Some(_), Ok(crate::protocol::results::DomainDecision::Applied { result })) =
            (self.embeddings(), &outcome)
            && let Err(error) = vectors::queue_after_write(tx, ctx, command, result)
        {
            outcome = Err(error.into());
        }
        decided(tx, outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::PrincipalKind;
    use crate::protocol::{ActorId, Counter, Revision, StoreId};

    fn ctx(kind: PrincipalKind, project: Option<&str>) -> InvocationContext {
        InvocationContext::new(
            ActorId(Id::new("agent-b").unwrap()),
            kind,
            Id::new("invocation-1").unwrap(),
            Id::new("ui").unwrap(),
            StoreId(Id::new("store-a").unwrap()),
            Counter::new(0).unwrap(),
            Revision::new(1).unwrap(),
        )
        .with_lineage(project.map(|p| ProjectId(Id::new(p).unwrap())), None, None)
    }
    fn project(p: &str) -> DataScope {
        DataScope::Project {
            project_id: ProjectId(Id::new(p).unwrap()),
        }
    }
    fn code(r: Result<Scoped, Failure>) -> Option<DomainErrorCode> {
        match r {
            Ok(_) => None,
            Err(Failure::Refused(r)) => Some(r.code),
            Err(Failure::Storage(e)) => panic!("{e:?}"),
        }
    }

    #[test]
    fn a_confined_reach_never_reads_or_writes_another_project_or_general_writes() {
        let b = Reach::Project(ProjectId(Id::new("project-b").unwrap()));
        let agent = ctx(PrincipalKind::Agent, None);
        let exists = |_: &ProjectId| Ok(true);
        // Another project, even from an invocation that names none.
        assert_eq!(
            code(resolve_scope_with(
                &b,
                &agent,
                &project("project-a"),
                false,
                exists
            )),
            Some(DomainErrorCode::PermissionDenied)
        );
        // Its own project and general Knowledge are readable; general is not writable.
        let own = resolve_scope_with(&b, &agent, &project("project-b"), true, exists).unwrap();
        assert_eq!(own.readable.len(), 2);
        assert!(
            code(resolve_scope_with(
                &b,
                &agent,
                &DataScope::Workspace,
                false,
                exists
            ))
            .is_none()
        );
        assert_eq!(
            code(resolve_scope_with(
                &b,
                &agent,
                &DataScope::Workspace,
                true,
                exists
            )),
            Some(DomainErrorCode::PermissionDenied)
        );
        // An invocation of project B cannot name project A, whatever its reach.
        let in_b = ctx(PrincipalKind::Human, Some("project-b"));
        assert_eq!(
            code(resolve_scope_with(
                &Reach::Workspace,
                &in_b,
                &project("project-a"),
                false,
                exists
            )),
            Some(DomainErrorCode::PermissionDenied)
        );
        // A project that does not exist is NotFound, not an empty scope.
        assert_eq!(
            code(resolve_scope_with(
                &Reach::Workspace,
                &agent,
                &project("project-z"),
                false,
                |_| Ok(false)
            )),
            Some(DomainErrorCode::NotFound)
        );
    }

    #[test]
    fn nested_references_never_widen_a_record_beyond_its_scope() {
        let a = KnowledgeScope::Project(ProjectId(Id::new("project-a").unwrap()));
        let b = KnowledgeScope::Project(ProjectId(Id::new("project-b").unwrap()));
        assert!(nests(&KnowledgeScope::Workspace, &a));
        assert!(nests(&a, &a));
        assert!(!nests(&a, &b));
        assert!(!nests(&a, &KnowledgeScope::Workspace));
        assert!(writable(&Reach::Workspace, &a));
        assert!(!writable(
            &Reach::Project(ProjectId(Id::new("project-b").unwrap())),
            &a
        ));
        assert!(!writable(
            &Reach::Project(ProjectId(Id::new("project-b").unwrap())),
            &KnowledgeScope::Workspace
        ));
    }

    #[test]
    fn a_time_position_never_resumes_a_relevance_listing() {
        let words = ClaimQuery {
            words: vec!["cache".into()],
            ..ClaimQuery::default()
        };
        // A cursor issued for a search with words before relevance ranking carried a time.
        assert!(matches!(
            parse_position(&words, "1700000000000|knowledgeclaim-1"),
            Err(StorageError::CursorExpired)
        ));
        for bad in [
            "rNaN|knowledgeclaim-1",
            "rinf|knowledgeclaim-1",
            "r|knowledgeclaim-1",
        ] {
            assert!(matches!(
                parse_position(&words, bad),
                Err(StorageError::CursorExpired)
            ));
        }
        let claim = RankedClaim {
            rank: ClaimRank::Relevance(1.234_567_890_123_456_7),
            claim: StoredClaim {
                id: Id::new("knowledgeclaim-7").unwrap(),
                scope: KnowledgeScope::Workspace,
                revision: 1,
                kind: "Fact".into(),
                entry_kind: "Fact".into(),
                lifecycle: "Active".into(),
                subject_id: Id::new("knowledgeentity-1").unwrap(),
                predicate: "fact".into(),
                object_id: None,
                source: None,
                author: ActorId(Id::new("user-a").unwrap()),
                session_id: None,
                tags: vec![],
                basis: vec![],
                lesson: None,
                body: crate::ports::storage::ClaimBody::default(),
                created_at_ms: 1,
                updated_at_ms: 1,
                deleted_at_ms: None,
            },
        };
        let at = claim_position(&words, &claim);
        assert_eq!(
            parse_position(&words, &at).unwrap(),
            (claim.rank, claim.id.as_str().to_owned())
        );
        // A search without words keeps the time position.
        let none = ClaimQuery::default();
        assert_eq!(
            parse_position(&none, "1700000000000|knowledgeclaim-1").unwrap(),
            (
                ClaimRank::Time(1_700_000_000_000),
                "knowledgeclaim-1".to_owned()
            )
        );
        // A vector position (DW-247) never resumes a relevance listing.
        assert!(matches!(
            parse_position(&words, "v0.9876543210123456|knowledgeclaim-7"),
            Err(StorageError::CursorExpired)
        ));
    }
}
