//! The native Documents tab displays exact retained versions and gateway results.
use crate::{
    knowledge_screen::GENERAL,
    paged::Paged,
    widgets::{self, Tone},
};
use devworks_core::protocol::{
    Id, Revision,
    data::{
        Citation, CitationSource, DataScope, Locator, QueryCompletion, RetrievalMode, RetrievalPage,
    },
    provider::ArtifactRef,
    records::{DocumentDetail, ResolvedDocumentCitation},
};
use eframe::egui::{self, Ui};
use std::path::PathBuf;

pub const TAB: usize = 1;

#[derive(Clone, Copy)]
pub enum Change {
    Index,
    Reextract,
    Retry,
    Cancel,
    Share,
    Move,
    Disable,
    Remove,
}
impl Change {
    pub fn label(self) -> &'static str {
        match self {
            Self::Index => "Index document",
            Self::Reextract => "Re-extract version",
            Self::Retry => "Retry indexing",
            Self::Cancel => "Cancel indexing",
            Self::Share => "Share document",
            Self::Move => "Move document",
            Self::Disable => "Disable retrieval",
            Self::Remove => "Remove index",
        }
    }
}

pub enum Action {
    Scope(Option<String>),
    Reload,
    More,
    Pick { replacement: bool },
    Drop(Vec<PathBuf>),
    Select(ArtifactRef),
    MoreVersions,
    Search,
    Follow(Citation),
    ViewOriginal(ArtifactRef),
    ExportOriginal(ArtifactRef, String),
    Change(Change),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ImportPlan {
    pub title: String,
    pub scope: DataScope,
    pub replacement: Option<(Id, Revision)>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StagedImport {
    pub plan: ImportPlan,
    pub frame: Vec<u8>,
}

pub enum Upload {
    Preparing(ImportPlan),
    Storing(StagedImport),
    Unconfirmed(StagedImport),
}

#[derive(Default)]
pub struct DocumentsScreen {
    pub list: Paged<DocumentDetail>,
    pub project: Option<String>,
    pub selected: Option<ArtifactRef>,
    pub detail: Option<DocumentDetail>,
    pub target_project: Option<String>,
    pub text: String,
    pub exact: bool,
    pub hybrid: bool,
    pub fallback: bool,
    pub results: Option<RetrievalPage>,
    pub citation: Option<ResolvedDocumentCitation>,
    pub original: Option<String>,
    pub message: Option<(Tone, String)>,
    pub writing: bool,
    pub upload: Option<Upload>,
    pub read_generation: u64,
    pub search_generation: u64,
    pub citation_generation: u64,
    pub source_generation: u64,
}

impl DocumentsScreen {
    pub fn scope(&mut self, project: Option<&str>) -> bool {
        if self.project.as_deref() == project {
            return false;
        }
        self.project = project.map(str::to_owned);
        self.list.reset();
        self.select(None);
        self.search_generation += 1;
        self.results = None;
        self.message = None;
        true
    }

    pub fn select(&mut self, version: Option<ArtifactRef>) {
        self.selected = version;
        self.detail = None;
        self.original = None;
        self.citation = None;
        self.read_generation += 1;
        self.source_generation += 1;
        self.citation_generation += 1;
    }

    pub fn busy(&self) -> bool {
        self.writing || self.upload.is_some()
    }

    pub fn staged_import(&self) -> Option<&StagedImport> {
        match self.upload.as_ref()? {
            Upload::Storing(saved) | Upload::Unconfirmed(saved) => Some(saved),
            Upload::Preparing(_) => None,
        }
    }

    pub fn main(
        &mut self,
        ui: &mut Ui,
        projects: &[(String, String)],
        online: Result<(), &str>,
        actions: &mut Vec<Action>,
    ) {
        if let Err(reason) = online {
            ui.label(reason);
            return;
        }
        let mut scope = self.project.clone();
        project_picker(ui, "Documents scope", &mut scope, projects, GENERAL);
        if scope != self.project {
            actions.push(Action::Scope(scope));
            return;
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!self.busy(), egui::Button::new("Import document…"))
                .clicked()
            {
                actions.push(Action::Pick { replacement: false });
            }
            if ui.button("Refresh documents").clicked() {
                actions.push(Action::Reload);
            }
        });
        if let Some(upload) = &self.upload {
            ui.label(match upload {
                Upload::Preparing(_) => "Preparing the source file…",
                Upload::Storing(_) => "Storing the source file…",
                Upload::Unconfirmed(_) => "The source save is not confirmed. Use Confirm outcomes.",
            });
        } else if self.writing {
            ui.label("Waiting for the document operation…");
        }
        if let Some((tone, message)) = &self.message {
            widgets::chip(ui, message, *tone);
        }
        ui.separator();
        let search =
            ui.add(egui::TextEdit::singleline(&mut self.text).hint_text("Search documents"));
        search.widget_info(|| {
            egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Search documents")
        });
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.exact, "Selected version")
                .on_hover_text("Search the selected retained version");
            ui.checkbox(&mut self.hybrid, "Hybrid");
            if self.hybrid {
                ui.checkbox(&mut self.fallback, "Allow lexical fallback");
            }
            if ui
                .add_enabled(
                    !self.text.trim().is_empty() && (!self.exact || self.selected.is_some()),
                    egui::Button::new("Search documents"),
                )
                .clicked()
            {
                actions.push(Action::Search);
            }
        });
        egui::ScrollArea::vertical()
            .id_salt("document-pane-results")
            .show(ui, |ui| {
                if let Some(results) = &self.results {
                    widgets::caption(
                        ui,
                        match results.effective_mode {
                            RetrievalMode::Lexical => "Lexical results",
                            RetrievalMode::Hybrid => "Hybrid results",
                            RetrievalMode::Vector => "Vector results",
                        },
                    );
                    if results.completion != QueryCompletion::Complete {
                        widgets::caption(ui, format!("Search ended with {:?}", results.completion));
                    }
                    if results.passages.is_empty() {
                        ui.label("No matching passages.");
                    }
                    for passage in &results.passages {
                        ui.push_id(passage.chunk_id.as_str(), |ui| {
                            if ui
                                .button(format!(
                                    "Open citation · {}",
                                    locator(&passage.citation.locator)
                                ))
                                .clicked()
                            {
                                actions.push(Action::Follow(passage.citation.clone()));
                            }
                            widgets::caption(
                                ui,
                                passage.text.chars().take(220).collect::<String>(),
                            );
                        });
                    }
                    ui.separator();
                }
                if self.list.loading() && !self.list.loaded {
                    ui.label("Loading documents…");
                }
                if let Some(error) = &self.list.error {
                    widgets::chip(ui, error, Tone::Attention);
                } else if self.list.loaded && self.list.items.is_empty() {
                    ui.label("No documents in this scope.");
                }
                for row in &self.list.items {
                    ui.push_id(row.id.as_str(), |ui| {
                        let version = row
                            .current_version
                            .as_ref()
                            .or_else(|| row.retained_versions.0.first());
                        if let Some(version) = version {
                            let selected = self
                                .selected
                                .as_ref()
                                .is_some_and(|v| v.artifact_id == row.id);
                            if ui.selectable_label(selected, &row.title.0).clicked() {
                                actions.push(Action::Select(version.clone()));
                            }
                            widgets::caption(
                                ui,
                                if row.retrieval_enabled {
                                    format!("{:?}", row.extraction_state)
                                } else {
                                    "Retrieval disabled".into()
                                },
                            );
                        } else {
                            ui.label(&row.title.0);
                            widgets::caption(ui, "No retained versions");
                        }
                    });
                }
                if self.list.more() && ui.button("More documents").clicked() {
                    actions.push(Action::More);
                }
            });
    }

    pub fn detail(
        &mut self,
        ui: &mut Ui,
        projects: &[(String, String)],
        actions: &mut Vec<Action>,
    ) {
        egui::ScrollArea::vertical().id_salt("document-pane-detail").show(ui, |ui| {
            if let Some(citation) = &self.citation {
                ui.heading("Exact citation");
                widgets::caption(ui, locator(&citation.citation.locator));
                if let CitationSource::Managed { version, extraction_revision } = &citation.citation.source {
                    widgets::caption(ui, format!("Version {} · extraction {}", version.version_id.as_str(), extraction_revision.as_str()));
                }
                ui.label(&citation.text.0);
                ui.separator();
            }
            let Some(detail) = &self.detail else {
                if self.selected.is_some() {
                    ui.label("Loading the selected version…");
                } else {
                    ui.label("Select a document or open a search citation.");
                }
                return;
            };
            ui.heading(&detail.title.0);
            let Some(version) = detail.selected_version.as_ref() else {
                ui.label("This version is unavailable.");
                return;
            };
            widgets::caption(ui, format!("{} · {} bytes", version.media_type, version.byte_length.get()));
            widgets::caption(ui, format!("Version {}", version.version_id.as_str()));
            ui.label(version.sha256.as_str()).on_hover_text("Source SHA-256");
            ui.horizontal_wrapped(|ui| {
                if matches!(version.media_type.as_str(), "text/plain" | "text/markdown")
                    && ui.button("View original text").clicked()
                {
                    actions.push(Action::ViewOriginal(version.clone()));
                }
                if ui.button("Save original…").clicked() {
                    actions.push(Action::ExportOriginal(version.clone(), detail.title.0.clone()));
                }
            });
            if let Some(original) = &self.original {
                ui.label(original);
            }
            ui.separator();
            document_status(ui, detail, projects);
            let owned = match &detail.scope {
                DataScope::Workspace => self.project.is_none(),
                DataScope::Project { project_id } => self.project.as_deref() == Some(project_id.0.as_str()),
            };
            ui.add_enabled_ui(!self.busy(), |ui| {
                if owned {
                    ui.horizontal_wrapped(|ui| {
                        for change in [Change::Index, Change::Retry, Change::Reextract, Change::Disable, Change::Remove] {
                            if ui.button(change.label()).clicked() { actions.push(Action::Change(change)); }
                        }
                        if detail.index_activity.as_ref().is_some_and(|a| matches!(a.stage.as_str(), "Queued" | "Indexing" | "Held"))
                            && ui.button(Change::Cancel.label()).clicked()
                        {
                            actions.push(Action::Change(Change::Cancel));
                        }
                        if detail.current_version.is_some() && ui.button("Replace source…").clicked() {
                            actions.push(Action::Pick { replacement: true });
                        }
                    });
                }
                project_picker(ui, "Destination project", &mut self.target_project, projects, "Choose a project");
                if let Some(target) = &self.target_project
                    && !matches!(&detail.scope, DataScope::Project { project_id } if project_id.0.as_str() == target)
                {
                    ui.horizontal_wrapped(|ui| {
                        for change in [Change::Share, Change::Move] {
                            if ui.button(change.label()).clicked() { actions.push(Action::Change(change)); }
                        }
                    });
                }
            });
            ui.separator();
            widgets::caption(ui, format!("{} retained versions", detail.retained_version_count.get()));
            for retained in &detail.retained_versions.0 {
                if ui.selectable_label(retained.version_id == version.version_id, retained.version_id.as_str()).clicked() {
                    actions.push(Action::Select(retained.clone()));
                }
            }
            if detail.next_version_cursor.is_some() && ui.button("More versions").clicked() {
                actions.push(Action::MoreVersions);
            }
        });
    }
}

fn project_picker(
    ui: &mut Ui,
    label: &str,
    value: &mut Option<String>,
    projects: &[(String, String)],
    empty: &str,
) {
    let name = value
        .as_ref()
        .and_then(|id| projects.iter().find(|(p, _)| p == id))
        .map_or(empty, |(_, name)| name);
    let label_text = label.to_owned();
    egui::ComboBox::from_id_salt(label)
        .selected_text(name)
        .show_ui(ui, |ui| {
            if ui.selectable_label(value.is_none(), empty).clicked() {
                *value = None;
            }
            for (id, name) in projects {
                if ui
                    .selectable_label(value.as_ref() == Some(id), name)
                    .clicked()
                {
                    *value = Some(id.clone());
                }
            }
        })
        .response
        .widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, true, label_text.clone()));
}

fn document_status(ui: &mut Ui, detail: &DocumentDetail, projects: &[(String, String)]) {
    let Some(activity) = &detail.index_activity else {
        ui.label("No index activity for this version.");
        return;
    };
    ui.label(format!("Index · {}", activity.stage.as_str()));
    if let Some(at) = &activity.indexed_at_ms {
        widgets::caption(ui, format!("Indexed {}", widgets::local_time(at.get())));
    }
    if let Some(chunks) = &activity.chunk_count {
        widgets::caption(ui, format!("{} indexed chunks", chunks.get()));
    }
    if !detail.retrieval_enabled {
        widgets::caption(ui, "Retrieval disabled.");
    }
    for gap in &detail.coverage.gaps {
        widgets::caption(ui, gap.as_str());
    }
    if detail.coverage.more_gaps {
        widgets::caption(ui, "Additional diagnostics available in the index record.");
    }
    if !activity.attempts.0.is_empty() {
        ui.collapsing("Model usage for this version", |ui| {
            for attempt in &activity.attempts.0 {
                let model = attempt
                    .provider_model
                    .as_ref()
                    .map_or(attempt.model_id.as_str(), |model| model.0.as_str());
                ui.label(format!(
                    "{} · {} · {}",
                    attempt.purpose.as_str(),
                    model,
                    attempt.state.as_str()
                ));
                widgets::caption(
                    ui,
                    format!(
                        "{} · {}",
                        attempt.endpoint_profile.as_str(),
                        attempt.connection_id.as_str()
                    ),
                );
                let charged = attempt.project_id.as_ref().map_or("Workspace", |project| {
                    projects
                        .iter()
                        .find(|(id, _)| id == project.0.as_str())
                        .map_or(project.0.as_str(), |(_, name)| name)
                });
                widgets::caption(ui, format!("Usage scope · {charged}"));
                let tokens = |value: &Option<devworks_core::protocol::Counter>| {
                    value
                        .as_ref()
                        .map(|v| v.get().to_string())
                        .unwrap_or_else(|| "unknown".into())
                };
                widgets::caption(
                    ui,
                    format!(
                        "{} input tokens · {} output tokens",
                        tokens(&attempt.input_tokens),
                        tokens(&attempt.output_tokens)
                    ),
                );
                let cost = attempt
                    .priced_micro_usd
                    .as_ref()
                    .map(|v| format!("${}.{:06}", v.get() / 1_000_000, v.get() % 1_000_000))
                    .unwrap_or_else(|| "Cost unavailable".into());
                widgets::caption(
                    ui,
                    format!(
                        "{} · {}{}",
                        cost,
                        attempt.pricing_classification.as_str(),
                        if attempt.monetary_complete {
                            ""
                        } else {
                            " · partial"
                        }
                    ),
                );
            }
            if activity.more_attempts {
                widgets::caption(ui, "Showing the latest 32 attempts.");
            }
        });
    }
}

fn locator(locator: &Locator) -> String {
    match locator {
        Locator::Pdf { page } => format!("Page {page}"),
        Locator::Text {
            first_line,
            last_line,
        } if first_line == last_line => format!("Line {first_line}"),
        Locator::Text {
            first_line,
            last_line,
        } => format!("Lines {first_line}–{last_line}"),
        Locator::Docx {
            part, paragraph, ..
        } => format!("{} · paragraph {paragraph}", part.as_str()),
        _ => "Selected passage".into(),
    }
}
