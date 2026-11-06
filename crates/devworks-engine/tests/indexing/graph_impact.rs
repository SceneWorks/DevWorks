//! Impact over real indexed repositories, SQLite composition, gateway and retained source.
use super::*;
use devworks_core::protocol::data::{GraphPage, GraphResult};
use devworks_core::protocol::records::CodeImpact;

fn graph_page(
    app: &Runtime,
    ctx: &InvocationContext,
    operation: &str,
    query: Value,
    views: Value,
    limits: (u16, u32),
    cursor: Option<&str>,
) -> GraphPage {
    let (max_results, max_visited) = limits;
    let mut request = json!({"protocol":"1.0-draft.1","schemaRevision":3,"queryId":"impact-native","scope":{"kind":"Workspace"},"view":views,"query":query,"limits":{"maxResults":max_results,"maxVisited":max_visited,"deadlineMs":300000}});
    if let Some(cursor) = cursor {
        request["cursor"] = json!(cursor);
    }
    let result = app
        .query(ctx, operation, request.to_string().as_bytes())
        .unwrap();
    match result {
        DomainQueryReply::Result {
            result:
                OperationResult::CodeImpact { value } | OperationResult::CodeFileImpact { value },
            ..
        } => value,
        other => panic!("{other:?}"),
    }
}
fn impact(page: &GraphPage) -> &CodeImpact {
    let GraphResult::Impact { value } = &page.result else {
        panic!("{page:?}")
    };
    value
}
fn node_query(repo: &str, depth: u16) -> Value {
    json!({"kind":"NodeImpact","selection":{"target":{"kind":"Name","name":"target","repositoryId":repo},"depth":depth}})
}
fn file_query(repo: &str, path: &str) -> Value {
    json!({"kind":"FileImpact","selection":{"repositoryId":repo,"path":path,"depth":3}})
}
fn indexed(
    executor: &tokio::runtime::Runtime,
    app: &Runtime,
    store: &Arc<dyn ApplicationStore>,
    ctx: &InvocationContext,
    repository: &str,
    key: &str,
) -> StoredIndexRun {
    let run = index(app, ctx, repository, key);
    executor.block_on(within("impact source publication", async {
        loop {
            let state = run_state(store, &run);
            if state.state == RUN_SUCCEEDED {
                return state;
            }
            assert!(!run_terminal(&state.state), "{state:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }))
}
fn write_repo(root: &Path, files: &[(&str, &str)]) {
    std::fs::create_dir_all(root).unwrap();
    for (path, text) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    git(root, &["init", "-q"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "impact source"]);
}

#[test]
fn native_impact_counts_the_full_closure_before_paging_and_uses_retained_sources() {
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    let repo = src.join("impact");
    write_repo(
        &repo,
        &[
            ("src/changed.rs", "pub fn target() {}\n"),
            (
                "src/lib.rs",
                "pub mod changed;\nuse crate::changed::target;\npub fn caller() { target(); }\npub fn outer() { caller(); }\n",
            ),
            (
                "tests/target_test.rs",
                "use app::changed::target;\n#[test]\nfn target_test() { target(); }\n",
            ),
        ],
    );
    let app = executor
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let ctx = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &ctx, &src);
    let repository = discovered(&app, &ctx).remove(0);
    indexed(&executor, &app, &store, &ctx, &repository, "impact-paging");
    let views = json!({"kind":"Published","repositoryIds":[repository]});
    let all = graph_page(
        &app,
        &ctx,
        "code.impact",
        node_query(&repository, 3),
        views.clone(),
        (500, 100000),
        None,
    );
    let full = impact(&all);
    assert!(full.found);
    assert_eq!(full.summary.total_affected.get(), 3);
    assert_eq!(full.summary.high_count.get(), 1);
    assert_eq!(full.summary.medium_count.get(), 2);
    assert_eq!(full.summary.critical_count.get(), 0);
    assert_eq!(
        full.affected
            .0
            .iter()
            .map(|n| n.node.name.0.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["caller", "outer", "target_test"])
    );
    let summary = serde_json::to_value(&full.summary).unwrap();
    let mut page = graph_page(
        &app,
        &ctx,
        "code.impact",
        node_query(&repository, 3),
        views.clone(),
        (1, 100000),
        None,
    );
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let value = impact(&page);
        assert_eq!(serde_json::to_value(&value.summary).unwrap(), summary);
        for n in value.changed.0.iter().chain(&value.affected.0) {
            assert!(seen.insert(n.node.node_id.as_str().to_owned()));
            assert!(n.node.source.is_some());
        }
        let Some(cursor) = page.next_cursor.clone() else {
            break;
        };
        page = graph_page(
            &app,
            &ctx,
            "code.impact",
            node_query(&repository, 3),
            views.clone(),
            (1, 100000),
            Some(&cursor),
        );
    }
    assert_eq!(seen.len(), 4);
    let shallow = graph_page(
        &app,
        &ctx,
        "code.impact",
        node_query(&repository, 1),
        views.clone(),
        (500, 100000),
        None,
    );
    assert_eq!(impact(&shallow).summary.total_affected.get(), 2);
    // Neither the live checkout nor current source files contribute to an exact graph answer.
    std::fs::remove_dir_all(&repo).unwrap();
    let retained = graph_page(
        &app,
        &ctx,
        "code.file_impact",
        file_query(&repository, "src/changed.rs"),
        views.clone(),
        (500, 100000),
        None,
    );
    assert_eq!(
        serde_json::to_value(&impact(&retained).summary).unwrap(),
        summary
    );
    assert!(
        impact(&retained)
            .changed
            .0
            .iter()
            .all(|n| n.node.source.as_ref().unwrap().path.0 == "src/changed.rs")
    );
    let partial = graph_page(
        &app,
        &ctx,
        "code.impact",
        node_query(&repository, 3),
        views,
        (500, 1),
        None,
    );
    assert_eq!(
        serde_json::to_value(&partial).unwrap()["scopeCoverage"]["coverage"]["state"],
        "Partial"
    );
    executor.block_on(app.shutdown());
    let _ = std::fs::remove_dir_all(src);
}

#[test]
fn native_file_impact_distinguishes_unindexed_language_and_missing_indexed_file() {
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    let repo = src.join("language");
    write_repo(&repo, &[("src/lib.rs", "pub fn target() {}\n")]);
    let app = executor
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let ctx = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &ctx, &src);
    let repository = discovered(&app, &ctx).remove(0);
    indexed(
        &executor,
        &app,
        &store,
        &ctx,
        &repository,
        "impact-language",
    );
    let views = json!({"kind":"Published","repositoryIds":[repository]});
    for (path, indexed) in [
        ("src/new.rs", true),
        ("src/New.RS", true),
        ("src/new.m", false),
        ("README.md", true),
    ] {
        let page = graph_page(
            &app,
            &ctx,
            "code.file_impact",
            file_query(&repository, path),
            views.clone(),
            (100, 100000),
            None,
        );
        let value = impact(&page);
        assert!(!value.found);
        assert_eq!(value.language_indexed, Some(indexed), "{path}");
        assert_eq!(value.summary.total_affected.get(), 0);
        assert!(matches!(
            value.blast_radius,
            devworks_core::protocol::records::CodeRisk::None
        ));
    }
    executor.block_on(app.shutdown());
    let _ = std::fs::remove_dir_all(src);
}

#[test]
fn native_file_impact_uses_only_pinned_crate_manifests_and_preserves_missing_repository_coverage() {
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    for (name, manifest) in [
        (
            "core",
            "[package]\nname = \"impact-core\"\nversion = \"0.1.0\"\n",
        ),
        (
            "api",
            "[package]\nname = \"impact-api\"\nversion = \"0.1.0\"\n[dependencies]\nimpact-core = { path = \"../core\" }\n",
        ),
        (
            "app",
            "[package]\nname = \"impact-app\"\nversion = \"0.1.0\"\n[dependencies]\nimpact-api = { path = \"../api\" }\n",
        ),
    ] {
        write_repo(
            &src.join(name),
            &[
                ("Cargo.toml", manifest),
                ("src/lib.rs", "pub fn target() {}\n"),
            ],
        );
    }
    let app = executor
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let ctx = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &ctx, &src);
    let repositories = discovered(&app, &ctx);
    let ids: std::collections::BTreeMap<_, _> = repositories
        .iter()
        .map(|id| {
            let repository = read(&*store, |r| r.projects().repository(&super::id(id))).unwrap();
            (repository.name, id.clone())
        })
        .collect();
    for name in ["core", "api"] {
        indexed(
            &executor,
            &app,
            &store,
            &ctx,
            &ids[name],
            &format!("impact-package-{name}"),
        );
    }
    let all_views = json!({"kind":"Published","repositoryIds":repositories});
    let partial = graph_page(
        &app,
        &ctx,
        "code.file_impact",
        file_query(&ids["core"], "src/lib.rs"),
        all_views.clone(),
        (500, 100000),
        None,
    );
    assert_eq!(
        serde_json::to_value(&partial).unwrap()["scopeCoverage"]["coverage"]["state"],
        "Partial"
    );
    assert_eq!(
        impact(&partial).summary.total_affected.get(),
        0,
        "crate dependents are not affected symbols"
    );
    assert!(
        partial
            .scope_coverage
            .as_ref()
            .unwrap()
            .unavailable_repositories
            .0
            .iter()
            .any(|r| r.repository_id.as_str() == ids["app"])
    );
    indexed(
        &executor,
        &app,
        &store,
        &ctx,
        &ids["app"],
        "impact-package-app",
    );
    let full = graph_page(
        &app,
        &ctx,
        "code.file_impact",
        file_query(&ids["core"], "src/lib.rs"),
        all_views,
        (500, 100000),
        None,
    );
    let value = impact(&full);
    assert!(value.found);
    assert_eq!(value.summary.total_affected.get(), 0);
    assert!(matches!(
        value.blast_radius,
        devworks_core::protocol::records::CodeRisk::None
    ));
    assert_eq!(value.summary.cross_repository_count.get(), 2);
    assert_eq!(value.cross_repository.0.len(), 2);
    assert!(
        value
            .cross_repository
            .0
            .iter()
            .all(|e| e.edge_kind.0 == "REFERENCES_PACKAGE")
    );
    let isolated = graph_page(
        &app,
        &ctx,
        "code.file_impact",
        file_query(&ids["core"], "src/lib.rs"),
        json!({"kind":"Published","repositoryIds":[ids["core"]]}),
        (500, 100000),
        None,
    );
    assert_eq!(impact(&isolated).summary.cross_repository_count.get(), 0);
    assert!(impact(&isolated).cross_repository.0.is_empty());
    executor.block_on(app.shutdown());
    let _ = std::fs::remove_dir_all(src);
}

#[test]
fn native_deleted_file_impact_reports_unmapped_inherited_callers_without_baseline_fallback() {
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    let repo = src.join("deleted");
    write_repo(
        &repo,
        &[
            ("src/changed.rs", "pub fn target() {}\n"),
            (
                "src/lib.rs",
                "pub mod changed;\nuse crate::changed::target;\npub fn caller() { target(); }\n",
            ),
        ],
    );
    let app = executor
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let ctx = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &ctx, &src);
    let repository = discovered(&app, &ctx).remove(0);
    let baseline = indexed(&executor, &app, &store, &ctx, &repository, "impact-base");
    git(&repo, &["rm", "src/changed.rs"]);
    git(&repo, &["commit", "-q", "-m", "delete changed target"]);
    let frame = json!({"protocol":"1.0-draft.1","schemaRevision":2,"requestKey":"impact-deleted-overlay","command":{"kind":"RefreshOverlay","repositoryId":repository,"ref":"HEAD","expectedSourceRevision":"1"}});
    let CommandReply::Recorded { receipt, .. } = app
        .command(
            &ctx,
            "indexing.refresh_overlay",
            frame.to_string().as_bytes(),
        )
        .unwrap()
    else {
        panic!("overlay receipt")
    };
    let DomainDecision::Applied {
        result: OperationResult::IndexingRefreshOverlay { value },
    } = receipt.decision
    else {
        panic!("overlay accepted")
    };
    let overlay = executor.block_on(within("deleted-file impact overlay publication", async {
        loop {
            let state = run_state(&store, value.id.as_str());
            if state.state == RUN_SUCCEEDED {
                break state;
            }
            assert!(!run_terminal(&state.state), "{state:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }));
    let old = graph_page(
        &app,
        &ctx,
        "code.file_impact",
        file_query(&repository, "src/changed.rs"),
        json!({"kind":"Exact","viewIds":[baseline.output_view_id.unwrap()]}),
        (100, 100000),
        None,
    );
    assert!(impact(&old).found);
    assert_eq!(impact(&old).summary.total_affected.get(), 1);
    let deleted = graph_page(
        &app,
        &ctx,
        "code.file_impact",
        file_query(&repository, "src/changed.rs"),
        json!({"kind":"Exact","viewIds":[overlay.output_view_id.unwrap()]}),
        (100, 100000),
        None,
    );
    let value = impact(&deleted);
    assert!(!value.found);
    assert!(value.changed.0.is_empty());
    assert_eq!(value.summary.total_affected.get(), 0);
    assert!(value.unmapped_inbound_edges.as_ref().unwrap().get() >= 1);
    assert_eq!(value.language_indexed, Some(true));
    assert_eq!(
        serde_json::to_value(&deleted).unwrap()["scopeCoverage"]["coverage"]["state"],
        "Partial"
    );
    executor.block_on(app.shutdown());
    let _ = std::fs::remove_dir_all(src);
}
