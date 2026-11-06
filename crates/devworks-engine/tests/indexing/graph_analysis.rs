//! Named analysis queries over the actual published SQLite composition and native parser.
//! No model, network, filesystem fallback, or fake graph backend participates.
use super::*;

fn graph(
    rt: &Runtime,
    ctx: &InvocationContext,
    operation: &str,
    query: Value,
    limit: u32,
) -> Value {
    let frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,"queryId":"analysis-query",
        "scope":{"kind":"Workspace"},"current":true,"query":query,
        "limits":{"maxResults":limit,"maxVisited":100000,"deadlineMs":30000}});
    match rt
        .query(ctx, operation, frame.to_string().as_bytes())
        .unwrap()
    {
        DomainQueryReply::Result { result, .. } => serde_json::to_value(result).unwrap(),
        DomainQueryReply::Rejected { fault, .. } => panic!("{operation}: {fault:?}"),
    }
}

#[test]
fn indexed_architecture_overview_and_dependencies_preserve_exact_facts_and_unknown_analysis() {
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    let repo = src.join("analysis");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"),"pub fn leaf() {}\npub fn a(){leaf();}\npub fn b(){leaf();}\npub fn c(){leaf();}\npub fn d(){leaf();}\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "analysis"]);
    let app = executor
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let ctx = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &ctx, &src);
    let repositories = discovered(&app, &ctx);
    let run = index(&app, &ctx, &repositories[0], "analysis-index");
    executor.block_on(within("analysis source publishes", async {
        loop {
            let state = run_state(&store, &run);
            if state.state == RUN_SUCCEEDED {
                break;
            }
            assert!(!run_terminal(&state.state), "{state:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }));
    let rid = id(&repositories[0]);
    let counts = read(&*store, |snapshot| {
        let published = snapshot.indexing().published(&rid)?.unwrap();
        let view = snapshot.indexing().view(&published.view_id)?.unwrap();
        snapshot.graph().graph_counts(&GraphScope {
            views: vec![GraphViewBinding {
                repository_id: rid.clone(),
                view_id: view.view_id,
                view_digest: view.view_digest,
            }],
        })
    });
    // Remove the source entirely: analysis must use the exact published composition.
    std::fs::remove_dir_all(&repo).unwrap();
    let result = graph(
        &app,
        &ctx,
        "code.architecture",
        json!({"kind":"Overview","repositoryId":rid}),
        100,
    );
    assert_eq!(result["value"]["result"]["kind"], "Architecture");
    let value = &result["value"]["result"]["value"];
    let nodes: u64 = value["nodeCounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["count"].as_str().unwrap().parse::<u64>().unwrap())
        .sum();
    assert_eq!(nodes, counts.nodes);
    assert_eq!(value["packageCount"], "0");
    assert!(
        value["hotspots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "leaf" && row["fanIn"] == "4"),
        "{result}"
    );
    assert!(value.get("summary").is_none());
    assert!(value.get("confidence").is_none());
    let result = graph(
        &app,
        &ctx,
        "code.repository_dependencies",
        json!({"kind":"RepositoryDependencies","repositoryId":rid}),
        100,
    );
    let semantic = &result["value"]["result"]["value"]["rustSemantic"];
    assert_eq!(semantic["status"], "not_analyzed");
    for field in [
        "manifest",
        "groupCount",
        "semanticGroups",
        "fallbackGroups",
        "skippedGroups",
    ] {
        assert!(semantic.get(field).is_none(), "{semantic}");
    }
    let result = graph(&app, &ctx, "code.overview", json!({"kind":"Overview"}), 100);
    assert_eq!(
        result["value"]["result"]["value"]["graphTotals"]["nodeCount"],
        counts.nodes.to_string()
    );
    assert_eq!(
        result["value"]["result"]["value"]["graphTotals"]["edgeCount"],
        counts.edges.to_string()
    );
    let result = graph(
        &app,
        &ctx,
        "code.graph_schema",
        json!({"kind":"Schema"}),
        100,
    );
    assert_eq!(result["value"]["result"]["value"]["origin"], "Observed");
    executor.block_on(app.shutdown());
    let _ = std::fs::remove_dir_all(src);
}

#[test]
fn declared_schema_and_metadata_overview_do_not_invent_empty_graph_measurements() {
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let app = executor
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let ctx = principal(&*store, "user", PrincipalKind::Human);
    let schema = graph(
        &app,
        &ctx,
        "code.graph_schema",
        json!({"kind":"Schema"}),
        100,
    );
    let page = &schema["value"];
    assert!(page.get("view").is_none());
    assert_eq!(page["result"]["value"]["origin"], "Declared");
    assert!(page["result"]["value"]["stamp"].is_object());
    assert!(
        page["result"]["value"]["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str().unwrap().contains("SwiftPM"))
    );
    let overview = graph(&app, &ctx, "code.overview", json!({"kind":"Overview"}), 100);
    let value = &overview["value"]["result"]["value"];
    assert_eq!(value["graphCoverage"]["state"], "Unavailable");
    assert!(value.get("graphTotals").is_none());
    assert_eq!(value["dependencies"], json!([]));
    assert_eq!(value["internalModules"], json!([]));
    executor.block_on(app.shutdown());
}
