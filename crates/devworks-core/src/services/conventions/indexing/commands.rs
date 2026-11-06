//! Durable index requests are captured with their original owner, authority and exact source.
use super::IndexService;
use crate::{
    context::{InvocationContext, PrincipalKind},
    gateway::AdmittedCommand,
    operations::conventions as ops,
    ports::{AuthorityFence, Decided, IdKind, storage::*},
    protocol::{
        ActorId, DomainErrorCode, EventId, Id, OperationId, StoreId, data::DataScope,
        results::DomainDecision,
    },
    services::{
        approvals,
        conventions::{access, model},
        sessions::{Failure, Refusal},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
pub const EFFECT_CONSUMER: &str = "convention_indexing";
pub const JOB_KIND: &str = "ConventionIndexV1";
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum IndexTarget {
    One {
        convention_id: Id,
        revision: i64,
    },
    All {
        scope: DataScope,
        scope_revision: i64,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IndexJob {
    pub kind: String,
    pub target: IndexTarget,
    pub scope: DataScope,
    pub source_context: Value,
    pub actor: ActorId,
    pub actor_kind: String,
    pub store_id: StoreId,
    pub restore_epoch: i64,
    pub fences: Vec<AuthorityFence>,
    pub delivery_id: Id,
    pub configuration_source_digest: String,
    pub embedding_space: Option<String>,
    pub selection_event: Option<EventId>,
    pub selection_revision: Option<i64>,
    #[serde(default)]
    pub control_roots: Vec<(crate::protocol::RootId, i64)>,
    #[serde(default)]
    pub assignment_generation: Option<i64>,
}
impl IndexService {
    pub fn decide_index(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        input: &ops::IndexInput,
    ) -> Result<Decided, Failure> {
        if self.is_stopped() {
            return Err(StorageError::Cancelled.into());
        }
        if input.resource_id.is_some() {
            return Err(Refusal::new(
                DomainErrorCode::InvalidArgument,
                "unsupported_resource_index",
                input.resource_id.clone(),
            )
            .into());
        }
        let (target, scope) = if let Some(id) = &input.convention_id {
            let scoped = access::scope_in(tx, ctx, input.scope.as_ref(), true)?;
            let row = tx
                .conventions()
                .convention(id, None, &scoped.readable)?
                .ok_or_else(|| {
                    Refusal::new(DomainErrorCode::NotFound, "convention", Some(id.clone()))
                })?;
            access::writable(tx, ctx, &row.scope)?;
            if row.lifecycle != "Active" {
                return Err(Refusal::new(
                    DomainErrorCode::Conflict,
                    "convention_lifecycle",
                    Some(id.clone()),
                )
                .into());
            }
            if row.revision != input.expected_revision.get() {
                return Err(StorageError::conflict(
                    crate::protocol::RevisionDimension::Source,
                    "convention",
                    id.clone(),
                    input.expected_revision.clone(),
                    crate::services::sessions::rev(row.revision),
                )
                .into());
            }
            (
                IndexTarget::One {
                    convention_id: id.clone(),
                    revision: row.revision,
                },
                model::scope(&row.scope),
            )
        } else {
            let requested = input.all_scope.as_ref().ok_or_else(|| {
                Refusal::new(DomainErrorCode::InvalidArgument, "convention_target", None)
            })?;
            let scoped = access::scope_in(tx, ctx, Some(requested), true)?;
            let revision = tx.conventions().convention_scope_revision(&scoped.exact)?;
            if revision != input.expected_revision.get() {
                return Err(StorageError::conflict(
                    crate::protocol::RevisionDimension::Source,
                    "convention_scope",
                    match &scoped.exact {
                        KnowledgeScope::Workspace => Id::new("workspace").expect("staticID"),
                        KnowledgeScope::Project(project) => project.0.clone(),
                    },
                    input.expected_revision.clone(),
                    crate::services::sessions::rev(revision),
                )
                .into());
            }
            (
                IndexTarget::All {
                    scope: requested.clone(),
                    scope_revision: revision,
                },
                requested.clone(),
            )
        };
        let op = self.enqueue(
            tx,
            ctx,
            target,
            scope,
            command.authorization().fences().to_vec(),
            command.fingerprint().as_str(),
        )?;
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: op.clone(),
            },
            operation_id: Some(op),
        })
    }
    fn enqueue(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        target: IndexTarget,
        scope: DataScope,
        fences: Vec<AuthorityFence>,
        fingerprint: &str,
    ) -> Result<OperationId, StorageError> {
        Self::enqueue_with(
            tx,
            ctx,
            (target, scope),
            fences,
            EnqueueIdentity {
                fingerprint,
                selection: None,
            },
            self.native_available(),
        )
    }
    pub(super) fn enqueue_with(
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        target_scope: (IndexTarget, DataScope),
        fences: Vec<AuthorityFence>,
        identity: EnqueueIdentity<'_>,
        available: bool,
    ) -> Result<OperationId, StorageError> {
        let (target, scope) = target_scope;
        let key = identity.selection.map(|(event, revision)| {
            format!(
                "{}:{revision}:{}",
                event.0.as_str(),
                serde_json::to_string(&scope).expect("typed scope")
            )
        });
        let operation = OperationId(key.as_ref().map_or_else(
            || tx.new_id(IdKind::Operation),
            |key| super::publisher::identity("csel_index_", key),
        ));
        if let Some(existing) = tx.operations().operation(&operation)? {
            let prior: IndexJob =
                serde_json::from_value(existing.payload).map_err(|_| StorageError::ScopeDenied)?;
            if existing.capability != "conventions.index"
                || existing.args_digest != identity.fingerprint
                || model::scope_storage(&prior.scope) != model::scope_storage(&scope)
                || prior.source_context != approvals::save_source_context(ctx)
                || prior.selection_event.as_ref() != identity.selection.map(|(event, _)| event)
            {
                return Err(StorageError::ScopeDenied);
            }
            return Ok(operation);
        }
        let delivery = key.as_ref().map_or_else(
            || tx.new_id(IdKind::QueueEntry),
            |key| super::publisher::identity("csel_delivery_", key),
        );
        let event = EventId(key.as_ref().map_or_else(
            || tx.new_id(IdKind::Event),
            |key| super::publisher::identity("csel_event_", key),
        ));
        let configuration = super::configuration::resolved_configuration(
            &crate::services::router::embedding_spaces::resolve_selected_in(tx, ctx)?,
            available,
        );
        if let Some(space) = &configuration.space {
            tx.conventions().put_convention_embedding_space(space)?;
        }
        let controls = crate::services::conventions::control::capture(tx, ctx)?;
        let job = IndexJob {
            kind: JOB_KIND.into(),
            target,
            scope: scope.clone(),
            source_context: approvals::save_source_context(ctx),
            actor: ctx.principal().clone(),
            actor_kind: format!("{:?}", ctx.principal_kind()),
            store_id: ctx.store_id().clone(),
            restore_epoch: ctx.restore_epoch().get(),
            fences,
            delivery_id: delivery.clone(),
            configuration_source_digest: configuration.source_digest,
            embedding_space: configuration.space.map(|space| space.id),
            selection_event: identity.selection.map(|(event, _)| event.clone()),
            selection_revision: identity.selection.map(|(_, revision)| revision),
            control_roots: controls.roots,
            assignment_generation: controls.assignment_generation,
        };
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: ctx.root_id().cloned(),
            assignment_id: ctx.assignment_id().cloned(),
            capability: "conventions.index".into(),
            schema_version: 1,
            args_digest: identity.fingerprint.into(),
            payload: serde_json::to_value(&job)
                .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
        })?;
        if let IndexTarget::One {
            convention_id,
            revision,
        } = &job.target
        {
            let scoped = access::scope_in(tx, ctx, Some(&job.scope), true)
                .map_err(|_| StorageError::ScopeDenied)?;
            let owner = tx
                .conventions()
                .convention(convention_id, Some(*revision), &scoped.readable)?
                .ok_or(StorageError::ScopeDenied)?;
            if owner.lifecycle == "Active" {
                let header = super::publisher::header(&operation, &job, &owner, tx.now_ms())?;
                tx.conventions().stage_convention_index(&header)?;
            }
        }
        let project = match scope {
            DataScope::Workspace => None,
            DataScope::Project { project_id } => Some(project_id),
        };
        tx.history().append(NewEvent {
            id: event.clone(),
            actor: ctx.principal().clone(),
            project_id: project,
            root_id: ctx.root_id().cloned(),
            operation_id: Some(operation.clone()),
            aggregate_kind: "convention_index".into(),
            aggregate_id: operation.0.as_str().into(),
            aggregate_revision: 1,
            kind: "ConventionIndexRequested".into(),
            payload: json!({"target":job.target}),
        })?;
        let ready = tx.now_ms();
        tx.queue().enqueue(NewQueueEntry {
            id: delivery,
            consumer: EFFECT_CONSUMER.into(),
            event_id: event,
            root_id: ctx.root_id().cloned(),
            operation_id: Some(operation.clone()),
            partition: None,
            priority: 0,
            ready_at_ms: ready,
            payload: json!({"kind":JOB_KIND}),
        })?;
        Ok(operation)
    }
    /// Called only from the actual authored-body owner in its editing transaction. A source
    /// change remains a durable queue fact even when no worker is awake or runtime is closing.
    pub fn record_changed(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        row: &StoredConvention,
    ) -> Result<OperationId, StorageError> {
        let fences = source_fences(tx, ctx)?;
        let fingerprint = crate::fingerprint::sha256_hex(
            format!(
                "{}:{}:{}",
                row.id.as_str(),
                row.revision,
                row.content_ref
                    .as_ref()
                    .map_or("", |r| r["sha256"].as_str().unwrap_or(""))
            )
            .as_bytes(),
        );
        self.enqueue(
            tx,
            ctx,
            IndexTarget::One {
                convention_id: row.id.clone(),
                revision: row.revision,
            },
            model::scope(&row.scope),
            fences,
            &fingerprint,
        )
    }
}

pub(super) struct EnqueueIdentity<'a> {
    pub fingerprint: &'a str,
    pub selection: Option<(&'a EventId, i64)>,
}
pub(super) fn source_fences(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
) -> Result<Vec<AuthorityFence>, StorageError> {
    let mut fences = Vec::new();
    if !matches!(
        ctx.principal_kind(),
        PrincipalKind::Human | PrincipalKind::Host
    ) {
        let binding = tx
            .authority()
            .principal(ctx.principal())?
            .ok_or(StorageError::ScopeDenied)?;
        if !binding.active {
            return Err(StorageError::ScopeDenied);
        }
        fences.push(AuthorityFence::Principal {
            actor: ctx.principal().clone(),
            revision: binding.revision,
        });
    }
    let policy = crate::authority::policy::resolve_policy(tx.configuration(), ctx.project_id())?;
    fences.push(AuthorityFence::Policy {
        project_id: ctx.project_id().cloned(),
        digest: policy.digest(),
    });
    Ok(fences)
}
