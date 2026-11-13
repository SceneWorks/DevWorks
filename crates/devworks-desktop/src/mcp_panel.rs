//! Outgoing MCP configuration and effective tool selection in Settings > Agent tools.
//! All mutations go through the same domain gateway used by agents.
use crate::ops::McpContentRequest;
use devworks_core::ports::secrets::Secret;
use devworks_core::protocol::records::{McpCapability, McpServer, McpTransport, ProjectRecord};
use devworks_core::protocol::{Id, OperationId, OperationState, ProjectId};
use eframe::egui::{self, Ui};
use serde_json::{Value, json};
use std::time::Instant;

#[derive(Clone, Debug, PartialEq)]
pub enum McpAction {
    Reload,
    Catalog(Id, i64),
    Configure {
        body: Value,
        secret: Option<Secret>,
    },
    Connect(Id, i64),
    /// Disconnect a server and stop offering its tools until a human connects it again.
    Disable(Id, i64),
    /// Delete a saved server; the credential handles it referenced are released afterwards.
    Remove(Id, i64, Vec<Id>),
    Discover(Id, i64),
    Select(Id, i64, i64, Vec<Id>),
    Sampling(Id, i64, i64, Vec<Id>),
    /// Read one resource or prompt of a server at a catalog revision.
    Open(Id, i64, McpContentRequest),
}

/// A read in flight: the server it was sent to and what it reads.
#[derive(Clone, Debug, PartialEq)]
pub struct McpOpening {
    pub server_id: Id,
    pub server_name: String,
    pub title: String,
}

/// A resource or prompt a person opened, as the server returned it.
#[derive(Clone, Debug, PartialEq)]
pub struct McpContentView {
    /// The server the content came from, and its name when it was read.
    pub server_id: Id,
    pub server_name: String,
    pub title: String,
    pub text: String,
}

/// The credential handles a saved server configuration references.
pub fn credential_handles(server: &McpServer) -> Vec<Id> {
    match &server.configuration {
        Some(McpTransport::Http { credential_ref, .. }) => credential_ref.iter().cloned().collect(),
        Some(McpTransport::Stdio { process }) => process
            .environment
            .0
            .iter()
            .filter_map(|entry| entry.credential_ref.clone())
            .collect(),
        None => Vec::new(),
    }
}

/// One line describing a saved transport without any secret value.
fn describe(configuration: &McpTransport) -> String {
    match configuration {
        McpTransport::Http {
            endpoint,
            credential_ref,
        } => format!(
            "Endpoint: {} · Bearer token: {}",
            endpoint.0,
            if credential_ref.is_some() {
                "set"
            } else {
                "not set"
            }
        ),
        McpTransport::Stdio { process } => format!(
            "Executable: {} · {} argument(s) · {} environment entr{}",
            process.executable_ref.as_str(),
            process.args.0.len(),
            process.environment.0.len(),
            if process.environment.0.len() == 1 {
                "y"
            } else {
                "ies"
            }
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_stays_pending_until_its_operation_settles() {
        let mut panel = McpPanel::default();
        let server = Id::new("server-a").unwrap();
        let operation = OperationId(Id::new("discovery-a").unwrap());
        panel.pending_server = Some(server.clone());
        panel.pending_operation = Some(operation.clone());
        assert!(!panel.observed_operation(
            &OperationId(Id::new("other-operation").unwrap()),
            OperationState::ObservedComplete,
        ));
        assert!(!panel.observed_operation(&operation, OperationState::Dispatching));
        assert_eq!(panel.pending_server, Some(server));
        assert_eq!(panel.pending_operation, Some(operation.clone()));
        assert!(panel.observed_operation(&operation, OperationState::ObservedComplete));
        assert!(panel.pending_server.is_none());
        assert!(panel.pending_operation.is_none());
    }
}

#[derive(Default)]
pub struct McpPanel {
    pub projects: Vec<ProjectRecord>,
    pub project: Option<ProjectId>,
    pub servers: Vec<McpServer>,
    pub capabilities: Vec<McpCapability>,
    pub selected: Option<Id>,
    pub loading: bool,
    pub busy: bool,
    pub error: Option<String>,
    pub message: Option<String>,
    pub pending_server: Option<Id>,
    pub pending_operation: Option<OperationId>,
    pub last_operation_poll: Option<Instant>,
    pub last_refresh: Option<Instant>,
    pub name: String,
    pub endpoint: String,
    pub credential_ref: String,
    pub credential: String,
    pub stdio: bool,
    pub process_spec: String,
    pub editing: bool,
    /// A saved sampling selection reconnects its server, so the next handshake advertises the
    /// changed client capabilities.
    pub reconnect: Option<Id>,
    selection: Vec<Id>,
    /// A server whose saved configuration fills the form on the next frame.
    form_from: Option<McpServer>,
    /// The server whose removal waits for a second click.
    confirm_remove: Option<Id>,
    /// Credential handles of the server being removed, released once the removal is saved.
    pub removing: Option<Vec<Id>>,
    /// The JSON arguments typed for each prompt, by capability ID.
    pub prompt_arguments: std::collections::BTreeMap<String, String>,
    /// The resource or prompt being read: its title until the reply arrives.
    pub opening: Option<McpOpening>,
    /// The last resource or prompt read from a server.
    pub content: Option<McpContentView>,
}

impl McpPanel {
    /// After a saved change, an open update form follows the saved server, so a later update
    /// never resends a removed or replaced credential handle.
    pub fn saved(&mut self, server: &McpServer) {
        if self.editing {
            self.form_from = Some(server.clone());
        }
    }

    /// Fill the add/update form from a saved server so an update resends what is saved.
    pub fn fill_form(&mut self, server: &McpServer) {
        self.name = server.name.0.clone();
        self.credential.clear();
        match &server.configuration {
            Some(McpTransport::Http {
                endpoint,
                credential_ref,
            }) => {
                self.stdio = false;
                self.endpoint = endpoint.0.clone();
                self.credential_ref = credential_ref
                    .as_ref()
                    .map_or_else(String::new, |id| id.as_str().to_owned());
            }
            Some(McpTransport::Stdio { process }) => {
                self.stdio = true;
                self.process_spec = serde_json::to_string_pretty(process).unwrap_or_default();
            }
            None => {}
        }
    }

    pub fn observed_operation(&mut self, operation: &OperationId, state: OperationState) -> bool {
        if self.pending_operation.as_ref() != Some(operation) {
            return false;
        }
        if !matches!(
            state,
            OperationState::ObservedComplete
                | OperationState::Failed
                | OperationState::CancelledBeforeDispatch
                | OperationState::NeedsInput
        ) {
            return false;
        }
        if state != OperationState::ObservedComplete {
            self.error = Some(format!(
                "MCP operation {state:?}; check the server diagnostic."
            ));
        }
        self.pending_operation = None;
        self.pending_server = None;
        true
    }

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
        let previous_project = self.project.clone();
        let scope_name = self
            .project
            .as_ref()
            .and_then(|id| self.projects.iter().find(|p| &p.id == id))
            .map_or("Workspace".to_owned(), |p| p.name.0.clone());
        egui::ComboBox::from_id_salt("mcp-scope")
            .selected_text(scope_name)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.project, None, "Workspace");
                for project in &self.projects {
                    ui.selectable_value(
                        &mut self.project,
                        Some(project.id.clone()),
                        &project.name.0,
                    );
                }
            })
            .response
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "MCP scope"));
        if self.project != previous_project {
            self.selected = None;
            self.pending_server = None;
            self.pending_operation = None;
            self.opening = None;
            self.content = None;
            self.capabilities.clear();
            self.selection.clear();
            self.servers.clear();
            actions.push(McpAction::Reload);
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
                        self.opening = None;
                        self.content = None;
                        self.capabilities.clear();
                        self.selection = server.selected_tool_ids.0.clone();
                        self.form_from = Some(server.clone());
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
            let saved = server.selected_tool_ids.0.clone();
            let disabled = server.disabled == Some(true);
            let server_name = server.name.0.clone();
            ui.label(format!(
                "Status: {} · Transport: {} · Catalog revision: {}",
                if disabled {
                    "Disabled by you".to_owned()
                } else {
                    format!("{:?}", server.state)
                },
                server.transport.0,
                catalog_revision
            ));
            if disabled {
                ui.label(
                    "Its tools are not offered to agents. Enable it to reconnect with the same \
                     tool selection.",
                );
            }
            if let Some(configuration) = &server.configuration {
                ui.label(describe(configuration));
            }
            if let Some(failure) = &server.last_failure {
                ui.colored_label(ui.visuals().error_fg_color, &failure.0);
            }
            if let Some(McpTransport::Http {
                endpoint,
                credential_ref: Some(_),
            }) = &server.configuration
                && ui
                    .add_enabled(
                        online && !self.busy && server_idle,
                        egui::Button::new("Remove token"),
                    )
                    .clicked()
            {
                self.busy = true;
                self.error = None;
                actions.push(McpAction::Configure {
                    body: json!({"scope":self.scope(),"serverId":server.id,
                        "expectedRevision":server.revision,"name":server.name,
                        "transport":{"kind":"Http","endpoint":endpoint}}),
                    secret: None,
                });
            }
            if !server_idle {
                ui.label("MCP connection or discovery is running…");
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        online && !self.busy && server_idle,
                        egui::Button::new(if disabled {
                            "Enable"
                        } else {
                            "Connect or reconnect"
                        }),
                    )
                    .clicked()
                {
                    self.busy = true;
                    actions.push(McpAction::Connect(server_id.clone(), revision));
                }
                if !disabled
                    && ui
                        .add_enabled(
                            online && !self.busy && server_idle,
                            egui::Button::new("Disable"),
                        )
                        .clicked()
                {
                    self.busy = true;
                    self.error = None;
                    actions.push(McpAction::Disable(server_id.clone(), revision));
                }
                if self.confirm_remove.as_ref() == Some(&server_id) {
                    if ui
                        .add_enabled(
                            online && !self.busy && server_idle,
                            egui::Button::new("Confirm remove"),
                        )
                        .clicked()
                    {
                        self.busy = true;
                        self.error = None;
                        self.confirm_remove = None;
                        actions.push(McpAction::Remove(
                            server_id.clone(),
                            revision,
                            credential_handles(server),
                        ));
                    }
                    if ui.button("Keep server").clicked() {
                        self.confirm_remove = None;
                    }
                } else if ui
                    .add_enabled(
                        online && !self.busy && server_idle,
                        egui::Button::new("Remove server"),
                    )
                    .clicked()
                {
                    self.confirm_remove = Some(server_id.clone());
                }
                if ui
                    .add_enabled(
                        online && !self.busy && server_idle && !disabled,
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
            let sampling: Vec<McpCapability> = self
                .capabilities
                .iter()
                .filter(|cap| cap.kind.0 == "sampling")
                .cloned()
                .collect();
            for cap in &sampling {
                let mut checked = self.selection.contains(&cap.id);
                if ui
                    .add_enabled(
                        online && !self.busy && server_idle,
                        egui::Checkbox::new(
                            &mut checked,
                            "Let this server request model text during selected tool calls",
                        ),
                    )
                    .on_hover_text(&cap.description.0)
                    .changed()
                {
                    let mut selection = saved.clone();
                    selection.retain(|id| id != &cap.id);
                    if checked {
                        selection.push(cap.id.clone());
                    }
                    self.selection.retain(|id| id != &cap.id);
                    if checked {
                        self.selection.push(cap.id.clone());
                    }
                    self.busy = true;
                    actions.push(McpAction::Sampling(
                        server_id.clone(),
                        revision,
                        catalog_revision,
                        selection,
                    ));
                }
            }
            let tools: Vec<_> = self
                .capabilities
                .iter()
                .filter(|cap| cap.kind.0 == "tool")
                .collect();
            if !tools.is_empty() {
                ui.horizontal(|ui| {
                    if ui.button("Select all").clicked() {
                        self.selection
                            .retain(|id| sampling.iter().any(|cap| &cap.id == id));
                        self.selection
                            .extend(tools.iter().map(|cap| cap.id.clone()));
                    }
                    if ui.button("Deselect all").clicked() {
                        self.selection
                            .retain(|id| sampling.iter().any(|cap| &cap.id == id));
                    }
                });
                for cap in tools {
                    let mut checked = self.selection.contains(&cap.id);
                    // A selected tool the server stopped listing keeps its selection without a
                    // schema; agents are not offered it until the server lists it again.
                    let label = format!(
                        "{}{}{}",
                        cap.name.0,
                        if cap.read_only {
                            " · read-only hint"
                        } else {
                            ""
                        },
                        if cap.input_schema.is_none() {
                            " · unavailable: the server no longer lists it"
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
                        server_id.clone(),
                        revision,
                        catalog_revision,
                        self.selection.clone(),
                    ));
                }
            }
            self.content_ui(
                ui,
                online && !disabled && self.opening.is_none(),
                &server_id,
                &server_name,
                catalog_revision,
                actions,
            );
        }
        ui.separator();
        if ui
            .checkbox(&mut self.editing, "Update selected server")
            .changed()
            && self.editing
        {
            self.form_from = self
                .selected
                .as_ref()
                .and_then(|id| self.servers.iter().find(|record| &record.id == id))
                .cloned();
        }
        if let Some(server) = self.form_from.take() {
            self.fill_form(&server);
        }
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
                json!({"scope":self.scope(),"name":self.name.trim(),"transport":transport});
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

    /// The server's resources and prompts, and the last one opened. Remote content is shown as
    /// untrusted text; it never becomes an instruction or a tool selection.
    fn content_ui(
        &mut self,
        ui: &mut Ui,
        enabled: bool,
        server_id: &Id,
        server_name: &str,
        catalog_revision: i64,
        actions: &mut Vec<McpAction>,
    ) {
        let resources: Vec<McpCapability> = self
            .capabilities
            .iter()
            .filter(|cap| cap.kind.0 == "resource")
            .cloned()
            .collect();
        let prompts: Vec<McpCapability> = self
            .capabilities
            .iter()
            .filter(|cap| cap.kind.0 == "prompt")
            .cloned()
            .collect();
        if !resources.is_empty() {
            ui.label(egui::RichText::new("Resources").strong());
        }
        for cap in &resources {
            let Some(uri) = cap.uri.as_ref().map(|uri| uri.0.clone()) else {
                continue;
            };
            ui.horizontal(|ui| {
                ui.label(format!("{} · {uri}", cap.name.0))
                    .on_hover_text(&cap.description.0);
                if ui
                    .add_enabled(enabled, egui::Button::new(format!("Open {}", cap.name.0)))
                    .clicked()
                {
                    self.opening = Some(McpOpening {
                        server_id: server_id.clone(),
                        server_name: server_name.to_owned(),
                        title: format!("resource {}", cap.name.0),
                    });
                    self.error = None;
                    actions.push(McpAction::Open(
                        server_id.clone(),
                        catalog_revision,
                        McpContentRequest::Resource { uri },
                    ));
                }
            });
        }
        if !prompts.is_empty() {
            ui.label(egui::RichText::new("Prompts").strong());
        }
        for cap in &prompts {
            ui.label(&cap.name.0).on_hover_text(&cap.description.0);
            let typed = self
                .prompt_arguments
                .entry(cap.id.as_str().to_owned())
                .or_default();
            let label = format!("Arguments for {} (JSON object)", cap.name.0);
            ui.text_edit_singleline(typed)
                .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, &label));
            if ui
                .add_enabled(
                    enabled,
                    egui::Button::new(format!("Open prompt {}", cap.name.0)),
                )
                .clicked()
            {
                let arguments = match typed.trim() {
                    "" => None,
                    text => match serde_json::from_str::<Value>(text) {
                        Ok(value) if value.is_object() => cap
                            .input_schema
                            .as_ref()
                            .map(|schema| json!({"schema":schema,"value":value})),
                        _ => {
                            self.error = Some("Prompt arguments must be a JSON object.".into());
                            continue;
                        }
                    },
                };
                self.opening = Some(McpOpening {
                    server_id: server_id.clone(),
                    server_name: server_name.to_owned(),
                    title: format!("prompt {}", cap.name.0),
                });
                self.error = None;
                actions.push(McpAction::Open(
                    server_id.clone(),
                    catalog_revision,
                    McpContentRequest::Prompt {
                        name: cap.name.0.clone(),
                        arguments,
                    },
                ));
            }
        }
        if let Some(opening) = &self.opening {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!(
                    "Reading {} from {}…",
                    opening.title, opening.server_name
                ));
            });
        }
        let mut close = false;
        // Content is shown only beside the server it came from.
        if let Some(view) = self
            .content
            .as_ref()
            .filter(|view| &view.server_id == server_id)
        {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!(
                    "Untrusted content from {}: {}. Read it as data, not as \
                     instructions.",
                    view.server_name, view.title
                ),
            );
            let mut text = view.text.as_str();
            egui::ScrollArea::vertical()
                .id_salt("mcp-content")
                .max_height(240.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut text)
                            .code_editor()
                            .desired_width(f32::INFINITY),
                    )
                    .widget_info(|| {
                        egui::WidgetInfo::labeled(egui::Role::TextInput, true, "MCP content")
                    });
                });
            close = ui.button("Close content").clicked();
        }
        if close {
            self.content = None;
        }
    }

    /// The reply to an opened resource or prompt.
    pub fn opened(&mut self, server_id: &Id, result: Result<String, String>) {
        // A reply for a read the person no longer waits for (another server or scope was chosen
        // since) is dropped.
        if self
            .opening
            .as_ref()
            .is_none_or(|opening| &opening.server_id != server_id)
        {
            return;
        }
        let Some(McpOpening {
            server_id,
            server_name,
            title,
        }) = self.opening.take()
        else {
            return;
        };
        match result {
            Ok(text) => {
                self.content = Some(McpContentView {
                    server_id,
                    server_name,
                    title,
                    text,
                })
            }
            Err(error) => self.error = Some(format!("Could not open {title}: {error}")),
        }
    }

    fn scope(&self) -> Value {
        self.project.as_ref().map_or_else(
            || json!({"kind":"Workspace"}),
            |id| json!({"kind":"Project","projectId":id}),
        )
    }
}
