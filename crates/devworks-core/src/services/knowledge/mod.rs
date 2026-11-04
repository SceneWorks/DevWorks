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
//! or reject a proposal. Delivery of rules to sessions is recorded
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
pub mod import;
pub mod model;
mod writes;

use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, knowledge as ops};
use crate::ports::files::ArtifactFiles;
use crate::ports::retrieval::{RetrievalMode, VectorIndex};
use crate::ports::storage::{
    ArtifactRead, AuthorityRead, ClaimQuery, ConfigScope, ConfigurationRead, KNOWLEDGE_PAGE_MAX,
    KnowledgeRead, KnowledgeScope, LessonFilter, ReadSnapshot, StorageError, StoredClaim,
    StoredEntity, StoredEvidence, StoredHistory, WriteTransaction,
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
    KnowledgeLifecycle, KnowledgeOrigin, KnowledgeRelation, KnowledgeRevisionEntry, KnowledgeState,
    KnowledgeWriteReceipt, LessonAddress, LessonAddressKind, LessonChallenge, LessonRecord,
    LessonSource, RuleEffect, RuleState, SchemaValue, Severity, SourceEvidence,
};
use crate::protocol::results::OperationResult;
use crate::protocol::support::{
    BoundedList, BoundedText, DynamicJson, Page, Probability, RelativePath,
};
use crate::protocol::{CorrelationId, DomainErrorCode, DomainFault, Id, OperationState, ProjectId};
use crate::registry::SchemaRevision;
use crate::services::cursor::CursorBinding;
use crate::services::sessions::{
    Failure, Refusal, content, counter, decided, label, not_found, query_fault, require_project,
    rev,
};
use crate::settings::catalog::descriptor;
use crate::settings::model::{chain, current_chain};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

pub use model::{TAG_KNOWLEDGE, TAG_LESSON, TAG_MEMORY};

/// The operations [`KnowledgeService`] is registered for.
pub const OPERATIONS: [&str; 17] = [
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
];

/// The setting that configures how many distinct source events make an inferred lesson a rule.
pub const THRESHOLD_KEY: &str = "knowledge.lessonRecurrenceThreshold";
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
/// Most claims one delete by source tombstones (a larger source is refused, never truncated).
pub const MAX_SOURCE_DELETE: usize = 10_000;
/// Most challenges, addresses, tags, cited evidence records and aliases one record carries,
/// and most identifiers one write receipt names: the wire lists' capacity. A write that would
/// exceed one is refused, so no list is ever cut short.
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
    vectors: Arc<dyn VectorIndex>,
}

// ---------------------------------------------------------------------------------------------
// Scope and authority

/// The principal's authorized reach: the user, the host and unconfined principals act
/// workspace-wide; a confined principal only in its bound project. A non-human principal
/// without an active host binding has none (fails closed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Reach {
    Workspace,
    Project(ProjectId),
}
pub(crate) fn reach(
    authority: &dyn AuthorityRead,
    ctx: &InvocationContext,
) -> Result<Reach, Failure> {
    if matches!(
        ctx.principal_kind(),
        PrincipalKind::Human | PrincipalKind::Host
    ) {
        return Ok(Reach::Workspace);
    }
    match authority.principal(ctx.principal())? {
        Some(b) if b.active => Ok(b.project_id.map_or(Reach::Workspace, Reach::Project)),
        _ => Err(Refusal::new(DomainErrorCode::PermissionDenied, "principal", None).into()),
    }
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

pub(crate) fn data_scope(scope: &KnowledgeScope) -> DataScope {
    match scope {
        KnowledgeScope::Workspace => DataScope::Workspace,
        KnowledgeScope::Project(p) => DataScope::Project {
            project_id: p.clone(),
        },
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
    let d = descriptor(THRESHOLD_KEY).expect("catalog setting");
    let mut out = BTreeMap::new();
    for s in scopes {
        let config_scope = match s {
            KnowledgeScope::Workspace => ConfigScope::Workspace,
            KnowledgeScope::Project(p) => ConfigScope::Project(p.clone()),
        };
        let layers = current_chain(config, &config_scope)?;
        let entries = chain(d, &layers).map_err(|m| {
            StorageError::InvalidRecord(format!("{} stored under another schema", m.key))
        })?;
        let value = entries
            .first()
            .map(|e| e.value.value.0.clone())
            .unwrap_or_else(|| d.default_value());
        out.insert(s.key(), value.as_i64().unwrap_or(3).max(1));
    }
    Ok(out)
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
        scope: data_scope(&e.scope),
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
    let stale = c.lifecycle == "Active" && read.knowledge_claim_stale(&c.id)?;
    // A deleted claim shows no challenge or address content: only its identity remains.
    let deleted = c.lifecycle == "Deleted";
    let challenges = if deleted {
        Vec::new()
    } else {
        read.knowledge_challenges(&c.id, MAX_CHALLENGES as u16)?
    };
    let open_challenge = !deleted && read.knowledge_open_challenges(&c.id)? > 0;
    let state = match c.lifecycle.as_str() {
        "Retracted" | "Deleted" => KnowledgeState::Retracted,
        _ if open_challenge => KnowledgeState::Challenged,
        _ if c.body.verified_at_ms.is_some() && !stale => KnowledgeState::Verified,
        _ => KnowledgeState::Active,
    };
    let lesson = match &c.lesson {
        None => None,
        Some(l) => {
            let events = read.knowledge_lesson_event_count(&c.id)?;
            let threshold = thresholds.get(&c.scope.key()).copied().unwrap_or(3);
            let (rule_state, effect) = model::effective_rule_state(
                &l.rule_state,
                c.body.lesson_effect.as_deref(),
                events,
                threshold,
            );
            let applicability: Applicability = serde_json::from_value(l.applicability.clone())
                .map_err(|_| corrupt("applicability"))?;
            let deliveries = read.knowledge_deliveries(&c.id)?;
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
                challenges: BoundedList(
                    challenges
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
                        .collect::<Result<_, StorageError>>()?,
                ),
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
        scope: data_scope(&c.scope),
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

fn receipt_of(w: &crate::ports::storage::StoredWrite) -> KnowledgeWriteReceipt {
    KnowledgeWriteReceipt {
        write_id: w.write_id.clone(),
        state: OperationState::ObservedComplete,
        entity_ids: id_list(&w.entity_ids),
        claim_ids: id_list(&w.claim_ids),
        revision: rev(w.scope_revision),
        diagnostics: BoundedList(
            w.diagnostics
                .iter()
                .take(128)
                .filter_map(|d| serde_json::from_value(d.clone()).ok())
                .collect(),
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Paging

/// One page of claims: rows strictly after `after`, at most `limit`, and whether more follow.
/// The repository maximum is never exceeded: a full page of the maximum probes one more row.
fn claim_page(
    read: &dyn KnowledgeRead,
    query: &ClaimQuery,
    after: Option<(i64, String)>,
    limit: usize,
) -> Result<(Vec<StoredClaim>, bool), StorageError> {
    let limit = limit.clamp(1, KNOWLEDGE_PAGE_MAX as usize);
    let ask = (limit + 1).min(KNOWLEDGE_PAGE_MAX as usize) as u16;
    let mut rows =
        read.knowledge_claims(query, after.as_ref().map(|(t, id)| (*t, id.as_str())), ask)?;
    let more = if rows.len() > limit {
        rows.truncate(limit);
        true
    } else if rows.len() == limit && (ask as usize) == limit {
        let last = rows.last().expect("full page");
        !read
            .knowledge_claims(query, Some((sort_key(query, last), last.id.as_str())), 1)?
            .is_empty()
    } else {
        false
    };
    Ok((rows, more))
}

/// The sort time of a claim in `query`'s order.
fn sort_key(query: &ClaimQuery, c: &StoredClaim) -> i64 {
    match query.lesson.as_ref().and_then(|l| l.view.as_deref()) {
        Some("RecentlyLearned") => c
            .lesson
            .as_ref()
            .and_then(|l| l.effective_at_ms)
            .unwrap_or(c.updated_at_ms),
        _ => c.updated_at_ms,
    }
}

fn decode_position(binding: &CursorBinding, cursor: &str) -> Result<(i64, String), StorageError> {
    let at = binding.decode(cursor)?;
    let (t, id) = at.split_once('|').ok_or(StorageError::CursorExpired)?;
    Ok((
        t.parse().map_err(|_| StorageError::CursorExpired)?,
        id.to_owned(),
    ))
}

// ---------------------------------------------------------------------------------------------
// Service

impl KnowledgeService {
    pub fn new(
        ids: Arc<dyn IdSource>,
        clock: Arc<dyn Clock>,
        files: Option<Arc<dyn ArtifactFiles>>,
        vectors: Arc<dyn VectorIndex>,
    ) -> Self {
        Self {
            ids,
            clock,
            files,
            vectors,
        }
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
        let (mut parts, exact_revision) = Self::revision_parts(k, &scoped)?;
        parts.push(format!("{query:?}"));
        parts.push(limit.to_string());
        let binding = CursorBinding::new(ctx, "knowledge.search", &refs(&parts), exact_revision);
        let after = match &i.page.cursor {
            None => None,
            Some(c) => Some(decode_position(&binding, &c.0)?),
        };
        let (rows, more) = claim_page(k, &query, after, limit)?;
        let th = thresholds(snapshot.configuration(), &scoped.readable)?;
        let items = rows
            .iter()
            .map(|c| claim_wire(k, c, &th))
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = match (more, rows.last()) {
            (true, Some(last)) => Some(BoundedText(binding.encode(&format!(
                "{}|{}",
                sort_key(&query, last),
                last.id.as_str()
            )))),
            _ => None,
        };
        Ok((
            OperationResult::KnowledgeSearch {
                value: Page {
                    items: BoundedList(items),
                    has_more: next_cursor.is_some(),
                    next_cursor,
                },
            },
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
        let after = match &i.page.cursor {
            None => None,
            Some(c) => Some(decode_position(&binding, &c.0)?),
        };
        let (rows, more) = claim_page(k, &query, after, limit)?;
        let items = rows
            .iter()
            .map(|c| claim_wire(k, c, &th))
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = match (more, rows.last()) {
            (true, Some(last)) => Some(BoundedText(binding.encode(&format!(
                "{}|{}",
                sort_key(&query, last),
                last.id.as_str()
            )))),
            _ => None,
        };
        Ok((
            OperationResult::KnowledgeTaggedLessons {
                value: Page {
                    items: BoundedList(items),
                    has_more: next_cursor.is_some(),
                    next_cursor,
                },
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
        let bundle = bundle_of(k, &scoped, &claim, wire, &th, at)?;
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
        Ok((
            OperationResult::KnowledgeWriteStatus {
                value: receipt_of(&write),
            },
            revision,
        ))
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
        match self.vectors.mode() {
            RetrievalMode::LexicalOnly { reason } => items.push(diagnostic(
                "knowledge-retrieval",
                Severity::Info,
                RETRIEVAL_COMPONENT,
                "LexicalOnly",
                &format!(
                    "Retrieval is lexical only ({reason}): no embedding route exists, so vector \
                     search is not run."
                ),
            )),
            RetrievalMode::Hybrid { embedding_space } => items.push(diagnostic(
                "knowledge-retrieval",
                Severity::Info,
                RETRIEVAL_COMPONENT,
                "Hybrid",
                &format!("Lexical and vector retrieval over embedding space {embedding_space}."),
            )),
        }
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
        if i.source_id.is_some() && i.import_source.is_some() {
            return Err(Refusal::new(DomainErrorCode::InvalidArgument, PREVIEW, None).into());
        }
        if let Some(source) = &i.source_id {
            let plan = writes::deletion_plan(k, &scoped.exact, source.as_str(), exact_revision)?;
            items.push(diagnostic(
                &plan.digest,
                Severity::Info,
                PREVIEW_COMPONENT,
                "DeleteBySource",
                &format!(
                    "Deleting source {} at scope revision {exact_revision} tombstones {} claims, \
                     {} entities and {} evidence records{}. Confirm with this diagnostic's id as \
                     previewDigest.",
                    source.as_str(),
                    plan.claims.len(),
                    plan.entities.len(),
                    plan.evidence.len(),
                    if plan.claims.len() > MAX_SOURCE_DELETE {
                        " (more than one deletion may remove; it will be refused)"
                    } else {
                        ""
                    }
                ),
            ));
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
    // Host pipeline API (context assembly, indexing)

    /// Records that `session` was supplied `rules` (claim and revision) in the same unit of
    /// work as the context that carried them (KNOW-06). Unknown sessions or claims are refused.
    pub fn record_delivery(
        &self,
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
            // Only a rule the session may be given: general, or of the session's own project.
            let scope = match tx.knowledge().knowledge_claim(claim)? {
                Some(c)
                    if c.revision >= *revision
                        && *revision >= 1
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

    /// Records the current revision of a source (a repository path, a document version); claims
    /// recorded or verified against an earlier revision read stale until verified again.
    /// Returns the claims whose stale status changed (stale now, or current again).
    pub fn observe_source(
        &self,
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

    /// Every effective rule a session of `project` (`None`: a workspace session) may be given:
    /// the project's and general rules, read in full (never only a first page), filtered by
    /// applicability `activity` when given, in newest-first order (CONTEXT seam).
    pub fn rules_for(
        &self,
        snapshot: &dyn ReadSnapshot,
        project: Option<&ProjectId>,
        activity: Option<&str>,
    ) -> Result<Vec<KnowledgeClaim>, StorageError> {
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
        let k = knowledge_read(snapshot);
        let mut out = Vec::new();
        let mut after: Option<(i64, String)> = None;
        loop {
            let (rows, more) = claim_page(k, &query, after.clone(), KNOWLEDGE_PAGE_MAX as usize)?;
            for c in &rows {
                out.push(claim_wire(k, c, &th)?);
            }
            match (more, rows.last()) {
                (true, Some(last)) => {
                    after = Some((sort_key(&query, last), last.id.as_str().to_owned()))
                }
                _ => return Ok(out),
            }
        }
    }
}

/// Whether a write of `scope` is visible from this invocation.
fn visible_from(ctx: &InvocationContext, reach: &Reach, scope: &KnowledgeScope) -> bool {
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

fn refs(parts: &[String]) -> Vec<&str> {
    parts.iter().map(String::as_str).collect()
}

/// The Knowledge view of a snapshot.
fn knowledge_read(snapshot: &dyn ReadSnapshot) -> &dyn KnowledgeRead {
    snapshot.knowledge()
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
    for id in &wire.evidence_ids.0 {
        if let Some(e) = k.knowledge_evidence(id)?
            && e.deleted_at_ms.is_none()
            && scoped.sees(&e.scope)
        {
            evidence.push(evidence_wire(&e, Some(&claim.id))?);
        }
    }
    let historical = at.is_some();
    let mut claims = vec![wire.clone()];
    let mut conflicts = Vec::new();
    let mut truncated = false;
    if claim.lifecycle == "Active" && !historical {
        let others =
            k.knowledge_claims_about(&scoped.keys(), &claim.subject_id, &claim.predicate, 129)?;
        truncated |= others.len() > 128;
        let different: Vec<&StoredClaim> = others
            .iter()
            .take(128)
            .filter(|o| {
                o.id != claim.id
                    && (o.body.value != claim.body.value
                        || o.body.value_json != claim.body.value_json
                        || o.object_id != claim.object_id)
            })
            .collect();
        if !different.is_empty() {
            let mut ids = vec![claim.id.clone()];
            ids.extend(different.iter().take(127).map(|o| o.id.clone()));
            conflicts.push(KnowledgeConflict {
                id: Id::new(
                    format!("conflict-{}", claim.id.as_str())
                        .chars()
                        .take(128)
                        .collect::<String>(),
                )
                .map_err(|_| StorageError::InvalidRecord("conflict id".into()))?,
                claim_ids: BoundedList(ids),
                state: label("Open", "Open"),
                explanation: content(&format!(
                    "{} active claims about the same subject state {} differently.",
                    different.len() + 1,
                    claim.predicate
                )),
            });
            for o in different.iter().take(16) {
                claims.push(claim_wire(k, o, th)?);
            }
        }
    }
    // Relations and contradictions are current facts: a past revision is shown without them.
    let relations = if historical {
        Vec::new()
    } else {
        k.knowledge_relations(&claim.id, 129)?
    };
    truncated |= relations.len() > 128;
    let visible_relations: Vec<_> = relations
        .iter()
        .take(128)
        .filter(|r| scoped.sees(&r.scope))
        .collect();
    for r in &visible_relations {
        if r.predicate == model::CONTRADICTS {
            let other = if r.subject_id == claim.id {
                &r.object_id
            } else {
                &r.subject_id
            };
            if let Some(o) = k.knowledge_claim(other)?
                && scoped.sees(&o.scope)
                && o.lifecycle == "Active"
                && claim.lifecycle == "Active"
            {
                conflicts.push(KnowledgeConflict {
                    id: r.id.clone(),
                    claim_ids: BoundedList(vec![claim.id.clone(), o.id.clone()]),
                    state: label("Open", "Open"),
                    explanation: content("Recorded as contradicting each other."),
                });
                if !claims.iter().any(|c| c.id == o.id) {
                    claims.push(claim_wire(k, &o, th)?);
                }
            }
        }
    }
    // The supersession chain, both directions, bounded.
    let mut supersession = Vec::new();
    let mut back = wire.supersedes.clone();
    while let Some(id) = back.take() {
        if supersession.len() >= 32 || supersession.contains(&id) {
            truncated = true;
            break;
        }
        match k.knowledge_claim(&id)? {
            Some(c) if scoped.sees(&c.scope) => {
                back = c.body.supersedes.clone();
                supersession.push(id);
            }
            _ => break,
        }
    }
    let mut forward = wire.superseded_by.clone();
    while let Some(id) = forward.take() {
        if supersession.len() >= 64 || supersession.contains(&id) {
            truncated = true;
            break;
        }
        match k.knowledge_claim(&id)? {
            Some(c) if scoped.sees(&c.scope) => {
                forward = c.body.superseded_by.clone();
                supersession.push(id);
            }
            _ => break,
        }
    }
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

impl QueryHandler for KnowledgeService {
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
            ops::Input::Search(i) => self.search(snapshot, ctx, i),
            ops::Input::Read(i) => self.read(snapshot, ctx, i),
            ops::Input::Diagnostics(i) => self.diagnostics(snapshot, ctx, i),
            ops::Input::WriteStatus(i) => self.write_status(snapshot, ctx, i),
            ops::Input::TaggedLessons(i) => self.tagged_lessons(snapshot, ctx, i),
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
        let outcome = writes::decide(ctx, command, input, tx);
        decided(tx, outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
