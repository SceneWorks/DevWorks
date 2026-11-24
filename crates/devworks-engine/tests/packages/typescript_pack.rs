//! Native qualification through the signed Add-ons installer, compiler and published graph.
use super::semantic_fixture::{
    configure, git, index, invocations, overlay, overlay_layer, published,
};
use super::*;

const PACKAGE: &str = "devworks-typescript-semantic";

#[cfg(unix)]
fn node_proxy(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn use_test_node_proxy(app: &App, proxy: &std::path::Path) {
    let observed = app.runtimes.observations();
    let executable = devworks_core::ports::packages::RuntimeExecutable::inspect(proxy).unwrap();
    app.runtimes
        .set_probed(observed, [("node".into(), executable)].into());
}

#[cfg(unix)]
#[test]
#[ignore = "native macOS arm64 qualification requires Node 24 and a signed TypeScript bundle"]
fn native_typescript_pack_indexes_framework_relationships() {
    let _serial = serial();
    let bundle =
        std::env::var_os("DEVWORKS_TYPESCRIPT_PACK_BUNDLE").expect("signed TypeScript bundle");
    let mut app = App::unbooted(Harness::new(), None, None);
    app.indexing = true;
    let search = std::env::var_os("PATH").expect("Node 24 PATH");
    let node = std::env::split_paths(&search)
        .map(|path| path.join("node"))
        .find(|path| path.is_file())
        .expect("Node 24");
    let probe = app.h.root.join("node-probe");
    std::fs::create_dir(&probe).unwrap();
    let proxy = probe.join("node");
    let original = format!(
        "#!/bin/sh\nexec '{}' \"$@\"\n",
        node.display().to_string().replace('\'', "'\"'\"'")
    );
    node_proxy(&proxy, &original);
    app.probe_path = Some(probe.to_str().unwrap().to_owned());
    app.boot();
    assert_eq!(
        app.runtimes.executable("node").unwrap().1.program,
        node.canonicalize().unwrap()
    );
    use_test_node_proxy(&app, &proxy);
    configure(&app, PACKAGE);
    let artifact = app.save(&app.user, &std::fs::read(bundle).unwrap());
    package(app.lifecycle(
        &app.user,
        "addons.install",
        json!({"kind":"Install","bundle":artifact,"expectedRegistryRevision":app.registry().to_string()}),
    ));
    app.until(PACKAGE, "Node 24 is available", |record| {
        availability(record) == "Ready"
    });
    app.ready(PACKAGE);

    let root = app.sources.join("typescript-workspace");
    for (file, text) in [
        (
            "package.json",
            r#"{"name":"typescript-fixture","private":true,"workspaces":["packages/*"],"dependencies":{"react":"19.0.0","@angular/core":"20.0.0","express":"5.1.0"}}"#,
        ),
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"jsx":"preserve","experimentalDecorators":true,"baseUrl":".","paths":{"@client/*":["src/*"]}}}"#,
        ),
        (
            "angular.json",
            r#"{"projects":{"admin":{"root":"src/admin","projectType":"application"}}}"#,
        ),
        (
            "packages/common/package.json",
            r#"{"name":"@fixture/common","version":"3.1.4","exports":{".":"./src/index.ts"}}"#,
        ),
        (
            "packages/common/src/index.ts",
            "export { stableValue } from './value';\n",
        ),
        (
            "packages/common/src/value.ts",
            "// café\r\nexport function stableValue() { return 41; }\r\n",
        ),
        (
            "src/run.ts",
            "import {stableValue} from '@fixture/common'; export function run() { return stableValue(); }\n",
        ),
        (
            "src/unbound.ts",
            "export function caller() { return lookupWithoutImport(); }\n",
        ),
        (
            "src/other.ts",
            "export function lookupWithoutImport() { return 11; }\n",
        ),
        ("src/shared.cjs", "module.exports.answer = 42;\n"),
        (
            "src/Child.tsx",
            "export function Child() { return <button>Open</button>; }\n",
        ),
        (
            "src/Dashboard.tsx",
            "import { Child } from '@client/Child'; export function Dashboard() { return <Child/>; }\n",
        ),
        (
            "src/admin/child.ts",
            "import {Component} from '@angular/core'; @Component({selector:'admin-child',template:''}) export class AdminChild {}\n",
        ),
        (
            "src/admin/app.ts",
            "import {Component} from '@angular/core'; import {AdminChild} from './child'; @Component({selector:'admin-app',imports:[AdminChild],templateUrl:'./app.html'}) export class AdminApp {}\n",
        ),
        ("src/admin/app.html", "<admin-child/>\n"),
        (
            "src/backend.go",
            "package demo\nimport \"github.com/gin-gonic/gin\"\nfunc handler(ctx *gin.Context) {}\nfunc register() { app := gin.New(); app.GET(\"/api/items\", handler) }\n",
        ),
        (
            "src/api.ts",
            "export async function list() { return fetch('/api/items'); }\n",
        ),
        (
            "src/desktop.ts",
            "import {invoke} from '@tauri-apps/api/core'; export function readOverview() { return invoke('read_overview'); }\n",
        ),
    ] {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    std::fs::write(root.join("src/unrelated.html"), [0xff, 0xfe]).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "original"]);

    let discover = json!({"mode":"Query","protocol":"1.0-draft.1","queryId":"typescript-discover",
        "body":{"kind":"indexing.discover","scope":{"kind":"Workspace"},"roots":[],"page":{"limit":50}}});
    let DomainQueryReply::Result {
        result: OperationResult::IndexingDiscover { value },
        ..
    } = app
        .app()
        .query(
            &app.user,
            "indexing.discover",
            discover.to_string().as_bytes(),
        )
        .unwrap()
    else {
        panic!("TypeScript fixture discovery failed");
    };
    let repository = value.items.0.first().unwrap().id.clone();
    let project = devworks_core::protocol::ProjectId(id("project-a"));
    let member = repository.clone();
    let project_for_write = project.clone();
    write(&*app.store, move |tx| {
        tx.projects().set_members(&project_for_write, &[member])
    })
    .unwrap();
    index(&app, &repository, "indexing.index");
    let initial = published(&app, &repository, None);
    assert_eq!(initial.manifest["packs"][0]["id"], PACKAGE, "{initial:?}");

    let nodes = |unit: &str| {
        read(&*app.store, |snapshot| {
            snapshot.indexing().nodes(&initial.id, unit)
        })
    };
    let edges = |unit: &str| {
        read(&*app.store, |snapshot| {
            snapshot.indexing().edges(&initial.id, unit)
        })
    };
    let value = nodes("packages/common/src/value.ts")
        .into_iter()
        .find(|node| {
            node.name == "stableValue"
                && node.payload["qualifiedName"]
                    .as_str()
                    .is_some_and(|name| name.contains("#stableValue"))
        })
        .expect("the compiler-backed workspace function");
    assert!(
        edges("src/run.ts").iter().any(|edge| {
            edge.kind == "CALLS"
                && edge.payload["resolution"] == "Resolved"
                && edge.target_node_id == value.node_id
        }),
        "{:?}",
        edges("src/run.ts")
    );
    assert!(
        nodes("packages/common/package.json")
            .iter()
            .any(|node| { node.kind == "Module" && node.name == "@fixture/common" })
    );
    for (key, label) in [
        ("src/Dashboard.tsx", "RENDERS"),
        ("src/admin/app.ts", "RENDERS"),
        ("src/api.ts", "HTTP_CALLS"),
        ("src/desktop.ts", "CALLS"),
    ] {
        let observed = edges(key);
        assert!(
            observed.iter().any(
                |edge| edge.kind == label && edge.payload["provenance"]["extractor"] == PACKAGE
            ),
            "{key}: {observed:?}"
        );
    }
    let unresolved = edges("src/unbound.ts");
    assert!(
        unresolved.iter().any(|edge| {
            edge.kind == "CALLS"
                && edge.payload["provenance"]["extractor"] == PACKAGE
                && edge.payload["targetName"] == "lookupWithoutImport"
                && edge.payload["resolution"] == "Unresolved"
                && edge.payload["properties"]["compiler_unresolved"] == true
        }),
        "{unresolved:?}"
    );
    let backend = nodes("src/backend.go");
    assert!(
        backend.iter().any(|node| {
            node.kind == "Route" && node.payload["properties"]["route_template"] == "/api/items"
        }),
        "{backend:?}"
    );
    let http = edges("src/api.ts");
    assert!(
        http.iter().any(|edge| {
            edge.kind == "HTTP_CALLS"
                && edge.payload["targetName"] == "route:*:GET:/api/items"
                && edge.payload["properties"]["http_method"] == "GET"
        }),
        "{http:?}"
    );
    assert!(
        nodes("src/desktop.ts")
            .iter()
            .any(|node| node.name == "read_overview")
    );
    for (operation, query, expected) in [
        (
            "code.graph_scope_status",
            json!({"kind":"ScopeStatus"}),
            "current",
        ),
        (
            "code.source",
            json!({"kind":"Source","selection":{"kind":"File",
            "repositoryId":repository,"path":"packages/common/src/value.ts","startLine":"0","endLine":"0"}}),
            "stableValue",
        ),
        (
            "code.call_path",
            json!({"kind":"Path","selection":{"target":{"kind":"Node",
            "nodeId":value.node_id},"direction":"Inbound","depth":3}}),
            "run",
        ),
        (
            "code.nodes_by_file",
            json!({"kind":"NodesByFile","selection":{"repositoryId":repository,
            "path":"packages/common/src/value.ts","line":"2"}}),
            "stableValue",
        ),
    ] {
        let frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,"queryId":app.key("typescript-graph"),
            "scope":{"kind":"Workspace"},"current":true,"query":query,
            "limits":{"maxResults":100,"maxVisited":100000,"deadlineMs":30000}});
        let reply = app
            .app()
            .query(&app.user, operation, frame.to_string().as_bytes())
            .unwrap();
        let DomainQueryReply::Result { result, .. } = reply else {
            panic!("{operation}: {reply:?}");
        };
        let body = serde_json::to_string(&result).unwrap();
        assert!(body.contains(expected), "{operation}: {body}");
    }

    let agent = app.agent.clone().with_lineage(Some(project), None, None);
    let agent_frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,
        "queryId":app.key("typescript-agent"),"scope":{"kind":"Project","projectId":"project-a"},
        "view":{"kind":"Published","repositoryIds":[repository]},
        "query":{"kind":"Path","selection":{"target":{"kind":"Node","nodeId":value.node_id},
            "direction":"Inbound","depth":3}},
        "limits":{"maxResults":100,"maxVisited":100000,"deadlineMs":30000}});
    let agent_reply = app
        .app()
        .query(&agent, "code.call_path", agent_frame.to_string().as_bytes())
        .unwrap();
    let DomainQueryReply::Result { result, .. } = agent_reply else {
        panic!("the scoped graph tool refused the TypeScript view: {agent_reply:?}");
    };
    assert!(
        serde_json::to_string(&result)
            .unwrap()
            .contains("stableValue")
    );

    let count = invocations(&app, PACKAGE);
    assert_eq!(count, 1);
    index(&app, &repository, "indexing.index");
    let reused = published(&app, &repository, Some(&initial.id));
    assert_eq!(invocations(&app, PACKAGE), count);
    assert_eq!(reused.manifest["packs"], initial.manifest["packs"]);

    // A template-only dirty edit replaces semantic edges in the requested worktree view.
    let worktree = read(&*app.store, |snapshot| {
        Ok(snapshot
            .indexing()
            .worktrees(&repository)?
            .into_iter()
            .find(|tree| tree.main))
    })
    .expect("the discovered worktree");
    std::fs::write(root.join("src/admin/app.html"), "<p>Dirty</p>").unwrap();
    let dirty = overlay_layer(&app, &repository, "worktreeId", json!(worktree.worktree_id));
    let dirty_edges = read(&*app.store, |snapshot| {
        snapshot.indexing().edges(&dirty.id, "src/admin/app.ts")
    });
    assert!(
        dirty_edges.iter().all(|edge| edge.kind != "RENDERS"),
        "{dirty_edges:?}"
    );
    assert_eq!(published(&app, &repository, None).id, reused.id);

    // A replaced executable is refused before captured source leaves the helper.
    node_proxy(&proxy, &(original.clone() + "# same-version replacement\n"));
    let runtime_run = index(&app, &repository, "indexing.reindex");
    let runtime_changed = published(&app, &repository, Some(&reused.id));
    assert_eq!(
        runtime_changed.manifest["packs"],
        json!([]),
        "{runtime_changed:?}"
    );
    let changed_run = read(&*app.store, |snapshot| {
        snapshot.indexing().run(&runtime_run)
    })
    .unwrap();
    assert!(
        changed_run
            .warnings
            .iter()
            .any(|warning| warning.message.contains("node-runtime-changed")),
        "{changed_run:?}"
    );
    let native_host = app.app().packages().unwrap();
    native_host.refresh_runtimes();
    index(&app, &repository, "indexing.index");
    let refreshed = published(&app, &repository, Some(&runtime_changed.id));
    assert_eq!(
        refreshed.manifest["packs"][0]["id"], PACKAGE,
        "{refreshed:?}"
    );
    let retained_count = invocations(&app, PACKAGE);
    node_proxy(&proxy, "#!/bin/sh\nprintf 'v22.0.0\\n'\n");
    native_host.refresh_runtimes();
    index(&app, &repository, "indexing.index");
    let incompatible = published(&app, &repository, Some(&refreshed.id));
    assert_eq!(incompatible.manifest["packs"], json!([]));
    assert_eq!(invocations(&app, PACKAGE), retained_count);
    node_proxy(&proxy, &original);
    native_host.refresh_runtimes();
    app.ready(PACKAGE);
    index(&app, &repository, "indexing.index");
    let reused = published(&app, &repository, Some(&incompatible.id));
    assert_eq!(reused.manifest["packs"][0]["id"], PACKAGE);
    assert_eq!(invocations(&app, PACKAGE), retained_count + 1);

    std::fs::write(root.join("src/admin/app.html"), "<p>Updated</p>").unwrap();
    std::fs::remove_file(root.join("src/Child.tsx")).unwrap();
    std::fs::write(
        root.join("src/Dashboard.tsx"),
        "export function Dashboard() { return <span/>; }\n",
    )
    .unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "replace and delete"]);
    git(&root, &["branch", "typescript-review"]);
    let branch = overlay(&app, &repository, "refs/heads/typescript-review");
    assert_eq!(branch.manifest["packs"][0]["id"], PACKAGE, "{branch:?}");
    let overlay_units = read(&*app.store, |snapshot| {
        snapshot.indexing().units(&branch.id, None, 500)
    });
    assert!(
        overlay_units
            .iter()
            .any(|unit| unit.unit_key == "src/Child.tsx" && unit.action == "Delete")
    );
    let changed_edges = read(&*app.store, |snapshot| {
        snapshot.indexing().edges(&branch.id, "src/admin/app.ts")
    });
    assert!(
        changed_edges.iter().all(|edge| edge.kind != "RENDERS"),
        "{changed_edges:?}"
    );
    assert_eq!(published(&app, &repository, None).id, reused.id);

    std::fs::write(root.join("package.json"), "{malformed json").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "bad compiler input"]);
    let before_failure = invocations(&app, PACKAGE);
    index(&app, &repository, "indexing.index");
    let failed = published(&app, &repository, Some(&reused.id));
    assert_eq!(
        failed.manifest["coverage"]["state"], "Partial",
        "{failed:?}"
    );
    assert_eq!(failed.manifest["packs"], json!([]));
    assert_eq!(invocations(&app, PACKAGE), before_failure + 1);
    index(&app, &repository, "indexing.index");
    let reused_failure = published(&app, &repository, Some(&failed.id));
    assert_eq!(
        reused_failure.manifest["coverage"],
        failed.manifest["coverage"]
    );
    assert_eq!(invocations(&app, PACKAGE), before_failure + 1);
    index(&app, &repository, "indexing.reindex");
    let _rebuilt = published(&app, &repository, Some(&reused_failure.id));
    assert_eq!(invocations(&app, PACKAGE), before_failure + 2);
}

#[cfg(unix)]
fn wait_for(mut predicate: impl FnMut() -> bool, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(55);
    while !predicate() {
        assert!(Instant::now() < deadline, "{label}");
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[cfg(unix)]
fn fixture_path(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\"'\"'"))
}

#[cfg(unix)]
#[test]
#[ignore = "child process for the native TypeScript crash-recovery harness"]
fn typescript_crash_child_fixture() {
    let root = std::env::var_os("DW_TYPESCRIPT_CRASH_ROOT").expect("parent's private fixture root");
    let mut harness = Harness::new();
    harness.root = root.into();
    let root = harness.root.clone();
    let proxy = root.join("node-probe");
    let mut app = App::unbooted(harness, Some(proxy.to_str().unwrap().to_owned()), None);
    app.indexing = true;
    app.boot();
    use_test_node_proxy(&app, &proxy.join("node"));
    configure(&app, PACKAGE);
    let bundle =
        std::env::var_os("DEVWORKS_TYPESCRIPT_PACK_BUNDLE").expect("signed TypeScript bundle");
    let artifact = app.save(&app.user, &std::fs::read(bundle).unwrap());
    package(app.lifecycle(
        &app.user, "addons.install",
        json!({"kind":"Install","bundle":artifact,"expectedRegistryRevision":app.registry().to_string()}),
    ));
    app.ready(PACKAGE);
    let repo = app.sources.join("crash");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("package.json"),
        r#"{"name":"typescript-recovery"}"#,
    )
    .unwrap();
    std::fs::write(
        repo.join("index.ts"),
        "export function priorValue() { return 1; }",
    )
    .unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "original"]);
    let frame = json!({"mode":"Query","protocol":"1.0-draft.1","queryId":"typescript-crash-discover",
        "body":{"kind":"indexing.discover","scope":{"kind":"Workspace"},"roots":[],"page":{"limit":50}}});
    let DomainQueryReply::Result {
        result: OperationResult::IndexingDiscover { value },
        ..
    } = app
        .app()
        .query(&app.user, "indexing.discover", frame.to_string().as_bytes())
        .unwrap()
    else {
        panic!("crash fixture discovery failed");
    };
    let repository = value.items.0.first().unwrap().id.clone();
    index(&app, &repository, "indexing.index");
    let baseline = published(&app, &repository, None);
    assert_eq!(baseline.manifest["packs"][0]["id"], PACKAGE, "{baseline:?}");
    std::fs::write(
        root.join("baseline.json"),
        json!({
            "repository":repository,"generation":baseline.id,
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        repo.join("index.ts"),
        "export function recoveredValue() { return 2; }",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "pending during host loss"]);
    std::fs::write(root.join("hold-node"), "").unwrap();
    index(&app, &repository, "indexing.index");
    let deadline = Instant::now() + Duration::from_secs(55);
    while Instant::now() < deadline {
        super::event_intents::drive(&app, Duration::from_millis(50));
    }
    panic!("the crash fixture was never interrupted");
}

#[cfg(unix)]
#[test]
#[ignore = "kills and recovers a private native host process running the TypeScript pack"]
fn native_typescript_host_crash_recovers_owned_node_and_index_receipt() {
    let harness = Harness::new();
    let root = harness.root.clone();
    let node = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|path| path.join("node"))
        .find(|path| path.is_file())
        .expect("Node 24");
    let temp = root.join("compiler-scratch");
    let probe = root.join("node-probe");
    let child_marker = root.join("node-running");
    let hold = root.join("hold-node");
    std::fs::create_dir_all(&temp).unwrap();
    std::fs::create_dir(&probe).unwrap();
    node_proxy(
        &probe.join("node"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" != --version ] && [ -f {} ]; then\n  printf '%s\\n%s\\n' \"$$\" \"$PWD\" > {}\n  exec {} -e 'setInterval(() => {{}}, 1000)'\nfi\nexec {} \"$@\"\n",
            fixture_path(&hold),
            fixture_path(&child_marker),
            fixture_path(&node),
            fixture_path(&node),
        ),
    );
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "typescript_pack::typescript_crash_child_fixture",
            "--nocapture",
        ])
        .env("DW_TYPESCRIPT_CRASH_ROOT", &root)
        .env("TMPDIR", &temp)
        .spawn()
        .unwrap();
    wait_for(
        || {
            assert!(
                child.try_wait().unwrap().is_none(),
                "native host exited before Node started"
            );
            std::fs::read_to_string(&child_marker).is_ok_and(|marker| marker.lines().count() == 2)
        },
        "native host did not reach a live Node invocation",
    );
    let marker = std::fs::read_to_string(&child_marker).unwrap();
    let mut values = marker.lines();
    let pid = values.next().unwrap();
    let scratch = std::path::PathBuf::from(values.next().unwrap());
    assert!(scratch.starts_with(temp.canonicalize().unwrap()));
    assert!(scratch.join("repository/index.ts").is_file());
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    std::fs::remove_file(&hold).unwrap();
    wait_for(
        || {
            let active = std::process::Command::new("ps")
                .args(["-p", pid, "-o", "stat="])
                .output()
                .is_ok_and(|result| {
                    result.status.success()
                        && !String::from_utf8_lossy(&result.stdout)
                            .trim_start()
                            .starts_with('Z')
                });
            !active && !scratch.exists()
        },
        "the owned Node process or its scratch survived hard host loss",
    );

    harness.ids.set(1_000_000);
    let old: Value =
        serde_json::from_slice(&std::fs::read(root.join("baseline.json")).unwrap()).unwrap();
    let repository: Id = serde_json::from_value(old["repository"].clone()).unwrap();
    let baseline: Id = serde_json::from_value(old["generation"].clone()).unwrap();
    let mut app = App::unbooted(harness, Some(probe.to_str().unwrap().to_owned()), None);
    let interrupted = read(&*app.store, |snapshot| {
        snapshot.indexing().active_run(&repository, None)
    })
    .expect("durable interrupted index");
    assert_eq!(interrupted.attempt, 1);
    assert_eq!(interrupted.state, "Running");
    assert!(interrupted.output_view_id.is_none());
    let old_head = read(&*app.store, |snapshot| {
        let index = snapshot.indexing();
        let pointer = index.published(&repository)?.unwrap();
        index.view(&pointer.view_id)
    })
    .unwrap();
    assert_eq!(old_head.baseline_id, baseline);
    app.indexing = true;
    app.boot();
    let recovered = published(&app, &repository, Some(&baseline));
    let run = read(&*app.store, |snapshot| {
        snapshot.indexing().run(&interrupted.id)
    })
    .unwrap();
    assert_eq!(run.attempt, 2);
    assert_eq!(run.state, "Succeeded");
    assert_eq!(run.generation_id.as_ref(), Some(&recovered.id));
    assert_ne!(run.generation_id, interrupted.generation_id);
    let nodes = read(&*app.store, |snapshot| {
        snapshot.indexing().nodes(&recovered.id, "index.ts")
    });
    assert!(nodes.iter().any(|node| {
        node.name == "recoveredValue"
            && node.payload["qualifiedName"]
                .as_str()
                .is_some_and(|value| value.contains("#recoveredValue"))
    }));
    assert!(nodes.iter().all(|node| node.name != "priorValue"));

    // Removing authority while a second Node process is live must fence the graph write.
    use_test_node_proxy(&app, &probe.join("node"));
    std::fs::remove_file(&child_marker).unwrap();
    let repo = app.sources.join("crash");
    std::fs::write(
        repo.join("index.ts"),
        "export function disabledValue() { return 3; }",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "pending during disable"]);
    std::fs::write(&hold, "").unwrap();
    let pending_disable = index(&app, &repository, "indexing.index");
    wait_for(
        || {
            super::event_intents::drive(&app, Duration::from_millis(25));
            let run = read(&*app.store, |snapshot| {
                snapshot.indexing().run(&pending_disable)
            })
            .unwrap();
            assert!(
                matches!(run.state.as_str(), "Queued" | "Running"),
                "{run:?}"
            );
            std::fs::read_to_string(&child_marker).is_ok_and(|marker| marker.lines().count() == 2)
        },
        "Node was not live before disable",
    );
    package(app.simple(&app.user, "addons.disable", "Disable", PACKAGE));
    std::fs::remove_file(&hold).unwrap();
    let stopped_scratch = std::fs::read_to_string(&child_marker).unwrap();
    let stopped_scratch = std::path::Path::new(stopped_scratch.lines().nth(1).unwrap());
    wait_for(
        || !stopped_scratch.exists(),
        "the disabled package kept its private source",
    );
    assert_eq!(published(&app, &repository, None).id, recovered.id);
    app.h.clock.advance(20_000);
    let fallback = published(&app, &repository, Some(&recovered.id));
    assert_eq!(fallback.manifest["packs"], json!([]), "{fallback:?}");
    assert!(
        read(&*app.store, |snapshot| snapshot
            .indexing()
            .nodes(&fallback.id, "index.ts"))
        .iter()
        .any(|node| node.name == "disabledValue")
    );
}
