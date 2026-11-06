//! The dedicated convention manager. All records and transitions come from shared handlers;
//! this module retains only native selection, source pages and independent authoring drafts.
use crate::widgets::{self, Tone};
use devworks_core::protocol::{data::DataScope, records::*};
use eframe::egui::{self, RichText, Ui};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Workspace,
    Project(String),
}
impl Default for Scope {
    fn default() -> Self {
        Self::Workspace
    }
}
impl Scope {
    pub fn key(&self) -> String {
        match self {
            Self::Workspace => "workspace".into(),
            Self::Project(id) => format!("project:{id}"),
        }
    }
    pub fn wire(&self) -> serde_json::Value {
        match self {
            Self::Workspace => serde_json::json!({"kind":"Workspace"}),
            Self::Project(id) => serde_json::json!({"kind":"Project","projectId":id}),
        }
    }
    pub fn of(scope: &DataScope) -> Self {
        match scope {
            DataScope::Workspace => Self::Workspace,
            DataScope::Project { project_id } => Self::Project(project_id.0.as_str().into()),
        }
    }
}
#[derive(Clone)]
pub struct Target {
    pub id: String,
    pub revision: Option<i64>,
    pub format: usize,
    pub section: Option<ConventionIndexedPath>,
}
#[derive(Default)]
pub struct Reader {
    pub target: Option<Target>,
    pub view: Option<ConventionReadView>,
    pub cursors: Vec<Option<String>>,
    pub loading: bool,
    pub error: Option<String>,
    pub generation: u64,
}
#[derive(Clone)]
pub struct Draft {
    pub identity: u64,
    pub generation: u64,
    pub base: Option<ConventionRecord>,
    pub title: String,
    pub slug: String,
    pub parent: Option<String>,
    pub parent_path: Vec<String>,
    pub order: String,
    pub applicability: Vec<String>,
    pub retired: bool,
    pub markdown: Arc<String>,
    pub raw: Arc<String>,
    pub raw_mode: usize,
    pub markdown_changed: bool,
    pub raw_changed: bool,
    pub body_complete: bool,
    pub loading: bool,
    pub error: Option<String>,
    pub busy: bool,
}
impl Default for Draft {
    fn default() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            identity: NEXT.fetch_add(1, Ordering::Relaxed),
            generation: 0,
            base: None,
            title: String::new(),
            slug: String::new(),
            parent: None,
            parent_path: vec![],
            order: "0".into(),
            applicability: vec![],
            retired: false,
            markdown: Arc::new(String::new()),
            raw: Arc::new(String::new()),
            raw_mode: 0,
            markdown_changed: false,
            raw_changed: false,
            body_complete: true,
            loading: false,
            error: None,
            busy: false,
        }
    }
}
impl Draft {
    pub fn of(record: &ConventionRecord) -> Self {
        Self {
            base: Some(record.clone()),
            title: record.title.0.clone(),
            slug: record.slug.0.clone(),
            parent: record.parent_id.as_ref().map(|id| id.as_str().into()),
            order: record.sort_order.0.clone(),
            applicability: record.applicability.0.iter().map(|s| s.0.clone()).collect(),
            retired: matches!(record.lifecycle, ConventionLifecycle::Retired),
            raw_mode: usize::from(record.raw_content.is_some()),
            body_complete: false,
            loading: true,
            ..Self::default()
        }
    }
}
#[derive(Clone, PartialEq, Eq)]
pub enum Editor {
    New,
    Existing(String),
}
pub struct MoveDraft {
    pub base: ConventionRecord,
    pub scope: Scope,
    pub parent: Option<String>,
    pub order: String,
    pub choices: Vec<ConventionRecord>,
    pub next: Option<String>,
    pub generation: u64,
}
impl MoveDraft {
    pub fn of(record: &ConventionRecord) -> Self {
        Self {
            base: record.clone(),
            scope: Scope::of(&record.scope),
            parent: record.parent_id.as_ref().map(|id| id.as_str().into()),
            order: record.sort_order.0.clone(),
            choices: vec![],
            next: None,
            generation: 0,
        }
    }
}
#[derive(Default)]
pub struct ScopeState {
    pub items: Vec<ConventionRecord>,
    pub hits: Vec<ConventionSearchHit>,
    pub listing: bool,
    pub available: bool,
    pub loaded: bool,
    pub list_error: Option<String>,
    pub next: Option<String>,
    pub scope_revision: Option<i64>,
    pub list_generation: u64,
    pub text: String,
    pub applicability: Vec<String>,
    pub lifecycle: usize,
    pub order: usize,
    pub top_k: String,
    pub searched: bool,
    pub retrieval: Option<ConventionRetrieval>,
    pub reader: Reader,
    pub history: Vec<Target>,
    pub editor: Option<Editor>,
    pub new_draft: Option<Draft>,
    pub drafts: BTreeMap<String, Draft>,
    pub detail_tab: usize,
    pub versions: Vec<ConventionRecord>,
    pub version_next: Option<String>,
    pub version_loading: bool,
    pub resources: Vec<ConventionResourceRecord>,
    pub resource_next: Option<String>,
    pub resource_loading: bool,
    pub operation_ids: Vec<String>,
    pub operation_input: String,
    pub operation_text: BTreeMap<String, String>,
    pub operation_revisions: BTreeMap<String, i64>,
    pub notice: Option<(Tone, String)>,
    pub busy: bool,
    pub transfer: Option<String>,
    pub move_drafts: BTreeMap<String, MoveDraft>,
    pub resources_all: bool,
    pub upload_base: Option<ConventionRecord>,
    pub resource_base: Option<ConventionRecord>,
    pub resource_name: String,
    pub resource_retired: bool,
    pub upload_path: Option<std::path::PathBuf>,
    pub resource_edit: Option<ConventionResourceRecord>,
    pub confirm_delete: Option<Delete>,
}
impl ScopeState {
    pub fn draft(&self) -> Option<&Draft> {
        match self.editor.as_ref()? {
            Editor::New => self.new_draft.as_ref(),
            Editor::Existing(id) => self.drafts.get(id),
        }
    }
    pub fn draft_mut(&mut self) -> Option<&mut Draft> {
        match self.editor.clone()? {
            Editor::New => self.new_draft.as_mut(),
            Editor::Existing(id) => self.drafts.get_mut(&id),
        }
    }
    pub fn record(&self) -> Option<&ConventionRecord> {
        self.reader.view.as_ref().map(|v| &v.convention)
    }
}
#[derive(Clone)]
pub enum Delete {
    Convention(ConventionRecord),
    Resource(ConventionResourceRecord),
}
#[derive(Default)]
pub struct ConventionsScreen {
    pub scope: Scope,
    pub initialized: bool,
    pub states: BTreeMap<Scope, ScopeState>,
    pub projects: Vec<(String, String)>,
    pub generation: u64,
}
pub enum Action {
    Knowledge(usize),
    Scope(Scope),
    Refresh,
    List(bool),
    Search,
    Read(Target),
    Page(bool),
    Back,
    New,
    Edit,
    Save,
    Discard,
    Rebase,
    Versions(bool),
    Resources(bool),
    Import,
    Export(usize),
    Applicability,
    Retire,
    Delete(Delete),
    ConfirmDelete,
    Move,
    MoveChoices(bool),
    Upload,
    Download(ConventionResourceRecord),
    UpdateResource(ConventionResourceRecord),
    SaveResource,
    ReplaceResource,
    CommitUpload,
    Index(Option<ConventionResourceRecord>),
    IndexAll,
    InspectOperation(String),
    CancelOperation(String),
    EmbeddingSetup,
}
fn field(ui: &mut Ui, label: &str, value: &mut String) -> bool {
    ui.label(label);
    let response = ui.add(
        egui::TextEdit::singleline(value)
            .id_salt(label)
            .desired_width(220.0),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, label));
    response.changed()
}
fn labels(ui: &mut Ui, values: &mut Vec<String>, owner: &str) -> bool {
    let mut changed = false;
    let mut remove = None;
    for (index, value) in values.iter_mut().enumerate() {
        ui.push_id((owner, index), |ui| {
            ui.horizontal(|ui| {
                let response = ui.add(egui::TextEdit::singleline(value).desired_width(180.0));
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::Role::TextInput,
                        true,
                        format!("{owner} {}", index + 1),
                    )
                });
                changed |= response.changed();
                if ui
                    .small_button(format!("Remove {owner} {}", index + 1))
                    .clicked()
                {
                    remove = Some(index);
                }
            })
        });
    }
    if let Some(index) = remove {
        values.remove(index);
        changed = true;
    }
    if ui.small_button(format!("Add {owner}")).clicked() {
        values.push(String::new());
        changed = true;
    }
    changed
}
fn scope_picker(
    ui: &mut Ui,
    label: &str,
    scope: &mut Scope,
    projects: &[(String, String)],
) -> bool {
    let before = scope.clone();
    let title = match scope {
        Scope::Workspace => "Workspace".into(),
        Scope::Project(id) => projects
            .iter()
            .find(|(p, _)| p == id)
            .map_or_else(|| id.clone(), |(_, name)| name.clone()),
    };
    egui::ComboBox::from_label(label)
        .selected_text(title)
        .show_ui(ui, |ui| {
            ui.selectable_value(scope, Scope::Workspace, "Workspace");
            for (id, name) in projects {
                ui.selectable_value(scope, Scope::Project(id.clone()), name);
            }
        });
    *scope != before
}
impl ConventionsScreen {
    pub fn state(&self) -> Option<&ScopeState> {
        self.states.get(&self.scope)
    }
    pub fn state_mut(&mut self) -> &mut ScopeState {
        self.states.entry(self.scope.clone()).or_default()
    }
    pub fn ui(&mut self, ui: &mut Ui, online: bool, actions: &mut Vec<Action>) {
        let projects = self.projects.clone();
        let mut scope = self.scope.clone();
        let state = self.states.entry(self.scope.clone()).or_default();
        widgets::pane(
            ui,
            widgets::Header {
                title: "Conventions",
                summary: "Versioned guidance, source documents and retrieval resources.",
                tabs: crate::route::Route::Knowledge.screen().subviews,
                tab: 2,
            },
            &mut (state, actions),
            |ui, (s, a)| {
                if ui
                    .button(if s.new_draft.is_some() {
                        "Resume new convention"
                    } else {
                        "New convention"
                    })
                    .clicked()
                {
                    a.push(Action::New);
                }
            },
            |ui, (s, a), tab| {
                if tab != 2 {
                    a.push(Action::Knowledge(tab));
                }
                ui.horizontal_wrapped(|ui| {
                    if scope_picker(ui, "Convention scope", &mut scope, &projects) {
                        a.push(Action::Scope(scope.clone()));
                    }
                    if ui
                        .add_enabled(
                            online && !s.listing,
                            egui::Button::new("Refresh conventions"),
                        )
                        .clicked()
                    {
                        a.push(Action::Refresh);
                    }
                    if ui
                        .add_enabled(
                            online && s.available && !s.busy,
                            egui::Button::new("Import convention"),
                        )
                        .clicked()
                    {
                        a.push(Action::Import);
                    }
                    if ui
                        .add_enabled(
                            online && s.available && !s.busy && s.scope_revision.is_some(),
                            egui::Button::new("Index this scope"),
                        )
                        .clicked()
                    {
                        a.push(Action::IndexAll);
                    }
                    if ui.small_button("Embedding setup").clicked() {
                        a.push(Action::EmbeddingSetup);
                    }
                });
                if !online {
                    widgets::chip(ui, "Offline — drafts are kept", Tone::Attention);
                }
                if let Some((tone, message)) = &s.notice {
                    widgets::chip(ui, message, *tone);
                }
                if let Some(error) = &s.list_error {
                    widgets::chip(ui, error, Tone::Attention);
                }
                if let Some(transfer) = &s.transfer {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(transfer);
                    });
                }
                widgets::with_right_sidebar(
                    ui,
                    300.0,
                    &mut (&mut **s, &mut **a),
                    |ui, (s, a)| {
                        egui::ScrollArea::vertical()
                            .id_salt("convention-detail")
                            .show(ui, |ui| detail(ui, s, &projects, online, a));
                    },
                    |ui, (s, a)| {
                        widgets::secondary_pane(ui, "Find conventions", |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("convention-list")
                                .show(ui, |ui| browse(ui, s, online, a))
                        });
                    },
                );
            },
        );
    }
}
fn browse(ui: &mut Ui, s: &mut ScopeState, online: bool, a: &mut Vec<Action>) {
    field(ui, "Find convention", &mut s.text);
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(
                online && !s.listing,
                egui::Button::new("Search conventions"),
            )
            .clicked()
        {
            a.push(Action::Search);
        }
        if ui
            .add_enabled(
                online && !s.listing,
                egui::Button::new("Browse conventions"),
            )
            .clicked()
        {
            a.push(Action::List(false));
        }
    });
    egui::CollapsingHeader::new("Convention filters").show(ui, |ui| {
        egui::ComboBox::from_label("Lifecycle filter")
            .selected_text(["Active", "Retired", "Active and retired", "Deleted"][s.lifecycle])
            .show_ui(ui, |ui| {
                for (i, name) in ["Active", "Retired", "Active and retired", "Deleted"]
                    .iter()
                    .enumerate()
                {
                    ui.selectable_value(&mut s.lifecycle, i, *name);
                }
            });
        egui::ComboBox::from_label("Convention ordering")
            .selected_text(["Title", "Organization"][s.order])
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut s.order, 0, "Title");
                ui.selectable_value(&mut s.order, 1, "Organization");
            });
        field(ui, "Search result limit", &mut s.top_k);
        labels(ui, &mut s.applicability, "Applicability filter");
    });
    if s.listing {
        ui.spinner();
    }
    if s.loaded && s.items.is_empty() && s.hits.is_empty() {
        ui.label("No conventions matched this scope and filter.");
    }
    if let Some(retrieval) = &s.retrieval {
        widgets::caption(ui, format!("Search: {:?}", retrieval.mode));
        if let Some(reason) = &retrieval.reason {
            ui.label(&reason.0);
        }
        for gap in &retrieval.gaps.0 {
            widgets::caption(ui, &gap.0);
        }
        if retrieval.more_gaps {
            widgets::caption(ui, "Additional retrieval gaps are recorded.");
        }
    }
    if s.searched {
        for (index, hit) in s.hits.iter().enumerate() {
            ui.push_id(("hit", index), |ui| {
                if ui.button(&hit.convention.title.0).clicked() {
                    a.push(Action::Read(Target {
                        id: hit.convention.id.as_str().into(),
                        revision: Some(hit.convention.revision.get()),
                        format: 0,
                        section: None,
                    }));
                }
                widgets::caption(
                    ui,
                    format!(
                        "{} · revision {} · score {}",
                        hit.convention.slug.0,
                        hit.convention.revision.get(),
                        serde_json::to_string(&hit.score).expect("finite score")
                    ),
                );
                if let Some(resource) = &hit.resource {
                    widgets::caption(
                        ui,
                        format!(
                            "Resource: {} · revision {}",
                            resource.name.0,
                            resource.revision.get()
                        ),
                    );
                }
                ui.add(egui::Label::new(&hit.excerpt.0).selectable(true).wrap());
                ui.separator();
            });
        }
    } else {
        for record in &s.items {
            ui.push_id(record.id.as_str(), |ui| {
                if ui.button(&record.title.0).clicked() {
                    a.push(Action::Read(Target {
                        id: record.id.as_str().into(),
                        revision: Some(record.revision.get()),
                        format: 0,
                        section: None,
                    }));
                }
                widgets::caption(
                    ui,
                    format!(
                        "{} · revision {} · {:?}",
                        record.slug.0,
                        record.revision.get(),
                        record.lifecycle
                    ),
                );
                if let Some(parent) = &record.parent_id {
                    widgets::caption(
                        ui,
                        format!(
                            "Within {}",
                            s.items
                                .iter()
                                .find(|row| &row.id == parent)
                                .map_or(parent.as_str(), |row| row.title.0.as_str())
                        ),
                    );
                }
                widgets::caption(
                    ui,
                    format!(
                        "Earlier approval marker: {}",
                        if record.legacy_approved {
                            "present"
                        } else {
                            "absent"
                        }
                    ),
                );
                widgets::caption(ui, format!("Index: {:?}", record.index.status));
                ui.separator();
            });
        }
    }
    if ui
        .add_enabled(
            online && !s.listing && s.next.is_some(),
            egui::Button::new("More conventions"),
        )
        .clicked()
    {
        a.push(Action::List(true));
    }
}
fn detail(
    ui: &mut Ui,
    s: &mut ScopeState,
    projects: &[(String, String)],
    online: bool,
    a: &mut Vec<Action>,
) {
    widgets::tab_row(
        ui,
        "Convention detail",
        &["Read", "Edit", "Versions", "Resources", "Operations"],
        &mut s.detail_tab,
    );
    ui.separator();
    match s.detail_tab {
        0 => reader(ui, s, online, a),
        1 => editor(ui, s, online, a),
        2 => versions(ui, s, online, a),
        3 => resources(ui, s, online, a),
        _ => operations(ui, s, online, a),
    }
    if let Some(delete) = s.confirm_delete.clone() {
        ui.separator();
        let title = match &delete {
            Delete::Convention(c) => {
                format!("Delete {} at revision {}?", c.title.0, c.revision.get())
            }
            Delete::Resource(r) => format!(
                "Delete resource {} at revision {}?",
                r.name.0,
                r.revision.get()
            ),
        };
        widgets::chip(ui, title, Tone::Attention);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    online && !s.busy,
                    egui::Button::new("Confirm convention deletion"),
                )
                .clicked()
            {
                a.push(Action::ConfirmDelete);
            }
            if ui.button("Keep convention content").clicked() {
                s.confirm_delete = None;
            }
        });
    }
    if let Some(record) = s.record().cloned() {
        egui::CollapsingHeader::new("Move and organize").show(ui,|ui|{
            let draft=s.move_drafts.entry(record.id.as_str().into()).or_insert_with(||MoveDraft::of(&record));
            widgets::caption(ui,format!("Move draft for {} at revision {}",draft.base.title.0,draft.base.revision.get()));
            if scope_picker(ui,"Move destination",&mut draft.scope,projects){draft.choices.clear();draft.next=None;draft.generation=draft.generation.wrapping_add(1);}
            if ui.add_enabled(online,egui::Button::new("Load destination parents")).clicked(){a.push(Action::MoveChoices(false));}
            let parent=draft.parent.as_ref().map_or("Root",|id|draft.choices.iter().find(|r|r.id.as_str()==id).map_or(id.as_str(),|r|r.title.0.as_str()));
            egui::ComboBox::from_label("Parent convention").selected_text(parent).show_ui(ui,|ui|{
                ui.selectable_value(&mut draft.parent,None,"Root");for parent in &draft.choices {ui.selectable_value(&mut draft.parent,Some(parent.id.as_str().into()),&parent.title.0);}
            });
            if draft.next.is_some()&&ui.button("More destination parents").clicked(){a.push(Action::MoveChoices(true));}
            field(ui,"Convention sort order",&mut draft.order);
            if record.revision.get()!=draft.base.revision.get(){widgets::chip(ui,"This move draft keeps its opening revision. Inspect and rebase before applying a changed target.",Tone::Attention);}
            if ui.add_enabled(online&&s.available&&!s.busy,egui::Button::new("Move convention")).clicked(){a.push(Action::Move);}
        });
    }
}
fn reader(ui: &mut Ui, s: &mut ScopeState, online: bool, a: &mut Vec<Action>) {
    if s.reader.loading {
        ui.spinner();
        ui.label("Reading the exact convention revision…");
    }
    if let Some(error) = &s.reader.error {
        widgets::chip(ui, error, Tone::Attention);
    }
    let Some(view) = &s.reader.view else {
        ui.label("Choose a convention, or create a new draft.");
        return;
    };
    let record = &view.convention;
    ui.heading(&record.title.0);
    widgets::caption(
        ui,
        format!(
            "{} · revision {} · {:?}",
            record.slug.0,
            record.revision.get(),
            record.lifecycle
        ),
    );
    ui.horizontal_wrapped(|ui| {
        if !s.history.is_empty() && ui.button("Back to previous convention").clicked() {
            a.push(Action::Back);
        }
        for (label, format) in [("Read Markdown", 0), ("Read raw source", 1)] {
            if ui
                .add_enabled(online && !s.reader.loading, egui::Button::new(label))
                .clicked()
            {
                a.push(Action::Read(Target {
                    id: record.id.as_str().into(),
                    revision: Some(record.revision.get()),
                    format,
                    section: None,
                }));
            }
        }
        if ui.button("Edit convention").clicked() {
            a.push(Action::Edit);
        }
        if ui
            .add_enabled(online && !s.busy, egui::Button::new("Index convention"))
            .clicked()
        {
            a.push(Action::Index(None));
        }
    });
    if let Some(reason) = &view.reason {
        widgets::chip(ui, &reason.0, Tone::Attention);
    }
    if let Some(passage) = &view.passage {
        widgets::caption(
            ui,
            format!(
                "{:?} source · passage offset {}",
                view.format,
                passage.offset.get()
            ),
        );
        ui.add(
            egui::Label::new(RichText::new(&passage.text.0).monospace())
                .selectable(true)
                .wrap(),
        );
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    online && !s.reader.loading && s.reader.cursors.len() > 1,
                    egui::Button::new("Previous convention passage"),
                )
                .clicked()
            {
                a.push(Action::Page(false));
            }
            if ui
                .add_enabled(
                    online && !s.reader.loading && passage.has_more,
                    egui::Button::new("Next convention passage"),
                )
                .clicked()
            {
                a.push(Action::Page(true));
            }
        });
    }
    egui::CollapsingHeader::new("Version and provenance").show(ui, |ui| {
        widgets::caption(
            ui,
            format!(
                "Author {} · created {} · updated {}",
                record.author_id.0.as_str(),
                record.created_at_ms.get(),
                record.updated_at_ms.get()
            ),
        );
        match &record.provenance {
            ConventionProvenance::Authored { actor_id } => {
                ui.label(format!("Authored by {}", actor_id.0.as_str()));
            }
            ConventionProvenance::Imported {
                actor_id,
                source,
                schema,
                imported_at_ms,
            } => {
                ui.label(format!(
                    "Imported by {} at {}",
                    actor_id.0.as_str(),
                    imported_at_ms.get()
                ));
                ui.label(format!(
                    "Source {} · version {} · {}",
                    source.artifact_id.as_str(),
                    source.version_id.as_str(),
                    source.sha256.as_str()
                ));
                ui.label(format!(
                    "Import format {} · revision {}",
                    schema.schema_id.as_str(),
                    schema.revision.get()
                ));
            }
        }
        widgets::caption(ui, format!("Index: {:?}", record.index.status));
        if let Some(reason) = &record.index.reason {
            ui.label(&reason.0);
        }
        if let Some(revision) = &record.index.source_revision {
            widgets::caption(ui, format!("Index source revision {}", revision.get()));
        }
        for value in &record.applicability.0 {
            widgets::chip(ui, &value.0, Tone::Neutral);
        }
    });
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(
                online && !s.busy,
                egui::Button::new(
                    if matches!(record.lifecycle, ConventionLifecycle::Retired) {
                        "Activate convention"
                    } else {
                        "Retire convention"
                    },
                ),
            )
            .clicked()
        {
            a.push(Action::Retire);
        }
    });
    ui.horizontal_wrapped(|ui| {
        for (label, format) in [
            ("Export portable convention", 2),
            ("Download Markdown", 0),
            ("Download raw source", 1),
        ] {
            if ui
                .add_enabled(online && !s.busy, egui::Button::new(label))
                .clicked()
            {
                a.push(Action::Export(format));
            }
        }
        if ui
            .add_enabled(online && !s.busy, egui::Button::new("Delete convention"))
            .clicked()
        {
            a.push(Action::Delete(Delete::Convention(record.clone())));
        }
    });
}
fn editor(ui: &mut Ui, s: &mut ScopeState, online: bool, a: &mut Vec<Action>) {
    let available = s.available;
    let current = s.record().cloned();
    let Some(draft) = s.draft_mut() else {
        ui.label("Choose Edit convention or New convention to start a draft.");
        return;
    };
    if let Some(base) = &draft.base {
        widgets::caption(
            ui,
            format!(
                "Draft based on revision {}. Refreshing does not change its opening revision.",
                base.revision.get()
            ),
        );
    } else {
        widgets::caption(
            ui,
            "New convention draft — no convention exists until import is confirmed.",
        );
    }
    let mut changed = false;
    changed |= field(ui, "Convention title", &mut draft.title);
    changed |= field(ui, "Convention slug", &mut draft.slug);
    if draft.base.is_none() {
        changed |= labels(ui, &mut draft.parent_path, "Parent path component");
        changed |= field(ui, "Initial sort order", &mut draft.order);
    }
    changed |= labels(ui, &mut draft.applicability, "Applicability label");
    changed |= ui
        .checkbox(&mut draft.retired, "Retired convention")
        .changed();
    if draft.loading {
        ui.spinner();
        ui.label("Loading the complete document before editing…");
    }
    if let Some(error) = &draft.error {
        widgets::chip(ui, error, Tone::Attention);
    }
    ui.label("Markdown source");
    let response = ui.add_enabled(
        draft.body_complete && !draft.loading,
        egui::TextEdit::multiline(Arc::<String>::make_mut(&mut draft.markdown))
            .id_salt("convention-markdown")
            .desired_rows(14)
            .desired_width(f32::INFINITY)
            .code_editor(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::Role::TextInput,
            draft.body_complete,
            "Convention Markdown",
        )
    });
    if response.changed() {
        draft.markdown_changed = true;
        changed = true;
    }
    egui::ComboBox::from_label("Raw source change")
        .selected_text(
            ["Preserve raw source", "Set raw source", "Remove raw source"][draft.raw_mode],
        )
        .show_ui(ui, |ui| {
            for (i, label) in ["Preserve raw source", "Set raw source", "Remove raw source"]
                .iter()
                .enumerate()
            {
                changed |= ui
                    .selectable_value(&mut draft.raw_mode, i, *label)
                    .changed();
            }
        });
    if draft.raw_mode == 1 {
        let response = ui.add_enabled(
            draft.body_complete && !draft.loading,
            egui::TextEdit::multiline(Arc::<String>::make_mut(&mut draft.raw))
                .id_salt("convention-raw")
                .desired_rows(8)
                .desired_width(f32::INFINITY)
                .code_editor(),
        );
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::Role::TextInput,
                draft.body_complete,
                "Convention raw source",
            )
        });
        if response.changed() {
            draft.raw_changed = true;
            changed = true;
        }
    }
    if draft.base.is_some()
        && ui
            .add_enabled(
                online && available && !draft.busy && !draft.loading,
                egui::Button::new("Apply applicability and lifecycle"),
            )
            .clicked()
    {
        a.push(Action::Applicability);
    }
    if changed {
        draft.generation = draft.generation.wrapping_add(1);
    }
    if let Some(current) = current
        && draft.base.as_ref().is_some_and(|base| {
            base.id == current.id && base.revision.get() != current.revision.get()
        })
    {
        widgets::chip(
            ui,
            format!(
                "Inspected revision {} differs from this draft. Your draft is retained.",
                current.revision.get()
            ),
            Tone::Attention,
        );
        if ui
            .add_enabled(
                !draft.busy,
                egui::Button::new("Keep draft and rebase to inspected revision"),
            )
            .clicked()
        {
            a.push(Action::Rebase);
        }
    }
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(
                online && available && !draft.busy && draft.body_complete && !draft.loading,
                egui::Button::new(if draft.base.is_some() {
                    "Save convention revision"
                } else {
                    "Create convention"
                }),
            )
            .clicked()
        {
            a.push(Action::Save);
        }
        if ui
            .add_enabled(!draft.busy, egui::Button::new("Discard convention draft"))
            .clicked()
        {
            a.push(Action::Discard);
        }
    });
}
fn versions(ui: &mut Ui, s: &mut ScopeState, online: bool, a: &mut Vec<Action>) {
    if ui
        .add_enabled(
            online && !s.version_loading && s.record().is_some(),
            egui::Button::new("Load convention versions"),
        )
        .clicked()
    {
        a.push(Action::Versions(false));
    }
    if s.version_loading {
        ui.spinner();
    }
    for record in &s.versions {
        if ui
            .button(format!(
                "Inspect version {} · {}",
                record.revision.get(),
                record.title.0
            ))
            .clicked()
        {
            a.push(Action::Read(Target {
                id: record.id.as_str().into(),
                revision: Some(record.revision.get()),
                format: 0,
                section: None,
            }));
        }
    }
    if ui
        .add_enabled(
            online && !s.version_loading && s.version_next.is_some(),
            egui::Button::new("More convention versions"),
        )
        .clicked()
    {
        a.push(Action::Versions(true));
    }
}
fn resources(ui: &mut Ui, s: &mut ScopeState, online: bool, a: &mut Vec<Action>) {
    ui.checkbox(
        &mut s.resources_all,
        "Resources from all conventions in this scope",
    );
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(
                online && !s.resource_loading && (s.resources_all || s.record().is_some()),
                egui::Button::new("Load convention resources"),
            )
            .clicked()
        {
            a.push(Action::Resources(false));
        }
        if ui
            .add_enabled(
                online && s.available && !s.busy && s.record().is_some(),
                egui::Button::new("Upload convention resource"),
            )
            .clicked()
        {
            a.push(Action::Upload);
        }
    });
    if s.resource_loading {
        ui.spinner();
    }
    for resource in &s.resources {
        ui.push_id(resource.id.as_str(), |ui| {
            ui.heading(&resource.name.0);
            widgets::caption(
                ui,
                format!(
                    "Resource revision {} · convention revision {} · {:?}",
                    resource.revision.get(),
                    resource.convention_revision.get(),
                    resource.lifecycle
                ),
            );
            widgets::caption(
                ui,
                format!(
                    "{} · {} bytes · {:?}",
                    resource.content.media_type,
                    resource.content.byte_length.get(),
                    resource.index.status
                ),
            );
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(online && !s.busy, egui::Button::new("Download resource"))
                    .clicked()
                {
                    a.push(Action::Download(resource.clone()));
                }
                if ui
                    .add_enabled(online && !s.busy, egui::Button::new("Edit resource"))
                    .clicked()
                {
                    a.push(Action::UpdateResource(resource.clone()));
                }
                if ui
                    .add_enabled(online && !s.busy, egui::Button::new("Index resource"))
                    .clicked()
                {
                    a.push(Action::Index(Some(resource.clone())));
                }
                if ui
                    .add_enabled(online && !s.busy, egui::Button::new("Delete resource"))
                    .clicked()
                {
                    a.push(Action::Delete(Delete::Resource(resource.clone())));
                }
            });
        });
    }
    if let Some(path) = &s.upload_path {
        widgets::caption(ui, format!("Selected resource file: {}", path.display()));
        field(ui, "New resource name", &mut s.resource_name);
        if ui
            .add_enabled(
                online && !s.busy,
                egui::Button::new("Attach selected resource"),
            )
            .clicked()
        {
            a.push(Action::CommitUpload);
        }
    }
    if let Some(resource) = &s.resource_edit {
        widgets::caption(
            ui,
            format!(
                "Editing resource {} at revision {}",
                resource.name.0,
                resource.revision.get()
            ),
        );
        field(ui, "Resource name", &mut s.resource_name);
        ui.checkbox(&mut s.resource_retired, "Retired resource");
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    online && !s.busy,
                    egui::Button::new("Save resource metadata"),
                )
                .clicked()
            {
                a.push(Action::SaveResource);
            }
            if ui
                .add_enabled(
                    online && !s.busy,
                    egui::Button::new("Replace resource bytes"),
                )
                .clicked()
            {
                a.push(Action::ReplaceResource);
            }
        });
    }
    if ui
        .add_enabled(
            online && !s.resource_loading && s.resource_next.is_some(),
            egui::Button::new("More convention resources"),
        )
        .clicked()
    {
        a.push(Action::Resources(true));
    }
}
fn operations(ui: &mut Ui, s: &mut ScopeState, online: bool, a: &mut Vec<Action>) {
    field(ui, "Convention operation ID", &mut s.operation_input);
    if ui
        .add_enabled(
            online && !s.operation_input.is_empty(),
            egui::Button::new("Inspect convention operation"),
        )
        .clicked()
    {
        a.push(Action::InspectOperation(s.operation_input.clone()));
    }
    for id in &s.operation_ids {
        ui.push_id(id, |ui| {
            ui.label(id);
            if let Some(text) = s.operation_text.get(id) {
                ui.label(text);
            }
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(online, egui::Button::new("Refresh convention operation"))
                    .clicked()
                {
                    a.push(Action::InspectOperation(id.clone()));
                }
                if ui
                    .add_enabled(
                        online && !s.busy,
                        egui::Button::new("Cancel convention operation"),
                    )
                    .clicked()
                {
                    a.push(Action::CancelOperation(id.clone()));
                }
            });
        });
    }
}
