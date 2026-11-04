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
use devworks_core::protocol::{ActorId, CommandReply, ProjectId, Rejection};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::artifacts::replies::REPLY_CONSUMER;
use devworks_core::services::diagnostics::{DiagnosticService, ExportFailure, LogEntry, LogQuery};
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
    exports: Arc<DiagnosticExports>,
}

fn exports_dir(h: &Harness) -> std::path::PathBuf {
    h.root.join("diagnostics").join("exports")
}
/// Files staged in the export directory right now.
fn staged_files(h: &Harness) -> Vec<String> {
    std::fs::read_dir(exports_dir(h))
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
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
    let exports = Arc::new(DiagnosticExports::new(exports_dir(h), sources.clone()));
    diagnostics.set_export_staging(exports.clone());
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
            runs: devworks_engine::RunComposition::new(Arc::new(
                devworks_core::services::runs::ControlService::new(h.ids.clone()),
            )),
            power: Arc::new(NativePower),
            diagnostics: diagnostics.clone(),
            browser: None,
            packages: None,
            use_cases: UseCases::new().with_queue(),
            consumers,
            indexing: None,
            config,
        },
        Parts {
            sqlite,
            sources,
            diagnostics,
            exports,
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
    let outcome = app
        .export_diagnostics(&user, &LogQuery::default(), "export-1")
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
    // The staged file went once the save copied it.
    assert!(staged_files(&h).is_empty(), "{:?}", staged_files(&h));
    // A non-human principal cannot stage an export.
    let agent = principal(&**app.store(), "agent-a", PrincipalKind::Agent).with_lineage(
        Some(ProjectId(id("project-a"))),
        None,
        None,
    );
    assert!(
        app.export_diagnostics(&agent, &LogQuery::default(), "export-2")
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

/// The recorded save of a command outcome, and whether it was a replay.
fn saved_artifact(
    outcome: devworks_core::gateway::CommandOutcome,
) -> (devworks_core::protocol::provider::ArtifactRef, bool) {
    match outcome.expect("addressable") {
        CommandReply::Recorded {
            receipt, replayed, ..
        } => match receipt.decision {
            DomainDecision::Applied {
                result: OperationResult::ArtifactsSave { value },
            } => (value.artifact, replayed),
            other => panic!("{other:?}"),
        },
        other => panic!("expected the recorded save, got {other:?}"),
    }
}
fn conflicted(outcome: devworks_core::gateway::CommandOutcome) {
    match outcome.expect("addressable") {
        CommandReply::Rejected { code, .. } => {
            assert!(matches!(code, Rejection::IdempotencyConflict), "{code:?}")
        }
        other => panic!("expected an idempotency conflict, got {other:?}"),
    }
}

#[test]
fn an_export_retry_replays_through_the_gateway_and_other_use_of_its_key_conflicts() {
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
    // The first attempt commits, but its response is lost on the way back to the caller.
    let (first, replayed) = saved_artifact(
        app.export_diagnostics(&user, &LogQuery::default(), "export-lost")
            .expect("exported"),
    );
    assert!(!replayed);
    // More happened since; a retry with the same key replays the recorded save.
    parts
        .diagnostics
        .log(LogEntry::new("client", "later", Severity::Info, "later"))
        .unwrap();
    let (again, replayed) = saved_artifact(
        app.export_diagnostics(&user, &LogQuery::default(), "export-lost")
            .expect("exported"),
    );
    assert!(replayed, "the retry replays");
    assert_eq!(again.version_id, first.version_id);
    // A restarted service (nothing in memory) resubmits the durably recorded command, and the
    // gateway replays it under current authority.
    let fresh = DiagnosticService::new(app.store().clone(), h.ids.clone(), h.clock(), None);
    fresh.set_export_staging(parts.exports.clone());
    let submitted = std::cell::Cell::new(false);
    let (durable, replayed) = saved_artifact(
        fresh
            .export(&user, &LogQuery::default(), "export-lost", |raw| {
                submitted.set(true);
                app.command(&user, "artifacts.save", raw)
            })
            .expect("replayed"),
    );
    assert!(submitted.get(), "the replay goes through the gateway");
    assert!(replayed);
    assert_eq!(durable.version_id, first.version_id);
    // The same key with another selection is a conflict; nothing is staged.
    let other = LogQuery {
        component: Some("client".into()),
        ..LogQuery::default()
    };
    conflicted(
        fresh
            .export(&user, &other, "export-lost", |raw| {
                app.command(&user, "artifacts.save", raw)
            })
            .expect("addressable"),
    );
    // An ordinary save under a key is never returned as an export, even with nothing cached.
    let file = h.root.with_extension("upload");
    std::fs::create_dir_all(&file).unwrap();
    let file = file.join("notes.txt");
    std::fs::write(&file, b"unrelated notes").unwrap();
    let ticket = parts.sources.mint_upload(&user, &file, 60_000).unwrap();
    let frame = json!({"protocol": "1.0-draft.1", "requestKey": "shared-key", "command": {
        "kind": "Save", "projectId": "project-a", "title": "Notes", "mediaType": "text/plain",
        "source": {"kind": "Upload", "ticketId": ticket},
        "expectedDigest": devworks_core::fingerprint::sha256_hex(b"unrelated notes"),
        "expectedBytes": "15"}});
    let (unrelated, _) =
        saved_artifact(app.command(&user, "artifacts.save", frame.to_string().as_bytes()));
    conflicted(
        fresh
            .export(&user, &LogQuery::default(), "shared-key", |raw| {
                app.command(&user, "artifacts.save", raw)
            })
            .expect("addressable"),
    );
    assert_ne!(unrelated.version_id, first.version_id);
    assert!(staged_files(&h).is_empty(), "{:?}", staged_files(&h));
    rt.block_on(app.shutdown());
}

#[test]
fn the_monitor_sweeps_stale_export_staging_files() {
    let h = Harness::new();
    let rt = runtime();
    drop(h.open());
    // Leftovers of an earlier run: one past the ticket lifetime, one still fresh.
    std::fs::create_dir_all(exports_dir(&h)).unwrap();
    let stale = exports_dir(&h).join("1-0-devworks-diagnostics-stale.json");
    let fresh = exports_dir(&h).join("1-1-devworks-diagnostics-fresh.json");
    std::fs::write(&stale, b"{}").unwrap();
    std::fs::write(&fresh, b"{}").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(3_600))
        .unwrap();
    let (composition, _parts) = compose(&h, Vec::new());
    let app = rt.block_on(boot(composition)).expect("boot");
    rt.block_on(until("the boot monitor pass sweeps the stale file", || {
        !stale.exists()
    }));
    assert!(fresh.exists(), "a file within its ticket lifetime stays");
    rt.block_on(app.shutdown());
}

/// Staging that holds each `stage` call until a second attempt is staging too (at most
/// 1.5 s): with a single claimant the gate times out and nothing else ever stages.
struct Gate {
    inner: Arc<DiagnosticExports>,
    entered: std::sync::Mutex<usize>,
    changed: std::sync::Condvar,
}
impl Gate {
    fn wait_entered(&self, n: usize) {
        let entered = self.entered.lock().unwrap();
        let _ = self
            .changed
            .wait_timeout_while(entered, Duration::from_secs(30), |e| *e < n)
            .unwrap();
    }
}
impl devworks_core::ports::diagnostics::ExportStaging for Gate {
    fn stage(
        &self,
        ctx: &InvocationContext,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<
        devworks_core::protocol::data::ArtifactSource,
        devworks_core::ports::diagnostics::ExportRefused,
    > {
        {
            let mut entered = self.entered.lock().unwrap();
            *entered += 1;
            self.changed.notify_all();
            let _ = self
                .changed
                .wait_timeout_while(entered, Duration::from_millis(1_500), |e| *e < 2)
                .unwrap();
        }
        self.inner.stage(ctx, file_name, bytes)
    }
    fn release(&self, source: &devworks_core::protocol::data::ArtifactSource) {
        self.inner.release(source)
    }
    fn sweep(&self) -> usize {
        self.inner.sweep()
    }
}

#[test]
fn overlapping_exports_of_one_request_key_have_one_claimant_and_one_command() {
    let h = Harness::new();
    let rt = runtime();
    drop(h.open());
    seed_root(&h, "root-a");
    let (composition, parts) = compose(&h, Vec::new());
    let app = rt.block_on(boot(composition)).expect("boot");
    let gate = Arc::new(Gate {
        inner: parts.exports.clone(),
        entered: std::sync::Mutex::new(0),
        changed: std::sync::Condvar::new(),
    });
    parts.diagnostics.set_export_staging(gate.clone());
    let user = principal(&**app.store(), "user-a", PrincipalKind::Human).with_lineage(
        Some(ProjectId(id("project-a"))),
        None,
        None,
    );
    let (first, second) = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            app.export_diagnostics(&user, &LogQuery::default(), "export-race")
                .expect("exported")
        });
        // The first attempt is staging; the second starts while it is.
        gate.wait_entered(1);
        let b = scope.spawn(|| {
            app.export_diagnostics(&user, &LogQuery::default(), "export-race")
                .expect("exported")
        });
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_eq!(*gate.entered.lock().unwrap(), 1, "only the claimant staged");
    let (a, a_replayed) = saved_artifact(first);
    let (b, b_replayed) = saved_artifact(second);
    assert_eq!(a.version_id, b.version_id, "one save");
    assert!(a_replayed != b_replayed, "one decided, one replayed");
    // The durable intent holds the command whose receipt was recorded: later retries replay.
    let (later, replayed) = saved_artifact(
        app.export_diagnostics(&user, &LogQuery::default(), "export-race")
            .expect("exported"),
    );
    assert!(replayed);
    assert_eq!(later.version_id, a.version_id);
    assert!(staged_files(&h).is_empty(), "{:?}", staged_files(&h));
    rt.block_on(app.shutdown());
}

#[test]
fn a_quarantined_corrupt_artifact_at_boot_needs_attention() {
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
    let dir = h.root.with_extension("upload");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("notes.txt");
    std::fs::write(&file, b"notes to keep").unwrap();
    let ticket = parts.sources.mint_upload(&user, &file, 60_000).unwrap();
    let frame = json!({"protocol": "1.0-draft.1", "requestKey": "notes-1", "command": {
        "kind": "Save", "projectId": "project-a", "title": "Notes", "mediaType": "text/plain",
        "source": {"kind": "Upload", "ticketId": ticket},
        "expectedDigest": devworks_core::fingerprint::sha256_hex(b"notes to keep"),
        "expectedBytes": "13"}});
    let (artifact, _) =
        saved_artifact(app.command(&user, "artifacts.save", frame.to_string().as_bytes()));
    let digest = String::from(artifact.sha256.clone());
    let blob = read(&*parts.sqlite, |s| s.artifacts().blob(&digest)).expect("blob");
    rt.block_on(app.shutdown());
    drop(app);
    drop(parts);
    // The managed file is corrupted while DevWorks is closed.
    std::fs::write(h.root.join(&blob.relative_path), b"tampered bytes!").unwrap();
    let (composition, parts) = compose(&h, Vec::new());
    let app = rt.block_on(boot(composition)).expect("boot");
    assert_eq!(app.report().artifacts.quarantined, vec![digest.clone()]);
    let user = principal(&**app.store(), "user-a", PrincipalKind::Human);
    let all = checks(&app, &user);
    let recovery = check(&all, "boot.recovery");
    assert_eq!(health(recovery), "Degraded", "{}", recovery.message.0);
    assert!(
        recovery.message.0.contains(&digest),
        "{}",
        recovery.message.0
    );
    assert!(recovery.message.0.contains("quarantined"));
    assert!(text!(recovery.recovery.as_ref().unwrap()).contains("workspace.restore"));
    let readiness = check(&all, "health.readiness");
    assert_eq!(health(readiness), "Degraded");
    let boot_record = logs(&app, &user, json!({"component": "boot"}))
        .into_iter()
        .rev()
        .find(|e| e.code.0 == "recovered")
        .expect("boot record");
    assert_eq!(text!(&boot_record.outcome), "\"Partial\"");
    assert!(boot_record.message.0.contains(&digest));
    drop(parts);
    rt.block_on(app.shutdown());
}

fn rejected(
    code: Rejection,
    retry: devworks_core::protocol::RetryAdvice,
) -> devworks_core::gateway::CommandOutcome {
    Ok(CommandReply::Rejected {
        protocol: devworks_core::protocol::Protocol::Draft1,
        request_key: devworks_core::protocol::RequestKey(id("transient")),
        code,
        retry,
        correlation_id: devworks_core::protocol::CorrelationId(id("correlation-fixture")),
    })
}

#[test]
fn a_retryable_save_rejection_keeps_the_staged_file_for_the_retry() {
    use devworks_core::protocol::RetryAdvice;
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
    // The save is refused transiently (the store was busy) and records no receipt.
    let outcome = parts
        .diagnostics
        .export(&user, &LogQuery::default(), "transient", |_| {
            rejected(Rejection::StorageUnavailable, RetryAdvice::SameCommand)
        })
        .expect("submitted");
    assert!(matches!(outcome, Ok(CommandReply::Rejected { .. })));
    assert_eq!(
        staged_files(&h).len(),
        1,
        "the staged file stays for the retry"
    );
    // The retry resubmits the recorded command, whose source still resolves, and saves.
    let (saved, replayed) = saved_artifact(
        app.export_diagnostics(&user, &LogQuery::default(), "transient")
            .expect("exported"),
    );
    assert!(!replayed);
    assert_eq!(saved.media_type, "application/json");
    assert!(staged_files(&h).is_empty(), "{:?}", staged_files(&h));
    // A rejection the same command cannot overcome releases the file at once.
    let outcome = parts
        .diagnostics
        .export(&user, &LogQuery::default(), "final", |_| {
            rejected(Rejection::PermissionDenied, RetryAdvice::Never)
        })
        .expect("submitted");
    assert!(matches!(outcome, Ok(CommandReply::Rejected { .. })));
    assert!(staged_files(&h).is_empty(), "{:?}", staged_files(&h));
    // An unknown outcome keeps it too (until the sweep, once its ticket expired).
    let outcome = parts
        .diagnostics
        .export(&user, &LogQuery::default(), "unknown", |_| {
            Err(devworks_core::gateway::GatewayFailure::NoUseCase(
                "artifacts.save".into(),
            ))
        })
        .expect("submitted");
    assert!(outcome.is_err());
    assert_eq!(staged_files(&h).len(), 1);
    rt.block_on(app.shutdown());
}

#[test]
fn an_abandoned_export_claim_is_taken_over() {
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
    // An earlier process claimed the key and stopped before staging: its claim is pending and
    // older than the takeover age.
    let scope = devworks_core::context::ScopeKey::of(&user)
        .as_str()
        .to_owned();
    let stale = ExportIntent {
        actor: user.principal().clone(),
        scope_key: scope.clone(),
        request_key: "orphaned".into(),
        project_id: ProjectId(id("project-a")),
        selection_sha256: devworks_core::fingerprint::sha256_hex(
            json!({"selection": LogQuery::default().selection(), "projectId": "project-a"})
                .to_string()
                .as_bytes(),
        ),
        command: None,
        claim: "dead-claimant".into(),
        created_at_ms: h.now() - 10 * 60 * 1000,
    };
    write(&*parts.sqlite, move |tx| {
        tx.diagnostics().claim_export_intent(&stale).map(|_| ())
    })
    .unwrap();
    let (saved, replayed) = saved_artifact(
        app.export_diagnostics(&user, &LogQuery::default(), "orphaned")
            .expect("exported after takeover"),
    );
    assert!(!replayed);
    assert_eq!(saved.media_type, "application/json");
    let intent = read(&*parts.sqlite, |s| {
        s.diagnostics()
            .export_intent(user.principal(), &scope, "orphaned")
    })
    .expect("intent");
    assert_ne!(intent.claim, "dead-claimant");
    assert!(intent.command.is_some());
    rt.block_on(app.shutdown());
}

/// A store whose next write, once armed, reports `CommitUnknown`: it commits but loses its
/// acknowledgment, or (not `commits`) rolls back while reporting the same, optionally followed by
/// one failed read.
struct LostAck {
    inner: Arc<dyn ApplicationStore>,
    /// Writes to let through before the one whose acknowledgment is lost (0: none armed).
    writes_before_loss: std::sync::atomic::AtomicUsize,
    armed: std::sync::atomic::AtomicBool,
    commits: std::sync::atomic::AtomicBool,
    fail_read_after: std::sync::atomic::AtomicBool,
    fail_next_read: std::sync::atomic::AtomicBool,
}
impl LostAck {
    fn arm(&self, writes_before: usize, commits: bool, then_fail_read: bool) {
        use std::sync::atomic::Ordering::SeqCst;
        self.writes_before_loss.store(writes_before, SeqCst);
        self.commits.store(commits, SeqCst);
        self.fail_read_after.store(then_fail_read, SeqCst);
        self.armed.store(true, SeqCst);
    }
}
struct LoseAck(Box<dyn WriteJob>);
impl WriteJob for LoseAck {
    fn run(&mut self, tx: &mut dyn WriteTransaction) -> Result<(), StorageError> {
        self.0.run(tx)
    }
    fn finish(self: Box<Self>, outcome: Result<(), StorageError>) {
        self.0.finish(outcome.and(Err(StorageError::CommitUnknown)));
    }
}
/// Rolls the write back (its work never runs) but reports the outcome as unknown.
struct DropWrite(Box<dyn WriteJob>);
impl WriteJob for DropWrite {
    fn run(&mut self, _: &mut dyn WriteTransaction) -> Result<(), StorageError> {
        Err(StorageError::Busy)
    }
    fn finish(self: Box<Self>, _: Result<(), StorageError>) {
        self.0.finish(Err(StorageError::CommitUnknown));
    }
}
impl ApplicationStore for LostAck {
    fn info(&self) -> StoreInfo {
        self.inner.info()
    }
    fn submit_write(
        &self,
        options: WriteOptions,
        job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.armed.load(SeqCst) {
            if self.writes_before_loss.load(SeqCst) > 0 {
                self.writes_before_loss.fetch_sub(1, SeqCst);
            } else {
                self.armed.store(false, SeqCst);
                if self.fail_read_after.load(SeqCst) {
                    self.fail_next_read.store(true, SeqCst);
                }
                return if self.commits.load(SeqCst) {
                    self.inner.submit_write(options, Box::new(LoseAck(job)))
                } else {
                    self.inner.submit_write(options, Box::new(DropWrite(job)))
                };
            }
        }
        self.inner.submit_write(options, job)
    }
    fn read(
        &self,
        options: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        if self
            .fail_next_read
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(StorageError::Busy);
        }
        self.inner.read(options, work)
    }
}

#[test]
fn a_lost_acknowledgment_of_the_intent_keeps_the_staged_file_and_resolves_durably() {
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
    let flaky = Arc::new(LostAck {
        inner: app.store().clone(),
        writes_before_loss: Default::default(),
        armed: Default::default(),
        commits: Default::default(),
        fail_read_after: Default::default(),
        fail_next_read: Default::default(),
    });
    let svc = DiagnosticService::new(flaky.clone(), h.ids.clone(), h.clock(), None);
    svc.set_export_staging(parts.exports.clone());
    let submit = |raw: &[u8]| app.command(&user, "artifacts.save", raw);
    // The claim commits; recording the command commits but its acknowledgment is lost. The
    // re-read shows the command recorded, so the export goes on and saves.
    flaky.arm(1, true, false);
    let (saved, replayed) = saved_artifact(
        svc.export(&user, &LogQuery::default(), "lost-ack", submit)
            .expect("resolved from the durable intent"),
    );
    assert!(!replayed);
    assert!(staged_files(&h).is_empty(), "{:?}", staged_files(&h));
    let (again, replayed) = saved_artifact(
        svc.export(&user, &LogQuery::default(), "lost-ack", submit)
            .expect("exported"),
    );
    assert!(replayed);
    assert_eq!(again.version_id, saved.version_id);
    // The same, but the re-read fails too: the outcome stays unknown, so the staged file the
    // recorded command names is kept, and a later retry resolves and saves with it.
    flaky.arm(1, true, true);
    let unknown = svc.export(&user, &LogQuery::default(), "lost-twice", submit);
    assert!(
        matches!(
            unknown,
            Err(ExportFailure::Storage(StorageError::CommitUnknown))
        ),
        "{unknown:?}"
    );
    assert_eq!(staged_files(&h).len(), 1, "the staged file stays");
    let (retried, replayed) = saved_artifact(
        svc.export(&user, &LogQuery::default(), "lost-twice", submit)
            .expect("exported"),
    );
    assert!(!replayed, "the recorded command was never submitted before");
    assert_eq!(retried.media_type, "application/json");
    assert!(staged_files(&h).is_empty(), "{:?}", staged_files(&h));
    // The write did not commit: the re-read shows the claim still pending, so the staged file
    // and the claim both go, and a retry exports at once (a held claim would leave it pending).
    flaky.arm(1, false, false);
    let failed = svc.export(&user, &LogQuery::default(), "never-recorded", submit);
    assert!(
        matches!(
            failed,
            Err(ExportFailure::Storage(StorageError::CommitUnknown))
        ),
        "{failed:?}"
    );
    assert!(staged_files(&h).is_empty(), "{:?}", staged_files(&h));
    let (fresh, replayed) = saved_artifact(
        svc.export(&user, &LogQuery::default(), "never-recorded", submit)
            .expect("exported"),
    );
    assert!(!replayed);
    assert_eq!(fresh.media_type, "application/json");
    assert!(staged_files(&h).is_empty(), "{:?}", staged_files(&h));
    rt.block_on(app.shutdown());
}
