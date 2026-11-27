//! Drive the real qualified router, producer journal, SQLite vector index and document gateway
//! together. Transport replies are recorded vectors and usage, with no learned execution.
use super::{
    convention_search::{VECTOR, push, qualify},
    *,
};
use devworks_core::{
    authority::HostAuthority,
    gateway::Gateway,
    protocol::{
        CommandReply, DomainErrorCode, OperationState, ProjectId,
        data::{Coverage, CoverageState, EmbeddingSpace, RetrievalMode, RetrievalPage},
        records::DocumentDetail,
        results::{DomainDecision, DomainQueryReply, OperationResult},
    },
    services::{
        artifacts::host::HostArtifactPublication,
        conventions::indexing::embedding::{
            ConventionEmbeddings, EmbeddedBatch, EmbeddingConfiguration, EmbeddingFailure,
            EmbeddingRequest,
        },
        documents::{
            DocumentLibrary,
            extraction::{DocumentExtractor, ExtractionControl, ExtractionInput, TextExtractor},
            indexing::{DocumentExtraction, DocumentIndexer, DocumentProcessor},
        },
        router::embedding_spaces,
    },
};
use devworks_engine::{
    conventions::embedding::NativeEmbeddings, documents::indexing::consumer as document_consumer,
};
use devworks_test_support::seed;

struct TextDocuments;
impl DocumentProcessor for TextDocuments {
    fn revision(&self) -> Revision {
        Revision::new(1).unwrap()
    }

    fn extract(
        &self,
        input: ExtractionInput<'_>,
        control: &ExtractionControl<'_>,
    ) -> Result<DocumentExtraction, Id> {
        Ok(DocumentExtraction {
            blocks: TextExtractor::default()
                .extract(input, control)
                .map_err(|error| id(&format!("Text{error:?}")))?,
            vision_targets: vec![],
            coverage: Coverage {
                state: CoverageState::Complete,
                gaps: vec![],
                more_gaps: false,
            },
        })
    }
}

struct LoseFirstAcknowledgement {
    native: Arc<NativeEmbeddings>,
    first: AtomicBool,
    first_observation: AtomicBool,
    send_calls: AtomicUsize,
}
impl ConventionEmbeddings for LoseFirstAcknowledgement {
    fn is_available(&self) -> bool {
        self.native.is_available()
    }

    fn configured_space(
        &self,
        ctx: &InvocationContext,
    ) -> Result<EmbeddingConfiguration, EmbeddingFailure> {
        self.native.configured_space(ctx)
    }

    fn embed(
        &self,
        ctx: &InvocationContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddedBatch, EmbeddingFailure> {
        self.send_calls.fetch_add(1, Ordering::SeqCst);
        let result = self.native.embed(ctx, request)?;
        if self.first.swap(false, Ordering::SeqCst) {
            Err(EmbeddingFailure::Unknown {
                request_id: result.request_id,
                reason: "Lost acknowledgement after the native journal settled".into(),
            })
        } else {
            Ok(result)
        }
    }

    fn observe(
        &self,
        ctx: &InvocationContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddedBatch, EmbeddingFailure> {
        if self.first_observation.swap(false, Ordering::SeqCst) {
            return Err(EmbeddingFailure::Unknown {
                request_id: request.request_id,
                reason: "The original closed outcome is not visible on the first observation"
                    .into(),
            });
        }
        self.native.observe(ctx, request)
    }
}

pub(super) fn command(
    f: &Fx,
    cases: &UseCases,
    ctx: &InvocationContext,
    operation: &str,
    frame: Value,
) -> OperationResult {
    let authority = HostAuthority {
        store: f.store.as_ref(),
        clock: &*f.h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        store: f.store.as_ref(),
        ids: &*f.h.ids,
        authority: &authority,
    };
    let reply = cases
        .command(&gateway, ctx, operation, frame.to_string().as_bytes())
        .unwrap();
    let CommandReply::Recorded { receipt, .. } = reply else {
        panic!("{reply:?}")
    };
    let DomainDecision::Applied { result } = receipt.decision else {
        panic!("{:?}", receipt.decision)
    };
    result
}

pub(super) fn inspect_document(
    f: &Fx,
    cases: &UseCases,
    ctx: &InvocationContext,
    source: &ArtifactRef,
    revision: u32,
) -> DocumentDetail {
    let authority = HostAuthority {
        store: f.store.as_ref(),
        clock: &*f.h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        store: f.store.as_ref(),
        ids: &*f.h.ids,
        authority: &authority,
    };
    let raw = json!({"protocol":"1.0-draft.1","queryId":"inspect-version","schemaRevision":revision,
        "query":{"kind":"Read","artifactId":source.artifact_id,"versionId":source.version_id},"limit":20});
    let reply = cases
        .query(&gateway, ctx, "documents.read", raw.to_string().as_bytes())
        .unwrap();
    let DomainQueryReply::Result {
        result:
            OperationResult::DocumentsRead {
                value: devworks_core::protocol::records::DocumentResult::Current(value),
            },
        ..
    } = reply
    else {
        panic!("{reply:?}")
    };
    value
}

pub(super) fn search(
    f: &Fx,
    cases: &UseCases,
    ctx: &InvocationContext,
    key: &str,
    input: Value,
    policy: (&str, bool),
    exact: &[&ArtifactRef],
) -> DomainQueryReply {
    let authority = HostAuthority {
        store: f.store.as_ref(),
        clock: &*f.h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        store: f.store.as_ref(),
        ids: &*f.h.ids,
        authority: &authority,
    };
    let versions = if exact.is_empty() {
        json!({"kind":"Current"})
    } else {
        json!({"kind":"Exact","versions":exact})
    };
    let (mode, fallback) = policy;
    let frame = json!({"protocol":"1.0-draft.1","queryId":key,
        "scope":{"kind":"Project","projectId":ctx.project_id()},"sources":["Document"],
        "versions":versions,"input":input,"mode":mode,"lexicalFallback":fallback,
        "limits":{"maxResults":10,"maxVisited":1000,"deadlineMs":10000}});
    cases
        .query(
            &gateway,
            ctx,
            "documents.search",
            frame.to_string().as_bytes(),
        )
        .unwrap()
}

pub(super) fn page(reply: DomainQueryReply) -> RetrievalPage {
    let DomainQueryReply::Result {
        result: OperationResult::DocumentsSearch { value },
        ..
    } = reply
    else {
        panic!("{reply:?}")
    };
    value
}

fn artifact(f: &Fx, ctx: &InvocationContext, bytes: &[u8], media: &'static str) -> ArtifactRef {
    let files = f.artifacts.clone();
    let staged = files
        .stage_host_bytes(f.store.as_ref(), bytes, json!({"kind":"DocumentFixture"}))
        .unwrap()
        .unwrap();
    let (context, pending, publisher) = (ctx.clone(), staged.clone(), files.clone());
    let stored = write(f.store.as_ref(), move |tx| {
        publisher.commit_host_bytes_in_scope(
            tx,
            &pending,
            HostArtifactPublication {
                title: "Document fixture",
                media_type: media,
                provenance: json!({"kind":"Fixture"}),
                scope: ArtifactScope::Project(context.project_id().unwrap().clone()),
                owner: context.principal().clone(),
            },
        )
    })
    .unwrap();
    files.settle_host_bytes(f.store.as_ref(), &staged, true);
    version_ref(&stored)
}

pub(super) fn import(
    f: &Fx,
    cases: &UseCases,
    ctx: &InvocationContext,
    key: &str,
    bytes: &[u8],
    old: Option<&DocumentDetail>,
) -> DocumentDetail {
    let mut body = json!({"kind":"documents.import",
        "scope":{"kind":"Project","projectId":ctx.project_id()},
        "title":key,"source":artifact(f,ctx,bytes,"text/markdown")});
    if let Some(old) = old {
        body["replaceDocumentId"] = json!(old.id);
        body["expectedRevision"] = json!(old.revision);
    }
    let result = command(
        f,
        cases,
        ctx,
        "documents.import",
        json!({"protocol":"1.0-draft.1",
        "schemaRevision":25,"mode":"Command","requestKey":key,"body":body}),
    );
    let OperationResult::DocumentsImport {
        value: devworks_core::protocol::records::DocumentResult::Current(value),
    } = result
    else {
        panic!("{result:?}")
    };
    value
}

fn captured(
    f: &Fx,
    kind: &str,
) -> Vec<(
    OperationRecord,
    StoredInferenceAttempt,
    StoredAttemptBalance,
)> {
    read(f.store.as_ref(), |read| {
        let prepared = read.operations().operations_for_capability(
            "conventions.embedding_prepare",
            &[OperationState::ObservedComplete],
            None,
            50,
        )?;
        prepared
            .into_iter()
            .filter(|row| row.payload["origin"]["kind"] == kind)
            .map(|row| {
                let request: Id = serde_json::from_value(row.payload["requestId"].clone()).unwrap();
                let route = read.router().route_request(&request)?.unwrap();
                let journal = read.operations().operation(&route.operation_id)?.unwrap();
                assert_eq!(journal.attempts.len(), 1);
                let attempt = read
                    .usage()
                    .inference_attempt(&journal.attempts[0].id.0)?
                    .unwrap();
                let balance = read.usage().attempt_balance(&attempt.attempt_id)?.unwrap();
                Ok((row, attempt, balance))
            })
            .collect()
    })
}

pub(super) fn sent(f: &Fx) -> usize {
    f.http.lock().unwrap().requests.lock().unwrap().len()
}

#[test]
fn real_document_index_and_hybrid_search_account_once_and_scope_exact_vectors() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (human, native, embeddings) = qualify(&f);
    seed::rows(
        f.store.as_ref(),
        &[
            seed::ProjectSeed::new("documents-a"),
            seed::ProjectSeed::new("documents-b"),
        ],
        &[],
    )
    .unwrap();
    let a = human
        .clone()
        .with_lineage(Some(ProjectId(id("documents-a"))), None, None);
    let b = human
        .clone()
        .with_lineage(Some(ProjectId(id("documents-b"))), None, None);
    let controlled = Arc::new(LoseFirstAcknowledgement {
        native,
        first: AtomicBool::new(true),
        first_observation: AtomicBool::new(true),
        send_calls: AtomicUsize::new(0),
    });
    embeddings.set_embeddings(Some(controlled.clone()));
    let indexer = Arc::new(
        DocumentIndexer::new(
            f.store.clone(),
            f.artifacts.clone(),
            Arc::new(TextDocuments),
        )
        .with_embeddings(embeddings),
    );
    let cases = UseCases::new().with_document_library(Arc::new(
        DocumentLibrary::new(f.artifacts.clone(), f.h.ids.clone(), f.h.clock())
            .with_indexer(indexer.clone()),
    ));
    let note = import(
        &f,
        &cases,
        &a,
        "original",
        b"Exact botanical passage.",
        None,
    );
    let exact = note.current_version.as_ref().unwrap();
    let result = command(
        &f,
        &cases,
        &a,
        "documents.index",
        json!({"protocol":"1.0-draft.1",
        "mode":"Command","requestKey":"native-index","body":{"kind":"documents.index",
            "documentId":note.id,"versionId":exact.version_id,"expectedRevision":note.revision}}),
    );
    let OperationResult::DocumentsIndex { value: run } = result else {
        panic!("{result:?}")
    };
    push(&f);
    let dispatcher = dispatcher(&f.h, f.store.clone(), vec![document_consumer(indexer)]);
    runtime()
        .block_on(within("document embeddings", dispatcher.run_until_idle()))
        .unwrap();
    let pending = read(f.store.as_ref(), |r| r.queue().entry(&run.id)).unwrap();
    assert_eq!(pending.state, QueueState::Reconciling);
    assert_eq!(
        pending.last_error.as_deref(),
        Some("EmbeddingOutcomeUnknown")
    );
    let partial = read(f.store.as_ref(), |r| {
        r.documents().document_index(&exact.version_id)
    })
    .unwrap();
    assert_eq!(partial.stage, DocumentIndexStage::Partial);
    let direct = page(search(
        &f,
        &cases,
        &a,
        "held-direct-text",
        json!({"kind":"Text","text":"botanical"}),
        ("Hybrid", true),
        &[],
    ));
    assert!(matches!(direct.effective_mode, RetrievalMode::Lexical));
    assert_eq!(direct.passages[0].text, "Exact botanical passage.");

    let retry = command(
        &f,
        &cases,
        &a,
        "documents.retry",
        json!({"protocol":"1.0-draft.1",
        "mode":"Command","requestKey":"native-reconcile-index","body":{"kind":"documents.retry",
            "documentId":note.id,"versionId":exact.version_id,"expectedRevision":note.revision}}),
    );
    let OperationResult::DocumentsRetry { value: resumed } = retry else {
        panic!("{retry:?}")
    };
    assert_eq!(
        resumed.id, run.id,
        "a held outcome may only observe its original delivery"
    );
    runtime()
        .block_on(within(
            "document embedding observation",
            dispatcher.run_until_idle(),
        ))
        .unwrap();

    let status = read(f.store.as_ref(), |r| {
        r.documents().document_index(&exact.version_id)
    })
    .unwrap();
    assert_eq!(status.stage, DocumentIndexStage::Ready, "{status:?}");
    assert_eq!(
        sent(&f),
        2,
        "one probe and one index batch across the held outcome and its reconciliation"
    );
    assert_eq!(
        controlled.send_calls.load(Ordering::SeqCst),
        1,
        "reconciliation uses observation only"
    );
    let view = inspect_document(&f, &cases, &a, exact, 28);
    let activity = view.index_activity.unwrap();
    assert_eq!(activity.chunk_count.unwrap().get(), 1);
    assert_eq!(activity.attempts.0.len(), 1);
    assert_eq!(activity.attempts.0[0].purpose.as_str(), "Embedding");
    assert_eq!(
        activity.attempts.0[0].input_tokens.as_ref().unwrap().get(),
        3
    );
    let indexed = captured(&f, "DocumentIndex");
    assert_eq!(indexed.len(), 1);
    let (operation, attempt, balance) = &indexed[0];
    assert_eq!(operation.payload["origin"]["source"], json!(exact));
    assert_eq!(attempt.project_id.as_ref(), a.project_id());
    assert_eq!(&attempt.actor_id, a.principal());
    assert_eq!(attempt.state, AttemptState::Settled);
    assert!(balance.local_quiescent && !balance.unknown_tail);
    assert_eq!(
        devworks_core::services::usage::accounting::summary(balance)
            .unwrap()
            .meter
            .quantities
            .input_tokens,
        Some(3)
    );

    let request_id: Id = serde_json::from_value(operation.payload["requestId"].clone()).unwrap();
    let capture_metadata = read(f.store.as_ref(), |r| {
        r.conventions().convention_embedding_inputs(&request_id)
    })
    .unwrap();
    let input_artifact: ArtifactRef =
        serde_json::from_value(capture_metadata.input_refs[0].clone()).unwrap();
    let outputs = read(f.store.as_ref(), |r| {
        r.history().page(
            &EventFilter::Aggregate {
                kind: "provider_output".into(),
                id: attempt.attempt_id.as_str().into(),
            },
            0,
            10,
        )
    });
    let output_refs: Vec<ArtifactRef> = serde_json::from_value(
        outputs
            .events
            .iter()
            .find(|event| event.kind == "ProviderEmbeddingResultRecorded")
            .unwrap()
            .payload["artifacts"]
            .clone(),
    )
    .unwrap();
    assert_eq!(output_refs.len(), 2);
    for output in &output_refs {
        let (_, provenance) = f
            .artifacts
            .read_exact(f.store.as_ref(), &a, output, 65536)
            .unwrap();
        assert_eq!(provenance["derived"]["kind"], "DocumentEmbeddingOutput");
        assert_eq!(provenance["derived"]["documentSource"], json!(exact));
    }
    let captured_bytes = f
        .artifacts
        .read_exact(f.store.as_ref(), &a, &input_artifact, 4096)
        .unwrap()
        .0;
    assert_eq!(captured_bytes, b"Exact botanical passage.");

    let lexical = Arc::new(DocumentIndexer::new(
        f.store.clone(),
        f.artifacts.clone(),
        Arc::new(TextDocuments),
    ));
    let lexical_cases = UseCases::new().with_document_library(Arc::new(
        DocumentLibrary::new(f.artifacts.clone(), f.h.ids.clone(), f.h.clock())
            .with_indexer(lexical.clone()),
    ));
    let lexical_note = import(&f, &lexical_cases, &a, "sparse", b"A lexical term.", None);
    command(
        &f,
        &lexical_cases,
        &a,
        "documents.index",
        json!({"protocol":"1.0-draft.1",
        "mode":"Command","requestKey":"sparse-index","body":{"kind":"documents.index",
            "documentId":lexical_note.id,"versionId":lexical_note.current_version.unwrap().version_id,
            "expectedRevision":lexical_note.revision}}),
    );
    let lexical_dispatcher =
        common::dispatcher(&f.h, f.store.clone(), vec![document_consumer(lexical)]);
    runtime()
        .block_on(within(
            "lexical-only index",
            lexical_dispatcher.run_until_idle(),
        ))
        .unwrap();

    let text = json!({"kind":"Text","text":"unrelated lexical term"});
    push(&f);
    for _ in 0..2 {
        let hit = page(search(
            &f,
            &cases,
            &a,
            "hybrid-query",
            text.clone(),
            ("Hybrid", false),
            &[],
        ));
        assert!(matches!(hit.effective_mode, RetrievalMode::Hybrid));
        assert_eq!(
            hit.passages
                .iter()
                .map(|hit| hit.text.as_str())
                .collect::<Vec<_>>(),
            ["Exact botanical passage.", "A lexical term."]
        );
    }
    assert_eq!(
        sent(&f),
        3,
        "the exact text query is observed without another dispatch"
    );
    let queried = captured(&f, "Query");
    assert_eq!(queried.len(), 1);
    assert_eq!(queried[0].1.project_id.as_ref(), a.project_id());
    assert_eq!(&queried[0].1.actor_id, a.principal());
    assert_eq!(queried[0].1.state, AttemptState::Settled);
    assert!(queried[0].2.local_quiescent && !queried[0].2.unknown_tail);
    assert_eq!(
        devworks_core::services::usage::accounting::summary(&queried[0].2)
            .unwrap()
            .meter
            .quantities
            .input_tokens,
        Some(3)
    );
    assert!(f.transcript().is_empty());

    let space: EmbeddingSpace = read(f.store.as_ref(), |r| {
        Ok(embedding_spaces::resolve_selected_read(r, &b)?
            .binding()
            .unwrap()
            .space()
            .clone())
    });
    let raw = [0.25_f32, 0.75_f32]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect::<Vec<_>>();
    let vector = artifact(&f, &b, &raw, "application/vnd.devworks.float32-le");
    let input = json!({"kind":"Vector","space":space,"float32Le":vector});
    let hidden = page(search(
        &f,
        &cases,
        &b,
        "vector-hidden",
        input.clone(),
        ("Vector", false),
        &[],
    ));
    assert!(hidden.passages.is_empty());
    assert_eq!(hidden.space.as_ref(), Some(&space));
    let unavailable = search(
        &f,
        &cases,
        &b,
        "without-share",
        text.clone(),
        ("Hybrid", false),
        &[],
    );
    assert!(
        matches!(unavailable, DomainQueryReply::Rejected { fault, .. }
        if fault.code==DomainErrorCode::ViewUnavailable)
    );
    let fallback = page(search(
        &f,
        &cases,
        &b,
        "explicit-fallback",
        text,
        ("Hybrid", true),
        &[],
    ));
    assert!(matches!(fallback.effective_mode, RetrievalMode::Lexical));
    assert!(fallback.passages.is_empty());
    assert_eq!(sent(&f), 3);

    for fallback in [false, true] {
        let (store, document) = (f.store.clone(), note.id.clone());
        f.http.lock().unwrap().after_open = Some(Arc::new(move || {
            let document = document.clone();
            write(store.as_ref(), move |tx| {
                tx.documents().set_document_retrieval(&document, false)
            })
            .unwrap();
        }));
        push(&f);
        let result = search(
            &f,
            &cases,
            &a,
            if fallback {
                "mode-with-fallback"
            } else {
                "mode-without-fallback"
            },
            json!({"kind":"Text","text":"lexical term"}),
            ("Hybrid", fallback),
            &[],
        );
        f.http.lock().unwrap().after_open = None;
        if fallback {
            let result = page(result);
            assert_eq!(result.effective_mode, RetrievalMode::Lexical);
            assert!(result.space.is_none());
            assert_eq!(result.passages[0].text, "A lexical term.");
        } else {
            assert!(
                matches!(&result, DomainQueryReply::Rejected { fault, .. }
                if fault.code == DomainErrorCode::ViewUnavailable),
                "{result:?}"
            );
        }
        let document = note.id.clone();
        write(f.store.as_ref(), move |tx| {
            tx.documents().set_document_retrieval(&document, true)
        })
        .unwrap();
    }
    command(
        &f,
        &cases,
        &human,
        "documents.share",
        json!({"protocol":"1.0-draft.1","schemaRevision":25,
        "requestKey":"share-native","command":{"kind":"Share","artifactId":note.id,
            "targetProjectId":"documents-b","expectedScopeRevision":note.scope_revision}}),
    );
    let shared_plan = read(f.store.as_ref(), |r| {
        r.projects().purge_planning(a.project_id().unwrap())
    })
    .unwrap();
    for table in ["document_embedding_runs", "document_vectors"] {
        assert!(
            shared_plan
                .rows
                .iter()
                .any(|row| row.table == table && row.action == ProjectPurgeRowAction::Keep),
            "{table}: {:?}",
            shared_plan.unresolved
        );
    }
    let shared = page(search(
        &f,
        &cases,
        &b,
        "vector-shared",
        input.clone(),
        ("Vector", false),
        &[],
    ));
    assert_eq!(shared.passages[0].text, "Exact botanical passage.");
    let raw_nan = [f32::NAN, 1.0]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect::<Vec<_>>();
    let mut invalid = input.clone();
    invalid["float32Le"] = json!(artifact(
        &f,
        &b,
        &raw_nan,
        "application/vnd.devworks.float32-le"
    ));
    assert!(
        matches!(search(&f,&cases,&b,"vector-invalid",invalid,("Vector",false),&[]),
        DomainQueryReply::Rejected { fault, .. } if fault.code==DomainErrorCode::InvalidArgument)
    );
    let mut wrong = input.clone();
    wrong["space"]["modelDigest"] = json!("e".repeat(64));
    assert!(
        matches!(search(&f,&cases,&b,"vector-wrong-space",wrong,("Vector",false),&[]),
        DomainQueryReply::Rejected { fault, .. } if fault.code==DomainErrorCode::InvalidArgument)
    );
    let current = read(f.store.as_ref(), |r| r.artifacts().artifact(&note.id)).unwrap();
    let note = DocumentDetail {
        revision: Revision::new(current.revision).unwrap(),
        ..note.clone()
    };
    import(
        &f,
        &cases,
        &a,
        "replacement",
        b"A new unindexed version.",
        Some(&note),
    );
    let current = page(search(
        &f,
        &cases,
        &b,
        "vector-current",
        input.clone(),
        ("Vector", false),
        &[],
    ));
    assert!(current.passages.is_empty());
    let historical = page(search(
        &f,
        &cases,
        &b,
        "vector-exact",
        input,
        ("Vector", false),
        &[exact],
    ));
    assert_eq!(historical.passages[0].text, "Exact botanical passage.");
    assert_eq!(
        sent(&f),
        5,
        "only the two explicitly requested text queries added provider work"
    );

    let current = read(f.store.as_ref(), |r| r.artifacts().artifact(&note.id)).unwrap();
    command(
        &f,
        &cases,
        &human,
        "documents.move",
        json!({"protocol":"1.0-draft.1","schemaRevision":25,
        "requestKey":"move-native","command":{"kind":"Move","artifactId":note.id,
            "targetProjectId":"documents-b","expectedScopeRevision":current.scope_revision.to_string()}}),
    );
    assert!(matches!(
        f.artifacts
            .read_exact(f.store.as_ref(), &a, &input_artifact, 4096),
        Err(devworks_core::services::artifacts::lifecycle::ExactReadRefusal::NotFound)
    ));
    let moved_plan = read(f.store.as_ref(), |r| {
        r.projects().purge_planning(b.project_id().unwrap())
    })
    .unwrap();
    for table in ["document_embedding_runs", "document_vectors"] {
        assert!(
            moved_plan
                .rows
                .iter()
                .any(|row| row.table == table && row.action == ProjectPurgeRowAction::Delete),
            "{table}: {:?}",
            moved_plan.unresolved
        );
    }
    for artifact in std::iter::once(&input_artifact).chain(&output_refs) {
        assert!(matches!(
            f.artifacts
                .read_exact(f.store.as_ref(), &a, artifact, 65536),
            Err(devworks_core::services::artifacts::lifecycle::ExactReadRefusal::NotFound)
        ));
        let key = devworks_core::fingerprint::sha256_hex(
            json!([artifact.artifact_id]).to_string().as_bytes(),
        );
        assert!(
            moved_plan.rows.iter().any(|row| row.table == "artifacts"
                && row.key_digest == key
                && row.action == ProjectPurgeRowAction::Delete),
            "the moved document owns captured input and output bytes: {:?}",
            moved_plan.unresolved
        );
    }
}

#[test]
fn deselecting_a_qualified_space_invalidates_the_native_send_fence() {
    let f = Fx::new("Embedding", Script::aggregate(VECTOR), true);
    let (ctx, _native, _index) = qualify(&f);
    let resolved = read(f.store.as_ref(), |read| {
        embedding_spaces::resolve_selected_read(read, &ctx)
    });
    let connection_revision = resolved.binding().unwrap().connection().revision;
    let fences = [devworks_core::ports::AuthorityFence::QualifiedEmbedding {
        source_digest: resolved.source_digest().to_owned(),
    }];
    write(f.store.as_ref(), move |tx| {
        assert!(devworks_core::authority::service::inference_fences_hold(
            tx, &ctx, &fences
        )?);
        let selection = tx.router().embedding_selection()?;
        tx.router()
            .select_embedding_candidate(selection.revision, None)?;
        assert_eq!(
            tx.router().connection(&id("connection"))?.unwrap().revision,
            connection_revision
        );
        assert!(!devworks_core::authority::service::inference_fences_hold(
            tx, &ctx, &fences
        )?);
        Ok(())
    })
    .unwrap();
    assert_eq!(sent(&f), 1, "only the qualification probe used transport");
}
