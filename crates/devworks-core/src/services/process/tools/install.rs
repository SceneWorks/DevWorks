//! Dependency install commands and their jobs.
use super::*;

/// One package-manager invocation of a dependency install.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct InstallCommand {
    pub(super) program: &'static str,
    pub(super) args: Vec<String>,
    pub(super) env: Vec<(String, String)>,
}

/// The interpreter and environment the host chooses for a script-free uv install.
#[derive(Debug)]
pub(super) struct HostPython {
    /// A canonical interpreter resolved through the executable allowlist.
    pub(super) interpreter: String,
    /// The project environment, in a directory the host owns outside the repository.
    pub(super) environment: PathBuf,
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
pub(super) fn install_command(
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
pub(super) fn install_home_environment(home: &Path) -> Vec<(String, String)> {
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
pub(super) fn script_free_environment() -> Vec<(String, String)> {
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct InstallJob {
    pub(super) actor: crate::protocol::ActorId,
    pub(super) kind: String,
    pub(super) session: Option<Id>,
    pub(super) project: Option<ProjectId>,
    pub(super) root: Option<crate::protocol::RootId>,
    pub(super) assignment: Option<crate::protocol::AssignmentId>,
    pub(super) restore_epoch: i64,
    pub(super) policy_revision: i64,
    pub(super) fingerprint: String,
    #[serde(default)]
    pub(super) authority_sources: Vec<Id>,
    pub(super) repository_id: Id,
    pub(super) expected_revision: crate::protocol::Revision,
    pub(super) manifest: ArtifactRef,
    pub(super) profile: ToolProfile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) allow_scripts: Option<bool>,
}

impl InstallJob {
    pub(super) fn from_context(
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
    pub(super) fn context(
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
    pub(super) fn input(&self) -> ops::InstallProjectDependenciesInput {
        ops::InstallProjectDependenciesInput {
            repository_id: self.repository_id.clone(),
            expected_revision: self.expected_revision.clone(),
            manifest: self.manifest.clone(),
            profile: self.profile.clone(),
            allow_scripts: self.allow_scripts,
        }
    }
}

pub(super) struct InstallPlan {
    pub(super) spec: NativeProcessSpec,
    /// The install's home, in the host's folder for the checkout.
    pub(super) home: PathBuf,
}
