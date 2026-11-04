//! The Knowledge workspace (KNOW-01): one tagged store for memory, knowledge and lessons with
//! scope selection, word and tag search, entry-kind and lifecycle filters, manual creation and
//! editing, lifecycle changes (verify, retract, challenge, retire, promote, reject), tags,
//! permissioned deletion and source evidence. Every request is the same `knowledge.*`
//! operation an agent tool submits; this module holds view state and drafts only.
//!
//! Markers move only on confirmed outcomes: a draft stays in its form until the write is
//! recorded; a search reply from an older request generation is ignored; a page continuation
//! whose cursor expired (the scope changed) restarts the listing instead of mixing revisions.
use crate::backend::Backend;
use crate::ops::{self, Failure};
use crate::theme;
use crate::widgets::{self, Tone, ViewState, chip};
use devworks_core::gateway::{CommandOutcome, QueryOutcome};
use devworks_core::protocol::records::{KnowledgeBundle, KnowledgeClaim};
use devworks_core::protocol::results::OperationResult;
use devworks_core::protocol::{DomainErrorCode, Id};
use eframe::egui::{self, RichText, Ui};
use serde_json::{Value, json};

/// The entry kinds a person creates (KNOW-03), with the recorded kind each projects onto.
pub const ENTRY_KINDS: [(&str, &str); 10] = [
    ("Fact", "Fact"),
    ("Decision", "Decision"),
    ("Constraint", "Constraint"),
    ("Goal", "Fact"),
    ("Gotcha", "Finding"),
    ("Attempt", "Finding"),
    ("Architecture", "Fact"),
    ("Lesson", "Lesson"),
    ("Finding", "Finding"),
    ("Proposal", "Proposal"),
];
/// Lifecycle filters, in order.
pub const LIFECYCLES: [&str; 4] = ["Active", "Superseded", "Retracted", "Deleted"];
/// Rows one search page asks for.
pub const PAGE: u32 = 100;

/// The newest schema revision `op` registers (frames carry it, so revision-2 members are kept).
pub fn revision_of(op: &str) -> u64 {
    devworks_core::registry::find(op)
        .and_then(|d| d.revisions.iter().map(|r| u64::from(r.0)).max())
        .unwrap_or(1)
}
fn at_revision(frame: Vec<u8>, revision: u64) -> Vec<u8> {
    let mut v: Value = serde_json::from_slice(&frame).expect("frames are JSON");
    if revision > 1 {
        v["schemaRevision"] = json!(revision);
    }
    v.to_string().into_bytes()
}
pub fn query(backend: &dyn Backend, op: &str, body: Value) -> Vec<u8> {
    at_revision(ops::query_frame(backend, op, body), revision_of(op))
}
pub fn command(backend: &dyn Backend, op: &str, body: Value) -> Vec<u8> {
    at_revision(ops::command_frame(backend, op, body), revision_of(op))
}
fn wire<T: std::fmt::Debug>(v: &T) -> String {
    format!("{v:?}")
}
fn split(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A create or edit draft.
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    /// The claim and revision it revises (`None`: a new entry).
    pub revises: Option<(Id, i64)>,
    pub entry_kind: &'static str,
    pub subject: String,
    pub value: String,
    pub tags: String,
    /// Lessons: the activities it applies to (comma-separated).
    pub activities: String,
}
impl Default for Draft {
    fn default() -> Self {
        Self {
            revises: None,
            entry_kind: "Fact",
            subject: String::new(),
            value: String::new(),
            tags: String::new(),
            activities: String::new(),
        }
    }
}

/// A lifecycle change of the selected entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Verify,
    Retract,
    Challenge,
    Retire,
    Promote,
    Reject,
    Delete,
    MakeGeneral,
    SaveTags,
}
impl Change {
    pub fn label(self) -> &'static str {
        match self {
            Self::Verify => "Verify",
            Self::Retract => "Retract",
            Self::Challenge => "Challenge",
            Self::Retire => "Retire rule",
            Self::Promote => "Make effective",
            Self::Reject => "Reject",
            Self::Delete => "Delete",
            Self::MakeGeneral => "Make general",
            Self::SaveTags => "Save tags",
        }
    }
    fn needs_reason(self) -> bool {
        matches!(
            self,
            Self::Verify | Self::Retract | Self::Challenge | Self::Retire | Self::Reject
        )
    }
}

/// What the screen asks the shell to do.
#[derive(Clone, Debug, PartialEq)]
pub enum KnowledgeAction {
    /// A new listing (first page).
    Search,
    /// The next page of the current listing.
    More,
    Select(Id),
    Save(Draft),
    Change(Change),
    /// Previews deleting every entry recorded from a source (test data, a run).
    PreviewSource,
    /// Deletes the previewed source contributions (the user only).
    DeleteSource,
}

/// The exact preview of a delete by source.
#[derive(Clone, Debug, PartialEq)]
pub struct SourcePreview {
    pub source: String,
    pub digest: String,
    pub scope_revision: i64,
    pub message: String,
}

/// The listing request in flight: its generation and whether it continues the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Listing {
    pub generation: u64,
    pub append: bool,
}

#[derive(Default)]
pub struct KnowledgeScreen {
    /// Empty: general (workspace) Knowledge; otherwise a project ID.
    pub project: String,
    pub text: String,
    pub tags: String,
    pub entry_kind: Option<&'static str>,
    pub lifecycles: [bool; 4],
    pub lessons_only: bool,
    pub items: Vec<KnowledgeClaim>,
    pub cursor: Option<String>,
    pub loaded: bool,
    pub error: Option<String>,
    pub generation: u64,
    pub listing: Option<Listing>,
    pub selected: Option<Id>,
    pub detail: Option<KnowledgeBundle>,
    pub detail_error: Option<String>,
    pub retrieval: Option<String>,
    pub draft: Option<Draft>,
    /// The write in flight (its label); further writes wait for its outcome.
    pub writing: Option<String>,
    pub message: Option<(Tone, String)>,
    pub reason: String,
    pub tag_edit: String,
    pub scrolls: crate::virtual_list::Scrolls,
    /// The selected subview (Entries, Documents).
    pub tab: usize,
    /// The scope the shown listing was read in.
    pub listed_scope: Option<Value>,
    /// The source a cleanup names, and its preview once read.
    pub cleanup_source: String,
    pub source_preview: Option<SourcePreview>,
}

impl KnowledgeScreen {
    pub fn new() -> Self {
        Self {
            lifecycles: [true, false, false, false],
            ..Self::default()
        }
    }
    /// The scope typed into the scope field (applied by the next search).
    fn typed_scope(&self) -> Value {
        let p = self.project.trim();
        if p.is_empty() {
            json!({"kind": "Workspace"})
        } else {
            json!({"kind": "Project", "projectId": p})
        }
    }
    /// The scope of the listing shown: reads and writes of its entries use it, never a scope
    /// typed but not searched yet.
    fn scope(&self) -> Value {
        self.listed_scope
            .clone()
            .unwrap_or_else(|| self.typed_scope())
    }

    // -----------------------------------------------------------------------------------------
    // Frames

    /// Starts a new listing generation and returns its frame body (`knowledge.search`, or
    /// `knowledge.tagged_lessons` for lessons only).
    pub fn begin_listing(&mut self, append: bool) -> (&'static str, Value) {
        if !append {
            let scope = self.typed_scope();
            if self.listed_scope.as_ref() != Some(&scope) {
                // Another scope: the selection and an edit of one of its entries no longer
                // belong to what is shown.
                self.selected = None;
                self.detail = None;
                self.detail_error = None;
                if self.draft.as_ref().is_some_and(|d| d.revises.is_some()) {
                    self.draft = None;
                }
                self.cursor = None;
                self.source_preview = None;
            }
            self.listed_scope = Some(scope);
        }
        self.generation += 1;
        self.listing = Some(Listing {
            generation: self.generation,
            append,
        });
        let mut page = json!({"limit": PAGE});
        if append && let Some(c) = &self.cursor {
            page["cursor"] = json!(c);
        }
        let tags = split(&self.tags);
        if self.lessons_only {
            let mut tags = tags;
            if tags.is_empty() {
                tags.push("lesson".into());
            }
            return (
                "knowledge.tagged_lessons",
                json!({"scope": self.scope(), "tags": tags, "page": page}),
            );
        }
        let states: Vec<&str> = LIFECYCLES
            .iter()
            .zip(self.lifecycles)
            .filter(|(_, on)| *on)
            .map(|(s, _)| *s)
            .collect();
        let mut body = json!({"scope": self.scope(),
            "text": if self.text.trim().is_empty() { " " } else { self.text.as_str() },
            "tags": tags, "kinds": [], "page": page, "entityLimit": 500, "claimLimit": PAGE,
            "states": if states.is_empty() { vec!["Active"] } else { states }});
        if let Some(k) = self.entry_kind {
            body["entryKinds"] = json!([k]);
        }
        ("knowledge.search", body)
    }
    pub fn read_body(&self, claim: &Id) -> Value {
        json!({"scope": self.scope(), "claimId": claim})
    }
    pub fn diagnostics_body(&self) -> Value {
        json!({"scope": self.scope()})
    }
    fn selected_claim(&self) -> Option<&KnowledgeClaim> {
        let id = self.selected.as_ref()?;
        self.detail
            .as_ref()
            .and_then(|b| b.claims.0.iter().find(|c| c.id == *id))
            .or_else(|| self.items.iter().find(|c| c.id == *id))
    }

    /// The command a draft submits: a new entry (with its subject entity) or a revision.
    pub fn save_body(&self, d: &Draft) -> Result<(&'static str, Value), String> {
        if d.value.trim().is_empty() {
            return Err("Write what the entry says.".into());
        }
        let kind = ENTRY_KINDS
            .iter()
            .find(|(e, _)| *e == d.entry_kind)
            .map(|(_, k)| *k)
            .ok_or("Unknown entry kind.")?;
        let mut claim = json!({"kind": kind, "entryKind": d.entry_kind,
            "predicate": d.entry_kind.to_lowercase(), "value": d.value.trim(), "confidence": 0.8,
            "evidenceIds": [], "tags": split(&d.tags)});
        if d.entry_kind == "Lesson" {
            claim["lesson"] = json!({"source": "Prompt", "applicability": {
                "activities": split(&d.activities), "paths": [], "tools": [],
                "repositoryIds": []}});
        }
        match &d.revises {
            Some((id, revision)) => {
                let Some(current) = self.selected_claim().filter(|c| c.id == *id) else {
                    return Err("Select the entry again before saving.".into());
                };
                claim["subjectId"] = json!(current.subject_id);
                if d.entry_kind == "Lesson" {
                    // A revision keeps the lesson's provenance; only applicability changes.
                    let source = current
                        .lesson
                        .as_ref()
                        .map_or("Prompt".to_owned(), |l| wire(&l.source));
                    claim["lesson"]["source"] = json!(source);
                }
                Ok((
                    "knowledge.revise",
                    json!({"scope": self.scope(), "claimId": id,
                        "expectedRevision": revision.to_string(), "replacement": claim}),
                ))
            }
            None => {
                let subject = if d.subject.trim().is_empty() {
                    "General"
                } else {
                    d.subject.trim()
                };
                claim["subjectId"] = json!("subject");
                Ok((
                    "knowledge.record",
                    json!({"scope": self.scope(), "batch": {
                        "entities": [{"clientId": "subject", "label": subject,
                            "entityType": "topic", "aliases": []}],
                        "claims": [claim], "evidence": []}}),
                ))
            }
        }
    }

    /// The command a lifecycle change of the selected entry submits.
    pub fn change_body(&self, change: Change) -> Result<(&'static str, Value), String> {
        let c = self.selected_claim().ok_or("Select an entry first.")?;
        let reason = self.reason.trim();
        if change.needs_reason() && reason.is_empty() {
            return Err(format!("{} needs a reason.", change.label()));
        }
        let revision = c.revision.get().to_string();
        let scope = self.scope();
        Ok(match change {
            Change::Verify => (
                "knowledge.verify",
                json!({"scope": scope, "claimId": c.id, "expectedRevision": revision,
                    "reason": reason, "evidenceIds": []}),
            ),
            Change::Retract => (
                "knowledge.retract",
                json!({"scope": scope, "claimId": c.id, "expectedRevision": revision,
                    "reason": reason, "evidenceIds": []}),
            ),
            Change::Challenge => (
                "knowledge.challenge",
                json!({"scope": scope, "claimId": c.id, "expectedRevision": revision,
                    "reason": reason, "evidenceIds": []}),
            ),
            Change::Retire => (
                "knowledge.challenge",
                json!({"scope": scope, "claimId": c.id, "expectedRevision": revision,
                    "reason": reason, "evidenceIds": [], "outcome": "Retire"}),
            ),
            Change::Promote => (
                "knowledge.promote",
                json!({"claimId": c.id, "expectedRevision": revision, "destination": scope}),
            ),
            Change::Reject => (
                "knowledge.promote",
                json!({"claimId": c.id, "expectedRevision": revision, "destination": scope,
                    "action": "Reject", "reason": reason}),
            ),
            Change::Delete => (
                "knowledge.delete_selected",
                json!({"scope": scope, "claims": [{"id": c.id, "revision": revision}]}),
            ),
            Change::MakeGeneral => (
                "knowledge.scope",
                json!({"claimIds": [{"id": c.id, "revision": revision}],
                    "destination": {"kind": "Workspace"}}),
            ),
            Change::SaveTags => (
                "knowledge.tag",
                json!({"scope": scope, "claimIds": [c.id],
                    "expectedRevisions": [{"id": c.id, "revision": revision}],
                    "tags": split(&self.tag_edit)}),
            ),
        })
    }

    // -----------------------------------------------------------------------------------------
    // Replies

    /// Applies a listing reply of `generation`; returns `true` when the listing must restart
    /// (its continuation expired because the scope changed).
    pub fn listed(&mut self, generation: u64, outcome: QueryOutcome) -> bool {
        let Some(listing) = self.listing.filter(|l| l.generation == generation) else {
            return false; // superseded by a newer listing
        };
        self.listing = None;
        match ops::query_result(outcome) {
            Ok((
                OperationResult::KnowledgeSearch { value }
                | OperationResult::KnowledgeTaggedLessons { value },
                _,
            )) => {
                if listing.append {
                    self.items.extend(value.items.0);
                } else {
                    self.items = value.items.0;
                }
                self.cursor = value.next_cursor.map(|c| c.0);
                self.loaded = true;
                self.error = None;
                false
            }
            Ok((other, _)) => {
                self.error = Some(format!("Unexpected reply {}", other.operation()));
                false
            }
            Err(Failure::Refused(f)) if f.code == DomainErrorCode::CursorExpired => {
                self.message = Some((
                    Tone::Attention,
                    "Knowledge changed while paging; the list was reloaded from the start.".into(),
                ));
                true
            }
            Err(e) => {
                self.error = Some(e.describe());
                false
            }
        }
    }
    /// Applies a `knowledge.read` reply for `claim` (ignored when another entry is selected).
    pub fn read_reply(&mut self, claim: &str, outcome: QueryOutcome) {
        if self.selected.as_ref().map(Id::as_str) != Some(claim) {
            return;
        }
        match ops::query_result(outcome) {
            Ok((OperationResult::KnowledgeRead { value }, _)) => {
                self.tag_edit = value
                    .claims
                    .0
                    .iter()
                    .find(|c| c.id.as_str() == claim)
                    .map(|c| {
                        c.tags
                            .0
                            .iter()
                            .map(|t| t.0.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                self.detail = Some(value);
                self.detail_error = None;
            }
            Ok((other, _)) => self.detail_error = Some(format!("Unexpected {}", other.operation())),
            Err(e) => self.detail_error = Some(e.describe()),
        }
    }
    /// The diagnostics query that previews deleting `cleanup_source` in the listed scope.
    pub fn preview_body(&self) -> Result<(String, Value), String> {
        let source = self.cleanup_source.trim();
        if Id::new(source).is_err() {
            return Err("A source is 1-128 letters, digits, '-' or '_'.".into());
        }
        Ok((
            source.to_owned(),
            json!({"scope": self.scope(), "sourceId": source}),
        ))
    }
    /// Applies a preview reply for `source` (ignored when the field names another source now).
    pub fn preview_reply(&mut self, source: &str, outcome: QueryOutcome) {
        self.source_preview = None;
        if self.cleanup_source.trim() != source {
            return;
        }
        match ops::query_result(outcome) {
            Ok((OperationResult::KnowledgeDiagnostics { value }, scope_revision)) => {
                match value
                    .items
                    .0
                    .iter()
                    .find(|d| d.component.0 == "knowledge.preview")
                {
                    Some(d) => {
                        self.source_preview = Some(SourcePreview {
                            source: source.to_owned(),
                            digest: d.id.as_str().to_owned(),
                            scope_revision,
                            message: d.message.0.clone(),
                        })
                    }
                    None => {
                        self.message = Some((Tone::Bad, "The preview was not returned.".into()))
                    }
                }
            }
            Ok((other, _)) => {
                self.message = Some((Tone::Bad, format!("Unexpected {}", other.operation())))
            }
            Err(e) => self.message = Some((Tone::Bad, e.describe())),
        }
    }
    /// The delete by source the shown preview describes, exactly (digest and scope revision).
    pub fn delete_source_body(&self) -> Result<(&'static str, Value), String> {
        let p = self
            .source_preview
            .as_ref()
            .filter(|p| p.source == self.cleanup_source.trim())
            .ok_or("Preview the source first.")?;
        Ok((
            "knowledge.delete_by_source",
            json!({"scope": self.scope(), "sourceId": p.source,
                "expectedScopeRevision": p.scope_revision.to_string(),
                "previewDigest": p.digest}),
        ))
    }
    fn cleanup(&mut self, ui: &mut Ui, actions: &mut Vec<KnowledgeAction>) {
        egui::CollapsingHeader::new("Clean up by source")
            .id_salt("knowledge-cleanup")
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.cleanup_source)
                            .hint_text("Source, such as a test run")
                            .desired_width(220.0),
                    );
                    r.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Cleanup source")
                    });
                    if r.changed() {
                        self.source_preview = None;
                    }
                    if ui.button("Preview deletion").clicked() {
                        actions.push(KnowledgeAction::PreviewSource);
                    }
                });
                if let Some(p) = &self.source_preview {
                    widgets::caption(ui, p.message.clone());
                    let busy = self.writing.is_some();
                    if ui
                        .add_enabled(!busy, egui::Button::new("Delete previewed entries"))
                        .clicked()
                    {
                        actions.push(KnowledgeAction::DeleteSource);
                    }
                }
            });
    }
    pub fn diagnostics_reply(&mut self, outcome: QueryOutcome) {
        if let Ok((OperationResult::KnowledgeDiagnostics { value }, _)) = ops::query_result(outcome)
        {
            self.retrieval = value
                .items
                .0
                .iter()
                .find(|d| d.component.0 == "knowledge.retrieval")
                .map(|d| d.code.0.clone());
        }
    }
    /// Applies a write's outcome; `true` when it was recorded (the listing and selection are
    /// re-read). A draft or reason is cleared only now, never before the outcome is known.
    pub fn written(&mut self, outcome: CommandOutcome) -> bool {
        let label = self.writing.take().unwrap_or_else(|| "Change".into());
        match ops::command_result(outcome) {
            Ok(_) => {
                self.draft = None;
                self.reason.clear();
                self.source_preview = None;
                self.message = Some((Tone::Good, format!("{label} recorded.")));
                true
            }
            Err(Failure::Unknown) => {
                self.message = Some((
                    Tone::Attention,
                    format!(
                        "{label}: the outcome is not confirmed yet; it is resolved from its receipt."
                    ),
                ));
                false
            }
            Err(e) => {
                self.message = Some((
                    Tone::Bad,
                    format!("{label} was not recorded. {}", e.describe()),
                ));
                false
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Rendering

    pub fn ui(
        &mut self,
        ui: &mut Ui,
        online: Result<(), &str>,
        actions: &mut Vec<KnowledgeAction>,
    ) {
        let screen = crate::route::Route::Knowledge.screen();
        let mut st = (&mut *self, &mut *actions, online);
        widgets::with_right_sidebar(
            ui,
            360.0,
            &mut st,
            |ui, st| {
                let (s, a, online) = st;
                let tab = s.tab_memory();
                let (_, tab) = widgets::pane(
                    ui,
                    widgets::Header {
                        title: screen.title,
                        summary: screen.summary,
                        tabs: screen.subviews,
                        tab,
                    },
                    &mut (&mut **s, &mut **a),
                    |ui, st| {
                        if widgets::secondary_button(ui, "New entry").clicked() {
                            st.0.draft = Some(Draft::default());
                        }
                    },
                    |ui, st, tab| {
                        if tab == 1 {
                            widgets::state_block(
                                ui,
                                &ViewState::Unavailable {
                                    what: "knowledge documents".into(),
                                    owner: "DW-35".into(),
                                },
                            );
                            return;
                        }
                        st.0.entries(ui, *online, st.1);
                    },
                );
                s.set_tab_memory(tab);
            },
            |ui, st| {
                let (s, a, online) = st;
                widgets::secondary_pane(ui, "Entry", |ui| s.detail_pane(ui, *online, a));
            },
        );
    }
    fn tab_memory(&self) -> usize {
        self.tab
    }
    fn set_tab_memory(&mut self, tab: usize) {
        self.tab = tab;
    }

    fn entries(
        &mut self,
        ui: &mut Ui,
        online: Result<(), &str>,
        actions: &mut Vec<KnowledgeAction>,
    ) {
        if let Err(reason) = online {
            widgets::state_block(
                ui,
                &ViewState::Offline {
                    reason: reason.into(),
                },
            );
            return;
        }
        ui.horizontal_wrapped(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.project)
                    .hint_text("General (all projects)")
                    .desired_width(180.0),
            );
            r.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Project scope")
            });
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.text)
                    .hint_text("Search words")
                    .desired_width(220.0),
            );
            r.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Search Knowledge")
            });
            let submitted = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.tags)
                    .hint_text("Tags, comma-separated")
                    .desired_width(180.0),
            );
            r.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Filter by tags")
            });
            if ui.button("Search").clicked() || submitted {
                actions.push(KnowledgeAction::Search);
            }
        });
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt("knowledge-kind")
                .selected_text(self.entry_kind.unwrap_or("Every kind"))
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.entry_kind.is_none(), "Every kind")
                        .clicked()
                    {
                        self.entry_kind = None;
                        actions.push(KnowledgeAction::Search);
                    }
                    for (k, _) in ENTRY_KINDS {
                        if ui.selectable_label(self.entry_kind == Some(k), k).clicked() {
                            self.entry_kind = Some(k);
                            actions.push(KnowledgeAction::Search);
                        }
                    }
                })
                .response
                .widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Filter by kind")
                });
            for (i, l) in LIFECYCLES.iter().enumerate() {
                if ui.checkbox(&mut self.lifecycles[i], *l).changed() {
                    actions.push(KnowledgeAction::Search);
                }
            }
            if ui.checkbox(&mut self.lessons_only, "Lessons").changed() {
                actions.push(KnowledgeAction::Search);
            }
            match self.retrieval.as_deref() {
                Some("LexicalOnly") => {
                    chip(ui, "Word search only (no embedding route)", Tone::Neutral);
                }
                Some(other) => {
                    chip(ui, other.to_owned(), Tone::Neutral);
                }
                None => {}
            }
        });
        if let Some((tone, text)) = &self.message {
            chip(ui, text.clone(), *tone);
        }
        self.cleanup(ui, actions);
        if let Some(d) = self.draft.clone() {
            self.editor(ui, d, actions);
        }
        ui.add_space(6.0);
        if let Some(e) = &self.error {
            let state = if self.loaded {
                ViewState::Stale {
                    what: "Knowledge".into(),
                }
            } else {
                ViewState::Failed {
                    what: "Knowledge".into(),
                    error: e.clone(),
                }
            };
            if widgets::state_block(ui, &state) == widgets::StateAction::Retry {
                actions.push(KnowledgeAction::Search);
            }
        }
        if !self.loaded {
            if self.error.is_none() {
                widgets::state_block(
                    ui,
                    &ViewState::Loading {
                        what: "Knowledge".into(),
                    },
                );
            }
            return;
        }
        if self.items.is_empty() {
            widgets::state_block(
                ui,
                &ViewState::Empty {
                    title: "No entries match".into(),
                    body: "Record a fact, decision, constraint, gotcha or lesson with New entry, or change the filters.".into(),
                },
            );
            return;
        }
        egui::ScrollArea::vertical()
            .id_salt("knowledge-list")
            .max_height((ui.available_height() - 40.0).max(120.0))
            .show(ui, |ui| {
                for c in &self.items {
                    let selected = self.selected.as_ref() == Some(&c.id);
                    let kind = c.entry_kind.as_ref().map_or_else(|| wire(&c.kind), wire);
                    let mut line = format!(
                        "{}  ·  {}",
                        kind,
                        c.value
                            .as_ref()
                            .map_or(c.predicate.0.as_str(), |v| v.0.as_str())
                    );
                    if let Some(l) = c.lifecycle.as_ref().filter(|l| wire(*l) != "Active") {
                        line.push_str(&format!("  ·  {}", wire(l)));
                    }
                    if let Some(lesson) = &c.lesson {
                        line.push_str(&format!("  ·  rule {}", wire(&lesson.rule_state)));
                    }
                    if c.stale == Some(true) {
                        line.push_str("  ·  stale");
                    }
                    if ui.selectable_label(selected, line).clicked() {
                        actions.push(KnowledgeAction::Select(c.id.clone()));
                    }
                }
            });
        if self.cursor.is_some() {
            let busy = self.listing.is_some();
            if ui
                .add_enabled(!busy, egui::Button::new("Load more"))
                .clicked()
            {
                actions.push(KnowledgeAction::More);
            }
        }
    }

    fn editor(&mut self, ui: &mut Ui, mut d: Draft, actions: &mut Vec<KnowledgeAction>) {
        egui::Frame::new()
            .stroke(theme::soft_line())
            .corner_radius(theme::radius())
            .inner_margin(theme::margin(12, 10))
            .show(ui, |ui| {
                ui.label(RichText::new(if d.revises.is_some() { "Edit entry" } else { "New entry" }).strong());
                egui::ComboBox::from_id_salt("knowledge-draft-kind")
                    .selected_text(d.entry_kind)
                    .show_ui(ui, |ui| {
                        for (k, _) in ENTRY_KINDS {
                            if d.revises.is_none() || (k == "Lesson") == (d.entry_kind == "Lesson") {
                                ui.selectable_value(&mut d.entry_kind, k, k);
                            }
                        }
                    })
                    .response
                    .widget_info(|| {
                        egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Entry kind")
                    });
                if d.revises.is_none() {
                    let r = ui.add(egui::TextEdit::singleline(&mut d.subject).hint_text("Subject (topic, component, person)"));
                    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Entry subject"));
                }
                let r = ui.add(egui::TextEdit::multiline(&mut d.value).hint_text("What it says").desired_rows(3));
                r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Entry text"));
                let r = ui.add(egui::TextEdit::singleline(&mut d.tags).hint_text("Tags, comma-separated"));
                r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Entry tags"));
                if d.entry_kind == "Lesson" {
                    let r = ui.add(egui::TextEdit::singleline(&mut d.activities).hint_text("Applies when: implementing, reviewing, …"));
                    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Lesson activities"));
                    widgets::caption(ui, "A rule in your own words takes effect at once; nothing waits for approval.");
                }
                ui.horizontal(|ui| {
                    let busy = self.writing.is_some();
                    if ui.add_enabled(!busy, egui::Button::new("Save entry")).clicked() {
                        actions.push(KnowledgeAction::Save(d.clone()));
                    }
                    if ui.button("Cancel").clicked() {
                        self.draft = None;
                    }
                });
            });
        if self.draft.is_some() {
            self.draft = Some(d);
        }
    }

    fn detail_pane(
        &mut self,
        ui: &mut Ui,
        online: Result<(), &str>,
        actions: &mut Vec<KnowledgeAction>,
    ) {
        let Some(id) = self.selected.clone() else {
            widgets::caption(
                ui,
                "Select an entry to see its provenance, evidence, revisions and lesson state.",
            );
            return;
        };
        if let Some(e) = &self.detail_error {
            widgets::state_block(
                ui,
                &ViewState::Failed {
                    what: "the entry".into(),
                    error: e.clone(),
                },
            );
            return;
        }
        let Some(bundle) = self.detail.clone() else {
            widgets::state_block(
                ui,
                &ViewState::Loading {
                    what: "the entry".into(),
                },
            );
            return;
        };
        let Some(c) = bundle.claims.0.iter().find(|c| c.id == id).cloned() else {
            return;
        };
        egui::ScrollArea::vertical().id_salt("knowledge-detail").show(ui, |ui| {
            ui.label(RichText::new(c.value.as_ref().map_or(c.predicate.0.clone(), |v| v.0.clone())).strong());
            if let Some(n) = &c.normalized_text {
                ui.label(&n.0);
            }
            ui.horizontal_wrapped(|ui| {
                chip(ui, c.entry_kind.as_ref().map_or_else(|| wire(&c.kind), wire), Tone::Accent);
                chip(ui, c.lifecycle.as_ref().map_or_else(|| wire(&c.state), wire), Tone::Neutral);
                if c.stale == Some(true) {
                    chip(ui, "Stale: its source changed", Tone::Attention);
                }
                if c.autonomous == Some(true) {
                    chip(ui, "Decided by an agent", Tone::Neutral);
                }
            });
            let subject = bundle.entities.0.iter().find(|e| e.id == c.subject_id);
            widgets::caption(ui, format!(
                "About {} · {} · confidence {:.0}% (not proof)",
                subject.map_or("an entity", |e| e.label.0.as_str()),
                if matches!(c.scope, devworks_core::protocol::data::DataScope::Workspace) { "general" } else { "project" },
                c.confidence.get() * 100.0
            ));
            widgets::caption(ui, format!(
                "Origin {} · recorded by {} · revision {}",
                c.origin.as_ref().map_or("—".into(), wire),
                c.author_id.as_ref().map_or("—", |a| a.0.as_str()),
                c.revision.get()
            ));
            if let Some(v) = &c.verified_source_revision {
                widgets::caption(ui, format!("Verified against {}", v.0));
            }
            if let Some(l) = &c.lesson {
                widgets::divider(ui);
                ui.label(RichText::new("Rule").strong());
                widgets::caption(ui, format!(
                    "{} · {} · from {} · recurred in {} of {} events · supplied to {} sessions",
                    wire(&l.rule_state),
                    l.effect.as_ref().map_or("not in effect".into(), wire),
                    wire(&l.source),
                    l.recurrence.get(),
                    l.recurrence_threshold.get(),
                    l.delivered_sessions.get()
                ));
                if !l.applicability.activities.0.is_empty() {
                    widgets::caption(ui, format!("Applies when {}", l.applicability.activities.0.iter().map(|a| a.0.as_str()).collect::<Vec<_>>().join(", ")));
                }
                for a in &l.addressed_by.0 {
                    widgets::caption(ui, format!("Addressed by {} {}", wire(&a.kind), a.name.0));
                }
                for ch in &l.challenges.0 {
                    widgets::caption(ui, format!("Challenge ({}): {}", wire(&ch.state), ch.reason.0));
                }
            }
            if !bundle.conflicts.0.is_empty() {
                widgets::divider(ui);
                for k in &bundle.conflicts.0 {
                    chip(ui, format!("Conflict: {}", k.explanation.0), Tone::Attention);
                }
            }
            widgets::divider(ui);
            ui.label(RichText::new("Evidence").strong());
            if bundle.evidence.0.is_empty() {
                widgets::caption(ui, "No source evidence recorded.");
            }
            for e in &bundle.evidence.0 {
                widgets::caption(ui, format!("{} {}: {}", wire(&e.source.kind), e.source.source_ref.0, e.snippet.0));
            }
            if let Some(revisions) = &bundle.revisions {
                widgets::divider(ui);
                ui.label(RichText::new("Revisions").strong());
                for r in &revisions.0 {
                    widgets::caption(ui, format!("{} · {} by {}", r.revision.get(), r.change.0, r.author_id.0.as_str()));
                }
            }
            widgets::divider(ui);
            let r = ui.add(egui::TextEdit::singleline(&mut self.tag_edit).hint_text("Tags"));
            r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Edit tags"));
            let r = ui.add(egui::TextEdit::singleline(&mut self.reason).hint_text("Reason (verify, retract, challenge, retire, reject)"));
            r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Change reason"));
            let enabled = online.is_ok() && self.writing.is_none();
            let lifecycle = c.lifecycle.as_ref().map_or_else(|| "Active".to_owned(), wire);
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(enabled && lifecycle == "Active", egui::Button::new("Edit")).clicked() {
                    self.draft = Some(Draft {
                        revises: Some((c.id.clone(), c.revision.get())),
                        entry_kind: ENTRY_KINDS
                            .iter()
                            .map(|(e, _)| *e)
                            .find(|e| Some(e.to_string()) == c.entry_kind.as_ref().map(wire))
                            .unwrap_or("Fact"),
                        subject: String::new(),
                        value: c.value.as_ref().map(|v| v.0.clone()).unwrap_or_default(),
                        tags: c.tags.0.iter().map(|t| t.0.clone()).collect::<Vec<_>>().join(", "),
                        activities: c.lesson.as_ref().map(|l| l.applicability.activities.0.iter().map(|a| a.0.clone()).collect::<Vec<_>>().join(", ")).unwrap_or_default(),
                    });
                }
                let mut offer = vec![Change::SaveTags];
                if lifecycle == "Active" {
                    offer.extend([Change::Verify, Change::Retract, Change::Challenge]);
                    if c.lesson.is_some() {
                        offer.extend([Change::Promote, Change::Reject, Change::Retire]);
                    }
                    if !matches!(c.scope, devworks_core::protocol::data::DataScope::Workspace) {
                        offer.push(Change::MakeGeneral);
                    }
                }
                if lifecycle != "Deleted" {
                    offer.push(Change::Delete);
                }
                for change in offer {
                    if ui.add_enabled(enabled, egui::Button::new(change.label())).clicked() {
                        actions.push(KnowledgeAction::Change(change));
                    }
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_use_the_scope_of_the_listing_shown_not_an_unsearched_one() {
        let mut s = KnowledgeScreen::new();
        let (_, body) = s.begin_listing(false);
        assert_eq!(body["scope"], json!({"kind": "Workspace"}));
        s.project = "project-x".into();
        let draft = Draft {
            value: "A fact".into(),
            ..Draft::default()
        };
        let (_, body) = s.save_body(&draft).unwrap();
        assert_eq!(
            body["scope"],
            json!({"kind": "Workspace"}),
            "typed, not searched"
        );
        let (_, body) = s.begin_listing(false);
        assert_eq!(
            body["scope"],
            json!({"kind": "Project", "projectId": "project-x"})
        );
        let (_, body) = s.save_body(&draft).unwrap();
        assert_eq!(
            body["scope"],
            json!({"kind": "Project", "projectId": "project-x"})
        );
        // A continuation stays in the listed scope even if the field changes again.
        s.cursor = Some("c".into());
        s.project.clear();
        let (_, body) = s.begin_listing(true);
        assert_eq!(
            body["scope"],
            json!({"kind": "Project", "projectId": "project-x"})
        );
        assert_eq!(body["page"]["cursor"], json!("c"));
    }

    #[test]
    fn a_reply_of_a_superseded_listing_is_ignored() {
        let mut s = KnowledgeScreen::new();
        s.begin_listing(false);
        let old = s.generation;
        s.begin_listing(false);
        let restart = s.listed(
            old,
            Err(devworks_core::gateway::GatewayFailure::NoUseCase(
                "knowledge.search".into(),
            )),
        );
        assert!(!restart);
        assert!(s.listing.is_some(), "the newer listing is still awaited");
        assert!(s.error.is_none(), "a stale reply never overwrites state");
    }
}
