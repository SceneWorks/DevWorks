//! Native, read-only Help workspace. All content, versions and permission facts are supplied
//! by HelpService; search and passage navigation never access the network or infer authority.
use crate::{
    route::Route,
    theme,
    widgets::{self, Tone},
};
use devworks_core::protocol::records::{
    HelpProvenance, HelpReadView, HelpSnapshot, HelpTopicSummary,
};
use eframe::egui::{self, RichText, Ui};

#[derive(Clone, Debug)]
pub enum HelpScope {
    Search(String),
    Screen(String),
    Package { id: String, version: String },
}
impl Default for HelpScope {
    fn default() -> Self {
        Self::Search(String::new())
    }
}
#[derive(Default)]
pub struct HelpScreen {
    pub query: String,
    pub scope: HelpScope,
    pub topics: Vec<HelpTopicSummary>,
    pub next_cursor: Option<String>,
    pub loaded: bool,
    pub loading: bool,
    pub error: Option<String>,
    pub snapshot: Option<HelpSnapshot>,
    pub selected: Option<(String, String)>,
    pub read: Option<HelpReadView>,
    pub reading: bool,
    pub read_error: Option<String>,
    pub read_cursors: Vec<Option<String>>,
    pub list_generation: u64,
    pub read_generation: u64,
    pub back: Option<Route>,
    pub generating: bool,
    pub generation_unknown: bool,
    pub generation_operation: Option<String>,
    pub notice: Option<String>,
    pub generation_notice: Option<(Tone, String)>,
}
#[derive(Clone)]
pub enum HelpAction {
    Search,
    AllTopics,
    MoreTopics,
    Select(String, String),
    Guides(String, String),
    NextPassage,
    PreviousPassage,
    ReloadPassage,
    OpenVersion(String, String),
    Refresh,
    Generate,
    Back,
}
impl HelpScreen {
    pub fn ui(&mut self, ui: &mut Ui, online: Result<(), &str>, actions: &mut Vec<HelpAction>) {
        let mut state = (&mut *self, actions);
        widgets::pane(
            ui,
            widgets::Header {
                title: "Help",
                summary: "Search and read local documentation for your installed version.",
                tabs: &[],
                tab: 0,
            },
            &mut state,
            |ui, state| {
                if state.0.back.is_some()
                    && widgets::secondary_button(ui, "Back to previous screen").clicked()
                {
                    state.1.push(HelpAction::Back);
                }
                if ui
                    .add_enabled(
                        online.is_ok() && !state.0.loading,
                        egui::Button::new("Refresh Help"),
                    )
                    .clicked()
                {
                    state.1.push(HelpAction::Refresh);
                }
            },
            |ui, state, _| state.0.body(ui, online, state.1),
        );
    }
    fn body(&mut self, ui: &mut Ui, online: Result<(), &str>, actions: &mut Vec<HelpAction>) {
        ui.horizontal_wrapped(|ui| {
            let input = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .id_salt("help-search")
                    .hint_text("Search local Help")
                    .desired_width(320.0),
            );
            input.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Search local Help")
            });
            let enter = input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui
                .add_enabled(
                    online.is_ok() && !self.loading,
                    egui::Button::new("Search Help"),
                )
                .clicked()
                || enter && online.is_ok() && !self.loading
            {
                actions.push(HelpAction::Search);
            }
            if ui
                .add_enabled(
                    online.is_ok() && !self.loading,
                    egui::Button::new("All Help topics"),
                )
                .clicked()
            {
                actions.push(HelpAction::AllTopics);
            }
        });
        if let Err(reason) = online {
            widgets::chip(
                ui,
                format!("Help service unavailable: {reason}"),
                Tone::Attention,
            );
        }
        if let Some(snapshot) = &self.snapshot {
            ui.horizontal_wrapped(|ui| {
                widgets::caption(
                    ui,
                    format!(
                        "App {} · Help revision {}",
                        snapshot.app_version.0,
                        snapshot.help_revision.as_str()
                    ),
                );
                let allowed = online.is_ok() && snapshot.can_generate && !self.generating;
                if ui
                    .add_enabled(
                        allowed,
                        egui::Button::new("Regenerate capability documentation"),
                    )
                    .on_disabled_hover_text(snapshot.cannot_generate_reason.as_ref().map_or(
                        "Documentation generation is unavailable or already running.",
                        |reason| reason.0.as_str(),
                    ))
                    .clicked()
                {
                    actions.push(HelpAction::Generate);
                }
            });
            if let Some(operation) = &snapshot.generation_operation {
                widgets::caption(
                    ui,
                    format!(
                        "Capability documentation status: {} · reference {}",
                        operation_label(operation.state),
                        operation.operation_id.0.as_str()
                    ),
                );
            }
            if !snapshot.can_generate
                && let Some(reason) = &snapshot.cannot_generate_reason
            {
                widgets::caption(ui, &reason.0);
            }
        }
        if let Some(notice) = &self.notice {
            widgets::chip(ui, notice, Tone::Attention);
        }
        if let Some((tone, notice)) = &self.generation_notice {
            widgets::chip(ui, notice, *tone);
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.set_min_height(20.0);
            if self.loading || self.reading {
                ui.spinner();
            }
            widgets::caption(
                ui,
                if self.loading {
                    "Searching local documentation…"
                } else if self.reading {
                    "Reading the selected passage…"
                } else if self.generating {
                    "Updating capability documentation…"
                } else {
                    "Local content · no network required"
                },
            );
        });
        let width = ui.available_width();
        let height = ui.available_height().max(120.0);
        let left = (width * 0.32).clamp(210.0, 330.0).min(width * 0.5);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(left, height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(left);
                    self.results(ui, online.is_ok(), actions);
                },
            );
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);
            ui.allocate_ui_with_layout(
                egui::vec2((width - left - 32.0).max(80.0), height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width((width - left - 32.0).max(80.0));
                    self.reader(ui, online.is_ok(), actions);
                },
            );
        });
    }
    fn results(&self, ui: &mut Ui, online: bool, actions: &mut Vec<HelpAction>) {
        match &self.scope {
            HelpScope::Search(query) if query.is_empty() => {
                ui.label(RichText::new("All topics").strong());
            }
            HelpScope::Search(query) => {
                ui.label(RichText::new(format!("Results for “{query}”")).strong());
            }
            HelpScope::Screen(_) => {
                ui.label(RichText::new("Help for this screen").strong());
            }
            HelpScope::Package { id, version } => {
                ui.label(RichText::new(format!("{id} · {version}")).strong());
            }
        }
        if let Some(error) = &self.error {
            ui.colored_label(theme::RED, error);
        }
        if self.loaded && self.topics.is_empty() {
            ui.label("No matching local documentation. Try another search.");
        }
        egui::ScrollArea::vertical()
            .id_salt("help-results")
            .max_height((ui.available_height() - 40.0).max(80.0))
            .show(ui, |ui| {
                for topic in &self.topics {
                    ui.scope_builder(
                        egui::UiBuilder::new().scope_id(egui::Id::unique((
                            "help-topic",
                            topic.id.as_str(),
                            &topic.version.0,
                        ))),
                        |ui| {
                            let selected = self.selected.as_ref().is_some_and(|(id, version)| {
                                id == topic.id.as_str() && version == &topic.version.0
                            });
                            let response = ui.add_enabled(
                                online,
                                egui::Button::selectable(selected, &topic.title.0),
                            );
                            response.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::Role::Button,
                                    online,
                                    format!(
                                        "Read Help topic {} version {}",
                                        topic.id.as_str(),
                                        topic.version.0
                                    ),
                                )
                            });
                            if response.clicked() && online {
                                actions.push(HelpAction::Select(
                                    topic.id.as_str().to_owned(),
                                    topic.version.0.clone(),
                                ));
                            }
                            widgets::caption(
                                ui,
                                format!("{} · {:?}", topic.version.0, topic.availability),
                            );
                            if let Some(reason) = &topic.reason {
                                widgets::caption(ui, &reason.0);
                            }
                            if let Some(provenance) = &topic.provenance {
                                provenance_ui(ui, provenance);
                            }
                            ui.add_space(8.0);
                        },
                    );
                }
            });
        if self.next_cursor.is_some()
            && ui
                .add_enabled(
                    online && !self.loading,
                    egui::Button::new("More Help topics"),
                )
                .clicked()
        {
            actions.push(HelpAction::MoreTopics);
        }
    }
    fn reader(&self, ui: &mut Ui, online: bool, actions: &mut Vec<HelpAction>) {
        if let Some(error) = &self.read_error {
            ui.colored_label(theme::RED, error);
            if ui
                .add_enabled(
                    online && !self.reading,
                    egui::Button::new("Retry Help passage"),
                )
                .clicked()
            {
                actions.push(HelpAction::ReloadPassage);
            }
        }
        match &self.read {
            None => {
                ui.heading("Read a topic");
                ui.label("Choose a result, or use Help on another screen to open its local guide.");
            }
            Some(HelpReadView::Unavailable {
                topic_id,
                requested_version,
                availability,
                reason,
                current_version,
                ..
            }) => {
                ui.heading("Documentation unavailable");
                widgets::chip(ui, format!("{availability:?}"), Tone::Attention);
                ui.label(&reason.0);
                widgets::caption(
                    ui,
                    format!(
                        "Topic {} · requested version {}",
                        topic_id.as_str(),
                        requested_version.0
                    ),
                );
                if let Some(version) = current_version
                    && version.0 != requested_version.0
                    && ui
                        .add_enabled(
                            online && !self.reading,
                            egui::Button::new(format!("Open installed version {}", version.0)),
                        )
                        .clicked()
                {
                    actions.push(HelpAction::OpenVersion(
                        topic_id.as_str().to_owned(),
                        version.0.clone(),
                    ));
                }
            }
            Some(HelpReadView::Available { topic, passage, .. }) => {
                ui.heading(&topic.title.0);
                widgets::caption(
                    ui,
                    format!(
                        "Topic {} · version {} · character {}",
                        topic.id.as_str(),
                        topic.version.0,
                        passage.offset.as_str()
                    ),
                );
                if let Some(provenance) = &topic.provenance {
                    provenance_ui(ui, provenance);
                }
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(
                            online && !self.reading && self.read_cursors.len() > 1,
                            egui::Button::new("Previous Help passage"),
                        )
                        .clicked()
                    {
                        actions.push(HelpAction::PreviousPassage);
                    }
                    if ui
                        .add_enabled(
                            online && !self.reading && passage.has_more,
                            egui::Button::new("Next Help passage"),
                        )
                        .clicked()
                    {
                        actions.push(HelpAction::NextPassage);
                    }
                    if let Some(HelpProvenance::Package {
                        package_id,
                        package_version,
                        ..
                    }) = &topic.provenance
                        && ui
                            .add_enabled(
                                online && !self.loading,
                                egui::Button::new("All guides for this package"),
                            )
                            .clicked()
                    {
                        actions.push(HelpAction::Guides(
                            package_id.as_str().to_owned(),
                            package_version.0.clone(),
                        ));
                    }
                });
                egui::ScrollArea::vertical()
                    .id_salt((
                        "help-passage",
                        topic.id.as_str(),
                        &topic.version.0,
                        &passage.offset.as_str(),
                    ))
                    .max_height(ui.available_height().max(80.0))
                    .show(ui, |ui| {
                        // Immutable text can be selected/copied; no local edit becomes documentation.
                        ui.add(egui::Label::new(&passage.text.0).selectable(true).wrap());
                        if !topic.related_capabilities.0.is_empty() {
                            ui.add_space(12.0);
                            widgets::caption(
                                ui,
                                format!(
                                    "Related capabilities: {}",
                                    topic
                                        .related_capabilities
                                        .0
                                        .iter()
                                        .map(|name| name.0.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ),
                            );
                        }
                        if !topic.related_capability_ids.0.is_empty() {
                            ui.add_space(12.0);
                            widgets::caption(
                                ui,
                                format!(
                                    "Related capabilities: {}",
                                    topic
                                        .related_capability_ids
                                        .0
                                        .iter()
                                        .map(|id| id.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ),
                            );
                        }
                        if !topic.capabilities.0.is_empty() {
                            ui.collapsing("Capability reference", |ui| {
                                for capability in &topic.capabilities.0 {
                                    ui.label(
                                        RichText::new(format!(
                                            "{} · {}",
                                            capability.operation.0, capability.mode.0
                                        ))
                                        .strong(),
                                    );
                                    ui.label(format!(
                                        "Authority: {} · effect: {} · service: {}",
                                        capability.authority.0,
                                        capability.effect.0,
                                        capability.owner.0
                                    ));
                                    if !capability.available {
                                        widgets::chip(ui, "Unavailable", Tone::Attention);
                                    }
                                    if let Some(reason) = &capability.unavailable_reason {
                                        ui.label(&reason.0);
                                    }
                                    widgets::caption(
                                        ui,
                                        format!(
                                            "Schema {} · input {} · result {}",
                                            capability.schema_revision.as_str(),
                                            capability.input_type.0,
                                            capability.result_type.0
                                        ),
                                    );
                                    widgets::caption(
                                        ui,
                                        format!(
                                            "Input digest {} · result digest {}",
                                            capability.input_schema_digest.as_str(),
                                            capability.result_schema_digest.as_str()
                                        ),
                                    );
                                    ui.add_space(8.0);
                                }
                            });
                        }
                    });
            }
        }
    }
}
fn provenance_ui(ui: &mut Ui, provenance: &HelpProvenance) {
    match provenance {
        HelpProvenance::Bundled { app_version, .. } => {
            widgets::caption(ui, format!("Bundled with app {}", app_version.0));
        }
        HelpProvenance::Package {
            package_id,
            package_version,
            ..
        } => {
            widgets::caption(
                ui,
                format!(
                    "Installed package {} · {}",
                    package_id.as_str(),
                    package_version.0
                ),
            );
        }
        HelpProvenance::Generated {
            app_version,
            registry_revision,
            ..
        } => {
            widgets::caption(
                ui,
                format!(
                    "Generated for app {} · capability revision {}",
                    app_version.0,
                    registry_revision.as_str()
                ),
            );
        }
    }
}

fn operation_label(state: devworks_core::protocol::OperationState) -> &'static str {
    use devworks_core::protocol::OperationState::*;
    match state {
        IntentRecorded => "Queued",
        Preflighted => "Ready",
        Dispatching => "Generating",
        ObservedComplete => "Complete",
        Failed => "Failed",
        OutcomeUnknown => "Outcome not confirmed",
        NeedsInput => "Needs input",
        RetryPending => "Waiting to retry",
        CancelledBeforeDispatch => "Cancelled",
    }
}
