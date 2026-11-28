//! The Knowledge workspace (KNOW-01): one tagged store for memory, knowledge and lessons with
//! scope selection, word and tag search, entry-kind and lifecycle filters, manual creation and
//! editing, lifecycle changes (verify, retract, challenge, retire, promote, reject), tags,
//! permissioned deletion and source evidence. Every request is the same `knowledge.*`
//! operation an agent tool submits; this module holds view state and drafts only.
//!
//! The graph view ([`graph`]) reads claim and entity bundles, an entity's edges, subgraphs, a
//! graph query, one frontier step at a time and a rendered summary; the entry detail relates
//! the selected entry to another (`knowledge.relate`). The Lessons filter can list a lesson
//! view (`knowledge.tagged_lessons`), and the last write's receipt is read back
//! (`knowledge.write_status`) so a write still running says so.
//!
//! Markers move only on confirmed outcomes: a draft stays in its form until the write is
//! recorded; a search reply from an older request generation is ignored; a page continuation
//! whose cursor expired (the scope changed) restarts the listing instead of mixing revisions.
pub mod graph;

use crate::backend::Backend;
use crate::frame::{self, Negotiate};
use crate::ops::{self, Failure};
use crate::paged::{self, Applied, Fetch, Page, Paged};
use crate::theme;
use crate::widgets::{self, Tone, ViewState, chip};
use devworks_core::gateway::{CommandOutcome, QueryOutcome};
use devworks_core::operations::{OperationInput as In, knowledge};
use devworks_core::protocol::OperationState;
use devworks_core::protocol::data::DataScope;
use devworks_core::protocol::provider::{ArtifactRef, SchemaRef};
use devworks_core::protocol::records::{
    Applicability, ChallengeOutcome, ClaimDraft, KnowledgeBatch, KnowledgeBundle, KnowledgeClaim,
    KnowledgeEntityDraft, KnowledgeKind, KnowledgeRetrievalMode, KnowledgeSearchResult,
    KnowledgeWriteReceipt, LessonDraft, LessonSource, PromoteAction, RevisionedRef,
};
use devworks_core::protocol::results::OperationResult;
use devworks_core::protocol::support::{BoundedList, Limit, Probability};
use devworks_core::protocol::wire_name;
use devworks_core::protocol::{DomainErrorCode, Id};
use eframe::egui::{self, RichText, Ui};

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
/// How a person relates two entries (`knowledge.relate` predicates): the label shown and the
/// predicate sent. `same_as` merges two lessons and `unmerge` undoes that merge.
pub const RELATIONS: [(&str, &str); 6] = [
    ("Related to", "related_to"),
    ("Supports", "supports"),
    ("Depends on", "depends_on"),
    ("Contradicts", "contradicts"),
    ("Same lesson as", "same_as"),
    ("Unmerge from", "unmerge"),
];
/// The lesson views of `knowledge.tagged_lessons` (label, wire name).
pub const LESSON_VIEWS: [(&str, &str); 5] = [
    ("All lessons", "All"),
    ("Recently learned", "RecentlyLearned"),
    ("Recurring", "Recurring"),
    ("Unaddressed", "Unaddressed"),
    ("Undelivered", "Undelivered"),
];
/// The rule states a lesson view can narrow to.
pub const RULE_STATES: [&str; 5] = ["Candidate", "Proposed", "Effective", "Rejected", "Retired"];
/// The tag every lesson carries (a lesson view lists by tag).
const LESSON_TAG: &str = "lesson";

/// A Knowledge request at the operation's newest schema revision (so revision-2 members are
/// kept). A request that did not convert (`None`) is sent as no bytes, which the gateway rejects
/// like any malformed request.
pub fn query(backend: &dyn Backend, input: impl Into<Option<In>>) -> Vec<u8> {
    frame::build(backend, Negotiate::Latest, input.into())
}
pub fn command(backend: &dyn Backend, input: impl Into<Option<In>>) -> Vec<u8> {
    frame::build(backend, Negotiate::Latest, input.into())
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
pub fn claim_draft(c: &KnowledgeClaim, d: &Draft, kind: &str) -> Option<ClaimDraft> {
    let lesson = match &c.lesson {
        Some(l) => {
            let mut applicability = l.applicability.clone();
            applicability.activities = frame::texts(&split(&d.activities))?;
            Some(LessonDraft {
                source: l.source.clone(),
                source_ref: None,
                quoted_words: None,
                applicability,
                same_as: None,
            })
        }
        None => None,
    };
    Some(ClaimDraft {
        kind: frame::text(kind)?,
        subject_id: c.subject_id.clone(),
        predicate: c.predicate.clone(),
        object_id: c.object_id.clone(),
        value: Some(frame::text(d.value.trim())?),
        confidence: c.confidence.clone(),
        evidence_ids: c.evidence_ids.clone(),
        tags: frame::texts(&split(&d.tags))?,
        value_json: c.value_json.clone(),
        normalized_text: c.normalized_text.clone(),
        effective_at_ms: c.effective_at_ms.clone(),
        supersedes: None,
        source: c.source.clone(),
        entry_kind: Some(frame::text(d.entry_kind)?),
        origin: None,
        origin_ref: c.origin_ref.clone(),
        lesson,
        source_revision: None,
    })
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
                .find(|e| Some(e.to_string()) == c.entry_kind.as_ref().map(wire_name))
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
    /// A relation (its predicate) to the entry picked (cleared once recorded, if still picked).
    Relation {
        object: Option<Id>,
        predicate: &'static str,
    },
}

/// The last recorded write and its receipt as `knowledge.write_status` reads it back.
#[derive(Clone, Debug, PartialEq)]
pub struct LastWrite {
    pub label: String,
    pub write_id: Id,
    /// The receipt in words (`None` until it is read).
    pub status: Option<String>,
    /// Whether the write is still running (a delete by source continuing as a job).
    pub running: bool,
}

/// The write ID of a recorded Knowledge write's result.
fn write_id_of(result: &OperationResult) -> Option<Id> {
    match result {
        OperationResult::KnowledgeRecord { value }
        | OperationResult::KnowledgeRevise { value }
        | OperationResult::KnowledgeRelate { value }
        | OperationResult::KnowledgeTag { value }
        | OperationResult::KnowledgeVerify { value }
        | OperationResult::KnowledgeRetract { value }
        | OperationResult::KnowledgeScope { value }
        | OperationResult::KnowledgePromote { value }
        | OperationResult::KnowledgeChallenge { value } => Some(value.write_id.clone()),
        OperationResult::KnowledgeDeleteBySource { value }
        | OperationResult::KnowledgeDeleteSelected { value } => Some(value.id.clone()),
        _ => None,
    }
}

/// A write receipt in words, and whether the write is still running.
fn receipt_words(r: &KnowledgeWriteReceipt) -> (String, bool) {
    let running = matches!(r.state, OperationState::Dispatching);
    let state = match r.state {
        OperationState::ObservedComplete => "complete".to_owned(),
        OperationState::Dispatching => "still running".to_owned(),
        OperationState::Failed => "failed".to_owned(),
        OperationState::OutcomeUnknown => "outcome not known yet".to_owned(),
        ref other => wire_name(other),
    };
    let claims = r
        .claim_count
        .as_ref()
        .map_or(r.claim_ids.0.len() as i64, |n| n.get());
    let entities = r
        .entity_count
        .as_ref()
        .map_or(r.entity_ids.0.len() as i64, |n| n.get());
    (
        format!(
            "{state} · {claims} entries, {entities} entities at revision {}",
            r.revision.get()
        ),
        running,
    )
}

/// A relation's refusal in plain words.
fn relate_refusal(e: &Failure, predicate: &str) -> String {
    let Failure::Refused(f) = e else {
        return e.describe();
    };
    match (&f.code, f.target_kind.as_ref().map(|k| k.0.as_str())) {
        (DomainErrorCode::InvalidArgument, Some("object_id")) => {
            "An entry cannot be related to itself.".into()
        }
        (DomainErrorCode::InvalidArgument, Some("contradicts")) => {
            "Only two entries can contradict each other.".into()
        }
        (DomainErrorCode::InvalidArgument, Some("knowledge_lesson")) => {
            "Same lesson as and Unmerge from relate two lessons: choose two lessons.".into()
        }
        (DomainErrorCode::Conflict, Some("knowledge_lifecycle")) => {
            "One of the lessons is not active, or is not merged into the other. Read them again."
                .into()
        }
        // Only a merge or unmerge of two lessons needs both in the exact scope shown.
        (DomainErrorCode::PermissionDenied, Some("knowledge_scope"))
            if matches!(predicate, "same_as" | "unmerge") =>
        {
            "Both lessons must belong to the scope shown to merge them.".into()
        }
        (DomainErrorCode::PermissionDenied, _) => {
            "You cannot change Knowledge in this scope.".into()
        }
        (DomainErrorCode::NotFound, _) => {
            "One of the entries is no longer in this scope. Search again.".into()
        }
        _ => e.describe(),
    }
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
        scope: Option<String>,
    },
    Ready {
        name: String,
        source: ArtifactRef,
        scope: Option<String>,
        digest: String,
        candidates: Vec<ImportCandidate>,
        summary: String,
    },
    Importing {
        name: String,
        source: ArtifactRef,
        scope: Option<String>,
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
#[derive(Clone, Debug)]
pub enum ImportNext {
    /// Admit this exact `artifacts.save` frame.
    Store(Vec<u8>),
    /// Run this preview query (`knowledge.diagnostics`).
    Preview(Option<In>),
    Nothing,
}
/// The import format's schema reference.
fn import_schema() -> Option<SchemaRef> {
    use devworks_core::protocol::helpers::knowledge::{
        IMPORT_SCHEMA_ID, IMPORT_SCHEMA_REVISION, import_schema_digest,
    };
    Some(SchemaRef {
        schema_id: frame::text(IMPORT_SCHEMA_ID)?,
        revision: frame::number(IMPORT_SCHEMA_REVISION)?,
        sha256: frame::text(&import_schema_digest())?,
    })
}
/// The preview query of an import file saved as `source`, read in the scope of `project`.
pub fn import_preview_input(project: Option<&str>, source: &ArtifactRef) -> Option<In> {
    Some(In::Knowledge(knowledge::Input::Diagnostics(
        knowledge::DiagnosticsInput {
            import_source: Some(source.clone()),
            import_schema: Some(import_schema()?),
            ..diagnostics(frame::scope(project)?)
        },
    )))
}
/// A `knowledge.diagnostics` query of `scope` with no preview members.
fn diagnostics(scope: DataScope) -> knowledge::DiagnosticsInput {
    knowledge::DiagnosticsInput {
        scope,
        source_id: None,
        import_source: None,
        import_schema: None,
        source_label: None,
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
    /// Chooses a file to import (the native file dialog).
    ImportDialog,
    /// Imports these files (the first; a dropped file arrives here).
    ImportFiles(Vec<std::path::PathBuf>),
    /// Confirms the previewed import exactly.
    ConfirmImport,
    CancelImport,
    /// A graph view request (bundle, edges, subgraph, query, frontier step, summary).
    Graph(graph::GraphAction),
    /// Relates the selected entry to the one picked (`knowledge.relate`).
    Relate,
}

/// The exact preview of a delete by source.
#[derive(Clone, Debug, PartialEq)]
pub struct SourcePreview {
    pub source: String,
    pub digest: String,
    pub scope_revision: i64,
    pub message: String,
}

/// What a listing reply asks of the caller.
#[derive(Debug, PartialEq, Eq)]
pub enum Listed {
    /// Nothing more to send.
    Done,
    /// The continuation expired: list again from the first page.
    Restart,
    /// A gathering re-read needs the page this request reads.
    Continue(Fetch),
}

#[derive(Default)]
pub struct KnowledgeScreen {
    pub documents: crate::documents_screen::DocumentsScreen,
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
    /// The listed entries: a listing reads the first page and each Load more the next.
    pub claims: Paged<KnowledgeClaim>,
    pub selected: Option<Id>,
    pub detail: Option<KnowledgeBundle>,
    pub detail_error: Option<String>,
    /// The retrieval diagnostic's code and message (`knowledge.diagnostics`).
    pub retrieval: Option<(String, String)>,
    /// How the shown search was ranked: its retrieval mode and whether fusion ranked only its
    /// head (`None` for a lesson listing or a reply without them).
    pub search_ranking: Option<(KnowledgeRetrievalMode, bool)>,
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
    /// The scope the shown listing was read in: its project (`None`: general Knowledge), or
    /// `None` before any listing.
    pub listed_scope: Option<Option<String>>,
    /// The first-page request the listing shown was read with: a re-read and a continuation are
    /// built from it, so a filter typed since (and not submitted) never reaches them.
    listed_query: Option<(&'static str, Option<In>)>,
    /// The source a cleanup names, and its preview once read.
    pub cleanup_source: String,
    pub source_preview: Option<SourcePreview>,
    /// A user data import in progress or just finished.
    pub import: Option<ImportStage>,
    /// The graph view (bundles, edges, subgraphs, query, frontier steps, summary).
    pub graph: graph::Graph,
    /// The relation the entry detail sends ([`RELATIONS`] index) and the entry it names.
    pub relate_predicate: usize,
    pub relate_object: Option<Id>,
    /// With Lessons on: a lesson view ([`LESSON_VIEWS`] wire name) lists through
    /// `knowledge.tagged_lessons`; `None` narrows the search instead.
    pub lesson_view: Option<&'static str>,
    /// A lesson view's activity and rule-state filters.
    pub lesson_activity: String,
    pub rule_state: Option<&'static str>,
    /// The last recorded write and its read-back receipt.
    pub last_write: Option<LastWrite>,
}

impl KnowledgeScreen {
    pub(crate) fn purge_owner(&self) -> Option<&str> {
        match self.listed_scope.as_ref() {
            Some(project) => project.as_deref(),
            None => self.project.as_deref(),
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
    fn typed_scope(&self) -> Option<String> {
        self.project
            .clone()
            .filter(|p| self.project_name(Some(p)).is_some())
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
    fn scope_project(&self) -> Option<String> {
        self.listed_scope
            .clone()
            .unwrap_or_else(|| self.typed_scope())
    }
    fn scope(&self) -> Option<DataScope> {
        frame::scope(self.scope_project().as_deref())
    }

    // -----------------------------------------------------------------------------------------
    // Frames

    /// Starts a new listing generation and returns its request (`knowledge.search`, narrowed
    /// to lessons when only lessons are listed).
    pub fn begin_listing(&mut self, append: bool) -> (&'static str, Option<In>) {
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
                self.claims.reset();
                self.source_preview = None;
                self.graph.close();
                self.relate_object = None;
            }
            self.listed_scope = Some(scope);
        }
        if append {
            let cursor = self.claims.more_page().and_then(|f| f.cursor);
            return self.listing_after(cursor);
        }
        self.claims.reload();
        let first = self.listing_input(None);
        self.listed_query = Some(first.clone());
        first
    }
    /// Starts re-reading the pages shown, in the scope and query listed, and returns the first
    /// request (a changed scope starts over like [`begin_listing`](Self::begin_listing)).
    pub fn begin_refresh(&mut self) -> (&'static str, Option<In>) {
        if self.listed_query.is_none() || self.listed_scope.as_ref() != Some(&self.typed_scope()) {
            return self.begin_listing(false);
        }
        let fetch = self.claims.reread();
        self.listing_after(fetch.cursor)
    }
    /// The request after `fetch`, which [`Applied::Continue`] named for a gathering read.
    pub fn continue_listing(&self, fetch: &Fetch) -> (&'static str, Option<In>) {
        self.listing_after(fetch.cursor.clone())
    }
    /// The request for the page after `cursor` (the first without one) of the listed query.
    fn listing_after(&self, cursor: Option<String>) -> (&'static str, Option<In>) {
        let Some((op, first)) = &self.listed_query else {
            return self.listing_input(cursor);
        };
        let input = match cursor {
            None => first.clone(),
            Some(cursor) => first.clone().and_then(|mut input| {
                match &mut input {
                    In::Knowledge(knowledge::Input::Search(search)) => {
                        search.page = frame::page(PAGE, Some(&cursor))?;
                    }
                    In::Knowledge(knowledge::Input::TaggedLessons(lessons)) => {
                        lessons.page = frame::page(PAGE, Some(&cursor))?;
                    }
                    _ => return None,
                }
                Some(input)
            }),
        };
        (op, input)
    }
    fn listing_input(&self, cursor: Option<String>) -> (&'static str, Option<In>) {
        let states: Vec<String> = LIFECYCLES
            .iter()
            .zip(self.lifecycles)
            .filter(|(_, on)| *on)
            .map(|(s, _)| (*s).to_owned())
            .collect();
        let search = || {
            Some(In::Knowledge(knowledge::Input::Search(
                knowledge::SearchInput {
                    scope: self.scope()?,
                    text: frame::text(if self.text.trim().is_empty() {
                        " "
                    } else {
                        self.text.as_str()
                    })?,
                    tags: frame::texts(&split(&self.tags))?,
                    // Lessons only narrows the same search (its text, tags, entry kind and
                    // lifecycles all still apply).
                    kinds: BoundedList(if self.lessons_only {
                        vec![KnowledgeKind::Lesson]
                    } else {
                        Vec::new()
                    }),
                    page: frame::page(PAGE, cursor.as_deref())?,
                    entity_limit: Limit::new(500)?,
                    claim_limit: Limit::new(PAGE)?,
                    states: Some(frame::texts(&if states.is_empty() {
                        vec!["Active".to_owned()]
                    } else {
                        states.clone()
                    })?),
                    entry_kinds: frame::lift(
                        self.entry_kind.map(|k| frame::texts(&[k.to_owned()])),
                    )?,
                    source: None,
                    stale: None,
                    // A person searching here pages by count: no token budget.
                    budget_tokens: None,
                },
            )))
        };
        if self.lessons_only
            && let Some(view) = self.lesson_view
        {
            return ("knowledge.tagged_lessons", self.lessons_input(view, cursor));
        }
        ("knowledge.search", search())
    }
    /// A lesson view of the listed scope: the lessons carrying the tags typed (every lesson
    /// carries `lesson`), narrowed to an activity and rule state when chosen.
    fn lessons_input(&self, view: &str, cursor: Option<String>) -> Option<In> {
        let mut tags = split(&self.tags);
        if tags.is_empty() {
            tags.push(LESSON_TAG.to_owned());
        }
        let activity = self.lesson_activity.trim();
        let rule_states: Vec<String> = self.rule_state.map(str::to_owned).into_iter().collect();
        Some(In::Knowledge(knowledge::Input::TaggedLessons(
            knowledge::TaggedLessonsInput {
                scope: self.scope()?,
                tags: BoundedList(frame::texts::<_, 128>(&tags)?.0),
                page: frame::page(PAGE, cursor.as_deref())?,
                rule_states: Some(frame::texts(&rule_states)?),
                activity: frame::lift((!activity.is_empty()).then(|| frame::text(activity)))?,
                view: Some(frame::text(view)?),
            },
        )))
    }
    /// The scope graph reads and relations use: the listing's (never one typed and not
    /// searched).
    pub fn graph_scope(&self) -> Option<DataScope> {
        self.scope()
    }
    /// The relation the entry detail describes: the selected entry, the relation chosen and
    /// the entry picked, in the listed scope. `knowledge.relate` names no entry revision: it
    /// adds a relation record (a merge of two lessons checks that both are active).
    pub fn relate_input(&self) -> Result<Option<In>, String> {
        let c = self.selected_claim().ok_or("Select an entry first.")?;
        let object = self
            .relate_object
            .as_ref()
            .ok_or("Choose the entry to relate it to.")?;
        if *object == c.id {
            return Err("An entry cannot be related to itself.".into());
        }
        let (_, predicate) = RELATIONS
            .get(self.relate_predicate)
            .ok_or("Choose how the entries relate.")?;
        Ok((|| {
            Some(In::Knowledge(knowledge::Input::Relate(
                knowledge::RelateInput {
                    scope: self.scope()?,
                    subject_id: c.id.clone(),
                    predicate: frame::text(predicate)?,
                    object_id: object.clone(),
                    evidence_ids: BoundedList(Vec::new()),
                },
            )))
        })())
    }
    /// The predicate the relation form sends.
    pub fn relate_predicate_wire(&self) -> &'static str {
        RELATIONS
            .get(self.relate_predicate)
            .map_or("related_to", |(_, p)| *p)
    }
    /// The entries the selected one can be related to: the listing's and the graph view's,
    /// without the selected entry.
    pub fn relate_candidates(&self) -> Vec<(Id, String)> {
        let mut out: Vec<(Id, String)> = Vec::new();
        let graph = self
            .graph
            .view
            .iter()
            .flat_map(|v| v.bundle.claims.0.iter());
        for c in self.claims.items.iter().chain(graph) {
            if Some(&c.id) == self.selected.as_ref() || out.iter().any(|(id, _)| *id == c.id) {
                continue;
            }
            out.push((c.id.clone(), graph::text_of(c)));
        }
        out
    }
    /// The `knowledge.write_status` read of the last recorded write: its ID and request.
    pub fn write_status_input(&self) -> Option<(String, Option<In>)> {
        let w = self.last_write.as_ref()?;
        Some((
            w.write_id.as_str().to_owned(),
            Some(In::Knowledge(knowledge::Input::WriteStatus(
                knowledge::WriteStatusInput {
                    write_id: w.write_id.clone(),
                    cursor: None,
                },
            ))),
        ))
    }
    /// Applies the last write's receipt (ignored when another write is the last now).
    pub fn write_status_reply(&mut self, write_id: &str, outcome: QueryOutcome) {
        let Some(w) = self
            .last_write
            .as_mut()
            .filter(|w| w.write_id.as_str() == write_id)
        else {
            return;
        };
        match ops::query_result(outcome) {
            Ok((OperationResult::KnowledgeWriteStatus { value }, _)) => {
                let (words, running) = receipt_words(&value);
                w.status = Some(words);
                w.running = running;
            }
            Ok((other, _)) => {
                w.status = Some(format!("unexpected reply {}", other.operation()));
                w.running = false;
            }
            Err(e) => {
                w.status = Some(format!("its receipt could not be read. {}", e.describe()));
                w.running = false;
            }
        }
    }
    pub fn read_input(&self, claim: &Id) -> Option<In> {
        Some(In::Knowledge(knowledge::Input::Read(
            knowledge::ReadInput {
                scope: self.scope()?,
                claim_id: claim.clone(),
                revision: None,
            },
        )))
    }
    pub fn diagnostics_input(&self) -> Option<In> {
        Some(In::Knowledge(knowledge::Input::Diagnostics(diagnostics(
            self.scope()?,
        ))))
    }
    fn selected_claim(&self) -> Option<&KnowledgeClaim> {
        let id = self.selected.as_ref()?;
        self.detail
            .as_ref()
            .and_then(|b| b.claims.0.iter().find(|c| c.id == *id))
            .or_else(|| self.claims.items.iter().find(|c| c.id == *id))
    }

    /// The command a draft submits: a new entry (with its subject entity) or a revision.
    pub fn save_input(&self, d: &Draft) -> Result<(&'static str, Option<In>), String> {
        if d.value.trim().is_empty() {
            return Err("Write what the entry says.".into());
        }
        let kind = ENTRY_KINDS
            .iter()
            .find(|(e, _)| *e == d.entry_kind)
            .map(|(_, k)| *k)
            .ok_or("Unknown entry kind.")?;
        match &d.revises {
            Some((id, revision)) => {
                let Some(current) = self.selected_claim().filter(|c| c.id == *id) else {
                    return Err("Select the entry again before saving.".into());
                };
                Ok((
                    "knowledge.revise",
                    (|| {
                        Some(In::Knowledge(knowledge::Input::Revise(
                            knowledge::ReviseInput {
                                scope: current.scope.clone(),
                                claim_id: id.clone(),
                                expected_revision: frame::number(*revision)?,
                                replacement: claim_draft(current, d, kind)?,
                            },
                        )))
                    })(),
                ))
            }
            None => Ok(("knowledge.record", self.record_input(d, kind))),
        }
    }
    /// The `knowledge.record` of a new entry of recorded `kind`, under its subject entity.
    fn record_input(&self, d: &Draft, kind: &str) -> Option<In> {
        let subject = if d.subject.trim().is_empty() {
            "General"
        } else {
            d.subject.trim()
        };
        let lesson = if d.entry_kind == "Lesson" {
            Some(LessonDraft {
                source: LessonSource::Prompt,
                source_ref: None,
                quoted_words: None,
                applicability: Applicability {
                    activities: frame::texts(&split(&d.activities))?,
                    paths: BoundedList(Vec::new()),
                    tools: BoundedList(Vec::new()),
                    harness: None,
                    repository_ids: BoundedList(Vec::new()),
                },
                same_as: None,
            })
        } else {
            None
        };
        let claim = ClaimDraft {
            kind: frame::text(kind)?,
            subject_id: frame::id("subject")?,
            predicate: frame::text(&d.entry_kind.to_lowercase())?,
            object_id: None,
            value: Some(frame::text(d.value.trim())?),
            confidence: Probability::new(0.8)?,
            evidence_ids: BoundedList(Vec::new()),
            tags: frame::texts(&split(&d.tags))?,
            value_json: None,
            normalized_text: None,
            effective_at_ms: None,
            supersedes: None,
            source: None,
            entry_kind: Some(frame::text(d.entry_kind)?),
            origin: None,
            origin_ref: None,
            lesson,
            source_revision: None,
        };
        Some(In::Knowledge(knowledge::Input::Record(
            knowledge::RecordInput {
                scope: self.scope()?,
                batch: KnowledgeBatch {
                    entities: BoundedList(vec![KnowledgeEntityDraft {
                        client_id: frame::id("subject")?,
                        label: frame::text(subject)?,
                        entity_type: frame::text("topic")?,
                        aliases: BoundedList(Vec::new()),
                        canonical_name: None,
                        external_id: None,
                        source: None,
                    }]),
                    claims: BoundedList(vec![claim]),
                    evidence: BoundedList(Vec::new()),
                },
            },
        )))
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
    pub fn change_input(&self, change: Change) -> Result<(&'static str, Option<In>), String> {
        let c = self.selected_claim().ok_or("Select an entry first.")?;
        let reason = self.reason.trim();
        if change.needs_reason() && reason.is_empty() {
            return Err(format!("{} needs a reason.", change.label()));
        }
        // Every change acts in the scope the claim lives in (a general claim shown in a project
        // listing stays general); only "Make general" names another scope.
        let scope = c.scope.clone();
        let at = || RevisionedRef {
            id: c.id.clone(),
            revision: c.revision.clone(),
        };
        let none = || BoundedList(Vec::new());
        let one = || BoundedList(vec![at()]);
        let challenge = |outcome| {
            Some(knowledge::ChallengeInput {
                scope: scope.clone(),
                claim_id: c.id.clone(),
                expected_revision: c.revision.clone(),
                reason: frame::text(reason)?,
                evidence_ids: none(),
                resolve_challenge_id: None,
                outcome,
            })
        };
        let promote = |action, reason| {
            Some(knowledge::PromoteInput {
                claim_id: c.id.clone(),
                expected_revision: c.revision.clone(),
                destination: scope.clone(),
                proposal_id: None,
                action,
                address: None,
                reason,
            })
        };
        let k = In::Knowledge;
        Ok(match change {
            Change::Verify => (
                "knowledge.verify",
                (|| {
                    Some(k(knowledge::Input::Verify(knowledge::VerifyInput {
                        scope: scope.clone(),
                        claim_id: c.id.clone(),
                        expected_revision: c.revision.clone(),
                        reason: frame::text(reason)?,
                        evidence_ids: none(),
                        source_revision: None,
                    })))
                })(),
            ),
            Change::Retract => (
                "knowledge.retract",
                (|| {
                    Some(k(knowledge::Input::Retract(knowledge::RetractInput {
                        scope: scope.clone(),
                        claim_id: c.id.clone(),
                        expected_revision: c.revision.clone(),
                        reason: frame::text(reason)?,
                        evidence_ids: none(),
                    })))
                })(),
            ),
            Change::Challenge => (
                "knowledge.challenge",
                challenge(None).map(|i| k(knowledge::Input::Challenge(i))),
            ),
            Change::Retire => (
                "knowledge.challenge",
                challenge(Some(ChallengeOutcome::Retire))
                    .map(|i| k(knowledge::Input::Challenge(i))),
            ),
            Change::Promote => (
                "knowledge.promote",
                promote(None, None).map(|i| k(knowledge::Input::Promote(i))),
            ),
            Change::Reject => (
                "knowledge.promote",
                (|| {
                    promote(Some(PromoteAction::Reject), Some(frame::text(reason)?))
                        .map(|i| k(knowledge::Input::Promote(i)))
                })(),
            ),
            Change::Delete => (
                "knowledge.delete_selected",
                Some(k(knowledge::Input::DeleteSelected(
                    knowledge::DeleteSelectedInput {
                        scope: scope.clone(),
                        claims: one(),
                        proposal_id: None,
                    },
                ))),
            ),
            Change::MakeGeneral => (
                "knowledge.scope",
                Some(k(knowledge::Input::Scope(knowledge::ScopeInput {
                    claim_ids: one(),
                    destination: DataScope::Workspace,
                }))),
            ),
            Change::SaveTags => (
                "knowledge.tag",
                (|| {
                    Some(k(knowledge::Input::Tag(knowledge::TagInput {
                        scope: scope.clone(),
                        claim_ids: BoundedList(vec![c.id.clone()]),
                        expected_revisions: one(),
                        tags: frame::texts(&split(&self.tag_edit))?,
                    })))
                })(),
            ),
        })
    }

    // -----------------------------------------------------------------------------------------
    // Replies

    /// Applies a listing reply of `generation`; says whether the listing must restart (its
    /// continuation expired because the scope changed) or a gathering re-read needs its next
    /// page.
    pub fn listed(&mut self, generation: u64, outcome: QueryOutcome) -> Listed {
        // A search page carries its retrieval mode beside the claims; the listing shows claims.
        let mut ranking = None;
        let outcome = ops::query_result(outcome).map(|(result, revision)| {
            let page = match result {
                OperationResult::KnowledgeSearch {
                    value: KnowledgeSearchResult::Current(value),
                } => {
                    ranking = Some((value.mode, value.work_limited));
                    Ok((value.items.0, value.next_cursor))
                }
                OperationResult::KnowledgeSearch {
                    value: KnowledgeSearchResult::Legacy(value),
                }
                | OperationResult::KnowledgeTaggedLessons { value } => {
                    Ok((value.items.0, value.next_cursor))
                }
                other => Err(other),
            };
            (page, revision)
        });
        match outcome {
            Ok((Ok((items, cursor)), _)) => {
                let page = Page::new(items, cursor.map(|c| c.0));
                let applied = self.claims.apply(generation, Ok(page));
                if matches!(
                    applied,
                    Applied::Replaced | Applied::Appended | Applied::Continue(_)
                ) {
                    self.search_ranking = ranking;
                }
                match applied {
                    Applied::Continue(fetch) => Listed::Continue(fetch),
                    _ => Listed::Done,
                }
            }
            Ok((Err(other), _)) => {
                let error = format!("Unexpected reply {}", other.operation());
                self.claims.apply(generation, Err(error));
                Listed::Done
            }
            Err(e)
                if self.claims.gathering()
                    && matches!(&e, Failure::Refused(f) if f.code == DomainErrorCode::CursorExpired) =>
            {
                // A re-read of the shown pages met a changed list: it reads again once (the
                // rows stay), and only then shows the failure.
                match self.claims.apply(generation, Err(e.describe())) {
                    Applied::Continue(fetch) => Listed::Continue(fetch),
                    _ => Listed::Done,
                }
            }
            Err(Failure::Refused(f)) if f.code == DomainErrorCode::CursorExpired => {
                if !self.claims.abandon(generation) {
                    return Listed::Done; // superseded by a newer listing
                }
                self.message = Some((
                    Tone::Attention,
                    "Knowledge changed while paging; the list was reloaded from the start.".into(),
                ));
                Listed::Restart
            }
            Err(e) => {
                self.claims.apply(generation, Err(e.describe()));
                Listed::Done
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
    pub fn preview_input(&self) -> Result<(String, Option<In>), String> {
        let source = self.cleanup_source.as_str();
        let label: devworks_core::protocol::support::Label =
            frame::text(source).ok_or("A source is a nonempty label of at most 256 characters.")?;
        Ok((
            source.to_owned(),
            self.scope().map(|scope| {
                In::Knowledge(knowledge::Input::Diagnostics(knowledge::DiagnosticsInput {
                    source_label: Some(label),
                    ..diagnostics(scope)
                }))
            }),
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
    pub fn delete_source_input(&self) -> Result<(&'static str, Option<In>), String> {
        let p = self
            .source_preview
            .as_ref()
            .filter(|p| p.source == self.cleanup_source.as_str())
            .ok_or("Preview the source first.")?;
        Ok((
            "knowledge.delete_by_source",
            (|| {
                Some(In::Knowledge(knowledge::Input::DeleteBySource(
                    knowledge::DeleteBySourceInput {
                        scope: self.scope()?,
                        source_id: None,
                        expected_scope_revision: frame::number(p.scope_revision)?,
                        preview_digest: frame::text(&p.digest)?,
                        proposal_id: None,
                        source_label: Some(frame::text(&p.source)?),
                    },
                )))
            })(),
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
                let scope = self.scope_project();
                let body = import_preview_input(scope.as_deref(), &record.artifact);
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
    pub fn import_input(&self) -> Result<Option<In>, String> {
        match &self.import {
            Some(ImportStage::Ready {
                source,
                scope,
                digest,
                ..
            }) => Ok((|| {
                Some(In::Knowledge(knowledge::Input::ImportUserData(
                    knowledge::ImportUserDataInput {
                        scope: frame::scope(scope.as_deref())?,
                        source: source.clone(),
                        schema: import_schema()?,
                        preview_digest: frame::text(digest)?,
                    },
                )))
            })()),
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
                .map(|d| (d.code.0.clone(), d.message.0.clone()));
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
                    Some(Submitted::Relation { object, .. }) if self.relate_object == object => {
                        self.relate_object = None;
                    }
                    _ => {}
                }
                self.last_write = write_id_of(&result).map(|write_id| LastWrite {
                    label: label.clone(),
                    write_id,
                    status: None,
                    running: false,
                });
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
                let words = match self.submitted.take() {
                    Some(Submitted::Relation { predicate, .. }) => relate_refusal(&e, predicate),
                    _ => e.describe(),
                };
                self.message = Some((Tone::Bad, format!("{label} was not recorded. {words}")));
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
        document_actions: &mut Vec<crate::documents_screen::Action>,
    ) {
        let screen = crate::route::Route::Knowledge.screen();
        let mut st = (&mut *self, &mut *actions, &mut *document_actions, online);
        widgets::with_right_sidebar(
            ui,
            360.0,
            &mut st,
            |ui, st| {
                let (s, a, documents, online) = st;
                let tab = s.tab_memory();
                let (_, tab) = widgets::pane(
                    ui,
                    widgets::Header {
                        title: screen.title,
                        summary: screen.summary,
                        tabs: screen.subviews,
                        tab,
                    },
                    &mut (&mut **s, &mut **a, &mut **documents),
                    |ui, st| {
                        if tab == 0 && widgets::secondary_button(ui, "New entry").clicked() {
                            st.0.draft = Some(Draft::default());
                            st.0.draft_base = None;
                        }
                    },
                    |ui, st, tab| {
                        if tab == 2 {
                            return;
                        }
                        if tab == crate::documents_screen::TAB {
                            st.0.documents.main(ui, &st.0.projects, *online, st.2);
                            return;
                        }
                        st.0.entries(ui, *online, st.1);
                    },
                );
                s.set_tab_memory(tab);
            },
            |ui, st| {
                let (s, a, documents, online) = st;
                if s.tab == crate::documents_screen::TAB {
                    widgets::secondary_pane(ui, "Document", |ui| {
                        if online.is_ok() {
                            s.documents.detail(ui, &s.projects, documents);
                        }
                    });
                } else {
                    widgets::secondary_pane(ui, "Entry", |ui| s.detail_pane(ui, *online, a));
                }
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
            if self.lessons_only {
                self.lesson_filters(ui, actions);
            }
            if let Some((code, message)) = &self.retrieval {
                chip(ui, retrieval_label(code, message), Tone::Neutral);
            }
            if let Some((mode, limited)) = &self.search_ranking {
                let (shown, notice) = ranking_label(mode, *limited);
                // The diagnostic above names the mode searches run in; the shown search's own
                // mode is named only where it differs (its query could not be embedded).
                let diagnosed = self.retrieval.as_ref().map(|(code, _)| code.as_str());
                if diagnosed != Some(mode_code(mode)) {
                    chip(ui, shown, Tone::Neutral);
                }
                if let Some(notice) = notice {
                    chip(ui, notice, Tone::Attention);
                }
            }
        });
        ui.horizontal_wrapped(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.graph.query)
                    .hint_text("Ask the Knowledge graph")
                    .desired_width(260.0),
            );
            r.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Query the graph")
            });
            let asked = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let idle = !self.graph.loading();
            let text = self.graph.query.trim().to_owned();
            if ui
                .add_enabled(idle && !text.is_empty(), egui::Button::new("Query graph"))
                .clicked()
                || (asked && idle && !text.is_empty())
            {
                actions.push(KnowledgeAction::Graph(graph::GraphAction::Open(
                    graph::GraphRead::Query(text),
                )));
            }
            if ui
                .add_enabled(idle, egui::Button::new("Whole graph"))
                .clicked()
            {
                actions.push(KnowledgeAction::Graph(graph::GraphAction::Open(
                    graph::GraphRead::Whole { cursor: None },
                )));
            }
        });
        if let Some((tone, text)) = &self.message {
            chip(ui, text.clone(), *tone);
        }
        if let Some(w) = &self.last_write {
            let status = w.status.as_deref().unwrap_or("reading its receipt…");
            let tone = if w.running {
                Tone::Attention
            } else {
                Tone::Neutral
            };
            chip(ui, format!("Last change: {} · {status}", w.label), tone);
        }
        self.import_section(ui, actions);
        self.cleanup(ui, actions);
        if let Some(d) = self.draft.clone() {
            self.editor(ui, d, actions);
        }
        ui.add_space(6.0);
        let mut graph_actions = Vec::new();
        self.graph
            .panel(ui, online.is_ok(), &self.claims.items, &mut graph_actions);
        actions.extend(graph_actions.into_iter().map(KnowledgeAction::Graph));
        let (mut reload, mut more) = (false, false);
        let ready = paged::gate(
            ui,
            &Ok(()),
            &self.claims,
            "Knowledge",
            &mut reload,
            &mut more,
        );
        if reload {
            actions.push(KnowledgeAction::Search);
        }
        if more {
            actions.push(KnowledgeAction::More);
        }
        if !ready {
            return;
        }
        if self.claims.items.is_empty() {
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
                for c in &self.claims.items {
                    let selected = self.selected.as_ref() == Some(&c.id);
                    let kind = c
                        .entry_kind
                        .as_ref()
                        .map_or_else(|| wire_name(&c.kind), wire_name);
                    let mut line = format!(
                        "{}  ·  {}",
                        kind,
                        c.value
                            .as_ref()
                            .map_or(c.predicate.0.as_str(), |v| v.0.as_str())
                    );
                    if let Some(l) = c.lifecycle.as_ref().filter(|l| wire_name(*l) != "Active") {
                        line.push_str(&format!("  ·  {}", wire_name(l)));
                    }
                    if let Some(lesson) = &c.lesson {
                        line.push_str(&format!("  ·  rule {}", wire_name(&lesson.rule_state)));
                    }
                    if c.stale == Some(true) {
                        line.push_str("  ·  stale");
                    }
                    if ui.selectable_label(selected, line).clicked() {
                        actions.push(KnowledgeAction::Select(c.id.clone()));
                    }
                }
            });
        let mut more = false;
        paged::load_more_control(ui, &self.claims, &mut more);
        if more {
            actions.push(KnowledgeAction::More);
        }
    }

    /// With Lessons on: the lesson view (a search, or one of `knowledge.tagged_lessons`'
    /// views) and, for a view, its activity and rule state.
    fn lesson_filters(&mut self, ui: &mut Ui, actions: &mut Vec<KnowledgeAction>) {
        const SEARCHED: &str = "Matching the search";
        let shown = self
            .lesson_view
            .and_then(|v| LESSON_VIEWS.iter().find(|(_, w)| *w == v))
            .map_or(SEARCHED, |(l, _)| *l);
        egui::ComboBox::from_id_salt("knowledge-lesson-view")
            .selected_text(shown)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(self.lesson_view.is_none(), SEARCHED)
                    .clicked()
                {
                    self.lesson_view = None;
                    actions.push(KnowledgeAction::Search);
                }
                for (label, wire) in LESSON_VIEWS {
                    if ui
                        .selectable_label(self.lesson_view == Some(wire), label)
                        .clicked()
                    {
                        self.lesson_view = Some(wire);
                        actions.push(KnowledgeAction::Search);
                    }
                }
            })
            .response
            .widget_info(|| {
                let mut info = egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Lesson view");
                info.current_text_value = Some(shown.to_owned());
                info
            });
        if self.lesson_view.is_none() {
            return;
        }
        let r = ui.add(
            egui::TextEdit::singleline(&mut self.lesson_activity)
                .hint_text("Activity, such as reviewing")
                .desired_width(160.0),
        );
        r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Lesson activity"));
        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            actions.push(KnowledgeAction::Search);
        }
        egui::ComboBox::from_id_salt("knowledge-rule-state")
            .selected_text(self.rule_state.unwrap_or("Any rule state"))
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(self.rule_state.is_none(), "Any rule state")
                    .clicked()
                {
                    self.rule_state = None;
                    actions.push(KnowledgeAction::Search);
                }
                for state in RULE_STATES {
                    if ui
                        .selectable_label(self.rule_state == Some(state), state)
                        .clicked()
                    {
                        self.rule_state = Some(state);
                        actions.push(KnowledgeAction::Search);
                    }
                }
            })
            .response
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Rule state"));
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

    /// The selected entry's relations, its way into the graph, and the form that relates it to
    /// another entry.
    fn relations(
        &mut self,
        ui: &mut Ui,
        bundle: &KnowledgeBundle,
        c: &KnowledgeClaim,
        online: bool,
        actions: &mut Vec<KnowledgeAction>,
    ) {
        let name = |id: &Id| -> String {
            let graph = self
                .graph
                .view
                .iter()
                .flat_map(|v| v.bundle.claims.0.iter());
            bundle
                .claims
                .0
                .iter()
                .chain(self.claims.items.iter())
                .chain(graph)
                .find(|x| x.id == *id)
                .map(graph::text_of)
                .or_else(|| {
                    bundle
                        .entities
                        .0
                        .iter()
                        .find(|e| e.id == *id)
                        .map(|e| e.label.0.clone())
                })
                .unwrap_or_else(|| id.as_str().to_owned())
        };
        widgets::divider(ui);
        ui.label(RichText::new("Relations").strong());
        let relations = bundle
            .relations
            .as_ref()
            .map_or(&[][..], |r| r.0.as_slice());
        if relations.is_empty() {
            widgets::caption(ui, "No relations recorded.");
        }
        for r in relations {
            let predicate = r.predicate.0.replace('_', " ");
            widgets::caption(
                ui,
                if r.subject_id == c.id {
                    format!("This entry {predicate} {}", name(&r.object_id))
                } else {
                    format!("{} {predicate} this entry", name(&r.subject_id))
                },
            );
        }
        let idle = online && !self.graph.loading();
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(idle, egui::Button::new("Open claim bundle"))
                .clicked()
            {
                actions.push(KnowledgeAction::Graph(graph::GraphAction::Open(
                    graph::GraphRead::Claim(c.id.clone()),
                )));
            }
            if ui
                .add_enabled(idle, egui::Button::new("Open subject entity"))
                .clicked()
            {
                actions.push(KnowledgeAction::Graph(graph::GraphAction::Open(
                    graph::GraphRead::Entity(c.subject_id.clone()),
                )));
            }
        });
        let candidates = self.relate_candidates();
        if self
            .relate_object
            .as_ref()
            .is_some_and(|o| !candidates.iter().any(|(id, _)| id == o))
        {
            self.relate_object = None;
        }
        let relation = RELATIONS
            .get(self.relate_predicate)
            .map_or("Related to", |(l, _)| *l);
        egui::ComboBox::from_id_salt("knowledge-relation")
            .selected_text(relation)
            .show_ui(ui, |ui| {
                for (i, (label, _)) in RELATIONS.iter().enumerate() {
                    ui.selectable_value(&mut self.relate_predicate, i, *label);
                }
            })
            .response
            .widget_info(|| {
                let mut info = egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Relation");
                info.current_text_value = Some(relation.to_owned());
                info
            });
        let picked = self
            .relate_object
            .as_ref()
            .and_then(|o| candidates.iter().find(|(id, _)| id == o))
            .map_or_else(|| "Choose an entry".to_owned(), |(_, t)| t.clone());
        egui::ComboBox::from_id_salt("knowledge-related-entry")
            .selected_text(picked.clone())
            .width(260.0)
            .show_ui(ui, |ui| {
                for (id, text) in &candidates {
                    if ui
                        .selectable_label(self.relate_object.as_ref() == Some(id), text)
                        .clicked()
                    {
                        self.relate_object = Some(id.clone());
                    }
                }
            })
            .response
            .widget_info(|| {
                let mut info =
                    egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Related entry");
                info.current_text_value = Some(picked.clone());
                info
            });
        if candidates.is_empty() {
            widgets::caption(ui, "List or open another entry to relate this one to it.");
        }
        // One write at a time: the button stays disabled while any Knowledge write is in
        // flight or unconfirmed, and the shell refuses a second submission besides.
        let ready = online && self.writing.is_none() && self.relate_object.is_some();
        if ui
            .add_enabled(ready, egui::Button::new("Relate entries"))
            .clicked()
        {
            actions.push(KnowledgeAction::Relate);
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
                chip(ui, c.entry_kind.as_ref().map_or_else(|| wire_name(&c.kind), wire_name), Tone::Accent);
                chip(ui, c.lifecycle.as_ref().map_or_else(|| wire_name(&c.state), wire_name), Tone::Neutral);
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
                c.origin.as_ref().map_or("—".into(), wire_name),
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
                    wire_name(&l.rule_state),
                    l.effect.as_ref().map_or("not in effect".into(), wire_name),
                    wire_name(&l.source),
                    l.recurrence.get(),
                    l.recurrence_threshold.get(),
                    l.delivered_sessions.get()
                ));
                if !l.applicability.activities.0.is_empty() {
                    widgets::caption(ui, format!("Applies when {}", l.applicability.activities.0.iter().map(|a| a.0.as_str()).collect::<Vec<_>>().join(", ")));
                }
                for a in &l.addressed_by.0 {
                    widgets::caption(ui, format!("Addressed by {} {}", wire_name(&a.kind), a.name.0));
                }
            }
            let challenges = c.challenges.as_ref().map(|v| v.0.as_slice())
                .or_else(|| c.lesson.as_ref().map(|l| l.challenges.0.as_slice())).unwrap_or_default();
            for ch in challenges {
                widgets::caption(ui, format!("Challenge ({}): {}", wire_name(&ch.state), ch.reason.0));
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
                widgets::caption(ui, format!("{} {}: {}", wire_name(&e.source.kind), e.source.source_ref.0, e.snippet.0));
            }
            if let Some(revisions) = &bundle.revisions {
                widgets::divider(ui);
                ui.label(RichText::new("Revisions").strong());
                for r in &revisions.0 {
                    widgets::caption(ui, format!("{} · {} by {}", r.revision.get(), r.change.0, r.author_id.0.as_str()));
                }
            }
            self.relations(ui, &bundle, &c, online.is_ok(), actions);
            widgets::divider(ui);
            let r = ui.add(egui::TextEdit::singleline(&mut self.tag_edit).hint_text("Tags"));
            r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Edit tags"));
            let r = ui.add(egui::TextEdit::singleline(&mut self.reason).hint_text("Reason (verify, retract, challenge, retire, reject)"));
            r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Change reason"));
            let enabled = online.is_ok() && self.writing.is_none();
            let lifecycle = c.lifecycle.as_ref().map_or_else(|| "Active".to_owned(), wire_name);
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
    use serde_json::{Value, json};

    /// The `/body` members of a typed request.
    fn body_of(input: Option<In>) -> Value {
        input.expect("a typed input").body().expect("a body")
    }

    #[test]
    fn a_re_read_and_its_pages_use_the_query_listed_not_the_form() {
        let mut s = KnowledgeScreen::new();
        s.text = "listed".into();
        s.begin_listing(false);
        let first = s.claims.generation();
        s.claims
            .apply(first, Ok(Page::new(Vec::new(), Some("c1".into()))));
        // Typed since, not submitted.
        s.text = "half typed".into();
        let (_, input) = s.begin_refresh();
        assert_eq!(body_of(input)["text"], "listed");
        let fetch = Fetch {
            generation: s.claims.generation(),
            cursor: Some("c9".into()),
        };
        let (_, input) = s.continue_listing(&fetch);
        let body = body_of(input);
        assert_eq!(body["text"], "listed");
        assert_eq!(body["page"]["cursor"], "c9");
        // Load more reads the listed query's next page too.
        s.claims.reload();
        let again = s.claims.generation();
        s.claims
            .apply(again, Ok(Page::new(Vec::new(), Some("c2".into()))));
        let (_, input) = s.begin_listing(true);
        let body = body_of(input);
        assert_eq!(body["text"], "listed");
        assert_eq!(body["page"]["cursor"], "c2");
    }

    #[test]
    fn writes_use_the_scope_of_the_listing_shown_not_an_unsearched_one() {
        let mut s = KnowledgeScreen::new();
        let (_, input) = s.begin_listing(false);
        let body = body_of(input);
        assert_eq!(body["scope"], json!({"kind": "Workspace"}));
        s.set_projects(vec![("project-x".into(), "Project X".into())]);
        s.project = Some("project-x".into());
        let draft = Draft {
            value: "A fact".into(),
            ..Draft::default()
        };
        let (_, input) = s.save_input(&draft).unwrap();
        let body = body_of(input);
        assert_eq!(
            body["scope"],
            json!({"kind": "Workspace"}),
            "typed, not searched"
        );
        let (_, input) = s.begin_listing(false);
        let body = body_of(input);
        assert_eq!(
            body["scope"],
            json!({"kind": "Project", "projectId": "project-x"})
        );
        let (_, input) = s.save_input(&draft).unwrap();
        let body = body_of(input);
        assert_eq!(
            body["scope"],
            json!({"kind": "Project", "projectId": "project-x"})
        );
        // A continuation stays in the listed scope even if the field changes again.
        let first = s.claims.reload();
        s.claims.apply(
            first.generation,
            Ok(Page::new(Vec::new(), Some("c".into()))),
        );
        s.project = None;
        let (_, input) = s.begin_listing(true);
        let body = body_of(input);
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
        s.claims.items = vec![claim];
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
        let (op, input) = s.save_input(&d).unwrap();
        assert_eq!(op, "knowledge.revise");
        let body = body_of(input);
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
            let (_, input) = s.change_input(change).unwrap();
            let body = body_of(input);
            assert_eq!(body["scope"], json!({"kind": "Workspace"}), "{change:?}");
        }
        // Making a general lesson effective (or rejecting it) keeps it general.
        for change in [Change::Promote, Change::Reject] {
            let (_, input) = s.change_input(change).unwrap();
            let body = body_of(input);
            assert_eq!(
                body["destination"],
                json!({"kind": "Workspace"}),
                "{change:?}"
            );
        }
        // Only the explicit scope change names another scope.
        let (op, input) = s.change_input(Change::MakeGeneral).unwrap();
        let body = body_of(input);
        assert_eq!(op, "knowledge.scope");
        assert_eq!(body["destination"], json!({"kind": "Workspace"}));
    }

    fn no_use_case(op: &str) -> QueryOutcome {
        Err(devworks_core::gateway::GatewayFailure::NoUseCase(op.into()))
    }

    #[test]
    fn a_graph_view_stays_until_a_reply_replaces_it_and_older_replies_are_ignored() {
        let mut g = graph::Graph::default();
        let scope = Some(DataScope::Workspace);
        let (first, op, input) = g.begin(scope.clone(), graph::GraphRead::Whole { cursor: None });
        assert_eq!(op, "knowledge.subgraph");
        assert_eq!(body_of(input)["query"]["claimIds"], json!([]));
        let (second, op, input) = g.begin(
            scope.clone(),
            graph::GraphRead::Claim(Id::new("claim-1").unwrap()),
        );
        assert_eq!(op, "knowledge.claim_bundle");
        assert_eq!(body_of(input)["query"]["claimIds"], json!(["claim-1"]));
        g.reply(first, no_use_case("knowledge.subgraph"));
        assert!(g.loading(), "the newer read is still awaited");
        assert!(g.error.is_none(), "an older reply never changes the view");
        g.reply(second, no_use_case("knowledge.claim_bundle"));
        assert!(!g.loading());
        assert!(g.error.is_some());
        // A shown view stays while it is read again, and after a failed re-read.
        g.view = Some(graph::GraphView {
            read: graph::GraphRead::Claim(Id::new("claim-1").unwrap()),
            bundle: serde_json::from_value(json!({"entities": [], "claims": [],
                "evidence": [], "frontier": [], "truncated": false, "conflicts": [],
                "supersessionIds": []}))
            .unwrap(),
            steps: 0,
            summary: None,
        });
        let (again, op, _) = g.begin_refresh(scope.clone()).expect("a view to re-read");
        assert_eq!(op, "knowledge.claim_bundle");
        assert!(g.view.is_some(), "kept until the reply");
        g.reply(again, no_use_case("knowledge.claim_bundle"));
        assert!(g.view.is_some(), "a failed re-read keeps what is shown");
        assert_eq!(
            g.begin_expand(scope).unwrap_err(),
            "Nothing is shown to expand from."
        );
    }

    #[test]
    fn a_relation_names_the_selected_entry_the_relation_and_the_entry_picked() {
        let mut s = showing(general_lesson());
        assert_eq!(
            s.relate_input().unwrap_err(),
            "Choose the entry to relate it to."
        );
        s.relate_object = Some(Id::new("claim-g").unwrap());
        assert_eq!(
            s.relate_input().unwrap_err(),
            "An entry cannot be related to itself."
        );
        s.relate_object = Some(Id::new("claim-h").unwrap());
        s.relate_predicate = 3;
        let body = body_of(s.relate_input().unwrap());
        assert_eq!(body["subjectId"], json!("claim-g"));
        assert_eq!(body["objectId"], json!("claim-h"));
        assert_eq!(body["predicate"], json!("contradicts"));
        assert_eq!(
            body["scope"],
            json!({"kind": "Project", "projectId": "project-x"}),
            "the listed scope"
        );
    }

    fn refused(code: &str, target_kind: &str) -> Failure {
        Failure::Refused(
            serde_json::from_value(json!({"code": code, "retry": "Never",
                "correlationId": "c", "targetKind": target_kind, "blockerIds": []}))
            .unwrap(),
        )
    }

    #[test]
    fn a_scope_refusal_reads_as_a_lesson_merge_only_for_a_merge() {
        let scope = refused("PermissionDenied", "knowledge_scope");
        for merge in ["same_as", "unmerge"] {
            assert_eq!(
                relate_refusal(&scope, merge),
                "Both lessons must belong to the scope shown to merge them."
            );
        }
        for other in ["related_to", "supports", "contradicts"] {
            assert_eq!(
                relate_refusal(&scope, other),
                "You cannot change Knowledge in this scope."
            );
        }
    }

    fn receipt(state: &str) -> QueryOutcome {
        Ok(
            serde_json::from_value(json!({"kind": "Result", "protocol": "1.0-draft.1",
            "queryId": "q", "stamp": {"storeId": "store", "restoreEpoch": "1", "sequence": "1",
                "policyRevision": "1", "scopeRevision": "1"},
            "result": {"kind": "knowledge.write_status", "value": {"writeId": "write-1",
                "state": state, "entityIds": [], "claimIds": ["c1"], "revision": "3",
                "diagnostics": []}}}))
            .unwrap(),
        )
    }

    #[test]
    fn a_write_still_running_is_marked_until_its_receipt_says_it_is_done() {
        let mut s = KnowledgeScreen::new();
        s.last_write = Some(LastWrite {
            label: "Delete by source".into(),
            write_id: Id::new("write-1").unwrap(),
            status: None,
            running: false,
        });
        let (id, input) = s.write_status_input().unwrap();
        assert_eq!(body_of(input)["writeId"], json!("write-1"));
        s.write_status_reply(&id, receipt("Dispatching"));
        let w = s.last_write.as_ref().unwrap();
        assert!(w.running, "a refresh reads it again");
        assert!(w.status.as_deref().unwrap().starts_with("still running"));
        s.write_status_reply("write-other", receipt("ObservedComplete"));
        assert!(
            s.last_write.as_ref().unwrap().running,
            "another write's receipt"
        );
        s.write_status_reply(&id, receipt("ObservedComplete"));
        let w = s.last_write.as_ref().unwrap();
        assert!(!w.running, "no more re-reads");
        assert!(w.status.as_deref().unwrap().starts_with("complete"));
    }

    #[test]
    fn a_lesson_view_lists_through_tagged_lessons_and_pages_the_same_query() {
        let mut s = KnowledgeScreen::new();
        s.lessons_only = true;
        s.lesson_view = Some("Recurring");
        s.rule_state = Some("Effective");
        let (op, input) = s.begin_listing(false);
        assert_eq!(op, "knowledge.tagged_lessons");
        let body = body_of(input);
        assert_eq!(body["tags"], json!(["lesson"]), "every lesson carries it");
        assert_eq!(body["view"], json!("Recurring"));
        assert_eq!(body["ruleStates"], json!(["Effective"]));
        let generation = s.claims.generation();
        s.claims
            .apply(generation, Ok(Page::new(Vec::new(), Some("c1".into()))));
        let (op, input) = s.begin_listing(true);
        assert_eq!(op, "knowledge.tagged_lessons");
        assert_eq!(body_of(input)["page"]["cursor"], json!("c1"));
    }

    #[test]
    fn a_search_page_shows_its_mode_and_says_when_its_results_are_limited() {
        use devworks_core::protocol::records::KnowledgeSearchPage;
        use devworks_core::protocol::results::DomainQueryReply;
        use devworks_core::protocol::{Counter, Protocol, QueryId, ReadStamp, Revision, StoreId};
        let reply = |mode, work_limited| {
            Ok(DomainQueryReply::Result {
                protocol: Protocol::Draft1,
                query_id: QueryId(Id::new("q").unwrap()),
                stamp: ReadStamp {
                    store_id: StoreId(Id::new("store-1").unwrap()),
                    restore_epoch: Counter::new(0).unwrap(),
                    sequence: Counter::new(1).unwrap(),
                    policy_revision: Revision::new(1).unwrap(),
                    scope_revision: Revision::new(1).unwrap(),
                },
                result: OperationResult::KnowledgeSearch {
                    value: KnowledgeSearchResult::Current(KnowledgeSearchPage {
                        items: BoundedList(Vec::new()),
                        next_cursor: None,
                        has_more: false,
                        mode,
                        work_limited,
                        approximate_tokens: None,
                        budget_tokens: None,
                    }),
                },
            })
        };
        let mut s = KnowledgeScreen::new();
        s.begin_listing(false);
        let generation = s.claims.generation();
        s.listed(generation, reply(KnowledgeRetrievalMode::Hybrid, true));
        let Some((mode, limited)) = &s.search_ranking else {
            panic!("the ranking is kept")
        };
        let (shown, notice) = ranking_label(mode, *limited);
        assert!(shown.contains("meaning"), "{shown}");
        assert!(notice.unwrap().starts_with("Results limited"));
        s.begin_listing(false);
        let generation = s.claims.generation();
        s.listed(
            generation,
            reply(KnowledgeRetrievalMode::LexicalOnly, false),
        );
        let (mode, limited) = s.search_ranking.as_ref().unwrap();
        let (shown, notice) = ranking_label(mode, *limited);
        assert!(!shown.contains("meaning"), "{shown}");
        assert!(notice.is_none());
        assert_eq!(mode_code(mode), "LexicalOnly");
    }

    #[test]
    fn a_reply_of_a_superseded_listing_is_ignored() {
        let mut s = KnowledgeScreen::new();
        s.begin_listing(false);
        let old = s.claims.generation();
        s.begin_listing(false);
        let restart = s.listed(
            old,
            Err(devworks_core::gateway::GatewayFailure::NoUseCase(
                "knowledge.search".into(),
            )),
        );
        assert_eq!(restart, Listed::Done);
        assert!(s.claims.loading(), "the newer listing is still awaited");
        assert!(
            s.claims.error.is_none(),
            "a stale reply never overwrites state"
        );
    }
}

/// The retrieval diagnostic's code for a search's mode.
fn mode_code(mode: &KnowledgeRetrievalMode) -> &'static str {
    match mode {
        KnowledgeRetrievalMode::Hybrid => "Hybrid",
        KnowledgeRetrievalMode::LexicalOnly => "LexicalOnly",
    }
}

/// What the shown search's ranking says: its mode, and a notice when fusion ranked only its
/// first claims.
fn ranking_label(mode: &KnowledgeRetrievalMode, limited: bool) -> (String, Option<String>) {
    let shown = match mode {
        KnowledgeRetrievalMode::Hybrid => "This search: words, meaning and related entities",
        KnowledgeRetrievalMode::LexicalOnly => "This search: words and related entities",
    };
    let notice = limited.then(|| {
        format!(
            "Results limited: the best {} matches of each kind are ranked together; the rest \
             follow in each kind's own order",
            devworks_core::services::knowledge::CANDIDATES
        )
    });
    (shown.into(), notice)
}

/// The retrieval mode as the user reads it: what kind of search runs, then the diagnostic's
/// own words (the embedding space for hybrid search, the reason for word search only).
fn retrieval_label(code: &str, message: &str) -> String {
    match code {
        "Hybrid" => format!("Word and meaning search. {message}"),
        "LexicalOnly" => format!("Word search only. {message}"),
        other => format!("{other}. {message}"),
    }
}

#[cfg(test)]
mod retrieval_label_tests {
    #[test]
    fn each_retrieval_mode_names_its_space_or_reason() {
        let hybrid = super::retrieval_label(
            "Hybrid",
            "Retrieval is hybrid in embedding space space-1 (m, 2 dimensions).",
        );
        assert!(hybrid.starts_with("Word and meaning search.") && hybrid.contains("space-1"));
        let lexical = super::retrieval_label(
            "LexicalOnly",
            "Retrieval is lexical only (NoClaimVectors): none has a vector.",
        );
        assert!(lexical.starts_with("Word search only.") && lexical.contains("NoClaimVectors"));
    }
}
