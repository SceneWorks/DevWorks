//! Impact over real indexed repositories, SQLite composition, gateway and retained source.
use super::*;
use devworks_core::protocol::data::{GraphPage, GraphResult};
use devworks_core::protocol::records::CodeImpact;

fn graph_page(
    app: &Runtime,
    ctx: &InvocationContext,
    operation: &str,
    query: Value,
    views: Option<Value>,
    limits: (u16, u32),
    cursor: Option<&str>,
) -> GraphPage {
    let (max_results, max_visited) = limits;
    let mut request = json!({"protocol":"1.0-draft.1","schemaRevision":3,"queryId":"impact-native","scope":{"kind":"Workspace"},"query":query,"limits":{"maxResults":max_results,"maxVisited":max_visited,"deadlineMs":300000}});
    match views {
        Some(view) => request["view"] = view,
        None => request["current"] = json!(true),
    }
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
        Some(views.clone()),
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
        Some(views.clone()),
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
            Some(views.clone()),
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
        Some(views.clone()),
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
        Some(views.clone()),
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
        Some(views),
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
            Some(views.clone()),
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
        None,
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
        Some(all_views),
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
        Some(json!({"kind":"Published","repositoryIds":[ids["core"]]})),
        (500, 100000),
        None,
    );
    assert_eq!(impact(&isolated).summary.cross_repository_count.get(), 0);
    assert!(impact(&isolated).cross_repository.0.is_empty());
    executor.block_on(app.shutdown());
    let _ = std::fs::remove_dir_all(src);
}

fn refresh_overlay(
    executor: &tokio::runtime::Runtime,
    app: &Runtime,
    store: &Arc<dyn ApplicationStore>,
    ctx: &InvocationContext,
    repository: &str,
    key: &str,
) -> StoredIndexRun {
    let source_revision = read(&**store, |r| {
        devworks_core::services::indexing::section_config(
            r.indexing(),
            &devworks_core::services::indexing::section_key(ctx.project_id()),
        )
        .map(|s| s.revision)
    });
    let frame = json!({"protocol":"1.0-draft.1","schemaRevision":2,"requestKey":key,"command":{"kind":"RefreshOverlay","repositoryId":repository,"ref":"refs/heads/main","expectedSourceRevision":source_revision.to_string()}});
    let CommandReply::Recorded { receipt, .. } = app
        .command(
            ctx,
            "indexing.refresh_overlay",
            frame.to_string().as_bytes(),
        )
        .unwrap()
    else {
        panic!("overlay reply was not recorded")
    };
    let value = match receipt.decision {
        DomainDecision::Applied {
            result: OperationResult::IndexingRefreshOverlay { value },
        } => value,
        other => panic!("overlay expected Applied, got {other:?}"),
    };
    executor.block_on(within("impact overlay publication", async {
        loop {
            let state = run_state(store, value.id.as_str());
            if state.state == RUN_SUCCEEDED {
                break state;
            }
            assert!(!run_terminal(&state.state), "{state:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }))
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
                "pub mod changed;\nuse crate::changed::target;\npub fn caller() { target(); never_indexed(); }\n",
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
    let overlay = refresh_overlay(
        &executor,
        &app,
        &store,
        &ctx,
        &repository,
        "impact-deleted-overlay",
    );
    let old = graph_page(
        &app,
        &ctx,
        "code.file_impact",
        file_query(&repository, "src/changed.rs"),
        Some(json!({"kind":"Exact","viewIds":[baseline.output_view_id.unwrap()]})),
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
        Some(json!({"kind":"Exact","viewIds":[overlay.output_view_id.clone().unwrap()]})),
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
    let unknown = graph_page(
        &app,
        &ctx,
        "code.impact",
        json!({"kind":"NodeImpact","selection":{"target":{"kind":"Name","name":"never_indexed","repositoryId":repository},"depth":3}}),
        Some(json!({"kind":"Exact","viewIds":[overlay.output_view_id.unwrap()]})),
        (100, 100000),
        None,
    );
    assert!(!impact(&unknown).found);
    assert_eq!(
        impact(&unknown)
            .unmapped_inbound_edges
            .as_ref()
            .unwrap()
            .get(),
        0,
        "a never-mapped symbol is not an unmapped former endpoint"
    );
    executor.block_on(app.shutdown());
    let _ = std::fs::remove_dir_all(src);
}

/// Controlled exact semantic capture. Definitions still come from the real Tree-sitter
/// parser; only explicitly listed foreign edges are supplied by the fixture. This does not
/// claim rust-analyzer/SCIP or SemanticEnricher execution.
#[derive(Clone)]
struct ForeignCapture {
    source: String,
    repository_id: devworks_core::protocol::Id,
    target_kind: String,
    target_name: String,
    target_qualified: String,
    edge: String,
}
struct ScriptedSemanticCapture {
    parser: TreeSitterParser,
    captures: std::sync::Mutex<std::collections::BTreeMap<String, Vec<ForeignCapture>>>,
}
impl devworks_core::ports::indexing::StructuralParser for ScriptedSemanticCapture {
    fn languages(&self) -> &[devworks_core::ports::indexing::LanguageInfo] {
        devworks_core::ports::indexing::StructuralParser::languages(&self.parser)
    }
    fn language_of(&self, path: &str) -> Option<&devworks_core::ports::indexing::LanguageInfo> {
        devworks_core::ports::indexing::StructuralParser::language_of(&self.parser, path)
    }
    fn extract(
        &self,
        language: &str,
        path: &str,
        repository: &str,
        source: &[u8],
        stop: &(dyn Fn() -> bool + Sync),
        budget_ms: u64,
    ) -> Result<
        devworks_core::ports::indexing::FileExtract,
        devworks_core::ports::indexing::ExtractFault,
    > {
        use devworks_core::ports::indexing::{
            EdgeTarget, ExtractedEdge, Resolution, StructuralParser,
        };
        let mut extracted = StructuralParser::extract(
            &self.parser,
            language,
            path,
            repository,
            source,
            stop,
            budget_ms,
        )?;
        if path != "src/lib.rs" {
            return Ok(extracted);
        }
        let captures = self
            .captures
            .lock()
            .unwrap()
            .get(repository)
            .cloned()
            .unwrap_or_default();
        let captures: Vec<_> = captures
            .into_iter()
            .filter_map(|capture| {
                extracted
                    .nodes
                    .iter()
                    .position(|n| n.name == capture.source)
                    .map(|source| (source, capture))
            })
            .collect();
        extracted.edges.retain(|e| {
            !captures
                .iter()
                .any(|(source, capture)| e.source == *source && e.kind == capture.edge)
        });
        for (source, capture) in captures {
            extracted.edges.push(ExtractedEdge {
                kind: capture.edge,
                source,
                target: EdgeTarget::RepositoryNode {
                    repository_id: capture.repository_id,
                    unit: "src/lib.rs".into(),
                    kind: capture.target_kind,
                    target_name: Some(capture.target_name),
                    qualified_name: capture.target_qualified,
                },
                resolution: Resolution::Resolved,
                confidence: 1.0,
                properties: json!({"fixtureSource":"ScriptedSemanticCapture","resolutionBasis":"scripted-exact-capture"})
                    .as_object().unwrap().clone(),
            });
        }
        Ok(extracted)
    }
    fn group_of(&self, path: &str) -> Option<&'static str> {
        devworks_core::ports::indexing::StructuralParser::group_of(&self.parser, path)
    }
    fn extract_group(
        &self,
        group: &str,
        repository: &str,
        files: &[(String, Vec<u8>)],
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<
        std::collections::BTreeMap<String, devworks_core::ports::indexing::FileExtract>,
        devworks_core::ports::indexing::ExtractFault,
    > {
        devworks_core::ports::indexing::StructuralParser::extract_group(
            &self.parser,
            group,
            repository,
            files,
            stop,
        )
    }
}

#[test]
fn native_exact_semantic_capture_cross_repository_risk_and_publisher_direction_are_scoped() {
    use devworks_core::ports::indexing::StructuralParser;
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    let sources = std::collections::BTreeMap::from([
        ("origin", "pub fn target() {}\npub fn publisher() {}\n"),
        (
            "consumer",
            "pub fn caller() { target(); }\npub fn consumer() {}\n",
        ),
        ("downstream", "pub fn outer() { caller(); consumer(); }\n"),
    ]);
    for (name, source) in &sources {
        write_repo(&src.join(name), &[("src/lib.rs", source)]);
    }
    let parser = Arc::new(ScriptedSemanticCapture {
        parser: TreeSitterParser::new(),
        captures: Default::default(),
    });
    let mut composition = composition(&h, store.clone(), "git");
    composition.indexing.as_mut().unwrap().parser = parser.clone();
    let app = executor.block_on(boot(composition)).unwrap();
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
    let capture = |source: &str, repo: &str, target: &str, edge: &str| {
        let extracted = parser
            .parser
            .extract(
                "rust",
                "src/lib.rs",
                repo,
                sources[repo].as_bytes(),
                &|| false,
                1000,
            )
            .unwrap();
        let node = extracted.nodes.iter().find(|n| n.name == target).unwrap();
        ForeignCapture {
            source: source.into(),
            repository_id: id(&ids[repo]),
            target_kind: node.kind.clone(),
            target_name: node.name.clone(),
            target_qualified: node.qualified_name.clone(),
            edge: edge.into(),
        }
    };
    {
        let mut captures = parser.captures.lock().unwrap();
        captures.insert(
            "origin".into(),
            vec![capture("publisher", "consumer", "consumer", "PUBLISHES")],
        );
        captures.insert(
            "consumer".into(),
            vec![capture("caller", "origin", "target", "CALLS")],
        );
        captures.insert(
            "downstream".into(),
            vec![
                capture("outer", "consumer", "caller", "CALLS"),
                capture("outer", "consumer", "consumer", "CALLS"),
            ],
        );
    }
    let mut baseline_views = std::collections::BTreeMap::new();
    for name in ["origin", "consumer", "downstream"] {
        let run = indexed(
            &executor,
            &app,
            &store,
            &ctx,
            &ids[name],
            &format!("semantic-{name}"),
        );
        baseline_views.insert(name, run.output_view_id.unwrap());
    }
    let views = json!({"kind":"Published","repositoryIds":repositories});
    let direct = graph_page(
        &app,
        &ctx,
        "code.impact",
        node_query(&ids["origin"], 3),
        Some(views.clone()),
        (500, 100000),
        None,
    );
    let value = impact(&direct);
    assert_eq!(value.summary.total_affected.get(), 2);
    assert_eq!(value.summary.critical_count.get(), 1);
    assert_eq!(value.summary.high_count.get(), 1);
    assert_eq!(value.summary.cross_repository_count.get(), 2);
    assert!(value.affected.0.iter().any(|n| n.node.name.0 == "caller"
        && matches!(n.risk, devworks_core::protocol::records::CodeRisk::Critical)));
    assert!(
        value
            .cross_repository
            .0
            .iter()
            .any(|e| e.edge_kind.0 == "TRANSITIVE")
    );
    let publisher = json!({"kind":"NodeImpact","selection":{"target":{"kind":"Name","name":"publisher","repositoryId":ids["origin"]},"depth":3}});
    let published = graph_page(
        &app,
        &ctx,
        "code.impact",
        publisher,
        Some(views),
        (500, 100000),
        None,
    );
    let value = impact(&published);
    assert_eq!(value.summary.total_affected.get(), 2);
    assert!(value.affected.0.iter().any(|n| n.node.name.0 == "consumer"
        && n.edge_kind.0 == "PUBLISHES"
        && matches!(n.risk, devworks_core::protocol::records::CodeRisk::Critical)));
    let isolated = graph_page(
        &app,
        &ctx,
        "code.impact",
        node_query(&ids["origin"], 3),
        Some(json!({"kind":"Published","repositoryIds":[ids["origin"]]})),
        (500, 100000),
        None,
    );
    assert_eq!(impact(&isolated).summary.total_affected.get(), 0);
    assert!(impact(&isolated).affected.0.is_empty());
    // The exact capture marker is durably present in the actual sealed index, independently
    // from the public impact projection and without any direct graph-table test writes.
    let marked = read(&*store, |r| {
        let published = r.indexing().published(&id(&ids["consumer"]))?.unwrap();
        let view = r.indexing().view(&published.view_id)?.unwrap();
        Ok(r.indexing()
            .edges(&view.baseline_id, "src/lib.rs")?
            .iter()
            .any(|e| e.payload["properties"]["fixtureSource"] == "ScriptedSemanticCapture"))
    });
    assert!(marked);
    // A caller's retained baseline can lose its exact foreign target in another repository's
    // selected overlay. The operator must report that prior endpoint, not a current fallback.
    std::fs::write(src.join("origin/src/lib.rs"), "pub fn publisher() {}\n").unwrap();
    git(&src.join("origin"), &["add", "-A"]);
    git(
        &src.join("origin"),
        &["commit", "-q", "-m", "remove exact foreign target"],
    );
    let overlay = refresh_overlay(
        &executor,
        &app,
        &store,
        &ctx,
        &ids["origin"],
        "semantic-remove-target",
    );
    let lost = graph_page(
        &app,
        &ctx,
        "code.impact",
        node_query(&ids["origin"], 3),
        Some(
            json!({"kind":"Exact","viewIds":[overlay.output_view_id.unwrap(),baseline_views["consumer"],baseline_views["downstream"]]}),
        ),
        (500, 100000),
        None,
    );
    assert!(!impact(&lost).found);
    assert!(
        impact(&lost).unmapped_inbound_edges.as_ref().unwrap().get() >= 1,
        "baseline foreign callers retain lost-target provenance"
    );
    executor.block_on(app.shutdown());
    let _ = std::fs::remove_dir_all(src);
}

#[test]
fn native_impact_totals_are_not_capped_by_the_five_hundred_row_wire_page() {
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    let repo = src.join("wide-impact");
    let mut source = String::from("pub fn target() {}\n");
    for n in 0..510 {
        source.push_str(&format!("pub fn caller_{n}() {{ target(); }}\n"));
    }
    write_repo(&repo, &[("src/lib.rs", &source)]);
    let app = executor
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let ctx = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &ctx, &src);
    let repository = discovered(&app, &ctx).remove(0);
    indexed(&executor, &app, &store, &ctx, &repository, "wide-impact");
    let views = json!({"kind":"Published","repositoryIds":[repository]});
    let mut cursor = None;
    let mut seen = std::collections::BTreeSet::new();
    let mut pages = 0;
    loop {
        let page = graph_page(
            &app,
            &ctx,
            "code.impact",
            node_query(&repository, 3),
            Some(views.clone()),
            (500, 100000),
            cursor.as_deref(),
        );
        let value = impact(&page);
        assert_eq!(value.summary.total_affected.get(), 510);
        assert_eq!(value.summary.high_count.get(), 510);
        for node in value.changed.0.iter().chain(&value.affected.0) {
            assert!(seen.insert(node.node.node_id.as_str().to_owned()));
        }
        pages += 1;
        assert!(pages < 20, "stable cursor makes progress");
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert!(pages >= 2);
    assert_eq!(seen.len(), 511);
    executor.block_on(app.shutdown());
    let _ = std::fs::remove_dir_all(src);
}
