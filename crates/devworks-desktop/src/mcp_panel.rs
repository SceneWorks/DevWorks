//! Outgoing MCP configuration and effective tool selection in Settings > Agent tools.
//! All mutations go through the same domain gateway used by agents.
use devworks_core::ports::secrets::Secret;
use devworks_core::protocol::Id;
use devworks_core::protocol::records::{McpCapability, McpServer};
use eframe::egui::{self, Ui};
use serde_json::{Value, json};
use std::time::Instant;

#[derive(Clone, Debug, PartialEq)]
pub enum McpAction {
    Reload,
    Catalog(Id, i64),
    Configure { body: Value, secret: Option<Secret> },
    Connect(Id, i64),
    Discover(Id, i64),
    Select(Id, i64, i64, Vec<Id>),
}

#[derive(Default)]
pub struct McpPanel {
    pub servers: Vec<McpServer>,
    pub capabilities: Vec<McpCapability>,
    pub selected: Option<Id>,
    pub loading: bool,
    pub busy: bool,
    pub error: Option<String>,
    pub message: Option<String>,
    pub pending_server: Option<Id>,
    pub last_refresh: Option<Instant>,
    pub name: String,
    pub endpoint: String,
    pub credential_ref: String,
    pub credential: String,
    pub stdio: bool,
    pub process_spec: String,
    pub editing: bool,
    selection: Vec<Id>,
}

impl McpPanel {
    pub fn loaded_servers(&mut self, servers: Vec<McpServer>, append: bool, complete: bool) {
        self.error = None;
        if append {
            self.servers.extend(servers);
        } else {
            self.servers = servers;
        }
        if complete
            && let Some(selected) = &self.selected
            && !self.servers.iter().any(|server| &server.id == selected)
        {
            self.selected = None;
            self.capabilities.clear();
            self.selection.clear();
        }
    }

    pub fn loaded_catalog(&mut self, server: &Id, capabilities: Vec<McpCapability>, append: bool) {
        if self.selected.as_ref() != Some(server) {
            return;
        }
        if append {
            self.capabilities.extend(capabilities);
        } else {
            self.capabilities = capabilities;
        }
        if !append {
            self.selection = self
                .servers
                .iter()
                .find(|record| &record.id == server)
                .map(|record| record.selected_tool_ids.0.clone())
                .unwrap_or_default();
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, online: bool, actions: &mut Vec<McpAction>) {
        ui.add_space(12.0);
        ui.heading("External MCP servers");
        ui.label("Choose the tools agents can use. Read-only tools start selected after discovery; other tools need an explicit selection.");
        if ui.button("Refresh MCP servers").clicked() {
            self.loading = true;
            actions.push(McpAction::Reload);
        }
        if self.loading {
            ui.spinner();
        }
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        if let Some(message) = &self.message {
            ui.label(message);
        }
        let chosen = self
            .selected
            .as_ref()
            .and_then(|id| self.servers.iter().find(|server| &server.id == id));
        egui::ComboBox::from_id_salt("mcp-server")
            .selected_text(
                chosen.map_or("Choose a server".to_owned(), |server| server.name.0.clone()),
            )
            .show_ui(ui, |ui| {
                for server in &self.servers {
                    if ui
                        .selectable_label(
                            self.selected.as_ref() == Some(&server.id),
                            &server.name.0,
                        )
                        .clicked()
                    {
                        self.selected = Some(server.id.clone());
                        self.capabilities.clear();
                        self.selection = server.selected_tool_ids.0.clone();
                        actions.push(McpAction::Catalog(
                            server.id.clone(),
                            server.catalog_revision.get(),
                        ));
                    }
                }
            })
            .response
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "MCP server"));
        if let Some(server) = self
            .selected
            .as_ref()
            .and_then(|id| self.servers.iter().find(|record| &record.id == id))
        {
            let server_id = server.id.clone();
            let server_idle = self.pending_server.as_ref() != Some(&server_id);
            let revision = server.revision.get();
            let catalog_revision = server.catalog_revision.get();
            ui.label(format!(
                "Status: {:?} · Transport: {} · Catalog revision: {}",
                server.state, server.transport.0, catalog_revision
            ));
            if !server_idle {
                ui.label("MCP connection or discovery is running…");
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        online && !self.busy && server_idle,
                        egui::Button::new("Connect or reconnect"),
                    )
                    .clicked()
                {
                    self.busy = true;
                    actions.push(McpAction::Connect(server_id.clone(), revision));
                }
                if ui
                    .add_enabled(
                        online && !self.busy && server_idle,
                        egui::Button::new("Discover capabilities"),
                    )
                    .clicked()
                {
                    self.busy = true;
                    actions.push(McpAction::Discover(server_id.clone(), catalog_revision));
                }
                if ui.button("Refresh capabilities").clicked() {
                    actions.push(McpAction::Catalog(server_id.clone(), catalog_revision));
                }
            });
            let tools: Vec<_> = self
                .capabilities
                .iter()
                .filter(|cap| cap.kind.0 == "tool")
                .collect();
            if !tools.is_empty() {
                ui.horizontal(|ui| {
                    if ui.button("Select all").clicked() {
                        self.selection = tools.iter().map(|cap| cap.id.clone()).collect();
                    }
                    if ui.button("Deselect all").clicked() {
                        self.selection.clear();
                    }
                });
                for cap in tools {
                    let mut checked = self.selection.contains(&cap.id);
                    let label = format!(
                        "{}{}",
                        cap.name.0,
                        if cap.read_only {
                            " · read-only hint"
                        } else {
                            ""
                        }
                    );
                    if ui
                        .checkbox(&mut checked, label)
                        .on_hover_text(&cap.description.0)
                        .changed()
                    {
                        if checked {
                            self.selection.push(cap.id.clone());
                        } else {
                            self.selection.retain(|id| id != &cap.id);
                        }
                    }
                }
                if ui
                    .add_enabled(
                        online && !self.busy && server_idle,
                        egui::Button::new("Save tool selection"),
                    )
                    .clicked()
                {
                    self.busy = true;
                    actions.push(McpAction::Select(
                        server_id,
                        revision,
                        catalog_revision,
                        self.selection.clone(),
                    ));
                }
            }
        }
        ui.separator();
        ui.checkbox(&mut self.editing, "Update selected server");
        ui.label("Server name");
        ui.text_edit_singleline(&mut self.name).widget_info(|| {
            egui::WidgetInfo::labeled(egui::Role::TextInput, true, "MCP server name")
        });
        ui.checkbox(&mut self.stdio, "Launch a registered local process (stdio)");
        if self.stdio {
            ui.label("Process spec JSON: registered executable, working root, arguments, environment, profile and limits");
            ui.add(egui::TextEdit::multiline(&mut self.process_spec).desired_rows(5))
                .widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::TextInput, true, "MCP process spec JSON")
                });
        } else {
            ui.label("HTTPS MCP endpoint");
            ui.text_edit_singleline(&mut self.endpoint).widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::TextInput, true, "MCP HTTPS endpoint")
            });
            ui.label("OS secret reference (optional)");
            ui.text_edit_singleline(&mut self.credential_ref)
                .widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::TextInput, true, "MCP secret reference")
                });
            ui.label("New bearer token (optional; stored in the OS secret store)");
            ui.add(egui::TextEdit::singleline(&mut self.credential).password(true))
                .widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::Role::PasswordInput,
                        true,
                        "New MCP bearer token",
                    )
                });
        }
        if ui
            .add_enabled(
                online && !self.busy && !self.name.trim().is_empty(),
                egui::Button::new(if self.editing {
                    "Update server"
                } else {
                    "Add server"
                }),
            )
            .clicked()
        {
            let transport = if self.stdio {
                match serde_json::from_str::<Value>(&self.process_spec) {
                    Ok(process) => json!({"kind":"Stdio","process":process}),
                    Err(error) => {
                        self.error = Some(format!("Invalid process spec JSON: {error}"));
                        return;
                    }
                }
            } else {
                let mut transport = json!({"kind":"Http","endpoint":self.endpoint.trim()});
                if !self.credential_ref.trim().is_empty() {
                    transport["credentialRef"] = json!(self.credential_ref.trim());
                }
                transport
            };
            let mut body =
                json!({"scope":{"kind":"Workspace"},"name":self.name.trim(),"transport":transport});
            if self.editing {
                let Some(server) = self
                    .selected
                    .as_ref()
                    .and_then(|id| self.servers.iter().find(|record| &record.id == id))
                else {
                    self.error = Some("Choose a server to update.".into());
                    return;
                };
                body["serverId"] = json!(server.id);
                body["expectedRevision"] = json!(server.revision);
            }
            self.busy = true;
            self.error = None;
            let secret = (!self.stdio && !self.credential.is_empty())
                .then(|| Secret::new(std::mem::take(&mut self.credential)));
            actions.push(McpAction::Configure { body, secret });
        }
    }
}
