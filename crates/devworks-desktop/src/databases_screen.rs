//! Native database source forms and structural exploration. All records come from SchemaService.
use devworks_core::protocol::records::*;
use eframe::egui::{self, Ui};
use serde_json::{Value, json};
#[derive(Default)]
pub struct DatabasesScreen {
    pub tab: usize,
    pub generated_key: Option<devworks_core::ports::secrets::Secret>,
    pub focus_object: bool,
    pub project: Option<String>,
    pub loaded: bool,
    pub loading: bool,
    pub busy: bool,
    pub sequence: u64,
    pub connections: Vec<SchemaConnection>,
    pub next: Option<String>,
    pub selected: Option<String>,
    pub draft: Option<Draft>,
    pub objects: Vec<SchemaObject>,
    pub object_next: Option<String>,
    pub relations: Vec<SchemaRelation>,
    pub relation_next: Option<String>,
    pub selected_object: Option<SchemaObject>,
    pub view: Option<String>,
    pub read_pin: Option<String>,
    pub filter: String,
    pub kind: String,
    pub parent: Option<String>,
    pub object_id: Option<String>,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub confirm: Option<&'static str>,
    pub batch_scanning: bool,
    pub batch_active: bool,
    pub batch_scope: Option<String>,
    pub batch_remaining: std::collections::VecDeque<(String, i64)>,
    pub batch_submitted: usize,
    pub batch_failures: usize,
    pub read_sequence: u64,
    pub read_loading: bool,
    pub pending_read: Option<(String, ReadIntent)>,
    pub relation_ids: Vec<String>,
}
#[derive(Clone)]
pub enum ReadIntent {
    Objects(bool),
    Relationships { ids: Vec<String>, append: bool },
}
impl DatabasesScreen {
    pub fn clear_explorer(&mut self) {
        self.view = None;
        self.read_pin = None;
        self.object_id = None;
        self.parent = None;
        self.object_next = None;
        self.relation_next = None;
        self.relation_ids.clear();
        self.objects.clear();
        self.relations.clear();
        self.selected_object = None;
        self.pending_read = None;
        self.read_loading = false;
        self.read_sequence = self.read_sequence.wrapping_add(1);
    }
}
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Draft {
    pub scope: Option<String>,
    pub id: Option<String>,
    pub revision: Option<i64>,
    pub config: Value,
    #[serde(skip)]
    pub password: String,
    #[serde(skip)]
    pub ca_file: Option<std::path::PathBuf>,
}
impl Draft {
    pub fn new(project: Option<String>) -> Self {
        let access=project.iter().map(|p|json!({"projectId":p,"metadataEnabled":true,"dataReadEnabled":false,"dataWriteEnabled":false})).collect::<Vec<_>>();
        Self {
            scope: project,
            id: None,
            revision: None,
            config: json!({"name":"","engine":"Sqlite","endpoint":"","catalogs":[],"schemas":[],"includePatterns":[],"excludePatterns":[],"writesEnabled":false,"enabled":true,"workspaceExposed":false,"projectAccess":access,"exposureMode":"SchemaOnly","authentication":"File"}),
            password: String::new(),
            ca_file: None,
        }
    }
    fn changed_engine(&mut self) {
        let file = self.config["engine"] == "Sqlite";
        self.config["authentication"] = json!(if file { "File" } else { "Password" });
        if file {
            for key in ["tls", "credentialRef", "caCertificate", "username"] {
                self.config.as_object_mut().unwrap().remove(key);
            }
            self.password.clear();
            self.ca_file = None;
        } else {
            self.config["tls"] = json!("VerifyFull");
        }
    }
    pub fn edit(source: &SchemaConnection, scope: Option<String>) -> Option<Self> {
        Some(Self {
            id: Some(source.id.as_str().into()),
            revision: Some(source.revision.get()),
            config: serde_json::to_value(source.config.as_ref()?).ok()?,
            scope,
            password: String::new(),
            ca_file: None,
        })
    }
}
pub enum Action {
    GenerateKey,
    ClearKey,
    Refresh,
    More,
    New,
    Edit(String),
    Select(String),
    Save,
    CancelDraft,
    PickDatabase,
    PickCa,
    Index(String, bool),
    SyncAll,
    Remove(&'static str),
    ConfirmRemove,
    CancelRemove,
    Objects(bool),
    Inspect(SchemaObject),
    Children(String),
    Endpoint(String),
    Relations(Vec<String>, bool),
    ConfirmOutcome,
}
// Explicit UI roots keep control identity independent of conditional metadata and live hints.
fn button(ui: &mut Ui, label: impl AsRef<str>) -> egui::Response {
    button_enabled(ui, label, true)
}
fn button_enabled(ui: &mut Ui, label: impl AsRef<str>, enabled: bool) -> egui::Response {
    let label = label.as_ref();
    let id = ui.make_persistent_id(("database-button", label));
    ui.scope_builder(egui::UiBuilder::new().scope_id(id), |ui| {
        ui.add_enabled(enabled, egui::Button::new(label))
    })
    .inner
}
fn field(ui: &mut Ui, label: &str, value: &mut String, password: bool) {
    ui.label(label);
    ui.add(
        egui::TextEdit::singleline(value)
            .id_salt(label)
            .password(password)
            .desired_width(420.0),
    )
    .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, label));
}
fn text(ui: &mut Ui, label: &str, config: &mut Value, key: &str) {
    let mut value = config[key].as_str().unwrap_or("").to_owned();
    field(ui, label, &mut value, false);
    if value.is_empty() && matches!(key, "username" | "displayName" | "environment") {
        config.as_object_mut().unwrap().remove(key);
    } else {
        config[key] = json!(value);
    }
}
fn lines(ui: &mut Ui, label: &str, config: &mut Value, key: &str) {
    let mut value = config[key]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    ui.label(label);
    let response = ui.add(
        egui::TextEdit::multiline(&mut value)
            .id_salt(label)
            .desired_rows(2)
            .desired_width(420.0),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, label));
    if response.changed() {
        config[key] = json!(value.lines().filter(|l| !l.is_empty()).collect::<Vec<_>>());
    }
}
/// DB-04/DB-05 data tools are not built yet (DW-37 owns them): these switches are saved and
/// validated, but nothing reads or writes table data, so the form says so beside them.
pub const DATA_ACCESS_INACTIVE: &str = "Not yet active: DevWorks has no database data tools yet. Data read access, data write access, separately authorized writes and the ReadOnly and ReadWrite tool exposures are saved but change nothing; agents can read indexed schema metadata only.";
fn toggle(ui: &mut Ui, label: &str, config: &mut Value, key: &str) {
    let mut b = config[key].as_bool().unwrap_or(false);
    if ui.checkbox(&mut b, label).changed() {
        config[key] = json!(b);
    }
}
fn choice(ui: &mut Ui, label: &str, config: &mut Value, key: &str, values: &[&str]) {
    let mut s = config[key].as_str().unwrap_or(values[0]).to_owned();
    egui::ComboBox::from_label(label)
        .selected_text(&s)
        .show_ui(ui, |ui| {
            for value in values {
                ui.selectable_value(&mut s, (*value).into(), *value);
            }
        });
    config[key] = json!(s);
}
fn project_access(ui: &mut Ui, config: &mut Value, projects: &[(String, String)]) {
    let mut access = config["projectAccess"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut all = projects.to_vec();
    for row in &access {
        if let Some(id) = row["projectId"].as_str()
            && !all.iter().any(|p| p.0 == id)
        {
            all.push((id.into(), id.into()));
        }
    }
    ui.label("Project access");
    for (id, title) in all {
        let position = access.iter().position(|a| a["projectId"] == id);
        let mut row=position.map(|p|access[p].clone()).unwrap_or_else(||json!({"projectId":id,"metadataEnabled":false,"dataReadEnabled":false,"dataWriteEnabled":false}));
        ui.scope_builder(
            egui::UiBuilder::new().scope_id(ui.make_persistent_id(("database-project", &id))),
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(title);
                    toggle(ui, "Metadata access", &mut row, "metadataEnabled");
                    toggle(
                        ui,
                        "Data read access (not yet active)",
                        &mut row,
                        "dataReadEnabled",
                    );
                    toggle(
                        ui,
                        "Data write access (not yet active)",
                        &mut row,
                        "dataWriteEnabled",
                    );
                });
            },
        );
        if config["writesEnabled"] != true || row["dataReadEnabled"] != true {
            row["dataWriteEnabled"] = json!(false);
        }
        if let Some(position) = position {
            access[position] = row;
        } else if row["metadataEnabled"] == true || row["dataReadEnabled"] == true {
            access.push(row);
        }
    }
    config["projectAccess"] = json!(access);
}
impl DatabasesScreen {
    pub fn scope(&self) -> Value {
        scope(self.project.as_deref())
    }
    pub fn source(&self) -> Option<&SchemaConnection> {
        self.selected
            .as_deref()
            .and_then(|id| self.connections.iter().find(|c| c.id.as_str() == id))
    }
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        availability: Result<(), &str>,
        projects: &[(String, String)],
        actions: &mut Vec<Action>,
    ) {
        let mut state = (self, actions);
        crate::widgets::with_right_sidebar(
            ui,
            crate::theme::SIDEBAR,
            &mut state,
            |ui, state| {
                let tab = state.0.tab;
                let (_, selected) = crate::widgets::pane(
                    ui,
                    crate::widgets::Header {
                        title: "Databases",
                        summary: "Manage connections and explore read-only structural metadata.",
                        tabs: &["Schema", "Relationships", "Access"],
                        tab,
                    },
                    state,
                    |_, _| {},
                    |ui, state, tab| {
                        state.0.tab = tab;
                        egui::ScrollArea::vertical()
                            .id_salt(("database-main", state.0.project.clone(), tab))
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                if let Err(reason) = availability {
                                    crate::widgets::state_block(
                                        ui,
                                        &crate::widgets::ViewState::Offline {
                                            reason: reason.to_owned(),
                                        },
                                    );
                                }
                                state.0.main_ui(ui, availability.is_ok(), projects, state.1);
                            });
                    },
                );
                state.0.tab = selected;
            },
            |ui, state| {
                crate::widgets::secondary_pane(ui, "Database sources", |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt(("database-sources", state.0.project.clone()))
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            state.0.sources_ui(ui, availability.is_ok(), state.1)
                        });
                });
            },
        );
    }
    fn main_ui(
        &mut self,
        ui: &mut Ui,
        online: bool,
        projects: &[(String, String)],
        actions: &mut Vec<Action>,
    ) {
        ui.label(self.project.as_ref().map_or_else(
            || "Workspace database sources".to_owned(),
            |id| {
                format!(
                    "Project database sources: {}",
                    projects
                        .iter()
                        .find(|p| &p.0 == id)
                        .map_or("Selected project", |p| p.1.as_str())
                )
            },
        ));
        ui.label("Manage target connections and explore read-only structural metadata.");
        ui.horizontal_wrapped(|ui| {
            if button_enabled(ui, "Generate encryption key", online).clicked() {
                actions.push(Action::GenerateKey);
            }
            if let Some(key) = &self.generated_key {
                ui.label("256-bit Base64 key ready");
                if button(ui, "Copy generated encryption key").clicked() {
                    ui.ctx().copy_text(key.expose().to_owned());
                }
                if button(ui, "Clear generated encryption key").clicked() {
                    actions.push(Action::ClearKey);
                }
            }
        });
        if let Some(error) = &self.error {
            ui.colored_label(egui::Color32::LIGHT_RED, error);
        }
        if let Some(notice) = &self.notice {
            ui.label(notice);
        }
        if !online {
            ui.label("Database operations are unavailable while offline or closing.");
        }
        if self.tab == 2
            && let Some(draft) = &mut self.draft
        {
            ui.separator();
            ui.heading(if draft.id.is_some() {
                "Edit database source"
            } else {
                "New database source"
            });
            ui.add_enabled_ui(!self.busy,|ui|{
                text(ui,"Database source name",&mut draft.config,"name");
                let old=draft.config["engine"].clone();choice(ui,"Database engine",&mut draft.config,"engine",&["Sqlite","MariaDb","MySql","Postgres","SqlServer"]);
                if draft.config["engine"]!=old {draft.changed_engine();}
                text(ui,"Database file or endpoint",&mut draft.config,"endpoint");
                if draft.config["engine"]=="Sqlite" {if button(ui,"Choose database file").clicked(){actions.push(Action::PickDatabase);}}else{
                    text(ui,"Database username",&mut draft.config,"username");field(ui,"Database password (blank keeps saved password)",&mut draft.password,true);
                    if draft.config.get("credentialRef").is_some(){ui.label("A password is stored in the configured credential facility.");if button(ui,"Remove saved database password").clicked(){draft.config.as_object_mut().unwrap().remove("credentialRef");}}
                    choice(ui,"Database TLS",&mut draft.config,"tls",&["VerifyFull","VerifyCa","LoopbackPlaintext"]);
                    if button(ui,"Choose database CA certificate").clicked(){actions.push(Action::PickCa);}
                    if let Some(path)=&draft.ca_file{ui.label(path.file_name().unwrap_or_default().to_string_lossy());}
                    if draft.config.get("caCertificate").is_some(){ui.label("An exact public CA certificate is copied for this connection.");if button(ui,"Remove database CA certificate").clicked(){draft.config.as_object_mut().unwrap().remove("caCertificate");}}
                }
                text(ui,"Database display name",&mut draft.config,"displayName");text(ui,"Database environment",&mut draft.config,"environment");
                lines(ui,"Database catalogs (one per line)",&mut draft.config,"catalogs");lines(ui,"Database schemas (one per line)",&mut draft.config,"schemas");
                lines(ui,"Included metadata patterns (one per line)",&mut draft.config,"includePatterns");lines(ui,"Excluded metadata patterns (one per line)",&mut draft.config,"excludePatterns");
                toggle(ui,"Enable database source",&mut draft.config,"enabled");toggle(ui,"Expose source in workspace",&mut draft.config,"workspaceExposed");choice(ui,"Database tool exposure",&mut draft.config,"exposureMode",&["Hidden","SchemaOnly","ReadOnly","ReadWrite"]);
                toggle(ui,"Enable separately authorized data writes (not yet active)",&mut draft.config,"writesEnabled");
                ui.label("Metadata indexing always uses read-only source access. Data operations require their own permissions.");
                crate::widgets::caption(ui, DATA_ACCESS_INACTIVE);
                project_access(ui,&mut draft.config,projects);
            });
            ui.horizontal(|ui| {
                if button_enabled(ui, "Save database source", online && !self.busy).clicked() {
                    actions.push(Action::Save);
                }
                if button_enabled(ui, "Cancel database edit", !self.busy).clicked() {
                    actions.push(Action::CancelDraft);
                }
            });
        }
        let Some(source) = self.source().cloned() else {
            return;
        };
        ui.separator();
        ui.heading(&source.name.0);
        ui.label(format!("Source revision {}", source.revision.get()));
        if let Some(last) = source.last_synced_at_ms {
            ui.label(format!("Last synced: {} (Unix milliseconds)", last.get()));
        }
        if let Some(error) = &source.last_error {
            ui.colored_label(egui::Color32::LIGHT_RED, &error.0);
        }
        if let Some(coverage) = &source.coverage {
            ui.label(format!("Coverage: {:?}", coverage.state));
            for gap in &coverage.gaps {
                ui.label(format!("Coverage gap: {}", gap.as_str()));
            }
        }
        if let Some(profile) = &source.profile {
            ui.label(format!(
                "{} · {} · {} · TLS {}",
                profile.driver.0, profile.server_version.0, profile.authentication.0, profile.tls.0
            ));
            ui.label(&profile.read_only_boundary.0);
            for limitation in &profile.limitations.0 {
                ui.label(&limitation.0);
            }
            ui.label(format!(
                "Unsupported metadata: {:?}",
                profile.unsupported_metadata.0
            ));
        }
        if let Some(op) = &source.active_operation_id {
            ui.label(format!("Metadata operation {} is pending", op.as_str()));
        }
        ui.scope_builder(
            egui::UiBuilder::new()
                .scope_id(ui.make_persistent_id(("database-controls", source.id.as_str()))),
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    let enabled = online && !self.busy && source.active_operation_id.is_none();
                    if ui
                        .add_enabled(enabled, egui::Button::new("Test database connection"))
                        .clicked()
                    {
                        actions.push(Action::Index(source.id.as_str().into(), true));
                    }
                    if ui
                        .add_enabled(enabled, egui::Button::new("Sync database metadata"))
                        .clicked()
                    {
                        actions.push(Action::Index(source.id.as_str().into(), false));
                    }
                    if ui
                        .add_enabled(
                            online && !self.busy,
                            egui::Button::new("Remove database index"),
                        )
                        .clicked()
                    {
                        actions.push(Action::Remove("index"));
                    }
                    let response = ui.add_enabled(
                        online && !self.busy,
                        egui::Button::new("Remove database source"),
                    );
                    if response.clicked() {
                        actions.push(Action::Remove("source"));
                    }
                })
            },
        );
        if let Some(kind) = self.confirm {
            ui.label(if kind=="source"{"Remove this saved source and its indexed metadata? The source database remains unchanged."}else{"Remove the indexed metadata? The connection and source database remain available."});
            ui.scope_builder(
                egui::UiBuilder::new()
                    .scope_id(ui.make_persistent_id(("database-removal", source.id.as_str()))),
                |ui| {
                    ui.horizontal(|ui| {
                        if button(ui, "Confirm database removal").clicked() {
                            actions.push(Action::ConfirmRemove);
                        }
                        if button(ui, "Keep database source").clicked() {
                            actions.push(Action::CancelRemove);
                        }
                    })
                },
            );
        }
        if self.tab == 2 {
            ui.separator();
            ui.heading("Saved source access");
            if let Some(config) = &source.config {
                ui.label(format!("Authentication: {:?}", config.authentication));
                ui.label(format!(
                    "Workspace exposure: {}",
                    config.workspace_exposed.unwrap_or(false)
                ));
                ui.label(format!("Tool access: {:?}", config.exposure_mode));
                ui.label(format!(
                    "Separately authorized writes: {}",
                    config.writes_enabled
                ));
                crate::widgets::caption(ui, DATA_ACCESS_INACTIVE);
                for access in config
                    .project_access
                    .as_ref()
                    .into_iter()
                    .flat_map(|v| &v.0)
                {
                    let name = projects
                        .iter()
                        .find(|p| p.0 == access.project_id.as_str())
                        .map_or(access.project_id.as_str(), |p| p.1.as_str());
                    ui.label(format!(
                        "{name}: metadata {}, read {}, write {}",
                        access.metadata_enabled,
                        access.data_read_enabled,
                        access.data_write_enabled
                    ));
                }
                if button_enabled(ui, "Edit selected database access", online && !self.busy)
                    .clicked()
                {
                    actions.push(Action::Edit(source.id.as_str().into()));
                }
            } else {
                ui.label("This source is shared. Its owner controls its saved connection and access settings.");
            }
            return;
        }
        if self.tab == 0 {
            ui.separator();
            field(ui, "Find schema objects", &mut self.filter, false);
            let mut kind = json!({"kind":self.kind});
            choice(
                ui,
                "Schema object kind",
                &mut kind,
                "kind",
                &[
                    "",
                    "Catalog",
                    "Schema",
                    "Table",
                    "Column",
                    "Index",
                    "Constraint",
                    "View",
                    "Procedure",
                    "Type",
                    "GraphEntity",
                    "GraphEdge",
                ],
            );
            self.kind = kind["kind"].as_str().unwrap_or("").into();
            ui.horizontal(|ui| {
                if button_enabled(ui, "Search schema objects", online && !self.read_loading)
                    .clicked()
                {
                    actions.push(Action::Objects(false));
                }
                if self.parent.is_some() && button(ui, "All schema objects").clicked() {
                    self.parent = None;
                    actions.push(Action::Objects(false));
                }
            });
            if self.read_loading {
                ui.spinner();
            }
            for object in &self.objects {
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .scope_id(ui.make_persistent_id(("schema-object", object.id.as_str()))),
                    |ui| {
                        if ui
                            .button(format!("{:?} {}", object.kind, object.qualified_name.0))
                            .clicked()
                        {
                            actions.push(Action::Inspect(object.clone()));
                        }
                    },
                );
            }
            if self.object_next.is_some()
                && ui
                    .scope_builder(
                        egui::UiBuilder::new()
                            .scope_id(ui.make_persistent_id("schema-object-paging")),
                        |ui| {
                            ui.add_enabled(
                                !self.read_loading,
                                egui::Button::new("More schema objects"),
                            )
                            .clicked()
                        },
                    )
                    .inner
            {
                actions.push(Action::Objects(true));
            }
            if let Some(object) = &self.selected_object {
                ui.separator();
                let heading = ui.heading(&object.qualified_name.0);
                if self.focus_object {
                    heading.scroll_to_me(Some(egui::Align::TOP));
                    self.focus_object = false;
                }
                let coverage = &object.coverage;
                ui.label(format!("Object coverage: {:?}", coverage.state));
                for gap in &coverage.gaps {
                    ui.label(format!("Object coverage gap: {}", gap.as_str()));
                }
                if let Some(m) = &object.metadata {
                    ui.label(&m.name.0);
                    for (label, value) in [
                        ("Type", m.data_type.as_ref().map(|v| v.0.as_str())),
                        ("Comment", m.comment.as_ref().map(|v| v.0.as_str())),
                        ("Default", m.default_value.as_ref().map(|v| v.0.as_str())),
                        ("Column key", m.column_key.as_ref().map(|v| v.0.as_str())),
                        ("Extra", m.extra.as_ref().map(|v| v.0.as_str())),
                        ("Index type", m.index_type.as_ref().map(|v| v.0.as_str())),
                        (
                            "Constraint",
                            m.constraint_type.as_ref().map(|v| v.0.as_str()),
                        ),
                        (
                            "Referenced table",
                            m.referenced_table.as_ref().map(|v| v.0.as_str()),
                        ),
                        ("Check", m.check_clause.as_ref().map(|v| v.0.as_str())),
                        (
                            "Routine type",
                            m.routine_type.as_ref().map(|v| v.0.as_str()),
                        ),
                    ] {
                        if let Some(value) = value {
                            ui.label(format!("{label}: {value}"));
                        }
                    }
                    if let Some(nullable) = m.nullable {
                        ui.label(format!("Nullable: {nullable}"));
                    }
                    if let Some(primary) = m.primary_key {
                        ui.label(format!("Primary key: {primary}"));
                    }
                    if let Some(unique) = m.unique {
                        ui.label(format!("Unique: {unique}"));
                    }
                    if let Some(ordinal) = &m.ordinal {
                        ui.label(format!("Ordinal: {}", ordinal.get()));
                    }
                    if !m.referenced_columns.0.is_empty() {
                        ui.label(format!(
                            "Referenced columns: {}",
                            m.referenced_columns
                                .0
                                .iter()
                                .map(|v| v.0.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    if !m.column_names.0.is_empty() {
                        ui.label(format!(
                            "Columns: {}",
                            m.column_names
                                .0
                                .iter()
                                .map(|n| n.0.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    if let Some(sql) = &m.definition_sql {
                        egui::CollapsingHeader::new("Structural definition").show(ui, |ui| {
                            ui.add(egui::Label::new(&sql.0).selectable(true).wrap());
                        });
                    }
                    for parameter in &m.parameters.0 {
                        ui.label(format!(
                            "{} {} {}",
                            parameter.mode.0, parameter.name.0, parameter.data_type.0
                        ));
                    }
                }
                ui.horizontal(|ui| {
                    if button(ui, "Inspect schema children").clicked() {
                        actions.push(Action::Children(object.id.as_str().into()));
                    }
                    if button(ui, "Inspect schema relationships").clicked() {
                        actions.push(Action::Relations(vec![object.id.as_str().into()], false));
                    }
                });
            }
        }
        if self.tab != 1 {
            return;
        }
        if button_enabled(
            ui,
            "Load database relationships",
            online && !self.read_loading && source.current_view_id.is_some(),
        )
        .clicked()
        {
            actions.push(Action::Relations(Vec::new(), false));
        }
        for relation in &self.relations {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!(
                    "{}{}",
                    relation.relation.0,
                    if relation.inferred { " (inferred)" } else { "" }
                ));
                for (direction, endpoint) in [
                    ("source", &relation.from_object_id),
                    ("target", &relation.to_object_id),
                ] {
                    let name = self
                        .objects
                        .iter()
                        .find(|o| &o.id == endpoint)
                        .map_or("Related schema object", |o| o.qualified_name.0.as_str());
                    if ui
                        .button(format!("Inspect relationship {direction}: {name}"))
                        .clicked()
                    {
                        actions.push(Action::Endpoint(endpoint.as_str().into()));
                    }
                }
            });
        }
        if self.relation_next.is_some() && button(ui, "More schema relationships").clicked() {
            actions.push(Action::Relations(self.relation_ids.clone(), true));
        }
    }
}
impl DatabasesScreen {
    fn sources_ui(&mut self, ui: &mut Ui, online: bool, actions: &mut Vec<Action>) {
        ui.scope_builder(
            egui::UiBuilder::new().scope_id(ui.make_persistent_id("database-toolbar")),
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(
                            online && !self.loading,
                            egui::Button::new("Refresh databases"),
                        )
                        .clicked()
                    {
                        actions.push(Action::Refresh);
                    }
                    if ui
                        .add_enabled(
                            online && !self.busy,
                            egui::Button::new("New database source"),
                        )
                        .clicked()
                    {
                        actions.push(Action::New);
                    }
                    if ui
                        .add_enabled(
                            online && !self.busy && !self.connections.is_empty(),
                            egui::Button::new("Sync all enabled sources"),
                        )
                        .clicked()
                    {
                        actions.push(Action::SyncAll);
                    }
                    if button(ui, "Confirm database outcome").clicked() {
                        actions.push(Action::ConfirmOutcome);
                    }
                })
            },
        );
        ui.separator();
        if self.loading {
            ui.spinner();
        } else if self.loaded && self.connections.is_empty() {
            ui.label("No database sources are visible in this scope.");
        }
        for source in &self.connections {
            ui.scope_builder(
                egui::UiBuilder::new()
                    .scope_id(ui.make_persistent_id(("database-source", source.id.as_str()))),
                |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .selectable_label(
                                self.selected.as_deref() == Some(source.id.as_str()),
                                &source.name.0,
                            )
                            .clicked()
                        {
                            actions.push(Action::Select(source.id.as_str().into()));
                        }
                        ui.label(format!("{:?} · {:?}", source.engine, source.state));
                        if ui
                            .add_enabled(
                                !self.busy,
                                egui::Button::new(format!("Edit {}", source.name.0)),
                            )
                            .clicked()
                        {
                            actions.push(Action::Edit(source.id.as_str().into()));
                        }
                    })
                },
            );
        }
        if self.next.is_some() && button(ui, "More database sources").clicked() {
            actions.push(Action::More);
        }
    }
}

pub fn scope(project: Option<&str>) -> Value {
    project.map_or_else(
        || json!({"kind":"Workspace"}),
        |p| json!({"kind":"Project","projectId":p}),
    )
}

impl DatabasesScreen {
    pub fn pending_memory(&self) -> Value {
        if self.draft.is_none()
            && self.batch_remaining.is_empty()
            && !self.batch_active
            && !self.batch_scanning
        {
            return Value::Null;
        }
        json!({"draft":self.draft,"batchActive":self.batch_active,"batchScanning":self.batch_scanning,"remaining":self.batch_remaining,"batchScope":self.batch_scope,"submitted":self.batch_submitted,"failures":self.batch_failures,"tab":self.tab})
    }
    pub fn restore_pending(&mut self, value: &Value) {
        self.batch_active = value["batchActive"].as_bool().unwrap_or(false);
        self.batch_scanning = value["batchScanning"].as_bool().unwrap_or(false);
        self.draft = serde_json::from_value(value["draft"].clone())
            .ok()
            .flatten();
        self.tab = value["tab"]
            .as_u64()
            .filter(|v| *v <= 2)
            .map_or(if self.draft.is_some() { 2 } else { 0 }, |v| v as usize);
        self.batch_remaining =
            serde_json::from_value(value["remaining"].clone()).unwrap_or_default();
        self.batch_scope = serde_json::from_value(value["batchScope"].clone()).unwrap_or_default();
        self.batch_submitted = value["submitted"].as_u64().unwrap_or(0) as usize;
        self.batch_failures = value["failures"].as_u64().unwrap_or(0) as usize;
    }
}
