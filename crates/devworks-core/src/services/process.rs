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
use crate::authority::destructive::{FilesystemResolver, ProtectedRoots};
use crate::authority::redact::Redactor;
use crate::authority::shell::{CommandVerdict, preflight_command, record_verdict};
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::process::{
    NativeIdentity, Probe, ProcessError, ProcessHost, ProcessSpec, ProcessTree,
};
use crate::ports::storage::{
    ApplicationStore, NewEvent, NewProcess, ProcessFilter, ProcessPhase, StorageError,
    StoredProcess, WriteOptions, WriteTransaction, run_write,
};
use crate::protocol::{ActorId, EventId, Id, OperationId, ProjectId, RootId};
use crate::services::resources::{
    DispatchFailure, ReconcileCause, ReconcileReport, ResourceService,
};
use serde_json::{Value, json};
use std::collections::HashMap;
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
    /// The dispatch-time resource check refused the lease.
    Resource(DispatchFailure),
    /// The native spawn failed (recorded `Failed`).
    Spawn(ProcessError),
    Storage(StorageError),
}
impl From<StorageError> for LaunchRefusal {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
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
    },
    /// It could not be verified stopped within the bound; recorded `Uncertain`.
    Uncertain(String),
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
    Lease(Id),
}

struct Live {
    tree: Box<dyn ProcessTree>,
    record: StoredProcess,
}

pub struct ProcessSupervisor {
    store: Arc<dyn ApplicationStore>,
    host: Arc<dyn ProcessHost>,
    resources: Arc<ResourceService>,
    live: Mutex<HashMap<String, Arc<Mutex<Live>>>>,
}

impl ProcessSupervisor {
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
        let argv = request.spec.argv();
        let cwd = request
            .spec
            .cwd
            .as_ref()
            .map(|c| c.to_string_lossy().into_owned());
        let verdict = preflight_command(
            policy.fs,
            policy.roots,
            &argv,
            cwd.as_deref(),
            policy.protected_branches,
        );
        let refusal = match &verdict {
            CommandVerdict::Blocked { .. } => Some(LaunchRefusal::Blocked(verdict.clone())),
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
                        Err(f) => return Ok(Err(f)),
                    }
                }
                None => None,
            };
            let id = tx.new_id(IdKind::Process);
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
            Ok(Ok(record))
        })?;
        let record = intent.map_err(LaunchRefusal::Resource)?;

        match self.host.spawn(&request.spec) {
            Err(e) => {
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
                    // The identity could not be recorded: never leave an untracked tree running.
                    let mut tree = tree;
                    let _ = tree.kill();
                    return Err(LaunchRefusal::Storage(e));
                }
                let id = running.id.clone();
                self.live.lock().unwrap_or_else(|p| p.into_inner()).insert(
                    id.as_str().to_owned(),
                    Arc::new(Mutex::new(Live {
                        tree,
                        record: running.clone(),
                    })),
                );
                Ok(OwnedProcess {
                    id,
                    identity,
                    lease: running.lease_id,
                })
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
                    None,
                    "ProcessReaped",
                )?;
                drop(live);
                self.forget(&p.id);
                Ok(Some(StopOutcome::Reaped {
                    graceful: true,
                    exit_code: s.exit_code,
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
    pub fn cancel(
        &self,
        p: &OwnedProcess,
        grace: Duration,
        bound: Duration,
    ) -> Result<StopOutcome, StorageError> {
        let start = Instant::now();
        let Some(entry) = self.entry(&p.id) else {
            return self.settled_outcome(&p.id);
        };
        let mut live = entry.lock().unwrap_or_else(|e| e.into_inner());
        let mut record = live.record.clone();
        if record.phase == ProcessPhase::Running {
            self.write_phase(
                &mut record,
                ProcessPhase::Stopping,
                "ProcessStopRequested",
                json!({"graceMs": grace.as_millis() as u64}),
            )?;
            live.record = record.clone();
        }
        let mut observed = String::new();
        let mut graceful = true;
        let mut status = None;
        let cooperative = live.tree.request_stop();
        if let Err(e) = &cooperative {
            observed = e.to_string();
        }
        let grace_until = start + grace.min(bound);
        let bound_until = start + bound;
        let mut killed = false;
        loop {
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
        let outcome = match status {
            Some(s) => {
                self.settle(
                    &mut record,
                    ProcessPhase::Reaped,
                    s.exit_code,
                    None,
                    "ProcessReaped",
                )?;
                StopOutcome::Reaped {
                    graceful,
                    exit_code: s.exit_code,
                }
            }
            None => {
                let detail = Redactor::new().redact_text(&format!(
                    "the tree was not verified empty within {} ms{}{}",
                    bound.as_millis(),
                    if observed.is_empty() { "" } else { ": " },
                    observed
                ));
                self.settle(
                    &mut record,
                    ProcessPhase::Uncertain,
                    None,
                    Some(detail.clone()),
                    "ProcessUncertain",
                )?;
                live.record = record;
                return Ok(StopOutcome::Uncertain(detail));
            }
        };
        drop(live);
        self.forget(&p.id);
        Ok(outcome)
    }

    /// Cancels every owned tree in `scope`, sharing one bound: the writer stop before Git
    /// cleanup of a work root (GIT-07), the immediate stop of an archived project, or the trees
    /// serving one lease. Each outcome is recorded as for [`Self::cancel`].
    pub fn stop(
        &self,
        scope: &StopScope,
        grace: Duration,
        bound: Duration,
    ) -> Result<Vec<(Id, StopOutcome)>, StorageError> {
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
                        StopScope::Lease(lease) => r.lease_id.as_ref() == Some(lease),
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
            out.push((id, self.cancel(&p, grace.min(remaining), remaining)?));
        }
        Ok(out)
    }

    /// Cancels every tree this supervisor owns (application shutdown), sharing one bound, then
    /// runs the shutdown reconciliation of resources.
    pub fn shutdown(
        &self,
        grace: Duration,
        bound: Duration,
    ) -> Result<Vec<(Id, StopOutcome)>, StorageError> {
        let out = self.stop(&StopScope::All, grace, bound)?;
        let resources = self.resources.clone();
        run_write(&*self.store, WriteOptions::default(), move |tx| {
            resources.reconcile(tx, ReconcileCause::Shutdown)
        })?;
        Ok(out)
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
