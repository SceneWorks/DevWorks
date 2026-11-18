//! The work views (DW-12, UI-14/15): the Features and Issues lists, the create form, the
//! Kanban board and its accessible ordering controls. Feature and card detail and the plan
//! editor are in [`detail`]. They draw [`WorkUi`] state and report what the user did as
//! actions the shell submits (`shell/work.rs`).
pub mod activity;
pub mod detail;
pub mod features;
pub mod issues;
pub mod messages;
pub mod overview;
use crate::frame;
use crate::shell::ListState;
use crate::theme;
use crate::widgets::{self, Tone, ViewState, chip};
use devworks_core::protocol::Id;
use devworks_core::protocol::records::{
    ApprovalState, FindingState, QueuePlacement, QueueSide, ReadinessFreshness, WorkKind,
    WorkRecord,
};
use devworks_core::services::work::fold;
use eframe::egui::{self, RichText, Ui};
use std::collections::BTreeSet;

/// Feature list tabs and Issues tabs.
use crate::shell::work::*;

/// The next free `<prefix><n>` ID, excluding visible entries and reserved base IDs.
pub fn next_id(entries: &[Entry], prefix: &str, reserved: &[String]) -> String {
    (1..)
        .map(|n| format!("{prefix}{n}"))
        .find(|c| !reserved.contains(c) && !entries.iter().any(|e| &e.id == c))
        .expect("a free identifier")
}

/// Work type is authored independently of feature membership; an unset legacy value stays unset.
pub(crate) fn metadata_fields(
    ui: &mut Ui,
    typed: bool,
    kind: &mut Option<String>,
    labels: &mut Vec<String>,
) {
    if typed {
        egui::ComboBox::from_label("Work type")
            .selected_text(kind.as_deref().unwrap_or("Not specified"))
            .show_ui(ui, |ui| {
                ui.selectable_value(kind, None, "Not specified");
                for name in ["Story", "Task", "Bug"] {
                    ui.selectable_value(kind, Some(name.into()), name);
                }
            });
    }
    ui.label("Labels");
    let mut remove = None;
    for (index, label) in labels.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::multiline(label).desired_rows(1))
                .widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::Role::TextInput,
                        true,
                        format!("Label {}", index + 1),
                    )
                });
            if ui
                .small_button(format!("Remove label {}", index + 1))
                .clicked()
            {
                remove = Some(index);
            }
        });
    }
    if let Some(index) = remove {
        labels.remove(index);
    }
    if ui.button("Add label").clicked() {
        labels.push(String::new());
    }
}

pub(crate) fn metadata_summary(ui: &mut Ui, work: &WorkRecord) {
    if !matches!(work.kind, WorkKind::Feature) {
        widgets::caption(
            ui,
            format!(
                "Work type: {}",
                task_type_of(work).as_deref().unwrap_or("Not specified")
            ),
        );
    }
    let labels = labels_of(work);
    if !labels.is_empty() {
        ui.horizontal_wrapped(|ui| {
            for label in &labels {
                chip(ui, label.clone(), Tone::Neutral);
            }
        });
    }
}

pub(crate) fn short(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{t}…")
    } else {
        t
    }
}

pub(crate) fn matches_text(w: &WorkRecord, needle: &str) -> bool {
    let n = fold(needle.trim());
    n.is_empty() || fold(&format!("{} {}", w.title.0, w.description.0)).contains(&n)
}

pub(crate) fn approval_label(w: &WorkRecord) -> String {
    let pending = w
        .pending_plan
        .as_ref()
        .and_then(|p| p.approval.as_ref())
        .map(|a| match a.state {
            ApprovalState::Pending => "revision awaiting approval",
            ApprovalState::Rejected => "revision rejected",
            _ => "revision",
        });
    match (&w.approval, &w.pending_plan, pending) {
        (Some(_), None, _) => "Plan approved".into(),
        (Some(_), Some(_), Some(p)) => format!("Plan approved · {p}"),
        (Some(_), Some(_), None) => "Plan approved · revision drafted".into(),
        (None, Some(_), Some(p)) => format!("Plan not approved · {p}"),
        (None, Some(_), None) => "Plan drafted, not approved".into(),
        (None, None, _) => "No plan".into(),
    }
}

pub(crate) fn readiness_label(w: &WorkRecord) -> String {
    match &w.readiness {
        None => "No readiness assessment".into(),
        Some(r) => {
            let open = r
                .findings
                .0
                .iter()
                .filter(|f| f.blocking && matches!(f.state, FindingState::Open))
                .count();
            match (&r.freshness, r.ready) {
                (_, true) => "Ready".into(),
                (ReadinessFreshness::Stale, _) => format!(
                    "Readiness stale for {}",
                    r.stale_scope_keys
                        .0
                        .iter()
                        .map(|k| if k == &w.id {
                            "the feature"
                        } else {
                            k.as_str()
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                _ if open > 0 => format!("Not ready: {open} open blocking finding(s)"),
                _ => "Not ready: the plan is not approved".into(),
            }
        }
    }
}

pub(crate) fn create_form(ui: &mut Ui, work: &mut WorkUi, busy: bool, actions: &mut Vec<Action>) {
    let features: Vec<(String, String)> = work
        .features
        .items
        .iter()
        .map(|f| (f.id.as_str().to_owned(), f.title.0.clone()))
        .collect();
    let Some(form) = work.create.as_mut() else {
        return;
    };
    let mut submit = None;
    let mut cancel = false;
    egui::Frame::new()
        .stroke(theme::soft_line())
        .corner_radius(theme::radius())
        .inner_margin(theme::margin(12, 10))
        .show(ui, |ui| {
            // A creation consumes the form: it is locked while it runs, so nothing typed
            // meanwhile is dropped by its reply.
            if busy {
                ui.disable();
            }
            ui.label(RichText::new(format!("New {}", form.kind)).strong());
            let r = ui.add(egui::TextEdit::singleline(&mut form.title).hint_text("Title"));
            r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Title"));
            let r = ui.add(
                egui::TextEdit::multiline(&mut form.description)
                    .hint_text("Description, reproduction, expected and actual behavior")
                    .desired_rows(3),
            );
            r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Description"));
            metadata_fields(
                ui,
                form.kind != "feature",
                &mut form.task_type,
                &mut form.labels,
            );
            ui.horizontal(|ui| {
                ui.label("Priority");
                ui.add(egui::DragValue::new(&mut form.priority).range(-1000..=1000))
                    .widget_info(|| {
                        egui::WidgetInfo::labeled(egui::Role::SpinButton, true, "Priority")
                    });
            });
            if form.kind != "feature" {
                let label = form
                    .feature
                    .as_ref()
                    .and_then(|f| features.iter().find(|(id, _)| id == f))
                    .map_or("No feature (standalone issue)".to_owned(), |(_, t)| {
                        t.clone()
                    });
                egui::ComboBox::from_id_salt("create-feature")
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut form.feature,
                            None,
                            "No feature (standalone issue)",
                        );
                        for (id, t) in &features {
                            ui.selectable_value(&mut form.feature, Some(id.clone()), t);
                        }
                    })
                    .response
                    .widget_info(|| {
                        egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Feature")
                    });
            }
            if form.kind == "feature" {
                form.budget.ui(ui);
            }
            ui.horizontal(|ui| {
                let ok = !busy
                    && !form.title.trim().is_empty()
                    && (form.kind != "feature" || form.budget.input().is_ok());
                if ui.add_enabled(ok, egui::Button::new("Create")).clicked() {
                    submit = Some(form.clone());
                }
                if widgets::secondary_button(ui, "Cancel").clicked() {
                    cancel = true;
                }
            });
        });
    if let Some(f) = submit {
        actions.push(Action::Create(f));
    }
    if cancel {
        work.create = None;
    }
    ui.add_space(8.0);
}

/// The placement moving `bucket[pos]` one step up (before its predecessor) or down (after its
/// successor): one bounded request whatever the bucket's size (`None` at the bucket's edge).
pub fn adjacent_move(bucket: &[&str], pos: usize, up: bool) -> Option<QueuePlacement> {
    let anchor = if up {
        bucket.get(pos.checked_sub(1)?)?
    } else {
        bucket.get(pos + 1)?
    };
    Some(QueuePlacement {
        work_id: frame::id(bucket.get(pos)?)?,
        anchor_id: frame::id(anchor)?,
        side: if up {
            QueueSide::Before
        } else {
            QueueSide::After
        },
    })
}

/// Up/Down controls of one item among `bucket` (the items of its priority in queue order).
pub(crate) fn order_controls(
    ui: &mut Ui,
    w: &WorkRecord,
    bucket: &[&WorkRecord],
    enabled: bool,
    action: impl Fn(QueuePlacement) -> Action,
    actions: &mut Vec<Action>,
) {
    let pos = bucket.iter().position(|b| b.id == w.id).unwrap_or(0);
    let ids: Vec<&str> = bucket.iter().map(|b| b.id.as_str()).collect();
    let up = ui
        .add_enabled(enabled && pos > 0, egui::Button::new("Up").small())
        .on_hover_text(format!("Move {} up", w.title.0));
    up.widget_info(|| {
        egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Move {} up", w.title.0))
    });
    if up.clicked()
        && let Some(m) = adjacent_move(&ids, pos, true)
    {
        actions.push(action(m));
    }
    let down = ui
        .add_enabled(
            enabled && pos + 1 < bucket.len(),
            egui::Button::new("Down").small(),
        )
        .on_hover_text(format!("Move {} down", w.title.0));
    down.widget_info(|| {
        egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Move {} down", w.title.0))
    });
    if down.clicked()
        && let Some(m) = adjacent_move(&ids, pos, false)
    {
        actions.push(action(m));
    }
}

pub(crate) fn feature_list(
    ui: &mut Ui,
    work: &mut WorkUi,
    mem: &mut crate::memory::RouteMemory,
    cols: &[Column],
    open: bool,
    actions: &mut Vec<Action>,
) {
    ui.horizontal_wrapped(|ui| {
        let r = ui.add(
            egui::TextEdit::singleline(&mut mem.filter)
                .hint_text("Search features")
                .desired_width(240.0),
        );
        r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Search features"));
        let label = work
            .status_filter
            .as_ref()
            .map_or("Every status".to_owned(), |s| status_name(cols, s));
        egui::ComboBox::from_id_salt("feature-status-filter")
            .selected_text(label)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut work.status_filter, None, "Every status");
                for c in cols {
                    ui.selectable_value(&mut work.status_filter, Some(c.id.clone()), &c.name);
                }
            })
            .response
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Status filter"));
    });
    if !list_state(ui, &work.features, "features") {
        return;
    }
    let all = &work.features.items;
    let shown: Vec<&WorkRecord> = all
        .iter()
        .filter(|f| matches_text(f, &mem.filter))
        .filter(|f| {
            work.status_filter
                .as_ref()
                .is_none_or(|s| f.status_id.as_str() == s)
        })
        .collect();
    widgets::caption(
        ui,
        format!("{} of {} features shown", shown.len(), all.len()),
    );
    if shown.is_empty() {
        widgets::state_block(
            ui,
            &ViewState::Empty {
                title: if all.is_empty() {
                    "No features yet".into()
                } else {
                    "Nothing matches the filter".into()
                },
                body: if all.is_empty() {
                    "Create a feature to plan and deliver work.".into()
                } else {
                    "Clear or change the filter.".into()
                },
            },
        );
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("features-list")
        .max_height(ui.available_height())
        .show(ui, |ui| {
            for f in &shown {
                let bucket: Vec<&WorkRecord> =
                    all.iter().filter(|x| x.priority == f.priority).collect();
                let expected = f
                    .queue_revision
                    .as_ref()
                    .map_or("1".to_owned(), |r| r.as_str().to_owned());
                ui.horizontal(|ui| {
                    let selected = mem.selection.as_deref() == Some(f.id.as_str());
                    let text = format!(
                        "{}  ·  P{}  ·  {}  ·  {}  ·  {}",
                        f.title.0,
                        f.priority,
                        status_name(cols, f.status_id.as_str()),
                        approval_label(f),
                        readiness_label(f)
                    );
                    if ui.selectable_label(selected, text).clicked() {
                        mem.selection = Some(f.id.as_str().to_owned());
                        actions.push(Action::OpenDetail(f.id.as_str().to_owned()));
                    }
                    order_controls(
                        ui,
                        f,
                        &bucket,
                        open,
                        |ids| {
                            Action::Order("features.prioritize", None, None, ids, expected.clone())
                        },
                        actions,
                    );
                });
            }
        });
}

pub(crate) fn issue_list(
    ui: &mut Ui,
    work: &mut WorkUi,
    mem: &mut crate::memory::RouteMemory,
    cols: &[Column],
    history: bool,
    open: bool,
    actions: &mut Vec<Action>,
) {
    let r = ui.add(
        egui::TextEdit::singleline(&mut mem.filter)
            .hint_text("Search issues")
            .desired_width(240.0),
    );
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Search issues"));
    if !list_state(ui, &work.cards, "issues") {
        return;
    }
    let standalone: Vec<&WorkRecord> = work
        .cards
        .items
        .iter()
        .filter(|c| c.parent_id.is_none())
        .collect();
    let shown: Vec<&WorkRecord> = standalone
        .iter()
        .copied()
        .filter(|c| finished(&semantic_of(cols, c.status_id.as_str())) == history)
        .filter(|c| matches_text(c, &mem.filter))
        .collect();
    widgets::caption(
        ui,
        format!("{} of {} issues shown", shown.len(), standalone.len()),
    );
    if shown.is_empty() {
        widgets::state_block(
            ui,
            &ViewState::Empty {
                title: if history {
                    "No finished issues"
                } else {
                    "No open issues"
                }
                .into(),
                body: if history {
                    String::new()
                } else {
                    "Create an issue for work that needs no feature.".into()
                },
            },
        );
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("issues-list")
        .max_height(ui.available_height())
        .show(ui, |ui| {
            for c in &shown {
                let bucket: Vec<&WorkRecord> = standalone
                    .iter()
                    .copied()
                    .filter(|x| x.priority == c.priority)
                    .collect();
                let expected = c
                    .queue_revision
                    .as_ref()
                    .map_or("1".to_owned(), |r| r.as_str().to_owned());
                ui.horizontal(|ui| {
                    let selected = mem.selection.as_deref() == Some(c.id.as_str());
                    let blocked = if c.dependency_ids.0.is_empty() {
                        ""
                    } else {
                        " · blocked by work"
                    };
                    let text = format!(
                        "{}  ·  P{}  ·  {}{blocked}",
                        c.title.0,
                        c.priority,
                        status_name(cols, c.status_id.as_str())
                    );
                    if ui.selectable_label(selected, text).clicked() {
                        mem.selection = Some(c.id.as_str().to_owned());
                        actions.push(Action::OpenDetail(c.id.as_str().to_owned()));
                    }
                    order_controls(
                        ui,
                        c,
                        &bucket,
                        open && !history,
                        |ids| Action::Order("issues.prioritize", None, None, ids, expected.clone()),
                        actions,
                    );
                });
            }
        });
}

/// What a dragged card carries.
#[derive(Clone)]
pub(crate) struct DraggedCard(WorkRecord);

pub(crate) fn board(
    ui: &mut Ui,
    work: &mut WorkUi,
    cols: &[Column],
    open: bool,
    waiting: &BTreeSet<String>,
    actions: &mut Vec<Action>,
) {
    if !list_state(ui, &work.features, "features") || !list_state(ui, &work.cards, "cards") {
        return;
    }
    let label = work
        .board
        .as_ref()
        .and_then(|b| work.features.items.iter().find(|f| f.id.as_str() == b))
        .map_or("Choose a feature".to_owned(), |f| f.title.0.clone());
    let mut chosen = work.board.clone();
    egui::ComboBox::from_id_salt("board-feature")
        .selected_text(label)
        .show_ui(ui, |ui| {
            for f in &work.features.items {
                ui.selectable_value(&mut chosen, Some(f.id.as_str().to_owned()), &f.title.0);
            }
        })
        .response
        .widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Board feature"));
    if chosen != work.board {
        work.board = chosen.clone();
        work.card = None;
        if let Some(f) = chosen {
            actions.push(Action::OpenDetail(f));
        }
    }
    let Some(feature) = work.board.clone() else {
        widgets::state_block(
            ui,
            &ViewState::Empty {
                title: "No feature chosen".into(),
                body: "Choose a feature to see its board.".into(),
            },
        );
        return;
    };
    workflow::next_task(ui, work, &feature, open, actions);
    let cards: Vec<WorkRecord> = work
        .cards
        .items
        .iter()
        .filter(|c| c.parent_id.as_ref().map(Id::as_str) == Some(feature.as_str()))
        .cloned()
        .collect();
    if cards.iter().any(|card| {
        !cols
            .iter()
            .any(|column| column.id == card.status_id.as_str())
    }) {
        widgets::state_block(
            ui,
            &ViewState::Stale {
                what: "workflow columns and cards; refresh to reconcile their current versions"
                    .into(),
            },
        );
        return;
    }

    let mut dropped: Option<(WorkRecord, String)> = None;
    egui::ScrollArea::horizontal()
        .id_salt("board")
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                for col in cols {
                    let in_col: Vec<&WorkRecord> = cards
                        .iter()
                        .filter(|c| c.status_id.as_str() == col.id)
                        .collect();
                    let frame = egui::Frame::new()
                        .stroke(theme::soft_line())
                        .corner_radius(theme::radius())
                        .inner_margin(theme::margin(8, 8));
                    let (_, payload) = ui.dnd_drop_zone::<DraggedCard, _>(frame, |ui| {
                        ui.set_width(200.0);
                        ui.set_min_height(240.0);
                        ui.label(
                            RichText::new(format!("{} ({})", col.name, in_col.len())).strong(),
                        );
                        for c in &in_col {
                            let bucket: Vec<&WorkRecord> =
                                cards.iter().filter(|x| x.priority == c.priority).collect();
                            let id = c.id.as_str().to_owned();
                            let pending = waiting.contains(&id);
                            let marker = work.moving.get(&id).map(|s| status_name(cols, s));
                            egui::Frame::new()
                                .fill(theme::PANEL)
                                .stroke(theme::line())
                                .corner_radius(theme::radius())
                                .inner_margin(theme::margin(8, 6))
                                .show(ui, |ui| {
                                    ui.set_width(184.0);
                                    let r = ui
                                        .dnd_drag_source(
                                            egui::Id::unique(("card", &id)),
                                            DraggedCard((*c).clone()),
                                            |ui| {
                                                ui.label(short(&c.title.0, 40));
                                            },
                                        )
                                        .response;
                                    if r.clicked() {
                                        actions.push(Action::SelectCard(Some(id.clone())));
                                    }
                                    if ui
                                        .small_button(format!("Open {}", short(&c.title.0, 18)))
                                        .clicked()
                                    {
                                        actions.push(Action::SelectCard(Some(id.clone())));
                                    }
                                    match (&marker, pending) {
                                        (Some(to), true) => {
                                            widgets::caption(ui, format!("Moving to {to}…"))
                                        }
                                        (_, true) => {
                                            widgets::caption(ui, "A change is not confirmed yet")
                                        }
                                        _ => {}
                                    }
                                    if !c.dependency_ids.0.is_empty() {
                                        widgets::caption(
                                            ui,
                                            format!("Blocked by {}", c.dependency_ids.0.len()),
                                        );
                                    }
                                    // Accessible alternatives to drag and drop.
                                    let mut target = c.status_id.as_str().to_owned();
                                    egui::ComboBox::from_id_salt(("move", &id))
                                        .selected_text(format!("Move {}", short(&c.title.0, 12)))
                                        .show_ui(ui, |ui| {
                                            for col in cols {
                                                ui.selectable_value(
                                                    &mut target,
                                                    col.id.clone(),
                                                    &col.name,
                                                );
                                            }
                                        })
                                        .response
                                        .widget_info(|| {
                                            egui::WidgetInfo::labeled(
                                                egui::Role::ComboBox,
                                                open && !pending,
                                                format!("Move {}", c.title.0),
                                            )
                                        });
                                    if target != c.status_id.as_str() && open && !pending {
                                        actions.push(Action::Move((*c).clone(), target));
                                    }
                                    let expected = c
                                        .queue_revision
                                        .as_ref()
                                        .map_or("1".to_owned(), |r| r.as_str().to_owned());
                                    let prio = c.priority;
                                    let f = feature.clone();
                                    ui.horizontal(|ui| {
                                        order_controls(
                                            ui,
                                            c,
                                            &bucket,
                                            open && !pending,
                                            |ids| {
                                                Action::Order(
                                                    "tasks.prioritize",
                                                    Some(f.clone()),
                                                    Some(prio),
                                                    ids,
                                                    expected.clone(),
                                                )
                                            },
                                            actions,
                                        );
                                    });
                                });
                        }
                    });
                    if let Some(p) = payload
                        && p.0.status_id.as_str() != col.id
                        && open
                        && !waiting.contains(p.0.id.as_str())
                    {
                        dropped = Some((p.0.clone(), col.id.clone()));
                    }
                }
            });
        });
    if let Some((c, to)) = dropped {
        actions.push(Action::Move(c, to));
    }
}

pub(crate) fn no_project(ui: &mut Ui, online: &Result<(), String>) {
    let state = match online {
        Err(reason) => ViewState::Offline {
            reason: reason.clone(),
        },
        Ok(()) => ViewState::Empty {
            title: "No project selected".into(),
            body: "Select a project in the navigation to see its work.".into(),
        },
    };
    widgets::state_block(ui, &state);
}
pub(crate) fn list_state(ui: &mut Ui, list: &ListState<WorkRecord>, what: &str) -> bool {
    if let Some(e) = &list.error {
        widgets::state_block(
            ui,
            &if list.loaded {
                ViewState::Stale { what: what.into() }
            } else {
                ViewState::Failed {
                    what: what.into(),
                    error: e.clone(),
                }
            },
        );
        if !list.loaded {
            return false;
        }
    }
    if !list.loaded {
        widgets::state_block(ui, &ViewState::Loading { what: what.into() });
        return false;
    }
    true
}
