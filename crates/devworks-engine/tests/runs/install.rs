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
        Some(&tools.with_file_name("environments")),
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
    if matches!(
        op,
        "process.execute"
            | "git.worktree"
            | "containers.build"
            | "containers.start"
            | "containers.exec"
    ) {
        // These operations negotiate only their first schema revision.
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
fn planning_feature_agents_need_current_readiness_for_git_commit_and_process_execution() {
    use devworks_core::ports::storage::{StoredFeaturePlan, StoredReadiness};
    use devworks_core::protocol::{RetryAdvice, records::PlanDraft};
    use devworks_core::services::git::{GitConfig, RegisteredGitRoot};
    use devworks_core::services::work::{
        model::{ActivePlan, plan_digest, plan_units},
        readiness::Assessment,
    };
    use devworks_engine::dispatch::RootEligibility;

    let _native_case = native_runtime_case();
    let fx = Fx::new();
    let source = fx.h.root.join("readiness-source");
    let owned = fx.h.root.join("readiness-worktrees");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&owned).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(&source)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {:?}", output.stderr);
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.name", "Fixture"]);
    git(&["config", "user.email", "fixture@example.invalid"]);
    git(&["commit", "--allow-empty", "-m", "Initial"]);
    let head = GitOid {
        format: ObjectFormat::Sha1,
        hex: git(&["rev-parse", "HEAD"]),
    };
    let content = register_manifest(&fx, &source, "Plan", b"Fixture plan", "text/plain");
    let draft: PlanDraft = serde_json::from_value(json!({
        "content": content, "repositoryIds": ["install-repository"],
        "acceptance": [], "acceptanceTests": [], "goals": [], "requirements": [],
    }))
    .unwrap();
    let digest = plan_digest(&draft);
    let approved = serde_json::to_value(ActivePlan {
        draft: draft.clone(),
        digest: digest.clone(),
        plan_revision: 1,
        proposal_id: id("approved-fixture"),
        review_required: false,
    })
    .unwrap();
    fx.tx(move |_, tx| {
        tx.work()
            .set_work_repositories(&id("root-f"), &[id("install-repository")])?;
        tx.work().put_feature_plan(
            &StoredFeaturePlan {
                feature_id: id("root-f"),
                revision: 1,
                approved: Some(approved),
                approved_by: Some(ActorId(id("user-a"))),
                approved_at_ms: Some(T0),
                pending: None,
                pending_revision: None,
            },
            None,
        )
    });

    let mut composition = composition_with_tools(
        &fx,
        vec![RegisteredExecutable {
            id: id("sh"),
            path: "/bin/sh".into(),
        }],
        None,
    );
    composition.git = Some(devworks_engine::git::GitComposition {
        config: GitConfig {
            roots: vec![RegisteredGitRoot {
                id: id("readiness-root"),
                path: owned.canonicalize().unwrap(),
            }],
            filesystem: Some(Arc::new(NativeFilesystem::new())),
            ..GitConfig::default()
        },
        github_http: None,
    });
    let private_files = Arc::new(
        devworks_adapters::containers::NativeRegistryAuth::new(fx.h.root.join("container-auth"))
            .unwrap(),
    );
    composition.config.containers = Some(devworks_engine::runtime::ContainerComposition {
        bindings: ToolBindings::new([], []).unwrap(),
        filesystem: Arc::new(NativeFilesystem::new()),
        protected_roots: ProtectedRoots::new(Vec::new(), Vec::new(), None),
        secrets: Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
        registry_auth: private_files.clone(),
        workload_environment: private_files,
        host: devworks_core::services::containers::probe::HostPlatform {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            macos_major: 26,
        },
    });
    runtime().block_on(async {
        let app = boot(composition).await.unwrap();
        applied(&fx.start(&fx.user, "root-f", 1, 1));
        assert_eq!(fx.live("root-f").unwrap().purpose, ActivationPurpose::Planning);
        let (agent, _) = super::watch_tests::bind(&fx, "readiness-fixture");
        let create = decide(&app, &agent, "git.worktree", json!({
            "repositoryId": "install-repository", "workId": "root-f", "expectedRevision": "1",
            "action": "Create", "base": head, "registeredRootId": "readiness-root",
        }), "readiness-create-tree");
        assert_eq!(settled(&app, create).await, OperationState::ObservedComplete);
        let tree = read(&*fx.store, |s| s.git().git_worktrees(&id("root-f"), None, 10)).remove(0);
        let change = Path::new(&tree.path).join("change.txt");
        std::fs::write(&change, b"changed").unwrap();
        std::fs::create_dir(Path::new(&tree.path).join("work")).unwrap();
        let message = app.artifacts().publish_evidence(
            &*fx.store,
            devworks_core::services::artifacts::evidence::Evidence {
                scope: ArtifactScope::Project(ProjectId(id("project-a"))),
                producer: agent.principal().clone(),
                principal_kind: "Agent".into(),
                agent_session: agent.session_id().cloned(),
                root: agent.root_id().cloned(),
                assignment: agent.assignment_id().cloned(),
                title: "Commit message".into(),
                media_type: "text/plain".into(),
                tags: vec![],
                source: json!({"kind":"TestMessage"}),
            },
            b"Fixture change",
            |_, _| Ok(()),
        ).unwrap().0;
        let commit = |key: &str| {
            let frame = json!({"protocol": PROTOCOL, "requestKey": key, "command": {
                "kind": "Commit", "worktreeId": tree.id, "expectedHead": tree.head, "message": message,
            }});
            match app.command(&agent, "git.commit", frame.to_string().as_bytes()).unwrap() {
                CommandReply::Recorded { receipt, .. } => receipt.decision,
                other => panic!("{other:?}"),
            }
        };
        let spec = json!({
            "executableRef": "sh", "args": ["-c", "exit 0"],
            "workingDirectory": {"rootId": "install-repository", "relative": "work"},
            "environment": [], "profile": "Full", "timeoutMs": "10000", "outputLimitBytes": "4096",
        });
        let process = |key: &str| decide(&app, &agent, "process.execute", json!({"spec": spec}), key);
        let container_build = |key: &str| decide(&app, &agent, "containers.build", json!({
            "engineId": "docker", "projectId": "project-a",
            "context": {"rootId": "install-repository", "relative": "work"},
            "definition": message, "tag": "readiness-test:latest",
        }), key);
        let container_start = |key: &str| decide(&app, &agent, "containers.start", json!({
            "containerId": "missing-container", "expectedRevision": "1",
        }), key);
        let container_exec = |key: &str| decide(&app, &agent, "containers.exec", json!({
            "containerId": "missing-container", "expectedRevision": "1",
            "command": ["true"], "environment": [], "timeoutMs": "10000",
        }), key);
        let missing = [
            commit("missing-commit"),
            process("missing-process"),
            container_build("missing-ready-container-build"),
            container_start("missing-ready-container-start"),
            container_exec("missing-ready-container-exec"),
            decide(&app, &agent, "watch.create", json!({"process": spec}), "missing-watch"),
            decide(&app, &agent, "watch.create", json!({"probe": {
                "process": spec, "intervalMs": "1000", "condition": {},
            }}), "missing-probe"),
        ];
        for decision in &missing {
            assert!(matches!(decision, DomainDecision::Refused { fault }
                if fault.target_kind.as_ref().is_some_and(|kind| kind.0 == "readiness")
                    && fault.code == DomainErrorCode::DependencyBlocked
                    && !fault.blocker_ids.0.is_empty()
                    && fault.retry == RetryAdvice::AfterBlockerClears), "{missing:?}");
        }

        let assessment = serde_json::to_value(Assessment {
            id: id("readiness-fixture"),
            plan_revision: 1,
            plan_digest: digest,
            reviewed_pending: false,
            units: plan_units(&draft),
            summary: "Ready".into(),
            findings: vec![],
            assessed_by: ActorId(id("user-a")),
            assessed_at_ms: T0,
        }).unwrap();
        fx.tx(move |_, tx| tx.work().put_readiness(&StoredReadiness {
            feature_id: id("root-f"), revision: 1, body: json!({"active": assessment}),
        }, None));
        assert_eq!(settled(&app, commit("ready-commit")).await, OperationState::ObservedComplete);
        let accepted = process("ready-process");
        assert_eq!(settled(&app, accepted.clone()).await, OperationState::ObservedComplete);

        // Once ready, the requests reach the real container service, which rejects the
        // nonexistent fixture ID before starting any local container or contacting a runtime.
        for result in [
            container_start("ready-container-start"),
            container_exec("ready-container-exec"),
        ] {
            assert!(matches!(&result, DomainDecision::Refused { fault }
                if fault.code == DomainErrorCode::NotFound), "{result:?}");
        }

        // A queued launch can encounter a newer assessment before the dispatcher begins it.
        // Exercise the real queue gate while the write owner controls that ordering.
        let gate = RunGate::new(fx.service.clone(), None);
        let assignment = agent.assignment_id().cloned();
        fx.tx(move |_, tx| {
            let effects = [
                ("process.execute", devworks_core::services::process::tools::EFFECT_CONSUMER),
                ("containers.build", devworks_core::services::containers::EFFECT_CONSUMER),
                ("containers.start", devworks_core::services::containers::EFFECT_CONSUMER),
                ("containers.exec", devworks_core::services::containers::EFFECT_CONSUMER),
            ];
            let mut entries = Vec::new();
            for (index, (capability, consumer)) in effects.into_iter().enumerate() {
                let operation = OperationId(id(&format!("queued-readiness-effect-{index}")));
                tx.operations().record_intent(NewOperation {
                    id: operation.clone(),
                    root_id: Some(rid("root-f")),
                    assignment_id: assignment.clone(),
                    capability: capability.into(),
                    schema_version: 1,
                    args_digest: "a".repeat(64),
                    payload: json!({"kind": "Agent"}),
                })?;
                let event_id = event(tx, "Changed")?;
                let entry_id = id(&format!("queued-readiness-entry-{index}"));
                tx.queue().enqueue(NewQueueEntry {
                    id: entry_id.clone(),
                    consumer: consumer.into(),
                    event_id,
                    root_id: Some(rid("root-f")),
                    operation_id: Some(operation),
                    partition: None,
                    priority: 0,
                    ready_at_ms: T0 + 60_000,
                    payload: json!({}),
                })?;
                let entry = tx.queue().entry(&entry_id)?.unwrap();
                assert!(gate.may_dispatch(tx, &entry)?);
                entries.push(entry);
            }
            let mut stale = tx.work().readiness(&id("root-f"))?.unwrap();
            stale.revision += 1;
            stale.body["active"]["units"] = json!({});
            tx.work().put_readiness(&stale, Some(1))?;
            for entry in entries {
                assert!(!gate.may_dispatch(tx, &entry)?);
                assert!(!gate.may_resume(tx, &entry)?);
            }
            Ok(())
        });
        let replay = process("ready-process");
        assert!(
            matches!((&replay, &accepted),
                (DomainDecision::Accepted { operation_id: replayed },
                 DomainDecision::Accepted { operation_id: original }) if replayed == original),
            "readiness must not hide an accepted receipt: {replay:?}",
        );
        app.shutdown().await;
    });
}

#[test]
fn a_dependency_install_runs_no_lifecycle_script_unless_approved_and_shares_the_working_root_rule()
{
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    let work = fx.h.root.with_extension("install-scripts-work");
    let _ = std::fs::remove_dir_all(&work);
    for directory in ["tools", "primary", "worktree", "environments"] {
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
        // Git reads no global or system configuration.
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
    script_free_decision(fx, checkout, name, bytes, executables, environments)
        .unwrap_or_else(|decision| panic!("expected an accepted install, got {decision:?}"))
}

/// Like [`script_free_install`], but an install that is not accepted returns its decision.
fn script_free_decision(
    fx: &Fx,
    checkout: &Path,
    name: &str,
    bytes: &[u8],
    executables: Vec<RegisteredExecutable>,
    environments: &Path,
) -> Result<OperationState, Box<DomainDecision>> {
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
        let decision = decide(
            &app,
            &agent,
            "process.install_project_dependencies",
            install(None, &manifest),
            "install-python",
        );
        let outcome = match decision {
            DomainDecision::Accepted { .. } => Ok(settled(&app, decision).await),
            refused => Err(Box::new(refused)),
        };
        app.shutdown().await;
        outcome
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

/// Writes `text` to `path` as an executable shell script.
fn write_script(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Like `go mod download`: reads the Go `env` file (`$GOENV`, else
/// `$XDG_CONFIG_HOME/go/env`, else `$HOME/.config/go/env`) unless `GOENV=off` and records the
/// proxy it chose; and unless `GOTOOLCHAIN=local`, runs the toolchain the `go.mod` `toolchain`
/// line names from the module cache, where Go keeps a toolchain it downloaded. It also records
/// where its home, temporary folder and caches are.
const FAKE_GO: &str = "#!/bin/sh\n\
config=${GOENV:-${XDG_CONFIG_HOME:-$HOME/.config}/go/env}\n\
proxy=https://proxy.golang.org\n\
if [ \"$config\" != off ] && [ -f \"$config\" ]; then\n\
  proxy=$(sed -n 's/^GOPROXY=//p' \"$config\")\n\
fi\n\
printf %s \"$proxy\" > .go-proxy\n\
for value in \"$HOME\" \"$TMPDIR\" \"$XDG_CONFIG_HOME\" \"$XDG_CACHE_HOME\" \"$XDG_DATA_HOME\" \
\"$XDG_STATE_HOME\" \"$CARGO_HOME\" \"$npm_config_cache\" \"$YARN_CACHE_FOLDER\" \
\"$COREPACK_HOME\" \"$PIP_CACHE_DIR\" \"$UV_CACHE_DIR\" \"$GOPATH\" \"$GOMODCACHE\" \
\"$COMPOSER_HOME\" \"$COMPOSER_CACHE_DIR\"; do\n\
  printf '%s\\n' \"$value\"\n\
done > .install-homes\n\
toolchain=$(sed -n 's/^toolchain //p' go.mod)\n\
if [ -n \"$toolchain\" ] && [ \"${GOTOOLCHAIN:-auto}\" != local ]; then\n\
  release=\"${GOMODCACHE:-$HOME/go/pkg/mod}/golang.org/toolchain@v0.0.1-$toolchain/bin/go\"\n\
  if [ -x \"$release\" ]; then \"$release\" \"$@\"; fi\n\
fi\n";

/// Like a Corepack `yarn` shim: reads `.corepack.env` unless `COREPACK_ENV_FILE=0`, and runs
/// the release its `COREPACK_NPM_REGISTRY` (a `file://` folder) holds; then runs the release
/// `packageManager` names from the Corepack home (`$COREPACK_HOME`, else
/// `$XDG_CACHE_HOME/node/corepack`, else `$HOME/.cache/node/corepack`) when it is there.
const FAKE_COREPACK_YARN: &str = "#!/bin/sh\n\
if [ \"$COREPACK_ENV_FILE\" != 0 ] && [ -f .corepack.env ]; then\n\
  registry=$(sed -n 's|^COREPACK_NPM_REGISTRY=file://||p' .corepack.env)\n\
  if [ -n \"$registry\" ] && [ -f \"$registry/yarn.js\" ]; then sh \"$registry/yarn.js\"; fi\n\
fi\n\
version=$(sed -n 's/.*\"packageManager\": *\"yarn@\\([^\"]*\\)\".*/\\1/p' package.json)\n\
corepack=${COREPACK_HOME:-${XDG_CACHE_HOME:-$HOME/.cache}/node/corepack}\n\
release=\"$corepack/v1/yarn/$version/yarn.js\"\n\
if [ -n \"$version\" ] && [ -f \"$release\" ]; then sh \"$release\"; fi\n\
pwd > .install-ran\n";

/// Like npm: runs the `git` an `.npmrc` names (unless `npm_config_git` overrides it) to fetch
/// a Git dependency, and to prepare that dependency runs a nested install whose scripts run
/// with the `.npmrc` `script-shell` and `node-options` (unless `npm_config_script_shell`
/// overrides the shell), which `npm_config_ignore_scripts=true` stops.
const FAKE_NPM_GIT_DEPENDENCY: &str = "#!/bin/sh\n\
setting() { sed -n \"s/^$1=//p\" .npmrc; }\n\
git=${npm_config_git:-$(setting git)}\n\
\"${git:-git}\" --version > /dev/null 2>&1\n\
if [ \"$npm_config_ignore_scripts\" != true ]; then\n\
  shell=${npm_config_script_shell:-$(setting script-shell)}\n\
  NODE_OPTIONS=$(setting node-options) \"${shell:-sh}\" -c true\n\
fi\n\
pwd > .install-ran\n";

/// Like pnpm: unless `--ignore-pnpmfile`, loads `.pnpmfile.cjs`; and unless version switching
/// is off, runs the pnpm `packageManager` names, fetched from the `.npmrc` `registry` (a
/// `file://` folder).
const FAKE_PNPM: &str = "#!/bin/sh\n\
pnpmfile=yes\n\
switch=yes\n\
for argument in \"$@\"; do\n\
  case \"$argument\" in\n\
    --ignore-pnpmfile) pnpmfile=no ;;\n\
    --config.manage-package-manager-versions=false|--config.pm-on-fail=ignore) switch=no ;;\n\
  esac\n\
done\n\
if [ $pnpmfile = yes ] && [ -f .pnpmfile.cjs ]; then sh .pnpmfile.cjs; fi\n\
version=$(sed -n 's/.*\"packageManager\": *\"pnpm@\\([^\"]*\\)\".*/\\1/p' package.json)\n\
registry=$(sed -n 's|^registry=file://||p' .npmrc 2>/dev/null)\n\
if [ $switch = yes ] && [ -n \"$version\" ] && [ -f \"$registry/pnpm-$version.js\" ]; then\n\
  sh \"$registry/pnpm-$version.js\"\n\
fi\n\
pwd > .install-ran\n";

/// Runs real Git in the checkout the way a manager does (`git ls-remote` over SSH, the Git
/// protocol and HTTPS, a credential lookup, `git status`), so the checkout's `.git/config`
/// applies.
const FAKE_NPM_GIT: &str = "#!/bin/sh\n\
git ls-remote ssh://127.0.0.1:1/x > /dev/null 2>&1\n\
git ls-remote git://127.0.0.1:1/x > /dev/null 2>&1\n\
git ls-remote https://example.invalid/x > /dev/null 2>&1\n\
printf 'protocol=https\\nhost=example.invalid\\n\\n' | git credential fill > /dev/null 2>&1\n\
git status > /dev/null 2>&1\n\
pwd > .install-ran\n";

/// Each planted program touches this marker in its working directory, the checkout.
const PLANTED: &str = "#!/bin/sh\ntouch program-ran\nexit 1\n";

/// DW-318: an install's home, temporary folder and caches are the host's, outside the
/// repository, so a Go `env` file, a Go toolchain in the module cache and a Corepack release
/// that the repository plants where the old in-repository home and caches were are ignored.
#[test]
fn a_script_free_install_ignores_a_home_and_caches_the_repository_plants() {
    let _native_case = native_runtime_case();
    // Go: a planted `env` file selects a proxy and turns the checksum database off, and a
    // planted toolchain sits in the module cache for the `go.mod` `toolchain` line.
    let fx = Fx::new();
    let work = python_work(&fx, "go-home-work");
    let (checkout, environments) = (work.join("checkout"), work.join("environments"));
    let config = checkout.join(".devworks-home/.config/go/env");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "GOPROXY=https://attacker.invalid\nGOSUMDB=off\n").unwrap();
    for cache in [".devworks-deps/go", ".devworks-home/go/pkg/mod"] {
        write_script(
            &checkout
                .join(cache)
                .join("golang.org/toolchain@v0.0.1-go1.99.0/bin/go"),
            PLANTED,
        );
    }
    let go = work.join("tools/go");
    write_script(&go, FAKE_GO);
    let state = script_free_install(
        &fx,
        &checkout,
        "go.mod",
        b"module example.invalid/fixture\n\ngo 1.22\n\ntoolchain go1.99.0\n",
        vec![RegisteredExecutable {
            id: id("go"),
            path: go,
        }],
        &environments,
    );
    assert_eq!(state, OperationState::ObservedComplete);
    let mut failures = Vec::new();
    if checkout.join("program-ran").exists() {
        failures.push("a script-free Go install ran a toolchain the repository planted".into());
    }
    let proxy = std::fs::read_to_string(checkout.join(".go-proxy")).unwrap();
    if proxy != "https://proxy.golang.org" {
        failures.push(format!(
            "a script-free Go install read a planted env file: {proxy}"
        ));
    }
    let homes = std::fs::read_to_string(checkout.join(".install-homes")).unwrap();
    assert_eq!(homes.lines().count(), 16, "{homes}");
    for home in homes.lines() {
        if !Path::new(home).starts_with(&environments) || Path::new(home).starts_with(&checkout) {
            failures.push(format!(
                "an install home or cache is not the host's: {home:?}"
            ));
        }
    }

    // Corepack: a planted Yarn release in the old home's Corepack cache.
    let fx = Fx::new();
    let work = python_work(&fx, "corepack-home-work");
    let checkout = work.join("checkout");
    std::fs::write(
        checkout.join("package.json"),
        r#"{"name":"fixture","packageManager":"yarn@1.22.22"}"#,
    )
    .unwrap();
    write_script(
        &checkout.join(".devworks-home/.cache/node/corepack/v1/yarn/1.22.22/yarn.js"),
        PLANTED,
    );
    let yarn = work.join("tools/yarn");
    write_script(&yarn, FAKE_COREPACK_YARN);
    let state = script_free_install(
        &fx,
        &checkout,
        "yarn.lock",
        b"# yarn lockfile v1\n",
        vec![RegisteredExecutable {
            id: id("yarn"),
            path: yarn,
        }],
        &work.join("environments"),
    );
    assert_eq!(state, OperationState::ObservedComplete);
    assert!(checkout.join(".install-ran").exists());
    if checkout.join("program-ran").exists() {
        failures.push("a script-free Yarn install ran a planted Corepack release".into());
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// DW-318: repository configuration that names a program does not run it during a
/// script-free install; the install overrides the setting and still completes. Settings the
/// install refuses instead are covered by the core tests of `install_runs_repository_code`.
#[test]
fn a_script_free_install_runs_no_program_repository_configuration_names() {
    let _native_case = native_runtime_case();
    let switching = br#"{"name":"fixture","packageManager":"pnpm@9.0.0"}"#.as_slice();
    // (case, manager, fake manager, manifest, manifest bytes, planted files)
    type Case<'a> = (
        &'a str,
        &'a str,
        &'a str,
        &'a str,
        &'a [u8],
        Vec<(&'a str, &'a [u8])>,
    );
    let mut cases: Vec<Case> = vec![
        (
            "npm .npmrc git",
            "npm",
            FAKE_NPM_GIT_DEPENDENCY,
            "package.json",
            br#"{"name":"fixture"}"#,
            vec![(".npmrc", b"git=./planted-program\n")],
        ),
        (
            "npm .npmrc script-shell and node-options",
            "npm",
            FAKE_NPM_GIT_DEPENDENCY,
            "package.json",
            br#"{"name":"fixture"}"#,
            vec![(
                ".npmrc",
                b"script-shell=./planted-program\nnode-options=--require ./planted.js\n",
            )],
        ),
        (
            "pnpm .pnpmfile.cjs",
            "pnpm",
            FAKE_PNPM,
            "pnpm-lock.yaml",
            b"lockfileVersion: '9.0'\n",
            vec![
                ("package.json", br#"{"name":"fixture"}"#),
                (".pnpmfile.cjs", PLANTED.as_bytes()),
            ],
        ),
        (
            "pnpm packageManager switch from the .npmrc registry",
            "pnpm",
            FAKE_PNPM,
            "pnpm-lock.yaml",
            b"lockfileVersion: '9.0'\n",
            vec![
                ("package.json", switching),
                (".npmrc", b"registry=file://planted-registry\n"),
                ("planted-registry/pnpm-9.0.0.js", PLANTED.as_bytes()),
            ],
        ),
        (
            "Corepack .corepack.env registry",
            "yarn",
            FAKE_COREPACK_YARN,
            "yarn.lock",
            b"# yarn lockfile v1\n",
            vec![
                ("package.json", br#"{"name":"fixture"}"#),
                (
                    ".corepack.env",
                    b"COREPACK_NPM_REGISTRY=file://planted-registry\nCOREPACK_INTEGRITY_KEYS=0\n",
                ),
                ("planted-registry/yarn.js", PLANTED.as_bytes()),
            ],
        ),
    ];
    // The checkout's own Git configuration, one setting per case.
    for (case, config) in [
        (
            "Git core.sshCommand",
            "[core]\n\tsshCommand = ./planted-program\n",
        ),
        (
            "Git core.gitProxy",
            "[core]\n\tgitProxy = ./planted-program\n",
        ),
        (
            "Git core.askPass",
            "[core]\n\taskPass = ./planted-program\n",
        ),
        (
            "Git core.fsmonitor",
            "[core]\n\tfsmonitor = ./planted-program\n",
        ),
        (
            "Git credential.helper",
            "[credential]\n\thelper = !./planted-program\n",
        ),
        (
            "Git URL-scoped credential.helper",
            "[credential \"https://example.invalid\"]\n\thelper = !./planted-program\n",
        ),
        (
            "Git url.insteadOf to the ext transport",
            "[protocol \"ext\"]\n\tallow = always\n\
             [url \"ext::./planted-program %S \"]\n\tinsteadOf = https://example.invalid/\n",
        ),
    ] {
        cases.push((
            case,
            "npm",
            FAKE_NPM_GIT,
            "package.json",
            br#"{"name":"fixture"}"#,
            vec![(".git/config", config.as_bytes())],
        ));
    }
    let mut ran = Vec::new();
    for (case, manager, fake, manifest, bytes, planted) in cases {
        let fx = Fx::new();
        let work = python_work(&fx, "program-config-work");
        let checkout = work.join("checkout");
        if planted.iter().any(|(path, _)| path.starts_with(".git/")) {
            let status = std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&checkout)
                .status()
                .unwrap();
            assert!(status.success(), "{case}: git init");
        }
        write_script(&checkout.join("planted-program"), PLANTED);
        for (path, text) in planted {
            let target = checkout.join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            let mut contents = if path == ".git/config" {
                std::fs::read(&target).unwrap()
            } else {
                Vec::new()
            };
            contents.extend_from_slice(text);
            std::fs::write(&target, contents).unwrap();
        }
        let program = work.join("tools").join(manager);
        write_script(&program, fake);
        let state = script_free_install(
            &fx,
            &checkout,
            manifest,
            bytes,
            vec![RegisteredExecutable {
                id: id(manager),
                path: program,
            }],
            &work.join("environments"),
        );
        assert_eq!(state, OperationState::ObservedComplete, "{case}");
        assert!(checkout.join(".install-ran").exists(), "{case}: no install");
        if checkout.join("program-ran").exists() {
            ran.push(case);
        }
    }
    assert!(
        ran.is_empty(),
        "a script-free install ran the program repository configuration names: {ran:#?}"
    );
}

/// Like `cargo fetch`: queries the compiler (`build.rustc`, run through `build.rustc-wrapper`
/// when one is set) that `.cargo/config.toml` names, and runs a registry's
/// `credential-provider`; like the compiler it queries, it expands an `@file` in `rustflags`
/// (printing the file, as rustc's diagnostic for an invalid `--cfg` does, and marking that a
/// file was read); it records that it ran.
const FAKE_CARGO: &str = "#!/bin/sh\n\
setting() { sed -n \"s/^$1 *= *\\\"\\(.*\\)\\\"/\\1/p\" .cargo/config.toml 2>/dev/null; }\n\
for program in \"$(setting rustc)\" \"$(setting rustc-wrapper)\" \"$(setting credential-provider)\"; do\n\
  if [ -n \"$program\" ]; then \"$program\" -vV > /dev/null 2>&1; fi\n\
done\n\
for file in $(sed -n 's/.*\"@\\([^\"]*\\)\".*/\\1/p' .cargo/config.toml 2>/dev/null); do\n\
  cat \"$file\" && touch program-ran\n\
done\n\
pwd > .install-ran\n";

/// DW-319: an agent's script-free Cargo install runs when the repository's Cargo
/// configuration only names registries or other inert settings, and is refused, before
/// anything runs, when it names a program.
#[test]
fn a_script_free_cargo_install_accepts_inert_configuration_and_refuses_a_named_program() {
    let _native_case = native_runtime_case();
    let manifest = b"[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
    // (case, `.cargo/config.toml`, accepted)
    let cases = [
        (
            "registries only",
            "[registries.internal]\nindex = \"sparse+https://cargo.example.invalid/index/\"\n",
            true,
        ),
        (
            "vendored source replacement and an alias",
            "[source.crates-io]\nreplace-with = \"vendored\"\n[source.vendored]\n\
             directory = \"vendor\"\n[alias]\nxtask = \"run --package xtask --\"\n",
            true,
        ),
        (
            "tokio_unstable rustflags",
            "[build]\nrustflags = [\"--cfg\", \"tokio_unstable\"]\n",
            true,
        ),
        (
            "build.rustc",
            "[build]\nrustc = \"./planted-program\"\n",
            false,
        ),
        (
            "build.rustc-wrapper",
            "[build]\nrustc-wrapper = \"./planted-program\"\n",
            false,
        ),
        (
            "registries credential-provider",
            "[registries.internal]\nindex = \"https://example.invalid/i\"\n\
             credential-provider = \"./planted-program\"\n",
            false,
        ),
        (
            // rustc would expand the file into flags (or print it in a diagnostic).
            "rustflags --cfg @file",
            "[build]\nrustflags = [\"--cfg\", \"@planted-args\"]\n",
            false,
        ),
        (
            "target linker",
            "[target.x86_64-unknown-linux-gnu]\nlinker = \"./planted-program\"\n",
            false,
        ),
    ];
    let mut wrong = Vec::new();
    for (case, config, accepted) in cases {
        let fx = Fx::new();
        let work = python_work(&fx, "cargo-config-work");
        let checkout = work.join("checkout");
        write_script(&checkout.join("planted-program"), PLANTED);
        std::fs::write(
            checkout.join("planted-args"),
            "-Zcodegen-backend=./planted.so\n",
        )
        .unwrap();
        std::fs::create_dir_all(checkout.join(".cargo")).unwrap();
        std::fs::write(checkout.join(".cargo/config.toml"), config).unwrap();
        let cargo = work.join("tools/cargo");
        write_script(&cargo, FAKE_CARGO);
        let outcome = script_free_decision(
            &fx,
            &checkout,
            "Cargo.toml",
            manifest,
            vec![RegisteredExecutable {
                id: id("cargo"),
                path: cargo,
            }],
            &work.join("environments"),
        );
        let right = match (&outcome, accepted) {
            (Ok(state), true) => {
                *state == OperationState::ObservedComplete && checkout.join(".install-ran").exists()
            }
            (Err(decision), false) => matches!(&**decision, DomainDecision::Refused { fault }
                if fault.code == DomainErrorCode::InvalidArgument),
            _ => false,
        };
        if !right || checkout.join("program-ran").exists() {
            wrong.push(format!("{case}: {outcome:?}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
