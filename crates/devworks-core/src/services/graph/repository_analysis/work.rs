//! Worker authority and revision fences shared by deterministic and model-assisted phases.
use super::selection::{self, Selection};
pub(super) use crate::kernel::analysis::{Claim, Work};
use crate::ports::Authorization;
use crate::ports::storage::*;
use crate::protocol::OperationState;
use crate::protocol::data::{DataScope, ViewSelection};
use crate::services::kernel::{Failure, rev};
use crate::services::usage::auxiliary;

impl Claim {
    pub(super) fn run(
        &self,
        read: &dyn ReadSnapshot,
    ) -> Result<(StoredAnalysisRun, Selection), Failure> {
        let run = self.current_run(read)?;
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
        if !crate::services::fences::authorization_holds_in(tx, &self.context, &gate)?
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
