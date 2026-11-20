use super::*;
use devworks_core::{
    ports::storage::{
        DocumentIndexStage, DocumentIndexVersion, ProjectPurgeRowAction, QueueState, ReadOptions,
        ReadSnapshot, StoreInfo, WriteJob, WriteOptions,
    },
    protocol::{
        Id, Revision,
        data::{Coverage, CoverageState},
        records::IndexRunRecord,
    },
    services::documents::{
        extraction::{DocumentExtractor, ExtractionControl, ExtractionInput, TextExtractor},
        indexing::{DocumentExtraction, DocumentIndexer, DocumentProcessor},
    },
};
use devworks_engine::documents::indexing::consumer;
use std::{
    sync::{
        Mutex,
        atomic::{AtomicI64, AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

pub(super) struct Processor {
    calls: AtomicUsize,
    revision: AtomicI64,
}

impl Processor {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            revision: AtomicI64::new(1),
        })
    }
}

impl DocumentProcessor for Processor {
    fn revision(&self) -> Revision {
        Revision::new(self.revision.load(Ordering::SeqCst)).unwrap()
    }
    fn extract(
        &self,
        input: ExtractionInput<'_>,
        control: &ExtractionControl<'_>,
    ) -> Result<DocumentExtraction, Id> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let blocks = TextExtractor::default()
            .extract(input, control)
            .map_err(|failure| id(&format!("Text{failure:?}")))?;
        Ok(DocumentExtraction {
            blocks,
            coverage: Coverage {
                state: CoverageState::Complete,
                gaps: vec![],
                more_gaps: false,
            },
        })
    }
}

/// Interposes only on the indexer's writes. The dispatcher, gateway and user actions continue
/// through the native SQLite writer independently.
struct BeforePublish {
    inner: Arc<dyn ApplicationStore>,
    writes: AtomicUsize,
    at_boundary: Option<mpsc::SyncSender<()>>,
    continue_publish: Option<Mutex<mpsc::Receiver<()>>>,
}

impl ApplicationStore for BeforePublish {
    fn info(&self) -> StoreInfo {
        self.inner.info()
    }
    fn submit_write(
        &self,
        options: WriteOptions,
        job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError> {
        if self.writes.fetch_add(1, Ordering::SeqCst) == 2 {
            let Some(signal) = &self.at_boundary else {
                return Err(StorageError::Busy);
            };
            signal.send(()).unwrap();
            self.continue_publish
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        }
        self.inner.submit_write(options, job)
    }
    fn read(
        &self,
        options: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.inner.read(options, work)
    }
}

pub(super) fn indexed(
    fixture: &mut Fixture<'_>,
    worker_store: Arc<dyn ApplicationStore>,
    processor: Arc<Processor>,
) -> Arc<DocumentIndexer> {
    let indexer = Arc::new(DocumentIndexer::new(
        worker_store,
        fixture.artifacts.clone(),
        processor,
    ));
    fixture.cases = UseCases::new()
        .with_artifacts(fixture.artifacts.clone())
        .with_document_library(Arc::new(
            DocumentLibrary::new(
                fixture.artifacts.clone(),
                fixture.h.ids.clone(),
                fixture.h.clock(),
            )
            .with_indexer(indexer.clone()),
        ));
    indexer
}

fn imported(fixture: &Fixture<'_>, note: &[u8]) -> DocumentDetail {
    seed::rows(
        fixture.store.as_ref(),
        &[
            seed::ProjectSeed::new("document-a"),
            seed::ProjectSeed::new("document-b"),
        ],
        &[],
    )
    .unwrap();
    let source = fixture.source("document-a", note);
    imported_result(fixture.import(&fixture.scoped("document-a"), "import-notes", &source, None))
}

pub(super) fn imported_result(reply: CommandReply<DomainDecision>) -> DocumentDetail {
    let OperationResult::DocumentsImport {
        value: DocumentResult::Current(detail),
    } = applied(reply)
    else {
        panic!("expected imported document")
    };
    detail
}

pub(super) fn index_frame(operation: &str, key: &str, doc: &DocumentDetail) -> Value {
    json!({
        "protocol":"1.0-draft.1","requestKey":key,"mode":"Command",
        "body":{"kind":operation,"documentId":doc.id,
            "versionId":doc.current_version.as_ref().unwrap().version_id,
            "expectedRevision":doc.revision}
    })
}

fn cancel_frame(key: &str, run: &Id) -> Value {
    json!({"protocol":"1.0-draft.1","requestKey":key,
        "command":{"kind":"Cancel","operationId":run}})
}

pub(super) fn run_id(reply: CommandReply<DomainDecision>) -> (IndexRunRecord, bool) {
    let CommandReply::Recorded {
        receipt, replayed, ..
    } = reply
    else {
        panic!("expected a receipt for local indexing: {reply:?}");
    };
    let DomainDecision::Applied { result } = receipt.decision else {
        panic!("expected a queued index run: {:?}", receipt.decision);
    };
    let (OperationResult::DocumentsIndex { value } | OperationResult::DocumentsRetry { value }) =
        result
    else {
        panic!("expected a typed index run: {result:?}")
    };
    (value, replayed)
}

fn state(fixture: &Fixture<'_>, version: &Id) -> DocumentIndexVersion {
    read(fixture.store.as_ref(), |r| {
        r.documents().document_index(version)
    })
    .unwrap()
}

pub(super) fn drive(fixture: &Fixture<'_>, indexer: Arc<DocumentIndexer>) {
    let dispatcher = dispatcher(fixture.h, fixture.store.clone(), vec![consumer(indexer)]);
    runtime()
        .block_on(within("document dispatcher", dispatcher.run_until_idle()))
        .unwrap();
}

async fn reach_publication_boundary(
    dispatcher: &devworks_engine::dispatch::Dispatcher,
    reached: mpsc::Receiver<()>,
) {
    within("claim document job", dispatcher.tick())
        .await
        .unwrap();
    within(
        "reach publication boundary",
        tokio::task::spawn_blocking(move || reached.recv_timeout(Duration::from_secs(8))),
    )
    .await
    .unwrap()
    .unwrap();
}

#[test]
fn receipt_replay_and_explicit_retry_preserve_one_exact_checkpoint() {
    let _case = native_runtime_case();
    let h = Harness::new();
    let mut f = Fixture::open(&h);
    let processor = Processor::new();
    let store = f.store.clone();
    let indexer = indexed(&mut f, store, processor.clone());
    let note = imported(&f, b"# Project notes\nAn exact passage.");
    let a = f.scoped("document-a");
    let frame = index_frame("documents.index", "index-this", &note);
    let (one, replay) = run_id(f.command(&a, "documents.index", frame.clone()));
    assert!(!replay);
    let (same, replay) = run_id(f.command(&a, "documents.index", frame));
    assert!(replay);
    assert_eq!(one.id, same.id);
    assert_eq!(
        one.state,
        devworks_core::protocol::OperationState::IntentRecorded
    );
    assert!(read(f.store.as_ref(), |r| r.queue().entry(&one.id)).is_some());
    drive(&f, indexer.clone());
    let exact = note.current_version.as_ref().unwrap();
    let ready = state(&f, &exact.version_id);
    assert_eq!(ready.stage, DocumentIndexStage::Ready);
    let (head, extraction) = ready.published.unwrap();
    assert_eq!(head, one.id);
    let saved = read(f.store.as_ref(), |r| {
        r.documents()
            .document_checkpoint(&exact.version_id, &extraction)
    })
    .unwrap();
    assert_eq!(saved.source_digest, exact.sha256.as_str());
    let expected = TextExtractor::default()
        .extract(
            ExtractionInput {
                version: exact,
                revision: &extraction,
                bytes: b"# Project notes\nAn exact passage.",
            },
            &ExtractionControl {
                cancelled: &|| false,
                deadline: std::time::Instant::now() + Duration::from_secs(2),
            },
        )
        .unwrap();
    let passage = read(f.store.as_ref(), |r| {
        r.documents()
            .document_passage(&saved.id, &expected[0].citation.block_id)
    })
    .unwrap();
    assert_eq!(passage.text, expected[0].text);
    let (second, _) = run_id(f.command(
        &a,
        "documents.retry",
        index_frame("documents.retry", "retry-this", &note),
    ));
    drive(&f, indexer);
    assert_eq!(state(&f, &exact.version_id).published.unwrap().0, second.id);
    assert_eq!(processor.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn invalid_native_text_reports_a_terminal_failure_without_an_index() {
    use devworks_core::protocol::records::ItemState;
    let _case = native_runtime_case();
    let h = Harness::new();
    let mut f = Fixture::open(&h);
    let store = f.store.clone();
    let indexer = indexed(&mut f, store, Processor::new());
    let broken = imported(&f, b"\xffinvalid text");
    let exact = broken.current_version.as_ref().unwrap();
    let a = f.scoped("document-a");
    let (first, _) = run_id(f.command(
        &a,
        "documents.index",
        index_frame("documents.index", "bad-source", &broken),
    ));
    drive(&f, indexer.clone());
    let status = state(&f, &exact.version_id);
    assert_eq!(status.stage, DocumentIndexStage::Failed);
    assert_eq!(status.diagnostic, Some(id("TextInvalidEncoding")));
    assert!(status.published.is_none());
    let detail = result(f.read_ref(&a, "documents.read", exact, 10));
    assert!(
        matches!(detail, OperationResult::DocumentsRead { value: DocumentResult::Current(detail) }
        if matches!(detail.extraction_state, ItemState::Failed)
            && detail.coverage.gaps == [id("TextInvalidEncoding")])
    );
    let first_entry = read(f.store.as_ref(), |r| r.queue().entry(&first.id)).unwrap();
    assert_eq!(first_entry.state, QueueState::Failed);
    assert_eq!(
        first_entry.last_error.as_deref(),
        Some("TextInvalidEncoding")
    );
    let (retry, _) = run_id(f.command(
        &a,
        "documents.retry",
        index_frame("documents.retry", "bad-source-retry", &broken),
    ));
    drive(&f, indexer);
    assert_eq!(state(&f, &exact.version_id).latest, retry.id);
    let status = f.command(
        &a,
        "indexing.cancel",
        cancel_frame("cancel-ended-run", &first.id),
    );
    let CommandReply::Recorded { receipt, .. } = status else {
        panic!("{status:?}")
    };
    let DomainDecision::NoChange {
        result: OperationResult::IndexingCancel { value },
    } = receipt.decision
    else {
        panic!("{:?}", receipt.decision)
    };
    assert_eq!(value.id, first.id);
    assert_eq!(value.state, devworks_core::protocol::OperationState::Failed);
    assert_eq!(value.diagnostics.0[0].code.0, "TextInvalidEncoding");
}

#[test]
fn native_store_restarts_between_checkpoint_and_publish_without_reextracting() {
    let _case = native_runtime_case();
    let h = Harness::new();
    let processor = Processor::new();
    let (version, run) = {
        let mut f = Fixture::open(&h);
        let intercepted = Arc::new(BeforePublish {
            inner: f.store.clone(),
            writes: AtomicUsize::new(0),
            at_boundary: None,
            continue_publish: None,
        });
        let indexer = indexed(&mut f, intercepted, processor.clone());
        let note = imported(&f, b"# Retained\nSurvives a stopped writer.");
        let a = f.scoped("document-a");
        let (run, _) = run_id(f.command(
            &a,
            "documents.index",
            index_frame("documents.index", "index-once", &note),
        ));
        drive(&f, indexer);
        let version = note.current_version.unwrap().version_id;
        let staged = state(&f, &version);
        assert_eq!(staged.stage, DocumentIndexStage::Extracted);
        assert!(staged.published.is_none());
        assert_eq!(
            read(f.store.as_ref(), |r| r.queue().entry(&run.id))
                .unwrap()
                .state,
            QueueState::Ready
        );
        (version, run.id)
    };
    h.wait_released();
    h.advance(5_000);
    let mut restarted = Fixture::open(&h);
    let store = restarted.store.clone();
    let indexer = indexed(&mut restarted, store, processor.clone());
    drive(&restarted, indexer);
    let published = state(&restarted, &version);
    assert_eq!(published.stage, DocumentIndexStage::Ready);
    assert_eq!(published.published.unwrap().0, run);
    assert_eq!(processor.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn cancellation_after_extract_leaves_the_prior_publication_and_retained_blocks() {
    let _case = native_runtime_case();
    let h = Harness::new();
    let mut f = Fixture::open(&h);
    let processor = Processor::new();
    let store = f.store.clone();
    let initial = indexed(&mut f, store, processor.clone());
    let note = imported(&f, b"# Before cancel\nKeep this index.");
    let a = f.scoped("document-a");
    let exact = note.current_version.as_ref().unwrap();
    let (first, _) = run_id(f.command(
        &a,
        "documents.index",
        index_frame("documents.index", "first", &note),
    ));
    drive(&f, initial);
    processor.revision.store(2, Ordering::SeqCst);
    let (at, reached) = mpsc::sync_channel(1);
    let (release, again) = mpsc::sync_channel(1);
    let worker = Arc::new(BeforePublish {
        inner: f.store.clone(),
        writes: AtomicUsize::new(0),
        at_boundary: Some(at),
        continue_publish: Some(Mutex::new(again)),
    });
    let indexer = indexed(&mut f, worker, processor.clone());
    let (retry, _) = run_id(f.command(
        &a,
        "documents.retry",
        index_frame("documents.retry", "second", &note),
    ));
    let dispatcher = dispatcher(&h, f.store.clone(), vec![consumer(indexer)]);
    let runtime = runtime();
    runtime.block_on(reach_publication_boundary(&dispatcher, reached));
    assert_eq!(
        state(&f, &exact.version_id).stage,
        DocumentIndexStage::Extracted
    );
    let OperationResult::IndexingCancel { value } = applied(f.command(
        &a,
        "indexing.cancel",
        cancel_frame("cancel-in-flight", &retry.id),
    )) else {
        panic!("expected native index cancellation")
    };
    assert_eq!(value.id, retry.id);
    assert_eq!(value.stage.0, "Cancelling");
    release.send(()).unwrap();
    runtime.block_on(within("cancel reindex", dispatcher.idle()));
    let view = state(&f, &exact.version_id);
    assert_eq!(view.stage, DocumentIndexStage::Cancelled);
    assert_eq!(view.published.unwrap().0, first.id);
    assert!(
        read(f.store.as_ref(), |r| r.documents().document_checkpoint(
            &exact.version_id,
            &Revision::new(2).unwrap()
        ))
        .is_some()
    );
    assert_eq!(
        read(f.store.as_ref(), |r| r.queue().entry(&retry.id))
            .unwrap()
            .state,
        QueueState::Cancelled
    );
}

#[derive(Clone, Copy)]
enum StaleSource {
    Replacement,
    Move,
    Deletion,
    ExtractionRevision,
}

#[test]
fn stale_sources_and_configuration_cannot_publish_after_checkpoint() {
    for change in [
        StaleSource::Replacement,
        StaleSource::Move,
        StaleSource::Deletion,
        StaleSource::ExtractionRevision,
    ] {
        check_stale_source(change);
    }
}

fn check_stale_source(change: StaleSource) {
    let _case = native_runtime_case();
    let h = Harness::new();
    let mut f = Fixture::open(&h);
    let (at, reached) = mpsc::sync_channel(1);
    let (release, again) = mpsc::sync_channel(1);
    let worker = Arc::new(BeforePublish {
        inner: f.store.clone(),
        writes: AtomicUsize::new(0),
        at_boundary: Some(at),
        continue_publish: Some(Mutex::new(again)),
    });
    let processor = Processor::new();
    let indexer = indexed(&mut f, worker, processor.clone());
    let one = imported(&f, b"# Earlier\nHistorical text.");
    let exact = one.current_version.as_ref().unwrap();
    let a = f.scoped("document-a");
    let (run, _) = run_id(f.command(
        &a,
        "documents.index",
        index_frame("documents.index", "earlier", &one),
    ));
    let d = dispatcher(&h, f.store.clone(), vec![consumer(indexer)]);
    let rt = runtime();
    rt.block_on(reach_publication_boundary(&d, reached));
    let current =
        match change {
            StaleSource::Replacement => {
                let source = f.source("document-a", b"# Current\nReplacement text.");
                let new = imported_result(f.import(&a, "replace-notes", &source, Some(&one)));
                let current = new.current_version.unwrap();
                assert_ne!(exact.version_id, current.version_id);
                Some(current.version_id)
            }
            StaleSource::Move => {
                let moved = applied(f.command(&f.human, "documents.move", json!({
                "protocol":"1.0-draft.1","schemaRevision":10,"requestKey":"move-index-source",
                "command":{"kind":"Move","artifactId":one.id,"targetProjectId":"document-b",
                    "expectedScopeRevision":one.scope_revision}
            })));
                assert!(matches!(moved, OperationResult::DocumentsMove { .. }));
                assert!(matches!(
                    f.read_ref(&a, "documents.read", exact, 10),
                    DomainQueryReply::Rejected { .. }
                ));
                None
            }
            StaleSource::Deletion => {
                f.delete(exact, "delete-index-source");
                None
            }
            StaleSource::ExtractionRevision => {
                processor.revision.store(2, Ordering::SeqCst);
                None
            }
        };
    release.send(()).unwrap();
    rt.block_on(within("settle stale index", d.idle()));
    if matches!(change, StaleSource::Deletion) {
        let (index, checkpoint) = read(f.store.as_ref(), |r| {
            Ok((
                r.documents().document_index(&exact.version_id)?,
                r.documents()
                    .document_checkpoint(&exact.version_id, &Revision::new(1).unwrap())?,
            ))
        });
        assert!(index.is_none() && checkpoint.is_none());
        return;
    }
    let old = state(&f, &exact.version_id);
    assert_eq!(old.latest, run.id);
    assert_eq!(old.stage, DocumentIndexStage::Failed);
    assert_eq!(old.diagnostic, Some(id("SourceChanged")));
    assert!(old.published.is_none());
    if let Some(current) = current {
        assert!(read(f.store.as_ref(), |r| r.documents().document_index(&current)).is_none());
    }
    assert!(
        read(f.store.as_ref(), |r| r.documents().document_checkpoint(
            &exact.version_id,
            &Revision::new(1).unwrap()
        ))
        .is_some()
    );
}

pub(super) fn management_frame(kind: &str, request: &str, doc: &DocumentDetail) -> Value {
    json!({"protocol":"1.0-draft.1","schemaRevision":10,"requestKey":request,
        "command":{"kind":kind,"artifactId":doc.id,"expectedRevision":doc.revision}})
}

#[test]
fn disable_and_remove_preserve_extraction_until_explicit_version_deletion() {
    let _case = native_runtime_case();
    let h = Harness::new();
    let mut f = Fixture::open(&h);
    let store = f.store.clone();
    let indexer = indexed(&mut f, store, Processor::new());
    let note = imported(&f, b"# Evidence\nThis text is retained.");
    let exact = note.current_version.as_ref().unwrap();
    let a = f.scoped("document-a");
    run_id(f.command(
        &a,
        "documents.index",
        index_frame("documents.index", "initial", &note),
    ));
    drive(&f, indexer);
    let before = state(&f, &exact.version_id).published.unwrap();
    let OperationResult::DocumentsDisableRetrieval {
        value: DocumentResult::Current(disabled),
    } = applied(f.command(
        &a,
        "documents.disable_retrieval",
        management_frame("DisableRetrieval", "disable", &note),
    ))
    else {
        panic!("expected disabled retrieval")
    };
    assert!(!disabled.retrieval_enabled);
    assert_eq!(state(&f, &exact.version_id).published, Some(before.clone()));
    let OperationResult::DocumentsRemoveIndex {
        value: DocumentResult::Current(removed),
    } = applied(f.command(
        &a,
        "documents.remove_index",
        management_frame("RemoveIndex", "remove", &disabled),
    ))
    else {
        panic!("expected removed index")
    };
    assert!(!removed.retrieval_enabled);
    assert!(matches!(
        removed.extraction_state,
        devworks_core::protocol::records::ItemState::Healthy
    ));
    assert!(matches!(removed.coverage.state, CoverageState::Complete));
    let detail = result(f.read_ref(&a, "documents.read", exact, 10));
    assert!(
        matches!(detail, OperationResult::DocumentsRead { value: DocumentResult::Current(detail) }
        if matches!(detail.extraction_state, devworks_core::protocol::records::ItemState::Healthy)
            && matches!(detail.coverage.state, CoverageState::Complete))
    );
    let view = state(&f, &exact.version_id);
    assert_eq!(view.stage, DocumentIndexStage::Removed);
    assert!(view.published.is_none());
    assert!(
        read(f.store.as_ref(), |r| r
            .documents()
            .document_checkpoint(&exact.version_id, &before.1))
        .is_some()
    );
    f.delete(exact, "delete-exact-original");
    assert!(
        read(f.store.as_ref(), |r| r
            .documents()
            .document_index(&exact.version_id))
        .is_none()
    );
    assert!(
        read(f.store.as_ref(), |r| r
            .documents()
            .document_checkpoint(&exact.version_id, &before.1))
        .is_none()
    );
}

#[test]
fn purge_planning_preserves_a_shared_documents_extraction_and_index() {
    let _case = native_runtime_case();
    let h = Harness::new();
    let mut f = Fixture::open(&h);
    let store = f.store.clone();
    let indexer = indexed(&mut f, store, Processor::new());
    let note = imported(
        &f,
        b"# Shared checkpoint\nThe original owns all derived rows.",
    );
    let a = f.scoped("document-a");
    let b = f.scoped("document-b");
    run_id(f.command(
        &a,
        "documents.index",
        index_frame("documents.index", "index-shared", &note),
    ));
    drive(&f, indexer);
    let a_id = ProjectId(id("document-a"));
    let verify_plan = |action| {
        let plan = read(f.store.as_ref(), |r| r.projects().purge_planning(&a_id)).unwrap();
        for table in [
            "document_extracted_blocks",
            "document_extractions",
            "document_index_versions",
            "document_lexical_rows",
            "document_retrieval_preferences",
        ] {
            let rows: Vec<_> = plan.rows.iter().filter(|row| row.table == table).collect();
            assert!(!rows.is_empty(), "{table}: {:?}", plan.unresolved);
            assert!(
                rows.iter().all(|row| row.action == action),
                "{table}: {rows:?}"
            );
        }
    };
    verify_plan(ProjectPurgeRowAction::Delete);
    let shared = applied(f.command(
        &f.human,
        "documents.share",
        json!({
            "protocol":"1.0-draft.1","schemaRevision":10,"requestKey":"share-indexed",
            "command":{"kind":"Share","artifactId":note.id,"targetProjectId":"document-b",
                "expectedScopeRevision":note.scope_revision}
        }),
    ));
    let OperationResult::DocumentsShare {
        value: DocumentResult::Current(shared),
    } = shared
    else {
        panic!("expected shared document")
    };
    let attempt = f.command(
        &b,
        "documents.index",
        index_frame("documents.index", "foreign-index", &shared),
    );
    assert!(
        matches!(&attempt, CommandReply::Recorded { receipt, .. } if matches!(&receipt.decision,
        DomainDecision::Refused { fault } if fault.code == DomainErrorCode::PermissionDenied)),
        "{attempt:?}"
    );
    verify_plan(ProjectPurgeRowAction::Keep);
}
