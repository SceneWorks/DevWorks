//! A route whose content belongs to a later item: the full shell, integrated header,
//! subviews, summary strip and inner sidebar, with a designed unavailable state. It makes no
//! requests and keeps only the route's selected subview (route memory).
use crate::memory::RouteMemory;
use crate::route::Route;
use crate::screen::{Cx, NoSlot, Screen, ScreenContext, ScreenId};
use crate::theme;
use crate::widgets::{self, Header, ViewState};
use eframe::egui::Ui;

/// The state of the placeholder screens: none.
#[derive(Default)]
pub struct PlaceholderScreen;

impl Screen for PlaceholderScreen {
    type Slot = NoSlot;
    const ID: ScreenId = ScreenId::Placeholder;
    fn load(&mut self, _cx: &mut Cx, _route: Route) {}
    fn refresh(&mut self, _cx: &mut Cx, _route: Route) {}
    fn apply(&mut self, cx: &mut Cx, slot: NoSlot, reply: crate::backend::Reply) {
        cx.unhandled(&slot, reply);
    }
    fn ui(&mut self, cx: &mut Cx, route: Route, ui: &mut Ui) {
        let screen = route.screen();
        let online = cx.online().map_err(str::to_owned);
        let mut link = false;
        let tab = cx.routes.entry(route).or_default().tab;
        let primary = |ui: &mut Ui, link: &mut bool| -> usize {
            widgets::pane(
                ui,
                Header {
                    title: screen.title,
                    summary: screen.summary,
                    tabs: screen.subviews,
                    tab,
                },
                link,
                |ui, link| {
                    if let Some((label, _)) = screen.link
                        && widgets::secondary_button(ui, label).clicked()
                    {
                        *link = true;
                    }
                },
                |ui, _, tab| {
                    if !screen.summary_strip.is_empty() {
                        let items: Vec<(&str, Option<String>)> =
                            screen.summary_strip.iter().map(|l| (*l, None)).collect();
                        let owner = screen.pending.map_or("", |p| p.owner);
                        widgets::summary_strip(ui, &items, &format!("not available until {owner}"));
                        ui.add_space(10.0);
                    }
                    let state = match (&online, screen.pending) {
                        (Err(reason), _) => ViewState::Offline {
                            reason: reason.clone(),
                        },
                        (Ok(()), Some(p)) => ViewState::Unavailable {
                            what: match screen.subviews.get(tab) {
                                Some(s) => format!("{s}: {}", p.what),
                                None => p.what.to_owned(),
                            },
                            owner: p.owner.to_owned(),
                        },
                        (Ok(()), None) => ViewState::Empty {
                            title: "Nothing here yet".into(),
                            body: String::new(),
                        },
                    };
                    widgets::state_block(ui, &state);
                },
            )
            .1
        };
        let new_tab = match screen.inner_sidebar {
            Some(title) => {
                let mut st = (&mut link, tab);
                widgets::with_right_sidebar(
                    ui,
                    theme::SIDEBAR,
                    &mut st,
                    |ui, st| st.1 = primary(ui, st.0),
                    |ui, _| {
                        widgets::secondary_pane(ui, title, |ui| {
                            widgets::state_block(
                                ui,
                                &ViewState::Empty {
                                    title: "Nothing selected".into(),
                                    body: "Select an item to see its detail here.".into(),
                                },
                            );
                        });
                    },
                );
                st.1
            }
            None => primary(ui, &mut link),
        };
        cx.routes.entry(route).or_default().tab = new_tab;
        if link && let Some((_, target)) = screen.link {
            cx.navigate(target);
        }
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: mem.tab,
            selection: mem.selection.clone(),
        }
    }
}
