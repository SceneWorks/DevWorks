//! Index supervision in the booted runtime (DW-023): the `indexing.*` use cases are registered
//! by `boot`, `indexing.discover` answers from a fresh scan, a recorded run is executed on the
//! blocking pool and published, and shutdown stops a run within the runtime's one deadline
//! (killing its Git tree) while the run keeps its checkpoint for the next boot. Real SQLite, real
//! Git repositories in temporary directories, the Tree-sitter parser; no network, no inference.
mod common;
#[path = "indexing/graph_source.rs"]
mod graph_source;

use common::*;
use devworks_adapters::fs::NativeFilesystem;
use devworks_adapters::fs::artifacts::NativeArtifactFiles;
use devworks_adapters::fs::sources::HostArtifactSources;
use devworks_adapters::index::NativeSourceTree;
use devworks_adapters::index::treesitter::TreeSitterParser;
use devworks_adapters::process::NativeProcessHost;
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::ports::process::{PowerAssertion, PowerPort, PowerRequest};
use devworks_core::ports::storage::*;
use devworks_core::protocol::CommandReply;
use devworks_core::protocol::results::{DomainDecision, DomainQueryReply, OperationResult};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::indexing::IndexSettings;
use devworks_core::services::resources::ResourceService;
use devworks_core::settings::catalog::descriptor;
use devworks_engine::{
    Composition, IndexComposition, IndexingConfig, Runtime, RuntimeConfig, boot,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

struct NoPower;
struct NoHold;
impl PowerAssertion for NoHold {
    fn status(&mut self) -> PowerRequest {
        PowerRequest::Effective { mechanism: "test" }
    }
}
impl PowerPort for NoPower {
    fn prevent_idle_sleep(&self) -> Result<Box<dyn PowerAssertion>, String> {
        Ok(Box::new(NoHold))
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn sources() -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "devworks-engine-index-{}-{}",
        std::process::id(),
        id_counter()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    // The identity form discovery records (no Windows verbatim prefix).
    devworks_core::ports::indexing::SourceTree::canonical_dir(&NativeSourceTree::new(), &base)
        .unwrap()
}
fn id_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

fn composition(h: &Harness, store: Arc<dyn ApplicationStore>, git_program: &str) -> Composition {
    let artifacts = Arc::new(ArtifactService::new(
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.with_extension("cache"),
        )),
        Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
        Arc::new(NativeFilesystem::new()),
        h.clock(),
        h.ids.clone(),
    ));
    let mut config = RuntimeConfig::new(id("dispatcher-1"));
    config.collect_interval = Duration::from_secs(3600);
    config.shutdown_grace = Duration::from_millis(200);
    config.shutdown_bound = Duration::from_secs(10);
    let settings = IndexSettings {
        git_program: git_program.into(),
        ..IndexSettings::default()
    };
    Composition {
        git: None,
        browser: None,
        packages: None,
        store: store.clone(),
        clock: h.clock(),
        ids: h.ids.clone(),
        authority: Arc::new(Authority),
        artifacts,
        resources: Arc::new(ResourceService::new(h.ids.clone(), h.clock())),
        process_host: Arc::new(NativeProcessHost),
        runs: devworks_engine::RunComposition::new(Arc::new(
            devworks_core::services::runs::ControlService::new(h.ids.clone()),
        )),
        power: Arc::new(NoPower),
        diagnostics: Arc::new(
            devworks_core::services::diagnostics::DiagnosticService::new(
                store.clone(),
                h.ids.clone(),
                h.clock(),
                None,
            ),
        ),
        use_cases: UseCases::new(),
        consumers: Vec::new(),
        indexing: Some(IndexComposition {
            parser: Arc::new(TreeSitterParser::new()),
            source: Arc::new(NativeSourceTree::new()),
            fs: Arc::new(NativeFilesystem::new()),
            settings,
            config: IndexingConfig {
                max_runs: 2,
                tick: Duration::from_millis(50),
                housekeeping_interval: Duration::from_secs(3600),
                discovery_interval: Duration::from_secs(3600),
                discover_at_boot: false,
            },
            enrichers: Vec::new(),
        }),
        config,
    }
}

fn base_directory(store: &Arc<dyn ApplicationStore>, ctx: &InvocationContext, dir: &Path) {
    let d = descriptor("repositories.baseDirectory").unwrap();
    let assignment = serde_json::from_value(json!({
        "key": "repositories.baseDirectory",
        "value": {"schema": d.schema_ref(), "value": dir.display().to_string()},
    }))
    .unwrap();
    let ctx = ctx.clone();
    write(&**store, move |tx| {
        devworks_core::settings::service::apply_human_assignments(
            tx,
            &ctx,
            &ConfigScope::Workspace,
            &[assignment],
            json!({}),
        )
        .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    })
    .unwrap();
}

fn query(rt: &Runtime, ctx: &InvocationContext, op: &str, mut body: Value) -> OperationResult {
    body["kind"] = json!(op);
    let frame = json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-1", "schemaRevision": 2, "body": body});
    let frame = if op == "indexing.status" {
        frame
    } else {
        let mut f = frame;
        f.as_object_mut().unwrap().remove("schemaRevision");
        f
    };
    match rt.query(ctx, op, frame.to_string().as_bytes()).unwrap() {
        DomainQueryReply::Result { result, .. } => result,
        DomainQueryReply::Rejected { fault, .. } => panic!("{fault:?}"),
    }
}
fn index(rt: &Runtime, ctx: &InvocationContext, repo: &str, key: &str) -> String {
    let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": key, "body": {
        "kind": "indexing.index", "scope": {"kind": "Workspace"}, "repositoryIds": [repo],
        "expectedSourceRevision": "1", "views": {"kind": "Published", "repositoryIds": [repo]}}});
    match rt
        .command(ctx, "indexing.index", frame.to_string().as_bytes())
        .unwrap()
    {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied {
                result: OperationResult::IndexingIndex { value },
            } => value.id.as_str().to_owned(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}
fn discovered(rt: &Runtime, ctx: &InvocationContext) -> Vec<String> {
    match query(
        rt,
        ctx,
        "indexing.discover",
        json!({"scope": {"kind": "Workspace"}, "roots": [], "page": {"limit": 50}}),
    ) {
        OperationResult::IndexingDiscover { value } => value
            .items
            .0
            .iter()
            .map(|r| r.id.as_str().to_owned())
            .collect(),
        other => panic!("{other:?}"),
    }
}
fn run_state(store: &Arc<dyn ApplicationStore>, run: &str) -> StoredIndexRun {
    let r = id(run);
    read(&**store, move |s| s.indexing().run(&r)).expect("run")
}

#[test]
fn discovery_scans_only_after_admission_and_only_for_people() {
    let h = Harness::new();
    let rt = runtime();
    let src = sources();
    let repo = src.join("svc");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    let store = h.open();
    let runtime_ = rt
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let user = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &user, &src);
    // The stored identity of the checkout (the product's form for every lookup by location).
    let location = devworks_core::services::indexing::paths::location_identity(
        &NativeSourceTree::new(),
        &repo,
    );
    let registered = |store: &Arc<dyn ApplicationStore>| {
        let l = location.clone();
        read(&**store, move |s| s.indexing().repository_at(&l))
    };
    // A query the gateway refuses (an unknown member) has no effect, even for a person.
    let refused = json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-x", "body": {
        "kind": "indexing.discover", "scope": {"kind": "Workspace"}, "roots": [],
        "page": {"limit": 50}, "unexpected": true}});
    let reply = runtime_.query(&user, "indexing.discover", refused.to_string().as_bytes());
    assert!(
        !matches!(reply, Ok(DomainQueryReply::Result { .. })),
        "{reply:?}"
    );
    assert_eq!(registered(&store), None, "a refused query scanned");
    // An agent's admitted read answers from the recorded registrations without a host scan.
    let agent = principal(&*store, "agent", PrincipalKind::Agent);
    let frame = json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-a", "body": {
        "kind": "indexing.discover", "scope": {"kind": "Workspace"}, "roots": [],
        "page": {"limit": 50}}});
    let _ = runtime_.query(&agent, "indexing.discover", frame.to_string().as_bytes());
    assert_eq!(registered(&store), None, "an agent's read scanned");
    // A person's admitted query scans first.
    assert_eq!(discovered(&runtime_, &user).len(), 1);
    assert!(registered(&store).is_some());
    rt.block_on(runtime_.shutdown());
    let _ = std::fs::remove_dir_all(&src);
}

#[test]
fn the_runtime_discovers_indexes_and_publishes_through_its_supervisor() {
    let h = Harness::new();
    let rt = runtime();
    let store = h.open();
    let src = sources();
    let repo = src.join("svc");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn run() { helper(); }\nfn helper() {}\n",
    )
    .unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    let runtime_ = rt
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let ctx = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &ctx, &src);
    // The discover query scans first: the checkout is registered and answered.
    let found = discovered(&runtime_, &ctx);
    assert_eq!(found.len(), 1);
    let run = index(&runtime_, &ctx, &found[0], "key-1");
    rt.block_on(within("the run publishes", async {
        while run_state(&store, &run).state != RUN_SUCCEEDED {
            assert!(
                !run_terminal(&run_state(&store, &run).state),
                "{:?}",
                run_state(&store, &run)
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }));
    match query(
        &runtime_,
        &ctx,
        "indexing.status",
        json!({"repositoryId": found[0]}),
    ) {
        OperationResult::IndexingStatus { value } => {
            assert_eq!(value.current_view_ids.0.len(), 1);
            assert!(matches!(
                value.freshness.unwrap().state,
                devworks_core::protocol::records::IndexFreshnessState::Current
            ));
        }
        other => panic!("{other:?}"),
    }
    rt.block_on(runtime_.shutdown());
    let _ = std::fs::remove_dir_all(&src);
}

#[test]
fn a_native_reindex_job_uses_the_registered_service_and_wakes_index_supervision() {
    let h = Harness::new();
    let rt = runtime();
    let store = h.open();
    let src = sources();
    let repo = src.join("scheduled");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("lib.rs"), "pub fn scheduled() {}\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    let mut composed = composition(&h, store.clone(), "git");
    // The command wakeup drives publication; polling is only a failure fallback an hour away.
    composed.indexing.as_mut().unwrap().config.tick = Duration::from_secs(3600);
    let runtime_ = rt.block_on(boot(composed)).unwrap();
    let capability = runtime_
        .jobs()
        .catalog()
        .capabilities()
        .into_iter()
        .find(|c| c.operation == "indexing.reindex")
        .expect("indexing is registered before the maintenance catalog is qualified");
    let ctx = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &ctx, &src);
    let found = discovered(&runtime_, &ctx);
    assert_eq!(found.len(), 1);
    let command = |op: &str, key: &str, mut body: Value| {
        body["kind"] = json!(op);
        let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "schemaRevision": 2,
            "requestKey": key, "body": body});
        match runtime_
            .command(&ctx, op, frame.to_string().as_bytes())
            .unwrap()
        {
            CommandReply::Recorded { receipt, .. } => match receipt.decision {
                DomainDecision::Applied { result } => result,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    };
    let OperationResult::JobsCreate { value: job } = command(
        "jobs.create",
        "create-job",
        json!({
            "scope": {"kind": "Workspace"}, "title": "Reindex", "enabled": true,
            "schedule": {"kind": "Interval", "seconds": "60"},
            "definition": {"kind": "Native", "capabilityId": capability.capability_id,
                "arguments": {"schema": capability.schema, "value": {
                    "scope": {"kind": "Workspace"}, "repositoryIds": found,
                    "expectedSourceRevision": "1", "views": {"kind": "Published", "repositoryIds": found}
                }}}
        }),
    ) else {
        panic!("job creation result")
    };
    let OperationResult::JobsRunNow { value: run } = command(
        "jobs.run_now",
        "run-job",
        json!({"jobId": job.id, "expectedRevision": job.revision}),
    ) else {
        panic!("job run result")
    };
    rt.block_on(within(
        "the native job dispatches and indexing publishes",
        async {
            loop {
                let stored = read(&*store, |s| s.jobs().job_run(&run.id)).unwrap();
                assert!(
                    !matches!(stored.status.as_str(), "Failed" | "OutcomeUnknown"),
                    "{stored:?}"
                );
                let OperationResult::IndexingStatus { value } = query(
                    &runtime_,
                    &ctx,
                    "indexing.status",
                    json!({"repositoryId": found[0]}),
                ) else {
                    panic!("index status")
                };
                if stored.status == "Succeeded" && !value.current_view_ids.0.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        },
    ));
    rt.block_on(runtime_.shutdown());
    let _ = std::fs::remove_dir_all(&src);
}

/// Uses a POSIX shell fake Git; Windows process-tree supervision is covered by the process
/// tests.
#[cfg(unix)]
#[test]
fn shutdown_stops_a_run_within_its_deadline_and_the_next_boot_resumes_it() {
    let h = Harness::new();
    let rt = runtime();
    let src = sources();
    let repo = src.join("slow");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    // A Git that hangs on reading the tree (after the HEAD lookups succeed).
    let fake = src.join("hanging-git");
    std::fs::write(
        &fake,
        "#!/bin/sh\nfor a in \"$@\"; do if [ \"$a\" = ls-tree ]; then sleep 30 & wait; fi; done\nexec git \"$@\"\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let store = h.open();
    let runtime_ = rt
        .block_on(boot(composition(&h, store.clone(), fake.to_str().unwrap())))
        .unwrap();
    let ctx = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &ctx, &src);
    let found = discovered(&runtime_, &ctx);
    let run = index(&runtime_, &ctx, &found[0], "key-1");
    rt.block_on(within("the run reaches its tree read", async {
        while run_state(&store, &run).stage != "inventory" {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }));
    let started = Instant::now();
    rt.block_on(runtime_.shutdown());
    assert!(
        started.elapsed() < Duration::from_secs(12),
        "one shutdown deadline: {:?}",
        started.elapsed()
    );
    let interrupted = run_state(&store, &run);
    assert_eq!(
        interrupted.state, RUN_RUNNING,
        "shutdown is not a cancellation: {interrupted:?}"
    );
    assert!(interrupted.cancel_requested_at_ms.is_none());
    let unsettled = read(&*store, |s| {
        s.resources().processes(&ProcessFilter::Unsettled, None, 50)
    });
    assert!(
        unsettled.iter().all(|p| p.phase != ProcessPhase::Running),
        "no Git tree outlives shutdown: {unsettled:?}"
    );
    drop(runtime_);
    drop(store);
    // The next boot (with a working Git) resumes the run from its claim.
    let store = h.open();
    let runtime_ = rt
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    assert_eq!(runtime_.report().index_runs_resumed, 1);
    rt.block_on(within("the resumed run publishes", async {
        while run_state(&store, &run).state != RUN_SUCCEEDED {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }));
    assert_eq!(run_state(&store, &run).attempt, 2);
    rt.block_on(runtime_.shutdown());
    let _ = std::fs::remove_dir_all(&src);
}
