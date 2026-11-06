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
    std::fs::write(
        repo.join("README.md"),
        "# Exact captured README\n\nThis survives source removal.\n",
    )
    .unwrap();
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
    let row = &result["value"]["result"]["value"]["repositories"][0];
    assert!(row.get("foundational").is_none());
    assert_eq!(row["readmeSource"]["path"], "README.md");
    assert_eq!(row["readmeSource"]["snapshot"]["repositoryId"], json!(rid));
    assert!(
        result["value"]["view"]["views"]
            .as_array()
            .unwrap()
            .iter()
            .any(|view| view["snapshot"] == row["readmeSource"]["snapshot"])
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

/// A captured, sealed index payload models an already recorded analysis producer. This
/// qualifies the read projection on real SQLite, not a producer or model execution.
fn captured_modules(store: &Arc<dyn ApplicationStore>) {
    write(&**store, |tx| {
        let repository=id("recorded-repository");let generation=id("recorded-generation");let view=id("recorded-view");
        tx.projects().insert_repository(&StoredRepository{id:repository.clone(),location:"/captured/recorded".into(),name:"Recorded".into(),remote_url:None,primary:true,lifecycle:ACTIVE.into(),languages:vec!["csharp".into()],revision:1,registered_at_ms:1})?;
        tx.indexing().insert_generation(&StoredGeneration{id:generation.clone(),repository_id:repository.clone(),layer:LAYER_BASELINE.into(),state:GEN_STAGING.into(),source_digest:"a".repeat(40),watermark:0,manifest:json!({})})?;
        let key="Recorded.csproj";
        let endpoints:Vec<_>=(0..650).map(|i|json!({"route":format!("/recorded/{i}"),"httpMethod":"GET","description":"Actual recorded fixture endpoint"})).collect();
        let recorded=json!({"summary":"Recorded source summary","confidence":"high","endpoints":endpoints,"services":[],"externalDependencies":[],"databaseTables":[],"updatedAtMs":"17","modelUsed":"recorded-producer","programTotals":{"endpointCount":"999","serviceCount":"2","tableCount":"3"}});
        let mut nodes=vec![StoredNode{unit_key:key.into(),node_id:"module-recorded".into(),kind:"DotnetProject".into(),name:"Recorded".into(),payload:json!({"properties":{"dotnet_project":"Recorded","analysis":recorded}})}];
        for i in 0..601{nodes.push(StoredNode{unit_key:key.into(),node_id:format!("route-{i}"),kind:"Route".into(),name:format!("route-{i}"),payload:json!({"properties":{"dotnet_project":"Recorded"}})});}
        tx.indexing().put_unit(&StoredUnit{generation_id:generation.clone(),unit_key:key.into(),content_digest:"opaque-recorded-content".into(),action:UNIT_REPLACE.into(),payload:json!({})},&nodes,&[])?;
        tx.indexing().put_unit(&StoredUnit{generation_id:generation.clone(),unit_key:"Other.csproj".into(),content_digest:"opaque-other-content".into(),action:UNIT_REPLACE.into(),payload:json!({})},&[StoredNode{unit_key:"Other.csproj".into(),node_id:"module-other".into(),kind:"DotnetProject".into(),name:"Other".into(),payload:json!({"properties":{"dotnet_project":"Other"}})}],&[])?;
        tx.indexing().put_unit(&StoredUnit{generation_id:generation.clone(),unit_key:"/".into(),content_digest:"opaque-root-content".into(),action:UNIT_REPLACE.into(),payload:json!({})},&[StoredNode{unit_key:"/".into(),node_id:"root-recorded".into(),kind:"Repository".into(),name:"Recorded".into(),payload:json!({"properties":{"foundational":true,"sourceGroup":"captured-source-group","framework":"captured-framework"}})}],&[])?;
        tx.indexing().update_generation(&StoredGeneration{id:generation.clone(),repository_id:repository.clone(),layer:LAYER_BASELINE.into(),state:GEN_SEALED.into(),source_digest:"a".repeat(40),watermark:3,manifest:json!({})})?;
        tx.indexing().insert_view(&StoredView{view_id:view.clone(),repository_id:repository.clone(),view_digest:devworks_core::fingerprint::sha256_hex(b"recorded-view"),baseline_id:generation,branch_id:None,worktree_id:None,coverage:json!({"state":"Complete","commit":"a".repeat(40),"sourceRevision":1})})?;
        tx.indexing().publish(&repository,&view,0)?;
        tx.projects().publish_index_view(&StoredIndexView{view_id:view,repository_id:repository,source_revision:"a".repeat(40),coverage:"Complete".into(),published_at_ms:1})?;
        Ok(())
    }).unwrap();
}

#[test]
fn recorded_modules_page_full_semantic_arrays_and_indexed_drilldowns_without_fabricating_provenance()
 {
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    captured_modules(&store);
    let app = executor
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let ctx = principal(&*store, "user", PrincipalKind::Human);
    let base = json!({"protocol":"1.0-draft.1","schemaRevision":3,"queryId":"recorded-modules","scope":{"kind":"Workspace"},"view":{"kind":"Exact","viewIds":["recorded-view"]},"query":{"kind":"Overview","repositoryId":"recorded-repository"},"limits":{"maxResults":100,"maxVisited":100000,"deadlineMs":30000}});
    let invoke = |frame: &Value| match app
        .query(&ctx, "code.architecture", frame.to_string().as_bytes())
        .unwrap()
    {
        DomainQueryReply::Result { result, .. } => serde_json::to_value(result).unwrap(),
        DomainQueryReply::Rejected { fault, .. } => panic!("{fault:?}"),
    };
    let overview = match app
        .query(&ctx, "code.overview", base.to_string().as_bytes())
        .unwrap()
    {
        DomainQueryReply::Result { result, .. } => serde_json::to_value(result).unwrap(),
        DomainQueryReply::Rejected { fault, .. } => panic!("{fault:?}"),
    };
    let repository = &overview["value"]["result"]["value"]["repositories"][0];
    assert_eq!(repository["foundational"], true);
    assert_eq!(repository["sourceGroup"], "captured-source-group");
    assert_eq!(repository["framework"], "captured-framework");
    assert!(repository.get("readmeSource").is_none());
    let mut limited = base.clone();
    limited["limits"]["maxVisited"] = json!(1);
    let limited_result = invoke(&limited);
    assert_eq!(limited_result["value"]["completion"], "WorkLimit");
    assert!(
        limited_result["value"]["result"]["value"]
            .get("packageCount")
            .is_none()
    );
    assert_eq!(
        limited_result["value"]["result"]["value"]["packages"],
        json!([])
    );
    let mut frame = base.clone();
    let mut routes = std::collections::BTreeSet::new();
    let mut refs = std::collections::BTreeSet::new();
    let mut drill = None;
    let mut saw_unrecorded = false;
    for page_index in 0..30 {
        let result = invoke(&frame);
        let page = &result["value"];
        for module in page["result"]["value"]["internalModules"]
            .as_array()
            .unwrap()
        {
            if module["module"]["moduleName"] == "Other" {
                assert_eq!(module["state"], "NotAnalyzed");
                assert!(module.get("recorded").is_none());
                saw_unrecorded = true;
                continue;
            }
            assert_eq!(module["state"], "Recorded");
            assert_eq!(module["recorded"]["summary"], "Recorded source summary");
            assert_eq!(module["recorded"]["updatedAtMs"], "17");
            assert_eq!(module["recorded"]["programTotals"]["endpointCount"], "999");
            for field in ["sourceHash", "sourceRevision", "sourceViewId"] {
                assert!(module["recorded"].get(field).is_none());
            }
            assert_eq!(module["endpoints"]["count"], "601");
            for row in module["recorded"]["endpoints"].as_array().unwrap() {
                routes.insert(row["route"].as_str().unwrap().to_owned());
            }
            for row in module["endpoints"]["nodes"].as_array().unwrap() {
                refs.insert(row["nodeId"].as_str().unwrap().to_owned());
            }
            if drill.is_none() {
                drill = module["endpoints"]
                    .get("nextCursor")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
        }
        match page.get("nextCursor").and_then(Value::as_str) {
            Some(cursor) => {
                assert_eq!(page["completion"], "ResultLimit");
                frame["cursor"] = json!(cursor);
            }
            None => break,
        }
        assert!(page_index < 29, "continuation failed to advance");
    }
    assert_eq!(routes.len(), 650);
    assert_eq!(refs.len(), 601);
    assert!(saw_unrecorded);
    let mut nested = base;
    nested["cursor"] = json!(drill.expect("actual authenticated nested cursor"));
    let result = invoke(&nested);
    let modules = result["value"]["result"]["value"]["internalModules"]
        .as_array()
        .unwrap();
    assert_eq!(modules.len(), 1);
    assert!(
        !modules[0]["endpoints"]["nodes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(modules[0]["endpoints"]["nodes"].as_array().unwrap().len() <= 100);
    executor.block_on(app.shutdown());
}
