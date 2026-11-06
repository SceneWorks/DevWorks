use super::{EFFECT_CONSUMER, HelpService, JOB_KIND};
use crate::context::InvocationContext;
use crate::gateway::AdmittedCommand;
use crate::operations::{OperationInput, help as ops};
use crate::ports::storage::*;
use crate::ports::{AuthorityFence, CommandHandler, Decided, HandlerFailure, IdKind};
use crate::protocol::records::OperationRef;
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::{ActorId, DomainErrorCode, EventId, Id, OperationId, StoreId};
use crate::services::sessions::{Failure, Refusal, decided};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct GenerationJob {
    pub kind: String,
    pub generation: Id,
    pub delivery_id: Id,
    pub expected_help_revision: i64,
    pub registry_revision: i64,
    pub input_digest: String,
    pub actor: ActorId,
    pub actor_kind: String,
    pub store_id: StoreId,
    pub restore_epoch: i64,
    pub fences: Vec<AuthorityFence>,
}

impl HelpService {
    fn generate(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        input: &ops::GenerateCapabilityDocsInput,
    ) -> Result<Decided, Failure> {
        let head = tx.help().help_head()?;
        let registry = tx.packages().registry_revision()?;
        if head.revision != input.expected_help_revision.get() {
            return Err(StorageError::conflict(
                crate::protocol::RevisionDimension::Source,
                "help_catalog",
                Id::new("help").expect("owned ID"),
                input.expected_help_revision.clone(),
                crate::services::sessions::rev(head.revision),
            )
            .into());
        }
        if registry != input.registry_revision.get() {
            return Err(StorageError::conflict(
                crate::protocol::RevisionDimension::Registry,
                "capability_registry",
                Id::new("registry").expect("owned ID"),
                input.registry_revision.clone(),
                crate::services::sessions::rev(registry),
            )
            .into());
        }
        if let Some(id) = head.generation_operation_id.as_ref()
            && let Some(operation) = tx.operations().operation(id)?
            && !is_terminal(operation.state)
        {
            return Ok(Decided {
                decision: DomainDecision::NoChange {
                    result: OperationResult::HelpGenerateCapabilityDocs {
                        value: OperationRef {
                            operation_id: operation.id,
                            state: operation.state,
                        },
                    },
                },
                operation_id: None,
            });
        }
        let operation = OperationId(tx.new_id(IdKind::Operation));
        let queue = tx.new_id(IdKind::QueueEntry);
        let event = EventId(tx.new_id(IdKind::Event));
        let job = GenerationJob {
            kind: JOB_KIND.into(),
            generation: tx.new_id(IdKind::Generation),
            delivery_id: queue.clone(),
            expected_help_revision: head.revision,
            registry_revision: registry,
            input_digest: self.source_digest(registry),
            actor: ctx.principal().clone(),
            actor_kind: format!("{:?}", ctx.principal_kind()),
            store_id: ctx.store_id().clone(),
            restore_epoch: ctx.restore_epoch().get(),
            fences: command.authorization().fences().to_vec(),
        };
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: None,
            assignment_id: None,
            capability: "help.generate_capability_docs".into(),
            schema_version: 1,
            args_digest: command.fingerprint().as_str().into(),
            payload: serde_json::to_value(job)
                .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
        })?;
        tx.help()
            .set_help_generation_operation(head.revision, registry, &operation)?;
        tx.history().append(NewEvent {
            id: event.clone(),
            actor: ctx.principal().clone(),
            project_id: None,
            root_id: None,
            operation_id: Some(operation.clone()),
            aggregate_kind: "help".into(),
            aggregate_id: "catalog".into(),
            aggregate_revision: head.revision,
            kind: "HelpGenerationRequested".into(),
            payload: json!({"registryRevision":registry,"expectedHelpRevision":head.revision}),
        })?;
        let ready_at_ms = tx.now_ms();
        tx.queue().enqueue(NewQueueEntry {
            id: queue,
            consumer: EFFECT_CONSUMER.into(),
            event_id: event,
            root_id: None,
            operation_id: Some(operation.clone()),
            partition: Some("help-catalog".into()),
            priority: 0,
            ready_at_ms,
            payload: json!({"kind":JOB_KIND}),
        })?;
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: operation.clone(),
            },
            operation_id: Some(operation),
        })
    }
}

impl CommandHandler for HelpService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let result = match command.input() {
            OperationInput::Help(ops::Input::GenerateCapabilityDocs(input)) => {
                self.generate(ctx, command, tx, input)
            }
            _ => Err(Refusal::new(
                DomainErrorCode::UnsupportedCapability,
                "help_operation",
                None,
            )
            .into()),
        };
        match result {
            Ok(result) => Ok(result),
            Err(failure) => decided(tx, Err(failure)),
        }
    }
}
