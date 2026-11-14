//! Feature-only monetary controls. Configuration, incurred cost and the manual Continue
//! latch are independent: neither a larger cap nor a negative correction clears a pause.
use crate::ports::storage::*;
use crate::protocol::records::{FeatureBudgetInput, FeatureBudgetView};
use crate::protocol::{ActorId, EventId, Id, RootId};
use crate::services::runs::{ControlService, HoldSubject, active_blockers, raise_blocker};
use crate::services::sessions::{counter, rev};
use serde_json::json;

const ORIGIN: &str = "feature_budget";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetBlock {
    AccountingUnavailable,
    Reached,
    ReservationWait,
}

pub fn check(
    read: &dyn UsageRead,
    feature: &Id,
    requested_micro_usd: Option<i64>,
) -> Result<Option<BudgetBlock>, StorageError> {
    let Some(limit) = read
        .feature_budget(feature)?
        .and_then(|b| b.limit_micro_usd)
    else {
        return Ok(None);
    };
    if read.usage_bootstrap()?.phase != "Complete" {
        return Ok(Some(BudgetBlock::AccountingUnavailable));
    }
    let totals = read.feature_usage_totals(feature)?;
    if totals.spent_micro_usd >= limit {
        return Ok(Some(BudgetBlock::Reached));
    }
    // Unknown custom charges are allowed with explicitly partial coverage. They do not
    // manufacture a zero reservation or waive a reached cap or any other hold.
    if let Some(requested) = requested_micro_usd {
        if requested < 0 {
            return Err(StorageError::InvalidRecord(
                "negative monetary reservation".into(),
            ));
        }
        let committed = i128::from(totals.spent_micro_usd)
            + i128::from(totals.reserved_micro_usd)
            + i128::from(requested);
        if committed > i128::from(limit) {
            return Ok(Some(BudgetBlock::ReservationWait));
        }
    }
    Ok(None)
}

fn view_parts(
    budget: Option<StoredFeatureBudget>,
    totals: StoredFeatureUsageTotals,
    ready: bool,
    continue_required: bool,
) -> FeatureBudgetView {
    let limit = budget.as_ref().and_then(|b| b.limit_micro_usd);
    FeatureBudgetView {
        revision: rev(budget.map_or(1, |b| b.revision)),
        limit_micro_usd: limit.map(counter),
        spent_micro_usd: counter(totals.spent_micro_usd),
        reserved_micro_usd: counter(totals.reserved_micro_usd),
        warning: limit
            .is_some_and(|limit| i128::from(totals.spent_micro_usd) * 4 >= i128::from(limit) * 3),
        continue_required,
        untracked_attempts: counter(totals.untracked_attempts),
        unknown_tail_attempts: counter(totals.unknown_tail_attempts),
        accounting_ready: ready,
        monetary_incomplete_attempts: counter(totals.monetary_incomplete_attempts),
        legacy_usage_facts: counter(totals.legacy_usage_facts),
    }
}

pub fn view(read: &dyn ReadSnapshot, feature: &Id) -> Result<FeatureBudgetView, StorageError> {
    let holds = active_blockers(
        read.runs(),
        &BlockerFilter::RootLevel(RootId(feature.clone())),
    )?;
    Ok(view_parts(
        read.usage().feature_budget(feature)?,
        read.usage().feature_usage_totals(feature)?,
        read.usage().usage_bootstrap()?.phase == "Complete",
        holds.iter().any(|h| h.cause == HoldCause::BudgetContinue),
    ))
}
pub fn tx_view(
    tx: &mut dyn WriteTransaction,
    feature: &Id,
) -> Result<FeatureBudgetView, StorageError> {
    let budget = tx.usage().feature_budget(feature)?;
    let totals = tx.usage().feature_usage_totals(feature)?;
    let ready = tx.usage().usage_bootstrap()?.phase == "Complete";
    let holds = active_blockers(
        tx.runs(),
        &BlockerFilter::RootLevel(RootId(feature.clone())),
    )?;
    Ok(view_parts(
        budget,
        totals,
        ready,
        holds.iter().any(|h| h.cause == HoldCause::BudgetContinue),
    ))
}

/// The caller must bind this to a registered Human's direct edit or exact reviewed approval.
/// Host bookkeeping may initialize an absent Off row; it cannot choose a new spending cap.
pub fn configure(
    tx: &mut dyn WriteTransaction,
    feature: &StoredWork,
    human: &ActorId,
    input: &FeatureBudgetInput,
) -> Result<bool, StorageError> {
    if !feature.is_feature()
        || tx.actors().actor_kind(human)? != Some(crate::context::PrincipalKind::Human)
    {
        return Err(StorageError::ScopeDenied);
    }
    let limit = match input {
        FeatureBudgetInput::Off {} => None,
        FeatureBudgetInput::Limit { micro_usd } => Some(micro_usd.get()),
    };
    let before = tx.usage().feature_budget(&feature.id)?;
    if before.as_ref().and_then(|b| b.limit_micro_usd) == limit {
        return Ok(false);
    }
    let after = StoredFeatureBudget {
        feature_id: feature.id.clone(),
        revision: before.as_ref().map_or(1, |b| b.revision + 1),
        limit_micro_usd: limit,
        warning_revision: None,
    };
    tx.usage()
        .put_feature_budget(&after, before.as_ref().map(|b| b.revision))?;
    record(
        tx,
        human,
        feature,
        &after,
        "FeatureBudgetConfigured",
        json!({"limitMicroUsd":limit,"previousLimitMicroUsd":before.and_then(|b|b.limit_micro_usd),"manualContinueLatchPreserved":true}),
    )?;
    reconcile_latch(tx, feature)?;
    Ok(true)
}

fn record(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    feature: &StoredWork,
    budget: &StoredFeatureBudget,
    kind: &str,
    payload: serde_json::Value,
) -> Result<(), StorageError> {
    let id = tx.new_id(crate::ports::IdKind::Event);
    tx.history().append(NewEvent {
        id: EventId(id.clone()),
        actor: actor.clone(),
        project_id: Some(feature.project_id.clone()),
        root_id: Some(RootId(feature.id.clone())),
        operation_id: None,
        aggregate_kind: "feature_budget".into(),
        aggregate_id: feature.id.as_str().into(),
        aggregate_revision: budget.revision,
        kind: kind.into(),
        payload: payload.clone(),
    })?;
    let created_at_ms = tx.now_ms();
    tx.usage().append_usage_change(&StoredUsageChange {
        event_id: id,
        attempt_id: None,
        feature_id: Some(feature.id.clone()),
        kind: kind.into(),
        delta_micro_usd: None,
        payload,
        created_at_ms,
    })
}

/// Reconcile after a canonical charge, correction or configured-cap change, in the same write.
pub fn reconcile(
    tx: &mut dyn WriteTransaction,
    control: &ControlService,
    feature: &StoredWork,
) -> Result<(), StorageError> {
    if reconcile_latch(tx, feature)? {
        let root = tx
            .sessions()
            .work_root(&RootId(feature.id.clone()))?
            .ok_or_else(|| StorageError::NotFound {
                kind: "feature",
                id: feature.id.as_str().into(),
            })?;
        let actor = tx.store().host_actor.clone();
        control.request_stop(tx, &actor, &root, StopCause::Pause)?;
        for linked in crate::services::runs::linked_roots(tx, &root)? {
            control.request_stop(tx, &actor, &linked, StopCause::Pause)?;
        }
    }
    Ok(())
}
fn reconcile_latch(
    tx: &mut dyn WriteTransaction,
    feature: &StoredWork,
) -> Result<bool, StorageError> {
    let Some(mut budget) = tx.usage().feature_budget(&feature.id)? else {
        return Ok(false);
    };
    let Some(limit) = budget.limit_micro_usd else {
        return Ok(false);
    };
    let totals = tx.usage().feature_usage_totals(&feature.id)?;
    let actor = tx.store().host_actor.clone();
    if budget.warning_revision.is_none()
        && i128::from(totals.spent_micro_usd) * 4 >= i128::from(limit) * 3
    {
        let expected = budget.revision;
        budget.revision += 1;
        budget.warning_revision = Some(budget.revision);
        tx.usage().put_feature_budget(&budget, Some(expected))?;
        record(
            tx,
            &actor,
            feature,
            &budget,
            "FeatureBudgetWarning",
            json!({"spentMicroUsd":totals.spent_micro_usd,"limitMicroUsd":limit,"reservedMicroUsd":totals.reserved_micro_usd,"monetaryIncompleteAttempts":totals.monetary_incomplete_attempts,"legacyUsageFacts":totals.legacy_usage_facts,"thresholdPercent":75}),
        )?;
    }
    if totals.spent_micro_usd >= limit {
        let root = tx
            .sessions()
            .work_root(&RootId(feature.id.clone()))?
            .ok_or_else(|| StorageError::NotFound {
                kind: "feature",
                id: feature.id.as_str().into(),
            })?;
        crate::services::runs::ensure_control(tx, &root.id)?;
        let existing = active_blockers(tx.runs(), &BlockerFilter::RootLevel(root.id.clone()))?;
        if !existing
            .iter()
            .any(|h| h.cause == HoldCause::BudgetContinue && h.origin_id == ORIGIN)
        {
            raise_blocker(
                tx,
                &actor,
                &root,
                HoldSubject::default(),
                HoldCause::BudgetContinue,
                ORIGIN,
                budget.revision,
                "tracked feature spending reached its configured limit; adjust the cap if needed and select Continue",
                json!({"manualContinue":true,"budgetRevision":budget.revision,"spentMicroUsd":totals.spent_micro_usd,"limitMicroUsd":limit}),
            )?;
            record(
                tx,
                &actor,
                feature,
                &budget,
                "FeatureBudgetReached",
                json!({"spentMicroUsd":totals.spent_micro_usd,"limitMicroUsd":limit}),
            )?;
            crate::services::notifications::budget_paused(tx, feature, budget.revision)?;
        }
    }
    Ok(totals.spent_micro_usd >= limit)
}
