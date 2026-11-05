//! Settings → Provider connections (ROUTE-09, ROUTE-10, ROUTE-18, SEC-01; DW-18): the native
//! controls for Codex and Claude subscriptions, Anthropic/OpenRouter/OpenAI API keys, OpenAI-
//! compatible endpoints (Ollama, LM Studio), embedding and SystemOne endpoints, plus the
//! conversational catalog's context windows and connection bindings.
//!
//! Everything shown comes from `router.connections` and `router.catalog` (schema revision 3) and
//! every change is a `router.configure` command: the panel decides nothing. Secrets never sit in
//! this state longer than one submission: an API key or pasted sign-in code is handed to the host
//! intake (the OS secret store, or memory for a pasted code) by the request that publishes it, and
//! the field is cleared at once. The Claude subscription shows the exact risk notice and can only
//! be enabled by accepting it.
use crate::backend::SecretStoreStatus;
use crate::widgets::{self, Tone, chip};
use devworks_core::ports::secrets::Secret;
use devworks_core::protocol::Id;
use devworks_core::protocol::records::{
    ConnectionRecord, OpenRouterConstraints, OpenRouterProviderRouting, RouteCatalogModel,
    RouteModel,
};
use devworks_core::services::router::connections::profiles::{
    Auth, PROFILES, Profile, RISK_NOTICE, profile, risk_notice_sha256,
};
use eframe::egui::{self, RichText, Ui};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The accessible label of the panel's add-connection button.
pub const ADD_LABEL: &str = "Add connection";
/// Efforts a catalog model may declare (never `Default`, which is the provider's own).
pub const EFFORTS: [&str; 6] = ["Minimal", "Low", "Medium", "High", "XHigh", "Max"];
/// The OpenRouter auto-router cost tiers (`""` is OpenRouter's default).
pub const COST_TIERS: [&str; 6] = ["", "low", "medium", "high", "xhigh", "max"];
/// The accessible label of the session-only fallback button.
pub const SESSION_ONLY_LABEL: &str = "Keep credentials for this session only";

/// What the panel asks the shell to do.
#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionAction {
    /// Reload connections and the catalog.
    Reload,
    /// A `router.configure` action; `key` (an API key the user typed) is staged in the OS secret
    /// store first and its handle set as the action's `credentialRef`; `code` (a pasted
    /// `code#state`) is attached to the open sign-in `flow` first.
    Configure {
        label: String,
        action: Value,
        key: Option<Secret>,
        code: Option<(Id, Secret)>,
    },
    /// The user's explicit choice to keep credentials for this session only.
    UseSessionOnly,
}

/// Per-connection editing state.
#[derive(Default)]
struct Edit {
    name: String,
    endpoint: String,
    /// A replacement key being typed (cleared when submitted).
    key: String,
    /// A pasted `code#state` (cleared when submitted).
    code: String,
    notice_read: bool,
    open: bool,
    /// OpenRouter constraints: comma-separated model patterns, the cost tier index into
    /// [`COST_TIERS`] and the provider-routing object as JSON.
    allowed: String,
    excluded: String,
    cost_tier: usize,
    provider: String,
    constraints_error: Option<String>,
}

/// One catalog row's editable fields.
#[derive(Default, Clone, PartialEq)]
struct ModelEdit {
    context: String,
    connection: String,
    enabled: bool,
}

#[derive(Default)]
pub struct ConnectionsPanel {
    pub records: Vec<ConnectionRecord>,
    pub loaded: bool,
    pub loading: bool,
    pub error: Option<String>,
    pub catalog: Vec<RouteCatalogModel>,
    pub catalog_error: Option<String>,
    pub message: Option<(Tone, String)>,
    /// Commands submitted and not answered yet (their labels).
    pub busy: Vec<(u64, String)>,
    /// The latest load of each list, the pages gathered so far, when it started, and the next
    /// configure sequence number (owned by the shell).
    pub generation: u64,
    pub gathering: Vec<ConnectionRecord>,
    pub catalog_generation: u64,
    pub catalog_gathering: Vec<RouteCatalogModel>,
    pub last_load: Option<std::time::Instant>,
    pub sequence: u64,
    /// The secret facility, whether it is session-only and whether it is refusing requests
    /// (from the host).
    pub secret_store: Option<SecretStoreStatus>,
    edits: BTreeMap<String, Edit>,
    models: BTreeMap<String, ModelEdit>,
    /// The stored values each draft started from (a draft equal to its base is untouched).
    model_bases: BTreeMap<String, ModelEdit>,
    add_profile: usize,
    add_name: String,
    add_endpoint: String,
    add_key: String,
    new_model: NewModel,
}

#[derive(Default)]
struct NewModel {
    id: String,
    name: String,
    profile: usize,
    upstream: String,
    context: String,
    connection: String,
    efforts: [bool; 6],
    vision: bool,
    tools: bool,
}

fn profile_label(wire: &str) -> &'static str {
    profile(wire).map_or("Unknown profile", |p| p.label)
}
fn wire<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}
fn lifecycle_tone(l: &str) -> Tone {
    match l {
        "Connected" => Tone::Good,
        "Authorizing" | "Refreshing" => Tone::Accent,
        "Disabled" | "Disconnected" => Tone::Neutral,
        "Unavailable" => Tone::Attention,
        _ => Tone::Bad,
    }
}
fn lifecycle_text(l: &str) -> &'static str {
    match l {
        "Connected" => "Connected",
        "Authorizing" => "Signing in",
        "Refreshing" => "Refreshing credentials",
        "ReauthenticationRequired" => "Sign in again",
        "Unavailable" => "Unavailable",
        "Disabled" => "Disabled",
        _ => "Not connected",
    }
}
fn ago(now_ms: i64, at: i64) -> String {
    let s = ((now_ms - at) / 1000).max(0);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => format!("{} d ago", s / 86400),
    }
}

impl ConnectionsPanel {
    /// Replaces the listed connections (a complete load).
    pub fn loaded(&mut self, records: Vec<ConnectionRecord>) {
        for r in &records {
            let e = self.edits.entry(r.id.as_str().to_owned()).or_default();
            if !e.open {
                e.name = r.name.0.clone();
                e.endpoint = r.endpoint.as_ref().map(|u| u.0.clone()).unwrap_or_default();
                let o = r.open_router.as_ref();
                e.allowed = o.map(|o| joined(&o.allowed_models.0)).unwrap_or_default();
                e.excluded = o.map(|o| joined(&o.excluded_models.0)).unwrap_or_default();
                e.cost_tier = o
                    .and_then(|o| o.cost_tier.as_ref())
                    .and_then(|t| COST_TIERS.iter().position(|c| *c == wire(t)))
                    .unwrap_or(0);
                e.provider = o
                    .and_then(|o| o.provider.as_ref())
                    .and_then(|p| serde_json::to_string(p).ok())
                    .unwrap_or_default();
                e.constraints_error = None;
            }
        }
        self.edits
            .retain(|k, _| records.iter().any(|r| r.id.as_str() == k));
        self.records = records;
        self.loaded = true;
        self.loading = false;
        self.error = None;
    }
    /// Replaces the catalog (a complete load), keeping unsaved row edits.
    pub fn catalog_loaded(&mut self, models: Vec<RouteCatalogModel>) {
        for m in &models {
            let stored = ModelEdit {
                context: m
                    .context_tokens
                    .as_ref()
                    .map(|c| c.0.clone())
                    .unwrap_or_default(),
                connection: m
                    .connection_id
                    .as_ref()
                    .map(|c| c.as_str().to_owned())
                    .unwrap_or_default(),
                enabled: m.enabled,
            };
            let key = m.id.as_str().to_owned();
            // A row the user has not edited follows the stored model (a save, discovery or
            // another window changed it); an edited row keeps its draft until saved.
            let untouched = match (self.models.get(&key), self.model_bases.get(&key)) {
                (Some(draft), Some(base)) => draft == base,
                _ => true,
            };
            if untouched {
                self.models.insert(key.clone(), stored.clone());
            }
            self.model_bases.insert(key, stored);
        }
        self.models
            .retain(|k, _| models.iter().any(|m| m.id.as_str() == k));
        self.model_bases
            .retain(|k, _| models.iter().any(|m| m.id.as_str() == k));
        self.catalog = models;
        self.catalog_error = None;
    }

    pub fn ui(&mut self, ui: &mut Ui, now_ms: i64, actions: &mut Vec<ConnectionAction>) {
        ui.horizontal(|ui| {
            if widgets::secondary_button(ui, "Reload").clicked() {
                actions.push(ConnectionAction::Reload);
            }
            if let Some(s) = &self.secret_store {
                let text = if s.session_only {
                    format!("Credentials: {} (this session only)", s.name)
                } else {
                    format!("Credentials: {}", s.name)
                };
                chip(
                    ui,
                    text,
                    if s.session_only {
                        Tone::Attention
                    } else {
                        Tone::Neutral
                    },
                );
            }
            for (_, b) in &self.busy {
                chip(ui, format!("{b}…"), Tone::Accent);
            }
        });
        if let Some((tone, text)) = &self.message {
            chip(ui, text.clone(), *tone);
        }
        // The fallback follows the facility's own state (any path that hit it: staging a key,
        // a sign-in, a refresh), never the wording of a message.
        if let Some(s) = &self.secret_store
            && !s.session_only
            && let Some(why) = &s.unavailable
        {
            chip(
                ui,
                format!("The secret store is unavailable: {why}"),
                Tone::Bad,
            );
            if ui
                .button(SESSION_ONLY_LABEL)
                .on_hover_text("Nothing is written to disk; you sign in again after a restart.")
                .clicked()
            {
                actions.push(ConnectionAction::UseSessionOnly);
            }
        }
        if let Some(e) = &self.error {
            chip(
                ui,
                format!("Connections could not be loaded: {e}"),
                Tone::Bad,
            );
        }
        let mut scroll_actions = Vec::new();
        egui::ScrollArea::vertical()
            .id_salt("connections-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if !self.loaded {
                    widgets::caption(ui, "Loading provider connections…");
                } else if self.records.is_empty() {
                    widgets::caption(
                        ui,
                        "No provider is connected. Add a subscription, an API key or a local \
                         OpenAI-compatible endpoint below; until then no route is available.",
                    );
                }
                let records = self.records.clone();
                for r in &records {
                    self.connection_ui(ui, r, now_ms, &mut scroll_actions);
                    ui.add_space(10.0);
                }
                widgets::divider(ui);
                self.add_ui(ui, &mut scroll_actions);
                widgets::divider(ui);
                self.catalog_ui(ui, &mut scroll_actions);
            });
        actions.extend(scroll_actions);
    }

    fn connection_ui(
        &mut self,
        ui: &mut Ui,
        r: &ConnectionRecord,
        now_ms: i64,
        actions: &mut Vec<ConnectionAction>,
    ) {
        let id = r.id.as_str().to_owned();
        let wire_profile = wire(&r.profile);
        let p = profile(&wire_profile);
        let lifecycle = r.lifecycle.as_ref().map(wire).unwrap_or_default();
        let revision = r.revision.get().to_string();
        ui.group(|ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&r.name.0).strong());
                chip(ui, profile_label(&wire_profile), Tone::Neutral);
                chip(ui, lifecycle_text(&lifecycle), lifecycle_tone(&lifecycle));
                if let Some(h) = r.healthy {
                    chip(
                        ui,
                        if h { "Test passed" } else { "Test failed" },
                        if h { Tone::Good } else { Tone::Bad },
                    );
                }
                if p.is_some_and(|p| p.subscription) {
                    chip(ui, "Subscription billing", Tone::Neutral);
                } else {
                    chip(ui, "API billing", Tone::Neutral);
                }
                if r.enabled == Some(false) {
                    chip(ui, "Turned off: not used for routing", Tone::Attention);
                }
            });
            if let Some(e) = &r.endpoint {
                widgets::caption(ui, format!("Endpoint: {}", e.0));
            }
            if let Some(reason) = &r.state_reason {
                widgets::caption(ui, reason.0.clone());
            }
            let mut facts = Vec::new();
            facts.push(if r.credential_ref.is_some() {
                format!(
                    "Credential stored (revision {})",
                    r.credential_revision.as_ref().map_or(0, |c| c.get())
                )
            } else {
                "No credential stored".into()
            });
            if let Some(n) = &r.discovered_model_count {
                facts.push(format!("{} models discovered", n.get()));
            }
            if let Some(t) = &r.checked_at_ms {
                facts.push(format!("checked {}", ago(now_ms, t.get())));
            }
            if let Some(t) = &r.access_expires_at_ms {
                facts.push(if t.get() > now_ms {
                    format!(
                        "access token valid for {} min",
                        (t.get() - now_ms + 59_999) / 60_000
                    )
                } else {
                    "access token expired (refreshed at its next use)".into()
                });
            }
            widgets::caption(ui, facts.join(" · "));
            if let Some(ids) = &r.discovered_model_ids {
                let names: Vec<&str> = ids.0.iter().map(|t| t.0.as_str()).take(12).collect();
                if !names.is_empty() {
                    widgets::caption(
                        ui,
                        format!(
                            "Models: {}{}",
                            names.join(", "),
                            if ids.0.len() > names.len() {
                                ", …"
                            } else {
                                ""
                            }
                        ),
                    );
                }
            }
            let Some(p) = p else {
                return;
            };
            // The Claude subscription: the exact notice. Until it is accepted nothing reaches
            // the provider (sign-in, test, refresh); local edits, turning the connection off,
            // removing it and deleting its credential stay available.
            let gated = p.risk_gated && !r.subscription_risk_accepted;
            if p.risk_gated {
                self.risk_ui(ui, r, &id, &revision, actions);
            }
            if !gated && let Some(ch) = &r.sign_in {
                self.challenge_ui(ui, r, &id, ch, actions);
            }
            ui.horizontal_wrapped(|ui| {
                match p.auth {
                    Auth::DeviceCode | Auth::AuthorizationCode if !gated && r.sign_in.is_none() => {
                        let label = if r.credential_ref.is_some() {
                            "Sign in again"
                        } else {
                            "Sign in"
                        };
                        if ui.button(label).clicked() {
                            actions.push(sign_in(&id, "BeginSignIn", None, "Starting sign-in"));
                        }
                    }
                    _ => {}
                }
                if !gated && (r.credential_ref.is_some() || !p.needs_credential()) {
                    if ui.button("Test connection").clicked() {
                        actions.push(sign_in(&id, "Inspect", None, "Testing the connection"));
                    }
                    if ui.button("Refresh models").clicked() {
                        actions.push(sign_in(&id, "Refresh", None, "Refreshing"));
                    }
                }
                if r.credential_ref.is_some() {
                    let what = if p.oauth() { "Sign out" } else { "Remove key" };
                    if ui.button(what).clicked() {
                        actions.push(sign_in(&id, "Disconnect", None, "Removing the credential"));
                    }
                }
                let e = self.edits.entry(id.clone()).or_default();
                if ui
                    .button(if e.open { "Close editor" } else { "Edit" })
                    .clicked()
                {
                    e.open = !e.open;
                }
                let enabled = r.enabled != Some(false);
                if ui
                    .button(if enabled { "Turn off" } else { "Turn on" })
                    .clicked()
                {
                    actions.push(ConnectionAction::Configure {
                        label: if enabled {
                            "Turning the connection off".into()
                        } else {
                            "Turning the connection on".into()
                        },
                        action: json!({"kind": "Connection", "connectionId": id,
                            "expectedRevision": revision, "name": r.name.0, "profile": p.name,
                            "endpoint": r.endpoint.as_ref().map(|u| u.0.as_str()).unwrap_or(""),
                            "enabled": !enabled}),
                        key: None,
                        code: None,
                    });
                }
                if ui.button("Remove connection").clicked() {
                    actions.push(ConnectionAction::Configure {
                        label: "Removing the connection".into(),
                        action: json!({"kind": "RemoveConnection", "connectionId": id,
                            "expectedRevision": revision}),
                        key: None,
                        code: None,
                    });
                }
            });
            if self.edits.get(&id).is_some_and(|e| e.open) {
                self.editor_ui(ui, r, p, &id, &revision, actions);
            }
        });
    }

    fn risk_ui(
        &mut self,
        ui: &mut Ui,
        r: &ConnectionRecord,
        id: &str,
        revision: &str,
        actions: &mut Vec<ConnectionAction>,
    ) {
        let e = self.edits.entry(id.to_owned()).or_default();
        if r.subscription_risk_accepted {
            ui.horizontal(|ui| {
                chip(ui, "Risk notice accepted", Tone::Attention);
                if ui.button("Withdraw acceptance").clicked() {
                    actions.push(ConnectionAction::Configure {
                        label: "Withdrawing the risk acceptance".into(),
                        action: json!({"kind": "SubscriptionRisk", "connectionId": id,
                            "expectedRevision": revision, "noticeSha256": risk_notice_sha256(),
                            "accepted": false}),
                        key: None,
                        code: None,
                    });
                }
            });
            return;
        }
        ui.label(RichText::new("Risk notice").strong());
        ui.label(RISK_NOTICE);
        ui.checkbox(
            &mut e.notice_read,
            "I have read this notice and accept the risk",
        );
        if ui
            .add_enabled(e.notice_read, egui::Button::new("Accept and enable"))
            .clicked()
        {
            e.notice_read = false;
            actions.push(ConnectionAction::Configure {
                label: "Accepting the risk notice".into(),
                action: json!({"kind": "SubscriptionRisk", "connectionId": id,
                    "expectedRevision": revision, "noticeSha256": risk_notice_sha256(),
                    "accepted": true}),
                key: None,
                code: None,
            });
        }
    }

    fn challenge_ui(
        &mut self,
        ui: &mut Ui,
        r: &ConnectionRecord,
        id: &str,
        ch: &devworks_core::protocol::records::SignInChallenge,
        actions: &mut Vec<ConnectionAction>,
    ) {
        let _ = r;
        if let Some(code) = &ch.user_code {
            let url = ch
                .verification_url
                .as_ref()
                .map(|u| u.0.clone())
                .unwrap_or_default();
            ui.label(format!(
                "Open {url} in your browser, sign in to ChatGPT and enter:"
            ));
            ui.label(RichText::new(&code.0).monospace().size(20.0));
            ui.hyperlink_to("Open the sign-in page", url);
            widgets::caption(
                ui,
                "DevWorks finishes the sign-in as soon as the code is approved.",
            );
        }
        if let Some(url) = &ch.authorize_url {
            ui.label("Open the Claude sign-in page, approve access and paste the code it shows:");
            ui.hyperlink_to("Open the Claude sign-in page", url.0.clone());
            let e = self.edits.entry(id.to_owned()).or_default();
            ui.horizontal(|ui| {
                let field = ui.add(
                    egui::TextEdit::singleline(&mut e.code)
                        .password(true)
                        .hint_text("code#state"),
                );
                field.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Authorization code")
                });
                if ui
                    .add_enabled(
                        !e.code.trim().is_empty(),
                        egui::Button::new("Finish sign-in"),
                    )
                    .clicked()
                {
                    let pasted = Secret::new(std::mem::take(&mut e.code));
                    actions.push(ConnectionAction::Configure {
                        label: "Finishing sign-in".into(),
                        action: json!({"kind": "SignIn", "connectionId": id,
                            "action": "CompleteSignIn", "flowId": ch.flow_id.as_str()}),
                        key: None,
                        code: Some((ch.flow_id.clone(), pasted)),
                    });
                }
            });
        }
    }

    fn editor_ui(
        &mut self,
        ui: &mut Ui,
        r: &ConnectionRecord,
        p: &'static Profile,
        id: &str,
        revision: &str,
        actions: &mut Vec<ConnectionAction>,
    ) {
        let e = self.edits.entry(id.to_owned()).or_default();
        field(
            ui,
            "Name",
            &format!("{} name", r.name.0),
            &mut e.name,
            false,
        );
        field(
            ui,
            "Endpoint",
            &format!("{} endpoint", r.name.0),
            &mut e.endpoint,
            false,
        );
        if matches!(p.auth, Auth::ApiKey { .. }) {
            let shown = if r.credential_ref.is_some() {
                "Replace key"
            } else {
                "API key"
            };
            field(ui, shown, &format!("{} key", r.name.0), &mut e.key, true);
        }
        let open_router = p.name == "OpenRouterChat";
        if open_router {
            ui.label(RichText::new("OpenRouter constraints").strong());
            field(
                ui,
                "Allowed models (patterns, comma separated; empty allows all)",
                &format!("{} allowed models", r.name.0),
                &mut e.allowed,
                false,
            );
            field(
                ui,
                "Excluded models (patterns, comma separated)",
                &format!("{} excluded models", r.name.0),
                &mut e.excluded,
                false,
            );
            let shown = match COST_TIERS[e.cost_tier.min(COST_TIERS.len() - 1)] {
                "" => "OpenRouter default",
                t => t,
            };
            egui::ComboBox::from_label("Auto-router cost tier")
                .selected_text(shown)
                .show_ui(ui, |ui| {
                    for (i, t) in COST_TIERS.iter().enumerate() {
                        let label = if t.is_empty() {
                            "OpenRouter default"
                        } else {
                            t
                        };
                        ui.selectable_value(&mut e.cost_tier, i, label);
                    }
                });
            ui.label("Provider routing (OpenRouter provider object, JSON)");
            let provider = ui.add(
                egui::TextEdit::multiline(&mut e.provider)
                    .desired_rows(3)
                    .hint_text(r#"{"order": ["anthropic"], "allowFallbacks": false}"#),
            );
            named(
                provider,
                egui::Role::MultilineTextInput,
                format!("{} provider routing", r.name.0),
            );
            if let Some(err) = &e.constraints_error {
                chip(ui, err.clone(), Tone::Bad);
            }
        }
        widgets::caption(
            ui,
            "Changing the endpoint's host drops the stored credential: a key is never sent to \
             another origin.",
        );
        if ui.button("Save connection").clicked() {
            let mut action = json!({"kind": "Connection", "connectionId": id,
                "expectedRevision": revision, "name": e.name.trim(), "profile": p.name,
                "endpoint": e.endpoint.trim()});
            if open_router {
                match constraints_of_edit(e) {
                    Ok(c) => {
                        action["openRouter"] = c;
                        e.constraints_error = None;
                    }
                    Err(err) => {
                        e.constraints_error = Some(err);
                        return;
                    }
                }
            }
            let key = submitted_key(p, &mut e.key);
            actions.push(ConnectionAction::Configure {
                label: "Saving the connection".into(),
                action,
                key,
                code: None,
            });
            e.open = false;
        }
    }

    fn add_ui(&mut self, ui: &mut Ui, actions: &mut Vec<ConnectionAction>) {
        ui.label(RichText::new("Add a connection").strong());
        let current = PROFILES[self.add_profile.min(PROFILES.len() - 1)];
        egui::ComboBox::from_label("Kind")
            .selected_text(current.label)
            .show_ui(ui, |ui| {
                for (i, p) in PROFILES.iter().enumerate() {
                    if ui
                        .selectable_value(&mut self.add_profile, i, p.label)
                        .clicked()
                    {
                        self.add_endpoint = p.default_endpoint.to_owned();
                        // A key typed for another kind is never kept behind a hidden field.
                        self.add_key.clear();
                    }
                }
            });
        let p = PROFILES[self.add_profile.min(PROFILES.len() - 1)];
        if self.add_endpoint.is_empty() {
            self.add_endpoint = p.default_endpoint.to_owned();
        }
        field(ui, "Name", NEW_NAME_LABEL, &mut self.add_name, false);
        field(
            ui,
            "Endpoint",
            NEW_ENDPOINT_LABEL,
            &mut self.add_endpoint,
            false,
        );
        match p.auth {
            Auth::ApiKey { required } => {
                let shown = if required {
                    "API key"
                } else {
                    "API key (optional)"
                };
                field(ui, shown, NEW_KEY_LABEL, &mut self.add_key, true);
            }
            Auth::DeviceCode => widgets::caption(
                ui,
                "Sign in with a device code after adding it (your ChatGPT subscription).",
            ),
            Auth::AuthorizationCode => widgets::caption(
                ui,
                "Not supported by Anthropic for third-party apps: you will be asked to accept \
                 the risk notice before you can sign in. Billed to your Claude subscription, \
                 never to API credits.",
            ),
        }
        let ready = !self.add_name.trim().is_empty() && !self.add_endpoint.trim().is_empty();
        if ui
            .add_enabled(ready, egui::Button::new(ADD_LABEL))
            .clicked()
        {
            // Only a kind that takes a key sends one (and so writes it to the secret store).
            let key = submitted_key(&p, &mut self.add_key);
            actions.push(ConnectionAction::Configure {
                label: "Adding the connection".into(),
                action: json!({"kind": "Connection", "name": self.add_name.trim(),
                    "profile": p.name, "endpoint": self.add_endpoint.trim()}),
                key,
                code: None,
            });
        }
    }

    fn catalog_ui(&mut self, ui: &mut Ui, actions: &mut Vec<ConnectionAction>) {
        ui.label(RichText::new("Models").strong());
        widgets::caption(
            ui,
            "Context windows and the connection that serves each model. An unknown window never \
             satisfies a context requirement; discovery fills it when the provider reports one.",
        );
        if let Some(e) = &self.catalog_error {
            chip(ui, e.clone(), Tone::Bad);
        }
        let connections: Vec<(String, String, String)> = self
            .records
            .iter()
            .map(|r| (r.id.as_str().to_owned(), r.name.0.clone(), wire(&r.profile)))
            .collect();
        let catalog = self.catalog.clone();
        egui::Grid::new("catalog-grid")
            .striped(true)
            .show(ui, |ui| {
                ui.label("Model");
                ui.label("Upstream");
                ui.label("Enabled");
                ui.label("Context tokens");
                ui.label("Connection");
                ui.label("");
                ui.end_row();
                for m in &catalog {
                    let key = m.id.as_str().to_owned();
                    let e = self.models.entry(key.clone()).or_default();
                    let name = m.display_name.0.clone();
                    ui.vertical(|ui| {
                        ui.label(&name);
                        let state = match m.available {
                            Some(true) => "In provider inventory",
                            Some(false) => "Not in provider inventory",
                            None => "Inventory not confirmed",
                        };
                        widgets::caption(ui, state);
                        if let Some(why) = &m.availability_reason {
                            widgets::caption(ui, crate::routing::reason(&why.0));
                        }
                        if let Some(ids) = &m.resolves_to_model_ids {
                            let names: Vec<_> = ids
                                .0
                                .iter()
                                .map(|id| {
                                    catalog.iter().find(|m| &m.id == id).map_or_else(
                                        || id.as_str().to_owned(),
                                        |m| m.display_name.0.clone(),
                                    )
                                })
                                .collect();
                            widgets::caption(
                                ui,
                                if names.is_empty() {
                                    "No current version".into()
                                } else {
                                    format!("Current: {}", names.join(", "))
                                },
                            );
                        }
                    });
                    if crate::routing::is_latest(m) {
                        ui.label("Latest available version");
                        ui.label("Managed");
                        ui.label("From current versions");
                        ui.label("From current versions");
                        ui.label("");
                        ui.end_row();
                        continue;
                    }
                    ui.label(&m.upstream_model_id.0);
                    let r = ui.checkbox(&mut e.enabled, "");
                    named(r, egui::Role::CheckBox, format!("{name} enabled"));
                    let r = ui.add(egui::TextEdit::singleline(&mut e.context).desired_width(90.0));
                    named(r, egui::Role::TextInput, format!("{name} context tokens"));
                    let profile_wire = wire(&m.endpoint_profile);
                    let selected = connections
                        .iter()
                        .find(|(id, _, _)| *id == e.connection)
                        .map_or("Any of its profile".to_owned(), |(_, n, _)| n.clone());
                    let r = egui::ComboBox::from_id_salt(("bind", &key))
                        .selected_text(selected)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut e.connection,
                                String::new(),
                                "Any of its profile",
                            );
                            for (id, name, p) in &connections {
                                if *p == profile_wire {
                                    ui.selectable_value(&mut e.connection, id.clone(), name);
                                }
                            }
                        })
                        .response;
                    named(r, egui::Role::ComboBox, format!("{name} connection"));
                    if ui
                        .button("Save")
                        .on_hover_text(format!("Save {name}"))
                        .clicked()
                    {
                        match model_action(m, e) {
                            Ok(action) => actions.push(ConnectionAction::Configure {
                                label: format!("Saving {}", m.display_name.0),
                                action,
                                key: None,
                                code: None,
                            }),
                            Err(why) => self.message = Some((Tone::Bad, why)),
                        }
                    }
                    ui.end_row();
                }
            });
        ui.add_space(8.0);
        ui.label(RichText::new("Add a custom model").strong());
        let n = &mut self.new_model;
        field(ui, "ID", "New model ID", &mut n.id, false);
        field(ui, "Name", "New model name", &mut n.name, false);
        field(
            ui,
            "Upstream model",
            "New model upstream ID",
            &mut n.upstream,
            false,
        );
        let chat: Vec<&Profile> = PROFILES
            .iter()
            .filter(|p| p.name != "SystemOne" && p.name != "Embedding")
            .collect();
        let current = chat[n.profile.min(chat.len() - 1)];
        field(
            ui,
            "Context tokens",
            "New model context tokens",
            &mut n.context,
            false,
        );
        ui.horizontal(|ui| {
            let r = egui::ComboBox::from_id_salt("new-model-profile")
                .selected_text(current.label)
                .show_ui(ui, |ui| {
                    for (i, p) in chat.iter().enumerate() {
                        if ui.selectable_value(&mut n.profile, i, p.label).clicked() {
                            // A binding to another profile's connection is not offered here.
                            n.connection.clear();
                        }
                    }
                })
                .response;
            named(r, egui::Role::ComboBox, "New model profile".into());
            let r = egui::ComboBox::from_id_salt("new-model-connection")
                .selected_text(
                    connections
                        .iter()
                        .find(|(id, _, _)| *id == n.connection)
                        .map_or("Any of its profile".to_owned(), |(_, name, _)| name.clone()),
                )
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut n.connection, String::new(), "Any of its profile");
                    for (id, name, p) in &connections {
                        if *p == current.name {
                            ui.selectable_value(&mut n.connection, id.clone(), name);
                        }
                    }
                })
                .response;
            named(r, egui::Role::ComboBox, "New model connection".into());
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("Efforts");
            for (i, e) in EFFORTS.iter().enumerate() {
                ui.checkbox(&mut n.efforts[i], *e);
            }
            ui.checkbox(&mut n.vision, "Images");
            ui.checkbox(&mut n.tools, "Tools");
        });
        if ui.button("Add model").clicked() {
            let mut model = json!({"id": n.id.trim(), "displayName": n.name.trim(),
                "endpointProfile": current.name, "upstreamModelId": n.upstream.trim(),
                "enabled": true, "tierMemberships": [],
                "efforts": EFFORTS.iter().zip(n.efforts).filter(|(_, on)| *on).map(|(e, _)| *e).collect::<Vec<_>>(),
                "imageInput": n.vision, "tools": n.tools, "revision": "1"});
            if !n.context.trim().is_empty() {
                model["contextTokens"] = json!(n.context.trim());
            }
            // Only a binding the form offers for the chosen profile is sent.
            if connections
                .iter()
                .any(|(id, _, p)| *id == n.connection && *p == current.name)
            {
                model["connectionId"] = json!(n.connection);
            }
            actions.push(ConnectionAction::Configure {
                label: "Adding the model".into(),
                action: json!({"kind": "Model", "model": model}),
                key: None,
                code: None,
            });
            *n = NewModel::default();
        }
    }
}

/// Accessible labels of the add-connection fields.
pub const NEW_NAME_LABEL: &str = "New connection name";
pub const NEW_ENDPOINT_LABEL: &str = "New connection endpoint";
pub const NEW_KEY_LABEL: &str = "New connection API key";

/// Gives a widget without visible text its accessible name.
fn named(r: egui::Response, role: egui::Role, name: String) {
    r.widget_info(move || egui::WidgetInfo::labeled(role, true, &name));
}

/// A labelled single-line field; a password field never shows its text.
fn field(ui: &mut Ui, shown: &str, accessible: &str, text: &mut String, password: bool) {
    ui.horizontal(|ui| {
        ui.label(shown);
        let r = ui.add(egui::TextEdit::singleline(text).password(password));
        let accessible = accessible.to_owned();
        r.widget_info(move || egui::WidgetInfo::labeled(egui::Role::TextInput, true, &accessible));
    });
}

/// The key a form sends: only for a profile that takes an API key; any other text in the
/// field (typed before the kind changed) is discarded, never staged.
fn submitted_key(p: &Profile, field: &mut String) -> Option<Secret> {
    let typed = std::mem::take(field);
    (matches!(p.auth, Auth::ApiKey { .. }) && !typed.trim().is_empty()).then(|| Secret::new(typed))
}

fn joined(l: &[devworks_core::protocol::support::Text]) -> String {
    l.iter()
        .map(|t| t.0.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `openRouter` member an editor's constraint fields describe (all of them: an empty set
/// clears the stored constraints), validated against the wire types before it is sent.
fn constraints_of_edit(e: &Edit) -> Result<Value, String> {
    let list = |s: &str| -> Vec<String> {
        s.split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let mut c = json!({"allowedModels": list(&e.allowed), "excludedModels": list(&e.excluded)});
    if let Some(t) = COST_TIERS.get(e.cost_tier).filter(|t| !t.is_empty()) {
        c["costTier"] = json!(t);
    }
    if !e.provider.trim().is_empty() {
        let provider: OpenRouterProviderRouting = serde_json::from_str(e.provider.trim())
            .map_err(|err| format!("The provider routing is not valid: {err}"))?;
        c["provider"] = serde_json::to_value(provider).map_err(|err| err.to_string())?;
    }
    serde_json::from_value::<OpenRouterConstraints>(c.clone())
        .map_err(|err| format!("The OpenRouter constraints are not valid: {err}"))?;
    Ok(c)
}

fn sign_in(id: &str, action: &str, flow: Option<&Id>, label: &str) -> ConnectionAction {
    let mut a = json!({"kind": "SignIn", "connectionId": id, "action": action});
    if let Some(f) = flow {
        a["flowId"] = json!(f.as_str());
    }
    ConnectionAction::Configure {
        label: label.into(),
        action: a,
        key: None,
        code: None,
    }
}

/// The `Model` action saving a catalog row's edits (its tier memberships unchanged).
fn model_action(m: &RouteCatalogModel, e: &ModelEdit) -> Result<Value, String> {
    // Build the configure DTO explicitly: query facts never become writable input.
    let configured = RouteModel {
        id: m.id.clone(),
        display_name: m.display_name.clone(),
        endpoint_profile: m.endpoint_profile.clone(),
        upstream_model_id: m.upstream_model_id.clone(),
        enabled: m.enabled,
        tier_memberships: m.tier_memberships.clone(),
        efforts: m.efforts.clone(),
        image_input: m.image_input,
        tools: m.tools,
        revision: m.revision.clone(),
        context_tokens: m.context_tokens.clone(),
        connection_id: m.connection_id.clone(),
    };
    let mut model = serde_json::to_value(configured).map_err(|e| e.to_string())?;
    model["enabled"] = json!(e.enabled);
    match e.context.trim() {
        "" => {
            model.as_object_mut().map(|o| o.remove("contextTokens"));
        }
        c => {
            let n: u64 = c
                .parse()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("{c} is not a positive token count"))?;
            model["contextTokens"] = json!(n.to_string());
        }
    }
    if e.connection.is_empty() {
        model.as_object_mut().map(|o| o.remove("connectionId"));
    } else {
        model["connectionId"] = json!(e.connection);
    }
    Ok(json!({"kind": "Model", "model": model,
        "expectedRevision": m.revision.get().to_string()}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_configuration_never_submits_query_facts() {
        let model: RouteCatalogModel = serde_json::from_value(json!({
            "id":"version-1", "displayName":"Version 1", "endpointProfile":"CompatibleChat", "upstreamModelId":"version-1",
            "enabled":true,"tierMemberships":[],"efforts":[],"imageInput":false,"tools":true,"revision":"1",
            "available":false,"availabilityReason":"NotDiscovered"
        })).unwrap();
        let action = model_action(
            &model,
            &ModelEdit {
                enabled: true,
                ..Default::default()
            },
        )
        .unwrap();
        let configured: devworks_core::protocol::records::RouteModel =
            serde_json::from_value(action["model"].clone()).unwrap();
        assert_eq!(configured.id.as_str(), "version-1");
        for key in [
            "available",
            "availabilityReason",
            "latestFamily",
            "resolvesToModelIds",
        ] {
            assert!(action["model"].get(key).is_none());
        }
    }
}
