//! The activation launch boundary for a root coordinator. RunSupervisor owns the lease and stop
//! signal; this launcher materializes the current agent identity before a turn executor runs.
use crate::runs::{ActivationLauncher, Launch, LaunchEnd, LaunchFuture};
use crate::store;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::ports::storage::{ApplicationStore, StorageError};
use devworks_core::ports::{IdKind, IdSource};
use devworks_core::protocol::records::AgentRecord;
use devworks_core::protocol::{Counter, Id, Revision};
use devworks_core::services::agents::AgentService;
use devworks_core::services::runs::substrate::ActivationFailure;
use devworks_core::services::sessions::Failure;
use std::sync::Arc;
use tokio::sync::watch;

/// A turn executor receives an already bound, current coordinator. Its work still passes through
/// the normal gateway and run dispatch fences; the stop receiver forbids new work after a stop.
/// It reports Completed only once its child assignments and effects have settled.
pub trait AgentTurnExecutor: Send + Sync {
    fn run(
        &self,
        context: InvocationContext,
        coordinator: AgentRecord,
        stop: watch::Receiver<bool>,
    ) -> LaunchFuture;
}

pub struct AgentLauncher {
    store: Arc<dyn ApplicationStore>,
    ids: Arc<dyn IdSource>,
    agents: Arc<AgentService>,
    executor: Arc<dyn AgentTurnExecutor>,
}

impl AgentLauncher {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        ids: Arc<dyn IdSource>,
        agents: Arc<AgentService>,
        executor: Arc<dyn AgentTurnExecutor>,
    ) -> Self {
        Self {
            store,
            ids,
            agents,
            executor,
        }
    }

    fn host_context(store: &dyn ApplicationStore, ids: &dyn IdSource) -> InvocationContext {
        let info = store.info();
        HostIngress::new(store).context(
            info.host_actor,
            PrincipalKind::Host,
            ids.next_id(IdKind::Invocation),
            Id::new("agent-supervisor").expect("static ingress identity"),
            Counter::new(info.restore_epoch).expect("nonnegative restore epoch"),
            Revision::new(1).expect("positive policy revision"),
        )
    }
}

fn failure(error: Failure) -> StorageError {
    match error {
        Failure::Storage(error) => error,
        Failure::Refused(refusal) => {
            StorageError::InvalidRecord(format!("coordinator refused: {refusal:?}"))
        }
    }
}

fn failed(error: StorageError) -> LaunchEnd {
    let detail = format!("coordinator launch: {error:?}");
    let category = match error {
        StorageError::ScopeDenied
        | StorageError::InvalidRecord(_)
        | StorageError::NotFound { .. } => ActivationFailure::Deterministic(detail),
        _ => ActivationFailure::Transient(detail),
    };
    LaunchEnd::Failed(category)
}

fn stopped() -> LaunchEnd {
    LaunchEnd::Finished {
        outcome: devworks_core::ports::storage::ActivationOutcome::Cancelled,
        detail: Some("coordinator stop requested".into()),
    }
}

impl ActivationLauncher for AgentLauncher {
    fn launch(&self, launch: Launch) -> LaunchFuture {
        let store = self.store.clone();
        let ids = self.ids.clone();
        let agents = self.agents.clone();
        let executor = self.executor.clone();
        Box::pin(async move {
            if *launch.stop.borrow() {
                return stopped();
            }
            let host = Self::host_context(&*store, &*ids);
            let activation_id = launch.activation.id;
            let epoch = launch.activation.owner_epoch;
            let materialized = {
                let agents = agents.clone();
                let host = host.clone();
                let activation_id = activation_id.clone();
                store::write(&store, move |tx| {
                    let coordinator = agents
                        .materialize_primary(tx, &host, &activation_id, epoch)
                        .map_err(failure)?;
                    let assignment = tx
                        .runs()
                        .assignment(&coordinator.assignment_id)?
                        .ok_or(StorageError::ScopeDenied)?;
                    let session = tx
                        .sessions()
                        .session(&assignment.session_id)?
                        .ok_or(StorageError::ScopeDenied)?;
                    Ok((coordinator, session))
                })
                .await
            };
            let (coordinator, session) = match materialized {
                Ok(value) => value,
                Err(error) => return failed(error),
            };
            if *launch.stop.borrow() {
                return stopped();
            }
            let context = HostIngress::new(&*store)
                .context(
                    session.agent_id,
                    PrincipalKind::Agent,
                    ids.next_id(IdKind::Invocation),
                    Id::new("agent-turn").expect("static ingress identity"),
                    Counter::new(store.info().restore_epoch).expect("nonnegative restore epoch"),
                    Revision::new(1).expect("positive policy revision"),
                )
                .with_session(session.id)
                .with_lineage(
                    session.project_id,
                    session.root_id,
                    Some(coordinator.assignment_id.clone()),
                );
            let stop_watch = launch.stop.clone();
            let end = executor
                .run(context, coordinator.clone(), launch.stop)
                .await;
            if *stop_watch.borrow() {
                return stopped();
            }
            if let LaunchEnd::Finished {
                outcome: devworks_core::ports::storage::ActivationOutcome::Completed,
                ..
            } = &end
            {
                let assignment_id = coordinator.assignment_id;
                let complete = {
                    let agents = agents.clone();
                    store::write(&store, move |tx| {
                        agents
                            .complete_primary(tx, &host, &assignment_id, &activation_id, epoch)
                            .map_err(failure)
                    })
                    .await
                };
                if let Err(error) = complete {
                    return failed(error);
                }
            }
            end
        })
    }
}
