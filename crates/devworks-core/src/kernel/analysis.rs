//! Durable repository-analysis claim and exact graph evidence used by the authority fence.
use crate::context::{BoundWorktree, InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::kernel::{
    failure::{Failure, Refusal, rev},
    graph_view as graph, index_config,
};
use crate::ports::{Authorization, storage::*};
use crate::protocol::{
    Counter, DomainErrorCode, Id, OperationId, OperationState, StoreId, data::DataScope,
    records::AnalysisViewBinding,
};
use graph::ViewLayers;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const CONSUMER: &str = "RepositoryAnalysis";
pub(crate) const REPORT_ALGORITHM_REVISION: u32 = 1;

pub(crate) fn unavailable() -> Failure {
    Refusal::new(DomainErrorCode::ViewUnavailable, "analysis_view", None).into()
}

pub(crate) fn configuration_revision(
    read: &dyn IndexRead,
    scope: &DataScope,
) -> Result<i64, Failure> {
    let project = match scope {
        DataScope::Project { project_id } => Some(project_id),
        DataScope::Workspace => None,
    };
    Ok(index_config::section_config(read, &index_config::section_key(project))?.revision)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct Work {
    pub run_id: Id,
    pub delivery_id: Id,
    pub authority: AnalysisOwner,
}

pub(crate) struct Claim {
    pub work: Work,
    pub context: InvocationContext,
    pub operation_id: OperationId,
    pub(crate) operation_revision: i64,
    pub(crate) attempt: AttemptFence,
    pub(crate) queue_fence: QueueFence,
    pub(crate) cancellation_epoch: i64,
}

impl Work {
    pub(crate) fn from_operation(operation: &OperationRecord) -> Result<Self, Failure> {
        serde_json::from_value(operation.payload.clone()).map_err(|_| {
            StorageError::InvalidRecord("invalid repository analysis operation".into()).into()
        })
    }

    pub(crate) fn context(&self, store: &StoreId) -> Result<InvocationContext, Failure> {
        let owner = &self.authority;
        let kind = PrincipalKind::parse(&owner.kind).ok_or(StorageError::ScopeDenied)?;
        let epoch = Counter::new(owner.restore_epoch).map_err(|_| StorageError::ScopeDenied)?;
        let mut ctx = InvocationContext::new(
            owner.actor.clone(),
            kind,
            owner.invocation.clone(),
            owner.ingress.clone(),
            store.clone(),
            epoch,
            rev(owner.policy_revision),
        )
        .with_lineage(
            owner.project.clone(),
            owner.root.clone(),
            owner.assignment.clone(),
        );
        if let Some(session) = &owner.session {
            ctx = ctx.with_session(session.clone());
        }
        if let Some((id, epoch)) = &owner.activation {
            ctx = ctx.with_activation(id.clone(), *epoch);
        }
        if let Some(tree) = &owner.worktree {
            ctx = ctx.bind_worktree(BoundWorktree::resolved(
                tree.id.clone(),
                tree.repository.clone(),
                tree.work.clone(),
                tree.ownership_revision,
            ));
        }
        if self.authority.namespace != graph::owner(&ctx) {
            return Err(StorageError::ScopeDenied.into());
        }
        Ok(ctx)
    }

    pub(crate) fn authorize(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
    ) -> Result<(), Failure> {
        let gate = Authorization::direct().fenced_by(self.authority.fences.clone());
        if read.stamp().restore_epoch != ctx.restore_epoch().get()
            || read.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
            || !crate::authority::authorization_holds(read, &gate)?
        {
            return Err(StorageError::ScopeDenied.into());
        }
        Ok(())
    }
}

impl Claim {
    pub(crate) fn load(
        read: &dyn ReadSnapshot,
        operation: &OperationRecord,
    ) -> Result<Self, Failure> {
        let work = Work::from_operation(operation)?;
        let ctx = work.context(&read.stamp().store_id)?;
        work.authorize(read, &ctx)?;
        let current = read
            .operations()
            .operation(&operation.id)?
            .ok_or(StorageError::ScopeDenied)?;
        if current.state != OperationState::Dispatching || current.revision != operation.revision {
            return Err(StorageError::ScopeDenied.into());
        }
        let attempt = current
            .latest_attempt()
            .ok_or(StorageError::ScopeDenied)?
            .fence();
        let delivery = read
            .queue()
            .entry(&work.delivery_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if delivery.consumer != CONSUMER
            || delivery.operation_id.as_ref() != Some(&operation.id)
            || delivery.state != QueueState::Claimed
            || delivery.cancel_requested
            || delivery.claim_boot_id != read.stamp().boot_id
            || delivery
                .lease_until_ms
                .is_none_or(|expiry| expiry <= read.stamp().now_ms)
        {
            return Err(StorageError::Cancelled.into());
        }
        let queue_fence = delivery.fence().ok_or(StorageError::ScopeDenied)?;
        Ok(Self {
            work,
            context: ctx,
            operation_id: operation.id.clone(),
            operation_revision: operation.revision,
            attempt,
            queue_fence,
            cancellation_epoch: delivery.cancellation_epoch,
        })
    }

    pub(crate) fn current_run(
        &self,
        read: &dyn ReadSnapshot,
    ) -> Result<StoredAnalysisRun, Failure> {
        self.work.authorize(read, &self.context)?;
        let entry = read
            .queue()
            .entry(&self.work.delivery_id)?
            .ok_or(StorageError::Cancelled)?;
        if !self.delivery_current(&entry, read.stamp().now_ms, read.stamp().boot_id.as_ref()) {
            return Err(StorageError::Cancelled.into());
        }
        let run = read
            .analysis()
            .analysis_run(&self.work.run_id)?
            .filter(|run| run.active_operation == self.operation_id)
            .ok_or_else(unavailable)?;
        Ok(run)
    }

    pub(crate) fn delivery_current(&self, entry: &QueueEntry, now: i64, boot: Option<&Id>) -> bool {
        entry.consumer == CONSUMER
            && entry.operation_id.as_ref() == Some(&self.operation_id)
            && entry.state == QueueState::Claimed
            && entry.fence().as_ref() == Some(&self.queue_fence)
            && entry.cancellation_epoch == self.cancellation_epoch
            && !entry.cancel_requested
            && entry.lease_until_ms.is_some_and(|expiry| expiry > now)
            && entry.claim_boot_id.as_ref() == boot
    }
}
pub(crate) fn check_worktree_layers(
    layers: &[ViewLayers],
    owned: Option<&StoredGitWorktree>,
) -> Result<(), Failure> {
    for layer in layers {
        let snapshot = graph::snapshot(&layer.view).map_err(hide_view_identity)?;
        if let Some(worktree) = &snapshot.worktree_id
            && owned.is_none_or(|tree| {
                &tree.id != worktree || tree.repository_id != snapshot.repository_id
            })
        {
            return Err(unavailable());
        }
    }
    Ok(())
}

pub(crate) fn validate_worktree_in(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    views: &[AnalysisViewBinding],
) -> Result<(), Failure> {
    for view in views {
        let Some(id) = &view.snapshot.worktree_id else {
            continue;
        };
        let bound = ctx
            .worktree()
            .filter(|bound| bound.id() == id && bound.repository_id() == &view.repository_id)
            .ok_or_else(unavailable)?;
        let tree = tx.git().git_worktree(id)?.ok_or_else(unavailable)?;
        let work = tx.work().work(bound.work_id())?.ok_or_else(unavailable)?;
        if tree.removed
            || &tree.owner != ctx.principal()
            || tree.repository_id != view.repository_id
            || tree.work_id != *bound.work_id()
            || tree.ownership_revision != bound.ownership_revision()
            || ctx.project_id() != Some(&work.project_id)
            || !ctx.root_id().is_some_and(|root| {
                root.0 == work.id || work.parent_feature_id.as_ref() == Some(&root.0)
            })
            || !tx
                .work()
                .work_repositories(&work.id)?
                .contains(&view.repository_id)
            || !tx
                .projects()
                .members(&work.project_id)?
                .contains(&view.repository_id)
        {
            return Err(unavailable());
        }
    }
    Ok(())
}

pub(crate) fn digest(value: &serde_json::Value) -> String {
    sha256_hex(value.to_string().as_bytes())
}

/// One inaccessible member cannot reveal the presence or identity of other
/// constituents of a saved view set.
pub(crate) fn hide_view_identity(error: Failure) -> Failure {
    match error {
        Failure::Refused(r)
            if matches!(
                r.code,
                DomainErrorCode::NotFound
                    | DomainErrorCode::PermissionDenied
                    | DomainErrorCode::ViewUnavailable
            ) =>
        {
            unavailable()
        }
        other => other,
    }
}

pub(crate) struct PinnedSelection {
    pub(crate) layers: Vec<ViewLayers>,
    pub(crate) witnesses: Vec<GraphViewWitness>,
    pub(crate) unavailable: Vec<GraphUnavailableRepository>,
}

pub(crate) fn resolve_pinned(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    data_scope: &DataScope,
    pin_id: &Id,
    now: i64,
) -> Result<PinnedSelection, Failure> {
    if matches!(data_scope, DataScope::Workspace) && ctx.project_id().is_some() {
        return Err(
            Refusal::new(DomainErrorCode::PermissionDenied, "workspace_scope", None).into(),
        );
    }
    let owned = graph::owned(read, ctx)
        .map_err(hide_view_identity)?
        .filter(|tree| &tree.owner == ctx.principal());
    let pin = read.graph().graph_pin(pin_id)?.ok_or_else(unavailable)?;
    graph::check_pin(&pin, ctx, data_scope, now).map_err(hide_view_identity)?;
    let mut layers = Vec::with_capacity(pin.scope.views.len());
    for binding in &pin.scope.views {
        layers.push(
            graph::retained(read, ctx, data_scope, &pin, binding, now)
                .map_err(hide_view_identity)?,
        );
    }
    for missing in &pin.unavailable_repositories {
        graph::authorize(read, ctx, data_scope, &missing.repository_id)
            .map_err(hide_view_identity)?;
    }
    check_worktree_layers(&layers, owned.as_ref())?;

    Ok(PinnedSelection {
        layers,
        witnesses: pin.view_witnesses,
        unavailable: pin.unavailable_repositories,
    })
}

pub(crate) fn normalize_selection(
    layers: &mut [ViewLayers],
    witnesses: &mut [GraphViewWitness],
    unavailable: &mut [GraphUnavailableRepository],
) {
    layers.sort_by(|a, b| {
        a.view
            .repository_id
            .as_str()
            .cmp(b.view.repository_id.as_str())
    });
    witnesses.sort_by(|a, b| a.view_id.as_str().cmp(b.view_id.as_str()));
    unavailable.sort_by(|a, b| a.repository_id.as_str().cmp(b.repository_id.as_str()));
}

pub(crate) fn checked_views(layers: &[ViewLayers]) -> Result<Vec<AnalysisViewBinding>, Failure> {
    layers
        .iter()
        .map(|layer| {
            let view = &layer.view;
            let snapshot = graph::snapshot(view).map_err(hide_view_identity)?;
            snapshot.validate().map_err(|_| unavailable())?;
            Ok(AnalysisViewBinding {
                repository_id: view.repository_id.clone(),
                view_id: view.view_id.clone(),
                view_digest: graph::text(&view.view_digest)?,
                snapshot,
            })
        })
        .collect::<Result<Vec<_>, Failure>>()
}

pub(crate) fn binding_key(
    scope: &DataScope,
    views: &[AnalysisViewBinding],
    unavailable: &[GraphUnavailableRepository],
    configuration_revision: i64,
) -> String {
    digest(&json!({
        "algorithm": REPORT_ALGORITHM_REVISION,
        "scope": scope,
        "views": views,
        "unavailable": unavailable,
        "configuration": configuration_revision,
    }))
}
/// The paid request belongs to this still-dispatching analysis operation.
fn dispatch_binding(
    operation: Option<&OperationRecord>,
    run: Option<&StoredAnalysisRun>,
    request: &crate::protocol::Id,
) -> bool {
    let (Some(operation), Some(run)) = (operation, run) else {
        return false;
    };
    matches!(
        operation.capability.as_str(),
        "analysis.reanalyze" | "analysis.synthesize"
    ) && operation.state == OperationState::Dispatching
        && run.active_operation == operation.id
        && run
            .phases
            .iter()
            .filter(|phase| {
                matches!(
                    phase.phase,
                    AnalysisPhase::Projects | AnalysisPhase::Synthesis
                )
            })
            .any(|phase| {
                phase.subjects.iter().any(|subject| {
                    subject.request == *request && subject.state == OperationState::Dispatching
                })
            })
}

pub fn dispatch_binding_in(
    tx: &mut dyn WriteTransaction,
    operation: &crate::protocol::OperationId,
    request: &crate::protocol::Id,
) -> Result<bool, StorageError> {
    let Some(op) = tx.operations().operation(operation)? else {
        return Ok(false);
    };
    let Ok(work) = Work::from_operation(&op) else {
        return Ok(false);
    };
    let Some(run) = tx.analysis().analysis_run(&work.run_id)? else {
        return Ok(false);
    };
    if !dispatch_binding(Some(&op), Some(&run), request)
        || run.owner.restore_epoch != tx.store().restore_epoch
        || tx.actors().actor_kind(&run.owner.actor)?
            != crate::context::PrincipalKind::parse(&run.owner.kind)
        || configuration_revision(tx.indexing(), &run.intent.scope)
            .map_err(Failure::into_storage)?
            != run.intent.configuration_revision
    {
        return Ok(false);
    }
    let now = tx.now_ms();
    let Some(delivery) = tx.queue().entry(&work.delivery_id)? else {
        return Ok(false);
    };
    if delivery.consumer != CONSUMER
        || delivery.operation_id.as_ref() != Some(operation)
        || delivery.state != QueueState::Claimed
        || delivery.cancel_requested
        || delivery.claim_boot_id != tx.store().boot_id
        || delivery.lease_until_ms.is_none_or(|expiry| expiry <= now)
    {
        return Ok(false);
    }
    let Some(pin) = tx.graph().graph_pin(&run.pin_id)? else {
        return Ok(false);
    };
    if pin.owner_namespace != run.owner.namespace
        || pin.expires_at_ms <= now
        || pin.restore_epoch != run.owner.restore_epoch
        || pin.scope_digest != graph::scope_digest(&run.intent.scope)
    {
        return Ok(false);
    }
    let context = work
        .context(&tx.store().store_id)
        .map_err(Failure::into_storage)?;
    match validate_worktree_in(tx, &context, &run.intent.views) {
        Ok(()) => {}
        Err(Failure::Refused(_)) => return Ok(false),
        Err(Failure::Storage(error)) => return Err(error),
    }
    let members = match &run.intent.scope {
        crate::protocol::data::DataScope::Workspace => None,
        crate::protocol::data::DataScope::Project { project_id } => {
            if tx
                .projects()
                .project(project_id)?
                .is_none_or(|project| project.lifecycle != ACTIVE)
            {
                return Ok(false);
            }
            Some(tx.projects().members(project_id)?)
        }
    };
    for repository in run
        .intent
        .views
        .iter()
        .map(|view| &view.repository_id)
        .chain(
            run.intent
                .unavailable
                .iter()
                .map(|view| &view.repository_id),
        )
    {
        if tx
            .projects()
            .repository(repository)?
            .is_none_or(|row| row.lifecycle == "Deleted")
            || members
                .as_ref()
                .is_some_and(|members| !members.contains(repository))
        {
            return Ok(false);
        }
    }
    for view in &run.intent.views {
        if tx
            .indexing()
            .view(&view.view_id)?
            .is_none_or(|row| row.view_digest != view.view_digest.as_str())
            || !pin.scope.views.iter().any(|bound| {
                bound.view_id == view.view_id
                    && bound.repository_id == view.repository_id
                    && bound.view_digest == view.view_digest.as_str()
            })
        {
            return Ok(false);
        }
    }
    Ok(!run
        .intent
        .witnesses
        .iter()
        .any(|witness| witness.expires_at_ms.is_some_and(|expiry| expiry <= now)))
}

pub fn dispatch_binding_snapshot(
    read: &dyn ReadSnapshot,
    operation: &crate::protocol::OperationId,
    request: &crate::protocol::Id,
) -> Result<bool, StorageError> {
    let Some(op) = read.operations().operation(operation)? else {
        return Ok(false);
    };
    let valid = (|| {
        let claim = Claim::load(read, &op)?;
        let run = claim.current_run(read)?;
        let mut selected = resolve_pinned(
            read,
            &claim.context,
            &run.intent.scope,
            &run.pin_id,
            read.stamp().now_ms,
        )?;
        normalize_selection(
            &mut selected.layers,
            &mut selected.witnesses,
            &mut selected.unavailable,
        );
        let views = checked_views(&selected.layers)?;
        let configuration = configuration_revision(read.indexing(), &run.intent.scope)?;
        if binding_key(
            &run.intent.scope,
            &views,
            &selected.unavailable,
            configuration,
        ) != run.binding_key
        {
            return Err(unavailable());
        }
        Ok(run)
    })();
    match valid {
        Ok(run) => Ok(dispatch_binding(Some(&op), Some(&run), request)),
        Err(Failure::Storage(StorageError::StorageUnavailable { cause, diagnostic })) => {
            Err(StorageError::StorageUnavailable { cause, diagnostic })
        }
        Err(_) => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::storage::StoredView;
    use crate::protocol::ActorId;

    #[test]
    fn exact_worktree_view_requires_matching_owned_capture() {
        let id = |name| Id::new(name).unwrap();
        let repo = id("repo");
        let baseline = id("baseline");
        let own_capture = id("own-capture");
        let oid = "a".repeat(40);
        let overlay = ViewLayers {
            view: StoredView {
                view_id: id("captured-view"),
                repository_id: repo.clone(),
                view_digest: "b".repeat(64),
                baseline_id: baseline.clone(),
                branch_id: None,
                worktree_id: Some(id("tree-layer")),
                coverage: json!({"commit": oid, "worktreeId": own_capture, "captureDigest": "c".repeat(64)}),
            },
            layers: vec![id("tree-layer"), baseline],
        };
        let mut owner = StoredGitWorktree {
            id: id("a-different-capture"),
            repository_id: repo,
            work_id: id("work"),
            owner: ActorId(id("agent")),
            ownership_revision: 1,
            owner_root_id: None,
            owner_assignment_id: None,
            registered_root_id: id("local-root"),
            path: "test-source".into(),
            branch: "test".into(),
            head: graph::snapshot(&overlay.view).unwrap().head,
            revision: 1,
            removed: false,
            location: None,
            pending_operation_id: None,
            last_operation_id: None,
            last_result: None,
        };
        assert!(check_worktree_layers(std::slice::from_ref(&overlay), None).is_err());
        assert!(check_worktree_layers(std::slice::from_ref(&overlay), Some(&owner)).is_err());
        owner.id = own_capture;
        assert!(check_worktree_layers(&[overlay], Some(&owner)).is_ok());
    }
}
