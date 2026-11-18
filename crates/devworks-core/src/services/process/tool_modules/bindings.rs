//! Registered executables, working roots and container credentials, and how a request's
//! names resolve against them.
use super::*;

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
    pub(super) executables: HashMap<String, (PathBuf, PathBuf)>,
    /// Configured names resolved on PATH when first selected, optionally limited to a fixed
    /// set (a container backend may select only its own CLI).
    pub(super) path_executables: Option<(Arc<PathExecutables>, Option<&'static [&'static str]>)>,
    pub(super) roots: HashMap<String, RegisteredWorkingRoot>,
    pub(super) credentials: HashMap<String, RegisteredCredential>,
    /// A host-owned directory, outside every repository, that holds each checkout's install
    /// home (its home, temporary folder and package caches) and the environment of each
    /// script-free uv install. Without one, every dependency install is refused.
    pub(super) install_environments: Option<PathBuf>,
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

    pub(super) fn launch_of(
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

    pub(super) fn executable(
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
            Some(read) => {
                registry.lookup_in(name, super::super::executables::configured_names(read).ok())
            }
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
    pub(super) fn resolve_credentials(
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
pub(super) fn exact_file(path: &Path, target: &Path) -> Option<PathBuf> {
    let actual = path.canonicalize().ok()?;
    (actual == target && actual.is_file()).then(|| launch_path(actual))
}

/// Windows canonical paths carry the verbatim `\\?\` prefix, which the child would see in
/// its command line. A plain drive path short enough for the classic API means the same file,
/// so it launches without the prefix; any other verbatim path is kept as it is.
#[cfg(windows)]
pub(super) fn launch_path(canonical: PathBuf) -> PathBuf {
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
pub(super) fn launch_path(canonical: PathBuf) -> PathBuf {
    canonical
}

pub(super) fn exact_directory(path: &Path) -> Option<PathBuf> {
    let actual = path.canonicalize().ok()?;
    (actual == path && actual.is_dir()).then_some(actual)
}

/// Whether the invocation's project (if it has one) has `repository` as a member.
pub(super) fn visible_to(
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
pub(super) fn owned_by(tree: &StoredGitWorktree, ctx: &InvocationContext) -> bool {
    !tree.removed
        && tree.owner == *ctx.principal()
        && tree.owner_root_id.as_ref() == ctx.root_id()
        && tree.owner_assignment_id.as_ref() == ctx.assignment_id()
}

/// Whether a dependency install may run package lifecycle scripts (arbitrary dependency code):
/// the user's own request, the replay of a person's approval of the exact request (the staged
/// saved-operation approval), or a request admitted by spending a reviewed native grant, which
/// always binds the exact request. Anything else is refused `ApprovalRequired` and staged.
pub(super) fn scripts_approved(ctx: &InvocationContext, native_grant: bool) -> bool {
    ctx.principal_kind() == PrincipalKind::Human
        || ctx.reviewed_operation().is_some()
        || native_grant
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
