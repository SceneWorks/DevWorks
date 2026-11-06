//! Immutable source facts. Old observations have no proven attempt identity and are never
//! silently promoted to attempts or added to canonical incurred spending.
use super::money::{CostAccumulator, Rounding};
use crate::ports::storage::*;
use crate::protocol::records::UsageItem;
use crate::protocol::{Id, RootId};
use serde_json::json;

fn invalid(message: impl Into<String>) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
fn raw_amount(amount: Option<&str>) -> Option<i64> {
    let mut exact = CostAccumulator::default();
    exact.amount_usd(amount?).ok()?;
    exact.micro_usd(Rounding::Nearest).ok()
}
fn class(kind: &str, origin: &str) -> String {
    match (kind, origin) {
        ("Reported", _) => "ReportedCharge",
        ("Estimated", "SubscriptionApiEquivalent") => "SubscriptionApiEquivalent",
        ("Estimated", "UserConfigured") => "ConfiguredEstimate",
        ("Estimated", _) => "OfficialEstimate",
        ("Free", _) => "LocalFree",
        _ => "Untracked",
    }
    .into()
}
fn recorded_feature(
    tx: &mut dyn WriteTransaction,
    root: Option<&RootId>,
) -> Result<Option<Id>, StorageError> {
    // Work kind is durable identity. A task's current parent is not historical attribution.
    let Some(root) = root else {
        return Ok(None);
    };
    Ok(tx
        .work()
        .work(&root.0)?
        .filter(|w| w.is_feature())
        .map(|w| w.id))
}
pub fn session_usage(
    tx: &mut dyn WriteTransaction,
    source: &StoredUsage,
) -> Result<(), StorageError> {
    let item: UsageItem =
        serde_json::from_value(source.item.clone()).map_err(|e| invalid(e.to_string()))?;
    let batch = tx
        .sessions()
        .batch(&source.session_id, &source.batch_id)?
        .ok_or_else(|| invalid("usage source batch missing"))?;
    let feature_id = recorded_feature(tx, source.root_id.as_ref())?;
    let source_id = serde_json::to_string(&(
        source.session_id.as_str(),
        source.batch_id.as_str(),
        source.ordinal,
    ))
    .map_err(|e| invalid(e.to_string()))?;
    let price_kind = serde_json::to_value(&item.price_kind).map_err(|e| invalid(e.to_string()))?;
    let price_origin =
        serde_json::to_value(&item.price_origin).map_err(|e| invalid(e.to_string()))?;
    let priced = raw_amount(item.amount_usd.as_ref().map(|v| v.0.as_str()));
    tx.usage().insert_usage_origin(&StoredUsageOrigin {
        source_kind: "SessionUsage".into(),
        source_id,
        attempt_id: None,
        request_id: Some(item.request_id),
        project_id: source.project_id.clone(),
        feature_id,
        work_id: source.root_id.as_ref().map(|r| r.0.clone()),
        session_id: Some(source.session_id.clone()),
        job_run_id: None,
        payload: json!({"version":1,"item":source.item}),
        recorded_at_ms: batch.accepted_at_ms,
        priced_micro_usd: priced,
        classification: class(
            price_kind.as_str().unwrap_or("Untracked"),
            price_origin.as_str().unwrap_or("Unknown"),
        ),
        monetary_complete: false,
        incomplete: item.incomplete,
        unknown_tail: item.incomplete,
    })
}
pub fn tool_invocation(
    tx: &mut dyn WriteTransaction,
    source: &StoredInvocation,
) -> Result<(), StorageError> {
    let feature_id = recorded_feature(tx, source.root_id.as_ref())?;
    let source_id = serde_json::to_string(&(source.session_id.as_str(), source.response_sequence))
        .map_err(|e| invalid(e.to_string()))?;
    tx.usage().insert_usage_origin(&StoredUsageOrigin {
        source_kind:"ToolInvocation".into(),source_id,attempt_id:None,request_id:None,
        project_id:source.project_id.clone(),feature_id,work_id:source.root_id.as_ref().map(|r|r.0.clone()),
        session_id:Some(source.session_id.clone()),job_run_id:None,
        payload:json!({"version":1,"capabilityId":source.capability_id,"outcome":source.outcome,"errorCode":source.error_code,"actorId":source.actor,"calledAtMs":source.called_at_ms,"respondedAtMs":source.responded_at_ms}),
        recorded_at_ms:source.responded_at_ms,priced_micro_usd:None,classification:"Untracked".into(),monetary_complete:false,incomplete:false,unknown_tail:false,
    })
}
pub fn job_usage(
    tx: &mut dyn WriteTransaction,
    source: &StoredJobUsage,
) -> Result<(), StorageError> {
    let run = tx
        .jobs()
        .job_run(&source.run_id)?
        .ok_or_else(|| invalid("usage source run missing"))?;
    let source_id = serde_json::to_string(&(source.run_id.as_str(), source.seq))
        .map_err(|e| invalid(e.to_string()))?;
    tx.usage().insert_usage_origin(&StoredUsageOrigin {
        source_kind:"JobUsage".into(),source_id,attempt_id:None,request_id:Some(source.request_id.clone()),
        project_id:run.project_id,feature_id:None,work_id:None,session_id:None,job_run_id:Some(source.run_id.clone()),
        payload:json!({"version":1,"purpose":source.purpose,"modelId":source.model_id,"inputTokens":source.input_tokens,"outputTokens":source.output_tokens,"cacheReadTokens":source.cache_read_tokens,"cacheWriteTokens":source.cache_write_tokens,"amountUsd":source.amount_usd,"priceKind":source.price_kind,"priceOrigin":source.price_origin,"priceRevision":source.price_revision,"incomplete":source.incomplete}),
        recorded_at_ms:source.recorded_at_ms,priced_micro_usd:raw_amount(source.amount_usd.as_deref()),classification:class(&source.price_kind,&source.price_origin),monetary_complete:false,incomplete:source.incomplete,unknown_tail:source.incomplete,
    })
}
pub fn router_request(
    tx: &mut dyn WriteTransaction,
    source: &StoredRouteRequest,
) -> Result<(), StorageError> {
    let operation = tx
        .operations()
        .operation(&source.operation_id)?
        .ok_or_else(|| invalid("router source operation missing"))?;
    tx.usage().insert_usage_origin(&StoredUsageOrigin {
        source_kind:"RouterRequest".into(),source_id:source.request_id.as_str().into(),attempt_id:None,request_id:Some(source.request_id.clone()),
        project_id:source.project_id.clone(),feature_id:None,work_id:operation.root_id.as_ref().map(|r|r.0.clone()),session_id:source.session_id.clone(),job_run_id:None,
        payload:json!({"version":1,"purpose":source.purpose,"modelId":source.model_id,"connectionId":source.connection_id,"operationId":source.operation_id,"actorId":operation.payload.get("actorId"),"reportCategory":operation.payload.get("reportCategory")}),
        recorded_at_ms:source.created_at_ms,priced_micro_usd:None,classification:"Untracked".into(),monetary_complete:false,incomplete:false,unknown_tail:false,
    })
}

#[derive(Clone, Debug)]
pub struct BootstrapPage {
    pub visited: usize,
    pub complete: bool,
}
/// One bounded, restartable unit. New source writes call these same ingestion functions in
/// their source transaction, so arbitrary new IDs behind the cursor cannot be missed.
pub fn bootstrap_page(
    tx: &mut dyn WriteTransaction,
    limit: u16,
) -> Result<BootstrapPage, StorageError> {
    if limit == 0 || limit > MAX_PAGE {
        return Err(invalid("invalid usage bootstrap page size"));
    }
    let mut state = tx.usage().usage_bootstrap()?;
    if state.phase == "Complete" {
        return Ok(BootstrapPage {
            visited: 0,
            complete: true,
        });
    }
    let before = state.revision;
    let visited = match state.phase.as_str() {
        "Sessions" => {
            let rows = tx
                .sessions()
                .all_usage_facts(state.session_after.as_ref(), limit)?;
            let count = rows.len();
            for fact in rows {
                state.session_after = Some(fact.key().clone());
                match fact {
                    UsageFact::Inference { usage, .. } => session_usage(tx, &usage)?,
                    UsageFact::Tool { invocation, .. } => tool_invocation(tx, &invocation)?,
                }
            }
            if count < usize::from(limit) {
                state.phase = "Jobs".into();
            }
            count
        }
        "Jobs" => {
            let rows = tx.jobs().usage_history(state.job_after.as_ref(), limit)?;
            let count = rows.len();
            for row in rows {
                state.job_after = Some(UsageJobPosition {
                    run_id: row.run_id.clone(),
                    seq: row.seq,
                });
                job_usage(tx, &row)?;
            }
            if count < usize::from(limit) {
                state.phase = "RouterRequests".into();
            }
            count
        }
        "RouterRequests" => {
            let rows = tx
                .router()
                .route_requests(state.router_after.as_ref(), limit)?;
            let count = rows.len();
            for row in rows {
                state.router_after = Some(row.request_id.clone());
                router_request(tx, &row)?;
            }
            if count < usize::from(limit) {
                state.phase = "Complete".into();
                state.completed_at_ms = Some(tx.now_ms());
            }
            count
        }
        _ => return Err(invalid("unknown usage bootstrap phase")),
    };
    state.revision += 1;
    state.processed = state
        .processed
        .checked_add(visited as i64)
        .ok_or_else(|| invalid("bootstrap count overflow"))?;
    let event_id = crate::protocol::EventId(tx.new_id(crate::ports::IdKind::Event));
    let host_actor = tx.store().host_actor.clone();
    let event = tx.history().append(NewEvent {
        id: event_id,
        actor: host_actor,
        project_id: None,
        root_id: None,
        operation_id: None,
        aggregate_kind: "usage_bootstrap".into(),
        aggregate_id: "host".into(),
        aggregate_revision: state.revision,
        kind: "UsageHistoryIndexed".into(),
        payload: json!({"phase":state.phase,"processed":state.processed}),
    })?;
    state.source_sequence = event;
    tx.usage().update_usage_bootstrap(&state, before)?;
    Ok(BootstrapPage {
        visited,
        complete: state.phase == "Complete",
    })
}
