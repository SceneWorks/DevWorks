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
use devworks_core::protocol::records::ConnectionRecord;
use devworks_core::protocol::records::{EffortName, RouteIntent};
use devworks_desktop::backend::SecretStoreStatus;
use devworks_desktop::connections_screen::{
    ADD_LABEL, ConnectionAction, ConnectionsPanel, NEW_KEY_LABEL, NEW_NAME_LABEL,
    SESSION_ONLY_LABEL,
};
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

// ---------------------------------------------------------------------------------------------
// The panel alone, over records as `router.connections` lists them (no runtime).

struct Panel {
    panel: ConnectionsPanel,
    actions: Vec<ConnectionAction>,
}

fn record(v: serde_json::Value) -> ConnectionRecord {
    serde_json::from_value(v).expect("a revision-2 connection record")
}

fn panel(
    records: Vec<ConnectionRecord>,
    store: Option<SecretStoreStatus>,
) -> egui_kittest::Harness<'static, Panel> {
    let mut panel = ConnectionsPanel::default();
    panel.loaded(records);
    panel.secret_store = store;
    let mut h = egui_kittest::Harness::builder()
        .with_size(eframe::egui::vec2(1200.0, 1800.0))
        .build_ui_state(
            |ui, s: &mut Panel| {
                let mut actions = Vec::new();
                s.panel.ui(ui, 0, &mut actions);
                s.actions.extend(actions);
            },
            Panel {
                panel,
                actions: Vec::new(),
            },
        );
    h.run();
    h
}

fn button(h: &egui_kittest::Harness<'_, Panel>, label: &str) -> bool {
    h.query_by_role_and_label(Role::Button, label).is_some()
}

fn press(h: &mut egui_kittest::Harness<'_, Panel>, label: &str) -> serde_json::Value {
    h.state_mut().actions.clear();
    h.get_by_role_and_label(Role::Button, label).click();
    h.run();
    match h.state().actions.as_slice() {
        [ConnectionAction::Configure { action, .. }] => action.clone(),
        other => panic!("{label}: {other:?}"),
    }
}

/// A Claude subscription signed in earlier whose risk acceptance was withdrawn.
fn withdrawn_claude() -> ConnectionRecord {
    record(
        serde_json::json!({"id": "claude", "revision": "4", "name": "Claude",
        "profile": "ClaudeSubscriptionMessages", "state": "Disabled", "credentialRef": "h-claude",
        "subscriptionRiskAccepted": false, "endpoint": "https://api.anthropic.com",
        "lifecycle": "Disabled", "credentialRevision": "1"}),
    )
}

#[test]
fn an_unaccepted_risk_notice_gates_only_what_reaches_the_provider() {
    let mut h = panel(vec![withdrawn_claude()], None);
    // Nothing that contacts the provider.
    for gated in [
        "Sign in",
        "Sign in again",
        "Test connection",
        "Refresh models",
    ] {
        assert!(
            !button(&h, gated),
            "{gated} is shown before the notice is accepted"
        );
    }
    assert!(button(&h, "Accept and enable"));
    // Local edits, turning it off, removal and deleting the credential stay available.
    for local in ["Edit", "Turn off", "Remove connection", "Sign out"] {
        assert!(button(&h, local), "{local} is hidden by the risk notice");
    }
    let signed_out = press(&mut h, "Sign out");
    assert_eq!(signed_out["kind"], "SignIn");
    assert_eq!(signed_out["action"], "Disconnect");
    let removed = press(&mut h, "Remove connection");
    assert_eq!(removed["kind"], "RemoveConnection");
    assert_eq!(removed["expectedRevision"], "4");
    h.get_by_role_and_label(Role::Button, "Edit").click();
    h.run();
    assert!(button(&h, "Save connection"));
}

#[test]
fn the_session_only_fallback_follows_the_secret_store_state_not_a_message() {
    let locked = SecretStoreStatus {
        name: "macOS Keychain".into(),
        session_only: false,
        unavailable: Some("the keychain is locked".into()),
    };
    // No message mentions the store (a background refresh hit it): the fallback is offered.
    let mut h = panel(Vec::new(), Some(locked));
    assert!(
        h.query_all_by_label_contains("the keychain is locked")
            .next()
            .is_some()
    );
    h.get_by_role_and_label(Role::Button, SESSION_ONLY_LABEL)
        .click();
    h.run();
    assert!(matches!(
        h.state().actions.as_slice(),
        [ConnectionAction::UseSessionOnly]
    ));
    // A message that merely mentions the store offers nothing while the store is available.
    let available = SecretStoreStatus {
        name: "macOS Keychain".into(),
        session_only: false,
        unavailable: None,
    };
    let mut h = panel(Vec::new(), Some(available));
    h.state_mut().panel.message = Some((
        devworks_desktop::widgets::Tone::Bad,
        "Adding the connection: the secret store refused the key".into(),
    ));
    h.run();
    assert!(!button(&h, SESSION_ONLY_LABEL));
    // Once session-only, it is not offered again.
    let session = SecretStoreStatus {
        name: "memory".into(),
        session_only: true,
        unavailable: Some("the keychain is locked".into()),
    };
    let h = panel(Vec::new(), Some(session));
    assert!(!button(&h, SESSION_ONLY_LABEL));
}

fn type_in(h: &mut egui_kittest::Harness<'_, Panel>, role: Role, label: &str, text: &str) {
    h.get_by_role_and_label(role, label).focus();
    h.run();
    h.get_by_role_and_label(role, label).type_text(text);
    h.run();
}

/// Opens the editor and fills the OpenRouter constraint fields.
fn edit_constraints(h: &mut egui_kittest::Harness<'_, Panel>, provider: &str) {
    h.get_by_role_and_label(Role::Button, "Edit").click();
    h.run();
    type_in(
        h,
        Role::TextInput,
        "OpenRouter allowed models",
        "anthropic/*, openai/gpt-5*",
    );
    type_in(
        h,
        Role::TextInput,
        "OpenRouter excluded models",
        "anthropic/claude-haiku*",
    );
    type_in(
        h,
        Role::MultilineTextInput,
        "OpenRouter provider routing",
        provider,
    );
}

#[test]
fn settings_edits_open_router_constraints_and_turns_a_connection_off() {
    let mut r = record(
        serde_json::json!({"id": "or", "revision": "2", "name": "OpenRouter",
        "profile": "OpenRouterChat", "state": "Enabled", "credentialRef": "h-or",
        "subscriptionRiskAccepted": false, "endpoint": "https://openrouter.ai/api/v1",
        "lifecycle": "Connected", "credentialRevision": "1"}),
    );
    r.enabled = Some(true);
    let mut h = panel(vec![r.clone()], None);
    let off = press(&mut h, "Turn off");
    assert_eq!(off["kind"], "Connection");
    assert_eq!(off["enabled"], false);
    assert_eq!(off["endpoint"], "https://openrouter.ai/api/v1");

    // An invalid provider object is refused in place, never sent.
    let mut h = panel(vec![r.clone()], None);
    edit_constraints(&mut h, r#"{"sort": "cheapest"}"#);
    h.state_mut().actions.clear();
    h.get_by_role_and_label(Role::Button, "Save connection")
        .click();
    h.run();
    assert!(h.state().actions.is_empty());
    assert!(
        h.query_all_by_label_contains("The provider routing is not valid")
            .next()
            .is_some()
    );
    // A valid one is sent with the lists, as the wire members.
    let mut h = panel(vec![r.clone()], None);
    edit_constraints(
        &mut h,
        r#"{"order": ["anthropic"], "allowFallbacks": false}"#,
    );
    let saved = press(&mut h, "Save connection");
    assert_eq!(
        saved["openRouter"],
        serde_json::json!({"allowedModels": ["anthropic/*", "openai/gpt-5*"],
            "excludedModels": ["anthropic/claude-haiku*"],
            "provider": {"order": ["anthropic"], "allowFallbacks": false}})
    );

    // A turned-off connection says so and offers to turn it on.
    r.enabled = Some(false);
    let h = panel(vec![r], None);
    assert!(
        h.query_all_by_label_contains("Turned off: not used for routing")
            .next()
            .is_some()
    );
    assert!(button(&h, "Turn on"));
}

/// Opens a combo box by its accessible label and picks an option.
fn choose(h: &mut egui_kittest::Harness<'_, Panel>, combo: &str, option: &str) {
    h.get_by_role_and_label(Role::ComboBox, combo).click();
    h.run();
    h.get_by_role_and_label(Role::Button, option).click();
    h.run();
}

#[test]
fn a_key_hidden_by_another_kind_is_never_sent_to_the_secret_store() {
    let mut h = panel(Vec::new(), None);
    choose(&mut h, "Kind", "OpenRouter");
    type_in(
        &mut h,
        Role::TextInput,
        NEW_KEY_LABEL,
        "sk-typed-then-hidden",
    );
    // A subscription takes no key: the field is hidden and its text discarded.
    choose(&mut h, "Kind", "Claude subscription");
    assert!(
        h.query_by_role_and_label(Role::TextInput, NEW_KEY_LABEL)
            .is_none()
    );
    type_in(&mut h, Role::TextInput, NEW_NAME_LABEL, "Claude");
    match press_any(&mut h, ADD_LABEL) {
        ConnectionAction::Configure { action, key, .. } => {
            assert_eq!(action["profile"], "ClaudeSubscriptionMessages");
            assert!(key.is_none(), "the hidden key is never staged");
        }
        other => panic!("{other:?}"),
    }
    // A key hidden by a kind change and shown again by switching back is gone: it was
    // discarded when it was hidden, not kept for a later submission.
    let mut h = panel(Vec::new(), None);
    choose(&mut h, "Kind", "OpenRouter");
    type_in(
        &mut h,
        Role::TextInput,
        NEW_KEY_LABEL,
        "sk-hidden-then-shown",
    );
    choose(&mut h, "Kind", "Codex subscription");
    choose(&mut h, "Kind", "OpenRouter");
    type_in(&mut h, Role::TextInput, NEW_NAME_LABEL, "x");
    match press_any(&mut h, ADD_LABEL) {
        ConnectionAction::Configure { key, .. } => assert!(key.is_none()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_model_binding_hidden_by_another_profile_is_not_sent() {
    let lm = record(
        serde_json::json!({"id": "lmstudio", "revision": "1", "name": "LM Studio",
        "profile": "CompatibleChat", "state": "Enabled", "subscriptionRiskAccepted": false,
        "endpoint": "http://localhost:1234/v1", "lifecycle": "Connected"}),
    );
    let mut h = panel(vec![lm], None);
    choose(&mut h, "New model profile", "OpenAI-compatible endpoint");
    choose(&mut h, "New model connection", "LM Studio");
    // Another profile: the LM Studio binding is not offered for it, so it is not sent.
    choose(&mut h, "New model profile", "OpenRouter");
    type_in(&mut h, Role::TextInput, "New model ID", "m1");
    match press_any(&mut h, "Add model") {
        ConnectionAction::Configure { action, .. } => {
            assert_eq!(action["model"]["endpointProfile"], "OpenRouterChat");
            assert!(action["model"].get("connectionId").is_none(), "{action}");
        }
        other => panic!("{other:?}"),
    }
}

fn press_any(h: &mut egui_kittest::Harness<'_, Panel>, label: &str) -> ConnectionAction {
    h.state_mut().actions.clear();
    h.get_by_role_and_label(Role::Button, label).click();
    h.run();
    let mut actions = std::mem::take(&mut h.state_mut().actions);
    assert_eq!(actions.len(), 1, "{actions:?}");
    actions.remove(0)
}

#[test]
fn an_editor_key_hidden_by_a_profile_change_is_not_sent() {
    let api = record(
        serde_json::json!({"id": "c1", "revision": "1", "name": "Mine",
        "profile": "AnthropicMessages", "state": "Enabled", "subscriptionRiskAccepted": false,
        "endpoint": "https://api.anthropic.com", "lifecycle": "Connected"}),
    );
    let mut h = panel(vec![api], None);
    h.get_by_role_and_label(Role::Button, "Edit").click();
    h.run();
    type_in(&mut h, Role::TextInput, "Mine key", "sk-ant-typed");
    // Another window made it a subscription: the key field is hidden while the editor is open.
    let sub = record(
        serde_json::json!({"id": "c1", "revision": "2", "name": "Mine",
        "profile": "CodexSubscriptionResponses", "state": "Partial",
        "subscriptionRiskAccepted": false, "endpoint": "https://chatgpt.com/backend-api/codex",
        "lifecycle": "Disconnected"}),
    );
    h.state_mut().panel.loaded(vec![sub]);
    h.run();
    assert!(
        h.query_by_role_and_label(Role::TextInput, "Mine key")
            .is_none()
    );
    match press_any(&mut h, "Save connection") {
        ConnectionAction::Configure { key, .. } => assert!(key.is_none(), "never staged"),
        other => panic!("{other:?}"),
    }
}
