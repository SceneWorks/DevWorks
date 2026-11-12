use super::*;
use devworks_adapters::{power::NativePower, process::NativeProcessHost};
use devworks_core::ports::files::ArtifactFiles;
use devworks_core::services::{
    diagnostics::DiagnosticService, resources::ResourceService, runs::ControlService,
};
use devworks_engine::{Composition, RunComposition, Runtime, RuntimeConfig, boot};
use std::time::Duration;

fn composition(
    h: &Harness,
    store: Arc<dyn ApplicationStore>,
    artifacts: Arc<ArtifactService>,
) -> Composition {
    let mut config = RuntimeConfig::new(id("import-cleanup-runtime"));
    // Reclamation must be triggered by import maintenance, not a frequent test collector.
    config.collect_interval = Duration::from_secs(3600);
    config.diagnostics_interval = Duration::from_secs(3600);
    Composition {
        store: store.clone(),
        clock: h.clock(),
        ids: h.ids.clone(),
        authority: Arc::new(Authority),
        artifacts,
        resources: Arc::new(ResourceService::new(h.ids.clone(), h.clock())),
        process_host: Arc::new(NativeProcessHost),
        runs: RunComposition::new(Arc::new(ControlService::new(h.ids.clone()))),
        power: Arc::new(NativePower),
        diagnostics: Arc::new(DiagnosticService::new(
            store,
            h.ids.clone(),
            h.clock(),
            None,
        )),
        git: None,
        browser: None,
        packages: None,
        use_cases: UseCases::new(),
        consumers: Vec::new(),
        indexing: None,
        config,
    }
}

fn admit(
    runtime: &Runtime,
    ctx: &InvocationContext,
    source: &ArtifactRef,
    key: &str,
) -> OperationId {
    let frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,"mode":"Command","requestKey":key,"body":{"kind":"conventions.import","scope":{"kind":"Workspace"},"source":source,"schema":model::manifest_schema()}});
    match runtime
        .command(ctx, "conventions.import", frame.to_string().as_bytes())
        .unwrap()
    {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

async fn until(what: &str, mut complete: impl FnMut() -> bool) {
    within(what, async {
        while !complete() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

fn captures(store: &dyn ApplicationStore, op: &OperationId) -> Vec<StoredConventionImportArtifact> {
    read(store, |r| {
        r.conventions().convention_import_artifacts(op, None, 500)
    })
}

fn cleanup_fault(h: &Harness, enabled: bool) {
    let db = rusqlite::Connection::open(h.root.join("workspace.sqlite")).unwrap();
    // The open store's writer or its maintenance may hold the lock; wait for it like the store does.
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db.execute_batch(if enabled {
        "CREATE TRIGGER refuse_private_import_cleanup BEFORE DELETE ON convention_import_artifacts BEGIN SELECT RAISE(ABORT,'controlled cleanup interruption'); END;"
    } else {
        "DROP TRIGGER refuse_private_import_cleanup;"
    }).unwrap();
}

#[test]
fn cleanup_pass_deletes_only_one_bounded_capture_page_and_keeps_queued_owners() {
    let f = Fx::new();
    let mut invalid = manifest(70);
    invalid["unknown"] = json!(true);
    let input = f.source(&serde_json::to_vec(&invalid).unwrap());
    let failed = f.admit(&input, "bounded-cleanup-failure");
    assert_eq!(f.dispatch(&failed).state, OperationState::Failed);
    let queued = f.admit(&input, "bounded-cleanup-live-owner");
    let original = read(&*f.store, |r| r.conventions().convention_import(&queued)).unwrap();
    assert_eq!(captures(&*f.store, &failed).len(), 72);
    for remaining in [40, 8, 0] {
        let progress = f.service.cleanup_terminal_page(None).unwrap();
        assert!(progress.released);
        assert!(progress.next.is_none());
        assert_eq!(captures(&*f.store, &failed).len(), remaining);
        let live = read(&*f.store, |r| r.conventions().convention_import(&queued)).unwrap();
        assert_eq!(live.phase, "Queued");
        assert_eq!(live.revision, original.revision);
        assert_eq!(live.checkpoint, original.checkpoint);
    }
    assert!(
        f.artifacts
            .read_exact(&*f.store, &f.ctx, &input, 65536)
            .is_ok()
    );
    f.service.stop();
    assert!(matches!(
        f.service.cleanup_terminal_page(None),
        Err(StorageError::Cancelled)
    ));
}

#[test]
fn runtime_failed_trailing_manifest_cleans_on_reopen_without_manual_recovery_and_preserves_history()
{
    let f = Fx::new();
    // Keep the terminal private captures durable through the first runtime's drop, like an
    // unavailable cleanup writer. The second boot must discover them without a manual call.
    cleanup_fault(&f.h, true);
    let mut invalid = manifest(70);
    invalid["unknown"] = json!(true);
    let private_body = "# Failed private body";
    invalid["markdown"] = json!({"text":private_body,"byteLength":private_body.len().to_string(),"sha256":devworks_core::fingerprint::sha256_hex(private_body.as_bytes())});
    let input_bytes = serde_json::to_vec(&invalid).unwrap();
    let input = f.source(&input_bytes);
    let good_input = f.source(&serde_json::to_vec(&manifest(1)).unwrap());
    let rt = runtime();
    let (failed, published, private_blob) = rt.block_on(async {
        let engine = boot(composition(&f.h, f.store.clone(), f.artifacts.clone()))
            .await
            .unwrap();
        let failed = admit(&engine, &f.ctx, &input, "runtime-failed-import");
        until("real runtime import fails after private staging", || {
            read(&*f.store, |r| r.operations().operation(&failed))
                .is_some_and(|op| op.state == OperationState::Failed)
        })
        .await;
        let private = captures(&*f.store, &failed);
        assert_eq!(
            private.len(),
            72,
            "all entries preceded the invalid trailing field"
        );
        let digest = private
            .iter()
            .find(|a| a.kind == "Markdown")
            .unwrap()
            .version
            .blob_digest
            .clone();
        let private_blob = read(&*f.store, |r| r.artifacts().blob(&digest)).unwrap();
        let published = admit(&engine, &f.ctx, &good_input, "runtime-published-import");
        until("real runtime good import publishes", || {
            read(&*f.store, |r| r.operations().operation(&published))
                .is_some_and(|op| op.state == OperationState::ObservedComplete)
        })
        .await;
        assert!(captures(&*f.store, &failed).len() > 32);
        drop(engine);
        (failed, published, private_blob)
    });
    drop(rt);
    let Fx {
        h,
        store,
        artifacts,
        service,
        catalog,
        ctx,
    } = f;
    drop(service);
    drop(catalog);
    drop(store);
    cleanup_fault(&h, false);
    let store = h.open();
    let good = read(&*store, |r| r.conventions().convention_import(&published)).unwrap();
    let owner = read(&*store, |r| {
        r.conventions()
            .convention(&good.convention_id, None, &[KnowledgeScope::Workspace])
    })
    .unwrap();
    let content: ArtifactRef = serde_json::from_value(owner.content_ref.clone().unwrap()).unwrap();
    runtime().block_on(async {
        let engine = boot(composition(&h, store.clone(), artifacts.clone()))
            .await
            .unwrap();
        until(
            "boot maintenance removes every failed private capture",
            || captures(&*store, &failed).is_empty(),
        )
        .await;
        assert_eq!(
            read(&*store, |r| r.conventions().convention_import(&failed))
                .unwrap()
                .phase,
            "Failed"
        );
        assert_eq!(
            read(&*store, |r| r.conventions().convention(
                &owner.id,
                Some(owner.revision),
                &[KnowledgeScope::Workspace]
            ))
            .unwrap()
            .content_ref,
            owner.content_ref
        );
        assert_eq!(
            artifacts
                .read_exact(&*store, &ctx, &input, 65536)
                .unwrap()
                .0,
            input_bytes
        );
        assert_eq!(
            artifacts
                .read_exact(&*store, &ctx, &content, 100)
                .unwrap()
                .0,
            b"# Actual\n\0body"
        );
        let files = NativeArtifactFiles::new(h.root.clone(), h.root.with_extension("import-cache"));
        until(
            "reopen cleanup wakes collection of the failed-only managed bytes",
            || {
                files
                    .inspect(&private_blob.digest, &private_blob.generation)
                    .unwrap()
                    .is_none()
            },
        )
        .await;
        assert_eq!(
            read(&*store, |r| r
                .artifacts()
                .live_references(&private_blob.digest)),
            0
        );
        assert!(
            !captures(&*store, &published).is_empty(),
            "published audit captures are retained"
        );
        engine.shutdown().await;
    });
}

#[test]
fn runtime_cancel_after_staging_releases_private_blobs_automatically_without_rewinding_live_owner()
{
    let h = Harness::new();
    let (started, waiting) = std::sync::mpsc::sync_channel(1);
    let files = Arc::new(GateFiles {
        native: NativeArtifactFiles::new(h.root.clone(), h.root.with_extension("import-cache")),
        calls: std::sync::atomic::AtomicUsize::new(0),
        started,
        release: std::sync::Mutex::new(false),
        changed: std::sync::Condvar::new(),
    });
    let _release = ReleaseGate(files.clone());
    let f = Fx::with_files(h, files.clone());
    let input = f.source(&serde_json::to_vec(&manifest(3)).unwrap());
    runtime().block_on(async {
        let engine = boot(composition(&f.h, f.store.clone(), f.artifacts.clone())).await.unwrap();
        let op = admit(&engine, &f.ctx, &input, "runtime-cancel-staged");
        tokio::task::spawn_blocking(move || waiting.recv_timeout(Duration::from_secs(30))).await.unwrap().unwrap();
        let draft = read(&*f.store, |r| r.conventions().convention_import(&op)).unwrap();
        let private = captures(&*f.store, &op);
        assert_eq!(draft.phase, "Parsing");
        assert!(!private.is_empty());
        let blob = read(&*f.store, |r| r.artifacts().blob(&private[0].version.blob_digest)).unwrap();
        // A complete maintenance pass with a live owner must not rewind/delete its checkpoint.
        let progress = engine.conventions().imports().cleanup_terminal_page(None).unwrap();
        assert!(!progress.released);
        assert_eq!(read(&*f.store, |r| r.conventions().convention_import(&op)).unwrap().checkpoint, draft.checkpoint);
        assert_eq!(captures(&*f.store, &op).len(), private.len());
        let saved = read(&*f.store, |r| r.operations().operation(&op)).unwrap();
        let frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,"mode":"Command","requestKey":"cancel-real-runtime","body":{"kind":"conventions.retire","operationId":op,"expectedRevision":saved.revision.to_string(),"scope":{"kind":"Workspace"}}});
        let reply = engine.command(&f.ctx, "conventions.retire", frame.to_string().as_bytes()).unwrap();
        assert!(matches!(reply, CommandReply::Recorded { ref receipt, .. } if matches!(receipt.decision, DomainDecision::Applied { .. })), "{reply:?}");
        *files.release.lock().unwrap() = true;
        files.changed.notify_all();
        until("actual cancelled owner settles and maintenance releases staged captures", || {
            read(&*f.store, |r| r.operations().operation(&op)).is_some_and(|o| o.state == OperationState::CancelledBeforeDispatch)
                && captures(&*f.store, &op).is_empty()
        }).await;
        until("released captures wake real managed blob collection", || {
            files.native.inspect(&blob.digest, &blob.generation).unwrap().is_none()
        }).await;
        assert_eq!(read(&*f.store, |r| r.artifacts().live_references(&blob.digest)), 0);
        assert!(f.artifacts.read_exact(&*f.store, &f.ctx, &input, 65536).is_ok());
        assert!(read(&*f.store, |r| r.conventions().convention(&draft.convention_id, None, &[KnowledgeScope::Workspace])).is_none());
        engine.shutdown().await;
    });
}
