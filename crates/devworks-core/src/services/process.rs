//! `ProcessSupervisor`: the process owner (DW-006 acceptance A1). Every owned process tree is
//! launched through it, so each launch passes the same safety preflight, dispatch-time resource
//! check, durable launch record and attribution, and every tree has bounded cancellation and
//! crash recovery.
//!
//! # Launch
//!
//! 1. **Safety preflight (SEC-04/05).** The argv goes through the authority shell/process
//!    backstop ([`crate::authority::shell::preflight_command`]): a `Blocked` command is never
//!    started; a `NeedsInput` command starts only when the caller resolved its hold by an
//!    approval of this exact request ([`LaunchRequest::approved_hold`]), and never when any step
//!    is blocked. Blocked and held verdicts are recorded, attributed, in history.
//! 2. **Resource check (RES-02).** A launch serving a lease is admitted by
//!    [`ResourceService::authorize_dispatch`] in the same unit of work that records the launch
//!    intent (`Launching`, with actor, session, project, work, assignment, operation and lease),
//!    so a stale fence never starts a process and the intent exists before anything runs.
//! 3. **Spawn** through the native [`ProcessHost`], then the native identity (pid, birth token,
//!    process group) is recorded (`Running`). A spawn failure records `Failed` (nothing ran).
//!
//! # Cancellation (bounded)
//!
//! [`ProcessSupervisor::cancel`] records `Stopping`, requests a cooperative stop of the whole
//! tree, waits at most the grace period for the tree to empty, then forcibly kills the tree and
//! waits until the overall bound. A tree verified empty is `Reaped`; otherwise it is recorded
//! `Uncertain` with what was observed, and a physical lease it serves stays uncertain: the
//! supervisor never reports a stop it could not verify and a slot never releases an uncertain
//! physical resource. Settlement and the lease reconciliation commit together.
//!
//! # Recovery
//!
//! [`ProcessSupervisor::recover`] reconciles trees recorded by earlier boots: a recorded root
//! that is still alive (same pid **and** birth token) is killed with its group; a vanished one
//! is `Reaped`; anything that cannot be established stays `Uncertain`; a launch that recorded no
//! identity is `Lost` (its effects are unknown). It then runs the resource boot reconciliation.
//! Only a person can declare a `Lost` or `Uncertain` tree stopped
//! ([`ProcessSupervisor::confirm_stopped`]), and that is recorded as their statement.
pub mod executables;
pub mod tools;
use crate::authority::destructive::{FilesystemResolver, ProtectedRoots};
use crate::authority::redact::Redactor;
use crate::authority::shell::{
    ChildEnvironment, CommandVerdict, preflight_command_in, record_verdict,
};
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::process::{
    NativeIdentity, Probe, ProcessError, ProcessHost, ProcessSpec, ProcessTree,
};
use crate::ports::storage::{
    ApplicationStore, NewEvent, NewProcess, ProcessFilter, ProcessPhase, ReadOptions, StorageError,
    StoredProcess, WriteOptions, WriteTransaction, read_snapshot, run_write,
};
use crate::protocol::{ActorId, EventId, Id, OperationId, ProjectId, RootId};
use crate::services::resources::{
    DispatchFailure, ReconcileCause, ReconcileReport, ResourceService,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// History aggregate of owned process records.
pub const PROCESS_AGGREGATE: &str = "owned_process";
/// Poll interval while waiting for a tree to empty.
pub const POLL: Duration = Duration::from_millis(10);

/// The lease a launch serves (checked at dispatch).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseClaim {
    pub resource: String,
    pub lease: Id,
    pub fence: i64,
}

/// One launch.
#[derive(Clone, Debug)]
pub struct LaunchRequest {
    pub spec: ProcessSpec,
    pub operation_id: Option<OperationId>,
    pub lease: Option<LeaseClaim>,
    /// The caller resolved a `NeedsInput` preflight hold by an approval of this exact request.
    /// It never admits a `Blocked` command.
    pub approved_hold: bool,
}

/// What the safety preflight resolves against.
pub struct LaunchPolicy<'a> {
    pub fs: &'a dyn FilesystemResolver,
    pub roots: &'a ProtectedRoots,
    pub protected_branches: &'a [String],
}

#[derive(Debug)]
pub enum LaunchRefusal {
    /// The preflight refused a step; nothing was started.
    Blocked(CommandVerdict),
    /// The preflight holds the command for input or approval; nothing was started.
    NeedsInput(CommandVerdict),
    /// An approval of a generic process cannot replace the Git owner's ref/lease checks.
    GitPushOwnerRequired,
    /// The dispatch-time resource check refused the lease.
    Resource(DispatchFailure),
    /// The invocation's project is archived or deleted: it allocates no work (nothing was
    /// started).
    ProjectClosed,
    /// The native spawn failed (recorded `Failed`).
    Spawn(ProcessError),
    Storage(StorageError),
}
impl From<StorageError> for LaunchRefusal {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

/// Bound on verifying the kill of a tree that registered after its project closed.
const LATE_LAUNCH_STOP_BOUND: Duration = Duration::from_secs(5);

/// The detail of a process record that a requested stop reaped: the tree was still running when
/// [`ProcessSupervisor::cancel`] began, so the stop, not the program, ended it.
pub const STOPPED_ON_REQUEST: &str = "stopped on request";

/// A project's launch fence: closed (archived, deleted or unknown), or open at an archival epoch.
enum Fence {
    Closed,
    Open(i64),
}
fn project_fence(
    read: &dyn crate::ports::storage::ProjectRead,
    project: &ProjectId,
) -> Result<Fence, StorageError> {
    Ok(match read.project(project)? {
        Some(p) if !crate::services::runs::closed_to_work(&p.lifecycle) => {
            Fence::Open(p.archive_epoch)
        }
        _ => Fence::Closed,
    })
}

/// A launched, owned tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedProcess {
    pub id: Id,
    pub identity: NativeIdentity,
    pub lease: Option<Id>,
}

/// How a tree ended, as verified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopOutcome {
    /// The whole tree is verified gone. `graceful` when it emptied before the forced kill.
    Reaped {
        graceful: bool,
        exit_code: Option<i32>,
        /// The terminating signal where the platform reported one when this boot reaped it.
        signal: Option<i32>,
    },
    /// It could not be verified stopped within the bound; recorded `Uncertain`.
    Uncertain(String),
}

/// A stop whose native part ran but whose record (or reconciliation) could not be written.
#[derive(Clone, Debug)]
pub struct StopFailure {
    /// What the native stop verified.
    pub outcome: StopOutcome,
    /// The first record write that failed.
    pub error: StorageError,
}
/// One tree's stop: verified and recorded, or verified natively with the failed write.
pub type StopResult = (Id, Result<StopOutcome, StopFailure>);

/// What an orderly shutdown of owned trees did.
#[derive(Clone, Debug)]
pub struct ShutdownReport {
    pub stopped: Vec<StopResult>,
    /// Externally managed agent-work containers verified stopped before the process owner closes.
    pub containers: Option<crate::services::containers::ShutdownReport>,
    /// The `Shutdown` reconciliation of resources.
    pub reconciliation: Result<ReconcileReport, StorageError>,
    /// Actual inference attempts whose local transport closure was not verified. The process
    /// supervisor alone supplies none; the composed runtime fills this after bounded draining.
    pub unclosed_inference_attempts: Vec<Id>,
}

impl ShutdownReport {
    /// Every outcome when every record and the reconciliation were written; otherwise the first
    /// failure (the native stops ran regardless).
    pub fn into_result(self) -> Result<Vec<(Id, StopOutcome)>, StorageError> {
        let stopped = all_recorded(self.stopped)?;
        if self
            .containers
            .as_ref()
            .is_some_and(|report| report.read_error.is_some() || !report.uncertain.is_empty())
        {
            return Err(StorageError::Busy);
        }
        self.reconciliation?;
        if !self.unclosed_inference_attempts.is_empty() {
            return Err(StorageError::Busy);
        }
        Ok(stopped)
    }
}
/// Every outcome when every record was written; otherwise the first failure.
pub fn all_recorded(results: Vec<StopResult>) -> Result<Vec<(Id, StopOutcome)>, StorageError> {
    results
        .into_iter()
        .map(|(id, r)| r.map(|o| (id, o)).map_err(|f| f.error))
        .collect()
}

/// What recovery found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub reaped: usize,
    pub killed: usize,
    pub uncertain: usize,
    pub lost: usize,
    pub resources: ReconcileReport,
}

#[derive(Debug)]
pub enum ConfirmRefusal {
    /// Only a person can declare a tree stopped.
    HumanOnly,
    NotFound,
    /// The tree is running under this boot's ownership; cancel it instead.
    StillOwned,
    Storage(StorageError),
}

/// Which owned trees [`ProcessSupervisor::stop`] cancels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopScope {
    All,
    Root(RootId),
    Project(ProjectId),
    /// The project's trees launched under an archival epoch before `epoch` (the trees an
    /// archival of `epoch` stops): a stop of one archival never reaches work launched under a
    /// later epoch, after a restore.
    ProjectBefore(ProjectId, i64),
    Lease(Id),
    /// The trees launched for one journaled operation (a scheduled job run's operation).
    Operation(OperationId),
}

struct Live {
    tree: Box<dyn ProcessTree>,
    record: StoredProcess,
    /// The archival epoch of its project the launch was recorded under (`None`: no project).
    epoch: Option<i64>,
    /// A stop found the tree running: kept until its settlement is recorded, so a retried poll
    /// or cancel still records that the stop ended it.
    stopped: bool,
}

pub struct ProcessSupervisor {
    store: Arc<dyn ApplicationStore>,
    host: Arc<dyn ProcessHost>,
    resources: Arc<ResourceService>,
    live: Mutex<HashMap<String, Arc<Mutex<Live>>>>,
    /// Cancellation can arrive after the durable launch intent but before native registration.
    /// Access this while holding `live`, so registration cannot slip between lookup and marking.
    pending_cancel: Mutex<HashSet<String>>,
}

impl ProcessSupervisor {
    /// Inherited environment supplied by the native process host. Helpers that clear the
    /// environment can preserve platform necessities while removing command-specific overrides.
    pub fn environment(&self) -> Vec<(String, String)> {
        self.host.environment()
    }

    pub fn new(
        store: Arc<dyn ApplicationStore>,
        host: Arc<dyn ProcessHost>,
        resources: Arc<ResourceService>,
    ) -> Self {
        Self {
            store,
            host,
            resources,
            live: Mutex::new(HashMap::new()),
            pending_cancel: Mutex::new(HashSet::new()),
        }
    }

    fn entry(&self, id: &Id) -> Option<Arc<Mutex<Live>>> {
        self.live
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id.as_str())
            .cloned()
    }
    fn forget(&self, id: &Id) {
        self.live
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id.as_str());
    }
    /// IDs of the trees this supervisor currently owns.
    pub fn owned(&self) -> Vec<Id> {
        let map = self.live.lock().unwrap_or_else(|p| p.into_inner());
        let mut ids: Vec<Id> = map.keys().filter_map(|k| Id::new(k.clone()).ok()).collect();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        ids
    }

    /// Launches `request` for `ctx` (see the module documentation for the order of checks).
    pub fn launch(
        &self,
        ctx: &InvocationContext,
        request: LaunchRequest,
        policy: &LaunchPolicy<'_>,
    ) -> Result<OwnedProcess, LaunchRefusal> {
        self.launch_with_id(ctx, request, policy, None)
    }

    /// A journaled process tool reserves its public process ID with its operation receipt, so
    /// output and cancellation can address the attempt before a child is launched.
    pub fn launch_reserved(
        &self,
        ctx: &InvocationContext,
        request: LaunchRequest,
        policy: &LaunchPolicy<'_>,
        process_id: Id,
    ) -> Result<OwnedProcess, LaunchRefusal> {
        self.launch_with_id(ctx, request, policy, Some(process_id))
    }

    /// The SEC-04/05 shell preflight verdict on launching `spec` now, without launching it.
    pub fn preflight(&self, spec: &ProcessSpec, policy: &LaunchPolicy<'_>) -> CommandVerdict {
        let argv = spec.argv();
        let cwd = spec.cwd.as_ref().map(|c| c.to_string_lossy().into_owned());
        // Destructive targets are resolved as the child will see them: its effective
        // environment (what it inherits from the host unless cleared, plus what the launch sets)
        // and its working directory, never the application's own environment.
        let env = ChildEnvironment::effective(
            &self.host.environment(),
            spec.clear_env,
            &spec.env,
            self.host.platform() == "windows",
        );
        preflight_command_in(
            policy.fs,
            policy.roots,
            &argv,
            cwd.as_deref(),
            policy.protected_branches,
            &env,
        )
    }

    fn launch_with_id(
        &self,
        ctx: &InvocationContext,
        request: LaunchRequest,
        policy: &LaunchPolicy<'_>,
        reserved_id: Option<Id>,
    ) -> Result<OwnedProcess, LaunchRefusal> {
        let argv = request.spec.argv();
        let verdict = self.preflight(&request.spec, policy);
        let git_owner_required = matches!(&verdict,CommandVerdict::NeedsInput { held,.. } if held.contains(&crate::authority::destructive::InputNeeded::GitPushOwnerRequired));
        let owned_git_push = if git_owner_required {
            if let Some(operation) = &request.operation_id {
                read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
                    Ok(snapshot
                        .operations()
                        .operation(operation)?
                        .is_some_and(|record| {
                            record.capability == "git.push"
                                && record.root_id.as_ref() == ctx.root_id()
                                && record.assignment_id.as_ref() == ctx.assignment_id()
                        }))
                })?
            } else {
                false
            }
        } else {
            false
        };
        let refusal = match &verdict {
            CommandVerdict::Blocked { .. } => Some(LaunchRefusal::Blocked(verdict.clone())),
            CommandVerdict::NeedsInput { .. } if git_owner_required && !owned_git_push => {
                Some(LaunchRefusal::GitPushOwnerRequired)
            }
            CommandVerdict::NeedsInput { .. } if !request.approved_hold => {
                Some(LaunchRefusal::NeedsInput(verdict.clone()))
            }
            _ => None,
        };
        if let Some(refusal) = refusal {
            let (c, a, v) = (ctx.clone(), argv.clone(), verdict.clone());
            run_write(&*self.store, WriteOptions::default(), move |tx| {
                record_verdict(tx, &c, "process.launch", &a, &v)
            })?;
            return Err(refusal);
        }

        let platform = self.host.platform();
        let program = Redactor::new().redact_text(
            std::path::Path::new(&request.spec.program)
                .file_name()
                .map_or(request.spec.program.as_str(), |n| {
                    n.to_str().unwrap_or("program")
                }),
        );
        let program: String = program.chars().take(256).collect();
        let digest = sha256_hex(argv.join("\u{0}").as_bytes());
        let resources = self.resources.clone();
        let (c, claim) = (ctx.clone(), request.lease.clone());
        let operation_id = request.operation_id.clone();
        let intent = run_write(&*self.store, WriteOptions::default(), move |tx| {
            // An archived (or deleted) project's work is fenced from the archival's commit: no
            // tree is launched for it, whatever path asks. The archival epoch this intent was
            // recorded under is rechecked once the tree is registered (see below).
            let epoch = match c.project_id() {
                None => None,
                Some(p) => match project_fence(tx.projects(), p)? {
                    Fence::Closed => return Ok(Err(LaunchRefusal::ProjectClosed)),
                    Fence::Open(epoch) => Some(epoch),
                },
            };
            let ticket = match &claim {
                Some(claim) => {
                    match resources.authorize_dispatch(
                        tx,
                        &c,
                        &claim.resource,
                        &claim.lease,
                        claim.fence,
                    ) {
                        Ok(t) => Some(t),
                        Err(f) => return Ok(Err(LaunchRefusal::Resource(f))),
                    }
                }
                None => None,
            };
            let id = reserved_id
                .clone()
                .unwrap_or_else(|| tx.new_id(IdKind::Process));
            let record = tx.resources().insert_process(NewProcess {
                id,
                platform: platform.to_owned(),
                program: if program.is_empty() {
                    "program".into()
                } else {
                    program
                },
                argv_sha256: digest,
                operation_id,
                lease_id: claim.as_ref().map(|c| c.lease.clone()),
                actor: c.principal().clone(),
                session_id: c.session_id().cloned(),
                project_id: c.project_id().cloned(),
                root_id: c.root_id().cloned(),
                assignment_id: c.assignment_id().cloned(),
            })?;
            // The launch record now guards the lease; the dispatch ticket is not needed.
            drop(ticket);
            Ok(Ok((record, epoch)))
        })?;
        let (record, epoch) = intent?;

        match self.host.spawn(&request.spec) {
            Err(e) => {
                self.pending_cancel
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(record.id.as_str());
                let detail = Redactor::new().redact_text(&e.to_string());
                let mut failed = record.clone();
                self.settle(
                    &mut failed,
                    ProcessPhase::Failed,
                    None,
                    Some(detail),
                    "ProcessLaunchFailed",
                )?;
                Err(LaunchRefusal::Spawn(e))
            }
            Ok(tree) => {
                let identity = tree.identity().clone();
                let mut running = record.clone();
                running.pid = Some(i64::from(identity.pid));
                running.birth = Some(identity.birth.clone());
                running.group = identity.group;
                let recorded = self.write_phase(
                    &mut running,
                    ProcessPhase::Running,
                    "ProcessStarted",
                    json!({"pid": identity.pid}),
                );
                if let Err(e) = recorded {
                    self.pending_cancel
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(record.id.as_str());
                    // The identity could not be recorded: never leave an untracked tree running.
                    let mut tree = tree;
                    let _ = tree.kill();
                    return Err(LaunchRefusal::Storage(e));
                }
                let id = running.id.clone();
                let cancel_requested = {
                    let mut live = self.live.lock().unwrap_or_else(|p| p.into_inner());
                    live.insert(
                        id.as_str().to_owned(),
                        Arc::new(Mutex::new(Live {
                            tree,
                            epoch,
                            record: running.clone(),
                            stopped: false,
                        })),
                    );
                    self.pending_cancel
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(id.as_str())
                };
                let owned = OwnedProcess {
                    id,
                    identity,
                    lease: running.lease_id.clone(),
                };
                if cancel_requested {
                    let _ = self.cancel(&owned, Duration::from_secs(1), Duration::from_secs(5));
                }
                // An archival that committed after the intent above may have snapshotted the live
                // trees before this one was registered. Rechecked now that it is registered,
                // either that archival's stop saw it, or this read sees the archival (closed, or a
                // later epoch after a restore): then the tree is killed at once. A fence that
                // cannot be read fails closed.
                if let (Some(project), Some(epoch)) = (running.project_id.as_ref(), epoch) {
                    let fence = read_snapshot(&*self.store, &ReadOptions::default(), |s| {
                        project_fence(s.projects(), project)
                    });
                    let open = matches!(fence, Ok(Fence::Open(e)) if e == epoch);
                    if !open {
                        let _ = self.cancel(&owned, Duration::ZERO, LATE_LAUNCH_STOP_BOUND);
                        return Err(match fence {
                            Err(e) => LaunchRefusal::Storage(e),
                            Ok(_) => LaunchRefusal::ProjectClosed,
                        });
                    }
                }
                Ok(owned)
            }
        }
    }

    /// The captured stdout and stderr of a tree launched with `capture_output`, once.
    pub fn take_output(
        &self,
        p: &OwnedProcess,
    ) -> Option<(Box<dyn Read + Send>, Box<dyn Read + Send>)> {
        let entry = self.entry(&p.id)?;
        let mut live = entry.lock().unwrap_or_else(|e| e.into_inner());
        live.tree.take_output()
    }

    /// The control channel of a tree launched with a control pipe, once.
    pub fn take_control(&self, p: &OwnedProcess) -> Option<crate::ports::process::ControlChannel> {
        let entry = self.entry(&p.id)?;
        let mut live = entry.lock().unwrap_or_else(|e| e.into_inner());
        live.tree.take_control()
    }

    /// Whether the root of an owned tree has exited (its descendants may still run; stop the
    /// tree with [`Self::cancel`]). A tree this supervisor no longer owns has ended.
    pub fn root_exited(&self, p: &OwnedProcess) -> bool {
        let Some(entry) = self.entry(&p.id) else {
            return true;
        };
        let mut live = entry.lock().unwrap_or_else(|e| e.into_inner());
        live.tree.root_exited().unwrap_or(false)
    }

    /// The stdin pipe of a tree launched with `pipe_stdin`, once.
    pub fn take_input(&self, p: &OwnedProcess) -> Option<Box<dyn std::io::Write + Send>> {
        let entry = self.entry(&p.id)?;
        let mut live = entry.lock().unwrap_or_else(|e| e.into_inner());
        live.tree.take_input()
    }

    /// Observes a tree without blocking; a tree found empty is settled `Reaped`.
    pub fn poll(&self, p: &OwnedProcess) -> Result<Option<StopOutcome>, StorageError> {
        let Some(entry) = self.entry(&p.id) else {
            return Ok(Some(self.settled_outcome(&p.id)?));
        };
        let mut live = entry.lock().unwrap_or_else(|e| e.into_inner());
        match live.tree.poll() {
            Ok(s) if s.empty => {
                let mut record = live.record.clone();
                self.settle(
                    &mut record,
                    ProcessPhase::Reaped,
                    s.exit_code,
                    live.stopped.then(|| STOPPED_ON_REQUEST.to_owned()),
                    "ProcessReaped",
                )?;
                drop(live);
                self.forget(&p.id);
                Ok(Some(StopOutcome::Reaped {
                    graceful: true,
                    exit_code: s.exit_code,
                    signal: s.signal,
                }))
            }
            Ok(_) => Ok(None),
            Err(e) => Err(StorageError::InvalidRecord(e.to_string())),
        }
    }

    /// Waits up to `timeout` for the tree to finish on its own.
    pub fn wait(
        &self,
        p: &OwnedProcess,
        timeout: Duration,
    ) -> Result<Option<StopOutcome>, StorageError> {
        let start = Instant::now();
        loop {
            if let Some(o) = self.poll(p)? {
                return Ok(Some(o));
            }
            if start.elapsed() >= timeout {
                return Ok(None);
            }
            std::thread::sleep(POLL);
        }
    }

    /// Bounded cancellation: cooperative stop, at most `grace` to empty, then a forced kill of
    /// the whole tree, all within `bound`.
    ///
    /// The native stop never depends on the store: a record write that fails (`Stopping` before
    /// the stop, `Reaped`/`Uncertain` after it) is reported in [`StopFailure`] together with what
    /// the native stop verified, and the stop runs regardless. The stored record is then left
    /// `Running` or `Stopping`, which this supervisor settles again on the next poll or cancel,
    /// and boot recovery reconciles by native identity if the process ends first.
    pub fn cancel(
        &self,
        p: &OwnedProcess,
        grace: Duration,
        bound: Duration,
    ) -> Result<StopOutcome, StopFailure> {
        let start = Instant::now();
        let entry = {
            let live = self.live.lock().unwrap_or_else(|p| p.into_inner());
            let entry = live.get(p.id.as_str()).cloned();
            if entry.is_none() {
                self.pending_cancel
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(p.id.as_str().to_owned());
            }
            entry
        };
        let Some(entry) = entry else {
            let outcome = self.settled_outcome(&p.id);
            if matches!(
                &outcome,
                Ok(StopOutcome::Reaped { .. }) | Err(StorageError::NotFound { .. })
            ) {
                self.pending_cancel
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(p.id.as_str());
            }
            return outcome.map_err(|error| StopFailure {
                outcome: StopOutcome::Uncertain("the stored record could not be read".into()),
                error,
            });
        };
        let mut live = entry.lock().unwrap_or_else(|e| e.into_inner());
        let mut record = live.record.clone();
        let mut unrecorded = None;
        // A tree that already exited keeps its own outcome: the stop did not end it.
        let mut status = match live.tree.poll() {
            Ok(s) if s.empty => Some(s),
            _ => None,
        };
        live.stopped |= status.is_none();
        let stopped = live.stopped;
        if status.is_none() && record.phase == ProcessPhase::Running {
            match self.write_phase(
                &mut record,
                ProcessPhase::Stopping,
                "ProcessStopRequested",
                json!({"graceMs": grace.as_millis() as u64}),
            ) {
                Ok(()) => live.record = record.clone(),
                Err(e) => unrecorded = Some(e),
            }
        }
        let mut observed = String::new();
        let mut graceful = true;
        let cooperative = if status.is_none() {
            live.tree.request_stop()
        } else {
            Ok(true)
        };
        if let Err(e) = &cooperative {
            observed = e.to_string();
        }
        let grace_until = start + grace.min(bound);
        let bound_until = start + bound;
        let mut killed = false;
        while status.is_none() {
            match live.tree.poll() {
                Ok(s) if s.empty => {
                    status = Some(s);
                    break;
                }
                Ok(_) => {}
                Err(e) => observed = e.to_string(),
            }
            let now = Instant::now();
            if killed || now >= grace_until || matches!(cooperative, Ok(false)) {
                // The forced stop is repeated while the tree is not empty: a member that forked
                // as the previous signal was delivered may have a child it missed.
                graceful = false;
                killed = true;
                if let Err(e) = live.tree.kill() {
                    observed = e.to_string();
                }
            }
            if now >= bound_until {
                break;
            }
            std::thread::sleep(POLL);
        }
        let (outcome, recorded) = match status {
            Some(s) => (
                StopOutcome::Reaped {
                    graceful,
                    exit_code: s.exit_code,
                    signal: s.signal,
                },
                self.settle(
                    &mut record,
                    ProcessPhase::Reaped,
                    s.exit_code,
                    stopped.then(|| STOPPED_ON_REQUEST.to_owned()),
                    "ProcessReaped",
                ),
            ),
            None => {
                let detail = Redactor::new().redact_text(&format!(
                    "the tree was not verified empty within {} ms{}{}",
                    bound.as_millis(),
                    if observed.is_empty() { "" } else { ": " },
                    observed
                ));
                let recorded = self.settle(
                    &mut record,
                    ProcessPhase::Uncertain,
                    None,
                    Some(detail.clone()),
                    "ProcessUncertain",
                );
                (StopOutcome::Uncertain(detail), recorded)
            }
        };
        if recorded.is_ok() {
            live.record = record;
        }
        let settled = recorded.is_ok() && matches!(outcome, StopOutcome::Reaped { .. });
        drop(live);
        // An unrecorded reaped tree stays owned (its tree is empty) so the next poll or cancel
        // records it; an uncertain one stays owned so it can be stopped again.
        if settled {
            self.forget(&p.id);
        }
        match unrecorded.or(recorded.err()) {
            None => Ok(outcome),
            Some(error) => Err(StopFailure { outcome, error }),
        }
    }

    /// Asks the OS whether a recorded tree still exists, by its native identity (pid and birth
    /// token), independently of any owner in this boot. `None` when the record has no identity.
    pub fn probe_recorded(&self, record: &StoredProcess) -> Option<Probe> {
        identity_of(record).map(|identity| self.host.probe(&identity))
    }

    /// Asks for the stop of a tree that may not be recorded yet (a launch in progress): the stop is
    /// held and applied as soon as a launch with this ID registers its tree.
    pub fn hold_cancel(&self, id: &Id) {
        self.pending_cancel
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.as_str().to_owned());
    }

    /// Cancels every owned tree in `scope`, sharing one bound: the writer stop before Git
    /// cleanup of a work root (GIT-07), the immediate stop of an archived project, or the trees
    /// serving one lease. Every matching tree is stopped natively even when recording an earlier
    /// one failed; each result is reported as for [`Self::cancel`].
    pub fn stop(&self, scope: &StopScope, grace: Duration, bound: Duration) -> Vec<StopResult> {
        let start = Instant::now();
        let matching: Vec<Id> = {
            let map = self.live.lock().unwrap_or_else(|p| p.into_inner());
            let mut ids: Vec<Id> = map
                .values()
                .filter_map(|e| {
                    let live = e.lock().unwrap_or_else(|p| p.into_inner());
                    let r = &live.record;
                    let hit = match scope {
                        StopScope::All => true,
                        StopScope::Root(root) => r.root_id.as_ref() == Some(root),
                        StopScope::Project(project) => r.project_id.as_ref() == Some(project),
                        StopScope::ProjectBefore(project, epoch) => {
                            r.project_id.as_ref() == Some(project)
                                && live.epoch.is_some_and(|e| e < *epoch)
                        }
                        StopScope::Lease(lease) => r.lease_id.as_ref() == Some(lease),
                        StopScope::Operation(op) => r.operation_id.as_ref() == Some(op),
                    };
                    hit.then(|| r.id.clone())
                })
                .collect();
            ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            ids
        };
        let mut out = Vec::new();
        for id in matching {
            let remaining = bound.saturating_sub(start.elapsed());
            let p = OwnedProcess {
                id: id.clone(),
                identity: NativeIdentity {
                    pid: 0,
                    birth: String::new(),
                    group: None,
                },
                lease: None,
            };
            out.push((id, self.cancel(&p, grace.min(remaining), remaining)));
        }
        out
    }

    /// Cancels every tree this supervisor owns (application shutdown), sharing one bound, then
    /// runs the shutdown reconciliation of resources. Every tree is stopped natively whatever
    /// the store does; record and reconciliation failures are reported in the result.
    pub fn shutdown(&self, grace: Duration, bound: Duration) -> ShutdownReport {
        let stopped = self.stop(&StopScope::All, grace, bound);
        let resources = self.resources.clone();
        let reconciliation = run_write(&*self.store, WriteOptions::default(), move |tx| {
            resources.reconcile(tx, ReconcileCause::Shutdown)
        });
        ShutdownReport {
            stopped,
            containers: None,
            reconciliation,
            unclosed_inference_attempts: Vec::new(),
        }
    }

    /// Reconciles the trees of earlier boots, then the resources they served (boot recovery).
    pub fn recover(&self) -> Result<RecoveryReport, StorageError> {
        let info = self.store.info();
        let boot = info
            .boot_id
            .clone()
            .ok_or_else(|| StorageError::IncompatibleSchema("store is read-only".into()))?;
        let mut report = RecoveryReport::default();
        let mut after: Option<Id> = None;
        loop {
            let (b, a) = (boot.clone(), after.clone());
            let page = crate::ports::storage::read_snapshot(
                &*self.store,
                &Default::default(),
                move |s| {
                    s.resources()
                        .processes(&ProcessFilter::UnsettledPriorBoots(b), a.as_ref(), 100)
                },
            )?;
            let full = page.len() == 100;
            after = page.last().map(|p| p.id.clone());
            for mut p in page {
                match (p.phase, identity_of(&p)) {
                    (ProcessPhase::Lost, _) => {}
                    (ProcessPhase::Launching, _) | (_, None) => {
                        self.settle(&mut p, ProcessPhase::Lost, None, Some("launched by an earlier boot without a recorded identity; its effects are unknown".into()), "ProcessLost")?;
                        report.lost += 1;
                    }
                    (_, Some(_)) if p.platform != self.host.platform() => {
                        self.settle(
                            &mut p,
                            ProcessPhase::Uncertain,
                            None,
                            Some("recorded on another platform".into()),
                            "ProcessUncertain",
                        )?;
                        report.uncertain += 1;
                    }
                    (_, Some(identity)) => {
                        let (probe, killed) = match self.host.probe(&identity) {
                            Probe::Alive => (self.host.terminate(&identity), true),
                            other => (other, false),
                        };
                        match probe {
                            Probe::Gone => {
                                let detail = killed.then(|| "killed by boot recovery".to_owned());
                                self.settle(
                                    &mut p,
                                    ProcessPhase::Reaped,
                                    None,
                                    detail,
                                    "ProcessReaped",
                                )?;
                                if killed {
                                    report.killed += 1;
                                } else {
                                    report.reaped += 1;
                                }
                            }
                            Probe::Alive => {
                                self.settle(
                                    &mut p,
                                    ProcessPhase::Uncertain,
                                    None,
                                    Some("still alive after a forced kill".into()),
                                    "ProcessUncertain",
                                )?;
                                report.uncertain += 1;
                            }
                            Probe::Unknown(why) => {
                                let why = Redactor::new().redact_text(&why);
                                self.settle(
                                    &mut p,
                                    ProcessPhase::Uncertain,
                                    None,
                                    Some(why),
                                    "ProcessUncertain",
                                )?;
                                report.uncertain += 1;
                            }
                        }
                    }
                }
            }
            if !full {
                break;
            }
        }
        let resources = self.resources.clone();
        report.resources = run_write(&*self.store, WriteOptions::default(), move |tx| {
            resources.reconcile(tx, ReconcileCause::Boot)
        })?;
        Ok(report)
    }

    /// A person's statement that a `Lost` or `Uncertain` tree is stopped (for example after
    /// checking the machine). Recorded as theirs; the resources it served may then transfer.
    pub fn confirm_stopped(
        &self,
        ctx: &InvocationContext,
        process: &Id,
    ) -> Result<StoredProcess, ConfirmRefusal> {
        if ctx.principal_kind() != PrincipalKind::Human {
            return Err(ConfirmRefusal::HumanOnly);
        }
        if self.entry(process).is_some() {
            return Err(ConfirmRefusal::StillOwned);
        }
        let (p, c, resources) = (process.clone(), ctx.clone(), self.resources.clone());
        run_write(&*self.store, WriteOptions::default(), move |tx| {
            let Some(mut record) = tx.resources().process(&p)? else {
                return Ok(Err(ConfirmRefusal::NotFound));
            };
            if !matches!(record.phase, ProcessPhase::Lost | ProcessPhase::Uncertain) {
                return Ok(Err(ConfirmRefusal::StillOwned));
            }
            settle_in(
                tx,
                c.principal(),
                &resources,
                &mut record,
                ProcessPhase::Reaped,
                None,
                Some("confirmed stopped by the user".into()),
                "ProcessConfirmedStopped",
            )?;
            Ok(Ok(record))
        })
        .map_err(ConfirmRefusal::Storage)?
    }

    fn settled_outcome(&self, id: &Id) -> Result<StopOutcome, StorageError> {
        let id = id.clone();
        let record =
            crate::ports::storage::read_snapshot(&*self.store, &Default::default(), move |s| {
                s.resources().process(&id)
            })?
            .ok_or_else(|| StorageError::NotFound {
                kind: "owned_process",
                id: String::new(),
            })?;
        Ok(match record.phase {
            ProcessPhase::Reaped | ProcessPhase::Failed => StopOutcome::Reaped {
                graceful: true,
                exit_code: record.exit_code.map(|c| c as i32),
                signal: None,
            },
            _ => StopOutcome::Uncertain(record.detail.unwrap_or_default()),
        })
    }

    fn write_phase(
        &self,
        record: &mut StoredProcess,
        phase: ProcessPhase,
        kind: &'static str,
        payload: Value,
    ) -> Result<(), StorageError> {
        let mut next = record.clone();
        let host = self.store.info().host_actor;
        let updated = run_write(&*self.store, WriteOptions::default(), move |tx| {
            apply(tx, &host, &mut next, phase, kind, payload)?;
            Ok(next)
        })?;
        *record = updated;
        Ok(())
    }

    /// Records a terminal or uncertain phase and reconciles the lease it serves, together.
    fn settle(
        &self,
        record: &mut StoredProcess,
        phase: ProcessPhase,
        exit_code: Option<i32>,
        detail: Option<String>,
        kind: &'static str,
    ) -> Result<(), StorageError> {
        let mut next = record.clone();
        let (host, resources) = (self.store.info().host_actor, self.resources.clone());
        let updated = run_write(&*self.store, WriteOptions::default(), move |tx| {
            settle_in(
                tx, &host, &resources, &mut next, phase, exit_code, detail, kind,
            )?;
            Ok(next)
        })?;
        *record = updated;
        Ok(())
    }
}

fn identity_of(p: &StoredProcess) -> Option<NativeIdentity> {
    Some(NativeIdentity {
        pid: u32::try_from(p.pid?).ok()?,
        birth: p.birth.clone()?,
        group: p.group,
    })
}

#[allow(clippy::too_many_arguments)]
fn settle_in(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    resources: &ResourceService,
    record: &mut StoredProcess,
    phase: ProcessPhase,
    exit_code: Option<i32>,
    detail: Option<String>,
    kind: &'static str,
) -> Result<(), StorageError> {
    // Settlement reads the stored record: recovery and a live owner may both settle it.
    if let Some(current) = tx.resources().process(&record.id)? {
        *record = current;
    }
    if record.phase.quiesced() {
        return Ok(());
    }
    record.exit_code = exit_code.map(i64::from).or(record.exit_code);
    record.detail = detail.map(|d| d.chars().take(1024).collect());
    let payload = json!({"exitCode": record.exit_code, "detail": record.detail});
    apply(tx, actor, record, phase, kind, payload)?;
    if let Some(lease) = record.lease_id.clone() {
        resources.reconcile_lease(tx, &lease)?;
    }
    Ok(())
}

fn apply(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    record: &mut StoredProcess,
    phase: ProcessPhase,
    kind: &'static str,
    mut payload: Value,
) -> Result<(), StorageError> {
    let expected = record.revision;
    record.phase = phase;
    record.revision += 1;
    tx.resources().update_process(record, expected)?;
    if let Value::Object(m) = &mut payload {
        m.insert("state".into(), json!(phase.as_str()));
        m.insert("program".into(), json!(record.program));
        m.insert("ownerId".into(), json!(record.actor.0.as_str()));
        if let Some(l) = &record.lease_id {
            m.insert("leaseId".into(), json!(l.as_str()));
        }
    }
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id,
        actor: actor.clone(),
        project_id: record.project_id.clone(),
        root_id: record.root_id.clone(),
        operation_id: record.operation_id.clone(),
        aggregate_kind: PROCESS_AGGREGATE.into(),
        aggregate_id: record.id.as_str().to_owned(),
        aggregate_revision: record.revision,
        kind: kind.into(),
        payload,
    })?;
    Ok(())
}
