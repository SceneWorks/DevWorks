//! Actual Settings UI, native store/gateway and Brave wire adapter; no OS secrets/network.
mod common;
use common::*;
use devworks_adapters::web::brave::BraveSearch;
use devworks_core::ports::secrets::{Secret, SecretError, SecretKey, SecretStore, SessionSecrets};
use devworks_core::ports::storage::Cancellation;
use devworks_core::ports::web::{HopRequest, HopResponse, HttpTransport, NetFault, Resolver};
use devworks_core::protocol::Id;
use devworks_core::services::web::WebPorts;
use devworks_core::settings::catalog::{
    WEB_SEARCH_CREDENTIAL_KEY, WEB_SEARCH_PROVIDER_KEY, descriptor,
};
use devworks_desktop::host::Engine;
use devworks_desktop::{backend::Backend, ops, shell::Shell};
use eframe::egui::accesskit::Role;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

struct Names;
impl Resolver for Names {
    fn resolve(
        &self,
        host: &str,
        _: u16,
        _: Instant,
        _: &[Cancellation],
    ) -> Result<Vec<IpAddr>, NetFault> {
        assert_eq!(host, "api.search.brave.com");
        Ok(vec!["93.184.216.34".parse().unwrap()])
    }
}
#[derive(Default)]
struct RecordedBrave(Mutex<Vec<String>>);
impl HttpTransport for RecordedBrave {
    fn get(&self, request: &HopRequest<'_>) -> Result<HopResponse, NetFault> {
        assert_eq!(request.host_header, "api.search.brave.com");
        let token = request
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("X-Subscription-Token"))
            .unwrap()
            .1;
        self.0.lock().unwrap().push(token.to_owned());
        Ok(HopResponse { status: 200, headers: vec![], body: Box::new(std::io::Cursor::new(
            serde_json::to_vec(&json!({"web":{"results":[{"title":"Settings credential result","url":"https://example.org/result","description":"Recorded Brave wire response"}]},"query":{"more_results_available":false}})).unwrap()
        )) })
    }
}
fn set(backend: &dyn Backend, key: &str, value: Value) {
    let (revision, _) = ops::settings_entries(
        backend.query("settings.read_effective", &ops::settings_read(backend)),
    )
    .unwrap();
    let d = descriptor(key).unwrap();
    let body = json!({"scope":{"kind":"Workspace"},"expectedRevision":revision.to_string(),"entries":[{"key":key,"value":{"schema":d.schema_ref(),"value":value}}]});
    ops::settings_saved(backend.command(
        "settings.save_revision",
        &ops::command_frame(backend, "settings.save_revision", body),
    ))
    .unwrap();
}
fn effective(backend: &dyn Backend, key: &str) -> Value {
    ops::settings_entries(backend.query("settings.read_effective", &ops::settings_read(backend)))
        .unwrap()
        .1
        .into_iter()
        .find(|entry| entry.key.0 == key)
        .unwrap()
        .value
        .value
        .0
}
fn type_into(h: &mut Harness<'_, Shell>, role: Role, label: &str, text: &str) {
    h.get_by_role_and_label(role, label).focus();
    h.step();
    h.get_by_role_and_label(role, label).type_text(text);
    h.step();
}
fn screen(engine: &Engine, root: &Root, key: &str) -> Harness<'static, Shell> {
    let mut h = harness(shell(engine, root, Script::default()), 1440.0, 1200.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    type_into(&mut h, Role::TextInput, "Search settings", key);
    settle(&mut h);
    h
}
fn enter(h: &mut Harness<'_, Shell>, key: &str, secret: &str) {
    let label = descriptor(key).unwrap().label;
    type_into(
        h,
        Role::PasswordInput,
        &format!("Credential for {label}"),
        secret,
    );
    // Neither the raw field nor its undo history is persisted in SettingsMemory.
    assert!(
        !serde_json::to_string(&h.state().settings.memory())
            .unwrap()
            .contains(secret)
    );
    click(h, Role::Button, &format!("Save credential for {label}"));
    assert!(
        h.get_by_role_and_label(Role::PasswordInput, &format!("Credential for {label}"))
            .accesskit_node()
            .value()
            .is_none_or(|value| value.is_empty()),
        "submitted credential is never restored to the editor"
    );
}

#[test]
fn actual_settings_ui_enters_replaces_removes_and_resolves_through_native_brave() {
    let _case = native_runtime_case();
    let root = Root::new("dw91-brave-ui");
    let mut c = config(&root.0);
    let secrets = Arc::new(SessionSecrets::new());
    c.secrets = Some(secrets.clone());
    let transport = Arc::new(RecordedBrave::default());
    let wire = transport.clone();
    c.web = Arc::new(move |secrets| {
        let resolver = Arc::new(Names);
        WebPorts {
            resolver: resolver.clone(),
            transport: wire.clone(),
            providers: vec![Arc::new(BraveSearch::new(resolver, wire.clone()))],
            secrets,
        }
    });
    let engine = Engine::start(c).unwrap();
    let backend = engine.backend();
    set(&*backend, WEB_SEARCH_PROVIDER_KEY, json!("brave"));
    let mut h = screen(&engine, &root, WEB_SEARCH_CREDENTIAL_KEY);
    enter(&mut h, WEB_SEARCH_CREDENTIAL_KEY, "dw91-first-test-key");
    assert!(has_text(&h, "Settings saved."));
    let first = effective(&*backend, WEB_SEARCH_CREDENTIAL_KEY);
    assert!(
        first["secretRef"]
            .as_str()
            .unwrap()
            .starts_with("dwsecret-")
    );
    assert_eq!(secrets.len(), 1);
    assert!(
        !serde_json::to_string(&h.state().settings.memory())
            .unwrap()
            .contains("dw91-first-test-key")
    );
    let hits = ops::web_results(backend.query(
        "web.search",
        &ops::web_search(&*backend, "settings credential", "brave"),
    ))
    .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(*transport.0.lock().unwrap(), ["dw91-first-test-key"]);
    enter(&mut h, WEB_SEARCH_CREDENTIAL_KEY, "dw91-replaced-test-key");
    let second = effective(&*backend, WEB_SEARCH_CREDENTIAL_KEY);
    assert_ne!(first, second);
    ops::web_results(backend.query(
        "web.search",
        &ops::web_search(&*backend, "replacement", "brave"),
    ))
    .unwrap();
    assert_eq!(
        transport.0.lock().unwrap().last().unwrap(),
        "dw91-replaced-test-key"
    );
    click(
        &mut h,
        Role::Button,
        &format!(
            "Remove credential for {}",
            descriptor(WEB_SEARCH_CREDENTIAL_KEY).unwrap().label
        ),
    );
    assert!(effective(&*backend, WEB_SEARCH_CREDENTIAL_KEY).is_null());
    assert!(
        ops::web_results(backend.query(
            "web.search",
            &ops::web_search(&*backend, "removed", "brave")
        ))
        .is_err()
    );
    assert_eq!(
        transport.0.lock().unwrap().len(),
        2,
        "removed credential never sends another request"
    );
    h.state_mut().on_exit();
    let memory = std::fs::read_to_string(root.memory()).unwrap();
    assert!(!memory.contains("dw91-first-test-key") && !memory.contains("dw91-replaced-test-key"));
    drop(h);
    drop(backend);
    engine.shutdown();
}

struct Denied;
impl SecretStore for Denied {
    fn name(&self) -> &'static str {
        "Unavailable test secret store"
    }
    fn put(&self, _: &SecretKey, _: &Secret) -> Result<(), SecretError> {
        Err(SecretError::Denied)
    }
    fn get(&self, _: &SecretKey) -> Result<Option<Secret>, SecretError> {
        Ok(None)
    }
    fn delete(&self, _: &SecretKey) -> Result<bool, SecretError> {
        Ok(false)
    }
    fn list(&self, _: &str) -> Result<Vec<Id>, SecretError> {
        Ok(vec![])
    }
}
#[test]
fn unavailable_secret_store_reports_refusal_without_saving_a_reference() {
    let _case = native_runtime_case();
    let root = Root::new("dw91-denied");
    let mut c = config(&root.0);
    c.secrets = Some(Arc::new(Denied));
    let engine = Engine::start(c).unwrap();
    let backend = engine.backend();
    let mut h = screen(&engine, &root, "github.credential");
    enter(&mut h, "github.credential", "dw91-denied-test-key");
    assert!(has_text(&h, "Credential was not saved"));
    assert!(has_text(&h, "denied"));
    assert!(effective(&*backend, "github.credential").is_null());
    assert!(!h.state().settings.saving);
    assert!(
        !serde_json::to_string(&h.state().settings.memory())
            .unwrap()
            .contains("dw91-denied-test-key")
    );
    drop(h);
    drop(backend);
    engine.shutdown();
}

#[derive(Default)]
struct GatedStore {
    inner: SessionSecrets,
    state: Mutex<(bool, bool)>,
    changed: Condvar,
    puts: AtomicUsize,
}
impl GatedStore {
    fn pause_next(&self) {
        *self.state.lock().unwrap() = (false, false);
    }
    fn entered(&self) {
        let state = self
            .changed
            .wait_timeout_while(self.state.lock().unwrap(), SETTLE_BOUND, |s| !s.0)
            .unwrap()
            .0;
        let entered = state.0;
        drop(state);
        assert!(entered, "native secret staging starts");
    }
    fn release(&self) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).1 = true;
        self.changed.notify_all();
    }
}
impl SecretStore for GatedStore {
    fn name(&self) -> &'static str {
        "Controlled in-memory credentials"
    }
    fn put(&self, key: &SecretKey, value: &Secret) -> Result<(), SecretError> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        self.changed.notify_all();
        let state = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(30), |s| !s.1)
            .unwrap()
            .0;
        assert!(state.1, "test releases native staging");
        drop(state);
        self.inner.put(key, value)
    }
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        self.inner.get(key)
    }
    fn delete(&self, key: &SecretKey) -> Result<bool, SecretError> {
        self.inner.delete(key)
    }
    fn list(&self, namespace: &str) -> Result<Vec<Id>, SecretError> {
        self.inner.list(namespace)
    }
}
struct Release(Arc<GatedStore>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[test]
fn reset_controls_wait_for_native_credential_intake_then_allow_explicit_removal() {
    let _case = native_runtime_case();
    let root = Root::new("dw91-intake-reset");
    let secrets = Arc::new(GatedStore::default());
    secrets.release();
    let mut c = config(&root.0);
    c.secrets = Some(secrets.clone());
    let engine = Engine::start(c).unwrap();
    let backend = engine.backend();
    let original = backend
        .stage_setting_secret(Secret::new("dw91-original-test-key"))
        .unwrap();
    set(
        &*backend,
        "github.credential",
        json!({"secretRef": original}),
    );
    let mut h = screen(&engine, &root, "github.credential");
    h.state_mut()
        .settings
        .view
        .as_mut()
        .unwrap()
        .edit(
            devworks_core::settings::catalog::WEB_TIMEOUT_KEY,
            json!(7000),
        )
        .unwrap();
    settle(&mut h);
    secrets.pause_next();
    let label = descriptor("github.credential").unwrap().label;
    type_into(
        &mut h,
        Role::PasswordInput,
        &format!("Credential for {label}"),
        "dw91-pending-test-key",
    );
    let button = format!("Save credential for {label}");
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, &button)
        .scroll_to_me();
    settle(&mut h);
    let _release = Release(secrets.clone());
    h.get_by_role_and_label(Role::Button, &button).click();
    step_until(&mut h, "replacement intake", |_| {
        secrets.puts.load(Ordering::SeqCst) == 2
    });
    secrets.entered();
    h.step();
    assert!(
        h.get_by_role_and_label(Role::Button, &format!("Reset {label} to default"))
            .accesskit_node()
            .is_disabled(),
        "a later reset cannot be silently overwritten by prepared intake"
    );
    assert!(
        h.get_by_role_and_label(Role::Button, "Discard all")
            .accesskit_node()
            .is_disabled()
    );
    secrets.release();
    settle(&mut h);
    assert_ne!(
        effective(&*backend, "github.credential"),
        json!({"secretRef":original})
    );
    click(&mut h, Role::Button, &format!("Reset {label} to default"));
    click(&mut h, Role::Button, "Save changes");
    assert!(effective(&*backend, "github.credential").is_null());
    assert_eq!(secrets.puts.load(Ordering::SeqCst), 2);
    drop(h);
    drop(backend);
    engine.shutdown();
}

#[test]
fn secret_intake_keeps_captured_revision_and_reference_across_conflict_without_restaging() {
    let _case = native_runtime_case();
    let root = Root::new("dw91-cas");
    let mut c = config(&root.0);
    let secrets = Arc::new(GatedStore::default());
    c.secrets = Some(secrets.clone());
    let engine = Engine::start(c).unwrap();
    let backend = engine.backend();
    let mut h = screen(&engine, &root, "github.credential");
    h.state_mut()
        .settings
        .view
        .as_mut()
        .unwrap()
        .edit(WEB_SEARCH_PROVIDER_KEY, json!("brave"))
        .unwrap();
    // The draft banner changes layout; render it before focusing the password field.
    settle(&mut h);
    let label = descriptor("github.credential").unwrap().label;
    type_into(
        &mut h,
        Role::PasswordInput,
        &format!("Credential for {label}"),
        "dw91-conflict-test-key",
    );
    let button = format!("Save credential for {label}");
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, &button)
        .scroll_to_me();
    settle(&mut h);
    assert!(
        !h.get_by_role_and_label(Role::Button, &button)
            .accesskit_node()
            .is_disabled(),
        "credential entry enables its save gesture"
    );
    let _release = Release(secrets.clone());
    h.get_by_role_and_label(Role::Button, &button).click();
    let deadline = Instant::now() + SETTLE_BOUND;
    while secrets.puts.load(Ordering::SeqCst) == 0 {
        h.step();
        assert!(
            Instant::now() < deadline,
            "native secret staging: saving={}, message={:?}, activities={:?}",
            h.state().settings.saving,
            h.state().settings.message,
            h.state().client().unwrap().activities()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    secrets.entered();
    set(
        &*backend,
        devworks_core::settings::catalog::WEB_TIMEOUT_KEY,
        json!(7000),
    );
    secrets.release();
    settle(&mut h);
    assert!(has_text(&h, "Settings changed elsewhere"));
    assert!(effective(&*backend, "github.credential").is_null());
    assert_eq!(
        h.state()
            .settings
            .view
            .as_ref()
            .unwrap()
            .item(WEB_SEARCH_PROVIDER_KEY)
            .unwrap()
            .draft,
        Some(json!("brave")),
        "other draft survives credential-only submission"
    );
    let memory = serde_json::to_string(&h.state().settings.memory()).unwrap();
    assert!(memory.contains("dwsecret-"));
    assert!(!memory.contains("dw91-conflict-test-key"));
    click(&mut h, Role::Button, "Save changes");
    assert!(has_text(&h, "Settings saved."));
    assert!(effective(&*backend, "github.credential")["secretRef"].is_string());
    assert_eq!(
        effective(&*backend, WEB_SEARCH_PROVIDER_KEY),
        json!("brave")
    );
    assert_eq!(
        secrets.puts.load(Ordering::SeqCst),
        1,
        "retry reuses the staged handle"
    );
    drop(h);
    drop(backend);
    engine.shutdown();
}
