//! Production index publication, analysis dispatch and SQLite reports through the native gateway.
use super::*;
use devworks_core::protocol::{
    DomainErrorCode, OperationState,
    records::{AnalysisAvailability, AnalysisMetricPageResult},
};

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
            &format!("create-{operation}"),
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
            &format!("run-{operation}"),
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
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "first"]);
    let app = executor
        .block_on(boot(composition(&h, store.clone(), "git")))
        .unwrap();
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

    let current = result(analysis_query(
        &restarted,
        &user,
        "analysis.health",
        read_body,
    ));
    let OperationResult::AnalysisHealth { value } = current else {
        panic!("{current:?}")
    };
    let trend = value.observation.unwrap().trends.unwrap().0;
    assert_eq!(trend.len(), 2, "{trend:?}");
    assert_eq!(trend[0].view.view_id, first);
    assert_eq!(trend[1].view.view_id, second);
    assert!(trend[0].computed_at_ms.get() < trend[1].computed_at_ms.get());
    run_maintenance(&executor, &restarted, &store, &user, views);
    executor.block_on(restarted.shutdown());
    std::fs::remove_dir_all(src).unwrap();
}
