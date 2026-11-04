//! Settings → Provider connections over the real runtime (DW-18): the native panel lists
//! connections from `router.connections`, adds an API-key connection through the host intake and
//! `router.configure`, tests it with an authenticated model list and makes Open Router Auto
//! routable. The key goes to a session-only secret store and never into a frame, the store or
//! the accessibility tree. The provider is a scripted in-memory transport: no network, no
//! inference.
mod common;

use common::*;
use devworks_core::ports::http::{HttpError, HttpRequest, HttpResponse, HttpTransport};
use devworks_core::ports::secrets::SessionSecrets;
use devworks_core::protocol::records::{EffortName, RouteIntent};
use devworks_desktop::connections_screen::{ADD_LABEL, NEW_KEY_LABEL, NEW_NAME_LABEL};
use devworks_desktop::host::Engine;
use devworks_desktop::ops;
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::Queryable;
use std::sync::{Arc, Mutex};

/// OpenRouter's model list, answered only with the right key.
#[derive(Default)]
struct OpenRouter {
    seen: Mutex<Vec<HttpRequest>>,
}
impl HttpTransport for OpenRouter {
    fn send(&self, r: &HttpRequest) -> Result<HttpResponse, HttpError> {
        self.seen.lock().unwrap().push(r.clone());
        if r.url == "https://openrouter.ai/api/v1/models"
            && r.header_value("authorization") == Some("Bearer sk-or-desktop")
        {
            return Ok(HttpResponse {
                status: 200,
                body: br#"{"data":[{"id":"openrouter/auto"},{"id":"a/b","context_length":8000},{"id":"c/d","context_length":4000}]}"#.to_vec(),
            });
        }
        Ok(HttpResponse {
            status: 401,
            body: Vec::new(),
        })
    }
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

#[test]
fn settings_adds_tests_and_routes_an_api_key_connection_without_exposing_the_key() {
    let root = Root::new("connections");
    let http = Arc::new(OpenRouter::default());
    let secrets = Arc::new(SessionSecrets::new());
    let mut c = config(&root.0);
    c.http = Some(http.clone());
    c.secrets = Some(secrets.clone());
    c.connection_tick = None;
    let engine = Engine::start(c).expect("the runtime boots");
    let backend = engine.backend();
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 1000.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    click(&mut h, Role::Button, "Provider connections");
    assert!(has_text(&h, "No provider is connected"));

    // Choose OpenRouter, name it and type its key.
    click(&mut h, Role::ComboBox, "Kind");
    click(&mut h, Role::Button, "OpenRouter");
    type_into(&mut h, NEW_NAME_LABEL, "OpenRouter");
    type_into(&mut h, NEW_KEY_LABEL, "sk-or-desktop");
    click(&mut h, Role::Button, ADD_LABEL);
    assert!(
        has_text(&h, "Adding the connection: done."),
        "{:?}",
        labels(&h, Role::Label)
    );
    assert!(has_text(&h, "Credential stored (revision 1)"));
    // The key went to the secret store, not the tree, the frames or the store's rows.
    assert_eq!(secrets.len(), 1);
    assert!(!has_text(&h, "sk-or-desktop"));
    let listed = ops::connection_records(backend.query(
        "router.connections",
        &ops::router_connections(&*backend, None),
    ))
    .unwrap()
    .0;
    assert_eq!(listed.len(), 1);
    assert!(
        !serde_json::to_string(&listed)
            .unwrap()
            .contains("sk-or-desktop")
    );
    let db = std::fs::read(root.0.join("workspace.sqlite")).unwrap();
    assert!(!db.windows(13).any(|w| w == b"sk-or-desktop"));

    // Test it: an authenticated model list, no inference.
    click(&mut h, Role::Button, "Test connection");
    assert!(has_text(&h, "Test passed"), "{:?}", labels(&h, Role::Label));
    assert!(has_text(&h, "3 models discovered"));
    assert!(
        http.seen
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.url.ends_with("/models"))
    );

    // The router routes Open Router Auto through it now.
    let a = ops::availability(backend.query(
        "router.availability",
        &ops::router_availability(
            &*backend,
            &RouteIntent::OpenRouterAuto {},
            &EffortName::Default,
            None,
        ),
    ))
    .unwrap();
    assert!(a.available, "{a:?}");
    assert_eq!(a.eligible_model_ids.0[0].as_str(), "openrouter-auto");

    // Removing the key: the secret is deleted and routing needs credentials again.
    click(&mut h, Role::Button, "Remove key");
    assert!(has_text(&h, "No credential stored"));
    assert_eq!(
        secrets.len(),
        0,
        "the removed key is deleted from the secret store"
    );
    drop(h);
    drop(engine);
}
