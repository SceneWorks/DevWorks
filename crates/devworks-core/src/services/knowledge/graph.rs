//! Knowledge graph reads (DW-031): `knowledge.subgraph`, `.entity_bundle`, `.entity_graph`,
//! `.claim_bundle`, `.expand_frontier`, `.render_summary` and `.query`.
//!
//! Every read resolves its scope first and asks the store only for rows of the readable scope
//! keys, so traversal never materializes another project's Knowledge. Every list is bounded by
//! the request's limits (clamped to the retained CodeGraph bounds) and a hit bound is reported as
//! `truncated`; entities at the edge of a bounded expansion are returned in `frontier`, so a
//! client continues with `knowledge.expand_frontier` instead of an unbounded read. A subgraph
//! without seeds or text pages through the whole visible graph instead.
//!
//! The graph is the native Knowledge model: entities are nodes and a claim whose object is an
//! entity is an edge between its subject and object. Only active edge claims are traversed
//! (CodeGraph's `ACTIVE_RELATES_TO`). Retained behavior preserved: seed ordering (explicit
//! entity seeds, then entities of seed claims, then traversal by distance and id), scores
//! (100 for a seed, `max(1, 50 − 10·hop)` for an entity, `max(1, 60 − 10·hop)` for a claim),
//! the subgraph limits used by frontier expansion and summaries, the summary layout, the
//! "No relevant memories found." empty answer and the required-seed refusals. CodeGraph's claim
//! query did not apply its own status filters; here `includeSuperseded`, `states`, predicates,
//! tags and `confidenceMin` are always enforced.
use super::{
    KNOWLEDGE_PAGE_MAX, KnowledgeService, Scoped, claim_wire, entity_wire, evidence_wire,
    knowledge_read, model, resolve_scope, thresholds,
};
use crate::context::InvocationContext;
use crate::ports::storage::{
    ClaimQuery, KnowledgeRead, ReadSnapshot, StorageError, StoredClaim, StoredEntity,
};
use crate::protocol::records::{
    KnowledgeBundle, KnowledgeClaim, KnowledgeConflict, KnowledgeEntity, KnowledgeQuery,
    KnowledgeRelation, KnowledgeState, SourceEvidence,
};
use crate::protocol::support::{BoundedList, BoundedText};
use crate::protocol::{DomainErrorCode, Id};
use crate::services::cursor::CursorBinding;
use crate::services::sessions::{Failure, Refusal, content, counter, label, not_found, rev};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// The empty answer CodeGraph returned for a query that matched nothing.
pub const NO_MEMORIES: &str = "No relevant memories found.";
/// Claims listed per entity in a rendered summary.
const PER_ENTITY: usize = 8;
const MAX_HOPS: u32 = 5;
const MAX_CHAIN: usize = 5;
/// The store's page maximum.
const MAX_ROWS: u16 = crate::ports::storage::KNOWLEDGE_PAGE_MAX;

fn invalid(kind: &'static str) -> Failure {
    Refusal::new(DomainErrorCode::InvalidArgument, kind, None).into()
}
fn clamp(v: u32, lo: u32, hi: u32) -> usize {
    v.clamp(lo, hi) as usize
}
fn probability(p: &crate::protocol::support::Probability) -> f64 {
    serde_json::to_value(p)
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
}

/// Which claims a read returns.
#[derive(Clone, Debug)]
pub(super) struct ClaimFilter {
    pub include_superseded: bool,
    pub states: Vec<KnowledgeState>,
    pub predicates: Vec<String>,
    pub tags: Vec<String>,
    pub confidence_min: Option<f64>,
}
impl ClaimFilter {
    pub fn of(q: &KnowledgeQuery) -> Self {
        Self {
            include_superseded: q.include_superseded,
            states: q.states.0.clone(),
            predicates: q.predicates.0.iter().map(|p| p.0.to_lowercase()).collect(),
            tags: model::fold_tags(q.tags.0.iter().map(|t| t.0.as_str())),
            confidence_min: q.confidence_min.as_ref().map(probability),
        }
    }
    fn retracted_asked(&self) -> bool {
        self.states
            .iter()
            .any(|s| matches!(s, KnowledgeState::Retracted))
    }
    /// The stored lifecycles this filter can admit, so a text seed search reaches the
    /// historical claims a request explicitly asks for.
    fn lifecycles(&self) -> Vec<String> {
        let mut out = vec!["Active".to_owned()];
        if self.include_superseded {
            out.push("Superseded".into());
        }
        if self.retracted_asked() {
            out.push("Retracted".into());
        }
        out
    }
    fn admits(&self, c: &StoredClaim, wire: &KnowledgeClaim) -> bool {
        let retracted_asked = self.retracted_asked();
        match c.lifecycle.as_str() {
            "Deleted" => return false,
            "Superseded" if !self.include_superseded => return false,
            "Retracted" if !retracted_asked => return false,
            _ => {}
        }
        if !self.states.is_empty()
            && !self
                .states
                .iter()
                .any(|s| model::wire(s) == model::wire(&wire.state))
        {
            return false;
        }
        if !self.predicates.is_empty() && !self.predicates.contains(&c.predicate.to_lowercase()) {
            return false;
        }
        if !self.tags.iter().all(|t| c.tags.contains(t)) {
            return false;
        }
        self.confidence_min.is_none_or(|m| c.body.confidence >= m)
    }
}

/// Bounds of one subgraph expansion.
#[derive(Clone, Copy, Debug)]
pub(super) struct Bounds {
    pub hops: u32,
    pub entities: usize,
    pub claims: usize,
    /// Edge claims read per traversed entity.
    pub edges: u16,
}

/// A bounded subgraph around seeds.
pub(super) struct Selection {
    /// `(entity, hop, score)` in result order (hop ascending, score descending).
    pub entities: Vec<(StoredEntity, u32, i64)>,
    /// `(claim, wire, score)` in result order (seeds first, then newest).
    pub claims: Vec<(StoredClaim, KnowledgeClaim, i64)>,
    /// Entities whose neighbors were not all expanded (depth or a bound reached).
    pub frontier: Vec<Id>,
    pub truncated: bool,
}

struct Ctx<'a> {
    k: &'a dyn KnowledgeRead,
    scoped: &'a Scoped,
    keys: Vec<String>,
    th: &'a BTreeMap<String, i64>,
}
impl Ctx<'_> {
    fn entity(&self, id: &Id) -> Result<Option<StoredEntity>, StorageError> {
        Ok(self
            .k
            .knowledge_entity(id)?
            .filter(|e| e.deleted_at_ms.is_none() && self.scoped.sees(&e.scope)))
    }
    fn claim(&self, id: &Id) -> Result<Option<StoredClaim>, StorageError> {
        Ok(self
            .k
            .knowledge_claim(id)?
            .filter(|c| c.lifecycle != "Deleted" && self.scoped.sees(&c.scope)))
    }
    /// Active claims linking `entity` to another entity, newest first, and whether more exist.
    /// The edge filter applies in the store, before the limit.
    fn edges(&self, entity: &Id, limit: u16) -> Result<(Vec<StoredClaim>, bool), StorageError> {
        // One past the limit detects more rows; at the store's maximum a full page counts.
        let probe = limit.saturating_add(1).min(MAX_ROWS);
        let mut all = self.k.knowledge_entity_edges(entity, &self.keys, probe)?;
        let more = all.len() > limit as usize || all.len() >= MAX_ROWS as usize;
        all.truncate(limit as usize);
        Ok((all, more))
    }
}

/// Candidate claims a text seed search examines before it stops looking for admitted ones.
const SEED_SCAN_MAX: usize = 2000;

/// Seeds of a request: explicit ids (each must be visible) plus, for text, the matching claims
/// the request's filters admit (up to `text_limit`) and their subject entities. The flag is
/// set when the scan bound stopped the text search before it was exhausted.
fn seeds(
    c: &Ctx<'_>,
    q: &KnowledgeQuery,
    text_limit: usize,
    filter: &ClaimFilter,
) -> Result<(Vec<Id>, Vec<Id>, bool), Failure> {
    let mut more = false;
    let mut entities: Vec<Id> = Vec::new();
    let mut claims: Vec<Id> = Vec::new();
    for id in &q.entity_ids.0 {
        if c.entity(id)?.is_none() {
            return Err(not_found("knowledge_entity", id).into());
        }
        if !entities.contains(id) {
            entities.push(id.clone());
        }
    }
    for id in &q.claim_ids.0 {
        if c.claim(id)?.is_none() {
            return Err(not_found(super::CLAIM, id).into());
        }
        if !claims.contains(id) {
            claims.push(id.clone());
        }
    }
    if let Some(text) = &q.text
        && text_limit > 0
    {
        let words = model::words(&text.0);
        if words.len() > model::MAX_WORDS {
            return Err(invalid("text"));
        }
        if !words.is_empty() {
            // Filters the store applies (lifecycle, tags) narrow the search itself; the rest are
            // applied per page, so ineligible newer matches never use up the seed budget.
            let query = ClaimQuery {
                scopes: c.keys.clone(),
                lifecycles: filter.lifecycles(),
                tags: filter.tags.clone(),
                words,
                ..Default::default()
            };
            let mut admitted = 0;
            let mut scanned = 0;
            let mut after: Option<(i64, String)> = None;
            'scan: loop {
                let page = c.k.knowledge_claims(
                    &query,
                    after.as_ref().map(|(t, id)| (*t, id.as_str())),
                    KNOWLEDGE_PAGE_MAX,
                )?;
                let last = page.len() < KNOWLEDGE_PAGE_MAX as usize;
                for claim in page {
                    if scanned >= SEED_SCAN_MAX {
                        more = true;
                        break 'scan;
                    }
                    scanned += 1;
                    after = Some((claim.updated_at_ms, claim.id.as_str().to_owned()));
                    let wire = claim_wire(c.k, &claim, c.th)?;
                    if !filter.admits(&claim, &wire) || claims.contains(&claim.id) {
                        continue;
                    }
                    // A full budget probes on: one more admitted match means the seeds omit
                    // eligible knowledge.
                    if admitted >= text_limit {
                        more = true;
                        break 'scan;
                    }
                    if !entities.contains(&claim.subject_id)
                        && c.entity(&claim.subject_id)?.is_some()
                    {
                        entities.push(claim.subject_id.clone());
                    }
                    claims.push(claim.id);
                    admitted += 1;
                }
                if last {
                    break;
                }
            }
        }
    }
    Ok((entities, claims, more))
}

fn entity_score(hop: u32, seed: bool) -> i64 {
    if seed {
        100
    } else {
        (50 - 10 * i64::from(hop)).max(1)
    }
}
fn claim_score(hop: u32, seed: bool) -> i64 {
    if seed {
        100
    } else {
        (60 - 10 * i64::from(hop)).max(1)
    }
}

/// The bounded subgraph around explicit seeds.
fn select(
    c: &Ctx<'_>,
    entity_seeds: &[Id],
    claim_seeds: &[Id],
    bounds: Bounds,
    filter: &ClaimFilter,
) -> Result<Selection, Failure> {
    let mut truncated = false;
    // Distance per entity: direct seeds 0, entities of seed claims 1 (not traversed further).
    let mut hop: HashMap<Id, u32> = HashMap::new();
    let mut rank: HashMap<Id, u8> = HashMap::new();
    for id in entity_seeds {
        hop.insert(id.clone(), 0);
        rank.insert(id.clone(), 0);
    }
    let mut seed_claims = Vec::new();
    for id in claim_seeds {
        let Some(claim) = c.claim(id)? else { continue };
        for e in std::iter::once(&claim.subject_id).chain(claim.object_id.iter()) {
            if !hop.contains_key(e) && c.entity(e)?.is_some() {
                hop.insert(e.clone(), 1);
                rank.insert(e.clone(), 1);
            }
        }
        seed_claims.push(claim);
    }
    // Breadth-first over active edge claims from the direct seeds, bounded in candidates.
    let mut frontier: HashSet<Id> = HashSet::new();
    // Who discovered each traversed entity, so a bound that drops it keeps its discoverer on
    // the frontier.
    let mut parent: HashMap<Id, Id> = HashMap::new();
    let mut queue: VecDeque<Id> = entity_seeds.iter().cloned().collect();
    let candidate_bound = bounds.entities.saturating_mul(4).max(1);
    while let Some(at) = queue.pop_front() {
        let d = hop[&at];
        if d >= bounds.hops {
            frontier.insert(at);
            continue;
        }
        let (edges, more) = c.edges(&at, bounds.edges)?;
        if more {
            frontier.insert(at.clone());
            truncated = true;
        }
        let mut next: Vec<Id> = edges
            .iter()
            .filter_map(|e| {
                let other = if e.subject_id == at {
                    e.object_id.clone()?
                } else {
                    e.subject_id.clone()
                };
                (!hop.contains_key(&other)).then_some(other)
            })
            .collect();
        next.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        next.dedup();
        for other in next {
            if hop.len() >= candidate_bound {
                frontier.insert(at.clone());
                truncated = true;
                break;
            }
            if c.entity(&other)?.is_none() {
                continue;
            }
            hop.insert(other.clone(), d + 1);
            rank.insert(other.clone(), 2);
            parent.insert(other.clone(), at.clone());
            queue.push_back(other);
        }
    }
    // Rank: direct seeds, then seed-claim entities, then by distance and id; then bound.
    let mut ordered: Vec<(Id, u32, u8)> = hop
        .iter()
        .map(|(id, h)| (id.clone(), *h, rank[id]))
        .collect();
    ordered.sort_by(|a, b| {
        a.2.cmp(&b.2)
            .then(a.1.cmp(&b.1))
            .then(a.0.as_str().cmp(b.0.as_str()))
    });
    let dropped: Vec<Id> = if ordered.len() > bounds.entities {
        truncated = true;
        ordered
            .split_off(bounds.entities)
            .into_iter()
            .map(|o| o.0)
            .collect()
    } else {
        Vec::new()
    };
    let kept: HashSet<Id> = ordered.iter().map(|o| o.0.clone()).collect();
    for id in &dropped {
        // Walk up to the nearest kept ancestor: continuing from it reaches the dropped node.
        let mut at = parent.get(id);
        while let Some(p) = at {
            if kept.contains(p) {
                frontier.insert(p.clone());
                break;
            }
            at = parent.get(p);
        }
    }
    let mut entities = Vec::new();
    for (id, h, r) in &ordered {
        if let Some(e) = c.entity(id)? {
            entities.push((e, *h, entity_score(*h, *r == 0)));
        }
    }
    entities.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then(b.2.cmp(&a.2))
            .then(a.0.id.as_str().cmp(b.0.id.as_str()))
    });
    // Claims: seeds first, then claims about kept entities (object absent or kept), newest first.
    let mut claims: Vec<(StoredClaim, KnowledgeClaim, i64)> = Vec::new();
    let mut seen: HashSet<Id> = HashSet::new();
    for claim in seed_claims {
        let wire = claim_wire(c.k, &claim, c.th)?;
        seen.insert(claim.id.clone());
        // Seeds obey the request's filters and claim budget like every other claim.
        if filter.admits(&claim, &wire) {
            if claims.len() >= bounds.claims {
                truncated = true;
                continue;
            }
            claims.push((claim, wire, 100));
        }
    }
    let mut candidates: Vec<(StoredClaim, u32)> = Vec::new();
    for (e, h, _) in &entities {
        let all = c.k.knowledge_entity_claims(&e.id, &c.keys, MAX_ROWS)?;
        truncated |= all.len() >= MAX_ROWS as usize;
        for claim in all.into_iter().take(500) {
            if seen.contains(&claim.id)
                || claim.subject_id != e.id
                || claim.object_id.as_ref().is_some_and(|o| !kept.contains(o))
            {
                continue;
            }
            let other = claim
                .object_id
                .as_ref()
                .and_then(|o| hop.get(o).copied())
                .unwrap_or(*h);
            seen.insert(claim.id.clone());
            candidates.push((claim, (*h).min(other)));
        }
    }
    candidates.sort_by(|a, b| {
        b.0.updated_at_ms
            .cmp(&a.0.updated_at_ms)
            .then(a.0.id.as_str().cmp(b.0.id.as_str()))
    });
    for (claim, h) in candidates {
        let wire = claim_wire(c.k, &claim, c.th)?;
        if !filter.admits(&claim, &wire) {
            continue;
        }
        if claims.len() >= bounds.claims {
            truncated = true;
            break;
        }
        claims.push((claim, wire, claim_score(h, false)));
    }
    let mut frontier: Vec<Id> = frontier
        .into_iter()
        .filter(|id| kept.contains(id))
        .collect();
    frontier.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(Selection {
        entities,
        claims,
        frontier,
        truncated,
    })
}

/// Conflicts among `claims`: the same subject and predicate with different values, and
/// recorded `contradicts` relations between returned claims.
/// The open conflicts among `claims`; the flag is set when a group or a relation read was cut.
fn conflicts(
    c: &Ctx<'_>,
    claims: &[(StoredClaim, KnowledgeClaim, i64)],
) -> Result<(Vec<KnowledgeConflict>, bool), StorageError> {
    let mut out = Vec::new();
    let mut cut = false;
    let mut groups: BTreeMap<(String, String), Vec<&StoredClaim>> = BTreeMap::new();
    for (claim, _, _) in claims.iter().filter(|(c, _, _)| c.lifecycle == "Active") {
        groups
            .entry((
                claim.subject_id.as_str().to_owned(),
                claim.predicate.clone(),
            ))
            .or_default()
            .push(claim);
    }
    for ((_, predicate), group) in &groups {
        let first = group[0];
        if group.iter().any(|o| {
            o.body.value != first.body.value
                || o.body.value_json != first.body.value_json
                || o.object_id != first.object_id
        }) {
            cut |= group.len() > 128;
            out.push(KnowledgeConflict {
                id: Id::new(
                    format!("conflict-{}", first.id.as_str())
                        .chars()
                        .take(128)
                        .collect::<String>(),
                )
                .map_err(|_| StorageError::InvalidRecord("conflict id".into()))?,
                claim_ids: BoundedList(group.iter().take(128).map(|c| c.id.clone()).collect()),
                state: label("Open", "Open"),
                explanation: content(&format!(
                    "{} active claims about the same subject state {predicate} differently.",
                    group.len()
                )),
            });
        }
    }
    // A recorded contradiction is open only while both of its claims are active.
    let active: HashSet<&Id> = claims
        .iter()
        .filter(|(c, _, _)| c.lifecycle == "Active")
        .map(|(c, _, _)| &c.id)
        .collect();
    let mut recorded = HashSet::new();
    for (claim, _, _) in claims {
        let found = c.k.knowledge_relations(&claim.id, &c.keys, 129)?;
        cut |= found.len() > 128;
        for r in found {
            if r.predicate == model::CONTRADICTS
                && active.contains(&r.subject_id)
                && active.contains(&r.object_id)
                && recorded.insert(r.id.clone())
            {
                out.push(KnowledgeConflict {
                    id: r.id.clone(),
                    claim_ids: BoundedList(vec![r.subject_id.clone(), r.object_id.clone()]),
                    state: label("Open", "Open"),
                    explanation: content("Recorded as contradicting each other."),
                });
            }
        }
    }
    Ok((out, cut))
}

/// The supersession chains of `claims` (both directions, at most five steps each).
fn chains(
    c: &Ctx<'_>,
    claims: &[(StoredClaim, KnowledgeClaim, i64)],
) -> Result<Vec<Id>, StorageError> {
    let mut out: Vec<Id> = Vec::new();
    for (claim, _, _) in claims {
        for backward in [true, false] {
            let mut next = if backward {
                claim.body.supersedes.clone()
            } else {
                claim.body.superseded_by.clone()
            };
            let mut steps = 0;
            while let Some(id) = next.take() {
                if steps >= MAX_CHAIN || out.contains(&id) || id == claim.id {
                    break;
                }
                let Some(found) = c.claim(&id)? else { break };
                next = if backward {
                    found.body.supersedes.clone()
                } else {
                    found.body.superseded_by.clone()
                };
                out.push(id);
                steps += 1;
            }
        }
    }
    Ok(out)
}

/// The bundle of a selection, with the request's optional parts.
fn bundle(
    c: &Ctx<'_>,
    q: &KnowledgeQuery,
    s: &Selection,
    include_conflicts: bool,
) -> Result<KnowledgeBundle, StorageError> {
    let mut truncated = s.truncated;
    let conflicts = if include_conflicts {
        let (found, cut) = conflicts(c, &s.claims)?;
        truncated |= cut;
        found
    } else {
        Vec::new()
    };
    let supersession = if q.include_supersession_chain {
        chains(c, &s.claims)?
    } else {
        Vec::new()
    };
    truncated |= supersession.len() > 128 || s.frontier.len() > 128;
    let claim_ids: HashSet<&Id> = s.claims.iter().map(|(c, _, _)| &c.id).collect();
    let mut relations = Vec::new();
    let mut seen = HashSet::new();
    for (claim, _, _) in &s.claims {
        let found = c.k.knowledge_relations(&claim.id, &c.keys, 129)?;
        truncated |= found.len() > 128;
        for r in found {
            if claim_ids.contains(&r.subject_id)
                && claim_ids.contains(&r.object_id)
                && seen.insert(r.id.clone())
            {
                relations.push(KnowledgeRelation {
                    id: r.id.clone(),
                    subject_id: r.subject_id.clone(),
                    predicate: label(&r.predicate, "-"),
                    object_id: r.object_id.clone(),
                    evidence_ids: {
                        truncated |= r.evidence_ids.len() > 128;
                        BoundedList(r.evidence_ids.iter().take(128).cloned().collect())
                    },
                    author_id: r.author.clone(),
                    created_at_ms: counter(r.created_at_ms),
                });
            }
        }
    }
    // Capped lists say so: an incomplete conflict or relation view never reads as complete.
    truncated |= conflicts.len() > 500 || relations.len() > 500;
    relations.truncate(500);
    // Evidence of every returned claim, its challenges and the returned relations (as a claim
    // bundle shows it), visible, deduplicated and within the evidence budget.
    let mut evidence: Vec<SourceEvidence> = Vec::new();
    if q.include_evidence {
        let limit = clamp(q.evidence_limit.get(), 1, 500);
        let cited = s
            .claims
            .iter()
            .flat_map(|(claim, wire, _)| {
                let challenges = wire
                    .challenges
                    .iter()
                    .flat_map(|list| &list.0)
                    .flat_map(|ch| &ch.evidence_ids.0);
                wire.evidence_ids
                    .0
                    .iter()
                    .chain(challenges)
                    .map(move |id| (id, &claim.id))
            })
            .chain(
                relations
                    .iter()
                    .flat_map(|r| r.evidence_ids.0.iter().map(move |id| (id, &r.subject_id))),
            );
        for (id, owner) in cited {
            if evidence.iter().any(|e| e.id == *id) {
                continue;
            }
            if evidence.len() >= limit {
                truncated = true;
                break;
            }
            if let Some(e) = c.k.knowledge_evidence(id)?
                && e.deleted_at_ms.is_none()
                && c.scoped.sees(&e.scope)
            {
                evidence.push(evidence_wire(&e, Some(owner))?);
            }
        }
    }
    Ok(KnowledgeBundle {
        entities: BoundedList(s.entities.iter().map(|(e, _, _)| entity_wire(e)).collect()),
        claims: BoundedList(s.claims.iter().map(|(_, w, _)| w.clone()).collect()),
        evidence: BoundedList(evidence),
        frontier: BoundedList(s.frontier.iter().take(128).cloned().collect()),
        next_cursor: None,
        truncated,
        conflicts: BoundedList(conflicts.into_iter().take(500).collect()),
        supersession_ids: BoundedList(supersession.into_iter().take(128).collect()),
        summary: None,
        relations: Some(BoundedList(relations)),
        revisions: Some(BoundedList(Vec::new())),
        scope_revision: Some(rev(c.k.knowledge_scope_revision(&c.scoped.exact)?)),
    })
}

/// The retained summary layout over a selection (Markdown).
pub(super) fn render(s: &Selection, conflicts: &[KnowledgeConflict]) -> String {
    let mut out = String::from("## Relevant Memory\n\n");
    let labels: HashMap<&Id, &str> = s
        .entities
        .iter()
        .map(|(e, _, _)| (&e.id, e.label.as_str()))
        .collect();
    let text = |w: &KnowledgeClaim| -> String {
        w.normalized_text
            .as_ref()
            .map(|t| t.0.clone())
            .or_else(|| {
                w.value
                    .as_ref()
                    .map(|v| format!("{} {}", w.predicate.0, v.0))
            })
            .unwrap_or_else(|| w.predicate.0.clone())
    };
    let marker = |c: &StoredClaim| match c.lifecycle.as_str() {
        "Superseded" => "superseded: ",
        "Retracted" => "retracted: ",
        _ => "",
    };
    let conflicted: HashSet<&Id> = conflicts
        .iter()
        .flat_map(|c| c.claim_ids.0.iter())
        .collect();
    let mut placed: HashSet<&Id> = HashSet::new();
    for (e, _, _) in &s.entities {
        out.push_str(&format!("**{}** ({})\n", e.label, e.entity_type));
        let mut about: Vec<&(StoredClaim, KnowledgeClaim, i64)> = s
            .claims
            .iter()
            .filter(|(c, _, _)| c.subject_id == e.id)
            .collect();
        about.sort_by(|a, b| {
            b.2.cmp(&a.2)
                .then(b.0.updated_at_ms.cmp(&a.0.updated_at_ms))
        });
        for (claim, wire, _) in about.iter().take(PER_ENTITY) {
            placed.insert(&claim.id);
            let flag = if conflicted.contains(&claim.id) {
                "conflicted: "
            } else {
                marker(claim)
            };
            match claim.object_id.as_ref().and_then(|o| labels.get(o)) {
                Some(target) => out.push_str(&format!(
                    "  - {flag}{} -> {target}\n",
                    wire.predicate.0.to_uppercase()
                )),
                None => out.push_str(&format!("  - {flag}{}\n", text(wire))),
            }
        }
        out.push('\n');
    }
    let other: Vec<_> = s
        .claims
        .iter()
        .filter(|(c, _, _)| !placed.contains(&c.id) && !labels.contains_key(&c.subject_id))
        .take(PER_ENTITY)
        .collect();
    if !other.is_empty() {
        out.push_str("**Other Claims**\n");
        for (claim, wire, _) in other {
            out.push_str(&format!("  - {}{}\n", marker(claim), text(wire)));
        }
        out.push('\n');
    }
    let wires: HashMap<&Id, (&StoredClaim, &KnowledgeClaim)> =
        s.claims.iter().map(|(c, w, _)| (&c.id, (c, w))).collect();
    for conflict in conflicts {
        let mut ids = conflict.claim_ids.0.iter();
        let Some(first) = ids.next() else { continue };
        let Some((stored, w)) = wires.get(first) else {
            continue;
        };
        out.push_str(&format!(
            "⚠️ CONFLICT (unresolved): \"{}\" ({})\n",
            text(w),
            date(stored.updated_at_ms)
        ));
        for id in ids {
            if let Some((_, o)) = wires.get(id) {
                out.push_str(&format!("   conflicts with: \"{}\"\n", text(o)));
            }
        }
        out.push('\n');
    }
    out.trim_end().to_owned()
}

/// `yyyy-mm-dd` of a UTC millisecond timestamp.
fn date(ms: i64) -> String {
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = ms.div_euclid(86_400_000) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// What a graph read answers, before the operation wraps it.
pub(super) enum Read {
    Bundle(KnowledgeBundle),
    /// Summary text to publish (`knowledge.render_summary`).
    Summary(String),
}

fn no_text(q: &KnowledgeQuery) -> KnowledgeQuery {
    KnowledgeQuery {
        text: None,
        ..q.clone()
    }
}

impl KnowledgeService {
    /// One of the seven graph reads over a resolved, scope-first snapshot.
    pub(super) fn graph_read(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        operation: &'static str,
        scope: &crate::protocol::data::DataScope,
        q: &KnowledgeQuery,
    ) -> Result<(Read, i64), Failure> {
        let scoped = resolve_scope(
            snapshot.artifacts(),
            snapshot.authority(),
            ctx,
            scope,
            false,
        )?;
        let k = knowledge_read(snapshot);
        let th = thresholds(snapshot.configuration(), &scoped.readable)?;
        let c = Ctx {
            k,
            keys: scoped.keys(),
            scoped: &scoped,
            th: &th,
        };
        let revision = k.knowledge_scope_revision(&scoped.exact)?;
        let filter = ClaimFilter::of(q);
        let edges = clamp(q.neighbor_limit.get(), 1, 500) as u16;
        let read = match operation {
            // Without seeds or text a subgraph is a page of the whole visible graph (CodeGraph's
            // `GET graph`), continued with `page.cursor`.
            "knowledge.subgraph"
                if q.entity_ids.0.is_empty() && q.claim_ids.0.is_empty() && q.text.is_none() =>
            {
                Read::Bundle(full_graph(&c, ctx, q, revision, &filter)?)
            }
            "knowledge.subgraph" => {
                let (e, cl, more) = seeds(&c, q, clamp(q.entity_limit.get(), 1, 10), &filter)?;
                let bounds = Bounds {
                    hops: q.max_depth.get().min(MAX_HOPS),
                    entities: clamp(q.entity_limit.get(), 1, 100),
                    claims: clamp(q.claim_limit.get(), 1, 200),
                    edges,
                };
                let mut s = select(&c, &e, &cl, bounds, &filter)?;
                s.truncated |= more;
                Read::Bundle(bundle(&c, q, &s, q.include_conflicts)?)
            }
            "knowledge.query" => {
                let max_e = clamp(q.entity_limit.get(), 1, 50);
                let (e, cl, more) = seeds(&c, q, max_e.min(10), &filter)?;
                if e.is_empty() && cl.is_empty() {
                    let mut b = empty_bundle(&c)?;
                    b.truncated |= more;
                    b.summary = Some(content(NO_MEMORIES));
                    Read::Bundle(b)
                } else {
                    let bounds = Bounds {
                        hops: q.max_depth.get().min(MAX_HOPS),
                        entities: max_e,
                        claims: (2 * max_e).max(8).min(clamp(q.claim_limit.get(), 1, 200)),
                        edges,
                    };
                    let mut s = select(&c, &e, &cl, bounds, &filter)?;
                    s.truncated |= more;
                    let mut b = bundle(&c, q, &s, q.include_conflicts)?;
                    let text = render(&s, &b.conflicts.0);
                    b.truncated |= text.chars().count() > 65_536;
                    b.summary = Some(content(&text));
                    Read::Bundle(b)
                }
            }
            "knowledge.expand_frontier" => {
                if q.entity_ids.0.is_empty() && q.claim_ids.0.is_empty() {
                    return Err(invalid("frontier"));
                }
                let (e, cl, _) = seeds(&c, &no_text(q), 0, &filter)?;
                let limit = clamp(q.neighbor_limit.get(), 1, 100);
                let bounds = Bounds {
                    hops: q.max_depth.get().clamp(1, MAX_HOPS),
                    entities: (2 * limit).clamp(1, 100),
                    claims: (2 * limit).clamp(1, 200),
                    edges,
                };
                let s = select(&c, &e, &cl, bounds, &filter)?;
                let s = additions(s, &e, &cl, limit);
                Read::Bundle(bundle(&c, q, &s, q.include_conflicts)?)
            }
            "knowledge.render_summary" => {
                if q.entity_ids.0.is_empty() && q.claim_ids.0.is_empty() {
                    return Err(invalid("summary_seeds"));
                }
                let (e, cl, _) = seeds(&c, &no_text(q), 0, &filter)?;
                let n = e.len() + cl.len();
                let bounds = Bounds {
                    hops: 1,
                    entities: (3 * n).clamp(10, 100),
                    claims: (4 * n).clamp(20, 200),
                    edges,
                };
                let summary_filter = ClaimFilter {
                    include_superseded: true,
                    ..filter
                };
                let s = select(&c, &e, &cl, bounds, &summary_filter)?;
                let (found, _) = conflicts(&c, &s.claims)?;
                Read::Summary(if s.entities.is_empty() && s.claims.is_empty() {
                    NO_MEMORIES.to_owned()
                } else {
                    render(&s, &found)
                })
            }
            "knowledge.claim_bundle" => {
                let [id] = q.claim_ids.0.as_slice() else {
                    return Err(invalid("claim_id"));
                };
                let claim = c.claim(id)?.ok_or_else(|| not_found(super::CLAIM, id))?;
                let wire = claim_wire(k, &claim, &th)?;
                let mut b = super::bundle_of(k, &scoped, &claim, wire, &th, None)?;
                if !q.include_conflicts {
                    let conflicting: HashSet<Id> = b
                        .conflicts
                        .0
                        .iter()
                        .flat_map(|x| x.claim_ids.0.iter().cloned())
                        .filter(|x| x != &claim.id)
                        .collect();
                    b.conflicts = BoundedList(Vec::new());
                    b.claims.0.retain(|x| !conflicting.contains(&x.id));
                }
                if !q.include_supersession_chain {
                    b.supersession_ids = BoundedList(Vec::new());
                }
                if !q.include_evidence {
                    b.evidence = BoundedList(Vec::new());
                } else {
                    let limit = clamp(q.evidence_limit.get(), 1, 500);
                    b.truncated |= b.evidence.0.len() > limit;
                    b.evidence.0.truncate(limit);
                }
                Read::Bundle(b)
            }
            "knowledge.entity_bundle" => {
                let [id] = q.entity_ids.0.as_slice() else {
                    return Err(invalid("entity_id"));
                };
                let center = c
                    .entity(id)?
                    .ok_or_else(|| not_found("knowledge_entity", id))?;
                let neighbors = clamp(q.neighbor_limit.get(), 1, 100);
                let all = k.knowledge_entity_claims(&center.id, &c.keys, MAX_ROWS)?;
                let mut truncated = all.len() >= MAX_ROWS as usize;
                let mut entities: Vec<(StoredEntity, u32, i64)> = vec![(center.clone(), 0, 100)];
                let mut claims = Vec::new();
                // Neighbors the bound drops stay reachable: they and the center are the frontier.
                let mut omitted: Vec<Id> = Vec::new();
                for claim in all.into_iter().take(500) {
                    let wire = claim_wire(k, &claim, &th)?;
                    if !filter.admits(&claim, &wire) {
                        continue;
                    }
                    let other = [Some(&claim.subject_id), claim.object_id.as_ref()]
                        .into_iter()
                        .flatten()
                        .find(|x| **x != center.id)
                        .cloned();
                    if let Some(other) = other
                        && claim.lifecycle == "Active"
                        && !entities.iter().any(|(e, _, _)| e.id == other)
                    {
                        if entities.len() > neighbors {
                            truncated = true;
                            if !omitted.contains(&other) && c.entity(&other)?.is_some() {
                                omitted.push(other);
                            }
                        } else if let Some(e) = c.entity(&other)? {
                            entities.push((e, 1, entity_score(1, false)));
                        }
                    }
                    claims.push((claim, wire, 100));
                }
                let frontier = if truncated {
                    std::iter::once(center.id.clone()).chain(omitted).collect()
                } else {
                    Vec::new()
                };
                let s = Selection {
                    entities,
                    claims,
                    frontier,
                    truncated,
                };
                Read::Bundle(bundle(&c, q, &s, q.include_conflicts)?)
            }
            "knowledge.entity_graph" => match q.entity_ids.0.as_slice() {
                [id] => {
                    let center = c
                        .entity(id)?
                        .ok_or_else(|| not_found("knowledge_entity", id))?;
                    // Every edge up to the store page is read, so the neighbors beyond the bound
                    // are known: they join the frontier and expanding them reaches their edges.
                    let found = k.knowledge_entity_edges(&center.id, &c.keys, MAX_ROWS)?;
                    let mut more = found.len() >= MAX_ROWS as usize;
                    let mut entities: Vec<(StoredEntity, u32, i64)> =
                        vec![(center.clone(), 0, 100)];
                    let mut claims = Vec::new();
                    let mut omitted: Vec<Id> = Vec::new();
                    for claim in found {
                        let wire = claim_wire(k, &claim, &th)?;
                        if !filter.admits(&claim, &wire) {
                            continue;
                        }
                        let other = if claim.subject_id == center.id {
                            claim.object_id.clone().expect("edge claims have an object")
                        } else {
                            claim.subject_id.clone()
                        };
                        let known = entities.iter().any(|(e, _, _)| e.id == other);
                        // The bundle holds `neighborLimit` edges and at most the store page of
                        // entities, center included.
                        if claims.len() >= edges as usize
                            || (!known && entities.len() >= MAX_ROWS as usize)
                        {
                            more = true;
                            if !known && !omitted.contains(&other) && c.entity(&other)?.is_some() {
                                omitted.push(other);
                            }
                            continue;
                        }
                        if !known && let Some(e) = c.entity(&other)? {
                            entities.push((e, 1, entity_score(1, false)));
                        }
                        claims.push((claim, wire, claim_score(1, false)));
                    }
                    let s = Selection {
                        entities,
                        claims,
                        frontier: if more {
                            std::iter::once(center.id.clone()).chain(omitted).collect()
                        } else {
                            Vec::new()
                        },
                        truncated: more,
                    };
                    Read::Bundle(bundle(&c, q, &s, false)?)
                }
                // Admission requires exactly one entity.
                _ => return Err(invalid("entity_id")),
            },
            _ => return Err(invalid("knowledge_operation")),
        };
        Ok((read, revision))
    }
}

/// Frontier expansion keeps only what the expansion added, ranked by score, distance and label.
fn additions(mut s: Selection, entity_seeds: &[Id], claim_seeds: &[Id], limit: usize) -> Selection {
    let seeds: HashSet<&Id> = entity_seeds.iter().chain(claim_seeds.iter()).collect();
    // (score, hop, folded label, is entity, index)
    let mut nodes: Vec<(i64, u32, String, bool, usize)> = Vec::new();
    for (i, (e, hop, score)) in s.entities.iter().enumerate() {
        if !seeds.contains(&e.id) {
            nodes.push((*score, *hop, e.label.to_lowercase(), true, i));
        }
    }
    for (i, (claim, wire, score)) in s.claims.iter().enumerate() {
        if !seeds.contains(&claim.id) {
            let text = wire
                .normalized_text
                .as_ref()
                .map(|t| t.0.to_lowercase())
                .unwrap_or_default();
            nodes.push((*score, u32::MAX, text, false, i));
        }
    }
    nodes.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let over = nodes.len() > limit;
    // Additions that lose the ranking stay reachable: their entities become frontier entries.
    let ranked_out: Vec<Id> = nodes
        .iter()
        .skip(limit)
        .filter(|n| n.3)
        .map(|n| s.entities[n.4].0.id.clone())
        .collect();
    nodes.truncate(limit);
    let keep_e: HashSet<usize> = nodes.iter().filter(|n| n.3).map(|n| n.4).collect();
    let keep_c: HashSet<usize> = nodes.iter().filter(|n| !n.3).map(|n| n.4).collect();
    s.entities = std::mem::take(&mut s.entities)
        .into_iter()
        .enumerate()
        .filter(|(i, _)| keep_e.contains(i))
        .map(|(_, x)| x)
        .collect();
    s.claims = std::mem::take(&mut s.claims)
        .into_iter()
        .enumerate()
        .filter(|(i, _)| keep_c.contains(i))
        .map(|(_, x)| x)
        .collect();
    let added: HashSet<Id> = s.entities.iter().map(|(e, _, _)| e.id.clone()).collect();
    s.frontier.retain(|id| added.contains(id));
    for id in ranked_out {
        if !s.frontier.contains(&id) {
            s.frontier.push(id);
        }
    }
    s.truncated |= over;
    s
}

/// One page of the whole visible graph: entities newest first and the active edge claims
/// between entities on the page.
fn full_graph(
    c: &Ctx<'_>,
    ctx: &InvocationContext,
    q: &KnowledgeQuery,
    revision: i64,
    filter: &ClaimFilter,
) -> Result<KnowledgeBundle, Failure> {
    let limit = clamp(q.page.limit.get(), 1, 500);
    // Bound to every readable scope's revision: a change to any of them expires the cursor.
    let mut parts = Vec::new();
    for scope in &c.scoped.readable {
        parts.push(format!(
            "{}={}",
            scope.key(),
            c.k.knowledge_scope_revision(scope)?
        ));
    }
    parts.push(format!("{filter:?}"));
    let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
    let binding = CursorBinding::new(ctx, "knowledge.subgraph", &parts, revision);
    let after = match &q.page.cursor {
        None => None,
        Some(cursor) => {
            let at = binding.decode(&cursor.0)?;
            let (t, id) = at.split_once('|').ok_or(StorageError::CursorExpired)?;
            Some((
                t.parse::<i64>().map_err(|_| StorageError::CursorExpired)?,
                id.to_owned(),
            ))
        }
    };
    let rows = c.k.knowledge_entities(
        &c.keys,
        after.as_ref().map(|(t, id)| (*t, id.as_str())),
        ((limit + 1) as u16).min(MAX_ROWS),
    )?;
    let mut more = rows.len() > limit;
    let page: Vec<StoredEntity> = rows.into_iter().take(limit).collect();
    // At the store's page maximum the probe row does not fit: ask whether anything follows.
    if !more
        && page.len() == limit
        && let Some(last) = page.last()
    {
        more =
            !c.k.knowledge_entities(&c.keys, Some((last.updated_at_ms, last.id.as_str())), 1)?
                .is_empty();
    }
    let on_page: HashSet<&Id> = page.iter().map(|e| &e.id).collect();
    let mut claims = Vec::new();
    let mut seen = HashSet::new();
    let mut truncated = false;
    let per = clamp(q.neighbor_limit.get(), 1, 500) as u16;
    for e in &page {
        let (edges, edge_more) = c.edges(&e.id, per)?;
        truncated |= edge_more;
        for claim in edges {
            if claim.subject_id == e.id
                && claim
                    .object_id
                    .as_ref()
                    .is_some_and(|o| on_page.contains(o))
                && seen.insert(claim.id.clone())
            {
                // A page's claims fit one bundle; the rest is reported as truncated.
                if claims.len() >= MAX_ROWS as usize {
                    truncated = true;
                    break;
                }
                let wire = claim_wire(c.k, &claim, c.th)?;
                if !filter.admits(&claim, &wire) {
                    continue;
                }
                claims.push((claim, wire, 60));
            }
        }
    }
    let next = if more {
        page.last()
            .map(|last| binding.encode(&format!("{}|{}", last.updated_at_ms, last.id.as_str())))
    } else {
        None
    };
    let s = Selection {
        entities: page.into_iter().map(|e| (e, 0, 0)).collect(),
        claims,
        frontier: Vec::new(),
        truncated,
    };
    let mut b = bundle(c, q, &s, false)?;
    b.next_cursor = next.map(BoundedText);
    Ok(b)
}

fn empty_bundle(c: &Ctx<'_>) -> Result<KnowledgeBundle, StorageError> {
    Ok(KnowledgeBundle {
        entities: BoundedList(Vec::<KnowledgeEntity>::new()),
        claims: BoundedList(Vec::new()),
        evidence: BoundedList(Vec::new()),
        frontier: BoundedList(Vec::new()),
        next_cursor: None,
        truncated: false,
        conflicts: BoundedList(Vec::new()),
        supersession_ids: BoundedList(Vec::new()),
        summary: None,
        relations: Some(BoundedList(Vec::new())),
        revisions: Some(BoundedList(Vec::new())),
        scope_revision: Some(rev(c.k.knowledge_scope_revision(&c.scoped.exact)?)),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn civil_dates_match_known_days() {
        assert_eq!(super::date(0), "1970-01-01");
        assert_eq!(super::date(1_700_000_000_000), "2023-11-14");
        assert_eq!(super::date(951_782_400_000), "2000-02-29");
    }
}
