//! The Activity feed: one line per project change, newest first.
use crate::shell::work::*;
use crate::widgets::{self, ViewState};
use devworks_core::protocol::helpers::work::fold;
use devworks_core::protocol::records::{WorkKind, WorkRecord};
use devworks_core::protocol::{BlockerChange, ControlReason, Event, EventBody, EventSubject};
use eframe::egui::{self, Ui};
use std::collections::BTreeMap;
/// What an Activity event is and what it says, from its body.
pub fn event_summary(e: &Event) -> (&'static str, String) {
    match &e.event {
        EventBody::CommandDecided { command_id } => (
            "Change",
            format!(
                "command {} decided; revision {}",
                command_id.0.as_str(),
                e.aggregate_revision.as_str()
            ),
        ),
        EventBody::RootControlChanged { reason, .. } => (
            "Run control",
            match reason {
                ControlReason::Paused => "paused".into(),
                ControlReason::Continued => "continued".into(),
            },
        ),
        EventBody::ActivationRequested {
            execution_id,
            generation,
            ..
        } => (
            "Run start",
            format!(
                "activation {} requested (generation {})",
                execution_id.0.as_str(),
                generation.as_str()
            ),
        ),
        EventBody::BlockerChanged { blocker, change } => (
            "Blocker",
            format!(
                "{} {:?}",
                match change {
                    BlockerChange::Raised => "raised",
                    BlockerChange::Cleared => "cleared",
                },
                blocker.cause
            ),
        ),
        EventBody::OperationObserved {
            operation_id,
            state,
            ..
        } => (
            "Operation",
            format!("operation {} {state:?}", operation_id.0.as_str()),
        ),
    }
}

/// One Activity row: its sequence, the event's kind and summary, who, what it concerns and when,
/// in local time.
pub fn activity_line(e: &Event, who: &str, what: &str) -> String {
    let (kind, summary) = event_summary(e);
    format!(
        "#{} · {kind}: {summary} · {who} · {what} · {}",
        e.sequence.as_str(),
        crate::shell::jobs::local_time(Some(&e.occurred_at_ms)),
    )
}

pub(crate) fn activity(
    ui: &mut Ui,
    work: &mut WorkUi,
    cols: &[Column],
    user: &str,
    actions: &mut Vec<Action>,
) {
    let a = &mut work.activity;
    ui.horizontal_wrapped(|ui| {
        egui::ComboBox::from_id_salt("activity-kind")
            .selected_text(["Everything", "Features", "Issues", "Tasks", "Project"][a.kind.min(4)])
            .show_ui(ui, |ui| {
                for (i, l) in ["Everything", "Features", "Issues", "Tasks", "Project"]
                    .iter()
                    .enumerate()
                {
                    ui.selectable_value(&mut a.kind, i, *l);
                }
            })
            .response
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Activity kind"));
        egui::ComboBox::from_id_salt("activity-actor")
            .selected_text(["Everyone", "You", "Agents and others"][a.actor.min(2)])
            .show_ui(ui, |ui| {
                for (i, l) in ["Everyone", "You", "Agents and others"].iter().enumerate() {
                    ui.selectable_value(&mut a.actor, i, *l);
                }
            })
            .response
            .widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Activity actor")
            });
        let r = ui.add(
            egui::TextEdit::singleline(&mut a.text)
                .hint_text("Filter activity")
                .desired_width(200.0),
        );
        r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Filter activity"));
        if a.item.is_some() && widgets::secondary_button(ui, "All work").clicked() {
            a.item = None;
        }
    });
    let root = a.item.clone();
    if let Some(root) = &root {
        work.histories.entry(root.clone()).or_default();
    }
    let feed = match &root {
        Some(root) => &work.histories[root],
        None => &work.activity,
    };
    if let Some(error) = &feed.error {
        widgets::state_block(
            ui,
            &ViewState::Failed {
                what: "history".into(),
                error: error.clone(),
            },
        );
        if widgets::secondary_button(ui, "Reload history").clicked() {
            actions.push(Action::RefreshHistory(root.clone()));
        }
    }
    if !feed.loaded {
        if feed.error.is_none() {
            widgets::state_block(
                ui,
                &ViewState::Loading {
                    what: "history".into(),
                },
            );
        }
        return;
    }
    let items = &feed.items;
    let more = feed.more();
    let loading = feed.loading();
    let a = &work.activity;
    let titles: BTreeMap<String, &WorkRecord> = work
        .features
        .items
        .iter()
        .chain(&work.cards.items)
        .map(|w| (w.id.as_str().to_owned(), w))
        .collect();
    let rows: Vec<String> = items
        .iter()
        .filter_map(|e| {
            let subject = match (&e.aggregate, &e.root_id) {
                (EventSubject::Root(r), _) => Some(r.0.as_str().to_owned()),
                (_, Some(r)) => Some(r.0.as_str().to_owned()),
                _ => None,
            };
            let w = subject.as_ref().and_then(|s| titles.get(s)).copied();
            let kind_ok = match a.kind {
                1 => w.is_some_and(|w| matches!(w.kind, WorkKind::Feature)),
                2 => {
                    w.is_some_and(|w| w.parent_id.is_none() && !matches!(w.kind, WorkKind::Feature))
                }
                3 => w.is_some_and(|w| w.parent_id.is_some()),
                4 => subject.is_none(),
                _ => true,
            };
            let mine = e.actor_id.0.as_str() == user;
            let actor_ok = match a.actor {
                1 => mine,
                2 => !mine,
                _ => true,
            };
            let item_ok = a
                .item
                .as_ref()
                .is_none_or(|i| subject.as_deref() == Some(i.as_str()));
            let what = match w {
                Some(w) => format!(
                    "{} {} ({})",
                    kind_label(w),
                    w.title.0,
                    status_name(cols, w.status_id.as_str())
                ),
                None => subject.map_or("the project".into(), |s| format!("work {s}")),
            };
            let line = activity_line(e, if mine { "You" } else { e.actor_id.0.as_str() }, &what);
            (kind_ok && actor_ok && item_ok && matches_text_line(&line, &a.text)).then_some(line)
        })
        .collect();
    widgets::caption(
        ui,
        format!(
            "{} of {} loaded changes shown · newest first · older history on demand",
            rows.len(),
            items.len(),
        ),
    );
    if more
        && ui
            .add_enabled(!loading, egui::Button::new("Load older changes"))
            .clicked()
    {
        actions.push(Action::OlderHistory(root));
    }
    if rows.is_empty() {
        widgets::state_block(
            ui,
            &ViewState::Empty {
                title: "No activity".into(),
                body: "Changes to this project's work appear here.".into(),
            },
        );
    }
    egui::ScrollArea::vertical()
        .id_salt("activity-rows")
        .max_height(ui.available_height() - 40.0)
        .show(ui, |ui| {
            for r in &rows {
                widgets::text_with_github_links(ui, r);
            }
        });
}
pub(crate) fn matches_text_line(line: &str, needle: &str) -> bool {
    let n = fold(needle.trim());
    n.is_empty() || fold(line).contains(&n)
}
