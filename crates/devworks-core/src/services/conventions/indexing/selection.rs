//! Durable, bounded fanout from an actual embedding selection event. Scope narrowing never
//! changes the original actor, work/session lineage, or invents authority.
use super::{
    IndexService,
    commands::{self, EFFECT_CONSUMER, EnqueueIdentity, IndexJob, IndexTarget},
    publisher::{IndexOutcome, identity},
};
use crate::{
    context::{InvocationContext, PrincipalKind},
    ports::{AuthorityFence, storage::*},
    protocol::{
        ActorId, EventId, Id, OperationId, OperationState, ProjectId, StoreId, data::DataScope,
    },
    services::{
        approvals,
        conventions::{access, model},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
pub const SELECTION_KIND: &str = "ConventionSelectionFanoutV1";
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SelectionJob {
    kind: String,
    event_id: EventId,
    selection_revision: i64,
    source_context: Value,
    actor: ActorId,
    actor_kind: String,
    store_id: StoreId,
    restore_epoch: i64,
    fences: Vec<AuthorityFence>,
    delivery_id: Id,
    #[serde(default)]
    control_roots: Vec<(crate::protocol::RootId, i64)>,
    #[serde(default)]
    assignment_generation: Option<i64>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Progress {
    anchor_done: bool,
    after_ms: Option<i64>,
    after_id: Option<ProjectId>,
    scopes: u64,
    done: bool,
}
fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
fn broad(ctx: &InvocationContext) -> bool {
    ctx.principal_kind() == PrincipalKind::Human
        && ctx.project_id().is_none()
        && ctx.root_id().is_none()
        && ctx.assignment_id().is_none()
        && ctx.session_id().is_none()
        && ctx.job_run_id().is_none()
        && ctx.worktree().is_none()
        && ctx.capability_registration().is_none()
}
fn bound_scope(ctx: &InvocationContext) -> DataScope {
    ctx.project_id()
        .map_or(DataScope::Workspace, |project_id| DataScope::Project {
            project_id: project_id.clone(),
        })
}
fn lineage(tx: &mut dyn WriteTransaction, ctx: &InvocationContext) -> Result<(), StorageError> {
    if let Some(root) = ctx.root_id() {
        let row = tx
            .sessions()
            .work_root(root)?
            .ok_or(StorageError::ScopeDenied)?;
        if ctx.project_id() != Some(&row.project_id) {
            return Err(StorageError::ScopeDenied);
        }
    }
    if let Some(session) = ctx.session_id() {
        let row = tx
            .sessions()
            .session(session)?
            .ok_or(StorageError::ScopeDenied)?;
        if row.project_id.as_ref() != ctx.project_id() || row.root_id.as_ref() != ctx.root_id() {
            return Err(StorageError::ScopeDenied);
        }
    }
    if let Some(assignment) = ctx.assignment_id() {
        let row = tx
            .runs()
            .assignment(assignment)?
            .ok_or(StorageError::ScopeDenied)?;
        if Some(&row.root_id) != ctx.root_id() {
            return Err(StorageError::ScopeDenied);
        }
    }
    Ok(())
}
fn narrow(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    scope: &DataScope,
) -> Result<InvocationContext, StorageError> {
    lineage(tx, ctx)?;
    let child = if broad(ctx) {
        match scope {
            DataScope::Workspace => ctx.clone(),
            DataScope::Project { project_id } => {
                ctx.clone()
                    .with_lineage(Some(project_id.clone()), None, None)
            }
        }
    } else {
        if model::scope_storage(&bound_scope(ctx)) != model::scope_storage(scope)
            && !matches!(scope, DataScope::Workspace)
        {
            return Err(StorageError::ScopeDenied);
        }
        ctx.clone()
    };
    access::scope_in(tx, &child, Some(scope), true).map_err(|_| StorageError::ScopeDenied)?;
    if let DataScope::Project { project_id } = scope
        && tx
            .projects()
            .project(project_id)?
            .is_none_or(|p| p.lifecycle != "Active")
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(child)
}
fn event_context(
    tx: &mut dyn WriteTransaction,
    event: &EventId,
    revision: i64,
    actor: &ActorId,
    kind: PrincipalKind,
) -> Result<InvocationContext, StorageError> {
    let event = tx
        .history()
        .event(event)?
        .ok_or(StorageError::ScopeDenied)?;
    if event.actor != *actor
        || event.kind != "EmbeddingSelectionChanged"
        || event.aggregate_kind != "embedding_selection"
        || event.aggregate_revision != revision
        || event.payload["selectionRevision"] != revision
    {
        return Err(StorageError::ScopeDenied);
    }
    approvals::restore_source_context(tx.store(), actor, kind, &event.payload["sourceContext"])
}
pub(super) fn validate_child(
    tx: &mut dyn WriteTransaction,
    job: &IndexJob,
    context: &InvocationContext,
) -> Result<(), StorageError> {
    match (&job.selection_event, job.selection_revision) {
        (None, None) => Ok(()),
        (Some(event), Some(revision)) => {
            let original =
                event_context(tx, event, revision, &job.actor, context.principal_kind())?;
            let expected = narrow(tx, &original, &job.scope)?;
            if approvals::save_source_context(&expected) != job.source_context {
                return Err(StorageError::ScopeDenied);
            }
            Ok(())
        }
        _ => Err(StorageError::ScopeDenied),
    }
}
fn check(
    tx: &mut dyn WriteTransaction,
    operation: &OperationRecord,
    job: &SelectionJob,
) -> Result<InvocationContext, StorageError> {
    if tx.store().store_id != job.store_id
        || tx.store().restore_epoch != job.restore_epoch
        || !crate::authority::fences_hold(tx, &job.fences)?
    {
        return Err(StorageError::ScopeDenied);
    }
    let kind = tx
        .actors()
        .actor_kind(&job.actor)?
        .ok_or(StorageError::ScopeDenied)?;
    if kind.as_str() != job.actor_kind {
        return Err(StorageError::ScopeDenied);
    }
    let context = event_context(tx, &job.event_id, job.selection_revision, &job.actor, kind)?;
    if approvals::save_source_context(&context) != job.source_context
        || tx.router().embedding_selection()?.revision != job.selection_revision
    {
        return Err(StorageError::ScopeDenied);
    }
    lineage(tx, &context)?;
    crate::services::conventions::control::check(
        tx,
        &context,
        &operation.id,
        &job.control_roots,
        job.assignment_generation,
    )?;
    let actual = tx
        .operations()
        .operation(&operation.id)?
        .ok_or_else(|| invalid("selection fanout disappeared"))?;
    if actual.payload != operation.payload
        || actual.latest_attempt().map(|a| a.fence())
            != operation.latest_attempt().map(|a| a.fence())
        || !matches!(
            actual.state,
            OperationState::Dispatching | OperationState::OutcomeUnknown
        )
    {
        return Err(StorageError::LeaseLost {
            kind: "convention_selection",
            id: operation.id.0.as_str().into(),
        });
    }
    let entry = tx
        .queue()
        .entry(&job.delivery_id)?
        .ok_or_else(|| invalid("selection delivery missing"))?;
    if entry.consumer != EFFECT_CONSUMER || entry.operation_id.as_ref() != Some(&operation.id) {
        return Err(StorageError::ScopeDenied);
    }
    if entry.cancel_requested {
        return Err(StorageError::Cancelled);
    }
    Ok(context)
}
impl IndexService {
    pub fn record_selection_change(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        event_id: &EventId,
        selected_revision: i64,
    ) -> Result<(), StorageError> {
        if self.is_stopped() {
            return Err(StorageError::Cancelled);
        }
        if &tx.store().store_id != ctx.store_id()
            || tx.store().restore_epoch != ctx.restore_epoch().get()
            || tx.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
            || tx.router().embedding_selection()?.revision != selected_revision
        {
            return Err(StorageError::ScopeDenied);
        }
        let original = event_context(
            tx,
            event_id,
            selected_revision,
            ctx.principal(),
            ctx.principal_kind(),
        )?;
        if approvals::save_source_context(&original) != approvals::save_source_context(ctx) {
            return Err(StorageError::ScopeDenied);
        }
        narrow(tx, ctx, &bound_scope(ctx))?;
        let key = format!("{}:{selected_revision}", event_id.0.as_str());
        let operation = OperationId(identity("csel_fanout_", &key));
        let delivery = identity("csel_fanout_delivery_", &key);
        if let Some(existing) = tx.operations().operation(&operation)? {
            if existing.payload["eventId"] != json!(event_id)
                || existing.payload["sourceContext"] != approvals::save_source_context(ctx)
            {
                return Err(StorageError::ScopeDenied);
            }
            return Ok(());
        }
        let controls = crate::services::conventions::control::capture(tx, ctx)?;
        let job = SelectionJob {
            kind: SELECTION_KIND.into(),
            event_id: event_id.clone(),
            selection_revision: selected_revision,
            source_context: approvals::save_source_context(ctx),
            actor: ctx.principal().clone(),
            actor_kind: ctx.principal_kind().as_str().to_string(),
            store_id: ctx.store_id().clone(),
            restore_epoch: ctx.restore_epoch().get(),
            fences: commands::source_fences(tx, ctx)?,
            delivery_id: delivery.clone(),
            control_roots: controls.roots,
            assignment_generation: controls.assignment_generation,
        };
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: ctx.root_id().cloned(),
            assignment_id: ctx.assignment_id().cloned(),
            capability: "conventions.index".into(),
            schema_version: 1,
            args_digest: crate::fingerprint::sha256_hex(key.as_bytes()),
            payload: serde_json::to_value(job).map_err(|_| invalid("selection fanout payload"))?,
        })?;
        let now = tx.now_ms();
        tx.queue().enqueue(NewQueueEntry {
            id: delivery,
            consumer: EFFECT_CONSUMER.into(),
            event_id: event_id.clone(),
            root_id: ctx.root_id().cloned(),
            operation_id: Some(operation),
            partition: None,
            priority: 0,
            ready_at_ms: now,
            payload: json!({"kind":SELECTION_KIND}),
        })?;
        Ok(())
    }
    pub(super) fn run_selection(
        &self,
        operation: &OperationRecord,
        cancel: &Cancellation,
        deadline: Option<i64>,
    ) -> Result<IndexOutcome, StorageError> {
        let actual = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            read.operations().operation(&operation.id)
        })?
        .ok_or_else(|| invalid("selection operation missing"))?;
        if let Some(value) = actual
            .attempts
            .iter()
            .rev()
            .flat_map(|a| a.observations.iter().rev())
            .find_map(|v| v.get("conventionPublication"))
        {
            return Ok(IndexOutcome::Completed(value.clone()));
        }
        let job: SelectionJob = serde_json::from_value(actual.payload.clone())
            .map_err(|_| invalid("invalid selection fanout"))?;
        if job.kind != SELECTION_KIND || actual.capability != "conventions.index" {
            return Err(invalid("unsupported selection fanout"));
        }
        let mut progress = actual
            .attempts
            .iter()
            .rev()
            .flat_map(|a| a.observations.iter().rev())
            .find_map(|v| v.get("selectionFanoutPage"))
            .map(|v| {
                serde_json::from_value::<Progress>(v.clone())
                    .map_err(|_| invalid("invalid selection page"))
            })
            .transpose()?
            .unwrap_or_default();
        loop {
            if self.is_stopped()
                || cancel.is_cancelled()
                || deadline.is_some_and(|d| self.clock.now_ms().get() >= d)
            {
                return Err(StorageError::Cancelled);
            }
            let (op, source) = (operation.clone(), job.clone());
            let context = self.write_options(
                WriteOptions {
                    cancellation: Some(cancel.clone()),
                    deadline_ms: deadline,
                    ..Default::default()
                },
                move |tx| check(tx, &op, &source),
            )?;
            let after = match (progress.after_ms, progress.after_id.clone()) {
                (Some(created_at_ms), Some(id)) => Some(ProjectPosition { created_at_ms, id }),
                (None, None) => None,
                _ => return Err(invalid("invalid selection project cursor")),
            };
            let projects = if broad(&context) {
                read_snapshot(
                    &*self.store,
                    &ReadOptions {
                        cancellation: Some(cancel.clone()),
                        deadline_ms: deadline,
                    },
                    |read| {
                        read.projects().projects(
                            &ProjectFilter {
                                only: None,
                                lifecycles: vec!["Active".into()],
                            },
                            after.as_ref(),
                            32,
                        )
                    },
                )?
            } else {
                Vec::new()
            };
            let mut scopes = if progress.anchor_done {
                Vec::new()
            } else {
                if context.principal_kind() == PrincipalKind::Human
                    && context.project_id().is_some()
                {
                    vec![DataScope::Workspace, bound_scope(&context)]
                } else {
                    vec![bound_scope(&context)]
                }
            };
            scopes.extend(projects.iter().map(|p| DataScope::Project {
                project_id: p.id.clone(),
            }));
            let mut next = progress.clone();
            next.anchor_done = true;
            if let Some(last) = projects.last() {
                next.after_id = Some(last.id.clone());
                next.after_ms = Some(last.created_at_ms);
            }
            next.done = !broad(&context) || projects.len() < 32;
            let (op, source, saved, available) = (
                operation.clone(),
                job.clone(),
                next.clone(),
                self.native_available(),
            );
            let committed=self.write_options(WriteOptions{cancellation:Some(cancel.clone()),deadline_ms:deadline,..Default::default()},move|tx|{
    let mut saved=saved;
    let context=check(tx,&op,&source)?;
    for scope in scopes {
     if let DataScope::Project{project_id}=&scope && tx.projects().project(project_id)?.is_none_or(|p|p.lifecycle!="Active"){continue;}
     let child=narrow(tx,&context,&scope)?;
     let scoped=access::scope_in(tx,&child,Some(&scope),true).map_err(|_|StorageError::ScopeDenied)?;
     let revision=tx.conventions().convention_scope_revision(&scoped.exact)?;
     let fingerprint=crate::fingerprint::sha256_hex(format!("{}:{}:{}",source.event_id.0.as_str(),source.selection_revision,serde_json::to_string(&scope).expect("typed scope")).as_bytes());
     let fences=commands::source_fences(tx,&child)?;
     IndexService::enqueue_with(tx,&child,(IndexTarget::All{scope:scope.clone(),scope_revision:revision},scope),fences,EnqueueIdentity{fingerprint:&fingerprint,selection:Some((&source.event_id,source.selection_revision))},available)?;
     saved.scopes=saved.scopes.checked_add(1).ok_or_else(||invalid("selection scope count overflow"))?;
    }
    let attempt=op.latest_attempt().ok_or_else(||invalid("selection attempt missing"))?;tx.operations().observe(&op.id,attempt.attempt_no,json!({"selectionFanoutPage":saved}))?;
    if saved.done{tx.operations().observe(&op.id,attempt.attempt_no,json!({"conventionPublication":{"operationId":op.id,"scopes":saved.scopes}}))?;}Ok(saved)
   })?;
            if committed.done {
                return Ok(IndexOutcome::Completed(
                    json!({"operationId":operation.id,"scopes":committed.scopes}),
                ));
            }
            progress = committed;
        }
    }
}
