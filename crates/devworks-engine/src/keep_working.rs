//! Explicit, boot-local project scheduling through the ordinary allocation and Start commands.
use crate::runtime::Admitter;
use crate::workers::Ticker;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::locks::LockExt;
use devworks_core::ports::storage::{
    ACTIVE, ActivationFilter, ApplicationStore, ClaimState, PROJECT_QUEUE, ReadOptions,
    StorageError, StoredAllocationClaim, WriteOptions, read_snapshot,
};
use devworks_core::ports::{IdKind, IdSource};
use devworks_core::protocol::records::{AllocationOutcome, AllocationSelection};
use devworks_core::protocol::results::{DomainDecision, OperationResult};
use devworks_core::protocol::{
    CONTRACT, CommandReply, DomainErrorCode, DomainFault, Id, ProjectId, Rejection, RootId,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ALLOCATE: &str = "next_work.claim_project_queue";
const START: &str = "runs.start";
const RELEASE: &str = "next_work.release_claim";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeepWorkingStatus {
    pub enabled: bool,
    pub root: Option<RootId>,
    pub detail: Option<String>,
}

struct Command {
    operation: &'static str,
    frame: Vec<u8>,
    candidate: Option<(Id, i64)>,
}

struct ProjectMode {
    context: InvocationContext,
    archive_epoch: i64,
    status: KeepWorkingStatus,
    pending: Option<Command>,
    last_started: Option<(Id, i64)>,
    release_before_selecting: bool,
    settling: bool,
}

impl ProjectMode {
    fn stop(&mut self, reason: impl Into<String>) {
        self.status.enabled = false;
        self.status.root = None;
        self.status.detail = Some(reason.into());
        // A stopped mode never resubmits an unconfirmed Start or allocation.
        self.pending = None;
        self.settling = true;
    }
}

struct ProjectPass {
    epoch_is_current: bool,
    queue_revision: i64,
    owned: Option<StoredAllocationClaim>,
    occupied: Option<RootId>,
}

pub(crate) struct KeepWorking {
    store: Arc<dyn ApplicationStore>,
    ids: Arc<dyn IdSource>,
    admitter: Arc<Admitter>,
    projects: Mutex<HashMap<ProjectId, Arc<Mutex<ProjectMode>>>>,
    ticker: Ticker,
}

impl KeepWorking {
    pub(crate) fn new(
        store: Arc<dyn ApplicationStore>,
        ids: Arc<dyn IdSource>,
        admitter: Arc<Admitter>,
    ) -> Self {
        Self {
            store,
            ids,
            admitter,
            projects: Mutex::new(HashMap::new()),
            ticker: Ticker::new(),
        }
    }

    pub(crate) fn status(&self, project: &ProjectId) -> Option<KeepWorkingStatus> {
        let mode = self.projects.locked().get(project).cloned();
        let Some(mode) = mode else {
            return Some(KeepWorkingStatus::default());
        };
        match mode.try_lock() {
            Ok(mode) => Some(mode.status.clone()),
            Err(std::sync::TryLockError::Poisoned(error)) => {
                Some(error.into_inner().status.clone())
            }
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    pub(crate) fn set(
        &self,
        context: &InvocationContext,
        project: &ProjectId,
        enabled: bool,
    ) -> Result<KeepWorkingStatus, StorageError> {
        let info = self.store.info();
        if self.ticker.is_stopped()
            || context.principal_kind() != PrincipalKind::Human
            || context.store_id() != &info.store_id
            || context.restore_epoch().get() != info.restore_epoch
            || context.project_id().is_some_and(|scope| scope != project)
        {
            return Err(StorageError::ScopeDenied);
        }
        let archive_epoch = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            if read.actors().actor_kind(context.principal())? != Some(PrincipalKind::Human) {
                return Err(StorageError::ScopeDenied);
            }
            let stored = read
                .projects()
                .project(project)?
                .ok_or(StorageError::ScopeDenied)?;
            if enabled && (stored.lifecycle != ACTIVE || stored.archive_stop_pending) {
                return Err(StorageError::ScopeDenied);
            }
            Ok(stored.archive_epoch)
        })?;
        let scoped = HostIngress::new(&*self.store)
            .keep_working_context(
                context,
                project.clone(),
                archive_epoch,
                self.ids.next_id(IdKind::Link),
            )
            .ok_or(StorageError::ScopeDenied)?;
        let new_mode = || ProjectMode {
            context: scoped.clone(),
            archive_epoch,
            status: KeepWorkingStatus {
                enabled,
                ..KeepWorkingStatus::default()
            },
            pending: None,
            last_started: None,
            release_before_selecting: false,
            settling: false,
        };
        let mode = self
            .projects
            .locked()
            .entry(project.clone())
            .or_insert_with(|| Arc::new(Mutex::new(new_mode())))
            .clone();
        let mut mode = mode.locked();
        if mode.context.principal() != context.principal() {
            return Err(StorageError::ScopeDenied);
        }
        if !enabled {
            mode.stop("Automatic scheduling stopped.");
        } else if !mode.status.enabled {
            // Do not replace a namespace until its unused reservation is known to be gone.
            if self
                .observe(project, &mode)?
                .owned
                .is_some_and(|claim| claim.state == ClaimState::Active)
            {
                return Err(StorageError::Busy);
            }
            *mode = new_mode();
        }
        self.ticker.notify();
        Ok(mode.status.clone())
    }

    pub(crate) fn stop(&self) {
        self.ticker.stop();
    }

    pub(crate) async fn run(self: Arc<Self>, interval: Duration) {
        self.ticker
            .run(interval, || async {
                let entries: Vec<_> = self
                    .projects
                    .locked()
                    .iter()
                    .map(|(id, mode)| (id.clone(), mode.clone()))
                    .collect();
                for (project, mode) in entries {
                    if self.ticker.is_stopped() {
                        break;
                    }
                    let owner = self.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        // Enable, disable and the decision of a project command serialize here.
                        let mut mode = mode.locked();
                        if let Err(error) = owner.advance(&project, &mut mode) {
                            mode.status.detail = Some(error.to_string());
                        }
                    })
                    .await;
                }
            })
            .await;
    }

    fn observe(
        &self,
        project: &ProjectId,
        mode: &ProjectMode,
    ) -> Result<ProjectPass, StorageError> {
        let boot = self.store.info().boot_id;
        read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            let epoch_is_current = read.projects().project(project)?.is_some_and(|current| {
                current.lifecycle == ACTIVE
                    && !current.archive_stop_pending
                    && current.archive_epoch == mode.archive_epoch
            });
            let mut owned = None;
            let mut occupied = None;
            for claim in read.work().live_allocation_claims(project)? {
                if claim.queue_key != PROJECT_QUEUE {
                    continue;
                }
                if claim.automatic
                    && claim.ingress_namespace == *mode.context.ingress_namespace()
                    && (claim.state != ClaimState::Active
                        || claim.owner_id == *mode.context.principal())
                {
                    owned = Some(claim);
                } else {
                    occupied = Some(claim.root_id);
                }
            }
            if mode.status.enabled
                && owned.is_none()
                && occupied.is_none()
                && let Some(boot) = boot
            {
                let active = ActivationFilter::LiveOfBoot(boot);
                let mut after = None;
                loop {
                    let activations = read.runs().activations(&active, after.as_ref(), 128)?;
                    for activation in &activations {
                        if read
                            .sessions()
                            .work_root(&activation.root_id)?
                            .is_some_and(|root| root.project_id == *project)
                        {
                            occupied = Some(activation.root_id.clone());
                            break;
                        }
                    }
                    if occupied.is_some() || activations.len() < 128 {
                        break;
                    }
                    after = activations.last().map(|activation| activation.id.clone());
                }
            }
            Ok(ProjectPass {
                epoch_is_current,
                queue_revision: read.work().queue_revision(project, PROJECT_QUEUE)?,
                owned,
                occupied,
            })
        })
    }

    fn advance(&self, project: &ProjectId, mode: &mut ProjectMode) -> Result<(), StorageError> {
        if self.ticker.is_stopped() || !(mode.status.enabled || mode.settling) {
            return Ok(());
        }
        let ProjectPass {
            epoch_is_current,
            owned,
            occupied,
            queue_revision,
        } = self.observe(project, mode)?;
        let candidate = owned
            .as_ref()
            .map(|claim| (claim.work_id.clone(), claim.work_revision));
        if mode.status.enabled && !epoch_is_current {
            mode.stop("The project was archived or closed. Enable automatic scheduling again after restoring it.");
        }
        // Even if the activation has already ended, resolve the exact unknown command
        // before deciding whether its work may be picked again.
        if mode.status.enabled && mode.pending.is_some() {
            self.submit(mode);
            return Ok(());
        }
        let release = !mode.status.enabled || mode.release_before_selecting;
        let (operation, body) = if release
            && let Some(claim) = owned
                .as_ref()
                .filter(|claim| claim.state == ClaimState::Active)
        {
            (RELEASE, release_body(claim))
        } else if !mode.status.enabled {
            mode.pending = None;
            mode.settling = false;
            return Ok(());
        } else if let Some(claim) = owned {
            mode.release_before_selecting = false;
            if claim.state != ClaimState::Active {
                if claim.state == ClaimState::Transferred {
                    mode.last_started = candidate;
                }
                mode.status.root = Some(claim.root_id);
                mode.status.detail = None;
                mode.pending = None;
                return Ok(());
            }
            if mode.last_started.as_ref() == Some(&(claim.work_id.clone(), claim.work_revision)) {
                mode.stop(
                    "The last automatically started item ended without advancing its work state.",
                );
                (RELEASE, release_body(&claim))
            } else {
                mode.status.root = Some(claim.root_id.clone());
                (
                    START,
                    json!({
                        "kind": START,
                        "rootId": claim.root_id,
                        "expectedWorkRevision": claim.work_revision.to_string(),
                        "expectedControlRevision": claim.root_control_revision.to_string(),
                    }),
                )
            }
        } else if let Some(root) = occupied {
            mode.status.root = Some(root);
            mode.status.detail = Some("This project already has an allocated item.".into());
            return Ok(());
        } else {
            mode.release_before_selecting = false;
            mode.status.root = None;
            (
                ALLOCATE,
                json!({
                    "kind": ALLOCATE,
                    "projectId": project,
                    "expectedQueueRevision": queue_revision.to_string(),
                    "leaseMs": "30000",
                }),
            )
        };
        if !mode
            .pending
            .as_ref()
            .is_some_and(|c| c.operation == operation)
        {
            let key = self.ids.next_id(IdKind::Link);
            let frame = if operation == START {
                json!({"protocol": CONTRACT, "requestKey": key, "command": body})
            } else {
                json!({"protocol": CONTRACT, "mode": "Command", "schemaRevision": 3,
                    "requestKey": key, "body": body})
            };
            mode.pending = Some(Command {
                operation,
                frame: frame.to_string().into_bytes(),
                candidate,
            });
        }
        self.submit(mode);
        Ok(())
    }

    fn submit(&self, mode: &mut ProjectMode) {
        let Some(pending) = mode.pending.as_ref() else {
            return;
        };
        if self.ticker.is_stopped() {
            return;
        }
        let operation = pending.operation;
        let outcome = self.admitter.command_with(
            &mode.context,
            operation,
            &pending.frame,
            WriteOptions::command(),
        );
        match outcome {
            Ok(CommandReply::CommandStatusUnknown { .. })
            | Ok(CommandReply::Rejected {
                code: Rejection::StorageUnavailable,
                ..
            }) => {
                mode.status.detail = Some("Waiting to confirm the last automatic command.".into());
                return;
            }
            Ok(CommandReply::Recorded { receipt, .. }) => match receipt.decision {
                DomainDecision::Applied { result } | DomainDecision::NoChange { result } => {
                    match result {
                        OperationResult::NextWorkClaimProjectQueue {
                            value:
                                AllocationSelection::Current(AllocationOutcome::NoEligible {
                                    reason,
                                    ..
                                }),
                        } => mode.stop(reason.0),
                        OperationResult::NextWorkClaimProjectQueue { .. }
                        | OperationResult::NextWorkReleaseClaim { .. } => {
                            // The next pass must see the new reservation while its lease is
                            // live. The ordinary housekeeping interval is as long as the lease.
                            self.ticker.notify();
                        }
                        OperationResult::RunsStart { value } if value.activation.is_some() => {
                            mode.last_started = pending.candidate.clone();
                            mode.status.detail = None;
                        }
                        _ => mode.stop("The automatic command returned an unexpected result."),
                    }
                }
                DomainDecision::Refused { fault } => {
                    if operation == ALLOCATE && fault.code == DomainErrorCode::RevisionConflict {
                        mode.status.detail =
                            Some("The project queue changed. Checking it again.".into());
                    } else if operation == START && reconsider_start(&fault) {
                        mode.release_before_selecting = true;
                        mode.status.detail = Some(
                            "Work changed before Start. Checking the project queue again.".into(),
                        );
                        self.ticker.notify();
                    } else if operation != RELEASE {
                        mode.stop(format!("Automatic scheduling stopped. {:?}", fault.code));
                    }
                }
                _ => mode.stop("The automatic command returned an unexpected decision."),
            },
            Ok(CommandReply::Rejected { code, .. }) => {
                mode.stop(format!("Automatic scheduling stopped. {code:?}"))
            }
            Ok(CommandReply::ReceiptPurged { .. }) => {
                mode.stop("The automatic command's receipt was purged.")
            }
            Err(error) => mode.stop(error.to_string()),
        }
        mode.pending = None;
    }
}

fn release_body(claim: &StoredAllocationClaim) -> Value {
    json!({"kind": RELEASE, "claimId": claim.id, "expectedFence": claim.fence.to_string()})
}

fn reconsider_start(fault: &DomainFault) -> bool {
    matches!(
        fault.code,
        DomainErrorCode::RevisionConflict
            | DomainErrorCode::DependencyBlocked
            | DomainErrorCode::LeaseLost
            | DomainErrorCode::SourceChanged
            | DomainErrorCode::ContinueRequired
    ) || (fault.code == DomainErrorCode::Conflict
        && fault
            .target_kind
            .as_ref()
            .is_some_and(|kind| kind.0 == "project_execution_owned"))
}
