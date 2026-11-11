//! Host-owned executable and working-root bindings for the public process operations.
//!
//! Wire IDs are selectors only. A caller cannot supply a program path or an ambient working
//! directory through `ProcessSpec`; the host resolves both against its own registrations and
//! rechecks the selected files immediately before launch. A project-scoped invocation may use
//! only roots registered to that project. `Full` is ambient execution with normal user rights,
//! not an OS sandbox. The current authority policy requires an exact native exception under
//! Project scope; its effective execution profile is still `Full`. No confined launcher is
//! qualified, so `Project` and `Confined` are refused instead of silently upgraded.
use super::{
    LaunchPolicy, LaunchRefusal, LaunchRequest, OwnedProcess, ProcessSupervisor, StopOutcome,
};
use crate::authority::destructive::{FilesystemResolver, ProtectedRoots};
use crate::authority::redact::Redactor;
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::Fingerprint;
use crate::fingerprint::sha256_hex;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, process as ops};
use crate::ports::process::{NativeIdentity, ProcessSpec as NativeProcessSpec};
use crate::ports::secrets::{SecretKey, SecretStore};
use crate::ports::storage::{
    ApplicationStore, ArtifactState, ConfigScope, DeliveryFilter, NewEvent, NewOperation,
    NewQueueEntry, OperationRecord, ProcessPhase, ReadOptions, ReadSnapshot, StorageError,
    StoredApproval, WriteTransaction, read_snapshot,
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
use crate::services::sessions::{Refusal, counter, decided, query_fault, rev};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct RegisteredExecutable {
    pub id: Id,
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct RegisteredWorkingRoot {
    pub id: Id,
    pub project: Option<ProjectId>,
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct RegisteredCredential {
    pub id: Id,
    pub project: Option<ProjectId>,
    pub handle: Id,
}

#[derive(Clone, Debug, Default)]
pub struct ToolBindings {
    executables: HashMap<String, (PathBuf, PathBuf)>,
    roots: HashMap<String, RegisteredWorkingRoot>,
    credentials: HashMap<String, RegisteredCredential>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingRefusal {
    UnknownExecutable,
    UnknownRoot,
    OutOfProject,
    ChangedExecutable,
    ChangedRoot,
    UnknownCredential,
    DuplicateEnvironment,
    InvalidLimits,
    UnsupportedProfile,
    UnknownRepository,
    StaleRepository,
    UnknownManifest,
    ChangedManifest,
    UnsupportedManifest,
    MissingWorktree,
}

impl ToolBindings {
    pub fn resolve_container_credential(
        &self,
        reference: &Id,
        project: Option<&ProjectId>,
        secrets: &dyn SecretStore,
        namespace: &str,
    ) -> Result<String, BindingRefusal> {
        let registered = self
            .credentials
            .get(reference.as_str())
            .ok_or(BindingRefusal::UnknownCredential)?;
        if registered.project.as_ref() != project {
            return Err(BindingRefusal::OutOfProject);
        }
        secrets
            .get(&SecretKey::new(namespace, registered.handle.clone()))
            .ok()
            .flatten()
            .map(|secret| secret.expose().to_owned())
            .ok_or(BindingRefusal::UnknownCredential)
    }

    /// Resolve container environment entries only after its command intent has committed.
    /// Registered handles stay project-bound and values never enter the receipt or CLI argv.
    pub fn resolve_container_environment(
        &self,
        entries: &[crate::protocol::records::EnvironmentEntry],
        project: Option<&ProjectId>,
        secrets: &dyn SecretStore,
        namespace: &str,
    ) -> Result<Vec<(String, String)>, BindingRefusal> {
        let mut names = HashSet::new();
        let mut values = Vec::with_capacity(entries.len());
        for entry in entries {
            if !names.insert(entry.name.0.as_str()) {
                return Err(BindingRefusal::DuplicateEnvironment);
            }
            let value = match (&entry.value, &entry.credential_ref) {
                (Some(value), None) => value.0.clone(),
                (None, Some(reference)) => {
                    let registered = self
                        .credentials
                        .get(reference.as_str())
                        .ok_or(BindingRefusal::UnknownCredential)?;
                    if registered.project.as_ref() != project {
                        return Err(BindingRefusal::OutOfProject);
                    }
                    secrets
                        .get(&SecretKey::new(namespace, registered.handle.clone()))
                        .ok()
                        .flatten()
                        .ok_or(BindingRefusal::UnknownCredential)?
                        .expose()
                        .to_owned()
                }
                _ => return Err(BindingRefusal::UnknownCredential),
            };
            values.push((entry.name.0.clone(), value));
        }
        Ok(values)
    }

    /// Resolve a project path under its exact registered root, rejecting symlink escapes.
    pub fn resolve_registered_path(
        &self,
        path: &RegisteredPath,
        project: Option<&ProjectId>,
    ) -> Result<PathBuf, BindingRefusal> {
        let root = self
            .roots
            .get(path.root_id.as_str())
            .ok_or(BindingRefusal::UnknownRoot)?;
        if root.project.as_ref() != project {
            return Err(BindingRefusal::OutOfProject);
        }
        let root_path = exact_directory(&root.path).ok_or(BindingRefusal::ChangedRoot)?;
        let resolved = root_path
            .join(&path.relative.0)
            .canonicalize()
            .map_err(|_| BindingRefusal::ChangedRoot)?;
        if !resolved.starts_with(&root_path) {
            return Err(BindingRefusal::OutOfProject);
        }
        Ok(resolved)
    }

    /// A backend selector resolves only a host-registered executable, rechecking its exact
    /// target at dispatch. Missing runtimes remain unavailable; no PATH fallback occurs here.
    pub fn resolve_named_executable(&self, name: &str) -> Result<String, BindingRefusal> {
        let (path, target) = self
            .executables
            .get(name)
            .ok_or(BindingRefusal::UnknownExecutable)?;
        exact_file(path, target)
            .map(|path| path.to_string_lossy().into_owned())
            .ok_or(BindingRefusal::ChangedExecutable)
    }

    /// Duplicate IDs fail closed; a host must never let registration order select an executable.
    pub fn new(
        executables: impl IntoIterator<Item = RegisteredExecutable>,
        roots: impl IntoIterator<Item = RegisteredWorkingRoot>,
    ) -> Result<Self, &'static str> {
        let mut bindings = Self::default();
        for executable in executables {
            let target = executable
                .path
                .canonicalize()
                .map_err(|_| "process executable is unavailable")?;
            if !target.is_file() {
                return Err("process executable is not a file");
            }
            if bindings
                .executables
                .insert(executable.id.as_str().to_owned(), (executable.path, target))
                .is_some()
            {
                return Err("duplicate process executable ID");
            }
        }
        for root in roots {
            if bindings
                .roots
                .insert(root.id.as_str().to_owned(), root)
                .is_some()
            {
                return Err("duplicate process working-root ID");
            }
        }
        Ok(bindings)
    }

    pub fn with_credentials(
        mut self,
        credentials: impl IntoIterator<Item = RegisteredCredential>,
    ) -> Result<Self, &'static str> {
        for credential in credentials {
            if self
                .credentials
                .insert(credential.id.as_str().to_owned(), credential)
                .is_some()
            {
                return Err("duplicate process credential ID");
            }
        }
        Ok(self)
    }

    /// Resolves a frozen wire spec to the native process owner. This is an identity check,
    /// not confinement: the supervisor still runs current authority, shell safety and resource
    /// preflight. Only the ambient `Full` profile has a qualified launch path today.
    pub fn resolve(
        &self,
        input: &ProcessSpec,
        project: Option<&ProjectId>,
    ) -> Result<NativeProcessSpec, BindingRefusal> {
        if !matches!(input.profile, ToolProfile::Full) {
            return Err(BindingRefusal::UnsupportedProfile);
        }
        // Wire Counters admit zero and very large values for other operations. A process needs
        // finite nonzero bounds before it can take a worker or retain output.
        if !(1..=3_600_000).contains(&input.timeout_ms.get())
            || !(1..=1_048_576).contains(&input.output_limit_bytes.get())
        {
            return Err(BindingRefusal::InvalidLimits);
        }
        let mut names = HashSet::new();
        if input
            .environment
            .0
            .iter()
            .any(|entry| !names.insert(entry.name.0.as_str()))
        {
            return Err(BindingRefusal::DuplicateEnvironment);
        }
        for entry in &input.environment.0 {
            if let Some(reference) = &entry.credential_ref {
                let credential = self
                    .credentials
                    .get(reference.as_str())
                    .ok_or(BindingRefusal::UnknownCredential)?;
                if credential.project.as_ref() != project {
                    return Err(BindingRefusal::OutOfProject);
                }
            }
        }
        let executable = self
            .executables
            .get(input.executable_ref.as_str())
            .ok_or(BindingRefusal::UnknownExecutable)?;
        let root = self
            .roots
            .get(input.working_directory.root_id.as_str())
            .ok_or(BindingRefusal::UnknownRoot)?;
        if let Some(project) = project
            && root.project.as_ref() != Some(project)
        {
            return Err(BindingRefusal::OutOfProject);
        }
        let program =
            exact_file(&executable.0, &executable.1).ok_or(BindingRefusal::ChangedExecutable)?;
        let root_path = exact_directory(&root.path).ok_or(BindingRefusal::ChangedRoot)?;
        let cwd = exact_directory(&root_path.join(&input.working_directory.relative.0))
            .filter(|p| p.starts_with(&root_path))
            .ok_or(BindingRefusal::ChangedRoot)?;
        Ok(NativeProcessSpec {
            program: program.to_string_lossy().into_owned(),
            args: input.args.0.iter().map(|x| x.0.clone()).collect(),
            cwd: Some(cwd),
            env: input
                .environment
                .0
                .iter()
                .filter_map(|entry| {
                    entry
                        .value
                        .as_ref()
                        .map(|value| (entry.name.0.clone(), value.0.clone()))
                })
                .collect(),
            clear_env: false,
            capture_output: true,
            merge_output: false,
            no_child_processes: false,
            control_pipe: None,
            pipe_stdin: false,
            stdin: None,
        })
    }

    /// Resolve only pre-registered handles, after the journal intent and current authority
    /// check. No value is serialized into an operation, receipt or history event.
    fn resolve_credentials(
        &self,
        input: &ProcessSpec,
        project: Option<&ProjectId>,
        secrets: &dyn SecretStore,
        namespace: &str,
        spec: &mut NativeProcessSpec,
    ) -> Result<Vec<String>, BindingRefusal> {
        let mut redactions = Vec::new();
        for entry in &input.environment.0 {
            let Some(reference) = &entry.credential_ref else {
                continue;
            };
            let registered = self
                .credentials
                .get(reference.as_str())
                .ok_or(BindingRefusal::UnknownCredential)?;
            if registered.project.as_ref() != project {
                return Err(BindingRefusal::OutOfProject);
            }
            let secret = secrets
                .get(&SecretKey::new(namespace, registered.handle.clone()))
                .ok()
                .flatten()
                .ok_or(BindingRefusal::UnknownCredential)?;
            redactions.push(secret.expose().to_owned());
            spec.env
                .push((entry.name.0.clone(), secret.expose().to_owned()));
        }
        Ok(redactions)
    }
}

fn exact_file(path: &Path, target: &Path) -> Option<PathBuf> {
    let actual = path.canonicalize().ok()?;
    (actual == target && actual.is_file()).then_some(path.to_path_buf())
}

fn exact_directory(path: &Path) -> Option<PathBuf> {
    let actual = path.canonicalize().ok()?;
    (actual == path && actual.is_dir()).then_some(actual)
}

/// Secret values can straddle a capture page or budget boundary. Remove any retained suffix
/// matching the beginning of a secret before publishing an incomplete output prefix.
pub(crate) fn redacted_output(bytes: &[u8], values: &[String], incomplete: bool) -> Vec<u8> {
    if values.is_empty() {
        return bytes.to_vec();
    }
    // The shared redactor intentionally ignores values shorter than four bytes. A process
    // credential has no such length guarantee, so suppress the body in that case.
    if values
        .iter()
        .any(|value| !value.is_empty() && value.len() < 4)
    {
        return Vec::new();
    }
    let mut redactor = Redactor::new();
    for value in values {
        redactor.register(value);
    }
    let mut remaining = bytes;
    let mut redacted = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(text) => {
                redacted.extend_from_slice(redactor.redact_text(text).as_bytes());
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    // A registered credential is valid UTF-8, so its bytes cannot straddle an
                    // invalid byte. Redact each valid span without changing unrelated binary.
                    let text = std::str::from_utf8(&remaining[..valid]).expect("valid prefix");
                    redacted.extend_from_slice(redactor.redact_text(text).as_bytes());
                }
                let invalid = error.error_len().unwrap_or(remaining.len() - valid);
                redacted.extend_from_slice(&remaining[valid..valid + invalid]);
                remaining = &remaining[valid + invalid..];
            }
        }
    }
    // Redact complete matches first. Trimming a repeated secret's suffix beforehand can
    // split its full match and expose the retained prefix.
    if incomplete {
        let mut trim = 0;
        for value in values {
            let secret = value.as_bytes();
            for length in 1..secret.len() {
                if redacted.ends_with(&secret[..length]) {
                    trim = trim.max(length);
                }
            }
        }
        redacted.truncate(redacted.len() - trim);
    }
    redacted
}

/// A capture budget applies to the combined stdout and stderr stream. Readers continue draining
/// after the budget is reached so a child cannot remain blocked on a full pipe; `seen` records
/// the discarded tail without retaining it. Only the retained prefix may be paged to callers.
#[derive(Clone, Debug)]
pub struct BoundedOutput {
    bytes: Vec<u8>,
    seen: u64,
    start: u64,
    limit: usize,
}

pub const EFFECT_CONSUMER: &str = "process_tool_effects";
pub const CANCEL_CONSUMER: &str = "process_tool_cancellations";
/// Processes launched by watchers run on their own consumer, so a long-running watched process
/// never takes a `process.execute` worker slot.
pub const WATCH_CONSUMER: &str = "watch_process_effects";

/// The last `limit` bytes of a combined output stream (a watcher reports the end of a run).
#[derive(Clone, Debug)]
pub struct TailOutput {
    bytes: std::collections::VecDeque<u8>,
    seen: u64,
    limit: usize,
}
impl TailOutput {
    pub fn new(limit: usize) -> Self {
        Self {
            bytes: std::collections::VecDeque::with_capacity(limit),
            seen: 0,
            limit,
        }
    }
    pub fn append(&mut self, chunk: &[u8]) {
        self.seen = self.seen.saturating_add(chunk.len() as u64);
        let keep = &chunk[chunk.len().saturating_sub(self.limit)..];
        let overflow = (self.bytes.len() + keep.len()).saturating_sub(self.limit);
        self.bytes.drain(..overflow.min(self.bytes.len()));
        self.bytes.extend(keep);
    }
    /// Earlier bytes were discarded.
    pub fn truncated(&self) -> bool {
        self.seen > self.bytes.len() as u64
    }
    pub fn retained(&self) -> Vec<u8> {
        self.bytes.iter().copied().collect()
    }
}

/// Redacts a tail whose front may be cut: a credential's suffix at the start of the retained
/// bytes is withheld along with complete matches.
pub(crate) fn redacted_tail(bytes: &[u8], values: &[String], cut_front: bool) -> Vec<u8> {
    let mut redacted = redacted_output(bytes, values, false);
    if cut_front
        && !values
            .iter()
            .any(|value| !value.is_empty() && value.len() < 4)
    {
        let mut trim = 0;
        for value in values {
            let secret = value.as_bytes();
            for length in 1..secret.len() {
                if redacted.starts_with(&secret[secret.len() - length..]) {
                    trim = trim.max(length);
                }
            }
        }
        redacted.drain(..trim);
    }
    redacted
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ExecuteJob {
    actor: crate::protocol::ActorId,
    kind: String,
    session: Option<Id>,
    project: Option<ProjectId>,
    root: Option<crate::protocol::RootId>,
    assignment: Option<crate::protocol::AssignmentId>,
    restore_epoch: i64,
    policy_revision: i64,
    fingerprint: String,
    #[serde(default)]
    authority_sources: Vec<Id>,
    spec: ProcessSpec,
    /// A watcher's output tail budget; zero (omitted) for a plain `process.execute`.
    #[serde(default, skip_serializing_if = "is_zero")]
    tail_bytes: u32,
    /// Set when `watch.create` admitted this launch: the effect re-authorizes that request (its
    /// grant binds the watch.create fingerprint), not a `process.execute` request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watch: Option<WatchOrigin>,
}
fn is_zero(value: &u32) -> bool {
    *value == 0
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct WatchOrigin {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    requested_tail_bytes: Option<u32>,
}

/// Who admitted a process launch, which decides its consumer and its re-authorization.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ExecuteOrigin {
    /// `process.execute`.
    Tool,
    /// `watch.create`, with its effective and requested tail budgets.
    Watch {
        tail_bytes: u32,
        requested: Option<u32>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InstallJob {
    actor: crate::protocol::ActorId,
    kind: String,
    session: Option<Id>,
    project: Option<ProjectId>,
    root: Option<crate::protocol::RootId>,
    assignment: Option<crate::protocol::AssignmentId>,
    restore_epoch: i64,
    policy_revision: i64,
    fingerprint: String,
    #[serde(default)]
    authority_sources: Vec<Id>,
    repository_id: Id,
    expected_revision: crate::protocol::Revision,
    manifest: ArtifactRef,
    profile: ToolProfile,
}

impl InstallJob {
    fn from_context(
        ctx: &InvocationContext,
        fingerprint: &Fingerprint,
        input: &ops::InstallProjectDependenciesInput,
    ) -> Self {
        Self {
            actor: ctx.principal().clone(),
            kind: format!("{:?}", ctx.principal_kind()),
            session: ctx.session_id().cloned(),
            project: ctx.project_id().cloned(),
            root: ctx.root_id().cloned(),
            assignment: ctx.assignment_id().cloned(),
            restore_epoch: ctx.restore_epoch().get(),
            policy_revision: ctx.policy_revision().get(),
            fingerprint: fingerprint.as_str().into(),
            authority_sources: ctx.authority_sources().to_vec(),
            repository_id: input.repository_id.clone(),
            expected_revision: input.expected_revision.clone(),
            manifest: input.manifest.clone(),
            profile: input.profile.clone(),
        }
    }
    fn context(&self, store: &dyn ApplicationStore, operation: &OperationId) -> InvocationContext {
        process_context(
            store,
            operation,
            &self.actor,
            &self.kind,
            self.session.as_ref(),
            self.project.as_ref(),
            self.root.as_ref(),
            self.assignment.as_ref(),
            self.restore_epoch,
            self.policy_revision,
            &self.authority_sources,
        )
    }
    fn input(&self) -> ops::InstallProjectDependenciesInput {
        ops::InstallProjectDependenciesInput {
            repository_id: self.repository_id.clone(),
            expected_revision: self.expected_revision.clone(),
            manifest: self.manifest.clone(),
            profile: self.profile.clone(),
        }
    }
}

struct InstallPlan {
    spec: NativeProcessSpec,
}

impl ExecuteJob {
    fn from_context(ctx: &InvocationContext, fingerprint: &Fingerprint, spec: ProcessSpec) -> Self {
        Self {
            actor: ctx.principal().clone(),
            kind: format!("{:?}", ctx.principal_kind()),
            session: ctx.session_id().cloned(),
            project: ctx.project_id().cloned(),
            root: ctx.root_id().cloned(),
            assignment: ctx.assignment_id().cloned(),
            restore_epoch: ctx.restore_epoch().get(),
            policy_revision: ctx.policy_revision().get(),
            fingerprint: fingerprint.as_str().to_owned(),
            authority_sources: ctx.authority_sources().to_vec(),
            spec,
            tail_bytes: 0,
            watch: None,
        }
    }

    fn context(&self, store: &dyn ApplicationStore, operation: &OperationId) -> InvocationContext {
        process_context(
            store,
            operation,
            &self.actor,
            &self.kind,
            self.session.as_ref(),
            self.project.as_ref(),
            self.root.as_ref(),
            self.assignment.as_ref(),
            self.restore_epoch,
            self.policy_revision,
            &self.authority_sources,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn process_context(
    store: &dyn ApplicationStore,
    operation: &OperationId,
    actor: &crate::protocol::ActorId,
    kind: &str,
    session: Option<&Id>,
    project: Option<&ProjectId>,
    root: Option<&crate::protocol::RootId>,
    assignment: Option<&crate::protocol::AssignmentId>,
    restore_epoch: i64,
    policy_revision: i64,
    authority_sources: &[Id],
) -> InvocationContext {
    let kind = match kind {
        "Human" => PrincipalKind::Human,
        "Agent" => PrincipalKind::Agent,
        "Package" => PrincipalKind::Package,
        "McpClient" => PrincipalKind::McpClient,
        "Schedule" => PrincipalKind::Schedule,
        _ => PrincipalKind::Host,
    };
    let mut context = InvocationContext::new(
        actor.clone(),
        kind,
        operation.0.clone(),
        Id::new(EFFECT_CONSUMER).expect("process effect namespace"),
        store.info().store_id,
        counter(restore_epoch),
        rev(policy_revision),
    )
    .with_lineage(project.cloned(), root.cloned(), assignment.cloned());
    if let Some(session) = session {
        context = context.with_session(session.clone());
    }
    for source in authority_sources {
        context = context.with_authority_source(source.clone());
    }
    context
}

pub struct ProcessToolService {
    store: Arc<dyn ApplicationStore>,
    processes: Arc<ProcessSupervisor>,
    authority: Arc<dyn AuthorityPort>,
    secrets: Arc<dyn SecretStore>,
    bindings: ToolBindings,
    fs: Arc<dyn FilesystemResolver>,
    protected_roots: ProtectedRoots,
    ids: Arc<dyn IdSource>,
    live_output: Mutex<HashMap<String, Arc<Mutex<BoundedOutput>>>>,
    live_tails: Mutex<HashMap<String, LiveTail>>,
}
type LiveTail = (Arc<Mutex<TailOutput>>, Arc<Vec<String>>);

struct LiveOutputGuard<'a> {
    service: &'a ProcessToolService,
    id: Id,
}
impl Drop for LiveOutputGuard<'_> {
    fn drop(&mut self) {
        self.service
            .live_output
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(self.id.as_str());
        self.service
            .live_tails
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(self.id.as_str());
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum EffectOutcome {
    Completed(Value),
    Failed(String),
    Unknown(String),
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
            live_output: Mutex::new(HashMap::new()),
            live_tails: Mutex::new(HashMap::new()),
        }
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
            } => {
                job.tail_bytes = tail_bytes;
                job.watch = Some(WatchOrigin {
                    requested_tail_bytes: requested,
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
        let (tail, redactions) = self
            .live_tails
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(process_id.as_str())
            .cloned()?;
        let tail = tail.lock().unwrap_or_else(|p| p.into_inner());
        Some((
            redacted_tail(&tail.retained(), &redactions, tail.truncated()),
            tail.truncated(),
        ))
    }

    fn bound_spec(
        &self,
        input: &ProcessSpec,
        ctx: &InvocationContext,
    ) -> Result<NativeProcessSpec, BindingRefusal> {
        let mut bindings = self.bindings.clone();
        let root_id = input.working_directory.root_id.as_str();
        if !bindings.roots.contains_key(root_id) {
            let path = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
                let id = &input.working_directory.root_id;
                if let Some(tree) = snapshot.git().git_worktree(id)? {
                    if tree.removed {
                        return Ok(None);
                    }
                    let visible = match ctx.project_id() {
                        Some(project) => snapshot
                            .projects()
                            .members(project)?
                            .contains(&tree.repository_id),
                        None => true,
                    };
                    let owned = tree.owner == *ctx.principal()
                        && tree.owner_root_id.as_ref() == ctx.root_id()
                        && tree.owner_assignment_id.as_ref() == ctx.assignment_id();
                    if !visible || !owned {
                        return Ok(None);
                    }
                    let Some(location) = &tree.location else {
                        return Ok(None);
                    };
                    let actual =
                        self.fs.resolve(&tree.path, None).ok().and_then(|resolved| {
                            resolved.target.and_then(|target| target.identity)
                        });
                    if actual != Some(location.root) {
                        return Ok(None);
                    }
                    return Ok(Some(PathBuf::from(tree.path)));
                }
                if let Some(repository) = snapshot.projects().repository(id)? {
                    if repository.lifecycle == "Deleted" {
                        return Ok(None);
                    }
                    let visible = match ctx.project_id() {
                        Some(project) => snapshot
                            .projects()
                            .members(project)?
                            .contains(&repository.id),
                        None => true,
                    };
                    if visible {
                        return Ok(Some(PathBuf::from(repository.location)));
                    }
                }
                Ok(None)
            })
            .map_err(|_| BindingRefusal::ChangedRoot)?
            .ok_or(BindingRefusal::UnknownRoot)?;
            bindings.roots.insert(
                root_id.to_owned(),
                RegisteredWorkingRoot {
                    id: input.working_directory.root_id.clone(),
                    project: ctx.project_id().cloned(),
                    path,
                },
            );
        }
        bindings.resolve(input, ctx.project_id())
    }

    fn install_plan(
        &self,
        ctx: &InvocationContext,
        input: &ops::InstallProjectDependenciesInput,
    ) -> Result<InstallPlan, BindingRefusal> {
        if !matches!(input.profile, ToolProfile::Full) {
            return Err(BindingRefusal::UnsupportedProfile);
        }
        let root = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            let Some(repository) = snapshot.projects().repository(&input.repository_id)? else {
                return Ok(Err(BindingRefusal::UnknownRepository));
            };
            if repository.lifecycle == "Deleted" {
                return Ok(Err(BindingRefusal::UnknownRepository));
            }
            if repository.revision != input.expected_revision.get() {
                return Ok(Err(BindingRefusal::StaleRepository));
            }
            if let Some(project) = ctx.project_id()
                && !snapshot
                    .projects()
                    .members(project)?
                    .contains(&repository.id)
            {
                return Ok(Err(BindingRefusal::UnknownRepository));
            }
            let viewer = Viewer::resolve(snapshot.authority(), ctx)?;
            let Some(artifact) = snapshot.artifacts().artifact(&input.manifest.artifact_id)? else {
                return Ok(Err(BindingRefusal::UnknownManifest));
            };
            if artifact.state != ArtifactState::Live
                || !visible(snapshot.artifacts(), &viewer, &artifact)?
            {
                return Ok(Err(BindingRefusal::UnknownManifest));
            }
            let version = snapshot.artifacts().version(&input.manifest.version_id)?;
            let exact = version.is_some_and(|version| {
                version.live()
                    && version.artifact_id == artifact.id
                    && version.blob_digest == input.manifest.sha256.0
                    && version.byte_count == input.manifest.byte_length.get()
                    && version.media_type == input.manifest.media_type
            });
            if !exact {
                return Ok(Err(BindingRefusal::ChangedManifest));
            }
            let path = if let Some(work) = ctx.root_id() {
                let mut after = None;
                let mut selected = None;
                loop {
                    let page = snapshot.git().git_worktrees(&work.0, after.as_ref(), 500)?;
                    if page.is_empty() {
                        break;
                    }
                    for tree in &page {
                        if tree.repository_id == repository.id
                            && !tree.removed
                            && tree.owner == *ctx.principal()
                            && tree.owner_root_id.as_ref() == ctx.root_id()
                            && tree.owner_assignment_id.as_ref() == ctx.assignment_id()
                        {
                            let Some(location) = &tree.location else {
                                continue;
                            };
                            let actual =
                                self.fs.resolve(&tree.path, None).ok().and_then(|resolved| {
                                    resolved.target.and_then(|target| target.identity)
                                });
                            if actual == Some(location.root) {
                                selected = Some(PathBuf::from(&tree.path));
                                break;
                            }
                        }
                    }
                    if selected.is_some() {
                        break;
                    }
                    after = page.last().map(|tree| tree.id.clone());
                }
                selected.ok_or(BindingRefusal::MissingWorktree)
            } else if matches!(
                ctx.principal_kind(),
                PrincipalKind::Human | PrincipalKind::Host
            ) {
                Ok(PathBuf::from(repository.location))
            } else {
                Err(BindingRefusal::MissingWorktree)
            };
            Ok(path)
        })
        .map_err(|_| BindingRefusal::ChangedRoot)??;
        let root = exact_directory(&root).ok_or(BindingRefusal::ChangedRoot)?;
        const MANIFESTS: &[&str] = &[
            "Cargo.toml",
            "Cargo.lock",
            "package-lock.json",
            "package.json",
            "pnpm-lock.yaml",
            "yarn.lock",
            "pyproject.toml",
            "uv.lock",
            "requirements.txt",
            "go.mod",
            "Gemfile.lock",
            "composer.lock",
        ];
        let mut selected = None;
        for name in MANIFESTS {
            let path = root.join(name);
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.file_type().is_file()
                || metadata.len() > 8 * 1024 * 1024
                || metadata.len() != input.manifest.byte_length.get() as u64
            {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if sha256_hex(&bytes) == input.manifest.sha256.0
                && selected.replace((*name).to_owned()).is_some()
            {
                return Err(BindingRefusal::ChangedManifest);
            }
        }
        let name = selected.ok_or(BindingRefusal::ChangedManifest)?;
        let root_text = root.to_string_lossy().into_owned();
        let manifest = root.join(&name).to_string_lossy().into_owned();
        let (program, args, extra_env): (&str, Vec<String>, Vec<(String, String)>) =
            match name.as_str() {
                "Cargo.toml" | "Cargo.lock" => (
                    "cargo",
                    std::iter::once("build".into())
                        .chain((name == "Cargo.lock").then_some("--locked".into()))
                        .chain([
                            "--manifest-path".into(),
                            root.join("Cargo.toml").to_string_lossy().into_owned(),
                        ])
                        .collect(),
                    vec![
                        (
                            "CARGO_HOME".into(),
                            root.join(".devworks-cache/cargo")
                                .to_string_lossy()
                                .into_owned(),
                        ),
                        (
                            "CARGO_TARGET_DIR".into(),
                            root.join("target").to_string_lossy().into_owned(),
                        ),
                    ],
                ),
                "package-lock.json" => (
                    "npm",
                    vec!["ci".into(), "--prefix".into(), root_text.clone()],
                    vec![(
                        "NPM_CONFIG_CACHE".into(),
                        root.join(".devworks-cache/npm")
                            .to_string_lossy()
                            .into_owned(),
                    )],
                ),
                "package.json" => (
                    "npm",
                    vec!["install".into(), "--prefix".into(), root_text.clone()],
                    vec![(
                        "NPM_CONFIG_CACHE".into(),
                        root.join(".devworks-cache/npm")
                            .to_string_lossy()
                            .into_owned(),
                    )],
                ),
                "pnpm-lock.yaml" => (
                    "pnpm",
                    vec![
                        "install".into(),
                        "--frozen-lockfile".into(),
                        "--dir".into(),
                        root_text.clone(),
                    ],
                    vec![],
                ),
                "yarn.lock" => (
                    "yarn",
                    vec![
                        "install".into(),
                        "--frozen-lockfile".into(),
                        "--cwd".into(),
                        root_text.clone(),
                    ],
                    vec![],
                ),
                "pyproject.toml" | "uv.lock" => (
                    "uv",
                    ["sync".into(), "--project".into(), root_text.clone()]
                        .into_iter()
                        .chain((name == "uv.lock").then_some("--frozen".into()))
                        .collect(),
                    vec![(
                        "UV_CACHE_DIR".into(),
                        root.join(".devworks-cache/uv")
                            .to_string_lossy()
                            .into_owned(),
                    )],
                ),
                "requirements.txt" => (
                    "python3",
                    vec![
                        "-m".into(),
                        "pip".into(),
                        "install".into(),
                        "--target".into(),
                        root.join(".devworks-deps/python")
                            .to_string_lossy()
                            .into_owned(),
                        "-r".into(),
                        manifest,
                    ],
                    vec![(
                        "PIP_CACHE_DIR".into(),
                        root.join(".devworks-cache/pip")
                            .to_string_lossy()
                            .into_owned(),
                    )],
                ),
                "go.mod" => (
                    "go",
                    vec!["mod".into(), "download".into()],
                    vec![(
                        "GOMODCACHE".into(),
                        root.join(".devworks-deps/go")
                            .to_string_lossy()
                            .into_owned(),
                    )],
                ),
                "Gemfile.lock" => (
                    "bundle",
                    vec![
                        "install".into(),
                        "--gemfile".into(),
                        root.join("Gemfile").to_string_lossy().into_owned(),
                    ],
                    vec![(
                        "BUNDLE_PATH".into(),
                        root.join("vendor/bundle").to_string_lossy().into_owned(),
                    )],
                ),
                "composer.lock" => (
                    "composer",
                    vec!["install".into(), "--working-dir".into(), root_text.clone()],
                    vec![],
                ),
                _ => return Err(BindingRefusal::UnsupportedManifest),
            };
        let executable = self
            .bindings
            .executables
            .get(program)
            .ok_or(BindingRefusal::UnknownExecutable)?;
        let executable =
            exact_file(&executable.0, &executable.1).ok_or(BindingRefusal::ChangedExecutable)?;
        let mut env = vec![
            (
                "HOME".into(),
                root.join(".devworks-home").to_string_lossy().into_owned(),
            ),
            (
                "TMPDIR".into(),
                root.join(".devworks-tmp").to_string_lossy().into_owned(),
            ),
            ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
        ];
        if program == "cargo" {
            let rustup_home = std::env::var_os("RUSTUP_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup"))
                });
            if let Some(home) = rustup_home.filter(|path| path.is_dir()) {
                env.push(("RUSTUP_HOME".into(), home.to_string_lossy().into_owned()));
            }
        }
        env.extend(extra_env);
        Ok(InstallPlan {
            spec: NativeProcessSpec {
                program: executable.to_string_lossy().into_owned(),
                args,
                cwd: Some(root),
                env,
                clear_env: true,
                capture_output: true,
                merge_output: false,
                no_child_processes: false,
                control_pipe: None,
                pipe_stdin: false,
                stdin: None,
            },
        })
    }

    /// Runs the committed effect once. An unknown observation never launches a second tree.
    pub fn execute(&self, operation: &OperationRecord) -> EffectOutcome {
        if operation.capability == "process.cancel" {
            return self.execute_cancel(operation);
        }
        if operation.capability == "process.install_project_dependencies" {
            return self.execute_install(operation);
        }
        if operation.capability != "process.execute"
            && operation.capability != "process.install_project_dependencies"
        {
            return EffectOutcome::Failed("unsupported process effect".into());
        }
        let Ok(job) = serde_json::from_value::<ExecuteJob>(operation.payload.clone()) else {
            return EffectOutcome::Failed("invalid process intent".into());
        };
        let ctx = job.context(&*self.store, &operation.id);
        let admitted_by = if job.watch.is_some() {
            "watch.create"
        } else {
            "process.execute"
        };
        let Some(descriptor) = crate::registry::find(admitted_by) else {
            return EffectOutcome::Failed("process descriptor unavailable".into());
        };
        let Some(binding) = descriptor.binding(Mode::Command) else {
            return EffectOutcome::Failed("process command binding unavailable".into());
        };
        let Some(fingerprint) = Fingerprint::from_hex(&job.fingerprint) else {
            return EffectOutcome::Failed("invalid process intent fingerprint".into());
        };
        let input = match &job.watch {
            Some(origin) => OperationInput::Watch(crate::operations::watch::Input::Create(
                crate::operations::watch::CreateInput {
                    process: job.spec.clone(),
                    tail_bytes: origin
                        .requested_tail_bytes
                        .and_then(crate::protocol::support::ByteLimit::new),
                },
            )),
            None => OperationInput::Process(ops::Input::Execute(ops::ExecuteInput {
                spec: job.spec.clone(),
            })),
        };
        if self
            .authority
            .authorize(
                &ctx,
                &AuthorityRequest {
                    descriptor,
                    binding,
                    effect: binding.effect,
                    action: None,
                    input: &input,
                    fingerprint: &fingerprint,
                },
            )
            .is_err()
        {
            return EffectOutcome::Failed("current process authority denied dispatch".into());
        }
        let mut spec = match self.bound_spec(&job.spec, &ctx) {
            Ok(spec) => spec,
            Err(reason) => {
                return EffectOutcome::Failed(format!("process binding changed: {reason:?}"));
            }
        };
        let secret_values = match self.bindings.resolve_credentials(
            &job.spec,
            job.project.as_ref(),
            &*self.secrets,
            self.store.info().store_id.0.as_str(),
            &mut spec,
        ) {
            Ok(values) => values,
            Err(_) => return EffectOutcome::Failed("process credential unavailable".into()),
        };
        let redactions = job
            .spec
            .environment
            .0
            .iter()
            .filter_map(|entry| entry.value.as_ref().map(|value| value.0.clone()))
            .chain(secret_values)
            .collect();
        self.run_owned(
            operation,
            &ctx,
            spec,
            job.spec.timeout_ms.get(),
            job.spec.output_limit_bytes.get() as usize,
            job.tail_bytes as usize,
            redactions,
            false,
        )
    }

    fn execute_install(&self, operation: &OperationRecord) -> EffectOutcome {
        let Ok(job) = serde_json::from_value::<InstallJob>(operation.payload.clone()) else {
            return EffectOutcome::Failed("invalid project install intent".into());
        };
        let ctx = job.context(&*self.store, &operation.id);
        let Some(descriptor) = crate::registry::find("process.install_project_dependencies") else {
            return EffectOutcome::Failed("project install descriptor unavailable".into());
        };
        let Some(binding) = descriptor.binding(Mode::Command) else {
            return EffectOutcome::Failed("project install binding unavailable".into());
        };
        let Some(fingerprint) = Fingerprint::from_hex(&job.fingerprint) else {
            return EffectOutcome::Failed("invalid project install fingerprint".into());
        };
        let input = job.input();
        let typed = OperationInput::Process(ops::Input::InstallProjectDependencies(input.clone()));
        if self
            .authority
            .authorize(
                &ctx,
                &AuthorityRequest {
                    descriptor,
                    binding,
                    effect: binding.effect,
                    action: None,
                    input: &typed,
                    fingerprint: &fingerprint,
                },
            )
            .is_err()
        {
            return EffectOutcome::Failed(
                "current project install authority denied dispatch".into(),
            );
        }
        let plan = match self.install_plan(&ctx, &input) {
            Ok(plan) => plan,
            Err(reason) => {
                return EffectOutcome::Failed(format!(
                    "project install source changed: {reason:?}"
                ));
            }
        };
        self.run_owned(
            operation,
            &ctx,
            plan.spec,
            3_600_000,
            1_048_576,
            0,
            Vec::new(),
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn run_owned(
        &self,
        operation: &OperationRecord,
        ctx: &InvocationContext,
        spec: NativeProcessSpec,
        timeout_ms: i64,
        output_limit: usize,
        tail_limit: usize,
        redactions: Vec<String>,
        install: bool,
    ) -> EffectOutcome {
        if install {
            let Some(root) = spec.cwd.as_ref() else {
                return EffectOutcome::Failed("project install has no working root".into());
            };
            for name in [
                ".devworks-home",
                ".devworks-tmp",
                ".devworks-cache",
                ".devworks-deps",
            ] {
                if std::fs::create_dir_all(root.join(name)).is_err() {
                    return EffectOutcome::Failed("project install workspace unavailable".into());
                }
            }
        }
        let policy = LaunchPolicy {
            fs: &*self.fs,
            roots: &self.protected_roots,
            protected_branches: &["main".into(), "master".into()],
        };
        let process = match self.processes.launch_reserved(
            ctx,
            LaunchRequest {
                spec,
                operation_id: Some(operation.id.clone()),
                lease: None,
                approved_hold: false,
            },
            &policy,
            operation.id.0.clone(),
        ) {
            Ok(process) => process,
            Err(LaunchRefusal::Storage(error)) => {
                return EffectOutcome::Unknown(format!(
                    "process launch record unavailable: {error}"
                ));
            }
            Err(reason) => {
                return EffectOutcome::Failed(format!("process launch refused: {reason:?}"));
            }
        };
        let started = Instant::now();
        let output = Arc::new(Mutex::new(BoundedOutput::new(output_limit)));
        self.live_output
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(process.id.as_str().to_owned(), output.clone());
        let redactions = Arc::new(redactions);
        let tail = (tail_limit > 0).then(|| Arc::new(Mutex::new(TailOutput::new(tail_limit))));
        if let Some(tail) = &tail {
            self.live_tails
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(
                    process.id.as_str().to_owned(),
                    (tail.clone(), redactions.clone()),
                );
        }
        let _output_guard = LiveOutputGuard {
            service: self,
            id: process.id.clone(),
        };
        let Some((stdout, stderr)) = self.processes.take_output(&process) else {
            let _ = self
                .processes
                .cancel(&process, Duration::ZERO, Duration::from_secs(5));
            return EffectOutcome::Unknown("owned process output could not be captured".into());
        };
        let (drained_tx, drained_rx) = std::sync::mpsc::channel();
        for mut pipe in [stdout, stderr] {
            let output = output.clone();
            let tail = tail.clone();
            let drained_tx = drained_tx.clone();
            std::thread::spawn(move || {
                use std::io::Read;
                let mut chunk = [0u8; 8192];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            output
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .append(&chunk[..n]);
                            if let Some(tail) = &tail {
                                tail.lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .append(&chunk[..n]);
                            }
                        }
                    }
                }
                let _ = drained_tx.send(());
            });
        }
        drop(drained_tx);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        let mut timed_out = false;
        let outcome = loop {
            match self.processes.poll(&process) {
                Ok(Some(outcome)) => break Ok(outcome),
                Ok(None) => {}
                // A native poll or its record write can fail transiently. Keep ownership and
                // the timeout guard active; a later poll may still verify the exit.
                Err(_) => {}
            }
            if Instant::now() >= deadline {
                timed_out = true;
                break self
                    .processes
                    .cancel(&process, Duration::from_secs(1), Duration::from_secs(5))
                    .map_err(|failure| {
                        format!("process timeout stop was not recorded: {}", failure.error)
                    });
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let duration_ms = started.elapsed().as_millis();
        let Ok(StopOutcome::Reaped {
            exit_code, signal, ..
        }) = outcome
        else {
            return EffectOutcome::Unknown(match outcome {
                Ok(StopOutcome::Uncertain(reason)) | Err(reason) => reason,
                _ => "process outcome unknown".into(),
            });
        };
        // A descendant can inherit a pipe after the observed parent exits. Do not let that
        // keep a dispatcher worker blocked forever; its tree remains under the process owner.
        for _ in 0..2 {
            if drained_rx.recv_timeout(Duration::from_secs(5)).is_err() {
                let _ = self
                    .processes
                    .cancel(&process, Duration::ZERO, Duration::from_secs(5));
                return EffectOutcome::Unknown("owned process output did not drain".into());
            }
        }
        let stored = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            snapshot.resources().process(&process.id)
        });
        let Ok(Some(stored)) = stored else {
            return EffectOutcome::Unknown("owned process record unavailable".into());
        };
        let output = output.lock().unwrap_or_else(|p| p.into_inner());
        let bytes = redacted_output(output.retained(), &redactions, output.truncated());
        let truncated = output.truncated()
            || redactions
                .iter()
                .any(|value| !value.is_empty() && value.len() < 4);
        let record = ProcessRecord {
            id: stored.id.clone(),
            revision: rev(stored.revision),
            state: OperationState::ObservedComplete,
            owner_id: stored.actor.0.clone(),
            exit_code,
            output: None,
            truncated,
        };
        drop(output);
        let mut evidence = json!({
            "result": if install {
                OperationResult::ProcessInstallProjectDependencies { value: record }
            } else {
                OperationResult::ProcessExecute { value: record }
            },
            "outputBase64": base64::engine::general_purpose::STANDARD.encode(bytes),
            "truncated": truncated,
        });
        evidence["durationMs"] = json!(u64::try_from(duration_ms).unwrap_or(u64::MAX));
        if timed_out {
            evidence["timedOut"] = json!(true);
        }
        if let Some(signal) = signal {
            evidence["signal"] = json!(signal);
        }
        if let Some(tail) = &tail {
            let tail = tail.lock().unwrap_or_else(|p| p.into_inner());
            let bytes = redacted_tail(&tail.retained(), &redactions, tail.truncated());
            evidence["tailBase64"] = json!(base64::engine::general_purpose::STANDARD.encode(bytes));
            evidence["tailTruncated"] = json!(tail.truncated());
        }
        EffectOutcome::Completed(evidence)
    }

    /// An earlier boot's effect is observed from its owned-process record; it is never
    /// redispatched. A verified terminal process can settle its outcome without inventing
    /// output bytes; `process.output` reports the missing body as OutcomeUnknown.
    pub fn observe(&self, operation: &OperationRecord) -> EffectOutcome {
        if operation.capability == "process.cancel" {
            return self.observe_cancel(operation);
        }
        if operation.capability != "process.execute" {
            return EffectOutcome::Failed("unsupported process effect".into());
        }
        let found = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            snapshot.resources().processes(
                &crate::ports::storage::ProcessFilter::Operation(operation.id.clone()),
                None,
                2,
            )
        });
        match found {
            Ok(ref records) if records.len() == 1 && records[0].phase == ProcessPhase::Reaped => {
                let process = &records[0];
                EffectOutcome::Completed(json!({
                    "result": if operation.capability == "process.install_project_dependencies" {
                        OperationResult::ProcessInstallProjectDependencies { value: ProcessRecord {
                            id: process.id.clone(), revision: rev(process.revision),
                            state: OperationState::ObservedComplete, owner_id: process.actor.0.clone(),
                            exit_code: process.exit_code.and_then(|code| i32::try_from(code).ok()),
                            output: None, truncated: true,
                        } }
                    } else { OperationResult::ProcessExecute { value: ProcessRecord {
                        id: process.id.clone(), revision: rev(process.revision),
                        state: OperationState::ObservedComplete, owner_id: process.actor.0.clone(),
                        exit_code: process.exit_code.and_then(|code| i32::try_from(code).ok()),
                        output: None, truncated: true,
                    } } },
                    "outputLost": true,
                    "truncated": true,
                }))
            }
            Ok(ref records) if records.len() == 1 && records[0].phase == ProcessPhase::Failed => {
                EffectOutcome::Failed("owned process failed before a verified result".into())
            }
            Ok(_) => EffectOutcome::Unknown("the owned process still needs reconciliation".into()),
            Err(error) => {
                EffectOutcome::Unknown(format!("process observation unavailable: {error}"))
            }
        }
    }

    fn execute_cancel(&self, operation: &OperationRecord) -> EffectOutcome {
        let Some(process_id) = operation
            .payload
            .get("processId")
            .and_then(Value::as_str)
            .and_then(|s| Id::new(s).ok())
        else {
            return EffectOutcome::Failed("invalid process cancellation target".into());
        };
        let owned = OwnedProcess {
            id: process_id.clone(),
            identity: NativeIdentity {
                pid: 0,
                birth: String::new(),
                group: None,
            },
            lease: None,
        };
        match self
            .processes
            .cancel(&owned, Duration::from_secs(1), Duration::from_secs(5))
        {
            Ok(StopOutcome::Reaped { .. }) => self.observe_cancel(operation),
            Ok(StopOutcome::Uncertain(reason)) => EffectOutcome::Unknown(reason),
            Err(failure) => EffectOutcome::Unknown(format!(
                "process cancellation record unavailable: {}",
                failure.error
            )),
        }
    }

    fn observe_cancel(&self, operation: &OperationRecord) -> EffectOutcome {
        let Some(process_id) = operation
            .payload
            .get("processId")
            .and_then(Value::as_str)
            .and_then(|s| Id::new(s).ok())
        else {
            return EffectOutcome::Failed("invalid process cancellation target".into());
        };
        let record = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            snapshot.resources().process(&process_id)
        });
        let Ok(Some(record)) = record else {
            return EffectOutcome::Unknown("process cancellation state unavailable".into());
        };
        if !record.phase.quiesced() {
            return EffectOutcome::Unknown("owned process has not been verified stopped".into());
        }
        EffectOutcome::Completed(json!({"result":OperationResult::ProcessCancel {
            value: ProcessRecord {
                id: record.id,
                revision: rev(record.revision),
                state: if record.phase == ProcessPhase::Failed { OperationState::Failed } else { OperationState::ObservedComplete },
                owner_id: record.actor.0,
                exit_code: record.exit_code.and_then(|code| i32::try_from(code).ok()),
                output: None,
                truncated: false,
            }
        }}))
    }

    fn decide_cancel(
        &self,
        ctx: &InvocationContext,
        input: &ops::CancelInput,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let Some(process) = tx.resources().process(&input.process_id)? else {
            return self.decide_cancel_before_launch(ctx, input, tx);
        };
        if process.project_id.as_ref() != ctx.project_id()
            || (process.actor != *ctx.principal()
                && !matches!(
                    ctx.principal_kind(),
                    PrincipalKind::Human | PrincipalKind::Host
                ))
        {
            return decided(
                tx,
                Err(Refusal::new(
                    DomainErrorCode::NotFound,
                    "process",
                    Some(input.process_id.clone()),
                )
                .into()),
            );
        }
        if input.expected_revision.get() != process.revision {
            return Err(StorageError::conflict(
                RevisionDimension::Operation,
                "process",
                process.id,
                input.expected_revision.clone(),
                rev(process.revision),
            )
            .into());
        }
        let Some(target) = process.operation_id else {
            return decided(
                tx,
                Err(Refusal::new(
                    DomainErrorCode::Conflict,
                    "process_operation",
                    Some(input.process_id.clone()),
                )
                .into()),
            );
        };
        let operation = OperationId(tx.new_id(IdKind::Operation));
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: ctx.root_id().cloned(),
            assignment_id: ctx.assignment_id().cloned(),
            capability: "process.cancel".into(),
            schema_version: 1,
            args_digest: sha256_hex(format!("{}|{}", input.process_id.as_str(), process.revision).as_bytes()),
            payload: json!({"processId":input.process_id.as_str(),"sourceOperationId":target.0.as_str()}),
        })?;
        let event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event.clone(),
            actor: ctx.principal().clone(),
            project_id: ctx.project_id().cloned(),
            root_id: ctx.root_id().cloned(),
            operation_id: Some(operation.clone()),
            aggregate_kind: "process_tool".into(),
            aggregate_id: input.process_id.as_str().into(),
            aggregate_revision: process.revision + 1,
            kind: "ProcessCancelRequested".into(),
            payload: json!({"processId": input.process_id.as_str()}),
        })?;
        let entry = tx.new_id(IdKind::QueueEntry);
        let now = tx.now_ms();
        tx.queue().enqueue(NewQueueEntry {
            id: entry,
            consumer: CANCEL_CONSUMER.into(),
            event_id: event,
            // Cleanup must run even when the root is paused or archived.
            root_id: None,
            operation_id: Some(operation.clone()),
            partition: None,
            priority: 1,
            ready_at_ms: now,
            payload: json!({"processId":input.process_id.as_str()}),
        })?;
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: operation.clone(),
            },
            operation_id: Some(operation),
        })
    }

    fn decide_cancel_before_launch(
        &self,
        ctx: &InvocationContext,
        input: &ops::CancelInput,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let operation_id = OperationId(input.process_id.clone());
        let operation = tx.operations().operation(&operation_id)?;
        let owner = operation
            .as_ref()
            .and_then(|op| match op.capability.as_str() {
                "process.execute" => serde_json::from_value::<ExecuteJob>(op.payload.clone())
                    .ok()
                    .map(|job| (job.project, job.actor)),
                "process.install_project_dependencies" => {
                    serde_json::from_value::<InstallJob>(op.payload.clone())
                        .ok()
                        .map(|job| (job.project, job.actor))
                }
                _ => None,
            });
        let visible = owner.as_ref().is_some_and(|(project, actor)| {
            project.as_ref() == ctx.project_id()
                && (*actor == *ctx.principal()
                    || matches!(
                        ctx.principal_kind(),
                        PrincipalKind::Human | PrincipalKind::Host
                    ))
        });
        if !visible {
            return decided(
                tx,
                Err(Refusal::new(
                    DomainErrorCode::NotFound,
                    "process",
                    Some(input.process_id.clone()),
                )
                .into()),
            );
        }
        let operation = operation.expect("visible operation");
        let (_, actor) = owner.expect("visible owner");
        if input.expected_revision.get() != operation.revision {
            return Err(StorageError::conflict(
                RevisionDimension::Operation,
                "process",
                input.process_id.clone(),
                input.expected_revision.clone(),
                rev(operation.revision),
            )
            .into());
        }
        if operation.state != OperationState::CancelledBeforeDispatch {
            if !crate::ports::storage::cancellable_before_dispatch(operation.state) {
                return decided(
                    tx,
                    Err(Refusal::new(
                        DomainErrorCode::OutcomeUnknown,
                        "process_launch",
                        Some(input.process_id.clone()),
                    )
                    .retry(RetryAdvice::RefreshAndNewRequest)
                    .into()),
                );
            }
            let mut page = tx.queue().deliveries(
                &DeliveryFilter {
                    consumer: Some(EFFECT_CONSUMER.into()),
                    operation_id: Some(operation_id.clone()),
                },
                None,
                2,
            )?;
            if page.entries.is_empty() {
                page = tx.queue().deliveries(
                    &DeliveryFilter {
                        consumer: Some(WATCH_CONSUMER.into()),
                        operation_id: Some(operation_id.clone()),
                    },
                    None,
                    2,
                )?;
            }
            if page.entries.len() != 1 {
                return decided(
                    tx,
                    Err(Refusal::new(
                        DomainErrorCode::Conflict,
                        "process_delivery",
                        Some(input.process_id.clone()),
                    )
                    .into()),
                );
            }
            tx.queue().cancel(&page.entries[0].id)?;
        }
        let current = tx.operations().operation(&operation_id)?.ok_or_else(|| {
            StorageError::InvalidRecord("cancelled process intent disappeared".into())
        })?;
        let record = ProcessRecord {
            id: input.process_id.clone(),
            revision: rev(current.revision),
            state: current.state,
            owner_id: actor.0,
            exit_code: None,
            output: None,
            truncated: false,
        };
        decided(
            tx,
            Ok(DomainDecision::Applied {
                result: OperationResult::ProcessCancel { value: record },
            }),
        )
    }

    fn decide_request_system_install(
        &self,
        ctx: &InvocationContext,
        input: &ops::RequestSystemInstallInput,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        if input.content_digest.0 != input.package_spec.sha256.0 {
            return decided(
                tx,
                Err(Refusal::new(DomainErrorCode::InvalidArgument, "content_digest", None).into()),
            );
        }
        let viewer = Viewer::resolve(tx.authority(), ctx)?;
        let artifact = tx.artifacts().artifact(&input.package_spec.artifact_id)?;
        let Some(artifact) = artifact.filter(|artifact| artifact.state == ArtifactState::Live)
        else {
            return decided(
                tx,
                Err(Refusal::new(
                    DomainErrorCode::NotFound,
                    "package_spec",
                    Some(input.package_spec.artifact_id.clone()),
                )
                .into()),
            );
        };
        if !visible(tx.artifacts(), &viewer, &artifact)? {
            return decided(
                tx,
                Err(Refusal::new(
                    DomainErrorCode::NotFound,
                    "package_spec",
                    Some(input.package_spec.artifact_id.clone()),
                )
                .into()),
            );
        }
        let version = tx.artifacts().version(&input.package_spec.version_id)?;
        let exact = version.is_some_and(|version| {
            version.live()
                && version.artifact_id == artifact.id
                && version.blob_digest == input.package_spec.sha256.0
                && version.byte_count == input.package_spec.byte_length.get()
                && version.media_type == input.package_spec.media_type
        });
        if !exact {
            return decided(
                tx,
                Err(Refusal::new(
                    DomainErrorCode::SourceChanged,
                    "package_spec",
                    Some(input.package_spec.artifact_id.clone()),
                )
                .into()),
            );
        }
        let scope = ctx
            .project_id()
            .cloned()
            .map_or(ConfigScope::Workspace, ConfigScope::Project);
        let id = tx.new_id(IdKind::Event);
        let package_spec = serde_json::to_value(&input.package_spec)
            .map_err(|error| StorageError::InvalidRecord(error.to_string()))?;
        let reviewed = json!({
            "packageSpec": package_spec,
            "contentDigest": input.content_digest.0,
            "reason": input.reason.0,
            "effect": "Human approval records consent; no system install is executed by this request"
        });
        let scope_digest = sha256_hex(
            json!({
                "scope": format!("{scope:?}"),
                "target": artifact.id.as_str(),
                "targetRevision": artifact.revision,
                "contentDigest": input.content_digest.0,
                "restoreEpoch": tx.store().restore_epoch,
            })
            .to_string()
            .as_bytes(),
        );
        let proposal = StoredApproval {
            id: id.clone(),
            scope,
            revision: 1,
            state: "Pending".into(),
            operation_kind: "process.request_system_install".into(),
            target_id: artifact.id.clone(),
            target_revision: artifact.revision,
            content_digest: input.content_digest.0.clone(),
            scope_digest,
            summary: input.reason.0.clone(),
            artifact: Some(package_spec),
            payload: json!({"content":null,"reviewed":reviewed}),
            author: ctx.principal().clone(),
            author_kind: ctx.principal_kind(),
            created_at_ms: tx.now_ms(),
            decided_by: None,
            decided_at_ms: None,
            rationale: None,
            restore_epoch: tx.store().restore_epoch,
        };
        tx.decisions().put_approval(&proposal, None)?;
        crate::services::notifications::notify_approval(
            tx,
            crate::ports::storage::ApprovalSource::Generic,
            &proposal.id,
            proposal.scope.project_id(),
            "System install requested",
            &proposal.summary,
        )?;
        let history_event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: history_event,
            actor: ctx.principal().clone(),
            project_id: ctx.project_id().cloned(),
            root_id: ctx.root_id().cloned(),
            operation_id: None,
            aggregate_kind: "system_install_proposal".into(),
            aggregate_id: proposal.id.as_str().into(),
            aggregate_revision: 1,
            kind: "SystemInstallRequested".into(),
            payload: json!({"proposalId":proposal.id,"contentDigest":proposal.content_digest}),
        })?;
        let value = ProposalRecord {
            id,
            revision: rev(1),
            content_digest: input.content_digest.clone(),
            state: ApprovalState::Pending,
            operation_kind: BoundedText("process.request_system_install".into()),
            target_id: artifact.id,
            target_revision: rev(artifact.revision),
            summary: input.reason.clone(),
            artifact: Some(input.package_spec.clone()),
        };
        decided(
            tx,
            Ok(DomainDecision::Applied {
                result: OperationResult::ProcessRequestSystemInstall { value },
            }),
        )
    }

    fn decide_install(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        input: &ops::InstallProjectDependenciesInput,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let Some(prepared) = command.prepared::<Result<InstallPlan, BindingRefusal>>() else {
            return decided(
                tx,
                Err(Refusal::new(DomainErrorCode::Conflict, "install_preparation", None).into()),
            );
        };
        if let Err(reason) = prepared {
            return decided(tx, Err(binding_refusal(*reason).into()));
        }
        let repository = tx.projects().repository(&input.repository_id)?;
        let Some(repository) = repository.filter(|repository| repository.lifecycle != "Deleted")
        else {
            return decided(
                tx,
                Err(binding_refusal(BindingRefusal::UnknownRepository).into()),
            );
        };
        if repository.revision != input.expected_revision.get() {
            return Err(StorageError::conflict(
                RevisionDimension::Registry,
                "repository",
                repository.id,
                input.expected_revision.clone(),
                rev(repository.revision),
            )
            .into());
        }
        let version = tx.artifacts().version(&input.manifest.version_id)?;
        if !version.is_some_and(|version| {
            version.live()
                && version.artifact_id == input.manifest.artifact_id
                && version.blob_digest == input.manifest.sha256.0
                && version.byte_count == input.manifest.byte_length.get()
        }) {
            return decided(
                tx,
                Err(binding_refusal(BindingRefusal::ChangedManifest).into()),
            );
        }
        let operation = OperationId(tx.new_id(IdKind::Operation));
        let job = InstallJob::from_context(ctx, command.fingerprint(), input);
        let payload = serde_json::to_value(&job)
            .map_err(|error| StorageError::InvalidRecord(error.to_string()))?;
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: ctx.root_id().cloned(),
            assignment_id: ctx.assignment_id().cloned(),
            capability: "process.install_project_dependencies".into(),
            schema_version: 1,
            args_digest: sha256_hex(payload.to_string().as_bytes()),
            payload,
        })?;
        let event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event.clone(), actor: ctx.principal().clone(), project_id: ctx.project_id().cloned(),
            root_id: ctx.root_id().cloned(), operation_id: Some(operation.clone()),
            aggregate_kind: "process_tool".into(), aggregate_id: operation.0.as_str().into(),
            aggregate_revision: 1, kind: "ProjectDependenciesRequested".into(),
            payload: json!({"repositoryId":input.repository_id,"manifest":input.manifest,"profile":input.profile}),
        })?;
        let entry = tx.new_id(IdKind::QueueEntry);
        let now = tx.now_ms();
        tx.queue().enqueue(NewQueueEntry {
            id: entry,
            consumer: EFFECT_CONSUMER.into(),
            event_id: event,
            root_id: ctx.root_id().cloned(),
            operation_id: Some(operation.clone()),
            partition: None,
            priority: 0,
            ready_at_ms: now,
            payload: json!({"repositoryId":input.repository_id}),
        })?;
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: operation.clone(),
            },
            operation_id: Some(operation),
        })
    }
}

pub(crate) fn binding_refusal(reason: BindingRefusal) -> Refusal {
    let (code, kind) = match reason {
        BindingRefusal::UnknownExecutable => (DomainErrorCode::NotFound, "process_executable"),
        BindingRefusal::UnknownRoot => (DomainErrorCode::NotFound, "process_root"),
        BindingRefusal::OutOfProject => (DomainErrorCode::PermissionDenied, "process_root"),
        BindingRefusal::ChangedExecutable => (DomainErrorCode::SourceChanged, "process_executable"),
        BindingRefusal::ChangedRoot => (DomainErrorCode::SourceChanged, "process_root"),
        BindingRefusal::UnknownCredential => (DomainErrorCode::NotFound, "process_credential"),
        BindingRefusal::UnknownRepository => (DomainErrorCode::NotFound, "repository"),
        BindingRefusal::StaleRepository => (DomainErrorCode::SourceChanged, "repository"),
        BindingRefusal::UnknownManifest => (DomainErrorCode::NotFound, "manifest"),
        BindingRefusal::ChangedManifest => (DomainErrorCode::SourceChanged, "manifest"),
        BindingRefusal::UnsupportedManifest => (DomainErrorCode::UnsupportedCapability, "manifest"),
        BindingRefusal::MissingWorktree => (DomainErrorCode::PermissionDenied, "process_root"),
        BindingRefusal::DuplicateEnvironment | BindingRefusal::InvalidLimits => {
            (DomainErrorCode::InvalidArgument, "process_spec")
        }
        BindingRefusal::UnsupportedProfile => {
            (DomainErrorCode::UnsupportedCapability, "process_profile")
        }
    };
    Refusal::new(code, kind, None)
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
        let evidence = operation.latest_attempt().and_then(|attempt| {
            attempt.observations.iter().rev().find_map(|observation| {
                observation
                    .pointer("/effect/evidence/outputBase64")
                    .and_then(Value::as_str)
                    .and_then(|value| base64::engine::general_purpose::STANDARD.decode(value).ok())
                    .or_else(|| {
                        observation
                            .pointer("/effect/evidence/outputUtf8")
                            .and_then(Value::as_str)
                            .map(|value| value.as_bytes().to_vec())
                    })
            })
        });
        let live = self
            .live_output
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(process.id.as_str())
            .cloned();
        if operation.state == OperationState::ObservedComplete && evidence.is_none() {
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
            let live = live.lock().unwrap_or_else(|p| p.into_inner());
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
            truncated: live_truncated
                || operation
                    .latest_attempt()
                    .and_then(|attempt| {
                        attempt.observations.iter().rev().find_map(|observation| {
                            observation
                                .pointer("/effect/evidence/truncated")
                                .and_then(Value::as_bool)
                        })
                    })
                    .unwrap_or(false),
            complete: operation.state == OperationState::ObservedComplete,
        };
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(1)),
            result: OperationResult::ProcessOutput { value },
        })
    }
}

impl BoundedOutput {
    pub fn new(limit: usize) -> Self {
        Self::from_offset(limit, 0)
    }

    pub fn from_offset(limit: usize, start: u64) -> Self {
        Self {
            bytes: Vec::new(),
            seen: 0,
            start,
            limit,
        }
    }

    pub fn append(&mut self, chunk: &[u8]) {
        let before = self.seen;
        self.seen = self.seen.saturating_add(chunk.len() as u64);
        let skip = self.start.saturating_sub(before).min(chunk.len() as u64) as usize;
        let keep = self
            .limit
            .saturating_sub(self.bytes.len())
            .min(chunk.len() - skip);
        self.bytes.extend_from_slice(&chunk[skip..skip + keep]);
    }

    pub fn truncated(&self) -> bool {
        self.seen.saturating_sub(self.start) > self.bytes.len() as u64
    }

    pub fn seen(&self) -> u64 {
        self.seen
    }

    pub fn page(&self, after: u64, max: usize) -> (u64, &[u8], u64) {
        let first = after.min(self.bytes.len() as u64) as usize;
        let end = first.saturating_add(max).min(self.bytes.len());
        (first as u64, &self.bytes[first..end], end as u64)
    }

    pub fn retained(&self) -> &[u8] {
        &self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn id(value: &str) -> Id {
        Id::new(value).expect("test ID")
    }

    fn spec(profile: &str, credential: bool) -> ProcessSpec {
        let environment = if credential {
            json!([{"name":"TOKEN","credentialRef":"credential-1"}])
        } else {
            json!([{"name":"MODE","value":"test"}])
        };
        serde_json::from_value(json!({
            "executableRef":"test-executable",
            "args":["--version"],
            "workingDirectory":{"rootId":"test-root","relative":"work"},
            "environment":environment,
            "profile":profile,
            "timeoutMs":"1000",
            "outputLimitBytes":"1024"
        }))
        .expect("test process spec")
    }

    #[test]
    fn truncated_secret_prefix_is_withheld_before_output_publication() {
        let secrets = vec!["hunter2".to_owned()];
        assert_eq!(redacted_output(b"hello hunt", &secrets, true), b"hello ");
        for incomplete in [false, true] {
            let output = redacted_output(b"abcdabcd", &["abcdabcd".into()], incomplete);
            assert!(!String::from_utf8_lossy(&output).contains("abcd"));
        }
        let overlapping = redacted_output(b"abcabcabc", &["abcabc".into()], true);
        assert!(!String::from_utf8_lossy(&overlapping).contains("abc"));
        assert_eq!(
            redacted_output(b"hello hunter2!", &secrets, false),
            b"hello [REDACTED]!"
        );
        assert!(redacted_output(b"short=abc", &["abc".to_owned()], false).is_empty());
        assert_eq!(redacted_output(&[0xff], &[], true), vec![0xff]);
        assert_eq!(
            redacted_output(&[0xff, b'x'], &["test".into()], false),
            vec![0xff, b'x']
        );
        assert_eq!(
            redacted_output(b"\xfftest\x00", &["test".into()], false),
            b"\xff[REDACTED]\x00"
        );
    }

    #[test]
    fn committed_process_intent_retains_host_bound_authority_sources() {
        let context = InvocationContext::new(
            crate::protocol::ActorId(id("agent-1")),
            PrincipalKind::Agent,
            id("invocation-1"),
            id("ingress-1"),
            crate::protocol::StoreId(id("store-1")),
            counter(0),
            rev(1),
        )
        .with_authority_source(id("grant-1"));
        let fingerprint = Fingerprint::from_hex(&"a".repeat(64)).expect("fingerprint");
        let job = ExecuteJob::from_context(&context, &fingerprint, spec("Full", false));
        let stored: ExecuteJob =
            serde_json::from_value(serde_json::to_value(job).unwrap()).unwrap();
        assert_eq!(stored.authority_sources, vec![id("grant-1")]);
    }

    #[test]
    fn registered_ids_and_project_scope_bound_native_launch() {
        let base =
            std::env::temp_dir().join(format!("devworks-process-bindings-{}", std::process::id()));
        let work = base.join("work");
        std::fs::create_dir_all(&work).expect("working root");
        let executable = std::env::current_exe().expect("test binary");
        let executable = executable.canonicalize().expect("canonical test binary");
        let project = ProjectId(id("project-1"));
        let bindings = ToolBindings::new(
            [RegisteredExecutable {
                id: id("test-executable"),
                path: executable.clone(),
            }],
            [RegisteredWorkingRoot {
                id: id("test-root"),
                project: Some(project.clone()),
                path: base.canonicalize().expect("canonical test root"),
            }],
        )
        .expect("registrations");
        let resolved = bindings
            .resolve(&spec("Full", false), Some(&project))
            .expect("authorized project root");
        assert_eq!(resolved.program, executable.to_string_lossy());
        assert_eq!(
            resolved.cwd,
            Some(work.canonicalize().expect("canonical work"))
        );
        assert_eq!(resolved.env, vec![("MODE".into(), "test".into())]);
        assert!(resolved.capture_output);
        assert_eq!(
            bindings.resolve(&spec("Full", false), Some(&ProjectId(id("project-2")))),
            Err(BindingRefusal::OutOfProject)
        );
        assert_eq!(
            bindings.resolve(&spec("Project", false), Some(&project)),
            Err(BindingRefusal::UnsupportedProfile)
        );
        assert_eq!(
            bindings.resolve(&spec("Confined", false), Some(&project)),
            Err(BindingRefusal::UnsupportedProfile)
        );
        assert_eq!(
            bindings.resolve(&spec("Full", true), Some(&project)),
            Err(BindingRefusal::UnknownCredential)
        );
        std::fs::remove_dir_all(base).expect("remove test root");
    }

    #[test]
    fn a_watcher_tail_keeps_the_last_bytes_and_withholds_a_cut_credential_suffix() {
        let mut tail = TailOutput::new(8);
        tail.append(b"0123");
        assert!(!tail.truncated());
        tail.append(b"456789ab");
        tail.append(b"c");
        assert_eq!(tail.retained(), b"56789abc");
        assert!(tail.truncated());
        let mut big = TailOutput::new(4);
        big.append(b"abcdefghij");
        assert_eq!(big.retained(), b"ghij");
        // A complete credential is redacted; one cut at the front of a truncated tail is
        // withheld rather than shown as a suffix.
        let secret = vec!["s3cret-token".to_owned()];
        let whole = redacted_tail(b"key=s3cret-token done", &secret, false);
        assert!(!String::from_utf8_lossy(&whole).contains("s3cret"));
        let cut = redacted_tail(b"et-token done", &secret, true);
        assert_eq!(cut, b" done");
        let kept = redacted_tail(b"et-token done", &secret, false);
        assert_eq!(kept, b"et-token done");
    }

    #[test]
    fn combined_output_drains_without_retaining_the_discarded_tail() {
        let mut output = BoundedOutput::new(5);
        output.append(b"ab");
        output.append(b"cdefgh");
        assert_eq!(output.retained(), b"abcde");
        assert!(output.truncated());
        assert_eq!(output.page(2, 2), (2, b"cd".as_slice(), 4));
        assert_eq!(output.page(5, 2), (5, b"".as_slice(), 5));
        assert_eq!(output.page(u64::MAX, usize::MAX), (5, b"".as_slice(), 5));
        let mut later = BoundedOutput::from_offset(3, 4);
        later.append(b"abc");
        later.append(b"defghij");
        assert_eq!(later.retained(), b"efg");
        assert_eq!(later.seen(), 10);
        assert!(later.truncated());
    }

    #[test]
    fn process_credential_is_resolved_only_from_registered_project_handle() {
        use crate::ports::secrets::{Secret, SecretStore, SessionSecrets};
        let base = std::env::current_dir().expect("current directory");
        let executable = std::env::current_exe()
            .expect("test binary")
            .canonicalize()
            .expect("canonical test binary");
        let project = ProjectId(id("project-1"));
        let bindings = ToolBindings::new(
            [RegisteredExecutable {
                id: id("test-executable"),
                path: executable,
            }],
            [RegisteredWorkingRoot {
                id: id("test-root"),
                project: Some(project.clone()),
                path: base.canonicalize().expect("canonical test root"),
            }],
        )
        .expect("bindings")
        .with_credentials([RegisteredCredential {
            id: id("credential-1"),
            project: Some(project.clone()),
            handle: id("secret-handle-1"),
        }])
        .expect("credential registry");
        let secrets = SessionSecrets::new();
        secrets
            .put(
                &SecretKey::new("store-1", id("secret-handle-1")),
                &Secret::new("private-token"),
            )
            .expect("store secret");
        let mut input = spec("Full", true);
        input.working_directory.relative.0 = String::new();
        let mut native = bindings
            .resolve(&input, Some(&project))
            .expect("bound spec");
        assert!(native.env.is_empty());
        let redactions = bindings
            .resolve_credentials(&input, Some(&project), &secrets, "store-1", &mut native)
            .expect("late secret resolution");
        assert_eq!(redactions, vec!["private-token"]);
        assert_eq!(native.env, vec![("TOKEN".into(), "private-token".into())]);
        assert_eq!(
            bindings.resolve(&input, Some(&ProjectId(id("project-2")))),
            Err(BindingRefusal::OutOfProject)
        );
        let intent = serde_json::to_string(&input).expect("serialize intent");
        assert!(!intent.contains("private-token"));
    }
}
