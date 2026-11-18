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
use crate::frame;
use crate::ops::{self, Failure};
use crate::route::Route;
use crate::theme;
use crate::watch_ops::{WatchScope, WatchView};
use crate::widgets::{self, Header, Tone, ViewState, chip};
use crate::work_ops as wops;
use devworks_core::operations::{
    OperationInput as In, agents, features, issues, next_work, tasks, workflows,
};
use devworks_core::protocol::provider::{ArtifactRef, Digest};
use devworks_core::protocol::records::{
    AcceptanceCriterion, ApprovalState, CriterionState, FeatureCreation, FeaturePatch,
    FindingState, LifeState, PendingFeaturePlan, PlanDraft, QueuePlacement, ReadinessDisposition,
    RequirementItem, RequirementStatus, RetainedMessage, RetainedMessageEvent, RootSnapshot,
    TaskPatch, WorkKind, WorkRecord,
};
use devworks_core::protocol::support::{BoundedList, Label};
use devworks_core::protocol::{
    BlockerChange, ControlReason, DomainErrorCode, Event, EventBody, EventSubject,
};
use devworks_core::protocol::{Id, ProjectId, Revision, RootId};
use devworks_core::services::work::fold;
pub(crate) mod workflow;
pub use crate::work_screen::detail::{BlockerEdit, blocker_change, plan_error};
use crate::work_screen::detail::{card_detail, feature_detail};
pub use crate::work_screen::{adjacent_move, next_id};
use crate::work_screen::{board, create_form, feature_list, issue_list};
use devworks_core::protocol::records::{
    AllocationClaimRecord, AllocationOutcome, AllocationQueueSnapshot, AllocationRecovery,
    AllocationSelection, StatusRecord,
};
use eframe::egui::{self, RichText, Ui};
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
fn merged_criteria(
    entries: &[Entry],
    base: &[AcceptanceCriterion],
) -> Option<BoundedList<AcceptanceCriterion, 0, 128>> {
    frame::bounded(
        entries
            .iter()
            .filter(|e| !e.text.trim().is_empty())
            .map(|e| match base.iter().find(|b| b.id.as_str() == e.id) {
                Some(b) if b.text.0 == e.text => Some(b.clone()),
                _ => Some(AcceptanceCriterion {
                    id: frame::id(e.id.trim())?,
                    text: frame::text(&e.text)?,
                    evidence_ids: BoundedList(Vec::new()),
                    state: CriterionState::Unmet,
                }),
            })
            .collect::<Option<Vec<_>>>()?,
    )
}

/// The `features.edit_plan` draft of a plan form: what the user wrote, with what the form does
/// not show kept from the draft it started from — each requirement's status, drop decision and
/// covering tasks, dropped requirements (their decision stays recorded), and each unchanged
/// criterion's state and evidence. None when a value is outside its contract.
pub fn plan_body(form: &PlanForm, content: &ArtifactRef) -> Option<PlanDraft> {
    let written: Vec<&Entry> = form
        .requirements
        .iter()
        .filter(|e| !e.text.trim().is_empty() || form.drops.contains_key(&e.id))
        .collect();
    let mut reqs = written
        .iter()
        .map(|e| {
            Some(
                match form
                    .base
                    .requirements
                    .iter()
                    .find(|b| b.id.as_str() == e.id)
                {
                    Some(b) => {
                        let mut v = b.clone();
                        if !e.text.trim().is_empty() {
                            v.statement = frame::text(&e.text)?;
                        }
                        if let Some(status) = form.statuses.get(&e.id) {
                            v.status = status.clone();
                        }
                        if let Some(drop) = form.drops.get(&e.id) {
                            v.status = RequirementStatus::Dropped;
                            v.drop_decision = drop.artifact.clone();
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
                            v.status = form.statuses.get(&e.id).cloned().unwrap_or(status);
                            v.drop_decision = None;
                        }
                        v
                    }
                    None => RequirementItem {
                        id: frame::id(e.id.trim())?,
                        statement: frame::text(&e.text)?,
                        status: form
                            .statuses
                            .get(&e.id)
                            .cloned()
                            .unwrap_or(RequirementStatus::Confirmed),
                        drop_decision: None,
                        covered_by_task_ids: BoundedList(Vec::new()),
                    },
                },
            )
        })
        .collect::<Option<Vec<_>>>()?;
    for b in &form.base.requirements {
        if matches!(b.status, RequirementStatus::Dropped)
            && !written.iter().any(|e| e.id == b.id.as_str())
        {
            reqs.push(b.clone());
        }
    }
    let goals: Vec<String> = form
        .goals
        .iter()
        .filter(|g| !g.trim().is_empty())
        .cloned()
        .collect();
    let repositories: Vec<String> = form.repositories.iter().cloned().collect();
    Some(PlanDraft {
        content: content.clone(),
        acceptance: merged_criteria(&form.acceptance, &form.base.acceptance)?,
        repository_ids: frame::ids(&repositories)?,
        goals: frame::texts(&goals)?,
        requirements: frame::bounded(reqs)?,
        acceptance_tests: merged_criteria(&form.acceptance_tests, &form.base.acceptance_tests)?,
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
}

impl WorkUi {
    pub(super) fn remove_purged(
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
    /// A work command on one item.
    Command(String, In),
    Move(WorkRecord, String),
    /// Opens Overview's Activity with one work item's changes.
    ShowActivity(String),
    /// A queue order: operation, feature, priority, placement, expected revision.
    Order(
        &'static str,
        Option<String>,
        Option<i32>,
        QueuePlacement,
        String,
    ),
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

impl Shell {
    // -----------------------------------------------------------------------------------------
    // Requests

    /// Reads the selected project's features, cards and Activity (every page).
    pub(super) fn load_work(&mut self) {
        // Saves still running belong to their items, not to the project shown: a project
        // switch keeps them, so each reply still finishes its own save.
        let Some(p) = self.selected_project.clone() else {
            self.work = WorkUi {
                history_serial: self.work.history_serial,
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
            return;
        };
        if self.work.of.as_deref() != Some(p.as_str()) {
            self.work = WorkUi {
                history_serial: self.work.history_serial,
                of: Some(p.clone()),
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
        self.work.allocation_repositories = self
            .selected_project_record()
            .map(|project| {
                project
                    .repository_ids
                    .0
                    .iter()
                    .map(|id| {
                        let label = self
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
            Slot::WorkFeatures(p.clone()),
            "Loading features",
            "features.list",
            move |be| wops::work_list(be, wops::Family::Features, &a, None),
        );
        self.query(
            Slot::WorkCards(p.clone()),
            "Loading tasks and issues",
            "tasks.list",
            move |be| wops::work_list(be, wops::Family::Tasks, &b, None),
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
    /// Whether a work list read is in flight.
    pub(super) fn work_loading(&self) -> bool {
        self.work.of.as_ref().is_some_and(|p| {
            self.pending(&Slot::WorkFeatures(p.clone()))
                || self.pending(&Slot::WorkCards(p.clone()))
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
            Slot::WorkActivity(project.clone(), root.clone(), generation),
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
                same_project && self.pending(&Slot::WorkMessagesHistory(project.clone()));
            let inbox_refresh_pending =
                same_project && self.pending(&Slot::WorkMessagesInbox(project.clone()));
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
            if !self.pending(&Slot::WorkMessagesEvents(project.clone())) {
                let p = project.clone();
                self.query(
                    Slot::WorkMessagesEvents(project),
                    "Reading message changes",
                    "agents.events",
                    move |be| wops::message_events(be, &p, None),
                );
            }
            return;
        }
        if !self.pending(&Slot::WorkMessagesHistory(project.clone())) {
            let p = project.clone();
            self.query(
                Slot::WorkMessagesHistory(project.clone()),
                "Reading messages",
                "agents.history",
                move |be| wops::message_history(be, &p, history_before),
            );
        }
        if !self.pending(&Slot::WorkMessagesInbox(project.clone())) {
            let p = project.clone();
            self.command(
                Slot::WorkMessagesInbox(project.clone()),
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
        if self.pending(&Slot::WorkMessagesHistory(project.clone())) {
            return;
        }
        let p = project.clone();
        self.query(
            Slot::WorkMessagesHistory(project),
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
        let (op, family) = if matches!(w.kind, WorkKind::Feature) {
            ("features.get", wops::Family::Features)
        } else {
            ("tasks.get", wops::Family::Tasks)
        };
        let id = w.id.as_str().to_owned();
        self.query(
            Slot::WorkDetail(id.clone()),
            "Reading work",
            op,
            move |be| wops::work_get(be, family, &id),
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
            Slot::WorkGit(root.clone()),
            "Reading source control",
            "history.git_links",
            move |be| wops::git_links(be, &root, cursor.as_deref()),
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
            move |be| wops::work_get(be, wops::Family::Tasks, &w),
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
            let (op, family) = if features {
                ("features.list", wops::Family::Features)
            } else {
                ("issues.list", wops::Family::Issues)
            };
            self.query(
                Slot::ArchiveCount(project.to_owned(), features),
                "Counting running work",
                op,
                move |be| wops::work_list(be, family, &p, None),
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
                        || self.pending(&Slot::WorkMessagesInbox(project.clone()))
                    {
                        continue;
                    }
                    let p = project.clone();
                    self.command(
                        Slot::WorkMessagesInbox(project),
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
                        || self.pending(&Slot::WorkMessagesEvents(project.clone()))
                    {
                        continue;
                    }
                    self.work.messages.event_poll_at =
                        Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
                    let p = project.clone();
                    let after = self.work.messages.event_after;
                    self.query(
                        Slot::WorkMessagesEvents(project),
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
                        || self.pending(&Slot::WorkMessagePost(project.clone()))
                    {
                        continue;
                    }
                    let input = message_post_body(&draft, &project);
                    if self.command(
                        Slot::WorkMessagePost(project),
                        "Posting message",
                        "agents.post",
                        move |be| wops::message_post(be, input),
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
                    if self.pending(&Slot::WorkMessageAck(project.clone(), id.clone())) {
                        continue;
                    }
                    let p = project.clone();
                    let key = id.clone();
                    self.command(
                        Slot::WorkMessageAck(project, id),
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
                    let (op, input) = match create_request(&form, &p, &backlog) {
                        Ok(request) => request,
                        Err(error) => {
                            self.work.message = Some((Tone::Bad, error));
                            continue;
                        }
                    };
                    if self.command(
                        Slot::WorkCreate(p.clone()),
                        "Creating work",
                        op,
                        move |be| wops::work_command(be, input),
                    ) {
                        self.work.create_saving.insert(p, form);
                    }
                }
                Action::Edit(form) => {
                    let w = form.snapshot.clone();
                    let (op, input) = match edit_request(&form) {
                        Ok(Some(request)) => request,
                        Ok(None) => {
                            self.work.edit = None;
                            continue;
                        }
                        Err(error) => {
                            self.work.message = Some((Tone::Bad, error));
                            continue;
                        }
                    };
                    self.work.edit_saving.insert(w.id.as_str().to_owned(), form);
                    self.command(
                        Slot::WorkCommand(w.id.as_str().to_owned()),
                        "Saving work",
                        op,
                        move |be| wops::work_command(be, input),
                    );
                }
                Action::Command(id, input) => {
                    self.command(
                        Slot::WorkCommand(id),
                        "Changing work",
                        input.operation(),
                        move |be| wops::work_command(be, input),
                    );
                }
                Action::Move(w, status) => {
                    let id = w.id.as_str().to_owned();
                    let input = move_request(&w, &status);
                    // The marker shows the move as submitted; the card stays where it is until
                    // the outcome is confirmed.
                    if self.command(
                        Slot::WorkCommand(id.clone()),
                        "Moving a card",
                        "tasks.move",
                        move |be| wops::work_command(be, input),
                    ) {
                        self.work.moving.insert(id, status);
                    }
                }
                Action::Order(op, feature, priority, placement, expected) => {
                    let Some(p) = self.work.of.clone() else {
                        continue;
                    };
                    // One bounded placement, whatever the bucket's size.
                    let input =
                        order_request(op, &p, feature.as_deref(), priority, placement, &expected);
                    let key = format!("{p}/{}", feature.unwrap_or_default());
                    self.command(Slot::WorkOrder(key), "Reordering work", op, move |be| {
                        wops::work_command(be, input)
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
                    let input = approval_request(&w, &pending, digest);
                    self.command(
                        Slot::WorkCommand(w.id.as_str().to_owned()),
                        "Requesting plan approval",
                        "features.request_plan_approval",
                        move |be| wops::work_command(be, input),
                    );
                }
                Action::InspectGit(repository, tree) => {
                    self.query(
                        Slot::WorkGitInspect(repository.clone(), tree.clone()),
                        "Inspecting checkout",
                        "git.inspect",
                        move |be| ops::git_inspect_worktree(be, &repository, Some(&tree)),
                    );
                }
                Action::RefreshGit(root) => self.load_git_links(&root, None),
                Action::OpenGitSession(session) => {
                    self.chat.selected = Some(session.clone());
                    self.chat.notice = None;
                    self.load_transcript(&session);
                    self.navigate(Route::Chat);
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
        let input = match plan_request(form, content) {
            Ok(input) => input,
            Err(error) => {
                self.work.message = Some((Tone::Bad, error));
                return;
            }
        };
        self.command(
            Slot::WorkPlan(w.id.as_str().to_owned()),
            "Saving the plan draft",
            "features.edit_plan",
            move |be| wops::work_command(be, input),
        );
    }

    // -----------------------------------------------------------------------------------------
    // Replies

    /// Applies a reply for one of the work slots; returns it back when it is not one.
    pub(super) fn apply_work(&mut self, slot: Slot, reply: Reply) -> Option<(Slot, Reply)> {
        let (slot, reply) = self.apply_workflow(slot, reply)?;
        match (slot, reply) {
            (Slot::WorkMessagesHistory(project), Reply::Query(reply)) => {
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
            (Slot::WorkMessagesInbox(project), Reply::Command(reply)) => {
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
            (Slot::WorkMessagesEvents(project), Reply::Query(reply)) => {
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
            (Slot::WorkMessagePost(project), Reply::Command(reply)) => {
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
            (Slot::WorkMessageAck(project, id), Reply::Command(reply)) => {
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
                                move |be| wops::work_list(be, wops::Family::Features, &p, Some(&c)),
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
                                move |be| wops::work_list(be, wops::Family::Tasks, &p, Some(&c)),
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
            (Slot::WorkActivity(project, root, generation), Reply::Query(reply)) => {
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
            (Slot::WorkGitInspect(_, tree), Reply::Query(q)) => {
                self.work
                    .git_inspections
                    .insert(tree, ops::git_inspected(q).map_err(|e| e.describe()));
            }
            (Slot::WorkGit(id), Reply::Query(q)) => match wops::git_links_result(q) {
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
            (Slot::WorkDetail(id), Reply::Query(q)) => match wops::work_record(q) {
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
            (Slot::WorkOpen(id, feature), Reply::Query(q)) => match wops::work_record(q) {
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
                        Slot::WorkOpen(id.clone(), true),
                        "Opening work",
                        "features.get",
                        move |be| wops::work_get(be, wops::Family::Features, &id),
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
                                let (op, family) = if features {
                                    ("features.list", wops::Family::Features)
                                } else {
                                    ("issues.list", wops::Family::Issues)
                                };
                                self.query(
                                    Slot::ArchiveCount(p.clone(), features),
                                    "Counting running work",
                                    op,
                                    move |be| wops::work_list(be, family, &p, Some(&c)),
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
    pub(crate) fn list_state(ui: &mut Ui, list: &ListState<WorkRecord>, what: &str) -> bool {
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
        let open = self.project_open() && online.is_ok();
        let mut mem = self.routes.remove(&Route::Overview).unwrap_or_default();
        let mut actions: Vec<Action> = Vec::new();
        let project = self.selected_project.clone();
        let cols = self
            .work
            .workflows
            .get(project.as_deref().unwrap_or(""))
            .map(|workflow| columns(&workflow.rows))
            .unwrap_or_default();
        let user = devworks_desktop_user();
        let messages_tab = mem.tab.min(2) == 2;
        let work = &mut self.work;
        let (_, tab) = widgets::pane(
            ui,
            Header {
                title: screen.title,
                summary: screen.summary,
                tabs: &OVERVIEW_TABS,
                tab: mem.tab.min(2),
            },
            &mut actions,
            |ui, actions| {
                if widgets::secondary_button(ui, "Refresh").clicked() {
                    actions.push(if messages_tab {
                        Action::RefreshMessages
                    } else {
                        Action::Reload
                    });
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
                workflow::controls(ui, work, open, online.is_ok(), actions);
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
                } else if tab == 1 {
                    activity(ui, work, &cols, &user, actions);
                } else {
                    if work.messages.project.as_deref() == project.as_deref() {
                        message_board(ui, &mut work.messages, open, actions);
                    } else {
                        widgets::caption(ui, "Loading this project's messages…");
                    }
                }
            },
        );
        if tab == 2 && self.work.messages.project.as_deref() != project.as_deref() {
            actions.push(Action::RefreshMessages);
        } else if tab == 2 {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs(1));
            if self
                .work
                .messages
                .event_poll_at
                .is_some_and(|at| at <= std::time::Instant::now())
            {
                actions.push(Action::PollMessages);
            }
        }
        mem.tab = tab;
        self.routes.insert(Route::Overview, mem);
        self.workflow_dialog(ui.ctx(), &mut actions);
        self.work_act(actions);
    }

    pub(super) fn features_screen(&mut self, ui: &mut Ui) {
        let screen = Route::Features.screen();
        let online = self.online();
        let mut mem = self.routes.remove(&Route::Features).unwrap_or_default();
        let mut actions: Vec<Action> = Vec::new();
        let project = self.selected_project.clone();
        let open = self.project_open() && online.is_ok();
        let cols = self
            .work
            .workflows
            .get(project.as_deref().unwrap_or(""))
            .map(|workflow| columns(&workflow.rows))
            .unwrap_or_default();
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
        let mut watch_actions = Vec::new();
        {
            let work = &mut self.work;
            let watch = &self.watchers;
            let watch_online = online.as_ref().map(|_| ()).map_err(String::as_str);
            let mut st = (&mut *work, &mut mem, &mut actions, &mut watch_actions);
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
                            workflow::controls(ui, work, open, online.is_ok(), actions);
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
                    let watch_actions = &mut *st.3;
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
                                let scope = WatchScope::Feature(id.clone());
                                feature_detail(
                                    ui,
                                    work,
                                    &f,
                                    &cols,
                                    &projects_repos,
                                    open && !waiting.contains(&id),
                                    actions,
                                    &mut |ui| {
                                        watch.section(
                                            ui,
                                            WatchView::Feature,
                                            scope.clone(),
                                            watch_online,
                                            watch_actions,
                                        )
                                    },
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
        self.workflow_dialog(ui.ctx(), &mut actions);
        self.work_act(actions);
        self.watch_act(watch_actions);
    }

    pub(super) fn issues_screen(&mut self, ui: &mut Ui) {
        let screen = Route::Issues.screen();
        let online = self.online();
        let mut mem = self.routes.remove(&Route::Issues).unwrap_or_default();
        let mut actions: Vec<Action> = Vec::new();
        let project = self.selected_project.clone();
        let open = self.project_open() && online.is_ok();
        let cols = self
            .work
            .workflows
            .get(project.as_deref().unwrap_or(""))
            .map(|workflow| columns(&workflow.rows))
            .unwrap_or_default();
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
                            workflow::controls(ui, work, open, online.is_ok(), actions);
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
        self.workflow_dialog(ui.ctx(), &mut actions);
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
                    unlink_request(&w),
                ));
            }
            Some(false) => self.work.unlink = None,
            None => {}
        }
    }
}

/// The command that creates what `form` describes in project `p`, whose backlog status is
/// `backlog`: its operation, and the input (none when a value is outside its contract, which
/// the gateway then rejects as a malformed request).
pub(crate) fn create_request(
    form: &CreateForm,
    p: &str,
    backlog: &str,
) -> Result<(&'static str, Option<In>), String> {
    let feature = form.kind == "feature";
    let budget = if feature {
        Some(form.budget.input()?)
    } else {
        None
    };
    let op = match form.kind.as_str() {
        "feature" => "features.create",
        "issue" => "issues.create",
        _ => "tasks.create",
    };
    let input = || {
        let project_id = ProjectId(frame::id(p)?);
        let title = frame::text(&form.title)?;
        let description = frame::text(&form.description)?;
        let repository_ids = BoundedList(Vec::new());
        let status_id = frame::id(backlog)?;
        let priority = form.priority;
        let labels = Some(frame::texts::<Label, 128>(&form.labels)?);
        Some(match op {
            "features.create" => In::Features(features::Input::Create(features::CreateInput {
                project_id,
                creation: FeatureCreation::Single {
                    title,
                    description,
                    repository_ids,
                    status_id,
                    priority,
                    plan: None,
                    labels,
                    budget,
                },
            })),
            "issues.create" => In::Issues(issues::Input::Create(issues::CreateInput {
                project_id,
                title,
                description,
                repository_ids,
                status_id,
                priority,
                feature_id: frame::lift(form.feature.as_deref().map(frame::text::<RootId>))?,
                task_type: frame::lift(form.task_type.as_deref().map(frame::text))?,
                labels,
            })),
            _ => In::Tasks(tasks::Input::Create(tasks::CreateInput {
                project_id,
                title,
                description,
                repository_ids,
                parent_id: frame::lift(form.feature.as_deref().map(frame::text::<RootId>))?,
                status_id,
                priority,
                task_type: frame::lift(form.task_type.as_deref().map(frame::text))?,
                labels,
            })),
        })
    };
    Ok((op, input()))
}

/// The command that saves `form`'s changes to its item (its operation and input, as
/// [`create_request`]), or none when nothing changed.
pub(crate) fn edit_request(form: &EditForm) -> Result<Option<(&'static str, Option<In>)>, String> {
    let w = &form.snapshot;
    let feature = matches!(w.kind, WorkKind::Feature);
    let title = (form.title != w.title.0).then_some(form.title.as_str());
    let description = (form.description != w.description.0).then_some(form.description.as_str());
    let labels = (form.labels != labels_of(w)).then_some(form.labels.as_slice());
    let task_type = (!feature && form.task_type != task_type_of(w)).then_some(&form.task_type);
    let budget = if feature && form.budget_changed {
        Some(form.budget.input()?)
    } else {
        None
    };
    let priority = (!feature && form.priority != w.priority).then_some(form.priority);
    if title.is_none()
        && description.is_none()
        && labels.is_none()
        && task_type.is_none()
        && budget.is_none()
        && priority.is_none()
    {
        return Ok(None);
    }
    let input = || {
        let work_id = w.id.clone();
        let expected_revision = w.revision.clone();
        let title = frame::lift(title.map(frame::text))?;
        let description = frame::lift(description.map(frame::text))?;
        let labels = frame::lift(labels.map(frame::texts::<Label, 128>))?;
        Some(if feature {
            In::Features(features::Input::Edit(features::EditInput {
                work_id,
                expected_revision,
                changes: FeaturePatch {
                    title,
                    description,
                    labels,
                    budget,
                    ..wops::empty()?
                },
            }))
        } else {
            let (task_type, clear_task_type) = match task_type {
                Some(Some(kind)) => (Some(frame::text(kind)?), None),
                Some(None) => (None, Some(true)),
                None => (None, None),
            };
            In::Tasks(tasks::Input::Edit(tasks::EditInput {
                work_id,
                expected_revision,
                changes: TaskPatch {
                    title,
                    description,
                    labels,
                    priority,
                    task_type,
                    clear_task_type,
                    ..wops::empty()?
                },
            }))
        })
    };
    Ok(Some((
        if feature {
            "features.edit"
        } else {
            "tasks.edit"
        },
        input(),
    )))
}

/// The `tasks.move` command moving `w` to workflow status `status`.
pub(crate) fn move_request(w: &WorkRecord, status: &str) -> Option<In> {
    Some(In::Tasks(tasks::Input::Move(tasks::MoveInput {
        task_id: w.id.clone(),
        expected_revision: w.revision.clone(),
        status_id: frame::id(status)?,
        expected_workflow_revision: match &w.workflow_revision {
            Some(revision) => revision.clone(),
            None => frame::number(1)?,
        },
    })))
}

/// The queue placement command of `op` for one bounded `placement`.
pub(crate) fn order_request(
    op: &str,
    p: &str,
    feature: Option<&str>,
    priority: Option<i32>,
    placement: QueuePlacement,
    expected: &str,
) -> Option<In> {
    let project_id = ProjectId(frame::id(p)?);
    let expected_queue_revision = frame::text(expected)?;
    let placement = Some(placement);
    Some(match op {
        "features.prioritize" => {
            In::Features(features::Input::Prioritize(features::PrioritizeInput {
                project_id,
                expected_queue_revision,
                ordered_work_ids: None,
                placement,
            }))
        }
        "issues.prioritize" => In::Issues(issues::Input::Prioritize(issues::PrioritizeInput {
            project_id,
            expected_queue_revision,
            ordered_work_ids: None,
            placement,
        })),
        _ => In::Tasks(tasks::Input::Prioritize(tasks::PrioritizeInput {
            project_id,
            feature_id: frame::lift(feature.map(frame::text::<RootId>))?,
            expected_queue_revision,
            priority: priority.unwrap_or(0),
            ordered_task_ids: None,
            placement,
        })),
    })
}

/// The `features.request_plan_approval` command for the pending plan of `w`.
pub(crate) fn approval_request(
    w: &WorkRecord,
    pending: &PendingFeaturePlan,
    digest: &Digest,
) -> In {
    In::Features(features::Input::RequestPlanApproval(
        features::RequestPlanApprovalInput {
            feature_id: RootId(w.id.clone()),
            expected_revision: w.revision.clone(),
            plan_revision: pending.revision.clone(),
            plan_digest: digest.clone(),
        },
    ))
}

/// The `tasks.set_feature` command making `w` a standalone issue.
pub(crate) fn unlink_request(w: &WorkRecord) -> In {
    In::Tasks(tasks::Input::SetFeature(tasks::SetFeatureInput {
        task_id: w.id.clone(),
        expected_revision: w.revision.clone(),
        feature_id: None,
        clear_feature: true,
    }))
}

/// The `features.edit_plan` command saving `form` (as [`create_request`]). Every live
/// requirement of the active plan stays in the draft (dropping one needs an explicit decision);
/// what the form does not show is kept from its starting draft.
pub(crate) fn plan_request(form: &PlanForm, content: &ArtifactRef) -> Result<Option<In>, String> {
    let w = &form.snapshot;
    let (budget, clear_budget_proposal) = match &form.budget {
        Some(budget) => (Some(budget.input()?), None),
        None if w
            .pending_plan
            .as_ref()
            .is_some_and(|pending| pending.budget.is_some()) =>
        {
            (None, Some(true))
        }
        None => (None, None),
    };
    Ok(plan_body(form, content).map(|plan| {
        In::Features(features::Input::EditPlan(features::EditPlanInput {
            feature_id: RootId(w.id.clone()),
            expected_revision: w.revision.clone(),
            plan,
            budget,
            clear_budget_proposal,
        }))
    }))
}

/// The desktop user's actor (the host binds it), for "You" in Activity.
fn devworks_desktop_user() -> String {
    crate::host::USER_ACTOR.to_owned()
}

/// The `agents.post` command of `draft` in `project` (none when a value is outside its
/// contract).
pub(crate) fn message_post_body(draft: &MessageDraft, project: &str) -> Option<In> {
    let kind = ["Info", "Question", "Request", "Blocker", "Announcement"]
        .get(draft.kind)
        .copied()
        .unwrap_or("Info");
    fn field(value: &str) -> Option<&str> {
        Some(value.trim()).filter(|v| !v.is_empty())
    }
    let reply = field(&draft.reply_to);
    let resolution = match (reply, draft.resolution) {
        (Some(_), 1) => Some("Answered"),
        (Some(_), 2) => Some("Declined"),
        _ => None,
    };
    Some(In::Agents(agents::Input::Post(agents::PostInput {
        session_id: None,
        project_id: Some(ProjectId(frame::id(project)?)),
        body: frame::text(&draft.body)?,
        message_kind: Some(frame::text(kind)?),
        item_id: frame::lift(field(&draft.item).map(frame::text))?,
        feature_id: frame::lift(field(&draft.feature).map(frame::text))?,
        resource_name: frame::lift(field(&draft.resource).map(frame::text))?,
        to_session_id: frame::lift(field(&draft.to_session).map(frame::text))?,
        to_human: draft.to_human.then_some(true),
        blocking: draft.blocking.then_some(true),
        in_reply_to_id: frame::lift(reply.map(frame::text))?,
        resolves: frame::lift(resolution.map(frame::text))?,
        idempotency_key: None,
    })))
}

fn message_board(ui: &mut Ui, board: &mut MessageBoard, open: bool, actions: &mut Vec<Action>) {
    if let Some(error) = &board.error {
        chip(ui, error.clone(), Tone::Bad);
    }
    ui.heading("Your inbox");
    if !board.inbox_loaded && board.inbox.is_empty() {
        widgets::caption(ui, "Loading your messages…");
    } else if board.inbox.is_empty() {
        widgets::caption(ui, "No unread messages.");
    }
    if board.inbox_has_more
        && ui
            .add_enabled(open, egui::Button::new("Load more inbox messages"))
            .clicked()
    {
        actions.push(Action::MoreInbox);
    }
    for message in &board.inbox {
        ui.group(|ui| {
            ui.label(format!("{:?} · {}", message.kind, message.from.name.0));
            ui.label(&message.body.0);
            if let Some(item) = &message.item_id {
                widgets::caption(ui, format!("Work: {}", item.0.as_str()));
            }
            if message.blocking {
                chip(ui, "Blocking", Tone::Attention);
            }
            if ui
                .add_enabled(open, egui::Button::new("Mark read"))
                .clicked()
            {
                actions.push(Action::AckMessage(message.id.as_str().to_owned(), None));
            }
            if matches!(
                message.kind,
                devworks_core::protocol::records::RetainedMessageKind::Question
                    | devworks_core::protocol::records::RetainedMessageKind::Request
            ) && matches!(
                message.state,
                Some(devworks_core::protocol::records::RetainedMessageState::Open)
            ) {
                if ui
                    .add_enabled(open, egui::Button::new("Answer question"))
                    .clicked()
                {
                    actions.push(Action::AckMessage(
                        message.id.as_str().to_owned(),
                        Some("Answered"),
                    ));
                }
                if ui
                    .add_enabled(open, egui::Button::new("Decline question"))
                    .clicked()
                {
                    actions.push(Action::AckMessage(
                        message.id.as_str().to_owned(),
                        Some("Declined"),
                    ));
                }
            }
        });
    }
    ui.add_space(10.0);
    ui.heading("Project messages");
    if !board.history_loaded && board.history.is_empty() {
        widgets::caption(ui, "Loading message history…");
    } else if board.history.is_empty() {
        widgets::caption(ui, "No project messages yet.");
    }
    for message in &board.history {
        ui.group(|ui| {
            ui.label(format!("{:?} · {}", message.kind, message.from.name.0));
            ui.label(&message.body.0);
            let mut meta = Vec::new();
            if let Some(item) = &message.item_id {
                meta.push(format!("work {}", item.0.as_str()));
            }
            if let Some(feature) = &message.feature_id {
                meta.push(format!("feature {}", feature.0.as_str()));
            }
            if let Some(resource) = &message.resource_name {
                meta.push(format!("resource {}", resource.0));
            }
            if let Some(state) = &message.state {
                meta.push(format!("{state:?}"));
            }
            if !meta.is_empty() {
                widgets::caption(ui, meta.join(" · "));
            }
            // Project history retains read questions after their inbox delivery is acknowledged.
            // Keep their resolution actions available while the question remains open; an
            // unread question already offers them in the inbox above.
            if message.to_human
                && !board.inbox.iter().any(|unread| unread.id == message.id)
                && matches!(
                    message.kind,
                    devworks_core::protocol::records::RetainedMessageKind::Question
                        | devworks_core::protocol::records::RetainedMessageKind::Request
                )
                && matches!(
                    message.state,
                    Some(devworks_core::protocol::records::RetainedMessageState::Open)
                )
            {
                if ui
                    .add_enabled(open, egui::Button::new("Answer question"))
                    .clicked()
                {
                    actions.push(Action::AckMessage(
                        message.id.as_str().to_owned(),
                        Some("Answered"),
                    ));
                }
                if ui
                    .add_enabled(open, egui::Button::new("Decline question"))
                    .clicked()
                {
                    actions.push(Action::AckMessage(
                        message.id.as_str().to_owned(),
                        Some("Declined"),
                    ));
                }
            }
        });
    }
    if board.history_before.is_some() && widgets::secondary_button(ui, "Older messages").clicked() {
        actions.push(Action::OlderMessages);
    }
    ui.add_space(10.0);
    ui.heading("Post a message");
    let draft = &mut board.draft;
    ui.horizontal(|ui| {
        ui.label("Kind");
        egui::ComboBox::from_id_salt("project-message-kind")
            .selected_text(
                ["Info", "Question", "Request", "Blocker", "Announcement"]
                    .get(draft.kind)
                    .copied()
                    .unwrap_or("Info"),
            )
            .show_ui(ui, |ui| {
                for (index, kind) in ["Info", "Question", "Request", "Blocker", "Announcement"]
                    .iter()
                    .enumerate()
                {
                    ui.selectable_value(&mut draft.kind, index, *kind);
                }
            })
            .response
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Message kind"));
    });
    ui.add(
        egui::TextEdit::multiline(&mut draft.body)
            .desired_rows(3)
            .hint_text("Message"),
    )
    .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Message body"));
    ui.horizontal(|ui| {
        ui.label("To session");
        ui.text_edit_singleline(&mut draft.to_session)
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "To session"));
        ui.checkbox(&mut draft.to_human, "To you");
    });
    ui.horizontal(|ui| {
        ui.label("Work item");
        ui.text_edit_singleline(&mut draft.item)
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Work item"));
        ui.label("Feature");
        ui.text_edit_singleline(&mut draft.feature)
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Feature"));
    });
    ui.horizontal(|ui| {
        ui.label("Resource");
        ui.text_edit_singleline(&mut draft.resource)
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Resource"));
        ui.checkbox(&mut draft.blocking, "Blocks work");
    });
    ui.horizontal(|ui| {
        ui.label("Reply to message");
        ui.text_edit_singleline(&mut draft.reply_to)
            .widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Reply to message")
            });
        egui::ComboBox::from_id_salt("project-message-resolution")
            .selected_text(
                ["No resolution", "Answered", "Declined"]
                    .get(draft.resolution)
                    .copied()
                    .unwrap_or("No resolution"),
            )
            .show_ui(ui, |ui| {
                for (index, resolution) in
                    ["No resolution", "Answered", "Declined"].iter().enumerate()
                {
                    ui.selectable_value(&mut draft.resolution, index, *resolution);
                }
            })
            .response
            .widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Message resolution")
            });
    });
    if ui
        .add_enabled(
            open && !draft.body.trim().is_empty(),
            egui::Button::new("Post message"),
        )
        .clicked()
    {
        actions.push(Action::PostMessage(draft.clone()));
    }
    if !board.events.is_empty() {
        ui.add_space(10.0);
        ui.heading("Recent message changes");
        for event in board.events.iter().rev().take(25) {
            widgets::caption(
                ui,
                format!(
                    "{} · {} · {}",
                    event.kind.0,
                    event.actor.name.0,
                    event.message_id.as_str()
                ),
            );
        }
    }
}

/// What an Activity event is and what it says, from its body.
pub fn event_summary(e: &Event) -> (&'static str, String) {
    match &e.event {
        EventBody::CommandDecided { command_id } => (
            "Change",
            format!(
                "command {} decided; revision {}",
                command_id.0.as_str(),
                e.aggregate_revision.as_str()
            ),
        ),
        EventBody::RootControlChanged { reason, .. } => (
            "Run control",
            match reason {
                ControlReason::Paused => "paused".into(),
                ControlReason::Continued => "continued".into(),
            },
        ),
        EventBody::ActivationRequested {
            execution_id,
            generation,
            ..
        } => (
            "Run start",
            format!(
                "activation {} requested (generation {})",
                execution_id.0.as_str(),
                generation.as_str()
            ),
        ),
        EventBody::BlockerChanged { blocker, change } => (
            "Blocker",
            format!(
                "{} {:?}",
                match change {
                    BlockerChange::Raised => "raised",
                    BlockerChange::Cleared => "cleared",
                },
                blocker.cause
            ),
        ),
        EventBody::OperationObserved {
            operation_id,
            state,
            ..
        } => (
            "Operation",
            format!("operation {} {state:?}", operation_id.0.as_str()),
        ),
    }
}

/// One Activity row: its sequence, the event's kind and summary, who, what it concerns and when,
/// in local time.
pub fn activity_line(e: &Event, who: &str, what: &str) -> String {
    let (kind, summary) = event_summary(e);
    format!(
        "#{} · {kind}: {summary} · {who} · {what} · {}",
        e.sequence.as_str(),
        super::jobs::local_time(Some(&e.occurred_at_ms)),
    )
}

fn activity(
    ui: &mut Ui,
    work: &mut WorkUi,
    cols: &[Column],
    user: &str,
    actions: &mut Vec<Action>,
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
    let root = a.item.clone();
    if let Some(root) = &root {
        work.histories.entry(root.clone()).or_default();
    }
    let feed = match &root {
        Some(root) => &work.histories[root],
        None => &work.activity,
    };
    if let Some(error) = &feed.error {
        widgets::state_block(
            ui,
            &ViewState::Failed {
                what: "history".into(),
                error: error.clone(),
            },
        );
        if widgets::secondary_button(ui, "Reload history").clicked() {
            actions.push(Action::RefreshHistory(root.clone()));
        }
    }
    if !feed.loaded {
        if feed.error.is_none() {
            widgets::state_block(
                ui,
                &ViewState::Loading {
                    what: "history".into(),
                },
            );
        }
        return;
    }
    let items = &feed.items;
    let more = feed.more;
    let loading = feed.loading;
    let a = &work.activity;
    let titles: BTreeMap<String, &WorkRecord> = work
        .features
        .items
        .iter()
        .chain(&work.cards.items)
        .map(|w| (w.id.as_str().to_owned(), w))
        .collect();
    let rows: Vec<String> = items
        .iter()
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
            let line = activity_line(e, if mine { "You" } else { e.actor_id.0.as_str() }, &what);
            (kind_ok && actor_ok && item_ok && matches_text_line(&line, &a.text)).then_some(line)
        })
        .collect();
    widgets::caption(
        ui,
        format!(
            "{} of {} loaded changes shown · newest first · older history on demand",
            rows.len(),
            items.len(),
        ),
    );
    if more
        && ui
            .add_enabled(!loading, egui::Button::new("Load older changes"))
            .clicked()
    {
        actions.push(Action::OlderHistory(root));
    }
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
