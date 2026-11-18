//! The project Overview: its summary, the Activity feed and the message board.
use crate::route::Route;
use crate::shell::work::*;
use crate::widgets::{self, Header};
use crate::work_screen::activity::activity;
use crate::work_screen::messages::message_board;
use crate::work_screen::{list_state, no_project};
use devworks_core::protocol::records::{
    ApprovalState, FindingState, ReadinessDisposition, WorkRecord,
};
use eframe::egui::{RichText, Ui};
impl WorkCtl<'_, '_> {
    pub(crate) fn overview_screen(&mut self, ui: &mut Ui) {
        let screen = Route::Overview.screen();
        let online = self.cx.online.clone();
        let open = self.project_open() && online.is_ok();
        let mut mem = self.routes.remove(&Route::Overview).unwrap_or_default();
        let mut actions: Vec<Action> = Vec::new();
        let project = self.selected_project.clone();
        let cols = self
            .work
            .workflows
            .get(project.as_deref().unwrap_or(""))
            .map(|workflow| columns(&workflow.rows))
            .unwrap_or_default();
        let user = devworks_desktop_user();
        let messages_tab = mem.tab.min(2) == 2;
        let work = &mut self.work;
        let (_, tab) = widgets::pane(
            ui,
            Header {
                title: screen.title,
                summary: screen.summary,
                tabs: &OVERVIEW_TABS,
                tab: mem.tab.min(2),
            },
            &mut actions,
            |ui, actions| {
                if widgets::secondary_button(ui, "Refresh").clicked() {
                    actions.push(if messages_tab {
                        Action::RefreshMessages
                    } else {
                        Action::Reload
                    });
                }
            },
            |ui, actions, tab| {
                let loaded = project.is_some() && work.features.loaded && work.cards.loaded;
                let issues: Vec<&WorkRecord> = work
                    .cards
                    .items
                    .iter()
                    .filter(|c| c.parent_id.is_none())
                    .collect();
                let working = work
                    .features
                    .items
                    .iter()
                    .chain(&work.cards.items)
                    .filter(|w| !w.attempts.0.is_empty())
                    .count();
                let needs = work
                    .features
                    .items
                    .iter()
                    .filter(|f| {
                        f.pending_plan
                            .as_ref()
                            .and_then(|p| p.approval.as_ref())
                            .is_some_and(|a| matches!(a.state, ApprovalState::Pending))
                            || f.readiness.as_ref().is_some_and(|r| {
                                r.findings.0.iter().any(|x| {
                                    matches!(x.disposition, ReadinessDisposition::NeedsUserDecision)
                                        && matches!(x.state, FindingState::Open)
                                })
                            })
                    })
                    .count();
                let value = |n: usize| loaded.then(|| n.to_string());
                widgets::summary_strip(
                    ui,
                    &[
                        ("Features", value(work.features.items.len())),
                        ("Issues", value(issues.len())),
                        ("Working", value(working)),
                        ("Needs you", value(needs)),
                    ],
                    "select a project and wait for its work to load",
                );
                ui.add_space(10.0);
                if project.is_none() {
                    no_project(ui, &online);
                    return;
                }
                workflow::controls(ui, work, open, online.is_ok(), actions);
                if tab == 0 {
                    if !list_state(ui, &work.features, "features")
                        || !list_state(ui, &work.cards, "tasks and issues")
                    {
                        return;
                    }
                    ui.label(RichText::new("In progress").strong());
                    let active: Vec<&WorkRecord> = work
                        .features
                        .items
                        .iter()
                        .chain(&work.cards.items)
                        .filter(|w| {
                            let s = semantic_of(&cols, w.status_id.as_str());
                            s == "InProgress" || s == "InReview"
                        })
                        .collect();
                    if active.is_empty() {
                        widgets::caption(ui, "Nothing is in progress.");
                    }
                    for w in active {
                        ui.label(format!(
                            "{} · {} · {}",
                            kind_label(w),
                            w.title.0,
                            status_name(&cols, w.status_id.as_str())
                        ));
                    }
                } else if tab == 1 {
                    activity(ui, work, &cols, &user, actions);
                } else {
                    if work.messages.project.as_deref() == project.as_deref() {
                        message_board(ui, &mut work.messages, open, actions);
                    } else {
                        widgets::caption(ui, "Loading this project's messages…");
                    }
                }
            },
        );
        if tab == 2 && self.work.messages.project.as_deref() != project.as_deref() {
            actions.push(Action::RefreshMessages);
        } else if tab == 2 {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs(1));
            if self
                .work
                .messages
                .event_poll_at
                .is_some_and(|at| at <= std::time::Instant::now())
            {
                actions.push(Action::PollMessages);
            }
        }
        mem.tab = tab;
        self.routes.insert(Route::Overview, mem);
        self.workflow_dialog(ui.ctx(), &mut actions);
        self.work_act(actions);
    }
}
