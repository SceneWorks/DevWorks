//! Freeze a compact view of the native Decisions and Knowledge review queues for one tool call.
use super::{NativeDreaming, invalid};
use devworks_core::context::InvocationContext;
use devworks_core::ports::storage::{
    DreamActionRow, DreamRunRow, ReadOptions, StorageError, WriteOptions, run_write,
};
use devworks_core::protocol::data::DataScope;
use devworks_core::protocol::results::{DomainQueryReply, OperationResult};
use devworks_core::protocol::{Id, ReadStamp};
use devworks_core::services::dreaming::SweepLease;
use devworks_core::services::dreaming::windows::{
    WindowPosition, sign_cursor, verify_owned_cursor,
};
use serde_json::{Value, json};

const TOOL: &str = "dream_review_queue";

impl NativeDreaming {
    pub(super) fn record_review_page(
        &self,
        lease: &SweepLease,
        ctx: &InvocationContext,
        run: &DreamRunRow,
        action: &Id,
        args: &Value,
    ) -> Result<DreamActionRow, StorageError> {
        let cursor = args["cursor"].as_str().map(str::to_owned);
        let snapshot = if cursor.is_none() {
            Some(self.capture_review(ctx, run, action)?)
        } else {
            None
        };
        let limit = args["limit"].as_u64().unwrap_or(20) as usize;
        let (lease, action) = (lease.clone(), action.clone());
        run_write(self.store.as_ref(), WriteOptions::observed(), move |tx| {
            lease.context(tx)?;
            let run = lease.read(tx, true)?;
            let repo = tx.dreaming()?;
            if let Some(prior) = repo.dream_action(&run.id, &action)? {
                return Ok(prior);
            }
            let key = run
                .checkpoint
                .as_ref()
                .and_then(|checkpoint| checkpoint.cursor_key.as_ref())
                .ok_or(StorageError::CursorExpired)?;
            let mut envelope = json!({"tool":TOOL});
            let result = if let Some(cursor) = cursor {
                let continuing =
                    verify_owned_cursor(repo, &run, TOOL, &cursor).and_then(|position| {
                        let owner =
                            Id::new(&position.run).map_err(|_| StorageError::CursorExpired)?;
                        let window =
                            Id::new(&position.window).map_err(|_| StorageError::CursorExpired)?;
                        let entry = repo
                            .dream_action(&owner, &window)?
                            .ok_or(StorageError::CursorExpired)?;
                        if entry.action["tool"] != TOOL {
                            return Err(StorageError::CursorExpired);
                        }
                        let rows = entry.action["reviewWindow"]
                            .as_array()
                            .ok_or(StorageError::CursorExpired)?;
                        render(key, &run, position, rows, limit)
                    });
                match continuing {
                    Ok(result) => result,
                    Err(StorageError::CursorExpired) => json!({"error":"CursorExpired"}),
                    Err(error) => return Err(error),
                }
            } else if let Some(Some(rows)) = snapshot {
                let position = WindowPosition {
                    run: run.id.as_str().into(),
                    scope: run.scope.key(),
                    tool: TOOL.into(),
                    selection: "review-queue".into(),
                    window: action.as_str().into(),
                    offset: 0,
                };
                let result = render(key, &run, position, &rows, limit)?;
                envelope["reviewWindow"] = Value::Array(rows);
                result
            } else {
                json!({"error":"ReviewQueueUnavailable"})
            };
            envelope["result"] = result;
            repo.put_dream_action(&run.id, run.generation, &action, &envelope)
        })
    }

    fn capture_review(
        &self,
        ctx: &InvocationContext,
        run: &DreamRunRow,
        query: &Id,
    ) -> Result<Option<Vec<Value>>, StorageError> {
        let Some(gateway) = self.gateway.upgrade() else {
            return Ok(None);
        };
        let mut selected = Vec::new();
        let scope = DataScope::from(&run.scope);
        for operation in ["approvals.review_queue", "approvals.list"] {
            if operation == "approvals.review_queue" && run.scope.project().is_none() {
                continue;
            }
            let mut cursor: Option<String> = None;
            let mut stamp: Option<ReadStamp> = None;
            loop {
                let mut page = json!({"limit":50});
                if let Some(cursor) = &cursor {
                    page["cursor"] = json!(cursor);
                }
                let body = if operation == "approvals.review_queue" {
                    json!({"kind":operation,"projectId":run.scope.project(),"page":page})
                } else {
                    json!({"kind":operation,"scope":scope,"page":page,"state":"Pending"})
                };
                let request = json!({"mode":"Query","protocol":"1.0-draft.1","schemaRevision":3,"queryId":query,"body":body});
                let outcome = gateway.query_with(
                    ctx,
                    operation,
                    request.to_string().as_bytes(),
                    &ReadOptions::default(),
                );
                let Ok(DomainQueryReply::Result {
                    result,
                    stamp: current,
                    ..
                }) = outcome
                else {
                    return Ok(None);
                };
                if stamp.as_ref().is_some_and(|previous| {
                    previous.scope_revision != current.scope_revision
                        || previous.policy_revision != current.policy_revision
                }) {
                    return Ok(None);
                }
                stamp = Some(current);
                let next = match result {
                    OperationResult::ApprovalsReviewQueue { value } => {
                        for record in value
                            .items
                            .0
                            .into_iter()
                            .filter(|record| record.since_ms.get() <= run.sweep_started_at_ms())
                        {
                            let challenges = record.challenges.0.into_iter().map(|challenge| json!({
                                "id":challenge.id, "revision":record.entry.revision, "state":challenge.state,
                                "reason":preview(&challenge.reason.0), "evidence_ids":challenge.evidence_ids,
                            })).collect::<Vec<_>>();
                            selected.push(json!({"kind":record.kind,"entry_id":record.entry.id,"entry_revision":record.entry.revision,
                                "priority":record.priority,"recurrence":record.recurrence,"since_ms":record.since_ms,
                                "challenges":challenges,"more_challenges":record.more_challenges,
                                "other":record.other.map(|other| json!({"id":other.id,"revision":other.revision})) }));
                        }
                        value.next_cursor.map(|cursor| cursor.0)
                    }
                    OperationResult::ApprovalsList { value } => {
                        for record in value.items.0.into_iter().filter(|record| {
                            record.operation_kind.0 == "dreaming.propose"
                                && record
                                    .created_at_ms
                                    .as_ref()
                                    .is_some_and(|at| at.get() <= run.sweep_started_at_ms())
                        }) {
                            selected.push(json!({"kind":"DreamingProposal","id":record.id,"revision":record.revision,
                                "entry_id":record.target_id,"entry_revision":record.target_revision,
                                "summary":preview(&record.summary.0),"artifact":record.artifact,"fingerprint":record.content_digest }));
                        }
                        value.next_cursor.map(|cursor| cursor.0)
                    }
                    _ => return Err(invalid("unexpected native review queue reply")),
                };
                if next.is_none() {
                    break;
                }
                if next == cursor {
                    return Err(StorageError::CursorExpired);
                }
                cursor = next;
            }
        }
        Ok(Some(selected))
    }
}

fn preview(text: &str) -> String {
    text.chars().take(640).collect()
}

fn render(
    key: &[u8; 32],
    run: &DreamRunRow,
    mut position: WindowPosition,
    rows: &[Value],
    limit: usize,
) -> Result<Value, StorageError> {
    // A cursor emitted just before a resume still names the immutable parent action. Its
    // owner remains part of the signed reference throughout the continuation.
    let scope = run.scope.key();
    if scope != position.scope || position.offset > rows.len() {
        return Err(StorageError::CursorExpired);
    }
    let end = (position.offset + limit).min(rows.len());
    let items = &rows[position.offset..end];
    let next = if end < rows.len() {
        position.offset = end;
        Some(sign_cursor(key, &position)?)
    } else {
        None
    };
    Ok(json!({"reviews":items,"has_more":next.is_some(),"next_cursor":next,"untrusted_data":true}))
}
