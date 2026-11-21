use super::{Action, DatabasesScreen, button_enabled};
use devworks_core::operations::{OperationInput, database};
use devworks_core::protocol::records::{
    DatabaseSensitiveRuleSet, DbReadResult, DbScalar, SchemaConnection, SchemaExposureMode,
};
use devworks_core::protocol::{OperationId, OperationState};
use eframe::egui::{self, Ui};
use serde_json::{Value, json};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Read,
    Write,
    Direct,
    Migration,
}

#[derive(Default)]
pub struct DataScreen {
    pub query_generation: u64,
    pub result: Option<DbReadResult>,
    pub operation: Option<OperationId>,
    pub operation_state: Option<OperationState>,
    pub rules: Option<DatabaseSensitiveRuleSet>,
    mode: Mode,
    connection: Option<String>,
    statement: String,
    params: String,
    reason: String,
    migration: String,
    provenance: String,
    guard_digest: bool,
    confirm_write: bool,
    workspace_rules: bool,
    rule_id: String,
    rule_catalog: String,
    rule_schema: String,
    rule_table: String,
    rule_column: String,
    rule_reason: String,
    rule_allow: bool,
}

fn typed<T: serde::de::DeserializeOwned>(
    body: Value,
    input: impl FnOnce(T) -> database::Input,
) -> Result<OperationInput, String> {
    serde_json::from_value(body)
        .map(input)
        .map(OperationInput::Database)
        .map_err(|error| error.to_string())
}

impl DataScreen {
    fn frame(
        &self,
        source: &SchemaConnection,
        project: Option<&str>,
    ) -> Result<OperationInput, String> {
        let params = if self.params.trim().is_empty() {
            json!([])
        } else {
            serde_json::from_str(&self.params)
                .map_err(|_| "Enter parameters as a JSON array of named, typed values.")?
        };
        let scope = project.map_or_else(
            || json!({"kind":"Workspace"}),
            |project| json!({"kind":"Project","projectId":project}),
        );
        let mut body = json!({
            "scope":scope,
            "connectionId":source.id,
            "expectedConnectionRevision":source.revision,
            "statement":self.statement,
            "parameters":params,
            "timeoutMs":"30000",
        });
        let digest = self
            .result
            .as_ref()
            .and_then(|answer| answer.schema_digest.as_ref())
            .ok_or_else(|| {
                "Read from this source with Capture schema digest selected first.".to_owned()
            });
        match self.mode {
            Mode::Read => {
                body["includeSchemaDigest"] = json!(self.guard_digest);
                body["rowLimit"] = json!(100);
                typed(body, database::Input::Read)
            }
            Mode::Write => typed(body, database::Input::ExecuteWrite),
            Mode::Direct => {
                body["expectedSchemaDigest"] = json!(digest?);
                body["fallbackReason"] = json!(self.reason);
                typed(body, database::Input::DirectSchemaChange)
            }
            Mode::Migration => {
                let migration: Value = serde_json::from_str(&self.migration)
                    .map_err(|_| "Enter the exact migration artifact reference.")?;
                let provenance: Value = serde_json::from_str(&self.provenance).map_err(
                    |_| "Enter the repository ID, commit, and repository-relative path.",
                )?;
                typed(
                    json!({
                        "scope":scope,
                    "connectionId":source.id,
                        "expectedConnectionRevision":source.revision,
                        "migration":migration,
                        "source":provenance,
                        "expectedSchemaDigest":digest?,
                    }),
                    database::Input::ApplyMigration,
                )
            }
        }
    }

    fn permissions(source: &SchemaConnection, project: Option<&str>) -> (bool, bool) {
        let Some(config) = source.config.as_ref() else {
            return (false, false);
        };
        if config.enabled == Some(false)
            || !matches!(
                config.exposure_mode,
                Some(SchemaExposureMode::ReadOnly | SchemaExposureMode::ReadWrite)
            )
        {
            return (false, false);
        }
        let access = project.and_then(|id| {
            config
                .project_access
                .as_ref()?
                .0
                .iter()
                .find(|entry| entry.project_id.as_str() == id)
        });
        let read = project.is_none() || access.is_some_and(|grant| grant.data_read_enabled);
        let write = config.writes_enabled
            && matches!(config.exposure_mode, Some(SchemaExposureMode::ReadWrite))
            && (project.is_none() || access.is_some_and(|grant| grant.data_write_enabled));
        (read, write)
    }

    fn rule_edit(&self, delete: bool) -> Result<OperationInput, String> {
        let state = self
            .rules
            .as_ref()
            .ok_or("Load the rules before editing.")?;
        let optional = |name: &str| (!name.is_empty()).then_some(name.to_owned());
        let edit = if delete {
            json!({"kind":"Delete","id":self.rule_id})
        } else {
            json!({"kind":"Upsert","rule":{
                "id":self.rule_id,
                "catalog":optional(&self.rule_catalog),
                "schema":optional(&self.rule_schema),
                "table":optional(&self.rule_table),
                "column":self.rule_column,
                "allow":self.rule_allow,
                "reason":optional(&self.rule_reason),
            }})
        };
        typed(
            json!({"connectionId":state.connection_id,"expectedRevision":state.revision,"edit":edit}),
            database::Input::EditSensitiveColumnRules,
        )
    }

    fn rules_ui(
        &mut self,
        ui: &mut Ui,
        source: Option<&SchemaConnection>,
        online: bool,
        actions: &mut Vec<Action>,
    ) {
        ui.separator();
        ui.heading("Sensitive columns");
        let previous = self.workspace_rules;
        ui.checkbox(&mut self.workspace_rules, "Workspace defaults");
        if self.workspace_rules != previous {
            self.rules = None;
        }
        let connection = (!self.workspace_rules)
            .then(|| source.map(|source| source.id.clone()))
            .flatten();
        if button_enabled(
            ui,
            "Load rules",
            online && (connection.is_some() || self.workspace_rules),
        )
        .clicked()
        {
            actions.push(Action::Data(Box::new(OperationInput::Database(
                database::Input::SensitiveColumns(database::SensitiveColumnsInput {
                    connection_id: connection,
                }),
            ))));
        }
        let Some(rules) = &self.rules else { return };
        for rule in &rules.rules.0 {
            let line = format!(
                "{} · {}.{}.{}.{}",
                if rule.allow { "Allow" } else { "Deny" },
                rule.catalog.as_ref().map_or("*", |v| v.0.as_str()),
                rule.schema.as_ref().map_or("*", |v| v.0.as_str()),
                rule.table.as_ref().map_or("*", |v| v.0.as_str()),
                rule.column.0
            );
            if ui.button(line).clicked() {
                self.rule_id = rule.id.as_str().to_owned();
                self.rule_catalog = rule
                    .catalog
                    .as_ref()
                    .map(|field| field.0.clone())
                    .unwrap_or_default();
                self.rule_schema = rule
                    .schema
                    .as_ref()
                    .map(|field| field.0.clone())
                    .unwrap_or_default();
                self.rule_table = rule
                    .table
                    .as_ref()
                    .map(|field| field.0.clone())
                    .unwrap_or_default();
                self.rule_column.clone_from(&rule.column.0);
                self.rule_reason = rule
                    .reason
                    .as_ref()
                    .map(|field| field.0.clone())
                    .unwrap_or_default();
                self.rule_allow = rule.allow;
            }
        }
        for (label, field) in [
            ("Rule ID", &mut self.rule_id),
            ("Catalog", &mut self.rule_catalog),
            ("Schema", &mut self.rule_schema),
            ("Table", &mut self.rule_table),
            ("Column", &mut self.rule_column),
            ("Reason", &mut self.rule_reason),
        ] {
            ui.horizontal(|ui| {
                ui.label(label);
                ui.text_edit_singleline(field);
            });
        }
        ui.checkbox(&mut self.rule_allow, "Allow this column");
        ui.horizontal(|ui| {
            for (label, delete) in [("Save rule", false), ("Delete rule", true)] {
                if button_enabled(ui, label, online && !self.rule_id.is_empty()).clicked() {
                    match self.rule_edit(delete) {
                        Ok(input) => actions.push(Action::Data(Box::new(input))),
                        Err(reason) => actions.push(Action::DataError(reason)),
                    }
                }
            }
        });
    }
}

impl DatabasesScreen {
    pub(super) fn data_ui(&mut self, ui: &mut Ui, online: bool, actions: &mut Vec<Action>) {
        let source = self.source().cloned();
        let draft = &mut self.data;
        if draft.connection != self.selected {
            draft.connection.clone_from(&self.selected);
            draft.result = None;
            draft.rules = None;
            draft.query_generation = draft.query_generation.wrapping_add(1);
            draft.confirm_write = false;
        }
        if let Some(source) = &source {
            let (read, write) = DataScreen::permissions(source, self.project.as_deref());
            ui.heading(&source.name.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut draft.mode, Mode::Read, "Read data");
                ui.selectable_value(&mut draft.mode, Mode::Write, "Write data");
                ui.selectable_value(&mut draft.mode, Mode::Migration, "Apply migration");
                ui.selectable_value(&mut draft.mode, Mode::Direct, "Direct schema change");
            });
            if draft.mode != Mode::Migration {
                let statement_label = ui.label("SQL statement");
                ui.add(
                    egui::TextEdit::multiline(&mut draft.statement)
                        .code_editor()
                        .desired_rows(5),
                )
                .labelled_by(statement_label.id);
                ui.collapsing("Bound parameters", |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut draft.params)
                            .code_editor()
                            .hint_text(r#"[{"name":"id","value":{"kind":"Integer","value":"1"}}]"#),
                    );
                });
            }
            match draft.mode {
                Mode::Read => {
                    ui.checkbox(&mut draft.guard_digest, "Capture schema digest");
                }
                Mode::Direct => {
                    ui.label("Reason for changing schema directly");
                    ui.text_edit_singleline(&mut draft.reason);
                }
                Mode::Migration => {
                    ui.label("Migration artifact reference");
                    ui.add(egui::TextEdit::multiline(&mut draft.migration).code_editor());
                    ui.label("Versioned migration source");
                    ui.add(egui::TextEdit::multiline(&mut draft.provenance).code_editor()
                        .hint_text(r#"{"repositoryId":"…","commit":{"format":"Sha1","hex":"…"},"path":"migrations/…"}"#));
                }
                _ => {}
            }
            if draft.mode != Mode::Read {
                ui.checkbox(&mut draft.confirm_write, "I intend to change this database");
            }
            let allowed = online
                && !self.busy
                && if draft.mode == Mode::Read {
                    read
                } else {
                    write && draft.confirm_write
                };
            if button_enabled(
                ui,
                if draft.mode == Mode::Read {
                    "Run read"
                } else {
                    "Submit change"
                },
                allowed,
            )
            .clicked()
            {
                match draft.frame(source, self.project.as_deref()) {
                    Ok(input) => {
                        draft.confirm_write = false;
                        actions.push(Action::Data(Box::new(input)));
                    }
                    Err(reason) => actions.push(Action::DataError(reason)),
                }
            }
            if let Some(result) = &draft.result {
                ui.label(format!(
                    "{} rows{}",
                    result.rows.0.len(),
                    if result.truncated {
                        " (more available)"
                    } else {
                        ""
                    }
                ));
                if let Some(digest) = &result.schema_digest {
                    ui.monospace(digest.as_str());
                }
                egui::ScrollArea::horizontal().show(ui, |ui| {
                    egui::Grid::new("database-data-results")
                        .striped(true)
                        .show(ui, |ui| {
                            for column in &result.columns.0 {
                                ui.strong(&column.name.0);
                            }
                            ui.end_row();
                            for row in &result.rows.0 {
                                for cell in &row.cells.0 {
                                    match cell {
                                        DbScalar::Null {} => ui.monospace("NULL"),
                                        DbScalar::Boolean { value } => {
                                            ui.monospace(value.to_string())
                                        }
                                        DbScalar::Integer { value } => ui.monospace(&value.0),
                                        DbScalar::Decimal { value } => ui.monospace(&value.0),
                                        DbScalar::Text { value } => ui.monospace(&value.0),
                                        DbScalar::Bytes { value } => ui.monospace(format!(
                                            "Binary artifact {}",
                                            value.artifact_id.as_str()
                                        )),
                                    };
                                }
                                ui.end_row();
                            }
                        });
                });
            }
        } else {
            ui.label("Select a database source.");
        }
        if let Some(operation) = &draft.operation {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("Operation {}", operation.0.as_str()));
                if let Some(status) = &draft.operation_state {
                    ui.label(format!("{status:?}"));
                }
                if button_enabled(ui, "Check operation", online).clicked() {
                    actions.push(Action::DataStatus(operation.clone()));
                }
            });
        }
        if self.project.is_none() {
            draft.rules_ui(ui, source.as_ref(), online && !self.busy, actions);
        }
    }
}
