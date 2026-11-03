//! Shared native components that carry the approved treatment through every page (UI-15):
//! a primary pane whose header is integrated into its continuous outer border with a divider
//! into the body, secondary panes that start alongside that header, inline summary strips,
//! chips, and designed empty / loading / offline / unavailable / failed / stale states (UI-06).
use crate::theme;
use eframe::egui::{
    self, Align, Color32, Layout, Rect, RichText, Sense, Stroke, Ui, UiBuilder, Vec2,
};

/// Tone of a chip or state block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Accent,
    Good,
    Attention,
    Bad,
}
impl Tone {
    fn colors(self) -> (Color32, Color32) {
        match self {
            Self::Neutral => (theme::MUTED, theme::RAISED),
            Self::Accent => (theme::ACCENT, theme::ACCENT_BG),
            Self::Good => (theme::GREEN, theme::GREEN_BG),
            Self::Attention => (theme::AMBER, theme::AMBER_BG),
            Self::Bad => (theme::RED, theme::RED_BG),
        }
    }
}

/// A small bordered label.
pub fn chip(ui: &mut Ui, text: impl Into<String>, tone: Tone) -> egui::Response {
    let (fg, bg) = tone.colors();
    egui::Frame::new()
        .fill(bg)
        .stroke(Stroke::new(1.0, fg.gamma_multiply(0.35)))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(theme::margin(6, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text.into()).small().color(fg));
        })
        .response
}

/// A quieter secondary action.
pub fn secondary_button(ui: &mut Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).color(theme::MUTED)))
}

/// A designed non-success state. Every variant says what is missing and why; none renders a
/// fabricated value.
#[derive(Clone, Debug, PartialEq)]
pub enum ViewState {
    /// The source answered and there is nothing yet.
    Empty {
        title: String,
        body: String,
    },
    Loading {
        what: String,
    },
    /// The runtime is not available (it did not start, or it is shutting down).
    Offline {
        reason: String,
    },
    /// The content belongs to an item not delivered in this build.
    Unavailable {
        what: String,
        owner: String,
    },
    /// The request failed; it can be retried.
    Failed {
        what: String,
        error: String,
    },
    /// Showing earlier data while a refresh is pending or after it failed.
    Stale {
        what: String,
    },
    /// Only part of the data could be shown.
    Partial {
        what: String,
        missing: String,
    },
}

/// What a state block asks the caller to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateAction {
    None,
    Retry,
}

pub fn state_block(ui: &mut Ui, state: &ViewState) -> StateAction {
    let (title, body, tone, retry) = match state {
        ViewState::Empty { title, body } => (title.clone(), body.clone(), Tone::Neutral, false),
        ViewState::Loading { what } => (
            format!("Loading {what}…"),
            String::new(),
            Tone::Neutral,
            false,
        ),
        ViewState::Offline { reason } => (
            "Offline".to_owned(),
            format!(
                "The DevWorks runtime is not available: {reason} Local screens stay open; nothing is shown as current."
            ),
            Tone::Attention,
            false,
        ),
        ViewState::Unavailable { what, owner } => (
            "Not available in this build".to_owned(),
            format!(
                "{} — delivered by {owner}. Nothing is shown until it is real.",
                capitalize(what)
            ),
            Tone::Neutral,
            false,
        ),
        ViewState::Failed { what, error } => (
            format!("Could not load {what}"),
            error.clone(),
            Tone::Bad,
            true,
        ),
        ViewState::Stale { what } => (
            format!("Showing earlier {what}"),
            "A refresh is pending; values may be out of date.".to_owned(),
            Tone::Attention,
            false,
        ),
        ViewState::Partial { what, missing } => (
            format!("Partial {what}"),
            format!("Not shown: {missing}."),
            Tone::Attention,
            false,
        ),
    };
    let (fg, _) = tone.colors();
    let mut action = StateAction::None;
    egui::Frame::new()
        .fill(theme::PANEL)
        .stroke(theme::line())
        .corner_radius(theme::radius())
        .inner_margin(theme::margin(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let loading = matches!(state, ViewState::Loading { .. });
            ui.horizontal(|ui| {
                if loading {
                    ui.add(egui::Spinner::new().size(14.0));
                }
                ui.label(RichText::new(&title).strong().color(fg));
            });
            if !body.is_empty() {
                ui.label(RichText::new(&body).color(theme::MUTED));
            }
            if retry && ui.button("Retry").clicked() {
                action = StateAction::Retry;
            }
        });
    action
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// The integrated header of a primary pane.
pub struct Header<'a> {
    pub title: &'a str,
    pub summary: &'a str,
    /// Subview tabs (empty: none).
    pub tabs: &'a [&'a str],
    /// The selected subview.
    pub tab: usize,
}

/// A primary pane: one continuous bordered surface, the header inside it, a divider, then the
/// body. `actions` renders right-aligned header actions and `body` the content for the selected
/// subview; both share `state`. Returns the body's result and the selected subview.
pub fn pane<S, R>(
    ui: &mut Ui,
    header: Header<'_>,
    state: &mut S,
    actions: impl FnOnce(&mut Ui, &mut S),
    body: impl FnOnce(&mut Ui, &mut S, usize) -> R,
) -> (R, usize) {
    let Header {
        title,
        summary,
        tabs,
        mut tab,
    } = header;
    let r = egui::Frame::new()
        .fill(theme::PANEL)
        .stroke(theme::line())
        .corner_radius(theme::radius())
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            egui::Frame::new()
                .inner_margin(theme::margin(22, 16))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_top(|ui| {
                        ui.vertical(|ui| {
                            ui.heading(RichText::new(title).color(theme::TEXT));
                            if !summary.is_empty() {
                                ui.label(RichText::new(summary).color(theme::MUTED));
                            }
                        });
                        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                            actions(ui, &mut *state);
                        });
                    });
                    if !tabs.is_empty() {
                        ui.add_space(8.0);
                        tab_row(ui, title, tabs, &mut tab);
                    }
                });
            divider(ui);
            egui::Frame::new()
                .inner_margin(theme::margin(22, 16))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    body(ui, state, tab)
                })
                .inner
        })
        .inner;
    (r, tab)
}

/// The full-width divider between an integrated header and its body.
pub fn divider(ui: &mut Ui) {
    let rect = ui.available_rect_before_wrap();
    let y = ui.cursor().min.y;
    ui.painter().hline(rect.x_range(), y, theme::soft_line());
    ui.add_space(1.0);
}

/// Subview tabs: selectable buttons in a row, reachable with Tab and named for screen readers.
pub fn tab_row(ui: &mut Ui, owner: &str, labels: &[&str], selected: &mut usize) {
    ui.horizontal_wrapped(|ui| {
        for (i, label) in labels.iter().enumerate() {
            let r = ui
                .selectable_label(*selected == i, *label)
                .on_hover_text(format!("{owner}: {label}"));
            if r.clicked() {
                *selected = i;
            }
        }
    });
}

/// A secondary pane (inner sidebar) with its own bordered surface and title.
pub fn secondary_pane<R>(ui: &mut Ui, title: &str, body: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(theme::PANEL)
        .stroke(theme::line())
        .corner_radius(theme::radius())
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            egui::Frame::new()
                .inner_margin(theme::margin(16, 14))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(title).strong());
                });
            divider(ui);
            egui::Frame::new()
                .inner_margin(theme::margin(16, 12))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    body(ui)
                })
                .inner
        })
        .inner
}

/// The compact inline summary strip beneath an integrated header: each value sits beside its
/// label on one baseline with one size and spacing. `None` values are not known and are shown
/// as unavailable, never as zero.
pub fn summary_strip(ui: &mut Ui, items: &[(&str, Option<String>)], unavailable: &str) {
    egui::Frame::new()
        .stroke(theme::soft_line())
        .corner_radius(theme::radius())
        .inner_margin(theme::margin(14, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                for (label, value) in items {
                    let shown = value.clone().unwrap_or_else(|| "—".to_owned());
                    let r = ui.label(RichText::new(format!("{shown} {label}")).color(
                        if value.is_some() {
                            theme::TEXT
                        } else {
                            theme::DIM
                        },
                    ));
                    if value.is_none() {
                        r.on_hover_text(format!("{label}: {unavailable}"));
                    }
                    ui.add_space(18.0);
                }
            });
        });
}

/// Splits `ui` into a primary area and a right secondary area of `side` width that starts at
/// the same top edge (aligned with the integrated header). Below [`theme::NARROW`] the
/// secondary area stacks under the primary one.
pub fn with_right_sidebar<S>(
    ui: &mut Ui,
    side: f32,
    state: &mut S,
    primary: impl FnOnce(&mut Ui, &mut S),
    secondary: impl FnOnce(&mut Ui, &mut S),
) {
    let full = ui.available_rect_before_wrap();
    let gap = 16.0;
    let (left, right) = if full.width() < theme::NARROW + side {
        let (top, bottom) = full.split_top_bottom_at_fraction(0.62);
        (
            top,
            Rect::from_min_max(bottom.min + Vec2::new(0.0, gap), bottom.max),
        )
    } else {
        (
            Rect::from_min_max(full.min, egui::pos2(full.max.x - side - gap, full.max.y)),
            Rect::from_min_max(egui::pos2(full.max.x - side, full.min.y), full.max),
        )
    };
    ui.scope_builder(UiBuilder::new().max_rect(left), |ui| primary(ui, state));
    ui.scope_builder(UiBuilder::new().max_rect(right), |ui| secondary(ui, state));
    ui.allocate_rect(full, Sense::hover());
}

/// A muted caption.
pub fn caption(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).small().color(theme::DIM));
}

/// Painted icons (the default fonts carry no icon glyphs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Chat,
    Bell,
    Plus,
    ChevronDown,
    ChevronRight,
    Dot,
}

/// Paints `icon` centred in `rect` with `color`.
pub fn paint_icon(ui: &Ui, rect: Rect, icon: Icon, color: Color32) {
    let p = ui.painter();
    let c = rect.center();
    let s = Stroke::new(1.4, color);
    match icon {
        Icon::Chat => {
            let r = Rect::from_center_size(c - Vec2::new(0.0, 1.0), Vec2::new(14.0, 10.0));
            p.rect_stroke(r, 2.0, s, egui::StrokeKind::Middle);
            p.line_segment(
                [
                    r.left_bottom() + Vec2::new(3.0, 0.0),
                    r.left_bottom() + Vec2::new(2.0, 4.0),
                ],
                s,
            );
            p.line_segment(
                [
                    r.left_bottom() + Vec2::new(2.0, 4.0),
                    r.left_bottom() + Vec2::new(7.0, 0.0),
                ],
                s,
            );
        }
        Icon::Bell => {
            let top = c - Vec2::new(0.0, 6.0);
            p.circle_stroke(c - Vec2::new(0.0, 1.0), 4.5, s);
            p.line_segment([c + Vec2::new(-6.0, 4.0), c + Vec2::new(6.0, 4.0)], s);
            p.circle_filled(c + Vec2::new(0.0, 6.0), 1.4, color);
            p.circle_filled(top, 1.0, color);
        }
        Icon::Plus => {
            p.line_segment([c - Vec2::new(5.0, 0.0), c + Vec2::new(5.0, 0.0)], s);
            p.line_segment([c - Vec2::new(0.0, 5.0), c + Vec2::new(0.0, 5.0)], s);
        }
        Icon::ChevronDown => {
            p.line_segment([c + Vec2::new(-4.0, -2.0), c + Vec2::new(0.0, 2.0)], s);
            p.line_segment([c + Vec2::new(0.0, 2.0), c + Vec2::new(4.0, -2.0)], s);
        }
        Icon::ChevronRight => {
            p.line_segment([c + Vec2::new(-2.0, -4.0), c + Vec2::new(2.0, 0.0)], s);
            p.line_segment([c + Vec2::new(2.0, 0.0), c + Vec2::new(-2.0, 4.0)], s);
        }
        Icon::Dot => {
            p.circle_filled(c, 3.0, color);
        }
    }
}

/// An icon-only button with an accessible name and tooltip (`selected` for toggles).
pub fn icon_button(
    ui: &mut Ui,
    icon: Icon,
    label: &str,
    tooltip: &str,
    enabled: bool,
    selected: Option<bool>,
) -> egui::Response {
    let size = Vec2::new(30.0, 26.0);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    let visuals = ui
        .style()
        .interact_selectable(&response, selected.unwrap_or(false));
    if response.hovered() || selected == Some(true) {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(5), visuals.weak_bg_fill);
    }
    ui.painter().rect_stroke(
        rect,
        egui::CornerRadius::same(5),
        theme::line(),
        egui::StrokeKind::Inside,
    );
    let color = if enabled {
        visuals.fg_stroke.color
    } else {
        theme::DIM
    };
    paint_icon(ui, rect, icon, color);
    let label = label.to_owned();
    response.widget_info(|| match selected {
        Some(s) => egui::WidgetInfo::selected(egui::Role::Button, enabled, s, label.clone()),
        None => egui::WidgetInfo::labeled(egui::Role::Button, enabled, label.clone()),
    });
    response.on_hover_text(tooltip)
}
