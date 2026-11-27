//! Host-owned executable and working-root bindings for the public process operations.
//!
//! Wire IDs are selectors only. A caller cannot supply a program path or an ambient working
//! directory through `ProcessSpec`; the host resolves both against its own registrations and
//! rechecks the selected files immediately before launch. A project-scoped invocation may use
//! only roots registered to that project. `Full` is ambient execution with normal user rights,
//! not an OS sandbox. The current authority policy requires an exact native exception under
//! Project scope; its effective execution profile is still `Full`. No confined launcher is
//! qualified, so `Project` and `Confined` are refused instead of silently upgraded.
#[path = "tool_modules/bindings.rs"]
mod bindings;
#[path = "tool_modules/decide.rs"]
mod decide;
#[path = "tool_modules/execute.rs"]
mod execute;
#[path = "tool_modules/install.rs"]
mod install;
#[path = "tool_modules/install_safety.rs"]
mod install_safety;
#[path = "tool_modules/jobs.rs"]
mod jobs;
#[path = "tool_modules/output.rs"]
mod output;
use super::executables::PathExecutables;
use super::{
    CaptureFault, CapturePolicy, CaptureStream, CaptureWait, LaunchPolicy, LaunchRefusal,
    LaunchRequest, OwnedProcess, ProcessSupervisor, StopOutcome,
};
use crate::authority::destructive::{FilesystemResolver, ProtectedRoots};
use crate::authority::redact::Redactor;
use crate::authority::shell::CommandVerdict;
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::Fingerprint;
use crate::fingerprint::sha256_hex;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::locks::LockExt;
use crate::operations::{OperationInput, process as ops};
use crate::ports::process::{NativeIdentity, Probe, ProcessSpec as NativeProcessSpec};
use crate::ports::secrets::{SecretKey, SecretStore};
use crate::ports::storage::{
    ApplicationStore, ArtifactState, ConfigScope, DeliveryFilter, GrantKind, NewEvent,
    NewOperation, NewQueueEntry, OperationRecord, ProcessPhase, ReadOptions, ReadSnapshot,
    StorageError, StoredApproval, StoredGitWorktree, StoredProcess, StoredRepository,
    WriteTransaction, read_snapshot,
};
use crate::ports::{
    AuthorityPort, AuthorityRequest, CommandHandler, Decided, HandlerFailure, IdKind, IdSource,
    Preparation, Prepared, QueryAnswer, QueryHandler,
};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{
    ApprovalState, OutputEncoding, ProcessOutput, ProcessRecord, ProposalRecord,
};
use crate::protocol::records::{ProcessSpec, RegisteredPath, ToolProfile};
use crate::protocol::results::{DomainDecision, OperationCompletion, OperationResult};
use crate::protocol::support::BoundedText;
use crate::protocol::{
    CorrelationId, DomainErrorCode, DomainFault, EventId, OperationId, OperationState, Protocol,
    RetryAdvice, RevisionDimension,
};
use crate::protocol::{Id, ProjectId};
use crate::registry::Mode;
use crate::services::artifacts::{Viewer, visible};
use crate::services::effect::{EffectOutcome, JournaledEffect};
use crate::services::kernel::{Refusal, counter, decided, query_fault, rev};
use base64::Engine;
pub use bindings::*;
use install::*;
use install_safety::*;
pub(crate) use jobs::*;
pub use output::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const EFFECT_CONSUMER: &str = "process_tool_effects";
pub const CANCEL_CONSUMER: &str = "process_tool_cancellations";
/// Processes launched by watchers run on their own consumer, so a long-running watched process
/// never takes a `process.execute` worker slot.
pub const WATCH_CONSUMER: &str = "watch_process_effects";

pub struct ProcessToolService {
    store: Arc<dyn ApplicationStore>,
    processes: Arc<ProcessSupervisor>,
    authority: Arc<dyn AuthorityPort>,
    secrets: Arc<dyn SecretStore>,
    bindings: ToolBindings,
    fs: Arc<dyn FilesystemResolver>,
    protected_roots: ProtectedRoots,
    ids: Arc<dyn IdSource>,
    git: Option<Arc<crate::services::git::GitService>>,
    live_output: Mutex<HashMap<String, Arc<Mutex<BoundedOutput>>>>,
    /// Match counters of watched runs with a pattern, by process ID: stdout's and stderr's.
    live_matches: Mutex<HashMap<String, Arc<Mutex<[MatchCounter; 2]>>>>,
    live_tails: Mutex<HashMap<String, LiveTail>>,
    /// Watched processes whose monitor thread has not yet recorded their outcome.
    monitoring: Mutex<HashSet<String>>,
}

impl ProcessToolService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        processes: Arc<ProcessSupervisor>,
        authority: Arc<dyn AuthorityPort>,
        secrets: Arc<dyn SecretStore>,
        bindings: ToolBindings,
        fs: Arc<dyn FilesystemResolver>,
        protected_roots: ProtectedRoots,
        ids: Arc<dyn IdSource>,
    ) -> Self {
        Self {
            store,
            processes,
            authority,
            secrets,
            bindings,
            fs,
            protected_roots,
            ids,
            git: None,
            live_output: Mutex::new(HashMap::new()),
            live_matches: Mutex::new(HashMap::new()),
            live_tails: Mutex::new(HashMap::new()),
            monitoring: Mutex::new(HashSet::new()),
        }
    }

    pub fn with_git_source(mut self, git: Option<Arc<crate::services::git::GitService>>) -> Self {
        self.git = git;
        self
    }

    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }

    /// The operation receipt is returned at admission. This scoped completion supplies its
    /// typed ProcessRecord (and therefore the ID needed by output/cancel) once the effect settles.
    pub fn completion(
        &self,
        ctx: &InvocationContext,
        operation_id: &OperationId,
    ) -> Result<Option<OperationCompletion>, DomainFault> {
        let record = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            snapshot.operations().operation(operation_id)
        })
        .map_err(|error| query_fault(error.into(), "operation", self.correlation()))?;
        let not_found = || {
            Refusal::new(
                DomainErrorCode::NotFound,
                "operation",
                Some(operation_id.0.clone()),
            )
            .fault(self.correlation())
        };
        let operation = record.ok_or_else(not_found)?;
        if !operation.capability.starts_with("process.") {
            return Err(not_found());
        }
        let visible = if operation.capability == "process.execute" {
            serde_json::from_value::<ExecuteJob>(operation.payload.clone())
                .ok()
                .is_some_and(|job| {
                    job.project.as_ref() == ctx.project_id()
                        && (job.actor == *ctx.principal()
                            || matches!(
                                ctx.principal_kind(),
                                PrincipalKind::Human | PrincipalKind::Host
                            ))
                })
        } else if operation.capability == "process.install_project_dependencies" {
            serde_json::from_value::<InstallJob>(operation.payload.clone())
                .ok()
                .is_some_and(|job| {
                    job.project.as_ref() == ctx.project_id()
                        && (job.actor == *ctx.principal()
                            || matches!(
                                ctx.principal_kind(),
                                PrincipalKind::Human | PrincipalKind::Host
                            ))
                })
        } else {
            operation
                .payload
                .get("processId")
                .and_then(Value::as_str)
                .and_then(|id| Id::new(id).ok())
                .and_then(|id| {
                    read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
                        snapshot.resources().process(&id)
                    })
                    .ok()
                    .flatten()
                })
                .is_some_and(|process| {
                    process.project_id.as_ref() == ctx.project_id()
                        && (process.actor == *ctx.principal()
                            || matches!(
                                ctx.principal_kind(),
                                PrincipalKind::Human | PrincipalKind::Host
                            ))
                })
        };
        if !visible {
            return Err(not_found());
        }
        let revision = rev(operation.revision);
        let effect = operation.latest_attempt().and_then(|attempt| {
            attempt.observations.iter().rev().find_map(|observation| {
                observation
                    .get("effect")
                    .or_else(|| observation.get("reconciliation"))
                    .cloned()
            })
        });
        Ok(match operation.state {
            OperationState::ObservedComplete => {
                match effect
                    .as_ref()
                    .and_then(|e| e.pointer("/evidence/result"))
                    .and_then(|result| {
                        serde_json::from_value::<OperationResult>(result.clone()).ok()
                    }) {
                    Some(result) => Some(OperationCompletion::Succeeded {
                        protocol: Protocol::Draft1,
                        operation_id: operation.id,
                        revision,
                        result,
                    }),
                    None => Some(OperationCompletion::Failed {
                        protocol: Protocol::Draft1,
                        operation_id: operation.id,
                        revision,
                        fault: Refusal::new(DomainErrorCode::OutcomeUnknown, "process", None)
                            .retry(RetryAdvice::ReconcileOperation)
                            .fault(self.correlation()),
                    }),
                }
            }
            OperationState::Failed | OperationState::CancelledBeforeDispatch => {
                Some(OperationCompletion::Failed {
                    protocol: Protocol::Draft1,
                    operation_id: operation.id,
                    revision,
                    fault: Refusal::new(
                        if operation.state == OperationState::Failed {
                            DomainErrorCode::ExternalFailure
                        } else {
                            DomainErrorCode::Cancelled
                        },
                        "process",
                        None,
                    )
                    .fault(self.correlation()),
                })
            }
            OperationState::OutcomeUnknown => Some(OperationCompletion::OutcomeUnknown {
                protocol: Protocol::Draft1,
                operation_id: operation.id,
                revision,
                correlation_id: self.correlation(),
            }),
            _ => None,
        })
    }

    /// Binds a wire process spec to the host's registered executables and roots, as
    /// `process.execute` preparation does.
    pub fn bind_spec(
        &self,
        input: &ProcessSpec,
        ctx: &InvocationContext,
    ) -> Result<NativeProcessSpec, BindingRefusal> {
        self.bound_spec(input, ctx)
    }

    /// Records a `process.execute` intent for `spec` and enqueues its effect on `consumer`, in
    /// the caller's transaction. The returned operation ID is also the owned process's ID.
    pub(crate) fn record_execute(
        &self,
        ctx: &InvocationContext,
        fingerprint: &Fingerprint,
        spec: &ProcessSpec,
        origin: ExecuteOrigin,
        tx: &mut dyn WriteTransaction,
    ) -> Result<OperationId, StorageError> {
        let operation = OperationId(tx.new_id(IdKind::Operation));
        let mut job = ExecuteJob::from_context(ctx, fingerprint, spec.clone());
        let consumer = match origin {
            ExecuteOrigin::Tool => EFFECT_CONSUMER,
            ExecuteOrigin::Watch {
                tail_bytes,
                requested,
                pattern,
            } => {
                job.tail_bytes = tail_bytes;
                job.watch = Some(WatchOrigin {
                    requested_tail_bytes: requested,
                    pattern,
                });
                WATCH_CONSUMER
            }
        };
        let payload =
            serde_json::to_value(&job).map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: ctx.root_id().cloned(),
            assignment_id: ctx.assignment_id().cloned(),
            capability: "process.execute".into(),
            schema_version: 1,
            args_digest: sha256_hex(payload.to_string().as_bytes()),
            payload,
        })?;
        let event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event.clone(),
            actor: ctx.principal().clone(),
            project_id: ctx.project_id().cloned(),
            root_id: ctx.root_id().cloned(),
            operation_id: Some(operation.clone()),
            aggregate_kind: "process_tool".into(),
            aggregate_id: operation.0.as_str().into(),
            aggregate_revision: 1,
            kind: "ProcessRequested".into(),
            payload: json!({"operationId": operation.0.as_str(), "executableRef": spec.executable_ref.as_str()}),
        })?;
        let entry = tx.new_id(IdKind::QueueEntry);
        let now = tx.now_ms();
        tx.queue().enqueue(NewQueueEntry {
            id: entry,
            consumer: consumer.into(),
            event_id: event,
            root_id: ctx.root_id().cloned(),
            operation_id: Some(operation.clone()),
            partition: None,
            priority: 0,
            ready_at_ms: now,
            payload: json!({"operationId":operation.0.as_str()}),
        })?;
        Ok(operation)
    }

    /// The shell preflight verdict on launching `spec` in the working root `root`, as a launch
    /// would judge it now.
    pub(crate) fn preflight(
        &self,
        spec: &NativeProcessSpec,
        root: &Id,
    ) -> Result<CommandVerdict, StorageError> {
        let branches = self.protected_branches(root)?;
        let policy = LaunchPolicy {
            fs: &*self.fs,
            roots: &self.protected_roots,
            protected_branches: &branches,
        };
        Ok(self.processes.preflight(spec, &policy))
    }

    /// The branches the shell guard protects for a command run in the working root `root`
    /// besides `main` and `master`: the default branch of the repository the root belongs to
    /// (a worktree's repository, or the registered repository itself).
    fn protected_branches(&self, root: &Id) -> Result<Vec<String>, StorageError> {
        read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            let repository = match snapshot.git().git_worktree(root)? {
                Some(tree) => tree.repository_id,
                None => root.clone(),
            };
            crate::authority::policy::repository_protected_branches(snapshot, &repository)
        })
    }

    /// Resolves a file a watcher reads: its directory must be a real directory under the
    /// project's registered root (no symlink escape); the file itself may not exist yet.
    pub(crate) fn resolve_watched_file(
        &self,
        path: &RegisteredPath,
        ctx: &InvocationContext,
    ) -> Result<std::path::PathBuf, BindingRefusal> {
        // The same roots a process watcher may use: static bindings, member repositories and
        // the caller's own worktrees.
        let bindings = self.bindings_with_root(&path.root_id, ctx)?;
        let relative = std::path::Path::new(&path.relative.0);
        let name = relative
            .file_name()
            .filter(|n| *n != "..")
            .ok_or(BindingRefusal::OutOfProject)?;
        let parent = relative
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|p| !p.is_empty());
        // A file at the root's top level is joined to the root itself; a deeper one to its
        // directory, resolved under the root like any registered path.
        let directory = match parent {
            None => bindings.root_directory(&path.root_id, ctx.project_id())?,
            Some(parent) => {
                let directory = RegisteredPath {
                    root_id: path.root_id.clone(),
                    relative: serde_json::from_value(json!(parent))
                        .map_err(|_| BindingRefusal::OutOfProject)?,
                };
                bindings.resolve_registered_path(&directory, ctx.project_id())?
            }
        };
        Ok(directory.join(name))
    }

    /// The matches of a running watched run's pattern counted so far (`None` once it ended or
    /// without a pattern; its outcome then carries the final count).
    pub(crate) fn live_match_count(&self, process_id: &Id) -> Option<i64> {
        let matches = self
            .live_matches
            .locked()
            .get(process_id.as_str())
            .cloned()?;
        let counters = matches.locked();
        Some(counters[0].count() + counters[1].count())
    }

    /// Whether a watched run's recorded tree is still alive, asked of the OS by its pid and birth
    /// token rather than through the run's monitor (the watcher backup check's own path).
    pub(crate) fn probe_watched_run(&self, record: &StoredProcess) -> Option<Probe> {
        self.processes.probe_recorded(record)
    }

    /// Stops a watched run's tree now (bounded), as `process.cancel` would; a failed stop leaves
    /// the tree to its own time limit.
    pub(crate) fn stop_watched_run(&self, process_id: &Id) {
        let owned = OwnedProcess {
            id: process_id.clone(),
            identity: NativeIdentity {
                pid: 0,
                birth: String::new(),
                group: None,
            },
            lease: None,
        };
        let stop = self
            .processes
            .cancel(&owned, Duration::from_secs(1), Duration::from_secs(5));
        // Not recorded yet: a dispatch past its checks may still launch it, so the stop is held
        // for that launch to apply as soon as the tree registers.
        if let Err(crate::services::process::StopFailure {
            error: StorageError::NotFound { .. },
            ..
        }) = stop
        {
            self.processes.hold_cancel(process_id);
        }
    }

    /// Records one more run of the watched `process.execute` job `source` (a probe's next run)
    /// and enqueues it on the watcher consumer, in the caller's transaction. The run repeats the
    /// job exactly, so it is re-authorized at dispatch as the request that admitted it.
    pub(crate) fn record_watch_rerun(
        &self,
        source: &OperationId,
        tx: &mut dyn WriteTransaction,
    ) -> Result<OperationId, StorageError> {
        let job = tx
            .operations()
            .operation(source)?
            .ok_or_else(|| StorageError::NotFound {
                kind: "operation",
                id: source.0.as_str().to_owned(),
            })?;
        let parsed = serde_json::from_value::<ExecuteJob>(job.payload.clone())
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        let operation = OperationId(tx.new_id(IdKind::Operation));
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: job.root_id.clone(),
            assignment_id: job.assignment_id.clone(),
            capability: "process.execute".into(),
            schema_version: 1,
            args_digest: job.args_digest.clone(),
            payload: job.payload.clone(),
        })?;
        let event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event.clone(),
            actor: parsed.actor.clone(),
            project_id: parsed.project.clone(),
            root_id: job.root_id.clone(),
            operation_id: Some(operation.clone()),
            aggregate_kind: "process_tool".into(),
            aggregate_id: operation.0.as_str().into(),
            aggregate_revision: 1,
            kind: "ProcessRequested".into(),
            payload: json!({"operationId": operation.0.as_str(), "executableRef": parsed.spec.executable_ref.as_str(), "repeats": source.0.as_str()}),
        })?;
        let entry = tx.new_id(IdKind::QueueEntry);
        let now = tx.now_ms();
        tx.queue().enqueue(NewQueueEntry {
            id: entry,
            consumer: WATCH_CONSUMER.into(),
            event_id: event,
            root_id: job.root_id.clone(),
            operation_id: Some(operation.clone()),
            partition: None,
            priority: 0,
            ready_at_ms: now,
            payload: json!({"operationId":operation.0.as_str()}),
        })?;
        Ok(operation)
    }

    /// Requests a stop of the process `process_id` launched, at its current revision, through
    /// the `process.cancel` path (before launch it cancels the undispatched intent).
    pub(crate) fn request_cancel(
        &self,
        ctx: &InvocationContext,
        process_id: &Id,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let revision = match tx.resources().process(process_id)? {
            Some(process) => process.revision,
            None => tx
                .operations()
                .operation(&OperationId(process_id.clone()))?
                .map_or(1, |operation| operation.revision),
        };
        let input = ops::CancelInput {
            process_id: process_id.clone(),
            expected_revision: rev(revision),
        };
        self.decide_cancel(ctx, &input, tx)
    }

    /// The redacted output tail of a running watcher process, and whether earlier bytes were
    /// discarded. `None` once the process settled (its tail is in the completion).
    pub fn live_tail(&self, process_id: &Id) -> Option<(Vec<u8>, bool)> {
        let (tail, redactions) = self.live_tails.locked().get(process_id.as_str()).cloned()?;
        let tail = tail.locked();
        Some((
            redacted_tail(&tail.retained(), &redactions, tail.truncated(), true),
            tail.truncated(),
        ))
    }

    fn bound_spec(
        &self,
        input: &ProcessSpec,
        ctx: &InvocationContext,
    ) -> Result<NativeProcessSpec, BindingRefusal> {
        self.bindings_with_root(&input.working_directory.root_id, ctx)?
            .resolve(input, ctx.project_id())
    }

    /// The static bindings, plus `root_id` resolved from storage when no static binding names
    /// it: a live worktree this caller owns whose directory is still the recorded one, or a
    /// project member repository resolved by [`Self::repository_root`].
    fn bindings_with_root(
        &self,
        root_id: &Id,
        ctx: &InvocationContext,
    ) -> Result<ToolBindings, BindingRefusal> {
        let mut bindings = self.bindings.clone();
        if !bindings.roots.contains_key(root_id.as_str()) {
            let path = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
                if let Some(tree) = snapshot.git().git_worktree(root_id)? {
                    let usable = visible_to(snapshot, ctx, &tree.repository_id)?
                        && owned_by(&tree, ctx)
                        && self.at_recorded_location(&tree);
                    return Ok(if usable {
                        Ok(PathBuf::from(tree.path))
                    } else {
                        Err(BindingRefusal::UnknownRoot)
                    });
                }
                match snapshot.projects().repository(root_id)? {
                    Some(repository)
                        if repository.lifecycle != "Deleted"
                            && visible_to(snapshot, ctx, &repository.id)? =>
                    {
                        self.repository_root(snapshot, ctx, &repository)
                    }
                    _ => Ok(Err(BindingRefusal::UnknownRoot)),
                }
            })
            .map_err(|_| BindingRefusal::ChangedRoot)??;
            bindings.roots.insert(
                root_id.as_str().to_owned(),
                RegisteredWorkingRoot {
                    id: root_id.clone(),
                    project: ctx.project_id().cloned(),
                    path,
                },
            );
        }
        Ok(bindings)
    }

    /// The one working-root rule for a registered repository, shared by `process.execute`
    /// (and the watchers that launch through it) and `process.install_project_dependencies`:
    /// the caller's own live worktree of the repository in its current root and assignment
    /// when it has one, otherwise the repository's primary checkout. A worktree the caller owns
    /// whose directory is no longer the recorded one is refused rather than replaced by the
    /// primary checkout.
    fn repository_root(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        repository: &StoredRepository,
    ) -> Result<Result<PathBuf, BindingRefusal>, StorageError> {
        if let Some(work) = ctx.root_id() {
            let mut after = None;
            let mut moved = false;
            loop {
                let page = snapshot.git().git_worktrees(&work.0, after.as_ref(), 500)?;
                if page.is_empty() {
                    break;
                }
                for tree in &page {
                    if tree.repository_id != repository.id || !owned_by(tree, ctx) {
                        continue;
                    }
                    if self.at_recorded_location(tree) {
                        return Ok(Ok(PathBuf::from(&tree.path)));
                    }
                    moved = true;
                }
                after = page.last().map(|tree| tree.id.clone());
            }
            if moved {
                return Ok(Err(BindingRefusal::ChangedRoot));
            }
        }
        Ok(Ok(PathBuf::from(&repository.location)))
    }

    /// Whether the worktree's directory is still the one recorded when it was created.
    fn at_recorded_location(&self, tree: &StoredGitWorktree) -> bool {
        let Some(location) = &tree.location else {
            return false;
        };
        self.fs
            .resolve(&tree.path, None)
            .ok()
            .and_then(|resolved| resolved.target.and_then(|target| target.identity))
            == Some(location.root)
    }
}

impl CommandHandler for ProcessToolService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        _env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        Ok(match command.input() {
            OperationInput::Process(ops::Input::Execute(input)) => {
                Some(Prepared::new(self.bound_spec(&input.spec, ctx)))
            }
            OperationInput::Process(ops::Input::InstallProjectDependencies(input)) => {
                Some(Prepared::new(self.install_plan(ctx, input)))
            }
            _ => None,
        })
    }

    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let input = match command.input() {
            OperationInput::Process(ops::Input::Execute(input)) => input,
            OperationInput::Process(ops::Input::Cancel(input)) => {
                return self.decide_cancel(ctx, input, tx);
            }
            OperationInput::Process(ops::Input::InstallProjectDependencies(input)) => {
                return self.decide_install(ctx, command, input, tx);
            }
            OperationInput::Process(ops::Input::RequestSystemInstall(input)) => {
                return self.decide_request_system_install(ctx, input, tx);
            }
            _ => {
                return decided(
                    tx,
                    Err(
                        Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None)
                            .into(),
                    ),
                );
            }
        };
        let Some(prepared) = command.prepared::<Result<NativeProcessSpec, BindingRefusal>>() else {
            return decided(
                tx,
                Err(Refusal::new(DomainErrorCode::Conflict, "process_preparation", None).into()),
            );
        };
        if let Err(reason) = prepared {
            return decided(tx, Err(binding_refusal(*reason).into()));
        }
        let operation = self.record_execute(
            ctx,
            command.fingerprint(),
            &input.spec,
            ExecuteOrigin::Tool,
            tx,
        )?;
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: operation.clone(),
            },
            operation_id: Some(operation),
        })
    }
}

impl QueryHandler for ProcessToolService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let OperationInput::Process(ops::Input::Output(input)) = query.input() else {
            return Err(
                Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None)
                    .fault(self.correlation()),
            );
        };
        let not_found = || {
            Refusal::new(
                DomainErrorCode::NotFound,
                "process",
                Some(input.process_id.clone()),
            )
            .fault(self.correlation())
        };
        let process = snapshot
            .resources()
            .process(&input.process_id)
            .map_err(|error| query_fault(error.into(), "process", self.correlation()))?;
        let Some(process) = process else {
            let operation = snapshot
                .operations()
                .operation(&OperationId(input.process_id.clone()))
                .map_err(|error| query_fault(error.into(), "operation", self.correlation()))?
                .filter(|operation| {
                    matches!(
                        operation.capability.as_str(),
                        "process.execute" | "process.install_project_dependencies"
                    )
                })
                .ok_or_else(not_found)?;
            let (project, actor) = if operation.capability == "process.execute" {
                let job = serde_json::from_value::<ExecuteJob>(operation.payload.clone())
                    .map_err(|_| not_found())?;
                (job.project, job.actor)
            } else {
                let job = serde_json::from_value::<InstallJob>(operation.payload.clone())
                    .map_err(|_| not_found())?;
                (job.project, job.actor)
            };
            if project.as_ref() != ctx.project_id()
                || (actor != *ctx.principal()
                    && !matches!(
                        ctx.principal_kind(),
                        PrincipalKind::Human | PrincipalKind::Host
                    ))
            {
                return Err(not_found());
            }
            let value = ProcessOutput {
                process_id: input.process_id.clone(),
                revision: rev(operation.revision),
                state: operation.state,
                first_byte: counter(0),
                next_byte: counter(0),
                content: BoundedText(String::new()),
                encoding: OutputEncoding::Base64,
                truncated: false,
                complete: operation.state == OperationState::CancelledBeforeDispatch,
            };
            return Ok(QueryAnswer {
                stamp: snapshot
                    .stamp()
                    .read_stamp(ctx.policy_revision().clone(), rev(1)),
                result: OperationResult::ProcessOutput { value },
            });
        };
        let same_project = process.project_id.as_ref() == ctx.project_id();
        let owner = process.actor == *ctx.principal();
        if !same_project
            || (!owner
                && !matches!(
                    ctx.principal_kind(),
                    PrincipalKind::Human | PrincipalKind::Host
                ))
        {
            return Err(not_found());
        }
        let operation = match process.operation_id.as_ref() {
            Some(id) => snapshot
                .operations()
                .operation(id)
                .map_err(|error| query_fault(error.into(), "operation", self.correlation()))?,
            None => None,
        }
        .filter(|operation| {
            matches!(
                operation.capability.as_str(),
                "process.execute" | "process.install_project_dependencies"
            )
        })
        .ok_or_else(not_found)?;
        // A watched process's effect completes at launch; its monitor records the process's
        // own completion later as `watchOutcome`. Only that is the end of its output.
        let watch_outcome = operation.attempts.iter().rev().find_map(|attempt| {
            attempt
                .observations
                .iter()
                .rev()
                .find_map(|o| o.get("watchOutcome").cloned())
        });
        let launch_only = watch_outcome.is_none()
            && operation.latest_attempt().is_some_and(|attempt| {
                attempt.observations.iter().any(|observation| {
                    observation.pointer("/effect/evidence/launched") == Some(&Value::Bool(true))
                })
            });
        let output_of = |evidence: &Value| {
            evidence
                .get("outputBase64")
                .and_then(Value::as_str)
                .and_then(|value| base64::engine::general_purpose::STANDARD.decode(value).ok())
                .or_else(|| {
                    evidence
                        .get("outputUtf8")
                        .and_then(Value::as_str)
                        .map(|value| value.as_bytes().to_vec())
                })
        };
        let effect = |field: &str| {
            operation.latest_attempt().and_then(|attempt| {
                attempt
                    .observations
                    .iter()
                    .rev()
                    .find_map(|observation| observation.pointer(field).cloned())
            })
        };
        let (evidence, completion_truncated) = match &watch_outcome {
            Some(outcome) => (
                output_of(outcome),
                outcome.get("truncated").and_then(Value::as_bool),
            ),
            None => (
                operation.latest_attempt().and_then(|attempt| {
                    attempt.observations.iter().rev().find_map(|observation| {
                        observation.pointer("/effect/evidence").and_then(output_of)
                    })
                }),
                effect("/effect/evidence/truncated").and_then(|v| v.as_bool()),
            ),
        };
        let completed = operation.state == OperationState::ObservedComplete && !launch_only;
        let live = self.live_output.locked().get(process.id.as_str()).cloned();
        // Launched but no longer monitored and never observed ending: its output is unknown.
        let unobserved = launch_only && live.is_none() && !self.is_monitoring(&operation.id);
        if (completed && evidence.is_none()) || unobserved {
            return Err(Refusal::new(
                DomainErrorCode::OutcomeUnknown,
                "process_output",
                Some(process.id),
            )
            .fault(self.correlation()));
        }
        let (bytes, live_truncated) = if let Some(evidence) = evidence {
            (evidence, false)
        } else if let Some(live) = live {
            let job = serde_json::from_value::<ExecuteJob>(operation.payload.clone()).ok();
            // Credential output cannot be shown until the effect owner has redacted it. A
            // query cannot safely re-resolve a credential just to scrub a live pipe page.
            if job.as_ref().is_some_and(|job| {
                job.spec
                    .environment
                    .0
                    .iter()
                    .any(|e| e.credential_ref.is_some())
            }) {
                return Ok(QueryAnswer {
                    stamp: snapshot
                        .stamp()
                        .read_stamp(ctx.policy_revision().clone(), rev(1)),
                    result: OperationResult::ProcessOutput {
                        value: ProcessOutput {
                            process_id: process.id,
                            revision: rev(process.revision),
                            state: OperationState::Dispatching,
                            first_byte: counter(0),
                            next_byte: counter(0),
                            content: BoundedText(String::new()),
                            encoding: OutputEncoding::Base64,
                            truncated: false,
                            complete: false,
                        },
                    },
                });
            }
            let live = live.locked();
            let literals: Vec<String> = job
                .into_iter()
                .flat_map(|job| job.spec.environment.0.into_iter())
                .filter_map(|entry| entry.value.map(|value| value.0))
                .collect();
            (
                redacted_output(live.retained(), &literals, true),
                live.truncated()
                    || literals
                        .iter()
                        .any(|value| !value.is_empty() && value.len() < 4),
            )
        } else {
            (Vec::new(), false)
        };
        // Base64 keeps byte offsets stable for arbitrary requested boundaries and avoids an
        // accidental split of a multibyte UTF-8 character. 48 KiB encodes within Content's
        // 64 KiB bound, regardless of the caller's larger ByteLimit.
        let first = (input.after_bytes.get().max(0) as usize).min(bytes.len());
        let end = first
            .saturating_add(input.max_bytes.get().min(49_152) as usize)
            .min(bytes.len());
        let state = match process.phase {
            ProcessPhase::Launching | ProcessPhase::Running | ProcessPhase::Stopping => {
                OperationState::Dispatching
            }
            ProcessPhase::Reaped => OperationState::ObservedComplete,
            ProcessPhase::Failed => OperationState::Failed,
            ProcessPhase::Uncertain | ProcessPhase::Lost => OperationState::OutcomeUnknown,
        };
        let value = ProcessOutput {
            process_id: process.id,
            revision: rev(process.revision),
            state,
            first_byte: counter(first as i64),
            next_byte: counter(end as i64),
            content: BoundedText(
                base64::engine::general_purpose::STANDARD.encode(&bytes[first..end]),
            ),
            encoding: OutputEncoding::Base64,
            truncated: live_truncated || completion_truncated.unwrap_or(false),
            complete: completed,
        };
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(1)),
            result: OperationResult::ProcessOutput { value },
        })
    }
}

impl JournaledEffect for Arc<ProcessToolService> {
    fn execute(&self, operation: &OperationRecord) -> EffectOutcome {
        self.execute_detached(operation)
    }
    fn observe(&self, operation: &OperationRecord) -> EffectOutcome {
        ProcessToolService::observe(self, operation)
    }
}

#[cfg(test)]
#[path = "tool_modules/tests.rs"]
mod tests;
