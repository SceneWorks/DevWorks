//! The Git worker closes only owned checkouts whose final merge is recorded and current.
use super::*;
use crate::locks::LockExt;
use std::collections::VecDeque;

const CLEANUP_REQUESTS: &str = "git_cleanup_requests";

pub(super) fn record_cleanup_request(
    tx: &mut dyn WriteTransaction,
    job: &Job,
    operation: &OperationId,
) -> Result<(), StorageError> {
    let event = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: event,
        actor: job.actor.clone(),
        project_id: Some(job.project.clone()),
        root_id: Some(RootId(job.tree.work_id.clone())),
        operation_id: Some(operation.clone()),
        aggregate_kind: CLEANUP_REQUESTS.into(),
        aggregate_id: "workspace".into(),
        aggregate_revision: 1,
        kind: "FinalCleanupRequested".into(),
        payload: json!({}),
    })?;
    Ok(())
}

pub(super) fn stop_verified(
    operations: &dyn OperationRead,
    control: &StoredRootControl,
) -> Result<bool, StorageError> {
    Ok(match &control.stop_operation_id {
        Some(op) => operations
            .operation(op)?
            .is_some_and(|row| row.state == OperationState::ObservedComplete),
        None => false,
    })
}

#[derive(Default)]
pub(in crate::services::git) struct CleanupSchedule {
    after: i64,
    roots: VecDeque<RootId>,
}

impl GitService {
    pub fn schedule_completed_cleanup(self: &Arc<Self>) -> Result<bool, StorageError> {
        let mut schedule = self.cleanup_schedule.locked();
        let page = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |snapshot| {
            snapshot.history().page(
                &EventFilter::Aggregate {
                    kind: CLEANUP_REQUESTS.into(),
                    id: "workspace".into(),
                },
                schedule.after,
                32,
            )
        })?;
        schedule.after = page.next_after;
        for event in page.events {
            if let Some(root) = event.root_id
                && !schedule.roots.contains(&root)
            {
                schedule.roots.push_front(root);
            }
        }
        for _ in 0..schedule.roots.len().min(32) {
            let Some(root) = schedule.roots.pop_front() else {
                break;
            };
            match self.schedule_root_cleanup(&root) {
                Ok(true) => {}
                Ok(false) => schedule.roots.push_back(root),
                Err(error) => {
                    schedule.roots.push_back(root);
                    return Err(error);
                }
            }
        }
        Ok(page.has_more)
    }

    fn schedule_root_cleanup(self: &Arc<Self>, root: &RootId) -> Result<bool, StorageError> {
        let info = self.store.info();
        let captured = self.approval_control.read_locked().clone();
        let Some(control_api) = captured else {
            return Ok(false);
        };
        let root = root.clone();
        let actor = info.host_actor.clone();
        let ready = run_write(self.store.as_ref(), WriteOptions::host_state(), move |tx| {
            let Some(mut state) = delivery::read_delivery(tx.history(), &root)?
                .filter(delivery::Delivery::all_merged)
            else {
                return Ok(Some(Vec::new()));
            };
            let Some(op) = state.final_operation.clone() else {
                return Ok(Some(Vec::new()));
            };
            if !tx
                .operations()
                .operation(&op)?
                .is_some_and(|row| row.state == OperationState::ObservedComplete)
            {
                return Ok(None);
            }
            let Some(work) = tx
                .work()
                .work(&root.0)?
                .filter(|work| work.completion.as_deref() == Some(VERIFIED))
            else {
                return Ok(Some(Vec::new()));
            };
            if !state.cleanup_stop_requested {
                let run = tx
                    .sessions()
                    .work_root(&root)?
                    .ok_or(StorageError::ScopeDenied)?;
                let current = crate::services::runs::ensure_control(tx, &root)?;
                control_api.stop_root(
                    tx,
                    &actor,
                    &run,
                    &current,
                    StopCause::Superseded,
                    "verified final delivery",
                )?;
                state.cleanup_stop_requested = true;
                delivery::write_delivery(tx, &actor, &op, "FinalCleanupStopRequested", &mut state)?;
            }
            let current = crate::services::runs::control_of(tx.runs(), &root)?;
            if current.desired != DesiredExecution::Paused {
                return Ok(Some(Vec::new()));
            }
            if !stop_verified(tx.operations(), &current)?
                || tx.runs().live_activation(&root)?.is_some()
            {
                return Ok(None);
            }
            let mut work_ids = vec![root.0.clone()];
            if work.kind == FEATURE {
                let filter = WorkFilter {
                    project: work.project_id.clone(),
                    selection: WorkSelection::FeatureTasks(work.id.clone()),
                    status_ids: vec![],
                    repository_ids: vec![],
                    text: None,
                };
                let mut after = None;
                loop {
                    let cards = tx.work().works(&filter, after.as_ref(), MAX_PAGE)?;
                    work_ids.extend(cards.iter().map(|card| card.id.clone()));
                    if cards.len() < usize::from(MAX_PAGE) {
                        break;
                    }
                    after = cards.last().map(WorkPosition::of);
                }
            }
            let mut ready = Vec::new();
            for id in work_ids {
                let mut after = None;
                loop {
                    let batch = tx.git().git_worktrees(&id, after.as_ref(), MAX_PAGE)?;
                    for tree in &batch {
                        if tree.removed {
                            continue;
                        }
                        if let Some(operation) = &tree.pending_operation_id {
                            if !tx
                                .operations()
                                .operation(operation)?
                                .is_some_and(|row| row.capability == "git.cleanup")
                            {
                                return Ok(None);
                            }
                        } else {
                            ready.push((work.project_id.clone(), root.clone(), tree.clone()));
                        }
                    }
                    if batch.len() < usize::from(MAX_PAGE) {
                        break;
                    }
                    after = batch.last().map(|tree| tree.id.clone());
                }
            }
            Ok(Some(ready))
        })?;
        let Some(ready) = ready else {
            return Ok(false);
        };
        let authority = crate::authority::HostAuthority {
            store: self.store.as_ref(),
            clock: self.clock.as_ref(),
            protected_branches: &[],
        };
        let gateway = crate::gateway::Gateway {
            store: self.store.as_ref(),
            authority: &authority,
            ids: self.ids.as_ref(),
        };
        for (project, root, tree) in ready {
            let key = Id::new(format!(
                "cleanup-{}",
                &crate::fingerprint::sha256_hex(
                    format!(
                        "{}/{}/{}",
                        tree.id.as_str(),
                        tree.ownership_revision,
                        tree.head.hex
                    )
                    .as_bytes()
                )[..32]
            ))
            .expect("derived cleanup key");
            let ctx = InvocationContext::new(
                info.host_actor.clone(),
                PrincipalKind::Host,
                key.clone(),
                Id::new("delivery-cleanup").expect("namespace"),
                info.store_id.clone(),
                counter(info.restore_epoch),
                rev(1),
            )
            .with_lineage(Some(project), Some(root), None);
            let frame = json!({"protocol":"1.0-draft.1","requestKey":key,"command":{"kind":"Cleanup",
                "worktreeId":tree.id,"expectedOwnershipRevision":tree.ownership_revision.to_string(),"expectedHead":tree.head}});
            match gateway.command(
                &ctx,
                "git.cleanup",
                frame.to_string().as_bytes(),
                self.clone(),
            ) {
                Ok(crate::protocol::CommandReply::Recorded { receipt, .. }) => {
                    match receipt.decision {
                        DomainDecision::Accepted { .. } => {}
                        DomainDecision::Refused { fault }
                            if matches!(
                                fault.code,
                                DomainErrorCode::ApprovalRequired
                                    | DomainErrorCode::PermissionDenied
                                    | DomainErrorCode::NotFound
                            ) => {}
                        _ => return Ok(false),
                    }
                }
                _ => return Ok(false),
            }
        }
        Ok(true)
    }
}
