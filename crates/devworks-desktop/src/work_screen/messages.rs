//! The project message board: history, inbox and the post form.
use crate::shell::work::*;
use crate::widgets::{self, Tone, chip};
use eframe::egui::{self, Ui};
pub(crate) fn message_board(
    ui: &mut Ui,
    board: &mut MessageBoard,
    open: bool,
    actions: &mut Vec<Action>,
) {
    if let Some(error) = &board.error {
        chip(ui, error.clone(), Tone::Bad);
    }
    ui.heading("Your inbox");
    if !board.inbox_loaded && board.inbox.is_empty() {
        widgets::caption(ui, "Loading your messages…");
    } else if board.inbox.is_empty() {
        widgets::caption(ui, "No unread messages.");
    }
    if board.inbox_has_more
        && ui
            .add_enabled(open, egui::Button::new("Load more inbox messages"))
            .clicked()
    {
        actions.push(Action::MoreInbox);
    }
    for message in &board.inbox {
        ui.group(|ui| {
            ui.label(format!("{:?} · {}", message.kind, message.from.name.0));
            widgets::text_with_github_links(ui, &message.body.0);
            if let Some(item) = &message.item_id {
                widgets::caption(ui, format!("Work: {}", item.0.as_str()));
            }
            if message.blocking {
                chip(ui, "Blocking", Tone::Attention);
            }
            if ui
                .add_enabled(open, egui::Button::new("Mark read"))
                .clicked()
            {
                actions.push(Action::AckMessage(message.id.as_str().to_owned(), None));
            }
            if matches!(
                message.kind,
                devworks_core::protocol::records::RetainedMessageKind::Question
                    | devworks_core::protocol::records::RetainedMessageKind::Request
            ) && matches!(
                message.state,
                Some(devworks_core::protocol::records::RetainedMessageState::Open)
            ) {
                if ui
                    .add_enabled(open, egui::Button::new("Answer question"))
                    .clicked()
                {
                    actions.push(Action::AckMessage(
                        message.id.as_str().to_owned(),
                        Some("Answered"),
                    ));
                }
                if ui
                    .add_enabled(open, egui::Button::new("Decline question"))
                    .clicked()
                {
                    actions.push(Action::AckMessage(
                        message.id.as_str().to_owned(),
                        Some("Declined"),
                    ));
                }
            }
        });
    }
    ui.add_space(10.0);
    ui.heading("Project messages");
    if !board.history.loaded && board.history.items.is_empty() {
        widgets::caption(ui, "Loading message history…");
    } else if board.history.items.is_empty() {
        widgets::caption(ui, "No project messages yet.");
    }
    for message in &board.history.items {
        ui.group(|ui| {
            ui.label(format!("{:?} · {}", message.kind, message.from.name.0));
            widgets::text_with_github_links(ui, &message.body.0);
            let mut meta = Vec::new();
            if let Some(item) = &message.item_id {
                meta.push(format!("work {}", item.0.as_str()));
            }
            if let Some(feature) = &message.feature_id {
                meta.push(format!("feature {}", feature.0.as_str()));
            }
            if let Some(resource) = &message.resource_name {
                meta.push(format!("resource {}", resource.0));
            }
            if let Some(state) = &message.state {
                meta.push(format!("{state:?}"));
            }
            if !meta.is_empty() {
                widgets::caption(ui, meta.join(" · "));
            }
            // Project history retains read questions after their inbox delivery is acknowledged.
            // Keep their resolution actions available while the question remains open; an
            // unread question already offers them in the inbox above.
            if message.to_human
                && !board.inbox.iter().any(|unread| unread.id == message.id)
                && matches!(
                    message.kind,
                    devworks_core::protocol::records::RetainedMessageKind::Question
                        | devworks_core::protocol::records::RetainedMessageKind::Request
                )
                && matches!(
                    message.state,
                    Some(devworks_core::protocol::records::RetainedMessageState::Open)
                )
            {
                if ui
                    .add_enabled(open, egui::Button::new("Answer question"))
                    .clicked()
                {
                    actions.push(Action::AckMessage(
                        message.id.as_str().to_owned(),
                        Some("Answered"),
                    ));
                }
                if ui
                    .add_enabled(open, egui::Button::new("Decline question"))
                    .clicked()
                {
                    actions.push(Action::AckMessage(
                        message.id.as_str().to_owned(),
                        Some("Declined"),
                    ));
                }
            }
        });
    }
    if board.history.more() && widgets::secondary_button(ui, "Older messages").clicked() {
        actions.push(Action::OlderMessages);
    }
    ui.add_space(10.0);
    ui.heading("Post a message");
    let draft = &mut board.draft;
    ui.horizontal(|ui| {
        ui.label("Kind");
        egui::ComboBox::from_id_salt("project-message-kind")
            .selected_text(
                ["Info", "Question", "Request", "Blocker", "Announcement"]
                    .get(draft.kind)
                    .copied()
                    .unwrap_or("Info"),
            )
            .show_ui(ui, |ui| {
                for (index, kind) in ["Info", "Question", "Request", "Blocker", "Announcement"]
                    .iter()
                    .enumerate()
                {
                    ui.selectable_value(&mut draft.kind, index, *kind);
                }
            })
            .response
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Message kind"));
    });
    ui.add(
        egui::TextEdit::multiline(&mut draft.body)
            .desired_rows(3)
            .hint_text("Message"),
    )
    .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Message body"));
    ui.horizontal(|ui| {
        ui.label("To session");
        ui.text_edit_singleline(&mut draft.to_session)
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "To session"));
        ui.checkbox(&mut draft.to_human, "To you");
    });
    ui.horizontal(|ui| {
        ui.label("Work item");
        ui.text_edit_singleline(&mut draft.item)
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Work item"));
        ui.label("Feature");
        ui.text_edit_singleline(&mut draft.feature)
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Feature"));
    });
    ui.horizontal(|ui| {
        ui.label("Resource");
        ui.text_edit_singleline(&mut draft.resource)
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Resource"));
        ui.checkbox(&mut draft.blocking, "Blocks work");
    });
    ui.horizontal(|ui| {
        ui.label("Reply to message");
        ui.text_edit_singleline(&mut draft.reply_to)
            .widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Reply to message")
            });
        egui::ComboBox::from_id_salt("project-message-resolution")
            .selected_text(
                ["No resolution", "Answered", "Declined"]
                    .get(draft.resolution)
                    .copied()
                    .unwrap_or("No resolution"),
            )
            .show_ui(ui, |ui| {
                for (index, resolution) in
                    ["No resolution", "Answered", "Declined"].iter().enumerate()
                {
                    ui.selectable_value(&mut draft.resolution, index, *resolution);
                }
            })
            .response
            .widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Message resolution")
            });
    });
    if ui
        .add_enabled(
            open && !draft.body.trim().is_empty(),
            egui::Button::new("Post message"),
        )
        .clicked()
    {
        actions.push(Action::PostMessage(draft.clone()));
    }
    if !board.events.is_empty() {
        ui.add_space(10.0);
        ui.heading("Recent message changes");
        for event in board.events.iter().rev().take(25) {
            widgets::caption(
                ui,
                format!(
                    "{} · {} · {}",
                    event.kind.0,
                    event.actor.name.0,
                    event.message_id.as_str()
                ),
            );
        }
    }
}
