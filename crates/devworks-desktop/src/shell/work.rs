//! Features, the board, Issues and project Overview (DW-12, UI-14/15): the Features list with
//! search, status filter and accessible prioritization; feature detail with its plan (active and
//! pending, approval) and readiness shown apart; a Kanban board per feature whose cards move by
//! drag and drop or by their accessible Move and priority controls; the Issues list and the
//! shared task/issue detail (priority, workflow status, Blocked by/Blocks, feature membership,
//! acceptance with manual versus verified completion, assignment, run controls and history);
//! and Overview's summary and Activity (filters, full history).
//!
//! Every change is the same `features.*`, `issues.*`, `tasks.*` or `runs.*` frame an agent tool
//! submits, prepared from the record a form was opened on (its revision), so a concurrent change
//! is a refusal that keeps the draft, never a silent overwrite. Lists read every page (a cursor
//! that expired because the order changed restarts the read) and replace the shown list only
//! once complete. A moved card stays in its column, marked, until its move is confirmed; an
//! outcome that is not known keeps its exact frame for Confirm outcomes and the next launch.
use super::{ListState, Shell};
use crate::backend::{Reply, Slot, TaskOutput, is_unknown, resolve_command};
use crate::ops::{self, Failure};
use crate::route::Route;
use crate::theme;
use crate::widgets::{self, Header, Tone, ViewState, chip};
use crate::work_ops as wops;
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::{
    AcceptanceCriterion, ApprovalState, CriterionState, FindingState, LifeState,
    ReadinessDisposition, ReadinessFreshness, RequirementItem, RequirementStatus, RootSnapshot,
    WorkKind, WorkRecord,
};
use devworks_core::protocol::{DomainErrorCode, Event, EventSubject, Id, ProjectId};
use devworks_core::services::work::deps::MAX_PREREQUISITES;
use devworks_core::services::work::fold;
use devworks_core::services::work::workflow::{DEFAULT_STATUSES, default_status_id};
use eframe::egui::{self, RichText, Ui};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Feature list tabs and Issues tabs.
pub const FEATURE_TABS: [&str; 2] = ["List", "Board"];
pub const ISSUE_TABS: [&str; 2] = ["List", "History"];
pub const OVERVIEW_TABS: [&str; 2] = ["Summary", "Activity"];

/// One board column: a status of the project workflow.
#[derive(Clone, Debug)]
pub struct Column {
    pub id: String,
    pub name: String,
    pub semantic: String,
}
/// The project's workflow columns: the default workflow's stable statuses, then any other
/// status a card is in (a custom status of the workflow item), never dropped from view.
pub fn columns(project: &str, cards: &[WorkRecord]) -> Vec<Column> {
    let p = ProjectId(Id::new(project).unwrap_or_else(|_| Id::new("project").expect("id")));
    let mut out: Vec<Column> = DEFAULT_STATUSES
        .iter()
        .map(|(k, name, semantic)| Column {
            id: default_status_id(&p, k).as_str().to_owned(),
            name: (*name).to_owned(),
            semantic: (*semantic).to_owned(),
        })
        .collect();
    for c in cards {
        let s = c.status_id.as_str();
        if !out.iter().any(|x| x.id == s) {
            out.push(Column {
                id: s.to_owned(),
                name: format!("Status {s}"),
                semantic: String::new(),
            });
        }
    }
    out
}
fn semantic_of(cols: &[Column], status: &str) -> String {
    cols.iter()
        .find(|c| c.id == status)
        .map(|c| c.semantic.clone())
        .unwrap_or_default()
}
fn finished(semantic: &str) -> bool {
    semantic == "Done" || semantic == "Closed"
}
fn status_name(cols: &[Column], status: &str) -> String {
    cols.iter()
        .find(|c| c.id == status)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| status.to_owned())
}

/// A new feature, task or issue being written.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreateForm {
    /// `feature`, `task` or `issue`.
    pub kind: String,
    pub title: String,
    pub description: String,
    pub feature: Option<String>,
    pub priority: i32,
}
/// An edit of one item's title, description and priority, against the record it was opened on.
#[derive(Clone, Debug)]
pub struct EditForm {
    pub snapshot: WorkRecord,
    pub title: String,
    pub description: String,
    pub priority: i32,
}
/// A plan draft being written for a feature, against the record it was opened on.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PlanForm {
    pub snapshot: WorkRecord,
    pub text: String,
    /// One entry per requirement, criterion and test (an ID and its text, which may span
    /// lines): each round-trips exactly, and an entry the user did not change is submitted as
    /// it was read.
    pub requirements: Vec<Entry>,
    pub acceptance: Vec<Entry>,
    /// One goal per entry.
    pub goals: Vec<String>,
    pub repositories: BTreeSet<String>,
    /// The content the draft keeps when its text is unchanged.
    pub content: Option<ArtifactRef>,
    /// The text `content` holds (unchanged text keeps the content).
    pub original: String,
    pub acceptance_tests: Vec<Entry>,
    /// The draft the form started from: what the form does not show (requirement status, drop
    /// decisions and coverage; criterion state and evidence) is kept from it, never dropped.
    pub base: PlanBase,
    /// Explicit drops being authored; their decisions are stored before the draft command.
    #[serde(default)]
    pub drops: BTreeMap<String, DropDraft>,
    #[serde(default)]
    pub statuses: BTreeMap<String, RequirementStatus>,
}
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct DropDraft {
    pub reason: String,
    pub artifact: Option<ArtifactRef>,
}
impl DropDraft {
    fn same_edits(&self, other: &Self) -> bool {
        self.reason == other.reason
            && (self
                .artifact
                .as_ref()
                .map(|a| (&a.artifact_id, &a.version_id))
                == other
                    .artifact
                    .as_ref()
                    .map(|a| (&a.artifact_id, &a.version_id))
                || !self.reason.trim().is_empty())
    }
}
/// One requirement, criterion or test being edited: its ID and its full text.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub id: String,
    pub text: String,
}
/// The next free `<prefix><n>` ID, excluding visible entries and reserved base IDs.
pub fn next_id(entries: &[Entry], prefix: &str, reserved: &[String]) -> String {
    (1..)
        .map(|n| format!("{prefix}{n}"))
        .find(|c| !reserved.contains(c) && !entries.iter().any(|e| &e.id == c))
        .expect("a free identifier")
}

/// The item editor after the save of `saved` was applied (`current` is the saved record):
/// closed when it holds nothing beyond what was saved; otherwise its later edits stay open on
/// the saved revision. An editor of another item is unchanged.
pub fn edit_after_save(
    live: Option<EditForm>,
    saved: &EditForm,
    current: Option<WorkRecord>,
) -> Option<EditForm> {
    match live {
        Some(l) if l.snapshot.id == saved.snapshot.id => {
            let same = l.title == saved.title
                && l.description == saved.description
                && l.priority == saved.priority;
            match current {
                Some(c) if !same => Some(EditForm { snapshot: c, ..l }),
                _ => None,
            }
        }
        other => other,
    }
}

/// Whether two plan forms hold the same edits (what a save submits).
pub fn same_plan_edits(a: &PlanForm, b: &PlanForm) -> bool {
    a.text == b.text
        && a.requirements == b.requirements
        && a.acceptance == b.acceptance
        && a.acceptance_tests == b.acceptance_tests
        && a.goals == b.goals
        && a.repositories == b.repositories
        && a.statuses.len() == b.statuses.len()
        && a.statuses.iter().all(|(id, status)| {
            b.statuses.get(id).is_some_and(|other| {
                std::mem::discriminant(status) == std::mem::discriminant(other)
            })
        })
        && a.drops.len() == b.drops.len()
        && a.drops
            .iter()
            .all(|(id, d)| b.drops.get(id).is_some_and(|b| d.same_edits(b)))
}

/// The plan editor after the save of `saved` was applied as `record`: closed when it holds
/// nothing beyond what was saved; otherwise its later edits stay open as a draft on the saved
/// revision (what the form does not show now comes from the saved draft). An editor of another
/// feature is unchanged.
pub fn plan_after_save(
    live: Option<PlanForm>,
    saved: &PlanForm,
    record: &WorkRecord,
) -> Option<PlanForm> {
    match live {
        Some(mut l) if l.snapshot.id == saved.snapshot.id => {
            if same_plan_edits(&l, saved) {
                return None;
            }
            let fresh = open_plan_form(record);
            for (id, drop) in &mut l.drops {
                if let Some(done) = saved.drops.get(id)
                    && drop.same_edits(done)
                {
                    drop.artifact = done.artifact.clone();
                }
            }
            Some(PlanForm {
                snapshot: fresh.snapshot,
                base: fresh.base,
                content: fresh.content,
                original: saved.text.clone(),
                ..l
            })
        }
        other => other,
    }
}

/// The requirements and criteria a plan form started from.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct PlanBase {
    pub requirements: Vec<RequirementItem>,
    pub acceptance: Vec<AcceptanceCriterion>,
    pub acceptance_tests: Vec<AcceptanceCriterion>,
}

/// A plan form opened on feature `f`: its pending draft when it has one, otherwise its active
/// plan (requirements, acceptance, acceptance tests, goals, repositories, content).
pub fn open_plan_form(f: &WorkRecord) -> PlanForm {
    let (base, goals, repos, content) = match f.pending_plan.as_ref().map(|p| &p.draft) {
        Some(d) => (
            PlanBase {
                requirements: d.requirements.0.clone(),
                acceptance: d.acceptance.0.clone(),
                acceptance_tests: d.acceptance_tests.0.clone(),
            },
            d.goals.0.iter().map(|g| g.0.clone()).collect::<Vec<_>>(),
            d.repository_ids
                .0
                .iter()
                .map(|r| r.as_str().to_owned())
                .collect::<BTreeSet<_>>(),
            Some(d.content.clone()),
        ),
        None => (
            PlanBase {
                requirements: f.requirements.0.clone(),
                acceptance: f.acceptance.0.clone(),
                acceptance_tests: f
                    .acceptance_tests
                    .as_ref()
                    .map(|t| t.0.clone())
                    .unwrap_or_default(),
            },
            f.goals.0.iter().map(|g| g.0.clone()).collect(),
            f.repository_ids
                .0
                .iter()
                .map(|r| r.as_str().to_owned())
                .collect(),
            f.plan.clone(),
        ),
    };
    let criteria = |c: &[AcceptanceCriterion]| {
        c.iter()
            .map(|a| Entry {
                id: a.id.as_str().to_owned(),
                text: a.text.0.clone(),
            })
            .collect::<Vec<_>>()
    };
    PlanForm {
        snapshot: f.clone(),
        text: String::new(),
        requirements: base
            .requirements
            .iter()
            .filter(|r| !matches!(r.status, RequirementStatus::Dropped))
            .map(|r| Entry {
                id: r.id.as_str().to_owned(),
                text: r.statement.0.clone(),
            })
            .collect(),
        acceptance: criteria(&base.acceptance),
        acceptance_tests: criteria(&base.acceptance_tests),
        goals,
        repositories: repos,
        original: String::new(),
        content,
        base,
        drops: BTreeMap::new(),
        statuses: BTreeMap::new(),
    }
}

/// The criteria of `entries`, keeping the stored state and evidence of each one whose ID and
/// text are unchanged (an unchanged entry is submitted exactly as it was read).
fn merged_criteria(entries: &[Entry], base: &[AcceptanceCriterion]) -> Vec<Value> {
    entries
        .iter()
        .filter(|e| !e.text.trim().is_empty())
        .map(|e| match base.iter().find(|b| b.id.as_str() == e.id) {
            Some(b) if b.text.0 == e.text => serde_json::to_value(b).unwrap_or(Value::Null),
            _ => json!({"id": e.id.trim(), "text": e.text, "evidenceIds": [], "state": "Unmet"}),
        })
        .collect()
}

/// The `features.edit_plan` draft of a plan form: what the user wrote, with what the form does
/// not show kept from the draft it started from — each requirement's status, drop decision and
/// covering tasks, dropped requirements (their decision stays recorded), and each unchanged
/// criterion's state and evidence.
pub fn plan_body(form: &PlanForm, content: &ArtifactRef) -> Value {
    let written: Vec<&Entry> = form
        .requirements
        .iter()
        .filter(|e| !e.text.trim().is_empty() || form.drops.contains_key(&e.id))
        .collect();
    let mut reqs: Vec<Value> = written
        .iter()
        .map(|e| {
            match form
                .base
                .requirements
                .iter()
                .find(|b| b.id.as_str() == e.id)
            {
                Some(b) => {
                    let mut v = serde_json::to_value(b).unwrap_or(Value::Null);
                    if !e.text.trim().is_empty() {
                        v["statement"] = json!(e.text);
                    }
                    if let Some(status) = form.statuses.get(&e.id) { v["status"] = json!(status); }
                    if let Some(drop) = form.drops.get(&e.id) {
                        v["status"] = json!("Dropped");
                        v["dropDecision"] = json!(drop.artifact);
                    } else if matches!(b.status, RequirementStatus::Dropped) {
                        // A dropped entry is normally hidden and retained below. Making it
                        // editable is the explicit Restore action, never an implicit rewrite.
                        let status = form
                            .snapshot
                            .requirements
                            .0
                            .iter()
                            .find(|r| {
                                r.id == b.id && !matches!(r.status, RequirementStatus::Dropped)
                            })
                            .map(|r| r.status.clone())
                            .unwrap_or(RequirementStatus::Proposed);
                        let status = form.statuses.get(&e.id).cloned().unwrap_or(status);
                        v["status"] = json!(status);
                        v.as_object_mut()
                            .expect("requirement")
                            .remove("dropDecision");
                    }
                    v
                }
                None => json!({"id": e.id.trim(), "statement": e.text,
                    "status": form.statuses.get(&e.id).cloned().unwrap_or(RequirementStatus::Confirmed),
                    "coveredByTaskIds": []}),
            }
        })
        .collect();
    for b in &form.base.requirements {
        if matches!(b.status, RequirementStatus::Dropped)
            && !written.iter().any(|e| e.id == b.id.as_str())
        {
            reqs.push(serde_json::to_value(b).unwrap_or(Value::Null));
        }
    }
    let goals: Vec<&String> = form.goals.iter().filter(|g| !g.trim().is_empty()).collect();
    let repos: Vec<&String> = form.repositories.iter().collect();
    json!({
        "content": content,
        "acceptance": merged_criteria(&form.acceptance, &form.base.acceptance),
        "repositoryIds": repos,
        "goals": goals,
        "requirements": reqs,
        "acceptanceTests": merged_criteria(&form.acceptance_tests, &form.base.acceptance_tests),
    })
}
/// The project's Activity: its change feed, read page by page to its end and shown newest
/// first. The feed pages forward only, so a read follows its pages to the end; a refresh reads
/// on from where the last read stopped (the feed is append-only), so it adds the newest.
#[derive(Default)]
pub struct Activity {
    /// Every event read, oldest first.
    pub items: Vec<Event>,
    /// Where the next read starts (`None`: the beginning).
    pub resume: Option<String>,
    /// Whether every page up to the end was read at least once.
    pub loaded: bool,
    pub error: Option<String>,
    /// 0 everything, 1 features, 2 issues, 3 tasks, 4 the project itself.
    pub kind: usize,
    /// 0 everyone, 1 you, 2 agents and others.
    pub actor: usize,
    pub text: String,
    /// Only this work item (from a detail's history link).
    pub item: Option<String>,
}

#[derive(Default)]
pub struct WorkUi {
    /// The project the lists were read for.
    pub of: Option<String>,
    pub features: ListState<WorkRecord>,
    pub cards: ListState<WorkRecord>,
    staged_features: Vec<WorkRecord>,
    staged_cards: Vec<WorkRecord>,
    pub activity: Activity,
    /// Current records of opened features (readiness at schema revision 2).
    pub detail: BTreeMap<String, WorkRecord>,
    pub runs: BTreeMap<String, RootSnapshot>,
    /// The feature the board shows.
    pub board: Option<String>,
    /// The selected card of the board.
    pub card: Option<String>,
    pub create: Option<CreateForm>,
    /// Submitted creation drafts by project, retained across project switches.
    pub create_saving: BTreeMap<String, CreateForm>,
    pub edit: Option<EditForm>,
    pub plan: Option<PlanForm>,
    /// The exact plan form a save submitted, while that save runs: its upload and its command
    /// use this snapshot, never the editor, which stays open for later edits.
    /// The exact plan form each running save submitted, by feature: one feature's save never
    /// replaces another's (a close keeps them, and the next launch finishes each one).
    pub plan_saving: BTreeMap<String, PlanForm>,
    /// The exact edit form a save submitted, while it runs (the editor stays open).
    pub edit_saving: BTreeMap<String, EditForm>,
    /// A card whose removal from its feature waits for confirmation.
    pub unlink: Option<WorkRecord>,
    /// Cards whose move was submitted and is not confirmed yet, with the target status.
    pub moving: BTreeMap<String, String>,
    pub message: Option<(Tone, String)>,
    /// Applied work mutations; a list read started before the latest is read again.
    pub mutations: u64,
    pub list_at: u64,
    /// Roots and running roots of a project an archival would stop, once counted.
    pub archive_counts: BTreeMap<String, (usize, usize)>,
    counting: BTreeMap<(String, bool), (usize, usize)>,
    counted: BTreeSet<(String, bool)>,
    /// Status filter of the lists (a status ID).
    pub status_filter: Option<String>,
}

impl WorkUi {
    /// The newest known record of `id`: the one read on its own or the one in a list, whichever
    /// has the higher revision (a cached read never hides a newer change).
    pub fn freshest(&self, id: &str) -> Option<WorkRecord> {
        let listed = self
            .features
            .items
            .iter()
            .chain(&self.cards.items)
            .find(|w| w.id.as_str() == id);
        match (self.detail.get(id), listed) {
            (Some(d), Some(l)) if l.revision.get() > d.revision.get() => Some(l.clone()),
            (Some(d), _) => Some(d.clone()),
            (None, l) => l.cloned(),
        }
    }
    /// Forgets an archive count (the next confirmation counts afresh).
    pub fn forget_archive_count(&mut self, project: &str) {
        self.archive_counts.remove(project);
        for f in [true, false] {
            self.counted.remove(&(project.to_owned(), f));
            self.counting.remove(&(project.to_owned(), f));
        }
    }
}

/// What the screens ask the shell to do.
#[derive(Clone, Debug)]
enum Action {
    Reload,
    Create(CreateForm),
    Edit(EditForm),
    /// A work command on one item (`operation`, body).
    Command(String, &'static str, Value),
    Move(WorkRecord, String),
    /// Opens Overview's Activity with one work item's changes.
    ShowActivity(String),
    /// A queue order: operation, feature, priority, placement, expected revision.
    Order(&'static str, Option<String>, Option<i32>, Value, String),
    SavePlan(PlanForm),
    RequestApproval(WorkRecord),
    Inspect(String),
    Run(&'static str, WorkRecord),
    SelectCard(Option<String>),
    OpenDetail(String),
}

fn kind_label(w: &WorkRecord) -> &'static str {
    match w.kind {
        WorkKind::Feature => "Feature",
        WorkKind::Task => "Task",
        WorkKind::Issue => "Issue",
    }
}
fn short(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{t}…")
    } else {
        t
    }
}
fn matches_text(w: &WorkRecord, needle: &str) -> bool {
    let n = fold(needle.trim());
    n.is_empty() || fold(&format!("{} {}", w.title.0, w.description.0)).contains(&n)
}
fn approval_label(w: &WorkRecord) -> String {
    let pending = w
        .pending_plan
        .as_ref()
        .and_then(|p| p.approval.as_ref())
        .map(|a| match a.state {
            ApprovalState::Pending => "revision awaiting approval",
            ApprovalState::Rejected => "revision rejected",
            _ => "revision",
        });
    match (&w.approval, &w.pending_plan, pending) {
        (Some(_), None, _) => "Plan approved".into(),
        (Some(_), Some(_), Some(p)) => format!("Plan approved · {p}"),
        (Some(_), Some(_), None) => "Plan approved · revision drafted".into(),
        (None, Some(_), Some(p)) => format!("Plan not approved · {p}"),
        (None, Some(_), None) => "Plan drafted, not approved".into(),
        (None, None, _) => "No plan".into(),
    }
}
fn readiness_label(w: &WorkRecord) -> String {
    match &w.readiness {
        None => "No readiness assessment".into(),
        Some(r) => {
            let open = r
                .findings
                .0
                .iter()
                .filter(|f| f.blocking && matches!(f.state, FindingState::Open))
                .count();
            match (&r.freshness, r.ready) {
                (_, true) => "Ready".into(),
                (ReadinessFreshness::Stale, _) => format!(
                    "Readiness stale for {}",
                    r.stale_scope_keys
                        .0
                        .iter()
                        .map(|k| if k == &w.id {
                            "the feature"
                        } else {
                            k.as_str()
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                _ if open > 0 => format!("Not ready: {open} open blocking finding(s)"),
                _ => "Not ready: the plan is not approved".into(),
            }
        }
    }
}

impl Shell {
    // -----------------------------------------------------------------------------------------
    // Requests

    /// Reads the selected project's features, cards and Activity (every page).
    pub(super) fn load_work(&mut self) {
        // Saves still running belong to their items, not to the project shown: a project
        // switch keeps them, so each reply still finishes its own save.
        let Some(p) = self.selected_project.clone() else {
            self.work = WorkUi {
                archive_counts: std::mem::take(&mut self.work.archive_counts),
                plan_saving: std::mem::take(&mut self.work.plan_saving),
                edit_saving: std::mem::take(&mut self.work.edit_saving),
                create_saving: std::mem::take(&mut self.work.create_saving),
                ..WorkUi::default()
            };
            return;
        };
        if self.work.of.as_deref() != Some(p.as_str()) {
            self.work = WorkUi {
                of: Some(p.clone()),
                archive_counts: std::mem::take(&mut self.work.archive_counts),
                plan_saving: std::mem::take(&mut self.work.plan_saving),
                edit_saving: std::mem::take(&mut self.work.edit_saving),
                create_saving: std::mem::take(&mut self.work.create_saving),
                ..WorkUi::default()
            };
        }
        self.work.list_at = self.work.mutations;
        self.work.staged_features.clear();
        self.work.staged_cards.clear();
        let (a, b) = (p.clone(), p.clone());
        self.query(
            Slot::WorkFeatures(p.clone()),
            "Loading features",
            "features.list",
            move |be| wops::work_list(be, "features.list", &a, None),
        );
        self.query(
            Slot::WorkCards(p.clone()),
            "Loading tasks and issues",
            "tasks.list",
            move |be| wops::work_list(be, "tasks.list", &b, None),
        );
        // The Activity feed keeps what it read; a refresh reads on to the newest.
        if !self.work.activity.loaded && !self.pending(&Slot::WorkActivity(p)) {
            self.load_activity();
        }
    }
    /// Whether a work list read is in flight.
    pub(super) fn work_loading(&self) -> bool {
        self.work.of.as_ref().is_some_and(|p| {
            self.pending(&Slot::WorkFeatures(p.clone()))
                || self.pending(&Slot::WorkCards(p.clone()))
        })
    }
    /// Reads the project's change feed on from where the last read stopped, page after page to
    /// its end (each reply asks for the next page while more exist).
    fn load_activity(&mut self) {
        let Some(p) = self.work.of.clone() else {
            return;
        };
        let after = self.work.activity.resume.clone();
        self.query(
            Slot::WorkActivity(p.clone()),
            "Loading activity",
            "history.events",
            move |be| wops::project_events(be, &p, after.as_deref()),
        );
    }
    fn load_detail(&mut self, w: &WorkRecord) {
        let op = if matches!(w.kind, WorkKind::Feature) {
            "features.get"
        } else {
            "tasks.get"
        };
        let id = w.id.as_str().to_owned();
        self.query(
            Slot::WorkDetail(id.clone()),
            "Reading work",
            op,
            move |be| wops::work_get(be, op, &id),
        );
    }
    fn inspect(&mut self, root: &str) {
        let r = root.to_owned();
        self.query(
            Slot::WorkInspect(root.to_owned()),
            "Reading run controls",
            "runs.inspect",
            move |be| wops::run_inspect(be, &r),
        );
    }
    /// Opens a work item by ID (a completion card's link): its project is selected and its
    /// screen shown with it selected.
    pub(super) fn open_work(&mut self, id: &str) {
        let w = id.to_owned();
        self.query(
            Slot::WorkOpen(id.to_owned(), false),
            "Opening work",
            "tasks.get",
            move |be| wops::work_get(be, "tasks.get", &w),
        );
    }
    /// Counts the roots (features and standalone issues) and running roots of `project` for
    /// an archive confirmation.
    pub(super) fn count_archive_work(&mut self, project: &str) {
        for features in [true, false] {
            let key = (project.to_owned(), features);
            if self.work.counted.contains(&key)
                || self.pending(&Slot::ArchiveCount(key.0.clone(), features))
            {
                continue;
            }
            self.work.counting.insert(key, (0, 0));
            let p = project.to_owned();
            let op = if features {
                "features.list"
            } else {
                "issues.list"
            };
            self.query(
                Slot::ArchiveCount(project.to_owned(), features),
                "Counting running work",
                op,
                move |be| wops::work_list(be, op, &p, None),
            );
        }
    }
    fn record(&self, id: &str) -> Option<WorkRecord> {
        self.work.freshest(id)
    }
    fn project_open(&self) -> bool {
        self.selected_project_record()
            .is_some_and(|p| matches!(p.lifecycle, LifeState::Active))
    }

    fn work_act(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Reload => {
                    self.load_work();
                    self.load_activity();
                }
                Action::Create(form) => {
                    let Some(p) = self.work.of.clone() else {
                        continue;
                    };
                    let backlog = default_status_id(
                        &ProjectId(Id::new(p.clone()).expect("listed project id")),
                        "backlog",
                    )
                    .as_str()
                    .to_owned();
                    let (op, body): (&'static str, Value) = match form.kind.as_str() {
                        "feature" => (
                            "features.create",
                            json!({"projectId": p, "creation": {"kind": "Single",
                                "title": form.title, "description": form.description,
                                "repositoryIds": [], "statusId": backlog,
                                "priority": form.priority}}),
                        ),
                        "issue" => {
                            let mut b = json!({"projectId": p, "title": form.title,
                                "description": form.description, "repositoryIds": [],
                                "statusId": backlog, "priority": form.priority});
                            if let Some(f) = &form.feature {
                                b["featureId"] = json!(f);
                            }
                            ("issues.create", b)
                        }
                        _ => {
                            let mut b = json!({"projectId": p, "title": form.title,
                                "description": form.description, "repositoryIds": [],
                                "statusId": backlog, "priority": form.priority});
                            if let Some(f) = &form.feature {
                                b["parentId"] = json!(f);
                            }
                            ("tasks.create", b)
                        }
                    };
                    if self.command(
                        Slot::WorkCreate(p.clone()),
                        "Creating work",
                        op,
                        move |be| wops::work_command(be, op, body),
                    ) {
                        self.work.create_saving.insert(p, form);
                    }
                }
                Action::Edit(form) => {
                    let w = form.snapshot.clone();
                    let mut changes = json!({});
                    if form.title != w.title.0 {
                        changes["title"] = json!(form.title);
                    }
                    if form.description != w.description.0 {
                        changes["description"] = json!(form.description);
                    }
                    let feature = matches!(w.kind, WorkKind::Feature);
                    if !feature && form.priority != w.priority {
                        changes["priority"] = json!(form.priority);
                    }
                    if changes.as_object().is_some_and(|m| m.is_empty()) {
                        self.work.edit = None;
                        continue;
                    }
                    let op = if feature {
                        "features.edit"
                    } else {
                        "tasks.edit"
                    };
                    let body = json!({"workId": w.id, "expectedRevision": w.revision,
                        "changes": changes});
                    self.work.edit_saving.insert(w.id.as_str().to_owned(), form);
                    self.command(
                        Slot::WorkCommand(w.id.as_str().to_owned()),
                        "Saving work",
                        op,
                        move |be| wops::work_command(be, op, body),
                    );
                }
                Action::Command(id, op, body) => {
                    self.command(Slot::WorkCommand(id), "Changing work", op, move |be| {
                        wops::work_command(be, op, body)
                    });
                }
                Action::Move(w, status) => {
                    let id = w.id.as_str().to_owned();
                    let body = json!({"taskId": w.id, "expectedRevision": w.revision,
                        "statusId": status,
                        "expectedWorkflowRevision": w.workflow_revision.clone()
                            .map_or("1".to_owned(), |r| r.as_str().to_owned())});
                    // The marker shows the move as submitted; the card stays where it is until
                    // the outcome is confirmed.
                    if self.command(
                        Slot::WorkCommand(id.clone()),
                        "Moving a card",
                        "tasks.move",
                        move |be| wops::work_command(be, "tasks.move", body),
                    ) {
                        self.work.moving.insert(id, status);
                    }
                }
                Action::Order(op, feature, priority, placement, expected) => {
                    let Some(p) = self.work.of.clone() else {
                        continue;
                    };
                    // One bounded placement, whatever the bucket's size.
                    let mut body = json!({"projectId": p, "expectedQueueRevision": expected,
                        "placement": placement});
                    if op == "tasks.prioritize" {
                        body["priority"] = json!(priority.unwrap_or(0));
                        if let Some(f) = &feature {
                            body["featureId"] = json!(f);
                        }
                    }
                    let key = format!("{p}/{}", feature.unwrap_or_default());
                    self.command(Slot::WorkOrder(key), "Reordering work", op, move |be| {
                        wops::work_command(be, op, body)
                    });
                }
                Action::SavePlan(form) => self.save_plan(form),
                Action::ShowActivity(id) => {
                    self.work.activity.item = Some(id);
                    // Overview's second tab is Activity.
                    self.routes.entry(Route::Overview).or_default().tab = 1;
                    self.navigate(Route::Overview);
                }
                Action::RequestApproval(w) => {
                    let Some(pending) = w.pending_plan.clone() else {
                        continue;
                    };
                    let digest = devworks_core::services::work::plan_digest(&pending.draft);
                    let body = json!({"featureId": w.id, "expectedRevision": w.revision,
                        "planRevision": pending.revision, "planDigest": digest});
                    self.command(
                        Slot::WorkCommand(w.id.as_str().to_owned()),
                        "Requesting plan approval",
                        "features.request_plan_approval",
                        move |be| wops::work_command(be, "features.request_plan_approval", body),
                    );
                }
                Action::Inspect(root) => self.inspect(&root),
                Action::Run(op, w) => {
                    let Some(snapshot) = self.work.runs.get(w.id.as_str()).cloned() else {
                        self.inspect(w.id.as_str());
                        continue;
                    };
                    let id = w.id.as_str().to_owned();
                    self.command(Slot::WorkRun(id), "Changing run controls", op, move |be| {
                        wops::run_control(be, op, &w, &snapshot)
                    });
                }
                Action::SelectCard(c) => {
                    self.work.card = c.clone();
                    if let Some(c) = c
                        && let Some(w) = self.record(&c)
                    {
                        self.load_detail(&w);
                        self.inspect(&c);
                    }
                }
                Action::OpenDetail(id) => {
                    if let Some(w) = self.record(&id) {
                        self.load_detail(&w);
                        if w.parent_id.is_none() {
                            self.inspect(&id);
                        }
                    }
                }
            }
        }
    }

    /// Saves a plan draft: a changed (or first) plan text is stored as an artifact of the
    /// feature's project first (so its agents can read it), then the draft is submitted at the
    /// revision its form was opened on. The save is bound to `form` exactly as submitted; the
    /// editor stays open, and edits made meanwhile are kept as a draft (lock-free: the user
    /// never waits on an upload to keep writing, and nothing they write is lost or mixed in).
    fn save_plan(&mut self, form: PlanForm) {
        if let Some(error) = plan_error(&form) {
            self.work.message = Some((Tone::Bad, error));
            if self.work.plan.is_none() {
                self.work.plan = Some(form);
            }
            return;
        }
        let decisions: Vec<_> = form
            .drops
            .iter()
            .filter(|(_, d)| d.artifact.is_none())
            .map(|(id, d)| format!("## {id}\n\n{}", d.reason))
            .collect();
        let unchanged = form.content.is_some() && form.text == form.original;
        if decisions.is_empty() && unchanged {
            let content = form.content.clone().expect("checked");
            self.work
                .plan_saving
                .insert(form.snapshot.id.as_str().to_owned(), form.clone());
            self.submit_plan(&form, &content);
            return;
        }
        let id = form.snapshot.id.as_str().to_owned();
        let project = form.snapshot.project_id.clone();
        let (slot, title, text) = if decisions.is_empty() {
            (
                Slot::WorkPlanText(id.clone()),
                "Feature plan",
                form.text.clone(),
            )
        } else {
            (
                Slot::WorkPlanDrop(id.clone()),
                "Requirement drop decisions",
                format!("# Requirement drop decisions\n\n{}", decisions.join("\n\n")),
            )
        };
        let started = self.client.as_mut().is_some_and(|c| {
            c.task(slot, "Storing the plan and decisions", move |b| {
                let dir = b.scratch_dir();
                std::fs::create_dir_all(&dir)
                    .map_err(|e| format!("could not write the plan text: {e}"))?;
                let path = dir.join(format!("plan-{}.md", b.token("p")));
                std::fs::write(&path, text.as_bytes())
                    .map_err(|e| format!("could not write the plan text: {e}"))?;
                let digest = devworks_core::fingerprint::sha256_hex(text.as_bytes());
                let ticket = b.mint_project_upload(&project, &path)?;
                let frame = ops::save_project_artifact(
                    b,
                    project.0.as_str(),
                    ticket.as_str(),
                    title,
                    "text/markdown",
                    &digest,
                    text.len() as u64,
                );
                let outcome = resolve_command(b, "artifacts.save", &frame);
                if ops::saved_artifact(outcome.clone()).is_ok() {
                    crate::attachments::remove_owned(&path, &dir);
                }
                Ok(TaskOutput::Stored { outcome, frame })
            })
        });
        if started {
            self.work.plan_saving.insert(id, form);
        } else if self.work.plan.is_none() {
            // Not started (offline): the draft goes back to the editor, never dropped.
            self.work.plan = Some(form);
        }
    }
    /// A plan save of `feature` that ended without saving: its draft goes back to the editor
    /// when the editor is free (an open editor keeps what it holds).
    fn unsaved_plan(&mut self, feature: &str) {
        if let Some(saved) = self.work.plan_saving.remove(feature)
            && self.work.plan.is_none()
        {
            self.work.plan = Some(saved);
        }
    }
    /// Puts back the plan saves a close kept: one whose `features.edit_plan` is pending waits
    /// for its confirmation; one still storing its text is saved again from its snapshot.
    pub(super) fn restore_plan_saves(&mut self, saves: Vec<PlanForm>, confirming: &[String]) {
        for form in saves {
            let id = form.snapshot.id.as_str().to_owned();
            if confirming.contains(&id) {
                self.work.plan_saving.insert(id, form);
            } else {
                self.save_plan(form);
            }
        }
    }
    fn submit_plan(&mut self, form: &PlanForm, content: &ArtifactRef) {
        let w = &form.snapshot;
        // Every live requirement of the active plan stays in the draft (dropping one needs an
        // explicit decision); what the form does not show is kept from its starting draft.
        let body = json!({"featureId": w.id, "expectedRevision": w.revision,
            "plan": plan_body(form, content)});
        self.command(
            Slot::WorkPlan(w.id.as_str().to_owned()),
            "Saving the plan draft",
            "features.edit_plan",
            move |be| wops::work_command(be, "features.edit_plan", body),
        );
    }

    // -----------------------------------------------------------------------------------------
    // Replies

    /// Applies a reply for one of the work slots; returns it back when it is not one.
    pub(super) fn apply_work(&mut self, slot: Slot, reply: Reply) -> Option<(Slot, Reply)> {
        match (slot, reply) {
            (Slot::WorkFeatures(p) | Slot::WorkCards(p), Reply::Query(_))
                if self.work.of.as_deref() != Some(p.as_str()) => {}
            (Slot::WorkFeatures(_) | Slot::WorkCards(_), Reply::Query(_))
                if self.work.list_at != self.work.mutations =>
            {
                self.load_work();
            }
            (Slot::WorkFeatures(p), Reply::Query(q)) => match wops::work_page(q) {
                Ok((items, next)) => {
                    self.work.staged_features.extend(items);
                    match next {
                        Some(c) => {
                            self.query(
                                Slot::WorkFeatures(p.clone()),
                                "Loading features",
                                "features.list",
                                move |be| wops::work_list(be, "features.list", &p, Some(&c)),
                            );
                        }
                        None => {
                            self.work.features = ListState {
                                items: std::mem::take(&mut self.work.staged_features),
                                loaded: true,
                                error: None,
                                more: false,
                            };
                        }
                    }
                }
                Err(Failure::Refused(f)) if f.code == DomainErrorCode::CursorExpired => {
                    self.load_work()
                }
                Err(e) => {
                    self.work.staged_features.clear();
                    self.work.features.error = Some(e.describe());
                }
            },
            (Slot::WorkCards(p), Reply::Query(q)) => match wops::work_page(q) {
                Ok((items, next)) => {
                    self.work.staged_cards.extend(items);
                    match next {
                        Some(c) => {
                            self.query(
                                Slot::WorkCards(p.clone()),
                                "Loading tasks and issues",
                                "tasks.list",
                                move |be| wops::work_list(be, "tasks.list", &p, Some(&c)),
                            );
                        }
                        None => {
                            self.work.cards = ListState {
                                items: std::mem::take(&mut self.work.staged_cards),
                                loaded: true,
                                error: None,
                                more: false,
                            };
                            let known: BTreeSet<String> = self
                                .work
                                .cards
                                .items
                                .iter()
                                .map(|c| c.id.as_str().to_owned())
                                .collect();
                            // A card that left the project's lists is no longer selected.
                            if self.work.card.as_ref().is_some_and(|c| !known.contains(c)) {
                                self.work.card = None;
                            }
                        }
                    }
                }
                Err(Failure::Refused(f)) if f.code == DomainErrorCode::CursorExpired => {
                    self.load_work()
                }
                Err(e) => {
                    self.work.staged_cards.clear();
                    self.work.cards.error = Some(e.describe());
                }
            },
            (Slot::WorkActivity(p), Reply::Query(q)) => {
                if self.work.of.as_deref() == Some(p.as_str()) {
                    match wops::events_page(q) {
                        Ok((items, resume, more)) => {
                            let a = &mut self.work.activity;
                            a.items.extend(items);
                            if resume.is_some() {
                                a.resume = resume;
                            }
                            a.error = None;
                            if more {
                                self.load_activity();
                            } else {
                                self.work.activity.loaded = true;
                            }
                        }
                        Err(e) => {
                            // A continuation can expire (its scope changed): the feed is then
                            // read again from the beginning, once.
                            let a = &mut self.work.activity;
                            if a.resume.is_some() {
                                *a = Activity {
                                    kind: a.kind,
                                    actor: a.actor,
                                    text: std::mem::take(&mut a.text),
                                    item: a.item.take(),
                                    ..Activity::default()
                                };
                                self.load_activity();
                            } else {
                                a.error = Some(e.describe());
                            }
                        }
                    }
                }
            }
            (Slot::WorkDetail(id), Reply::Query(q)) => match wops::work_record(q) {
                Ok(w) => {
                    self.work.detail.insert(id, w);
                }
                Err(e) => self.work.message = Some((Tone::Bad, e.describe())),
            },
            (Slot::WorkOpen(id, feature), Reply::Query(q)) => match wops::work_record(q) {
                Ok(w) => {
                    self.select_project(Some(w.project_id.0.as_str().to_owned()));
                    let key = w.id.as_str().to_owned();
                    if feature {
                        self.routes.entry(Route::Features).or_default().selection = Some(key);
                        self.navigate(Route::Features);
                    } else if let Some(f) = &w.parent_id {
                        self.work.board = Some(f.as_str().to_owned());
                        self.work.card = Some(key);
                        self.routes.entry(Route::Features).or_default().tab = 1;
                        self.navigate(Route::Features);
                    } else {
                        self.routes.entry(Route::Issues).or_default().selection = Some(key);
                        self.navigate(Route::Issues);
                    }
                }
                Err(Failure::Refused(f))
                    if !feature && f.code == DomainErrorCode::UnsupportedTarget =>
                {
                    self.query(
                        Slot::WorkOpen(id.clone(), true),
                        "Opening work",
                        "features.get",
                        move |be| wops::work_get(be, "features.get", &id),
                    );
                }
                Err(e) => self.announce(format!("The work could not be opened: {}", e.describe())),
            },
            (Slot::WorkInspect(id), Reply::Query(q)) => {
                if let Ok(s) = wops::run_snapshot(q) {
                    self.work.runs.insert(id, s);
                }
            }
            (Slot::ArchiveCount(p, features), Reply::Query(q)) => {
                let key = (p.clone(), features);
                match wops::work_page(q) {
                    Ok((items, next)) => {
                        let entry = self.work.counting.entry(key.clone()).or_insert((0, 0));
                        entry.0 += items.len();
                        entry.1 += items.iter().filter(|w| !w.attempts.0.is_empty()).count();
                        match next {
                            Some(c) => {
                                let op = if features {
                                    "features.list"
                                } else {
                                    "issues.list"
                                };
                                self.query(
                                    Slot::ArchiveCount(p.clone(), features),
                                    "Counting running work",
                                    op,
                                    move |be| wops::work_list(be, op, &p, Some(&c)),
                                );
                            }
                            None => {
                                self.work.counted.insert(key.clone());
                                let other = (key.0.clone(), !features);
                                if self.work.counted.contains(&other) {
                                    let a =
                                        self.work.counting.get(&key).copied().unwrap_or_default();
                                    let b =
                                        self.work.counting.get(&other).copied().unwrap_or_default();
                                    self.work
                                        .archive_counts
                                        .insert(key.0.clone(), (a.0 + b.0, a.1 + b.1));
                                }
                            }
                        }
                    }
                    Err(_) => {
                        // Not counted: the confirmation says so rather than showing zero.
                        self.work.counting.remove(&key);
                    }
                }
            }
            (Slot::WorkCreate(project), Reply::Command(c)) => match wops::work_changed(c) {
                Ok(w) => {
                    self.work.mutations += 1;
                    if let Some(saved) = self.work.create_saving.remove(&project)
                        && self.work.of.as_ref() == Some(&project)
                        && self.work.create.as_ref() == Some(&saved)
                    {
                        self.work.create = None;
                    }
                    let text = format!("Created {} {}.", kind_label(&w).to_lowercase(), w.title.0);
                    self.work.message = Some((Tone::Good, text.clone()));
                    self.announce(text);
                    self.load_work();
                }
                Err(e) => self.work_failure("Creating work", e),
            },
            (Slot::WorkCommand(id), Reply::Command(c)) => {
                let unknown = is_unknown(&c);
                let outcome = match wops::approval_requested(c.clone()) {
                    Ok(p) => Ok(format!(
                        "Plan approval was requested ({}). The active plan and its work are unchanged until you decide.",
                        p.id.as_str()
                    )),
                    Err(_) => wops::work_changed(c).map(|w| {
                        self.work.detail.insert(id.clone(), w.clone());
                        format!("{} is saved.", w.title.0)
                    }),
                };
                if !unknown {
                    self.work.moving.remove(&id);
                }
                match outcome {
                    Ok(text) => {
                        self.work.mutations += 1;
                        // Only this item's saved edit is settled: an editor holding later edits
                        // stays open on the saved revision; a draft of other work is kept.
                        if let Some(saved) = self.work.edit_saving.remove(&id) {
                            let current = self.work.detail.get(&id).cloned();
                            self.work.edit =
                                edit_after_save(self.work.edit.take(), &saved, current);
                        }
                        if self
                            .work
                            .unlink
                            .as_ref()
                            .is_some_and(|u| u.id.as_str() == id)
                        {
                            self.work.unlink = None;
                        }
                        self.work.message = Some((Tone::Good, text.clone()));
                        self.announce(text);
                        self.load_work();
                        if let Some(w) = self.record(&id) {
                            self.load_detail(&w);
                        }
                    }
                    Err(e) => {
                        // The editor keeps its draft (an unknown outcome stays pending).
                        if !unknown {
                            self.work.edit_saving.remove(&id);
                        }
                        self.work_failure("The change", e)
                    }
                }
            }
            (Slot::WorkOrder(_), Reply::Command(c)) => match wops::work_ordered(c) {
                Ok(_) => {
                    self.work.mutations += 1;
                    self.announce("The order was saved.");
                    self.load_work();
                }
                Err(e) => self.work_failure("Reordering", e),
            },
            (Slot::WorkPlanDrop(id), Reply::Task(t)) => {
                let stored = match t {
                    Ok(TaskOutput::Stored { outcome, .. }) => ops::saved_artifact(outcome),
                    Ok(_) => Err(Failure::Other("unexpected reply".into())),
                    Err(e) => Err(Failure::Other(e)),
                };
                match (stored, self.work.plan_saving.get_mut(&id)) {
                    (Ok(record), Some(form)) => {
                        for drop in form.drops.values_mut().filter(|d| d.artifact.is_none()) {
                            drop.artifact = Some(record.artifact.clone());
                        }
                        let form = form.clone();
                        self.save_plan(form);
                    }
                    (Ok(_), None) => {}
                    (Err(e), _) => {
                        self.unsaved_plan(&id);
                        self.work_failure("Storing requirement decisions", e);
                    }
                }
            }
            (Slot::WorkPlanText(id), Reply::Task(t)) => {
                let stored = match t {
                    Ok(TaskOutput::Stored { outcome, .. }) => ops::saved_artifact(outcome),
                    Ok(_) => Err(Failure::Other("unexpected reply".into())),
                    Err(e) => Err(Failure::Other(e)),
                };
                // The draft submitted is the form this feature's save started from, never the
                // editor nor another feature's save.
                match (stored, self.work.plan_saving.get_mut(&id)) {
                    (Ok(record), Some(form)) => {
                        form.content = Some(record.artifact.clone());
                        form.original = form.text.clone();
                        let form = form.clone();
                        self.submit_plan(&form, &record.artifact);
                    }
                    (Ok(_), None) => {}
                    (Err(e), _) => {
                        self.unsaved_plan(&id);
                        self.work_failure("Storing the plan text", e)
                    }
                }
            }
            (Slot::WorkPlan(id), Reply::Command(c)) => {
                if is_unknown(&c) {
                    // Still pending: confirmed later with its exact frame.
                    self.work_failure("Saving the plan draft", Failure::Unknown);
                    return None;
                }
                let saved = self.work.plan_saving.remove(&id);
                match wops::work_changed(c) {
                    Ok(w) => {
                        self.work.mutations += 1;
                        self.work.detail.insert(id.clone(), w.clone());
                        if let Some(saved) = &saved {
                            self.work.plan = plan_after_save(self.work.plan.take(), saved, &w);
                        }
                        let text = format!("The plan draft of {} is saved.", w.title.0);
                        self.work.message = Some((Tone::Good, text.clone()));
                        self.announce(text);
                        self.load_work();
                        self.load_detail(&w);
                    }
                    Err(e) => {
                        if let Some(saved) = saved
                            && self.work.plan.is_none()
                        {
                            self.work.plan = Some(saved);
                        }
                        self.work_failure("Saving the plan draft", e)
                    }
                }
            }
            (Slot::WorkRun(id), Reply::Command(c)) => {
                match wops::run_changed(c) {
                    Ok(s) => {
                        self.work.runs.insert(id.clone(), s);
                        self.announce("Run controls changed.");
                    }
                    Err(e) => self.work_failure("Run control", e),
                }
                self.inspect(&id);
                self.load_work();
            }
            (slot, reply) => return Some((slot, reply)),
        }
        None
    }

    fn work_failure(&mut self, what: &str, e: Failure) {
        let text = match &e {
            Failure::Unknown => format!(
                "{what}: not yet confirmed whether it was applied. Use Confirm outcomes to check \
                 again; it is never applied twice."
            ),
            Failure::Refused(f) if f.code == DomainErrorCode::RevisionConflict => format!(
                "{what} was not applied: it changed meanwhile. Your draft is kept; review the \
                 current version and try again."
            ),
            Failure::Refused(f) => {
                let kind = f.target_kind.as_ref().map(|k| k.0.as_str()).unwrap_or("");
                let why = match kind {
                    "dependency_cycle" => "that link would create a dependency cycle.".to_owned(),
                    "plan_scope" => {
                        "it is outside the feature's approved scope; change the plan instead."
                            .to_owned()
                    }
                    "project_archived" => "the project is archived.".to_owned(),
                    "work_completed" => {
                        "completed work keeps its prerequisites; reopen it first.".to_owned()
                    }
                    "requirement_missing" => {
                        "a requirement of the approved plan is missing from the draft.".to_owned()
                    }
                    "plan_changed" => "the plan changed meanwhile.".to_owned(),
                    "feature_closed" => "that feature is finished.".to_owned(),
                    _ => e.describe(),
                };
                format!("{what} was not applied: {why}")
            }
            other => format!("{what} was not applied: {}", other.describe()),
        };
        // A refused or failed change keeps its form (the user's draft); only a confirmed
        // change clears it.
        self.work.message = Some((Tone::Bad, text.clone()));
        self.announce(text);
        if matches!(&e, Failure::Refused(_)) {
            self.load_work();
        }
    }

    // -----------------------------------------------------------------------------------------
    // Screens

    fn no_project(ui: &mut Ui, online: &Result<(), String>) {
        let state = match online {
            Err(reason) => ViewState::Offline {
                reason: reason.clone(),
            },
            Ok(()) => ViewState::Empty {
                title: "No project selected".into(),
                body: "Select a project in the navigation to see its work.".into(),
            },
        };
        widgets::state_block(ui, &state);
    }
    fn list_state(ui: &mut Ui, list: &ListState<WorkRecord>, what: &str) -> bool {
        if let Some(e) = &list.error {
            widgets::state_block(
                ui,
                &if list.loaded {
                    ViewState::Stale { what: what.into() }
                } else {
                    ViewState::Failed {
                        what: what.into(),
                        error: e.clone(),
                    }
                },
            );
            if !list.loaded {
                return false;
            }
        }
        if !list.loaded {
            widgets::state_block(ui, &ViewState::Loading { what: what.into() });
            return false;
        }
        true
    }

    pub(super) fn overview_screen(&mut self, ui: &mut Ui) {
        let screen = Route::Overview.screen();
        let online = self.online();
        let mut mem = self.routes.remove(&Route::Overview).unwrap_or_default();
        let mut actions: Vec<Action> = Vec::new();
        let project = self.selected_project.clone();
        let cols = columns(project.as_deref().unwrap_or("p"), &self.work.cards.items);
        let user = devworks_desktop_user();
        let work = &mut self.work;
        let (_, tab) = widgets::pane(
            ui,
            Header {
                title: screen.title,
                summary: screen.summary,
                tabs: &OVERVIEW_TABS,
                tab: mem.tab.min(1),
            },
            &mut actions,
            |ui, actions| {
                if widgets::secondary_button(ui, "Refresh").clicked() {
                    actions.push(Action::Reload);
                }
            },
            |ui, actions, tab| {
                let loaded = project.is_some() && work.features.loaded && work.cards.loaded;
                let issues: Vec<&WorkRecord> = work
                    .cards
                    .items
                    .iter()
                    .filter(|c| c.parent_id.is_none())
                    .collect();
                let working = work
                    .features
                    .items
                    .iter()
                    .chain(&work.cards.items)
                    .filter(|w| !w.attempts.0.is_empty())
                    .count();
                let needs = work
                    .features
                    .items
                    .iter()
                    .filter(|f| {
                        f.pending_plan
                            .as_ref()
                            .and_then(|p| p.approval.as_ref())
                            .is_some_and(|a| matches!(a.state, ApprovalState::Pending))
                            || f.readiness.as_ref().is_some_and(|r| {
                                r.findings.0.iter().any(|x| {
                                    matches!(x.disposition, ReadinessDisposition::NeedsUserDecision)
                                        && matches!(x.state, FindingState::Open)
                                })
                            })
                    })
                    .count();
                let value = |n: usize| loaded.then(|| n.to_string());
                widgets::summary_strip(
                    ui,
                    &[
                        ("Features", value(work.features.items.len())),
                        ("Issues", value(issues.len())),
                        ("Working", value(working)),
                        ("Needs you", value(needs)),
                    ],
                    "select a project and wait for its work to load",
                );
                ui.add_space(10.0);
                if project.is_none() {
                    Self::no_project(ui, &online);
                    return;
                }
                if tab == 0 {
                    if !Self::list_state(ui, &work.features, "features")
                        || !Self::list_state(ui, &work.cards, "tasks and issues")
                    {
                        return;
                    }
                    ui.label(RichText::new("In progress").strong());
                    let active: Vec<&WorkRecord> = work
                        .features
                        .items
                        .iter()
                        .chain(&work.cards.items)
                        .filter(|w| {
                            let s = semantic_of(&cols, w.status_id.as_str());
                            s == "InProgress" || s == "InReview"
                        })
                        .collect();
                    if active.is_empty() {
                        widgets::caption(ui, "Nothing is in progress.");
                    }
                    for w in active {
                        ui.label(format!(
                            "{} · {} · {}",
                            kind_label(w),
                            w.title.0,
                            status_name(&cols, w.status_id.as_str())
                        ));
                    }
                } else {
                    activity(ui, work, &cols, &user, actions);
                }
            },
        );
        mem.tab = tab;
        self.routes.insert(Route::Overview, mem);
        self.work_act(actions);
    }

    pub(super) fn features_screen(&mut self, ui: &mut Ui) {
        let screen = Route::Features.screen();
        let online = self.online();
        let mut mem = self.routes.remove(&Route::Features).unwrap_or_default();
        let mut actions: Vec<Action> = Vec::new();
        let project = self.selected_project.clone();
        let open = self.project_open() && online.is_ok();
        let cols = columns(project.as_deref().unwrap_or("p"), &self.work.cards.items);
        let waiting: BTreeSet<String> = self
            .work
            .features
            .items
            .iter()
            .chain(&self.work.cards.items)
            .map(|w| w.id.as_str().to_owned())
            .filter(|id| {
                self.waiting(&Slot::WorkCommand(id.clone()))
                    || self.waiting(&Slot::WorkPlanText(id.clone()))
                    || self.waiting(&Slot::WorkPlanDrop(id.clone()))
                    || self.waiting(&Slot::WorkPlan(id.clone()))
            })
            .collect();
        let creating = project
            .as_ref()
            .is_some_and(|p| self.waiting(&Slot::WorkCreate(p.clone())));
        let projects_repos: Vec<String> = self
            .selected_project_record()
            .map(|p| {
                p.repository_ids
                    .0
                    .iter()
                    .map(|r| r.as_str().to_owned())
                    .collect()
            })
            .unwrap_or_default();
        let tab = mem.tab.min(1);
        let mut new_tab = tab;
        {
            let work = &mut self.work;
            let mut st = (&mut *work, &mut mem, &mut actions);
            widgets::with_right_sidebar(
                ui,
                theme::SIDEBAR,
                &mut st,
                |ui, st| {
                    let (_, t) = widgets::pane(
                        ui,
                        Header {
                            title: screen.title,
                            summary: screen.summary,
                            tabs: &FEATURE_TABS,
                            tab,
                        },
                        st,
                        |ui, st| {
                            if widgets::secondary_button(ui, "Refresh").clicked() {
                                st.2.push(Action::Reload);
                            }
                            let r = ui.add_enabled(
                                open && !creating && project.is_some(),
                                egui::Button::new(if tab == 1 {
                                    "New task"
                                } else {
                                    "New feature"
                                }),
                            );
                            if r.clicked() && st.0.create.is_none() {
                                st.0.create = Some(CreateForm {
                                    kind: if tab == 1 { "task" } else { "feature" }.into(),
                                    feature: if tab == 1 { st.0.board.clone() } else { None },
                                    ..CreateForm::default()
                                });
                            }
                        },
                        |ui, st, tab| {
                            let (work, mem, actions) = (&mut *st.0, &mut *st.1, &mut *st.2);
                            let counts = |sem: &str| {
                                work.features
                                    .items
                                    .iter()
                                    .filter(|f| semantic_of(&cols, f.status_id.as_str()) == sem)
                                    .count()
                            };
                            let loaded = project.is_some() && work.features.loaded;
                            let v = |n: usize| loaded.then(|| n.to_string());
                            widgets::summary_strip(
                                ui,
                                &[
                                    ("Planned", v(counts("Backlog") + counts("Ready"))),
                                    ("In progress", v(counts("InProgress"))),
                                    ("In review", v(counts("InReview"))),
                                    ("Complete", v(counts("Done"))),
                                ],
                                "select a project and wait for its features to load",
                            );
                            ui.add_space(10.0);
                            if project.is_none() {
                                Self::no_project(ui, &online);
                                return;
                            }
                            if let Some((tone, text)) = &work.message {
                                chip(ui, text.clone(), *tone);
                                ui.add_space(6.0);
                            }
                            create_form(ui, work, creating, actions);
                            if tab == 0 {
                                feature_list(ui, work, mem, &cols, open, actions);
                            } else {
                                board(ui, work, &cols, open, &waiting, actions);
                            }
                        },
                    );
                    new_tab = t;
                },
                |ui, st| {
                    let (work, mem, actions) = (&mut *st.0, &*st.1, &mut *st.2);
                    widgets::secondary_pane(ui, "Feature", |ui| {
                        if tab == 1
                            && let Some(c) = work.card.clone()
                        {
                            let found = work.freshest(&c);
                            if let Some(w) = found {
                                card_detail(
                                    ui,
                                    work,
                                    &w,
                                    &cols,
                                    open && !waiting.contains(&c),
                                    actions,
                                );
                                return;
                            }
                        }
                        let selected = if tab == 1 {
                            work.board.clone()
                        } else {
                            mem.selection.clone()
                        };
                        let found = selected.and_then(|id| work.freshest(&id));
                        match found {
                            None => {
                                widgets::state_block(
                                    ui,
                                    &ViewState::Empty {
                                        title: "Nothing selected".into(),
                                        body: "Select a feature to see its plan, readiness and work here."
                                            .into(),
                                    },
                                );
                            }
                            Some(f) => {
                                let id = f.id.as_str().to_owned();
                                feature_detail(
                                    ui,
                                    work,
                                    &f,
                                    &cols,
                                    &projects_repos,
                                    open && !waiting.contains(&id),
                                    actions,
                                );
                            }
                        }
                    });
                },
            );
        }
        // A newly selected feature reads its current record (readiness).
        if let Some(sel) = mem.selection.clone()
            && !self.work.detail.contains_key(&sel)
            && !self.pending(&Slot::WorkDetail(sel.clone()))
        {
            actions.push(Action::OpenDetail(sel));
        }
        mem.tab = new_tab;
        self.routes.insert(Route::Features, mem);
        self.unlink_dialog(ui.ctx(), &mut actions);
        self.work_act(actions);
    }

    pub(super) fn issues_screen(&mut self, ui: &mut Ui) {
        let screen = Route::Issues.screen();
        let online = self.online();
        let mut mem = self.routes.remove(&Route::Issues).unwrap_or_default();
        let mut actions: Vec<Action> = Vec::new();
        let project = self.selected_project.clone();
        let open = self.project_open() && online.is_ok();
        let cols = columns(project.as_deref().unwrap_or("p"), &self.work.cards.items);
        let waiting: BTreeSet<String> = self
            .work
            .cards
            .items
            .iter()
            .map(|w| w.id.as_str().to_owned())
            .filter(|id| self.waiting(&Slot::WorkCommand(id.clone())))
            .collect();
        let creating = project
            .as_ref()
            .is_some_and(|p| self.waiting(&Slot::WorkCreate(p.clone())));
        let tab = mem.tab.min(1);
        let mut new_tab = tab;
        {
            let work = &mut self.work;
            let mut st = (&mut *work, &mut mem, &mut actions);
            widgets::with_right_sidebar(
                ui,
                theme::SIDEBAR,
                &mut st,
                |ui, st| {
                    let (_, t) = widgets::pane(
                        ui,
                        Header {
                            title: screen.title,
                            summary: screen.summary,
                            tabs: &ISSUE_TABS,
                            tab,
                        },
                        st,
                        |ui, st| {
                            if widgets::secondary_button(ui, "Refresh").clicked() {
                                st.2.push(Action::Reload);
                            }
                            let r = ui.add_enabled(
                                open && !creating && project.is_some(),
                                egui::Button::new("New issue"),
                            );
                            if r.clicked() && st.0.create.is_none() {
                                st.0.create = Some(CreateForm {
                                    kind: "issue".into(),
                                    ..CreateForm::default()
                                });
                            }
                        },
                        |ui, st, tab| {
                            let (work, mem, actions) = (&mut *st.0, &mut *st.1, &mut *st.2);
                            if project.is_none() {
                                Self::no_project(ui, &online);
                                return;
                            }
                            if let Some((tone, text)) = &work.message {
                                chip(ui, text.clone(), *tone);
                                ui.add_space(6.0);
                            }
                            create_form(ui, work, creating, actions);
                            issue_list(ui, work, mem, &cols, tab == 1, open, actions);
                        },
                    );
                    new_tab = t;
                },
                |ui, st| {
                    let (work, mem, actions) = (&mut *st.0, &*st.1, &mut *st.2);
                    widgets::secondary_pane(ui, "Issue", |ui| {
                        let found = mem.selection.as_ref().and_then(|id| work.freshest(id));
                        match found {
                            None => {
                                widgets::state_block(
                                    ui,
                                    &ViewState::Empty {
                                        title: "Nothing selected".into(),
                                        body: "Select an issue to see and change it here.".into(),
                                    },
                                );
                            }
                            Some(w) => {
                                let id = w.id.as_str().to_owned();
                                card_detail(
                                    ui,
                                    work,
                                    &w,
                                    &cols,
                                    open && !waiting.contains(&id),
                                    actions,
                                );
                            }
                        }
                    });
                },
            );
        }
        if let Some(sel) = mem.selection.clone()
            && !self.work.detail.contains_key(&sel)
            && !self.pending(&Slot::WorkDetail(sel.clone()))
        {
            actions.push(Action::OpenDetail(sel));
        }
        mem.tab = new_tab;
        self.routes.insert(Route::Issues, mem);
        self.unlink_dialog(ui.ctx(), &mut actions);
        self.work_act(actions);
    }

    fn unlink_dialog(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let Some(w) = self.work.unlink.clone() else {
            return;
        };
        let mut decided = None;
        egui::Window::new("Remove from feature")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label(format!(
                    "Remove {} from its feature? It becomes a standalone issue with the same \
                     identity, history, branches and workspace. Its work is paused and stays \
                     paused until you continue it; adding it to a feature again does not resume \
                     it.",
                    w.title.0
                ));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Remove from feature").clicked() {
                        decided = Some(true);
                    }
                    if widgets::secondary_button(ui, "Cancel").clicked() {
                        decided = Some(false);
                    }
                });
            });
        match decided {
            Some(true) => {
                self.work.unlink = None;
                actions.push(Action::Command(
                    w.id.as_str().to_owned(),
                    "tasks.set_feature",
                    json!({"taskId": w.id, "expectedRevision": w.revision, "clearFeature": true}),
                ));
            }
            Some(false) => self.work.unlink = None,
            None => {}
        }
    }

    /// The archive confirmation's line about the work it stops (WORK-16).
    pub(super) fn archive_work_line(&mut self, project: &str) -> String {
        match self.work.archive_counts.get(project) {
            Some((roots, running)) if *running > 0 => format!(
                " {running} of its {roots} features and issues have agents working on them now; \
                 archiving stops them at once."
            ),
            Some((roots, _)) => {
                format!(" Its {roots} features and issues have no agent working now.")
            }
            None => {
                self.count_archive_work(project);
                " Counting the work that is running…".into()
            }
        }
    }
}

/// The desktop user's actor (the host binds it), for "You" in Activity.
fn devworks_desktop_user() -> String {
    crate::host::USER_ACTOR.to_owned()
}

fn activity(
    ui: &mut Ui,
    work: &mut WorkUi,
    cols: &[Column],
    user: &str,
    _actions: &mut Vec<Action>,
) {
    let a = &mut work.activity;
    ui.horizontal_wrapped(|ui| {
        egui::ComboBox::from_id_salt("activity-kind")
            .selected_text(["Everything", "Features", "Issues", "Tasks", "Project"][a.kind.min(4)])
            .show_ui(ui, |ui| {
                for (i, l) in ["Everything", "Features", "Issues", "Tasks", "Project"]
                    .iter()
                    .enumerate()
                {
                    ui.selectable_value(&mut a.kind, i, *l);
                }
            })
            .response
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Activity kind"));
        egui::ComboBox::from_id_salt("activity-actor")
            .selected_text(["Everyone", "You", "Agents and others"][a.actor.min(2)])
            .show_ui(ui, |ui| {
                for (i, l) in ["Everyone", "You", "Agents and others"].iter().enumerate() {
                    ui.selectable_value(&mut a.actor, i, *l);
                }
            })
            .response
            .widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Activity actor")
            });
        let r = ui.add(
            egui::TextEdit::singleline(&mut a.text)
                .hint_text("Filter activity")
                .desired_width(200.0),
        );
        r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Filter activity"));
        if a.item.is_some() && widgets::secondary_button(ui, "All work").clicked() {
            a.item = None;
        }
    });
    if let Some(e) = &a.error {
        widgets::state_block(
            ui,
            &ViewState::Failed {
                what: "activity".into(),
                error: e.clone(),
            },
        );
    }
    if !a.loaded {
        widgets::state_block(
            ui,
            &ViewState::Loading {
                what: "activity".into(),
            },
        );
        return;
    }
    let titles: BTreeMap<String, &WorkRecord> = work
        .features
        .items
        .iter()
        .chain(&work.cards.items)
        .map(|w| (w.id.as_str().to_owned(), w))
        .collect();
    let rows: Vec<String> = a
        .items
        .iter()
        .rev()
        .filter_map(|e| {
            let subject = match (&e.aggregate, &e.root_id) {
                (EventSubject::Root(r), _) => Some(r.0.as_str().to_owned()),
                (_, Some(r)) => Some(r.0.as_str().to_owned()),
                _ => None,
            };
            let w = subject.as_ref().and_then(|s| titles.get(s)).copied();
            let kind_ok = match a.kind {
                1 => w.is_some_and(|w| matches!(w.kind, WorkKind::Feature)),
                2 => {
                    w.is_some_and(|w| w.parent_id.is_none() && !matches!(w.kind, WorkKind::Feature))
                }
                3 => w.is_some_and(|w| w.parent_id.is_some()),
                4 => subject.is_none(),
                _ => true,
            };
            let mine = e.actor_id.0.as_str() == user;
            let actor_ok = match a.actor {
                1 => mine,
                2 => !mine,
                _ => true,
            };
            let item_ok = a
                .item
                .as_ref()
                .is_none_or(|i| subject.as_deref() == Some(i.as_str()));
            let what = match w {
                Some(w) => format!(
                    "{} {} ({})",
                    kind_label(w),
                    w.title.0,
                    status_name(cols, w.status_id.as_str())
                ),
                None => subject.map_or("the project".into(), |s| format!("work {s}")),
            };
            let line = format!(
                "#{} · {} · {} · {}",
                e.sequence.as_str(),
                if mine { "You" } else { e.actor_id.0.as_str() },
                what,
                e.occurred_at_ms.as_str()
            );
            (kind_ok && actor_ok && item_ok && matches_text_line(&line, &a.text)).then_some(line)
        })
        .collect();
    widgets::caption(
        ui,
        format!(
            "{} of {} changes shown · newest first · full history (Refresh adds newer changes)",
            rows.len(),
            a.items.len(),
        ),
    );
    if rows.is_empty() {
        widgets::state_block(
            ui,
            &ViewState::Empty {
                title: "No activity".into(),
                body: "Changes to this project's work appear here.".into(),
            },
        );
    }
    egui::ScrollArea::vertical()
        .id_salt("activity-rows")
        .max_height(ui.available_height() - 40.0)
        .show(ui, |ui| {
            for r in &rows {
                ui.label(r);
            }
        });
}
fn matches_text_line(line: &str, needle: &str) -> bool {
    let n = fold(needle.trim());
    n.is_empty() || fold(line).contains(&n)
}

fn create_form(ui: &mut Ui, work: &mut WorkUi, busy: bool, actions: &mut Vec<Action>) {
    let features: Vec<(String, String)> = work
        .features
        .items
        .iter()
        .map(|f| (f.id.as_str().to_owned(), f.title.0.clone()))
        .collect();
    let Some(form) = work.create.as_mut() else {
        return;
    };
    let mut submit = None;
    let mut cancel = false;
    egui::Frame::new()
        .stroke(theme::soft_line())
        .corner_radius(theme::radius())
        .inner_margin(theme::margin(12, 10))
        .show(ui, |ui| {
            // A creation consumes the form: it is locked while it runs, so nothing typed
            // meanwhile is dropped by its reply.
            if busy {
                ui.disable();
            }
            ui.label(RichText::new(format!("New {}", form.kind)).strong());
            let r = ui.add(egui::TextEdit::singleline(&mut form.title).hint_text("Title"));
            r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Title"));
            let r = ui.add(
                egui::TextEdit::multiline(&mut form.description)
                    .hint_text("Description, reproduction, expected and actual behavior")
                    .desired_rows(3),
            );
            r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Description"));
            ui.horizontal(|ui| {
                ui.label("Priority");
                ui.add(egui::DragValue::new(&mut form.priority).range(-1000..=1000))
                    .widget_info(|| {
                        egui::WidgetInfo::labeled(egui::Role::SpinButton, true, "Priority")
                    });
            });
            if form.kind != "feature" {
                let label = form
                    .feature
                    .as_ref()
                    .and_then(|f| features.iter().find(|(id, _)| id == f))
                    .map_or("No feature (standalone issue)".to_owned(), |(_, t)| {
                        t.clone()
                    });
                egui::ComboBox::from_id_salt("create-feature")
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut form.feature,
                            None,
                            "No feature (standalone issue)",
                        );
                        for (id, t) in &features {
                            ui.selectable_value(&mut form.feature, Some(id.clone()), t);
                        }
                    })
                    .response
                    .widget_info(|| {
                        egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Feature")
                    });
            }
            ui.horizontal(|ui| {
                let ok = !busy && !form.title.trim().is_empty();
                if ui.add_enabled(ok, egui::Button::new("Create")).clicked() {
                    submit = Some(form.clone());
                }
                if widgets::secondary_button(ui, "Cancel").clicked() {
                    cancel = true;
                }
            });
        });
    if let Some(f) = submit {
        actions.push(Action::Create(f));
    }
    if cancel {
        work.create = None;
    }
    ui.add_space(8.0);
}

/// The placement moving `bucket[pos]` one step up (before its predecessor) or down (after its
/// successor): one bounded request whatever the bucket's size (`None` at the bucket's edge).
pub fn adjacent_move(bucket: &[&str], pos: usize, up: bool) -> Option<Value> {
    let anchor = if up {
        bucket.get(pos.checked_sub(1)?)?
    } else {
        bucket.get(pos + 1)?
    };
    Some(json!({"workId": bucket.get(pos)?, "anchorId": anchor,
        "side": if up { "Before" } else { "After" }}))
}

/// Up/Down controls of one item among `bucket` (the items of its priority in queue order).
fn order_controls(
    ui: &mut Ui,
    w: &WorkRecord,
    bucket: &[&WorkRecord],
    enabled: bool,
    action: impl Fn(Value) -> Action,
    actions: &mut Vec<Action>,
) {
    let pos = bucket.iter().position(|b| b.id == w.id).unwrap_or(0);
    let ids: Vec<&str> = bucket.iter().map(|b| b.id.as_str()).collect();
    let up = ui
        .add_enabled(enabled && pos > 0, egui::Button::new("Up").small())
        .on_hover_text(format!("Move {} up", w.title.0));
    up.widget_info(|| {
        egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Move {} up", w.title.0))
    });
    if up.clicked()
        && let Some(m) = adjacent_move(&ids, pos, true)
    {
        actions.push(action(m));
    }
    let down = ui
        .add_enabled(
            enabled && pos + 1 < bucket.len(),
            egui::Button::new("Down").small(),
        )
        .on_hover_text(format!("Move {} down", w.title.0));
    down.widget_info(|| {
        egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Move {} down", w.title.0))
    });
    if down.clicked()
        && let Some(m) = adjacent_move(&ids, pos, false)
    {
        actions.push(action(m));
    }
}

fn feature_list(
    ui: &mut Ui,
    work: &mut WorkUi,
    mem: &mut crate::memory::RouteMemory,
    cols: &[Column],
    open: bool,
    actions: &mut Vec<Action>,
) {
    ui.horizontal_wrapped(|ui| {
        let r = ui.add(
            egui::TextEdit::singleline(&mut mem.filter)
                .hint_text("Search features")
                .desired_width(240.0),
        );
        r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Search features"));
        let label = work
            .status_filter
            .as_ref()
            .map_or("Every status".to_owned(), |s| status_name(cols, s));
        egui::ComboBox::from_id_salt("feature-status-filter")
            .selected_text(label)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut work.status_filter, None, "Every status");
                for c in cols {
                    ui.selectable_value(&mut work.status_filter, Some(c.id.clone()), &c.name);
                }
            })
            .response
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Status filter"));
    });
    if !Shell::list_state(ui, &work.features, "features") {
        return;
    }
    let all = &work.features.items;
    let shown: Vec<&WorkRecord> = all
        .iter()
        .filter(|f| matches_text(f, &mem.filter))
        .filter(|f| {
            work.status_filter
                .as_ref()
                .is_none_or(|s| f.status_id.as_str() == s)
        })
        .collect();
    widgets::caption(
        ui,
        format!("{} of {} features shown", shown.len(), all.len()),
    );
    if shown.is_empty() {
        widgets::state_block(
            ui,
            &ViewState::Empty {
                title: if all.is_empty() {
                    "No features yet".into()
                } else {
                    "Nothing matches the filter".into()
                },
                body: if all.is_empty() {
                    "Create a feature to plan and deliver work.".into()
                } else {
                    "Clear or change the filter.".into()
                },
            },
        );
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("features-list")
        .max_height(ui.available_height())
        .show(ui, |ui| {
            for f in &shown {
                let bucket: Vec<&WorkRecord> =
                    all.iter().filter(|x| x.priority == f.priority).collect();
                let expected = f
                    .queue_revision
                    .as_ref()
                    .map_or("1".to_owned(), |r| r.as_str().to_owned());
                ui.horizontal(|ui| {
                    let selected = mem.selection.as_deref() == Some(f.id.as_str());
                    let text = format!(
                        "{}  ·  P{}  ·  {}  ·  {}  ·  {}",
                        f.title.0,
                        f.priority,
                        status_name(cols, f.status_id.as_str()),
                        approval_label(f),
                        readiness_label(f)
                    );
                    if ui.selectable_label(selected, text).clicked() {
                        mem.selection = Some(f.id.as_str().to_owned());
                        actions.push(Action::OpenDetail(f.id.as_str().to_owned()));
                    }
                    order_controls(
                        ui,
                        f,
                        &bucket,
                        open,
                        |ids| {
                            Action::Order("features.prioritize", None, None, ids, expected.clone())
                        },
                        actions,
                    );
                });
            }
        });
}

fn issue_list(
    ui: &mut Ui,
    work: &mut WorkUi,
    mem: &mut crate::memory::RouteMemory,
    cols: &[Column],
    history: bool,
    open: bool,
    actions: &mut Vec<Action>,
) {
    let r = ui.add(
        egui::TextEdit::singleline(&mut mem.filter)
            .hint_text("Search issues")
            .desired_width(240.0),
    );
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Search issues"));
    if !Shell::list_state(ui, &work.cards, "issues") {
        return;
    }
    let standalone: Vec<&WorkRecord> = work
        .cards
        .items
        .iter()
        .filter(|c| c.parent_id.is_none())
        .collect();
    let shown: Vec<&WorkRecord> = standalone
        .iter()
        .copied()
        .filter(|c| finished(&semantic_of(cols, c.status_id.as_str())) == history)
        .filter(|c| matches_text(c, &mem.filter))
        .collect();
    widgets::caption(
        ui,
        format!("{} of {} issues shown", shown.len(), standalone.len()),
    );
    if shown.is_empty() {
        widgets::state_block(
            ui,
            &ViewState::Empty {
                title: if history {
                    "No finished issues"
                } else {
                    "No open issues"
                }
                .into(),
                body: if history {
                    String::new()
                } else {
                    "Create an issue for work that needs no feature.".into()
                },
            },
        );
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("issues-list")
        .max_height(ui.available_height())
        .show(ui, |ui| {
            for c in &shown {
                let bucket: Vec<&WorkRecord> = standalone
                    .iter()
                    .copied()
                    .filter(|x| x.priority == c.priority)
                    .collect();
                let expected = c
                    .queue_revision
                    .as_ref()
                    .map_or("1".to_owned(), |r| r.as_str().to_owned());
                ui.horizontal(|ui| {
                    let selected = mem.selection.as_deref() == Some(c.id.as_str());
                    let blocked = if c.dependency_ids.0.is_empty() {
                        ""
                    } else {
                        " · blocked by work"
                    };
                    let text = format!(
                        "{}  ·  P{}  ·  {}{blocked}",
                        c.title.0,
                        c.priority,
                        status_name(cols, c.status_id.as_str())
                    );
                    if ui.selectable_label(selected, text).clicked() {
                        mem.selection = Some(c.id.as_str().to_owned());
                        actions.push(Action::OpenDetail(c.id.as_str().to_owned()));
                    }
                    order_controls(
                        ui,
                        c,
                        &bucket,
                        open && !history,
                        |ids| Action::Order("issues.prioritize", None, None, ids, expected.clone()),
                        actions,
                    );
                });
            }
        });
}

/// What a dragged card carries.
#[derive(Clone)]
struct DraggedCard(WorkRecord);

fn board(
    ui: &mut Ui,
    work: &mut WorkUi,
    cols: &[Column],
    open: bool,
    waiting: &BTreeSet<String>,
    actions: &mut Vec<Action>,
) {
    if !Shell::list_state(ui, &work.features, "features")
        || !Shell::list_state(ui, &work.cards, "cards")
    {
        return;
    }
    let label = work
        .board
        .as_ref()
        .and_then(|b| work.features.items.iter().find(|f| f.id.as_str() == b))
        .map_or("Choose a feature".to_owned(), |f| f.title.0.clone());
    let mut chosen = work.board.clone();
    egui::ComboBox::from_id_salt("board-feature")
        .selected_text(label)
        .show_ui(ui, |ui| {
            for f in &work.features.items {
                ui.selectable_value(&mut chosen, Some(f.id.as_str().to_owned()), &f.title.0);
            }
        })
        .response
        .widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Board feature"));
    if chosen != work.board {
        work.board = chosen.clone();
        work.card = None;
        if let Some(f) = chosen {
            actions.push(Action::OpenDetail(f));
        }
    }
    let Some(feature) = work.board.clone() else {
        widgets::state_block(
            ui,
            &ViewState::Empty {
                title: "No feature chosen".into(),
                body: "Choose a feature to see its board.".into(),
            },
        );
        return;
    };
    let cards: Vec<WorkRecord> = work
        .cards
        .items
        .iter()
        .filter(|c| c.parent_id.as_ref().map(Id::as_str) == Some(feature.as_str()))
        .cloned()
        .collect();
    let mut dropped: Option<(WorkRecord, String)> = None;
    egui::ScrollArea::horizontal()
        .id_salt("board")
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                for col in cols {
                    let in_col: Vec<&WorkRecord> = cards
                        .iter()
                        .filter(|c| c.status_id.as_str() == col.id)
                        .collect();
                    let frame = egui::Frame::new()
                        .stroke(theme::soft_line())
                        .corner_radius(theme::radius())
                        .inner_margin(theme::margin(8, 8));
                    let (_, payload) = ui.dnd_drop_zone::<DraggedCard, _>(frame, |ui| {
                        ui.set_width(200.0);
                        ui.set_min_height(240.0);
                        ui.label(
                            RichText::new(format!("{} ({})", col.name, in_col.len())).strong(),
                        );
                        for c in &in_col {
                            let bucket: Vec<&WorkRecord> =
                                cards.iter().filter(|x| x.priority == c.priority).collect();
                            let id = c.id.as_str().to_owned();
                            let pending = waiting.contains(&id);
                            let marker = work.moving.get(&id).map(|s| status_name(cols, s));
                            egui::Frame::new()
                                .fill(theme::PANEL)
                                .stroke(theme::line())
                                .corner_radius(theme::radius())
                                .inner_margin(theme::margin(8, 6))
                                .show(ui, |ui| {
                                    ui.set_width(184.0);
                                    let r = ui
                                        .dnd_drag_source(
                                            egui::Id::unique(("card", &id)),
                                            DraggedCard((*c).clone()),
                                            |ui| {
                                                ui.label(short(&c.title.0, 40));
                                            },
                                        )
                                        .response;
                                    if r.clicked() {
                                        actions.push(Action::SelectCard(Some(id.clone())));
                                    }
                                    if ui
                                        .small_button(format!("Open {}", short(&c.title.0, 18)))
                                        .clicked()
                                    {
                                        actions.push(Action::SelectCard(Some(id.clone())));
                                    }
                                    match (&marker, pending) {
                                        (Some(to), true) => {
                                            widgets::caption(ui, format!("Moving to {to}…"))
                                        }
                                        (_, true) => {
                                            widgets::caption(ui, "A change is not confirmed yet")
                                        }
                                        _ => {}
                                    }
                                    if !c.dependency_ids.0.is_empty() {
                                        widgets::caption(
                                            ui,
                                            format!("Blocked by {}", c.dependency_ids.0.len()),
                                        );
                                    }
                                    // Accessible alternatives to drag and drop.
                                    let mut target = c.status_id.as_str().to_owned();
                                    egui::ComboBox::from_id_salt(("move", &id))
                                        .selected_text(format!("Move {}", short(&c.title.0, 12)))
                                        .show_ui(ui, |ui| {
                                            for col in cols {
                                                ui.selectable_value(
                                                    &mut target,
                                                    col.id.clone(),
                                                    &col.name,
                                                );
                                            }
                                        })
                                        .response
                                        .widget_info(|| {
                                            egui::WidgetInfo::labeled(
                                                egui::Role::ComboBox,
                                                open && !pending,
                                                format!("Move {}", c.title.0),
                                            )
                                        });
                                    if target != c.status_id.as_str() && open && !pending {
                                        actions.push(Action::Move((*c).clone(), target));
                                    }
                                    let expected = c
                                        .queue_revision
                                        .as_ref()
                                        .map_or("1".to_owned(), |r| r.as_str().to_owned());
                                    let prio = c.priority;
                                    let f = feature.clone();
                                    ui.horizontal(|ui| {
                                        order_controls(
                                            ui,
                                            c,
                                            &bucket,
                                            open && !pending,
                                            |ids| {
                                                Action::Order(
                                                    "tasks.prioritize",
                                                    Some(f.clone()),
                                                    Some(prio),
                                                    ids,
                                                    expected.clone(),
                                                )
                                            },
                                            actions,
                                        );
                                    });
                                });
                        }
                    });
                    if let Some(p) = payload
                        && p.0.status_id.as_str() != col.id
                        && open
                        && !waiting.contains(p.0.id.as_str())
                    {
                        dropped = Some((p.0.clone(), col.id.clone()));
                    }
                }
            });
        });
    if let Some((c, to)) = dropped {
        actions.push(Action::Move(c, to));
    }
}

fn feature_detail(
    ui: &mut Ui,
    work: &mut WorkUi,
    f: &WorkRecord,
    cols: &[Column],
    project_repos: &[String],
    enabled: bool,
    actions: &mut Vec<Action>,
) {
    egui::ScrollArea::vertical().id_salt("feature-detail").show(ui, |ui| {
        ui.label(RichText::new(&f.title.0).strong());
        widgets::caption(ui, format!("Revision {} · {}", f.revision.as_str(), status_name(cols, f.status_id.as_str())));
        if !f.description.0.is_empty() {
            ui.label(&f.description.0);
        }
        ui.add_space(6.0);
        // Approval and readiness are separate facts, shown apart.
        chip(ui, approval_label(f), if f.approval.is_some() { Tone::Good } else { Tone::Attention });
        chip(
            ui,
            readiness_label(f),
            if f.readiness.as_ref().is_some_and(|r| r.ready) { Tone::Good } else { Tone::Attention },
        );
        ui.add_space(6.0);
        // Status (the feature's workflow column; not its execution).
        let mut target = f.status_id.as_str().to_owned();
        egui::ComboBox::from_id_salt(("feature-status", f.id.as_str()))
            .selected_text(status_name(cols, &target))
            .show_ui(ui, |ui| {
                for c in cols {
                    ui.selectable_value(&mut target, c.id.clone(), &c.name);
                }
            })
            .response
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, enabled, "Feature status"));
        if target != f.status_id.as_str() && enabled {
            actions.push(Action::Command(
                f.id.as_str().to_owned(),
                "features.edit",
                json!({"workId": f.id, "expectedRevision": f.revision, "changes": {"statusId": target}}),
            ));
        }
        edit_section(ui, work, f, enabled, actions);
        ui.add_space(8.0);
        ui.label(RichText::new("Plan").strong());
        match (&f.plan, &f.approval) {
            (Some(_), Some(a)) => widgets::caption(ui, format!("Active plan (approved, revision {})", a.plan_revision.as_str())),
            _ => widgets::caption(ui, "No approved plan yet: implementation is not authorized."),
        }
        for r in &f.requirements.0 {
            ui.label(format!("{}: {}", r.id.as_str(), r.statement.0));
        }
        for a in &f.acceptance.0 {
            ui.label(format!("Acceptance {}: {}", a.id.as_str(), a.text.0));
        }
        if !f.repository_ids.0.is_empty() {
            widgets::caption(
                ui,
                format!(
                    "Repositories: {}",
                    f.repository_ids.0.iter().map(Id::as_str).collect::<Vec<_>>().join(", ")
                ),
            );
        }
        if let Some(p) = &f.pending_plan {
            ui.add_space(4.0);
            ui.label(RichText::new(format!("Pending revision {}", p.revision.as_str())).strong());
            for r in &p.draft.requirements.0 {
                ui.label(format!("{}: {}", r.id.as_str(), r.statement.0));
            }
            let requested = p
                .approval
                .as_ref()
                .is_some_and(|a| matches!(a.state, ApprovalState::Pending));
            if requested {
                widgets::caption(ui, "Approval requested: decide it in Approvals.");
            } else if ui
                .add_enabled(enabled, egui::Button::new("Request approval"))
                .clicked()
            {
                actions.push(Action::RequestApproval(f.clone()));
            }
        }
        plan_form(ui, work, f, project_repos, enabled, actions);
        ui.add_space(8.0);
        ui.label(RichText::new("Readiness").strong());
        if let Some(p) = &f.pending_readiness {
            let open = p
                .findings
                .0
                .iter()
                .filter(|x| x.blocking && matches!(x.state, FindingState::Open))
                .count();
            widgets::caption(
                ui,
                format!(
                    "The pending draft (plan revision {}) is assessed by {}: {open} open blocking finding(s). It gates nothing until the draft is approved.",
                    p.plan_revision.as_str(),
                    p.assessed_by.0.as_str()
                ),
            );
        }
        match &f.readiness {
            None => widgets::caption(ui, "No readiness assessment yet. The coordinator assesses readiness before implementation."),
            Some(r) => {
                widgets::caption(
                    ui,
                    format!(
                        "Assessed by {} for plan revision {}{} · {}",
                        r.assessed_by.0.as_str(),
                        r.plan_revision.as_str(),
                        if r.reviewed_pending { " (pending draft)" } else { "" },
                        match r.freshness {
                            ReadinessFreshness::Current => "current",
                            ReadinessFreshness::Stale => "stale for changed scope",
                        }
                    ),
                );
                if !r.summary.0.is_empty() {
                    ui.label(&r.summary.0);
                }
                // Remaining findings together, then settled ones.
                for open in [true, false] {
                    for x in r.findings.0.iter().filter(|x| matches!(x.state, FindingState::Open) == open) {
                        egui::Frame::new()
                            .stroke(theme::soft_line())
                            .corner_radius(theme::radius())
                            .inner_margin(theme::margin(8, 6))
                            .show(ui, |ui| {
                                ui.label(RichText::new(format!(
                                    "{} · {} · {}{}",
                                    x.id.as_str(),
                                    match x.disposition {
                                        ReadinessDisposition::ResolvedDecision => "Resolved decision",
                                        ReadinessDisposition::BoundedInvestigation => "Bounded investigation",
                                        ReadinessDisposition::NeedsUserDecision => "Needs your decision",
                                    },
                                    match x.state {
                                        FindingState::Open => "open",
                                        FindingState::Resolved => "resolved",
                                        FindingState::RiskAccepted => "risk accepted",
                                    },
                                    if x.blocking { " · blocking" } else { "" }
                                )).strong());
                                ui.label(&x.summary.0);
                                if !x.requirement_ids.0.is_empty() {
                                    widgets::caption(ui, format!("Requirements: {}", x.requirement_ids.0.iter().map(Id::as_str).collect::<Vec<_>>().join(", ")));
                                }
                                if !x.affected_work_ids.0.is_empty() {
                                    widgets::caption(ui, format!("Gates {} item(s) of work", x.affected_work_ids.0.len()));
                                }
                                for (l, v) in [("Missing", &x.missing.0), ("Consequence", &x.consequence.0), ("Why not resolved autonomously", &x.insufficiency.0), ("Closes when", &x.closure_condition.0)] {
                                    if !v.is_empty() {
                                        widgets::caption(ui, format!("{l}: {v}"));
                                    }
                                }
                                for a in &x.attempts.0 {
                                    widgets::caption(ui, format!("Tried: {}", a.0));
                                }
                                for (l, v) in [("Decision", &x.decision), ("Objective", &x.objective), ("Limits", &x.limits), ("Question", &x.question), ("Recommendation", &x.recommendation), ("Closure", &x.closure_note)] {
                                    if let Some(v) = v {
                                        widgets::caption(ui, format!("{l}: {}", v.0));
                                    }
                                }
                                if let Some(c) = &x.confidence {
                                    widgets::caption(ui, format!("Confidence: {c:?}"));
                                }
                                if !x.evidence.0.is_empty() || !x.closure_evidence.0.is_empty() {
                                    widgets::caption(ui, format!("Evidence: {} item(s)", x.evidence.0.len() + x.closure_evidence.0.len()));
                                }
                            });
                    }
                }
            }
        }
        ui.add_space(8.0);
        let tasks = work.cards.items.iter().filter(|c| c.parent_id.as_ref() == Some(&f.id)).count();
        widgets::caption(ui, format!("{tasks} task(s) on its board"));
        if ui.button("Show its activity").clicked() {
            actions.push(Action::ShowActivity(f.id.as_str().to_owned()));
        }
    });
}

fn edit_section(
    ui: &mut Ui,
    work: &mut WorkUi,
    w: &WorkRecord,
    enabled: bool,
    actions: &mut Vec<Action>,
) {
    let editing = work.edit.as_ref().is_some_and(|e| e.snapshot.id == w.id);
    if !editing {
        if ui.add_enabled(enabled, egui::Button::new("Edit")).clicked() {
            work.edit = Some(EditForm {
                snapshot: w.clone(),
                title: w.title.0.clone(),
                description: w.description.0.clone(),
                priority: w.priority,
            });
        }
        return;
    }
    let form = work.edit.as_mut().expect("editing");
    if form.snapshot.revision != w.revision {
        // The draft keeps the revision it was opened on: saving it is refused as a conflict.
        widgets::caption(
            ui,
            "This changed since you began editing; saving will be refused. Your draft is kept.",
        );
    }
    let r = ui.add(egui::TextEdit::singleline(&mut form.title));
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Work title"));
    let r = ui.add(egui::TextEdit::multiline(&mut form.description).desired_rows(3));
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Work description"));
    if !matches!(w.kind, WorkKind::Feature) {
        ui.horizontal(|ui| {
            ui.label("Priority");
            ui.add(egui::DragValue::new(&mut form.priority).range(-1000..=1000))
                .widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::SpinButton, true, "Priority")
                });
        });
    }
    let mut cancel = false;
    ui.horizontal(|ui| {
        if ui.add_enabled(enabled, egui::Button::new("Save")).clicked() {
            actions.push(Action::Edit(form.clone()));
        }
        if widgets::secondary_button(ui, "Cancel").clicked() {
            cancel = true;
        }
    });
    if cancel {
        work.edit = None;
    }
}

fn plan_form(
    ui: &mut Ui,
    work: &mut WorkUi,
    f: &WorkRecord,
    project_repos: &[String],
    enabled: bool,
    actions: &mut Vec<Action>,
) {
    let open = work.plan.as_ref().is_some_and(|p| p.snapshot.id == f.id);
    if !open {
        if ui
            .add_enabled(enabled, egui::Button::new("Edit plan"))
            .clicked()
        {
            work.plan = Some(open_plan_form(f));
        }
        return;
    }
    let form = work.plan.as_mut().expect("open");
    widgets::caption(
        ui,
        "Saving makes this the pending revision; the active plan stays in force until it is approved.",
    );
    let r = ui.add(
        egui::TextEdit::multiline(&mut form.text)
            .hint_text("Plan text (leave empty to keep the current text)")
            .desired_rows(4),
    );
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Plan text"));
    requirement_editor(ui, form);
    entry_list(ui, "Acceptance criterion", "A", &mut form.acceptance, &[]);
    entry_list(ui, "Acceptance test", "T", &mut form.acceptance_tests, &[]);
    ui.label(RichText::new("Goals").strong());
    let mut remove: Option<usize> = None;
    for (i, g) in form.goals.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            let r = ui.add(egui::TextEdit::multiline(g).desired_rows(1));
            r.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::TextInput, true, format!("Goal {}", i + 1))
            });
            if ui.small_button(format!("Remove goal {}", i + 1)).clicked() {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        form.goals.remove(i);
    }
    if ui.small_button("Add goal").clicked() {
        form.goals.push(String::new());
    }
    for repo in project_repos {
        let mut on = form.repositories.contains(repo);
        if ui.checkbox(&mut on, repo).changed() {
            if on {
                form.repositories.insert(repo.clone());
            } else {
                form.repositories.remove(repo);
            }
        }
    }
    if form.text.is_empty() && form.content.is_some() {
        form.original = String::new();
    }
    let mut cancel = false;
    let error = plan_error(form);
    if let Some(error) = &error {
        widgets::caption(ui, error);
    }
    ui.horizontal(|ui| {
        let ok =
            enabled && error.is_none() && (form.content.is_some() || !form.text.trim().is_empty());
        if ui
            .add_enabled(ok, egui::Button::new("Save plan draft"))
            .clicked()
        {
            actions.push(Action::SavePlan(form.clone()));
        }
        if widgets::secondary_button(ui, "Cancel").clicked() {
            cancel = true;
        }
    });
    if cancel {
        work.plan = None;
    }
}

/// Refuse unsupported omissions before submitting, preserving the whole draft.
pub fn plan_error(form: &PlanForm) -> Option<String> {
    if form.snapshot.approval.is_some() {
        for r in &form.snapshot.requirements.0 {
            match form.requirements.iter().find(|e| e.id == r.id.as_str()) {
                None if !form
                    .base
                    .requirements
                    .iter()
                    .any(|b| b.id == r.id && matches!(b.status, RequirementStatus::Dropped)) =>
                {
                    return Some(format!(
                        "Requirement {} belongs to the approved plan; drop it with a decision.",
                        r.id.as_str()
                    ));
                }
                Some(e) if e.text.trim().is_empty() && !form.drops.contains_key(&e.id) => {
                    return Some(format!(
                        "Requirement {} needs its statement, or an explicit drop decision.",
                        r.id.as_str()
                    ));
                }
                _ => {}
            }
        }
    }
    form.drops
        .iter()
        .find(|(_, d)| d.artifact.is_none() && d.reason.trim().is_empty())
        .map(|(id, _)| format!("Give a decision to drop requirement {id}."))
}

fn requirement_editor(ui: &mut Ui, form: &mut PlanForm) {
    ui.label(RichText::new("Requirements").strong());
    let approved: BTreeSet<_> = if form.snapshot.approval.is_some() {
        form.snapshot
            .requirements
            .0
            .iter()
            .map(|r| r.id.as_str().to_owned())
            .collect()
    } else {
        BTreeSet::new()
    };
    let mut remove = None;
    for (i, entry) in form.requirements.iter_mut().enumerate() {
        let dropped = form.drops.contains_key(&entry.id);
        let previous_id = entry.id.clone();
        ui.horizontal(|ui| {
            let r = ui.add_enabled(
                !approved.contains(&entry.id) && !dropped,
                egui::TextEdit::singleline(&mut entry.id).desired_width(56.0),
            );
            r.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::Role::TextInput,
                    !approved.contains(&entry.id) && !dropped,
                    format!("Requirement {} ID", i + 1),
                )
            });
            if dropped {
                if ui
                    .small_button(format!("Keep requirement {}", entry.id))
                    .clicked()
                {
                    form.drops.remove(&entry.id);
                }
            } else if approved.contains(&entry.id) {
                if ui
                    .small_button(format!("Drop requirement {}", entry.id))
                    .clicked()
                {
                    let artifact = form
                        .base
                        .requirements
                        .iter()
                        .find(|r| {
                            r.id.as_str() == entry.id
                                && matches!(r.status, RequirementStatus::Dropped)
                        })
                        .and_then(|r| r.drop_decision.clone());
                    form.drops.insert(
                        entry.id.clone(),
                        DropDraft {
                            reason: String::new(),
                            artifact,
                        },
                    );
                }
            } else if ui
                .small_button(format!("Remove requirement {}", i + 1))
                .clicked()
            {
                remove = Some(i);
            }
        });
        let r = ui.add_enabled(
            !form.drops.contains_key(&entry.id),
            egui::TextEdit::multiline(&mut entry.text)
                .desired_rows(2)
                .desired_width(f32::INFINITY),
        );
        r.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::Role::TextInput,
                !form.drops.contains_key(&entry.id),
                format!("Requirement {} text", i + 1),
            )
        });
        if previous_id != entry.id
            && let Some(status) = form.statuses.remove(&previous_id)
        {
            form.statuses.insert(entry.id.clone(), status);
        }
        if !form.drops.contains_key(&entry.id) {
            let original = form
                .base
                .requirements
                .iter()
                .find(|r| r.id.as_str() == entry.id);
            let status = form
                .statuses
                .get(&entry.id)
                .cloned()
                .or_else(|| original.map(|r| r.status.clone()))
                .unwrap_or(RequirementStatus::Confirmed);
            let mut proposed = matches!(
                status,
                RequirementStatus::Proposed | RequirementStatus::Dropped
            );
            // A restoration whose old status is no longer known defaults to Proposed and
            // visibly offers both choices; it never silently promotes an old proposal.
            if matches!(status, RequirementStatus::Dropped)
                && let Some(r) = form.snapshot.requirements.0.iter().find(|r| {
                    r.id.as_str() == entry.id && !matches!(r.status, RequirementStatus::Dropped)
                })
            {
                proposed = matches!(r.status, RequirementStatus::Proposed);
            }
            let before = proposed;
            egui::ComboBox::from_id_salt(("requirement-status", &entry.id))
                .selected_text(if proposed { "Proposed" } else { "Confirmed" })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut proposed, true, "Proposed");
                    ui.selectable_value(&mut proposed, false, "Confirmed");
                })
                .response
                .widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::Role::ComboBox,
                        true,
                        format!("Status of requirement {}", entry.id),
                    )
                });
            if before != proposed {
                form.statuses.insert(
                    entry.id.clone(),
                    if proposed {
                        RequirementStatus::Proposed
                    } else {
                        RequirementStatus::Confirmed
                    },
                );
            }
        }
        if let Some(drop) = form.drops.get_mut(&entry.id) {
            if drop.artifact.is_some() {
                widgets::caption(
                    ui,
                    "The recorded decision is preserved unless you write a replacement.",
                );
            }
            let r = ui.add(
                egui::TextEdit::multiline(&mut drop.reason)
                    .desired_rows(2)
                    .hint_text("Why is this requirement being dropped?"),
            );
            r.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::Role::TextInput,
                    true,
                    format!("Decision to drop {}", entry.id),
                )
            });
            if r.changed() {
                drop.artifact = if drop.reason.trim().is_empty() {
                    form.base
                        .requirements
                        .iter()
                        .find(|r| {
                            r.id.as_str() == entry.id
                                && matches!(r.status, RequirementStatus::Dropped)
                        })
                        .and_then(|r| r.drop_decision.clone())
                } else {
                    None
                };
            }
        }
    }
    if let Some(i) = remove {
        let e = form.requirements.remove(i);
        form.drops.remove(&e.id);
        form.statuses.remove(&e.id);
    }
    let hidden: Vec<_> = form
        .base
        .requirements
        .iter()
        .filter(|r| {
            matches!(r.status, RequirementStatus::Dropped)
                && !form.requirements.iter().any(|e| e.id == r.id.as_str())
        })
        .cloned()
        .collect();
    if !hidden.is_empty() {
        ui.collapsing("Dropped requirements", |ui| {
            for r in hidden {
                ui.label(format!("{}: {}", r.id.as_str(), r.statement.0));
                if ui
                    .small_button(format!("Restore requirement {}", r.id.as_str()))
                    .clicked()
                {
                    form.requirements.push(Entry {
                        id: r.id.as_str().to_owned(),
                        text: r.statement.0,
                    });
                }
            }
        });
    }
    if !approved.is_empty() {
        widgets::caption(
            ui,
            "Approved requirement IDs stay stable. Dropping one records a decision with the pending draft.",
        );
    }
    if ui.small_button("Add requirement").clicked() {
        let reserved: Vec<_> = form
            .base
            .requirements
            .iter()
            .map(|r| r.id.as_str().to_owned())
            .collect();
        let id = next_id(&form.requirements, "E", &reserved);
        form.requirements.push(Entry {
            id,
            text: String::new(),
        });
    }
}

/// One change to a card's blockers.
#[derive(Clone, Copy, Debug)]
pub enum BlockerEdit<'a> {
    Add(&'a str),
    Remove(&'a str),
}

/// The `tasks.dependency_links` body applying one blocker change to `w`, at its revision. The
/// operation replaces the whole incoming list, so the change is applied to the record's
/// `dependencyIds`, which is complete: no writer stores more than [`MAX_PREREQUISITES`], the
/// projection's own bound. `None` when the change changes nothing or would exceed the bound.
pub fn blocker_change(w: &WorkRecord, edit: BlockerEdit<'_>) -> Option<Value> {
    let mut ids: Vec<&str> = w.dependency_ids.0.iter().map(|d| d.as_str()).collect();
    match edit {
        BlockerEdit::Add(a) if !ids.contains(&a) && ids.len() < MAX_PREREQUISITES => ids.push(a),
        BlockerEdit::Remove(r) if ids.contains(&r) => ids.retain(|d| *d != r),
        _ => return None,
    }
    let edges: Vec<Value> = ids
        .iter()
        .map(|p| json!({"predecessor": p, "successor": w.id}))
        .collect();
    Some(json!({"taskId": w.id, "expectedRevision": w.revision, "replacement": {"edges": edges}}))
}

/// An editable list of entries, one ID field and one multi-line text field each, with add and
/// remove controls.
fn entry_list(
    ui: &mut Ui,
    what: &str,
    prefix: &str,
    entries: &mut Vec<Entry>,
    reserved: &[String],
) {
    ui.label(RichText::new(format!("{what}s")).strong());
    let mut remove: Option<usize> = None;
    for (i, e) in entries.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut e.id).desired_width(56.0));
            r.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::Role::TextInput,
                    true,
                    format!("{what} {} ID", i + 1),
                )
            });
            if ui
                .small_button(format!("Remove {} {}", what.to_lowercase(), i + 1))
                .clicked()
            {
                remove = Some(i);
            }
        });
        let r = ui.add(
            egui::TextEdit::multiline(&mut e.text)
                .desired_rows(2)
                .desired_width(f32::INFINITY),
        );
        r.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::Role::TextInput,
                true,
                format!("{what} {} text", i + 1),
            )
        });
    }
    if let Some(i) = remove {
        entries.remove(i);
    }
    if ui
        .small_button(format!("Add {}", what.to_lowercase()))
        .clicked()
    {
        let id = next_id(entries, prefix, reserved);
        entries.push(Entry {
            id,
            text: String::new(),
        });
    }
}

fn card_detail(
    ui: &mut Ui,
    work: &mut WorkUi,
    w: &WorkRecord,
    cols: &[Column],
    enabled: bool,
    actions: &mut Vec<Action>,
) {
    let titles: BTreeMap<String, (String, String)> = work
        .cards
        .items
        .iter()
        .map(|c| {
            (
                c.id.as_str().to_owned(),
                (c.title.0.clone(), c.status_id.as_str().to_owned()),
            )
        })
        .collect();
    let features: Vec<(String, String)> = work
        .features
        .items
        .iter()
        .map(|f| (f.id.as_str().to_owned(), f.title.0.clone()))
        .collect();
    egui::ScrollArea::vertical().id_salt("card-detail").show(ui, |ui| {
        ui.label(RichText::new(&w.title.0).strong());
        widgets::caption(ui, format!("{} · revision {} · priority {}", kind_label(w), w.revision.as_str(), w.priority));
        if !w.description.0.is_empty() {
            ui.label(&w.description.0);
        }
        // Workflow status: moving it never starts, pauses or completes execution.
        let mut target = w.status_id.as_str().to_owned();
        egui::ComboBox::from_id_salt(("card-status", w.id.as_str()))
            .selected_text(status_name(cols, &target))
            .show_ui(ui, |ui| {
                for c in cols {
                    ui.selectable_value(&mut target, c.id.clone(), &c.name);
                }
            })
            .response
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, enabled, "Status"));
        if target != w.status_id.as_str() && enabled {
            actions.push(Action::Move(w.clone(), target));
        }
        let semantic = semantic_of(cols, w.status_id.as_str());
        if semantic == "Done" {
            let verified = !w.acceptance.0.is_empty()
                && w.acceptance.0.iter().all(|c| matches!(c.state, CriterionState::Met) && !c.evidence_ids.0.is_empty());
            chip(
                ui,
                if verified { "Completed with verified acceptance evidence" } else { "Completed manually (no verified evidence)" },
                if verified { Tone::Good } else { Tone::Attention },
            );
        }
        edit_section(ui, work, w, enabled, actions);
        ui.add_space(6.0);
        // Membership: link, transfer or remove (remove pauses).
        let current = w
            .parent_id
            .as_ref()
            .and_then(|p| features.iter().find(|(id, _)| id == p.as_str()))
            .map_or("No feature (standalone issue)".to_owned(), |(_, t)| t.clone());
        let mut chosen = w.parent_id.as_ref().map(|p| p.as_str().to_owned());
        egui::ComboBox::from_id_salt(("card-feature", w.id.as_str()))
            .selected_text(current)
            .show_ui(ui, |ui| {
                for (id, t) in &features {
                    ui.selectable_value(&mut chosen, Some(id.clone()), t);
                }
            })
            .response
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, enabled, "Feature membership"));
        if chosen != w.parent_id.as_ref().map(|p| p.as_str().to_owned()) && enabled
            && let Some(f) = chosen
        {
            actions.push(Action::Command(
                w.id.as_str().to_owned(),
                "tasks.set_feature",
                json!({"taskId": w.id, "expectedRevision": w.revision, "featureId": f, "clearFeature": false}),
            ));
        }
        if w.parent_id.is_some()
            && ui.add_enabled(enabled, egui::Button::new("Remove from feature…")).clicked()
        {
            work.unlink = Some(w.clone());
        }
        ui.add_space(6.0);
        // Blocking links (one stored edge, both views).
        ui.label(RichText::new("Blocked by").strong());
        if w.dependency_ids.0.is_empty() {
            widgets::caption(ui, "Nothing blocks it.");
        }
        for d in &w.dependency_ids.0 {
            let (t, s) = titles.get(d.as_str()).cloned().unwrap_or((d.as_str().to_owned(), String::new()));
            ui.horizontal(|ui| {
                ui.label(format!("{t} ({})", status_name(cols, &s)));
                if ui.add_enabled(enabled, egui::Button::new(format!("Remove blocker {}", short(&t, 16))).small()).clicked()
                    && let Some(body) = blocker_change(w, BlockerEdit::Remove(d.as_str()))
                {
                    actions.push(Action::Command(w.id.as_str().to_owned(), "tasks.dependency_links", body));
                }
            });
        }
        let mut add: Option<String> = None;
        let full = w.dependency_ids.0.len() >= MAX_PREREQUISITES;
        if full {
            widgets::caption(ui, "It has the most blockers an item can have; remove one to add another.");
        }
        egui::ComboBox::from_id_salt(("add-blocker", w.id.as_str()))
            .selected_text("Add a blocker…")
            .show_ui(ui, |ui| {
                for (id, (t, _)) in &titles {
                    if id != w.id.as_str() && !w.dependency_ids.0.iter().any(|d| d.as_str() == id) {
                        ui.selectable_value(&mut add, Some(id.clone()), t);
                    }
                }
            })
            .response
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, enabled && !full, "Add a blocker"));
        if let Some(a) = add
            && enabled
            && let Some(body) = blocker_change(w, BlockerEdit::Add(&a))
        {
            actions.push(Action::Command(w.id.as_str().to_owned(), "tasks.dependency_links", body));
        }
        ui.label(RichText::new("Blocks").strong());
        if w.dependent_ids.0.is_empty() {
            widgets::caption(ui, "It blocks nothing.");
        }
        for d in &w.dependent_ids.0 {
            let (t, _) = titles.get(d.as_str()).cloned().unwrap_or((d.as_str().to_owned(), String::new()));
            ui.label(t);
        }
        ui.add_space(6.0);
        ui.label(RichText::new("Acceptance").strong());
        if w.acceptance.0.is_empty() {
            widgets::caption(ui, "No acceptance criteria.");
        }
        for c in &w.acceptance.0 {
            let state = match (&c.state, c.evidence_ids.0.is_empty()) {
                (CriterionState::Met, false) => "met with evidence",
                (CriterionState::Met, true) => "met (manual)",
                _ => "unmet",
            };
            ui.label(format!("{}: {} · {state}", c.id.as_str(), c.text.0));
        }
        ui.add_space(6.0);
        ui.label(RichText::new("Agents").strong());
        match &w.assigned_agent_id {
            Some(a) => ui.label(format!("Assigned to {}", a.as_str())),
            None => ui.label("No agent assigned."),
        };
        if !w.attempts.0.is_empty() {
            widgets::caption(ui, "An agent is working on it now.");
        }
        // Run controls of a root (a standalone issue, or a card with controls of its own).
        if let Some(s) = work.runs.get(w.id.as_str()).cloned() {
            let held = s.blockers.items.iter().any(|b| wops::manual_hold(&b.cause));
            widgets::caption(ui, format!(
                "Execution: {:?}{}",
                s.root.desired_state,
                if held { " · paused until you continue it" } else { "" }
            ));
            ui.horizontal(|ui| {
                if w.parent_id.is_none() && ui.add_enabled(enabled, egui::Button::new("Start")).clicked() {
                    actions.push(Action::Run("runs.start", w.clone()));
                }
                if ui.add_enabled(enabled, egui::Button::new("Pause")).clicked() {
                    actions.push(Action::Run("runs.pause", w.clone()));
                }
                if ui.add_enabled(enabled && held, egui::Button::new("Continue")).clicked() {
                    actions.push(Action::Run("runs.continue", w.clone()));
                }
            });
        } else if w.parent_id.is_none() && ui.button("Show run controls").clicked() {
            actions.push(Action::Inspect(w.id.as_str().to_owned()));
        }
        ui.add_space(6.0);
        if ui.button("Show its history").clicked() {
            actions.push(Action::ShowActivity(w.id.as_str().to_owned()));
        }
        let history: Vec<&Event> = work
            .activity
            .items
            .iter()
            .rev()
            .filter(|e| matches!(&e.aggregate, EventSubject::Root(r) if r.0 == w.id))
            .take(10)
            .collect();
        for e in history {
            widgets::caption(ui, format!("#{} by {}", e.sequence.as_str(), e.actor_id.0.as_str()));
        }
    });
}
