//! Native password input is staged off the frame thread; only its reference reaches admission.
use super::*;
use devworks_core::ports::secrets::Secret;
use devworks_core::settings::catalog::ValueKind;
use serde_json::{Value, json};

impl Shell {
    pub(super) fn store_setting_secret(&mut self, key: &str, value: Option<Secret>) {
        if self.settings.saving || self.online().is_err() {
            return;
        }
        let Some(mut captured) = self.settings.view.clone() else {
            return;
        };
        let Some(item) = captured.item(key) else {
            return;
        };
        if !item.editable || item.descriptor.kind != ValueKind::SecretRef {
            return;
        }
        let key = item.descriptor.key;
        // This gesture saves only this key, with the revision captured before native intake.
        // Other settings drafts stay local and are preserved by the normal save/read path.
        captured.discard_all();
        let submitted = self.client.as_mut().is_some_and(|client| {
            client.task(
                Slot::SettingsSecret(key.into()),
                "Saving credential",
                move |backend| {
                    let value = match value {
                        Some(secret) => json!({"secretRef": backend.stage_setting_secret(secret)?}),
                        None => Value::Null,
                    };
                    captured
                        .edit(key, value.clone())
                        .map_err(|_| "This credential setting cannot be edited.".to_owned())?;
                    if captured.has_blocking_issues() {
                        return Err("The credential store returned an invalid reference.".into());
                    }
                    let frame = ops::settings_submit(backend, &captured, false)
                        .ok_or_else(|| "No credential change was prepared.".to_owned())?;
                    Ok(TaskOutput::SettingSecret {
                        frame,
                        key: key.into(),
                        value,
                    })
                },
            )
        });
        if submitted {
            self.settings.saving = true;
            self.settings.secret_intake = Some(key.into());
            self.settings.message = None;
        } else {
            self.settings.message =
                Some((Tone::Bad, "The credential change was not submitted.".into()));
        }
    }

    pub(super) fn setting_secret_prepared(
        &mut self,
        selected: &str,
        result: Result<TaskOutput, String>,
    ) {
        self.settings.secret_intake = None;
        if let Some(backend) = self.backend() {
            self.settings.connections.secret_store = Some(backend.secret_store());
        }
        match result {
            Ok(TaskOutput::SettingSecret { frame, key, value }) if key == selected => {
                // Retain the opaque reference on refusal or uncertainty, never stage the raw
                // credential again to retry this command. Admission still uses captured CAS.
                if let Some(view) = &mut self.settings.view {
                    let _ = view.edit(&key, value.clone());
                }
                self.settings.submitted = BTreeMap::from([(key, value)]);
                if let Some(client) = &mut self.client {
                    // The client owns exact prepared frames even during close/recovery.
                    client.command(
                        Slot::SettingsSave,
                        "Saving credential setting",
                        "settings.save_revision",
                        frame,
                    );
                }
            }
            error => {
                self.settings.saving = false;
                self.settings.message = Some((
                    Tone::Bad,
                    match error {
                        Err(error) => format!("Credential was not saved: {error}"),
                        Ok(_) => "Credential was not saved: unexpected preparation reply.".into(),
                    },
                ));
            }
        }
    }
}
