//! Native settings for external clients connecting to DevWorks.

use crate::backend::Backend;
use crate::frame::{self, Negotiate};
use crate::ops;
use crate::widgets;
use devworks_core::operations::{OperationInput, mcp_server as wire};
use devworks_core::protocol::records::{
    ItemState, McpClientAccess, McpClientAccessAction, McpInboundStatus, McpServerBinding,
    PageRequest,
};
use devworks_core::protocol::results::OperationResult;
use devworks_core::protocol::support::{BoundedList, BoundedText, Limit, Port};
use devworks_core::protocol::{Id, ProjectId, Revision};
use eframe::egui::{self, Ui};
use serde_json::json;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub enum Action {
    Reload,
    Save(Box<OperationInput>),
    CopyCredential(Id, Revision),
}

type ClientsPage = devworks_core::protocol::support::Page<McpClientAccess>;

#[derive(Default)]
pub struct Panel {
    pub(crate) status: Option<McpInboundStatus>,
    pub(crate) clients: Vec<McpClientAccess>,
    pub(crate) busy: bool,
    pub(crate) message: Option<String>,
    stdio_paths: Option<(PathBuf, PathBuf)>,
    load_requested: bool,
    enabled: bool,
    stdio: bool,
    http: bool,
    port: String,
    registration: String,
    client: Option<GrantDraft>,
}

struct GrantDraft {
    id: Id,
    revision: Revision,
    enabled: bool,
    name: String,
    projects: String,
    repositories: String,
    worktrees: String,
    reads: String,
    commands: String,
}

fn join<'a>(values: impl IntoIterator<Item = &'a str>) -> String {
    values.into_iter().collect::<Vec<_>>().join(", ")
}

impl From<&McpClientAccess> for GrantDraft {
    fn from(access: &McpClientAccess) -> Self {
        Self {
            id: access.client_id.clone(),
            revision: access.revision.clone(),
            enabled: access.enabled,
            name: access.name.0.clone(),
            projects: join(access.project_ids.0.iter().map(|p| p.0.as_str())),
            repositories: join(access.repository_ids.0.iter().map(Id::as_str)),
            worktrees: join(access.worktree_ids.0.iter().map(Id::as_str)),
            reads: join(access.read_operations.0.iter().map(|r| r.0.as_str())),
            commands: join(access.command_operations.0.iter().map(|r| r.0.as_str())),
        }
    }
}

fn split(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn ids(value: &str) -> Result<Vec<Id>, &'static str> {
    split(value).map(Id::new).collect()
}

impl GrantDraft {
    fn action(&self) -> Result<McpClientAccessAction, &'static str> {
        Ok(McpClientAccessAction::Replace {
            client_id: self.id.clone(),
            expected_revision: self.revision.clone(),
            name: BoundedText(self.name.trim().into()),
            enabled: self.enabled,
            project_ids: BoundedList(ids(&self.projects)?.into_iter().map(ProjectId).collect()),
            repository_ids: BoundedList(ids(&self.repositories)?),
            worktree_ids: BoundedList(ids(&self.worktrees)?),
            read_operations: BoundedList(
                split(&self.reads).map(|r| BoundedText(r.into())).collect(),
            ),
            command_operations: BoundedList(
                split(&self.commands)
                    .map(|r| BoundedText(r.into()))
                    .collect(),
            ),
        })
    }
}

fn save(input: wire::Input) -> Action {
    Action::Save(Box::new(OperationInput::McpServer(input)))
}

fn access(action: McpClientAccessAction) -> Action {
    save(wire::Input::SetClientAccess(wire::SetClientAccessInput {
        action,
    }))
}

impl Panel {
    pub(crate) fn loaded(
        &mut self,
        status: McpInboundStatus,
        clients: Vec<McpClientAccess>,
        stdio_paths: Option<(PathBuf, PathBuf)>,
    ) {
        self.enabled = status.settings.enabled;
        self.stdio = status
            .settings
            .bindings
            .0
            .iter()
            .any(|b| matches!(b, McpServerBinding::Stdio));
        self.http = status
            .settings
            .bindings
            .0
            .iter()
            .any(|b| matches!(b, McpServerBinding::LoopbackHttp));
        self.port = status
            .settings
            .http_port
            .as_ref()
            .map_or_else(String::new, |p| p.get().to_string());
        self.status = Some(status);
        self.clients = clients;
        self.stdio_paths = stdio_paths;
        if let Some(draft) = &self.client {
            self.client = self
                .clients
                .iter()
                .find(|c| c.client_id == draft.id)
                .map(GrantDraft::from);
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, online: bool, actions: &mut Vec<Action>) {
        ui.separator();
        ui.heading("Incoming MCP clients");
        if online && !self.load_requested {
            self.load_requested = true;
            actions.push(Action::Reload);
        }
        if let Some(message) = &self.message {
            ui.label(message);
        }
        ui.add_enabled_ui(online && !self.busy, |ui| {
            if widgets::secondary_button(ui, "Refresh client access").clicked() {
                actions.push(Action::Reload);
            }
            if let Some(status) = &self.status {
                if let Some(diagnostic) = &status.diagnostic {
                    ui.label(&diagnostic.0);
                }
                if let Some(endpoint) = &status.endpoint {
                    ui.label(&endpoint.0);
                }
                ui.checkbox(&mut self.enabled, "Enable incoming MCP");
                ui.checkbox(&mut self.stdio, "Local stdio bridge");
                ui.checkbox(&mut self.http, "Loopback HTTP");
                if self.http {
                    text(ui, "HTTP port", &mut self.port);
                }
                ui.horizontal(|ui| {
                    if widgets::secondary_button(ui, "Save listener settings").clicked() {
                        let mut bindings = Vec::new();
                        if self.stdio {
                            bindings.push(McpServerBinding::Stdio);
                        }
                        if self.http {
                            bindings.push(McpServerBinding::LoopbackHttp);
                        }
                        let port = self.port.trim().parse::<u32>().ok().and_then(Port::new);
                        if self.http && port.is_none() {
                            self.message = Some("Enter an HTTP port from 1 to 65535.".into());
                        } else {
                            actions.push(save(wire::Input::Configure(wire::ConfigureInput {
                                expected_revision: status.settings.revision.clone(),
                                enabled: self.enabled,
                                bindings: BoundedList(bindings),
                                http_port: if self.http { port } else { None },
                            })));
                        }
                    }
                    if widgets::secondary_button(ui, "Copy setup").clicked() {
                        ui.ctx().copy_text(setup(status, self.stdio_paths.as_ref()));
                    }
                });
            }

            ui.horizontal(|ui| {
                let label = ui.label("New client name");
                ui.text_edit_singleline(&mut self.registration)
                    .labelled_by(label.id);
                if ui
                    .add_enabled(
                        !self.registration.trim().is_empty(),
                        egui::Button::new("Register"),
                    )
                    .clicked()
                {
                    actions.push(access(McpClientAccessAction::Register {
                        name: BoundedText(self.registration.trim().into()),
                    }));
                    self.registration.clear();
                }
            });
            for client in &self.clients {
                ui.horizontal(|ui| {
                    ui.label(&client.name.0);
                    ui.label(if client.enabled {
                        "Enabled"
                    } else {
                        "Disabled"
                    });
                    if widgets::secondary_button(ui, "Edit access").clicked() {
                        self.client = Some(client.into());
                    }
                });
            }
            if let Some(draft) = &mut self.client {
                ui.separator();
                text(ui, "Client name", &mut draft.name);
                ui.checkbox(&mut draft.enabled, "Enable this client");
                ui.label("Separate exact IDs or operation names with commas.");
                text(ui, "Project IDs", &mut draft.projects);
                text(ui, "Repository IDs", &mut draft.repositories);
                text(ui, "Worktree IDs", &mut draft.worktrees);
                text(ui, "Read operations", &mut draft.reads);
                text(ui, "Command operations", &mut draft.commands);
                ui.horizontal_wrapped(|ui| {
                    if widgets::secondary_button(ui, "Save client access").clicked() {
                        match draft.action() {
                            Ok(action) => actions.push(access(action)),
                            Err(_) => self.message = Some("Enter valid scope IDs.".into()),
                        }
                    }
                    if widgets::secondary_button(ui, "Copy pairing credential").clicked() {
                        actions.push(Action::CopyCredential(
                            draft.id.clone(),
                            draft.revision.clone(),
                        ));
                    }
                    if widgets::secondary_button(ui, "Rotate credential").clicked() {
                        actions.push(access(McpClientAccessAction::RotateCredential {
                            client_id: draft.id.clone(),
                            expected_revision: draft.revision.clone(),
                        }));
                    }
                    if widgets::secondary_button(ui, "Revoke client").clicked() {
                        actions.push(access(McpClientAccessAction::Revoke {
                            client_id: draft.id.clone(),
                            expected_revision: draft.revision.clone(),
                        }));
                    }
                });
            }
        });
    }
}

fn text(ui: &mut Ui, label: &str, buffer: &mut String) {
    ui.horizontal(|ui| {
        let label = ui.label(label);
        ui.text_edit_singleline(buffer).labelled_by(label.id);
    });
}

/// Structural setup information only. A separately provisioned local credential is never
/// accepted by this function. Protocol versions come from actual host status.
fn setup(status: &McpInboundStatus, stdio_paths: Option<&(PathBuf, PathBuf)>) -> String {
    let mut value = json!({
        "available": matches!(status.state, ItemState::Enabled),
        "supportedProtocolVersions": status.protocol_versions.0,
    });
    for binding in &status.settings.bindings.0 {
        match binding {
            McpServerBinding::Stdio => {
                let (command, args) = match stdio_paths
                    .and_then(|(exe, root)| Some((exe.to_str()?, root.to_str()?)))
                {
                    Some((exe, root)) => (exe, vec!["mcp", "--data-dir", root]),
                    None => (
                        "<installed DevWorks executable>",
                        vec!["mcp", "--data-dir", "<active DevWorks data directory>"],
                    ),
                };
                value["stdio"] = json!({
                    "command": command,
                    "args": args,
                    "environment": {"DEVWORKS_MCP_TOKEN": "<protected credential reference>"},
                });
            }
            McpServerBinding::LoopbackHttp => {
                let endpoint = status.endpoint.as_ref().map(|endpoint| endpoint.0.clone());
                value["http"] = json!({
                    "url": endpoint,
                    "configuredPort": status.settings.http_port.as_ref().map(Port::get),
                    "authentication": "Manual local pairing",
                    "headers": {"Authorization": "Bearer <pairing credential>"},
                });
            }
        }
    }
    serde_json::to_string_pretty(&value).expect("JSON setup")
}

fn query(backend: &dyn Backend, input: wire::Input) -> Result<OperationResult, String> {
    let input = OperationInput::McpServer(input);
    let operation = input.operation();
    let frame = frame::build(backend, Negotiate::Latest, Some(input));
    ops::query_result(backend.query(operation, &frame))
        .map(|result| result.0)
        .map_err(|failure| failure.describe())
}

pub(crate) fn load(
    backend: &dyn Backend,
) -> Result<(McpInboundStatus, Vec<McpClientAccess>), String> {
    let OperationResult::McpServerStatus { value: status } =
        query(backend, wire::Input::Status(wire::StatusInput {}))?
    else {
        return Err("Could not read incoming MCP status.".into());
    };
    let mut clients = Vec::new();
    let mut cursor = None;
    loop {
        let input = wire::Input::Clients(wire::ClientsInput {
            page: PageRequest {
                limit: Limit::new(128).expect("valid limit"),
                cursor,
            },
        });
        let OperationResult::McpServerClients {
            value: ClientsPage {
                items, next_cursor, ..
            },
        } = query(backend, input)?
        else {
            return Err("Could not read incoming MCP clients.".into());
        };
        clients.extend(items.0);
        if next_cursor.is_none() {
            return Ok((status, clients));
        }
        cursor = next_cursor;
    }
}
