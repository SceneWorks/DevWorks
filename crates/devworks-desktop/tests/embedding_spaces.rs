//! DW119: actual native Settings → durable candidate → accounted recorded probe → selection.
//! The provider returns recorded vectors; no learned model or network runs in this fixture.
mod common;
use common::*;
use devworks_core::ports::http::{HttpError, HttpRequest, HttpResponse, HttpTransport};
use devworks_core::ports::provider_http::*;
use devworks_core::ports::storage::Cancellation;
use devworks_desktop::connections_screen::{ADD_LABEL, NEW_NAME_LABEL};
use devworks_desktop::host::Engine;
use devworks_desktop::ops;
use devworks_engine::providers::driver::ProviderComposition;
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::{NodeT, Queryable};
use std::io::Read;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct RecordedEmbedding {
    requests: Mutex<Vec<HttpRequest>>,
}
impl HttpTransport for RecordedEmbedding {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        Ok(HttpResponse {
            status: if request.url.ends_with("/models") {
                200
            } else {
                404
            },
            body: br#"{"data":[{"id":"fixture-embedding"}]}"#.to_vec(),
        })
    }
}
struct RecordedStream {
    head: ResponseHead,
    bytes: std::io::Cursor<Vec<u8>>,
}
struct Closed;
impl StreamCloser for Closed {
    fn close(&self) -> Result<(), TransportError> {
        Ok(())
    }
}
impl HttpStream for RecordedStream {
    fn head(&self) -> &ResponseHead {
        &self.head
    }
    fn closer(&self) -> Arc<dyn StreamCloser> {
        Arc::new(Closed)
    }
    fn read(
        &mut self,
        out: &mut [u8],
        cancellation: &Cancellation,
    ) -> Result<usize, TransportError> {
        if cancellation.is_cancelled() {
            return Err(TransportError {
                phase: SendPhase::AfterSend,
                kind: TransportFailure::Cancelled,
            });
        }
        Ok(self.bytes.read(out).unwrap())
    }
    fn close(&mut self) -> Result<(), TransportError> {
        Ok(())
    }
}
impl StreamingHttp for RecordedEmbedding {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        cancellation: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        assert!(!cancellation.is_cancelled());
        assert!(
            request.url.ends_with("/embeddings"),
            "only the explicit embedding endpoint"
        );
        self.requests.lock().unwrap().push(request.clone());
        Ok(Box::new(RecordedStream {
            head: ResponseHead { status: 200, headers: vec![("content-type".into(), "application/json".into())] },
            bytes: std::io::Cursor::new(br#"{"model":"fixture-embedding","data":[{"index":0,"embedding":[0.6,0.8]}],"usage":{"prompt_tokens":7,"total_tokens":7}}"#.to_vec()),
        }))
    }
}
fn configured(root: &Root, http: Arc<RecordedEmbedding>) -> Engine {
    let mut c = config(&root.0);
    c.http = Some(http.clone());
    c.connection_tick = None;
    // No host-qualified space/capability: the durable native owner must earn qualification.
    c.providers = Some(ProviderComposition::new(http));
    Engine::start(c).expect("the real desktop runtime boots")
}
fn type_into(
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    label: &str,
    text: &str,
) {
    h.get_by_role_and_label(Role::TextInput, label)
        .scroll_to_me();
    for _ in 0..3 {
        h.step();
    }
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, label)
        .type_text(text);
    h.step();
}

fn setup_qualify_reopen(paid: bool) {
    let root = Root::new(if paid {
        "embedding-spaces-paid"
    } else {
        "embedding-spaces-free"
    });
    let http = Arc::new(RecordedEmbedding::default());
    let engine = configured(&root, http.clone());
    let backend = engine.backend();
    let mut h = harness(shell(&engine, &root, Script::default()), 1500.0, 1100.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    click(&mut h, Role::Button, "Provider connections");
    click(&mut h, Role::ComboBox, "Kind");
    click(&mut h, Role::Button, "Compatible embedding endpoint");
    type_into(&mut h, NEW_NAME_LABEL, "Recorded embeddings");
    if !paid {
        click(&mut h, Role::CheckBox, "Local or free new connection");
    }
    click(&mut h, Role::Button, ADD_LABEL);
    click(&mut h, Role::Button, "Test connection");
    click(&mut h, Role::Button, "Configure embedding spaces");
    assert!(http.requests.lock().unwrap().is_empty());
    type_into(&mut h, "Embedding space name", "Recorded space");
    click(&mut h, Role::ComboBox, "Embedding connection");
    click(&mut h, Role::Button, "Recorded embeddings");
    type_into(&mut h, "Embedding upstream model", "fixture-embedding");
    type_into(&mut h, "Embedding dimensions (1–65536)", "2");
    click(&mut h, Role::Button, "Create embedding space");
    assert!(
        http.requests.lock().unwrap().is_empty(),
        "candidate creation performs no inference"
    );
    let page = || {
        ops::embedding_space_page(backend.query(
            "router.embedding_spaces",
            &ops::router_embedding_spaces(&*backend, None),
        ))
        .unwrap()
    };
    let before = page();
    assert_eq!(before.items.0.len(), 1);
    assert!(before.selected_candidate_id.is_none());
    assert_eq!(
        serde_json::to_value(&before.items.0[0].status).unwrap(),
        "Unqualified"
    );
    if paid {
        let model = before.items.0[0]
            .pricing_model_id
            .as_ref()
            .expect("actual price-only subject");
        click(&mut h, Role::Button, "Configure price for Recorded space");
        let edit = format!("Edit tariff override for {}", model.as_str());
        assert!(
            h.query_by_role_and_label(Role::Button, &edit).is_some(),
            "actual price subject navigation: buttons={:?}, labels={:?}",
            labels(&h, Role::Button),
            labels(&h, Role::Label)
        );
        click(&mut h, Role::Button, &edit);
        assert_eq!(
            h.get_by_role_and_label(Role::TextInput, "Tariff upstream model")
                .accesskit_node()
                .value()
                .as_deref(),
            Some("fixture-embedding")
        );
        type_into(&mut h, "Amount USD per rate", "1");
        click(&mut h, Role::Button, "Save tariff override");
        assert!(
            http.requests.lock().unwrap().is_empty(),
            "pricing does not run a probe"
        );
        click_nav(&mut h, "Settings");
        // Returning to Settings retains the explicitly opened connection/setup panel.
        settle(&mut h);
    }
    click(
        &mut h,
        Role::Button,
        "Queue qualification for Recorded space (may incur usage)",
    );
    let deadline = std::time::Instant::now() + SETTLE_BOUND;
    let qualified = loop {
        let p = page();
        let status = serde_json::to_value(&p.items.0[0].status).unwrap();
        if status != "Qualifying" {
            break p;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "qualification closes: {p:?}"
        );
        std::thread::yield_now();
    };
    assert_eq!(
        serde_json::to_value(&qualified.items.0[0].status).unwrap(),
        "Qualified",
        "{qualified:?}"
    );
    click(&mut h, Role::Button, "Reload embedding spaces");
    click(&mut h, Role::Button, "Use embedding space Recorded space");
    let selected = page();
    assert_eq!(
        selected.selected_candidate_id.as_ref(),
        Some(&selected.items.0[0].id)
    );
    let retained = selected.items.0[0].clone();
    assert!(retained.qualification_attempt_id.is_some());
    assert!(retained.result_artifact.is_some());
    let usage_body = serde_json::json!({"reportKind":"Attempts","scope":{"kind":"Workspace"},"page":{"limit":100}});
    let usage_frame = devworks_desktop::usage_ops::query(&*backend, "router.usage", usage_body);
    let usage =
        devworks_desktop::usage_ops::usage(backend.query("router.usage", &usage_frame)).unwrap();
    let devworks_core::protocol::records::UsageQueryResult::Attempts { page: attempts } = usage
    else {
        panic!("actual attempt report")
    };
    let actual = attempts
        .items
        .0
        .iter()
        .find(|a| a.attempt_id == retained.qualification_attempt_id)
        .expect("the qualified native attempt is accounted");
    assert_eq!(actual.input_tokens.as_ref().map(|n| n.as_str()), Some("7"));
    assert!(
        !actual.unknown_tail,
        "a positive qualification requires closed accounting"
    );
    assert_eq!(actual.request_id, retained.qualification_request_id);
    assert_eq!(
        actual.cost.amount_micro_usd.as_ref().map(|n| n.as_str()),
        Some(if paid { "7" } else { "0" })
    );
    assert!(
        actual.cost.monetary_complete,
        "the explicit cost source is accounted completely"
    );
    let requests = http.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value =
        serde_json::from_slice(requests[0].body.as_deref().unwrap()).unwrap();
    assert_eq!(body["model"], "fixture-embedding");
    assert_eq!(
        body["input"],
        serde_json::json!([devworks_core::services::router::embedding_spaces::PROBE_TEXT])
    );
    assert!(
        body.get("dimensions").is_none(),
        "Native dimensions are not a wire parameter"
    );
    drop(requests);
    drop(h);
    drop(backend);
    drop(engine);
    let reopened = configured(&root, http.clone());
    let backend = reopened.backend();
    let restored = ops::embedding_space_page(backend.query(
        "router.embedding_spaces",
        &ops::router_embedding_spaces(&*backend, None),
    ))
    .unwrap();
    assert_eq!(restored.selected_candidate_id, Some(retained.id));
    assert_eq!(restored.items.0[0].space_id, retained.space_id);
    assert_eq!(
        restored.items.0[0].qualification_attempt_id,
        retained.qualification_attempt_id
    );
    assert_eq!(
        http.requests.lock().unwrap().len(),
        1,
        "reopen never resends a probe"
    );
}

#[test]
fn settings_qualifies_selects_and_reopens_an_explicit_recorded_embedding_space() {
    setup_qualify_reopen(false);
}

#[test]
fn settings_prices_an_embedding_subject_before_accounted_paid_qualification() {
    setup_qualify_reopen(true);
}
