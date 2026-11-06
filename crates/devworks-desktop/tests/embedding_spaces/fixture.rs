//! Shared recorded transport and actual native embedding setup; no test registration.
use crate::common::*;
use devworks_core::ports::http::{HttpError, HttpRequest, HttpResponse, HttpTransport};
use devworks_core::ports::provider_http::*;
use devworks_core::ports::storage::Cancellation;
use devworks_desktop::connections_screen::{ADD_LABEL, NEW_NAME_LABEL};
use devworks_desktop::host::Engine;
use devworks_engine::providers::driver::ProviderComposition;
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::Queryable;
use std::io::Read;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct RecordedEmbedding {
    pub requests: Mutex<Vec<HttpRequest>>,
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
        let body: serde_json::Value =
            serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["model"], "fixture-embedding");
        let inputs = body["input"].as_array().expect("explicit embedding inputs");
        assert!(!inputs.is_empty() && inputs.iter().all(serde_json::Value::is_string));
        // Recorded vectors are repeated per requested ordinal, never computed from the text.
        let data: Vec<_> = inputs
            .iter()
            .enumerate()
            .map(|(index, _)| serde_json::json!({"index":index,"embedding":[0.6,0.8]}))
            .collect();
        let response=serde_json::json!({"model":"fixture-embedding","data":data,"usage":{"prompt_tokens":7,"total_tokens":7}}).to_string().into_bytes();
        Ok(Box::new(RecordedStream {
            head: ResponseHead {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
            },
            bytes: std::io::Cursor::new(response),
        }))
    }
}
pub fn configured(root: &Root, http: Arc<RecordedEmbedding>) -> Engine {
    let mut c = config(&root.0);
    c.http = Some(http.clone());
    c.connection_tick = None;
    // No host-qualified space/capability: the durable native owner must earn qualification.
    c.providers = Some(ProviderComposition::new(http));
    Engine::start(c).expect("the real desktop runtime boots")
}
pub fn type_into(
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

pub fn create_candidate(
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    http: &RecordedEmbedding,
    paid: bool,
) {
    settle(h);
    click_nav(h, "Settings");
    click(h, Role::Button, "Provider connections");
    click(h, Role::ComboBox, "Kind");
    click(h, Role::Button, "Compatible embedding endpoint");
    type_into(h, NEW_NAME_LABEL, "Recorded embeddings");
    if !paid {
        click(h, Role::CheckBox, "Local or free new connection");
    }
    click(h, Role::Button, ADD_LABEL);
    click(h, Role::Button, "Test connection");
    click(h, Role::Button, "Configure embedding spaces");
    assert!(http.requests.lock().unwrap().is_empty());
    type_into(h, "Embedding space name", "Recorded space");
    click(h, Role::ComboBox, "Embedding connection");
    click(h, Role::Button, "Recorded embeddings");
    type_into(h, "Embedding upstream model", "fixture-embedding");
    type_into(h, "Embedding dimensions (1–65536)", "2");
    click(h, Role::Button, "Create embedding space");
    assert!(
        http.requests.lock().unwrap().is_empty(),
        "candidate creation performs no inference"
    );
}
