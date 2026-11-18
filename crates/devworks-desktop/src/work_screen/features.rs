//! The Features screen: the feature list, the board, the selected feature or card and the
//! card-removal confirmation.
use crate::route::Route;
use crate::shell::work::*;
use crate::theme;
use crate::watch_ops::{WatchScope, WatchView};
use crate::widgets::{self, Header, ViewState, chip};
use crate::work_ops::WorkSlot;
use crate::work_screen::detail::{card_detail, feature_detail};
use crate::work_screen::{board, create_form, feature_list, no_project};
use eframe::egui::{self, Ui};
use std::collections::BTreeSet;
impl WorkCtl<'_, '_> {
    pub(crate) fn features_screen(&mut self, ui: &mut Ui) {
        let screen = Route::Features.screen();
        let online = self.cx.online.clone();
        let mut mem = self.routes.remove(&Route::Features).unwrap_or_default();
        let mut actions: Vec<Action> = Vec::new();
        let project = self.selected_project.clone();
        let open = self.project_open() && online.is_ok();
        let cols = self
            .work
            .workflows
            .get(project.as_deref().unwrap_or(""))
            .map(|workflow| columns(&workflow.rows))
            .unwrap_or_default();
        let waiting: BTreeSet<String> = self
            .work
            .features
            .items
            .iter()
            .chain(&self.work.cards.items)
            .map(|w| w.id.as_str().to_owned())
            .filter(|id| {
                self.waiting(&WorkSlot::WorkCommand(id.clone()))
                    || self.waiting(&WorkSlot::WorkPlanText(id.clone()))
                    || self.waiting(&WorkSlot::WorkPlanDrop(id.clone()))
                    || self.waiting(&WorkSlot::WorkPlan(id.clone()))
            })
            .collect();
        let creating = project
            .as_ref()
            .is_some_and(|p| self.waiting(&WorkSlot::WorkCreate(p.clone())));
        let projects_repos: Vec<String> = self
            .selected_project_record()
            .map(|p| {
                p.repository_ids
                    .0
                    .iter()
                    .map(|r| r.as_str().to_owned())
                    .collect()
            })
            .unwrap_or_default();
        let tab = mem.tab.min(1);
        let mut new_tab = tab;
        let mut watch_actions = Vec::new();
        {
            let work = &mut self.work;
            let watch = self.cx.watchers;
            let watch_online = online.as_ref().map(|_| ()).map_err(String::as_str);
            let mut st = (&mut *work, &mut mem, &mut actions, &mut watch_actions);
            widgets::with_right_sidebar(
                ui,
                theme::SIDEBAR,
                &mut st,
                |ui, st| {
                    let (_, t) = widgets::pane(
                        ui,
                        Header {
                            title: screen.title,
                            summary: screen.summary,
                            tabs: &FEATURE_TABS,
                            tab,
                        },
                        st,
                        |ui, st| {
                            if widgets::secondary_button(ui, "Refresh").clicked() {
                                st.2.push(Action::Reload);
                            }
                            let r = ui.add_enabled(
                                open && !creating && project.is_some(),
                                egui::Button::new(if tab == 1 {
                                    "New task"
                                } else {
                                    "New feature"
                                }),
                            );
                            if r.clicked() && st.0.create.is_none() {
                                st.0.create = Some(CreateForm {
                                    kind: if tab == 1 { "task" } else { "feature" }.into(),
                                    feature: if tab == 1 { st.0.board.clone() } else { None },
                                    ..CreateForm::default()
                                });
                            }
                        },
                        |ui, st, tab| {
                            let (work, mem, actions) = (&mut *st.0, &mut *st.1, &mut *st.2);
                            let counts = |sem: &str| {
                                work.features
                                    .items
                                    .iter()
                                    .filter(|f| semantic_of(&cols, f.status_id.as_str()) == sem)
                                    .count()
                            };
                            let loaded = project.is_some() && work.features.loaded;
                            let v = |n: usize| loaded.then(|| n.to_string());
                            widgets::summary_strip(
                                ui,
                                &[
                                    ("Planned", v(counts("Backlog") + counts("Ready"))),
                                    ("In progress", v(counts("InProgress"))),
                                    ("In review", v(counts("InReview"))),
                                    ("Complete", v(counts("Done"))),
                                ],
                                "select a project and wait for its features to load",
                            );
                            ui.add_space(10.0);
                            if project.is_none() {
                                no_project(ui, &online);
                                return;
                            }
                            if let Some((tone, text)) = &work.message {
                                chip(ui, text.clone(), *tone);
                                ui.add_space(6.0);
                            }
                            workflow::controls(ui, work, open, online.is_ok(), actions);
                            create_form(ui, work, creating, actions);
                            if tab == 0 {
                                feature_list(ui, work, mem, &cols, open, actions);
                            } else {
                                board(ui, work, &cols, open, &waiting, actions);
                            }
                        },
                    );
                    new_tab = t;
                },
                |ui, st| {
                    let (work, mem, actions) = (&mut *st.0, &*st.1, &mut *st.2);
                    let watch_actions = &mut *st.3;
                    widgets::secondary_pane(ui, "Feature", |ui| {
                        if tab == 1
                            && let Some(c) = work.card.clone()
                        {
                            let found = work.freshest(&c);
                            if let Some(w) = found {
                                card_detail(
                                    ui,
                                    work,
                                    &w,
                                    &cols,
                                    open && !waiting.contains(&c),
                                    actions,
                                );
                                return;
                            }
                        }
                        let selected = if tab == 1 {
                            work.board.clone()
                        } else {
                            mem.selection.clone()
                        };
                        let found = selected.and_then(|id| work.freshest(&id));
                        match found {
                            None => {
                                widgets::state_block(
                                    ui,
                                    &ViewState::Empty {
                                        title: "Nothing selected".into(),
                                        body: "Select a feature to see its plan, readiness and work here."
                                            .into(),
                                    },
                                );
                            }
                            Some(f) => {
                                let id = f.id.as_str().to_owned();
                                let scope = WatchScope::Feature(id.clone());
                                feature_detail(
                                    ui,
                                    work,
                                    &f,
                                    &cols,
                                    &projects_repos,
                                    open && !waiting.contains(&id),
                                    actions,
                                    &mut |ui| {
                                        if let Some(watch) = watch {
                                            watch.section(
                                                ui,
                                                WatchView::Feature,
                                                scope.clone(),
                                                watch_online,
                                                watch_actions,
                                            )
                                        }
                                    },
                                );
                            }
                        }
                    });
                },
            );
        }
        // A newly selected feature reads its current record (readiness).
        if let Some(sel) = mem.selection.clone()
            && !self.work.detail.contains_key(&sel)
            && !self.pending(&WorkSlot::WorkDetail(sel.clone()))
        {
            actions.push(Action::OpenDetail(sel));
        }
        mem.tab = new_tab;
        self.routes.insert(Route::Features, mem);
        self.unlink_dialog(ui.ctx(), &mut actions);
        self.workflow_dialog(ui.ctx(), &mut actions);
        self.work_act(actions);
        self.watch_act(watch_actions);
    }

    pub(crate) fn unlink_dialog(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let Some(w) = self.work.unlink.clone() else {
            return;
        };
        let mut decided = None;
        egui::Window::new("Remove from feature")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label(format!(
                    "Remove {} from its feature? It becomes a standalone issue with the same \
                     identity, history, branches and workspace. Its work is paused and stays \
                     paused until you continue it; adding it to a feature again does not resume \
                     it.",
                    w.title.0
                ));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Remove from feature").clicked() {
                        decided = Some(true);
                    }
                    if widgets::secondary_button(ui, "Cancel").clicked() {
                        decided = Some(false);
                    }
                });
            });
        match decided {
            Some(true) => {
                self.work.unlink = None;
                actions.push(Action::Command(
                    w.id.as_str().to_owned(),
                    unlink_request(&w),
                ));
            }
            Some(false) => self.work.unlink = None,
            None => {}
        }
    }
}
