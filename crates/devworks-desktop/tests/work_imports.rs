//! The native workspace and shared gateway import sealed selected work without adopting it.
mod common;
use common::*;
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::storage::read_snapshot;
use devworks_core::protocol::{
    DomainErrorCode, Id, OperationState, ProjectId,
    records::{ImportRun, MutationSummary},
    results::OperationResult,
};
use devworks_desktop::{backend::Backend, ops};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

const SOURCE_REVISION: &str = "afe73560215bbe7593fc59451de5cc3237e3eeca";
const NAMESPACE: &str = "frozen-codegraph-fixture";
const PROJECT: &str = "10000000-0000-0000-0000-000000000001";
const FEATURE: &str = "10000000-0000-0000-0000-000000000002";
const ITEM: &str = "10000000-0000-0000-0000-000000000003";
const BLOCKER: &str = "10000000-0000-0000-0000-000000000004";
const NEIGHBOR: &str = "10000000-0000-0000-0000-000000000005";

fn export(path: &Path, captured: i64, title: &str) {
    let at = captured - 100;
    let stamp = json!({"author":"Original owner","authorKind":"user","authorSession":null,
        "createdAtMs":1786516201120_i64,"updatedAtMs":at});
    let item = |id, name, status, order| {
        json!({"kind":"Item","id":id,"projectId":PROJECT,
        "featureId":FEATURE,"number":order,"itemType":"task","title":name,"description":"",
        "status":status,"priority":"medium","order":order,"labels":[],"repositoryName":null,"stamp":stamp})
    };
    let rows = [
        json!({"kind":"Project","id":PROJECT,"key":"SRC","name":"Historical project",
            "description":"Fixture scope","goals":[],"stamp":stamp}),
        json!({"kind":"Feature","id":FEATURE,"projectId":PROJECT,"number":1,
            "title":"Historical feature","description":"","stamp":stamp}),
        item(ITEM, title, "ready", 1),
        item(BLOCKER, "Historical completed prerequisite", "complete", 2),
        item(NEIGHBOR, "Excluded neighboring card", "in_progress", 3),
        json!({"kind":"Dependency","id":"legacy-dependency","projectId":PROJECT,
            "blocked":ITEM,"blocker":BLOCKER,"createdAtMs":at}),
        json!({"kind":"History","id":"legacy-comment","projectId":PROJECT,"subjectId":ITEM,
            "historyKind":"Comment","stamp":stamp,"evidence":{"body":"Original historical comment"}}),
        json!({"kind":"History","id":"legacy-neighbor","projectId":PROJECT,"subjectId":NEIGHBOR,
            "historyKind":"Comment","stamp":stamp,"evidence":{"body":"Outside selection"}}),
    ];
    let mut body = Vec::new();
    let mut counts = BTreeMap::<String, usize>::new();
    for row in rows {
        *counts
            .entry(row["kind"].as_str().unwrap().to_owned())
            .or_default() += 1;
        serde_json::to_writer(&mut body, &row).unwrap();
        body.push(b'\n');
    }
    let mut capture = serde_json::to_vec(&json!({
        "format":"DevWorksCodeGraphWork","version":1,"namespace":NAMESPACE,
        "sourceRevision":SOURCE_REVISION,"capturedAtMs":captured,"coverageFromMs":0,
        "recordsSha256":sha256_hex(&body),"counts":counts,"missing":{},"selectedProjects":[PROJECT],
    }))
    .unwrap();
    capture.push(b'\n');
    capture.extend(body);
    std::fs::write(path, capture).unwrap();
}

fn config_body() -> Value {
    json!({"source":{"rootId":"work-import-inbox","relative":"source.ndjson"},
        "projectIds":[],"workIds":[ITEM,BLOCKER],"includeHistory":true,"includeGitLinks":true})
}

fn wire(b: &dyn Backend, operation: &str, revision: u32, body: Value, query: bool) -> Vec<u8> {
    use devworks_desktop::frame::fixture;
    let frame = if query {
        fixture::query(b, operation, body)
    } else {
        fixture::command(b, operation, body)
    };
    fixture::at_revision(frame, u64::from(revision))
}
fn query(
    b: &dyn Backend,
    op: &str,
    rev: u32,
    body: Value,
) -> Result<OperationResult, Box<ops::Failure>> {
    ops::query_result(b.query(op, &wire(b, op, rev, body, true)))
        .map(|(result, _)| result)
        .map_err(Box::new)
}

fn preview(b: &dyn Backend, cfg: &Value, revision: u32, since: Option<i64>) -> ImportRun {
    let mut input = json!({"config":cfg});
    if let Some(since) = since {
        input["mode"] = json!("Delta");
        input["updatedSinceMs"] = json!(since.to_string());
    }
    match query(b, "imports.preview", revision, input) {
        Ok(OperationResult::ImportsPreview { value }) => value,
        other => panic!("unexpected preview {other:?}"),
    }
}

fn approve(b: &dyn Backend, cfg: &Value, revision: u32) -> MutationSummary {
    let mut input = json!({"config":cfg});
    if revision > 1 {
        input["approve"] = json!(true);
    }
    let frame = wire(b, "imports.configure", revision, input, false);
    match ops::command_result(b.command("imports.configure", &frame)) {
        Ok(OperationResult::ImportsConfigure { value }) => value,
        other => panic!("unexpected configuration {other:?}"),
    }
}

fn apply_frame(b: &dyn Backend, cfg: &MutationSummary, preview: &ImportRun, rev: u32) -> Vec<u8> {
    wire(
        b,
        "imports.apply",
        rev,
        json!({
            "configId":cfg.id,"expectedRevision":cfg.revision,
            "sourceSnapshot":preview.source_snapshot,"previewDigest":preview.report.sha256,
        }),
        false,
    )
}

fn inspect(b: &dyn Backend, id: &Id) -> ImportRun {
    match query(b, "imports.inspect", 2, json!({"runId":id})) {
        Ok(OperationResult::ImportsInspect { value }) => value,
        other => panic!("unexpected inspect {other:?}"),
    }
}

fn completed(b: &dyn Backend, id: &Id) -> (ImportRun, Value) {
    let deadline = Instant::now() + SETTLE_BOUND;
    loop {
        let run = inspect(b, id);
        if matches!(
            run.state,
            OperationState::ObservedComplete
                | OperationState::Failed
                | OperationState::CancelledBeforeDispatch
                | OperationState::NeedsInput
        ) {
            let report = ops::read_artifact_bytes(b, &run.report).unwrap();
            let result = serde_json::from_slice::<Value>(&report).unwrap();
            assert_eq!(run.state, OperationState::ObservedComplete, "{result}");
            return (run, result);
        }
        assert!(Instant::now() < deadline, "import did not settle: {run:?}");
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn local(kind: &str, source: &str) -> Id {
    let key = sha256_hex(format!("{NAMESPACE}\0{kind}\0{source}").as_bytes());
    Id::new(format!("import-{}", &key[..48])).unwrap()
}

#[test]
fn native_v1_import_deduplicates_preserves_history_and_never_adopts_source_work() {
    let _case = native_runtime_case();
    let root = Root::new("selected-work-import");
    let engine = start(&root);
    let backend = engine.backend();
    let b = &*backend;
    let path = root.0.join("work-imports/source.ndjson");
    let captured = 1795300702715_i64;
    export(&path, captured, "Historical ready card");
    let config = config_body();
    let preview = preview(b, &config, 1, None);
    assert!(preview.preview_digest.is_none());
    assert_eq!(preview.state, OperationState::ObservedComplete);
    let cfg = approve(b, &config, 1);
    let frame = apply_frame(b, &cfg, &preview, 1);
    let run = ops::accepted_decision(b.command("imports.apply", &frame)).unwrap();
    let replay = ops::accepted_decision(b.command("imports.apply", &frame)).unwrap();
    assert_eq!(run, replay);
    let (accepted, report) = completed(b, &run.0);
    assert_eq!(accepted.imported_ids.0.len(), 6);
    assert_eq!(report["counts"]["added"], 6);
    assert_eq!(report["counts"]["failures"], 0);
    let work = local("Item", ITEM);
    let blocker = local("Item", BLOCKER);
    read_snapshot(
        &**engine.host().runtime().store(),
        &Default::default(),
        |r| {
            let record = r.work().work(&work)?.expect("selected ready card");
            let prerequisite = r.work().work(&blocker)?.expect("selected prerequisite");
            let project = ProjectId(local("Project", PROJECT));
            assert_eq!(record.project_id, project);
            assert_eq!(record.brief["importedHistory"]["status"], "ready");
            assert_eq!(prerequisite.brief["importedHistory"]["status"], "complete");
            assert!(record.completion.is_none());
            assert!(prerequisite.completion.is_none());
            let statuses = r.work().statuses(&project)?;
            for card in [&record, &prerequisite] {
                assert_eq!(
                    statuses
                        .iter()
                        .find(|s| Some(&s.id) == card.status_id.as_ref())
                        .map(|s| s.semantic.as_str()),
                    Some("Backlog")
                );
            }
            assert_eq!(r.work().prerequisites(&work)?, vec![blocker.clone()]);
            assert!(r.work().work(&local("Item", NEIGHBOR))?.is_none());
            Ok(())
        },
    )
    .unwrap();
    match query(
        b,
        "imports.history",
        1,
        json!({"workId":work,"page":{"limit":10}}),
    )
    .unwrap()
    {
        OperationResult::ImportsHistory { value } => {
            assert!(value.items.0.iter().any(|r| r.kind.0 == "Comment"
                && r.evidence.0["source"]["body"] == "Original historical comment"));
            assert!(
                !serde_json::to_string(&value)
                    .unwrap()
                    .contains("Outside selection")
            );
        }
        other => panic!("unexpected history {other:?}"),
    }
    let old = apply_frame(b, &cfg, &preview, 1);
    let rejected = ops::accepted_decision(b.command("imports.apply", &old));
    assert!(
        matches!(rejected, Err(ops::Failure::Refused(f)) if f.code == DomainErrorCode::SourceChanged)
    );
    let fresh = self::preview(b, &config, 1, None);
    let frame = apply_frame(b, &cfg, &fresh, 1);
    let rerun = ops::accepted_decision(b.command("imports.apply", &frame)).unwrap();
    let (_, report) = completed(b, &rerun.0);
    assert_eq!(report["counts"]["unchanged"], 6);
    export(&path, captured + 60_000, "Updated source title");
    let delta = self::preview(b, &config, 2, Some(captured - 30 * 60 * 1000));
    let frame = apply_frame(b, &cfg, &delta, 2);
    let updated = ops::accepted_decision(b.command("imports.apply", &frame)).unwrap();
    let (_, report) = completed(b, &updated.0);
    assert_eq!(report["counts"]["added"], 0);
    assert_eq!(report["counts"]["failures"], 0);
    let restored = read_snapshot(
        &**engine.host().runtime().store(),
        &Default::default(),
        |r| r.work().work(&work),
    )
    .unwrap()
    .unwrap();
    assert_eq!(restored.title, "Updated source title");
    export(&path, captured - 1000, "Stale source title");
    let stale = query(b, "imports.preview", 1, json!({"config":config})).unwrap_err();
    assert!(matches!(*stale, ops::Failure::Refused(f) if f.code == DomainErrorCode::SourceChanged));
}

#[test]
fn native_runtime_accepts_export_from_frozen_mariadb_schema_when_supplied() {
    let Ok(fixture) = std::env::var("DEVWORKS_CODEGRAPH_IMPORT_FIXTURE") else {
        return;
    };
    let _case = native_runtime_case();
    let root = Root::new("pinned-codegraph-export");
    let engine = start(&root);
    std::fs::copy(fixture, root.0.join("work-imports/source.ndjson")).unwrap();
    let backend = engine.backend();
    let b = &*backend;
    let config = json!({"source":{"rootId":"work-import-inbox","relative":"source.ndjson"},
        "projectIds":[PROJECT],"workIds":[],"includeHistory":true,"includeGitLinks":true});
    let preview = preview(b, &config, 1, None);
    let profile = approve(b, &config, 1);
    let run =
        ops::accepted_decision(b.command("imports.apply", &apply_frame(b, &profile, &preview, 1)))
            .unwrap();
    let (observed, report) = completed(b, &run.0);
    assert!(observed.imported_ids.0.contains(&local("Item", ITEM)));
    assert_eq!(report["counts"]["failures"], 0);
    let project = match query(
        b,
        "imports.history",
        1,
        json!({"projectId":local("Project",PROJECT),"page":{"limit":20}}),
    )
    .unwrap()
    {
        OperationResult::ImportsHistory { value } => value.items.0,
        other => panic!("unexpected project history {other:?}"),
    };
    assert!(project.iter().any(|row| row.kind.0 == "Session"
        && row.evidence.0["source"]["transcriptAvailability"].is_string()));
    let history = match query(
        b,
        "imports.history",
        1,
        json!({"workId":local("Item",ITEM),"page":{"limit":20}}),
    )
    .unwrap()
    {
        OperationResult::ImportsHistory { value } => value.items.0,
        other => panic!("unexpected work history {other:?}"),
    };
    for kind in [
        "Attachment",
        "Attempt",
        "GitEvidence",
        "Handoff",
        "ItemThread",
    ] {
        assert!(
            history.iter().any(|row| row.kind.0 == kind),
            "missing {kind}"
        );
    }
    assert!(
        !serde_json::to_string(&history)
            .unwrap()
            .contains("PRIVATE-DIRECT-MESSAGE")
    );
}

#[test]
fn native_authorized_job_runs_delta_for_same_owner_and_scope() {
    use devworks_core::protocol::records::{JobDefinition, Schedule};
    use devworks_core::registry::SchemaRevision;
    use devworks_core::services::jobs::{argument_schema, capability_id};
    let _case = native_runtime_case();
    let root = Root::new("scheduled-work-import");
    let engine = start(&root);
    let backend = engine.backend();
    let b = &*backend;
    let path = root.0.join("work-imports/source.ndjson");
    let captured = 1795300702715_i64;
    export(&path, captured, "Historical ready card");
    let mut config = config_body();
    config["allowScheduledDelta"] = json!(true);
    let reviewed = preview(b, &config, 2, None);
    let cfg = approve(b, &config, 2);
    let original =
        ops::accepted_decision(b.command("imports.apply", &apply_frame(b, &cfg, &reviewed, 2)))
            .unwrap();
    completed(b, &original.0);
    let direct = wire(
        b,
        "imports.sync",
        2,
        json!({"configId":cfg.id,"expectedRevision":cfg.revision}),
        false,
    );
    assert!(
        matches!(ops::accepted_decision(b.command("imports.sync", &direct)),
        Err(ops::Failure::Refused(f)) if f.code == DomainErrorCode::PermissionDenied)
    );
    export(&path, captured + 80_000, "Scheduled source update");
    let definition: JobDefinition = serde_json::from_value(json!({
        "kind":"Native","capabilityId":capability_id("imports.sync"),
        "arguments":{"schema":argument_schema("imports.sync", SchemaRevision(2)).unwrap(),
            "value":{"configId":cfg.id,"expectedRevision":cfg.revision}}
    }))
    .unwrap();
    let create = ops::job_create(
        b,
        None,
        "Selected source delta",
        &Schedule::Manual {},
        &definition,
        true,
    );
    let job = match ops::job_changed(b.command("jobs.create", &create)) {
        Ok(ops::JobChange::Job(job)) => job,
        Err(error) => panic!("job creation failed: {}", error.describe()),
        _ => panic!("expected qualifying native job"),
    };
    let run_now = ops::job_command(b, ops::JobCommand::RunNow, &job);
    let schedule = match ops::job_changed(b.command("jobs.run_now", &run_now)) {
        Ok(ops::JobChange::Run(run)) => run,
        Err(error) => panic!("manual job run failed: {}", error.describe()),
        _ => panic!("expected an accepted job run"),
    };
    let deadline = Instant::now() + SETTLE_BOUND;
    let effect = loop {
        let run = read_snapshot(
            &**engine.host().runtime().store(),
            &Default::default(),
            |r| r.jobs().job_run(&schedule.id),
        )
        .unwrap()
        .unwrap();
        if matches!(
            run.status.as_str(),
            "Succeeded" | "Failed" | "Cancelled" | "Unknown"
        ) {
            assert_eq!(run.status, "Succeeded", "{run:?}");
            break run
                .inner_operation_id
                .expect("native job linked its imported operation");
        }
        assert!(
            Instant::now() < deadline,
            "schedule held unexpectedly: {run:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    let (_, result) = completed(b, &effect.0);
    assert_eq!(result["mode"], "Delta");
    assert_eq!(result["updatedSinceMs"], captured - 30 * 60_000);
    let updated = read_snapshot(
        &**engine.host().runtime().store(),
        &Default::default(),
        |read| read.work().work(&local("Item", ITEM)),
    )
    .unwrap()
    .unwrap();
    assert_eq!(updated.title, "Scheduled source update");
}

#[test]
fn historical_code_location_resolves_only_against_an_explicit_native_index() {
    use devworks_core::ports::storage::{ConfigScope, StorageError, WriteOptions, run_write};
    use std::process::Command;

    let _case = native_runtime_case();
    let root = Root::new("indexed-work-import");
    let sources = root.dir("import-index-sources");
    let repo = sources.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn retained_evidence() {}\n").unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(&repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Import Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Import Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "--quiet"]);
    git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/example/legacy-import-fixture.git",
    ]);
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "Unchanging source"]);
    let commit = git(&["rev-parse", "HEAD"]);

    let engine = start(&root);
    let backend = engine.backend();
    let b = &*backend;
    let registration = {
        let d = devworks_core::settings::catalog::descriptor("repositories.baseDirectory").unwrap();
        let assignment = serde_json::from_value(json!({
            "key":"repositories.baseDirectory",
            "value":{"schema":d.schema_ref(),"value":sources},
        }))
        .unwrap();
        let user = engine.host().user().clone();
        run_write(
            &**engine.host().runtime().store(),
            WriteOptions::fixture(),
            move |tx| {
                devworks_core::settings::service::apply_human_assignments(
                    tx,
                    &user,
                    &ConfigScope::Workspace,
                    &[assignment],
                    json!({}),
                )
                .map_err(|error| StorageError::InvalidRecord(format!("{error:?}")))
            },
        )
        .unwrap();
        let (discovered, _) =
            ops::discover_result(b.query("indexing.discover", &ops::discover(b))).unwrap();
        assert_eq!(discovered.len(), 1, "{discovered:?}");
        discovered[0].id.clone()
    };

    let path = root.0.join("work-imports/source.ndjson");
    let captured = 1795300702715_i64;
    export(&path, captured, "Historical ready card");
    let original = std::fs::read_to_string(&path).unwrap();
    let (header, body) = original.split_once('\n').unwrap();
    let mut header: Value = serde_json::from_str(header).unwrap();
    let mut body = body.as_bytes().to_vec();
    let remote = "git@github.com:example/legacy-import-fixture.git";
    let evidence = |id, path| {
        json!({
            "kind":"History","id":id,"projectId":PROJECT,"subjectId":ITEM,"historyKind":"CodeReference",
            "stamp":{"author":"Source","authorKind":"user","authorSession":null,
                "createdAtMs":captured - 200,"updatedAtMs":captured - 200},
            "evidence":{"remote":remote,"commit":commit,"path":path,"sourceSpan":"L1"}
        })
    };
    for record in [
        json!({"kind":"Repository","id":"legacy-repo","projectId":PROJECT,
            "name":"historic repo","remote":remote,"role":"owned"}),
        evidence("legacy-matched", "src/lib.rs"),
        evidence("legacy-missing", "src/missing.rs"),
    ] {
        let kind = record["kind"].as_str().unwrap();
        header["counts"][kind] = json!(header["counts"][kind].as_u64().unwrap_or(0) + 1);
        serde_json::to_writer(&mut body, &record).unwrap();
        body.push(b'\n');
    }
    header["recordsSha256"] = json!(sha256_hex(&body));
    let mut joined = serde_json::to_vec(&header).unwrap();
    joined.push(b'\n');
    joined.extend(body);
    std::fs::write(&path, joined).unwrap();

    let cfg = config_body();
    let reviewed = preview(b, &cfg, 1, None);
    let profile = approve(b, &cfg, 1);
    let run =
        ops::accepted_decision(b.command("imports.apply", &apply_frame(b, &profile, &reviewed, 1)))
            .unwrap();
    let (_, report) = completed(b, &run.0);
    assert_eq!(report["counts"]["added"], 9);
    let find = || match query(
        b,
        "imports.history",
        1,
        json!({"workId":local("Item", ITEM),"page":{"limit":10}}),
    )
    .unwrap()
    {
        OperationResult::ImportsHistory { value } => value.items.0,
        other => panic!("unexpected history {other:?}"),
    };
    assert!(find().iter().all(|row| row.current_reference.is_none()));
    let revision = read_snapshot(
        &**engine.host().runtime().store(),
        &Default::default(),
        |read| {
            Ok(read
                .indexing()
                .source_config("workspace")?
                .map_or(1, |c| c.revision))
        },
    )
    .unwrap();
    ops::index_run_result(b.command(
        "indexing.index",
        &ops::index_repository(b, registration.as_str(), false, revision),
    ))
    .unwrap();
    let deadline = Instant::now() + SETTLE_BOUND;
    let view = loop {
        if let Some(published) = read_snapshot(
            &**engine.host().runtime().store(),
            &Default::default(),
            |read| read.indexing().published(&registration),
        )
        .unwrap()
        {
            break published.view_id;
        }
        assert!(
            Instant::now() < deadline,
            "the explicit index never published"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    let rows = find();
    let matched = rows
        .iter()
        .find(|r| r.source_id.as_str() == "legacy-matched")
        .unwrap();
    let reference = matched
        .current_reference
        .as_ref()
        .expect("current exact indexed location");
    assert_eq!(reference.view_id, view);
    assert_eq!(reference.repository_id, registration);
    assert_eq!(reference.path.0, "src/lib.rs");
    let missing = rows
        .iter()
        .find(|r| r.source_id.as_str() == "legacy-missing")
        .unwrap();
    assert!(missing.current_reference.is_none());
    assert_eq!(missing.evidence.0["source"]["path"], "src/missing.rs");
}

#[test]
fn staged_import_survives_source_replacement_and_a_queued_stop_survives_restart() {
    let _case = native_runtime_case();
    let root = Root::new("work-import-recovery");
    let captured = 1795300702715_i64;
    let path = root.0.join("work-imports/source.ndjson");
    let engine = start(&root);
    let backend = engine.backend();
    let b = &*backend;
    let config = config_body();

    export(&path, captured, "The reviewed source");
    let reviewed = preview(b, &config, 1, None);
    let cfg = approve(b, &config, 1);
    engine.host().runtime().dispatcher().stop();
    let cancel_candidate =
        ops::accepted_decision(b.command("imports.apply", &apply_frame(b, &cfg, &reviewed, 1)))
            .unwrap();
    let pending = inspect(b, &cancel_candidate.0);
    let cancel = wire(
        b,
        "imports.cancel",
        1,
        json!({
            "runId":pending.id,"expectedRevision":pending.revision
        }),
        false,
    );
    assert!(
        matches!(ops::command_result(b.command("imports.cancel", &cancel)),
        Ok(OperationResult::ImportsCancel { value }) if value.changed)
    );
    drop(backend);
    drop(engine);

    // The delivery may have been claimed just before stop. Verify the durable cancellation
    // through boot recovery in either case, with no selected work published.
    let engine = start(&root);
    let backend = engine.backend();
    let b = &*backend;
    let deadline = Instant::now() + SETTLE_BOUND;
    loop {
        let run = inspect(b, &cancel_candidate.0);
        if matches!(
            run.state,
            OperationState::CancelledBeforeDispatch | OperationState::Failed
        ) {
            assert_eq!(run.processed.unwrap().get(), 0);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cancellation did not settle: {run:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    engine.host().runtime().dispatcher().stop();
    let exact = apply_frame(b, &cfg, &reviewed, 1);
    let saved = ops::accepted_decision(b.command("imports.apply", &exact)).unwrap();
    assert_eq!(inspect(b, &saved.0).state, OperationState::IntentRecorded);
    export(&path, captured + 100_000, "A later source file");
    drop(backend);
    drop(engine);

    let engine = start(&root);
    let backend = engine.backend();
    let b = &*backend;
    assert!(matches!(
        inspect(b, &cancel_candidate.0).state,
        OperationState::CancelledBeforeDispatch | OperationState::Failed
    ));
    let duplicate = ops::accepted_decision(b.command("imports.apply", &exact)).unwrap();
    assert_eq!(saved, duplicate);
    let (_, result) = completed(b, &saved.0);
    assert_eq!(result["sourceSnapshot"], reviewed.source_snapshot.as_str());
    let stored = read_snapshot(
        &**engine.host().runtime().store(),
        &Default::default(),
        |read| read.work().work(&local("Item", ITEM)),
    )
    .unwrap()
    .unwrap();
    assert_eq!(stored.title, "The reviewed source");
}

#[test]
fn settings_opens_the_native_snapshot_preview() {
    use eframe::egui::accesskit::Role;
    let _case = native_runtime_case();
    let root = Root::new("work-import-shell");
    let engine = start(&root);
    let path = root.0.join("work-imports/source.ndjson");
    export(
        &path,
        1795300702715,
        "Historical work visible in preview only",
    );
    let dialogs = Script::default();
    dialogs.files.lock().unwrap().push(vec![path]);
    let mut ui = harness(shell(&engine, &root, dialogs), 1440.0, 1040.0);
    settle(&mut ui);
    click_nav(&mut ui, "Settings");
    click(&mut ui, Role::Button, "Import work history");
    click(&mut ui, Role::Button, "Choose export…");
    click(
        &mut ui,
        Role::CheckBox,
        "Import all projects in this snapshot",
    );
    click(&mut ui, Role::Button, "Preview import");
    assert!(has_text(
        &ui,
        "Source frozen-codegraph-fixture · 8 selected records"
    ));
    assert!(has_text(&ui, "Approve reviewed selection"));
    assert!(
        read_snapshot(
            &**engine.host().runtime().store(),
            &Default::default(),
            |r| { r.work().work(&local("Item", ITEM)) }
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn first_delta_retains_the_older_parent_of_current_and_large_history() {
    let _case = native_runtime_case();
    let root = Root::new("work-import-history-window");
    let engine = start(&root);
    let backend = engine.backend();
    let b = &*backend;
    let path = root.0.join("work-imports/source.ndjson");
    let captured = 1795300702715_i64;
    export(&path, captured, "Older task required by recent history");
    let original = std::fs::read_to_string(&path).unwrap();
    let (header, body) = original.split_once('\n').unwrap();
    let mut header: Value = serde_json::from_str(header).unwrap();
    let oversized = "e".repeat(1_100_000);
    let mut rows = body
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    for row in &mut rows {
        if row["id"] == "legacy-comment" {
            row["stamp"]["updatedAtMs"] = json!(captured);
            row["evidence"]["body"] = json!(oversized);
        }
    }
    let mut body = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut body, &row).unwrap();
        body.push(b'\n');
    }
    header["recordsSha256"] = json!(sha256_hex(&body));
    let mut sealed = serde_json::to_vec(&header).unwrap();
    sealed.push(b'\n');
    sealed.extend(body);
    std::fs::write(&path, sealed).unwrap();

    let config = config_body();
    let reviewed = preview(b, &config, 2, Some(captured - 50));
    let profile = approve(b, &config, 2);
    let run =
        ops::accepted_decision(b.command("imports.apply", &apply_frame(b, &profile, &reviewed, 2)))
            .unwrap();
    let (_, report) = completed(b, &run.0);
    assert_eq!(report["counts"]["added"], 4);
    assert_eq!(report["counts"]["skipped"], 2);
    let first = match query(
        b,
        "imports.history",
        1,
        json!({"workId":local("Item",ITEM),"page":{"limit":1}}),
    )
    .unwrap()
    {
        OperationResult::ImportsHistory { value } => value,
        other => panic!("unexpected work history {other:?}"),
    };
    let cursor = first.next_cursor.unwrap();
    let second = match query(
        b,
        "imports.history",
        1,
        json!({"workId":local("Item",ITEM),"page":{"limit":1,"cursor":cursor}}),
    )
    .unwrap()
    {
        OperationResult::ImportsHistory { value } => value,
        other => panic!("unexpected continued work history {other:?}"),
    };
    let evidence = &second.items.0[0];
    assert_eq!(evidence.kind.0, "Comment");
    let reference =
        serde_json::from_value(evidence.evidence.0["source"]["fullEvidence"].clone()).unwrap();
    let bytes = ops::read_artifact_bytes(b, &reference).unwrap();
    let decoded: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded["body"], oversized);
}
