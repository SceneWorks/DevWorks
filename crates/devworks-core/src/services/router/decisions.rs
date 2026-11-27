//! Durable SystemOne allocation. The recorded operation delegates delivery to the native host.
use super::{InferenceRouter, decision_models, ops, select};
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::AdmittedCommand;
use crate::operations::OperationInput;
use crate::ports::storage::{
    AttemptRecord, NewEvent, NewOperation, NewQueueEntry, OperationRecord, RouterRead,
    StorageError, StoreInfo, StoredDecisionModel, WriteTransaction,
};
use crate::ports::{AuthorityFence, CommandHandler, Decided, HandlerFailure, IdKind};
use crate::protocol::records::DecisionPolicy;
use crate::protocol::results::DomainDecision;
use crate::protocol::{
    ActorId, DomainErrorCode, EventId, Id, OperationId, OperationState, RetryAdvice,
    RevisionDimension,
};
use crate::providers::systemone::{self, Requirements};
use crate::services::kernel::{Failure, Refusal, decided, not_found, rev};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

pub const OPERATION: &str = "router.decide";
pub const CONSUMER: &str = "systemone-decisions";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub model_id: Id,
    pub revision: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecisionIntent {
    pub input: ops::DecideInput,
    pub requirements: Requirements,
    pub candidates: Vec<Candidate>,
    pub delivery_id: Id,
    pub actor: ActorId,
    pub principal_kind: String,
    pub authority_fences: Vec<AuthorityFence>,
    pub context: Value,
    #[serde(default)]
    pub automatic_tier: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reused_allocation: Option<Allocation>,
}

impl DecisionIntent {
    pub fn source_context(&self, store: &StoreInfo) -> Result<InvocationContext, StorageError> {
        let kind = PrincipalKind::parse(&self.principal_kind).ok_or(StorageError::ScopeDenied)?;
        crate::services::approvals::restore_source_context(store, &self.actor, kind, &self.context)
    }
}

struct DecisionSource {
    schema_revision: i64,
    args_digest: String,
    fences: Vec<AuthorityFence>,
    automatic_tier: bool,
    reused_allocation: Option<Allocation>,
}

pub struct DecisionService {
    router: Arc<InferenceRouter>,
}

impl DecisionService {
    pub fn new(router: Arc<InferenceRouter>) -> Self {
        Self { router }
    }

    fn enqueue(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        source: DecisionSource,
        input: &ops::DecideInput,
    ) -> Result<Decided, Failure> {
        let requirements = systemone::requirements(input)
            .map_err(|_| Refusal::new(DomainErrorCode::InvalidArgument, "decision_input", None))?;
        let now = tx.now_ms();
        let connections = self.router.connections.connections(tx.router())?;
        let cooldowns = select::active_cooldowns(&tx.router().cooldowns()?, now);
        let available = |model: &StoredDecisionModel| -> Result<(), Refusal> {
            if let Err(blocked) = decision_models::availability(model, &connections, &cooldowns) {
                let (code, retry) = match blocked {
                    select::Block::Cooling(_) | select::Block::ConnectionUnavailable => (
                        DomainErrorCode::CapacityWait,
                        RetryAdvice::AfterBlockerClears,
                    ),
                    select::Block::CredentialsRequired | select::Block::NoConnection => (
                        DomainErrorCode::CredentialsRequired,
                        RetryAdvice::AfterBlockerClears,
                    ),
                    _ => (
                        DomainErrorCode::UnsupportedCapability,
                        RetryAdvice::RefreshAndNewRequest,
                    ),
                };
                return Err(Refusal::new(code, blocked.code(), Some(model.id.clone())).retry(retry));
            }
            Ok(())
        };
        let candidates =
            if let (Some(id), Some(expected)) = (&input.model_id, &input.expected_model_revision) {
                let model = tx
                    .router()
                    .decision_model(id)?
                    .ok_or_else(|| not_found("decision_model", id))?;
                if model.revision != expected.get() {
                    return Err(StorageError::conflict(
                        RevisionDimension::Configuration,
                        "decision_model",
                        id.clone(),
                        expected.clone(),
                        rev(model.revision),
                    )
                    .into());
                }
                available(&model)?;
                if !supports(&model, &requirements, false)? {
                    return Err(Refusal::new(
                        DomainErrorCode::UnsupportedCapability,
                        "decision_requirements",
                        Some(id.clone()),
                    )
                    .into());
                }
                vec![candidate(model)]
            } else {
                let read = tx.router();
                let pool = pool_models(read, &requirements, available)?;
                if pool.is_empty() {
                    return Err(Refusal::new(
                        DomainErrorCode::UnsupportedCapability,
                        "decision_pool_empty",
                        None,
                    )
                    .retry(RetryAdvice::RefreshAndNewRequest)
                    .into());
                }
                pool
            };
        let operation_id = OperationId(tx.new_id(IdKind::Operation));
        let delivery_id = tx.new_id(IdKind::QueueEntry);
        let intent = DecisionIntent {
            input: input.clone(),
            requirements,
            candidates,
            delivery_id: delivery_id.clone(),
            actor: ctx.principal().clone(),
            principal_kind: ctx.principal_kind().as_str().into(),
            authority_fences: source.fences,
            context: crate::services::approvals::save_source_context(ctx),
            automatic_tier: source.automatic_tier,
            reused_allocation: source.reused_allocation,
        };
        tx.operations().record_intent(NewOperation {
            id: operation_id.clone(),
            root_id: ctx.root_id().cloned(),
            assignment_id: ctx.assignment_id().cloned(),
            capability: OPERATION.into(),
            schema_version: source.schema_revision,
            args_digest: source.args_digest,
            payload: serde_json::to_value(&intent).map_err(invalid_record)?,
        })?;
        let event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event.clone(),
            actor: ctx.principal().clone(),
            project_id: ctx.project_id().cloned(),
            root_id: ctx.root_id().cloned(),
            operation_id: Some(operation_id.clone()),
            aggregate_kind: "systemone_decision".into(),
            aggregate_id: operation_id.0.as_str().into(),
            aggregate_revision: 1,
            kind: "SystemOneDecisionRequested".into(),
            payload: json!({"operationId":operation_id}),
        })?;
        tx.queue().enqueue(NewQueueEntry {
            id: delivery_id,
            consumer: CONSUMER.into(),
            event_id: event,
            operation_id: Some(operation_id.clone()),
            root_id: ctx.root_id().cloned(),
            partition: None,
            priority: 0,
            ready_at_ms: now,
            payload: json!({"operationId":operation_id}),
        })?;
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: operation_id.clone(),
            },
            operation_id: Some(operation_id),
        })
    }

    /// The native first-generation owner can queue an auxiliary choice without exposing
    /// a command to the model. Keep the original principal and native budget attribution.
    pub fn enqueue_auto(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        input: &ops::DecideInput,
    ) -> Result<Option<OperationId>, StorageError> {
        let session = ctx.session_id().ok_or(StorageError::ScopeDenied)?;
        let record = tx
            .router()
            .auto_route(session)?
            .ok_or(StorageError::ScopeDenied)?;
        if record.low_plan.is_some() || record.decision.is_some() {
            return Ok(record.systemone_operation);
        }
        let previous = record.systemone_operation;
        let mut reused_allocation = None;
        if let Some(operation) = &previous {
            let current = tx
                .operations()
                .operation(operation)?
                .ok_or(StorageError::ScopeDenied)?;
            if !auto_retry_without_send(tx, &current)? {
                return Ok(previous);
            }
            reused_allocation = prior_allocation(&current)?;
        }
        let fences = super::auto_routing::current_authority(tx, ctx)?;
        let args_digest = crate::fingerprint::canonical_digest(
            &serde_json::to_value(input).map_err(invalid_record)?,
        )
        .map_err(|_| StorageError::ScopeDenied)?;
        let result = self.enqueue(
            tx,
            ctx,
            DecisionSource {
                schema_revision: 4,
                args_digest,
                fences,
                automatic_tier: true,
                reused_allocation,
            },
            input,
        );
        match result {
            Ok(Decided {
                operation_id: Some(operation),
                ..
            }) => {
                tx.router()
                    .bind_auto_decision_operation(session, &operation, previous.as_ref())?;
                Ok(Some(operation))
            }
            Err(Failure::Refused(refusal))
                if matches!(
                    refusal.target_kind,
                    "decision_pool_empty" | "decision_input"
                ) =>
            {
                Ok(None)
            }
            Err(other) => Err(other.into_storage()),
            _ => Err(StorageError::ScopeDenied),
        }
    }

    /// Finish allocating the saved logical request after the native owner has resolved media,
    /// registered schemas and exact connection access. The cursor and reusable ordering commit
    /// together. Retries that proved no send reuse the ordering and never advance it again.
    pub fn allocate(
        &self,
        tx: &mut dyn WriteTransaction,
        operation: &OperationRecord,
        intent: &DecisionIntent,
        verified: Vec<Candidate>,
        input_digest: &str,
    ) -> Result<Vec<Candidate>, StorageError> {
        use crate::services::fences::fences_hold;
        let current = tx
            .operations()
            .operation(&operation.id)?
            .ok_or(StorageError::ScopeDenied)?;
        let attempt = current.latest_attempt().ok_or(StorageError::ScopeDenied)?;
        let delivery = tx
            .queue()
            .entry(&intent.delivery_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if delivery.cancel_requested {
            return Err(StorageError::Cancelled);
        }
        if current.state != OperationState::Dispatching
            || Some(attempt.fence()) != operation.latest_attempt().map(AttemptRecord::fence)
            || delivery.operation_id.as_ref() != Some(&current.id)
            || !fences_hold(tx, &intent.authority_fences)?
        {
            return Err(StorageError::ScopeDenied);
        }
        let pooled = intent.input.use_pool == Some(true);
        let mut candidates = Vec::new();
        let now = tx.now_ms();
        let read = tx.router();
        for candidate in &intent.candidates {
            if verified.contains(candidate)
                && self
                    .current_candidate(read, candidate, &intent.requirements, pooled, now)?
                    .is_some()
            {
                candidates.push(candidate.clone());
            }
        }
        if let Some(prior) =
            prior_allocation(&current)?.or_else(|| intent.reused_allocation.clone())
        {
            if prior.input_digest != input_digest {
                return Err(StorageError::ScopeDenied);
            }
            return Ok(prior
                .candidates
                .into_iter()
                .filter(|candidate| candidates.contains(candidate))
                .collect());
        }
        if pooled {
            let last = read.decision_pool_cursor()?;
            let next = candidates.partition_point(|model| {
                last.as_ref()
                    .is_some_and(|last| model.model_id.as_str() <= last.as_str())
            });
            if next < candidates.len() {
                candidates.rotate_left(next);
            }
            if let Some(first) = candidates.first() {
                tx.router().set_decision_pool_cursor(&first.model_id)?;
            }
        }
        if !candidates.is_empty() {
            tx.operations().observe(
                &operation.id,
                attempt.attempt_no,
                json!({"decisionAllocation": Allocation { input_digest: input_digest.to_owned(), candidates: candidates.clone() }}),
            )?;
        }
        Ok(candidates)
    }

    /// Recheck a saved candidate without reordering the request or reading a conversational pin.
    pub fn current_candidate(
        &self,
        read: &dyn RouterRead,
        candidate: &Candidate,
        requirements: &Requirements,
        pooled: bool,
        now: i64,
    ) -> Result<Option<StoredDecisionModel>, StorageError> {
        let Some(model) = read.decision_model(&candidate.model_id)? else {
            return Ok(None);
        };
        let connections = self.router.connections.connections(read)?;
        let cooldowns = select::active_cooldowns(&read.cooldowns()?, now);
        if model.revision != candidate.revision
            || !supports(&model, requirements, pooled)?
            || decision_models::availability(&model, &connections, &cooldowns).is_err()
        {
            return Ok(None);
        }
        Ok(Some(model))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Allocation {
    input_digest: String,
    candidates: Vec<Candidate>,
}

fn prior_allocation(operation: &OperationRecord) -> Result<Option<Allocation>, StorageError> {
    operation
        .attempts
        .iter()
        .flat_map(|attempt| &attempt.observations)
        .find_map(|note| note.get("decisionAllocation"))
        .map(|value| serde_json::from_value(value.clone()).map_err(invalid_record))
        .transpose()
}

/// The exact logical request identity for one candidate in one SystemOne pass.
pub fn candidate_request_id(operation: &OperationId, model: &Id) -> Result<Id, StorageError> {
    Id::new(format!(
        "systemone-{}",
        crate::fingerprint::sha256_hex(
            format!("{}\n{}", operation.0.as_str(), model.as_str()).as_bytes(),
        )
    ))
    .map_err(|_| StorageError::ScopeDenied)
}

fn auto_retry_without_send(
    tx: &mut dyn WriteTransaction,
    operation: &OperationRecord,
) -> Result<bool, StorageError> {
    use crate::ports::storage::AttemptState;
    if operation.state == OperationState::CancelledBeforeDispatch {
        return Ok(true);
    }
    if operation.state != OperationState::Failed
        || !operation.latest_attempt().is_some_and(|attempt| {
            attempt.observations.iter().any(|note| {
                note.pointer("/effect/cause")
                    .and_then(Value::as_str)
                    .is_some_and(|cause| {
                        matches!(
                            cause,
                            "Feature budget refused the decision request"
                                | "SystemOne decision cancelled before dispatch"
                                | "Decision was cancelled before dispatch"
                        )
                    })
            })
        })
    {
        return Ok(false);
    }
    let saved: DecisionIntent =
        serde_json::from_value(operation.payload.clone()).map_err(invalid_record)?;
    for candidate in &saved.candidates {
        let request = candidate_request_id(&operation.id, &candidate.model_id)?;
        let Some(route) = tx.router().route_request(&request)? else {
            continue;
        };
        let native = tx
            .operations()
            .operation(&route.operation_id)?
            .ok_or(StorageError::ScopeDenied)?;
        for journal in &native.attempts {
            if let Some(attempt) = tx.usage().inference_attempt(&journal.id.0)?
                && attempt.state != AttemptState::CancelledBeforeDispatch
            {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn candidate(model: StoredDecisionModel) -> Candidate {
    Candidate {
        model_id: model.id,
        revision: model.revision,
    }
}

fn invalid_record(_: serde_json::Error) -> StorageError {
    StorageError::InvalidRecord("stored SystemOne decision".into())
}

fn pool_models(
    read: &dyn RouterRead,
    requirements: &Requirements,
    available: impl Fn(&StoredDecisionModel) -> Result<(), Refusal>,
) -> Result<Vec<Candidate>, StorageError> {
    let mut candidates = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let models = read.decision_models(after.as_deref(), crate::ports::storage::MAX_PAGE)?;
        let next = models.last().map(|model| model.id.as_str().to_owned());
        if next.is_none() {
            break;
        }
        for model in models {
            if available(&model).is_ok() && supports(&model, requirements, true)? {
                candidates.push(candidate(model));
            }
        }
        after = next;
    }
    Ok(candidates)
}

pub(super) fn supports(
    model: &StoredDecisionModel,
    requirements: &Requirements,
    pooled: bool,
) -> Result<bool, StorageError> {
    let supported = requirements
        .question_types
        .iter()
        .all(|t| model.question_types.contains(t))
        && (!requirements.images || model.accepts_images)
        && (!requirements.videos || model.accepts_videos);
    if !supported {
        return Ok(false);
    }
    if !requirements.extension_schemas.is_empty() {
        let schemas: Vec<Value> =
            serde_json::from_str(&model.extension_schemas).map_err(invalid_record)?;
        for reference in &requirements.extension_schemas {
            if !schemas.contains(&serde_json::to_value(reference).map_err(invalid_record)?) {
                return Ok(false);
            }
        }
    }
    let Some(policy) = &model.decision_policy else {
        return Ok(!pooled);
    };
    let policy: DecisionPolicy = serde_json::from_str(policy).map_err(invalid_record)?;
    let within =
        |actual: usize, limit: &str| limit.parse::<usize>().is_ok_and(|limit| actual <= limit);
    Ok((!pooled || policy.pool_member)
        && within(requirements.bytes, &policy.max_input_bytes.0)
        && within(requirements.question_count, &policy.max_question_count.0)
        && within(requirements.batch_bytes, &policy.max_batch_bytes.0))
}

impl CommandHandler for DecisionService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let OperationInput::Router(ops::Input::Decide(input)) = command.input() else {
            return decided(
                tx,
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into()),
            );
        };
        let source = DecisionSource {
            schema_revision: i64::from(command.schema_revision().0),
            args_digest: command.fingerprint().as_str().to_owned(),
            fences: command.authorization().fences().to_vec(),
            automatic_tier: false,
            reused_allocation: None,
        };
        match self.enqueue(tx, ctx, source, input) {
            Ok(result) => Ok(result),
            Err(error) => decided(tx, Err(error)),
        }
    }
}
