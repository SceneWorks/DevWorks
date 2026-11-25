//! The Issues screen: the standalone issue list and the selected issue.
use crate::route::Route;
use crate::shell::work::*;
use crate::theme;
use crate::widgets::{self, Header, ViewState, chip};
use crate::work_ops::WorkSlot;
use crate::work_screen::detail::card_detail;
use crate::work_screen::{create_form, issue_list, no_project};
use eframe::egui::{self, Ui};
use std::collections::BTreeSet;
impl WorkCtl<'_, '_> {
    pub(crate) fn issues_screen(&mut self, ui: &mut Ui) {
        let screen = Route::Issues.screen();
        let online = self.cx.online.clone();
        let mut mem = self.routes.remove(&Route::Issues).unwrap_or_default();
        let mut actions: Vec<Action> = Vec::new();
        let project = self.selected_project.clone();
        let open = self.project_open() && online.is_ok();
        let cols = self
            .work
            .workflows
            .get(project.as_deref().unwrap_or(""))
            .map(|workflow| columns(&workflow.rows.items))
            .unwrap_or_default();
        let waiting: BTreeSet<String> = self
            .work
            .cards
            .items
            .iter()
            .map(|w| w.id.as_str().to_owned())
            .filter(|id| self.waiting(&WorkSlot::WorkCommand(id.clone())))
            .collect();
        let creating = project
            .as_ref()
            .is_some_and(|p| self.waiting(&WorkSlot::WorkCreate(p.clone())));
        let tab = mem.tab.min(1);
        let mut new_tab = tab;
        {
            let work = &mut self.work;
            let mut st = (&mut *work, &mut mem, &mut actions);
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
                            tabs: &ISSUE_TABS,
                            tab,
                        },
                        st,
                        |ui, st| {
                            if widgets::secondary_button(ui, "Refresh").clicked() {
                                st.2.push(Action::Reload);
                            }
                            let r = ui.add_enabled(
                                open && !creating && project.is_some(),
                                egui::Button::new("New issue"),
                            );
                            if r.clicked() && st.0.create.is_none() {
                                st.0.create = Some(CreateForm {
                                    kind: "issue".into(),
                                    ..CreateForm::default()
                                });
                            }
                        },
                        |ui, st, tab| {
                            let (work, mem, actions) = (&mut *st.0, &mut *st.1, &mut *st.2);
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
                            issue_list(ui, work, mem, &cols, tab == 1, open, actions);
                        },
                    );
                    new_tab = t;
                },
                |ui, st| {
                    let (work, mem, actions) = (&mut *st.0, &*st.1, &mut *st.2);
                    widgets::secondary_pane(ui, "Issue", |ui| {
                        let found = mem.selection.as_ref().and_then(|id| work.freshest(id));
                        match found {
                            None => {
                                widgets::state_block(
                                    ui,
                                    &ViewState::Empty {
                                        title: "Nothing selected".into(),
                                        body: "Select an issue to see and change it here.".into(),
                                    },
                                );
                            }
                            Some(w) => {
                                let id = w.id.as_str().to_owned();
                                card_detail(
                                    ui,
                                    work,
                                    &w,
                                    &cols,
                                    open && !waiting.contains(&id),
                                    actions,
                                );
                                reviews::section(ui, work, &id, actions);
                            }
                        }
                    });
                },
            );
        }
        if let Some(sel) = mem.selection.clone()
            && !self.work.detail.contains_key(&sel)
            && !self.pending(&WorkSlot::WorkDetail(sel.clone()))
        {
            actions.push(Action::OpenDetail(sel));
        }
        mem.tab = new_tab;
        self.routes.insert(Route::Issues, mem);
        self.unlink_dialog(ui.ctx(), &mut actions);
        self.workflow_dialog(ui.ctx(), &mut actions);
        self.work_act(actions);
    }
}
