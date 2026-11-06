//! Informational retained Knowledge review. Adopted KNOW-03/04 omit autonomous ACK and rule approval.
use crate::context::InvocationContext;
use crate::operations::approvals::ReviewQueueInput;
use crate::ports::storage::*;
use crate::protocol::data::DataScope;
use crate::protocol::results::OperationResult;
use crate::services::{cursor::CursorBinding, knowledge, sessions::Failure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
pub(crate) fn query(
    s: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    i: &ReviewQueueInput,
    revision: u32,
) -> Result<OperationResult, Failure> {
    if i.page.limit.get() > 50 {
        return Err(super::service::invalid("review_queue_limit"));
    }
    let project_id = i
        .project_id
        .clone()
        .or_else(|| ctx.project_id().cloned())
        .ok_or_else(|| super::service::invalid("project_required"))?;
    let policy_digest =
        crate::authority::policy::resolve_policy(s.configuration(), Some(&project_id))?.digest();
    let requested = DataScope::Project { project_id };
    let scoped = knowledge::resolve_scope(s.artifacts(), s.authority(), ctx, &requested, false)?;
    let scopes = vec![scoped.exact.clone()];
    let thresholds = knowledge::thresholds(s.configuration(), &scopes)?;
    let scope_keys = scopes.iter().map(KnowledgeScope::key).collect::<Vec<_>>();
    let challenged = s.knowledge().knowledge_review_candidates(
        &scope_keys,
        KnowledgeReviewKind::Challenge,
        500,
    )?;
    let proposed = s.knowledge().knowledge_review_candidates(
        &scope_keys,
        KnowledgeReviewKind::ProposedRule,
        500,
    )?;
    let challenged_ids = challenged
        .iter()
        .map(|c| c.id.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    let proposed_ids = proposed
        .iter()
        .map(|c| c.id.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    let relations = s
        .knowledge()
        .knowledge_review_contradictions(&scope_keys, 500)?;
    let mut claims = BTreeMap::new();
    for c in challenged.into_iter().chain(proposed) {
        claims.insert(c.id.as_str().to_owned(), c);
    }
    for r in &relations {
        for id in [&r.subject_id, &r.object_id] {
            if !claims.contains_key(id.as_str())
                && let Some(c) = s.knowledge().knowledge_claim(id)?
            {
                claims.insert(c.id.as_str().to_owned(), c);
            }
        }
    }
    let mut items: Vec<(u8, i64, i64, String, Value)> = Vec::new();
    let mut nc = 0;
    let mut np = 0;
    for c in claims.values() {
        let events = s.knowledge().knowledge_lesson_event_count(&c.id)?.max(1);
        let (rank, priority) = priority(c);
        let wire = knowledge::claim_wire(s.knowledge(), c, &thresholds)?;
        let open = s.knowledge().knowledge_open_challenges(&c.id)?;
        if open > 0 && challenged_ids.contains(c.id.as_str()) && nc < 500 {
            let challenges = s.knowledge().knowledge_open_challenge_records(&c.id, 3)?;
            let since = challenges
                .first()
                .map_or(c.updated_at_ms, |ch| ch.opened_at_ms);
            let rows:Vec<Value>=challenges.iter().take(3).map(|ch|json!({"id":ch.id,"state":ch.state,"reason":ch.reason,"evidenceIds":ch.evidence_ids,"authorId":ch.author,"openedAtMs":ch.opened_at_ms.to_string(),"resolvedAtMs":ch.resolved_at_ms.map(|v|v.to_string()),"resolution":ch.resolution})).collect();
            let mut v = json!({"kind":"Challenge","entry":wire,"priority":priority,"recurrence":events.max(0).to_string(),"sinceMs":since.max(0).to_string(),"challenges":rows,"moreChallenges":(open-rows.len() as i64).max(0).to_string(),"author":c.author,"informational":true});
            if open <= rows.len() as i64 {
                v.as_object_mut()
                    .expect("review item")
                    .remove("moreChallenges");
            }
            items.push((rank, -events, since, c.id.as_str().to_owned(), v));
            nc += 1;
        }
        if let Some(l) = &c.lesson {
            let threshold = thresholds.get(&c.scope.key()).copied().unwrap_or(i64::MAX);
            let (state, _) = knowledge::model::effective_rule_state(
                &l.rule_state,
                c.body.lesson_effect.as_deref(),
                events,
                threshold,
            );
            if state == "Proposed" && proposed_ids.contains(c.id.as_str()) && np < 500 {
                items.push((rank,-events,c.updated_at_ms,c.id.as_str().to_owned(),json!({"kind":"ProposedRule","entry":wire,"priority":priority,"recurrence":events.max(0).to_string(),"sinceMs":c.updated_at_ms.max(0).to_string(),"challenges":[],"author":c.author,"informational":true})));
                np += 1;
            }
        }
    }
    let mut seen = BTreeSet::new();
    let mut nr = 0;
    for r in relations {
        if r.predicate != "contradicts" || r.removed_at_ms.is_some() {
            continue;
        }
        let (Some(a), Some(b)) = (
            claims.get(r.subject_id.as_str()),
            claims.get(r.object_id.as_str()),
        ) else {
            continue;
        };
        if a.scope != b.scope {
            continue;
        }
        let key = if a.id.as_str() < b.id.as_str() {
            (a.id.as_str().to_owned(), b.id.as_str().to_owned())
        } else {
            (b.id.as_str().to_owned(), a.id.as_str().to_owned())
        };
        if !seen.insert(key) || nr >= 500 {
            continue;
        }
        let a_count = s.knowledge().knowledge_lesson_event_count(&a.id)?.max(1);
        let b_count = s.knowledge().knowledge_lesson_event_count(&b.id)?.max(1);
        let (a, b) = if priority(a).0 <= priority(b).0 {
            (a, b)
        } else {
            (b, a)
        };
        let (rank, p) = priority(a);
        let recurrence = a_count.max(b_count);
        items.push((rank,-recurrence,r.created_at_ms,format!("{}:{}",a.id.as_str(),b.id.as_str()),json!({"kind":"Contradiction","entry":knowledge::claim_wire(s.knowledge(),a,&thresholds)?,"other":knowledge::claim_wire(s.knowledge(),b,&thresholds)?,"priority":p,"recurrence":recurrence.max(0).to_string(),"sinceMs":r.created_at_ms.max(0).to_string(),"challenges":[],"author":a.author,"otherAuthor":b.author,"informational":true})));
        nr += 1;
    }
    items.sort_by(|a, b| (&a.0, &a.1, &a.2, &a.3).cmp(&(&b.0, &b.1, &b.2, &b.3)));
    let binding = CursorBinding::new(
        ctx,
        "approvals.review_queue",
        &[
            &serde_json::to_string(&requested).unwrap(),
            &revision.to_string(),
            ctx.policy_revision().as_str(),
            &policy_digest,
            &serde_json::to_string(&thresholds).expect("knowledge thresholds"),
        ],
        s.knowledge().knowledge_scope_revision(&scoped.exact)?,
    );
    let offset = i
        .page
        .cursor
        .as_ref()
        .map(|c| binding.decode(&c.0))
        .transpose()?
        .map(|p| p.parse::<usize>().map_err(|_| StorageError::CursorExpired))
        .transpose()?
        .unwrap_or(0);
    if offset > items.len() {
        return Err(StorageError::CursorExpired.into());
    }
    let limit = i.page.limit.get() as usize;
    let more = offset + limit < items.len();
    let rows = items
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|v| v.4)
        .collect::<Vec<_>>();
    let page=serde_json::from_value(json!({"items":rows,"nextCursor":if more{Some(binding.encode(&(offset+limit).to_string()))}else{None},"hasMore":more})).map_err(|e|StorageError::InvalidRecord(format!("review queue: {e}")))?;
    Ok(OperationResult::ApprovalsReviewQueue { value: page })
}
fn priority(c: &StoredClaim) -> (u8, &'static str) {
    match c.lesson.as_ref().map(|l| l.source.as_str()) {
        Some("Prompt" | "RelayedPrompt") => (0, "highest"),
        Some("Review" | "RelayedReview") => (1, "high"),
        _ => (2, "normal"),
    }
}
