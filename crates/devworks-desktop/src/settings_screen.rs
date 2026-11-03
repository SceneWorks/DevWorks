//! The categorized Settings screen (SET-01/02, SET-07/08, the SET-03..06 surfaces the settings
//! service supports today) rendered from core's toolkit-independent [`SettingsView`]. Values come
//! from `settings.read_effective` / `.explain_source`; local checks reuse core validation through
//! the view; saving submits the view's exact `settings.save_revision` input. Configured values,
//! drafts, validation and runtime health are kept apart.
use crate::memory::SettingsMemory;
use crate::theme;
use crate::widgets::{self, Tone, ViewState, chip};
use devworks_core::protocol::records::SettingEntry;
use devworks_core::settings::catalog::{Adoption, Category, ValueKind};
use devworks_core::settings::view::{Health, Item, SettingsView, Source, Status};
use eframe::egui::{self, RichText, Ui};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// The keys whose runtime health is checked: configured directories and keep-awake.
pub const KEEP_AWAKE: &str = "power.keepAwake";

#[derive(Default)]
pub struct SettingsScreen {
    pub view: Option<SettingsView>,
    pub loading: bool,
    pub error: Option<String>,
    pub saving: bool,
    /// The save's outcome is not known: still blocking a second save, but nothing is running
    /// until "Confirm outcomes" resubmits it.
    pub unconfirmed: bool,
    pub validating: bool,
    /// The next read follows a successful save (drafts are cleared, not kept as a conflict).
    pub after_save: bool,
    /// The draft values the pending save submitted, by key. Drafts that differ from them when
    /// the save's follow-up read arrives were edited meanwhile and stay unsaved.
    pub submitted: BTreeMap<String, Value>,
    pub message: Option<(Tone, String)>,
    pub category: Option<Category>,
    pub query: String,
    /// Drafts restored from the previous launch, applied once the view loads.
    pub restored: BTreeMap<String, Value>,
    pub explained: BTreeSet<String>,
    pub scrolls: crate::virtual_list::Scrolls,
    buffers: HashMap<&'static str, String>,
}

/// What the screen asks the shell to do.
#[derive(Clone, Debug, PartialEq)]
pub enum SettingsAction {
    Reload,
    Validate,
    Save,
    Explain(String),
    ChooseDirectory(String),
    OpenAddOns,
}

impl SettingsScreen {
    pub fn restore(&mut self, m: &SettingsMemory) {
        self.category = m
            .category
            .as_deref()
            .and_then(|l| Category::ALL.into_iter().find(|c| c.label() == l));
        self.query = m.query.clone();
        self.restored = m.drafts.clone();
    }
    pub fn memory(&self) -> SettingsMemory {
        let drafts = match &self.view {
            Some(v) => v
                .dirty_keys()
                .into_iter()
                .filter_map(|k| Some((k.to_owned(), v.item(k)?.draft.clone()?)))
                .collect(),
            None => self.restored.clone(),
        };
        SettingsMemory {
            category: self.category.map(|c| c.label().to_owned()),
            query: self.query.clone(),
            drafts,
        }
    }

    /// Applies a `settings.read_effective` result: builds the view the first time (re-applying
    /// restored drafts), afterwards refreshes it keeping drafts (a moved revision with drafts
    /// becomes a visible conflict), or clears drafts after this screen's own save.
    pub fn loaded(&mut self, revision: i64, entries: &[SettingEntry]) {
        self.loading = false;
        self.error = None;
        match &mut self.view {
            None => {
                let mut view = SettingsView::new(
                    devworks_core::ports::storage::ConfigScope::Workspace,
                    revision,
                    entries,
                );
                for (k, v) in std::mem::take(&mut self.restored) {
                    let _ = view.edit(&k, v);
                }
                self.view = Some(view);
            }
            Some(view) if self.after_save => {
                // Edits made while the save or this read were pending are not part of what
                // was saved: re-apply them as drafts against the new revision.
                let submitted = std::mem::take(&mut self.submitted);
                let later: Vec<(&'static str, Value)> = view
                    .dirty_keys()
                    .into_iter()
                    .filter_map(|k| Some((k, view.item(k)?.draft.clone()?)))
                    .filter(|(k, v)| submitted.get(*k) != Some(v))
                    .collect();
                view.saved(revision, entries);
                for (k, v) in later {
                    let _ = if v.is_null() {
                        view.inherit(k)
                    } else {
                        view.edit(k, v).map(|_| ())
                    };
                }
                self.after_save = false;
                self.saving = false;
            }
            Some(view) => view.refresh(revision, entries),
        }
        self.buffers.clear();
        if let Some(view) = &mut self.view {
            view.set_query(&self.query);
            view.set_category(self.category);
        }
    }

    pub fn set_health(&mut self, key: &str, health: Health) {
        if let Some(v) = &mut self.view {
            v.set_health(key, health);
        }
    }

    /// Directory values to probe for health (configured values, not drafts).
    pub fn directories(&self) -> Vec<(&'static str, Value)> {
        let Some(view) = &self.view else {
            return Vec::new();
        };
        view.visible_all()
            .into_iter()
            .filter(|i| i.descriptor.kind == ValueKind::Directory)
            .map(|i| (i.descriptor.key, i.effective.clone()))
            .collect()
    }

    /// The whole screen: categories and search on the left, the selected category on the right.
    pub fn ui(&mut self, ui: &mut Ui, online: Result<(), &str>, actions: &mut Vec<SettingsAction>) {
        let full = ui.available_rect_before_wrap();
        let nav_width = 230.0_f32.min(full.width() * 0.35);
        let gap = 16.0;
        let left = egui::Rect::from_min_size(full.min, egui::vec2(nav_width, full.height()));
        let right = egui::Rect::from_min_max(
            egui::pos2(full.min.x + nav_width + gap, full.min.y),
            full.max,
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(left), |ui| {
            widgets::secondary_pane(ui, "Settings topics", |ui| self.nav(ui));
        });
        ui.scope_builder(egui::UiBuilder::new().max_rect(right), |ui| {
            let title = self.category.map_or("All settings", Category::label);
            let summary = "Workspace scope · configured values, effective values and runtime health are shown separately.";
            let mut st = (&mut *self, &mut *actions);
            widgets::pane(
                ui,
                widgets::Header {
                    title,
                    summary,
                    tabs: &[],
                    tab: 0,
                },
                &mut st,
                |ui, st| {
                    if widgets::secondary_button(ui, "Add-ons").clicked() {
                        st.1.push(SettingsAction::OpenAddOns);
                    }
                },
                |ui, st, _| st.0.body(ui, online, st.1),
            );
        });
        ui.allocate_rect(full, egui::Sense::hover());
    }

    fn nav(&mut self, ui: &mut Ui) {
        let r = ui.add(
            egui::TextEdit::singleline(&mut self.query)
                .hint_text("Search settings")
                .desired_width(f32::INFINITY),
        );
        r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Search settings"));
        if r.changed()
            && let Some(v) = &mut self.view
        {
            v.set_query(&self.query);
        }
        ui.add_space(6.0);
        let counts: Vec<(Category, usize)> = self
            .view
            .as_ref()
            .map(SettingsView::categories)
            .unwrap_or_default();
        let mut pick = None;
        if ui
            .selectable_label(self.category.is_none(), "All settings")
            .clicked()
        {
            pick = Some(None);
        }
        for c in Category::ALL {
            let n = counts.iter().find(|(k, _)| *k == c).map(|(_, n)| *n);
            // A search hides categories with no match; without one every category is listed.
            if !self.query.trim().is_empty() && n.is_none() && self.view.is_some() {
                continue;
            }
            let text = match n {
                Some(n) if !self.query.trim().is_empty() => format!("{} · {n}", c.label()),
                _ => c.label().to_owned(),
            };
            if ui
                .selectable_label(self.category == Some(c), text)
                .clicked()
            {
                pick = Some(Some(c));
            }
        }
        if let Some(c) = pick {
            self.category = c;
            if let Some(v) = &mut self.view {
                v.set_category(c);
            }
        }
    }

    fn body(&mut self, ui: &mut Ui, online: Result<(), &str>, actions: &mut Vec<SettingsAction>) {
        if let Err(reason) = online {
            widgets::state_block(
                ui,
                &ViewState::Offline {
                    reason: reason.to_owned(),
                },
            );
            return;
        }
        if let Some(e) = &self.error {
            let state = if self.view.is_some() {
                ViewState::Stale {
                    what: "settings".into(),
                }
            } else {
                ViewState::Failed {
                    what: "settings".into(),
                    error: e.clone(),
                }
            };
            if widgets::state_block(ui, &state) == widgets::StateAction::Retry {
                actions.push(SettingsAction::Reload);
            }
        }
        if self.view.is_none() {
            if self.error.is_none() {
                widgets::state_block(
                    ui,
                    &ViewState::Loading {
                        what: "settings".into(),
                    },
                );
            }
            return;
        }
        self.scope_row(ui);
        if let Some((tone, text)) = &self.message {
            chip(ui, text.clone(), *tone);
        }
        self.conflict(ui);
        self.save_bar(ui, actions);
        ui.add_space(6.0);
        let keys: Vec<&'static str> = self
            .view
            .as_ref()
            .map(|v| v.visible().iter().map(|i| i.descriptor.key).collect())
            .unwrap_or_default();
        let mut scrolls = std::mem::take(&mut self.scrolls);
        let out = scrolls.area("settings-items").show(ui, |ui| {
            if keys.is_empty() {
                widgets::state_block(
                    ui,
                    &ViewState::Empty {
                        title: "No settings match".into(),
                        body: "Try another search or category.".into(),
                    },
                );
            }
            if self.category == Some(Category::Instructions) || self.category.is_none() {
                role_governance(ui);
            }
            for key in keys {
                self.item_ui(ui, key, actions);
                ui.add_space(8.0);
            }
        });
        scrolls.record("settings-items", out.state.clamped_offset().y);
        self.scrolls = scrolls;
    }

    fn scope_row(&self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Scope:");
            chip(ui, "Workspace", Tone::Accent);
            ui.add_enabled(false, egui::Button::new("Project…"))
                .on_disabled_hover_text(
                    "Project overrides need projects, which arrive with DW-11. Project-scope \
                     inheritance is already enforced by the settings service.",
                );
            if let Some(v) = &self.view {
                widgets::caption(ui, format!("Revision {}", v.revision));
            }
        });
    }

    fn conflict(&self, ui: &mut Ui) {
        let Some(c) = self.view.as_ref().and_then(|v| v.conflict.clone()) else {
            return;
        };
        egui::Frame::new()
            .fill(theme::AMBER_BG)
            .stroke(egui::Stroke::new(1.0, theme::AMBER))
            .corner_radius(theme::radius())
            .inner_margin(theme::margin(12, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new(format!(
                        "Settings changed elsewhere (from revision {} to {}). Your unsaved changes are kept; review before saving again.",
                        c.previous_revision, c.current_revision
                    ))
                    .color(theme::AMBER),
                );
                if !c.changed.is_empty() {
                    ui.label(format!("Also changed: {}", c.changed.join(", ")));
                }
            });
    }

    fn save_bar(&mut self, ui: &mut Ui, actions: &mut Vec<SettingsAction>) {
        let Some(view) = &mut self.view else { return };
        if !view.is_dirty() {
            return;
        }
        let buffers = &mut self.buffers;
        let n = view.dirty_keys().len();
        let blocked = view.has_blocking_issues();
        egui::Frame::new()
            .fill(theme::RAISED)
            .stroke(theme::line())
            .corner_radius(theme::radius())
            .inner_margin(theme::margin(12, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "{n} unsaved change{}",
                        if n == 1 { "" } else { "s" }
                    ));
                    if widgets::secondary_button(ui, "Discard all").clicked() {
                        view.discard_all();
                        // Editor text (model lists) shows drafts too: drop it with them.
                        buffers.clear();
                    }
                    if ui
                        .add_enabled(!self.validating, egui::Button::new("Validate"))
                        .clicked()
                    {
                        actions.push(SettingsAction::Validate);
                    }
                    let save = ui
                        .add_enabled(!blocked && !self.saving, egui::Button::new("Save changes"))
                        .on_disabled_hover_text(if blocked {
                            "Fix the highlighted values first."
                        } else if self.unconfirmed {
                            "The previous save is not confirmed yet."
                        } else {
                            "Saving…"
                        });
                    if save.clicked() {
                        actions.push(SettingsAction::Save);
                    }
                    if self.saving && !self.unconfirmed {
                        ui.add(egui::Spinner::new());
                    }
                });
            });
    }

    fn item_ui(&mut self, ui: &mut Ui, key: &'static str, actions: &mut Vec<SettingsAction>) {
        let Some(view) = &mut self.view else { return };
        let Some(item) = view.item(key).cloned() else {
            return;
        };
        let d = item.descriptor;
        let explained = self.explained.contains(key);
        let mut edit: Option<Value> = None;
        let mut inherit = false;
        let mut discard = false;
        egui::Frame::new()
            .fill(theme::BG)
            .stroke(if item.is_dirty() {
                egui::Stroke::new(1.0, theme::ACCENT)
            } else {
                theme::soft_line()
            })
            .corner_radius(theme::radius())
            .inner_margin(theme::margin(14, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(d.label).strong());
                    let (text, tone) = status(&item);
                    chip(ui, text, tone);
                    if item.is_dirty() {
                        chip(ui, "Unsaved", Tone::Accent);
                    }
                });
                ui.label(RichText::new(d.description).color(theme::MUTED));
                let enabled = item.editable;
                ui.add_enabled_ui(enabled, |ui| {
                    edit = self_editor(ui, &mut self.buffers, &item, actions);
                });
                ui.horizontal_wrapped(|ui| {
                    widgets::caption(
                        ui,
                        format!(
                            "Effective: {} · Source: {}",
                            short(&item.effective),
                            source_label(&item.source)
                        ),
                    );
                    widgets::caption(ui, adoption_label(d.adoption));
                });
                health_line(ui, &item);
                for issue in &item.issues {
                    ui.label(RichText::new(&issue.message).color(if issue.blocking {
                        theme::RED
                    } else {
                        theme::AMBER
                    }));
                }
                if explained && item.chain.len() > 1 {
                    for layer in &item.chain {
                        widgets::caption(
                            ui,
                            format!("{}: {}", source_label(&layer.source), short(&layer.value)),
                        );
                    }
                }
                ui.horizontal_wrapped(|ui| {
                    if item.is_dirty()
                        && widgets::secondary_button(ui, &format!("Discard change to {}", d.label))
                            .clicked()
                    {
                        discard = true;
                    }
                    if item.editable
                        && item.status == Status::Overridden
                        && widgets::secondary_button(ui, &format!("Reset {} to default", d.label))
                            .clicked()
                    {
                        inherit = true;
                    }
                    if widgets::secondary_button(ui, &format!("Explain source of {}", d.label))
                        .clicked()
                    {
                        actions.push(SettingsAction::Explain(key.to_owned()));
                    }
                });
            });
        if discard {
            view.discard(key);
            self.buffers.remove(key);
        } else if inherit {
            let _ = view.inherit(key);
            self.buffers.remove(key);
        } else if let Some(v) = edit {
            let _ = view.edit(key, v);
        }
    }

    /// The current drafts by key (what a validation or save would submit).
    pub fn drafts(&self) -> BTreeMap<String, Value> {
        self.view
            .as_ref()
            .map(|v| {
                v.dirty_keys()
                    .into_iter()
                    .filter_map(|k| Some((k.to_owned(), v.item(k)?.draft.clone()?)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Applies a chosen directory to its setting as a draft.
    pub fn choose(&mut self, key: &str, path: String) {
        if let Some(v) = &mut self.view {
            let _ = v.edit(key, json!(path));
        }
    }
}

fn self_editor(
    ui: &mut Ui,
    buffers: &mut HashMap<&'static str, String>,
    item: &Item,
    actions: &mut Vec<SettingsAction>,
) -> Option<Value> {
    let d = item.descriptor;
    let shown = item.shown().clone();
    let named = |r: egui::Response, typ: egui::Role| {
        let label = d.label.to_owned();
        let enabled = r.enabled();
        r.widget_info(|| egui::WidgetInfo::labeled(typ, enabled, label.clone()));
        r
    };
    match d.kind {
        ValueKind::Bool => {
            let mut v = shown.as_bool().unwrap_or(false);
            let r = ui.checkbox(&mut v, "Enabled");
            named(r, egui::Role::CheckBox).changed().then(|| json!(v))
        }
        ValueKind::Integer { min, max } => {
            let mut v = shown.as_i64().unwrap_or(min);
            let r = ui.add(egui::DragValue::new(&mut v).range(min..=max));
            named(r, egui::Role::SpinButton).changed().then(|| json!(v))
        }
        ValueKind::Choice(options) => {
            let current = shown.as_str().unwrap_or_default().to_owned();
            let mut picked = None;
            egui::ComboBox::from_id_salt(d.key)
                .selected_text(&current)
                .show_ui(ui, |ui| {
                    for o in options {
                        if ui.selectable_label(current == *o, *o).clicked() {
                            picked = Some(json!(o));
                        }
                    }
                })
                .response
                .widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, true, d.label));
            picked
        }
        ValueKind::Directory => {
            let mut text = shown.as_str().unwrap_or_default().to_owned();
            let mut out = None;
            ui.horizontal(|ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut text)
                        .desired_width((ui.available_width() - 110.0).max(120.0)),
                );
                if named(r, egui::Role::TextInput).changed() {
                    out = Some(json!(text));
                }
                let label = format!("Choose directory for {}", d.label);
                let r = ui.button("Choose…").on_hover_text(&label);
                r.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::Button, true, label.clone())
                });
                if r.clicked() {
                    actions.push(SettingsAction::ChooseDirectory(d.key.to_owned()));
                }
            });
            out
        }
        ValueKind::Instructions => {
            let mut text = shown.as_str().unwrap_or_default().to_owned();
            let r = ui.add(
                egui::TextEdit::multiline(&mut text)
                    .desired_rows(4)
                    .desired_width(f32::INFINITY),
            );
            named(r, egui::Role::TextInput)
                .changed()
                .then(|| json!(text))
        }
        ValueKind::ModelList => {
            let buffer = buffers.entry(d.key).or_insert_with(|| {
                shown
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default()
            });
            let r = ui.add(
                egui::TextEdit::singleline(buffer)
                    .hint_text("Model IDs, comma separated")
                    .desired_width(f32::INFINITY),
            );
            named(r, egui::Role::TextInput).changed().then(|| {
                json!(
                    buffer
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                )
            })
        }
        ValueKind::SecretRef => {
            let configured = shown.get("secretRef").is_some();
            ui.label(if configured {
                "A stored credential is referenced."
            } else {
                "No credential configured."
            });
            widgets::caption(
                ui,
                "Credentials are entered through the native secure credential flow (DW-18); \
                 Settings never shows or accepts a raw secret.",
            );
            None
        }
    }
}

fn status(item: &Item) -> (&'static str, Tone) {
    match item.status {
        Status::Default => ("Default", Tone::Neutral),
        Status::Inherited => ("Inherited", Tone::Neutral),
        Status::Overridden => ("Configured", Tone::Accent),
        Status::Enforced => ("Enforced by DevWorks", Tone::Good),
        Status::WorkspaceOnly => ("Workspace only", Tone::Neutral),
    }
}

fn source_label(s: &Source) -> String {
    match s {
        Source::Workspace { revision } => format!("Workspace (revision {revision})"),
        Source::Project { revision } => format!("Project (revision {revision})"),
        other => other.label().to_owned(),
    }
}

fn adoption_label(a: Adoption) -> &'static str {
    match a {
        Adoption::Immediate => "Applies immediately",
        Adoption::NextSafeStep => "Running agents adopt it at their next safe step",
        Adoption::NextAdmission => "Applies at the next admission decision",
        Adoption::NewClonesOnly => {
            "Used for new clones only; existing repositories keep their paths"
        }
    }
}

fn health_line(ui: &mut Ui, item: &Item) {
    let relevant =
        item.descriptor.kind == ValueKind::Directory || item.descriptor.key == KEEP_AWAKE;
    if !relevant {
        return;
    }
    let (text, tone) = match &item.health {
        Health::NotChecked => ("Runtime health: not checked yet".to_owned(), Tone::Neutral),
        Health::Available => ("Runtime health: available".to_owned(), Tone::Good),
        Health::Unavailable(why) => (
            format!("Runtime health: unavailable — {why}"),
            Tone::Attention,
        ),
    };
    chip(ui, text, tone);
}

fn short(v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let first = s.lines().next().unwrap_or_default();
    if first.chars().count() > 80 || s.lines().count() > 1 {
        format!("{}…", first.chars().take(80).collect::<String>())
    } else {
        first.to_owned()
    }
}

/// SET-03..06: what role-instruction governance the settings service supports today.
fn role_governance(ui: &mut Ui) {
    egui::Frame::new()
        .stroke(theme::soft_line())
        .corner_radius(theme::radius())
        .inner_margin(theme::margin(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Role instructions and proposals").strong());
            widgets::caption(
                ui,
                "Every role's instructions are edited and saved here; harness policies and \
                 selected tools still apply, and running agents adopt saved changes at their \
                 next safe step.",
            );
            widgets::state_block(
                ui,
                &ViewState::Unavailable {
                    what: "agent-drafted proposals, approval, version history and rollback".into(),
                    owner: "DW-14 (approvals) and DW-34 (roles)".into(),
                },
            );
        });
    ui.add_space(8.0);
}

/// `SettingsView` exposes visible items for the current filter; health checks need every item.
trait VisibleAll {
    fn visible_all(&self) -> Vec<&Item>;
}
impl VisibleAll for SettingsView {
    fn visible_all(&self) -> Vec<&Item> {
        devworks_core::settings::catalog::catalog()
            .iter()
            .filter_map(|d| self.item(d.key))
            .collect()
    }
}
