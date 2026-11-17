//! A repository's delivery settings inside one project (DELIVERY-04): each value shown with
//! the layer it comes from (Repository, Project, Workspace or the built-in default), set for
//! this repository only, or cleared so the repository inherits again. The values are the same
//! `settings.read_effective` / `settings.save_revision` entries an agent tool reads; this view
//! decides nothing the settings service does not. `delivery.mandatoryReview` is shown as
//! enforced: no scope, a repository included, can change it.
use crate::projects_screen::Action;
use crate::widgets::{self, Tone, ViewState, chip};
use devworks_core::settings::catalog::{Category, ValueKind};
use devworks_core::settings::view::{Item, SettingsView, Source, Status};
use eframe::egui::{self, Ui};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// One repository's delivery settings, viewed at its repository scope.
#[derive(Default)]
pub struct RepositoryDelivery {
    pub view: Option<SettingsView>,
    pub loading: bool,
    pub saving: bool,
    /// Reload after the save settles (drafts are cleared, not kept as a conflict).
    pub after_save: bool,
    pub error: Option<String>,
}

/// Delivery state by (project ID, repository ID).
pub type DeliveryByRepository = BTreeMap<(String, String), RepositoryDelivery>;

/// The layer an effective value comes from, with its revision.
pub fn source_text(source: &Source) -> String {
    match source {
        Source::Workspace { revision } => format!("Workspace (revision {revision})"),
        Source::Project { revision } => format!("Project (revision {revision})"),
        Source::Repository { revision } => format!("Repository (revision {revision})"),
        other => other.label().to_owned(),
    }
}

fn shown_text(value: &Value) -> String {
    match value {
        Value::Bool(true) => "On".into(),
        Value::Bool(false) => "Off".into(),
        other => other.to_string(),
    }
}

/// The effective value of one setting and where it comes from, as one line.
pub fn effective_line(item: &Item) -> String {
    format!(
        "Effective: {} · Source: {}",
        shown_text(&item.effective),
        source_text(&item.source)
    )
}

pub(crate) fn section(
    ui: &mut Ui,
    delivery: &mut DeliveryByRepository,
    project: &str,
    repository: &str,
    enabled: bool,
    actions: &mut Vec<Action>,
) {
    ui.add_space(8.0);
    widgets::caption(ui, "Delivery settings for this repository");
    let key = (project.to_owned(), repository.to_owned());
    let Some(state) = delivery.get_mut(&key) else {
        actions.push(Action::LoadDelivery(key.0, key.1));
        widgets::state_block(
            ui,
            &ViewState::Loading {
                what: "the repository's delivery settings".into(),
            },
        );
        return;
    };
    if let Some(e) = &state.error {
        chip(ui, e.clone(), Tone::Bad);
    }
    let Some(view) = state.view.as_mut() else {
        if !state.loading
            && state.error.is_some()
            && widgets::secondary_button(ui, "Retry").clicked()
        {
            actions.push(Action::LoadDelivery(key.0, key.1));
        }
        return;
    };
    let busy = state.saving || !enabled;
    let keys: Vec<&'static str> = view
        .visible()
        .into_iter()
        .filter(|i| i.descriptor.category == Category::Delivery)
        .map(|i| i.descriptor.key)
        .collect();
    for k in keys {
        let Some(item) = view.item(k) else { continue };
        let (label, line, editable, overridden, kind, shown) = (
            item.descriptor.label,
            effective_line(item),
            item.editable,
            item.status == Status::Overridden,
            item.descriptor.kind,
            item.shown().clone(),
        );
        ui.add_space(4.0);
        ui.label(label);
        widgets::caption(ui, line);
        if !editable {
            widgets::caption(
                ui,
                if item.status == Status::Enforced {
                    "Enforced by DevWorks. It applies to every repository and cannot be changed here."
                } else {
                    "Set at a wider scope; a repository inherits it."
                },
            );
            continue;
        }
        let mut next = shown.clone();
        ui.add_enabled_ui(!busy, |ui| match kind {
            ValueKind::Bool => {
                let mut on = shown.as_bool().unwrap_or(false);
                let r = ui.checkbox(&mut on, label);
                if r.changed() {
                    next = json!(on);
                }
            }
            ValueKind::Integer { min, max } => {
                let mut n = shown.as_i64().unwrap_or(min);
                let r = ui.add(egui::DragValue::new(&mut n).range(min..=max));
                r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::SpinButton, true, label));
                if r.changed() {
                    next = json!(n);
                }
            }
            _ => {}
        });
        if next != shown {
            let _ = view.edit(k, next);
        }
        let issues: Vec<String> = view
            .item(k)
            .map(|i| i.issues.iter().map(|x| x.message.clone()).collect())
            .unwrap_or_default();
        for i in issues {
            chip(ui, i, Tone::Bad);
        }
        let drafted = view.item(k).is_some_and(|i| i.draft.is_some());
        if ui
            .add_enabled(
                !busy && (overridden || drafted),
                egui::Button::new(format!("Use the project value for {label}")),
            )
            .clicked()
        {
            if overridden {
                let _ = view.inherit(k);
            } else {
                view.discard(k);
            }
        }
    }
    let dirty = !view.dirty_keys().is_empty();
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(
                dirty && !busy,
                egui::Button::new("Save repository delivery settings"),
            )
            .clicked()
        {
            actions.push(Action::SaveDelivery(key.0.clone(), key.1.clone()));
        }
        if state.saving {
            widgets::caption(ui, "Saving…");
        }
    });
}
