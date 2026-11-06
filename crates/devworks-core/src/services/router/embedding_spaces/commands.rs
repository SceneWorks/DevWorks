use super::{EFFECT_CONSUMER, EmbeddingSpaceService, PROBE_KIND, embedding_identity};
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::AdmittedCommand;
use crate::operations::{OperationInput, router as ops};
use crate::ports::storage::*;
use crate::ports::{CommandHandler, Decided, HandlerFailure, IdKind};
use crate::protocol::data::{DataScope, Metric};
use crate::protocol::records::{
    EmbeddingDimensionMode, EmbeddingMetric, MutationSummary, RouterConfigAction,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::{DomainErrorCode, EventId, Id, OperationId, RevisionDimension};
use crate::providers::capabilities::EmbeddingDimensions;
use crate::services::sessions::{Failure, Refusal, rev};
use serde_json::json;

fn result(id: Id, revision: i64, changed: bool, operation_id: Option<OperationId>) -> Decided {
    let result = OperationResult::RouterConfigure {
        value: MutationSummary {
            id,
            revision: rev(revision),
            changed,
        },
    };
    Decided {
        decision: if changed {
            DomainDecision::Applied { result }
        } else {
            DomainDecision::NoChange { result }
        },
        operation_id,
    }
}
fn denied(code: DomainErrorCode, kind: &'static str, id: Option<Id>) -> Failure {
    Refusal::new(code, kind, id).into()
}
fn qualification(
    tx: &mut dyn WriteTransaction,
    id: &Id,
    expected: i64,
) -> Result<StoredEmbeddingQualification, Failure> {
    let q = tx.router().embedding_qualification(id)?.ok_or_else(|| {
        denied(
            DomainErrorCode::NotFound,
            "embedding_space",
            Some(id.clone()),
        )
    })?;
    if q.revision != expected {
        return Err(StorageError::conflict(
            RevisionDimension::Configuration,
            "embedding_space",
            id.clone(),
            rev(expected),
            rev(q.revision),
        )
        .into());
    }
    Ok(q)
}
impl EmbeddingSpaceService {
    pub fn owns(action: &RouterConfigAction) -> bool {
        matches!(
            action,
            RouterConfigAction::CreateEmbeddingSpace { .. }
                | RouterConfigAction::QualifyEmbeddingSpace { .. }
                | RouterConfigAction::SelectEmbeddingSpace { .. }
                | RouterConfigAction::ClearEmbeddingSpace { .. }
        )
    }
    fn configure(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        action: &RouterConfigAction,
    ) -> Result<Decided, Failure> {
        if ctx.principal_kind() != PrincipalKind::Human
            || tx.actors().actor_kind(ctx.principal())? != Some(PrincipalKind::Human)
            || tx.store().store_id != *ctx.store_id()
            || tx.store().restore_epoch != ctx.restore_epoch().get()
        {
            return Err(denied(
                DomainErrorCode::PermissionDenied,
                "human_configuration",
                None,
            ));
        }
        match action {
            RouterConfigAction::CreateEmbeddingSpace {
                name,
                connection_id,
                expected_connection_revision,
                upstream_model_id,
                dimensions,
                dimension_mode,
                metric,
            } => {
                let connection = tx.router().connection(connection_id)?.ok_or_else(|| {
                    denied(
                        DomainErrorCode::NotFound,
                        "connection",
                        Some(connection_id.clone()),
                    )
                })?;
                if connection.revision != expected_connection_revision.get() {
                    return Err(StorageError::conflict(
                        RevisionDimension::Configuration,
                        "provider_connection",
                        connection_id.clone(),
                        expected_connection_revision.clone(),
                        rev(connection.revision),
                    )
                    .into());
                }
                if connection.profile != "Embedding" {
                    return Err(denied(
                        DomainErrorCode::UnsupportedCapability,
                        "embedding_profile",
                        Some(connection_id.clone()),
                    ));
                }
                if !connection.enabled || connection.lifecycle != "Connected" {
                    return Err(denied(
                        DomainErrorCode::CredentialsRequired,
                        "connection",
                        Some(connection_id.clone()),
                    ));
                }
                if !connection
                    .discovered
                    .as_ref()
                    .is_some_and(|list| list.iter().any(|m| m.id == upstream_model_id.0))
                {
                    return Err(denied(
                        DomainErrorCode::UnsupportedCapability,
                        "embedding_model",
                        Some(connection_id.clone()),
                    ));
                }
                let dimensions = dimensions
                    .0
                    .parse::<u32>()
                    .ok()
                    .filter(|d| (1..=65536).contains(d))
                    .ok_or_else(|| {
                        denied(
                            DomainErrorCode::InvalidArgument,
                            "embedding_dimensions",
                            None,
                        )
                    })?;
                let mode = match dimension_mode {
                    EmbeddingDimensionMode::Native => EmbeddingDimensions::Fixed(dimensions),
                    EmbeddingDimensionMode::Parameter => EmbeddingDimensions::Parameter,
                };
                let metric = match metric {
                    EmbeddingMetric::L2 => Metric::L2,
                    EmbeddingMetric::Cosine => Metric::Cosine,
                };
                let identity = embedding_identity(
                    &connection,
                    &upstream_model_id.0,
                    dimensions,
                    &mode,
                    metric,
                )?;
                let id = tx.new_id(IdKind::EmbeddingSpace);
                let candidate = StoredEmbeddingCandidate {
                    id: id.clone(),
                    name: name.0.clone(),
                    space_id: identity.space.space_id,
                    connection_id: connection.id,
                    account_revision: connection.account_revision,
                    account_digest: connection.account_digest,
                    endpoint_digest: crate::fingerprint::sha256_hex(connection.endpoint.as_bytes()),
                    endpoint: connection.endpoint,
                    upstream_model_id: upstream_model_id.0.clone(),
                    dimension_mode: if matches!(mode, EmbeddingDimensions::Parameter) {
                        "Parameter"
                    } else {
                        "Native"
                    }
                    .into(),
                    dimensions: i64::from(dimensions),
                    metric: if matches!(identity.space.metric, Metric::Cosine) {
                        "Cosine"
                    } else {
                        "L2"
                    }
                    .into(),
                    element_type: "Float32".into(),
                    encoder_config: identity.encoder_config,
                    configuration_digest: identity.configuration_digest,
                    model_digest: identity.model_digest,
                    created_by: ctx.principal().clone(),
                    created_at_ms: tx.now_ms(),
                };
                tx.router()
                    .insert_embedding_candidate(&candidate, expected_connection_revision.get())?;
                let event = EventId(tx.new_id(IdKind::Event));
                tx.history().append(NewEvent{id:event,actor:ctx.principal().clone(),project_id:None,root_id:None,operation_id:None,aggregate_kind:"embedding_space".into(),aggregate_id:id.as_str().into(),aggregate_revision:1,kind:"EmbeddingSpaceCreated".into(),payload:json!({"candidateId":id,"spaceId":candidate.space_id,"configurationDigest":candidate.configuration_digest})})?;
                Ok(result(id, 1, true, None))
            }
            RouterConfigAction::QualifyEmbeddingSpace {
                candidate_id,
                expected_revision,
            } => {
                if !self.qualification_available() {
                    return Err(denied(
                        DomainErrorCode::UnsupportedCapability,
                        "native_embedding_runtime",
                        Some(candidate_id.clone()),
                    ));
                }
                let q = qualification(tx, candidate_id, expected_revision.get())?;
                if let Some(previous) = q.probe.as_ref()
                    && tx
                        .operations()
                        .operation(&previous.operation_id)?
                        .is_some_and(|op| !is_terminal(op.state))
                {
                    return Err(StorageError::Busy.into());
                }
                let candidate =
                    tx.router()
                        .embedding_candidate(candidate_id)?
                        .ok_or_else(|| {
                            denied(
                                DomainErrorCode::NotFound,
                                "embedding_space",
                                Some(candidate_id.clone()),
                            )
                        })?;
                let connection = tx
                    .router()
                    .connection(&candidate.connection_id)?
                    .ok_or_else(|| {
                        denied(
                            DomainErrorCode::NotFound,
                            "connection",
                            Some(candidate.connection_id.clone()),
                        )
                    })?;
                if super::resolve::current_candidate(&candidate, Some(&connection))?.is_some() {
                    return Err(denied(
                        DomainErrorCode::UnsupportedCapability,
                        "embedding_source",
                        Some(candidate_id.clone()),
                    ));
                }
                let operation = OperationId(tx.new_id(IdKind::Operation));
                let delivery = tx.new_id(IdKind::QueueEntry);
                let request = Id::new(format!(
                    "eprobe-{}",
                    crate::fingerprint::sha256_hex(
                        format!("{}:{}", ctx.store_id().0.as_str(), operation.0.as_str())
                            .as_bytes()
                    )
                ))
                .map_err(|_| StorageError::InvalidRecord("embedding request ID".into()))?;
                let admission_key = Id::new(format!(
                    "eprobe-admit-{}",
                    crate::fingerprint::sha256_hex(operation.0.as_str().as_bytes())
                ))
                .map_err(|_| StorageError::InvalidRecord("embedding admission ID".into()))?;
                let probe = StoredEmbeddingProbe {
                    operation_id: operation.clone(),
                    delivery_id: delivery.clone(),
                    request_id: request,
                    admission_key,
                    candidate_revision: q.revision,
                    store_id: ctx.store_id().clone(),
                    restore_epoch: ctx.restore_epoch().get(),
                    actor_id: ctx.principal().clone(),
                    scope: ctx
                        .project_id()
                        .cloned()
                        .map_or(DataScope::Workspace, |project_id| DataScope::Project {
                            project_id,
                        }),
                    connection_revision: connection.revision,
                    source_context: crate::services::approvals::save_source_context(ctx),
                    authority_fences: command.authorization().fences().to_vec(),
                };
                tx.operations().record_intent(NewOperation{id:operation.clone(),root_id:None,assignment_id:None,capability:"router.configure".into(),schema_version:1,args_digest:command.fingerprint().as_str().into(),payload:json!({"kind":PROBE_KIND,"candidateId":candidate_id,"configurationDigest":candidate.configuration_digest,"probe":probe})})?;
                let event = EventId(tx.new_id(IdKind::Event));
                tx.history().append(NewEvent{id:event.clone(),actor:ctx.principal().clone(),project_id:None,root_id:None,operation_id:Some(operation.clone()),aggregate_kind:"embedding_space".into(),aggregate_id:candidate_id.as_str().into(),aggregate_revision:q.revision+1,kind:"EmbeddingQualificationRequested".into(),payload:json!({"candidateId":candidate_id,"spaceId":candidate.space_id,"requestId":probe.request_id})})?;
                let now = tx.now_ms();
                tx.queue().enqueue(NewQueueEntry {
                    id: delivery,
                    consumer: EFFECT_CONSUMER.into(),
                    event_id: event,
                    root_id: None,
                    operation_id: Some(operation.clone()),
                    partition: None,
                    priority: 0,
                    ready_at_ms: now,
                    payload: json!({"kind":PROBE_KIND,"candidateId":candidate_id}),
                })?;
                let saved =
                    tx.router()
                        .start_embedding_qualification(candidate_id, q.revision, &probe)?;
                Ok(result(
                    candidate_id.clone(),
                    saved.revision,
                    true,
                    Some(operation),
                ))
            }
            RouterConfigAction::SelectEmbeddingSpace {
                candidate_id,
                expected_revision,
                expected_selection_revision,
            } => {
                qualification(tx, candidate_id, expected_revision.get())?;
                let saved = tx.router().select_embedding_candidate(
                    expected_selection_revision.get(),
                    Some((candidate_id, expected_revision.get())),
                )?;
                self.selection_event(tx, ctx, &saved)?;
                Ok(result(
                    Id::new("embedding_selection").expect("owned singleton"),
                    saved.revision,
                    true,
                    None,
                ))
            }
            RouterConfigAction::ClearEmbeddingSpace {
                expected_selection_revision,
            } => {
                let before = tx.router().embedding_selection()?;
                if before.revision != expected_selection_revision.get() {
                    return Err(StorageError::conflict(
                        RevisionDimension::Configuration,
                        "embedding_selection",
                        Id::new("embedding_selection").expect("owned singleton"),
                        expected_selection_revision.clone(),
                        rev(before.revision),
                    )
                    .into());
                }
                if before.candidate_id.is_none() {
                    return Ok(result(
                        Id::new("embedding_selection").expect("owned singleton"),
                        before.revision,
                        false,
                        None,
                    ));
                }
                let saved = tx
                    .router()
                    .select_embedding_candidate(expected_selection_revision.get(), None)?;
                self.selection_event(tx, ctx, &saved)?;
                Ok(result(
                    Id::new("embedding_selection").expect("owned singleton"),
                    saved.revision,
                    true,
                    None,
                ))
            }
            _ => Err(denied(
                DomainErrorCode::UnsupportedCapability,
                "embedding_configuration",
                None,
            )),
        }
    }
    fn selection_event(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        selected: &StoredEmbeddingSelection,
    ) -> Result<(), StorageError> {
        let id = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent{id,actor:ctx.principal().clone(),project_id:None,root_id:None,operation_id:None,aggregate_kind:"embedding_selection".into(),aggregate_id:"embedding_selection".into(),aggregate_revision:selected.revision,kind:"EmbeddingSelectionChanged".into(),payload:json!({"candidateId":selected.candidate_id,"spaceId":selected.space_id,"selectionRevision":selected.revision,"scope":{"kind":"Workspace"},"sourceContext":crate::services::approvals::save_source_context(ctx)})})?;
        Ok(())
    }
}
impl CommandHandler for EmbeddingSpaceService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let outcome = match command.input() {
            OperationInput::Router(ops::Input::Configure(input)) => {
                self.configure(tx, ctx, command, &input.action)
            }
            _ => Err(denied(
                DomainErrorCode::UnsupportedCapability,
                "embedding_configuration",
                None,
            )),
        };
        match outcome {
            Ok(decision) => Ok(decision),
            Err(Failure::Storage(error)) => Err(HandlerFailure::Storage(error)),
            Err(Failure::Refused(refusal)) => Ok(Decided {
                decision: DomainDecision::Refused {
                    fault: refusal
                        .fault(crate::protocol::CorrelationId(ctx.invocation_id().clone())),
                },
                operation_id: None,
            }),
        }
    }
}
