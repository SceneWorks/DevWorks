//! Worker authority and revision fences shared by deterministic and model-assisted phases.
use super::commands::CONSUMER;
use super::selection::{self, Selection};
use crate::context::{BoundWorktree, InvocationContext, PrincipalKind};
use crate::ports::Authorization;
use crate::ports::storage::*;
use crate::protocol::data::{DataScope, ViewSelection};
use crate::protocol::{Counter, Id, OperationId, OperationState, StoreId};
use crate::services::kernel::{Failure, rev};
use crate::services::usage::auxiliary;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct Work {
    pub run_id: Id,
    pub delivery_id: Id,
    pub authority: AnalysisOwner,
}

pub(super) struct Claim {
    pub work: Work,
    pub context: InvocationContext,
    pub operation_id: OperationId,
    operation_revision: i64,
    attempt: AttemptFence,
    queue_fence: QueueFence,
    cancellation_epoch: i64,
}

impl Work {
    pub(super) fn from_operation(operation: &OperationRecord) -> Result<Self, Failure> {
        serde_json::from_value(operation.payload.clone()).map_err(|_| {
            StorageError::InvalidRecord("invalid repository analysis operation".into()).into()
        })
    }

    pub(super) fn context(&self, store: &StoreId) -> Result<InvocationContext, Failure> {
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
        if self.authority.namespace != super::super::lease::owner(&ctx) {
            return Err(StorageError::ScopeDenied.into());
        }
        Ok(ctx)
    }

    pub(super) fn authorize(
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
    pub(super) fn load(
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

    pub(super) fn run(
        &self,
        read: &dyn ReadSnapshot,
    ) -> Result<(StoredAnalysisRun, Selection), Failure> {
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
            .ok_or_else(selection::unavailable)?;
        let selected = Selection::resolve(
            read,
            &self.context,
            &run.intent.scope,
            &ViewSelection::Pinned {
                pin_id: run.pin_id.clone(),
            },
            read.stamp().now_ms,
        )?;
        if selected.binding_key != run.binding_key {
            return Err(selection::unavailable());
        }
        Ok((run, selected))
    }

    pub(super) fn validate_in(
        &self,
        tx: &mut dyn WriteTransaction,
        run: &StoredAnalysisRun,
        selected: &Selection,
    ) -> Result<(), Failure> {
        let boot = tx.store().boot_id.clone();
        let now = tx.now_ms();
        let gate = Authorization::direct().fenced_by(self.work.authority.fences.clone());
        if !crate::authority::authorization_holds_in(tx, &self.context, &gate)?
            || tx.actors().actor_kind(self.context.principal())?
                != Some(self.context.principal_kind())
        {
            return Err(StorageError::ScopeDenied.into());
        }
        let entry = tx
            .queue()
            .entry(&self.work.delivery_id)?
            .ok_or(StorageError::Cancelled)?;
        if !self.delivery_current(&entry, now, boot.as_ref()) {
            return Err(StorageError::Cancelled.into());
        }
        let current = tx
            .operations()
            .operation(&self.operation_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if current.revision != self.operation_revision
            || current.state != OperationState::Dispatching
            || current
                .latest_attempt()
                .is_none_or(|attempt| attempt.fence() != self.attempt)
        {
            return Err(StorageError::ScopeDenied.into());
        }
        let stored = tx
            .analysis()
            .analysis_run(&run.id)?
            .ok_or_else(selection::unavailable)?;
        if stored.active_operation != current.id || stored.revision != run.revision {
            return Err(StorageError::conflict(
                crate::protocol::RevisionDimension::Operation,
                "analysis_run",
                run.id.clone(),
                rev(run.revision),
                rev(stored.revision),
            )
            .into());
        }
        if selection::configuration_revision(tx.indexing(), &run.intent.scope)?
            != run.intent.configuration_revision
        {
            return Err(selection::unavailable());
        }
        let pin = tx
            .graph()
            .graph_pin(&run.pin_id)?
            .ok_or_else(selection::unavailable)?;
        super::super::lease::check_pin(&pin, &self.context, &run.intent.scope, now)?;
        selected.validate_in(tx, &self.context)
    }

    fn delivery_current(&self, entry: &QueueEntry, now: i64, boot: Option<&Id>) -> bool {
        entry.consumer == CONSUMER
            && entry.operation_id.as_ref() == Some(&self.operation_id)
            && entry.state == QueueState::Claimed
            && entry.fence().as_ref() == Some(&self.queue_fence)
            && entry.cancellation_epoch == self.cancellation_epoch
            && !entry.cancel_requested
            && entry.lease_until_ms.is_some_and(|expiry| expiry > now)
            && entry.claim_boot_id.as_ref() == boot
    }

    /// Derive the model owner before opening an auxiliary route. Its reservation
    /// rechecks the same owner, role, holds and context at the actual dispatch.
    pub(super) fn model_owner(
        &self,
        read: &dyn ReadSnapshot,
        store: &StoreInfo,
        run: &StoredAnalysisRun,
    ) -> Result<auxiliary::AuxiliaryOwnership, Failure> {
        if !matches!(
            run.intent.command,
            AnalysisCommand::Reanalyze | AnalysisCommand::Synthesize,
        ) {
            return Err(StorageError::ScopeDenied.into());
        }
        let work =
            auxiliary::ownership(read, store, &self.context).map_err(|error| match error {
                auxiliary::AuxiliaryError::Storage(error) => Failure::from(error),
                auxiliary::AuxiliaryError::Unavailable(_) => {
                    Failure::from(StorageError::ScopeDenied)
                }
            })?;
        if let Some(dispatch) = &work.dispatch
            && self.context.activation().is_none_or(|(execution, epoch)| {
                dispatch.execution_id != *execution || dispatch.owner_epoch != epoch
            })
        {
            return Err(StorageError::ScopeDenied.into());
        }
        if let DataScope::Project { project_id } = &run.intent.scope
            && self
                .context
                .project_id()
                .is_some_and(|own| own != project_id)
        {
            return Err(StorageError::ScopeDenied.into());
        }
        Ok(work)
    }
}
