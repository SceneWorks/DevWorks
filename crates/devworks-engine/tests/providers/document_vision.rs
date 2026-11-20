//! Recorded chat transport drives the native document producer and its actual ledger.
use super::{
    document_embeddings::{command, import, page, search, sent},
    *,
};
use devworks_core::{
    protocol::{
        ProjectId,
        data::{Coverage, CoverageState, Locator},
        records::{DocumentDetail, IndexRunRecord},
        results::OperationResult,
    },
    services::{
        documents::{
            DocumentLibrary,
            extraction::{ExtractedBlock, ExtractionControl, ExtractionInput},
            indexing::{DocumentExtraction, DocumentIndexer, DocumentProcessor},
            vision::{
                DocumentVision, MAX_TRANSCRIPTION_BYTES, RasterCapture, VisionExecution,
                VisionInvocation, VisionOutcome, VisionRaster, VisionTarget,
            },
        },
        usage::accounting,
    },
    settings::catalog::{DOCUMENT_VISION_KEY, DOCUMENT_VISION_MODELS_KEY, descriptor},
};
use devworks_engine::documents::{indexing::consumer, vision::NativeVision};

const PROJECT: &str = "document-vision-project";
const OTHER_PROJECT: &str = "document-vision-other";
const DIRECT: &str = "Native botanical passage.";
const TRANSCRIPTION: &str = "Scanned greenhouse register.";
const IMAGE: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0,
    0, 0, 58, 126, 155, 85, 0, 0, 0, 10, 73, 68, 65, 84, 120, 156, 99, 96, 0, 0, 0, 2, 0, 1, 72,
    175, 164, 113, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

struct MixedPages {
    renders: Arc<AtomicUsize>,
    oversized: bool,
}

impl DocumentProcessor for MixedPages {
    fn revision(&self) -> Revision {
        Revision::new(1).unwrap()
    }

    fn extract(
        &self,
        source: ExtractionInput<'_>,
        _: &ExtractionControl<'_>,
    ) -> Result<DocumentExtraction, Id> {
        Ok(DocumentExtraction {
            blocks: vec![ExtractedBlock::new(
                &source,
                "fixture-direct",
                Locator::Pdf { page: 1 },
                0,
                Arc::from([]),
                DIRECT.into(),
            )],
            coverage: Coverage {
                state: CoverageState::Partial,
                gaps: vec![id("PdfPageNeedsRaster")],
                more_gaps: false,
            },
            vision_targets: vec![VisionTarget::Pdf { page: 2 }],
        })
    }

    fn render_vision(
        &self,
        _: ExtractionInput<'_>,
        targets: &[VisionTarget],
        _: &ExtractionControl<'_>,
        capture: &mut RasterCapture<'_>,
    ) -> Result<(), StorageError> {
        self.renders.fetch_add(1, Ordering::SeqCst);
        assert!(matches!(targets, [VisionTarget::Pdf { page: 2 }]));
        let width = if self.oversized { u32::MAX } else { 1 };
        capture(
            &targets[0],
            Ok(VisionRaster {
                png: IMAGE.to_vec(),
                width,
                height: 1,
            }),
        )
    }
}

fn recorded_reply(text: &str) -> Script {
    Script::aggregate(
        &json!({
            "model":"fixture-model",
            "choices":[{"index":0,"message":{"content":text},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":5,"completion_tokens":2}
        })
        .to_string(),
    )
}

fn push_vision_reply(f: &Fx, text: &str) {
    f.http
        .lock()
        .unwrap()
        .body
        .extend(recorded_reply(text).body);
}

fn settings(tx: &mut dyn WriteTransaction, enabled: Option<bool>) -> Result<(), StorageError> {
    settings_routes(tx, enabled, &["model"], &[])
}

fn settings_routes(
    tx: &mut dyn WriteTransaction,
    enabled: Option<bool>,
    low: &[&str],
    preferred: &[&str],
) -> Result<(), StorageError> {
    let tier = "routing.tierLow.members";
    let mut values = json!({tier:{"schema":descriptor(tier).unwrap().schema_ref(),"value":low}});
    if let Some(enabled) = enabled {
        values[DOCUMENT_VISION_KEY] = json!({
            "schema":descriptor(DOCUMENT_VISION_KEY).unwrap().schema_ref(),"value":enabled
        });
    }
    if !preferred.is_empty() {
        values[DOCUMENT_VISION_MODELS_KEY] = json!({
            "schema":descriptor(DOCUMENT_VISION_MODELS_KEY).unwrap().schema_ref(),"value":preferred
        });
    }
    let scope = ConfigScope::Workspace;
    let revision = tx
        .configuration()
        .current(&scope, "settings")?
        .map_or(1, |r| r.revision);
    tx.configuration().append_revision(
        &scope,
        "settings",
        revision,
        1,
        json!({"values":values}),
    )?;
    Ok(())
}

struct VisionFixture {
    context: InvocationContext,
    native: Arc<NativeVision>,
    indexer: Arc<DocumentIndexer>,
    cases: Arc<UseCases>,
    renders: Arc<AtomicUsize>,
}

impl VisionFixture {
    fn new(f: &Fx, route: bool, enabled: Option<bool>, oversized: bool) -> Self {
        devworks_test_support::seed::rows(
            f.store.as_ref(),
            &[
                devworks_test_support::seed::ProjectSeed::new(PROJECT),
                devworks_test_support::seed::ProjectSeed::new(OTHER_PROJECT),
            ],
            &[],
        )
        .unwrap();
        let context = principal(f.store.as_ref(), "vision-human", PrincipalKind::Human)
            .with_lineage(Some(ProjectId(id(PROJECT))), None, None);
        write(f.store.as_ref(), move |tx| {
            let mut model = tx.router().route_model(&id("model"))?.unwrap();
            let revision = model.revision;
            model.image_input = route;
            tx.router().put_route_model(&model, Some(revision))?;
            settings(tx, enabled)
        })
        .unwrap();
        let native = Arc::new(NativeVision::new(
            f.store.clone(),
            f.artifacts.clone(),
            Arc::new(InferenceRouter::new(
                f.h.ids.clone(),
                f.h.clock(),
                f.connections.clone(),
            )),
            &f.driver,
            runtime().handle().clone(),
        ));
        let renders = Arc::new(AtomicUsize::new(0));
        let indexer = Arc::new(DocumentIndexer::new(
            f.store.clone(),
            f.artifacts.clone(),
            Arc::new(MixedPages {
                renders: renders.clone(),
                oversized,
            }),
        ));
        indexer.set_vision(native.clone());
        let cases = Arc::new(
            UseCases::new().with_document_library(Arc::new(
                DocumentLibrary::new(f.artifacts.clone(), f.h.ids.clone(), f.h.clock())
                    .with_indexer(indexer.clone()),
            )),
        );
        Self {
            context,
            native,
            indexer,
            cases,
            renders,
        }
    }

    fn import(&self, f: &Fx) -> DocumentDetail {
        import(
            f,
            &self.cases,
            &self.context,
            "mixed-scan",
            b"Exact fixture source",
            None,
        )
    }

    fn index(&self, f: &Fx, note: &DocumentDetail, operation: &str, key: &str) -> IndexRunRecord {
        let version = note.current_version.as_ref().unwrap();
        let result = command(
            f,
            &self.cases,
            &self.context,
            operation,
            json!({"protocol":"1.0-draft.1","mode":"Command","requestKey":key,"body":{
                "kind":operation,"documentId":note.id,"versionId":version.version_id,
                "expectedRevision":note.revision}}),
        );
        match result {
            OperationResult::DocumentsIndex { value }
            | OperationResult::DocumentsRetry { value } => value,
            _ => panic!("{result:?}"),
        }
    }

    fn run(&self, f: &Fx) {
        let dispatch = dispatcher(&f.h, f.store.clone(), vec![consumer(self.indexer.clone())]);
        runtime()
            .block_on(within("native document vision", dispatch.run_until_idle()))
            .unwrap();
    }

    fn find(&self, f: &Fx, key: &str, text: &str) -> devworks_core::protocol::data::RetrievalPage {
        page(search(
            f,
            &self.cases,
            &self.context,
            key,
            json!({"kind":"Text","text":text}),
            ("Lexical", false),
            &[],
        ))
    }
}

fn paid_prices(f: &Fx) {
    write(f.store.as_ref(), |tx| {
        let mut connection = tx.router().connection(&id("connection"))?.unwrap();
        let revision = connection.revision;
        connection.local_free = false;
        tx.router().put_connection(&connection, Some(revision))?;
        Ok(())
    })
    .unwrap();
    let human = principal(f.store.as_ref(), "vision-price-human", PrincipalKind::Human);
    let cases = UseCases::new().with_prices(Arc::new(
        devworks_core::services::prices::PriceService::new(f.h.ids.clone(), f.h.clock(), false),
    ));
    let tariffs = [
        ("Text", "Input", "1"),
        ("Text", "Output", "2"),
        ("Image", "Input", "1"),
    ]
    .map(|(modality, kind, amount)| {
        json!({
            "upstreamModelId":"fixture-model","serviceTier":"Standard","modality":modality,
            "chargeKind":kind,"unit":"Token","units":"1000000","amountUsd":amount
        })
    });
    command(
        f,
        &cases,
        &human,
        "router.price_override",
        json!({
            "protocol":"1.0-draft.1","mode":"Command","schemaRevision":3,
            "requestKey":"vision-paid-tariffs","body":{"kind":"router.price_override",
                "modelId":f.model,"expectedRevision":"1","change":{"kind":"Set","tariffs":tariffs}}
        }),
    );
}

#[test]
fn default_vision_publishes_exact_page_and_reuses_the_native_attempt() {
    let f = Fx::new("CompatibleChat", recorded_reply(TRANSCRIPTION), false);
    let fixture = VisionFixture::new(&f, true, None, false);
    paid_prices(&f);
    let note = fixture.import(&f);
    let source = note.current_version.as_ref().unwrap();
    let first = fixture.index(&f, &note, "documents.index", "index-native-vision");
    fixture.run(&f);
    let query_run = first.id.clone();
    let query_version = source.version_id.clone();
    let (queue, index, direct, evidence) = write(f.store.as_ref(), move |r| {
        let queue = r.queue().entry(&query_run)?.unwrap();
        let index = r.documents().document_index(&query_version)?.unwrap();
        let direct = r
            .documents()
            .document_extraction_by_key(&query_version, "direct:1")?
            .unwrap();
        let target = VisionTarget::Pdf { page: 2 };
        let unit = r
            .documents()
            .document_vision_unit(&direct.id, &target.id())?
            .unwrap();
        Ok((queue, index, direct, unit))
    })
    .unwrap();
    assert_eq!(queue.state, QueueState::Completed, "{queue:?}");
    assert_eq!(
        index.stage,
        DocumentIndexStage::Ready,
        "{index:?} {:?}",
        evidence.outcome
    );
    assert_eq!(sent(&f), 1);
    assert_eq!(fixture.renders.load(Ordering::SeqCst), 1);
    let request_body = {
        let script = f.http.lock().unwrap();
        let requests = script.requests.lock().unwrap();
        serde_json::from_slice::<Value>(requests[0].body.as_deref().unwrap()).unwrap()
    };
    assert_eq!(request_body["model"], "fixture-model");
    assert_eq!(request_body["max_tokens"], 2048);
    assert!(request_body.get("tools").is_none());
    assert!(
        request_body["messages"]
            .to_string()
            .contains("data:image/png;base64,")
    );
    let VisionOutcome::Complete { evidence: result } = evidence.outcome else {
        panic!("missing retained vision result");
    };
    read(f.store.as_ref(), |r| {
        let route = r.router().route_request(&result.request)?.unwrap();
        let operation = r.operations().operation(&route.operation_id)?.unwrap();
        let attempt = r.usage().inference_attempt(&result.attempt)?.unwrap();
        let balance = r.usage().attempt_balance(&result.attempt)?.unwrap();
        assert_eq!(operation.id, result.operation);
        assert_eq!(operation.attempts.len(), 1);
        assert_eq!(&attempt.actor_id, fixture.context.principal());
        assert_eq!(attempt.project_id.as_ref(), fixture.context.project_id());
        assert!(attempt.session_id.is_none());
        assert_eq!(attempt.purpose, "Vision");
        let usage = accounting::summary(&balance)?;
        assert!(balance.local_quiescent);
        assert_eq!(usage.meter.quantities.input_tokens, Some(5));
        assert_eq!(usage.meter.quantities.output_tokens, Some(2));
        assert_eq!(balance.priced_micro_usd, Some(4));
        assert_eq!(
            accounting::snapshot(&attempt)?.quote.classification,
            devworks_core::ports::usage::CostClassification::ConfiguredEstimate,
        );
        Ok(())
    });
    let (_, raster_provenance) = f
        .artifacts
        .read_exact(
            f.store.as_ref(),
            &fixture.context,
            evidence.raster.as_ref().unwrap(),
            1000,
        )
        .unwrap();
    assert_eq!(
        raster_provenance["derived"]["documentSource"],
        json!(source)
    );
    assert_eq!(raster_provenance["derived"]["width"], 1);
    assert_eq!(raster_provenance["derived"]["height"], 1);
    let scan = fixture.find(&f, "scan-search", "greenhouse");
    assert_eq!(scan.passages[0].text, TRANSCRIPTION);
    assert!(matches!(
        scan.passages[0].citation.locator,
        Locator::Pdf { page: 2 }
    ));
    let direct_result = fixture.find(&f, "native-search", "botanical");
    assert_eq!(direct_result.passages[0].text, DIRECT);
    assert!(matches!(
        direct_result.passages[0].citation.locator,
        Locator::Pdf { page: 1 }
    ));
    assert_ne!(index.published.as_ref().unwrap().1, direct.revision);
    fixture.index(&f, &note, "documents.index", "recover-native-vision");
    fixture.run(&f);
    assert_eq!(sent(&f), 1);
    assert_eq!(fixture.renders.load(Ordering::SeqCst), 1);
    let current = read(f.store.as_ref(), |r| {
        r.documents().document_index(&source.version_id)
    })
    .unwrap();
    assert_eq!(current.published.unwrap().1, index.published.unwrap().1);
    let refresh = json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":12,
        "requestKey":"explicit-reextract","body":{"kind":"documents.index","documentId":note.id,
            "versionId":source.version_id,"expectedRevision":note.revision,"reextract":true}});
    let first_refresh = command(
        &f,
        &fixture.cases,
        &fixture.context,
        "documents.index",
        refresh.clone(),
    );
    push_vision_reply(&f, "Refreshed greenhouse register.");
    fixture.run(&f);
    let replay = command(
        &f,
        &fixture.cases,
        &fixture.context,
        "documents.index",
        refresh,
    );
    assert_eq!(
        serde_json::to_value(first_refresh).unwrap(),
        serde_json::to_value(replay).unwrap()
    );
    assert_eq!(sent(&f), 2);
    assert_eq!(fixture.renders.load(Ordering::SeqCst), 2);
    fixture.index(&f, &note, "documents.index", "reuse-refreshed-extraction");
    fixture.run(&f);
    assert_eq!(sent(&f), 2);
    assert_eq!(fixture.renders.load(Ordering::SeqCst), 2);
    assert_eq!(
        fixture.find(&f, "refreshed-scan", "greenhouse").passages[0].text,
        "Refreshed greenhouse register."
    );
    assert!(f.transcript().is_empty());
    let direct_block = read(f.store.as_ref(), |r| {
        r.documents().document_blocks(&direct.id)
    })
    .remove(0);
    for (citation, expected) in [
        (&direct_block.citation, DIRECT),
        (&scan.passages[0].citation, TRANSCRIPTION),
    ] {
        let authority = devworks_core::authority::HostAuthority {
            store: f.store.as_ref(),
            clock: f.h.clock.as_ref(),
            protected_branches: &[],
        };
        let gateway = devworks_core::gateway::Gateway {
            authority: &authority,
            store: f.store.as_ref(),
            ids: f.h.ids.as_ref(),
        };
        let request = json!({"protocol":"1.0-draft.1","mode":"Query","schemaRevision":11,
            "queryId":"old-exact","body":{"kind":"documents.resolve_citation","citation":citation}});
        let reply = fixture
            .cases
            .query(
                &gateway,
                &fixture.context,
                "documents.resolve_citation",
                request.to_string().as_bytes(),
            )
            .unwrap();
        let devworks_core::protocol::results::DomainQueryReply::Result {
            result: OperationResult::DocumentsResolveCitation { value },
            ..
        } = reply
        else {
            panic!("{reply:?}")
        };
        assert_eq!(value.text.0, expected);
    }
}

#[test]
fn unavailable_disabled_and_oversized_vision_leave_direct_text_searchable() {
    for (route, enabled, oversized, renders, diagnostic) in [
        (false, None, false, 1, "VisionRouteUnavailable"),
        (true, Some(false), false, 0, "VisionDisabled"),
        (true, None, true, 1, "VisionRasterInvalid"),
    ] {
        let f = Fx::new("CompatibleChat", recorded_reply(TRANSCRIPTION), false);
        let fixture = VisionFixture::new(&f, route, enabled, oversized);
        let note = fixture.import(&f);
        fixture.index(&f, &note, "documents.index", "index-partial");
        fixture.run(&f);
        let version = &note.current_version.as_ref().unwrap().version_id;
        let index = read(f.store.as_ref(), |r| r.documents().document_index(version)).unwrap();
        assert_eq!(index.stage, DocumentIndexStage::Partial, "{index:?}");
        assert_eq!(index.diagnostic, Some(id(diagnostic)));
        assert_eq!(fixture.renders.load(Ordering::SeqCst), renders);
        assert_eq!(sent(&f), 0);
        assert_eq!(
            fixture.find(&f, "native-search", "botanical").passages[0].text,
            DIRECT
        );
        assert!(
            fixture
                .find(&f, "scan-search", "greenhouse")
                .passages
                .is_empty()
        );
    }
}

struct LoseAcknowledgement {
    native: Arc<NativeVision>,
    calls: AtomicUsize,
}
impl DocumentVision for LoseAcknowledgement {
    fn execute(
        &self,
        context: &InvocationContext,
        work: &VisionInvocation,
        cancellation: &Cancellation,
        observe_only: bool,
    ) -> Result<VisionExecution, StorageError> {
        let calls = self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(observe_only, calls > 0);
        let result = self
            .native
            .execute(context, work, cancellation, observe_only)?;
        assert!(matches!(result, VisionExecution::Recorded(_)));
        Ok(if calls < 2 {
            VisionExecution::Unknown
        } else {
            result
        })
    }
}

#[test]
fn uncertain_vision_retries_observe_the_original_attempt() {
    let f = Fx::new("CompatibleChat", recorded_reply(TRANSCRIPTION), false);
    let fixture = VisionFixture::new(&f, true, None, false);
    fixture.indexer.set_vision(Arc::new(LoseAcknowledgement {
        native: fixture.native.clone(),
        calls: AtomicUsize::new(0),
    }));
    let note = fixture.import(&f);
    let run = fixture.index(&f, &note, "documents.index", "hold-vision");
    fixture.run(&f);
    for n in 1..=2 {
        let queue = read(f.store.as_ref(), |r| r.queue().entry(&run.id)).unwrap();
        assert_eq!(queue.state, QueueState::Reconciling, "{queue:?}");
        assert_eq!(queue.last_error.as_deref(), Some("VisionOutcomeUnknown"));
        assert_eq!(
            fixture
                .find(&f, &format!("direct-while-held-{n}"), "botanical")
                .passages[0]
                .text,
            DIRECT
        );
        assert!(
            fixture
                .find(&f, &format!("scan-while-held-{n}"), "greenhouse")
                .passages
                .is_empty()
        );
        let enabled = (n == 1).then_some(false);
        write(f.store.as_ref(), move |tx| settings(tx, enabled)).unwrap();
        let retry = fixture.index(
            &f,
            &note,
            "documents.retry",
            &format!("observe-held-vision-{n}"),
        );
        assert_eq!(retry.id, run.id);
        fixture.run(&f);
    }
    let queue = read(f.store.as_ref(), |r| r.queue().entry(&run.id)).unwrap();
    assert_eq!(queue.state, QueueState::Completed, "{queue:?}");
    assert_eq!(sent(&f), 1);
    assert_eq!(fixture.renders.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture.find(&f, "recovered-scan", "greenhouse").passages[0].text,
        TRANSCRIPTION
    );
}

#[test]
fn oversized_provider_output_has_no_published_transcription() {
    let text = "greenhouse ".repeat(MAX_TRANSCRIPTION_BYTES / 10 + 1);
    let f = Fx::new("CompatibleChat", recorded_reply(&text), false);
    let fixture = VisionFixture::new(&f, true, None, false);
    let note = fixture.import(&f);
    fixture.index(&f, &note, "documents.index", "bounded-vision-output");
    fixture.run(&f);
    let version = &note.current_version.as_ref().unwrap().version_id;
    let index = read(f.store.as_ref(), |r| r.documents().document_index(version)).unwrap();
    assert_eq!(index.stage, DocumentIndexStage::Partial, "{index:?}");
    assert_eq!(index.diagnostic, Some(id("VisionTextLimit")));
    assert_eq!(sent(&f), 1);
    assert_eq!(
        fixture.find(&f, "bounded-direct", "botanical").passages[0].text,
        DIRECT
    );
    assert!(
        fixture
            .find(&f, "bounded-scan", "greenhouse")
            .passages
            .is_empty()
    );
}

#[test]
fn vision_source_change_and_cancellation_fence_raw_output() {
    for cancelled in [false, true] {
        let f = Fx::new("CompatibleChat", recorded_reply(TRANSCRIPTION), false);
        let fixture = VisionFixture::new(&f, true, None, false);
        let note = fixture.import(&f);
        let source = note.current_version.as_ref().unwrap();
        let run = fixture.index(&f, &note, "documents.index", "fence-vision");
        let store = f.store.clone();
        let delivery = run.id.clone();
        let document = note.id.clone();
        f.http.lock().unwrap().after_open = Some(Arc::new(move || {
            let delivery = delivery.clone();
            let document = document.clone();
            write(store.as_ref(), move |tx| {
                if cancelled {
                    tx.queue().cancel(&delivery)?;
                } else {
                    let mut row = tx.artifacts().artifact(&document)?.unwrap();
                    row.scope = ArtifactScope::Project(ProjectId(id(OTHER_PROJECT)));
                    row.scope_revision += 1;
                    row.revision += 1;
                    tx.artifacts().update_artifact(&row)?;
                }
                Ok(())
            })
            .unwrap();
        }));
        fixture.run(&f);
        assert_eq!(sent(&f), 1);
        let queue = read(f.store.as_ref(), |r| r.queue().entry(&run.id)).unwrap();
        let status = if cancelled {
            QueueState::Cancelled
        } else {
            QueueState::Failed
        };
        assert_eq!(queue.state, status, "{queue:?}");
        let index = read(f.store.as_ref(), |r| {
            r.documents().document_index(&source.version_id)
        })
        .unwrap();
        assert!(index.published.is_none(), "{index:?}");
        assert!(
            fixture
                .find(&f, "fenced-scan", "greenhouse")
                .passages
                .is_empty()
        );
    }
}

#[test]
fn disabling_vision_during_a_send_preserves_direct_text() {
    let f = Fx::new("CompatibleChat", recorded_reply(TRANSCRIPTION), false);
    let fixture = VisionFixture::new(&f, true, None, false);
    let note = fixture.import(&f);
    let source = note.current_version.as_ref().unwrap();
    let run = fixture.index(&f, &note, "documents.index", "disable-during-vision");
    let store = f.store.clone();
    f.http.lock().unwrap().after_open = Some(Arc::new(move || {
        write(store.as_ref(), |tx| settings(tx, Some(false))).unwrap();
    }));
    fixture.run(&f);
    assert_eq!(sent(&f), 1);
    let index = read(f.store.as_ref(), |r| {
        r.documents().document_index(&source.version_id)
    })
    .unwrap();
    assert_eq!(index.stage, DocumentIndexStage::Partial, "{index:?}");
    assert_eq!(index.diagnostic, Some(id("VisionAuthorityChanged")));
    let queue = read(f.store.as_ref(), |r| r.queue().entry(&run.id)).unwrap();
    assert_eq!(queue.state, QueueState::Completed, "{queue:?}");

    assert_eq!(
        fixture.find(&f, "fenced-direct", "botanical").passages[0].text,
        DIRECT
    );
    assert!(
        fixture
            .find(&f, "fenced-scan", "greenhouse")
            .passages
            .is_empty()
    );
}

#[test]
fn vision_custody_follows_exact_source_and_usage_keeps_original_project() {
    let f = Fx::new("CompatibleChat", recorded_reply(TRANSCRIPTION), false);
    let fixture = VisionFixture::new(&f, true, None, false);
    let note = fixture.import(&f);
    let version = note.current_version.as_ref().unwrap().version_id.clone();
    fixture.index(&f, &note, "documents.index", "purge-vision-evidence");
    fixture.run(&f);
    let unit = write(f.store.as_ref(), move |tx| {
        let direct = tx
            .documents()
            .document_extraction_by_key(&version, "direct:1")?
            .unwrap();
        let target = VisionTarget::Pdf { page: 2 };
        Ok(tx
            .documents()
            .document_vision_unit(&direct.id, &target.id())?
            .unwrap())
    })
    .unwrap();
    let VisionOutcome::Complete { evidence } = unit.outcome else {
        panic!("{:?}", unit.outcome)
    };
    let refs = [
        unit.raster.unwrap(),
        unit.material.unwrap().context,
        evidence.output,
    ];
    let project = fixture.context.project_id().unwrap();
    let owns = |plan: &ProjectPurgePlanning, table: &str, action: ProjectPurgeRowAction| {
        assert!(
            plan.rows
                .iter()
                .any(|row| row.table == table && row.action == action),
            "{table}: {:?}",
            plan.unresolved
        );
    };
    let plan = read(f.store.as_ref(), |r| r.projects().purge_planning(project)).unwrap();
    for table in [
        "document_extraction_keys",
        "document_vision_units",
        "usage_attempts",
        "usage_attempt_balances",
    ] {
        owns(&plan, table, ProjectPurgeRowAction::Delete);
    }
    for item in &refs {
        assert_purge_artifact(&plan, item, ProjectPurgeRowAction::Delete);
    }
    let workspace = fixture.context.clone().with_lineage(None, None, None);
    let share = command(
        &f,
        &fixture.cases,
        &workspace,
        "documents.share",
        json!({
            "protocol":"1.0-draft.1","schemaRevision":10,"requestKey":"share-vision-evidence",
            "command":{"kind":"Share","artifactId":note.id,"targetProjectId":OTHER_PROJECT,
                "expectedScopeRevision":note.scope_revision}
        }),
    );
    let OperationResult::DocumentsShare {
        value: devworks_core::protocol::records::DocumentResult::Current(shared),
    } = share
    else {
        panic!("{share:?}")
    };
    let shared_plan = read(f.store.as_ref(), |r| r.projects().purge_planning(project)).unwrap();
    for table in ["document_extraction_keys", "document_vision_units"] {
        owns(&shared_plan, table, ProjectPurgeRowAction::Keep);
    }
    owns(
        &shared_plan,
        "usage_attempts",
        ProjectPurgeRowAction::Delete,
    );
    for item in &refs {
        assert_purge_artifact(&shared_plan, item, ProjectPurgeRowAction::Keep);
    }
    command(
        &f,
        &fixture.cases,
        &workspace,
        "documents.move",
        json!({
            "protocol":"1.0-draft.1","schemaRevision":10,"requestKey":"move-vision-evidence",
            "command":{"kind":"Move","artifactId":note.id,"targetProjectId":OTHER_PROJECT,
                "expectedScopeRevision":shared.scope_revision}
        }),
    );
    let former_owner = read(f.store.as_ref(), |r| r.projects().purge_planning(project)).unwrap();
    for item in &refs {
        assert_purge_artifact(&former_owner, item, ProjectPurgeRowAction::Keep);
    }
    let moved = read(f.store.as_ref(), |r| {
        r.projects().purge_planning(&ProjectId(id(OTHER_PROJECT)))
    })
    .unwrap();
    for table in ["document_extraction_keys", "document_vision_units"] {
        owns(&moved, table, ProjectPurgeRowAction::Delete);
    }
    for item in &refs {
        assert_purge_artifact(&moved, item, ProjectPurgeRowAction::Delete);
        assert!(
            f.artifacts
                .read_exact(f.store.as_ref(), &fixture.context, item, 65536)
                .is_err()
        );
    }
    assert!(!moved.rows.iter().any(|row| row.table == "usage_attempts"));
    let attempt = read(f.store.as_ref(), |r| {
        r.usage().inference_attempt(&evidence.attempt)
    })
    .unwrap();
    assert_eq!(attempt.project_id.as_ref(), Some(project));
}

fn assert_purge_artifact(
    plan: &ProjectPurgePlanning,
    artifact: &ArtifactRef,
    action: ProjectPurgeRowAction,
) {
    let key = devworks_core::fingerprint::sha256_hex(
        json!([artifact.artifact_id]).to_string().as_bytes(),
    );
    let row = plan
        .rows
        .iter()
        .find(|row| row.table == "artifacts" && row.key_digest == key);
    assert_eq!(
        row.map(|row| &row.action),
        Some(&action),
        "{}: {:?}",
        artifact.artifact_id.as_str(),
        plan.unresolved
    );
}

#[test]
fn model_selection_skips_profiles_without_output_limits() {
    for preferred in [false, true] {
        let f = Fx::new(
            "CodexSubscriptionResponses",
            recorded_reply(TRANSCRIPTION),
            false,
        );
        let fixture = VisionFixture::new(&f, true, None, false);
        write(f.store.as_ref(), move |tx| {
            let mut alternative = tx.router().connection(&id("connection"))?.unwrap();
            alternative.id = id("compatible-connection");
            alternative.profile = "CompatibleChat".into();
            alternative.endpoint = "http://localhost:11434/v1".into();
            alternative.credential_ref = None;
            alternative.credential_revision = 0;
            alternative.credential_epoch = None;
            alternative.account_digest = None;
            tx.router().put_connection(&alternative, None)?;
            let mut model = tx.router().route_model(&id("model"))?.unwrap();
            model.id = id("compatible-model");
            model.connection_id = Some(alternative.id);
            model.endpoint_profile = alternative.profile;
            model.revision = 1;
            model.position = 2;
            tx.router().put_route_model(&model, None)?;
            let options = ["model", "compatible-model"];
            if preferred {
                settings_routes(tx, None, &["missing-model"], &options)
            } else {
                settings_routes(tx, None, &options, &[])
            }
        })
        .unwrap();
        let note = fixture.import(&f);
        fixture.index(&f, &note, "documents.index", "fallback-route");
        fixture.run(&f);
        assert_eq!(sent(&f), 1);
        assert_eq!(
            fixture
                .find(&f, "fallback-transcription", "greenhouse")
                .passages[0]
                .text,
            TRANSCRIPTION
        );
        let script = f.http.lock().unwrap();
        let requests = script.requests.lock().unwrap();
        assert!(requests[0].url.as_str().ends_with("/chat/completions"));
    }
}

#[test]
fn native_vision_admits_an_eligible_session_pin_with_its_original_intent() {
    use devworks_core::protocol::records::RouteIntent;
    let f = Fx::new("CompatibleChat", recorded_reply(TRANSCRIPTION), false);
    let mut fixture = VisionFixture::new(&f, true, None, false);
    let actor = fixture.context.principal().clone();
    let project = fixture.context.project_id().cloned();
    let session = id("vision-session");
    let session_id = session.clone();
    write(f.store.as_ref(), move |tx| {
        tx.sessions().insert_session(&StoredSession {
            id: session_id.clone(),
            conversation_id: id("vision-conversation"),
            agent_id: actor.clone(),
            project_id: project,
            root_id: None,
            title: "Vision fixture session".into(),
            context_epoch: 0,
            revision: 1,
            created_by: actor,
            created_at_ms: T0,
            closed_at_ms: None,
            closed_by: None,
            next_sequence: 1,
            checkpoint_sequence: 0,
        })?;
        tx.router().put_session_pin(
            &StoredPin {
                session_id,
                pin_id: id("vision-pin"),
                revision: 1,
                model_id: id("model"),
                connection_id: id("connection"),
                upstream_model_id: Some("fixture-model".into()),
                endpoint_profile: Some("CompatibleChat".into()),
                identity_origin: "Resolved".into(),
                intent: devworks_core::services::router::intent_key(&RouteIntent::Model {
                    model_id: id("model"),
                }),
                requested_effort: "Default".into(),
                effective_effort: "Default".into(),
                required_capabilities: vec![],
                replaced_pin_id: None,
                reason: "Initial".into(),
                updated_at_ms: T0,
            },
            None,
        )?;
        settings_routes(tx, None, &["missing-model"], &[])
    })
    .unwrap();
    fixture.context = fixture.context.with_session(session.clone());
    let note = fixture.import(&f);
    fixture.index(&f, &note, "documents.index", "use-exact-pin");
    fixture.run(&f);
    assert_eq!(sent(&f), 1);
    assert_eq!(
        fixture.find(&f, "pin-transcription", "greenhouse").passages[0].text,
        TRANSCRIPTION
    );
    let current = read(f.store.as_ref(), |r| r.router().session_pin(&session)).unwrap();
    assert_eq!(current.revision, 1);
}

#[test]
fn nonblank_scan_with_empty_output_stays_partial() {
    let f = Fx::new("CompatibleChat", recorded_reply("   "), false);
    let fixture = VisionFixture::new(&f, true, None, false);
    let note = fixture.import(&f);
    fixture.index(&f, &note, "documents.index", "empty-transcription");
    fixture.run(&f);
    let version = &note.current_version.as_ref().unwrap().version_id;
    let index = read(f.store.as_ref(), |r| r.documents().document_index(version)).unwrap();
    assert_eq!(index.stage, DocumentIndexStage::Partial, "{index:?}");
    assert_eq!(index.diagnostic, Some(id("VisionNoTranscription")));
    assert_eq!(sent(&f), 1);
    assert_eq!(
        fixture.find(&f, "empty-scan-direct", "botanical").passages[0].text,
        DIRECT
    );
}

struct ChangeRouteAfterPlan {
    store: Arc<dyn ApplicationStore>,
    reads: AtomicUsize,
}

impl ApplicationStore for ChangeRouteAfterPlan {
    fn info(&self) -> StoreInfo {
        self.store.info()
    }

    fn submit_write(
        &self,
        options: WriteOptions,
        job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError> {
        self.store.submit_write(options, job)
    }

    fn read(
        &self,
        options: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.store.read(options, work)?;
        // The executor first queries its original request, then reads the quote and plan.
        if self.reads.fetch_add(1, Ordering::SeqCst) == 1 {
            configure_fixture_image_model(self.store.as_ref(), false);
        }
        Ok(())
    }
}

fn configure_fixture_image_model(store: &dyn ApplicationStore, enabled: bool) {
    write(store, move |tx| {
        let mut model = tx.router().route_model(&id("model"))?.unwrap();
        let revision = model.revision;
        model.image_input = enabled;
        tx.router().put_route_model(&model, Some(revision))?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn refused_native_admission_leaves_exact_capture_reusable() {
    let f = Fx::new("CompatibleChat", recorded_reply(TRANSCRIPTION), false);
    let fixture = VisionFixture::new(&f, true, None, false);
    let native = Arc::new(NativeVision::new(
        Arc::new(ChangeRouteAfterPlan {
            store: f.store.clone(),
            reads: AtomicUsize::new(0),
        }),
        f.artifacts.clone(),
        Arc::new(InferenceRouter::new(
            f.h.ids.clone(),
            f.h.clock(),
            f.connections.clone(),
        )),
        &f.driver,
        runtime().handle().clone(),
    ));
    fixture.indexer.set_vision(native);
    let note = fixture.import(&f);
    fixture.index(&f, &note, "documents.index", "lost-eligibility");
    fixture.run(&f);
    assert_eq!(sent(&f), 0);
    let version = note.current_version.as_ref().unwrap().version_id.clone();
    let request = write(f.store.as_ref(), move |tx| {
        let direct = tx
            .documents()
            .document_extraction_by_key(&version, "direct:1")?
            .unwrap();
        let unit = tx
            .documents()
            .document_vision_unit(&direct.id, &VisionTarget::Pdf { page: 2 }.id())?
            .unwrap();
        assert!(unit.material.is_some());
        assert!(
            matches!(unit.outcome, VisionOutcome::Ready),
            "{:?}",
            unit.outcome
        );
        Ok(devworks_core::services::documents::vision::request_id(
            &unit,
        ))
    })
    .unwrap();
    assert!(read(f.store.as_ref(), |r| r.router().route_request(&request)).is_none());
    configure_fixture_image_model(f.store.as_ref(), true);
    fixture.indexer.set_vision(fixture.native.clone());
    fixture.index(&f, &note, "documents.retry", "restored-eligibility");
    fixture.run(&f);
    assert_eq!(sent(&f), 1);
    assert_eq!(fixture.renders.load(Ordering::SeqCst), 1);
    let admitted = read(f.store.as_ref(), |r| r.router().route_request(&request)).unwrap();
    assert_eq!(admitted.purpose, "Vision");
    assert_eq!(
        fixture
            .find(&f, "retry-transcription", "greenhouse")
            .passages[0]
            .text,
        TRANSCRIPTION
    );
}

#[test]
fn moved_source_observes_a_held_vision_attempt_without_rebasing_its_owner() {
    let f = Fx::new("CompatibleChat", recorded_reply(TRANSCRIPTION), false);
    let mut fixture = VisionFixture::new(&f, true, None, false);
    fixture.indexer.set_vision(Arc::new(LoseAcknowledgement {
        native: fixture.native.clone(),
        calls: AtomicUsize::new(0),
    }));
    let note = fixture.import(&f);
    let original = fixture.index(&f, &note, "documents.index", "old-source-custody");
    fixture.run(&f);
    let entry = read(f.store.as_ref(), |r| r.queue().entry(&original.id)).unwrap();
    assert_eq!(entry.state, QueueState::Reconciling, "{entry:?}");
    let workspace = fixture.context.clone().with_lineage(None, None, None);
    let moved = command(
        &f,
        &fixture.cases,
        &workspace,
        "documents.move",
        json!({
            "protocol":"1.0-draft.1","schemaRevision":10,"requestKey":"move-held-vision",
            "command":{"kind":"Move","artifactId":note.id,"targetProjectId":OTHER_PROJECT,
                "expectedScopeRevision":note.scope_revision}
        }),
    );
    let OperationResult::DocumentsMove {
        value: devworks_core::protocol::records::DocumentResult::Current(moved),
    } = moved
    else {
        panic!("{moved:?}")
    };
    fixture.context = principal(f.store.as_ref(), "new-vision-owner", PrincipalKind::Human)
        .with_lineage(Some(ProjectId(id(OTHER_PROJECT))), None, None);
    fixture.indexer.set_vision(fixture.native.clone());
    let observing = fixture.index(&f, &moved, "documents.retry", "observe-after-move");
    assert_ne!(observing.id, original.id);
    fixture.run(&f);
    let observed = read(f.store.as_ref(), |r| r.queue().entry(&observing.id)).unwrap();
    assert_eq!(observed.state, QueueState::Completed, "{observed:?}");
    assert_eq!(sent(&f), 1);
    assert_eq!(fixture.renders.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture.find(&f, "moved-vision", "greenhouse").passages[0].text,
        TRANSCRIPTION
    );
    let fresh_owner_plan = read(f.store.as_ref(), |r| {
        r.projects()
            .purge_planning(fixture.context.project_id().unwrap())
    })
    .unwrap();
    assert!(
        !fresh_owner_plan
            .rows
            .iter()
            .any(|row| row.table == "usage_attempts")
    );
}
