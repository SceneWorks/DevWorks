//! The Knowledge commands. Each runs inside the command's one unit of work: every identifier is
//! resolved against the invocation's scope first, every changed record is compare-and-set on
//! its revision, every revision is kept as an attributed snapshot, every touched scope's
//! revision advances, and the write's durable receipt, journal (before/after revisions) and
//! history event commit with it or not at all.
use super::import::{self, Candidate, Outcome};
use super::model::{self, fold, fold_tags, wire};
use super::{
    CHALLENGE, CLAIM, ENTITY, EVIDENCE, LESSON, LIFECYCLE, MAX_CHALLENGES, MAX_LIST,
    MAX_SOURCE_DELETE, PREVIEW, Reach, SCOPE, Scoped, WRITE_AGGREGATE, WRITE_EVENT, claim_wire,
    diagnostic, entity_wire, nests, no_applicability, reach, receipt_of, thresholds, writable,
};
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::AdmittedCommand;
use crate::operations::knowledge as ops;
use crate::ports::IdKind;
use crate::ports::storage::{
    ArtifactRead, AuthorityRead, ClaimBody, ClaimQuery, KNOWLEDGE_PAGE_MAX, KnowledgeRead,
    KnowledgeScope, NewEvent, StorageError, StoredAddress, StoredChallenge, StoredClaim,
    StoredEntity, StoredEvidence, StoredHistory, StoredLesson, StoredLessonEvent, StoredRelation,
    StoredVersion, StoredWrite, WriteChange, WriteTransaction,
};
use crate::protocol::provider::{ArtifactRef, SchemaRef};
use crate::protocol::records::{
    ClaimDraft, Diagnostic, KnowledgeWriteReceipt, LessonDraft, MutationSummary, Severity,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::{DomainErrorCode, EventId, Id, Revision, RevisionDimension};
use crate::services::artifacts::{Viewer, visible};
use crate::services::sessions::{Failure, Refusal, not_found, rev};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};

fn refuse(code: DomainErrorCode, kind: &'static str, id: Option<&Id>) -> Failure {
    Refusal::new(code, kind, id.cloned()).into()
}
fn invalid(kind: &'static str) -> Failure {
    refuse(DomainErrorCode::InvalidArgument, kind, None)
}
fn conflict(kind: &'static str, id: &Id) -> Failure {
    refuse(DomainErrorCode::Conflict, kind, Some(id))
}
fn revision_conflict(kind: &'static str, id: &Id, expected: &Revision, current: i64) -> Failure {
    Failure::Storage(StorageError::conflict(
        RevisionDimension::Claim,
        kind,
        id.clone(),
        expected.clone(),
        rev(current),
    ))
}
fn is_user(ctx: &InvocationContext) -> bool {
    matches!(
        ctx.principal_kind(),
        PrincipalKind::Human | PrincipalKind::Host
    )
}
/// Decisions only the user (or the host acting for the user) makes.
fn require_user(ctx: &InvocationContext, kind: &'static str) -> Result<(), Failure> {
    if is_user(ctx) {
        Ok(())
    } else {
        Err(refuse(DomainErrorCode::PermissionDenied, kind, None))
    }
}
/// Proposals belong to the dreaming review flow; no knowledge command decides one.
fn no_proposal(proposal: &Option<Id>) -> Result<(), Failure> {
    match proposal {
        Some(p) => Err(refuse(
            DomainErrorCode::UnsupportedCapability,
            "proposal",
            Some(p),
        )),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// The write unit

/// One Knowledge write: its identity, the changes it made and the scopes it touched.
pub(super) struct Writer<'a> {
    pub tx: &'a mut dyn WriteTransaction,
    ctx: &'a InvocationContext,
    op: &'static str,
    pub write_id: Id,
    pub now: i64,
    changes: Vec<WriteChange>,
    entity_ids: Vec<Id>,
    claim_ids: Vec<Id>,
    pub diagnostics: Vec<Diagnostic>,
    touched: BTreeMap<String, KnowledgeScope>,
    th: BTreeMap<String, i64>,
}
impl<'a> Writer<'a> {
    fn new(
        tx: &'a mut dyn WriteTransaction,
        ctx: &'a InvocationContext,
        op: &'static str,
    ) -> Result<Self, Failure> {
        let write_id = tx.new_id(IdKind::KnowledgeWrite);
        let now = tx.now_ms();
        Ok(Self {
            tx,
            ctx,
            op,
            write_id,
            now,
            changes: Vec::new(),
            entity_ids: Vec::new(),
            claim_ids: Vec::new(),
            diagnostics: Vec::new(),
            touched: BTreeMap::new(),
            th: BTreeMap::new(),
        })
    }
    fn k(&mut self) -> &mut dyn crate::ports::storage::KnowledgeRepository {
        self.tx.knowledge()
    }
    fn threshold(&mut self, scope: &KnowledgeScope) -> Result<i64, StorageError> {
        if let Some(t) = self.th.get(&scope.key()) {
            return Ok(*t);
        }
        let t = thresholds(self.tx.configuration(), std::slice::from_ref(scope))?;
        self.th.extend(t);
        Ok(self.th.get(&scope.key()).copied().unwrap_or(3))
    }
    fn note(&mut self, severity: Severity, code: &str, message: &str) {
        let id = format!("{}-{}", self.write_id.as_str(), self.diagnostics.len() + 1);
        self.diagnostics.push(diagnostic(
            &id,
            severity,
            super::WRITES_COMPONENT,
            code,
            message,
        ));
    }
    fn history(
        &mut self,
        target_id: &Id,
        revision: i64,
        target_kind: &str,
        change: &str,
        lifecycle: &str,
        snapshot: Value,
    ) -> Result<(), StorageError> {
        let h = StoredHistory {
            target_id: target_id.clone(),
            revision,
            target_kind: target_kind.into(),
            change: change.into(),
            author: self.ctx.principal().clone(),
            at_ms: self.now,
            write_id: self.write_id.clone(),
            lifecycle: lifecycle.into(),
            snapshot,
        };
        self.k().put_knowledge_history(&h)
    }
    /// Stores a claim (insert when `expected` is `None`), its snapshot and the change.
    pub fn put_claim(
        &mut self,
        mut claim: StoredClaim,
        expected: Option<i64>,
        change: &str,
    ) -> Result<StoredClaim, Failure> {
        // Every list a claim carries fits its wire list: a write past it is refused, never cut.
        if claim.tags.len() > MAX_LIST || claim.body.evidence_ids.len() > MAX_LIST {
            return Err(refuse(
                DomainErrorCode::InvalidArgument,
                "list_bound",
                Some(&claim.id),
            ));
        }
        claim.updated_at_ms = self.now.max(claim.created_at_ms);
        let stored = self.k().put_knowledge_claim(&claim, expected)?;
        let scope = stored.scope.clone();
        let th = self.threshold(&scope)?;
        let mut thm = BTreeMap::new();
        thm.insert(scope.key(), th);
        let wire = claim_wire(self.tx.knowledge(), &stored, &thm)?;
        let mut snapshot = serde_json::to_value(&wire)
            .map_err(|e| StorageError::InvalidRecord(format!("claim snapshot: {e}")))?;
        // The revisions of the entities this revision names, so a read of it shows them as
        // they were (not as later revisions changed them).
        let mut entities = serde_json::Map::new();
        for id in std::iter::once(&stored.subject_id).chain(stored.object_id.iter()) {
            if let Some(e) = self.k().knowledge_entity(id)? {
                entities.insert(e.id.as_str().to_owned(), json!(e.revision));
            }
        }
        snapshot[super::SNAPSHOT_ENTITIES] = Value::Object(entities);
        self.history(
            &stored.id,
            stored.revision,
            "Claim",
            change,
            &stored.lifecycle,
            snapshot,
        )?;
        self.changes.push(WriteChange {
            target_kind: "Claim".into(),
            id: stored.id.clone(),
            before: expected,
            after: Some(stored.revision),
        });
        if !self.claim_ids.contains(&stored.id) {
            self.claim_ids.push(stored.id.clone());
        }
        self.touched.insert(scope.key(), scope);
        Ok(stored)
    }
    pub fn put_entity(
        &mut self,
        mut entity: StoredEntity,
        expected: Option<i64>,
        change: &str,
    ) -> Result<StoredEntity, Failure> {
        if entity.aliases.len() > MAX_LIST {
            return Err(refuse(
                DomainErrorCode::InvalidArgument,
                "list_bound",
                Some(&entity.id),
            ));
        }
        entity.updated_at_ms = self.now.max(entity.created_at_ms);
        let stored = self.k().put_knowledge_entity(&entity, expected)?;
        let snapshot = serde_json::to_value(entity_wire(&stored))
            .map_err(|e| StorageError::InvalidRecord(format!("entity snapshot: {e}")))?;
        let lifecycle = if stored.deleted_at_ms.is_some() {
            "Deleted"
        } else {
            "Active"
        };
        self.history(
            &stored.id,
            stored.revision,
            "Entity",
            change,
            lifecycle,
            snapshot,
        )?;
        self.changes.push(WriteChange {
            target_kind: "Entity".into(),
            id: stored.id.clone(),
            before: expected,
            after: Some(stored.revision),
        });
        if !self.entity_ids.contains(&stored.id) {
            self.entity_ids.push(stored.id.clone());
        }
        self.touched
            .insert(stored.scope.key(), stored.scope.clone());
        Ok(stored)
    }
    fn put_evidence(&mut self, e: &StoredEvidence, replace: bool) -> Result<(), Failure> {
        self.k().put_knowledge_evidence(e, replace)?;
        self.changes.push(WriteChange {
            target_kind: "Evidence".into(),
            id: e.id.clone(),
            before: None,
            after: None,
        });
        self.touched.insert(e.scope.key(), e.scope.clone());
        Ok(())
    }
    fn touch(&mut self, scope: &KnowledgeScope) {
        self.touched.insert(scope.key(), scope.clone());
    }
    fn changed(&self) -> bool {
        !self.changes.is_empty() || !self.touched.is_empty()
    }
    /// Advances every touched scope, records the write and its history event, and returns the
    /// receipt (its revision is `primary`'s scope revision after the write).
    fn finish(mut self, primary: &KnowledgeScope) -> Result<KnowledgeWriteReceipt, Failure> {
        // The receipt (and write_status) names every claim and entity the write changed.
        if self.claim_ids.len() > MAX_LIST || self.entity_ids.len() > MAX_LIST {
            return Err(invalid("receipt_bound"));
        }
        for s in self.touched.clone().into_values() {
            self.k().bump_scope(&s)?;
        }
        let scope_revision = self.k().knowledge_scope_revision(primary)?.max(1);
        let write = StoredWrite {
            write_id: self.write_id.clone(),
            operation: self.op.into(),
            scope: primary.clone(),
            actor: self.ctx.principal().clone(),
            session_id: self.ctx.session_id().cloned(),
            at_ms: self.now,
            scope_revision,
            entity_ids: self.entity_ids.clone(),
            claim_ids: self.claim_ids.clone(),
            changes: self.changes.clone(),
            diagnostics: self
                .diagnostics
                .iter()
                .filter_map(|d| serde_json::to_value(d).ok())
                .collect(),
        };
        self.k().put_knowledge_write(&write)?;
        if !self.changed() {
            // Nothing changed: the receipt answers write_status; history records changes only.
            return Ok(receipt_of(&write));
        }
        let event = EventId(self.tx.new_id(IdKind::Event));
        let counts = |kind: &str| {
            self.changes
                .iter()
                .filter(|c| c.target_kind == kind)
                .count()
        };
        let payload = json!({
            "writeId": self.write_id.as_str(),
            "operation": self.op,
            "scope": primary.key(),
            "scopeRevision": scope_revision,
            "claimIds": self.claim_ids.iter().map(Id::as_str).collect::<Vec<_>>(),
            "entityIds": self.entity_ids.iter().map(Id::as_str).collect::<Vec<_>>(),
            "changed": {"claims": counts("Claim"), "entities": counts("Entity"),
                "evidence": counts("Evidence"), "relations": counts("Relation")},
        });
        self.tx.history().append(NewEvent {
            id: event,
            actor: self.ctx.principal().clone(),
            project_id: primary.project().cloned(),
            root_id: None,
            operation_id: None,
            aggregate_kind: WRITE_AGGREGATE.into(),
            aggregate_id: self.write_id.as_str().to_owned(),
            aggregate_revision: 1,
            kind: WRITE_EVENT.into(),
            payload,
        })?;
        Ok(receipt_of(&write))
    }
}

/// The decision of a write: applied when it changed something, otherwise no change.
fn outcome(
    w: Writer<'_>,
    primary: &KnowledgeScope,
    wrap: impl FnOnce(KnowledgeWriteReceipt) -> OperationResult,
) -> Result<DomainDecision, Failure> {
    let changed = w.changed();
    let receipt = w.finish(primary)?;
    let result = wrap(receipt);
    Ok(if changed {
        DomainDecision::Applied { result }
    } else {
        DomainDecision::NoChange { result }
    })
}

pub(super) fn decide(
    ctx: &InvocationContext,
    command: &AdmittedCommand,
    input: &ops::Input,
    tx: &mut dyn WriteTransaction,
) -> Result<DomainDecision, Failure> {
    match input {
        ops::Input::Record(i) => record(tx, ctx, i),
        ops::Input::Revise(i) => revise(tx, ctx, i),
        ops::Input::Relate(i) => relate(tx, ctx, i),
        ops::Input::Tag(i) => tag(tx, ctx, i),
        ops::Input::Verify(i) => verify(tx, ctx, i),
        ops::Input::Retract(i) => retract(tx, ctx, i),
        ops::Input::Scope(i) => move_scope(tx, ctx, i),
        ops::Input::DeleteBySource(i) => delete_by_source(tx, ctx, i),
        ops::Input::DeleteSelected(i) => delete_selected(tx, ctx, i),
        ops::Input::ImportUserData(i) => import_user_data(tx, ctx, command, i),
        ops::Input::Promote(i) => promote(tx, ctx, i),
        ops::Input::Challenge(i) => challenge(tx, ctx, i),
        _ => Err(refuse(
            DomainErrorCode::UnsupportedCapability,
            "operation",
            None,
        )),
    }
}

// ---------------------------------------------------------------------------------------------
// Scope and identifier resolution

/// The requested scope and the principal's reach, both read in this unit of work.
fn scope_for(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    scope: &crate::protocol::data::DataScope,
    write: bool,
) -> Result<(Scoped, Reach), Failure> {
    let r = reach(tx.authority(), ctx)?;
    let s = super::resolve_scope_with(&r, ctx, scope, write, |p| tx.artifacts().project_exists(p))?;
    Ok((s, r))
}

/// A claim the invocation may see in `scoped` (otherwise `NotFound`, never disclosed).
fn visible_claim(w: &mut Writer<'_>, scoped: &Scoped, id: &Id) -> Result<StoredClaim, Failure> {
    match w.k().knowledge_claim(id)? {
        Some(c) if scoped.sees(&c.scope) => Ok(c),
        _ => Err(not_found(CLAIM, id).into()),
    }
}
/// A claim of exactly the request's scope at its expected revision, not deleted.
fn exact_claim(
    w: &mut Writer<'_>,
    scoped: &Scoped,
    id: &Id,
    expected: &Revision,
) -> Result<StoredClaim, Failure> {
    let c = visible_claim(w, scoped, id)?;
    if c.scope != scoped.exact {
        return Err(refuse(DomainErrorCode::PermissionDenied, SCOPE, Some(id)));
    }
    if c.lifecycle == "Deleted" {
        return Err(conflict(LIFECYCLE, id));
    }
    if c.revision != expected.get() {
        return Err(revision_conflict(CLAIM, id, expected, c.revision));
    }
    Ok(c)
}
fn require_active(c: &StoredClaim) -> Result<(), Failure> {
    if c.lifecycle == "Active" {
        Ok(())
    } else {
        Err(conflict(LIFECYCLE, &c.id))
    }
}

/// Batch-local client identifiers resolved to stored identifiers.
#[derive(Default)]
struct Refs {
    entities: BTreeMap<String, Id>,
    evidence: BTreeMap<String, Id>,
}

/// An entity the claim may reference: one of the batch, or a live visible entity of the
/// claim's scope or general Knowledge.
fn resolve_entity(
    w: &mut Writer<'_>,
    scoped: &Scoped,
    refs: &Refs,
    id: &Id,
) -> Result<Id, Failure> {
    if let Some(x) = refs.entities.get(id.as_str()) {
        return Ok(x.clone());
    }
    match w.k().knowledge_entity(id)? {
        Some(e)
            if e.deleted_at_ms.is_none()
                && scoped.sees(&e.scope)
                && nests(&e.scope, &scoped.exact) =>
        {
            Ok(e.id)
        }
        _ => Err(not_found(ENTITY, id).into()),
    }
}
/// An object: an entity (as [`resolve_entity`]) or a visible, non-deleted claim.
fn resolve_object(
    w: &mut Writer<'_>,
    scoped: &Scoped,
    refs: &Refs,
    id: &Id,
) -> Result<Id, Failure> {
    if let Ok(e) = resolve_entity(w, scoped, refs, id) {
        return Ok(e);
    }
    match w.k().knowledge_claim(id)? {
        Some(c)
            if c.lifecycle != "Deleted"
                && scoped.sees(&c.scope)
                && nests(&c.scope, &scoped.exact) =>
        {
            Ok(c.id)
        }
        _ => Err(not_found(ENTITY, id).into()),
    }
}
/// Evidence the claim may cite: of the batch, or live visible evidence of its scope or general.
fn resolve_evidence(
    w: &mut Writer<'_>,
    scoped: &Scoped,
    refs: &Refs,
    ids: &[Id],
) -> Result<Vec<Id>, Failure> {
    let mut out: Vec<Id> = Vec::new();
    for id in ids {
        let resolved = match refs.evidence.get(id.as_str()) {
            Some(x) => x.clone(),
            None => match w.k().knowledge_evidence(id)? {
                Some(e)
                    if e.deleted_at_ms.is_none()
                        && scoped.sees(&e.scope)
                        && nests(&e.scope, &scoped.exact) =>
                {
                    e.id
                }
                _ => return Err(not_found(EVIDENCE, id).into()),
            },
        };
        if !out.contains(&resolved) {
            out.push(resolved);
        }
    }
    Ok(out)
}

/// The verification basis of `evidence`: each source with the revision it was taken at (the
/// evidence's own, else `default_revision`).
fn basis_of(
    w: &mut Writer<'_>,
    evidence: &[Id],
    default_revision: Option<&str>,
    mut basis: Vec<(String, String)>,
) -> Result<Vec<(String, String)>, Failure> {
    for id in evidence {
        if let Some(e) = w.k().knowledge_evidence(id)?
            && let Some(r) = e.source_revision.as_deref().or(default_revision)
        {
            basis.retain(|(s, _)| *s != e.source_ref);
            basis.push((e.source_ref.clone(), r.to_owned()));
        }
    }
    basis.sort();
    Ok(basis)
}

/// Who made a claim or decision (KNOW-03): the user's own ingress is always the user; an agent
/// records a decision the user made or approved with `originRef` (the approving message or
/// review); anything else an agent records is its own.
fn origin_for(
    ctx: &InvocationContext,
    draft: &ClaimDraft,
) -> Result<(String, Option<String>), Failure> {
    let origin = draft.origin.as_ref().map(wire);
    let reference = draft.origin_ref.as_ref().map(|r| r.0.clone());
    if is_user(ctx) {
        if origin.as_deref() == Some("Agent") {
            return Err(invalid("origin"));
        }
        return Ok(("User".into(), reference));
    }
    match origin.as_deref() {
        Some("User") if reference.is_some() => Ok(("User".into(), reference)),
        Some("User") => Err(invalid("origin_ref")),
        _ => Ok(("Agent".into(), reference)),
    }
}

/// A validated lesson draft: who may state which source (KNOW-05).
fn lesson_for(
    ctx: &InvocationContext,
    draft: Option<&LessonDraft>,
) -> Result<LessonDraft, Failure> {
    let d = match draft {
        Some(d) => d.clone(),
        None => LessonDraft {
            source: model::parse(if ctx.principal_kind() == PrincipalKind::Human {
                "Prompt"
            } else {
                "Agent"
            })
            .expect("lesson source"),
            source_ref: None,
            quoted_words: None,
            applicability: no_applicability(),
            same_as: None,
        },
    };
    let source = wire(&d.source);
    if model::direct_user_source(&source) && ctx.principal_kind() != PrincipalKind::Human {
        // An agent relays the user's prompt or review, with the user's words attached.
        return Err(refuse(DomainErrorCode::PermissionDenied, LESSON, None));
    }
    if matches!(source.as_str(), "RelayedPrompt" | "RelayedReview")
        && (d
            .quoted_words
            .as_ref()
            .is_none_or(|q| q.0.trim().is_empty())
            || d.source_ref.is_none())
    {
        return Err(invalid("quoted_words"));
    }
    Ok(d)
}

/// The stored claim a draft describes (`prior`: the claim it revises).
fn build_claim(
    w: &mut Writer<'_>,
    scoped: &Scoped,
    refs: &Refs,
    d: &ClaimDraft,
    prior: Option<&StoredClaim>,
) -> Result<StoredClaim, Failure> {
    let kind = wire(&d.kind);
    let entry = match &d.entry_kind {
        Some(e) => {
            let e = wire(e);
            if model::kind_of(&e) != kind {
                return Err(invalid("entry_kind"));
            }
            e
        }
        None => match prior {
            Some(p) if model::kind_of(&p.entry_kind) == kind => p.entry_kind.clone(),
            _ => model::default_entry(&kind).to_owned(),
        },
    };
    let subject = resolve_entity(w, scoped, refs, &d.subject_id)?;
    let object = d
        .object_id
        .as_ref()
        .map(|o| resolve_object(w, scoped, refs, o))
        .transpose()?;
    let evidence = resolve_evidence(w, scoped, refs, &d.evidence_ids.0)?;
    let category = model::category_tag(&entry);
    let tags = fold_tags(
        d.tags
            .0
            .iter()
            .map(|t| t.0.as_str())
            .chain(std::iter::once(category)),
    );
    let (origin, origin_ref) = origin_for(w.ctx, d)?;
    let autonomous = entry == "Decision" && origin == "Agent";
    let basis = basis_of(
        w,
        &evidence,
        d.source_revision.as_ref().map(|r| r.0.as_str()),
        Vec::new(),
    )?;
    let mut body = ClaimBody {
        value: d.value.as_ref().map(|v| v.0.clone()),
        value_json: d.value_json.as_ref().map(|v| v.0.clone()),
        normalized_text: d.normalized_text.as_ref().map(|v| v.0.clone()),
        confidence: d.confidence.get(),
        effective_at_ms: d.effective_at_ms.as_ref().map(|c| c.get()),
        supersedes: prior.and_then(|p| p.body.supersedes.clone()),
        superseded_by: prior.and_then(|p| p.body.superseded_by.clone()),
        origin,
        origin_ref,
        autonomous,
        author_kind: prior.map_or_else(
            || format!("{:?}", w.ctx.principal_kind()),
            |p| p.body.author_kind.clone(),
        ),
        verified_at_ms: None,
        verified_source_revision: None,
        verified_by: None,
        evidence_ids: evidence,
        lesson_source_ref: None,
        lesson_quoted_words: None,
        lesson_effect: None,
        lesson_reason: None,
        reason: None,
    };
    let lesson = if entry == "Lesson" {
        match prior {
            Some(p) => {
                let Some(stored) = p.lesson.clone() else {
                    return Err(invalid("entry_kind"));
                };
                let mut stored = stored;
                if let Some(l) = &d.lesson {
                    if wire(&l.source) != stored.source || l.same_as.is_some() {
                        // A lesson's provenance never changes by revision.
                        return Err(invalid("lesson"));
                    }
                    stored.applicability =
                        serde_json::to_value(model::fold_applicability(&l.applicability))
                            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
                }
                body.lesson_source_ref = p.body.lesson_source_ref.clone();
                body.lesson_quoted_words = p.body.lesson_quoted_words.clone();
                body.lesson_effect = p.body.lesson_effect.clone();
                body.lesson_reason = p.body.lesson_reason.clone();
                Some(stored)
            }
            None => {
                let l = lesson_for(w.ctx, d.lesson.as_ref())?;
                let source = wire(&l.source);
                let user = model::user_source(&source);
                body.lesson_source_ref = l.source_ref.as_ref().map(|r| r.0.clone());
                body.lesson_quoted_words = l.quoted_words.as_ref().map(|q| q.0.clone());
                body.lesson_effect = user.then(|| "UserInstruction".to_owned());
                Some(StoredLesson {
                    rule_state: if user { "Effective" } else { "Candidate" }.into(),
                    source,
                    applicability: serde_json::to_value(model::fold_applicability(
                        &l.applicability,
                    ))
                    .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
                    effective_at_ms: user.then_some(w.now),
                })
            }
        }
    } else {
        if d.lesson.is_some() || prior.is_some_and(|p| p.lesson.is_some()) {
            return Err(invalid("lesson"));
        }
        None
    };
    Ok(StoredClaim {
        id: prior.map_or_else(|| w.tx.new_id(IdKind::KnowledgeClaim), |p| p.id.clone()),
        scope: prior.map_or_else(|| scoped.exact.clone(), |p| p.scope.clone()),
        revision: prior.map_or(1, |p| p.revision),
        kind,
        entry_kind: entry,
        lifecycle: "Active".into(),
        subject_id: subject,
        predicate: d.predicate.0.clone(),
        object_id: object,
        source: d.source.as_ref().map(|s| s.0.clone()),
        author: prior.map_or_else(|| w.ctx.principal().clone(), |p| p.author.clone()),
        session_id: prior.map_or_else(|| w.ctx.session_id().cloned(), |p| p.session_id.clone()),
        tags,
        basis,
        lesson,
        body,
        created_at_ms: prior.map_or(w.now, |p| p.created_at_ms),
        updated_at_ms: w.now,
        deleted_at_ms: None,
    })
}

/// Records a lesson's source event; `false` when that event was already recorded.
fn add_event(
    w: &mut Writer<'_>,
    claim: &Id,
    source: &str,
    source_ref: Option<&str>,
) -> Result<bool, Failure> {
    let event = StoredLessonEvent {
        claim_id: claim.clone(),
        event_key: model::event_key(source, source_ref, w.write_id.as_str()),
        source: source.to_owned(),
        source_ref: source_ref.map(str::to_owned),
        session_id: w.ctx.session_id().cloned(),
        actor: w.ctx.principal().clone(),
        at_ms: w.now,
        write_id: w.write_id.clone(),
        merge_path: Vec::new(),
    };
    Ok(w.k().add_knowledge_lesson_event(&event)?)
}

/// An inferred lesson whose distinct events reach its scope's threshold takes effect now,
/// without approval (KNOW-05).
fn take_effect_if_due(w: &mut Writer<'_>, c: StoredClaim) -> Result<StoredClaim, Failure> {
    let Some(l) = &c.lesson else { return Ok(c) };
    if !matches!(l.rule_state.as_str(), "Candidate" | "Proposed") {
        return Ok(c);
    }
    let events = w.k().knowledge_lesson_event_count(&c.id)?;
    let threshold = w.threshold(&c.scope)?;
    if events < threshold {
        return Ok(c);
    }
    let mut next = c.clone();
    if let Some(l) = next.lesson.as_mut() {
        l.rule_state = "Effective".into();
        l.effective_at_ms = Some(w.now);
    }
    next.body.lesson_effect = Some("Recurrence".into());
    let message = format!(
        "Lesson {} recurred in {events} distinct source events (threshold {threshold}) and took \
         effect as a rule.",
        c.id.as_str()
    );
    w.note(Severity::Info, "rule_effective", &message);
    w.put_claim(next, Some(c.revision), "TookEffect")
}

/// Notes active claims of the same subject and predicate stating something else (kept, shown
/// as an open conflict).
fn note_conflicts(w: &mut Writer<'_>, scoped: &Scoped, c: &StoredClaim) -> Result<(), Failure> {
    let keys = scoped.keys();
    let others = w
        .k()
        .knowledge_claims_about(&keys, &c.subject_id, &c.predicate, 50)?;
    let ids: Vec<String> = others
        .iter()
        .filter(|o| {
            o.id != c.id
                && (o.body.value != c.body.value
                    || o.body.value_json != c.body.value_json
                    || o.object_id != c.object_id)
        })
        .map(|o| o.id.as_str().to_owned())
        .collect();
    if !ids.is_empty() {
        let message = format!(
            "Claim {} conflicts with active claims {}; both are kept until resolved.",
            c.id.as_str(),
            ids.join(", ")
        );
        w.note(Severity::Warning, "conflict", &message);
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// knowledge.record

fn record(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    i: &ops::RecordInput,
) -> Result<DomainDecision, Failure> {
    let (scoped, _) = scope_for(tx, ctx, &i.scope, true)?;
    let mut w = Writer::new(tx, ctx, "knowledge.record")?;
    let mut refs = Refs::default();
    for e in &i.batch.entities.0 {
        if refs.entities.contains_key(e.client_id.as_str()) {
            return Err(invalid("client_id"));
        }
        let name = e.canonical_name.as_ref().unwrap_or(&e.label);
        let key = model::entity_key(&e.entity_type.0, &name.0);
        let aliases = |existing: &[String]| {
            let mut out = existing.to_vec();
            for a in &e.aliases.0 {
                if !out.iter().any(|x| fold(x) == fold(&a.0)) {
                    out.push(a.0.clone());
                }
            }
            out
        };
        let id = match w.k().knowledge_entity_by_key(&scoped.exact, &key)? {
            Some(existing) => {
                let merged = aliases(&existing.aliases);
                let mut next = existing.clone();
                next.aliases = merged;
                if next.external_id.is_none() {
                    next.external_id = e.external_id.as_ref().map(|x| x.0.clone());
                }
                if next.canonical_name.is_none() {
                    next.canonical_name = e.canonical_name.as_ref().map(|x| x.0.clone());
                }
                if next != existing {
                    w.put_entity(next, Some(existing.revision), "Revised")?;
                }
                let message = format!(
                    "Entity draft {} is the existing entity {}.",
                    e.client_id.as_str(),
                    existing.id.as_str()
                );
                w.note(Severity::Info, "entity_reused", &message);
                existing.id
            }
            None => {
                let entity = StoredEntity {
                    id: w.tx.new_id(IdKind::KnowledgeEntity),
                    scope: scoped.exact.clone(),
                    revision: 1,
                    label: e.label.0.clone(),
                    entity_type: e.entity_type.0.clone(),
                    aliases: aliases(&[]),
                    canonical_key: key,
                    canonical_name: e.canonical_name.as_ref().map(|x| x.0.clone()),
                    external_id: e.external_id.as_ref().map(|x| x.0.clone()),
                    source: e.source.as_ref().map(|x| x.0.clone()),
                    author: ctx.principal().clone(),
                    created_at_ms: w.now,
                    updated_at_ms: w.now,
                    deleted_at_ms: None,
                };
                w.put_entity(entity, None, "Recorded")?.id
            }
        };
        refs.entities.insert(e.client_id.as_str().to_owned(), id);
    }
    // Evidence: new records first, so claims of the batch can cite them.
    let mut attach: Vec<(Id, Id)> = Vec::new();
    for ev in &i.batch.evidence.0 {
        if refs.evidence.contains_key(ev.id.as_str()) {
            return Err(invalid("evidence_id"));
        }
        // A cited artifact must be one the invocation may see, at exactly that version.
        if let Some(a) = &ev.source.artifact {
            let viewer = {
                let authority: &dyn AuthorityRead = w.tx.authority();
                Viewer::resolve(authority, ctx)?
            };
            import_version(w.tx.artifacts(), &viewer, a)?;
        }
        let id = w.tx.new_id(IdKind::KnowledgeEvidence);
        let stored = StoredEvidence {
            id: id.clone(),
            scope: scoped.exact.clone(),
            kind: wire(&ev.source.kind),
            source_ref: ev.source.source_ref.0.clone(),
            artifact: ev
                .source
                .artifact
                .as_ref()
                .map(serde_json::to_value)
                .transpose()
                .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
            locator: ev.source.locator.as_ref().map(|l| l.0.clone()),
            snippet: ev.snippet.0.clone(),
            metadata: ev
                .metadata
                .as_ref()
                .map(serde_json::to_value)
                .transpose()
                .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
            source_revision: ev.source_revision.as_ref().map(|r| r.0.clone()),
            author: ctx.principal().clone(),
            recorded_at_ms: w.now,
            deleted_at_ms: None,
        };
        w.put_evidence(&stored, false)?;
        refs.evidence.insert(ev.id.as_str().to_owned(), id.clone());
        if let Some(claim) = &ev.claim_id {
            attach.push((claim.clone(), id));
        }
    }
    // Evidence-only writes attach to existing claims of the scope.
    for (claim, evidence) in attach {
        let c = visible_claim(&mut w, &scoped, &claim)?;
        if c.scope != scoped.exact {
            return Err(refuse(
                DomainErrorCode::PermissionDenied,
                SCOPE,
                Some(&claim),
            ));
        }
        if c.lifecycle == "Deleted" {
            return Err(conflict(LIFECYCLE, &claim));
        }
        let mut next = c.clone();
        if !next.body.evidence_ids.contains(&evidence) {
            next.body.evidence_ids.push(evidence.clone());
        }
        next.basis = basis_of(&mut w, &[evidence], None, next.basis)?;
        w.put_claim(next, Some(c.revision), "EvidenceAdded")?;
    }
    for d in &i.batch.claims.0 {
        record_claim(&mut w, &scoped, &refs, d)?;
    }
    let primary = scoped.exact.clone();
    outcome(w, &primary, |value| OperationResult::KnowledgeRecord {
        value,
    })
}

fn record_claim(
    w: &mut Writer<'_>,
    scoped: &Scoped,
    refs: &Refs,
    d: &ClaimDraft,
) -> Result<(), Failure> {
    if let Some(target) = d.lesson.as_ref().and_then(|l| l.same_as.as_ref()) {
        return recurrence(w, scoped, refs, d, target);
    }
    let mut claim = build_claim(w, scoped, refs, d, None)?;
    let superseded = match &d.supersedes {
        None => None,
        Some(target) => {
            let t = visible_claim(w, scoped, target)?;
            if t.scope != scoped.exact {
                return Err(refuse(
                    DomainErrorCode::PermissionDenied,
                    SCOPE,
                    Some(target),
                ));
            }
            require_active(&t)?;
            claim.body.supersedes = Some(t.id.clone());
            Some(t)
        }
    };
    let stored = w.put_claim(claim, None, "Recorded")?;
    let stored = match &stored.lesson {
        Some(l) => {
            let source = l.source.clone();
            let reference = stored.body.lesson_source_ref.clone();
            add_event(w, &stored.id, &source, reference.as_deref())?;
            take_effect_if_due(w, stored)?
        }
        None => stored,
    };
    if let Some(t) = superseded {
        let mut next = t.clone();
        next.lifecycle = "Superseded".into();
        next.body.superseded_by = Some(stored.id.clone());
        w.put_claim(next, Some(t.revision), "Superseded")?;
    }
    note_conflicts(w, scoped, &stored)
}

/// A correction that says the same as a stored lesson (`same_as`): a recurrence of that lesson
/// with its evidence; no new lesson is stored.
fn recurrence(
    w: &mut Writer<'_>,
    scoped: &Scoped,
    refs: &Refs,
    d: &ClaimDraft,
    target: &Id,
) -> Result<(), Failure> {
    let t = visible_claim(w, scoped, target)?;
    if t.lesson.is_none() {
        return Err(invalid("same_as"));
    }
    if t.scope != scoped.exact {
        return Err(refuse(
            DomainErrorCode::PermissionDenied,
            SCOPE,
            Some(target),
        ));
    }
    require_active(&t)?;
    let l = lesson_for(w.ctx, d.lesson.as_ref())?;
    let source = wire(&l.source);
    let evidence = resolve_evidence(w, scoped, refs, &d.evidence_ids.0)?;
    let inserted = add_event(
        w,
        &t.id,
        &source,
        l.source_ref.as_ref().map(|r| r.0.as_str()),
    )?;
    if !inserted {
        let message = format!(
            "The source event was already recorded on lesson {}; recurrence is unchanged.",
            t.id.as_str()
        );
        w.note(Severity::Info, "duplicate_event", &message);
    }
    let mut next = t.clone();
    for e in &evidence {
        if !next.body.evidence_ids.contains(e) {
            next.body.evidence_ids.push(e.clone());
        }
    }
    next.basis = basis_of(
        w,
        &evidence,
        d.source_revision.as_ref().map(|r| r.0.as_str()),
        next.basis,
    )?;
    if model::user_source(&source)
        && let Some(lesson) = next.lesson.as_mut()
        && matches!(lesson.rule_state.as_str(), "Candidate" | "Proposed")
    {
        // The user's own words make the rule effective at once.
        lesson.rule_state = "Effective".into();
        lesson.effective_at_ms = Some(w.now);
        next.body.lesson_effect = Some("UserInstruction".into());
    }
    let stored = if inserted || next != t {
        w.put_claim(next, Some(t.revision), "Recurred")?
    } else {
        t
    };
    take_effect_if_due(w, stored)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// knowledge.revise

/// Whether two claims carry the same authored content.
fn same_content(a: &StoredClaim, b: &StoredClaim) -> bool {
    a.kind == b.kind
        && a.entry_kind == b.entry_kind
        && a.subject_id == b.subject_id
        && a.predicate == b.predicate
        && a.object_id == b.object_id
        && a.source == b.source
        && a.tags == b.tags
        && a.basis == b.basis
        && a.lesson.as_ref().map(|l| &l.applicability)
            == b.lesson.as_ref().map(|l| &l.applicability)
        && a.body.value == b.body.value
        && a.body.value_json == b.body.value_json
        && a.body.normalized_text == b.body.normalized_text
        && a.body.confidence == b.body.confidence
        && a.body.effective_at_ms == b.body.effective_at_ms
        && a.body.evidence_ids == b.body.evidence_ids
        && a.body.origin == b.body.origin
        && a.body.origin_ref == b.body.origin_ref
}

fn revise(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    i: &ops::ReviseInput,
) -> Result<DomainDecision, Failure> {
    let (scoped, _) = scope_for(tx, ctx, &i.scope, true)?;
    let mut w = Writer::new(tx, ctx, "knowledge.revise")?;
    let current = exact_claim(&mut w, &scoped, &i.claim_id, &i.expected_revision)?;
    require_active(&current)?;
    if i.replacement.supersedes.is_some() {
        return Err(invalid("supersedes"));
    }
    let next = build_claim(
        &mut w,
        &scoped,
        &Refs::default(),
        &i.replacement,
        Some(&current),
    )?;
    if next.lesson.is_some() != current.lesson.is_some() {
        return Err(invalid("entry_kind"));
    }
    if !same_content(&next, &current) {
        let stored = w.put_claim(next, Some(current.revision), "Revised")?;
        note_conflicts(&mut w, &scoped, &stored)?;
    }
    let primary = scoped.exact.clone();
    outcome(w, &primary, |value| OperationResult::KnowledgeRevise {
        value,
    })
}

// ---------------------------------------------------------------------------------------------
// knowledge.relate

enum Node {
    Claim(StoredClaim),
    Entity(StoredEntity),
}
impl Node {
    fn id(&self) -> &Id {
        match self {
            Self::Claim(c) => &c.id,
            Self::Entity(e) => &e.id,
        }
    }
}
fn node(w: &mut Writer<'_>, scoped: &Scoped, id: &Id) -> Result<Node, Failure> {
    if let Some(c) = w.k().knowledge_claim(id)?
        && c.lifecycle != "Deleted"
        && scoped.sees(&c.scope)
        && nests(&c.scope, &scoped.exact)
    {
        return Ok(Node::Claim(c));
    }
    match w.k().knowledge_entity(id)? {
        Some(e)
            if e.deleted_at_ms.is_none()
                && scoped.sees(&e.scope)
                && nests(&e.scope, &scoped.exact) =>
        {
            Ok(Node::Entity(e))
        }
        _ => Err(not_found(CLAIM, id).into()),
    }
}
fn lesson_node(n: Node, scoped: &Scoped) -> Result<StoredClaim, Failure> {
    match n {
        Node::Claim(c) if c.lesson.is_some() => {
            if c.scope != scoped.exact {
                return Err(refuse(
                    DomainErrorCode::PermissionDenied,
                    SCOPE,
                    Some(&c.id),
                ));
            }
            Ok(c)
        }
        other => Err(refuse(
            DomainErrorCode::InvalidArgument,
            LESSON,
            Some(other.id()),
        )),
    }
}

fn relate(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    i: &ops::RelateInput,
) -> Result<DomainDecision, Failure> {
    let (scoped, _) = scope_for(tx, ctx, &i.scope, true)?;
    let mut w = Writer::new(tx, ctx, "knowledge.relate")?;
    if i.subject_id == i.object_id {
        return Err(invalid("object_id"));
    }
    let subject = node(&mut w, &scoped, &i.subject_id)?;
    let object = node(&mut w, &scoped, &i.object_id)?;
    let evidence = resolve_evidence(&mut w, &scoped, &Refs::default(), &i.evidence_ids.0)?;
    let predicate = fold(&i.predicate.0);
    match predicate.as_str() {
        model::SAME_AS => {
            let from = lesson_node(subject, &scoped)?;
            let into = lesson_node(object, &scoped)?;
            require_active(&from)?;
            require_active(&into)?;
            let moved = w.k().merge_knowledge_lesson_events(&from.id, &into.id)?;
            let mut merged = from.clone();
            merged.lifecycle = "Superseded".into();
            merged.body.superseded_by = Some(into.id.clone());
            w.put_claim(merged, Some(from.revision), "Merged")?;
            let mut target = into.clone();
            for e in from.body.evidence_ids.iter().chain(&evidence) {
                if !target.body.evidence_ids.contains(e) {
                    target.body.evidence_ids.push(e.clone());
                }
            }
            let stored = w.put_claim(target, Some(into.revision), "MergedInto")?;
            store_relation(
                &mut w,
                &scoped,
                &from.id,
                model::SAME_AS,
                &into.id,
                &evidence,
            )?;
            let message = format!(
                "Lesson {} merged into {}: {moved} source events moved.",
                from.id.as_str(),
                into.id.as_str()
            );
            w.note(Severity::Info, "merged", &message);
            take_effect_if_due(&mut w, stored)?;
        }
        model::UNMERGE => {
            let from = lesson_node(subject, &scoped)?;
            let into = lesson_node(object, &scoped)?;
            if from.lifecycle != "Superseded" || from.body.superseded_by.as_ref() != Some(&into.id)
            {
                return Err(conflict(LIFECYCLE, &from.id));
            }
            // Nested merges unwind outermost first: while the target is itself merged into
            // another lesson, the events it holds are there, and unmerging it comes first.
            require_active(&into)?;
            let moved = w.k().unmerge_knowledge_lesson_events(&from.id)?;
            let mut restored = from.clone();
            restored.lifecycle = "Active".into();
            restored.body.superseded_by = None;
            let restored = w.put_claim(restored, Some(from.revision), "Unmerged")?;
            let now = w.now;
            if let Some(r) = w.k().remove_live_knowledge_relation(
                &scoped.exact,
                &from.id,
                model::SAME_AS,
                &into.id,
                now,
            )? {
                w.changes.push(WriteChange {
                    target_kind: "Relation".into(),
                    id: r,
                    before: None,
                    after: None,
                });
            }
            // A rule that took effect only by the merged events returns to candidate evidence.
            let events = w.k().knowledge_lesson_event_count(&into.id)?;
            let threshold = w.threshold(&into.scope)?;
            let mut target = into.clone();
            target
                .body
                .evidence_ids
                .retain(|e| !from.body.evidence_ids.contains(e) || evidence.contains(e));
            if into.body.lesson_effect.as_deref() == Some("Recurrence")
                && events < threshold
                && let Some(l) = target.lesson.as_mut()
                && l.rule_state == "Effective"
            {
                l.rule_state = "Candidate".into();
                l.effective_at_ms = None;
                target.body.lesson_effect = None;
            }
            if target != into {
                w.put_claim(target, Some(into.revision), "Unmerged")?;
            } else {
                w.touch(&into.scope);
            }
            let message = format!(
                "Lesson {} restored from {}: {moved} source events moved back.",
                restored.id.as_str(),
                into.id.as_str()
            );
            w.note(Severity::Info, "unmerged", &message);
        }
        model::CONTRADICTS => {
            let (Node::Claim(a), Node::Claim(b)) = (&subject, &object) else {
                return Err(invalid("contradicts"));
            };
            if store_relation(&mut w, &scoped, &a.id, model::CONTRADICTS, &b.id, &evidence)? {
                let message = format!(
                    "Claims {} and {} contradict each other; both are kept, with the conflict shown.",
                    a.id.as_str(),
                    b.id.as_str()
                );
                w.note(Severity::Warning, "conflict", &message);
            }
        }
        _ => {
            store_relation(
                &mut w,
                &scoped,
                subject.id(),
                &i.predicate.0,
                object.id(),
                &evidence,
            )?;
        }
    }
    let primary = scoped.exact.clone();
    outcome(w, &primary, |value| OperationResult::KnowledgeRelate {
        value,
    })
}

/// Stores a relation; `false` when the same live relation exists.
fn store_relation(
    w: &mut Writer<'_>,
    scoped: &Scoped,
    subject: &Id,
    predicate: &str,
    object: &Id,
    evidence: &[Id],
) -> Result<bool, Failure> {
    // The store decides duplicates within this scope, over every live relation (not a page):
    // another project's identical relation is its own record and never suppresses this one.
    let relation = StoredRelation {
        id: w.tx.new_id(IdKind::KnowledgeRelation),
        scope: scoped.exact.clone(),
        subject_id: subject.clone(),
        predicate: predicate.to_owned(),
        object_id: object.clone(),
        evidence_ids: evidence.to_vec(),
        author: w.ctx.principal().clone(),
        created_at_ms: w.now,
        removed_at_ms: None,
        write_id: w.write_id.clone(),
    };
    match w.k().put_knowledge_relation(&relation) {
        Ok(()) => {}
        Err(StorageError::Duplicate { .. }) => return Ok(false),
        Err(e) => return Err(e.into()),
    }
    w.changes.push(WriteChange {
        target_kind: "Relation".into(),
        id: relation.id.clone(),
        before: None,
        after: Some(1),
    });
    w.touch(&scoped.exact);
    Ok(true)
}

// ---------------------------------------------------------------------------------------------
// knowledge.tag, .verify, .retract

fn tag(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    i: &ops::TagInput,
) -> Result<DomainDecision, Failure> {
    let (scoped, _) = scope_for(tx, ctx, &i.scope, true)?;
    let mut w = Writer::new(tx, ctx, "knowledge.tag")?;
    let tags = fold_tags(i.tags.0.iter().map(|t| t.0.as_str()));
    for id in &i.claim_ids.0 {
        let expected = i
            .expected_revisions
            .0
            .iter()
            .find(|r| r.id == *id)
            .ok_or_else(|| invalid("expected_revisions"))?;
        let c = exact_claim(&mut w, &scoped, id, &expected.revision)?;
        if c.tags != tags {
            let mut next = c.clone();
            next.tags = tags.clone();
            w.put_claim(next, Some(c.revision), "Tagged")?;
        }
    }
    let primary = scoped.exact.clone();
    outcome(w, &primary, |value| OperationResult::KnowledgeTag { value })
}

fn verify(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    i: &ops::VerifyInput,
) -> Result<DomainDecision, Failure> {
    let (scoped, _) = scope_for(tx, ctx, &i.scope, true)?;
    let mut w = Writer::new(tx, ctx, "knowledge.verify")?;
    let c = exact_claim(&mut w, &scoped, &i.claim_id, &i.expected_revision)?;
    require_active(&c)?;
    let evidence = resolve_evidence(&mut w, &scoped, &Refs::default(), &i.evidence_ids.0)?;
    let mut next = c.clone();
    for e in &evidence {
        if !next.body.evidence_ids.contains(e) {
            next.body.evidence_ids.push(e.clone());
        }
    }
    let revision = i.source_revision.as_ref().map(|r| r.0.clone());
    // The new baseline: every cited source at the stated revision, otherwise its latest
    // observed revision, otherwise the revision its evidence names.
    let mut basis = Vec::new();
    let cited = next.body.evidence_ids.clone();
    for id in &cited {
        if let Some(e) = w.k().knowledge_evidence(id)? {
            let head = w.k().knowledge_source_head(&e.source_ref)?;
            if let Some(r) = revision.clone().or(head).or(e.source_revision.clone()) {
                basis.retain(|(s, _): &(String, String)| *s != e.source_ref);
                basis.push((e.source_ref.clone(), r));
            }
        }
    }
    for (source, old) in &c.basis {
        if !basis.iter().any(|(s, _)| s == source) {
            let head = w.k().knowledge_source_head(source)?;
            basis.push((
                source.clone(),
                revision.clone().or(head).unwrap_or_else(|| old.clone()),
            ));
        }
    }
    basis.sort();
    next.basis = basis;
    next.body.verified_at_ms = Some(w.now);
    next.body.verified_by = Some(ctx.principal().clone());
    next.body.verified_source_revision = revision;
    next.body.reason = Some(i.reason.0.clone());
    w.put_claim(next, Some(c.revision), "Verified")?;
    let primary = scoped.exact.clone();
    outcome(w, &primary, |value| OperationResult::KnowledgeVerify {
        value,
    })
}

fn retract(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    i: &ops::RetractInput,
) -> Result<DomainDecision, Failure> {
    let (scoped, _) = scope_for(tx, ctx, &i.scope, true)?;
    let mut w = Writer::new(tx, ctx, "knowledge.retract")?;
    let c = exact_claim(&mut w, &scoped, &i.claim_id, &i.expected_revision)?;
    require_active(&c)?;
    let evidence = resolve_evidence(&mut w, &scoped, &Refs::default(), &i.evidence_ids.0)?;
    let mut next = c.clone();
    for e in &evidence {
        if !next.body.evidence_ids.contains(e) {
            next.body.evidence_ids.push(e.clone());
        }
    }
    next.lifecycle = "Retracted".into();
    next.body.reason = Some(i.reason.0.clone());
    w.put_claim(next, Some(c.revision), "Retracted")?;
    let primary = scoped.exact.clone();
    outcome(w, &primary, |value| OperationResult::KnowledgeRetract {
        value,
    })
}

// ---------------------------------------------------------------------------------------------
// knowledge.scope and moves

/// A claim the principal may change wherever it lives (`NotFound` when it may not see it).
fn owned_claim(
    w: &mut Writer<'_>,
    reach: &Reach,
    id: &Id,
    expected: &Revision,
) -> Result<StoredClaim, Failure> {
    let c = match w.k().knowledge_claim(id)? {
        Some(c)
            if writable(reach, &c.scope)
                && c.scope
                    .project()
                    .is_none_or(|p| w.ctx.project_id().is_none_or(|q| q == p)) =>
        {
            c
        }
        _ => return Err(not_found(CLAIM, id).into()),
    };
    if c.lifecycle == "Deleted" {
        return Err(conflict(LIFECYCLE, id));
    }
    if c.revision != expected.get() {
        return Err(revision_conflict(CLAIM, id, expected, c.revision));
    }
    Ok(c)
}

/// Moves `claims` to `dest` with the entities and evidence only they use. A record another
/// claim still needs where it is cannot follow (`Conflict`, naming it), so nothing becomes
/// visible beyond its scope and no former project keeps access through a moved record.
fn relocate(
    w: &mut Writer<'_>,
    claims: Vec<StoredClaim>,
    dest: &KnowledgeScope,
) -> Result<Vec<StoredClaim>, Failure> {
    let moving: HashSet<Id> = claims
        .iter()
        .filter(|c| c.scope != *dest)
        .map(|c| c.id.clone())
        .collect();
    let mut entities: BTreeMap<String, StoredEntity> = BTreeMap::new();
    let mut evidence: BTreeMap<String, StoredEvidence> = BTreeMap::new();
    for c in claims.iter().filter(|c| moving.contains(&c.id)) {
        for id in std::iter::once(&c.subject_id).chain(c.object_id.iter()) {
            if let Some(e) = w.k().knowledge_entity(id)? {
                if nests(&e.scope, dest) || entities.contains_key(e.id.as_str()) {
                    continue;
                }
                let users = w.k().knowledge_entity_users(&e.id, KNOWLEDGE_PAGE_MAX)?;
                if users.len() >= KNOWLEDGE_PAGE_MAX as usize
                    || users.iter().any(|u| !moving.contains(&u.id))
                {
                    return Err(conflict(ENTITY, &e.id));
                }
                if let Some(other) = w.k().knowledge_entity_by_key(dest, &e.canonical_key)? {
                    return Err(conflict(ENTITY, &other.id));
                }
                entities.insert(e.id.as_str().to_owned(), e);
            } else if let Some(o) = w.k().knowledge_claim(id)?
                && !nests(&o.scope, dest)
                && !moving.contains(&o.id)
            {
                return Err(conflict(CLAIM, &o.id));
            }
        }
        for id in c.body.evidence_ids.clone() {
            let Some(e) = w.k().knowledge_evidence(&id)? else {
                continue;
            };
            if nests(&e.scope, dest) || evidence.contains_key(e.id.as_str()) {
                continue;
            }
            let users = w.k().knowledge_evidence_users(&e.id, KNOWLEDGE_PAGE_MAX)?;
            if users.len() >= KNOWLEDGE_PAGE_MAX as usize
                || users.iter().any(|u| !moving.contains(u))
            {
                return Err(conflict(EVIDENCE, &e.id));
            }
            evidence.insert(e.id.as_str().to_owned(), e);
        }
    }
    for (_, e) in entities {
        let mut next = e.clone();
        w.touch(&e.scope);
        next.scope = dest.clone();
        w.put_entity(next, Some(e.revision), "Moved")?;
    }
    for (_, e) in evidence {
        let mut next = e.clone();
        w.touch(&e.scope);
        next.scope = dest.clone();
        w.put_evidence(&next, true)?;
    }
    let mut out = Vec::new();
    for c in claims {
        if moving.contains(&c.id) {
            let mut next = c.clone();
            w.touch(&c.scope);
            next.scope = dest.clone();
            out.push(w.put_claim(next, Some(c.revision), "Moved")?);
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

fn move_scope(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    i: &ops::ScopeInput,
) -> Result<DomainDecision, Failure> {
    let (dest, reach) = scope_for(tx, ctx, &i.destination, true)?;
    let mut w = Writer::new(tx, ctx, "knowledge.scope")?;
    let mut claims = Vec::new();
    for r in &i.claim_ids.0 {
        if claims.iter().any(|c: &StoredClaim| c.id == r.id) {
            return Err(invalid("claim_ids"));
        }
        claims.push(owned_claim(&mut w, &reach, &r.id, &r.revision)?);
    }
    relocate(&mut w, claims, &dest.exact)?;
    let primary = dest.exact.clone();
    outcome(w, &primary, |value| OperationResult::KnowledgeScope {
        value,
    })
}

// ---------------------------------------------------------------------------------------------
// knowledge.promote, .challenge

fn promote(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    i: &ops::PromoteInput,
) -> Result<DomainDecision, Failure> {
    no_proposal(&i.proposal_id)?;
    let (dest, reach) = scope_for(tx, ctx, &i.destination, true)?;
    let mut w = Writer::new(tx, ctx, "knowledge.promote")?;
    let c = owned_claim(&mut w, &reach, &i.claim_id, &i.expected_revision)?;
    require_active(&c)?;
    let action = i
        .action
        .as_ref()
        .map(wire)
        .unwrap_or_else(|| "Effective".into());
    if action != "Effective" && c.lesson.is_none() {
        return Err(refuse(
            DomainErrorCode::InvalidArgument,
            LESSON,
            Some(&c.id),
        ));
    }
    if action != "Effective" && dest.exact != c.scope {
        return Err(invalid("destination"));
    }
    let primary = c.scope.clone();
    match action.as_str() {
        "Effective" => {
            require_user(ctx, LESSON)?;
            let mut current = c.clone();
            if let Some(l) = &c.lesson
                && l.rule_state != "Effective"
            {
                let mut next = c.clone();
                if let Some(nl) = next.lesson.as_mut() {
                    nl.rule_state = "Effective".into();
                    nl.effective_at_ms = Some(w.now);
                }
                next.body.lesson_effect = Some("Promoted".into());
                next.body.lesson_reason = i.reason.as_ref().map(|r| r.0.clone());
                current = w.put_claim(next, Some(c.revision), "Promoted")?;
            }
            if dest.exact != current.scope {
                relocate(&mut w, vec![current], &dest.exact)?;
            }
            let primary = dest.exact.clone();
            return outcome(w, &primary, |value| OperationResult::KnowledgePromote {
                value,
            });
        }
        "Propose" => {
            if c.lesson
                .as_ref()
                .is_some_and(|l| l.rule_state == "Candidate")
            {
                let mut next = c.clone();
                if let Some(l) = next.lesson.as_mut() {
                    l.rule_state = "Proposed".into();
                }
                let stored = w.put_claim(next, Some(c.revision), "Proposed")?;
                // Proposing never holds a rule back: it still takes effect on recurrence.
                take_effect_if_due(&mut w, stored)?;
            }
        }
        "Reject" => {
            require_user(ctx, LESSON)?;
            let Some(reason) = i.reason.as_ref().filter(|r| !r.0.trim().is_empty()) else {
                return Err(invalid("reason"));
            };
            if c.lesson
                .as_ref()
                .is_some_and(|l| l.rule_state != "Rejected")
            {
                let mut next = c.clone();
                if let Some(l) = next.lesson.as_mut() {
                    l.rule_state = "Rejected".into();
                    l.effective_at_ms = None;
                }
                next.body.lesson_effect = None;
                next.body.lesson_reason = Some(reason.0.clone());
                w.put_claim(next, Some(c.revision), "Rejected")?;
            }
        }
        _ => {
            let Some(address) = &i.address else {
                return Err(invalid("address"));
            };
            let existing = w.k().knowledge_addresses(&c.id)?;
            let kind = wire(&address.kind);
            if existing.len() >= MAX_LIST
                && !existing
                    .iter()
                    .any(|a| a.kind == kind && a.name == address.name.0)
            {
                return Err(refuse(
                    DomainErrorCode::InvalidArgument,
                    "list_bound",
                    Some(&c.id),
                ));
            }
            let reference = address.reference.as_ref().map(|r| r.0.clone());
            if !existing
                .iter()
                .any(|a| a.kind == kind && a.name == address.name.0 && a.reference == reference)
            {
                let stored = StoredAddress {
                    claim_id: c.id.clone(),
                    kind,
                    name: address.name.0.clone(),
                    reference,
                    actor: ctx.principal().clone(),
                    at_ms: w.now,
                };
                w.k().put_knowledge_address(&stored)?;
                w.put_claim(c.clone(), Some(c.revision), "Addressed")?;
            }
        }
    }
    outcome(w, &primary, |value| OperationResult::KnowledgePromote {
        value,
    })
}

/// Retires a rule (a lesson stays readable as `Retired`) or retracts any other claim.
fn retire(w: &mut Writer<'_>, c: &StoredClaim, reason: &str) -> Result<(), Failure> {
    let mut next = c.clone();
    match next.lesson.as_mut() {
        Some(l) => {
            if l.rule_state == "Retired" {
                return Ok(());
            }
            l.rule_state = "Retired".into();
            next.body.lesson_reason = Some(reason.to_owned());
        }
        None => {
            next.lifecycle = "Retracted".into();
            next.body.reason = Some(reason.to_owned());
        }
    }
    w.put_claim(next, Some(c.revision), "Retired")?;
    Ok(())
}

fn challenge(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    i: &ops::ChallengeInput,
) -> Result<DomainDecision, Failure> {
    let (scoped, _) = scope_for(tx, ctx, &i.scope, true)?;
    let mut w = Writer::new(tx, ctx, "knowledge.challenge")?;
    let c = exact_claim(&mut w, &scoped, &i.claim_id, &i.expected_revision)?;
    require_active(&c)?;
    let evidence = resolve_evidence(&mut w, &scoped, &Refs::default(), &i.evidence_ids.0)?;
    let outcome_name = i.outcome.as_ref().map(wire);
    match (&i.resolve_challenge_id, outcome_name.as_deref()) {
        (None, None) => {
            if w.k()
                .knowledge_challenges(&c.id, MAX_CHALLENGES as u16)?
                .len()
                >= MAX_CHALLENGES
            {
                return Err(refuse(
                    DomainErrorCode::InvalidArgument,
                    "list_bound",
                    Some(&c.id),
                ));
            }
            let ch = StoredChallenge {
                id: w.tx.new_id(IdKind::KnowledgeChallenge),
                claim_id: c.id.clone(),
                state: "Open".into(),
                reason: i.reason.0.clone(),
                evidence_ids: evidence,
                author: ctx.principal().clone(),
                opened_at_ms: w.now,
                resolved_at_ms: None,
                resolved_by: None,
                resolution: None,
            };
            w.k().put_knowledge_challenge(&ch)?;
            // The rule stays in effect while challenged; nothing waits on it.
            w.put_claim(c.clone(), Some(c.revision), "Challenged")?;
            let message = format!("Challenge {} opened on {}.", ch.id.as_str(), c.id.as_str());
            w.note(Severity::Info, "challenge_opened", &message);
        }
        (None, Some("Retire")) => {
            require_user(ctx, CHALLENGE)?;
            retire(&mut w, &c, &i.reason.0)?;
        }
        (Some(id), Some(o)) => {
            require_user(ctx, CHALLENGE)?;
            let ch = match w.k().knowledge_challenge(id)? {
                Some(ch) if ch.claim_id == c.id => ch,
                _ => return Err(not_found(CHALLENGE, id).into()),
            };
            if ch.state != "Open" {
                return Err(conflict(CHALLENGE, id));
            }
            let mut resolved = ch.clone();
            resolved.state = if o == "Retire" { "Retired" } else { "Upheld" }.into();
            resolved.resolved_at_ms = Some(w.now);
            resolved.resolved_by = Some(ctx.principal().clone());
            resolved.resolution = Some(i.reason.0.clone());
            w.k().put_knowledge_challenge(&resolved)?;
            if o == "Retire" {
                retire(&mut w, &c, &i.reason.0)?;
            } else {
                w.put_claim(c.clone(), Some(c.revision), "ChallengeUpheld")?;
            }
        }
        _ => return Err(invalid("outcome")),
    }
    let primary = scoped.exact.clone();
    outcome(w, &primary, |value| OperationResult::KnowledgeChallenge {
        value,
    })
}

// ---------------------------------------------------------------------------------------------
// Deletion

/// Strips authored content from a revision snapshot, keeping identity, attribution, times and
/// lifecycle (explicit deletion purges content, never provenance).
fn empty_applicability() -> Value {
    json!({"activities": [], "paths": [], "tools": [], "repositoryIds": []})
}
fn retained(snapshot: &Value) -> Value {
    let mut v = snapshot.clone();
    if let Some(m) = v.as_object_mut() {
        for key in [
            "value",
            "valueJson",
            "normalizedText",
            "label",
            "aliases",
            "canonicalName",
            "externalId",
        ] {
            m.remove(key);
        }
        if m.contains_key("tags") {
            m.insert("tags".into(), json!([]));
        }
        m.remove("originRef");
        if let Some(l) = m.get_mut("lesson").and_then(Value::as_object_mut) {
            l.remove("quotedWords");
            l.remove("sourceRef");
            l.remove("reason");
            // Challenge reasons and addresses are content too; their identity lives on in the
            // scrubbed side records.
            l.insert("challenges".into(), json!([]));
            l.insert("addressedBy".into(), json!([]));
            l.insert("applicability".into(), empty_applicability());
        }
    }
    v
}

fn tombstone_claim(w: &mut Writer<'_>, c: &StoredClaim) -> Result<(), Failure> {
    let mut next = c.clone();
    next.lifecycle = "Deleted".into();
    next.deleted_at_ms = Some(w.now);
    next.tags.clear();
    next.basis.clear();
    next.body.value = None;
    next.body.value_json = None;
    next.body.normalized_text = None;
    next.body.lesson_quoted_words = None;
    next.body.lesson_source_ref = None;
    next.body.lesson_reason = None;
    next.body.reason = Some(format!("Deleted by {}", w.ctx.principal().0.as_str()));
    next.body.origin_ref = None;
    if let Some(l) = next.lesson.as_mut() {
        l.applicability = empty_applicability();
    }
    w.k().purge_knowledge_lesson_records(&c.id)?;
    // Its relations go too: a deleted endpoint leaves no live link in another bundle.
    let now = w.now;
    w.k().remove_knowledge_relations_of(&c.id, now)?;
    w.k().purge_knowledge_history(&c.id, &retained)?;
    w.put_claim(next, Some(c.revision), "Deleted")?;
    Ok(())
}
fn tombstone_entity(w: &mut Writer<'_>, e: &StoredEntity) -> Result<(), Failure> {
    let mut next = e.clone();
    next.deleted_at_ms = Some(w.now);
    next.label = "(deleted)".into();
    next.aliases.clear();
    next.canonical_name = None;
    next.external_id = None;
    let now = w.now;
    w.k().remove_knowledge_relations_of(&e.id, now)?;
    w.k().purge_knowledge_history(&e.id, &retained)?;
    w.put_entity(next, Some(e.revision), "Deleted")?;
    Ok(())
}
fn purge_evidence(w: &mut Writer<'_>, e: &StoredEvidence) -> Result<(), Failure> {
    let mut next = e.clone();
    next.deleted_at_ms = Some(w.now);
    next.snippet = String::new();
    next.metadata = None;
    next.locator = None;
    w.put_evidence(&next, true)
}

/// What deleting a source of a scope removes, and its exact preview digest.
pub(super) struct DeletionPlan {
    pub claims: Vec<StoredClaim>,
    pub entities: Vec<StoredEntity>,
    pub evidence: Vec<StoredEvidence>,
    pub digest: String,
}

/// Evidence cited only by `deleted` claims (independent surviving evidence stays).
fn orphaned_evidence(
    k: &dyn KnowledgeRead,
    claims: &[StoredClaim],
) -> Result<Vec<StoredEvidence>, StorageError> {
    let deleted: HashSet<Id> = claims.iter().map(|c| c.id.clone()).collect();
    let mut out: BTreeMap<String, StoredEvidence> = BTreeMap::new();
    // Every cited evidence record, from the claims' complete citation lists (never a bounded
    // listing, which would leave content of later evidence behind).
    for claim in claims {
        for id in &claim.body.evidence_ids {
            if out.contains_key(id.as_str()) {
                continue;
            }
            let Some(e) = k.knowledge_evidence(id)? else {
                continue;
            };
            if e.deleted_at_ms.is_some() {
                continue;
            }
            let users = k.knowledge_evidence_users(&e.id, KNOWLEDGE_PAGE_MAX)?;
            if users.len() < KNOWLEDGE_PAGE_MAX as usize
                && users.iter().all(|u| deleted.contains(u))
            {
                out.insert(e.id.as_str().to_owned(), e);
            }
        }
    }
    Ok(out.into_values().collect())
}

/// Plans a delete by source: every non-deleted claim of exactly `scope` with that source, the
/// entities of that source no surviving claim uses, and the evidence only deleted claims cite.
pub(super) fn deletion_plan(
    k: &dyn KnowledgeRead,
    scope: &KnowledgeScope,
    source: &str,
    scope_revision: i64,
) -> Result<DeletionPlan, StorageError> {
    let mut claims: Vec<StoredClaim> = Vec::new();
    loop {
        let after = claims.last().map(|c| c.id.as_str().to_owned());
        let page = k.knowledge_source_claims(scope, source, after.as_deref(), 500)?;
        let full = page.len() == 500;
        claims.extend(page);
        if !full || claims.len() > MAX_SOURCE_DELETE {
            break;
        }
    }
    let deleted: HashSet<Id> = claims.iter().map(|c| c.id.clone()).collect();
    let mut entities: Vec<StoredEntity> = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = k.knowledge_source_entities(scope, source, after.as_deref(), 500)?;
        let full = page.len() == 500;
        after = page.last().map(|e| e.id.as_str().to_owned());
        for e in page {
            // An entity another source's claims still use is kept.
            let users = k.knowledge_entity_users(&e.id, KNOWLEDGE_PAGE_MAX)?;
            if users.len() < KNOWLEDGE_PAGE_MAX as usize
                && users.iter().all(|u| deleted.contains(&u.id))
            {
                entities.push(e);
            }
        }
        if !full {
            break;
        }
    }
    let evidence = orphaned_evidence(k, &claims)?;
    let mut parts = vec![
        "knowledge.delete_by_source".to_owned(),
        scope.key(),
        source.to_owned(),
        scope_revision.to_string(),
    ];
    parts.extend(
        claims
            .iter()
            .map(|c| format!("c:{}@{}", c.id.as_str(), c.revision)),
    );
    parts.extend(
        entities
            .iter()
            .map(|e| format!("e:{}@{}", e.id.as_str(), e.revision)),
    );
    parts.extend(evidence.iter().map(|e| format!("v:{}", e.id.as_str())));
    Ok(DeletionPlan {
        digest: model::preview_digest(&parts),
        claims,
        entities,
        evidence,
    })
}

fn delete_by_source(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    i: &ops::DeleteBySourceInput,
) -> Result<DomainDecision, Failure> {
    require_user(ctx, "deletion")?;
    no_proposal(&i.proposal_id)?;
    let (scoped, _) = scope_for(tx, ctx, &i.scope, true)?;
    let mut w = Writer::new(tx, ctx, "knowledge.delete_by_source")?;
    let current = w.k().knowledge_scope_revision(&scoped.exact)?.max(1);
    if current != i.expected_scope_revision.get() {
        return Err(Failure::Storage(StorageError::conflict(
            RevisionDimension::Scope,
            SCOPE,
            Id::new(scoped.exact.key().replace(':', "-")).unwrap_or_else(|_| i.source_id.clone()),
            i.expected_scope_revision.clone(),
            rev(current),
        )));
    }
    let plan = deletion_plan(
        w.tx.knowledge(),
        &scoped.exact,
        i.source_id.as_str(),
        current,
    )?;
    if plan.claims.len() > MAX_SOURCE_DELETE {
        return Err(refuse(
            DomainErrorCode::InvalidArgument,
            "delete_bound",
            Some(&i.source_id),
        ));
    }
    if plan.digest != i.preview_digest.as_str() {
        return Err(conflict(PREVIEW, &i.source_id));
    }
    for c in &plan.claims {
        tombstone_claim(&mut w, c)?;
    }
    for e in &plan.entities {
        tombstone_entity(&mut w, e)?;
    }
    for e in &plan.evidence {
        purge_evidence(&mut w, e)?;
    }
    let message = format!(
        "Deleted source {}: {} claims, {} entities and {} evidence records tombstoned.",
        i.source_id.as_str(),
        plan.claims.len(),
        plan.entities.len(),
        plan.evidence.len()
    );
    w.note(Severity::Info, "deleted", &message);
    let changed = w.changed();
    let primary = scoped.exact.clone();
    let receipt = w_finish_summary(w, &primary, changed)?;
    Ok(receipt)
}

fn w_finish_summary(
    w: Writer<'_>,
    primary: &KnowledgeScope,
    changed: bool,
) -> Result<DomainDecision, Failure> {
    let id = w.write_id.clone();
    let receipt = w.finish(primary)?;
    let result = OperationResult::KnowledgeDeleteBySource {
        value: MutationSummary {
            id,
            revision: receipt.revision,
            changed,
        },
    };
    Ok(if changed {
        DomainDecision::Applied { result }
    } else {
        DomainDecision::NoChange { result }
    })
}

fn delete_selected(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    i: &ops::DeleteSelectedInput,
) -> Result<DomainDecision, Failure> {
    require_user(ctx, "deletion")?;
    no_proposal(&i.proposal_id)?;
    let (scoped, _) = scope_for(tx, ctx, &i.scope, true)?;
    let mut w = Writer::new(tx, ctx, "knowledge.delete_selected")?;
    let mut selected = Vec::new();
    for r in &i.claims.0 {
        let c = visible_claim(&mut w, &scoped, &r.id)?;
        if c.scope != scoped.exact {
            return Err(refuse(
                DomainErrorCode::PermissionDenied,
                SCOPE,
                Some(&r.id),
            ));
        }
        if c.revision != r.revision.get() {
            return Err(revision_conflict(CLAIM, &r.id, &r.revision, c.revision));
        }
        if c.lifecycle != "Deleted" && !selected.iter().any(|s: &StoredClaim| s.id == c.id) {
            selected.push(c);
        }
    }
    let evidence = orphaned_evidence(w.tx.knowledge(), &selected)?;
    for c in &selected {
        tombstone_claim(&mut w, c)?;
    }
    for e in &evidence {
        purge_evidence(&mut w, e)?;
    }
    let changed = w.changed();
    let id = w.write_id.clone();
    let primary = scoped.exact.clone();
    let receipt = w.finish(&primary)?;
    let result = OperationResult::KnowledgeDeleteSelected {
        value: MutationSummary {
            id,
            revision: receipt.revision,
            changed,
        },
    };
    Ok(if changed {
        DomainDecision::Applied { result }
    } else {
        DomainDecision::NoChange { result }
    })
}

// ---------------------------------------------------------------------------------------------
// knowledge.import_user_data

/// The import format's schema must be exactly the built-in one.
pub(super) fn check_import_schema(schema: &SchemaRef) -> Result<(), Failure> {
    if schema.schema_id.as_str() != import::IMPORT_SCHEMA_ID
        || schema.revision.get() != import::IMPORT_SCHEMA_REVISION
        || schema.sha256.as_str() != import::import_schema_digest()
    {
        return Err(refuse(DomainErrorCode::SchemaMismatch, "schema", None));
    }
    Ok(())
}

/// The live artifact version an import names, visible to the invocation, with exactly the
/// digest and length the reference states.
pub(super) fn import_version(
    artifacts: &dyn ArtifactRead,
    viewer: &Viewer<'_>,
    source: &ArtifactRef,
) -> Result<StoredVersion, Failure> {
    let artifact = match artifacts.artifact(&source.artifact_id)? {
        Some(a) if visible(artifacts, viewer, &a)? => a,
        _ => return Err(not_found("artifact", &source.artifact_id).into()),
    };
    match artifacts.version(&source.version_id)? {
        Some(v)
            if v.artifact_id == artifact.id
                && v.live()
                && v.blob_digest == source.sha256.as_str()
                && v.byte_count == source.byte_length.get() =>
        {
            Ok(v)
        }
        Some(v) if v.artifact_id == artifact.id => Err(refuse(
            DomainErrorCode::SourceChanged,
            "artifact",
            Some(&source.version_id),
        )),
        _ => Err(not_found("artifact_version", &source.version_id).into()),
    }
}

/// Every candidate's outcome against current state, and the exact preview digest.
pub(super) fn import_outcomes(
    k: &dyn KnowledgeRead,
    scope: &KnowledgeScope,
    source: &ArtifactRef,
    candidates: &[Candidate],
) -> Result<(Vec<Outcome>, String), StorageError> {
    let mut duplicates: BTreeSet<(String, String)> = BTreeSet::new();
    for c in candidates {
        let folded = fold(&c.summary);
        let query = ClaimQuery {
            scopes: vec![scope.key()],
            lifecycles: vec!["Active".into()],
            entry_kinds: vec![c.entry_kind.clone()],
            words: model::words(&c.summary)
                .into_iter()
                .take(model::MAX_WORDS)
                .collect(),
            ..ClaimQuery::default()
        };
        let mut after: Option<(i64, String)> = None;
        loop {
            let rows =
                k.knowledge_claims(&query, after.as_ref().map(|(t, i)| (*t, i.as_str())), 500)?;
            if rows
                .iter()
                .any(|r| r.body.value.as_deref().map(fold).as_deref() == Some(folded.as_str()))
            {
                duplicates.insert((c.entry_kind.clone(), folded.clone()));
                break;
            }
            match rows.last() {
                Some(last) if rows.len() == 500 => {
                    after = Some((last.updated_at_ms, last.id.as_str().to_owned()))
                }
                _ => break,
            }
        }
    }
    let mut imported = BTreeSet::new();
    for c in candidates {
        if k.knowledge_imported(&source.version_id, c.index as i64)?
            .is_some()
        {
            imported.insert(c.index);
        }
    }
    let outcomes = import::evaluate(
        candidates,
        |kind, summary| duplicates.contains(&(kind.to_owned(), summary.to_owned())),
        |i| imported.contains(&i),
    );
    let digest = import::digest(
        &scope.key(),
        source.version_id.as_str(),
        source.sha256.as_str(),
        candidates,
        &outcomes,
    );
    Ok((outcomes, digest))
}

fn import_user_data(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    command: &AdmittedCommand,
    i: &ops::ImportUserDataInput,
) -> Result<DomainDecision, Failure> {
    // The user's own data: an agent cannot import on the user's behalf.
    require_user(ctx, "import")?;
    check_import_schema(&i.schema)?;
    let (scoped, _) = scope_for(tx, ctx, &i.scope, true)?;
    let candidates = match command.prepared::<Result<Vec<Candidate>, Refusal>>() {
        Some(Ok(c)) => c.clone(),
        Some(Err(r)) => return Err(r.clone().into()),
        None => {
            return Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "import",
                None,
            ));
        }
    };
    // The source is current authority, re-checked in this unit of work.
    let viewer = {
        let authority: &dyn AuthorityRead = tx.authority();
        Viewer::resolve(authority, ctx)?
    };
    let version = import_version(tx.artifacts(), &viewer, &i.source)?;
    let mut w = Writer::new(tx, ctx, "knowledge.import_user_data")?;
    let (outcomes, digest) =
        import_outcomes(w.tx.knowledge(), &scoped.exact, &i.source, &candidates)?;
    if digest != i.preview_digest.as_str() {
        return Err(conflict(PREVIEW, &i.source.version_id));
    }
    let source_label = format!("import-{}", version.id.as_str());
    for (c, o) in candidates.iter().zip(&outcomes) {
        if *o != Outcome::WouldStore {
            continue;
        }
        let key = model::entity_key("topic", &c.subject);
        let subject = match w.k().knowledge_entity_by_key(&scoped.exact, &key)? {
            Some(e) => e.id,
            None => {
                let e = StoredEntity {
                    id: w.tx.new_id(IdKind::KnowledgeEntity),
                    scope: scoped.exact.clone(),
                    revision: 1,
                    label: c.subject.clone(),
                    entity_type: "topic".into(),
                    aliases: Vec::new(),
                    canonical_key: key,
                    canonical_name: None,
                    external_id: None,
                    source: Some(source_label.clone()),
                    author: ctx.principal().clone(),
                    created_at_ms: w.now,
                    updated_at_ms: w.now,
                    deleted_at_ms: None,
                };
                w.put_entity(e, None, "Imported")?.id
            }
        };
        let evidence = StoredEvidence {
            id: w.tx.new_id(IdKind::KnowledgeEvidence),
            scope: scoped.exact.clone(),
            kind: "Document".into(),
            source_ref: format!(
                "artifact:{}/{}#{}",
                i.source.artifact_id.as_str(),
                version.id.as_str(),
                c.index
            ),
            artifact: Some(
                serde_json::to_value(&i.source)
                    .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
            ),
            locator: Some(c.source_file.chars().take(4096).collect()),
            snippet: match &c.verification_note {
                Some(n) => format!("{} ({}: {n})", c.summary, c.verification),
                None => format!("{} ({})", c.summary, c.verification),
            },
            metadata: None,
            source_revision: Some(version.blob_digest.clone()),
            author: ctx.principal().clone(),
            recorded_at_ms: w.now,
            deleted_at_ms: None,
        };
        w.put_evidence(&evidence, false)?;
        let lesson = (c.entry_kind == "Lesson").then(|| StoredLesson {
            // Imported lessons were written by a harness's agents: candidate evidence until
            // they recur (the user may promote one at any time).
            rule_state: "Candidate".into(),
            source: "Agent".into(),
            applicability: json!({
                "activities": c.activities, "paths": [], "tools": [], "repositoryIds": []
            }),
            effective_at_ms: None,
        });
        let claim = StoredClaim {
            id: w.tx.new_id(IdKind::KnowledgeClaim),
            scope: scoped.exact.clone(),
            revision: 1,
            kind: model::kind_of(&c.entry_kind).to_owned(),
            entry_kind: c.entry_kind.clone(),
            lifecycle: "Active".into(),
            subject_id: subject,
            predicate: c.entry_kind.to_lowercase(),
            object_id: None,
            source: Some(source_label.clone()),
            author: ctx.principal().clone(),
            session_id: ctx.session_id().cloned(),
            tags: fold_tags(
                c.tags
                    .iter()
                    .map(String::as_str)
                    .chain([model::category_tag(&c.entry_kind), "imported"]),
            ),
            basis: vec![(evidence.source_ref.clone(), version.blob_digest.clone())],
            lesson,
            body: ClaimBody {
                value: Some(c.summary.clone()),
                normalized_text: c.body.clone(),
                confidence: c.confidence,
                origin: "User".into(),
                autonomous: false,
                author_kind: format!("{:?}", ctx.principal_kind()),
                evidence_ids: vec![evidence.id.clone()],
                ..ClaimBody::default()
            },
            created_at_ms: w.now,
            updated_at_ms: w.now,
            deleted_at_ms: None,
        };
        let stored = w.put_claim(claim, None, "Imported")?;
        if stored.lesson.is_some() {
            let event_ref = format!("{}#{}", version.id.as_str(), c.index);
            add_event(&mut w, &stored.id, "Agent", Some(&event_ref))?;
        }
        let write_id = w.write_id.clone();
        w.k()
            .map_knowledge_import(&version.id, c.index as i64, &stored.id, &write_id)?;
    }
    let skipped = outcomes
        .iter()
        .filter(|o| **o != Outcome::WouldStore)
        .count();
    if skipped > 0 {
        let message = format!("{skipped} candidates were not stored (see the preview outcomes).");
        w.note(Severity::Info, "skipped", &message);
    }
    let primary = scoped.exact.clone();
    outcome(w, &primary, |value| {
        OperationResult::KnowledgeImportUserData { value }
    })
}
