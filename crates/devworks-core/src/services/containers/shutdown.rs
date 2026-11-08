//! Bounded shutdown of only the external containers this app recorded as agent work.
use super::backend::Backend;
use super::execution::bound_spec;
use super::inspection::{ObservedPhase, verified_lifecycle_target};
use super::service::ContainerService;
use crate::context::{HostIngress, InvocationContext, PrincipalKind};
use crate::ports::IdKind;
use crate::ports::storage::{
    ConfigScope, ContainerFilter, ContainerPhase, NewEvent, ReadOptions, StorageError,
    StoredContainer, WriteOptions, read_snapshot, run_write,
};
use crate::protocol::{EventId, Id, OperationState};
use crate::services::sessions::{counter, rev};
use serde_json::json;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct ShutdownReport {
    pub attempted: Vec<Id>,
    pub verified_stopped: Vec<Id>,
    pub uncertain: Vec<Id>,
    pub read_error: Option<String>,
}

impl ContainerService {
    fn shutdown_context(&self, _row: &StoredContainer) -> Result<InvocationContext, StorageError> {
        let policy = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            snapshot
                .configuration()
                .current(&ConfigScope::Workspace, crate::settings::model::NAMESPACE)
        })?
        .map_or(1, |setting| setting.revision);
        let info = self.store.info();
        // Cleanup uses the host's unscoped process identity so an archived project cannot
        // fence the ownership inspection needed to stop its already-running container. The
        // container row and exact labels still fence the external target and retain attribution.
        Ok(HostIngress::new(&*self.store).context(
            info.host_actor,
            PrincipalKind::Host,
            self.ids.next_id(IdKind::Command),
            Id::new("container-shutdown").expect("static ingress ID"),
            counter(info.restore_epoch),
            rev(policy),
        ))
    }

    fn mark_shutdown_phase(
        &self,
        row: &StoredContainer,
        phase: ContainerPhase,
        detail: Option<&str>,
    ) -> Result<(), StorageError> {
        let row = row.clone();
        let detail = detail.map(|value| value.chars().take(1024).collect::<String>());
        let resources = self.resources.clone();
        run_write(&*self.store, WriteOptions::default(), move |tx| {
            let current =
                tx.resources()
                    .container(&row.id)?
                    .ok_or_else(|| StorageError::NotFound {
                        kind: "container",
                        id: row.id.as_str().into(),
                    })?;
            if current.revision != row.revision
                || current.last_operation_id != row.last_operation_id
            {
                return Err(StorageError::InvalidRecord(
                    "container changed during shutdown".into(),
                ));
            }
            if phase == ContainerPhase::Failed
                && (current.phase != ContainerPhase::Creating
                    || !matches!(
                        tx.operations()
                            .operation(&current.last_operation_id)?
                            .map(|op| op.state),
                        Some(OperationState::CancelledBeforeDispatch)
                    ))
            {
                return Err(StorageError::InvalidRecord(
                    "container creation was not cancelled before dispatch".into(),
                ));
            }
            let mut next = current;
            next.phase = phase;
            next.detail = detail.clone();
            next.revision += 1;
            next.updated_at_ms = tx.now_ms();
            tx.resources().update_container(&next, row.revision)?;
            if matches!(phase, ContainerPhase::Stopped | ContainerPhase::Failed)
                && let Some(resources) = &resources
            {
                resources.release_container_leases(tx, &next)?;
            }
            let event_id = EventId(tx.new_id(IdKind::Event));
            let host = tx.store().host_actor.clone();
            tx.history().append(NewEvent {
                id: event_id,
                actor: host,
                project_id: Some(row.project_id.clone()),
                root_id: row.root_id.clone(),
                operation_id: Some(row.last_operation_id.clone()),
                aggregate_kind: "container".into(),
                aggregate_id: row.id.as_str().into(),
                aggregate_revision: next.revision,
                kind: match phase {
                    ContainerPhase::Stopped => "ContainerShutdownStopped",
                    ContainerPhase::Failed => "ContainerCreateCancelled",
                    _ => "ContainerShutdownUncertain",
                }
                .into(),
                payload: json!({"phase":phase.as_str(),"detail":detail}),
            })?;
            Ok(())
        })
    }

    fn stop_agent_row(&self, row: &StoredContainer, deadline: Instant) -> Result<(), String> {
        let context = self
            .shutdown_context(row)
            .map_err(|error| format!("shutdown identity unavailable: {error}"))?;
        let backend = Backend::parse(&row.engine).ok_or("stored container backend invalid")?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("container shutdown deadline elapsed".into());
        }
        let observed = self
            .inspect_live_with_timeout(&context, row, remaining.min(Duration::from_secs(5)))
            .map_err(|_| "container ownership or state could not be inspected")?;
        let target = verified_lifecycle_target(
            backend,
            &observed,
            row.create_operation_id.0.as_str(),
            row.project_id.0.as_str(),
            &row.owner_kind,
        )
        .map_err(|_| "container lacks an immutable owned shutdown target")?;
        if observed.phase != ObservedPhase::Stopped {
            let args = backend
                .stop(&target)
                .map_err(|_| "container shutdown target invalid")?;
            let spec = bound_spec(&self.bindings, backend, args, None)
                .map_err(|_| "container executable changed before shutdown")?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("container shutdown deadline elapsed".into());
            }
            let result = self
                .runner
                .run(
                    &context,
                    None,
                    spec,
                    remaining.min(Duration::from_secs(8)),
                    65_536,
                    &[],
                )
                .map_err(|_| "container shutdown CLI outcome unknown")?;
            if result.exit_code != Some(0) || result.truncated {
                return Err("container shutdown CLI outcome needs inspection".into());
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("container shutdown deadline elapsed".into());
        }
        let after = self
            .inspect_live_with_timeout(&context, row, remaining.min(Duration::from_secs(5)))
            .map_err(|_| "container shutdown postcondition unavailable")?;
        if after.native_id.as_deref() != Some(target.as_str())
            || after.phase != ObservedPhase::Stopped
        {
            return Err("container identity or running state changed during shutdown".into());
        }
        self.mark_shutdown_phase(row, ContainerPhase::Stopped, None)
            .map_err(|error| format!("verified stop could not be recorded: {error}"))
    }

    /// Stops agent-work containers after command admission and effects have drained. Other
    /// owners, pre-existing containers, images and volumes are never selected or deleted.
    /// An unverified or failed stop remains Uncertain for boot recovery and project purge.
    pub fn shutdown_agent_work(&self, bound: Duration) -> ShutdownReport {
        let deadline = Instant::now() + bound;
        let mut report = ShutdownReport::default();
        let mut after: Option<Id> = None;
        loop {
            let rows = match read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
                snapshot.resources().containers(
                    &ContainerFilter::AgentWorkMayRun,
                    after.as_ref(),
                    500,
                )
            }) {
                Ok(rows) => rows,
                Err(error) => {
                    report.read_error = Some(error.to_string());
                    break;
                }
            };
            if rows.is_empty() {
                break;
            }
            let full = rows.len() == 500;
            for row in rows {
                after = Some(row.id.clone());
                let gate = self.lifecycle_gate(&row.id);
                let _guard = gate.lock().unwrap_or_else(|p| p.into_inner());
                let row = match read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
                    snapshot.resources().container(&row.id)
                }) {
                    Ok(Some(current)) => current,
                    Ok(None) => continue,
                    Err(error) => {
                        report.read_error = Some(error.to_string());
                        return report;
                    }
                };
                if !matches!(
                    row.phase,
                    ContainerPhase::Creating
                        | ContainerPhase::Starting
                        | ContainerPhase::Running
                        | ContainerPhase::Stopping
                        | ContainerPhase::Uncertain
                ) {
                    continue;
                }
                if row.phase == ContainerPhase::Creating {
                    let cancelled =
                        read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
                            snapshot.operations().operation(&row.last_operation_id)
                        })
                        .ok()
                        .flatten()
                        .is_some_and(|operation| {
                            operation.state == OperationState::CancelledBeforeDispatch
                        });
                    if cancelled {
                        if let Err(error) = self.mark_shutdown_phase(
                            &row,
                            ContainerPhase::Failed,
                            Some("create cancelled before dispatch"),
                        ) {
                            report.read_error = Some(format!(
                                "cancelled container reservation release unavailable: {error}"
                            ));
                        }
                        continue;
                    }
                }
                // A committed intent that was never dispatched cannot have launched this
                // container. Leave it to the outbox on boot rather than turning it uncertain.
                if matches!(
                    row.phase,
                    ContainerPhase::Creating | ContainerPhase::Starting
                ) {
                    let untouched =
                        read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
                            snapshot.operations().operation(&row.last_operation_id)
                        })
                        .ok()
                        .flatten()
                        .is_some_and(|operation| {
                            matches!(
                                operation.state,
                                OperationState::IntentRecorded | OperationState::Preflighted
                            )
                        });
                    if untouched {
                        continue;
                    }
                }
                report.attempted.push(row.id.clone());
                match self.stop_agent_row(&row, deadline) {
                    Ok(()) => report.verified_stopped.push(row.id),
                    Err(reason) => {
                        let _ = self.mark_shutdown_phase(
                            &row,
                            ContainerPhase::Uncertain,
                            Some(&reason),
                        );
                        report.uncertain.push(row.id);
                    }
                }
            }
            if Instant::now() >= deadline && full {
                report.read_error =
                    Some("container shutdown deadline elapsed before inventory completed".into());
            }
            if !full || Instant::now() >= deadline {
                break;
            }
        }
        report
    }
}
