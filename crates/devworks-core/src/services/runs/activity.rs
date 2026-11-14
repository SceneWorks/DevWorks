//! Whether anything of a root may still be running: the one owner of root activity.
//!
//! Stop, stop settlement, linked-card coverage and allocation release each ask a different
//! question (is any work left, has admitted work quiesced, which trees are unsettled, has one
//! card's work quiesced), but all of them read the same facts. [`RootActivity::of`] reads them
//! once, through root-filtered store queries: the root's live activation, its root-scoped
//! deliveries, its unsettled owned process trees and its agent containers that may still run.
//! An open durable conversation is not activity: a stopped or recovered activation can leave
//! one open.
use crate::ports::storage::{
    ContainerFilter, MAX_PAGE, ProcessFilter, QueueState, StorageError, StoredActivation,
    StoredProcess, WriteTransaction,
};
use crate::protocol::{AssignmentId, Id, RootId};

/// A root-scoped delivery still on the queue.
#[derive(Clone, Debug)]
pub struct Delivery {
    pub id: Id,
    /// A worker claimed it and may be executing it now.
    pub claimed: bool,
    /// The card the delivery names, when it names one.
    pub work: Option<RootId>,
    /// It executes a journaled operation.
    pub operation: bool,
    /// The assignment whose operation the claimed delivery executes.
    pub assignment: Option<AssignmentId>,
}

/// What of one root may still be running, read in the caller's transaction.
#[derive(Clone, Debug)]
pub struct RootActivity {
    /// The root's live activation (any phase before `Stopped`).
    pub activation: Option<StoredActivation>,
    pub deliveries: Vec<Delivery>,
    /// Owned process trees recorded under the root and not yet verified quiesced.
    pub processes: Vec<StoredProcess>,
    /// An agent-owned container of the root may still be running.
    pub container: bool,
}

impl RootActivity {
    pub fn of(tx: &mut dyn WriteTransaction, root: &RootId) -> Result<Self, StorageError> {
        let activation = tx.runs().live_activation(root)?;
        let mut deliveries = Vec::new();
        let mut after: Option<Id> = None;
        loop {
            let page = tx.runs().root_deliveries(root, after.as_ref(), MAX_PAGE)?;
            for id in &page {
                let entry = tx.queue().entry(id)?;
                let claimed = entry
                    .as_ref()
                    .is_some_and(|e| e.state == QueueState::Claimed);
                let assignment = match entry.as_ref().and_then(|e| e.operation_id.as_ref()) {
                    Some(op) if claimed => {
                        tx.operations().operation(op)?.and_then(|o| o.assignment_id)
                    }
                    _ => None,
                };
                deliveries.push(Delivery {
                    id: id.clone(),
                    claimed,
                    operation: entry.as_ref().is_some_and(|e| e.operation_id.is_some()),
                    work: entry.and_then(|e| e.root_id),
                    assignment,
                });
            }
            if page.len() < usize::from(MAX_PAGE) {
                break;
            }
            after = page.last().cloned();
        }
        let mut processes: Vec<StoredProcess> = Vec::new();
        loop {
            let page = tx.resources().processes(
                &ProcessFilter::UnsettledRoot(root.clone()),
                processes.last().map(|p| &p.id),
                MAX_PAGE,
            )?;
            let more = page.len() == usize::from(MAX_PAGE);
            processes.extend(page);
            if !more {
                break;
            }
        }
        let container = !tx
            .resources()
            .containers(
                &ContainerFilter::AgentWorkMayRunForRoot(root.clone()),
                None,
                1,
            )?
            .is_empty();
        Ok(Self {
            activation,
            deliveries,
            processes,
            container,
        })
    }

    /// Work the root admitted may still run or wait: a root-scoped delivery, an unsettled
    /// process tree or an agent container. The live activation is not counted.
    pub fn has_work(&self) -> bool {
        !self.deliveries.is_empty() || !self.processes.is_empty() || self.container
    }

    /// Nothing admitted for the root is still running: no live activation with an owner, no
    /// claimed root-scoped delivery and no agent container. Process trees are not counted; a
    /// stop reconciles them separately.
    pub fn admitted_quiesced(&self) -> bool {
        !self.activation.as_ref().is_some_and(|a| a.owner.is_some())
            && !self.deliveries.iter().any(|d| d.claimed)
            && !self.container
    }

    /// [`Self::admitted_quiesced`], with every owned process tree settled as well.
    pub fn quiesced(&self) -> bool {
        self.admitted_quiesced() && self.processes.is_empty()
    }

    /// The unsettled process trees, each with its phase.
    pub fn unsettled(&self) -> Vec<(Id, String)> {
        self.processes
            .iter()
            .map(|p| (p.id.clone(), format!("{:?}", p.phase)))
            .collect()
    }
}
