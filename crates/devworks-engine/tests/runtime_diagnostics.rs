//! Local diagnostics through the boot entry point on the real SQLite store: boot recovery,
//! collection, the dispatcher and every consumer feed `workspace.diagnostics`; a poisoned
//! delivery is degraded queue health with safe recovery links; a failed boot stays visible after
//! the restart; a committed `workspace.record_diagnostic` raises the live-log hint; an export
//! becomes an artifact through `artifacts.save` and carries no registered secret, and that
//! artifact is accepted as evidence; the retention settings bound the log. No inference.
mod common;

use common::*;
use devworks_adapters::diagnostics::DiagnosticExports;
use devworks_adapters::fs::NativeFilesystem;
use devworks_adapters::fs::artifacts::NativeArtifactFiles;
use devworks_adapters::fs::sources::HostArtifactSources;
use devworks_adapters::power::NativePower;
use devworks_adapters::process::NativeProcessHost;
use devworks_adapters::storage::sqlite::SqliteStore;
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::ports::diagnostics::StorageHealthProbe;
use devworks_core::ports::storage::*;
use devworks_core::protocol::records::{DiagnosticEntry, Severity};
use devworks_core::protocol::results::{DomainDecision, DomainQueryReply, OperationResult};
use devworks_core::protocol::{ActorId, CommandReply, ProjectId};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::artifacts::replies::REPLY_CONSUMER;
use devworks_core::services::diagnostics::{DiagnosticService, LogEntry, LogQuery};
use devworks_core::services::resources::ResourceService;
use devworks_core::settings::catalog::{LOG_MAX_RECORDS_KEY, LOG_RETENTION_DAYS_KEY, descriptor};
use devworks_core::settings::model;
use devworks_engine::{BootError, Composition, Consumer, Outcome, Runtime, RuntimeConfig, boot};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

macro_rules! text {
    ($v:expr) => {
        serde_json::to_string($v).unwrap()
    };
}

const SECRET: &str = "s3cr3t-value-registered-by-a-provider";

struct Parts {
    sqlite: Arc<SqliteStore>,
    sources: Arc<HostArtifactSources>,
    diagnostics: Arc<DiagnosticService>,
}

fn compose(h: &Harness, consumers: Vec<Consumer>) -> (Composition, Parts) {
    let sqlite = Arc::new(h.open_sqlite());
    let store: Arc<dyn ApplicationStore> = sqlite.clone();
    let sources = Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone()));
    let artifacts = Arc::new(ArtifactService::new(
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.with_extension("cache"),
        )),
        sources.clone(),
        Arc::new(NativeFilesystem::new()),
        h.clock(),
        h.ids.clone(),
    ));
    let diagnostics = Arc::new(DiagnosticService::new(
        store.clone(),
        h.ids.clone(),
        h.clock(),
        Some(sqlite.clone() as Arc<dyn StorageHealthProbe>),
    ));
    diagnostics.register_secret(SECRET);
    let mut config = RuntimeConfig::new(id("dispatcher-1"));
    config.collect_interval = Duration::from_secs(3600);
    config.diagnostics_interval = Duration::from_secs(3600);
    (
        Composition {
            store,
            clock: h.clock(),
            ids: h.ids.clone(),
            authority: Arc::new(Authority),
            artifacts,
            resources: Arc::new(ResourceService::new(h.ids.clone(), h.clock())),
            process_host: Arc::new(NativeProcessHost),
            power: Arc::new(NativePower),
            diagnostics: diagnostics.clone(),
            use_cases: UseCases::new().with_queue(),
            consumers,
            config,
        },
        Parts {
            sqlite,
            sources,
            diagnostics,
        },
    )
}

fn poisoned() -> Consumer {
    let mut c = consumer(
        "indexer",
        Recorder::new(|_, _| Outcome::Poison("the payload names no repository".into())),
    );
    c.retry.max_attempts = 1;
    c
}

fn query(rt: &Runtime, ctx: &InvocationContext, op: &str, body: Value) -> OperationResult {
    let mut body = body;
    body["kind"] = json!(op);
    let frame = json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-1",
        "schemaRevision": 2, "body": body});
    match rt
        .query(ctx, op, frame.to_string().as_bytes())
        .expect("addressable")
    {
        DomainQueryReply::Result { result, .. } => result,
        other => panic!("{other:?}"),
    }
}
fn checks(rt: &Runtime, ctx: &InvocationContext) -> Vec<DiagnosticEntry> {
    match query(
        rt,
        ctx,
        "workspace.diagnostics",
        json!({"page": {"limit": 200}}),
    ) {
        OperationResult::WorkspaceDiagnostics { value } => value.items.0,
        other => panic!("{other:?}"),
    }
}
fn logs(rt: &Runtime, ctx: &InvocationContext, filter: Value) -> Vec<DiagnosticEntry> {
    let mut body = json!({"page": {"limit": 200}, "afterSequence": "0"});
    for (k, v) in filter.as_object().unwrap() {
        body[k] = v.clone();
    }
    match query(rt, ctx, "workspace.logs", body) {
        OperationResult::WorkspaceLogs { value } => value.items.0,
        other => panic!("{other:?}"),
    }
}
fn check<'a>(all: &'a [DiagnosticEntry], component: &str) -> &'a DiagnosticEntry {
    all.iter()
        .find(|c| c.component.0 == component)
        .unwrap_or_else(|| panic!("no {component} in {all:#?}"))
}
fn health(c: &DiagnosticEntry) -> String {
    serde_json::to_value(c.health.as_ref().unwrap())
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}

/// Waits (bounded, failing on expiry) until `done` holds, polling the durable state.
async fn until(what: &str, mut done: impl FnMut() -> bool) {
    within(what, async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
}

fn set_retention(store: &dyn ApplicationStore, days: i64, max: i64) {
    write(store, move |tx| {
        let current = model::current_layer(tx.configuration(), &ConfigScope::Workspace)?;
        let entry = |key: &str, value: i64| {
            let schema = serde_json::to_value(descriptor(key).unwrap().schema_ref()).unwrap();
            serde_json::from_value(json!({"key": key, "value": {"schema": schema, "value": value}}))
                .unwrap()
        };
        let plan = model::plan(
            &current,
            &[
                entry(LOG_RETENTION_DAYS_KEY, days),
                entry(LOG_MAX_RECORDS_KEY, max),
            ],
        );
        let body = model::body(
            &plan,
            &ActorId(id("host")),
            tx.now_ms(),
            json!({"kind": "test"}),
        );
        tx.configuration().append_revision(
            &ConfigScope::Workspace,
            model::NAMESPACE,
            current.revision,
            model::BODY_SCHEMA_VERSION,
            body,
        )?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn boot_feeds_health_and_records_recovery_and_failures() {
    let h = Harness::new();
    let rt = runtime();
    // A boot that stops (a caller consumer reusing a required name) records why.
    let (composition, parts) = compose(
        &h,
        vec![consumer(
            REPLY_CONSUMER,
            Recorder::new(|_, _| Outcome::Completed),
        )],
    );
    match rt.block_on(boot(composition)) {
        Err(BootError::Composition(_)) => {}
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("a reserved consumer name was accepted"),
    }
    drop(parts);
    // The next boot: a delivery its consumer cannot ever handle is waiting.
    let store = h.open();
    enqueue(&*store, "indexer", None, json!({"repository": null}));
    drop(store);
    let (composition, parts) = compose(&h, vec![poisoned()]);
    let app = rt.block_on(boot(composition)).expect("boot");
    let user = principal(&**app.store(), "user-a", PrincipalKind::Human);
    // The failed boot survived the restart, with its stage and class; this boot's record too.
    let boot_records = logs(&app, &user, json!({"component": "boot"}));
    let failed = boot_records
        .iter()
        .find(|e| e.code.0 == "failed")
        .expect("the failed boot is recorded");
    assert_eq!(failed.stage.as_ref().unwrap().0, "composition");
    assert_eq!(text!(&failed.outcome), "\"Failed\"");
    assert!(failed.error_class.is_some());
    let recovered = boot_records
        .iter()
        .find(|e| e.code.0 == "recovered")
        .expect("this boot's recovery is recorded");
    assert!(recovered.metrics.is_some());
    rt.block_on(until("the poisoned delivery fails", || {
        read(&*parts.sqlite, |s| s.queue().status("indexer")).failed == 1
    }));
    // Monitor probes ran at boot (or run now); either way the checks are measured.
    parts.diagnostics.refresh_monitor();
    let all = checks(&app, &user);
    assert_eq!(health(check(&all, "boot.recovery")), "Healthy");
    assert_eq!(health(check(&all, "queue.dispatcher")), "Healthy");
    let indexer = check(&all, "queue.indexer");
    assert_eq!(health(indexer), "Degraded");
    let links = text!(indexer.recovery.as_ref().unwrap());
    for op in ["queue.inspect", "queue.retry", "queue.resolve"] {
        assert!(links.contains(op), "{links}");
    }
    assert_eq!(
        health(check(&all, &format!("queue.{REPLY_CONSUMER}"))),
        "Healthy"
    );
    assert_eq!(health(check(&all, "storage.write")), "Healthy");
    assert_eq!(health(check(&all, "storage.integrity")), "Healthy");
    rt.block_on(until("the boot collection pass", || {
        app.collections().borrow().is_some()
    }));
    assert_eq!(
        health(check(&checks(&app, &user), "artifacts.collection")),
        "Healthy"
    );
    let readiness = check(&all, "health.readiness");
    assert_eq!(health(readiness), "Degraded");
    assert!(
        readiness.message.0.contains("queue.indexer"),
        "{}",
        readiness.message.0
    );
    rt.block_on(app.shutdown());
}

#[test]
fn records_raise_the_live_hint_and_an_export_is_a_redacted_artifact() {
    let h = Harness::new();
    let rt = runtime();
    let store = h.open();
    drop(store);
    seed_root(&h, "root-a");
    let (composition, parts) = compose(&h, Vec::new());
    let app = rt.block_on(boot(composition)).expect("boot");
    let user = principal(&**app.store(), "user-a", PrincipalKind::Human).with_lineage(
        Some(ProjectId(id("project-a"))),
        None,
        None,
    );
    // Host work with a secret in its message and output.
    parts
        .diagnostics
        .log(LogEntry {
            output: Some(format!(
                "connecting with key {SECRET}\nAuthorization: Bearer abc.def.ghi"
            )),
            ..LogEntry::new(
                "provider",
                "request_failed",
                Severity::Error,
                format!("Provider refused credential {SECRET}"),
            )
        })
        .unwrap();
    let before = app.diagnostics().watch().latest();
    let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": "client-1",
        "schemaRevision": 2, "body": {"kind": "workspace.record_diagnostic",
            "component": "client", "code": "render_failed", "severity": "Error",
            "message": format!("Render failed near {SECRET}")}});
    let reply = app
        .command(
            &user,
            "workspace.record_diagnostic",
            frame.to_string().as_bytes(),
        )
        .expect("addressable");
    let recorded = match reply {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied {
                result: OperationResult::WorkspaceRecordDiagnostic { value },
            } => value,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    let sequence = recorded.sequence.as_ref().unwrap().get();
    assert!(sequence > before);
    assert_eq!(
        app.diagnostics().watch().latest(),
        sequence,
        "the commit raised the hint"
    );
    // Logs never carry the secret.
    let all = logs(&app, &user, json!({}));
    assert!(!text!(&all).contains(SECRET));
    // The export: a bounded, redacted JSON document saved through artifacts.save.
    let exports = DiagnosticExports::new(
        h.root.join("diagnostics").join("exports"),
        parts.sources.clone(),
    );
    let outcome = app
        .export_diagnostics(&user, &LogQuery::default(), &exports, "export-1")
        .expect("exported");
    let saved = match outcome.expect("addressable") {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied {
                result: OperationResult::ArtifactsSave { value },
            } => value,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    assert_eq!(saved.artifact.media_type, "application/json");
    let digest = String::from(saved.artifact.sha256.clone());
    let blob = read(&*parts.sqlite, |s| s.artifacts().blob(&digest)).expect("blob");
    let bytes = std::fs::read(h.root.join(&blob.relative_path)).unwrap();
    let document = String::from_utf8(bytes).unwrap();
    assert!(!document.contains(SECRET), "the export carries the secret");
    let parsed: Value = serde_json::from_str(&document).unwrap();
    assert_eq!(parsed["format"], "devworks.diagnostics-export/1");
    assert_eq!(parsed["records"].as_array().unwrap().len(), all.len());
    assert!(parsed["health"].as_array().unwrap().len() > 5);
    assert_eq!(parsed["truncated"], false);
    // A non-human principal cannot stage an export.
    let agent = principal(&**app.store(), "agent-a", PrincipalKind::Agent).with_lineage(
        Some(ProjectId(id("project-a"))),
        None,
        None,
    );
    assert!(
        app.export_diagnostics(&agent, &LogQuery::default(), &exports, "export-2")
            .is_err()
    );
    // The exported artifact is visible evidence for a later report.
    let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": "client-2",
        "schemaRevision": 2, "body": {"kind": "workspace.record_diagnostic",
            "component": "client", "code": "attached", "severity": "Info",
            "message": "Diagnostics attached", "evidence": saved.artifact}});
    match app
        .command(
            &user,
            "workspace.record_diagnostic",
            frame.to_string().as_bytes(),
        )
        .expect("addressable")
    {
        CommandReply::Recorded { receipt, .. } => {
            assert!(
                matches!(receipt.decision, DomainDecision::Applied { .. }),
                "{:?}",
                receipt.decision
            )
        }
        other => panic!("{other:?}"),
    }
    rt.block_on(app.shutdown());
}

#[test]
fn the_retention_settings_bound_the_log() {
    let h = Harness::new();
    let rt = runtime();
    let store = h.open();
    set_retention(&*store, 1, 1_000);
    drop(store);
    let (composition, parts) = compose(&h, Vec::new());
    let app = rt.block_on(boot(composition)).expect("boot");
    let svc = parts.diagnostics.clone();
    write(&**app.store(), move |tx| {
        for i in 0..1_005 {
            svc.log_in(
                tx,
                &LogEntry::new("client", "bulk", Severity::Info, format!("record {i}")),
            )
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))?;
        }
        Ok(())
    })
    .unwrap();
    let pruned = parts.diagnostics.prune().unwrap();
    assert!(pruned.over_cap >= 5, "{pruned:?}");
    let records = read(&*parts.sqlite, |s| s.diagnostics().diagnostic_stats()).records;
    assert_eq!(records, 1_000);
    // A day later everything recorded before then has expired.
    h.advance(2 * 86_400_000);
    let pruned = parts.diagnostics.prune().unwrap();
    assert!(pruned.expired >= 999, "{pruned:?}");
    let left = read(&*parts.sqlite, |s| s.diagnostics().diagnostic_stats()).records;
    assert_eq!(left, 1, "only this pass's own record remains");
    rt.block_on(app.shutdown());
}

#[test]
fn an_export_retried_with_its_request_key_replays_instead_of_conflicting() {
    let h = Harness::new();
    let rt = runtime();
    drop(h.open());
    seed_root(&h, "root-a");
    let (composition, parts) = compose(&h, Vec::new());
    let app = rt.block_on(boot(composition)).expect("boot");
    let user = principal(&**app.store(), "user-a", PrincipalKind::Human).with_lineage(
        Some(ProjectId(id("project-a"))),
        None,
        None,
    );
    let exports = DiagnosticExports::new(
        h.root.join("diagnostics").join("exports"),
        parts.sources.clone(),
    );
    let saved =
        |outcome: devworks_core::gateway::CommandOutcome| match outcome.expect("addressable") {
            CommandReply::Recorded {
                receipt, replayed, ..
            } => match receipt.decision {
                DomainDecision::Applied {
                    result: OperationResult::ArtifactsSave { value },
                } => (value.artifact, replayed),
                other => panic!("{other:?}"),
            },
            other => panic!("expected the recorded save, got {other:?}"),
        };
    // The first attempt commits, but its response is lost on the way back to the caller.
    let lost = app
        .export_diagnostics(&user, &LogQuery::default(), &exports, "export-lost")
        .expect("exported");
    let (first, replayed) = saved(lost);
    assert!(!replayed);
    // More happened since; a retry with the same key replays the recorded save.
    parts
        .diagnostics
        .log(LogEntry::new("client", "later", Severity::Info, "later"))
        .unwrap();
    let retry = app
        .export_diagnostics(&user, &LogQuery::default(), &exports, "export-lost")
        .expect("exported");
    let (again, replayed) = saved(retry);
    assert!(replayed, "the retry replays");
    assert_eq!(again.artifact_id, first.artifact_id);
    assert_eq!(again.version_id, first.version_id);
    // After a restart of the service (no staged command in memory) the recorded receipt is
    // the replay, and nothing is staged or submitted again.
    let fresh = DiagnosticService::new(app.store().clone(), h.ids.clone(), h.clock(), None);
    let outcome = fresh
        .export(&user, &LogQuery::default(), &exports, "export-lost", |_| {
            panic!("a recorded export is not submitted again")
        })
        .expect("replayed");
    let (durable, replayed) = saved(outcome);
    assert!(replayed);
    assert_eq!(durable.version_id, first.version_id);
    rt.block_on(app.shutdown());
}
