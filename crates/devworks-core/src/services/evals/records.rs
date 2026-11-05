//! Stable, read-only observation of native records. These are deterministic proxy facts;
//! non-transcript records cannot invent the historical per-step revision manifest.
use crate::ports::storage::{
    EventFilter, EventRecord, HistoryRead, SessionRead, StorageError, StoredLink, StoredSession,
    WorkRead,
};
use crate::protocol::records::{
    EvalOutcome, EvalRecordSource, EvalRevisionContext, EvalStepSource,
};
use crate::protocol::support::{BoundedList, BoundedText};
use crate::protocol::{Counter, Id};
use serde_json::{Value, json};

pub enum RecordRead<'a> {
    Sessions(&'a dyn SessionRead),
    Work(&'a dyn WorkRead),
    History(&'a dyn HistoryRead),
}
pub struct ObservedRecord {
    pub body: Value,
    pub source: EvalStepSource,
    pub revision: EvalRevisionContext,
}
fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
fn observed(
    session: &StoredSession,
    kind: &str,
    id: Id,
    sequence: i64,
    body: Value,
) -> Result<ObservedRecord, StorageError> {
    Ok(ObservedRecord {
        body,
        source: EvalStepSource {
            session_id: session.id.clone(),
            sequence: Counter::new(sequence).map_err(invalid)?,
            entry_id: id,
            kind: BoundedText(kind.into()),
        },
        revision: EvalRevisionContext::Mixed {
            context_epoch: Counter::new(session.context_epoch).map_err(invalid)?,
            sources: BoundedList(vec![]),
            reason: BoundedText(
                "Native record has no recorded per-step effective revision manifest".into(),
            ),
        },
    })
}

/// Gateway decisions and authority denials identify their operation at payload.operation.
/// ReceiptIncompatible and ReceiptReplyPublished are recorded outside root/project feeds, so
/// the root history selection already excludes those receipt-publication bookkeeping events.
/// Never search arbitrary user text: mentioning an evaluation remains ordinary evidence.
fn evaluation_event(event: &EventRecord) -> bool {
    event
        .payload
        .get("operation")
        .and_then(Value::as_str)
        .is_some_and(|operation| operation.starts_with("evals."))
}

fn event_body(e: &EventRecord) -> Value {
    json!({"id":e.id,"sequence":e.sequence,"actor":e.actor,"projectId":e.project_id,
        "rootId":e.root_id,"operationId":e.operation_id,"aggregateKind":e.aggregate_kind,
        "aggregateId":e.aggregate_id,"aggregateRevision":e.aggregate_revision,"kind":e.kind,
        "createdAtMs":e.created_at_ms,"payload":e.payload})
}
fn link_body(l: &StoredLink) -> Value {
    json!({"id":l.id,"projectId":l.project_id,"rootId":l.root_id,"kind":l.kind,
        "targetKind":l.target_kind,"targetId":l.target_id,"parentId":l.parent_id,
        "supersedesId":l.supersedes_id,"actor":l.actor,"eventId":l.event_id,
        "sequence":l.sequence,"repositoryId":l.repository_id,"revision":l.revision,
        "summary":l.summary,"body":l.body,"createdAtMs":l.created_at_ms})
}

/// Visit every selected record in deterministic source order using bounded storage pages.
/// `watermark` bounds committed history/links. Session/work snapshots are captured state, not
/// reconstructed past state. The caller must recheck the same source bodies before persisting.
/// Body and durable identity are stable across unrelated/evaluation-only watermark advances.
pub fn visit_records(
    read: RecordRead<'_>,
    session: &StoredSession,
    source: &EvalRecordSource,
    watermark: i64,
    visitor: &mut dyn FnMut(ObservedRecord) -> Result<(), StorageError>,
) -> Result<(), StorageError> {
    match (source, read) {
        (EvalRecordSource::Session, RecordRead::Sessions(read)) => {
            let body = serde_json::to_value(crate::services::sessions::session_record(
                session,
                read.last_entry_at_ms(&session.id)?,
            ))
            .map_err(|_| invalid("session evaluation record"))?;
            visitor(observed(
                session,
                "Session",
                session.id.clone(),
                session.last_durable_sequence(),
                body,
            )?)
        }
        (EvalRecordSource::WorkState, RecordRead::Work(read)) => {
            let Some(root) = &session.root_id else {
                return Ok(());
            };
            let Some(work) = read.work(&root.0)? else {
                return Ok(());
            };
            if session.project_id.as_ref() != Some(&work.project_id) {
                return Err(StorageError::ScopeDenied);
            }
            let body = json!({"id":work.id,"projectId":work.project_id,"kind":work.kind,
                "parentFeatureId":work.parent_feature_id,"statusId":work.status_id,"title":work.title,
                "priority":work.priority,"manualOrder":work.manual_order,"revision":work.revision,
                "scopeRevision":work.scope_revision,"createdAtMs":work.created_at_ms,"updatedAtMs":work.updated_at_ms,
                "createdBy":work.created_by,"updatedBy":work.updated_by,"searchKey":work.search_key,
                "callerRef":work.caller_ref,"completion":work.completion,"brief":work.brief});
            visitor(observed(session, "WorkState", work.id, watermark, body)?)
        }
        (EvalRecordSource::HistoryEvent, RecordRead::History(read)) => {
            let Some(root) = &session.root_id else {
                return Ok(());
            };
            let mut after = 0;
            loop {
                let page = read.page(&EventFilter::Root(root.clone()), after, 500)?;
                for event in &page.events {
                    if event.sequence > watermark {
                        return Ok(());
                    }
                    if event.sequence <= after {
                        return Err(invalid("history evaluation cursor did not advance"));
                    }
                    after = event.sequence;
                    if event.root_id.as_ref() != Some(root)
                        || event.project_id != session.project_id
                    {
                        return Err(StorageError::ScopeDenied);
                    }
                    if !evaluation_event(event) {
                        visitor(observed(
                            session,
                            "HistoryEvent",
                            event.id.0.clone(),
                            event.sequence,
                            event_body(event),
                        )?)?;
                    }
                }
                if !page.has_more {
                    break;
                }
                if page.next_after < after || (page.events.is_empty() && page.next_after <= after) {
                    return Err(invalid("history evaluation cursor did not advance"));
                }
                after = page.next_after;
            }
            Ok(())
        }
        (
            EvalRecordSource::GitLink
            | EvalRecordSource::PullRequestLink
            | EvalRecordSource::ReviewLink,
            RecordRead::Sessions(read),
        ) => {
            let Some(root) = &session.root_id else {
                return Ok(());
            };
            let Some(work) = read.work_root(root)? else {
                return Ok(());
            };
            if session.project_id.as_ref() != Some(&work.project_id) {
                return Err(StorageError::ScopeDenied);
            }
            let (kind, label) = match source {
                EvalRecordSource::GitLink => ("git", "GitLink"),
                EvalRecordSource::PullRequestLink => ("pull_request", "PullRequestLink"),
                EvalRecordSource::ReviewLink => ("review", "ReviewLink"),
                _ => unreachable!(),
            };
            let mut after = None;
            loop {
                let page = read.links(root, &[kind], after.as_ref(), 500)?;
                let more = page.len() == 500;
                for link in &page {
                    if link.sequence > watermark {
                        return Ok(());
                    }
                    if link.root_id != *root
                        || Some(&link.project_id) != session.project_id.as_ref()
                    {
                        return Err(StorageError::ScopeDenied);
                    }
                    visitor(observed(
                        session,
                        label,
                        link.id.clone(),
                        link.sequence,
                        link_body(link),
                    )?)?;
                }
                let next = page.last().map(|l| crate::ports::storage::SequenceKey {
                    sequence: l.sequence,
                    source: 1,
                    id: l.id.as_str().into(),
                });
                if !more {
                    break;
                }
                if next.is_none() || next <= after {
                    return Err(invalid("link evaluation cursor did not advance"));
                }
                after = next;
            }
            Ok(())
        }
        _ => Err(invalid(
            "evaluation record source requires its matching read interface",
        )),
    }
}

/// Strict RFC 6901 string pointers. A missing value is missing evidence, never a failed rule.
/// Equality is JSON value equality; this reports a deterministic proxy, not semantic judgment.
pub fn field_outcome(body: &Value, pointer: &str, expected: &Value) -> (EvalOutcome, &'static str) {
    if !pointer.is_empty() && !pointer.starts_with('/') {
        return (EvalOutcome::GraderError, "Invalid JSON pointer");
    }
    let mut chars = pointer.chars();
    while let Some(c) = chars.next() {
        if c == '~' && !matches!(chars.next(), Some('0' | '1')) {
            return (EvalOutcome::GraderError, "Invalid JSON pointer escape");
        }
    }
    match body.pointer(pointer) {
        None => (
            EvalOutcome::EvidenceUnavailable,
            "Recorded field unavailable",
        ),
        Some(actual) if actual == expected => (
            EvalOutcome::Passed,
            "Recorded field equals expected value (proxy)",
        ),
        Some(_) => (
            EvalOutcome::Failed,
            "Recorded field differs from expected value (proxy)",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_pointer_escapes_missing_values_and_types_remain_distinct() {
        let body = json!({"a/b":{"~key":[false,null,1,"1"]}});
        for (pointer, expected, outcome) in [
            ("/a~1b/~0key/0", json!(false), EvalOutcome::Passed),
            ("/a~1b/~0key/2", json!("1"), EvalOutcome::Failed),
            ("/missing", Value::Null, EvalOutcome::EvidenceUnavailable),
            ("/a~2b", Value::Null, EvalOutcome::GraderError),
            ("bad", Value::Null, EvalOutcome::GraderError),
        ] {
            assert_eq!(
                serde_json::to_value(field_outcome(&body, pointer, &expected).0).unwrap(),
                serde_json::to_value(outcome).unwrap()
            );
        }
        assert!(matches!(
            field_outcome(&body, "/a~1b/~0key/1", &Value::Null).0,
            EvalOutcome::Passed
        ));
    }
}
