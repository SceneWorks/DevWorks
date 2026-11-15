//! DW-302 dependency installs over the real runtime: `process.install_project_dependencies`
//! runs the package manager with lifecycle scripts disabled unless an approved request allows
//! them, and its working root follows the same rule as `process.execute`. The package manager is
//! a fake `npm` shell script that records where it ran and, like npm, runs the project's
//! lifecycle script unless it is passed `--ignore-scripts`; nothing is downloaded. The Python
//! cases run the host's `python3` (pip) and a fake `uv` against a repository that plants its own
//! `pip` module and `.venv` interpreter.
#![cfg(unix)]
use super::watch_tests::{agent_context, frame};
use super::*;
use devworks_core::authority::destructive::FilesystemResolver;
use devworks_core::protocol::data::{GitOid, ObjectFormat};
use devworks_core::services::process::tools::{RegisteredExecutable, ToolBindings};
use devworks_engine::runtime::{ProcessToolComposition, Runtime};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const MANIFEST: &[u8] =
    br#"{"name":"fixture","scripts":{"postinstall":"touch lifecycle-script-ran"}}"#;
const FAKE_NPM: &str = "#!/bin/sh\n\
pwd > .install-ran\n\
printf %s \"$GIT_CONFIG_GLOBAL\" > .git-config-global\n\
for argument in \"$@\"; do\n\
  [ \"$argument\" = --ignore-scripts ] && exit 0\n\
done\n\
touch lifecycle-script-ran\n";

fn install_composition(fx: &Fx, tools: &Path) -> Composition {
    let npm = tools.join("npm");
    std::fs::write(&npm, FAKE_NPM).unwrap();
    std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).unwrap();
    composition_with_tools(
        fx,
        vec![
            RegisteredExecutable {
                id: id("npm"),
                path: npm,
            },
            RegisteredExecutable {
                id: id("sh"),
                path: "/bin/sh".into(),
            },
        ],
        None,
    )
}

fn composition_with_tools(
    fx: &Fx,
    executables: Vec<RegisteredExecutable>,
    install_environments: Option<&Path>,
) -> Composition {
    let mut composed = composition_with(
        fx,
        None,
        Arc::new(FakePower::default()),
        Arc::new(Authority),
        Arc::new(devworks_adapters::process::NativeProcessHost),
    );
    let mut bindings = ToolBindings::new(executables, []).unwrap();
    if let Some(directory) = install_environments {
        bindings = bindings.with_install_environments(directory.to_path_buf());
    }
    composed.config.process_tools = Some(ProcessToolComposition {
        bindings,
        filesystem: Arc::new(NativeFilesystem::new()),
        protected_roots: ProtectedRoots::new(Vec::new(), Vec::new(), None),
        secrets: Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
    });
    composed
}

fn decide(
    app: &Runtime,
    ctx: &InvocationContext,
    op: &str,
    body: Value,
    key: &str,
) -> DomainDecision {
    let mut frame: Value = serde_json::from_slice(&frame("Command", op, body, key)).unwrap();
    if op == "process.execute" {
        // process.execute negotiates only its first schema revision.
        frame.as_object_mut().unwrap().remove("schemaRevision");
    }
    match app
        .command(ctx, op, &serde_json::to_vec(&frame).unwrap())
        .unwrap()
    {
        CommandReply::Recorded { receipt, .. } => receipt.decision,
        other => panic!("{op}: {other:?}"),
    }
}

/// Waits until the accepted operation has completed and returns its final state.
async fn settled(app: &Runtime, decision: DomainDecision) -> OperationState {
    let DomainDecision::Accepted { operation_id } = decision else {
        panic!("expected an accepted operation, got {decision:?}");
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let operation =
            read(&**app.store(), |s| s.operations().operation(&operation_id)).expect("operation");
        if matches!(
            operation.state,
            OperationState::ObservedComplete
                | OperationState::Failed
                | OperationState::OutcomeUnknown
        ) {
            return operation.state;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "operation did not settle: {operation:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn install(allow_scripts: Option<bool>, manifest: &Value) -> Value {
    let mut body = json!({"repositoryId": "install-repository", "expectedRevision": "1",
        "manifest": manifest, "profile": "Full"});
    if let Some(allow) = allow_scripts {
        body["allowScripts"] = json!(allow);
    }
    body
}

fn identity(path: &Path) -> devworks_core::authority::destructive::FileIdentity {
    NativeFilesystem::new()
        .resolve(path.to_str().unwrap(), None)
        .unwrap()
        .target
        .unwrap()
        .identity
        .unwrap()
}

/// Registers `location` as the project's repository and saves `bytes` as the live manifest
/// artifact the install binds; returns the manifest reference an install request names.
fn register_manifest(
    fx: &Fx,
    location: &Path,
    title: &str,
    bytes: &[u8],
    media_type: &str,
) -> Value {
    let digest = devworks_core::fingerprint::sha256_hex(bytes);
    let location = location.to_string_lossy().into_owned();
    let (blob, title, media, length) = (
        digest.clone(),
        title.to_owned(),
        media_type.to_owned(),
        bytes.len() as i64,
    );
    fx.tx(move |_, tx| {
        let now = tx.now_ms();
        tx.projects().insert_repository(&StoredRepository {
            id: id("install-repository"),
            location,
            name: "install repository".into(),
            remote_url: None,
            primary: true,
            lifecycle: "Active".into(),
            languages: Vec::new(),
            revision: 1,
            registered_at_ms: now,
        })?;
        let project = ProjectId(id("project-a"));
        let mut members = tx.projects().members(&project)?;
        members.push(id("install-repository"));
        tx.projects().set_members(&project, &members)?;
        tx.artifacts()
            .put_blob(&blob, length, "fixture", "artifacts/manifest")?;
        tx.artifacts().insert_artifact(&StoredArtifact {
            id: id("install-manifest"),
            origin_project_id: Some(project.clone()),
            origin_work_id: None,
            title,
            revision: 1,
            scope: ArtifactScope::Project(project),
            owner: None,
            retention: ArtifactRetention::Explicit,
            scope_revision: 1,
            reference_revision: 1,
            current_version_id: None,
            state: ArtifactState::Live,
            created_at_ms: T0,
            provenance: json!({}),
        })?;
        tx.artifacts().insert_version(&StoredVersion {
            id: id("install-manifest-1"),
            artifact_id: id("install-manifest"),
            version_no: 1,
            blob_digest: blob,
            byte_count: length,
            blob_generation: "fixture".into(),
            media_type: media,
            created_at_ms: T0,
            deleted_at_ms: None,
            source: json!({}),
        })
    });
    json!({"artifactId": "install-manifest", "versionId": "install-manifest-1",
        "sha256": digest, "byteLength": bytes.len().to_string(), "mediaType": media_type})
}

#[test]
fn a_dependency_install_runs_no_lifecycle_script_unless_approved_and_shares_the_working_root_rule()
{
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    let work = fx.h.root.with_extension("install-scripts-work");
    let _ = std::fs::remove_dir_all(&work);
    for directory in ["tools", "primary", "worktree"] {
        std::fs::create_dir_all(work.join(directory)).unwrap();
    }
    let work = work.canonicalize().unwrap();
    let (primary, worktree) = (work.join("primary"), work.join("worktree"));
    for checkout in [&primary, &worktree] {
        std::fs::write(checkout.join("package.json"), MANIFEST).unwrap();
        std::fs::create_dir_all(checkout.join("work")).unwrap();
    }
    let rt = runtime();
    rt.block_on(async {
        let app = Arc::new(
            boot(install_composition(&fx, &work.join("tools")))
                .await
                .unwrap(),
        );
        let (agent, _) = agent_context(&fx);
        let manifest =
            register_manifest(&fx, &primary, "package.json", MANIFEST, "application/json");
        let ran = |checkout: &Path| checkout.join(".install-ran").exists();
        let scripted = |checkout: &Path| checkout.join("lifecycle-script-ran").exists();

        // An agent with no worktree of the repository installs in its primary checkout, with the
        // lifecycle script disabled.
        let state = settled(
            &app,
            decide(
                &app,
                &agent,
                "process.install_project_dependencies",
                install(None, &manifest),
                "install-agent",
            ),
        )
        .await;
        assert_eq!(state, OperationState::ObservedComplete);
        assert!(ran(&primary));
        assert!(!scripted(&primary), "an install ran a lifecycle script");
        // Git reads no global configuration from the home inside the repository.
        assert_eq!(
            std::fs::read_to_string(primary.join(".git-config-global")).unwrap(),
            "/dev/null"
        );

        // An agent asking to run the scripts is held for a person's approval of that request.
        let held = decide(
            &app,
            &agent,
            "process.install_project_dependencies",
            install(Some(true), &manifest),
            "install-agent-scripts",
        );
        assert!(
            matches!(&held, DomainDecision::Refused { fault }
                if fault.code == DomainErrorCode::ApprovalRequired),
            "{held:?}"
        );
        assert!(!scripted(&primary));

        // The user's own request to run them does.
        let state = settled(
            &app,
            decide(
                &app,
                &fx.user,
                "process.install_project_dependencies",
                install(Some(true), &manifest),
                "install-user-scripts",
            ),
        )
        .await;
        assert_eq!(state, OperationState::ObservedComplete);
        assert!(scripted(&primary));
        std::fs::remove_file(primary.join("lifecycle-script-ran")).unwrap();
        std::fs::remove_file(primary.join(".install-ran")).unwrap();

        // Once the agent owns a worktree of the repository, the install and a process naming the
        // repository both resolve to that worktree.
        let (owner, root, assignment) = (
            agent.principal().clone(),
            agent.root_id().cloned().expect("agent root"),
            agent.assignment_id().cloned(),
        );
        let tree_location = GitWorktreeLocation {
            root: identity(&worktree),
            git_dir: identity(&worktree),
            common_dir: identity(&worktree),
        };
        let tree_path = worktree.to_string_lossy().into_owned();
        fx.tx(move |_, tx| {
            tx.git().insert_git_worktree(&StoredGitWorktree {
                id: id("install-tree"),
                repository_id: id("install-repository"),
                work_id: root.0.clone(),
                owner,
                ownership_revision: 1,
                owner_root_id: Some(root),
                owner_assignment_id: assignment,
                registered_root_id: id("install-repository"),
                path: tree_path,
                branch: "refs/heads/install".into(),
                head: GitOid {
                    format: ObjectFormat::Sha1,
                    hex: "a".repeat(40),
                },
                revision: 1,
                removed: false,
                location: Some(tree_location),
                pending_operation_id: None,
                last_operation_id: None,
                last_result: None,
            })
        });
        let state = settled(
            &app,
            decide(
                &app,
                &agent,
                "process.install_project_dependencies",
                install(Some(false), &manifest),
                "install-agent-worktree",
            ),
        )
        .await;
        assert_eq!(state, OperationState::ObservedComplete);
        assert!(ran(&worktree) && !ran(&primary));
        assert!(!scripted(&worktree));
        let state = settled(
            &app,
            decide(
                &app,
                &agent,
                "process.execute",
                json!({"spec": {
                "executableRef": "sh", "args": ["-c", "pwd > .execute-ran"],
                "workingDirectory": {"rootId": "install-repository", "relative": "work"},
                "environment": [], "profile": "Full", "timeoutMs": "10000",
                "outputLimitBytes": "4096"}}),
                "execute-agent-worktree",
            ),
        )
        .await;
        assert_eq!(state, OperationState::ObservedComplete);
        assert!(worktree.join("work/.execute-ran").exists());
        assert!(!primary.join("work/.execute-ran").exists());
        app.shutdown().await;
    });
}

/// Like uv, syncs the project environment (`UV_PROJECT_ENVIRONMENT`, else `<project>/.venv`)
/// and runs its interpreter to inspect it, and runs the interpreter `--python` names or, without
/// it, the one `.python-version` names; it records the environment it synced.
const FAKE_UV: &str = "#!/bin/sh\n\
project=\n\
python=\n\
while [ $# -gt 0 ]; do\n\
  case \"$1\" in\n\
    --project) project=$2; shift ;;\n\
    --python) python=$2; shift ;;\n\
  esac\n\
  shift\n\
done\n\
environment=${UV_PROJECT_ENVIRONMENT:-$project/.venv}\n\
[ -n \"$python\" ] || python=$(cat \"$project/.python-version\" 2>/dev/null)\n\
if [ -x \"$environment/bin/python\" ]; then \"$environment/bin/python\" -c pass; fi\n\
if [ -n \"$python\" ]; then \"$python\" -c pass; fi\n\
printf %s \"$environment\" > \"$project/.uv-environment\"\n";

/// The host's `python3`, found on PATH as `tools.processExecutables` would find it.
fn host_python3() -> std::path::PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("python3"))
        .find(|candidate| candidate.is_file())
        .expect("python3 on PATH")
}

/// A fresh, canonical directory beside the test store holding `checkout`, `tools` and
/// `environments`.
fn python_work(fx: &Fx, label: &str) -> std::path::PathBuf {
    let work = fx.h.root.with_extension(label);
    let _ = std::fs::remove_dir_all(&work);
    for directory in ["checkout", "tools", "environments"] {
        std::fs::create_dir_all(work.join(directory)).unwrap();
    }
    work.canonicalize().unwrap()
}

/// Runs one script-free install of the manifest `name` (contents `bytes`) in `checkout`, with
/// `executables` registered and script-free uv environments kept under `environments`.
fn script_free_install(
    fx: &Fx,
    checkout: &Path,
    name: &str,
    bytes: &[u8],
    executables: Vec<RegisteredExecutable>,
    environments: &Path,
) -> OperationState {
    std::fs::write(checkout.join(name), bytes).unwrap();
    let rt = runtime();
    rt.block_on(async {
        let app = Arc::new(
            boot(composition_with_tools(fx, executables, Some(environments)))
                .await
                .unwrap(),
        );
        let (agent, _) = agent_context(fx);
        let manifest = register_manifest(fx, checkout, name, bytes, "text/plain");
        let state = settled(
            &app,
            decide(
                &app,
                &agent,
                "process.install_project_dependencies",
                install(None, &manifest),
                "install-python",
            ),
        )
        .await;
        app.shutdown().await;
        state
    })
}

/// DW-343: a script-free pip install imports the interpreter's own pip, not a `pip` module or
/// package the repository (its working directory) supplies.
#[test]
fn a_script_free_pip_install_runs_no_pip_module_from_the_repository() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    let work = python_work(&fx, "pip-isolation-work");
    let checkout = work.join("checkout");
    std::fs::write(
        checkout.join("pip.py"),
        "open('pip-module-ran', 'w').close()\n",
    )
    .unwrap();
    std::fs::create_dir_all(checkout.join("pip")).unwrap();
    std::fs::write(checkout.join("pip/__init__.py"), "").unwrap();
    std::fs::write(
        checkout.join("pip/__main__.py"),
        "open('pip-package-ran', 'w').close()\n",
    )
    .unwrap();
    let state = script_free_install(
        &fx,
        &checkout,
        "requirements.txt",
        b"# no requirements\n",
        vec![RegisteredExecutable {
            id: id("python3"),
            path: host_python3(),
        }],
        &work.join("environments"),
    );
    for marker in ["pip-module-ran", "pip-package-ran"] {
        assert!(
            !checkout.join(marker).exists(),
            "a script-free pip install ran the repository's {marker}"
        );
    }
    assert_eq!(state, OperationState::ObservedComplete);
}

/// DW-344: a script-free uv install runs the host's interpreter and syncs an environment the
/// host owns, so neither the repository's `.venv` interpreter nor one its `.python-version`
/// names runs.
#[test]
fn a_script_free_uv_install_runs_no_interpreter_from_the_repository() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    let work = python_work(&fx, "uv-isolation-work");
    let (checkout, tools) = (work.join("checkout"), work.join("tools"));
    let planted = checkout.join(".venv/bin/python");
    std::fs::create_dir_all(planted.parent().unwrap()).unwrap();
    std::fs::write(
        &planted,
        "#!/bin/sh\ntouch \"$(dirname \"$0\")/../../venv-python-ran\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        checkout.join(".python-version"),
        planted.to_string_lossy().as_bytes(),
    )
    .unwrap();
    let uv = tools.join("uv");
    std::fs::write(&uv, FAKE_UV).unwrap();
    std::fs::set_permissions(&uv, std::fs::Permissions::from_mode(0o755)).unwrap();
    let environments = work.join("environments");
    let state = script_free_install(
        &fx,
        &checkout,
        "pyproject.toml",
        b"[project]\nname = \"fixture\"\nversion = \"0.1.0\"\ndependencies = []\n",
        vec![
            RegisteredExecutable {
                id: id("uv"),
                path: uv,
            },
            RegisteredExecutable {
                id: id("python3"),
                path: host_python3(),
            },
        ],
        &environments,
    );
    assert!(
        !checkout.join("venv-python-ran").exists(),
        "a script-free uv install ran the repository's interpreter"
    );
    assert_eq!(state, OperationState::ObservedComplete);
    let synced = std::fs::read_to_string(checkout.join(".uv-environment")).unwrap();
    assert!(
        Path::new(&synced).starts_with(&environments) && !Path::new(&synced).starts_with(&checkout),
        "{synced}"
    );
}
