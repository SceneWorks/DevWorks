//! Password input is transient and excluded from SettingsMemory and command frames.
use super::*;
use crate::backend::SecretStoreStatus;
use devworks_core::ports::secrets::Secret;

#[derive(Default)]
pub(super) struct Input(String);
impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretInput(<redacted>)")
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        // Reuse the host's best-effort secret buffer clearing.
        drop(Secret::new(std::mem::take(&mut self.0)));
    }
}
pub(super) fn editor(
    ui: &mut Ui,
    input: &mut Input,
    item: &Item,
    saving: bool,
    store: Option<&SecretStoreStatus>,
    actions: &mut Vec<SettingsAction>,
) {
    let d = item.descriptor;
    ui.label(if item.effective.get("secretRef").is_some() {
        "A saved credential is configured."
    } else {
        "No saved credential configured."
    });
    let label = ui.label(format!("Credential for {}", d.label));
    let response = ui
        .add(
            egui::TextEdit::singleline(&mut input.0)
                .password(true)
                .hint_text("Enter a new credential"),
        )
        .labelled_by(label.id);
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(
                !saving && !input.0.trim().is_empty(),
                egui::Button::new(format!("Save credential for {}", d.label)),
            )
            .clicked()
        {
            actions.push(SettingsAction::StoreSecret {
                key: d.key.into(),
                value: Some(Secret::new(std::mem::take(&mut input.0))),
            });
            if let Some(mut state) = egui::text_edit::TextEditState::load(ui.ctx(), response.id) {
                state.set_undoer(Default::default());
                state.store(ui.ctx(), response.id);
            }
        }
        if ui
            .add_enabled(
                !saving && item.shown().get("secretRef").is_some(),
                egui::Button::new(format!("Remove credential for {}", d.label)),
            )
            .clicked()
        {
            // A retained settings revision may still own the old handle for rollback.
            drop(Secret::new(std::mem::take(&mut input.0)));
            actions.push(SettingsAction::StoreSecret {
                key: d.key.into(),
                value: None,
            });
            if let Some(mut state) = egui::text_edit::TextEditState::load(ui.ctx(), response.id) {
                state.set_undoer(Default::default());
                state.store(ui.ctx(), response.id);
            }
        }
    });
    if let Some(store) = store {
        widgets::caption(
            ui,
            format!(
                "Credential store: {}{}",
                store.name,
                if store.session_only {
                    " · this session only"
                } else {
                    ""
                }
            ),
        );
        if let Some(reason) = &store.unavailable {
            ui.colored_label(
                theme::RED,
                format!("Credential store unavailable: {reason}"),
            );
        }
    }
}
