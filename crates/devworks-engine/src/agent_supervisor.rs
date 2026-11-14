//! The activation launch boundary for a root coordinator. RunSupervisor owns the lease and stop
//! signal; this launcher materializes the current agent identity before a turn executor runs.
use crate::runs::{ActivationLauncher, Launch, LaunchEnd, LaunchFuture};
use crate::store;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::ports::storage::{
    ActivationOutcome, ApplicationStore, AssignmentPurpose, AssignmentState, StorageError,
    StoredSession, Unavailable,
};
use devworks_core::ports::{IdKind, IdSource};
use devworks_core::protocol::records::AgentRecord;
use devworks_core::protocol::{AssignmentId, Counter, DomainErrorCode, Id, Revision, RootId};
use devworks_core::services::agents::AgentService;
use devworks_core::services::runs::root_assignments;
use devworks_core::services::runs::substrate::{ActivationFailure, Fenced};
use devworks_core::services::sessions::Failure;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinSet;

/// A turn executor receives an already bound, current coordinator. Its work still passes through
/// the normal gateway and run dispatch fences; the stop receiver forbids new work after a stop.
/// It reports Completed only once its own effects have settled. The launcher also waits for child
/// assignments before completing the root.
pub trait AgentTurnExecutor: Send + Sync {
    fn run(
        &self,
        context: InvocationContext,
        coordinator: AgentRecord,
        stop: watch::Receiver<bool>,
    ) -> LaunchFuture;
}

#[derive(Clone, Debug)]
pub enum ChildLaunch {
    Fenced(Fenced),
    End(LaunchEnd),
}
pub type ChildLaunchFuture = Pin<Box<dyn Future<Output = ChildLaunch> + Send>>;

#[derive(Clone)]
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

    fn turn_context(
        store: &dyn ApplicationStore,
        ids: &dyn IdSource,
        session: StoredSession,
        assignment_id: AssignmentId,
        activation_id: Id,
        epoch: i64,
    ) -> InvocationContext {
        HostIngress::new(store)
            .context(
                session.agent_id,
                PrincipalKind::Agent,
                ids.next_id(IdKind::Invocation),
                Id::new("agent-turn").expect("static ingress identity"),
                Counter::new(store.info().restore_epoch).expect("nonnegative restore epoch"),
                Revision::new(1).expect("positive policy revision"),
            )
            .with_session(session.id)
            .with_lineage(session.project_id, session.root_id, Some(assignment_id))
            .with_activation(activation_id, epoch)
    }

    /// Executes one ready child under the root activation. The caller schedules children within
    /// the root's independent capacity; activation and completion each recheck current ownership.
    pub fn launch_child(
        &self,
        assignment_id: AssignmentId,
        activation_id: Id,
        epoch: i64,
        stop: watch::Receiver<bool>,
    ) -> ChildLaunchFuture {
        let store = self.store.clone();
        let ids = self.ids.clone();
        let agents = self.agents.clone();
        let executor = self.executor.clone();
        Box::pin(async move {
            if *stop.borrow() {
                return ChildLaunch::End(stopped());
            }
            let host = Self::host_context(&*store, &*ids);
            let child = {
                let agents = agents.clone();
                let host = host.clone();
                let assignment_id = assignment_id.clone();
                let activation_id = activation_id.clone();
                store::write(&store, move |tx| {
                    let child = match agents
                        .activate_child(tx, &host, &assignment_id, &activation_id, epoch)
                        .map_err(failure)?
                    {
                        Ok(child) => child,
                        Err(fenced) => return Ok(Err(fenced)),
                    };
                    let assignment = tx
                        .runs()
                        .assignment(&child.assignment_id)?
                        .ok_or(StorageError::ScopeDenied)?;
                    let session = tx
                        .sessions()
                        .session(&assignment.session_id)?
                        .ok_or(StorageError::ScopeDenied)?;
                    Ok(Ok((child, session, assignment.generation)))
                })
                .await
            };
            let (child, session, generation) = match child {
                Ok(Ok(value)) => value,
                Ok(Err(fenced)) => return ChildLaunch::Fenced(fenced),
                Err(error) => return ChildLaunch::End(failed(error)),
            };
            if *stop.borrow() {
                return ChildLaunch::End(stopped());
            }
            let context = Self::turn_context(
                &*store,
                &*ids,
                session,
                child.assignment_id.clone(),
                activation_id.clone(),
                epoch,
            );
            let stop_watch = stop.clone();
            let end = executor.run(context, child.clone(), stop).await;
            if *stop_watch.borrow() {
                return ChildLaunch::End(stopped());
            }
            let assignment_id = child.assignment_id.clone();
            let current = store::read(&store, move |snapshot| {
                snapshot.runs().assignment(&assignment_id)
            })
            .await;
            match current {
                Ok(Some(assignment))
                    if assignment.root_id == child.root_id
                        && assignment.execution_id.as_ref() == Some(&activation_id)
                        && assignment.generation == generation
                        && assignment.state == AssignmentState::Active => {}
                Ok(_) => {
                    return ChildLaunch::Fenced(Fenced::NotFound("child no longer owned here"));
                }
                Err(error) => return ChildLaunch::End(failed(error)),
            }
            if let LaunchEnd::Finished {
                outcome: devworks_core::ports::storage::ActivationOutcome::Completed,
                ..
            } = &end
            {
                let assignment_id = child.assignment_id;
                let child_root = child.root_id;
                loop {
                    let agents_for_write = agents.clone();
                    let host_for_write = host.clone();
                    let assignment_for_write = assignment_id.clone();
                    let activation_for_write = activation_id.clone();
                    let root_for_write = child_root.clone();
                    let complete = store::write(&store, move |tx| {
                        let Some(current) = tx.runs().assignment(&assignment_for_write)? else {
                            return Ok(Some(false));
                        };
                        if current.root_id != root_for_write
                            || current.execution_id.as_ref() != Some(&activation_for_write)
                            || current.generation != generation
                            || current.state != AssignmentState::Active
                        {
                            return Ok(Some(false));
                        }
                        match agents_for_write.complete_child(
                            tx,
                            &host_for_write,
                            &assignment_for_write,
                            &activation_for_write,
                            epoch,
                        ) {
                            Ok(()) => return Ok(Some(true)),
                            Err(Failure::Refused(refusal))
                                if refusal.code == DomainErrorCode::Conflict
                                    && refusal.target_kind == "assignment" => {}
                            Err(error) => return Err(failure(error)),
                        }
                        // The completion check and waiting transition share one write owner.
                        // A descendant cannot finish between these two decisions.
                        agents_for_write
                            .wait_for_descendants(
                                tx,
                                &host_for_write,
                                &assignment_for_write,
                                &activation_for_write,
                                epoch,
                            )
                            .map_err(failure)?;
                        Ok(None)
                    })
                    .await;
                    match complete {
                        Ok(Some(true)) => break,
                        Ok(Some(false)) => {
                            return ChildLaunch::Fenced(Fenced::NotFound(
                                "child no longer owned here",
                            ));
                        }
                        Ok(None) => {}
                        Err(error) => return ChildLaunch::End(failed(error)),
                    }
                    if *stop_watch.borrow() {
                        return ChildLaunch::End(stopped());
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
            ChildLaunch::End(end)
        })
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
    LaunchEnd::Failed(if crate::turn_error::storage_retryable(&error) {
        ActivationFailure::Transient(detail)
    } else {
        ActivationFailure::Deterministic(detail)
    })
}

fn stopped() -> LaunchEnd {
    LaunchEnd::Finished {
        outcome: devworks_core::ports::storage::ActivationOutcome::Cancelled,
        detail: Some("coordinator stop requested".into()),
    }
}

/// Watches the root's durable child queue while its coordinator turn runs and after that turn
/// ends. A parent completion waits for every child; blockers keep only their affected children
/// waiting. The root activation's stop signal is shared by every running child turn.
async fn supervise_children(
    launcher: AgentLauncher,
    root: RootId,
    activation_id: Id,
    epoch: i64,
    mut stop: watch::Receiver<bool>,
    mut parent_done: watch::Receiver<bool>,
    stop_all: watch::Sender<bool>,
) -> Result<(), LaunchEnd> {
    let mut running = HashSet::<AssignmentId>::new();
    let mut retry_after = HashMap::<AssignmentId, tokio::time::Instant>::new();
    let mut tasks = JoinSet::<(AssignmentId, ChildLaunch)>::new();
    let mut fatal = None;
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if !*stop.borrow() && fatal.is_none() {
            let target_root = root.clone();
            let read = store::read(&launcher.store, move |snapshot| {
                let work = snapshot
                    .sessions()
                    .work_root(&target_root)?
                    .ok_or(StorageError::ScopeDenied)?;
                let cap = devworks_core::settings::service::max_subagents(
                    snapshot.configuration(),
                    &work.project_id,
                )?;
                Ok((root_assignments(snapshot.runs(), &target_root, true)?, cap))
            })
            .await;
            let (assignments, cap) = match read {
                Ok(result) => result,
                Err(error) => {
                    fatal = Some(failed(error));
                    let _ = stop_all.send(true);
                    continue;
                }
            };
            let live_children: Vec<_> = assignments
                .into_iter()
                .filter(|assignment| assignment.purpose == AssignmentPurpose::Child)
                .collect();
            let live_ids: HashSet<_> = live_children.iter().map(|child| child.id.clone()).collect();
            retry_after.retain(|id, _| live_ids.contains(id));
            for child in &live_children {
                if child.state != AssignmentState::Active
                    || child.slot_held
                    || running.contains(&child.id)
                    || live_children
                        .iter()
                        .any(|descendant| descendant.parent_id.as_ref() == Some(&child.id))
                {
                    continue;
                }
                let agents = launcher.agents.clone();
                let host = AgentLauncher::host_context(&*launcher.store, &*launcher.ids);
                let id = child.id.clone();
                let activation = activation_id.clone();
                if let Err(error) = store::write(&launcher.store, move |tx| {
                    agents
                        .complete_child(tx, &host, &id, &activation, epoch)
                        .map_err(failure)
                })
                .await
                {
                    fatal = Some(failed(error));
                    let _ = stop_all.send(true);
                    break;
                }
            }
            if fatal.is_some() {
                continue;
            }
            let active = live_children.iter().filter(|child| child.slot_held).count();
            let launching = live_children
                .iter()
                .filter(|child| {
                    running.contains(&child.id) && child.state != AssignmentState::Active
                })
                .count();
            let mut available = cap.saturating_sub(active + launching);
            for child in &live_children {
                if available == 0
                    || !matches!(
                        child.state,
                        AssignmentState::Ready | AssignmentState::Waiting
                    )
                    || retry_after
                        .get(&child.id)
                        .is_some_and(|until| *until > tokio::time::Instant::now())
                    || !running.insert(child.id.clone())
                {
                    continue;
                }
                available -= 1;
                let worker = launcher.clone();
                let id = child.id.clone();
                let activation = activation_id.clone();
                let child_stop = stop.clone();
                tasks.spawn(async move {
                    let end = worker
                        .launch_child(id.clone(), activation, epoch, child_stop)
                        .await;
                    (id, end)
                });
            }
            if *parent_done.borrow() && live_children.is_empty() && tasks.is_empty() {
                return Ok(());
            }
        } else if tasks.is_empty() {
            return fatal.map_or(Ok(()), Err);
        }
        tokio::select! {
            _ = tick.tick() => {},
            changed = stop.changed() => {
                if changed.is_err() && tasks.is_empty() { return fatal.map_or(Ok(()), Err); }
            },
            changed = parent_done.changed() => {
                if changed.is_err() && tasks.is_empty() { return fatal.map_or(Ok(()), Err); }
            },
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                match result {
                    Ok((id, outcome)) => {
                        running.remove(&id);
                        let failure = match outcome {
                            ChildLaunch::End(LaunchEnd::Finished { outcome: ActivationOutcome::Completed, .. }) => None,
                            ChildLaunch::End(LaunchEnd::Finished { outcome: ActivationOutcome::Cancelled | ActivationOutcome::Interrupted, .. }) if *stop.borrow() => None,
                            ChildLaunch::Fenced(Fenced::Blocked(_) | Fenced::LeaseLost(_) | Fenced::StopRequested) => {
                                retry_after.insert(id.clone(), tokio::time::Instant::now() + Duration::from_secs(1));
                                None
                            },
                            ChildLaunch::Fenced(Fenced::NotFound(_)) => None,
                            ChildLaunch::Fenced(Fenced::Invalid(reason)) if reason.starts_with("the root's subagent allowance") || reason == "the child is not ready under this activation" => {
                                retry_after.insert(id.clone(), tokio::time::Instant::now() + Duration::from_secs(1));
                                None
                            },
                            ChildLaunch::Fenced(other) => Some(LaunchEnd::Failed(ActivationFailure::Deterministic(format!("child activation fenced: {other:?}")))),
                            ChildLaunch::End(end) => Some(end),
                        };
                        if let Some(failure) = failure {
                            fatal.get_or_insert(failure);
                            let _ = stop_all.send(true);
                        }
                    }
                    Err(error) => {
                        fatal.get_or_insert(LaunchEnd::Failed(ActivationFailure::Transient(format!("child turn task: {error}"))));
                        let _ = stop_all.send(true);
                    }
                }
            },
        }
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
            let context = Self::turn_context(
                &*store,
                &*ids,
                session,
                coordinator.assignment_id.clone(),
                activation_id.clone(),
                epoch,
            );
            let mut external_stop = launch.stop.clone();
            let (turn_stop_tx, turn_stop) = watch::channel(*external_stop.borrow());
            let bridge_tx = turn_stop_tx.clone();
            let bridge = tokio::spawn(async move {
                while external_stop.changed().await.is_ok() {
                    if *external_stop.borrow() {
                        let _ = bridge_tx.send(true);
                        break;
                    }
                }
            });
            let (parent_done_tx, parent_done) = watch::channel(false);
            let children = tokio::spawn(supervise_children(
                Self {
                    store: store.clone(),
                    ids: ids.clone(),
                    agents: agents.clone(),
                    executor: executor.clone(),
                },
                launch.activation.root_id.clone(),
                activation_id.clone(),
                epoch,
                turn_stop.clone(),
                parent_done,
                turn_stop_tx.clone(),
            ));
            let end = executor.run(context, coordinator.clone(), turn_stop).await;
            let _ = parent_done_tx.send(true);
            if !matches!(
                end,
                LaunchEnd::Finished {
                    outcome: ActivationOutcome::Completed,
                    ..
                }
            ) {
                let _ = turn_stop_tx.send(true);
            }
            let child_result = children.await;
            bridge.abort();
            if *launch.stop.borrow() {
                return stopped();
            }
            match child_result {
                Ok(Ok(())) => {}
                Ok(Err(end)) => return end,
                Err(error) => {
                    return failed(StorageError::unavailable(
                        Unavailable::HandlerPanicked,
                        format!("child supervisor: {error}"),
                    ));
                }
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
                    if *launch.stop.borrow() {
                        return stopped();
                    }
                    return failed(error);
                }
            }
            end
        })
    }
}
