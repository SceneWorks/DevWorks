//! Native host attempt admission and observation. The journal supplies the attempt identity;
//! codecs cannot choose budget ownership, prices, a free connection or dispatch authority.
use super::{
    budget,
    meter::{Meter, ObservationSemantics, Quantities},
    money::{CostAccumulator, Rounding},
    pricing::{self, PricingContext},
};
use crate::ports::storage::*;
use crate::ports::usage::{CostClassification, PriceQuery, PriceQuote};
use crate::protocol::{ActorId, AssignmentId, EventId, Id, OperationState, RootId};
use crate::services::runs::substrate::{DispatchRequest, Fenced};
use crate::services::runs::{ControlService, Target};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DispatchIdentity {
    pub root_id: RootId,
    pub execution_id: Id,
    pub owner_epoch: i64,
    pub assignment_id: Option<AssignmentId>,
    pub assignment_generation: i64,
    pub turn_id: Id,
    pub implementation: bool,
}
#[derive(Clone, Debug)]
pub struct AttemptRequest {
    /// Durable identity of this admission, reused if its acknowledgment is lost.
    pub admission_key: Id,
    pub request_id: Id,
    pub actor_id: ActorId,
    pub work_id: Option<Id>,
    pub job_run_id: Option<Id>,
    pub role: String,
    pub query: PriceQuery,
    pub quote: PriceQuote,
    pub price_generation: i64,
    pub override_revision: i64,
    pub pricing: PricingContext,
    pub selection: crate::services::router::accounting::AttemptSelection,
    pub input_tokens: i64,
    pub output_headroom: i64,
    pub unit_headroom: Vec<super::meter::MeasuredQuantity>,
    pub expected_context_epoch: Option<i64>,
    pub dispatch: Option<DispatchIdentity>,
    /// Bounded observable cache changes/digests only; never prompt or tool-argument copies.
    pub cache_diagnostics: Vec<crate::protocol::records::UsageCacheDiagnostic>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdmittedSnapshot {
    pub admission_key: Id,
    pub intent_digest: String,
    pub quote: PriceQuote,
    pub query: PriceQuery,
    pub price_generation: i64,
    pub override_revision: i64,
    pub pricing: PricingContext,
    pub selection: crate::services::router::accounting::AttemptSelection,
    pub work_revision: Option<i64>,
    pub parent_feature_id: Option<Id>,
    pub quoted_provider_model_id: Option<String>,
    pub reservation_micro_usd: Option<i64>,
    pub admitted_boot_id: Id,
    pub principal_revision: Option<i64>,
    pub policy_digest: Option<String>,
    pub authority_project_id: Option<crate::protocol::ProjectId>,
    pub context_epoch: Option<i64>,
    pub dispatch: Option<DispatchIdentity>,
    pub cache_diagnostics: Vec<crate::protocol::records::UsageCacheDiagnostic>,
}
#[derive(Clone, Debug)]
pub enum Admission {
    Admitted {
        attempt: StoredInferenceAttempt,
        reused: bool,
    },
    Blocked(budget::BudgetBlock),
    Fenced(Fenced),
    SourceChanged(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedCost {
    pub amount: String,
    pub currency: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Observation {
    pub key: String,
    pub sequence: i64,
    pub semantics: ObservationSemantics,
    pub quantities: Quantities,
    pub reported_cost: Option<ReportedCost>,
    pub provider_model_id: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BalanceSummary {
    pub meter: Meter,
    pub reported_cost: Option<ReportedCost>,
    pub unknown_components: Vec<String>,
    pub finished_at_ms: Option<i64>,
    pub cancellation_requested: bool,
    pub outcome: Option<String>,
}

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
fn encoded<T: Serialize>(value: &T) -> Result<Value, StorageError> {
    serde_json::to_value(value).map_err(|_| invalid("invalid accounting value"))
}
pub fn snapshot(attempt: &StoredInferenceAttempt) -> Result<AdmittedSnapshot, StorageError> {
    serde_json::from_value(attempt.pricing_snapshot.clone())
        .map_err(|_| invalid("invalid admitted price snapshot"))
}
pub fn summary(balance: &StoredAttemptBalance) -> Result<BalanceSummary, StorageError> {
    serde_json::from_value(balance.summary.clone()).map_err(|_| invalid("invalid attempt balance"))
}
fn classification(value: CostClassification) -> String {
    match value {
        CostClassification::ReportedCharge => "ReportedCharge",
        CostClassification::ConfiguredEstimate => "ConfiguredEstimate",
        CostClassification::OfficialEstimate => "OfficialEstimate",
        CostClassification::SubscriptionApiEquivalent => "SubscriptionApiEquivalent",
        CostClassification::LocalFree => "LocalFree",
        CostClassification::Untracked => "Untracked",
    }
    .into()
}

/// Validates the immutable quotation's publication and override fences inside the write owner.
fn quote_current(
    tx: &mut dyn WriteTransaction,
    request: &AttemptRequest,
) -> Result<Option<String>, StorageError> {
    let Some(connection) = tx.router().connection(&request.query.connection_id)? else {
        return Ok(Some("serving connection is unavailable".into()));
    };
    if request.quote.connection_revision != Some(connection.revision)
        || request.query.endpoint_profile != connection.profile
    {
        return Ok(Some(
            "serving connection or billing policy changed; refresh the quote".into(),
        ));
    }
    if request.quote.classification == CostClassification::LocalFree && !connection.local_free {
        return Err(StorageError::ScopeDenied);
    }

    if tx
        .usage()
        .price_override_revision(&request.query.catalog_model_id)?
        != request.override_revision
    {
        return Ok(Some(
            "price override changed; refresh the quote before admission".into(),
        ));
    }
    if let Some(id) = &request.quote.snapshot_id {
        let Some(source) = tx.usage().price_snapshot(id)? else {
            return Ok(Some("admitted price source is unavailable".into()));
        };
        if request.quote.price_revision != Some(source.revision)
            || tx
                .usage()
                .current_price_snapshot(&source.provider)?
                .as_ref()
                .map(|s| &s.id)
                != Some(id)
        {
            return Ok(Some(
                "price publication changed; refresh the quote before admission".into(),
            ));
        }
    }
    Ok(None)
}

fn intent(request: &AttemptRequest, route: &StoredRouteRequest) -> Result<String, StorageError> {
    let value = json!({"requestId":request.request_id,"actorId":request.actor_id,"workId":request.work_id,"jobRunId":request.job_run_id,"role":request.role,
        "modelId":route.model_id,"connectionId":route.connection_id,"sessionId":route.session_id,"purpose":route.purpose,
        "selection":request.selection,"inputTokens":request.input_tokens,"outputHeadroom":request.output_headroom,"unitHeadroom":request.unit_headroom,"dispatch":request.dispatch});
    crate::jcs::canonicalize(&value)
        .map(|value| crate::fingerprint::sha256_hex(value.as_bytes()))
        .map_err(|_| invalid("invalid accounting intent"))
}

pub(crate) fn admit_attempt(
    tx: &mut dyn WriteTransaction,
    control: &ControlService,
    router: &crate::services::router::InferenceRouter,
    ctx: &crate::context::InvocationContext,
    request: &AttemptRequest,
) -> Result<Admission, StorageError> {
    if ctx.principal() != &request.actor_id
        || tx.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
        || &tx.store().store_id != ctx.store_id()
        || tx.store().restore_epoch != ctx.restore_epoch().get()
    {
        return Err(StorageError::ScopeDenied);
    }
    let (principal_revision, policy_digest) = if matches!(
        ctx.principal_kind(),
        crate::context::PrincipalKind::Human | crate::context::PrincipalKind::Host
    ) {
        (None, None)
    } else {
        let principal = tx
            .authority()
            .principal(ctx.principal())?
            .ok_or(StorageError::ScopeDenied)?;
        if !principal.active
            || principal.kind != ctx.principal_kind()
            || principal
                .project_id
                .as_ref()
                .is_some_and(|p| ctx.project_id() != Some(p))
        {
            return Err(StorageError::ScopeDenied);
        }
        if let Some(role) = principal.role
            && format!("{role:?}") != request.role
        {
            return Err(StorageError::ScopeDenied);
        }
        let policy =
            crate::authority::policy::resolve_policy(tx.configuration(), ctx.project_id())?;
        if policy.profile.is_none() {
            return Err(StorageError::ScopeDenied);
        }
        (Some(principal.revision), Some(policy.digest()))
    };

    let route = tx
        .router()
        .route_request(&request.request_id)?
        .ok_or_else(|| invalid("logical router request is not admitted"))?;
    if !crate::services::sessions::project_visible(ctx, route.project_id.as_ref()) {
        return Err(StorageError::ScopeDenied);
    }
    if request.role.trim().is_empty()
        || request.role.len() > 128
        || request.cache_diagnostics.len() > 128
    {
        return Err(invalid("invalid accounting role or diagnostics"));
    }
    if tx.actors().actor_kind(&request.actor_id)?.is_none() {
        return Err(StorageError::ScopeDenied);
    }
    let operation = tx
        .operations()
        .operation(&route.operation_id)?
        .ok_or_else(|| invalid("missing router journal operation"))?;
    let digest = intent(request, &route)?;
    for prior in &operation.attempts {
        if let Some(attempt) = tx.usage().inference_attempt(&prior.id.0)? {
            let before = snapshot(&attempt)?;
            if before.admission_key == request.admission_key {
                if before.intent_digest != digest {
                    return Err(invalid(
                        "attempt admission key reused with different intent",
                    ));
                }
                return Ok(Admission::Admitted {
                    attempt,
                    reused: true,
                });
            }
        }
    }
    if let Some(reason) = router.attempt_route_current(
        tx,
        route.session_id.as_ref(),
        &request.selection,
        &request.query,
    )? {
        return Ok(Admission::SourceChanged(reason));
    }
    if let Some(reason) = quote_current(tx, request)? {
        return Ok(Admission::SourceChanged(reason));
    }
    let mut work_id = request.work_id.clone();
    let mut dispatch_control_revision = None;
    if let Some(dispatch) = &request.dispatch {
        if ctx.principal_kind() == crate::context::PrincipalKind::Human {
            return Err(StorageError::ScopeDenied);
        }
        let owner = managed_owner(tx, dispatch, route.session_id.as_ref())?;
        if work_id.as_ref().is_some_and(|id| id != &owner.work_id.0) {
            return Err(StorageError::ScopeDenied);
        }
        work_id = Some(owner.work_id.0);
        match control.begin_dispatch(
            tx,
            &DispatchRequest {
                root: dispatch.root_id.clone(),
                execution_id: dispatch.execution_id.clone(),
                owner_epoch: dispatch.owner_epoch,
                target: Target::Assignment(owner.id),
                purpose: if dispatch.implementation {
                    ActivationPurpose::Implementation
                } else {
                    ActivationPurpose::Planning
                },
            },
        )? {
            Ok(admitted) => dispatch_control_revision = Some(admitted.control_revision),
            Err(fenced) => return Ok(Admission::Fenced(fenced)),
        }
    }
    if let Some(session_id) = &route.session_id {
        let session = tx
            .sessions()
            .session(session_id)?
            .ok_or_else(|| invalid("missing inference session"))?;
        if session.closed() {
            return Err(StorageError::ScopeDenied);
        }
        if request.dispatch.is_some()
            && ctx.principal_kind() != crate::context::PrincipalKind::Host
            && session.agent_id != *ctx.principal()
        {
            return Err(StorageError::ScopeDenied);
        }
        if request.expected_context_epoch != Some(session.context_epoch) {
            return Ok(Admission::SourceChanged(
                "session context epoch changed before inference admission".into(),
            ));
        }
        if work_id.is_none() {
            work_id = session.root_id.map(|id| id.0);
        }
        if session.project_id != route.project_id {
            return Err(StorageError::ScopeDenied);
        }
    }
    let work = work_id
        .as_ref()
        .map(|id| tx.work().work(id))
        .transpose()?
        .flatten();
    if work_id.is_some() && work.is_none() {
        return Err(invalid("missing inference work"));
    }
    if let Some(work) = &work
        && route.project_id.as_ref() != Some(&work.project_id)
    {
        return Err(StorageError::ScopeDenied);
    }
    if request.dispatch.is_none()
        && let Some(work) = &work
    {
        let root = tx
            .sessions()
            .work_root(&RootId(work.id.clone()))?
            .ok_or_else(|| invalid("missing inference work root"))?;
        let allowed =
            control.evaluate(tx.runs(), &root, &Target::Root, ActivationPurpose::Planning)?;
        if !allowed.eligible {
            return Ok(Admission::Fenced(Fenced::Blocked(Box::new(allowed))));
        }
    }
    let feature_id = work.as_ref().and_then(|work| {
        if work.is_feature() {
            Some(work.id.clone())
        } else {
            work.parent_feature_id.clone()
        }
    });
    if let Some(run_id) = &request.job_run_id {
        let run = tx
            .jobs()
            .job_run(run_id)?
            .ok_or_else(|| invalid("missing inference job run"))?;
        if run.project_id.is_some() && run.project_id != route.project_id {
            return Err(StorageError::ScopeDenied);
        }
        // Delegated feature/issue work retains a finished run as provenance; its managed
        // root, not the schedule's later lifecycle, owns that work. Discovery is run-owned.
        if request.dispatch.is_none() && work_id.is_none() {
            let owned = crate::services::jobs::discovery::owned_run(tx, run_id)?;
            if owned.cancel_requested || owned.hold.is_some() || owned.status != "Running" {
                return Ok(Admission::Fenced(Fenced::StopRequested));
            }
            if owned.agent_actor.as_ref() != Some(ctx.principal())
                && ctx.principal_kind() != crate::context::PrincipalKind::Host
            {
                return Err(StorageError::ScopeDenied);
            }
        }
    }
    let reserved = pricing::reservation(
        &request.quote,
        request.input_tokens,
        request.output_headroom,
        request.pricing.cache_write_possible,
        &request.unit_headroom,
    )
    .map_err(|_| invalid("invalid monetary reservation"))?;
    if let Some(feature) = &feature_id
        && let Some(block) = budget::check(tx.usage(), feature, reserved.tracked_micro_usd)?
    {
        return Ok(Admission::Blocked(block));
    }
    let binding = AttemptBinding {
        execution_id: request
            .dispatch
            .as_ref()
            .map(|d| crate::protocol::ExecutionId(d.execution_id.clone())),
        owner_generation: request.dispatch.as_ref().map(|d| d.owner_epoch),
        control_revision: dispatch_control_revision,
        policy_revision: Some(ctx.policy_revision().get()),
    };
    let journal = tx
        .operations()
        .begin_attempt(&operation.id, operation.revision, binding)?;
    let connection = tx
        .router()
        .connection(&request.query.connection_id)?
        .ok_or_else(|| invalid("serving connection disappeared"))?;
    let profile = crate::services::router::connections::profiles::profile(&connection.profile)
        .ok_or_else(|| invalid("unsupported serving profile"))?;
    let access_mode = if profile.subscription {
        "Subscription"
    } else {
        match profile.auth {
            crate::services::router::connections::profiles::Auth::ApiKey { required: true } => {
                "ApiKey"
            }
            _ if connection.local_free => "Local",
            _ => "Custom",
        }
    }
    .to_owned();
    let mut query = request.query.clone();
    query.access_mode = access_mode.clone();
    let admitted = AdmittedSnapshot {
        admission_key: request.admission_key.clone(),
        intent_digest: digest,
        quote: request.quote.clone(),
        query,
        price_generation: request.price_generation,
        override_revision: request.override_revision,
        pricing: request.pricing.clone(),
        selection: request.selection.clone(),
        work_revision: work.as_ref().map(|w| w.revision),
        parent_feature_id: work.as_ref().and_then(|w| w.parent_feature_id.clone()),
        quoted_provider_model_id: request.query.provider_model_id.clone(),
        reservation_micro_usd: reserved.tracked_micro_usd,
        admitted_boot_id: journal.boot_id.clone(),
        principal_revision,
        policy_digest,
        authority_project_id: ctx.project_id().cloned(),
        context_epoch: request.expected_context_epoch,
        dispatch: request.dispatch.clone(),
        cache_diagnostics: request.cache_diagnostics.clone(),
    };
    let attempt = StoredInferenceAttempt {
        attempt_id: journal.id.0,
        request_id: request.request_id.clone(),
        operation_id: operation.id.clone(),
        attempt_number: journal.attempt_no,
        project_id: route.project_id,
        feature_id,
        work_id,
        assignment_id: request
            .dispatch
            .as_ref()
            .and_then(|d| d.assignment_id.clone()),
        execution_root_id: request.dispatch.as_ref().map(|d| d.root_id.clone()),
        execution_id: request.dispatch.as_ref().map(|d| d.execution_id.clone()),
        session_id: route.session_id,
        job_run_id: request.job_run_id.clone(),
        actor_id: request.actor_id.clone(),
        role: request.role.clone(),
        purpose: route.purpose,
        catalog_model_id: request.query.catalog_model_id.clone(),
        connection_id: request.query.connection_id.clone(),
        endpoint_profile: request.query.endpoint_profile.clone(),
        access_mode,
        // Configured/quoted upstream is retained in AdmittedSnapshot. Only a response
        // observation can establish which model the provider actually served.
        provider_model_id: None,
        pricing_snapshot: encoded(&admitted)?,
        state: "Admitted".into(),
        revision: 1,
        created_at_ms: tx.now_ms(),
    };
    tx.usage().insert_inference_attempt(&attempt)?;
    let state = BalanceSummary {
        unknown_components: reserved.unknown_components,
        ..Default::default()
    };
    tx.usage().put_attempt_balance(
        &StoredAttemptBalance {
            attempt_id: attempt.attempt_id.clone(),
            revision: 1,
            priced_micro_usd: if request.quote.classification == CostClassification::LocalFree {
                Some(0)
            } else {
                None
            },
            reserved_micro_usd: reserved.tracked_micro_usd,
            classification: classification(request.quote.classification),
            monetary_complete: request.quote.classification == CostClassification::LocalFree,
            incomplete: true,
            unknown_tail: false,
            local_quiescent: false,
            summary: encoded(&state)?,
        },
        None,
    )?;
    log(
        tx,
        &attempt,
        "InferenceAttemptAdmitted",
        None,
        json!({"reservationMicroUsd":reserved.tracked_micro_usd,"monetaryComplete":reserved.monetary_complete}),
    )?;
    Ok(Admission::Admitted {
        attempt,
        reused: false,
    })
}

fn log(
    tx: &mut dyn WriteTransaction,
    attempt: &StoredInferenceAttempt,
    kind: &str,
    delta: Option<i64>,
    payload: Value,
) -> Result<(), StorageError> {
    let event = tx.new_id(crate::ports::IdKind::Event);
    tx.history().append(NewEvent {
        id: EventId(event.clone()),
        actor: attempt.actor_id.clone(),
        project_id: attempt.project_id.clone(),
        root_id: attempt
            .feature_id
            .clone()
            .or_else(|| attempt.work_id.clone())
            .map(RootId),
        operation_id: Some(attempt.operation_id.clone()),
        aggregate_kind: "inference_attempt".into(),
        aggregate_id: attempt.attempt_id.as_str().into(),
        aggregate_revision: attempt.revision,
        kind: kind.into(),
        payload: payload.clone(),
    })?;
    let created_at_ms = tx.now_ms();
    tx.usage().append_usage_change(&StoredUsageChange {
        event_id: event,
        attempt_id: Some(attempt.attempt_id.clone()),
        feature_id: attempt.feature_id.clone(),
        kind: kind.into(),
        delta_micro_usd: delta,
        payload,
        created_at_ms,
    })
}

fn managed_owner(
    tx: &mut dyn WriteTransaction,
    proof: &DispatchIdentity,
    session: Option<&Id>,
) -> Result<StoredAssignment, StorageError> {
    let session = session.ok_or_else(|| invalid("managed inference has no session"))?;
    let owner = match &proof.assignment_id {
        Some(id) => tx.runs().assignment(id)?,
        None => crate::services::runs::coordinator(tx.runs(), &proof.root_id)?,
    }
    .ok_or_else(|| invalid("managed inference owner is unavailable"))?;
    if owner.root_id != proof.root_id
        || owner.session_id != *session
        || owner.generation != proof.assignment_generation
        || owner.execution_id.as_ref() != Some(&proof.execution_id)
        || !owner.state.is_live()
    {
        return Err(StorageError::ScopeDenied);
    }
    let current_boot = tx.store().boot_id.clone();
    if !tx.runs().session_turn(session)?.is_some_and(|turn| {
        turn.turn_id == proof.turn_id
            && turn.assignment_id == owner.id
            && turn.execution_id == proof.execution_id
            && turn.owner_epoch == proof.owner_epoch
            && Some(turn.boot_id) == current_boot
    }) {
        return Err(StorageError::ScopeDenied);
    }
    Ok(owner)
}

fn journal_attempt(
    tx: &mut dyn WriteTransaction,
    attempt: &StoredInferenceAttempt,
) -> Result<(OperationRecord, AttemptRecord), StorageError> {
    let op = tx
        .operations()
        .operation(&attempt.operation_id)?
        .ok_or_else(|| invalid("missing inference operation"))?;
    let journal = op
        .attempts
        .iter()
        .find(|a| a.id.0 == attempt.attempt_id)
        .cloned()
        .ok_or_else(|| invalid("missing inference journal attempt"))?;
    Ok((op, journal))
}

/// Final local admission, immediately before a transport starts. A membership move between
/// reservation and dispatch fences this attempt rather than charging its old owner for new work.
pub(crate) fn mark_dispatched(
    tx: &mut dyn WriteTransaction,
    control: &ControlService,
    id: &Id,
) -> Result<Admission, StorageError> {
    let mut attempt = tx
        .usage()
        .inference_attempt(id)?
        .ok_or_else(|| invalid("unknown inference attempt"))?;
    if attempt.state == "Dispatched" {
        return Ok(Admission::Admitted {
            attempt,
            reused: true,
        });
    }
    if attempt.state != "Admitted" {
        return Err(invalid("inference attempt is not dispatchable"));
    }
    let bound = snapshot(&attempt)?;
    let Some(connection) = tx.router().connection(&attempt.connection_id)? else {
        return Ok(Admission::SourceChanged(
            "serving connection was removed".into(),
        ));
    };
    if bound.quote.connection_revision != Some(connection.revision)
        || !connection.enabled
        || connection.lifecycle != "Connected"
    {
        return Ok(Admission::SourceChanged(
            "serving connection or billing policy changed".into(),
        ));
    }
    if let Some(revision) = bound.principal_revision
        && !tx
            .authority()
            .principal(&attempt.actor_id)?
            .is_some_and(|binding| binding.active && binding.revision == revision)
    {
        return Ok(Admission::SourceChanged(
            "inference principal was changed or revoked".into(),
        ));
    }
    if let Some(digest) = &bound.policy_digest
        && &crate::authority::policy::resolve_policy(
            tx.configuration(),
            bound.authority_project_id.as_ref(),
        )?
        .digest()
            != digest
    {
        return Ok(Admission::SourceChanged(
            "inference access policy changed".into(),
        ));
    }
    if tx.store().boot_id.as_ref() != Some(&bound.admitted_boot_id) {
        return Err(invalid("inference attempt belongs to an earlier host boot"));
    }
    if let Some(work) = &attempt.work_id {
        let current = tx
            .work()
            .work(work)?
            .ok_or_else(|| invalid("missing inference work"))?;
        let owner = if current.is_feature() {
            Some(current.id.clone())
        } else {
            current.parent_feature_id.clone()
        };
        if owner != attempt.feature_id {
            return Ok(Admission::Fenced(Fenced::Invalid(
                "work membership changed after reservation".into(),
            )));
        }
    }
    if let Some(dispatch) = &bound.dispatch {
        let owner = managed_owner(tx, dispatch, attempt.session_id.as_ref())?;
        if let Err(fenced) = control.begin_dispatch(
            tx,
            &DispatchRequest {
                root: dispatch.root_id.clone(),
                execution_id: dispatch.execution_id.clone(),
                owner_epoch: dispatch.owner_epoch,
                target: Target::Assignment(owner.id),
                purpose: if dispatch.implementation {
                    ActivationPurpose::Implementation
                } else {
                    ActivationPurpose::Planning
                },
            },
        )? {
            return Ok(Admission::Fenced(fenced));
        }
    } else if let Some(work) = &attempt.work_id {
        let root = tx
            .sessions()
            .work_root(&RootId(work.clone()))?
            .ok_or_else(|| invalid("missing feature"))?;
        let allowed =
            control.evaluate(tx.runs(), &root, &Target::Root, ActivationPurpose::Planning)?;
        if !allowed.eligible {
            return Ok(Admission::Fenced(Fenced::Blocked(Box::new(allowed))));
        }
    }
    if let Some(session) = &attempt.session_id
        && tx
            .sessions()
            .session(session)?
            .as_ref()
            .map(|s| s.context_epoch)
            != bound.context_epoch
    {
        return Ok(Admission::SourceChanged(
            "session context epoch changed before transport dispatch".into(),
        ));
    }
    // The already-owned reservation is not reserved twice; actual spend and bootstrap remain
    // current gates even for a free or untracked route.
    if let Some(feature) = &attempt.feature_id
        && let Some(block) = budget::check(tx.usage(), feature, None)?
    {
        return Ok(Admission::Blocked(block));
    }
    let (op, journal) = journal_attempt(tx, &attempt)?;
    if op.latest_attempt().map(|a| &a.id) != Some(&journal.id) {
        return Err(invalid("inference journal owner was superseded"));
    }
    tx.operations().transition(
        &op.id,
        op.revision,
        Some(&journal.fence()),
        OperationState::Dispatching,
        Some(json!({"accountingAttemptId":attempt.attempt_id})),
    )?;
    let expected = attempt.revision;
    attempt.revision += 1;
    attempt.state = "Dispatched".into();
    tx.usage().update_inference_attempt(&attempt, expected)?;
    log(
        tx,
        &attempt,
        "InferenceAttemptDispatched",
        None,
        json!({"attemptNumber":attempt.attempt_number}),
    )?;
    Ok(Admission::Admitted {
        attempt,
        reused: false,
    })
}

/// Dedupe immutable observations before merging fields. A later reported charge replaces an
/// estimate of the same attempt through one explicit delta, never a second expense.
pub fn observe(
    tx: &mut dyn WriteTransaction,
    control: &ControlService,
    id: &Id,
    observation: &Observation,
) -> Result<StoredAttemptBalance, StorageError> {
    if observation.key.is_empty() || observation.key.len() > 256 || observation.sequence <= 0 {
        return Err(invalid("invalid usage observation identity"));
    }
    let payload = encoded(observation)?;
    let digest = crate::jcs::canonicalize(&payload)
        .map(|v| crate::fingerprint::sha256_hex(v.as_bytes()))
        .map_err(|_| invalid("invalid usage observation"))?;
    if let Some(before) = tx.usage().usage_observation(id, &observation.key)? {
        if before.digest != digest {
            return Err(invalid("usage observation identity changed payload"));
        }
        return tx
            .usage()
            .attempt_balance(id)?
            .ok_or_else(|| invalid("missing observed balance"));
    }
    let mut attempt = tx
        .usage()
        .inference_attempt(id)?
        .ok_or_else(|| invalid("unknown inference attempt"))?;
    if !matches!(
        attempt.state.as_str(),
        "Dispatched" | "Settled" | "UnknownTail"
    ) {
        return Err(invalid("usage is not bound to a dispatched attempt"));
    }
    let bound = snapshot(&attempt)?;
    let before = tx
        .usage()
        .attempt_balance(id)?
        .ok_or_else(|| invalid("missing inference balance"))?;
    let mut state = summary(&before)?;
    state
        .meter
        .observe(
            observation.sequence,
            observation.semantics,
            &observation.quantities,
        )
        .map_err(|_| invalid("invalid or out-of-order usage quantities"))?;
    if let Some(reported) = &observation.reported_cost {
        if reported.currency.is_empty()
            || reported.currency.len() > 16
            || !reported.currency.bytes().all(|b| b.is_ascii_alphabetic())
        {
            return Err(invalid("invalid usage currency"));
        }
        // Preserve a valid original decimal even when its currency or precision cannot be
        // represented in the USD ledger. It remains an explicit coverage gap, never zero.
        let mut parts = reported.amount.split('.');
        let whole = parts.next().unwrap_or("");
        let fraction = parts.next();
        if reported.amount.len() > 128
            || whole.is_empty()
            || !whole.bytes().all(|b| b.is_ascii_digit())
            || fraction.is_some_and(|f| f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()))
            || parts.next().is_some()
        {
            return Err(invalid("invalid reported amount"));
        }
        state.reported_cost = Some(reported.clone());
    }
    if let Some(model) = &observation.provider_model_id {
        let _: crate::protocol::support::Text = serde_json::from_value(json!(model))
            .map_err(|_| invalid("invalid provider-resolved identity"))?;
        attempt.provider_model_id = Some(model.clone());
    }
    let mut priced = pricing::estimate(&bound.quote, &state.meter.quantities, &bound.pricing)
        .map_err(|_| invalid("usage estimate overflow or invalid quantities"))?;
    let mut effective = bound.quote.classification;
    if bound.quote.classification != CostClassification::LocalFree
        && attempt.provider_model_id.is_some()
        && attempt.provider_model_id != bound.quoted_provider_model_id
    {
        priced.tracked_micro_usd = None;
        priced.monetary_complete = false;
        priced.unknown_components =
            vec!["provider-resolved model has no exact admitted tariff".into()];
        effective = CostClassification::Untracked;
    }
    if let Some(reported) = &state.reported_cost
        && !matches!(
            bound.quote.classification,
            CostClassification::LocalFree | CostClassification::SubscriptionApiEquivalent
        )
    {
        if reported.currency.eq_ignore_ascii_case("USD") {
            let mut exact = CostAccumulator::default();
            match exact
                .amount_usd(&reported.amount)
                .and_then(|_| exact.micro_usd(Rounding::Nearest))
            {
                Ok(amount) => {
                    priced.tracked_micro_usd = Some(amount);
                    priced.monetary_complete = true;
                    priced.unknown_components.clear();
                    effective = CostClassification::ReportedCharge;
                }
                Err(_) => {
                    priced.tracked_micro_usd = None;
                    priced.monetary_complete = false;
                    priced.unknown_components = vec![
                        "reported USD amount exceeds supported exact precision or ledger range"
                            .into(),
                    ];
                    effective = CostClassification::Untracked;
                }
            }
        } else {
            priced.tracked_micro_usd = None;
            priced.monetary_complete = false;
            priced.unknown_components =
                vec!["reported currency has no configured USD conversion basis".into()];
            effective = CostClassification::Untracked;
        }
    }
    state.unknown_components = priced.unknown_components;
    let reserved = if matches!(attempt.state.as_str(), "Settled" | "UnknownTail") {
        before.reserved_micro_usd
    } else {
        bound.reservation_micro_usd.map(|reserved| {
            reserved
                .saturating_sub(priced.tracked_micro_usd.unwrap_or(0))
                .max(0)
        })
    };
    let changed_model = attempt.provider_model_id
        != tx
            .usage()
            .inference_attempt(id)?
            .and_then(|a| a.provider_model_id);
    if changed_model {
        let expected = attempt.revision;
        attempt.revision += 1;
        tx.usage().update_inference_attempt(&attempt, expected)?;
    }
    let balance = StoredAttemptBalance {
        attempt_id: id.clone(),
        revision: before.revision + 1,
        priced_micro_usd: priced.tracked_micro_usd,
        reserved_micro_usd: reserved,
        classification: classification(effective),
        monetary_complete: priced.monetary_complete,
        incomplete: before.incomplete,
        unknown_tail: before.unknown_tail,
        local_quiescent: before.local_quiescent,
        summary: encoded(&state)?,
    };
    let recorded_at_ms = tx.now_ms();
    tx.usage()
        .insert_usage_observation(&StoredUsageObservation {
            attempt_id: id.clone(),
            observation_key: observation.key.clone(),
            sequence: observation.sequence,
            digest,
            payload: payload.clone(),
            recorded_at_ms,
        })?;
    tx.usage()
        .put_attempt_balance(&balance, Some(before.revision))?;
    tx.operations().observe(
        &attempt.operation_id,
        attempt.attempt_number,
        json!({"usageObservation":observation.key,"sequence":observation.sequence}),
    )?;
    let delta = match (before.priced_micro_usd, balance.priced_micro_usd) {
        (None, None) => None,
        (before, after) => Some(
            after
                .unwrap_or(0)
                .checked_sub(before.unwrap_or(0))
                .ok_or_else(|| invalid("cost correction overflow"))?,
        ),
    };
    log(
        tx,
        &attempt,
        "InferenceUsageObserved",
        delta,
        json!({"observationKey":observation.key,"pricedMicroUsd":balance.priced_micro_usd,"classification":balance.classification,"monetaryComplete":balance.monetary_complete,"unknownComponents":state.unknown_components}),
    )?;
    if let Some(feature) = &attempt.feature_id
        && let Some(work) = tx.work().work(feature)?
    {
        budget::reconcile(tx, control, &work)?;
    }
    Ok(balance)
}

/// Constructed only by the owned source driver after its local transport has actually exited.
/// It says nothing about a missing remote tail or an as-yet unreported provider charge.
pub struct LocalQuiescence {
    pub(super) attempt_id: Id,
    pub(super) boot_id: Id,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptEnd {
    Completed,
    ToolCallsReady,
    Incomplete,
    Failed,
    Cancelled,
    UnknownTail,
}
impl AttemptEnd {
    pub fn label(self) -> &'static str {
        match self {
            Self::Completed => "Completed",
            Self::ToolCallsReady => "ToolCallsReady",
            Self::Incomplete => "Incomplete",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
            Self::UnknownTail => "UnknownTail",
        }
    }
}

pub(super) fn settle(
    tx: &mut dyn WriteTransaction,
    control: &ControlService,
    closed: &LocalQuiescence,
    outcome: AttemptEnd,
) -> Result<StoredAttemptBalance, StorageError> {
    let mut attempt = tx
        .usage()
        .inference_attempt(&closed.attempt_id)?
        .ok_or_else(|| invalid("unknown quiesced attempt"))?;
    let bound = snapshot(&attempt)?;
    if bound.admitted_boot_id != closed.boot_id {
        return Err(invalid("transport quiescence belongs to another owner"));
    }
    if !matches!(
        attempt.state.as_str(),
        "Dispatched" | "Settled" | "UnknownTail"
    ) {
        return Err(invalid("attempt was never dispatched"));
    }
    let before = tx
        .usage()
        .attempt_balance(&attempt.attempt_id)?
        .ok_or_else(|| invalid("missing inference balance"))?;
    let mut state = summary(&before)?;
    if state.finished_at_ms.is_some()
        && state.outcome.as_deref() == Some(outcome.label())
        && before.reserved_micro_usd.is_none()
    {
        return Ok(before);
    }
    state.finished_at_ms = Some(tx.now_ms());
    state.outcome = Some(outcome.label().into());
    let expected = attempt.revision;
    attempt.revision += 1;
    attempt.state = if outcome == AttemptEnd::UnknownTail {
        "UnknownTail"
    } else {
        "Settled"
    }
    .into();
    tx.usage().update_inference_attempt(&attempt, expected)?;
    let balance = StoredAttemptBalance {
        attempt_id: attempt.attempt_id.clone(),
        revision: before.revision + 1,
        priced_micro_usd: before.priced_micro_usd,
        reserved_micro_usd: None,
        classification: before.classification.clone(),
        monetary_complete: before.monetary_complete,
        incomplete: !matches!(outcome, AttemptEnd::Completed | AttemptEnd::ToolCallsReady),
        unknown_tail: outcome == AttemptEnd::UnknownTail,
        local_quiescent: true,
        summary: encoded(&state)?,
    };
    tx.usage()
        .put_attempt_balance(&balance, Some(before.revision))?;
    let (op, journal) = journal_attempt(tx, &attempt)?;
    let observation = json!({"outcome":outcome.label(),"transportQuiescent":true,"accountingAttemptId":attempt.attempt_id});
    if op
        .latest_attempt()
        .is_some_and(|last| last.id == journal.id)
        && matches!(
            op.state,
            OperationState::Dispatching
                | OperationState::OutcomeUnknown
                | OperationState::NeedsInput
        )
    {
        let to = match outcome {
            AttemptEnd::Completed | AttemptEnd::ToolCallsReady => OperationState::ObservedComplete,
            AttemptEnd::UnknownTail => OperationState::OutcomeUnknown,
            _ => OperationState::Failed,
        };
        if op.state != to && transition_allowed(op.state, to) {
            tx.operations().transition(
                &op.id,
                op.revision,
                Some(&journal.fence()),
                to,
                Some(observation.clone()),
            )?;
        } else {
            tx.operations()
                .observe(&op.id, journal.attempt_no, observation)?;
        }
    } else {
        tx.operations()
            .observe(&op.id, journal.attempt_no, observation)?;
    }
    log(
        tx,
        &attempt,
        "InferenceAttemptSettled",
        None,
        json!({"outcome":outcome.label(),"releasedReservationMicroUsd":before.reserved_micro_usd,"unknownTail":balance.unknown_tail,"monetaryComplete":balance.monetary_complete}),
    )?;
    if let Some(feature) = &attempt.feature_id
        && let Some(work) = tx.work().work(feature)?
    {
        budget::reconcile(tx, control, &work)?;
    }
    Ok(balance)
}

/// A preflight fenced by membership, policy or budget never opened a transport. Retiring that
/// reservation cannot cancel a newer journal attempt or fabricate a provider cancellation.
pub fn cancel_admitted(
    tx: &mut dyn WriteTransaction,
    id: &Id,
    reason: &str,
) -> Result<StoredAttemptBalance, StorageError> {
    let mut attempt = tx
        .usage()
        .inference_attempt(id)?
        .ok_or_else(|| invalid("unknown inference attempt"))?;
    let before = tx
        .usage()
        .attempt_balance(id)?
        .ok_or_else(|| invalid("missing inference balance"))?;
    if attempt.state == "CancelledBeforeDispatch" {
        return Ok(before);
    }
    if attempt.state != "Admitted" {
        return Err(invalid(
            "a dispatched inference requires actual transport quiescence",
        ));
    }
    let expected = attempt.revision;
    attempt.revision += 1;
    attempt.state = "CancelledBeforeDispatch".into();
    tx.usage().update_inference_attempt(&attempt, expected)?;
    let mut state = summary(&before)?;
    state.finished_at_ms = Some(tx.now_ms());
    state.outcome = Some("CancelledBeforeDispatch".into());
    state.unknown_components.clear();
    let balance = StoredAttemptBalance {
        attempt_id: id.clone(),
        revision: before.revision + 1,
        priced_micro_usd: Some(0),
        reserved_micro_usd: None,
        classification: before.classification.clone(),
        monetary_complete: true,
        incomplete: false,
        unknown_tail: false,
        local_quiescent: true,
        summary: encoded(&state)?,
    };
    tx.usage()
        .put_attempt_balance(&balance, Some(before.revision))?;
    let (op, journal) = journal_attempt(tx, &attempt)?;
    if op
        .latest_attempt()
        .is_some_and(|last| last.id == journal.id)
        && op.state == OperationState::Preflighted
    {
        tx.operations().transition(
            &op.id,
            op.revision,
            Some(&journal.fence()),
            OperationState::CancelledBeforeDispatch,
            Some(json!({"reason":reason})),
        )?;
    }
    log(
        tx,
        &attempt,
        "InferenceAttemptCancelledBeforeDispatch",
        None,
        json!({"reason":reason,"releasedReservationMicroUsd":before.reserved_micro_usd}),
    )?;
    Ok(balance)
}
