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
use devworks_core::protocol::provider::ArtifactRef;
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
/// The scope choice for general (workspace) Knowledge.
pub const GENERAL: &str = "General (all projects)";

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

/// The replacement an edit submits: the selected claim exactly as it is, with only the fields
/// the editor shows replaced (entry kind, text, tags and, for a lesson, its activities). The
/// subject, predicate, object, confidence, cited evidence, structured value, normalized text,
/// source, effective time, origin reference and the rest of the applicability are kept, so an
/// edit never drops citations or content it did not show.
pub fn revision_of_claim(c: &KnowledgeClaim, d: &Draft, kind: &str) -> Value {
    let mut v = json!({"kind": kind, "entryKind": d.entry_kind, "subjectId": c.subject_id,
        "predicate": c.predicate, "value": d.value.trim(), "confidence": c.confidence.get(),
        "evidenceIds": c.evidence_ids, "tags": split(&d.tags)});
    if let Some(o) = &c.object_id {
        v["objectId"] = json!(o);
    }
    if let Some(j) = &c.value_json {
        v["valueJson"] = j.0.clone();
    }
    if let Some(t) = &c.normalized_text {
        v["normalizedText"] = json!(t);
    }
    if let Some(t) = &c.effective_at_ms {
        v["effectiveAtMs"] = json!(t);
    }
    if let Some(s) = &c.source {
        v["source"] = json!(s);
    }
    if let Some(r) = &c.origin_ref {
        v["originRef"] = json!(r);
    }
    if let Some(l) = &c.lesson {
        let mut applicability = serde_json::to_value(&l.applicability).expect("applicability");
        applicability["activities"] = json!(split(&d.activities));
        v["lesson"] = json!({"source": l.source, "applicability": applicability});
    }
    v
}

/// A create or edit draft.
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    /// Identity of this logical draft, retained by cloning and rebasing.
    identity: u64,
    /// The claim and revision it revises (`None`: a new entry).
    pub revises: Option<(Id, i64)>,
    pub entry_kind: &'static str,
    pub subject: String,
    pub value: String,
    pub tags: String,
    /// Lessons: the activities it applies to (comma-separated).
    pub activities: String,
}
fn next_draft_identity() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl Default for Draft {
    fn default() -> Self {
        Self {
            identity: next_draft_identity(),
            revises: None,
            entry_kind: "Fact",
            subject: String::new(),
            value: String::new(),
            tags: String::new(),
            activities: String::new(),
        }
    }
}

impl Draft {
    /// An edit draft of `c` as it is stored, at its revision.
    pub fn of(c: &KnowledgeClaim) -> Self {
        let join = |items: Vec<String>| items.join(", ");
        Self {
            identity: next_draft_identity(),
            revises: Some((c.id.clone(), c.revision.get())),
            entry_kind: ENTRY_KINDS
                .iter()
                .map(|(e, _)| *e)
                .find(|e| Some(e.to_string()) == c.entry_kind.as_ref().map(wire))
                .unwrap_or("Fact"),
            subject: String::new(),
            value: c.value.as_ref().map(|v| v.0.clone()).unwrap_or_default(),
            tags: join(c.tags.0.iter().map(|t| t.0.clone()).collect()),
            activities: c.lesson.as_ref().map_or_else(String::new, |l| {
                join(
                    l.applicability
                        .activities
                        .0
                        .iter()
                        .map(|a| a.0.clone())
                        .collect(),
                )
            }),
        }
    }
    /// This draft rebased onto `stored`, a newer revision of the entry it edits (its own save,
    /// or another write), field by field (the rule project forms follow): a field the user did
    /// not change since `base` takes the stored value, a changed field keeps the user's, and
    /// the draft then revises the stored revision.
    pub fn rebased(self, base: &Draft, stored: &Draft) -> Self {
        fn pick<T: PartialEq>(mine: T, base: &T, stored: &T, clone: impl Fn(&T) -> T) -> T {
            if mine == *base { clone(stored) } else { mine }
        }
        Self {
            identity: self.identity,
            revises: stored.revises.clone(),
            entry_kind: pick(self.entry_kind, &base.entry_kind, &stored.entry_kind, |k| k),
            subject: self.subject,
            value: pick(self.value, &base.value, &stored.value, String::clone),
            tags: pick(self.tags, &base.tags, &stored.tags, String::clone),
            activities: pick(
                self.activities,
                &base.activities,
                &stored.activities,
                String::clone,
            ),
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

/// What the write in flight took from the screen's forms. On success only that is cleared,
/// and only if unchanged since (an edit made while it was saving stays); an unrelated write
/// leaves every form alone.
#[derive(Clone, Debug, PartialEq)]
pub enum Submitted {
    /// A create or edit draft, as saved.
    Draft(Draft),
    /// The reason a lifecycle change carried.
    Reason(String),
    /// Nothing typed is consumed (tags are re-read from the entry, a cleanup keeps its source).
    Nothing,
}

/// One previewed candidate of an import: its outcome code and description.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportCandidate {
    pub outcome: String,
    pub text: String,
}

/// A user data import from a file (`knowledge.import_user_data`): saved as the user's
/// artifact, previewed exactly, confirmed, and reported per candidate. Each stage moves only
/// on its confirmed reply.
#[derive(Clone, Debug)]
pub enum ImportStage {
    /// Preparing the file's `artifacts.save` frame (nothing admitted yet).
    Saving {
        name: String,
    },
    /// Its exact `artifacts.save` frame is admitted (or being resubmitted to confirm it).
    Storing {
        name: String,
        frame: Vec<u8>,
    },
    /// The save's outcome is not known: the frame is kept for Confirm outcomes, which
    /// resubmits it unchanged (so the file is never stored twice).
    Unconfirmed {
        name: String,
        frame: Vec<u8>,
    },
    Previewing {
        name: String,
        source: ArtifactRef,
        scope: Value,
    },
    Ready {
        name: String,
        source: ArtifactRef,
        scope: Value,
        digest: String,
        candidates: Vec<ImportCandidate>,
        summary: String,
    },
    Importing {
        name: String,
        source: ArtifactRef,
        scope: Value,
        digest: String,
        candidates: Vec<ImportCandidate>,
        summary: String,
    },
    Done {
        name: String,
        results: Vec<ImportCandidate>,
        stored: usize,
    },
    Failed {
        name: String,
        error: String,
    },
}
impl ImportStage {
    /// Whether a request of this import is in flight, or its file's save is unconfirmed (it
    /// may have committed: neither cancelled nor replaced until confirmed).
    pub fn busy(&self) -> bool {
        matches!(
            self,
            Self::Saving { .. }
                | Self::Storing { .. }
                | Self::Unconfirmed { .. }
                | Self::Previewing { .. }
                | Self::Importing { .. }
        )
    }
}

/// What the shell does after an import file's save task replied.
#[derive(Clone, Debug, PartialEq)]
pub enum ImportNext {
    /// Admit this exact `artifacts.save` frame.
    Store(Vec<u8>),
    /// Run this preview query (`knowledge.diagnostics`).
    Preview(Value),
    Nothing,
}
/// The import format's schema reference.
pub fn import_schema() -> Value {
    use devworks_core::services::knowledge::import::{
        IMPORT_SCHEMA_ID, IMPORT_SCHEMA_REVISION, import_schema_digest,
    };
    json!({"schemaId": IMPORT_SCHEMA_ID, "revision": IMPORT_SCHEMA_REVISION.to_string(),
        "sha256": import_schema_digest()})
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
    /// Chooses a file to import (the native file dialog).
    ImportDialog,
    /// Imports these files (the first; a dropped file arrives here).
    ImportFiles(Vec<std::path::PathBuf>),
    /// Confirms the previewed import exactly.
    ConfirmImport,
    CancelImport,
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
    /// `None`: general (workspace) Knowledge; otherwise one of [`Self::projects`].
    pub project: Option<String>,
    /// The projects the user can see (ID, name), from the projects listing; the only scope
    /// choices besides general Knowledge.
    pub projects: Vec<(String, String)>,
    /// The navigation selection last followed (`None` until the projects are listed).
    pub followed: Option<Option<String>>,
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
    /// What the write in flight took from the forms (`None`: unknown, as for a write held
    /// from an earlier launch, which clears nothing).
    pub submitted: Option<Submitted>,
    /// What an edit draft's fields were when it opened on its entry's revision, or as its last
    /// save submitted them: a field still equal to it is one the user did not change, which a
    /// newer revision of the entry replaces (`None` with a new-entry draft).
    pub draft_base: Option<Draft>,
    pub message: Option<(Tone, String)>,
    pub reason: String,
    pub tag_edit: String,
    /// The tags last read for the selected entry (the tag field is refreshed from a re-read
    /// only while it still shows them, so tags being typed survive another write's reload).
    pub tags_loaded: Option<(Id, String)>,
    pub scrolls: crate::virtual_list::Scrolls,
    /// The selected subview (Entries, Documents).
    pub tab: usize,
    /// The scope the shown listing was read in.
    pub listed_scope: Option<Value>,
    /// The source a cleanup names, and its preview once read.
    pub cleanup_source: String,
    pub source_preview: Option<SourcePreview>,
    /// A user data import in progress or just finished.
    pub import: Option<ImportStage>,
}

impl KnowledgeScreen {
    pub(crate) fn purge_owner(&self) -> Option<&str> {
        match self.listed_scope.as_ref() {
            Some(scope) if scope["kind"].as_str() == Some("Project") => scope["projectId"].as_str(),
            Some(scope) if scope["kind"].as_str() == Some("Workspace") => None,
            _ => self.project.as_deref(),
        }
    }

    pub fn new() -> Self {
        Self {
            lifecycles: [true, false, false, false],
            ..Self::default()
        }
    }
    /// The scope chosen in the picker (applied by the next search). A project that is no longer
    /// listed (deleted, or no longer visible) is never used: general Knowledge is.
    fn typed_scope(&self) -> Value {
        match self
            .project
            .as_deref()
            .filter(|p| self.project_name(Some(p)).is_some())
        {
            None => json!({"kind": "Workspace"}),
            Some(p) => json!({"kind": "Project", "projectId": p}),
        }
    }
    fn project_name(&self, id: Option<&str>) -> Option<&str> {
        let id = id?;
        self.projects
            .iter()
            .find(|(p, _)| p == id)
            .map(|(_, n)| n.as_str())
    }
    /// Replaces the visible projects; a chosen project no longer among them falls back to
    /// general Knowledge. Returns whether the choice changed.
    pub fn set_projects(&mut self, projects: Vec<(String, String)>) -> bool {
        self.projects = projects;
        if self.project.is_some() && self.project_name(self.project.as_deref()).is_none() {
            self.project = None;
            return true;
        }
        false
    }
    /// Follows the navigation's project selection (`None`: general Knowledge).
    pub fn follow_navigation(&mut self, project: Option<&str>) -> bool {
        let next = project
            .filter(|p| self.project_name(Some(p)).is_some())
            .map(str::to_owned);
        if next == self.project {
            return false;
        }
        self.project = next;
        true
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

    /// Starts a new listing generation and returns its frame body (`knowledge.search`, narrowed
    /// to lessons when only lessons are listed).
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
        // Lessons only narrows the same search (its text, tags, entry kind and lifecycles
        // all still apply).
        if self.lessons_only {
            body["kinds"] = json!(["Lesson"]);
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
                Ok((
                    "knowledge.revise",
                    json!({"scope": current.scope, "claimId": id,
                        "expectedRevision": revision.to_string(),
                        "replacement": revision_of_claim(current, d, kind)}),
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

    /// What a lifecycle change takes from the forms (its reason, when it needs one).
    pub fn change_submission(&self, change: Change) -> Submitted {
        if change.needs_reason() {
            Submitted::Reason(self.reason.clone())
        } else {
            Submitted::Nothing
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
        // Every change acts in the scope the claim lives in (a general claim shown in a project
        // listing stays general); only "Make general" names another scope.
        let scope = serde_json::to_value(&c.scope).expect("scope");
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
                let loaded = value
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
                let edited = self
                    .tags_loaded
                    .as_ref()
                    .is_some_and(|(id, shown)| id.as_str() == claim && *shown != self.tag_edit);
                if !edited {
                    self.tag_edit = loaded.clone();
                }
                if let Some(id) = &self.selected {
                    self.tags_loaded = Some((id.clone(), loaded));
                }
                // An edit draft of this entry at an older revision (its own save succeeded with
                // later edits, or another write changed the entry) is rebased onto the stored
                // one, so its next save applies instead of failing with a revision conflict.
                if let Some(c) = value.claims.0.iter().find(|c| c.id.as_str() == claim)
                    && let Some(d) = self.draft.take()
                {
                    self.draft = Some(match &d.revises {
                        Some((id, revision))
                            if id.as_str() == claim && c.revision.get() > *revision =>
                        {
                            let stored = Draft::of(c);
                            // Without a base, every field counts as the user's.
                            let base = self.draft_base.clone().unwrap_or_else(|| d.clone());
                            self.draft_base = Some(stored.clone());
                            d.rebased(&base, &stored)
                        }
                        _ => d,
                    });
                }
                self.detail = Some(value);
                self.detail_error = None;
            }
            Ok((other, _)) => self.detail_error = Some(format!("Unexpected {}", other.operation())),
            Err(e) => self.detail_error = Some(e.describe()),
        }
    }
    /// The diagnostics query that previews deleting `cleanup_source` in the listed scope.
    pub fn preview_body(&self) -> Result<(String, Value), String> {
        let source = self.cleanup_source.as_str();
        if serde_json::from_value::<devworks_core::protocol::support::Label>(json!(source)).is_err()
        {
            return Err("A source is a nonempty label of at most 256 characters.".into());
        }
        Ok((
            source.to_owned(),
            json!({"scope": self.scope(), "sourceLabel": source}),
        ))
    }
    /// Applies a preview reply for `source` (ignored when the field names another source now).
    pub fn preview_reply(&mut self, source: &str, outcome: QueryOutcome) {
        self.source_preview = None;
        if self.cleanup_source.as_str() != source {
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
            .filter(|p| p.source == self.cleanup_source.as_str())
            .ok_or("Preview the source first.")?;
        Ok((
            "knowledge.delete_by_source",
            json!({"scope": self.scope(), "sourceLabel": p.source,
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
                        egui::TextEdit::multiline(&mut self.cleanup_source)
                            .desired_rows(1)
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
    // -----------------------------------------------------------------------------------------
    // Import from a file

    /// Starts an import of `name`; `false` while another import is in flight.
    pub fn begin_import(&mut self, name: &str) -> bool {
        if self.import.as_ref().is_some_and(ImportStage::busy) {
            return false;
        }
        self.import = Some(ImportStage::Saving {
            name: name.to_owned(),
        });
        true
    }
    /// Applies the import file's save task: its prepared frame (admit it next), or its
    /// `artifacts.save` outcome: the exact preview query (`knowledge.diagnostics` with the saved
    /// version and the format's schema) once stored, the frame kept when the outcome is unknown.
    pub fn import_saved(
        &mut self,
        outcome: Result<crate::backend::TaskOutput, String>,
    ) -> ImportNext {
        use crate::backend::TaskOutput;
        let name = match self.import.clone() {
            Some(ImportStage::Saving { name } | ImportStage::Storing { name, .. }) => name,
            _ => return ImportNext::Nothing,
        };
        let stored = match outcome {
            Ok(TaskOutput::Prepared { frame }) => {
                self.import = Some(ImportStage::Storing {
                    name,
                    frame: frame.clone(),
                });
                return ImportNext::Store(frame);
            }
            Ok(TaskOutput::Stored { outcome, frame }) if crate::backend::is_unknown(&outcome) => {
                self.import = Some(ImportStage::Unconfirmed { name, frame });
                return ImportNext::Nothing;
            }
            Ok(TaskOutput::Stored { outcome, .. }) => {
                ops::saved_artifact(outcome).map_err(|e| e.describe())
            }
            Ok(_) => Err("unexpected task output".into()),
            Err(e) => Err(e),
        };
        match stored {
            Ok(record) => {
                let scope = self.scope();
                let body = json!({"scope": scope, "importSource": record.artifact,
                    "importSchema": import_schema()});
                self.import = Some(ImportStage::Previewing {
                    name,
                    source: record.artifact,
                    scope,
                });
                ImportNext::Preview(body)
            }
            Err(error) => {
                self.import = Some(ImportStage::Failed {
                    name,
                    error: format!("The file was not stored: {error}"),
                });
                ImportNext::Nothing
            }
        }
    }
    /// The import file save whose outcome is not known (or still being admitted): its name
    /// and exact frame.
    pub fn import_frame(&self, unconfirmed_only: bool) -> Option<(String, Vec<u8>)> {
        match self.import.as_ref()? {
            ImportStage::Unconfirmed { name, frame } => Some((name.clone(), frame.clone())),
            ImportStage::Storing { name, frame } if !unconfirmed_only => {
                Some((name.clone(), frame.clone()))
            }
            _ => None,
        }
    }
    /// Applies the preview: one result per candidate and the digest that confirms exactly it.
    pub fn import_previewed(&mut self, outcome: QueryOutcome) {
        let Some(ImportStage::Previewing {
            name,
            source,
            scope,
        }) = self.import.clone()
        else {
            return;
        };
        let reply = match ops::query_result(outcome) {
            Ok((OperationResult::KnowledgeDiagnostics { value }, _)) => Ok(value.items.0),
            Ok((other, _)) => Err(format!("Unexpected {}", other.operation())),
            Err(e) => Err(e.describe()),
        };
        self.import = Some(match reply {
            Ok(items) => {
                let candidates: Vec<ImportCandidate> = items
                    .iter()
                    .filter(|d| d.component.0 == "knowledge.import")
                    .map(|d| ImportCandidate {
                        outcome: d.code.0.clone(),
                        text: d.message.0.clone(),
                    })
                    .collect();
                match items.iter().find(|d| d.component.0 == "knowledge.preview") {
                    Some(p) => ImportStage::Ready {
                        name,
                        source,
                        scope,
                        digest: p.id.as_str().to_owned(),
                        candidates,
                        summary: p.message.0.clone(),
                    },
                    None => ImportStage::Failed {
                        name,
                        error: "The preview was not returned.".into(),
                    },
                }
            }
            Err(error) => ImportStage::Failed {
                name,
                error: format!("The file cannot be imported: {error}"),
            },
        });
    }
    /// The import the preview shows, exactly (same source, scope, schema and digest).
    pub fn import_body(&self) -> Result<Value, String> {
        match &self.import {
            Some(ImportStage::Ready {
                source,
                scope,
                digest,
                ..
            }) => Ok(
                json!({"scope": scope, "source": source, "schema": import_schema(),
                "previewDigest": digest}),
            ),
            _ => Err("Preview a file first.".into()),
        }
    }
    /// The confirmed import was submitted.
    pub fn import_started(&mut self) {
        if let Some(ImportStage::Ready {
            name,
            source,
            scope,
            digest,
            candidates,
            summary,
        }) = self.import.clone()
        {
            self.import = Some(ImportStage::Importing {
                name,
                source,
                scope,
                digest,
                candidates,
                summary,
            });
        }
    }
    /// Applies the import's outcome; `true` when it was recorded. A refusal returns to the
    /// preview; an unknown outcome stays importing until Confirm outcomes settles its frame.
    pub fn import_done(&mut self, outcome: CommandOutcome) -> bool {
        let Some(ImportStage::Importing {
            name,
            source,
            scope,
            digest,
            candidates,
            summary,
        }) = self.import.clone()
        else {
            // An import confirmed in an earlier launch: report its outcome only.
            let recorded = ops::command_result(outcome).is_ok();
            if recorded {
                self.message = Some((Tone::Good, "An earlier import was recorded.".into()));
            }
            return recorded;
        };
        match ops::command_result(outcome) {
            Ok(OperationResult::KnowledgeImportUserData { value }) => {
                let results = candidates
                    .iter()
                    .map(|c| ImportCandidate {
                        outcome: if c.outcome == "would_store" {
                            "stored".into()
                        } else {
                            c.outcome.clone()
                        },
                        text: c.text.clone(),
                    })
                    .collect();
                self.import = Some(ImportStage::Done {
                    name,
                    results,
                    stored: value
                        .claim_count
                        .map_or(value.claim_ids.0.len(), |n| n.get() as usize),
                });
                true
            }
            Err(Failure::Unknown) => {
                // Still importing: Confirm outcomes resubmits this exact frame; until then no
                // other import replaces it.
                self.message = Some((
                    Tone::Attention,
                    "The import's outcome is not confirmed yet. Use Confirm outcomes to check \
                     again; it is never imported twice."
                        .into(),
                ));
                false
            }
            other => {
                let error = match other {
                    Err(Failure::Refused(f)) if f.code == DomainErrorCode::Conflict => {
                        "Knowledge changed since the preview; preview the file again.".to_owned()
                    }
                    Err(e) => e.describe(),
                    Ok(r) => format!("Unexpected {}", r.operation()),
                };
                self.message = Some((Tone::Bad, format!("Import not recorded. {error}")));
                self.import = Some(ImportStage::Ready {
                    name,
                    source,
                    scope,
                    digest,
                    candidates,
                    summary,
                });
                false
            }
        }
    }
    fn import_section(&mut self, ui: &mut Ui, actions: &mut Vec<KnowledgeAction>) {
        ui.horizontal_wrapped(|ui| {
            let busy = self.import.as_ref().is_some_and(ImportStage::busy);
            if ui
                .add_enabled(!busy, egui::Button::new("Import from file"))
                .clicked()
            {
                actions.push(KnowledgeAction::ImportDialog);
            }
            widgets::caption(ui, "or drop a devworks/knowledge-import file here");
        });
        let Some(stage) = self.import.clone() else {
            return;
        };
        egui::Frame::new()
            .stroke(theme::soft_line())
            .corner_radius(theme::radius())
            .inner_margin(theme::margin(12, 10))
            .show(ui, |ui| {
                let candidates = |ui: &mut Ui, list: &[ImportCandidate]| {
                    for c in list {
                        let tone = match c.outcome.as_str() {
                            "would_store" | "stored" => Tone::Good,
                            _ => Tone::Attention,
                        };
                        ui.horizontal_wrapped(|ui| {
                            chip(ui, c.outcome.replace('_', " "), tone);
                            ui.label(&c.text);
                        });
                    }
                };
                match &stage {
                    ImportStage::Saving { name } | ImportStage::Storing { name, .. } => {
                        ui.label(format!("Storing {name}…"));
                    }
                    ImportStage::Unconfirmed { name, .. } => {
                        chip(
                            ui,
                            format!(
                                "{name}: not yet confirmed whether the file was stored. Use \
                                 Confirm outcomes; it is never stored twice."
                            ),
                            Tone::Attention,
                        );
                    }
                    ImportStage::Previewing { name, .. } => {
                        ui.label(format!("Previewing {name}…"));
                    }
                    ImportStage::Ready {
                        name,
                        candidates: list,
                        summary,
                        ..
                    } => {
                        ui.label(RichText::new(format!("Import {name}")).strong());
                        widgets::caption(ui, summary.clone());
                        candidates(ui, list);
                        ui.horizontal(|ui| {
                            if ui.button("Confirm import").clicked() {
                                actions.push(KnowledgeAction::ConfirmImport);
                            }
                            if ui.button("Cancel import").clicked() {
                                actions.push(KnowledgeAction::CancelImport);
                            }
                        });
                    }
                    ImportStage::Importing { name, .. } => {
                        ui.label(format!("Importing {name}…"));
                    }
                    ImportStage::Done {
                        name,
                        results,
                        stored,
                    } => {
                        ui.label(
                            RichText::new(format!("Imported {stored} entries from {name}"))
                                .strong(),
                        );
                        candidates(ui, results);
                        if ui.button("Close import").clicked() {
                            actions.push(KnowledgeAction::CancelImport);
                        }
                    }
                    ImportStage::Failed { name, error } => {
                        chip(ui, format!("{name}: {error}"), Tone::Bad);
                        if ui.button("Close import").clicked() {
                            actions.push(KnowledgeAction::CancelImport);
                        }
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
    /// re-read). Only what this write submitted is cleared, only now (never before the outcome
    /// is known), and only if unchanged since: an edit made while it was saving, or a draft or
    /// reason another write never used, stays. A previewed cleanup is read again (its digest
    /// names the scope revision the write moved).
    pub fn written(&mut self, outcome: CommandOutcome) -> bool {
        let label = self.writing.take().unwrap_or_else(|| "Change".into());
        match ops::command_result(outcome) {
            Ok(result) => {
                let mut kept = false;
                match self.submitted.take() {
                    Some(Submitted::Draft(d)) if self.draft.as_ref() == Some(&d) => {
                        self.draft = None;
                        self.draft_base = None;
                    }
                    Some(Submitted::Draft(d)) => {
                        kept = self.draft.is_some();
                        // The stored entry holds what was submitted: a field still equal to it is
                        // not an edit made since.
                        if let Some(live) = self.draft.as_mut()
                            && live.identity == d.identity
                            && live.revises == d.revises
                        {
                            if d.revises.is_none()
                                && let OperationResult::KnowledgeRecord { value } = &result
                                && let Some(created) = value.claim_ids.0.first()
                            {
                                // The entry it created is what the later edits change: the
                                // draft edits it (selected, so it is read and the draft rebased
                                // onto its revision), never records a second one.
                                live.revises = Some((created.clone(), 0));
                                self.selected = Some(created.clone());
                                self.detail = None;
                                self.detail_error = None;
                            }
                            self.draft_base = Some(d);
                        }
                    }
                    Some(Submitted::Reason(r)) if self.reason == r => self.reason.clear(),
                    _ => {}
                }
                self.source_preview = None;
                self.message = Some((
                    Tone::Good,
                    if kept {
                        format!(
                            "{label} recorded. The edits made while it was saving stay in the \
                             form."
                        )
                    } else {
                        format!("{label} recorded.")
                    },
                ));
                true
            }
            Err(Failure::Unknown) => {
                // Still the write in flight: Knowledge changes stay blocked until Confirm
                // outcomes resubmits its exact frame and a definite outcome arrives, so a new
                // save never replaces it (or commits twice).
                self.writing = Some(label.clone());
                self.message = Some((
                    Tone::Attention,
                    format!(
                        "{label}: the outcome is not confirmed yet. Use Confirm outcomes to check \
                         again; Knowledge changes wait until it is known, and it is never \
                         recorded twice."
                    ),
                ));
                false
            }
            Err(e) => {
                self.submitted = None;
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
                            st.0.draft_base = None;
                        }
                    },
                    |ui, st, tab| {
                        if tab == 2 {
                            return;
                        }
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
            let current = self
                .project_name(self.project.as_deref())
                .unwrap_or(GENERAL)
                .to_owned();
            let mut pick: Option<Option<String>> = None;
            egui::ComboBox::from_id_salt("knowledge-scope")
                .selected_text(current.clone())
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.project.is_none(), GENERAL)
                        .clicked()
                    {
                        pick = Some(None);
                    }
                    for (id, name) in &self.projects {
                        if ui
                            .selectable_label(self.project.as_deref() == Some(id), name)
                            .clicked()
                        {
                            pick = Some(Some(id.clone()));
                        }
                    }
                })
                .response
                .widget_info(|| {
                    let mut info =
                        egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Knowledge scope");
                    info.current_text_value = Some(current.clone());
                    info
                });
            if let Some(p) = pick
                && p != self.project
            {
                self.project = p;
                actions.push(KnowledgeAction::Search);
            }
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
        self.import_section(ui, actions);
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
            }
            let challenges = c.challenges.as_ref().map(|v| v.0.as_slice())
                .or_else(|| c.lesson.as_ref().map(|l| l.challenges.0.as_slice())).unwrap_or_default();
            for ch in challenges {
                widgets::caption(ui, format!("Challenge ({}): {}", wire(&ch.state), ch.reason.0));
                if !ch.evidence_ids.0.is_empty() {
                    widgets::caption(ui, format!("Citations: {}", ch.evidence_ids.0.iter().map(Id::as_str).collect::<Vec<_>>().join(", ")));
                }
                if let Some(resolution) = &ch.resolution {
                    widgets::caption(ui, format!("Resolution: {}", resolution.0));
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
                    let opened = Draft::of(&c);
                    self.draft_base = Some(opened.clone());
                    self.draft = Some(opened);
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
        s.set_projects(vec![("project-x".into(), "Project X".into())]);
        s.project = Some("project-x".into());
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
        s.project = None;
        let (_, body) = s.begin_listing(true);
        assert_eq!(
            body["scope"],
            json!({"kind": "Project", "projectId": "project-x"})
        );
        assert_eq!(body["page"]["cursor"], json!("c"));
    }

    fn general_lesson() -> KnowledgeClaim {
        serde_json::from_value(json!({
            "id": "claim-g", "revision": "4", "kind": "Lesson", "subjectId": "entity-s",
            "predicate": "observed", "objectId": "entity-o", "value": "Old text",
            "confidence": 0.3, "state": "Active", "evidenceIds": ["ev-1", "ev-2"],
            "tags": ["lesson"], "scope": {"kind": "Workspace"},
            "valueJson": {"steps": [1, 2]}, "normalizedText": "old text body",
            "effectiveAtMs": "9", "source": "run-7", "entryKind": "Lesson",
            "lifecycle": "Active", "originRef": "message-3",
            "lesson": {"ruleState": "Candidate", "source": "Agent",
                "applicability": {"activities": ["reviewing"], "paths": ["src/lib.rs"],
                    "tools": ["gh"], "harness": "claude-code", "repositoryIds": ["repo-a"]},
                "recurrence": "1", "recurrenceThreshold": "3", "challenges": [],
                "addressedBy": [], "deliveredSessions": "0"}
        }))
        .unwrap()
    }
    fn showing(claim: KnowledgeClaim) -> KnowledgeScreen {
        let mut s = KnowledgeScreen::new();
        s.set_projects(vec![("project-x".into(), "Project X".into())]);
        s.project = Some("project-x".into());
        s.begin_listing(false);
        s.selected = Some(claim.id.clone());
        s.items = vec![claim];
        s
    }

    #[test]
    fn an_edit_replaces_only_what_the_editor_shows() {
        let c = general_lesson();
        let s = showing(c.clone());
        let d = Draft {
            identity: next_draft_identity(),
            revises: Some((c.id.clone(), 4)),
            entry_kind: "Lesson",
            subject: String::new(),
            value: "New text".into(),
            tags: "lesson, edited".into(),
            activities: "implementing".into(),
        };
        let (op, body) = s.save_body(&d).unwrap();
        assert_eq!(op, "knowledge.revise");
        let r = &body["replacement"];
        assert_eq!(r["value"], json!("New text"));
        assert_eq!(r["tags"], json!(["lesson", "edited"]));
        // Everything the editor does not show is the claim's own.
        assert_eq!(r["evidenceIds"], json!(["ev-1", "ev-2"]));
        assert_eq!(r["objectId"], json!("entity-o"));
        assert_eq!(r["predicate"], json!("observed"));
        assert_eq!(r["confidence"], json!(0.3));
        assert_eq!(r["valueJson"], json!({"steps": [1, 2]}));
        assert_eq!(r["normalizedText"], json!("old text body"));
        assert_eq!(r["source"], json!("run-7"));
        assert_eq!(r["effectiveAtMs"], json!("9"));
        assert_eq!(r["lesson"]["source"], json!("Agent"));
        let a = &r["lesson"]["applicability"];
        assert_eq!(a["activities"], json!(["implementing"]));
        assert_eq!(a["paths"], json!(["src/lib.rs"]));
        assert_eq!(a["tools"], json!(["gh"]));
        assert_eq!(a["harness"], json!("claude-code"));
        // The edit acts in the claim's scope, not the project listing's.
        assert_eq!(body["scope"], json!({"kind": "Workspace"}));
    }

    #[test]
    fn changes_act_in_the_claims_own_scope() {
        let mut s = showing(general_lesson());
        s.reason = "checked".into();
        s.tag_edit = "lesson".into();
        for change in [
            Change::Verify,
            Change::Retract,
            Change::Challenge,
            Change::Retire,
            Change::Delete,
            Change::SaveTags,
        ] {
            let (_, body) = s.change_body(change).unwrap();
            assert_eq!(body["scope"], json!({"kind": "Workspace"}), "{change:?}");
        }
        // Making a general lesson effective (or rejecting it) keeps it general.
        for change in [Change::Promote, Change::Reject] {
            let (_, body) = s.change_body(change).unwrap();
            assert_eq!(
                body["destination"],
                json!({"kind": "Workspace"}),
                "{change:?}"
            );
        }
        // Only the explicit scope change names another scope.
        let (op, body) = s.change_body(Change::MakeGeneral).unwrap();
        assert_eq!(op, "knowledge.scope");
        assert_eq!(body["destination"], json!({"kind": "Workspace"}));
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
