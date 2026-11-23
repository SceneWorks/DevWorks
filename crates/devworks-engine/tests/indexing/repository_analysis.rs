//! Production index publication, analysis dispatch and SQLite reports through the native gateway.
use super::*;
use devworks_core::ports::indexing::{
    EdgeTarget, ExtractFault, ExtractedEdge, ExtractedNode, FileExtract, LanguageInfo, Resolution,
    StructuralParser,
};
use devworks_core::protocol::{
    DomainErrorCode, Id, OperationState,
    records::{AnalysisAvailability, AnalysisMetricPageResult},
};

/// Use the native manifest parsers and a controlled capture for the HTTP, message, SCIP and npm
/// inputs. The extraction boundary matches the native index worker's FileExtract contract.
struct LinkCapture {
    parser: TreeSitterParser,
}

const SCIP_SYMBOL: &str = "rust-analyzer cargo fixture_provider 1.0.0 fixture_provider/resolve().";

impl StructuralParser for LinkCapture {
    fn languages(&self) -> &[LanguageInfo] {
        self.parser.languages()
    }

    fn language_of(&self, path: &str) -> Option<&LanguageInfo> {
        self.parser.language_of(path)
    }

    fn extract(
        &self,
        language: &str,
        path: &str,
        repository: &str,
        source: &[u8],
        stop: &(dyn Fn() -> bool + Sync),
        budget_ms: u64,
    ) -> Result<FileExtract, ExtractFault> {
        let mut file = self
            .parser
            .extract(language, path, repository, source, stop, budget_ms)?;
        if path != "src/lib.rs" {
            return Ok(file);
        }
        let function = file
            .nodes
            .iter()
            .position(|node| node.name == "resolve_here")
            .expect("fixture function");
        let span = file.nodes[function].span;
        let npm_function = file
            .nodes
            .iter()
            .position(|node| node.name == "resolve_export")
            .expect("fixture npm export");
        let alias = json!({
            "package_name":"@fixture/provider", "import_path":"@fixture/provider/api",
            "export_name":"resolve", "member_path":""
        });
        if repository == "package-core" {
            file.nodes[function].properties.extend(
                json!({"source":"scip","scip_external":false,"scip_symbol":SCIP_SYMBOL})
                    .as_object()
                    .unwrap()
                    .clone(),
            );
            file.nodes[npm_function]
                .properties
                .insert("npm_exports".into(), json!([alias]));
        }
        let mut node = |kind: &str, name: &str, qualified: &str, properties: Value| {
            let index = file.nodes.len();
            file.nodes.push(ExtractedNode {
                kind: kind.into(),
                name: name.into(),
                qualified_name: qualified.into(),
                span,
                parent: Some(0),
                properties: properties.as_object().unwrap().clone(),
            });
            index
        };
        let mut edge = |kind: &str, target: EdgeTarget, properties: Value| {
            file.edges.push(ExtractedEdge {
                kind: kind.into(),
                source: function,
                target,
                resolution: Resolution::Resolved,
                confidence: 1.0,
                properties: properties.as_object().unwrap().clone(),
            });
        };
        let event = json!({"event_type":"Fixture.Events.Completed"});
        if repository == "package-core" || repository == "awaiting-index" {
            node(
                "Route",
                "fixture-item",
                "route:GET:/api/items/{id}",
                json!({"route_kind":"api","http_method":"GET","route_template":"/api/items/{id}"}),
            );
            if repository == "package-core" {
                edge("CONSUMES", EdgeTarget::Local(function), event);
            }
        } else if repository == "package-app" {
            let scip = node(
                "ExternalSymbol",
                "fixture-resolve",
                "scip:fixture-resolve",
                json!({"source":"scip","scip_external":true,"scip_symbol":SCIP_SYMBOL}),
            );
            let npm = node(
                "ExternalSymbol",
                "fixture-npm-resolve",
                "npm-symbol:@fixture/provider/api:resolve",
                alias,
            );
            edge("CALLS", EdgeTarget::Local(scip), json!({}));
            edge("CALLS", EdgeTarget::Local(npm), json!({}));
            edge("PUBLISHES", EdgeTarget::Local(function), event);
            edge(
                "HTTP_CALLS",
                EdgeTarget::Symbol("route:GET:/api/items/42".into()),
                json!({"url_pattern":"/api/items/42","http_method":"GET"}),
            );
        }
        Ok(file)
    }

    fn group_of(&self, path: &str) -> Option<&'static str> {
        self.parser.group_of(path)
    }

    fn extract_group(
        &self,
        group: &str,
        repository: &str,
        files: &[(String, Vec<u8>)],
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<std::collections::BTreeMap<String, FileExtract>, ExtractFault> {
        self.parser.extract_group(group, repository, files, stop)
    }
}

fn request_views(repositories: &[String]) -> Value {
    json!({"kind": "Published", "repositoryIds": repositories})
}

fn analysis_query(
    app: &Runtime,
    ctx: &InvocationContext,
    operation: &str,
    mut body: Value,
) -> DomainQueryReply {
    body["kind"] = json!(operation);
    let frame = json!({
        "protocol": "1.0-draft.1", "mode": "Query", "queryId": "analysis-read",
        "schemaRevision": 3, "body": body
    });
    app.query(ctx, operation, frame.to_string().as_bytes())
        .unwrap()
}

fn analysis_command(
    app: &Runtime,
    ctx: &InvocationContext,
    operation: &str,
    key: &str,
    mut body: Value,
) -> CommandReply<DomainDecision> {
    body["kind"] = json!(operation);
    let frame = json!({
        "protocol": "1.0-draft.1", "mode": "Command", "requestKey": key,
        "schemaRevision": 3, "body": body
    });
    app.command(ctx, operation, frame.to_string().as_bytes())
        .unwrap()
}

#[track_caller]
fn result(reply: DomainQueryReply) -> OperationResult {
    match reply {
        DomainQueryReply::Result { result, .. } => result,
        DomainQueryReply::Rejected { fault, .. } => panic!("{fault:?}"),
    }
}

#[track_caller]
fn accepted(reply: CommandReply<DomainDecision>) -> devworks_core::protocol::OperationId {
    match reply {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn wait_index(executor: &tokio::runtime::Runtime, store: &Arc<dyn ApplicationStore>, run: &str) {
    executor.block_on(within("index publishes an exact analysis input", async {
        loop {
            let state = run_state(store, run);
            if state.state == RUN_SUCCEEDED {
                break;
            }
            assert!(!run_terminal(&state.state), "{state:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }));
}

fn wait_analysis(
    executor: &tokio::runtime::Runtime,
    app: &Runtime,
    user: &InvocationContext,
    read_body: &Value,
) -> devworks_core::protocol::records::AnalysisRecord {
    executor.block_on(within("automatic analysis report", async {
        loop {
            let reply = analysis_query(app, user, "analysis.status", read_body.clone());
            if matches!(&reply, DomainQueryReply::Rejected { fault, .. }
                if fault.code == DomainErrorCode::ViewUnavailable)
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
            let value = result(reply);
            let OperationResult::AnalysisStatus { value } = value else {
                panic!("{value:?}")
            };
            if value.state == OperationState::ObservedComplete && value.report.is_some() {
                break value;
            }
            assert_ne!(value.state, OperationState::Failed, "{value:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }))
}

fn run_maintenance(
    executor: &tokio::runtime::Runtime,
    app: &Runtime,
    store: &Arc<dyn ApplicationStore>,
    user: &InvocationContext,
    views: Value,
) {
    let capabilities = app.jobs().catalog().capabilities();
    for operation in [
        "analysis.link_and_detect",
        "analysis.community_analysis",
        "analysis.process_batch",
    ] {
        let key = operation.replace('.', "-");
        let selected = capabilities
            .iter()
            .find(|cap| cap.operation == operation)
            .unwrap();
        let job_command = |name: &str, key: &str, mut body: Value| {
            body["kind"] = json!(name);
            let request = json!({"mode":"Command","protocol":"1.0-draft.1",
                "schemaRevision":2,"requestKey":key,"body":body});
            let reply = app
                .command(user, name, request.to_string().as_bytes())
                .unwrap();
            let CommandReply::Recorded { receipt, .. } = reply else {
                panic!("{reply:?}")
            };
            let DomainDecision::Applied { result } = receipt.decision else {
                panic!("{:?}", receipt.decision)
            };
            result
        };
        let created = job_command(
            "jobs.create",
            &format!("create-{key}"),
            json!({
                "scope":{"kind":"Workspace"}, "title":operation, "enabled":true,
                "schedule":{"kind":"Interval","seconds":"60"},
                "definition":{"kind":"Native","capabilityId":selected.capability_id,
                    "arguments":{"schema":selected.schema,"value":{
                        "scope":{"kind":"Workspace"}, "views":views, "expectedSourceRevision":"1",
                        "nodeIds":[], "batchLimit":8}}}
            }),
        );
        let OperationResult::JobsCreate { value: job } = created else {
            panic!("{created:?}")
        };
        let result = job_command(
            "jobs.run_now",
            &format!("run-{key}"),
            json!({
                "jobId":job.id, "expectedRevision":job.revision
            }),
        );
        let OperationResult::JobsRunNow { value: run } = result else {
            panic!("{result:?}")
        };
        executor.block_on(within("scheduled analysis completes", async {
            loop {
                let recorded = read(&**store, |read| read.jobs().job_run(&run.id)).unwrap();
                if recorded.status == "Succeeded" {
                    break;
                }
                assert!(
                    !matches!(recorded.status.as_str(), "Failed" | "OutcomeUnknown"),
                    "{operation}: {recorded:?}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }));
    }
}

#[test]
fn native_repository_analysis_survives_restart_and_keeps_provenance() {
    let _case = native_runtime_case();
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    let repo = src.join("analysis");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn can_run(n: u32) -> bool {\n    if n > 7 { return true; }\n    false\n}\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("src/client.ts"),
        "export const endpoint = \"http://fixture.test.invalid/status\";\n",
    )
    .unwrap();
    git(&repo, &["init", "-q"]);
    git(
        &repo,
        &[
            "commit",
            "-q",
            "--allow-empty",
            "--date=2023-01-01T00:00:00Z",
            "-m",
            "old repository root",
        ],
    );
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "recent source"]);
    let mut configured = composition(&h, store.clone(), "git");
    configured.use_cases = configured.use_cases.with_queue();
    let app = executor.block_on(boot(configured)).unwrap();
    let user = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &user, &src);
    let repositories = discovered(&app, &user);
    let views = request_views(&repositories);
    let read_body = json!({
        "scope": {"kind": "Workspace"}, "views": views, "page": {"limit": 1}, "filters": []
    });
    let no_data = result(analysis_query(
        &app,
        &user,
        "analysis.health",
        read_body.clone(),
    ));
    let OperationResult::AnalysisHealth { value } = no_data else {
        panic!("{no_data:?}")
    };
    assert!(matches!(
        value.observation.unwrap().availability,
        AnalysisAvailability::NoData
    ));

    let indexed = index(&app, &user, &repositories[0], "analysis-index");
    wait_index(&executor, &store, &indexed);
    let first = run_state(&store, &indexed).output_view_id.unwrap();
    let observed = wait_analysis(&executor, &app, &user, &read_body);
    let phases = observed.observation.unwrap().phases.unwrap().0;
    assert_eq!(phases.len(), 3, "{phases:?}");
    assert!(
        phases
            .iter()
            .all(|phase| phase.state == OperationState::ObservedComplete)
    );
    let run = read(&*store, |snapshot| {
        snapshot.analysis().analysis_run(&observed.id)
    })
    .unwrap();
    let vitals_report = run
        .phases
        .iter()
        .find(|phase| phase.phase.key() == "vitals")
        .and_then(|phase| phase.report.as_ref())
        .unwrap();
    let (bytes, _) = app
        .artifacts()
        .read_exact(&*store, &user, vitals_report, 1024 * 1024)
        .unwrap();
    let vitals: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        vitals["repositories"][0]["vitality"]["firstCommitAtMs"],
        json!(1_672_531_200_000_i64)
    );

    let mut file = read_body.clone();
    file["path"] = json!("src/lib.rs");
    let measured = result(analysis_query(
        &app,
        &user,
        "analysis.file_hotspot",
        file.clone(),
    ));
    let OperationResult::AnalysisFileHotspot {
        value: AnalysisMetricPageResult::Current(page),
    } = measured
    else {
        panic!("{measured:?}")
    };
    assert_eq!(page.found, Some(true));
    assert_eq!(page.items.0[0].view_id, first);
    assert!(
        page.items.0[0]
            .file_metrics
            .as_ref()
            .unwrap()
            .branches
            .get()
            > 0
    );
    let run_id = observed.id;

    let metrics = result(analysis_query(
        &app,
        &user,
        "analysis.metrics",
        read_body.clone(),
    ));
    let OperationResult::AnalysisMetrics {
        value: AnalysisMetricPageResult::Current(metrics),
    } = metrics
    else {
        panic!("{metrics:?}");
    };
    assert!(metrics.has_more);
    let mut next_page = read_body.clone();
    next_page["page"]["cursor"] = json!(metrics.next_cursor.unwrap());
    let next = result(analysis_query(
        &app,
        &user,
        "analysis.metrics",
        next_page.clone(),
    ));
    assert!(
        matches!(next, OperationResult::AnalysisMetrics { value: AnalysisMetricPageResult::Current(next) }
        if next.items.0.len() == 1 && next.items.0[0].path.as_ref().map(|path| &path.0)
            != metrics.items.0[0].path.as_ref().map(|path| &path.0) && !next.has_more)
    );
    assert!(
        matches!(analysis_query(&app, &user, "analysis.hotspots", next_page), DomainQueryReply::Rejected { fault, .. }
        if fault.code == DomainErrorCode::CursorExpired)
    );
    let hotspots = result(analysis_query(
        &app,
        &user,
        "analysis.hotspots",
        read_body.clone(),
    ));
    assert!(
        matches!(hotspots, OperationResult::AnalysisHotspots { value: AnalysisMetricPageResult::Current(page) }
        if page.items.0[0].path.as_ref().map(|path| &path.0)
            == metrics.items.0[0].path.as_ref().map(|path| &path.0) && page.items.0[0].view_id == first)
    );
    let security = result(analysis_query(
        &app,
        &user,
        "analysis.security",
        read_body.clone(),
    ));
    assert!(
        matches!(&security, OperationResult::AnalysisSecurity { value }
        if value.report.is_some() && value.observation.as_ref().and_then(|item| item.security.as_ref()).is_some_and(|rows|
           rows.0.len() == 1 && rows.0[0].view_id == first && rows.0[0].scanned_files.get() == 1
           && rows.0[0].medium.get() == 1)),
        "{security:?}"
    );
    assert!(
        !serde_json::to_string(&security)
            .unwrap()
            .contains("fixture.test.invalid")
    );

    let read_fleet = result(analysis_query(
        &app,
        &user,
        "analysis.fleet_health",
        read_body.clone(),
    ));
    let OperationResult::AnalysisFleetHealth {
        value: AnalysisMetricPageResult::Current(fleet),
    } = read_fleet
    else {
        panic!("{read_fleet:?}")
    };
    assert_eq!(fleet.fleet.unwrap().repository_count.get(), 1);
    let mut threshold = read_body.clone();
    threshold["maxHealth"] = json!(0);
    let filtered = result(analysis_query(
        &app,
        &user,
        "analysis.fleet_health",
        threshold,
    ));
    assert!(matches!(filtered, OperationResult::AnalysisFleetHealth {
        value: AnalysisMetricPageResult::Current(page)
    } if page.items.0.is_empty()
        && page.fleet.as_ref().is_some_and(|totals| totals.repository_count.get() == 0)
        && !matches!(page.observation.availability, AnalysisAvailability::NoData)));

    let mut bad_page = read_body.clone();
    bad_page["page"] = json!({});
    let fault = analysis_query(&app, &user, "analysis.health", bad_page);
    assert!(matches!(fault, DomainQueryReply::Rejected { .. }));
    let mut unknown = read_body.clone();
    unknown["views"] = request_views(&["missing-private-view".into()]);
    let fault = analysis_query(&app, &user, "analysis.health", unknown);
    assert!(matches!(fault, DomainQueryReply::Rejected { fault, .. }
        if fault.code == DomainErrorCode::ViewUnavailable));

    let command = json!({
        "scope": {"kind": "Workspace"}, "views": views, "expectedSourceRevision": "1",
        "nodeIds": [], "batchLimit": 8
    });
    let settled = analysis_command(
        &app,
        &user,
        "analysis.process_batch",
        "analysis-no-pending",
        command.clone(),
    );
    assert!(
        matches!(&settled, CommandReply::Recorded { receipt, .. }
        if matches!(receipt.decision, DomainDecision::NoChange { .. })),
        "{settled:?}"
    );

    app.dispatcher().stop();
    let mut link_only = command.clone();
    link_only["linkMode"] = json!("LinksOnly");
    let mut wrong_revision = link_only.clone();
    wrong_revision["expectedSourceRevision"] = json!("987");
    assert!(matches!(
        analysis_command(&app, &user, "analysis.link_and_detect", "wrong-link-revision", wrong_revision),
        CommandReply::Recorded { receipt, .. } if matches!(receipt.decision, DomainDecision::Refused { .. })
    ));
    let pending = accepted(analysis_command(
        &app,
        &user,
        "analysis.link_and_detect",
        "links-only",
        link_only.clone(),
    ));
    let (cancelled_run, delivery) = read(&*store, |r| {
        let op = r.operations().operation(&pending)?.unwrap();
        let run_id: Id = serde_json::from_value(op.payload["runId"].clone()).unwrap();
        let id: Id = serde_json::from_value(op.payload["deliveryId"].clone()).unwrap();
        Ok((run_id, r.queue().entry(&id)?.unwrap()))
    });
    let frame = json!({"mode":"Command","protocol":"1.0-draft.1","requestKey":"cancel-link-delivery",
        "body":{"kind":"queue.resolve","deliveryId":delivery.id,"expectedRevision":delivery.revision.to_string(),
            "expectedFence":delivery.generation.to_string(),"resolution":"CancelBeforeDispatch","reason":"native fixture"}});
    let cancelled = app
        .command(&user, "queue.resolve", frame.to_string().as_bytes())
        .unwrap();
    assert!(
        matches!(&cancelled, CommandReply::Recorded { receipt, .. } if matches!(receipt.decision, DomainDecision::Applied { .. })),
        "{cancelled:?}"
    );
    let saved = read(&*store, |r| r.analysis().analysis_run(&cancelled_run)).unwrap();
    assert_eq!(saved.state, OperationState::CancelledBeforeDispatch);
    assert_eq!(saved.phases.len(), 1);
    assert_eq!(
        saved.phases[0].state,
        OperationState::CancelledBeforeDispatch
    );

    link_only["resumeRunId"] = json!(cancelled_run);
    let resumed = accepted(analysis_command(
        &app,
        &user,
        "analysis.link_and_detect",
        "resume-links-only",
        link_only.clone(),
    ));
    assert_ne!(resumed, pending);
    assert_eq!(
        accepted(analysis_command(
            &app,
            &user,
            "analysis.link_and_detect",
            "resume-links-only",
            link_only
        )),
        resumed
    );

    // Restart the runtime over the persisted store. Uncommitted file contents cannot alter a saved report.
    std::fs::write(repo.join("src/lib.rs"), "malformed uncommitted content").unwrap();
    executor.block_on(app.shutdown());
    drop(app);
    drop(store);
    h.wait_released();
    let store = h.open();
    let user = principal(&*store, "user", PrincipalKind::Human);
    let restarted = executor
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
    let mut resumed_read = read_body.clone();
    resumed_read["runId"] = json!(cancelled_run);
    let resumed = wait_analysis(&executor, &restarted, &user, &resumed_read);
    assert_eq!(resumed.observation.unwrap().phases.unwrap().0.len(), 1);
    let after = result(analysis_query(
        &restarted,
        &user,
        "analysis.file_hotspot",
        file,
    ));
    let OperationResult::AnalysisFileHotspot {
        value: AnalysisMetricPageResult::Current(after),
    } = after
    else {
        panic!("{after:?}")
    };
    assert_eq!(after.items.0[0].view_id, first);
    assert_eq!(
        after.items.0[0].metric.value.get(),
        page.items.0[0].metric.value.get()
    );
    let stored = read(&*store, |s| s.analysis().analysis_run(&run_id)).unwrap();
    assert_eq!(stored.state, OperationState::ObservedComplete);
    assert_eq!(stored.phases.len(), 3);
    // A newer published commit must not alter the old report's score, binding or time.
    h.advance(1000);
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn can_run(_: u32) -> bool { true }\n",
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "second"]);
    let indexed = index(&restarted, &user, &repositories[0], "analysis-index-second");
    wait_index(&executor, &store, &indexed);
    let second = run_state(&store, &indexed).output_view_id.unwrap();
    assert_ne!(first, second);
    let latest = wait_analysis(&executor, &restarted, &user, &read_body);
    assert_ne!(latest.id, run_id);
    let mut historical = read_body.clone();
    historical["runId"] = json!(run_id);
    let historic = result(analysis_query(
        &restarted,
        &user,
        "analysis.health",
        historical,
    ));
    let OperationResult::AnalysisHealth { value } = historic else {
        panic!("{historic:?}")
    };
    let saved = value.observation.unwrap();
    assert!(matches!(saved.availability, AnalysisAvailability::Stale));
    assert_eq!(saved.bindings.0[0].view_id, first);
    assert_eq!(saved.indexed_bindings.unwrap().0[0].view_id, second);

    let mut old_archival = read_body.clone();
    old_archival["views"] = json!({"kind":"Exact", "viewIds":[first]});
    let stale_candidates = result(analysis_query(
        &restarted,
        &user,
        "analysis.archival_candidates",
        old_archival,
    ));
    assert!(
        matches!(stale_candidates, OperationResult::AnalysisArchivalCandidates {
        value: AnalysisMetricPageResult::Current(page)
    } if page.items.0.is_empty() && matches!(page.observation.availability, AnalysisAvailability::Stale))
    );

    let current = result(analysis_query(
        &restarted,
        &user,
        "analysis.health",
        read_body.clone(),
    ));
    let OperationResult::AnalysisHealth { value } = current else {
        panic!("{current:?}")
    };
    let trend = value.observation.unwrap().trends.unwrap().0;
    assert_eq!(trend.len(), 2, "{trend:?}");
    assert_eq!(trend[0].view.view_id, first);
    assert_eq!(trend[1].view.view_id, second);
    assert!(trend[0].computed_at_ms.get() < trend[1].computed_at_ms.get());
    let binding = read(&*store, |s| s.analysis().analysis_run(&latest.id))
        .unwrap()
        .binding_key;
    let counts = || {
        read(&*store, |s| {
            s.analysis().analysis_runs(&binding, false, None, 64)
        })
        .len()
    };
    let before = counts();
    run_maintenance(&executor, &restarted, &store, &user, views);
    assert_eq!(
        counts(),
        before + 1,
        "community maintenance should reuse the matching link-and-detect run"
    );
    let clusters = result(analysis_query(
        &restarted,
        &user,
        "analysis.clusters",
        read_body,
    ));
    assert!(matches!(clusters, OperationResult::AnalysisClusters {
        value: devworks_core::protocol::records::AnalysisClusterPageResult::Current(page)
    } if page.items.0.is_empty() && page.observation.phases.as_ref().is_some_and(|phases|
        phases.0.iter().any(|phase| phase.state == OperationState::ObservedComplete))));
    executor.block_on(restarted.shutdown());
    std::fs::remove_dir_all(src).unwrap();
}

#[test]
fn native_repository_analysis_links_seven_passes_and_keeps_missing_cluster_members() {
    use devworks_core::protocol::records::AnalysisClusterPageResult;
    let _case = native_runtime_case();
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    let writer = |name: &str, manifest: &str, extra: &[(&str, &str)]| {
        let repo = src.join(name);
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("Cargo.toml"), manifest).unwrap();
        std::fs::write(
            repo.join("src/lib.rs"),
            "pub fn resolve_here() {}\npub fn resolve_export() {}\n",
        )
        .unwrap();
        for (path, source) in extra {
            std::fs::write(repo.join(path), source).unwrap();
        }
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "candidate"]);
    };
    writer(
        "package-core",
        "[package]\nname = \"package-core\"\nversion = \"0.1.0\"\n",
        &[(
            "Package.swift",
            "import PackageDescription\nlet package = Package(name: \"package-core\")\n",
        )],
    );
    writer(
        "package-app",
        "[package]\nname = \"package-app\"\nversion = \"0.1.0\"\n[dependencies]\npackage-core = { path = \"../package-core\" }\n",
        &[
            (
                "Package.swift",
                "import PackageDescription\nlet package = Package(name: \"package-app\", dependencies: [.package(path: \"../package-core\")])\n",
            ),
            (
                "App.csproj",
                r#"<Project Sdk="Microsoft.NET.Sdk"><ItemGroup><PackageReference Include="package-core.Contracts" Version="1.2.3" /></ItemGroup></Project>"#,
            ),
        ],
    );
    writer(
        "awaiting-index",
        "[package]\nname = \"awaiting-index\"\nversion = \"0.1.0\"\n",
        &[],
    );
    let mut composition = composition(&h, store.clone(), "git");
    composition.indexing.as_mut().unwrap().parser = Arc::new(LinkCapture {
        parser: TreeSitterParser::new(),
    });
    let app = executor.block_on(boot(composition)).unwrap();
    let user = principal(&*store, "user", PrincipalKind::Human);
    base_directory(&store, &user, &src);
    let repositories = discovered(&app, &user);
    let typed = repositories
        .iter()
        .map(|repository| id(repository))
        .collect::<Vec<_>>();
    let registered = read(&*store, |read| {
        typed
            .iter()
            .map(|repository| read.projects().repository(repository))
            .collect::<Result<Vec<_>, _>>()
    });
    let lookup = |name: &str| {
        registered
            .iter()
            .flatten()
            .find(|row| row.name == name)
            .unwrap()
            .id
            .clone()
    };
    let core = lookup("package-core");
    let app_id = lookup("package-app");
    let missing = lookup("awaiting-index");
    let not_indexed = json!({
        "scope":{"kind":"Workspace"}, "views":{"kind":"Published","repositoryIds":[missing]},
        "page":{"limit":1}, "filters":[], "path":"src/lib.rs"
    });
    let empty = result(analysis_query(
        &app,
        &user,
        "analysis.file_hotspot",
        not_indexed,
    ));
    assert!(matches!(empty, OperationResult::AnalysisFileHotspot {
        value: AnalysisMetricPageResult::Current(page)
    } if page.found == Some(false) && matches!(page.observation.availability, AnalysisAvailability::NoData)));
    for (repository, request) in [(&core, "index-core"), (&app_id, "index-app")] {
        let run = index(&app, &user, repository.as_str(), request);
        wait_index(&executor, &store, &run);
    }
    let request = json!({
        "scope":{"kind":"Workspace"}, "views":request_views(&repositories), "page":{"limit":50}, "filters":[]
    });
    let observed = wait_analysis(&executor, &app, &user, &request);
    assert_eq!(observed.observation.unwrap().bindings.0.len(), 2);
    let links = result(analysis_query(
        &app,
        &user,
        "analysis.cross_repo_links",
        request.clone(),
    ));
    let OperationResult::AnalysisCrossRepoLinks {
        value: AnalysisMetricPageResult::Current(links),
    } = links
    else {
        panic!("{links:?}")
    };
    let link = links
        .items
        .0
        .iter()
        .filter_map(|item| item.link.as_ref())
        .find(|link| {
            link.source.repository_id == app_id
                && link
                    .target
                    .as_ref()
                    .is_some_and(|target| target.repository_id == core)
        })
        .expect("the exact path dependency links the two captured repository views");
    let target = link.target.as_ref().unwrap();
    assert_ne!(link.source.view_id, target.view_id);
    let actual: Vec<_> = links
        .items
        .0
        .iter()
        .filter_map(|item| item.link.as_ref())
        .filter(|link| {
            link.source.repository_id == app_id
                && link
                    .target
                    .as_ref()
                    .is_some_and(|target| target.repository_id == core)
        })
        .map(|link| (link.relationship.0.as_str(), link.matched_by.0.as_str()))
        .collect();
    for expected in [
        ("HTTP_CALLS", "HttpRoute"),
        ("PUBLISHES", "MessageType"),
        ("REFERENCES_PACKAGE", "NuGetPackageName"),
        ("CALLS", "ScipSymbol"),
        ("CALLS", "NpmExport"),
    ] {
        assert!(
            actual.contains(&expected),
            "missing {expected:?}: {actual:?}"
        );
    }
    assert_eq!(
        actual
            .iter()
            .filter(|row| **row == ("REFERENCES_PACKAGE", "selected-view-package-manifest"))
            .count(),
        2,
        "Cargo and SwiftPM: {actual:?}"
    );

    let graph_views = request_views(&[core.as_str().to_owned(), app_id.as_str().to_owned()]);
    let impact = json!({
        "protocol":"1.0-draft.1", "schemaRevision":3, "queryId":"analysis-graph-impact",
        "scope":{"kind":"Workspace"}, "view":graph_views,
        "limits":{"maxResults":100,"maxVisited":100000,"deadlineMs":30000},
        "query":{"kind":"NodeImpact","selection":{"target":{"kind":"Name","name":"resolve_export","repositoryId":core},"depth":1}}
    });
    let impact = result(
        app.query(&user, "code.impact", impact.to_string().as_bytes())
            .unwrap(),
    );
    let OperationResult::CodeImpact { value: impact } = impact else {
        panic!("{impact:?}");
    };
    let devworks_core::protocol::data::GraphResult::Impact { value: impact } = impact.result else {
        panic!("{impact:?}");
    };
    assert!(
        impact
            .affected
            .0
            .iter()
            .any(|node| node.node.repository_id == app_id && node.edge_kind.0 == "CALLS"),
        "{impact:?}"
    );

    let mut graph_request = request.clone();
    graph_request["projection"] = json!("Graph");
    let graph = result(analysis_query(
        &app,
        &user,
        "analysis.clusters",
        graph_request,
    ));
    let OperationResult::AnalysisClusters {
        value: AnalysisClusterPageResult::Current(graph),
    } = graph
    else {
        panic!("{graph:?}")
    };
    let edge = graph
        .graph_edges
        .unwrap()
        .0
        .into_iter()
        .find(|edge| edge.source_repository_id == app_id && edge.target_repository_id == core)
        .expect("directed cluster edge");
    assert!(edge.edge_count.get() >= 1);
    assert_eq!(edge.target_view_id, target.view_id);
    let cluster = graph
        .items
        .0
        .first()
        .expect("connected pair")
        .cluster
        .clone();
    let mut detail = request.clone();
    detail["clusterId"] = json!(cluster.id);
    detail["legacyOrdinal"] = json!(cluster.details.unwrap().legacy_ordinal);
    let got = result(analysis_query(
        &app,
        &user,
        "analysis.cluster_detail",
        detail,
    ));
    assert!(
        matches!(got, OperationResult::AnalysisClusterDetail { value }
        if value.id == cluster.id && value.details.as_ref().is_some_and(|details| details.members.0.len() == 2))
    );

    let archival = result(analysis_query(
        &app,
        &user,
        "analysis.archival_candidates",
        request.clone(),
    ));
    assert!(
        matches!(archival, OperationResult::AnalysisArchivalCandidates {
        value: AnalysisMetricPageResult::Current(page)
    } if page.items.0.is_empty() && !page.observation.diagnostics.0.is_empty())
    );
    let run = index(&app, &user, missing.as_str(), "index-second-route");
    wait_index(&executor, &store, &run);
    assert_eq!(
        wait_analysis(&executor, &app, &user, &request)
            .observation
            .unwrap()
            .bindings
            .0
            .len(),
        3
    );
    let after = result(analysis_query(
        &app,
        &user,
        "analysis.cross_repo_links",
        request,
    ));
    let OperationResult::AnalysisCrossRepoLinks {
        value: AnalysisMetricPageResult::Current(after),
    } = after
    else {
        panic!("{after:?}");
    };
    let http: Vec<_> = after
        .items
        .0
        .iter()
        .filter_map(|item| item.link.as_ref())
        .filter(|link| link.relationship.0 == "HTTP_CALLS" && link.source.repository_id == app_id)
        .collect();
    assert_eq!(http.len(), 1, "{http:?}");
    assert!(http[0].target.is_none());
    assert_eq!(http[0].matched_by.0, "AmbiguousRoute");
    executor.block_on(app.shutdown());
    std::fs::remove_dir_all(src).unwrap();
}
