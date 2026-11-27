//! Artifacts, Resources and Browser sessions: three lists, each bound to one query, with a
//! filter and a remembered selection in the route's memory.
use crate::backend::{Backend, Reply, TaskOutput, screen_slot};
use crate::chat;
use crate::memory::RouteMemory;
use crate::ops::{self, Failure};
use crate::paged::{Applied, Fetch, ListText, Page, Paged, list_body};
use crate::route::Route;
use crate::screen::{Cx, Screen, ScreenContext, ScreenId};
use crate::widgets::{self, Header, ViewState};
use devworks_core::gateway::QueryOutcome;
use devworks_core::ports::storage::ProjectPurgeClientScope;
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::{
    ArtifactRecord, BrowserSession, ItemState, LeaseRecord, LeaseState, ResourcePriority,
    ResourceRecord,
};
use eframe::egui::{self, Ui};
use std::collections::{BTreeMap, HashMap};

/// The Artifacts requests the screen waits for. The names are persisted (see
/// [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ArtifactsSlot {
    ArtifactsList,
    /// One project's artifacts (its ID).
    ProjectArtifacts(String),
    /// The preview of one artifact version (its version ID).
    ArtifactPreview(String),
    /// Exporting one artifact version to a file (its version ID).
    ArtifactExport(String),
    /// Replacing one artifact's tags (its ID).
    ArtifactTag(String),
    /// Deleting one artifact (its ID).
    ArtifactDelete(String),
}
screen_slot!(ArtifactsSlot, Artifacts);

/// The Resources requests the screen waits for. The names are persisted.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ResourcesSlot {
    ResourcesList,
    /// The leases holding one resource (its canonical key).
    ResourceHolders(String),
    /// The leases waiting for one resource (its canonical key).
    ResourceWaiters(String),
}
screen_slot!(ResourcesSlot, Resources);

/// The Browser sessions requests the screen waits for. The names are persisted.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum BrowserSlot {
    /// The workspace's browser sessions.
    BrowserSessions,
    /// The browser sessions of one project (its ID).
    ProjectBrowserSessions(String),
    /// Closing a browser session (its ID).
    BrowserClose(String),
}
screen_slot!(BrowserSlot, Browser);

/// What each list screen reports to the coordinator: its route memory.
fn route_context(mem: &RouteMemory) -> ScreenContext {
    ScreenContext {
        tab: mem.tab,
        selection: mem.selection.clone(),
    }
}

/// What a preview shows: a text excerpt, a decoded image, or the facts the preview reports.
#[derive(Clone, Debug)]
pub enum PreviewView {
    Text {
        text: String,
        partial: bool,
    },
    Image {
        size: [usize; 2],
        rgba: Vec<u8>,
        facts: String,
    },
    Facts(String),
}

enum Shown {
    Loading,
    Ready(PreviewView, Option<egui::TextureHandle>),
    Failed(String),
}

/// A request the view asks for once its frame is drawn.
enum Action {
    Preview(ArtifactRecord),
    Export(ArtifactRecord),
    EditTags(ArtifactRecord),
    SaveTags,
    CancelTags,
    Delete(ArtifactRecord),
    ConfirmDelete,
    CancelDelete,
}

#[derive(Default)]
pub struct ArtifactsScreen {
    /// The workspace's artifacts (the Workspace tab).
    pub list: Paged<ArtifactRecord>,
    /// The selected project's artifacts (the Project tab).
    pub project_list: Paged<ArtifactRecord>,
    /// The project `project_list` belongs to.
    project: Option<String>,
    /// Applied artifact mutations (attachment saves, tag changes and deletions). A list query
    /// started before the latest one may miss it: its reply is dropped and the list re-queried,
    /// so it never overwrites newer records.
    pub mutations: u64,
    list_at: u64,
    project_at: u64,
    /// The preview shown, by version ID.
    preview: Option<(String, Shown)>,
    /// The tags being edited: the record as the editor opened it (its revision is what the
    /// save expects, so a concurrent change is refused, never overwritten) and one entry per tag.
    tag_draft: Option<(ArtifactRecord, Vec<String>)>,
    /// The tag entries each in-flight save submitted, by artifact ID: a reply closes the editor
    /// only while it still shows exactly those entries for that artifact.
    /// With them, the revision the save expected, so a draft kept after its own save moves to
    /// the revision that save produced.
    tag_submitted: HashMap<String, (String, Vec<String>)>,
    /// The record whose deletion waits for a second click, as it was when Delete was clicked:
    /// its identities bind the deletion, so a version or reference added since is refused.
    confirm_delete: Option<ArtifactRecord>,
}

impl Screen for ArtifactsScreen {
    type Slot = ArtifactsSlot;
    const ID: ScreenId = ScreenId::Artifacts;
    fn load(&mut self, cx: &mut Cx, _route: Route) {
        self.load(cx);
    }
    fn refresh(&mut self, cx: &mut Cx, _route: Route) {
        if !cx.pending(&ArtifactsSlot::ArtifactsList) {
            self.load_workspace(cx);
        }
        let project = cx.selected_project().map(str::to_owned);
        if project != self.project
            || project
                .as_ref()
                .is_some_and(|p| !cx.pending(&ArtifactsSlot::ProjectArtifacts(p.clone())))
        {
            self.load_project(cx);
        }
    }
    fn apply(&mut self, cx: &mut Cx, slot: ArtifactsSlot, reply: Reply) {
        match (slot, reply) {
            (ArtifactsSlot::ArtifactsList, Reply::Query(_)) if self.list_at != self.mutations => {
                self.load_workspace(cx);
            }
            (ArtifactsSlot::ArtifactsList, Reply::Query(q)) => {
                let applied = self
                    .list
                    .apply_current(ops::artifacts(q).map_err(|e| e.describe()));
                if let Applied::Continue(fetch) = applied {
                    self.send_workspace(cx, fetch);
                }
            }
            (ArtifactsSlot::ProjectArtifacts(p), Reply::Query(_))
                if self.project.as_deref() != Some(p.as_str()) => {}
            (ArtifactsSlot::ProjectArtifacts(_), Reply::Query(_))
                if self.project_at != self.mutations =>
            {
                self.load_project(cx);
            }
            (ArtifactsSlot::ProjectArtifacts(p), Reply::Query(q)) => {
                let applied = self
                    .project_list
                    .apply_current(ops::artifacts(q).map_err(|e| e.describe()));
                if let Applied::Continue(fetch) = applied {
                    Self::send_project(cx, &mut self.project_list, p, fetch, "Loading");
                }
            }
            (ArtifactsSlot::ArtifactPreview(v), Reply::Task(result)) => {
                if let Some((shown, state)) = &mut self.preview
                    && *shown == v
                {
                    *state = match result {
                        Ok(TaskOutput::ArtifactPreview(view)) => Shown::Ready(view, None),
                        Ok(_) => Shown::Failed("unexpected preview reply".into()),
                        Err(e) => Shown::Failed(e),
                    };
                }
            }
            (ArtifactsSlot::ArtifactExport(_), Reply::Task(result)) => match result {
                Ok(TaskOutput::ArtifactExported(path)) => {
                    cx.announce(format!("Exported to {}", path.display()));
                }
                Ok(_) => cx.announce("The export did not finish."),
                Err(e) => cx.announce(format!("The artifact could not be exported: {e}")),
            },
            (ArtifactsSlot::ArtifactTag(id), Reply::Command(c)) => {
                let submitted = self.tag_submitted.remove(&id);
                match ops::artifact_tagged(c) {
                    Ok(summary) => {
                        // A draft opened for another artifact, or edited since, is kept; one that
                        // still derives from this save continues from the revision it produced.
                        let mut close = false;
                        if let (Some((opened, entries)), Some((base, sent))) =
                            (&mut self.tag_draft, &submitted)
                            && opened.artifact.artifact_id.as_str() == id
                            && opened.revision.as_ref().map(|r| r.as_str()) == Some(base.as_str())
                        {
                            if entries == sent {
                                close = true;
                            } else {
                                opened.revision = Some(summary.revision.clone());
                            }
                        }
                        if close {
                            self.tag_draft = None;
                        }
                        cx.announce("Tags saved.");
                    }
                    Err(Failure::Unknown) => cx.announce("Saving tags: outcome not confirmed."),
                    Err(e) => cx.announce(format!("The tags were not saved: {}", e.describe())),
                }
                self.changed();
                self.load(cx);
            }
            (ArtifactsSlot::ArtifactDelete(id), Reply::Command(c)) => {
                match ops::deleted(c) {
                    Ok(()) => {
                        self.preview = None;
                        let mut mem = cx.routes.remove(&Route::Artifacts).unwrap_or_default();
                        if mem.selection.as_deref() == Some(id.as_str()) {
                            mem.selection = None;
                        }
                        cx.routes.insert(Route::Artifacts, mem);
                        cx.announce("Artifact deleted.");
                    }
                    Err(Failure::Unknown) => {
                        cx.announce("Deleting the artifact: outcome not confirmed.")
                    }
                    Err(e) => {
                        cx.announce(format!("The artifact was not deleted: {}", e.describe()))
                    }
                }
                self.changed();
                self.load(cx);
            }
            (slot, reply) => cx.unhandled(&slot, reply),
        }
    }
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        self.view(cx, ui);
    }
    fn remove_purged(&mut self, scope: &ProjectPurgeClientScope) {
        for list in [&mut self.list, &mut self.project_list] {
            list.items
                .retain(|r| !scope.deleted_artifact_ids.contains(&r.artifact.artifact_id));
        }
        if self.project.as_deref() == Some(scope.project_id.0.as_str()) {
            self.project = None;
            self.project_list = Default::default();
        }
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        route_context(mem)
    }
}

impl ArtifactsScreen {
    /// An artifact was stored, retagged or deleted: the next list read supersedes any started
    /// before.
    pub(crate) fn changed(&mut self) {
        self.mutations += 1;
    }
    fn load(&mut self, cx: &mut Cx) {
        self.load_workspace(cx);
        self.load_project(cx);
    }
    fn load_workspace(&mut self, cx: &mut Cx) {
        self.list_at = self.mutations;
        // A re-read covers every page shown, so Load more's pages stay.
        let fetch = self.list.reread();
        self.send_workspace(cx, fetch);
    }
    /// The selected project's artifacts; a newly selected project starts from its first page.
    fn load_project(&mut self, cx: &mut Cx) {
        let project = cx.selected_project().map(str::to_owned);
        if project != self.project {
            self.project = project.clone();
            self.project_list = Default::default();
        }
        let Some(p) = project else {
            return;
        };
        self.project_at = self.mutations;
        let fetch = self.project_list.reread();
        Self::send_project(cx, &mut self.project_list, p, fetch, "Loading");
    }
    fn send_workspace(&mut self, cx: &mut Cx, fetch: Fetch) {
        cx.paged(
            &mut self.list,
            fetch,
            ArtifactsSlot::ArtifactsList,
            "Loading artifacts",
            "artifacts.list",
            ops::workspace_artifacts,
        );
    }
    fn send_project(
        cx: &mut Cx,
        list: &mut Paged<ArtifactRecord>,
        project: String,
        fetch: Fetch,
        verb: &str,
    ) {
        let p = project.clone();
        cx.paged(
            list,
            fetch,
            ArtifactsSlot::ProjectArtifacts(project),
            &format!("{verb} project artifacts"),
            "artifacts.list",
            move |b, cursor| ops::project_artifacts(b, &p, cursor),
        );
    }
    fn load_more(&mut self, cx: &mut Cx, tab: usize) {
        if tab == 1 {
            if let (Some(p), Some(fetch)) = (self.project.clone(), self.project_list.more_page()) {
                Self::send_project(cx, &mut self.project_list, p, fetch, "Loading more");
            }
        } else if let Some(fetch) = self.list.more_page() {
            cx.paged(
                &mut self.list,
                fetch,
                ArtifactsSlot::ArtifactsList,
                "Loading more artifacts",
                "artifacts.list",
                ops::workspace_artifacts,
            );
        }
    }

    fn act(&mut self, cx: &mut Cx, action: Action) {
        match action {
            Action::Preview(record) => {
                let version = record.artifact.version_id.as_str().to_owned();
                let artifact = record.artifact.clone();
                if cx.task(
                    ArtifactsSlot::ArtifactPreview(version.clone()),
                    "Previewing an artifact",
                    move |b| preview(b, &artifact).map(TaskOutput::ArtifactPreview),
                ) {
                    self.preview = Some((version, Shown::Loading));
                }
            }
            Action::Export(record) => {
                let Some(path) = cx.dialogs.save_artifact(&record.name.0) else {
                    return;
                };
                let version = record.artifact.version_id.as_str().to_owned();
                let artifact = record.artifact.clone();
                cx.task(
                    ArtifactsSlot::ArtifactExport(version),
                    "Exporting an artifact",
                    move |b| export(b, &artifact, &path).map(TaskOutput::ArtifactExported),
                );
            }
            Action::EditTags(record) => {
                let tags = record
                    .tags
                    .as_ref()
                    .map(|t| t.0.clone())
                    .unwrap_or_default();
                self.tag_draft = Some((record, tags));
            }
            Action::SaveTags => {
                let Some((record, entries)) = &self.tag_draft else {
                    return;
                };
                let record = record.clone();
                let submitted = entries.clone();
                let tags = draft_tags(entries);
                let Some(revision) = record.revision.as_ref().map(|r| r.as_str().to_owned()) else {
                    cx.announce("Reload the list before editing tags.");
                    return;
                };
                if devworks_core::protocol::data::ArtifactTags::new(tags.clone()).is_err() {
                    cx.announce("Tags must be distinct, at most 32, each 1 to 64 characters.");
                    return;
                }
                let id = record.artifact.artifact_id.as_str().to_owned();
                // One save per artifact at a time, whatever control asked for it.
                if cx.pending(&ArtifactsSlot::ArtifactTag(id.clone())) {
                    return;
                }
                let artifact = id.clone();
                self.tag_submitted
                    .insert(id.clone(), (revision.clone(), submitted));
                cx.command(
                    ArtifactsSlot::ArtifactTag(id),
                    "Saving tags",
                    "artifacts.tag",
                    move |b| ops::artifact_tag(b, &artifact, &revision, tags),
                );
            }
            Action::CancelTags => self.tag_draft = None,
            Action::Delete(record) => self.confirm_delete = Some(record),
            Action::CancelDelete => self.confirm_delete = None,
            Action::ConfirmDelete => {
                let Some(record) = self.confirm_delete.take() else {
                    return;
                };
                let Some(backend) = cx.backend() else {
                    return;
                };
                let Some(frame) = ops::delete_artifact(&*backend, &record) else {
                    cx.announce("Reload the list before deleting.");
                    return;
                };
                cx.command(
                    ArtifactsSlot::ArtifactDelete(record.artifact.artifact_id.as_str().to_owned()),
                    "Deleting an artifact",
                    "artifacts.delete",
                    move |_| frame,
                );
            }
        }
    }

    fn view(&mut self, cx: &mut Cx, ui: &mut Ui) {
        let screen = Route::Artifacts.screen();
        let online = cx.online().map_err(str::to_owned);
        let mut reload = false;
        let mut more = false;
        let mut action: Option<Action> = None;
        let mut mem = cx.routes.remove(&Route::Artifacts).unwrap_or_default();
        let tab = mem.tab;
        let Self {
            list,
            project_list,
            project,
            preview,
            tag_draft,
            tag_submitted,
            confirm_delete,
            ..
        } = self;
        let project_name = project.as_ref().map(|p| {
            cx.projects
                .list
                .items
                .iter()
                .find(|r| r.id.0.as_str() == p)
                .map_or_else(|| p.clone(), |r| r.name.0.clone())
        });
        // Artifacts whose tag save is in flight (`tag_submitted` holds every one of them).
        let saving: Vec<String> = tag_submitted
            .keys()
            .filter(|id| cx.pending(&ArtifactsSlot::ArtifactTag((*id).clone())))
            .cloned()
            .collect();
        let scrolls = &mut *cx.scrolls;
        let mut st = (&mut reload, &mut mem, &mut more, &mut action);
        let (_, tab) = widgets::pane(
            ui,
            Header {
                title: screen.title,
                summary: screen.summary,
                tabs: screen.subviews,
                tab,
            },
            &mut st,
            |ui, st| {
                if widgets::secondary_button(ui, "Refresh").clicked() {
                    *st.0 = true;
                }
            },
            |ui, st, tab| {
                let (list, text) = if tab == 1 {
                    let Some(name) = &project_name else {
                        widgets::state_block(
                            ui,
                            &ViewState::Empty {
                                title: "No project selected".into(),
                                body: "Select a project to see the artifacts it owns.".into(),
                            },
                        );
                        return;
                    };
                    widgets::caption(ui, format!("Artifacts of {name}"));
                    (
                        &*project_list,
                        ListText {
                            what: "project artifacts",
                            empty_title: "No artifacts in this project yet",
                            empty_body: "Files agents and people save in this project are kept here.",
                            id_salt: "project-artifacts-list",
                        },
                    )
                } else {
                    (
                        &*list,
                        ListText {
                            what: "workspace artifacts",
                            empty_title: "No workspace artifacts yet",
                            empty_body: "Attachments you add to a Home conversation are kept here.",
                            id_salt: "artifacts-list",
                        },
                    )
                };
                let mem = &mut *st.1;
                let r = ui.add(
                    egui::TextEdit::singleline(&mut mem.filter)
                        .hint_text("Filter by name, type or tag")
                        .desired_width(280.0),
                );
                r.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Filter artifacts")
                });
                let RouteMemory {
                    filter, selection, ..
                } = mem;
                // The list fills its column's height, so the selected artifact sits beside it.
                ui.columns(2, |cols| {
                    list_body(
                        &mut cols[0],
                        &online,
                        list,
                        text,
                        st.0,
                        st.2,
                        scrolls,
                        |a: &ArtifactRecord| {
                            let tags = a.tags.as_ref().map(|t| t.0.join(", ")).unwrap_or_default();
                            format!("{} {} {}", a.name.0, a.artifact.media_type, tags)
                        },
                        |a: &ArtifactRecord| a.artifact.artifact_id.as_str().to_owned(),
                        filter,
                        selection,
                        artifact_row,
                    );
                    let selected = selection.as_ref().and_then(|id| {
                        list.items
                            .iter()
                            .find(|a| a.artifact.artifact_id.as_str() == id)
                    });
                    let tag_saving = selection.as_ref().is_some_and(|id| saving.contains(id));
                    match selected {
                        // The detail scrolls on its own, so a long tag list keeps its actions
                        // reachable at the smallest window size.
                        Some(record) => {
                            egui::ScrollArea::vertical()
                                .id_salt("artifact-detail")
                                .auto_shrink([false, false])
                                .show(&mut cols[1], |ui| {
                                    detail(
                                        ui,
                                        record,
                                        preview,
                                        tag_draft,
                                        confirm_delete,
                                        tag_saving,
                                        st.3,
                                    )
                                });
                        }
                        None => widgets::caption(
                            &mut cols[1],
                            "Select an artifact to preview, export, tag or delete it.",
                        ),
                    }
                });
            },
        );
        mem.tab = tab;
        cx.routes.insert(Route::Artifacts, mem);
        if let Some(a) = action {
            self.act(cx, a);
        }
        if reload {
            self.load(cx);
        }
        if more {
            self.load_more(cx, tab);
        }
    }
}

fn artifact_row(ui: &mut Ui, a: &ArtifactRecord, selected: bool) -> bool {
    if let Some(source) = &a.tested_repository {
        if let Some(url) = &source.repository_url {
            ui.hyperlink_to(
                &source.head.hex,
                format!("{}/commit/{}", url.0, source.head.hex),
            );
        } else {
            ui.monospace(&source.head.hex);
        }
        widgets::caption(
            ui,
            format!(
                "{} · {}",
                source.repository_id.as_str(),
                if source.dirty {
                    "dirty at observation"
                } else {
                    "clean at observation"
                }
            ),
        );
    }
    let r = ui.selectable_label(
        selected,
        format!(
            "{}  ·  {}  ·  {}",
            a.name.0,
            a.artifact.media_type,
            chat::bytes_label(a.artifact.byte_length.get())
        ),
    );
    let r = match &a.tags {
        Some(t) if !t.0.is_empty() => r.on_hover_text(t.0.join(", ")),
        _ => r,
    };
    r.clicked()
}

/// The selected artifact: its facts, preview, and the export, tag and delete actions.
fn detail(
    ui: &mut Ui,
    record: &ArtifactRecord,
    preview: &mut Option<(String, Shown)>,
    tag_draft: &mut Option<(ArtifactRecord, Vec<String>)>,
    confirm_delete: &Option<ArtifactRecord>,
    tag_saving: bool,
    action: &mut Option<Action>,
) {
    let id = record.artifact.artifact_id.as_str();
    let version = record.artifact.version_id.as_str();
    ui.strong(&record.name.0);
    widgets::caption(
        ui,
        format!(
            "{} · {} · referenced {} time(s)",
            record.artifact.media_type,
            chat::bytes_label(record.artifact.byte_length.get()),
            record.reference_count.get()
        ),
    );
    let confirming = confirm_delete
        .as_ref()
        .is_some_and(|r| r.artifact.artifact_id.as_str() == id);
    // The actions wrap within the column; the deletion confirmation has a row of its own.
    ui.horizontal_wrapped(|ui| {
        if widgets::secondary_button(ui, "Preview").clicked() {
            *action = Some(Action::Preview(record.clone()));
        }
        if widgets::secondary_button(ui, "Export…").clicked() {
            *action = Some(Action::Export(record.clone()));
        }
        if widgets::secondary_button(ui, "Edit tags").clicked() {
            *action = Some(Action::EditTags(record.clone()));
        }
        if !confirming && widgets::secondary_button(ui, "Delete").clicked() {
            *action = Some(Action::Delete(record.clone()));
        }
    });
    if confirming {
        widgets::caption(
            ui,
            "Deleting removes every version of this artifact. A deleted version is never restored.",
        );
        ui.horizontal_wrapped(|ui| {
            if widgets::secondary_button(ui, "Confirm delete").clicked() {
                *action = Some(Action::ConfirmDelete);
            }
            if widgets::secondary_button(ui, "Keep artifact").clicked() {
                *action = Some(Action::CancelDelete);
            }
        });
    }
    // One field per tag: a tag is an opaque label (commas and spaces are kept as written).
    if let Some((opened, entries)) = tag_draft
        && opened.artifact.artifact_id.as_str() == id
    {
        let mut remove = None;
        for (i, tag) in entries.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                // The field takes what the column leaves beside its Remove button, so both stay
                // inside the column at the smallest window size.
                let right = ui.max_rect().right().min(ui.clip_rect().right());
                let width = (right - ui.cursor().left() - 80.0).max(60.0);
                let r = ui.add(egui::TextEdit::singleline(tag).desired_width(width));
                r.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::TextInput, true, format!("Tag {}", i + 1))
                });
                let r = ui.add(egui::Button::new("Remove"));
                r.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::Role::Button,
                        true,
                        format!("Remove tag {}", i + 1),
                    )
                });
                if r.clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            entries.remove(i);
        }
        ui.horizontal_wrapped(|ui| {
            if widgets::secondary_button(ui, "Add tag").clicked() {
                entries.push(String::new());
            }
            // One save per artifact at a time: a second would carry the same opening revision.
            let save =
                ui.add_enabled_ui(!tag_saving, |ui| widgets::secondary_button(ui, "Save tags"));
            if save.inner.clicked() {
                *action = Some(Action::SaveTags);
            }
            if widgets::secondary_button(ui, "Cancel").clicked() {
                *action = Some(Action::CancelTags);
            }
        });
    }
    let Some((shown, state)) = preview else {
        return;
    };
    if shown != version {
        return;
    }
    match state {
        Shown::Loading => {
            ui.spinner();
        }
        Shown::Failed(e) => {
            widgets::caption(ui, format!("No preview: {e}"));
        }
        Shown::Ready(PreviewView::Text { text, partial }, _) => {
            if *partial {
                widgets::caption(ui, "Preview of the beginning of the file");
            }
            egui::ScrollArea::vertical()
                .id_salt("artifact-preview-text")
                .max_height(320.0)
                .show(ui, |ui| {
                    ui.add(egui::Label::new(
                        egui::RichText::new(text.as_str()).monospace(),
                    ));
                });
        }
        Shown::Ready(PreviewView::Image { size, rgba, facts }, texture) => {
            let t = texture.get_or_insert_with(|| {
                ui.ctx().load_texture(
                    format!("artifact-preview-{version}"),
                    egui::ColorImage::from_rgba_unmultiplied(*size, rgba),
                    Default::default(),
                )
            });
            ui.image((t.id(), t.size_vec2()));
            widgets::caption(ui, facts.as_str());
        }
        Shown::Ready(PreviewView::Facts(facts), _) => {
            widgets::caption(ui, facts.as_str());
        }
    }
}

/// The draft's tags as written, without empty entries or repeats.
fn draft_tags(entries: &[String]) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for t in entries.iter().filter(|t| !t.is_empty()) {
        if !tags.contains(t) {
            tags.push(t.clone());
        }
    }
    tags
}

/// `artifacts.preview`, then the preview's bytes. A text preview is shown as text; an image
/// preview names the image, which is decoded from the artifact itself when it is small enough.
fn preview(b: &dyn Backend, artifact: &ArtifactRef) -> Result<PreviewView, String> {
    use devworks_core::protocol::data::CoverageState;
    use devworks_core::services::artifacts::preview::{INFO_PREVIEW_MEDIA, TEXT_PREVIEW_MEDIA};
    let p =
        ops::artifact_previewed(b.query("artifacts.preview", &ops::artifact_preview(b, artifact)))
            .map_err(|e| e.describe())?;
    let bytes = ops::read_artifact_bytes(b, &p.preview)?;
    if p.preview.media_type == TEXT_PREVIEW_MEDIA {
        return Ok(PreviewView::Text {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            partial: !matches!(p.coverage.state, CoverageState::Complete),
        });
    }
    if p.preview.media_type != INFO_PREVIEW_MEDIA {
        return Err(format!("unknown preview type {}", p.preview.media_type));
    }
    let facts: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("unreadable preview: {e}"))?;
    if facts["kind"] != "image" {
        return Ok(PreviewView::Facts(format!(
            "No preview for {}",
            artifact.media_type
        )));
    }
    let described = format!(
        "{} image, {} × {}",
        facts["format"].as_str().unwrap_or("An").to_uppercase(),
        facts["width"],
        facts["height"]
    );
    match crate::attachments::preview_stored(b, artifact) {
        Ok(TaskOutput::Preview { size, rgba }) => Ok(PreviewView::Image {
            size,
            rgba,
            facts: described,
        }),
        Ok(_) | Err(_) => Ok(PreviewView::Facts(described)),
    }
}

/// `artifacts.export`, then the exported stream written to `target` through a temporary file
/// beside it, after its length and digest match the listed version.
fn export(
    b: &dyn Backend,
    artifact: &ArtifactRef,
    target: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    let export =
        ops::artifact_exported(b.query("artifacts.export", &ops::artifact_export(b, artifact)))
            .map_err(|e| e.describe())?;
    // The platform save dialog already asked before choosing an existing file; the shared
    // export writer applies the destination preflight (never into application data).
    b.write_export(&export.stream_id, target, true)?;
    Ok(target.to_path_buf())
}

#[derive(Default)]
pub struct BrowserScreen {
    /// Every listed browser session: the workspace's, then each listed project's.
    pub list: Paged<BrowserSession>,
    /// The browser sessions per scope (`None`: the workspace), combined into `list`.
    parts: BTreeMap<Option<String>, Paged<BrowserSession>>,
}

impl Screen for BrowserScreen {
    type Slot = BrowserSlot;
    const ID: ScreenId = ScreenId::Browser;
    fn load(&mut self, cx: &mut Cx, _route: Route) {
        self.load(cx);
    }
    fn refresh(&mut self, cx: &mut Cx, _route: Route) {
        if !cx.pending(&BrowserSlot::BrowserSessions) {
            self.load(cx);
        }
    }
    fn apply(&mut self, cx: &mut Cx, slot: BrowserSlot, reply: Reply) {
        match (slot, reply) {
            (BrowserSlot::BrowserSessions, Reply::Query(q)) => {
                self.part(cx, None, ops::browser_sessions(q));
            }
            (BrowserSlot::ProjectBrowserSessions(p), Reply::Query(q)) => {
                self.part(cx, Some(p), ops::browser_sessions(q));
            }
            (BrowserSlot::BrowserClose(_), Reply::Command(c)) => {
                match ops::browser_closed(c) {
                    Ok(true) => cx.announce("The browser session is being closed."),
                    Ok(false) => cx.announce(
                        "Close is already requested, or the browser session has stopped.",
                    ),
                    Err(Failure::Unknown) => {
                        cx.announce("Closing the browser session: outcome not confirmed.")
                    }
                    Err(e) => cx.announce(format!(
                        "The browser session could not be closed: {}",
                        e.describe()
                    )),
                }
                self.load(cx);
            }
            (slot, reply) => cx.unhandled(&slot, reply),
        }
    }
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        self.view(cx, ui);
    }
    fn remove_purged(&mut self, scope: &ProjectPurgeClientScope) {
        self.parts
            .remove(&Some(scope.project_id.0.as_str().to_owned()));
        self.list = Default::default();
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        route_context(mem)
    }
}

impl BrowserScreen {
    /// The workspace's browser sessions and the selected project's, or every listed project's
    /// when none is selected: a person may see and interrupt each of them.
    fn load(&mut self, cx: &mut Cx) {
        let projects: Vec<String> = match &cx.selected_project {
            Some(p) => vec![p.clone()],
            None => {
                if !cx.projects.list.loaded
                    && !cx.pending(&crate::projects_ops::ProjectsSlot::ProjectsList)
                {
                    cx.load_projects();
                }
                cx.projects
                    .list
                    .items
                    .iter()
                    .map(|p| p.id.0.as_str().to_owned())
                    .collect()
            }
        };
        self.parts
            .retain(|scope, _| scope.as_ref().is_none_or(|p| projects.contains(p)));
        let scopes = std::iter::once(None).chain(projects.into_iter().map(Some));
        for scope in scopes {
            let part = self.parts.entry(scope.clone()).or_default();
            // Each part is one scope's list, so a re-read keeps the pages Load more added.
            let fetch = part.reread();
            Self::send(cx, part, scope, fetch, "Loading");
        }
        self.combine();
    }
    /// The next page of every scope that has one.
    fn load_more(&mut self, cx: &mut Cx) {
        for (scope, part) in &mut self.parts {
            if let Some(fetch) = part.more_page() {
                Self::send(cx, part, scope.clone(), fetch, "Loading more");
            }
        }
        self.combine();
    }
    fn send(
        cx: &mut Cx,
        part: &mut Paged<BrowserSession>,
        scope: Option<String>,
        fetch: Fetch,
        verb: &str,
    ) {
        match scope {
            None => cx.paged(
                part,
                fetch,
                BrowserSlot::BrowserSessions,
                &format!("{verb} browser sessions"),
                "browser.sessions",
                |b, cursor| ops::browser_sessions_list(b, None, cursor),
            ),
            Some(p) => {
                let q = p.clone();
                cx.paged(
                    part,
                    fetch,
                    BrowserSlot::ProjectBrowserSessions(p),
                    &format!("{verb} a project's browser sessions"),
                    "browser.sessions",
                    move |b, cursor| ops::browser_sessions_list(b, Some(&q), cursor),
                )
            }
        };
    }

    /// Records one scope's page and rebuilds the combined list.
    fn part(
        &mut self,
        cx: &mut Cx,
        scope: Option<String>,
        page: Result<Page<BrowserSession>, Failure>,
    ) {
        if let Some(part) = self.parts.get_mut(&scope)
            && let Applied::Continue(fetch) = part.apply_current(page.map_err(|e| e.describe()))
        {
            Self::send(cx, part, scope, fetch, "Loading");
        }
        self.combine();
    }
    fn combine(&mut self) {
        self.list = Paged::merged(self.parts.values());
    }
    /// The scope a listed browser session belongs to, for its row.
    fn scope_labels(&self, cx: &Cx) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for (scope, part) in &self.parts {
            let label = match scope {
                None => "Workspace".to_owned(),
                Some(p) => cx
                    .projects
                    .list
                    .items
                    .iter()
                    .find(|r| r.id.0.as_str() == p)
                    .map_or_else(|| p.clone(), |r| r.name.0.clone()),
            };
            for s in &part.items {
                out.insert(s.id.as_str().to_owned(), label.clone());
            }
        }
        out
    }

    /// Interrupts a browser session: Close stops its browser through the process owner.
    fn close(&mut self, cx: &mut Cx, session: &BrowserSession) {
        let (id, revision) = (
            session.id.as_str().to_owned(),
            session.revision.as_str().to_owned(),
        );
        let sid = id.clone();
        cx.command(
            BrowserSlot::BrowserClose(id),
            "Closing the browser session",
            "browser.sessions",
            move |b| ops::browser_close(b, &sid, &revision),
        );
    }

    fn view(&mut self, cx: &mut Cx, ui: &mut Ui) {
        let screen = Route::Browser.screen();
        let online = cx.online().map_err(str::to_owned);
        let mut reload = false;
        let mut more = false;
        let mut close: Option<BrowserSession> = None;
        let mut mem = cx.routes.remove(&Route::Browser).unwrap_or_default();
        let scopes = self.scope_labels(cx);
        let list = &self.list;
        let scrolls = &mut *cx.scrolls;
        let mut st = (&mut reload, &mut mem, &mut close, &mut more);
        widgets::pane(
            ui,
            Header {
                title: screen.title,
                summary: screen.summary,
                tabs: &[],
                tab: 0,
            },
            &mut st,
            |ui, st| {
                if widgets::secondary_button(ui, "Refresh").clicked() {
                    *st.0 = true;
                }
                let selected =
                    st.1.selection
                        .as_ref()
                        .and_then(|id| list.items.iter().find(|s| s.id.as_str() == id));
                if let Some(s) = selected.filter(|s| {
                    matches!(
                        s.state,
                        ItemState::Healthy | ItemState::Partial | ItemState::Unavailable
                    )
                }) && widgets::secondary_button(ui, "Close session").clicked()
                {
                    *st.2 = Some(s.clone());
                }
            },
            |ui, st, _| {
                let mem = &mut *st.1;
                let RouteMemory {
                    filter, selection, ..
                } = mem;
                list_body(
                    ui,
                    &online,
                    list,
                    ListText {
                        what: "browser sessions",
                        empty_title: "No browser sessions",
                        empty_body: "Agents' owned browser sessions appear here; evidence they record is kept in Artifacts.",
                        id_salt: "browser-list",
                    },
                    st.0,
                    st.3,
                    scrolls,
                    |s: &BrowserSession| s.id.as_str().to_owned(),
                    |s: &BrowserSession| s.id.as_str().to_owned(),
                    filter,
                    selection,
                    |ui, s: &BrowserSession, selected| {
                        let state = match s.state {
                            ItemState::Partial => "starting",
                            ItemState::Healthy => "running",
                            ItemState::Disabled => "closed",
                            ItemState::Failed => "failed",
                            ItemState::Unavailable => "stop not verified",
                            ItemState::Enabled => "enabled",
                        };
                        let scope = scopes.get(s.id.as_str()).map_or("", String::as_str);
                        if let Some(source) = &s.tested_repository {
                            if let Some(url) = &source.repository_url {
                                ui.hyperlink_to(
                                    &source.head.hex,
                                    format!("{}/commit/{}", url.0, source.head.hex),
                                );
                            }
                            widgets::caption(
                                ui,
                                format!(
                                    "{} · worktree {} · {} · {} · observed {}",
                                    source.repository_id.as_str(),
                                    source.worktree_id.as_str(),
                                    source.head.hex,
                                    if source.dirty { "dirty" } else { "clean" },
                                    source.observed_at_ms.as_str()
                                ),
                            );
                        } else {
                            widgets::caption(ui, "No tested repository revision recorded");
                        }
                        ui.selectable_label(
                            selected,
                            format!(
                                "{}  ·  {scope}  ·  {state}  ·  {} target(s)  ·  revision {}",
                                s.id.as_str(),
                                s.target_ids.0.len(),
                                s.revision.as_str()
                            ),
                        )
                        .clicked()
                    },
                );
            },
        );
        cx.routes.insert(Route::Browser, mem);
        if let Some(s) = close {
            self.close(cx, &s);
        }
        if reload {
            self.load(cx);
        }
        if more {
            self.load_more(cx);
        }
    }
}

#[derive(Default)]
pub struct ResourcesScreen {
    pub list: Paged<ResourceRecord>,
    /// The holders and waiters of the selected resource.
    pub leases: Option<ResourceLeases>,
}

/// Who holds and who waits for one resource.
pub struct ResourceLeases {
    /// The resource's canonical key, unique across projects.
    pub key: String,
    /// Its name, which the lease queries send.
    pub resource: String,
    /// The project of a project resource, which the lease queries name.
    pub project: Option<String>,
    pub holders: Paged<LeaseRecord>,
    pub waiters: Paged<LeaseRecord>,
}

impl Screen for ResourcesScreen {
    type Slot = ResourcesSlot;
    const ID: ScreenId = ScreenId::Resources;
    fn load(&mut self, cx: &mut Cx, _route: Route) {
        self.load(cx);
    }
    fn refresh(&mut self, cx: &mut Cx, _route: Route) {
        if !cx.pending(&ResourcesSlot::ResourcesList) {
            self.load(cx);
        }
        if let Some(leases) = &mut self.leases {
            for waiters in [false, true] {
                let slot = leases.slot(waiters);
                if !cx.pending(&slot) {
                    let fetch = leases.list(waiters).refresh();
                    leases.send(cx, waiters, fetch);
                }
            }
        }
    }
    fn apply(&mut self, cx: &mut Cx, slot: ResourcesSlot, reply: Reply) {
        match (slot, reply) {
            (ResourcesSlot::ResourcesList, Reply::Query(q)) => {
                let applied = self
                    .list
                    .apply_current(ops::resources(q).map_err(|e| e.describe()));
                if let Applied::Continue(fetch) = applied {
                    self.send(cx, fetch);
                }
            }
            (ResourcesSlot::ResourceHolders(key), Reply::Query(q)) => {
                self.apply_leases(cx, &key, false, q);
            }
            (ResourcesSlot::ResourceWaiters(key), Reply::Query(q)) => {
                self.apply_leases(cx, &key, true, q);
            }
            (slot, reply) => cx.unhandled(&slot, reply),
        }
    }
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        self.view(cx, ui);
    }
    fn remove_purged(&mut self, _scope: &ProjectPurgeClientScope) {
        self.list = Default::default();
        self.leases = None;
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        route_context(mem)
    }
}

impl ResourceLeases {
    fn new(key: String, resource: String, project: Option<String>) -> Self {
        Self {
            key,
            resource,
            project,
            holders: Paged::default(),
            waiters: Paged::default(),
        }
    }
    fn slot(&self, waiters: bool) -> ResourcesSlot {
        if waiters {
            ResourcesSlot::ResourceWaiters(self.key.clone())
        } else {
            ResourcesSlot::ResourceHolders(self.key.clone())
        }
    }
    fn list(&mut self, waiters: bool) -> &mut Paged<LeaseRecord> {
        if waiters {
            &mut self.waiters
        } else {
            &mut self.holders
        }
    }
    fn send(&mut self, cx: &mut Cx, waiters: bool, fetch: Fetch) {
        let slot = self.slot(waiters);
        let resource = self.resource.clone();
        let project = self.project.clone();
        let (label, op) = if waiters {
            ("Loading resource waiters", "resources.waiters")
        } else {
            ("Loading resource holders", "resources.holders")
        };
        cx.paged(
            self.list(waiters),
            fetch,
            slot,
            label,
            op,
            move |b, cursor| {
                ops::resource_leases(b, waiters, &resource, project.as_deref(), cursor)
            },
        );
    }
}

/// One lease as the Resources detail shows it: owner, state, purpose, priority, units and
/// until when it holds or waits.
fn lease_line(l: &LeaseRecord) -> String {
    let state = match l.state {
        LeaseState::Waiting => "waiting",
        LeaseState::Held => "holding",
        LeaseState::Releasing => "releasing",
        LeaseState::Uncertain => "uncertain (its holder stopped answering)",
        LeaseState::Released => "released",
    };
    let priority = match l.priority {
        Some(ResourcePriority::Low) => "low",
        Some(ResourcePriority::Medium) | None => "medium",
        Some(ResourcePriority::High) => "high",
        Some(ResourcePriority::Critical) => "critical",
    };
    let purpose = l
        .purpose
        .as_ref()
        .map_or("no purpose given".to_owned(), |p| format!("for {}", p.0));
    let units = l.units.as_ref().map_or(1, |u| u.get());
    let until = if matches!(l.state, LeaseState::Waiting) {
        "waits until"
    } else {
        "expires"
    };
    format!(
        "{}  ·  {state}  ·  {purpose}  ·  {priority} priority  ·  {units} unit{}  ·  {until} {}",
        l.owner_id.0.as_str(),
        if units == 1 { "" } else { "s" },
        widgets::local_time(l.expires_at_ms.get())
    )
}

/// The holders and the queue of the selected resource, each with a Load more control.
fn lease_detail(ui: &mut Ui, leases: &ResourceLeases, more: &mut [bool; 2]) {
    for (i, (title, list, empty)) in [
        ("Holders", &leases.holders, "Nobody holds this resource."),
        (
            "Waiting",
            &leases.waiters,
            "Nobody is waiting for this resource.",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        ui.strong(title);
        match list.result() {
            None => {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.label(format!("Loading {}…", title.to_lowercase()));
                });
            }
            Some(Err(e)) => {
                ui.label(format!("{title} could not be read: {e}"));
            }
            Some(Ok(rows)) if rows.is_empty() => {
                ui.label(empty);
            }
            Some(Ok(rows)) => {
                for l in rows {
                    ui.label(lease_line(l));
                }
            }
        }
        crate::paged::load_more_control(ui, list, &mut more[i]);
        ui.add_space(8.0);
    }
}

impl ResourcesScreen {
    fn apply_leases(&mut self, cx: &mut Cx, key: &str, waiters: bool, q: QueryOutcome) {
        let Some(leases) = &mut self.leases else {
            return;
        };
        if leases.key != key {
            return;
        }
        let applied = leases
            .list(waiters)
            .apply_current(ops::resource_leases_page(q).map_err(|e| e.describe()));
        if let Applied::Continue(fetch) = applied {
            leases.send(cx, waiters, fetch);
        }
    }
    /// Reads the holders and waiters of the selected resource (its canonical key) when the
    /// selection changes and the resource is listed.
    fn follow_selection(&mut self, cx: &mut Cx, selected: Option<&str>) {
        let record = selected.and_then(|key| {
            self.list
                .result()?
                .ok()?
                .iter()
                .find(|r| r.canonical_key.0 == key)
        });
        let Some(r) = record else {
            if selected.is_none() {
                self.leases = None;
            }
            return;
        };
        match &self.leases {
            Some(l) if l.key == r.canonical_key.0 => {}
            _ => {
                let mut leases = ResourceLeases::new(
                    r.canonical_key.0.clone(),
                    r.id.0.as_str().to_owned(),
                    r.project_id.as_ref().map(|p| p.0.as_str().to_owned()),
                );
                for waiters in [false, true] {
                    let fetch = leases.list(waiters).reload();
                    leases.send(cx, waiters, fetch);
                }
                self.leases = Some(leases);
            }
        }
    }
    fn load(&mut self, cx: &mut Cx) {
        let fetch = self.list.reread();
        self.send(cx, fetch);
    }
    fn send(&mut self, cx: &mut Cx, fetch: Fetch) {
        cx.paged(
            &mut self.list,
            fetch,
            ResourcesSlot::ResourcesList,
            "Loading resources",
            "resources.list",
            ops::resources_list,
        );
    }
    fn load_more(&mut self, cx: &mut Cx) {
        if let Some(fetch) = self.list.more_page() {
            cx.paged(
                &mut self.list,
                fetch,
                ResourcesSlot::ResourcesList,
                "Loading more resources",
                "resources.list",
                ops::resources_list,
            );
        }
    }

    fn view(&mut self, cx: &mut Cx, ui: &mut Ui) {
        let screen = Route::Resources.screen();
        let online = cx.online().map_err(str::to_owned);
        let mut reload = false;
        let mut more = false;
        let mut link = false;
        let mut more_leases = [false, false];
        let mut mem = cx.routes.remove(&Route::Resources).unwrap_or_default();
        let list = &self.list;
        let leases = &self.leases;
        let scrolls = &mut *cx.scrolls;
        let mut st = (
            &mut reload,
            &mut mem,
            &mut link,
            &mut more,
            &mut more_leases,
        );
        widgets::pane(
            ui,
            Header {
                title: screen.title,
                summary: screen.summary,
                tabs: &[],
                tab: 0,
            },
            &mut st,
            |ui, st| {
                if widgets::secondary_button(ui, "Refresh").clicked() {
                    *st.0 = true;
                }
                if let Some((label, _)) = screen.link
                    && widgets::secondary_button(ui, label).clicked()
                {
                    *st.2 = true;
                }
            },
            |ui, st, _| {
                let mem = &mut *st.1;
                let r = ui.add(
                    egui::TextEdit::singleline(&mut mem.filter)
                        .hint_text("Filter resources")
                        .desired_width(280.0),
                );
                r.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Filter resources")
                });
                let RouteMemory {
                    filter, selection, ..
                } = mem;
                let more_leases = &mut *st.4;
                ui.columns(2, |cols| {
                    list_body(
                        &mut cols[0],
                        &online,
                        list,
                        ListText {
                            what: "resources",
                            empty_title: "No shared resources registered",
                            empty_body: "Machine resources are registered at boot.",
                            id_salt: "resources-list",
                        },
                        st.0,
                        st.3,
                        scrolls,
                        |r: &ResourceRecord| r.canonical_key.0.clone(),
                        // Names repeat across projects; the canonical key is unique.
                        |r: &ResourceRecord| r.canonical_key.0.clone(),
                        filter,
                        selection,
                        |ui, r: &ResourceRecord, selected| {
                            ui.selectable_label(
                                selected,
                                format!(
                                    "{}  ·  capacity {}  ·  {} holding  ·  {} waiting",
                                    r.canonical_key.0,
                                    r.capacity.get(),
                                    r.holders.0.len(),
                                    r.waiters.0.len()
                                ),
                            )
                            .clicked()
                        },
                    );
                    let ui = &mut cols[1];
                    match leases {
                        Some(leases) => {
                            egui::ScrollArea::vertical()
                                .id_salt("resource-leases")
                                .auto_shrink([false, false])
                                .show(ui, |ui| lease_detail(ui, leases, more_leases));
                        }
                        None => widgets::caption(
                            ui,
                            "Select a resource to see who holds it and who waits for it.",
                        ),
                    }
                });
            },
        );
        let selected = mem.selection.clone();
        cx.routes.insert(Route::Resources, mem);
        self.follow_selection(cx, selected.as_deref());
        if let Some(leases) = &mut self.leases {
            for (waiters, wanted) in [(false, more_leases[0]), (true, more_leases[1])] {
                if wanted && let Some(fetch) = leases.list(waiters).more_page() {
                    leases.send(cx, waiters, fetch);
                }
            }
        }
        if link && let Some((_, target)) = screen.link {
            cx.navigate(target);
        }
        if reload {
            self.load(cx);
        }
        if more {
            self.load_more(cx);
        }
    }
}
