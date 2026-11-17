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
use super::ListState;
use crate::backend::{Reply, TaskOutput, is_unknown, resolve_command};
use crate::ops::{self, Failure};
use crate::route::Route;
use crate::screen::{Cx, Effect, Screen, ScreenContext, ScreenId, controller};
use crate::widgets::{self, Tone, chip};
use crate::work_ops as wops;
use crate::work_ops::WorkSlot;
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::{
    AcceptanceCriterion, LifeState, RequirementItem, RequirementStatus, RetainedMessage,
    RetainedMessageEvent, RootSnapshot, WorkKind, WorkRecord,
};
use devworks_core::protocol::{DomainErrorCode, Event};
pub(crate) mod workflow;
pub use crate::work_screen::activity::{activity_line, event_summary};
pub use crate::work_screen::detail::{BlockerEdit, blocker_change, plan_error};
pub use crate::work_screen::{adjacent_move, next_id};
use devworks_core::protocol::records::{
    AllocationClaimRecord, AllocationOutcome, AllocationQueueSnapshot, AllocationRecovery,
    AllocationSelection, StatusRecord,
};
use eframe::egui::{self, RichText, Ui};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use workflow::{WorkflowAction, WorkflowUi};

/// Feature list tabs and Issues tabs.
pub const FEATURE_TABS: [&str; 2] = ["List", "Board"];
pub const ISSUE_TABS: [&str; 2] = ["List", "History"];
pub const OVERVIEW_TABS: [&str; 3] = ["Summary", "Activity", "Messages"];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageDraft {
    pub body: String,
    pub kind: usize,
    pub to_session: String,
    pub to_human: bool,
    pub item: String,
    pub feature: String,
    pub resource: String,
    pub blocking: bool,
    pub reply_to: String,
    pub resolution: usize,
}

#[derive(Default)]
pub struct MessageBoard {
    pub project: Option<String>,
    pub history: Vec<RetainedMessage>,
    pub history_before: Option<i64>,
    pub history_loaded: bool,
    pub history_refresh_pending: bool,
    pub inbox: Vec<RetainedMessage>,
    pub inbox_cursor: Option<String>,
    pub inbox_loaded: bool,
    pub inbox_refresh_pending: bool,
    pub inbox_has_more: bool,
    pub events: Vec<RetainedMessageEvent>,
    pub event_after: Option<i64>,
    pub event_poll_at: Option<std::time::Instant>,
    pub draft: MessageDraft,
    pub sending: Option<MessageDraft>,
    pub error: Option<String>,
}

/// One board column: a status of the project workflow.
#[derive(Clone, Debug)]
pub struct Column {
    pub id: String,
    pub name: String,
    pub semantic: String,
}
/// Columns are exactly the project's backend workflow, in its stored order.
pub fn columns(statuses: &[StatusRecord]) -> Vec<Column> {
    statuses
        .iter()
        .map(|status| Column {
            id: status.id.as_str().to_owned(),
            name: status.label.0.clone(),
            semantic: status.category.0.clone(),
        })
        .collect()
}
pub(crate) fn semantic_of(cols: &[Column], status: &str) -> String {
    cols.iter()
        .find(|c| c.id == status)
        .map(|c| c.semantic.clone())
        .unwrap_or_default()
}
pub(crate) fn finished(semantic: &str) -> bool {
    semantic == "Done" || semantic == "Closed"
}
pub(crate) fn status_name(cols: &[Column], status: &str) -> String {
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
    pub task_type: Option<String>,
    pub labels: Vec<String>,
    pub budget: crate::usage_screen::BudgetDraft,
}
/// An edit of one item's title, description and priority, against the record it was opened on.
#[derive(Clone, Debug)]
pub struct EditForm {
    pub snapshot: WorkRecord,
    pub title: String,
    pub description: String,
    pub priority: i32,
    pub task_type: Option<String>,
    pub labels: Vec<String>,
    pub budget: crate::usage_screen::BudgetDraft,
    pub budget_changed: bool,
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
    /// None withdraws a proposal present in the starting snapshot; Some proposes an exact cap.
    #[serde(default)]
    pub budget: Option<crate::usage_screen::BudgetDraft>,
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
                && l.priority == saved.priority
                && l.task_type == saved.task_type
                && l.labels == saved.labels
                && l.budget == saved.budget
                && l.budget_changed == saved.budget_changed;
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
    a.budget == b.budget
        && a.text == b.text
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
        budget: f
            .pending_plan
            .as_ref()
            .and_then(|pending| pending.budget.as_ref())
            .map(crate::usage_screen::BudgetDraft::from_input),
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
/// A bounded newest-first project or card feed. Refresh re-reads only the number of
/// pages the user requested, publishing the replacement once the bounded refresh finishes.
#[derive(Default)]
pub struct Activity {
    /// Loaded events, newest first.
    pub items: Vec<Event>,
    /// Where the next read starts (`None`: the beginning).
    pub resume: Option<String>,
    /// Whether the initial page has been read.
    pub loaded: bool,
    pub more: bool,
    pub loading: bool,
    pub generation: u64,
    pub pages: usize,
    refresh: Option<HistoryRefresh>,
    pub error: Option<String>,
    /// 0 everything, 1 features, 2 issues, 3 tasks, 4 the project itself.
    pub kind: usize,
    /// 0 everyone, 1 you, 2 agents and others.
    pub actor: usize,
    pub text: String,
    /// Only this work item (from a detail's history link).
    pub item: Option<String>,
}

struct HistoryRefresh {
    target: usize,
    items: Vec<Event>,
    pages: usize,
}
impl Activity {
    fn refresh(&mut self, generation: u64) {
        self.generation = generation;
        self.loading = true;
        self.error = None;
        self.refresh = Some(HistoryRefresh {
            target: self.pages.max(1),
            items: Vec::new(),
            pages: 0,
        });
    }
    fn receive(&mut self, items: Vec<Event>, cursor: Option<String>, more: bool) -> Option<String> {
        self.loading = false;
        self.error = None;
        let append = |target: &mut Vec<Event>, items: Vec<Event>| {
            let mut seen: BTreeSet<_> = target
                .iter()
                .map(|e| e.event_id.0.as_str().to_owned())
                .collect();
            target.extend(
                items
                    .into_iter()
                    .filter(|e| seen.insert(e.event_id.0.as_str().to_owned())),
            );
        };
        if let Some(refresh) = &mut self.refresh {
            append(&mut refresh.items, items);
            refresh.pages += 1;
            if more && refresh.pages < refresh.target {
                self.loading = true;
                return cursor;
            }
            let refresh = self.refresh.take().unwrap();
            self.items = refresh.items;
            self.pages = refresh.pages;
        } else {
            append(&mut self.items, items);
            self.pages += 1;
        }
        self.resume = cursor;
        self.more = more;
        self.loaded = true;
        None
    }
    fn failed(&mut self, message: String) {
        self.loading = false;
        self.refresh = None;
        self.error = Some(message);
    }
}

#[derive(Default)]
pub struct WorkUi {
    pub messages: MessageBoard,
    pub histories: BTreeMap<String, Activity>,
    history_serial: u64,

    /// Workflow drafts and inspected claims survive project switches; records remain authoritative.
    pub workflows: BTreeMap<String, WorkflowUi>,
    pub allocation_claims: BTreeMap<String, AllocationClaimRecord>,
    pub allocation_recovery: BTreeMap<String, AllocationRecovery>,
    pub allocation_reused: BTreeSet<String>,
    pub allocation_inspect_errors: BTreeMap<String, String>,
    pub allocation_queues: BTreeMap<(String, Option<String>), AllocationQueueSnapshot>,
    pub allocation_busy: BTreeSet<(String, Option<String>)>,
    pub allocation_filters: BTreeMap<String, workflow::AllocationFilters>,
    pub allocation_requests: BTreeMap<(String, Option<String>), workflow::AllocationFilters>,
    pub allocation_repositories: Vec<(String, String)>,
    pub claim_lease_ms: Option<String>,
    pub inspect_claim_id: String,
    /// The project the lists were read for.
    pub of: Option<String>,
    pub features: ListState<WorkRecord>,
    pub cards: ListState<WorkRecord>,
    staged_features: Vec<WorkRecord>,
    staged_cards: Vec<WorkRecord>,
    pub activity: Activity,
    /// Current records of opened features (readiness at schema revision 2).
    pub detail: BTreeMap<String, WorkRecord>,
    pub git: BTreeMap<String, ListState<devworks_core::protocol::records::AttributedGitLink>>,
    /// Pages of a source-control re-read, shown only once complete: the list on screen stays
    /// until then, so a refresh never shifts the controls beneath it.
    git_reread: BTreeMap<String, Vec<devworks_core::protocol::records::AttributedGitLink>>,
    pub runs: BTreeMap<String, RootSnapshot>,
    pub git_inspections:
        BTreeMap<String, Result<devworks_core::protocol::records::GitInspection, String>>,
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
    /// Plan saves the last close kept, until they are restored.
    restoring_plans: Vec<PlanForm>,
    /// Features whose `features.edit_plan` was restored unconfirmed.
    confirming_plans: Vec<String>,
}

impl WorkUi {
    pub(super) fn drop_purged(
        &mut self,
        scope: &devworks_core::ports::storage::ProjectPurgeClientScope,
    ) {
        let project = scope.project_id.0.as_str();
        if self.messages.project.as_deref() == Some(project) {
            self.messages = MessageBoard::default();
        }
        let owned = |id: &str| scope.work_ids.iter().any(|i| i.as_str() == id);
        self.histories.retain(|id, _| !owned(id));
        self.workflows.remove(project);
        let claims = scope
            .record_ids
            .get(&devworks_core::ports::storage::ProjectRecordKind::AllocationClaims)
            .cloned()
            .unwrap_or_default();
        self.allocation_claims
            .retain(|id, _| !claims.iter().any(|c| c.as_str() == id));
        self.allocation_recovery
            .retain(|id, _| !claims.iter().any(|c| c.as_str() == id));
        self.allocation_reused
            .retain(|id| !claims.iter().any(|c| c.as_str() == id));
        self.allocation_inspect_errors
            .retain(|id, _| !claims.iter().any(|c| c.as_str() == id));

        self.allocation_filters.remove(project);
        self.allocation_requests.retain(|(p, _), _| p != project);
        self.allocation_queues.retain(|(p, _), _| p != project);
        self.allocation_busy.retain(|(p, _)| p != project);
        self.detail.retain(|id, _| !owned(id));
        self.git.retain(|id, _| !owned(id));
        self.git_reread.retain(|id, _| !owned(id));
        self.runs.retain(|id, _| !owned(id));
        self.git_inspections.retain(|id, _| !owned(id));
        self.features
            .items
            .retain(|r| r.project_id != scope.project_id);
        self.cards
            .items
            .retain(|r| r.project_id != scope.project_id);
        self.staged_features
            .retain(|r| r.project_id != scope.project_id);
        self.staged_cards
            .retain(|r| r.project_id != scope.project_id);
        self.create_saving.remove(project);
        self.edit = self
            .edit
            .take()
            .filter(|f| f.snapshot.project_id != scope.project_id);
        self.plan = self
            .plan
            .take()
            .filter(|f| f.snapshot.project_id != scope.project_id);
        self.plan_saving.retain(|id, _| !owned(id));
        self.edit_saving.retain(|id, _| !owned(id));
        self.moving.retain(|id, _| !owned(id));
        self.unlink = self
            .unlink
            .take()
            .filter(|r| r.project_id != scope.project_id);
        if self.board.as_deref().is_some_and(owned) {
            self.board = None;
        }
        if self.card.as_deref().is_some_and(owned) {
            self.card = None;
        }
        if self.of.as_deref() == Some(project) {
            self.of = None;
            self.create = None;
            self.activity = Default::default();
            self.message = None;
        }
        self.forget_archive_count(project);
    }

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
            (Some(d), Some(l)) if l.revision == d.revision => {
                let mut newest = d.clone();
                // Membership changes can advance readiness without changing the feature's
                // work revision. Keep full cached detail, but take the newer assessment slots.
                let readiness_revision = |w: &WorkRecord| {
                    w.readiness
                        .iter()
                        .chain(&w.pending_readiness)
                        .map(|r| r.revision.get())
                        .max()
                        .unwrap_or(0)
                };
                if readiness_revision(l) > readiness_revision(d) {
                    newest.readiness = l.readiness.clone();
                    newest.pending_readiness = l.pending_readiness.clone();
                }
                // A replayed older-schema receipt can omit metadata at the same work revision.
                // Keep the fields already observed by the current-schema listing.
                if newest.labels.is_none() && l.labels.is_some() {
                    newest.labels = l.labels.clone();
                    newest.task_type = l.task_type.clone();
                }
                Some(newest)
            }
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
pub(crate) enum Action {
    RefreshMessages,
    PollMessages,
    MoreInbox,
    OlderMessages,
    PostMessage(MessageDraft),
    AckMessage(String, Option<&'static str>),
    OlderHistory(Option<String>),
    RefreshHistory(Option<String>),

    Reload,
    Workflow(WorkflowAction),
    Allocate(Option<String>, u64),
    InspectClaim(String),
    ReleaseClaim(String),
    OpenWork(String),
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
    RefreshGit(String),
    InspectGit(String, String),
    OpenGitSession(String),
}

pub(crate) fn kind_label(w: &WorkRecord) -> &'static str {
    match w.kind {
        WorkKind::Feature => "Feature",
        WorkKind::Task => "Task",
        WorkKind::Issue => "Issue",
    }
}
pub(crate) fn task_type_of(work: &WorkRecord) -> Option<String> {
    work.task_type.as_ref().map(|kind| {
        match kind {
            devworks_core::protocol::records::WorkItemType::Story => "Story",
            devworks_core::protocol::records::WorkItemType::Task => "Task",
            devworks_core::protocol::records::WorkItemType::Bug => "Bug",
        }
        .to_owned()
    })
}
pub(crate) fn labels_of(work: &WorkRecord) -> Vec<String> {
    work.labels
        .as_ref()
        .map(|labels| labels.0.iter().map(|label| label.0.clone()).collect())
        .unwrap_or_default()
}

controller!(WorkCtl, work: WorkUi);

impl Screen for WorkUi {
    type Slot = WorkSlot;
    const ID: ScreenId = ScreenId::Work;
    fn load(&mut self, cx: &mut Cx, _route: Route) {
        cx.load_projects();
        WorkCtl { work: self, cx }.load_work();
    }
    /// Changes an agent made reach the work screens (the lists are read in full again). A list
    /// shown stays until its reply replaces it, so a refresh never moves what the user clicks.
    fn refresh(&mut self, cx: &mut Cx, _route: Route) {
        let mut ctl = WorkCtl { work: self, cx };
        if !ctl.work_loading() {
            ctl.load_work();
        }
    }
    fn apply(&mut self, cx: &mut Cx, slot: WorkSlot, reply: Reply) {
        if let Some((slot, reply)) = (WorkCtl { work: self, cx }).apply_work(slot, reply) {
            cx.unhandled(&slot, reply);
        }
    }
    fn ui(&mut self, cx: &mut Cx, route: Route, ui: &mut Ui) {
        let mut ctl = WorkCtl { work: self, cx };
        match route {
            Route::Features => ctl.features_screen(ui),
            Route::Issues => ctl.issues_screen(ui),
            _ => ctl.overview_screen(ui),
        }
    }
    /// Every plan save still running, kept as the snapshot it submitted: the next launch
    /// finishes each one.
    fn pending_memory(&self, _unresolved: &[&crate::backend::Unresolved]) -> Option<Value> {
        let pending = crate::memory::WorkPendingMemory {
            plans: self
                .plan_saving
                .values()
                .filter_map(|f| serde_json::to_value(f).ok())
                .collect(),
        };
        serde_json::to_value(pending).ok()
    }
    fn restore_pending(&mut self, saved: &Value) {
        if let Ok(pending) =
            serde_json::from_value::<crate::memory::WorkPendingMemory>(saved.clone())
        {
            self.restoring_plans = pending
                .plans
                .into_iter()
                .filter_map(|v| serde_json::from_value(v).ok())
                .collect();
        }
    }
    /// A plan save whose `features.edit_plan` is unconfirmed waits for its confirmation.
    fn restore_in_flight(&mut self, slot: &WorkSlot, _u: &crate::backend::Unresolved) {
        if let WorkSlot::WorkPlan(id) = slot {
            self.confirming_plans.push(id.clone());
        }
    }
    fn restored(&mut self, cx: &mut Cx) {
        let saves = std::mem::take(&mut self.restoring_plans);
        let confirming = std::mem::take(&mut self.confirming_plans);
        WorkCtl { work: self, cx }.restore_plan_saves(saves, &confirming);
    }
    /// The unsaved create, edit, plan, allocation filter and workflow drafts, by project.
    fn purge_live(&self, out: &mut crate::project_purge::Live) {
        let mut put = |project: &str, kind: &str, value: Value| {
            out.projects
                .entry(project.into())
                .or_insert_with(|| json!({}))[kind] = value;
        };
        if let Some(p) = &self.of
            && let Some(f) = &self.create
        {
            put(
                p,
                "createWork",
                json!({"kind":f.kind,"title":f.title,"description":f.description,"labels":f.labels}),
            );
        }
        if let Some(f) = &self.edit {
            put(
                f.snapshot.project_id.0.as_str(),
                "editWork",
                json!({"snapshot":f.snapshot,"title":f.title,"description":f.description,"labels":f.labels}),
            );
        }
        if let Some(f) = &self.plan {
            put(f.snapshot.project_id.0.as_str(), "plan", json!(f));
        }
        for (p, f) in &self.allocation_filters {
            put(p, "allocation", json!(format!("{f:?}")));
        }
        for (p, w) in &self.workflows {
            if let Some(d) = &w.draft {
                put(p, "workflow", json!(format!("{d:?}")));
            }
        }
    }
    fn remove_purged(&mut self, scope: &devworks_core::ports::storage::ProjectPurgeClientScope) {
        self.drop_purged(scope);
    }
    fn context(&self, _route: Route, mem: &crate::memory::RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: mem.tab,
            selection: mem.selection.clone(),
        }
    }
}

/// What the shell asks of the work screens.
impl WorkUi {
    pub(crate) fn load_work(&mut self, cx: &mut Cx) {
        WorkCtl { work: self, cx }.load_work();
    }
    pub(crate) fn open_work(&mut self, cx: &mut Cx, id: &str) {
        WorkCtl { work: self, cx }.open_work(id);
    }
    pub(crate) fn count_archive_work(&mut self, cx: &mut Cx, project: &str) {
        WorkCtl { work: self, cx }.count_archive_work(project);
    }
}

impl WorkCtl<'_, '_> {
    fn route_memory(&self, route: Route) -> crate::memory::RouteMemory {
        self.cx.routes.get(&route).cloned().unwrap_or_default()
    }
    pub(crate) fn selected_project_record(
        &self,
    ) -> Option<&devworks_core::protocol::records::ProjectRecord> {
        let id = self.cx.selected_project.as_deref()?;
        self.cx
            .projects
            .list
            .items
            .iter()
            .find(|p| p.id.0.as_str() == id)
    }
    /// Selects `project` in the navigation. Another project's lists are dropped first, so what
    /// the caller then opens (a board, a card) is kept when the selection reloads them.
    fn select_project(&mut self, project: Option<String>) {
        if self.cx.selected_project == project {
            return;
        }
        if let Some(p) = &project {
            self.reset_for(Some(p.clone()));
        }
        self.cx.selected_project = project.clone();
        self.cx.effects.push(Effect::SelectProject(project));
    }
    pub(crate) fn watch_act(&mut self, actions: Vec<crate::watchers_screen::WatchAction>) {
        if !actions.is_empty() {
            self.cx.effects.push(Effect::Watch(actions));
        }
    }
}

impl WorkCtl<'_, '_> {
    // -----------------------------------------------------------------------------------------
    // Requests

    /// Reads the selected project's features, cards and Activity (every page).
    pub(super) fn load_work(&mut self) {
        // Saves still running belong to their items, not to the project shown: a project
        // switch keeps them, so each reply still finishes its own save.
        let Some(p) = self.selected_project.clone() else {
            self.reset_for(None);
            return;
        };
        if self.work.of.as_deref() != Some(p.as_str()) {
            self.reset_for(Some(p.clone()));
        }
        self.work.allocation_repositories = self
            .selected_project_record()
            .map(|project| {
                project
                    .repository_ids
                    .0
                    .iter()
                    .map(|id| {
                        let label = self
                            .cx
                            .projects
                            .repositories
                            .items
                            .iter()
                            .find(|r| r.id == *id)
                            .map(|r| r.name.0.clone())
                            .unwrap_or_else(|| id.as_str().to_owned());
                        (id.as_str().to_owned(), label)
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.load_workflow(&p, false);
        self.load_allocation_queue(&p, None, None);
        if let Some(feature) = self.work.board.clone() {
            self.load_allocation_queue(&p, Some(feature), None);
        }
        self.refresh_allocation_claims();
        self.work.list_at = self.work.mutations;
        self.work.staged_features.clear();
        self.work.staged_cards.clear();
        let (a, b) = (p.clone(), p.clone());
        self.query(
            WorkSlot::WorkFeatures(p.clone()),
            "Loading features",
            "features.list",
            move |be| wops::work_list(be, "features.list", &a, None),
        );
        self.query(
            WorkSlot::WorkCards(p.clone()),
            "Loading tasks and issues",
            "tasks.list",
            move |be| wops::work_list(be, "tasks.list", &b, None),
        );
        // Derived reverse links have no independent revision on their prerequisite. Reload the
        // visible detail too; the client fences any older reply for the same detail slot.
        let mem = self.route_memory(self.route());
        let selected = match self.route() {
            Route::Features if mem.tab.min(1) == 1 => {
                self.work.card.clone().or_else(|| self.work.board.clone())
            }
            Route::Features | Route::Issues => mem.selection,
            _ => None,
        };
        if let Some(w) = selected.and_then(|id| self.record(&id)) {
            self.load_detail(&w);
        }
        // Initial activity loads one newest page.
        if !self.work.activity.loaded && !self.work.activity.loading {
            self.load_activity();
        }
    }
    /// Drops what the lists of another project showed, keeping the saves still running and
    /// what survives a project switch.
    fn reset_for(&mut self, project: Option<String>) {
        *self.work = WorkUi {
            history_serial: self.work.history_serial,
            of: project,
            workflows: std::mem::take(&mut self.work.workflows),
            allocation_claims: std::mem::take(&mut self.work.allocation_claims),
            allocation_recovery: std::mem::take(&mut self.work.allocation_recovery),
            allocation_reused: std::mem::take(&mut self.work.allocation_reused),
            allocation_inspect_errors: std::mem::take(&mut self.work.allocation_inspect_errors),
            allocation_queues: std::mem::take(&mut self.work.allocation_queues),
            allocation_busy: std::mem::take(&mut self.work.allocation_busy),
            allocation_filters: std::mem::take(&mut self.work.allocation_filters),
            allocation_requests: std::mem::take(&mut self.work.allocation_requests),
            claim_lease_ms: std::mem::take(&mut self.work.claim_lease_ms),
            inspect_claim_id: std::mem::take(&mut self.work.inspect_claim_id),
            archive_counts: std::mem::take(&mut self.work.archive_counts),
            plan_saving: std::mem::take(&mut self.work.plan_saving),
            edit_saving: std::mem::take(&mut self.work.edit_saving),
            create_saving: std::mem::take(&mut self.work.create_saving),
            ..WorkUi::default()
        };
    }
    /// Whether a work list read is in flight.
    pub(super) fn work_loading(&self) -> bool {
        self.work.of.as_ref().is_some_and(|p| {
            self.pending(&WorkSlot::WorkFeatures(p.clone()))
                || self.pending(&WorkSlot::WorkCards(p.clone()))
        })
    }
    fn history_feed(&mut self, root: Option<&str>) -> &mut Activity {
        match root {
            Some(root) => self.work.histories.entry(root.to_owned()).or_default(),
            None => &mut self.work.activity,
        }
    }
    fn request_history(&mut self, root: Option<String>, cursor: Option<String>, generation: u64) {
        let Some(project) = self.work.of.clone() else {
            return;
        };
        let requested = root.clone();
        let submitted = self.query(
            WorkSlot::WorkActivity(project.clone(), root.clone(), generation),
            "Loading history",
            "history.events",
            move |backend| {
                wops::history_events(backend, &project, requested.as_deref(), cursor.as_deref())
            },
        );
        if !submitted {
            self.history_feed(root.as_deref())
                .failed("History is unavailable while closing or offline.".into());
        }
    }
    fn refresh_history(&mut self, root: Option<String>) {
        if self.work.of.is_none() {
            return;
        }
        self.work.history_serial += 1;
        let generation = self.work.history_serial;
        self.history_feed(root.as_deref()).refresh(generation);
        self.request_history(root, None, generation);
    }
    fn older_history(&mut self, root: Option<String>) {
        let feed = self.history_feed(root.as_deref());
        if feed.loading || !feed.more {
            return;
        }
        let Some(cursor) = feed.resume.clone() else {
            return;
        };
        self.work.history_serial += 1;
        let generation = self.work.history_serial;
        let feed = self.history_feed(root.as_deref());
        feed.generation = generation;
        feed.loading = true;
        self.request_history(root, Some(cursor), generation);
    }
    fn load_activity(&mut self) {
        self.refresh_history(None);
    }
    fn load_messages(&mut self, reset: bool) {
        let Some(project) = self.selected_project.clone() else {
            return;
        };
        if reset || self.work.messages.project.as_deref() != Some(project.as_str()) {
            let same_project = self.work.messages.project.as_deref() == Some(project.as_str());
            let history_refresh_pending =
                same_project && self.pending(&WorkSlot::WorkMessagesHistory(project.clone()));
            let inbox_refresh_pending =
                same_project && self.pending(&WorkSlot::WorkMessagesInbox(project.clone()));
            let (draft, events, event_after, event_poll_at) = if same_project {
                (
                    self.work.messages.draft.clone(),
                    std::mem::take(&mut self.work.messages.events),
                    self.work.messages.event_after,
                    self.work.messages.event_poll_at,
                )
            } else {
                (MessageDraft::default(), Vec::new(), None, None)
            };
            // A refresh keeps the messages on screen until its replies replace them: emptying
            // the board meanwhile would remove the controls a click in that frame was aimed at.
            let (history, inbox) = if same_project {
                (
                    std::mem::take(&mut self.work.messages.history),
                    std::mem::take(&mut self.work.messages.inbox),
                )
            } else {
                (Vec::new(), Vec::new())
            };
            self.work.messages = MessageBoard {
                project: Some(project.clone()),
                history,
                inbox,
                draft,
                events,
                event_after,
                event_poll_at,
                history_refresh_pending,
                inbox_refresh_pending,
                ..MessageBoard::default()
            };
        }
        let history_before = self.work.messages.history_before;
        let event_after = self.work.messages.event_after;
        // Subscribe from the current event head before reading the initial inbox/history.
        // Otherwise a post between those reads and the subscription can be missed forever.
        if event_after.is_none() {
            if !self.pending(&WorkSlot::WorkMessagesEvents(project.clone())) {
                let p = project.clone();
                self.query(
                    WorkSlot::WorkMessagesEvents(project),
                    "Reading message changes",
                    "agents.events",
                    move |be| wops::message_events(be, &p, None),
                );
            }
            return;
        }
        if !self.pending(&WorkSlot::WorkMessagesHistory(project.clone())) {
            let p = project.clone();
            self.query(
                WorkSlot::WorkMessagesHistory(project.clone()),
                "Reading messages",
                "agents.history",
                move |be| wops::message_history(be, &p, history_before),
            );
        }
        if !self.pending(&WorkSlot::WorkMessagesInbox(project.clone())) {
            let p = project.clone();
            self.command(
                WorkSlot::WorkMessagesInbox(project.clone()),
                "Reading inbox",
                "agents.receive",
                move |be| wops::message_inbox(be, &p, None),
            );
        }
    }
    fn older_messages(&mut self) {
        let Some(project) = self.selected_project.clone() else {
            return;
        };
        let Some(before) = self.work.messages.history_before else {
            return;
        };
        if self.pending(&WorkSlot::WorkMessagesHistory(project.clone())) {
            return;
        }
        let p = project.clone();
        self.query(
            WorkSlot::WorkMessagesHistory(project),
            "Reading older messages",
            "agents.history",
            move |be| wops::message_history(be, &p, Some(before)),
        );
    }
    fn load_detail(&mut self, w: &WorkRecord) {
        if !self
            .work
            .histories
            .get(w.id.as_str())
            .is_some_and(|feed| feed.loading)
        {
            self.refresh_history(Some(w.id.as_str().to_owned()));
        }
        self.load_git_links(w.id.as_str(), None);
        let op = if matches!(w.kind, WorkKind::Feature) {
            "features.get"
        } else {
            "tasks.get"
        };
        let id = w.id.as_str().to_owned();
        self.query(
            WorkSlot::WorkDetail(id.clone()),
            "Reading work",
            op,
            move |be| wops::work_get(be, op, &id),
        );
    }
    fn load_git_links(&mut self, work: &str, cursor: Option<String>) {
        if cursor.is_none() {
            // A first read shows its spinner; a re-read keeps the shown list until it is done.
            self.work.git.entry(work.to_owned()).or_default();
            self.work.git_reread.insert(work.to_owned(), Vec::new());
        }
        let root = work.to_owned();
        self.query(
            WorkSlot::WorkGit(root.clone()),
            "Reading source control",
            "history.git_links",
            move |be| wops::git_links(be, &root, cursor.as_deref()),
        );
    }
    fn inspect(&mut self, root: &str) {
        let r = root.to_owned();
        self.query(
            WorkSlot::WorkInspect(root.to_owned()),
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
            WorkSlot::WorkOpen(id.to_owned(), false),
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
                || self.pending(&WorkSlot::ArchiveCount(key.0.clone(), features))
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
                WorkSlot::ArchiveCount(project.to_owned(), features),
                "Counting running work",
                op,
                move |be| wops::work_list(be, op, &p, None),
            );
        }
    }
    fn record(&self, id: &str) -> Option<WorkRecord> {
        self.work.freshest(id)
    }
    pub(crate) fn project_open(&self) -> bool {
        self.selected_project_record()
            .is_some_and(|p| matches!(p.lifecycle, LifeState::Active))
    }

    pub(crate) fn work_act(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::RefreshMessages => self.load_messages(true),
                Action::MoreInbox => {
                    let Some(project) = self.selected_project.clone() else {
                        continue;
                    };
                    let Some(cursor) = self.work.messages.inbox_cursor.clone() else {
                        continue;
                    };
                    if self.work.messages.project.as_deref() != Some(project.as_str())
                        || !self.work.messages.inbox_has_more
                        || self.pending(&WorkSlot::WorkMessagesInbox(project.clone()))
                    {
                        continue;
                    }
                    let p = project.clone();
                    self.command(
                        WorkSlot::WorkMessagesInbox(project),
                        "Reading more messages",
                        "agents.receive",
                        move |be| wops::message_inbox(be, &p, Some(&cursor)),
                    );
                }
                Action::PollMessages => {
                    let Some(project) = self.selected_project.clone() else {
                        continue;
                    };
                    if self.work.messages.project.as_deref() != Some(project.as_str())
                        || self.pending(&WorkSlot::WorkMessagesEvents(project.clone()))
                    {
                        continue;
                    }
                    self.work.messages.event_poll_at =
                        Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
                    let p = project.clone();
                    let after = self.work.messages.event_after;
                    self.query(
                        WorkSlot::WorkMessagesEvents(project),
                        "Reading message changes",
                        "agents.events",
                        move |be| wops::message_events(be, &p, after),
                    );
                }
                Action::OlderMessages => self.older_messages(),
                Action::PostMessage(draft) => {
                    let Some(project) = self.selected_project.clone() else {
                        continue;
                    };
                    if self.work.messages.project.as_deref() != Some(project.as_str()) {
                        continue;
                    }
                    if draft.body.trim().is_empty()
                        || self.pending(&WorkSlot::WorkMessagePost(project.clone()))
                    {
                        continue;
                    }
                    let body = message_post_body(&draft);
                    let p = project.clone();
                    if self.command(
                        WorkSlot::WorkMessagePost(project),
                        "Posting message",
                        "agents.post",
                        move |be| wops::message_post(be, &p, body),
                    ) {
                        self.work.messages.sending = Some(draft);
                    }
                }
                Action::AckMessage(id, resolution) => {
                    let Some(project) = self.selected_project.clone() else {
                        continue;
                    };
                    if self.work.messages.project.as_deref() != Some(project.as_str()) {
                        continue;
                    }
                    if self.pending(&WorkSlot::WorkMessageAck(project.clone(), id.clone())) {
                        continue;
                    }
                    let p = project.clone();
                    let key = id.clone();
                    self.command(
                        WorkSlot::WorkMessageAck(project, id),
                        "Acknowledging message",
                        "agents.ack",
                        move |be| wops::message_ack(be, &p, &key, resolution),
                    );
                }
                Action::OlderHistory(root) => self.older_history(root),
                Action::RefreshHistory(root) => self.refresh_history(root),

                Action::Workflow(action) => self.workflow_action(action),
                Action::Allocate(feature, lease) => {
                    if let Some(project) = self.work.of.clone() {
                        self.load_allocation_queue(&project, feature, Some(lease));
                    }
                }
                Action::InspectClaim(claim) => self.inspect_allocation(&claim),
                Action::ReleaseClaim(claim) => self.release_allocation(&claim),
                Action::OpenWork(id) => self.open_work(&id),
                Action::Reload => {
                    self.load_work();
                    self.refresh_history(self.work.activity.item.clone());
                }
                Action::Create(form) => {
                    let Some(p) = self.work.of.clone() else {
                        continue;
                    };
                    let Some(backlog) = self
                        .work
                        .workflows
                        .get(&p)
                        .filter(|w| w.loaded)
                        .and_then(|w| w.rows.iter().find(|s| s.category.0 == "Backlog"))
                        .map(|s| s.id.as_str().to_owned())
                    else {
                        self.work.message = Some((
                            Tone::Attention,
                            "Wait for the project's workflow before creating work.".into(),
                        ));
                        continue;
                    };
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
                    let mut body = body;
                    let metadata = if form.kind == "feature" {
                        &mut body["creation"]
                    } else {
                        &mut body
                    };
                    metadata["labels"] = json!(form.labels);
                    if form.kind == "feature" {
                        match form.budget.input() {
                            Ok(input) => metadata["budget"] = input,
                            Err(error) => {
                                self.work.message = Some((Tone::Bad, error));
                                continue;
                            }
                        }
                    }
                    if form.kind != "feature"
                        && let Some(kind) = &form.task_type
                    {
                        metadata["taskType"] = json!(kind);
                    }
                    if self.command(
                        WorkSlot::WorkCreate(p.clone()),
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
                    if form.labels != labels_of(&w) {
                        changes["labels"] = json!(form.labels);
                    }
                    if !feature && form.task_type != task_type_of(&w) {
                        if let Some(kind) = &form.task_type {
                            changes["taskType"] = json!(kind);
                        } else {
                            changes["clearTaskType"] = json!(true);
                        }
                    }
                    if feature && form.budget_changed {
                        match form.budget.input() {
                            Ok(input) => changes["budget"] = input,
                            Err(error) => {
                                self.work.message = Some((Tone::Bad, error));
                                continue;
                            }
                        }
                    }
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
                        WorkSlot::WorkCommand(w.id.as_str().to_owned()),
                        "Saving work",
                        op,
                        move |be| wops::work_command(be, op, body),
                    );
                }
                Action::Command(id, op, body) => {
                    self.command(WorkSlot::WorkCommand(id), "Changing work", op, move |be| {
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
                        WorkSlot::WorkCommand(id.clone()),
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
                    self.command(WorkSlot::WorkOrder(key), "Reordering work", op, move |be| {
                        wops::work_command(be, op, body)
                    });
                }
                Action::SavePlan(form) => self.save_plan(form),
                Action::ShowActivity(id) => {
                    if !self
                        .work
                        .histories
                        .get(&id)
                        .is_some_and(|feed| feed.loaded || feed.loading)
                    {
                        self.refresh_history(Some(id.clone()));
                    }
                    self.work.activity.item = Some(id);
                    // Overview's second tab is Activity.
                    self.routes.entry(Route::Overview).or_default().tab = 1;
                    self.navigate(Route::Overview);
                }
                Action::RequestApproval(w) => {
                    let Some(pending) = w.pending_plan.clone() else {
                        continue;
                    };
                    let Some(digest) = pending.plan_digest.as_ref() else {
                        self.work.message=Some((Tone::Attention,"Refresh the feature to read its captured pending plan token before requesting approval.".into()));
                        continue;
                    };
                    let body = json!({"featureId": w.id, "expectedRevision": w.revision,
                        "planRevision": pending.revision, "planDigest": digest});
                    self.command(
                        WorkSlot::WorkCommand(w.id.as_str().to_owned()),
                        "Requesting plan approval",
                        "features.request_plan_approval",
                        move |be| wops::work_command(be, "features.request_plan_approval", body),
                    );
                }
                Action::InspectGit(repository, tree) => {
                    self.query(
                        WorkSlot::WorkGitInspect(repository.clone(), tree.clone()),
                        "Inspecting checkout",
                        "git.inspect",
                        move |be| ops::git_inspect_worktree(be, &repository, Some(&tree)),
                    );
                }
                Action::RefreshGit(root) => self.load_git_links(&root, None),
                Action::OpenGitSession(session) => {
                    self.cx.effects.push(Effect::OpenGitSession(session));
                }
                Action::Inspect(root) => self.inspect(&root),
                Action::Run(op, w) => {
                    let Some(snapshot) = self.work.runs.get(w.id.as_str()).cloned() else {
                        self.inspect(w.id.as_str());
                        continue;
                    };
                    let id = w.id.as_str().to_owned();
                    self.command(
                        WorkSlot::WorkRun(id),
                        "Changing run controls",
                        op,
                        move |be| wops::run_control(be, op, &w, &snapshot),
                    );
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
                WorkSlot::WorkPlanText(id.clone()),
                "Feature plan",
                form.text.clone(),
            )
        } else {
            (
                WorkSlot::WorkPlanDrop(id.clone()),
                "Requirement drop decisions",
                format!("# Requirement drop decisions\n\n{}", decisions.join("\n\n")),
            )
        };
        let started = self.cx.client.as_deref_mut().is_some_and(|c| {
            c.task(slot.into(), "Storing the plan and decisions", move |b| {
                let dir = b.scratch_dir();
                let path = b.stage_project_text(Some(&project), "md", text.as_bytes())?;
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
        let mut body = json!({"featureId": w.id, "expectedRevision": w.revision,
            "plan": plan_body(form, content)});
        if let Some(budget) = &form.budget {
            match budget.input() {
                Ok(input) => body["budget"] = input,
                Err(error) => {
                    self.work.message = Some((Tone::Bad, error));
                    return;
                }
            }
        } else if w
            .pending_plan
            .as_ref()
            .is_some_and(|pending| pending.budget.is_some())
        {
            body["clearBudgetProposal"] = json!(true);
        }
        self.command(
            WorkSlot::WorkPlan(w.id.as_str().to_owned()),
            "Saving the plan draft",
            "features.edit_plan",
            move |be| wops::work_command(be, "features.edit_plan", body),
        );
    }

    // -----------------------------------------------------------------------------------------
    // Replies

    /// Applies a reply for one of the work slots; returns it back when it is not one.
    pub(super) fn apply_work(&mut self, slot: WorkSlot, reply: Reply) -> Option<(WorkSlot, Reply)> {
        let (slot, reply) = self.apply_workflow(slot, reply)?;
        match (slot, reply) {
            (WorkSlot::WorkMessagesHistory(project), Reply::Query(reply)) => {
                if self.work.messages.project.as_deref() != Some(project.as_str()) {
                    return None;
                }
                if self.work.messages.history_refresh_pending {
                    self.work.messages.history_refresh_pending = false;
                    self.load_messages(false);
                    return None;
                }
                match wops::message_history_page(reply) {
                    Ok(page) => {
                        let board = &mut self.work.messages;
                        if !board.history_loaded {
                            board.history.clear();
                        }
                        for message in page.items.0 {
                            if !board.history.iter().any(|known| known.id == message.id) {
                                board.history.push(message);
                            }
                        }
                        board.history_before = page.next_before.map(|before| before.get());
                        board.history_loaded = true;
                        board.error = None;
                    }
                    Err(error) => self.work.messages.error = Some(error.describe()),
                }
            }
            (WorkSlot::WorkMessagesInbox(project), Reply::Command(reply)) => {
                if self.work.messages.project.as_deref() != Some(project.as_str()) {
                    return None;
                }
                if self.work.messages.inbox_refresh_pending {
                    self.work.messages.inbox_refresh_pending = false;
                    self.load_messages(false);
                    return None;
                }
                match wops::message_inbox_page(reply) {
                    Ok(page) => {
                        let board = &mut self.work.messages;
                        // The first page after a refresh replaces what was kept on screen.
                        if !board.inbox_loaded {
                            board.inbox.clear();
                        }
                        board.inbox_has_more = page.items.0.len() == 100;
                        for message in page.items.0 {
                            if let Some(known) =
                                board.inbox.iter_mut().find(|known| known.id == message.id)
                            {
                                *known = message;
                            } else {
                                board.inbox.push(message);
                            }
                        }
                        board.inbox_cursor = Some(page.next_cursor.0);
                        board.inbox_loaded = true;
                        board.error = None;
                    }
                    Err(error) => self.work.messages.error = Some(error.describe()),
                }
            }
            (WorkSlot::WorkMessagesEvents(project), Reply::Query(reply)) => {
                if self.work.messages.project.as_deref() != Some(project.as_str()) {
                    return None;
                }
                match wops::message_events_page(reply) {
                    Ok(page) => {
                        let board = &mut self.work.messages;
                        let changed = !page.items.0.is_empty();
                        for event in page.items.0 {
                            if !board
                                .events
                                .iter()
                                .any(|known| known.sequence == event.sequence)
                            {
                                board.events.push(event);
                            }
                        }
                        board.event_after = Some(page.next_sequence.get());
                        board.event_poll_at =
                            Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
                        board.error = None;
                        if changed {
                            self.load_messages(true);
                        } else if !board.history_loaded || !board.inbox_loaded {
                            self.load_messages(false);
                        }
                    }
                    Err(error) => {
                        self.work.messages.error = Some(error.describe());
                        self.work.messages.event_poll_at =
                            Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
                    }
                }
            }
            (WorkSlot::WorkMessagePost(project), Reply::Command(reply)) => {
                if self.work.messages.project.as_deref() != Some(project.as_str()) {
                    return None;
                }
                match wops::message_posted(reply) {
                    Ok(result) => {
                        if result.item_versions.is_some() {
                            self.load_work();
                        }
                        if self.work.messages.sending.as_ref() == Some(&self.work.messages.draft) {
                            self.work.messages.draft = MessageDraft::default();
                        }
                        self.work.messages.sending = None;
                        self.load_messages(true);
                    }
                    Err(error) => {
                        self.work.messages.sending = None;
                        self.work.messages.error = Some(error.describe());
                    }
                }
            }
            (WorkSlot::WorkMessageAck(project, id), Reply::Command(reply)) => {
                if self.work.messages.project.as_deref() != Some(project.as_str()) {
                    return None;
                }
                match wops::message_acked(reply) {
                    Ok(result) => {
                        if result.item_versions.is_some() {
                            self.load_work();
                        }
                        self.work
                            .messages
                            .inbox
                            .retain(|message| message.id.as_str() != id);
                        self.load_messages(true);
                    }
                    Err(error) => self.work.messages.error = Some(error.describe()),
                }
            }
            (WorkSlot::WorkFeatures(p) | WorkSlot::WorkCards(p), Reply::Query(_))
                if self.work.of.as_deref() != Some(p.as_str()) => {}
            (WorkSlot::WorkFeatures(_) | WorkSlot::WorkCards(_), Reply::Query(_))
                if self.work.list_at != self.work.mutations =>
            {
                self.load_work();
            }
            (WorkSlot::WorkFeatures(p), Reply::Query(q)) => match wops::work_page(q) {
                Ok((items, next)) => {
                    self.work.staged_features.extend(items);
                    match next {
                        Some(c) => {
                            self.query(
                                WorkSlot::WorkFeatures(p.clone()),
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
            (WorkSlot::WorkCards(p), Reply::Query(q)) => match wops::work_page(q) {
                Ok((items, next)) => {
                    self.work.staged_cards.extend(items);
                    match next {
                        Some(c) => {
                            self.query(
                                WorkSlot::WorkCards(p.clone()),
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
            (WorkSlot::WorkActivity(project, root, generation), Reply::Query(reply)) => {
                if self.work.of.as_deref() != Some(project.as_str()) {
                    return None;
                }
                let feed = self.history_feed(root.as_deref());
                if feed.generation != generation {
                    return None;
                }
                match wops::events_page(reply) {
                    Ok((items, cursor, more)) => {
                        if let Some(next) = feed.receive(items, cursor, more) {
                            self.request_history(root, Some(next), generation);
                        }
                    }
                    Err(error) => feed.failed(error.describe()),
                }
            }
            (WorkSlot::WorkGitInspect(_, tree), Reply::Query(q)) => {
                self.work
                    .git_inspections
                    .insert(tree, ops::git_inspected(q).map_err(|e| e.describe()));
            }
            (WorkSlot::WorkGit(id), Reply::Query(q)) => match wops::git_links_result(q) {
                Ok(page) => {
                    let pages = self.work.git_reread.entry(id.clone()).or_default();
                    pages.extend(page.items.0);
                    if let Some(cursor) = page.next_cursor {
                        self.load_git_links(&id, Some(cursor.0));
                    } else {
                        let items = self.work.git_reread.remove(&id).unwrap_or_default();
                        let state = self.work.git.entry(id).or_default();
                        state.items = items;
                        state.loaded = true;
                        state.error = None;
                        state.more = page.has_more;
                    }
                }
                Err(error) => {
                    self.work.git_reread.remove(&id);
                    self.work.git.entry(id).or_default().error = Some(error.describe());
                }
            },
            (WorkSlot::WorkDetail(id), Reply::Query(q)) => match wops::work_record(q) {
                Ok(w) => {
                    // A command can publish a newer approval while this read is in
                    // flight. Slot fencing orders reads, not reads against commands.
                    if self
                        .work
                        .detail
                        .get(&id)
                        .is_none_or(|known| w.revision.get() >= known.revision.get())
                    {
                        self.work.detail.insert(id, w);
                    }
                }
                Err(e) => self.work.message = Some((Tone::Bad, e.describe())),
            },
            (WorkSlot::WorkOpen(id, feature), Reply::Query(q)) => match wops::work_record(q) {
                Ok(w) => {
                    self.select_project(Some(w.project_id.0.as_str().to_owned()));
                    let key = w.id.as_str().to_owned();
                    if feature {
                        let view = self.routes.entry(Route::Features).or_default();
                        view.tab = 0;
                        view.selection = Some(key);
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
                        WorkSlot::WorkOpen(id.clone(), true),
                        "Opening work",
                        "features.get",
                        move |be| wops::work_get(be, "features.get", &id),
                    );
                }
                Err(e) => self.announce(format!("The work could not be opened: {}", e.describe())),
            },
            (WorkSlot::WorkInspect(id), Reply::Query(q)) => {
                if let Ok(s) = wops::run_snapshot(q) {
                    self.work.runs.insert(id, s);
                }
            }
            (WorkSlot::ArchiveCount(p, features), Reply::Query(q)) => {
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
                                    WorkSlot::ArchiveCount(p.clone(), features),
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
            (WorkSlot::WorkCreate(project), Reply::Command(c)) => match wops::work_changed(c) {
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
            (WorkSlot::WorkCommand(id), Reply::Command(c)) => {
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
            (WorkSlot::WorkOrder(_), Reply::Command(c)) => match wops::work_ordered(c) {
                Ok(_) => {
                    self.work.mutations += 1;
                    self.announce("The order was saved.");
                    self.load_work();
                }
                Err(e) => self.work_failure("Reordering", e),
            },
            (WorkSlot::WorkPlanDrop(id), Reply::Task(t)) => {
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
            (WorkSlot::WorkPlanText(id), Reply::Task(t)) => {
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
            (WorkSlot::WorkPlan(id), Reply::Command(c)) => {
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
            (WorkSlot::WorkRun(id), Reply::Command(c)) => {
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
}

pub(crate) fn devworks_desktop_user() -> String {
    crate::host::USER_ACTOR.to_owned()
}

fn message_post_body(draft: &MessageDraft) -> Value {
    let kind = ["Info", "Question", "Request", "Blocker", "Announcement"]
        .get(draft.kind)
        .copied()
        .unwrap_or("Info");
    let mut body = json!({"body":draft.body,"messageKind":kind});
    for (name, value) in [
        ("toSessionId", &draft.to_session),
        ("itemId", &draft.item),
        ("featureId", &draft.feature),
        ("resourceName", &draft.resource),
        ("inReplyToId", &draft.reply_to),
    ] {
        if !value.trim().is_empty() {
            body[name] = json!(value.trim());
        }
    }
    if draft.to_human {
        body["toHuman"] = json!(true);
    }
    if draft.blocking {
        body["blocking"] = json!(true);
    }
    if !draft.reply_to.trim().is_empty() {
        let resolution = match draft.resolution {
            1 => Some("Answered"),
            2 => Some("Declined"),
            _ => None,
        };
        if let Some(resolution) = resolution {
            body["resolves"] = json!(resolution);
        }
    }
    body
}
