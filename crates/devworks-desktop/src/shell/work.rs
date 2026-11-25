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
use crate::backend::{Reply, TaskOutput, is_unknown, resolve_command};
use crate::frame;
use crate::ops::{self, Failure};
use crate::paged::{Applied, Fetch, Page, Paged};
use crate::route::Route;
use crate::screen::{Cx, Effect, Screen, ScreenContext, ScreenId, controller};
use crate::widgets::{self, Tone, chip};
use crate::work_ops as wops;
use crate::work_ops::WorkSlot;
use devworks_core::operations::{
    OperationInput as In, agents, features, issues, next_work, tasks, workflows,
};
use devworks_core::protocol::provider::{ArtifactRef, Digest};
use devworks_core::protocol::records::{
    AcceptanceCriterion, CriterionState, FeatureCreation, FeaturePatch, LifeState,
    PendingFeaturePlan, PlanDraft, QueuePlacement, RequirementItem, RequirementStatus,
    RetainedMessage, RetainedMessageEvent, RootSnapshot, TaskPatch, WorkKind, WorkRecord,
};
use devworks_core::protocol::support::{BoundedList, Label};
use devworks_core::protocol::{DomainErrorCode, Event};
use devworks_core::protocol::{Id, ProjectId, Revision, RootId};
use serde_json::{Value, json};
mod actions;
mod builders;
mod forms;
mod replies;
mod requests;
pub(crate) mod reviews;
pub(crate) mod workflow;

pub use crate::work_screen::activity::{activity_line, event_summary};
pub use crate::work_screen::detail::{BlockerEdit, blocker_change, plan_error};
pub use crate::work_screen::{adjacent_move, next_id};
pub(crate) use builders::*;
use devworks_core::protocol::records::{
    AllocationClaimRecord, AllocationOutcome, AllocationQueueSnapshot, AllocationRecovery,
    AllocationSelection, StatusRecord,
};
use eframe::egui::{self, RichText, Ui};
pub use forms::*;
use std::collections::{BTreeMap, BTreeSet};
use workflow::{WorkflowAction, WorkflowUi};

/// Feature list tabs and Issues tabs.
pub const FEATURE_TABS: [&str; 2] = ["List", "Board"];
pub const ISSUE_TABS: [&str; 2] = ["List", "History"];
pub const OVERVIEW_TABS: [&str; 3] = ["Summary", "Activity", "Messages"];

/// A bounded newest-first project or card feed: its events are a paged list (a refresh re-reads
/// only the pages the user requested, and publishes the replacement once they are read), and the
/// filters narrow what is shown of it.
#[derive(Default)]
pub struct Activity {
    /// Loaded events, newest first.
    pub feed: Paged<Event>,
    /// 0 everything, 1 features, 2 issues, 3 tasks, 4 the project itself.
    pub kind: usize,
    /// 0 everyone, 1 you, 2 agents and others.
    pub actor: usize,
    pub text: String,
    /// Only this work item (from a detail's history link).
    pub item: Option<String>,
}
impl std::ops::Deref for Activity {
    type Target = Paged<Event>;
    fn deref(&self) -> &Paged<Event> {
        &self.feed
    }
}

#[derive(Default)]
pub struct WorkUi {
    pub messages: MessageBoard,
    pub histories: BTreeMap<String, Activity>,
    pub keep_working: BTreeMap<String, devworks_engine::KeepWorkingStatus>,

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
    /// The project's features and cards: each read gathers every page before it replaces the
    /// list, so a board never shows part of itself.
    pub features: Paged<WorkRecord>,
    pub cards: Paged<WorkRecord>,
    pub activity: Activity,
    /// Current records of opened features (readiness at schema revision 2).
    pub detail: BTreeMap<String, WorkRecord>,
    pub(crate) readiness_history:
        BTreeMap<String, Paged<devworks_core::protocol::records::LineageRecord>>,
    /// Each root's source-control links: a re-read keeps the list on screen until every page
    /// arrived, so a refresh never shifts the controls beneath it.
    pub code_reviews: BTreeMap<String, reviews::ReviewPane>,
    pub git: BTreeMap<String, Paged<devworks_core::protocol::records::AttributedGitLink>>,
    pub runs: BTreeMap<String, RootSnapshot>,
    pub git_inspections:
        BTreeMap<String, Result<devworks_core::protocol::records::GitInspection, String>>,
    pub github_links: BTreeMap<String, devworks_core::protocol::records::GithubIssueLink>,
    pub github_link_number_drafts: BTreeMap<String, String>,
    pub github_pr_checks:
        BTreeMap<String, Vec<devworks_core::protocol::records::GithubObservation>>,
    pub github_pr_reviews:
        BTreeMap<String, Vec<devworks_core::protocol::records::GithubObservation>>,
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
        self.runs.retain(|id, _| !owned(id));
        self.git_inspections.retain(|id, _| !owned(id));
        self.features
            .items
            .retain(|r| r.project_id != scope.project_id);
        self.cards
            .items
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
    KeepWorking(bool),
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
    CodeReview(reviews::ReviewAction),
    ReadinessHistory(String, bool),
    InspectGit(String, String),
    OpenGitSession(String),
    PublishGithubIssue {
        issue_id: String,
        expected_revision: String,
        repository_id: String,
    },
    LinkGithubIssue {
        issue_id: String,
        expected_revision: String,
        repository_id: String,
        remote_issue_number: String,
    },
    ReadGithubPullObservations {
        repository_id: String,
        pull_request_url: String,
        expected_head: String,
    },
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

    // -----------------------------------------------------------------------------------------
    // Replies
}
