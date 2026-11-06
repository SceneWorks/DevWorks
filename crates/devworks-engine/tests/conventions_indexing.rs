//! Real SQLite and managed bytes. These tests do not execute a learned model or live HTTP.
mod common;
use common::*;
use devworks_adapters::fs::{
    NativeFilesystem, artifacts::NativeArtifactFiles, sources::HostArtifactSources,
};
use devworks_core::{
    context::PrincipalKind,
    ports::storage::*,
    services::{
        artifacts::ArtifactService,
        conventions::indexing::{
            IndexService, embedding::EmbeddingOrigin, preparation::InputPreparation,
        },
    },
};
use std::sync::Arc;

fn index_service(
    h: &Harness,
    store: Arc<dyn ApplicationStore>,
) -> (Arc<ArtifactService>, IndexService) {
    let artifacts = Arc::new(ArtifactService::new(
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.with_extension("cache"),
        )),
        Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
        Arc::new(NativeFilesystem::new()),
        h.clock(),
        h.ids.clone(),
    ));
    let index = IndexService::new(store, artifacts.clone(), h.ids.clone(), h.clock());
    (artifacts, index)
}
fn space(store: &dyn ApplicationStore) {
    write(store, |tx| {
        tx.conventions()
            .put_convention_embedding_space(&StoredConventionEmbeddingSpace {
                id: "actual-space".into(),
                provider: "fixture-endpoint".into(),
                model: "recorded-vector-model".into(),
                configuration_digest: "a".repeat(64),
                dimensions: 2,
                metric: "Cosine".into(),
                element_type: "Float32".into(),
            })
    })
    .unwrap();
}
#[test]
fn exact_input_publication_keeps_original_actor_and_dispatching_owner_across_reopen() {
    let h = Harness::new();
    let store = h.open();
    space(store.as_ref());
    let ctx = principal(store.as_ref(), "query-agent", PrincipalKind::Agent);
    let (artifacts, index) = index_service(&h, store.clone());
    let origin = EmbeddingOrigin::Query {
        query_id: id("actual-query"),
        fingerprint: "b".repeat(64),
        scope_digest: "c".repeat(64),
    };
    let cancellation = Cancellation::new();
    let prepare = || {
        index
            .prepare_embedding_inputs(InputPreparation {
                context: &ctx,
                origin: &origin,
                embedding_space: "actual-space",
                configuration_source_digest: &"d".repeat(64),
                texts: &["exact first\ninput", "second\0input"],
                fences: &[],
                cancellation: &cancellation,
                deadline_ms: None,
            })
            .unwrap()
    };
    let first = prepare();
    let replay = prepare();
    assert_eq!(first.request_id, replay.request_id);
    assert_eq!(
        serde_json::json!(first.input_refs),
        serde_json::json!(replay.input_refs)
    );
    assert_eq!(first.context, ctx);
    let operation = read(store.as_ref(), |s| {
        s.operations().operation(&first.operation_id)
    })
    .unwrap();
    assert_eq!(
        operation.state,
        devworks_core::protocol::OperationState::Dispatching,
        "input bytes do not complete actual embedding preparation"
    );
    assert_eq!(
        operation.payload["sourceContext"]["invocationContext"]["principal"],
        serde_json::json!(ctx.principal())
    );
    assert_eq!(
        artifacts
            .read_exact(store.as_ref(), &ctx, &first.input_refs[1], 1024)
            .unwrap()
            .0,
        b"second\0input"
    );
    let saved_id = first.request_id.clone();
    let row = read(store.as_ref(), |s| {
        s.conventions().convention_embedding_inputs(&saved_id)
    })
    .unwrap();
    assert_eq!(row.actor_id, *ctx.principal());
    drop(index);
    drop(artifacts);
    drop(store);
    let store = h.open();
    let (_, index) = index_service(&h, store.clone());
    let second = index
        .prepare_embedding_inputs(InputPreparation {
            context: &ctx,
            origin: &origin,
            embedding_space: "actual-space",
            configuration_source_digest: &"d".repeat(64),
            texts: &["exact first\ninput", "second\0input"],
            fences: &[],
            cancellation: &cancellation,
            deadline_ms: None,
        })
        .unwrap();
    assert_eq!(second.request_id, first.request_id);
    assert_eq!(
        serde_json::json!(second.input_refs),
        serde_json::json!(first.input_refs)
    );
    let changed = index
        .prepare_embedding_inputs(InputPreparation {
            context: &ctx,
            origin: &origin,
            embedding_space: "actual-space",
            configuration_source_digest: &"e".repeat(64),
            texts: &["exact first\ninput", "second\0input"],
            fences: &[],
            cancellation: &cancellation,
            deadline_ms: None,
        })
        .unwrap();
    assert_ne!(
        changed.request_id, first.request_id,
        "selection ABA revision cannot reuse an old preparation identity"
    );
    assert_ne!(changed.operation_id, first.operation_id);
}
#[test]
fn cancelled_or_stopped_preparation_never_creates_an_input_fact() {
    let h = Harness::new();
    let store = h.open();
    space(store.as_ref());
    let ctx = principal(store.as_ref(), "reader", PrincipalKind::Human);
    let (_, index) = index_service(&h, store.clone());
    let origin = EmbeddingOrigin::Query {
        query_id: id("actual-query"),
        fingerprint: "b".repeat(64),
        scope_digest: "c".repeat(64),
    };
    let cancellation = Cancellation::new();
    cancellation.cancel();
    let prepare = |c: &Cancellation| {
        index.prepare_embedding_inputs(InputPreparation {
            context: &ctx,
            origin: &origin,
            embedding_space: "actual-space",
            configuration_source_digest: &"d".repeat(64),
            texts: &["never submitted"],
            fences: &[],
            cancellation: c,
            deadline_ms: None,
        })
    };
    assert!(matches!(
        prepare(&cancellation),
        Err(StorageError::Cancelled)
    ));
    index.stop();
    assert!(matches!(
        prepare(&Cancellation::new()),
        Err(StorageError::Cancelled)
    ));
}

#[test]
fn shutdown_drains_the_actual_writer_ack_without_a_reentrant_lock_cycle() {
    use devworks_core::services::conventions::indexing::lifecycle::PublicationFence;
    use std::sync::mpsc;
    let h = Harness::new();
    let store = h.open();
    let fence = Arc::new(PublicationFence::default());
    let (entered, waiting) = mpsc::sync_channel(1);
    let (release, resume) = mpsc::sync_channel(1);
    let worker_store = store.clone();
    let worker_fence = fence.clone();
    let writer = std::thread::spawn(move || {
        let _phase = worker_fence.enter().unwrap();
        write(worker_store.as_ref(), move |tx| {
            event(tx, "NeverCommittedIndex")?;
            entered.send(()).unwrap();
            resume.recv().unwrap();
            // The writer must be able to consult lifecycle state while shutdown waits for
            // this SAME transaction's acknowledgment. A writer-preferring RwLock deadlocks.
            assert!(worker_fence.is_closed());
            Err::<(), _>(StorageError::Cancelled)
        })
    });
    waiting
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let (closed, closing) = mpsc::sync_channel(1);
    let closer = fence.clone();
    let shutdown = std::thread::spawn(move || {
        closer.begin_close();
        closed.send(()).unwrap();
        closer.drain();
    });
    closing
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(matches!(fence.enter(), Err(StorageError::Cancelled)));
    release.send(()).unwrap();
    assert!(matches!(
        writer.join().unwrap(),
        Err(StorageError::Cancelled)
    ));
    shutdown.join().unwrap();
    assert_eq!(
        read(store.as_ref(), |s| Ok(s
            .history()
            .page(
                &EventFilter::Aggregate {
                    kind: "fixture".into(),
                    id: "fixture".into()
                },
                0,
                50
            )?
            .events
            .len())),
        0
    );
    // The successful next transaction proves rollback completed and the writer remains usable.
    write(store.as_ref(), |tx| event(tx, "WriterReusable")).unwrap();
}
