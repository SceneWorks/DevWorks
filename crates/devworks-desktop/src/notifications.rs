//! The shell-owned completion-card overlay (NOTIFY-05 presentation; NOTIFY-01 surface seam).
//!
//! The approvals and notifications item (DW-14) owns the durable notification records,
//! duplicate-free completion events, `notifications.read` and the revision-guarded
//! `notifications.acknowledge`. This module is the presentation it feeds: cards float at the
//! top centre below the toolbar on every screen, reserve no layout space, never take focus,
//! stack newest first, expand to show every message, and leave only when dismissed one at a
//! time. A dismissal is queued for the owner to acknowledge ([`Completions::take_dismissed`]);
//! it never resolves an approval or resumes work, and opening linked work does not dismiss.
//! No card is ever fabricated here: with no feed connected the overlay is empty.
use crate::theme;
use eframe::egui::{self, Align2, Order, RichText};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionCard {
    /// The notification identity (dismissal and duplicate suppression key).
    pub id: String,
    /// Project label.
    pub project: String,
    /// "Feature completed", "Issue completed".
    pub kind: String,
    pub title: String,
    pub at: String,
    /// The completed work's navigation target label.
    pub link: String,
}

/// What the user did with a card this frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardAction {
    Open(String),
    Dismiss(String),
}

#[derive(Default)]
pub struct Completions {
    cards: Vec<CompletionCard>,
    seen: BTreeSet<String>,
    dismissed: Vec<String>,
    pub expanded: bool,
    /// The newest card not yet announced to assistive technology.
    announce: Option<String>,
}

impl Completions {
    /// Adds a card from the notification feed; a repeated identity is suppressed.
    pub fn push(&mut self, card: CompletionCard) -> bool {
        if !self.seen.insert(card.id.clone()) {
            return false;
        }
        self.announce = Some(format!("{}: {}", card.kind, card.title));
        self.cards.insert(0, card);
        true
    }
    pub fn cards(&self) -> &[CompletionCard] {
        &self.cards
    }
    /// Removes exactly that card and queues its acknowledgment.
    pub fn dismiss(&mut self, id: &str) {
        if let Some(i) = self.cards.iter().position(|c| c.id == id) {
            self.cards.remove(i);
            self.dismissed.push(id.to_owned());
        }
    }
    /// Dismissals for the owner to acknowledge durably (`notifications.acknowledge`, DW-14).
    pub fn take_dismissed(&mut self) -> Vec<String> {
        std::mem::take(&mut self.dismissed)
    }
    /// The status text to announce once (polite live region).
    pub fn take_announcement(&mut self) -> Option<String> {
        self.announce.take()
    }

    /// Draws the overlay `top` points below the window's top edge.
    pub fn ui(&mut self, ctx: &egui::Context, top: f32) -> Vec<CardAction> {
        let mut actions = Vec::new();
        if self.cards.is_empty() {
            return actions;
        }
        let shown = if self.expanded { self.cards.len() } else { 1 };
        egui::Area::new(egui::Id::unique("completion-cards"))
            .order(Order::Foreground)
            .anchor(Align2::CENTER_TOP, egui::vec2(0.0, top))
            .show(ctx, |ui| {
                ui.set_width(480.0);
                for card in self.cards.iter().take(shown) {
                    egui::Frame::new()
                        .fill(theme::GREEN_BG)
                        .stroke(egui::Stroke::new(1.0, theme::GREEN))
                        .corner_radius(theme::radius())
                        .inner_margin(theme::margin(14, 10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&card.project).small().color(theme::MUTED));
                                ui.label(
                                    RichText::new(format!("{} · {}", card.kind, card.at))
                                        .small()
                                        .color(theme::MUTED),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let label = format!("Dismiss {}", card.title);
                                        let r = ui.small_button("×").on_hover_text(&label);
                                        r.widget_info(|| {
                                            egui::WidgetInfo::labeled(
                                                egui::Role::Button,
                                                true,
                                                label.clone(),
                                            )
                                        });
                                        if r.clicked() {
                                            actions.push(CardAction::Dismiss(card.id.clone()));
                                        }
                                    },
                                );
                            });
                            ui.label(RichText::new(&card.title).strong());
                            if ui.link(format!("View {}", card.link)).clicked() {
                                actions.push(CardAction::Open(card.id.clone()));
                            }
                        });
                    ui.add_space(4.0);
                }
                let more = self.cards.len().saturating_sub(1);
                if more > 0 {
                    let text = if self.expanded {
                        "Collapse completions".to_owned()
                    } else {
                        format!("{more} more completion{}", if more == 1 { "" } else { "s" })
                    };
                    if ui.small_button(text).clicked() {
                        self.expanded = !self.expanded;
                    }
                }
            });
        for a in &actions {
            if let CardAction::Dismiss(id) = a {
                self.dismiss(id);
            }
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str) -> CompletionCard {
        CompletionCard {
            id: id.into(),
            project: "Fixture project".into(),
            kind: "Issue completed".into(),
            title: format!("Card {id}"),
            at: "20:22".into(),
            link: "completed issue".into(),
        }
    }

    #[test]
    fn cards_stack_newest_first_suppress_duplicates_and_dismiss_one_at_a_time() {
        let mut c = Completions::default();
        assert!(c.push(card("a")));
        assert!(c.push(card("b")));
        assert!(
            !c.push(card("a")),
            "duplicate completion events are suppressed"
        );
        assert_eq!(
            c.cards().iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["b", "a"]
        );
        c.dismiss("b");
        assert_eq!(c.cards().len(), 1);
        assert_eq!(c.take_dismissed(), ["b"]);
        assert!(
            !c.push(card("b")),
            "a dismissed card does not come back from a replayed event"
        );
    }
}
