//! PR42 regressions through the real SQLite/index/parser/runtime, without model execution.
use super::*;
use devworks_core::protocol::data::{GraphPage, GraphResult};

fn graph(
    rt: &Runtime,
    ctx: &InvocationContext,
    operation: &str,
    query: Value,
    cursor: Option<&str>,
    view: Option<Value>,
) -> GraphPage {
    let mut frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,"queryId":"review-native","scope":{"kind":"Workspace"},"query":query,"limits":{"maxResults":100,"maxVisited":20000,"deadlineMs":30000}});
    if operation == "repositories.nodes" {
        // This registered operation owns NodesInput and its 100,000-visit normalization;
        // it does not accept the code.* GraphReadFrame's caller-supplied limits.
        let mut body = json!({"kind":operation,"scope":{"kind":"Workspace"},"repositoryId":query["selection"]["repositoryId"],"nodeKind":query["selection"]["nodeKind"],"page":{"limit":100}});
        if let Some(view) = view {
            body["views"] = view;
        } else {
            body["current"] = json!(true);
        }
        if let Some(cursor) = cursor {
            body["page"]["cursor"] = json!(cursor);
        }
        frame = json!({"mode":"Query","protocol":"1.0-draft.1","schemaRevision":3,"queryId":"review-native","body":body});
    } else {
        if let Some(view) = view {
            frame["view"] = view;
        } else {
            frame["current"] = json!(true);
        }
        if let Some(cursor) = cursor {
            frame["cursor"] = json!(cursor);
        }
    }
    match rt
        .query(ctx, operation, frame.to_string().as_bytes())
        .unwrap()
    {
        DomainQueryReply::Result { result, .. } => {
            serde_json::from_value(serde_json::to_value(result).unwrap()["value"].clone()).unwrap()
        }
        other => panic!("{operation}: {other:?}"),
    }
}
fn commit(repo: &Path) {
    git(repo, &["init", "-q"]);
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "actual review source"]);
}
fn wait(executor: &tokio::runtime::Runtime, store: &Arc<dyn ApplicationStore>, run: &str) {
    executor.block_on(within("review index publishes", async {
        loop {
            let row = run_state(store, run);
            if row.state == RUN_SUCCEEDED {
                break;
            }
            assert!(!run_terminal(&row.state), "{row:?}");
            tokio::task::yield_now().await;
        }
    }));
}
fn scope(store: &dyn ApplicationStore, repository: &str) -> GraphScope {
    read(store, |r| {
        let id = id(repository);
        let published = r.indexing().published(&id)?.unwrap();
        let view = r.indexing().view(&published.view_id)?.unwrap();
        Ok(GraphScope {
            views: vec![GraphViewBinding {
                repository_id: id,
                view_id: view.view_id,
                view_digest: view.view_digest,
            }],
        })
    })
}
fn gaps(page: &GraphPage) -> Value {
    serde_json::to_value(&page.scope_coverage).unwrap()["coverage"]["gaps"].clone()
}

#[test]
fn native_search_keeps_matches_and_continuation_when_the_desktop_visit_budget_is_exhausted() {
    let _case = exclusive_native_runtime_case();
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    let repo = src.join("large-search");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    // Keep every file within the real parser's unchanged per-file limits.
    for unit in 0..210 {
        let mut source = String::new();
        for item in 0..100 {
            let i = unit * 100 + item;
            if i == 0 {
                source.push_str("pub fn matched_K() { matched_00001(); }\n");
            } else {
                source.push_str(&format!("pub fn matched_{i:05}() {{}}\n"));
            }
        }
        std::fs::write(repo.join(format!("src/unit_{unit:03}.rs")), source).unwrap();
    }
    commit(&repo);
    let app = executor
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let ctx = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &ctx, &src);
    let repositories = discovered(&app, &ctx);
    let run = index(&app, &ctx, &repositories[0], "review-search-index");
    wait(&executor, &store, &run);
    let selected = scope(&*store, &repositories[0]);
    let counts = read(&*store, |r| r.graph().graph_counts(&selected));
    assert!(
        counts.nodes > 20000,
        "fixture actually exceeds the desktop work budget: {counts:?}"
    );
    assert!(
        counts.edges > 20000,
        "real containment edges also exceed the budget: {counts:?}"
    );
    let kind = read(&*store, |r| {
        r.graph().graph_nodes(
            &selected,
            &GraphNodeFilter {
                pattern: Some("%matched%".into()),
                ..Default::default()
            },
            None,
            1,
        )
    })
    .items[0]
        .node
        .kind
        .clone();
    // Retained reads do not depend on a current filesystem fallback.
    std::fs::remove_dir_all(&repo).unwrap();
    for operation in ["code.search", "repositories.nodes"] {
        let query = json!({"kind":"Search","selection":{"mode":"NodeList","text":"matched_","repositoryId":repositories[0],"nodeKind":kind}});
        let first = graph(&app, &ctx, operation, query.clone(), None, None);
        let names = match &first.result {
            GraphResult::CodeSearch { value } => {
                assert!(value.total.is_none());
                value
                    .items
                    .0
                    .iter()
                    .map(|v| match v {
                        devworks_core::protocol::records::CodeSearchHit::Node { node } => {
                            node.name.0.clone()
                        }
                        _ => panic!("node only"),
                    })
                    .collect::<Vec<_>>()
            }
            GraphResult::CodeNodes { value } => {
                assert_eq!(value.total.as_ref().map(|v| v.as_str()), Some("21000"));
                value.nodes.0.iter().map(|n| n.name.0.clone()).collect()
            }
            _ => panic!("{first:?}"),
        };
        assert_eq!(
            names.len(),
            100,
            "already scanned matching nodes survive exhaustion: {first:?}"
        );
        assert!(names.iter().all(|n| n.starts_with("matched_")));
        if operation == "code.search" {
            assert!(gaps(&first).to_string().contains("graph_visited_limit"));
        }
        let cursor = first
            .next_cursor
            .as_deref()
            .expect("useful matches have a continuation");
        if operation == "repositories.nodes" {
            // Its real registered input and truthful exact total are covered above. The
            // budget-exhausted continuation is exercised through the caller-bounded API.
            continue;
        }
        let second = graph(&app, &ctx, operation, query, Some(cursor), None);
        assert_eq!(
            first.view.as_ref().unwrap().pin_id,
            second.view.as_ref().unwrap().pin_id
        );
        let second_names = match &second.result {
            GraphResult::CodeSearch { value } => value
                .items
                .0
                .iter()
                .map(|v| match v {
                    devworks_core::protocol::records::CodeSearchHit::Node { node } => {
                        node.name.0.clone()
                    }
                    _ => panic!("node only"),
                })
                .collect::<Vec<_>>(),
            GraphResult::CodeNodes { value } => {
                value.nodes.0.iter().map(|n| n.name.0.clone()).collect()
            }
            _ => panic!("{second:?}"),
        };
        assert_eq!(second_names.len(), 100);
        assert!(
            second_names.iter().all(|n| !names.contains(n)),
            "stable pages do not repeat matches"
        );
    }
    // Literal-name resolution must leave the fixed native visit budget available for
    // relationships, including Rust's Unicode lowercase match (Kelvin sign → k).
    for target in [
        json!({"kind":"Name","name":"MATCHED_k","repositoryId":repositories[0]}),
        json!({"kind":"Name","name":"HED_k"}),
    ] {
        let answer = graph(
            &app,
            &ctx,
            "code.call_path",
            json!({"kind":"Path","selection":{"target":target,"direction":"Outbound","depth":1}}),
            None,
            None,
        );
        let GraphResult::CallTrace { value } = &answer.result else {
            panic!("native name trace: {answer:?}")
        };
        assert_eq!(
            value.entries.0.len(),
            1,
            "named target retains its actual call: {answer:?}"
        );
        assert!(!gaps(&answer).to_string().contains("graph_visited_limit"));
    }
    executor.block_on(app.shutdown());
    let _ = std::fs::remove_dir_all(src);
}

#[test]
fn native_swift_scope_linking_reads_each_visible_units_edges_without_repository_rescans() {
    let _case = native_runtime_case();
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    let repo = src.join("swift-scopes");
    std::fs::create_dir_all(&repo).unwrap();
    let targets = (0..25)
        .map(|i| format!(".target(name: \"T{i:02}\", path: \"Sources/T{i:02}\")"))
        .collect::<Vec<_>>()
        .join(",");
    std::fs::write(repo.join("Package.swift"),format!("// swift-tools-version: 5.9\nimport PackageDescription\nlet package = Package(name: \"Scopes\", targets: [{targets}])\n")).unwrap();
    for i in 0..25 {
        let dir = repo.join(format!("Sources/T{i:02}"));
        std::fs::create_dir_all(&dir).unwrap();
        let mut source = (0..20)
            .map(|j| format!("import External{j}\n"))
            .collect::<String>();
        source.push_str("public func shared() {}\n");
        std::fs::write(dir.join("shared.swift"), source).unwrap();
        std::fs::write(
            dir.join("caller.swift"),
            format!("public func caller_{i:02}() {{ shared() }}\n"),
        )
        .unwrap();
    }
    commit(&repo);
    let app = executor
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let ctx = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &ctx, &src);
    let repositories = discovered(&app, &ctx);
    let run = index(&app, &ctx, &repositories[0], "review-swift-index");
    wait(&executor, &store, &run);
    let changed = repo.join("Sources/T00/caller.swift");
    let mut bytes = std::fs::read_to_string(&changed).unwrap();
    bytes.push_str("// captured overlay change\n");
    std::fs::write(changed, bytes).unwrap();
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &["commit", "-q", "-m", "compose inherited Swift scopes"],
    );
    let source_revision = read(&*store, |r| {
        devworks_core::services::indexing::section_config(
            r.indexing(),
            &devworks_core::services::indexing::section_key(ctx.project_id()),
        )
        .map(|s| s.revision)
    });
    let frame = json!({"protocol":"1.0-draft.1","schemaRevision":2,"requestKey":"swift-review-overlay","command":{"kind":"RefreshOverlay","repositoryId":repositories[0],"ref":"refs/heads/main","expectedSourceRevision":source_revision.to_string()}});
    let CommandReply::Recorded { receipt, .. } = app
        .command(
            &ctx,
            "indexing.refresh_overlay",
            frame.to_string().as_bytes(),
        )
        .unwrap()
    else {
        panic!("recorded overlay")
    };
    let DomainDecision::Applied {
        result: OperationResult::IndexingRefreshOverlay { value },
    } = receipt.decision
    else {
        panic!("actual overlay: {:?}", receipt.decision)
    };
    wait(&executor, &store, value.id.as_str());
    let overlay = run_state(&store, value.id.as_str()).output_view_id.unwrap();
    let view = read(&*store, |r| r.indexing().view(&overlay)).unwrap();
    assert!(
        view.branch_id.is_some(),
        "the query really composes an overlay with inherited Swift files"
    );
    let selected = scope(&*store, &repositories[0]);
    let counts = read(&*store, |r| r.graph().graph_counts(&selected));
    assert!(
        counts.units >= 50 && counts.edges >= 500,
        "actual captured Swift fixture: {counts:?}"
    );
    let found = graph(
        &app,
        &ctx,
        "code.search",
        json!({"kind":"Search","selection":{"mode":"NodeList","text":"caller_00","repositoryId":repositories[0]}}),
        None,
        Some(json!({"kind":"Exact","viewIds":[overlay]})),
    );
    let GraphResult::CodeSearch { value } = &found.result else {
        panic!("{found:?}")
    };
    let devworks_core::protocol::records::CodeSearchHit::Node { node } = &value.items.0[0] else {
        panic!("caller")
    };
    let detail = graph(
        &app,
        &ctx,
        "code.node_detail",
        json!({"kind":"Node","nodeId":node.node_id}),
        None,
        Some(json!({"kind":"Pinned","pinId":found.view.as_ref().unwrap().pin_id})),
    );
    assert!(
        !gaps(&detail).to_string().contains("graph_visited_limit"),
        "unit linking stays within the unchanged desktop budget: {detail:?}"
    );
    let GraphResult::NodeDetail { value } = detail.result else {
        panic!("detail")
    };
    assert!(
        value.outgoing.0.iter().any(|e| e.edge_kind.0 == "CALLS"),
        "the exact local module's call is actually linked: {value:?}"
    );
    let target = value
        .neighbors
        .0
        .iter()
        .find(|n| n.name.0 == "shared")
        .expect("actual linked local target");
    assert_eq!(
        target.unit_key.0.as_str(),
        "Sources/T00/shared.swift",
        "same-named sibling targets are not guessed"
    );
    executor.block_on(app.shutdown());
    let _ = std::fs::remove_dir_all(src);
}

#[test]
fn native_overview_keeps_unindexed_repository_metadata_when_another_scope_member_is_indexed() {
    let _case = native_runtime_case();
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    for name in ["indexed-a", "unindexed-b"] {
        let repo = src.join(name);
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/lib.rs"), "pub fn retained() {}\n").unwrap();
        commit(&repo);
    }
    let app = executor
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let ctx = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &ctx, &src);
    let repositories = discovered(&app, &ctx);
    let named = |name: &str| {
        repositories
            .iter()
            .find(|id_| {
                read(&*store, |r| r.projects().repository(&id(id_)))
                    .unwrap()
                    .name
                    == name
            })
            .unwrap()
            .clone()
    };
    let a = named("indexed-a");
    let b = named("unindexed-b");
    let run = index(&app, &ctx, &a, "review-overview-index");
    wait(&executor, &store, &run);
    assert!(read(&*store, |r| r.indexing().published(&id(&b))).is_none());
    let page = graph(
        &app,
        &ctx,
        "code.overview",
        json!({"kind":"Overview","repositoryId":b}),
        None,
        None,
    );
    let GraphResult::RepositoryOverview { value } = page.result else {
        panic!("overview")
    };
    let value = serde_json::to_value(value).unwrap();
    assert_eq!(value["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(value["repositories"][0]["name"], "unindexed-b");
    assert_eq!(value["graphCoverage"]["state"], "Unavailable");
    assert_eq!(value["unavailableReason"], "graph_not_indexed");
    assert!(value.get("graphTotals").is_none());
    assert!(value["dependencies"].as_array().unwrap().is_empty());
    executor.block_on(app.shutdown());
    let _ = std::fs::remove_dir_all(src);
}

#[test]
fn native_direct_id_traversal_retains_useful_relationships_when_edge_scan_is_partial() {
    let _case = exclusive_native_runtime_case();
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    let repo = src.join("large-calls");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    for unit in 0..210 {
        let mut source = format!("pub fn leaf_{unit:03}() {{}}\n");
        for item in 0..100 {
            source.push_str(&format!(
                "pub fn caller_{unit:03}_{item:03}() {{ leaf_{unit:03}(); }}\n"
            ));
        }
        std::fs::write(repo.join(format!("src/unit_{unit:03}.rs")), source).unwrap();
    }
    commit(&repo);
    let app = executor
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let ctx = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &ctx, &src);
    let repository = discovered(&app, &ctx).remove(0);
    let run = index(&app, &ctx, &repository, "large-calls-index");
    wait(&executor, &store, &run);
    let selected = scope(&*store, &repository);
    let mut position = None;
    let mut first = None;
    let mut count = 0;
    loop {
        let page = read(&*store, |r| {
            r.graph().graph_edges(
                &selected,
                &GraphEdgeFilter {
                    kinds: vec!["CALLS".into()],
                    ..Default::default()
                },
                position.as_ref(),
                500,
            )
        });
        if first.is_none() {
            first = page.items.first().cloned();
        }
        count += page.items.len();
        position = page.items.last().map(|edge| GraphPosition {
            repository_id: edge.repository_id.clone(),
            unit_key: edge.edge.unit_key.clone(),
            item_id: edge.edge.edge_id.clone(),
        });
        if !page.has_more {
            break;
        }
    }
    assert_eq!(
        count, 21_000,
        "actual selected edges exceed the unchanged visit budget"
    );
    let edge = first.unwrap();
    let result = graph(
        &app,
        &ctx,
        "code.traverse",
        json!({"kind":"Traverse","startIds":[edge.edge.source_node_id],"direction":"Forward","edgeKinds":["CALLS"],"maxDepth":1}),
        None,
        None,
    );
    assert!(matches!(
        result.scope_coverage.as_ref().unwrap().coverage.state,
        devworks_core::protocol::data::CoverageState::Partial
    ));
    let GraphResult::CodeGraph { value } = result.result else {
        panic!("traversal graph")
    };
    let value = serde_json::to_value(value).unwrap();
    assert!(
        value["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|found| found["edgeId"] == edge.edge.edge_id),
        "a retained actual relationship must be useful: {value}"
    );
    let target = value["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["edgeId"] == edge.edge.edge_id)
        .unwrap()["targetId"]
        .clone();
    let mut frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,"queryId":"large-call-path","scope":{"kind":"Workspace"},"current":true,
        "query":{"kind":"Path","selection":{"target":{"kind":"Node","nodeId":target},"direction":"Inbound","depth":1}},
        "limits":{"maxResults":1,"maxVisited":20000,"deadlineMs":30000}});
    let query = |frame: &Value| {
        let DomainQueryReply::Result { result, .. } = app
            .query(&ctx, "code.call_path", frame.to_string().as_bytes())
            .unwrap()
        else {
            panic!("bounded call path")
        };
        serde_json::from_value::<GraphPage>(serde_json::to_value(result).unwrap()["value"].clone())
            .unwrap()
    };
    let first = query(&frame);
    let GraphResult::CallTrace { value: first_trace } = &first.result else {
        panic!("call trace")
    };
    assert_eq!(first_trace.entries.0.len(), 1);
    assert!(matches!(
        first.scope_coverage.as_ref().unwrap().coverage.state,
        devworks_core::protocol::data::CoverageState::Partial
    ));
    assert!(
        first_trace.unmapped_inbound_edges.is_none(),
        "a partial scan cannot claim an exact unmapped count"
    );
    frame.as_object_mut().unwrap().remove("current");
    frame["view"] =
        json!({"kind":"Pinned","pinId":serde_json::to_value(&first.view).unwrap()["pinId"]});
    frame["cursor"] = json!(
        first
            .next_cursor
            .expect("retained callers have continuation")
    );
    let next = query(&frame);
    let GraphResult::CallTrace { value: next_trace } = next.result else {
        panic!("continued trace")
    };
    assert_eq!(next_trace.entries.0.len(), 1);
    assert_ne!(
        first_trace.entries.0[0].edge_id,
        next_trace.entries.0[0].edge_id
    );
    executor.block_on(app.shutdown());
    let _ = std::fs::remove_dir_all(src);
}
