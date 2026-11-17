//! Host-owned executable and working-root bindings for the public process operations.
//!
//! Wire IDs are selectors only. A caller cannot supply a program path or an ambient working
//! directory through `ProcessSpec`; the host resolves both against its own registrations and
//! rechecks the selected files immediately before launch. A project-scoped invocation may use
//! only roots registered to that project. `Full` is ambient execution with normal user rights,
//! not an OS sandbox. The current authority policy requires an exact native exception under
//! Project scope; its effective execution profile is still `Full`. No confined launcher is
//! qualified, so `Project` and `Confined` are refused instead of silently upgraded.
use super::executables::PathExecutables;
use super::{
    LaunchPolicy, LaunchRefusal, LaunchRequest, OwnedProcess, ProcessSupervisor, StopOutcome,
};
use crate::authority::destructive::{FilesystemResolver, ProtectedRoots};
use crate::authority::redact::Redactor;
use crate::authority::shell::CommandVerdict;
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::Fingerprint;
use crate::fingerprint::sha256_hex;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
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
use crate::services::kernel::{Refusal, counter, decided, query_fault, rev};
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
    /// Explicit registrations: the registered path and its canonical file.
    executables: HashMap<String, (PathBuf, PathBuf)>,
    /// Configured names resolved on PATH when first selected, optionally limited to a fixed
    /// set (a container backend may select only its own CLI).
    path_executables: Option<(Arc<PathExecutables>, Option<&'static [&'static str]>)>,
    roots: HashMap<String, RegisteredWorkingRoot>,
    credentials: HashMap<String, RegisteredCredential>,
    /// A host-owned directory, outside every repository, that holds each checkout's install
    /// home (its home, temporary folder and package caches) and the environment of each
    /// script-free uv install. Without one, every dependency install is refused.
    install_environments: Option<PathBuf>,
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
    /// The manifest's install cannot run without package lifecycle scripts; the request must
    /// set `allowScripts`.
    ScriptsRequired,
    /// The host keeps no folder for install homes outside the repository.
    NoInstallHome,
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

    /// The exact directory of a registered root of `project`.
    pub fn root_directory(
        &self,
        root_id: &Id,
        project: Option<&ProjectId>,
    ) -> Result<PathBuf, BindingRefusal> {
        let root = self
            .roots
            .get(root_id.as_str())
            .ok_or(BindingRefusal::UnknownRoot)?;
        if root.project.as_ref() != project {
            return Err(BindingRefusal::OutOfProject);
        }
        exact_directory(&root.path).ok_or(BindingRefusal::ChangedRoot)
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
    /// Returns the canonical file to launch.
    pub fn resolve_named_executable(&self, name: &str) -> Result<String, BindingRefusal> {
        self.resolve_launch(name).map(|(program, _)| program)
    }

    /// The canonical file `name` selects and the path it was registered at, which the child
    /// keeps as its argv\[0\], after rechecking that the path still resolves to that file.
    pub fn resolve_launch(&self, name: &str) -> Result<(String, String), BindingRefusal> {
        self.launch_of(self.executable(name, None))
    }

    /// Like [`Self::resolve_named_executable`] for a caller already inside a read: configured
    /// names come from `configuration` (that caller's snapshot) instead of a second read.
    pub fn resolve_named_executable_in(
        &self,
        name: &str,
        configuration: &dyn crate::ports::storage::ConfigurationRead,
    ) -> Result<String, BindingRefusal> {
        self.launch_of(self.executable(name, Some(configuration)))
            .map(|(program, _)| program)
    }

    fn launch_of(
        &self,
        executable: Option<(PathBuf, PathBuf)>,
    ) -> Result<(String, String), BindingRefusal> {
        let (path, target) = executable.ok_or(BindingRefusal::UnknownExecutable)?;
        let program = exact_file(&path, &target).ok_or(BindingRefusal::ChangedExecutable)?;
        Ok((
            program.to_string_lossy().into_owned(),
            path.to_string_lossy().into_owned(),
        ))
    }

    /// Lets these bindings select the configured names in `registry` (all of them, or only
    /// those in `only`). An explicit registration of the same name takes precedence.
    pub fn with_path_executables(
        mut self,
        registry: Arc<PathExecutables>,
        only: Option<&'static [&'static str]>,
    ) -> Self {
        self.path_executables = Some((registry, only));
        self
    }

    fn executable(
        &self,
        name: &str,
        configuration: Option<&dyn crate::ports::storage::ConfigurationRead>,
    ) -> Option<(PathBuf, PathBuf)> {
        if let Some(registered) = self.executables.get(name) {
            return Some(registered.clone());
        }
        let (registry, only) = self.path_executables.as_ref()?;
        if only.is_some_and(|only| !only.contains(&name)) {
            return None;
        }
        match configuration {
            Some(read) => registry.lookup_in(name, super::executables::configured_names(read).ok()),
            None => registry.lookup(name),
        }
        .map(|resolved| (resolved.invoked_as, resolved.canonical))
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

    /// Lets dependency installs keep their homes, package caches and script-free uv
    /// environments under `directory`, which the host owns and no repository contains.
    pub fn with_install_environments(mut self, directory: PathBuf) -> Self {
        self.install_environments = Some(directory);
        self
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
        let (program, arg0) = self.resolve_launch(input.executable_ref.as_str())?;
        let root = self
            .roots
            .get(input.working_directory.root_id.as_str())
            .ok_or(BindingRefusal::UnknownRoot)?;
        if let Some(project) = project
            && root.project.as_ref() != Some(project)
        {
            return Err(BindingRefusal::OutOfProject);
        }
        let root_path = exact_directory(&root.path).ok_or(BindingRefusal::ChangedRoot)?;
        let cwd = exact_directory(&root_path.join(&input.working_directory.relative.0))
            .filter(|p| p.starts_with(&root_path))
            .ok_or(BindingRefusal::ChangedRoot)?;
        Ok(NativeProcessSpec {
            program,
            arg0: Some(arg0),
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

/// The canonical file `path` still resolves to, when it is `target`, in the form a launch
/// passes as the program.
fn exact_file(path: &Path, target: &Path) -> Option<PathBuf> {
    let actual = path.canonicalize().ok()?;
    (actual == target && actual.is_file()).then(|| launch_path(actual))
}

/// Windows canonical paths carry the verbatim `\\?\` prefix, which the child would see in
/// its command line. A plain drive path short enough for the classic API means the same file,
/// so it launches without the prefix; any other verbatim path is kept as it is.
#[cfg(windows)]
fn launch_path(canonical: PathBuf) -> PathBuf {
    let text = canonical.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest)
            if rest.len() < 260
                && rest.as_bytes().get(1) == Some(&b':')
                && rest.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) =>
        {
            PathBuf::from(rest)
        }
        _ => canonical,
    }
}

#[cfg(not(windows))]
fn launch_path(canonical: PathBuf) -> PathBuf {
    canonical
}

fn exact_directory(path: &Path) -> Option<PathBuf> {
    let actual = path.canonicalize().ok()?;
    (actual == path && actual.is_dir()).then_some(actual)
}

/// Whether the invocation's project (if it has one) has `repository` as a member.
fn visible_to(
    snapshot: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    repository: &Id,
) -> Result<bool, StorageError> {
    Ok(match ctx.project_id() {
        Some(project) => snapshot.projects().members(project)?.contains(repository),
        None => true,
    })
}

/// Whether `tree` is a live worktree this invocation owns: same owner, root and assignment.
fn owned_by(tree: &StoredGitWorktree, ctx: &InvocationContext) -> bool {
    !tree.removed
        && tree.owner == *ctx.principal()
        && tree.owner_root_id.as_ref() == ctx.root_id()
        && tree.owner_assignment_id.as_ref() == ctx.assignment_id()
}

/// Whether a dependency install may run package lifecycle scripts (arbitrary dependency code):
/// the user's own request, the replay of a person's approval of the exact request (the staged
/// saved-operation approval), or a request admitted by spending a reviewed native grant, which
/// always binds the exact request. Anything else is refused `ApprovalRequired` and staged.
fn scripts_approved(ctx: &InvocationContext, native_grant: bool) -> bool {
    ctx.principal_kind() == PrincipalKind::Human
        || ctx.reviewed_operation().is_some()
        || native_grant
}

/// One package-manager invocation of a dependency install.
#[derive(Debug, PartialEq, Eq)]
struct InstallCommand {
    program: &'static str,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

/// The interpreter and environment the host chooses for a script-free uv install.
#[derive(Debug)]
struct HostPython {
    /// A canonical interpreter resolved through the executable allowlist.
    interpreter: String,
    /// The project environment, in a directory the host owns outside the repository.
    environment: PathBuf,
}

/// The package-manager command for the bound manifest `name` (with contents `manifest`) in
/// `root`, with its home and caches in `home` (see [`install_home_environment`]). Unless
/// `allow_scripts`, it runs no package lifecycle scripts, and no program that repository
/// configuration names (see also [`script_free_environment`]):
///
/// - Cargo: `cargo fetch` (downloads only; nothing is compiled, so no `build.rs` runs) instead
///   of `cargo build`.
/// - npm, pnpm and Yarn 1: `--ignore-scripts`; pnpm also `--ignore-pnpmfile` (its hooks, a
///   global pnpmfile and config-dependency plugins run regardless of scripts), and it does not
///   switch to the pnpm version `packageManager` names, which it would fetch from the registry
///   the repository's `.npmrc` names (`manage-package-manager-versions` before pnpm 11,
///   `pm-on-fail` since). pnpm always keeps its store and cache in `home`, since an `.npmrc` can
///   point them into the repository. Yarn 2+ (told apart by its lockfile) does not accept
///   `--ignore-scripts`; it gets `--mode=skip-build` (no build script runs at all) and
///   `YARN_ENABLE_SCRIPTS=false`, which overrides its rc files. A Yarn release or plugin the
///   repository configures is refused separately (see [`install_runs_repository_code`], which
///   also covers Cargo configuration and toolchain files and a Corepack `packageManager` URL).
/// - uv: `--no-build` (no source distribution is built, so no build backend runs) and
///   `--no-install-workspace` (the project and its workspace members are not built either).
///   uv runs the interpreter of the environment it syncs, and an interpreter that
///   `.python-version` names, before those flags apply; so `host_python` names the interpreter
///   (`--python`) and the environment (`UV_PROJECT_ENVIRONMENT`), and without it the install
///   is refused.
/// - pip: `--only-binary :all:`, and only for a requirements file in which nothing could
///   re-enable a source build (see [`binary_only_requirements`]). Python runs isolated (`-I`),
///   so a `pip` module in the repository (the working directory) is not imported.
/// - Composer: `--no-scripts` and `--no-plugins` (plugins run code on install).
/// - Go: `go mod download` runs no package code either way; `GOENV=off` and
///   `GOTOOLCHAIN=local` (see [`script_free_environment`]) keep a Go `env` file and a `go.mod`
///   or `go.work` `toolchain` line from choosing a proxy or a toolchain to download and run.
/// - Bundler has no switch that stops the Gemfile (Ruby) or native gem extensions from
///   running, so a Bundler install needs `allow_scripts`.
fn install_command(
    name: &str,
    root: &Path,
    home: &Path,
    manifest: &[u8],
    allow_scripts: bool,
    host_python: Option<&HostPython>,
) -> Result<InstallCommand, BindingRefusal> {
    let path = |relative: &str| root.join(relative).to_string_lossy().into_owned();
    let root_text = root.to_string_lossy().into_owned();
    let strings = |items: &[&str]| items.iter().map(|item| (*item).to_owned()).collect();
    let no_scripts = |args: &mut Vec<String>, flags: &[&str]| {
        if !allow_scripts {
            args.extend(flags.iter().map(|flag| (*flag).to_owned()));
        }
    };
    let pair = |name: &str, value: String| (name.to_owned(), value);
    let command = match name {
        "Cargo.toml" | "Cargo.lock" => {
            let mut args: Vec<String> = strings(&[if allow_scripts { "build" } else { "fetch" }]);
            if name == "Cargo.lock" {
                args.push("--locked".into());
            }
            args.extend(["--manifest-path".into(), path("Cargo.toml")]);
            InstallCommand {
                program: "cargo",
                args,
                env: vec![pair("CARGO_TARGET_DIR", path("target"))],
            }
        }
        "package-lock.json" | "package.json" => {
            let mut args: Vec<String> = strings(&[if name == "package.json" {
                "install"
            } else {
                "ci"
            }]);
            no_scripts(&mut args, &["--ignore-scripts"]);
            args.extend(["--prefix".into(), root_text]);
            InstallCommand {
                program: "npm",
                args,
                env: vec![],
            }
        }
        "pnpm-lock.yaml" => {
            let mut args = strings(&["install", "--frozen-lockfile"]);
            // `.pnpmfile.cjs` hooks run even with `--ignore-scripts`, and a `packageManager`
            // version switch runs another pnpm.
            no_scripts(
                &mut args,
                &[
                    "--ignore-scripts",
                    "--ignore-pnpmfile",
                    "--config.manage-package-manager-versions=false",
                    "--config.pm-on-fail=ignore",
                ],
            );
            args.extend([
                "--store-dir".into(),
                home.join("pnpm-store").to_string_lossy().into_owned(),
                format!(
                    "--config.cache-dir={}",
                    home.join(".cache/pnpm").to_string_lossy()
                ),
                "--dir".into(),
                root_text,
            ]);
            InstallCommand {
                program: "pnpm",
                args,
                env: vec![],
            }
        }
        "yarn.lock" => {
            // A Yarn 2+ lockfile carries a `__metadata` entry; Yarn 1's does not. The process
            // already runs in `root`.
            let berry = manifest
                .split(|byte| *byte == b'\n')
                .any(|line| line.starts_with(b"__metadata:"));
            let mut args;
            if berry {
                args = strings(&["install", "--immutable"]);
                no_scripts(&mut args, &["--mode=skip-build"]);
            } else {
                args = strings(&["install", "--frozen-lockfile"]);
                no_scripts(&mut args, &["--ignore-scripts"]);
                args.extend(["--cwd".into(), root_text]);
            }
            InstallCommand {
                program: "yarn",
                args,
                // The environment is cleared, so these are the only `YARN_*` settings, and they
                // override `.yarnrc.yml`: no scripts, no `yarnPath` release (`ignorePath`), and
                // the global folder in the host's home like `YARN_CACHE_FOLDER`.
                env: if allow_scripts {
                    vec![]
                } else {
                    vec![
                        pair("YARN_ENABLE_SCRIPTS", "false".into()),
                        pair("YARN_IGNORE_PATH", "true".into()),
                        pair(
                            "YARN_GLOBAL_FOLDER",
                            home.join(".yarn/berry").to_string_lossy().into_owned(),
                        ),
                    ]
                },
            }
        }
        "pyproject.toml" | "uv.lock" => {
            let mut args = vec!["sync".into(), "--project".into(), root_text];
            if name == "uv.lock" {
                args.push("--frozen".into());
            }
            no_scripts(&mut args, &["--no-build", "--no-install-workspace"]);
            let mut env = vec![];
            if !allow_scripts {
                let host = host_python.ok_or(BindingRefusal::ScriptsRequired)?;
                args.extend(["--python".into(), host.interpreter.clone()]);
                env.push(pair(
                    "UV_PROJECT_ENVIRONMENT",
                    host.environment.to_string_lossy().into_owned(),
                ));
            }
            InstallCommand {
                program: "uv",
                args,
                env,
            }
        }
        "requirements.txt" => {
            if !allow_scripts && !binary_only_requirements(manifest) {
                return Err(BindingRefusal::ScriptsRequired);
            }
            // Isolated mode keeps the working directory (the repository), PYTHON* variables
            // and the user site off sys.path, so `-m pip` imports the interpreter's own pip.
            let mut args = strings(&["-I", "-m", "pip", "install"]);
            no_scripts(&mut args, &["--only-binary", ":all:"]);
            args.extend([
                "--target".into(),
                path(".devworks-deps/python"),
                "-r".into(),
                path(name),
            ]);
            InstallCommand {
                program: "python3",
                args,
                env: vec![],
            }
        }
        "go.mod" => InstallCommand {
            program: "go",
            args: strings(&["mod", "download"]),
            env: vec![],
        },
        "Gemfile.lock" => {
            if !allow_scripts {
                return Err(BindingRefusal::ScriptsRequired);
            }
            InstallCommand {
                program: "bundle",
                args: vec!["install".into(), "--gemfile".into(), path("Gemfile")],
                env: vec![pair("BUNDLE_PATH", path("vendor/bundle"))],
            }
        }
        "composer.lock" => {
            let mut args = strings(&["install"]);
            no_scripts(&mut args, &["--no-scripts", "--no-plugins"]);
            args.extend(["--working-dir".into(), root_text]);
            InstallCommand {
                program: "composer",
                args,
                env: vec![],
            }
        }
        _ => return Err(BindingRefusal::UnsupportedManifest),
    };
    Ok(command)
}

/// The home, temporary folder and package caches of every dependency install, all inside
/// `home`: a folder the host owns for the checkout, outside every repository. A manager's user
/// configuration (a Go `env` file, a Corepack cache, an npm, pip or Composer user file) is then
/// never a repository file, and where repository configuration can also name a cache
/// (`.npmrc` `cache`, `uv.toml` `cache-dir`, `composer.json` `cache-*-dir`), the variable here
/// takes precedence over it.
fn install_home_environment(home: &Path) -> Vec<(String, String)> {
    let at = |relative: &str| home.join(relative).to_string_lossy().into_owned();
    [
        ("HOME", home.to_string_lossy().into_owned()),
        ("TMPDIR", at("tmp")),
        ("XDG_CONFIG_HOME", at(".config")),
        ("XDG_CACHE_HOME", at(".cache")),
        ("XDG_DATA_HOME", at(".local/share")),
        ("XDG_STATE_HOME", at(".local/state")),
        ("CARGO_HOME", at(".cargo")),
        ("npm_config_cache", at(".npm")),
        ("YARN_CACHE_FOLDER", at(".cache/yarn")),
        ("COREPACK_HOME", at(".cache/node/corepack")),
        ("PIP_CACHE_DIR", at(".cache/pip")),
        ("UV_CACHE_DIR", at(".cache/uv")),
        ("GOPATH", at("go")),
        ("GOMODCACHE", at("go/pkg/mod")),
        ("COMPOSER_HOME", at(".config/composer")),
        ("COMPOSER_CACHE_DIR", at(".cache/composer")),
        ("COMPOSER_CACHE_FILES_DIR", at(".cache/composer/files")),
        ("COMPOSER_CACHE_REPO_DIR", at(".cache/composer/repo")),
        ("COMPOSER_CACHE_VCS_DIR", at(".cache/composer/vcs")),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value))
    .collect()
}

/// Settings of every script-free install that override repository configuration naming a
/// program the install would run, whichever manager reads it:
///
/// - Git reads no global or system configuration, but the checkout's own `.git/config` (and
///   the `config` of a bare repository embedded in the checkout) still applies to Git run in
///   the checkout, for example a manager's `git ls-remote` or Composer's version query. The
///   environment overrides its `core.sshCommand`, `core.gitProxy` and `core.askPass`;
///   command-scope settings empty its credential helpers (an empty `credential.helper` resets
///   the list, URL-scoped helpers included), turn off `core.fsmonitor`, point
///   `core.hooksPath` at nothing, forbid the `ext::` transport (which a `url.*.insteadOf` could
///   select) and ignore embedded bare repositories.
/// - npm: `git` (run for Git dependencies), `shell` and `script-shell` from an `.npmrc` yield
///   to the environment, and `npm_config_ignore_scripts` reaches the nested `npm install` that
///   npm runs to prepare a Git dependency, which `--ignore-scripts` does not; with no script
///   running, `node-options` and `node-gyp` name nothing that runs.
/// - Corepack (behind a `yarn`, `pnpm` or `npm` shim) reads no `.corepack.env`, which could
///   name a registry, drop signature checks or allow custom URLs.
/// - Go reads no `env` file and runs only the local toolchain.
fn script_free_environment() -> Vec<(String, String)> {
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let shell = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" };
    let git_settings = [
        ("credential.helper", ""),
        ("core.fsmonitor", "false"),
        ("core.hooksPath", null),
        ("protocol.ext.allow", "never"),
        ("safe.bareRepository", "explicit"),
    ];
    let mut env: Vec<(String, String)> = [
        ("GIT_CONFIG_GLOBAL", null),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_SSH_COMMAND", "ssh"),
        ("GIT_PROXY_COMMAND", ""),
        ("GIT_ASKPASS", ""),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("npm_config_git", "git"),
        ("npm_config_shell", shell),
        ("npm_config_script_shell", shell),
        ("npm_config_ignore_scripts", "true"),
        ("COREPACK_ENV_FILE", "0"),
        ("COREPACK_ENABLE_UNSAFE_CUSTOM_URLS", "0"),
        ("GOENV", "off"),
        ("GOTOOLCHAIN", "local"),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value.to_owned()))
    .collect();
    env.push(("GIT_CONFIG_COUNT".into(), git_settings.len().to_string()));
    for (index, (key, value)) in git_settings.into_iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{index}"), key.into()));
        env.push((format!("GIT_CONFIG_VALUE_{index}"), value.into()));
    }
    env
}

/// Whether a pip requirements file names only packages from an index, so that
/// `--only-binary :all:` leaves pip nothing to build from source. A file may itself set
/// `--no-binary` or `--only-binary :none:`, include other files (`-r`, `-c`), name an editable
/// or a direct reference (a URL, a path, an archive) or pass build settings, and pip expands
/// environment variables in it; any of those could build a package, so such a file needs
/// `allowScripts`.
fn binary_only_requirements(manifest: &[u8]) -> bool {
    // (option, takes a value)
    const GLOBAL: &[(&str, bool)] = &[
        ("-i", true),
        ("--index-url", true),
        ("--extra-index-url", true),
        ("--no-index", false),
        ("-f", true),
        ("--find-links", true),
        ("--prefer-binary", false),
        ("--require-hashes", false),
        ("--pre", false),
        ("--trusted-host", true),
    ];
    const PER_REQUIREMENT: &[(&str, bool)] = &[("--hash", true)];
    // Every final suffix of a source archive pip recognizes (.zip, .tar, .tar.gz, .tgz,
    // .tar.bz2, .tbz, .tar.xz, .txz, .tlz, .tar.lz, .tar.lzma), and .tbz2 as well.
    const ARCHIVES: &[&str] = &[
        ".zip", ".tar", ".gz", ".tgz", ".bz2", ".tbz", ".tbz2", ".xz", ".txz", ".tlz", ".lz",
        ".lzma",
    ];
    let Ok(text) = std::str::from_utf8(manifest) else {
        return false;
    };
    let text = text.replace("\\\r\n", "").replace("\\\n", "");
    text.lines().all(|line| {
        // pip drops a comment that starts the line or follows whitespace.
        let mut code = line;
        let mut previous = None;
        for (index, character) in line.char_indices() {
            if character == '#' && previous.is_none_or(char::is_whitespace) {
                code = &line[..index];
                break;
            }
            previous = Some(character);
        }
        let code = code.trim();
        if code.is_empty() {
            return true;
        }
        if code.contains('$') {
            return false;
        }
        // Like pip, the options start at the first token that starts with `-`; every one of
        // them must be allowed, not just the first.
        let tokens: Vec<&str> = code.split_whitespace().collect();
        let first_option = tokens
            .iter()
            .position(|token| token.starts_with('-'))
            .unwrap_or(tokens.len());
        let (requirement, options) = tokens.split_at(first_option);
        if requirement.is_empty() {
            return allowed_options(options, GLOBAL);
        }
        let requirement = requirement.join(" ");
        let requirement = requirement.split(';').next().unwrap_or_default().trim();
        // The project name ends where its extras, version specifier or a space start; pip
        // strips extras before it recognizes a local archive.
        let name = requirement
            .split(['[', '<', '>', '=', '!', '~', ' ', '('])
            .next()
            .unwrap_or_default();
        let lower = name.to_ascii_lowercase();
        name.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            && !requirement.contains(['@', '/', '\\', ':'])
            && !ARCHIVES.iter().any(|archive| lower.ends_with(archive))
            && allowed_options(options, PER_REQUIREMENT)
    })
}

/// Whether the manager would run a program the repository configures before (or instead of)
/// the install itself, which no install option stops; such an install needs `allowScripts`.
///
/// - Yarn: a Yarn 1 `.yarnrc`, or a `.yarnrc.yml` that [`inert_yarn_config`] does not accept,
///   in the repository root or in the install's home (inside the repository).
/// - Cargo: a Cargo configuration in the repository root (`.cargo/config` or
///   `.cargo/config.toml`) with any setting outside [`inert_cargo_config`]'s allow-list, which
///   could name the compiler, a wrapper or a credential provider that even `cargo fetch` runs;
///   any configuration in the old in-repository `CARGO_HOME`; and a toolchain file naming a
///   toolchain by path (a TOML key spelled with an escape needs `\`, so a file with `path`, `/`
///   or `\` counts).
/// - npm, pnpm and Yarn: a dependency source that runs repository code or reads outside the
///   checkout (`exec:`, git, `file:`/`link:`/`portal:` leaving the checkout) in any
///   `package.json` or the lockfile (see [`node_dependency_source_unchecked`]).
/// - npm, pnpm and Yarn: a `package.json` `packageManager` that is not a plain
///   `name@version`, since Corepack (behind a manager shim) downloads and runs the release a
///   URL there names; and a `package.json` that does not parse, whose field cannot be checked.
fn install_runs_repository_code(name: &str, root: &Path) -> bool {
    let present = |path: PathBuf| match std::fs::symlink_metadata(&path) {
        Ok(_) => true,
        // An entry that cannot be inspected cannot be ruled out.
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    };
    let node = matches!(
        name,
        "package-lock.json" | "package.json" | "pnpm-lock.yaml" | "yarn.lock"
    );
    if node && (corepack_release_unchecked(root) || node_dependency_source_unchecked(name, root)) {
        return true;
    }
    match name {
        "yarn.lock" => {
            yarn_ancestor_rc_present(root)
                || [root.to_path_buf(), root.join(".devworks-home")]
                    .iter()
                    .any(|directory| {
                        present(directory.join(".yarnrc"))
                            || match std::fs::symlink_metadata(directory.join(".yarnrc.yml")) {
                                Ok(metadata) => {
                                    // A link, or a file too large to check, cannot be ruled out.
                                    !metadata.file_type().is_file()
                                        || metadata.len() > 1024 * 1024
                                        || std::fs::read(directory.join(".yarnrc.yml"))
                                            .map_or(true, |bytes| !inert_yarn_config(&bytes))
                                }
                                Err(error) => error.kind() != std::io::ErrorKind::NotFound,
                            }
                    })
        }
        "Cargo.toml" | "Cargo.lock" => {
            let repository_config =
                ["config", "config.toml"].iter().any(|file| {
                    match std::fs::read(root.join(".cargo").join(file)) {
                        Ok(bytes) => !inert_cargo_config(&bytes),
                        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
                    }
                });
            let old_home_config = ["config", "config.toml"]
                .iter()
                .any(|file| present(root.join(".devworks-cache/cargo").join(file)));
            let configured = repository_config || old_home_config;
            let toolchain_path = ["rust-toolchain", "rust-toolchain.toml"]
                .iter()
                .any(|file| match std::fs::read(root.join(file)) {
                    Ok(text) => {
                        let text = String::from_utf8_lossy(&text);
                        text.contains("path") || text.contains(['/', '\\'])
                    }
                    Err(error) => error.kind() != std::io::ErrorKind::NotFound,
                });
            configured || toolchain_path
        }
        _ => false,
    }
}

/// Whether a directory above the checkout holds a Yarn rc file. Yarn reads `.yarnrc.yml` from
/// every ancestor of the working directory and has no environment variable or flag that stops
/// that lookup (`YARN_RC_FILENAME` renames the file for the checkout too), so the check is
/// here. A folder that cannot be inspected counts as one with a file.
fn yarn_ancestor_rc_present(root: &Path) -> bool {
    // Every ancestor counts: Yarn reads them all, and which of them a repository can write is
    // not known here. A person's own rc file above the checkout only costs the script-free path.
    for directory in root.ancestors().skip(1) {
        for file in [".yarnrc.yml", ".yarnrc"] {
            match std::fs::symlink_metadata(directory.join(file)) {
                Ok(_) => return true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return true,
            }
        }
    }
    false
}

/// Whether a repository's `.yarnrc.yml` holds only settings that name no program Yarn would
/// load or run and inject no code or environment. It fails closed: a file that is not UTF-8 or
/// does not parse, and any key not listed here, counts as naming one.
///
/// The file is read as YAML events, not as values, so nothing is resolved before it is
/// checked: a document with an anchor, an alias, a `<<` merge key, an explicit tag, a duplicate
/// key, a non-scalar key, a second document or a top level that is not a mapping is refused,
/// since another YAML reader could resolve it to a different key than this one does.
///
/// Allowed (inert): `nodeLinker` (`node-modules`, `pnpm` or `pnp`), `npmRegistryServer` (an
/// `http(s)` URL), `npmScopes` (per scope: `npmRegistryServer`, `npmPublishRegistry`,
/// `npmAlwaysAuth`, `npmAuthToken`, `npmAuthIdent`), `enableGlobalCache`, `compressionLevel`,
/// `enableTelemetry`, `enableMirror`, `enableNetwork`, `enableOfflineMode`,
/// `enableImmutableInstalls`, `enableColors`, `enableProgressBars`, `enableMessageNames`,
/// `enableTimers`, `enableHyperlinks`, `enableTransparentWorkspaces`, `httpTimeout`,
/// `httpRetry`, `networkConcurrency`, `httpProxy`, `httpsProxy`, `defaultSemverRangePrefix`,
/// `defaultProtocol`, `nmMode`, `nmHoistingLimits`, `nmSelfReferences`, `winLinkType`,
/// `preferTruncatedLines`, and the folders `cacheFolder` and `globalFolder` (every script-free
/// install sets `YARN_CACHE_FOLDER` and `YARN_GLOBAL_FOLDER` to folders in the host's home, and
/// an environment variable overrides the file).
///
/// Refused: `yarnPath`, `plugins`, `injectEnvironmentFiles`, `constraintsPath`,
/// `packageExtensions` (adds dependencies), `enableScripts` and `ignorePath` (the install sets
/// both), `pnpUnpluggedFolder`, `virtualFolder`, `installStatePath`, `lockfileFilename` and
/// other paths in the checkout, `enableStrictSsl`, `unsafeHttpWhitelist`, `caFilePath` and
/// `checksumBehavior` (weaken verification), and everything not listed above.
fn inert_yarn_config(bytes: &[u8]) -> bool {
    use yaml_rust2::{
        parser::{Event, MarkedEventReceiver, Parser, Tag},
        scanner::Marker,
    };
    #[derive(Default)]
    struct Events(Vec<Event>);
    impl MarkedEventReceiver for Events {
        fn on_event(&mut self, event: Event, _mark: Marker) {
            self.0.push(event);
        }
    }
    enum Node {
        Scalar(String),
        Map(Vec<(String, Node)>),
        Seq,
    }
    fn node(events: &mut std::slice::Iter<Event>, depth: usize) -> Option<Node> {
        let plain = |anchor: &usize, tag: &Option<Tag>| *anchor == 0 && tag.is_none();
        if depth > 8 {
            return None;
        }
        match events.next()? {
            // Yarn expands `${VAR}` from the environment, which would leak host paths.
            Event::Scalar(value, _, anchor, tag) if plain(anchor, tag) && !value.contains("${") => {
                Some(Node::Scalar(value.clone()))
            }
            Event::SequenceStart(anchor, tag) if plain(anchor, tag) => {
                while !matches!(events.as_slice().first()?, Event::SequenceEnd) {
                    node(events, depth + 1)?;
                }
                events.next();
                Some(Node::Seq)
            }
            Event::MappingStart(anchor, tag) if plain(anchor, tag) => {
                let mut entries: Vec<(String, Node)> = Vec::new();
                while !matches!(events.as_slice().first()?, Event::MappingEnd) {
                    let Node::Scalar(key) = node(events, depth + 1)? else {
                        return None;
                    };
                    if key == "<<" || entries.iter().any(|(seen, _)| *seen == key) {
                        return None;
                    }
                    entries.push((key, node(events, depth + 1)?));
                }
                events.next();
                Some(Node::Map(entries))
            }
            _ => None,
        }
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    // Yarn's parser switches to its legacy lockfile reader when a leading comment says
    // `yarn lockfile v1`, while the YAML reader here sees only comments.
    let lowered = text.to_ascii_lowercase();
    let words: Vec<&str> = lowered.split_whitespace().collect();
    if words.windows(2).any(|pair| pair == ["lockfile", "v1"]) {
        return false;
    }
    let mut receiver = Events::default();
    if Parser::new_from_str(text)
        .load(&mut receiver, true)
        .is_err()
    {
        return false;
    }
    let mut events = receiver.0.iter();
    let mut documents = 0;
    let mut root = None;
    while let Some(event) = events.next() {
        match event {
            Event::StreamStart | Event::StreamEnd | Event::DocumentEnd => {}
            Event::DocumentStart => {
                documents += 1;
                if documents > 1 {
                    return false;
                }
                let Some(top) = node(&mut events, 0) else {
                    return false;
                };
                root = Some(top);
            }
            _ => return false,
        }
    }
    let entries = match root {
        None => return true,
        // An empty file (or only comments) parses as a document with an empty value.
        Some(Node::Scalar(value)) => return value.is_empty(),
        Some(Node::Map(entries)) => entries,
        Some(Node::Seq) => return false,
    };
    let scalar = |value: &Node| matches!(value, Node::Scalar(_));
    let registry = |value: &Node| {
        matches!(value, Node::Scalar(url)
            if url.starts_with("https://") || url.starts_with("http://"))
    };
    const SCALARS: &[&str] = &[
        "enableGlobalCache",
        "compressionLevel",
        "enableTelemetry",
        "enableMirror",
        "enableNetwork",
        "enableOfflineMode",
        "enableImmutableInstalls",
        "enableColors",
        "enableProgressBars",
        "enableMessageNames",
        "enableTimers",
        "enableHyperlinks",
        "enableTransparentWorkspaces",
        "httpTimeout",
        "httpRetry",
        "networkConcurrency",
        "httpProxy",
        "httpsProxy",
        "defaultSemverRangePrefix",
        "nmMode",
        "nmHoistingLimits",
        "nmSelfReferences",
        "winLinkType",
        "preferTruncatedLines",
        "cacheFolder",
        "globalFolder",
    ];
    entries.iter().all(|(key, value)| match key.as_str() {
        key if SCALARS.contains(&key) => scalar(value),
        // Yarn prepends this to every bare range, so any other protocol (`exec:`, `file:`)
        // would make ordinary dependencies resolve to repository files.
        "defaultProtocol" => matches!(value, Node::Scalar(protocol) if protocol == "npm:"),
        "nodeLinker" => {
            matches!(value, Node::Scalar(linker) if matches!(linker.as_str(), "node-modules" | "pnpm" | "pnp"))
        }
        "npmRegistryServer" => registry(value),
        "npmScopes" => match value {
            Node::Map(scopes) => scopes.iter().all(|(_, scope)| match scope {
                Node::Map(settings) => settings.iter().all(|(key, value)| match key.as_str() {
                    "npmRegistryServer" | "npmPublishRegistry" => registry(value),
                    "npmAlwaysAuth" | "npmAuthToken" | "npmAuthIdent" => scalar(value),
                    _ => false,
                }),
                _ => false,
            }),
            _ => false,
        },
        _ => false,
    })
}

/// Whether a repository's Cargo configuration file holds only settings that name no program a
/// script-free install (`cargo fetch`) could run. It fails closed: a file that does not parse,
/// and any table or key not listed here, counts as naming one.
///
/// Allowed: `[alias]` (Cargo ignores an alias that shadows a built-in command such as `fetch`),
/// `[cargo-new]`, `[future-incompat-report]`, `[cache]`, `[gc]`, `[http]`, `[patch]`, `[term]`,
/// `[resolver]`, `[install]`, `paths`; `build.jobs`, `build.target-dir`, `build.incremental`,
/// `build.dep-info-basedir`, a `build.target` that is a plain target name (not a `.json` target
/// specification), and `build.rustflags` of [`inert_rustflags`]; `target.<triple or cfg>`
/// holding only such `rustflags`; `net.retry`, `net.offline`, `net.git-fetch-with-cli` (it runs
/// the host's `git`, under the script-free Git overrides, in Cargo's own database) and `net.ssh`;
/// `[profile]` without `rustflags` or `codegen-backend`; `registry.default`, `registry.token`;
/// `registries.<name>.index`, `.token`, `.protocol`; and `[source]` replacement (`replace-with`,
/// `directory`, `registry`, `local-registry`, `git`, `branch`, `tag`, `rev`).
///
/// Everything else counts, notably `build.rustc`, `build.rustc-wrapper`,
/// `build.rustc-workspace-wrapper`, `build.rustdoc`, `target.*.runner`, `target.*.linker`,
/// `[host]`, `[env]` (it can set variables such as `LD_PRELOAD` for the compiler Cargo queries),
/// `[unstable]`, `include`, `doc.browser`, `registry.credential-provider`,
/// `registry.global-credential-providers`, `registries.*.credential-provider` and
/// `[credential-alias]`.
fn inert_cargo_config(bytes: &[u8]) -> bool {
    use toml::Value;
    let Some(config) = std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
    else {
        return false;
    };
    let only = |value: &Value, keys: &[&str]| {
        value
            .as_table()
            .is_some_and(|table| table.keys().all(|key| keys.contains(&key.as_str())))
    };
    let each = |value: &Value, keys: &[&str]| {
        value
            .as_table()
            .is_some_and(|table| table.values().all(|entry| only(entry, keys)))
    };
    let plain_target = |value: &Value| {
        let name = |value: &Value| {
            value.as_str().is_some_and(|name| {
                !name.is_empty()
                    && !name.to_ascii_lowercase().ends_with(".json")
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            })
        };
        name(value) || value.as_array().is_some_and(|names| names.iter().all(name))
    };
    fn compiler_settings(value: &Value) -> bool {
        value.as_table().is_some_and(|table| {
            table.iter().any(|(key, value)| {
                matches!(key.as_str(), "rustflags" | "codegen-backend") || compiler_settings(value)
            })
        })
    }
    config.iter().all(|(key, value)| match key.as_str() {
        "alias"
        | "cargo-new"
        | "future-incompat-report"
        | "cache"
        | "gc"
        | "http"
        | "patch"
        | "term"
        | "resolver"
        | "install"
        | "paths" => true,
        "build" => value.as_table().is_some_and(|build| {
            build.iter().all(|(key, value)| match key.as_str() {
                "jobs" | "target-dir" | "incremental" | "dep-info-basedir" => true,
                "target" => plain_target(value),
                "rustflags" => inert_rustflags(value),
                _ => false,
            })
        }),
        "target" => value.as_table().is_some_and(|targets| {
            targets.values().all(|target| {
                target.as_table().is_some_and(|target| {
                    target
                        .iter()
                        .all(|(key, value)| key == "rustflags" && inert_rustflags(value))
                })
            })
        }),
        "net" => only(value, &["retry", "offline", "git-fetch-with-cli", "ssh"]),
        "profile" => value.is_table() && !compiler_settings(value),
        "registry" => only(value, &["default", "token"]),
        "registries" => each(value, &["index", "token", "protocol"]),
        "source" => each(
            value,
            &[
                "replace-with",
                "directory",
                "registry",
                "local-registry",
                "git",
                "branch",
                "tag",
                "rev",
            ],
        ),
        _ => false,
    })
}

/// Whether Cargo `rustflags` (a string of space-separated flags or an array of flags) are only
/// lint levels (`-D`, `-W`, `-A`, `-F`, `--cap-lints`), `--cfg`/`--check-cfg`, `-g`, `-O`, or
/// codegen options (`-C`, `--codegen`) that choose how code is generated (`target-cpu`,
/// `target-feature`, `opt-level`, `debuginfo`, `force-frame-pointers`, `overflow-checks`,
/// `panic`, `relocation-model`, `code-model`, `strip`, `symbol-mangling-version`,
/// `split-debuginfo`, `embed-bitcode`, `lto`, `codegen-units`, `debug-assertions`). Cargo passes
/// them to the compiler it queries during `cargo fetch`; anything else (`-Z`, an `@file`,
/// `--sysroot`, `linker`, `codegen-backend`, `link-arg`) counts as naming a program. Values are
/// checked too: a lint name, a `--cap-lints` level, a `--cfg` of `name` or `name="value"`, a
/// `--check-cfg` of `cfg(...)`, and a codegen value of letters, digits and `+-_.,`, so no
/// element anywhere can be an `@file` that rustc would expand into other flags.
fn inert_rustflags(value: &toml::Value) -> bool {
    const CODEGEN: &[&str] = &[
        "target-cpu",
        "target-feature",
        "opt-level",
        "debuginfo",
        "force-frame-pointers",
        "overflow-checks",
        "panic",
        "relocation-model",
        "code-model",
        "strip",
        "symbol-mangling-version",
        "split-debuginfo",
        "embed-bitcode",
        "lto",
        "codegen-units",
        "debug-assertions",
    ];
    let flags: Vec<&str> = match value {
        toml::Value::String(flags) => flags.split_whitespace().collect(),
        toml::Value::Array(flags) => {
            let Some(flags) = flags.iter().map(toml::Value::as_str).collect::<Option<_>>() else {
                return false;
            };
            flags
        }
        _ => return false,
    };
    // Every value is checked as strictly as a flag: rustc expands any argument that starts with
    // `@` (a response file) before it parses options, so no element may start with `@` or `-`
    // where a value is expected.
    let identifier = |name: &str| {
        name.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    // `name` or `name="value"`, with no quote or backslash inside the value.
    let cfg = |spec: &str| match spec.split_once('=') {
        None => identifier(spec),
        Some((name, value)) => {
            identifier(name)
                && value.len() >= 2
                && value.starts_with('"')
                && value.ends_with('"')
                && !value[1..value.len() - 1].contains(['"', '\\'])
        }
    };
    // `cfg(...)` of names, string literals, commas, spaces and `values`/`any()`.
    let check_cfg = |spec: &str| {
        spec.starts_with("cfg(")
            && spec.ends_with(')')
            && spec.chars().all(|c| {
                c.is_ascii_alphanumeric()
                    || matches!(c, '_' | '(' | ')' | ',' | ' ' | '"' | '.' | '-')
            })
    };
    let lint = |name: &str| {
        name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':'))
    };
    let cap = |level: &str| matches!(level, "allow" | "warn" | "deny" | "forbid");
    // An allowed codegen option, with a value of letters, digits and `+-_.,` only.
    let codegen = |option: &str| {
        let (key, value) = option.split_once('=').unwrap_or((option, ""));
        CODEGEN.contains(&key)
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '_' | '.' | ','))
    };
    let mut flags = flags.into_iter();
    while let Some(flag) = flags.next() {
        let mut value = |check: &dyn Fn(&str) -> bool| flags.next().is_some_and(check);
        let inert = match flag {
            "-g" | "-O" => true,
            "-D" | "-W" | "-A" | "-F" => value(&lint),
            "--cap-lints" => value(&cap),
            "--cfg" => value(&cfg),
            "--check-cfg" => value(&check_cfg),
            "-C" | "--codegen" => value(&codegen),
            _ => {
                if let Some(option) = flag
                    .strip_prefix("--codegen=")
                    .or_else(|| flag.strip_prefix("-C"))
                {
                    codegen(option)
                } else if let Some(name) = ["-D", "-W", "-A", "-F"]
                    .iter()
                    .find_map(|prefix| flag.strip_prefix(prefix))
                {
                    lint(name)
                } else if let Some(spec) = flag.strip_prefix("--cfg=") {
                    cfg(spec)
                } else if let Some(spec) = flag.strip_prefix("--check-cfg=") {
                    check_cfg(spec)
                } else if let Some(level) = flag.strip_prefix("--cap-lints=") {
                    cap(level)
                } else {
                    false
                }
            }
        };
        if !inert {
            return false;
        }
    }
    true
}

/// Whether the `packageManager` of the checkout's `package.json` could name a release other
/// than a registry version: a value that is not `name@version` (letters, digits and `.+-_`
/// only, so no URL), or a file that cannot be read or parsed.
fn corepack_release_unchecked(root: &Path) -> bool {
    let bytes = match std::fs::read(root.join("package.json")) {
        Ok(bytes) => bytes,
        Err(error) => return error.kind() != std::io::ErrorKind::NotFound,
    };
    let Ok(Value::Object(package)) = serde_json::from_slice::<Value>(&bytes) else {
        return true;
    };
    match package.get("packageManager") {
        None => false,
        Some(Value::String(spec)) => !spec.split_once('@').is_some_and(|(name, version)| {
            !name.is_empty()
                && !version.is_empty()
                && format!("{name}{version}")
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-' | '_'))
        }),
        Some(_) => true,
    }
}

/// Whether a Node project's dependencies come from a source that runs repository code or reads
/// outside the checkout, so a script-free install is refused. It looks at every `package.json`
/// in the checkout (any one could be a workspace; `node_modules` and `.git` are skipped, and a
/// tree of more than 50,000 entries, a symlinked folder or file that cannot be read or parsed
/// is refused, so it fails closed) and at the lockfile `name`, which is repository-controlled
/// and can name a source the manifests do not.
///
/// Decided per source (see [`unsafe_dependency_source`]):
/// - `exec:` (Yarn 2+) runs a repository file to build the package, whatever
///   `YARN_ENABLE_SCRIPTS` says: refused.
/// - Git sources (`git:`, `git+…`, `github:`, `gitlab:`, `bitbucket:`, `gist:`, `ssh:`,
///   `git@host:…`, a URL ending in `.git`, a URL on a git host, `owner/repo`): refused. Yarn
///   packs a git dependency through a nested install that runs its `prepack` and `prepare`
///   scripts, and npm runs `prepare` for one; neither is known to honour the script-free
///   switch for that nested step, so it is not relied on.
/// - `file:`, `link:` and `portal:` (and a bare `./`, `../`, `/` or `~/` path) are refused when
///   the path is absolute, starts with `~`, names a drive, or leaves the checkout; one inside
///   the checkout installs, since copying or linking a folder runs nothing with scripts off.
/// - `patch:` applies a patch file with Yarn's own patcher and runs no code, so it is allowed;
///   the source it patches is checked like any other (`patch:x@exec:…` is refused).
/// - `npm:` aliases and registry ranges, `workspace:`, `catalog:` and `http(s)` tarballs install.
/// - Protocols that plugins add are refused already, since plugins are.
///
/// Manifest fields read: `dependencies`, `devDependencies`, `optionalDependencies`,
/// `peerDependencies`, `resolutions`, `overrides`, `pnpm.overrides`, `pnpm.packageExtensions`,
/// `pnpm.patchedDependencies` (patch paths must stay in the checkout), `workspaces.catalog(s)`
/// and the `workspaces` globs (which must stay in the checkout). Also read: `npm-shrinkwrap.json`
/// beside `package-lock.json`, and `pnpm-workspace.yaml` (`packages`, `overrides`, `catalog(s)`,
/// `packageExtensions`, `patchedDependencies`; anchors, aliases and tags are refused).
///
/// `package-lock.json` and `npm-shrinkwrap.json` are parsed as JSON and every string is checked,
/// as are `resolved` paths and `packages` locations. `yarn.lock` and `pnpm-lock.yaml` are read
/// by line, failing closed: any backslash other than in `\"` is refused (it could hide a
/// source behind `\u0065xec` or a line continuation), each line is cut into tokens at
/// whitespace, quotes, commas and brackets, each token is percent-decoded (Yarn encodes the
/// source of a `patch:`), and the token and the text after each `@` in it are checked, as is
/// the path after a `directory` or `resolved` key. A relative `link:`, `file:` or `portal:`
/// path is resolved against its pnpm importer or Berry workspace (the `importers` key, the
/// `@workspace:` path of the block or of a `::locator=`); where neither is named it counts from
/// the checkout root, so any `..` is refused. A token that is only `exec:` (a dependency named
/// `exec`) is not a source.
fn node_dependency_source_unchecked(name: &str, root: &Path) -> bool {
    if !matches!(
        name,
        "package-lock.json" | "package.json" | "pnpm-lock.yaml" | "yarn.lock"
    ) {
        return false;
    }
    // npm prefers `npm-shrinkwrap.json` to `package-lock.json`; pnpm reads its workspace file.
    let scanned: &[&str] = match name {
        "package-lock.json" => &["package-lock.json", "npm-shrinkwrap.json"],
        "pnpm-lock.yaml" => &["pnpm-lock.yaml", "pnpm-workspace.yaml"],
        "yarn.lock" => &["yarn.lock"],
        _ => &[],
    };
    for file in scanned {
        match std::fs::metadata(root.join(file)) {
            Ok(metadata) if metadata.len() <= 128 * 1024 * 1024 => {
                let Ok(bytes) = std::fs::read(root.join(file)) else {
                    return true;
                };
                let Ok(text) = String::from_utf8(bytes) else {
                    return true;
                };
                let unsafe_file = match *file {
                    "pnpm-workspace.yaml" => pnpm_workspace_unsafe(&text),
                    _ => lockfile_unsafe(file, &text),
                };
                if unsafe_file {
                    return true;
                }
            }
            // The caller read the lockfile; a missing one has nothing to scan.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return true,
        }
    }
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    let mut seen = 0usize;
    while let Some((directory, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            return true;
        };
        for entry in entries {
            seen += 1;
            let Ok(entry) = entry else { return true };
            if seen > 50_000 {
                return true;
            }
            let Ok(kind) = entry.file_type() else {
                return true;
            };
            let file_name = entry.file_name();
            if kind.is_symlink() {
                // A link to a folder may be a workspace outside the checkout.
                if std::fs::metadata(entry.path()).map_or(true, |m| m.is_dir())
                    || file_name == "package.json"
                {
                    return true;
                }
            } else if kind.is_dir() {
                if file_name != "node_modules" && file_name != ".git" {
                    pending.push((entry.path(), depth + 1));
                }
            } else if file_name == "package.json" {
                let path = entry.path();
                match std::fs::metadata(&path) {
                    Ok(metadata) if metadata.len() <= 4 * 1024 * 1024 => {}
                    _ => return true,
                }
                match std::fs::read(&path) {
                    Ok(bytes) if package_json_unsafe(&bytes, depth) => return true,
                    Ok(_) => {}
                    Err(_) => return true,
                }
            }
        }
    }
    false
}

/// Whether the dependency fields of a `package.json` (at `depth` folders below the checkout)
/// name a source [`unsafe_dependency_source`] refuses, a patch file or workspace glob outside the
/// checkout, or the file does not parse.
fn package_json_unsafe(bytes: &[u8], depth: usize) -> bool {
    fn any_string(value: &Value, test: &dyn Fn(&str) -> bool) -> bool {
        match value {
            Value::String(text) => test(text),
            Value::Array(items) => items.iter().any(|item| any_string(item, test)),
            Value::Object(map) => map.values().any(|item| any_string(item, test)),
            _ => false,
        }
    }
    let Ok(Value::Object(package)) = serde_json::from_slice::<Value>(bytes) else {
        return true;
    };
    let test = |text: &str| unsafe_dependency_source(text, depth, true);
    let pnpm = package.get("pnpm");
    let fields = [
        package.get("dependencies"),
        package.get("devDependencies"),
        package.get("optionalDependencies"),
        package.get("peerDependencies"),
        package.get("resolutions"),
        package.get("overrides"),
        pnpm.and_then(|pnpm| pnpm.get("overrides")),
        pnpm.and_then(|pnpm| pnpm.get("packageExtensions")),
        package.get("workspaces").and_then(|w| w.get("catalog")),
        package.get("workspaces").and_then(|w| w.get("catalogs")),
    ];
    if fields
        .into_iter()
        .flatten()
        .any(|field| any_string(field, &test))
    {
        return true;
    }
    // Patch files are paths relative to the `package.json`. (`bundleDependencies` holds names.)
    if let Some(patches) = pnpm.and_then(|pnpm| pnpm.get("patchedDependencies"))
        && any_string(patches, &|path| path_leaves_checkout(path, depth))
    {
        return true;
    }
    let globs = match package.get("workspaces") {
        Some(Value::Object(map)) => map.get("packages"),
        other => other,
    };
    globs.is_some_and(|globs| any_string(globs, &|glob| glob_leaves_checkout(glob, depth)))
}

/// Whether a workspace glob can match outside the checkout: absolute, a drive or home folder,
/// or `..` segments that climb past it (or follow a wildcard, which could match nothing).
fn glob_leaves_checkout(glob: &str, depth: usize) -> bool {
    let glob = glob.trim().trim_start_matches('!');
    if glob.starts_with(['/', '\\', '~']) || glob.contains(['\\', ':']) {
        return true;
    }
    let (mut depth, mut wild) = (depth, false);
    for segment in glob.split('/') {
        match segment {
            "" | "." => {}
            ".." => match depth.checked_sub(1) {
                Some(rest) if !wild => depth = rest,
                _ => return true,
            },
            _ => {
                wild |= segment.contains(['*', '?', '[', '{', '(']);
                depth += 1;
            }
        }
    }
    false
}

/// How many folders below the checkout the workspace in a Berry locator or descriptor
/// (`name@workspace:path`, possibly percent-encoded) is; `None` when it names none, and
/// `Some(None)` when its path leaves the checkout.
fn workspace_depth(text: &str) -> Option<Option<usize>> {
    let mut best: Option<Option<usize>> = None;
    for (index, _) in text.match_indices("@workspace:") {
        let path = &text[index + "@workspace:".len()..];
        let path = path
            .split(['"', '\'', ',', ':', '#', ' '])
            .next()
            .unwrap_or("");
        let depth = if path_leaves_checkout(path, 0) {
            None
        } else {
            Some(
                path.split('/')
                    .filter(|s| !matches!(*s, "" | "."))
                    .fold(0, |d: usize, s| {
                        if s == ".." {
                            d.saturating_sub(1)
                        } else {
                            d + 1
                        }
                    }),
            )
        };
        best = match (best, depth) {
            (None, found) => Some(found),
            (Some(None), _) | (_, None) => Some(None),
            (Some(Some(a)), Some(b)) => Some(Some(a.min(b))),
        };
    }
    best
}

/// Whether a lockfile names a source [`unsafe_dependency_source`] refuses (see
/// [`node_dependency_source_unchecked`] for how it is read). `name` picks the reader: npm's JSON
/// lockfiles are parsed and every string checked; the Yarn and pnpm YAML-like text is read by
/// line, and any backslash other than in `\"` is refused, since a quoted string could hide a
/// source behind `\u0065xec` or a `\`-newline continuation.
fn lockfile_unsafe(name: &str, text: &str) -> bool {
    if matches!(name, "package-lock.json" | "npm-shrinkwrap.json") {
        return match serde_json::from_str::<Value>(text) {
            Ok(value) => npm_lockfile_value_unsafe(&value, None),
            Err(_) => true,
        };
    }
    let bytes = text.as_bytes();
    if bytes
        .iter()
        .enumerate()
        .any(|(index, byte)| *byte == b'\\' && bytes.get(index + 1) != Some(&b'"'))
    {
        return true;
    }
    // The folder depth of the importer (pnpm) or workspace (Berry) block a line is in; paths
    // there are relative to it.
    let (mut base, mut in_importers, mut importer_indent) = (0usize, false, None::<usize>);
    for line in text.lines() {
        let trimmed = line.trim();
        let indent = line.len() - line.trim_start().len();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if indent == 0 {
            base = 0;
            in_importers = name == "pnpm-lock.yaml" && trimmed == "importers:";
            importer_indent = None;
            if name == "yarn.lock" {
                match workspace_depth(&percent_decoded(trimmed)) {
                    Some(None) => return true,
                    Some(Some(depth)) => base = depth,
                    None => {}
                }
            }
        } else if in_importers && *importer_indent.get_or_insert(indent) == indent {
            let key = trimmed.trim_matches(['\'', '"', ':']);
            if !trimmed.ends_with(':') || path_leaves_checkout(key, 0) {
                return true;
            }
            base = key
                .split('/')
                .filter(|segment| !matches!(*segment, "" | "."))
                .count();
        }
        let depth = match workspace_depth(&percent_decoded(line)) {
            Some(None) => return true,
            Some(Some(depth)) if line.contains("::locator=") => depth,
            _ => base,
        };
        if lockfile_line_unsafe(line, depth) {
            return true;
        }
    }
    false
}

/// Whether one lockfile line (or JSON string) names a source [`unsafe_dependency_source`]
/// refuses, or a bare path after a `directory` or `resolved` key that leaves the checkout.
fn lockfile_line_unsafe(line: &str, depth: usize) -> bool {
    // A pnpm `resolution: {type: git, …}` carries the kind as a word, not a prefix.
    let squeezed: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    if squeezed.contains("type:git") || squeezed.contains("\"type\":\"git\"") {
        return true;
    }
    let mut path_next = false;
    for token in line
        .split(|c: char| {
            c.is_whitespace() || matches!(c, '"' | '\'' | ',' | '{' | '}' | '[' | ']' | '(' | ')')
        })
        .filter(|token| !token.is_empty())
    {
        // A trailing `:` ends a YAML key (`x@file:vendor/x:`), not the path.
        if unsafe_dependency_source(token.trim_end_matches(':'), depth, false) {
            return true;
        }
        if path_next && token != ":" {
            path_next = false;
            if bare_path_unsafe(token, depth) {
                return true;
            }
        }
        path_next = path_next || matches!(token.trim_end_matches(':'), "directory" | "resolved");
    }
    false
}

/// Whether `value` is a path with no protocol that leaves the checkout.
fn bare_path_unsafe(value: &str, depth: usize) -> bool {
    let scheme = value.split_once(':').is_some_and(|(scheme, _)| {
        scheme.len() > 1
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    });
    !scheme && path_leaves_checkout(value, depth)
}

/// npm lockfile entry fields that describe a package but never choose where it comes from:
/// a `funding` URL on GitHub, say, is not a git dependency. Every other field is checked.
const NPM_LOCKFILE_METADATA: &[&str] = &[
    "funding",
    "license",
    "engines",
    "bin",
    "os",
    "cpu",
    "libc",
    "integrity",
    "deprecated",
    "hasInstallScript",
    "peerDependenciesMeta",
];

/// Whether a parsed npm lockfile (`package-lock.json`, `npm-shrinkwrap.json`) holds a source
/// [`unsafe_dependency_source`] refuses in any string or key, a `resolved` path (a linked
/// package) that leaves the checkout, or a `packages` location outside it.
fn npm_lockfile_value_unsafe(value: &Value, key: Option<&str>) -> bool {
    match value {
        Value::String(text) => {
            lockfile_line_unsafe(text, 0) || (key == Some("resolved") && bare_path_unsafe(text, 0))
        }
        Value::Array(items) => items
            .iter()
            .any(|item| npm_lockfile_value_unsafe(item, None)),
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("git") {
                return true;
            }
            if key == Some("packages")
                && map
                    .keys()
                    .any(|location| !location.is_empty() && path_leaves_checkout(location, 0))
            {
                return true;
            }
            map.iter()
                .filter(|(name, _)| !NPM_LOCKFILE_METADATA.contains(&name.as_str()))
                .any(|(name, item)| {
                    lockfile_line_unsafe(name, 0) || npm_lockfile_value_unsafe(item, Some(name))
                })
        }
        _ => false,
    }
}

/// Whether a `pnpm-workspace.yaml` lists workspace globs that leave the checkout, or settings
/// (`overrides`, `catalog`, `catalogs`, `packageExtensions`, `patchedDependencies`) naming a
/// refused source, or is not plain YAML (anchors, aliases, tags, merge keys and anything that
/// does not parse are refused).
fn pnpm_workspace_unsafe(text: &str) -> bool {
    use yaml_rust2::{
        parser::{Event, MarkedEventReceiver, Parser},
        scanner::Marker,
    };
    #[derive(Default)]
    struct Events(Vec<Event>);
    impl MarkedEventReceiver for Events {
        fn on_event(&mut self, event: Event, _mark: Marker) {
            self.0.push(event);
        }
    }
    enum Node {
        Scalar(String),
        Seq(Vec<Node>),
        Map(Vec<(String, Node)>),
    }
    fn node(events: &mut std::slice::Iter<Event>, depth: usize) -> Option<Node> {
        if depth > 16 {
            return None;
        }
        match events.next()? {
            Event::Scalar(value, _, 0, None) => Some(Node::Scalar(value.clone())),
            Event::SequenceStart(0, None) => {
                let mut items = Vec::new();
                while !matches!(events.as_slice().first()?, Event::SequenceEnd) {
                    items.push(node(events, depth + 1)?);
                }
                events.next();
                Some(Node::Seq(items))
            }
            Event::MappingStart(0, None) => {
                let mut entries = Vec::new();
                while !matches!(events.as_slice().first()?, Event::MappingEnd) {
                    let Node::Scalar(key) = node(events, depth + 1)? else {
                        return None;
                    };
                    if key == "<<" {
                        return None;
                    }
                    entries.push((key, node(events, depth + 1)?));
                }
                events.next();
                Some(Node::Map(entries))
            }
            _ => None,
        }
    }
    fn any_scalar(node: &Node, test: &dyn Fn(&str) -> bool) -> bool {
        match node {
            Node::Scalar(text) => test(text),
            Node::Seq(items) => items.iter().any(|item| any_scalar(item, test)),
            Node::Map(entries) => entries.iter().any(|(_, item)| any_scalar(item, test)),
        }
    }
    let mut receiver = Events::default();
    if Parser::new_from_str(text)
        .load(&mut receiver, true)
        .is_err()
    {
        return true;
    }
    let mut events = receiver.0.iter();
    let mut roots = Vec::new();
    while let Some(event) = events.next() {
        match event {
            Event::StreamStart | Event::StreamEnd => {}
            Event::DocumentStart => {
                let Some(root) = node(&mut events, 0) else {
                    return true;
                };
                if !matches!(events.next(), Some(Event::DocumentEnd)) {
                    return true;
                }
                roots.push(root);
            }
            _ => return true,
        }
    }
    let Some(root) = roots.first().filter(|_| roots.len() == 1) else {
        return !roots.is_empty();
    };
    let Node::Map(entries) = root else {
        return true;
    };
    entries.iter().any(|(key, value)| match key.as_str() {
        "packages" => any_scalar(value, &|glob| glob_leaves_checkout(glob, 0)),
        "overrides" | "catalog" | "catalogs" | "packageExtensions" => {
            any_scalar(value, &|spec| unsafe_dependency_source(spec, 0, true))
        }
        "patchedDependencies" => any_scalar(value, &|path| path_leaves_checkout(path, 0)),
        _ => false,
    })
}

/// Whether the dependency specifier `spec` names a source that runs repository code or reads
/// outside the checkout (see [`node_dependency_source_unchecked`] for each decision). `depth` is
/// how many folders below the checkout the file holding it is. `manifest` is true for a
/// `package.json` value, where a bare path and `owner/repo` are also sources; in a lockfile they
/// are names and paths of other things. The specifier and the text after each `@` in it are
/// checked, so `npm:x@exec:./g.js` and `patch:x@exec%3A./g.js#./p.patch` are caught.
fn unsafe_dependency_source(spec: &str, depth: usize, manifest: bool) -> bool {
    let decoded = percent_decoded(spec.trim());
    let spec = decoded.as_str();
    let prefixed = |text: &str, prefix: &str| {
        text.len() > prefix.len()
            && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    };
    let mut starts = vec![0];
    starts.extend(spec.match_indices('@').map(|(index, _)| index + 1));
    let source = |text: &str| {
        // `exec:` runs a file, git sources run `prepare`-style scripts.
        if [
            "exec:",
            "git:",
            "git+",
            "github:",
            "gitlab:",
            "bitbucket:",
            "gist:",
            "git@",
            "ssh:",
        ]
        .iter()
        .any(|prefix| prefixed(text, prefix))
        {
            return true;
        }
        for scheme in ["https://", "http://"] {
            if prefixed(text, scheme) {
                let address = &text[scheme.len()..];
                let host = address.split(['/', '#', '?']).next().unwrap_or("");
                let path = address.split(['#', '?']).next().unwrap_or("");
                return path.ends_with(".git")
                    || [
                        "github.com",
                        "codeload.github.com",
                        "gist.github.com",
                        "api.github.com",
                        "gitlab.com",
                        "bitbucket.org",
                    ]
                    .iter()
                    .any(|git_host| host.eq_ignore_ascii_case(git_host));
            }
        }
        // A Windows drive path, bare, in a lockfile or a manifest.
        if drive_path(text) {
            return true;
        }
        ["file:", "link:", "portal:"]
            .iter()
            .find(|prefix| prefixed(text, prefix))
            .is_some_and(|prefix| path_leaves_checkout(&text[prefix.len()..], depth))
    };
    if starts.iter().any(|start| source(&spec[*start..])) {
        return true;
    }
    if !manifest {
        return false;
    }
    // A bare path; `~1.2.3` is a version range, `~/x` a home folder.
    if ["./", "../", ".\\", "..\\", "/", "~/", "\\"]
        .iter()
        .any(|prefix| spec.starts_with(prefix))
        || matches!(spec, "." | ".." | "~")
        || drive_path(spec)
    {
        return path_leaves_checkout(spec, depth);
    }
    // npm reads `owner/repo` as a GitHub repository.
    let repository = spec.split('#').next().unwrap_or("");
    !repository.contains(':') && !repository.starts_with('@') && repository.contains('/')
}

/// Whether `text` starts with a Windows drive and a separator (`C:\`, `C:/`).
fn drive_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

/// Whether a dependency path (without its protocol) is absolute, names a drive or home folder,
/// or leaves the checkout, counting from `depth` folders below it. Anything after `#` or `::`
/// is a selector, not part of the path.
fn path_leaves_checkout(path: &str, depth: usize) -> bool {
    let path = path.split('#').next().unwrap_or("");
    let path = path.split("::").next().unwrap_or("");
    if path.is_empty() {
        return false;
    }
    if path.starts_with(['/', '\\', '~']) || path.contains([':', '\\']) {
        return true;
    }
    let mut depth = depth;
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => match depth.checked_sub(1) {
                Some(rest) => depth = rest,
                None => return true,
            },
            _ => depth += 1,
        }
    }
    false
}

/// `text` with `%XX` escapes decoded (invalid escapes stay as written).
fn percent_decoded(text: &str) -> String {
    if !text.contains('%') {
        return text.to_owned();
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = |byte: u8| (byte as char).to_digit(16);
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2]))
        {
            out.push((high * 16 + low) as u8);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether every token of a requirements-file line's options is one of `allowed`, each taking
/// its value inline (`--name=value`) or as the next token when it takes one. Quoted tokens are
/// refused, since pip unquotes options and a quoted token could hide another option.
fn allowed_options(tokens: &[&str], allowed: &[(&str, bool)]) -> bool {
    let quoted = |token: &str| token.contains(['"', '\'']);
    let mut tokens = tokens.iter();
    while let Some(token) = tokens.next() {
        if quoted(token) {
            return false;
        }
        let (name, inline) = match token.split_once('=') {
            Some((name, _)) => (name, true),
            None => (*token, false),
        };
        let Some(&(_, valued)) = allowed.iter().find(|(option, _)| *option == name) else {
            return false;
        };
        if valued && !inline {
            match tokens.next() {
                Some(value) if !value.starts_with('-') && !quoted(value) => {}
                _ => return false,
            }
        } else if !valued && inline {
            return false;
        }
    }
    true
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
/// bytes is withheld along with complete matches. While the process still runs (`open_end`) a
/// credential's prefix at the end is withheld too, since its remainder may not be written yet.
pub(crate) fn redacted_tail(
    bytes: &[u8],
    values: &[String],
    cut_front: bool,
    open_end: bool,
) -> Vec<u8> {
    let mut redacted = redacted_output(bytes, values, open_end);
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
    /// A watcher's regular expression, counted over the output as it streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pattern: Option<String>,
}

/// Counts a regular expression's matches over a stream, line by line (a line split across reads
/// is counted once complete; the last unterminated line at the end).
pub(crate) struct MatchCounter {
    regex: regex::Regex,
    partial: Vec<u8>,
    count: i64,
}
impl MatchCounter {
    pub(crate) fn new(pattern: &str) -> Option<Self> {
        regex::Regex::new(pattern).ok().map(|regex| Self {
            regex,
            partial: Vec::new(),
            count: 0,
        })
    }
    fn count_line(&mut self, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        self.count += self.regex.find_iter(&text).count() as i64;
    }
    pub(crate) fn feed(&mut self, chunk: &[u8]) {
        let mut rest = chunk;
        while let Some(end) = rest.iter().position(|b| *b == b'\n') {
            let (line, after) = rest.split_at(end);
            if self.partial.is_empty() {
                self.count_line(line);
            } else {
                self.partial.extend_from_slice(line);
                let whole = std::mem::take(&mut self.partial);
                self.count_line(&whole);
            }
            rest = &after[1..];
        }
        // A runaway line is counted in pieces rather than held without bound.
        self.partial.extend_from_slice(rest);
        if self.partial.len() > 65_536 {
            let whole = std::mem::take(&mut self.partial);
            self.count_line(&whole);
        }
    }
    pub(crate) fn finish(&mut self) {
        if !self.partial.is_empty() {
            let whole = std::mem::take(&mut self.partial);
            self.count_line(&whole);
        }
    }
    pub(crate) fn count(&self) -> i64 {
        self.count
    }
}

/// A watched launch's report to the effect caller, made from the monitor thread: `None` once
/// the process is launched, or the outcome when it never launched. Which of the two was sent,
/// not whether the caller still listens, decides who records the outcome.
pub(crate) struct LaunchReport {
    to_caller: std::sync::mpsc::Sender<Option<EffectOutcome>>,
    launched: std::cell::Cell<bool>,
}

impl LaunchReport {
    fn new(to_caller: std::sync::mpsc::Sender<Option<EffectOutcome>>) -> Self {
        Self {
            to_caller,
            launched: std::cell::Cell::new(false),
        }
    }

    fn launched(&self) {
        self.launched.set(true);
        let _ = self.to_caller.send(None);
    }

    /// Hands `outcome` to the caller as the effect result when the launch was never reported.
    /// Answers it back for the monitor to record when the launch was reported, or when the
    /// caller is gone.
    fn finish(&self, outcome: EffectOutcome) -> Option<EffectOutcome> {
        if self.launched.get() {
            return Some(outcome);
        }
        self.to_caller
            .send(Some(outcome))
            .err()
            .and_then(|unsent| unsent.0)
    }
}

/// Who admitted a process launch, which decides its consumer and its re-authorization.
#[derive(Clone, Debug)]
pub(crate) enum ExecuteOrigin {
    /// `process.execute`.
    Tool,
    /// `watch.create`, with its effective and requested tail budgets.
    Watch {
        tail_bytes: u32,
        requested: Option<u32>,
        /// The watcher's regular expression over the output, if it matches one.
        pattern: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allow_scripts: Option<bool>,
}

impl InstallJob {
    fn from_context(
        ctx: &InvocationContext,
        fingerprint: &Fingerprint,
        input: &ops::InstallProjectDependenciesInput,
    ) -> Self {
        Self {
            actor: ctx.principal().clone(),
            kind: ctx.principal_kind().as_str().to_string(),
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
            allow_scripts: input.allow_scripts,
        }
    }
    fn context(
        &self,
        store: &dyn ApplicationStore,
        operation: &OperationId,
    ) -> Option<InvocationContext> {
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
            allow_scripts: self.allow_scripts,
        }
    }
}

struct InstallPlan {
    spec: NativeProcessSpec,
    /// The install's home, in the host's folder for the checkout.
    home: PathBuf,
}

impl ExecuteJob {
    fn from_context(ctx: &InvocationContext, fingerprint: &Fingerprint, spec: ProcessSpec) -> Self {
        Self {
            actor: ctx.principal().clone(),
            kind: ctx.principal_kind().as_str().to_string(),
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

    fn context(
        &self,
        store: &dyn ApplicationStore,
        operation: &OperationId,
    ) -> Option<InvocationContext> {
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
) -> Option<InvocationContext> {
    let kind = PrincipalKind::parse(kind)?;
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
    Some(context)
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
    /// Match counters of watched runs with a pattern, by process ID: stdout's and stderr's.
    live_matches: Mutex<HashMap<String, Arc<Mutex<[MatchCounter; 2]>>>>,
    live_tails: Mutex<HashMap<String, LiveTail>>,
    /// Watched processes whose monitor thread has not yet recorded their outcome.
    monitoring: Mutex<HashSet<String>>,
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
            .live_matches
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
            live_matches: Mutex::new(HashMap::new()),
            live_tails: Mutex::new(HashMap::new()),
            monitoring: Mutex::new(HashSet::new()),
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
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(process_id.as_str())
            .cloned()?;
        let counters = matches.lock().unwrap_or_else(|p| p.into_inner());
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
        let (tail, redactions) = self
            .live_tails
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(process_id.as_str())
            .cloned()?;
        let tail = tail.lock().unwrap_or_else(|p| p.into_inner());
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
            if !visible_to(snapshot, ctx, &repository.id)? {
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
            self.repository_root(snapshot, ctx, &repository)
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
                && selected.replace(((*name).to_owned(), bytes)).is_some()
            {
                return Err(BindingRefusal::ChangedManifest);
            }
        }
        let (name, bytes) = selected.ok_or(BindingRefusal::ChangedManifest)?;
        let allow_scripts = input.allow_scripts == Some(true);
        if !allow_scripts && install_runs_repository_code(&name, &root) {
            return Err(BindingRefusal::ScriptsRequired);
        }
        // Every install keeps its home and caches in a host directory, one per checkout, so no
        // repository file configures the manager as user configuration. A script-free uv
        // install also runs the host's interpreter and keeps its environment there, so no
        // interpreter in the repository runs.
        let directory = self
            .bindings
            .install_environments
            .as_ref()
            .ok_or(BindingRefusal::NoInstallHome)?;
        let checkout = sha256_hex(root.to_string_lossy().as_bytes());
        let home = directory.join("home").join(&checkout);
        let host_python = match name.as_str() {
            "pyproject.toml" | "uv.lock" if !allow_scripts => {
                let (interpreter, _) = self.bindings.resolve_launch("python3")?;
                Some(HostPython {
                    interpreter,
                    environment: directory.join("uv").join(&checkout),
                })
            }
            _ => None,
        };
        let command = install_command(
            &name,
            &root,
            &home,
            &bytes,
            allow_scripts,
            host_python.as_ref(),
        )?;
        let (executable, arg0) = self.bindings.resolve_launch(command.program)?;
        let mut env = install_home_environment(&home);
        env.push(("PATH".into(), std::env::var("PATH").unwrap_or_default()));
        if command.program == "cargo" {
            let rustup_home = std::env::var_os("RUSTUP_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup"))
                });
            if let Some(home) = rustup_home.filter(|path| path.is_dir()) {
                env.push(("RUSTUP_HOME".into(), home.to_string_lossy().into_owned()));
            }
        }
        env.extend(command.env);
        if !allow_scripts {
            env.extend(script_free_environment());
        }
        Ok(InstallPlan {
            home,
            spec: NativeProcessSpec {
                program: executable,
                arg0: Some(arg0),
                args: command.args,
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
        self.execute_reporting(operation, None)
    }

    /// Like [`Self::execute`], except that a watcher's process does not hold the caller (a
    /// dispatcher worker) for its lifetime: once it is launched this answers `launched`, and a
    /// monitor thread records the observed outcome on the operation as `watchOutcome`.
    pub fn execute_detached(self: &Arc<Self>, operation: &OperationRecord) -> EffectOutcome {
        let watched = operation.capability == "process.execute"
            && serde_json::from_value::<ExecuteJob>(operation.payload.clone())
                .is_ok_and(|job| job.watch.is_some());
        if !watched {
            return self.execute(operation);
        }
        let (report, launched) = std::sync::mpsc::channel();
        let report = LaunchReport::new(report);
        let service = self.clone();
        let monitored = operation.clone();
        self.monitoring
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(operation.id.0.as_str().to_owned());
        std::thread::spawn(move || {
            let outcome = service.execute_reporting(&monitored, Some(&report));
            if let Some(outcome) = report.finish(outcome) {
                service.record_watch_outcome(&monitored, &outcome);
            }
            service
                .monitoring
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(monitored.id.0.as_str());
        });
        match launched.recv() {
            Ok(None) => {
                drop(launched);
                EffectOutcome::Completed(json!({"launched": true}))
            }
            Ok(Some(outcome)) => outcome,
            Err(_) => EffectOutcome::Unknown("watched process launch was not reported".into()),
        }
    }

    /// Whether a monitor thread of this boot still owns the watched process `operation`.
    pub fn is_monitoring(&self, operation: &OperationId) -> bool {
        self.monitoring
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(operation.0.as_str())
    }

    fn record_watch_outcome(&self, operation: &OperationRecord, outcome: &EffectOutcome) {
        let evidence = match outcome {
            EffectOutcome::Completed(evidence) => evidence.clone(),
            EffectOutcome::Failed(reason) => json!({"failed": reason}),
            EffectOutcome::Unknown(reason) => json!({"unknown": reason}),
        };
        let id = operation.id.clone();
        let _ = crate::ports::storage::run_write(
            &*self.store,
            crate::ports::storage::WriteOptions::observed(),
            move |tx| {
                let Some(current) = tx.operations().operation(&id)? else {
                    return Ok(());
                };
                let Some(attempt) = current.latest_attempt().map(|a| a.attempt_no) else {
                    return Ok(());
                };
                tx.operations()
                    .observe(&id, attempt, json!({"watchOutcome": evidence}))
            },
        );
    }

    fn execute_reporting(
        &self,
        operation: &OperationRecord,
        launched: Option<&LaunchReport>,
    ) -> EffectOutcome {
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
        // A probe's run starts only while its probe still observes: one queued before the probe
        // ended (it timed out, or was stopped between runs) never launches. A one-shot process
        // watcher's process is the owner's request and launches even when only its observation
        // was cancelled (an explicit stop cancels the undispatched intent itself).
        if job.watch.is_some() {
            let watcher = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
                snapshot.watchers().watcher_for_process(&operation.id)
            });
            match watcher {
                // ... and nothing a watcher launched starts once the watcher was ended with a stop
                // (its owner ended, or a stop was requested before the launch).
                Ok(Some(w))
                    if (w.source_kind == "Probe" && (!w.phase.is_open() || w.stop_requested))
                        || (!w.phase.is_open() && w.stop_requested) =>
                {
                    return EffectOutcome::Failed(
                        "the watcher ended before this run started".into(),
                    );
                }
                Err(error) => {
                    return EffectOutcome::Unknown(format!("watcher state unavailable: {error}"));
                }
                _ => {}
            }
        }
        let Some(ctx) = job.context(&*self.store, &operation.id) else {
            return EffectOutcome::Failed("invalid process principal kind".into());
        };
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
                    process: Some(job.spec.clone()),
                    probe: None,
                    timer: None,
                    file: None,
                    matching: None,
                    max_lifetime_ms: None,
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
            &job.spec.working_directory.root_id,
            job.spec.timeout_ms.get(),
            job.spec.output_limit_bytes.get() as usize,
            job.tail_bytes as usize,
            redactions,
            None,
            launched,
            job.watch.as_ref().and_then(|w| w.pattern.as_deref()),
        )
    }

    fn execute_install(&self, operation: &OperationRecord) -> EffectOutcome {
        let Ok(job) = serde_json::from_value::<InstallJob>(operation.payload.clone()) else {
            return EffectOutcome::Failed("invalid project install intent".into());
        };
        let Some(ctx) = job.context(&*self.store, &operation.id) else {
            return EffectOutcome::Failed("invalid process principal kind".into());
        };
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
            &input.repository_id,
            3_600_000,
            1_048_576,
            0,
            Vec::new(),
            Some(&plan.home),
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn run_owned(
        &self,
        operation: &OperationRecord,
        ctx: &InvocationContext,
        spec: NativeProcessSpec,
        root: &Id,
        timeout_ms: i64,
        output_limit: usize,
        tail_limit: usize,
        redactions: Vec<String>,
        install_home: Option<&Path>,
        launched: Option<&LaunchReport>,
        pattern: Option<&str>,
    ) -> EffectOutcome {
        if let Some(home) = install_home {
            let Some(root) = spec.cwd.as_ref() else {
                return EffectOutcome::Failed("project install has no working root".into());
            };
            // The home and its temporary folder are the host's; only the pip install target
            // stays in the checkout.
            for directory in [home.join("tmp"), root.join(".devworks-deps")] {
                if std::fs::create_dir_all(directory).is_err() {
                    return EffectOutcome::Failed("project install workspace unavailable".into());
                }
            }
        }
        let Ok(branches) = self.protected_branches(root) else {
            return EffectOutcome::Failed("protected branches unavailable".into());
        };
        let policy = LaunchPolicy {
            fs: &*self.fs,
            roots: &self.protected_roots,
            protected_branches: &branches,
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
            Ok(process) => {
                if let Some(report) = launched {
                    report.launched();
                }
                process
            }
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
        let matches = pattern
            .and_then(|p| Some([MatchCounter::new(p)?, MatchCounter::new(p)?]))
            .map(|counters| Arc::new(Mutex::new(counters)));
        if let Some(matches) = &matches {
            self.live_matches
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(process.id.as_str().to_owned(), matches.clone());
        }
        let (drained_tx, drained_rx) = std::sync::mpsc::channel();
        for (stream, mut pipe) in [stdout, stderr].into_iter().enumerate() {
            let output = output.clone();
            let tail = tail.clone();
            let matches = matches.clone();
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
                            if let Some(matches) = &matches {
                                matches.lock().unwrap_or_else(|p| p.into_inner())[stream]
                                    .feed(&chunk[..n]);
                            }
                        }
                    }
                }
                if let Some(matches) = &matches {
                    matches.lock().unwrap_or_else(|p| p.into_inner())[stream].finish();
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
            "result": if install_home.is_some() {
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
        // Every match in the whole output, counted as it streamed (both streams drained above).
        if let Some(matches) = &matches {
            let counters = matches.lock().unwrap_or_else(|p| p.into_inner());
            evidence["matches"] = json!(counters[0].count() + counters[1].count());
        }
        if let Some(tail) = &tail {
            let tail = tail.lock().unwrap_or_else(|p| p.into_inner());
            let bytes = redacted_tail(&tail.retained(), &redactions, tail.truncated(), false);
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
        if input.allow_scripts == Some(true) {
            let mut native_grant = false;
            for spent in command.authorization().authority_grants() {
                native_grant |= tx
                    .authority()
                    .authority_grant(&spent.grant_id)?
                    .is_some_and(|grant| grant.kind == GrantKind::NativeExecution);
            }
            if !scripts_approved(ctx, native_grant) {
                // The host stages an approval of this exact request; its replay runs it.
                return decided(
                    tx,
                    Err(Refusal::new(
                        DomainErrorCode::ApprovalRequired,
                        "install_scripts_review",
                        Some(input.repository_id.clone()),
                    )
                    .into()),
                );
            }
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
        BindingRefusal::ScriptsRequired => (DomainErrorCode::InvalidArgument, "install_scripts"),
        BindingRefusal::NoInstallHome => (DomainErrorCode::UnsupportedCapability, "install_home"),
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
        let live = self
            .live_output
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(process.id.as_str())
            .cloned();
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

    /// The process exited while the caller still held the receiver it had just read the launch
    /// report from: the monitor records the outcome rather than queueing it for nobody.
    #[test]
    fn a_reported_launch_leaves_its_outcome_to_the_monitor_while_the_caller_still_listens() {
        let exit = || EffectOutcome::Completed(json!({"exitCode": 7}));
        let (to_caller, from_monitor) = std::sync::mpsc::channel();
        let report = LaunchReport::new(to_caller);
        report.launched();
        assert!(matches!(from_monitor.recv(), Ok(None)));
        assert!(matches!(
            report.finish(exit()),
            Some(EffectOutcome::Completed(_))
        ));
        assert!(
            from_monitor.try_recv().is_err(),
            "the outcome was queued for the caller"
        );

        // Never launched: the caller receives the outcome as the effect result.
        let (to_caller, from_monitor) = std::sync::mpsc::channel();
        let report = LaunchReport::new(to_caller);
        assert!(report.finish(exit()).is_none());
        assert!(matches!(
            from_monitor.recv(),
            Ok(Some(EffectOutcome::Completed(_)))
        ));

        // Never launched and the caller is gone: the monitor still records it.
        let (to_caller, from_monitor) = std::sync::mpsc::channel();
        drop(from_monitor);
        assert!(LaunchReport::new(to_caller).finish(exit()).is_some());
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
        // The canonical file launches; on Windows without the verbatim prefix.
        assert_eq!(
            resolved.program,
            launch_path(executable.clone()).to_string_lossy()
        );
        assert_eq!(
            resolved.arg0.as_deref(),
            Some(&*executable.to_string_lossy())
        );
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
        let whole = redacted_tail(b"key=s3cret-token done", &secret, false, false);
        assert!(!String::from_utf8_lossy(&whole).contains("s3cret"));
        let cut = redacted_tail(b"et-token done", &secret, true, false);
        assert_eq!(cut, b" done");
        let kept = redacted_tail(b"et-token done", &secret, false, false);
        assert_eq!(kept, b"et-token done");
        // A running process may not have written a credential's remainder yet.
        let live = redacted_tail(b"key=s3cret", &secret, false, true);
        assert_eq!(live, b"key=");
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

    /// Each supported manager's install runs with lifecycle scripts disabled unless the request
    /// allows them; the allowed form is the manager's ordinary install.
    #[test]
    fn each_manager_installs_without_lifecycle_scripts_unless_allowed() {
        let root = Path::new("/repo");
        let home = Path::new("/host/home");
        let at = |relative: &str| root.join(relative).to_string_lossy().into_owned();
        let store = home.join("pnpm-store").to_string_lossy().into_owned();
        let cache = format!(
            "--config.cache-dir={}",
            home.join(".cache/pnpm").to_string_lossy()
        );
        let names = |args: &[&str]| args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        let requirements = b"requests==2.32.3 --hash=sha256:abc\n".as_slice();
        let host = HostPython {
            interpreter: "/host/python3".into(),
            environment: "/host/environments/uv/repo".into(),
        };
        // (manifest, without scripts, with scripts, extra environment without scripts)
        type Case<'a> = (&'a str, Vec<String>, Vec<String>, Vec<(&'a str, &'a str)>);
        let cases: Vec<Case> = vec![
            (
                "Cargo.toml",
                names(&["fetch", "--manifest-path", &at("Cargo.toml")]),
                names(&["build", "--manifest-path", &at("Cargo.toml")]),
                vec![],
            ),
            (
                "Cargo.lock",
                names(&["fetch", "--locked", "--manifest-path", &at("Cargo.toml")]),
                names(&["build", "--locked", "--manifest-path", &at("Cargo.toml")]),
                vec![],
            ),
            (
                "package-lock.json",
                names(&["ci", "--ignore-scripts", "--prefix", "/repo"]),
                names(&["ci", "--prefix", "/repo"]),
                vec![],
            ),
            (
                "package.json",
                names(&["install", "--ignore-scripts", "--prefix", "/repo"]),
                names(&["install", "--prefix", "/repo"]),
                vec![],
            ),
            (
                "pnpm-lock.yaml",
                names(&[
                    "install",
                    "--frozen-lockfile",
                    "--ignore-scripts",
                    "--ignore-pnpmfile",
                    "--config.manage-package-manager-versions=false",
                    "--config.pm-on-fail=ignore",
                    "--store-dir",
                    &store,
                    &cache,
                    "--dir",
                    "/repo",
                ]),
                names(&[
                    "install",
                    "--frozen-lockfile",
                    "--store-dir",
                    &store,
                    &cache,
                    "--dir",
                    "/repo",
                ]),
                vec![],
            ),
            (
                "yarn.lock",
                names(&[
                    "install",
                    "--frozen-lockfile",
                    "--ignore-scripts",
                    "--cwd",
                    "/repo",
                ]),
                names(&["install", "--frozen-lockfile", "--cwd", "/repo"]),
                vec![("YARN_ENABLE_SCRIPTS", "false")],
            ),
            (
                "pyproject.toml",
                names(&[
                    "sync",
                    "--project",
                    "/repo",
                    "--no-build",
                    "--no-install-workspace",
                    "--python",
                    "/host/python3",
                ]),
                names(&["sync", "--project", "/repo"]),
                vec![("UV_PROJECT_ENVIRONMENT", "/host/environments/uv/repo")],
            ),
            (
                "uv.lock",
                names(&[
                    "sync",
                    "--project",
                    "/repo",
                    "--frozen",
                    "--no-build",
                    "--no-install-workspace",
                    "--python",
                    "/host/python3",
                ]),
                names(&["sync", "--project", "/repo", "--frozen"]),
                vec![("UV_PROJECT_ENVIRONMENT", "/host/environments/uv/repo")],
            ),
            (
                "requirements.txt",
                names(&[
                    "-I",
                    "-m",
                    "pip",
                    "install",
                    "--only-binary",
                    ":all:",
                    "--target",
                    &at(".devworks-deps/python"),
                    "-r",
                    &at("requirements.txt"),
                ]),
                names(&[
                    "-I",
                    "-m",
                    "pip",
                    "install",
                    "--target",
                    &at(".devworks-deps/python"),
                    "-r",
                    &at("requirements.txt"),
                ]),
                vec![],
            ),
            (
                "go.mod",
                names(&["mod", "download"]),
                names(&["mod", "download"]),
                vec![],
            ),
            (
                "composer.lock",
                names(&[
                    "install",
                    "--no-scripts",
                    "--no-plugins",
                    "--working-dir",
                    "/repo",
                ]),
                names(&["install", "--working-dir", "/repo"]),
                vec![],
            ),
        ];
        for (name, without, with, extra) in cases {
            let off =
                install_command(name, root, home, requirements, false, Some(&host)).expect(name);
            let on = install_command(name, root, home, requirements, true, None).expect(name);
            assert_eq!(off.program, on.program, "{name}");
            assert_eq!(off.args, without, "{name} without scripts");
            assert_eq!(on.args, with, "{name} with scripts");
            for (key, value) in &extra {
                assert!(
                    off.env.contains(&((*key).to_owned(), (*value).to_owned())),
                    "{name}: {:?}",
                    off.env
                );
            }
            assert!(
                !on.env
                    .iter()
                    .any(|(key, _)| key == "YARN_ENABLE_SCRIPTS" || key == "UV_PROJECT_ENVIRONMENT"),
                "{name}"
            );
        }
        // Without a host interpreter and environment, a script-free uv install is refused.
        for name in ["pyproject.toml", "uv.lock"] {
            assert_eq!(
                install_command(name, root, home, b"", false, None),
                Err(BindingRefusal::ScriptsRequired),
                "{name}"
            );
        }
        // Bundler has no switch that keeps the Gemfile and native extensions from running.
        assert_eq!(
            install_command("Gemfile.lock", root, home, b"", false, None),
            Err(BindingRefusal::ScriptsRequired)
        );
        let bundle = install_command("Gemfile.lock", root, home, b"", true, None).expect("bundler");
        assert_eq!(
            bundle.args,
            names(&["install", "--gemfile", &at("Gemfile")])
        );
        // A requirements file that could build from source needs allowScripts.
        assert_eq!(
            install_command("requirements.txt", root, home, b"-e .\n", false, None),
            Err(BindingRefusal::ScriptsRequired)
        );
        assert!(install_command("requirements.txt", root, home, b"-e .\n", true, None).is_ok());
        // A Yarn 2+ lockfile selects Yarn 2+'s own switch; it has no `--ignore-scripts`.
        let berry = b"__metadata:\n  version: 8\n".as_slice();
        let off = install_command("yarn.lock", root, home, berry, false, None).expect("yarn 2+");
        assert_eq!(
            off.args,
            names(&["install", "--immutable", "--mode=skip-build"])
        );
        assert!(
            off.env
                .contains(&("YARN_ENABLE_SCRIPTS".into(), "false".into()))
        );
        let on = install_command("yarn.lock", root, home, berry, true, None).expect("yarn 2+");
        assert_eq!(on.args, names(&["install", "--immutable"]));
        assert!(on.env.is_empty());
    }

    #[test]
    fn only_a_requirements_file_of_index_packages_installs_without_scripts() {
        for safe in [
            "",
            "# pinned\nrequests==2.32.3\n",
            "requests[socks]>=2,<3 ; python_version >= \"3.9\"  # comment\n",
            "--index-url https://pypi.org/simple\n--prefer-binary\nnumpy\n",
            "-i https://example.invalid/simple\nflask \\\n  ==3.0.0\n",
            "--require-hashes\nidna==3.7 --hash=sha256:aaaa --hash=sha256:bbbb\n",
        ] {
            assert!(binary_only_requirements(safe.as_bytes()), "{safe:?}");
        }
        for unsafe_file in [
            "--no-binary :all:\nrequests\n",
            "--only-binary :none:\nrequests\n",
            "-e .\n",
            "--editable ./pkg\n",
            "-r other.txt\n",
            "-c constraints.txt\n",
            "./local-package\n",
            "/abs/path/pkg\n",
            "pkg @ https://example.invalid/pkg-1.0.tar.gz\n",
            "git+https://example.invalid/repo.git#egg=pkg\n",
            "pkg-1.0.tar.gz\n",
            "pkg.tlz\n",
            "pkg.zip[extra]\n",
            "pkg.tar.gz [extra] ==1.0\n",
            "pkg-1.0.tar.lzma\n",
            "pkg-1.0.TAR.LZ\n",
            "pkg --config-settings=key=value\n",
            // Every option on a line counts, not only the first.
            "--no-index --requirement nested.txt\n",
            "--prefer-binary -r nested.txt\n",
            "-i https://example.invalid/simple -e .\n",
            "--no-index=1\n",
            "--index-url\n",
            "requests --hash=sha256:aaaa -r nested.txt\n",
            "requests -c constraints.txt\n",
            "requests --hash=sha256:aaaa \"-r\" nested.txt\n",
            "${PKG}\n",
            "requests\n.\n",
        ] {
            assert!(
                !binary_only_requirements(unsafe_file.as_bytes()),
                "{unsafe_file:?}"
            );
        }
        assert!(!binary_only_requirements(&[0xff, 0xfe]));
    }

    #[test]
    fn a_manager_program_the_repository_configures_needs_install_scripts() {
        let root = std::env::temp_dir().join(format!("devworks-tool-rc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for directory in [".devworks-home", ".cargo", ".devworks-cache/cargo"] {
            std::fs::create_dir_all(root.join(directory)).expect("test root");
        }
        for name in ["yarn.lock", "Cargo.toml", "Cargo.lock", "package.json"] {
            assert!(!install_runs_repository_code(name, &root), "{name}");
        }
        // A toolchain file without a path is fine.
        std::fs::write(
            root.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.95\"\n",
        )
        .expect("toolchain");
        assert!(!install_runs_repository_code("Cargo.toml", &root));
        for (name, file, text) in [
            (
                "yarn.lock",
                ".yarnrc.yml",
                "\"yarn\\u0050ath\": ./local.cjs\n",
            ),
            ("yarn.lock", ".yarnrc", "yarn-path \"./tools/yarn.js\"\n"),
            ("yarn.lock", ".devworks-home/.yarnrc.yml", "yarnPath: x\n"),
            (
                "Cargo.toml",
                ".cargo/config.toml",
                "[build]\nrustc = \"./hook\"\n",
            ),
            (
                "Cargo.lock",
                ".cargo/config",
                "[build]\nrustc-wrapper = \"x\"\n",
            ),
            ("Cargo.toml", ".devworks-cache/cargo/config.toml", "[net]\n"),
            (
                "Cargo.toml",
                "rust-toolchain.toml",
                "[toolchain]\npath = \"./t\"\n",
            ),
            (
                "Cargo.toml",
                "rust-toolchain.toml",
                "[toolchain]\n\"\\u0070ath\" = \"t\"\n",
            ),
            ("Cargo.lock", "rust-toolchain", "./custom-toolchain\n"),
        ] {
            let before = std::fs::read(root.join(file)).ok();
            std::fs::write(root.join(file), text).expect("config");
            assert!(install_runs_repository_code(name, &root), "{file}: {text}");
            assert!(!install_runs_repository_code("package.json", &root));
            match before {
                Some(bytes) => std::fs::write(root.join(file), bytes).expect("restore"),
                None => std::fs::remove_file(root.join(file)).expect("remove config"),
            }
        }
        std::fs::remove_dir_all(root).expect("remove test root");
    }

    /// DW-362: a Yarn 2+ `.yarnrc.yml` of only inert settings installs without scripts; one
    /// naming a program, loading code, or using YAML that another reader could resolve to a
    /// different key (anchors, aliases, merge keys, tags, a second document) does not.
    #[test]
    fn only_a_yarn_rc_file_naming_no_program_installs_without_scripts() {
        let root = std::env::temp_dir().join(format!("devworks-tool-yarn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("test root");
        let inert = [
            "",
            "# only a comment\n",
            "nodeLinker: node-modules\nnpmRegistryServer: \"https://registry.example.invalid\"\n",
            "nodeLinker: pnpm\nenableGlobalCache: false\ncompressionLevel: mixed\n\
             cacheFolder: ./.yarn/cache\nglobalFolder: ./.yarn/global\nenableTelemetry: false\n",
            "npmScopes:\n  acme:\n    npmRegistryServer: https://npm.example.invalid\n    \
             npmAlwaysAuth: true\n    npmAuthToken: t\n",
            "---\nnodeLinker: pnp\nhttpTimeout: 60000\n",
            "defaultProtocol: \"npm:\"\n",
        ];
        for text in inert {
            std::fs::write(root.join(".yarnrc.yml"), text).expect("rc");
            assert!(!install_runs_repository_code("yarn.lock", &root), "{text}");
        }
        let refused = [
            "yarnPath: .yarn/releases/yarn-4.cjs\n",
            "\"yarn\\u0050ath\": ./local.cjs\n",
            "plugins:\n  - ./plugin.cjs\n",
            "plugins: []\n",
            "injectEnvironmentFiles:\n  - .env\n",
            "constraintsPath: ./c.pro\n",
            "packageExtensions:\n  a@*:\n    dependencies:\n      b: \"*\"\n",
            "nodeLinker: custom\n",
            "nodeLinker: node-modules\nunknownSetting: 1\n",
            "npmRegistryServer: file:///etc\n",
            "npmScopes:\n  acme:\n    yarnPath: x\n",
            "enableStrictSsl: false\n",
            "installStatePath: ./state\n",
            // YAML that another reader could resolve differently.
            "base: &base\n  nodeLinker: node-modules\nother: *base\n",
            "anchored: &a node-modules\nnodeLinker: *a\n",
            "defaults: &d\n  nodeLinker: node-modules\n<<: *d\n",
            "<<: {yarnPath: x}\nnodeLinker: pnp\n",
            "\"<<\": {nodeLinker: pnp}\n",
            "nodeLinker: !!str pnp\n",
            "!!map\nnodeLinker: pnp\n",
            "nodeLinker: pnp\n---\nyarnPath: x\n",
            "nodeLinker: pnp\nnodeLinker: pnp\n",
            "? [a, b]\n: c\n",
            "- nodeLinker\n",
            "just a string\n",
            "nodeLinker: [unterminated\n",
            "defaultProtocol: \"exec:\"\n",
            "defaultProtocol: \"file:\"\n",
            "# yarn lockfile v1\nnodeLinker: pnp\n",
            "# Yarn   Lockfile \t V1\r\nnodeLinker: pnp\r\n",
            "npmRegistryServer: \"https://r.example.invalid/${HOME}\"\n",
            "cacheFolder: \"${TMPDIR}/c\"\n",
        ];
        for text in refused {
            std::fs::write(root.join(".yarnrc.yml"), text).expect("rc");
            assert!(install_runs_repository_code("yarn.lock", &root), "{text}");
        }
        // Yarn 1's file is still refused, and so is a link in place of the file.
        std::fs::remove_file(root.join(".yarnrc.yml")).expect("remove");
        std::fs::write(root.join(".yarnrc"), "--install.frozen-lockfile true\n").expect("rc");
        assert!(install_runs_repository_code("yarn.lock", &root));
        std::fs::remove_file(root.join(".yarnrc")).expect("remove");
        std::fs::write(root.join("target.yml"), "nodeLinker: pnp\n").expect("target");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("target.yml"), root.join(".yarnrc.yml"))
                .expect("link");
            assert!(install_runs_repository_code("yarn.lock", &root));
        }
        std::fs::remove_dir_all(root).expect("remove test root");
    }

    /// DW-362: Yarn reads `.yarnrc.yml` and `.yarnrc` from every ancestor of the checkout, so one
    /// there refuses a script-free install unless it is in a DevWorks-owned host folder.
    #[test]
    fn a_yarn_rc_file_above_the_checkout_needs_scripts() {
        let top = std::env::temp_dir().join(format!("devworks-yarn-up-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        let root = top.join("a/b/checkout");
        std::fs::create_dir_all(&root).expect("root");
        // The temporary folder itself may have an rc file above it; the test only asserts
        // relative to files it creates.
        let base = install_runs_repository_code("yarn.lock", &root);
        for (file, text) in [(".yarnrc.yml", "nodeLinker: pnp\n"), (".yarnrc", "x\n")] {
            let rc = top.join("a").join(file);
            std::fs::write(&rc, text).expect("rc");
            assert!(install_runs_repository_code("yarn.lock", &root), "{file}");
            std::fs::remove_file(&rc).expect("remove");
        }
        assert_eq!(install_runs_repository_code("yarn.lock", &root), base);
        std::fs::remove_dir_all(top).expect("remove");
    }

    /// DW-362: a script-free Yarn install overrides, by environment (which the install clears
    /// first), the settings `.yarnrc.yml` could use to run a release or to write outside the
    /// host's home.
    #[test]
    fn a_script_free_yarn_install_overrides_rc_program_and_folder_settings() {
        let (root, home) = (Path::new("/repo"), Path::new("/host/home"));
        let berry = b"__metadata:\n  version: 8\n".as_slice();
        let off = install_command("yarn.lock", root, home, berry, false, None).expect("yarn");
        // Paths are built with the platform's separator, as the install builds them.
        let global = home.join(".yarn/berry").to_string_lossy().into_owned();
        for (key, value) in [
            ("YARN_ENABLE_SCRIPTS", "false"),
            ("YARN_IGNORE_PATH", "true"),
            ("YARN_GLOBAL_FOLDER", global.as_str()),
        ] {
            assert!(
                off.env.contains(&(key.to_owned(), value.to_owned())),
                "{key}: {:?}",
                off.env
            );
        }
        let cache = install_home_environment(home);
        let cache_folder = home.join(".cache/yarn").to_string_lossy().into_owned();
        assert!(cache.contains(&("YARN_CACHE_FOLDER".into(), cache_folder)));
    }

    /// DW-319: a Cargo configuration of only inert settings installs without scripts; one
    /// naming a program (or anything not on the allow-list, or that does not parse) does not.
    #[test]
    fn only_cargo_configuration_naming_no_program_installs_without_scripts() {
        let root = std::env::temp_dir().join(format!("devworks-tool-cargo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".cargo")).expect("test root");
        let inert = [
            "",
            "[registries.internal]\nindex = \"sparse+https://cargo.example.invalid/index/\"\n",
            "[registries.internal]\nindex = \"https://example.invalid/i\"\ntoken = \"t\"\n\
             protocol = \"sparse\"\n[registry]\ndefault = \"internal\"\n",
            "[source.crates-io]\nreplace-with = \"vendored\"\n\
             [source.vendored]\ndirectory = \"vendor\"\n",
            "[alias]\nxtask = \"run --package xtask --\"\nfetch = \"run\"\n",
            "[net]\nretry = 3\ngit-fetch-with-cli = true\noffline = false\n",
            "[http]\nproxy = \"http://proxy.example.invalid:3128\"\ntimeout = 30\n",
            "[build]\njobs = 4\ntarget-dir = \"out\"\ntarget = \"thumbv7em-none-eabihf\"\n",
            "[build]\ntarget = [\"x86_64-unknown-linux-gnu\", \"wasm32-unknown-unknown\"]\n",
            "[build]\nrustflags = [\"--cfg\", \"tokio_unstable\", \"-C\", \"target-cpu=native\"]\n",
            "[build]\nrustflags = \"-Dwarnings -Ctarget-feature=+crt-static --cfg=x\"\n",
            "[build]\nrustflags = [\"--cfg\", \"feature=\\\"serde\\\"\", \"--cap-lints\", \"warn\", \
             \"-D\", \"clippy::all\", \"--check-cfg\", \"cfg(tokio_unstable, values(\\\"x\\\"))\"]\n",
            "[target.'cfg(target_os = \"linux\")']\nrustflags = [\"-W\", \"unused\"]\n",
            "[profile.release]\nlto = true\ncodegen-units = 1\n[term]\nverbose = true\n",
            "[patch.crates-io]\nfoo = { path = \"../foo\" }\n[cargo-new]\nvcs = \"none\"\n",
            "paths = [\"../local\"]\n[resolver]\nincompatible-rust-versions = \"fallback\"\n",
        ];
        for file in ["config.toml", "config"] {
            for text in inert {
                std::fs::write(root.join(".cargo").join(file), text).expect("config");
                for name in ["Cargo.toml", "Cargo.lock"] {
                    assert!(!install_runs_repository_code(name, &root), "{file}: {text}");
                }
            }
            std::fs::remove_file(root.join(".cargo").join(file)).expect("remove config");
        }
        let naming = [
            "[build]\nrustc = \"./planted\"\n",
            "[build]\nrustc-wrapper = \"./planted\"\n",
            "[build]\nrustc-workspace-wrapper = \"./planted\"\n",
            "[build]\nrustdoc = \"./planted\"\n",
            "build.rustc = \"./planted\"\n",
            "[build]\n\"rustc\\u002dwrapper\" = \"./planted\"\n",
            "[build]\nrustc_wrapper = \"./planted\"\n",
            "[build]\ntarget = \"./planted-spec.json\"\n",
            "[build]\ntarget = \"custom.JSON\"\n",
            "[build]\nrustflags = [\"-Z\", \"codegen-backend=./planted.so\"]\n",
            "[build]\nrustflags = \"-Ccodegen-backend=./planted.so\"\n",
            "[build]\nrustflags = [\"-C\", \"linker=./planted\"]\n",
            "[build]\nrustflags = [\"@./planted-args\"]\n",
            "[build]\nrustflags = [\"--sysroot\", \"./planted\"]\n",
            "[build]\nrustflags = [\"-C\"]\n",
            // rustc expands an `@file` argument wherever it stands, value positions included.
            "[build]\nrustflags = [\"--cfg\", \"@./args\"]\n",
            "[build]\nrustflags = [\"--check-cfg\", \"@./args\"]\n",
            "[build]\nrustflags = [\"-W\", \"@./args\"]\n",
            "[build]\nrustflags = [\"--cap-lints\", \"@./args\"]\n",
            "[build]\nrustflags = [\"-C\", \"@./args\"]\n",
            "[build]\nrustflags = [\"-C\", \"target-cpu=@./args\"]\n",
            "[build]\nrustflags = \"--cfg=@./args\"\n",
            "[build]\nrustflags = \"--cfg @./args\"\n",
            "[target.'cfg(unix)']\nrustflags = [\"--cfg\", \"@./args\"]\n",
            "[build]\nrustflags = [\"-D\", \"-Zcodegen-backend=./planted.so\"]\n",
            "[build]\nrustflags = [\"--cfg\", \"x=unquoted\"]\n",
            "[build]\nrustflags = [\"--cfg\", \"x=\\\"a\\\" --y\"]\n",
            "[build]\nrustflags = [\"--check-cfg\", \"values(@x)\"]\n",
            "[build]\nrustflags = [\"--cap-lints\", \"warnings\"]\n",
            "[build]\nrustdocflags = [\"--cfg\", \"x\"]\n",
            "[target.x86_64-unknown-linux-gnu]\nrunner = \"./planted\"\n",
            "[target.x86_64-unknown-linux-gnu]\nlinker = \"./planted\"\n",
            "[target.'cfg(unix)']\nrustflags = [\"-C\", \"link-arg=-fuse-ld=./planted\"]\n",
            "[target.x86_64-unknown-linux-gnu.openssl]\nrustc-link-lib = [\"ssl\"]\n",
            "[host]\nlinker = \"./planted\"\n",
            "[env]\nLD_PRELOAD = \"./planted.so\"\n",
            "[unstable]\nbuild-std = [\"std\"]\n",
            "include = \"./other.toml\"\n",
            "[doc]\nbrowser = \"./planted\"\n",
            "[registry]\ncredential-provider = \"./planted\"\n",
            "[registry]\nglobal-credential-providers = [\"./planted\"]\n",
            "[registries.internal]\ncredential-provider = [\"./planted\"]\n",
            "[credential-alias]\nplanted = \"./planted\"\n",
            "[net]\ngit-fetch-with-cli = true\ncommand = \"./planted\"\n",
            "[profile.dev]\nrustflags = [\"-Z\", \"x\"]\n",
            "[profile.dev.package.foo]\ncodegen-backend = \"./planted\"\n",
            "[source.planted]\nprocess = \"./planted\"\n",
            "[build]\nrustc = \"a\"\n[build]\njobs = 1\n",
            "not toml = = =\n",
        ];
        for text in naming {
            std::fs::write(root.join(".cargo/config.toml"), text).expect("config");
            assert!(install_runs_repository_code("Cargo.toml", &root), "{text}");
            // Other managers do not read it.
            assert!(!install_runs_repository_code("package.json", &root));
        }
        // An unsafe `config` counts even beside an inert `config.toml`, and invalid UTF-8 counts.
        std::fs::write(root.join(".cargo/config.toml"), "[net]\nretry = 1\n").expect("config");
        std::fs::write(
            root.join(".cargo/config"),
            "[build]\nrustc = \"./planted\"\n",
        )
        .expect("config");
        assert!(install_runs_repository_code("Cargo.lock", &root));
        std::fs::write(root.join(".cargo/config"), [0xff, 0xfe]).expect("config");
        assert!(install_runs_repository_code("Cargo.lock", &root));
        std::fs::remove_dir_all(root).expect("remove test root");
    }

    /// DW-386: a script-free Node install must not run repository code or read outside the
    /// checkout: `exec:` and git dependencies and out-of-checkout `file:`/`link:`/`portal:`
    /// paths, in any `package.json` or the lockfile, need `allowScripts`; registry dependencies,
    /// `patch:`, and paths inside the checkout do not.
    #[test]
    fn a_dependency_that_runs_repository_code_needs_install_scripts() {
        let root =
            std::env::temp_dir().join(format!("devworks-tool-source-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("packages/a")).expect("test root");
        let managers = [
            "package-lock.json",
            "package.json",
            "pnpm-lock.yaml",
            "yarn.lock",
        ];
        let write = |file: &str, text: &str| std::fs::write(root.join(file), text).expect(file);
        let safe_lock = "__metadata:\n  version: 8\n\n\"exec@npm:^0.2.0\":\n  version: 0.2.0\n  \
             resolution: \"exec@npm:0.2.0\"\n  dependencies:\n    exec: ^1\n    \"@babel/core\": \"npm:^7\"\n\
             \"lodash@patch:lodash@npm%3A4.17.21#./p.patch::version=4.17.21&hash=1&locator=app%40workspace%3A.\":\n  \
             resolution: \"lodash@patch:lodash@npm%3A4.17.21#./p.patch::version=4.17.21&locator=app%40workspace%3A.\"\n  \
             resolved \"https://registry.yarnpkg.com/@babel/core/-/core-7.0.0.tgz#abc\"\n  \
             resolved \"https://npm.pkg.github.com/x/-/x-1.0.0.tgz\"\n  \
             \"link:./local\"\n  \"file:packages/a\"\n";
        write(
            "package.json",
            r#"{"name":"x","dependencies":{"lodash":"^4.17.21","a":"npm:b@^1","w":"workspace:*",
               "p":"patch:lodash@npm:4.17.21#./p.patch","l":"link:./local","f":"file:../root-sibling-ok-no"},
               "devDependencies":{"t":"https://example.invalid/t.tgz","s":"~1.2.3"}}"#,
        );
        // `file:../…` from the checkout root leaves it.
        assert!(install_runs_repository_code("package.json", &root));
        write(
            "package.json",
            r#"{"name":"x","dependencies":{"lodash":"^4.17.21","a":"npm:b@^1","w":"workspace:*",
               "p":"patch:lodash@npm:4.17.21#./p.patch","l":"link:./local","f":"file:packages/a",
               "e":"exec"},"devDependencies":{"t":"https://example.invalid/t.tgz","s":"~1.2.3",
               "dir":"./vendor/x","pr":"portal:./vendor/y"},"resolutions":{"lodash":"4.17.21"}}"#,
        );
        write(
            "packages/a/package.json",
            r#"{"dependencies":{"up":"link:../b"}}"#,
        );
        for name in managers {
            match name {
                "package.json" => {}
                "package-lock.json" => write(
                    name,
                    r#"{"lockfileVersion":3,"packages":{"":{},"node_modules/x":{"resolved":"https://registry.npmjs.org/x/-/x-1.tgz"},"node_modules/l":{"resolved":"local","link":true},"local":{}}}"#,
                ),
                _ => write(name, safe_lock),
            }
            assert!(!install_runs_repository_code(name, &root), "{name}");
        }
        let unsafe_specs = [
            "exec:./gen.js",
            "EXEC:./gen.js",
            "npm:foo@exec:./gen.js",
            "git+https://example.invalid/r.git",
            "git://example.invalid/r",
            "git@github.com:o/r.git",
            "github:o/r",
            "gitlab:o/r",
            "bitbucket:o/r",
            "o/r",
            "o/r#semver:^1",
            "https://example.invalid/r.git#abc",
            "https://github.com/o/r/archive/main.tar.gz",
            "file:/etc",
            "file:../outside",
            "link:~/x",
            "portal:../../x",
            "portal:C:/x",
            "../outside",
            "/abs",
            "~/home",
            "file:./a/../../out",
        ];
        for spec in unsafe_specs {
            for (field, wrapped) in [
                (
                    "dependencies",
                    format!(r#"{{"dependencies":{{"x":"{spec}"}}}}"#),
                ),
                (
                    "devDependencies",
                    format!(r#"{{"devDependencies":{{"x":"{spec}"}}}}"#),
                ),
                (
                    "optionalDependencies",
                    format!(r#"{{"optionalDependencies":{{"x":"{spec}"}}}}"#),
                ),
                (
                    "peerDependencies",
                    format!(r#"{{"peerDependencies":{{"x":"{spec}"}}}}"#),
                ),
                (
                    "resolutions",
                    format!(r#"{{"resolutions":{{"x@npm:1":"{spec}"}}}}"#),
                ),
                (
                    "overrides",
                    format!(r#"{{"overrides":{{"x":{{"y":"{spec}"}}}}}}"#),
                ),
                (
                    "pnpm",
                    format!(r#"{{"pnpm":{{"overrides":{{"x":"{spec}"}}}}}}"#),
                ),
            ] {
                write("package.json", &wrapped);
                for name in managers {
                    if name != "package.json" {
                        let _ = std::fs::remove_file(root.join(name));
                    }
                    assert!(install_runs_repository_code(name, &root), "{field}: {spec}");
                }
            }
            // A workspace package.json is checked too.
            write("package.json", r#"{"name":"x"}"#);
            write(
                "packages/a/package.json",
                &format!(r#"{{"dependencies":{{"x":"{spec}"}}}}"#),
            );
            // From `packages/a` (two folders down), up to two `..` stay inside the checkout.
            let inside = matches!(
                spec,
                "file:../outside" | "portal:../../x" | "../outside" | "file:./a/../../out"
            );
            assert_eq!(
                install_runs_repository_code("yarn.lock", &root),
                !inside,
                "workspace: {spec}"
            );
            write("packages/a/package.json", r#"{"name":"a"}"#);
        }
        // Lockfiles: a Berry resolution, an exec: descriptor, a Yarn 1 git line, a pnpm git
        // resolution, an npm git `resolved`.
        for (name, text) in [
            (
                "yarn.lock",
                "\"x@exec:./gen.js\":\n  resolution: \"x@exec:./gen.js::locator=a%40workspace%3A.\"\n",
            ),
            (
                "yarn.lock",
                "\"x@patch:x@exec%3A./gen.js#./p.patch\":\n  version: 1\n",
            ),
            ("yarn.lock", "  dependencies:\n    x: exec:./gen.js\n"),
            (
                "yarn.lock",
                "  resolution: \"x@https://github.com/o/r.git#commit=abc\"\n",
            ),
            (
                "yarn.lock",
                "x@o/r:\n  resolved \"git+ssh://git@github.com/o/r.git#abc\"\n",
            ),
            (
                "yarn.lock",
                "  resolution: \"x@portal:../x::locator=a%40workspace%3A.\"\n",
            ),
            (
                "pnpm-lock.yaml",
                "packages:\n  x@1.0.0:\n    resolution: {commit: abc, repo: https://example.invalid/r, type: git}\n",
            ),
            ("pnpm-lock.yaml", "    x:\n      version: link:../x\n"),
            (
                "package-lock.json",
                "{\"packages\":{\"node_modules/x\":{\"resolved\":\"git+ssh://git@github.com/o/r.git#abc\"}}}",
            ),
            (
                "package-lock.json",
                "{\"packages\":{\"node_modules/x\":{\"resolved\":\"git+https://example.invalid/r\"}}}",
            ),
        ] {
            for other in managers {
                let _ = std::fs::remove_file(root.join(other));
            }
            write("package.json", r#"{"name":"x"}"#);
            write(name, text);
            assert!(install_runs_repository_code(name, &root), "{name}: {text}");
        }
        // An unparseable workspace package.json cannot be ruled out.
        for name in managers {
            let _ = std::fs::remove_file(root.join(name));
        }
        write("package.json", r#"{"name":"x"}"#);
        write("packages/a/package.json", "{");
        assert!(install_runs_repository_code("package.json", &root));
        std::fs::remove_dir_all(root).expect("remove test root");
    }

    /// DW-386 review: escapes, bare paths, workspace globs, patches and sibling links.
    #[test]
    fn lockfile_escapes_bare_paths_and_globs_are_checked() {
        let root =
            std::env::temp_dir().join(format!("devworks-tool-lockfiles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("test root");
        let managers = [
            "package-lock.json",
            "npm-shrinkwrap.json",
            "pnpm-lock.yaml",
            "pnpm-workspace.yaml",
            "yarn.lock",
            "package.json",
        ];
        let reset = |files: &[(&str, &str)]| {
            for name in managers {
                let _ = std::fs::remove_file(root.join(name));
            }
            std::fs::write(root.join("package.json"), r#"{"name":"x"}"#).expect("package");
            for (name, text) in files {
                std::fs::write(root.join(name), text).expect(name);
            }
        };
        let refused = |manager: &str, files: &[(&str, &str)]| {
            reset(files);
            assert!(install_runs_repository_code(manager, &root), "{files:?}");
        };
        let allowed = |manager: &str, files: &[(&str, &str)]| {
            reset(files);
            assert!(!install_runs_repository_code(manager, &root), "{files:?}");
        };
        // Escapes that hide a source.
        refused(
            "yarn.lock",
            &[("yarn.lock", "\"x@\\u0065xec:./g.js\":\n  version: 1\n")],
        );
        refused(
            "yarn.lock",
            &[("yarn.lock", "x:\n  resolution: \"x@exec\\u003a./g.js\"\n")],
        );
        refused(
            "yarn.lock",
            &[("yarn.lock", "x:\n  resolution: \"x@ex\\\nec:./g.js\"\n")],
        );
        refused(
            "pnpm-lock.yaml",
            &[("pnpm-lock.yaml", "a: \"\\x65xec:./g.js\"\n")],
        );
        refused(
            "pnpm-lock.yaml",
            &[("pnpm-lock.yaml", "a: \"ex\\\n  ec:./g.js\"\n")],
        );
        for name in ["package-lock.json", "npm-shrinkwrap.json"] {
            refused(
                "package-lock.json",
                &[(
                    name,
                    r#"{"packages":{"node_modules/x":{"resolved":"\u0065xec:./g.js"}}}"#,
                )],
            );
            refused(
                "package-lock.json",
                &[(
                    name,
                    r#"{"packages":{"node_modules/x":{"resolved":"exec\u003a./g.js"}}}"#,
                )],
            );
            refused(
                "package-lock.json",
                &[(
                    name,
                    r#"{"packages":{"node_modules/x":{"resolved":"git+ssh://git@github.com/o/r.git"}}}"#,
                )],
            );
            refused("package-lock.json", &[(name, "{")]);
            // Funding metadata naming GitHub is not a source; a git resolution beside it is.
            allowed(
                "package-lock.json",
                &[(
                    name,
                    r#"{"packages":{"node_modules/x":{"resolved":"https://registry.npmjs.org/x/-/x-1.0.0.tgz","funding":{"url":"https://github.com/sponsors/o"},"license":"MIT"}}}"#,
                )],
            );
            refused(
                "package-lock.json",
                &[(
                    name,
                    r#"{"packages":{"node_modules/x":{"funding":{"url":"https://github.com/sponsors/o"},"dependencies":{"y":"github:o/y"}}}}"#,
                )],
            );
            // A linked package and a package location outside the checkout.
            refused(
                "package-lock.json",
                &[(
                    name,
                    r#"{"packages":{"node_modules/x":{"resolved":"../outside","link":true}}}"#,
                )],
            );
            refused(
                "package-lock.json",
                &[(name, r#"{"packages":{"../outside":{}}}"#)],
            );
            refused(
                "package-lock.json",
                &[(
                    name,
                    r#"{"packages":{"node_modules/x":{"resolved":"C:\\x","link":true}}}"#,
                )],
            );
        }
        allowed(
            "package-lock.json",
            &[(
                "npm-shrinkwrap.json",
                r#"{"packages":{"node_modules/x":{"resolved":"pkgs/x","link":true},"pkgs/x":{}}}"#,
            )],
        );
        // Bare paths in pnpm lockfiles, and a drive path.
        refused(
            "pnpm-lock.yaml",
            &[(
                "pnpm-lock.yaml",
                "packages:\n  x@file:../x:\n    resolution: {directory: ../x, type: directory}\n",
            )],
        );
        refused(
            "pnpm-lock.yaml",
            &[(
                "pnpm-lock.yaml",
                "packages:\n  x:\n    resolution:\n      directory: /abs\n",
            )],
        );
        refused(
            "pnpm-lock.yaml",
            &[(
                "pnpm-lock.yaml",
                "packages:\n  x:\n    resolution: {directory: 'C:/x'}\n",
            )],
        );
        allowed(
            "pnpm-lock.yaml",
            &[(
                "pnpm-lock.yaml",
                "packages:\n  x@file:vendor/x:\n    resolution: {directory: vendor/x, type: directory}\n",
            )],
        );
        // Sibling links resolve against the importer (pnpm) or workspace (Berry).
        let importers = |link: &str| {
            format!(
                "importers:\n  .:\n    dependencies:\n      r:\n        version: link:./r\n  packages/a:\n    dependencies:\n      b:\n        specifier: workspace:*\n        version: {link}\n"
            )
        };
        allowed(
            "pnpm-lock.yaml",
            &[("pnpm-lock.yaml", &importers("link:../b"))],
        );
        refused(
            "pnpm-lock.yaml",
            &[("pnpm-lock.yaml", &importers("link:../../../b"))],
        );
        refused(
            "pnpm-lock.yaml",
            &[(
                "pnpm-lock.yaml",
                "importers:\n  ../x:\n    dependencies: {}\n",
            )],
        );
        // The root importer cannot climb.
        refused(
            "pnpm-lock.yaml",
            &[(
                "pnpm-lock.yaml",
                "importers:\n  .:\n    dependencies:\n      b:\n        version: link:../b\n",
            )],
        );
        allowed(
            "yarn.lock",
            &[(
                "yarn.lock",
                "\"b@portal:../b::locator=a%40workspace%3Apackages/a\":\n  resolution: \"b@portal:../b::locator=a%40workspace%3Apackages/a\"\n\n\"a@workspace:packages/a\":\n  dependencies:\n    b: portal:../b\n",
            )],
        );
        refused(
            "yarn.lock",
            &[(
                "yarn.lock",
                "\"b@portal:../../../b::locator=a%40workspace%3Apackages/a\":\n  version: 0.0.0\n",
            )],
        );
        refused(
            "yarn.lock",
            &[("yarn.lock", "\"b@portal:../b\":\n  version: 0.0.0\n")],
        );
        // pnpm-workspace.yaml.
        allowed(
            "pnpm-lock.yaml",
            &[(
                "pnpm-workspace.yaml",
                "packages:\n  - packages/*\n  - '!**/test/**'\ncatalog:\n  a: ^1\noverrides:\n  b: npm:c@^1\n",
            )],
        );
        for text in [
            "packages:\n  - ../other/*\n",
            "packages:\n  - /abs/*\n",
            "packages:\n  - packages/*/../../..\n",
            "overrides:\n  a: exec:./g.js\n",
            "catalog:\n  a: github:o/r\n",
            "catalogs:\n  x:\n    a: link:../x\n",
            "overrides: &o\n  a: ^1\n",
            "overrides:\n  a: !!str exec:./g.js\n",
            "catalog:\n  a: *x\n",
            "packages: [",
        ] {
            refused("pnpm-lock.yaml", &[("pnpm-workspace.yaml", text)]);
        }
        // package.json: pnpm patches and extensions, workspace globs, Windows paths.
        allowed(
            "package.json",
            &[(
                "package.json",
                r#"{"workspaces":["packages/*"],"pnpm":{"patchedDependencies":{"x@1":"patches/x.patch"}}}"#,
            )],
        );
        allowed(
            "package.json",
            &[(
                "package.json",
                r#"{"workspaces":{"packages":["apps/*"]},"bundleDependencies":["../not-a-path"]}"#,
            )],
        );
        for text in [
            r#"{"pnpm":{"patchedDependencies":{"x@1":"../outside.patch"}}}"#,
            r#"{"pnpm":{"patchedDependencies":{"x@1":"/abs.patch"}}}"#,
            r#"{"pnpm":{"packageExtensions":{"x":{"dependencies":{"y":"exec:./g.js"}}}}}"#,
            r#"{"workspaces":["../*"]}"#,
            r#"{"workspaces":{"packages":["/abs/*"]}}"#,
            r#"{"workspaces":["packages/*/../../.."]}"#,
            r#"{"dependencies":{"x":"C:\\x"}}"#,
            r#"{"dependencies":{"x":"C:/x"}}"#,
            r#"{"dependencies":{"x":"..\\x"}}"#,
            r#"{"dependencies":{"x":".\\x"}}"#,
        ] {
            refused("package.json", &[("package.json", text)]);
        }
        // Windows drive and backslash paths in lockfiles.
        refused(
            "yarn.lock",
            &[("yarn.lock", "x:\n  dependencies:\n    y: file:..\\y\n")],
        );
        refused(
            "yarn.lock",
            &[("yarn.lock", "x:\n  dependencies:\n    y: C:/y\n")],
        );
        std::fs::remove_dir_all(root).expect("remove test root");
    }

    /// DW-318: Corepack downloads and runs the release a `packageManager` URL names before any
    /// manager sees a script flag, so a Node install with one (or with a `package.json` whose
    /// field cannot be checked) needs `allowScripts`; a registry version is fine.
    #[test]
    fn a_corepack_release_url_in_package_json_needs_install_scripts() {
        let root =
            std::env::temp_dir().join(format!("devworks-tool-corepack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("test root");
        let managers = [
            "package-lock.json",
            "package.json",
            "pnpm-lock.yaml",
            "yarn.lock",
        ];
        for name in managers {
            assert!(!install_runs_repository_code(name, &root), "{name}");
        }
        for safe in [
            r#"{"name":"x"}"#,
            r#"{"packageManager":"yarn@1.22.22"}"#,
            r#"{"packageManager":"pnpm@9.15.0+sha512.76e2379760a4328ec4415815bcd6628dee727af3779aaa4c914e3944156c4299921a89f976381ee107d41f12cfa4b66681ca9c718f0668fa0831ed4c6d8ba56c"}"#,
        ] {
            std::fs::write(root.join("package.json"), safe).expect("package.json");
            for name in managers {
                assert!(!install_runs_repository_code(name, &root), "{name}: {safe}");
            }
        }
        for unsafe_file in [
            r#"{"packageManager":"yarn@https://example.invalid/yarn-4.0.0.tgz"}"#,
            r#"{"packageManager":"yarn@https://example.invalid/yarn.tgz#sha1.0"}"#,
            r#"{"packageManager":"yarn@1.22.22","packageManager":"yarn@file:./yarn.js"}"#,
            r#"{"packageManager":"yarn@ 1.22.22"}"#,
            r#"{"packageManager":["yarn@1.22.22"]}"#,
            r#"{"packageManager":"yarn@1.22.22","x":"\ud800"}"#,
            "[]",
        ] {
            std::fs::write(root.join("package.json"), unsafe_file).expect("package.json");
            for name in managers {
                assert!(
                    install_runs_repository_code(name, &root),
                    "{name}: {unsafe_file}"
                );
            }
            // Other managers do not read the field.
            assert!(!install_runs_repository_code("go.mod", &root));
        }
        std::fs::remove_dir_all(root).expect("remove test root");
    }

    /// DW-318: every install keeps its home, temporary folder and package caches in the host's
    /// folder for the checkout, and a script-free install overrides each repository setting
    /// the audit found that names a program.
    #[test]
    fn install_homes_and_caches_are_the_hosts_and_program_settings_are_overridden() {
        let home = Path::new("/host/home");
        let env = install_home_environment(home);
        for name in [
            "HOME",
            "TMPDIR",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "CARGO_HOME",
            "npm_config_cache",
            "YARN_CACHE_FOLDER",
            "COREPACK_HOME",
            "PIP_CACHE_DIR",
            "UV_CACHE_DIR",
            "GOPATH",
            "GOMODCACHE",
            "COMPOSER_HOME",
            "COMPOSER_CACHE_DIR",
        ] {
            let value = env
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| Path::new(value))
                .unwrap_or_else(|| panic!("{name} is unset"));
            assert!(value.starts_with(home), "{name}={value:?}");
        }
        let env = script_free_environment();
        let value = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        let shell = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" };
        for (name, expected) in [
            ("GIT_SSH_COMMAND", "ssh"),
            ("GIT_PROXY_COMMAND", ""),
            ("GIT_ASKPASS", ""),
            ("npm_config_git", "git"),
            ("npm_config_shell", shell),
            ("npm_config_script_shell", shell),
            ("npm_config_ignore_scripts", "true"),
            ("COREPACK_ENV_FILE", "0"),
            ("GOENV", "off"),
            ("GOTOOLCHAIN", "local"),
        ] {
            assert_eq!(value(name), Some(expected), "{name}");
        }
        let count: usize = value("GIT_CONFIG_COUNT").expect("count").parse().unwrap();
        let git: Vec<(&str, &str)> = (0..count)
            .map(|index| {
                (
                    value(&format!("GIT_CONFIG_KEY_{index}")).unwrap(),
                    value(&format!("GIT_CONFIG_VALUE_{index}")).unwrap(),
                )
            })
            .collect();
        for setting in [
            ("credential.helper", ""),
            ("core.fsmonitor", "false"),
            ("protocol.ext.allow", "never"),
            ("safe.bareRepository", "explicit"),
        ] {
            assert!(git.contains(&setting), "{setting:?} in {git:?}");
        }
        assert!(git.iter().any(|(key, _)| *key == "core.hooksPath"));
    }

    #[test]
    fn install_scripts_run_only_for_the_user_an_approved_replay_or_a_native_grant() {
        let ctx = |kind| {
            InvocationContext::new(
                crate::protocol::ActorId(id("caller-1")),
                kind,
                id("invocation-1"),
                id("ingress-1"),
                crate::protocol::StoreId(id("store-1")),
                counter(0),
                rev(1),
            )
        };
        assert!(scripts_approved(&ctx(PrincipalKind::Human), false));
        for kind in [
            PrincipalKind::Agent,
            PrincipalKind::Host,
            PrincipalKind::Package,
            PrincipalKind::McpClient,
            PrincipalKind::Schedule,
        ] {
            assert!(!scripts_approved(&ctx(kind), false), "{kind:?}");
            assert!(scripts_approved(&ctx(kind), true), "{kind:?}");
        }
        let reviewed =
            ctx(PrincipalKind::Agent).with_reviewed_operation(crate::context::ReviewedOperation {
                approval_id: id("approval-1"),
                approver: crate::protocol::ActorId(id("user-1")),
                operation: "process.install_project_dependencies".into(),
                schema_revision: 8,
                request_digest: "a".repeat(64),
                owner_binding: json!({}),
            });
        assert!(scripts_approved(&reviewed, false));
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
