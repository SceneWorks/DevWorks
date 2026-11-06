//! `WorkService`: features, standalone issues and task cards (DW-012) behind `features.list`,
//! `.get`, `.create`, `.edit_plan`, `.prioritize`, `.dependencies`, `.request_plan_approval`,
//! `.edit`, `issues.list`, `.get`, `.create`, `.edit`, `.prioritize` and `tasks.list`, `.get`,
//! `.create`, `.edit`, `.prioritize`, `.move`, `.assign`, `.dependency_links`,
//! `.acceptance_evidence`, `.sync_plan`, `.set_feature`, registered through
//! [`crate::services::UseCases::with_work`]. The native Features, board, Issues and detail
//! screens and agent tools reach the same use cases through the gateway; nothing here has a
//! second copy in the shell, and an agent's change appears in the same board and history as a
//! manual one (WORK-03).
//!
//! # Work model (WORK-04, WORK-11/12, MODEL-01..03)
//!
//! Every card belongs to a project and has one optional, authoritative feature membership
//! (`parentId`). A card without a feature **is** an issue: `tasks.*` and `issues.*` address the
//! same stored identity (its wire `kind` is a view of its membership), so no entry point can
//! duplicate work. Features carry their plan (requirements, acceptance, goals, repository scope,
//! content), description, priority, workflow status and links; cards carry their own
//! description, priority, status, assignment, blocking links, acceptance criteria, repositories,
//! links, comments and handoffs. A feature or card may name several member repositories.
//!
//! Every edit names the revision it was prepared from; a different stored revision is a
//! `RevisionConflict` decision with no write (the gateway undoes every partial write of a
//! refused or conflicting decision), so a stale form or late agent update never overwrites a
//! newer change (WORK-06). Records keep stable IDs, project scope, creator and last editor,
//! timestamps and revisions; every change appends attributed history on the work's root
//! (MODEL-02). Workflow status (`tasks.move`), execution (run controls) and pause holds are
//! separate records: moving a card neither starts, pauses nor completes execution (MODEL-03).
//! Writes need an active project: an archived or deleted project is closed to work.
//!
//! # Plans (AR-02, WORK-08)
//!
//! A feature's plan slot holds one active approved plan and at most one pending revision.
//! `features.edit_plan` saves the pending draft (replacing an earlier draft and making its
//! approval request stale); `features.request_plan_approval` binds the exact pending revision and
//! digest in a request; the decision belongs to the approvals item, which calls
//! [`plans::decide_plan_approval`] (a human decision only): approval adopts that exact draft as
//! the active plan in one unit of work, rejection leaves everything as it was. Drafting,
//! requesting and rejection never change the active plan, its approval, effective scope or the
//! eligibility of work under it. Generic feature edits cannot change an approved feature's goals
//! or repository scope, and a re-plan (`tasks.sync_plan`) cannot place cards outside the active
//! scope.
//!
//! # Readiness (AGENT-12, AGENT-18..20)
//!
//! A feature's execution-readiness assessment ([`readiness`]) is bound to the plan revision and
//! digest it reviewed and to the digest of each scope unit (the feature-wide unit and one per
//! requirement). Freshness is computed against the active plan unit by unit, so a material change
//! invalidates only the affected units and every unchanged unit keeps its evidence. Readiness is
//! derived (a current assessment without an open blocking finding), never caller-asserted, and is
//! shown apart from plan approval; run control reads the same evidence through its scope port.
//!
//! # Membership, priority and links
//!
//! `tasks.set_feature` (and `issues.create` with a feature) is the one membership writer: linking
//! makes the card part of the feature's delivery without plan reapproval and keeps every hold;
//! clearing installs a durable human pause on the card ([`ControlService`]) that only an explicit
//! Continue clears (relinking never does); a direct transfer keeps its holds and creates no
//! pause. Priority edits and bulk ordering commit revisioned queue history; blocking links are
//! project-wide, validated for self, duplicate and cycles inside the write unit of work.
//!
//! # Retained CodeGraph behavior
//!
//! `WorkController.Create`/`Find`/`Get`/`Update` and `create_work`/`find_work`/`get_work`/
//! `update_work` for features and items map to `features.*`, `issues.*` and `tasks.*`;
//! `WorkController.SyncPlan` and `sync_plan` map to `features.edit_plan` and `tasks.sync_plan`.
//! Retained quirks: a re-plan matches cards by their caller reference (kept from creation) or by
//! ID, adds a card for an unknown reference, **closes** (status `Closed`, never deleted) a card of
//! the feature it leaves out unless it is already finished, never completes a card, keeps an
//! omitted priority, and keeps a criterion's state and evidence while its ID and text are
//! unchanged; a plan holds at most 200 cards, 20 features and 500 links and is all-or-nothing.
//! CodeGraph's preview token is replaced by the expected plan and task-set revisions.
use crate::context::InvocationContext;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{
    OperationInput, features as fops, issues as iops, tasks as tops, workflows as wops,
};
use crate::ports::storage::{
    ACTIVE, ARCHIVED, DELETED, FEATURE, NewEvent, ProjectRead, ReadSnapshot, StorageError,
    StoredProject, StoredStatus, StoredWork, WorkRead, WriteTransaction,
};
use crate::ports::{
    CommandHandler, Decided, HandlerFailure, IdKind, IdSource, QueryAnswer, QueryHandler,
};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::{
    CorrelationId, DomainErrorCode, DomainFault, EventId, Id, ProjectId, RevisionDimension,
};
use crate::services::runs::ControlService;
use crate::services::sessions::{
    Failure, Refusal, decided, not_found, project_visible, query_fault, rev,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;

pub mod allocation;
pub mod deps;
pub mod items;
pub mod membership;
pub mod model;
pub mod plans;
pub mod queue;
pub mod readiness;
pub mod workflow;
pub mod workflow_ops;

pub use model::{fold, link_id, plan_digest, plan_units, search_key};

/// The operations [`WorkService`] is registered for.
pub const OPERATIONS: [&str; 34] = [
    "next_work.claim_project_queue",
    "next_work.claim_feature_task",
    "next_work.inspect_claim",
    "next_work.inspect_queue",
    "next_work.release_claim",
    "features.list",
    "features.get",
    "features.create",
    "features.edit_plan",
    "features.prioritize",
    "features.dependencies",
    "features.request_plan_approval",
    "features.edit",
    "issues.list",
    "issues.get",
    "issues.create",
    "issues.edit",
    "issues.prioritize",
    "tasks.list",
    "tasks.get",
    "tasks.create",
    "tasks.edit",
    "tasks.prioritize",
    "tasks.move",
    "tasks.assign",
    "tasks.dependency_links",
    "tasks.acceptance_evidence",
    "tasks.sync_plan",
    "tasks.set_feature",
    "workflows.list",
    "workflows.create_status",
    "workflows.rename_status",
    "workflows.order",
    "workflows.reassign_and_remove",
];
/// History aggregate of a work item (`aggregate_id` = work ID).
pub const WORK_AGGREGATE: &str = "work";
/// History aggregate of a project's queue order (`aggregate_id` = project ID).
pub const QUEUE_AGGREGATE: &str = "work_queue";
/// Most items a plan (atomic creation or re-plan) holds, its features and its links.
pub const PLAN_ITEMS: usize = 200;
pub const PLAN_FEATURES: usize = 20;
pub const PLAN_LINKS: usize = 500;
/// Most comments or handoffs one item records.
pub const MAX_NOTES: usize = 10_000;
/// Largest staged atomic plan read.
pub const MAX_STAGED_PLAN: usize = 8 << 20;

pub(crate) type R<T> = Result<T, Failure>;

pub(crate) fn refuse(code: DomainErrorCode, kind: &'static str, id: Option<&Id>) -> Failure {
    Refusal::new(code, kind, id.cloned()).into()
}
pub(crate) fn conflict(kind: &'static str, id: &Id) -> Failure {
    refuse(DomainErrorCode::Conflict, kind, Some(id))
}
pub(crate) fn invalid(kind: &'static str, id: Option<&Id>) -> Failure {
    refuse(DomainErrorCode::InvalidArgument, kind, id)
}
pub(crate) fn unsupported(kind: &'static str, id: &Id) -> Failure {
    refuse(DomainErrorCode::UnsupportedTarget, kind, Some(id))
}

/// A revision conflict of `dimension` on `kind`/`id` (the gateway records it as the decision).
pub(crate) fn stale(
    dimension: RevisionDimension,
    kind: &'static str,
    id: &Id,
    expected: i64,
    current: i64,
) -> Failure {
    Failure::Storage(StorageError::conflict(
        dimension,
        kind,
        id.clone(),
        rev(expected),
        rev(current),
    ))
}
pub(crate) fn check_revision(w: &StoredWork, expected: i64) -> R<()> {
    if w.revision != expected {
        return Err(stale(
            RevisionDimension::Work,
            "work",
            &w.id,
            expected,
            w.revision,
        ));
    }
    Ok(())
}

/// The work item `id` if the invocation may see its project (otherwise `NotFound`).
pub fn visible_work(read: &dyn WorkRead, ctx: &InvocationContext, id: &Id) -> R<StoredWork> {
    match read.work(id)? {
        Some(w) if project_visible(ctx, Some(&w.project_id)) => Ok(w),
        _ => Err(not_found("work", id).into()),
    }
}
/// A visible feature.
pub(crate) fn visible_feature(
    read: &dyn WorkRead,
    ctx: &InvocationContext,
    id: &Id,
) -> R<StoredWork> {
    let w = visible_work(read, ctx, id)?;
    if w.kind != FEATURE {
        return Err(unsupported("feature", id));
    }
    Ok(w)
}
/// A visible card (a feature task or a standalone issue).
pub(crate) fn visible_card(read: &dyn WorkRead, ctx: &InvocationContext, id: &Id) -> R<StoredWork> {
    let w = visible_work(read, ctx, id)?;
    if w.kind == FEATURE {
        return Err(unsupported("task", id));
    }
    Ok(w)
}
/// The visible project `id`, in any lifecycle.
pub(crate) fn visible_project(
    read: &dyn ProjectRead,
    ctx: &InvocationContext,
    id: &ProjectId,
) -> R<StoredProject> {
    crate::services::projects::visible_project(read, ctx, id)
}
/// The visible project `id`, open to work (active): an archived or deleted project accepts no
/// work change.
pub(crate) fn open_project(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    id: &ProjectId,
) -> R<StoredProject> {
    let p = visible_project(tx.projects(), ctx, id)?;
    match p.lifecycle.as_str() {
        ACTIVE => Ok(p),
        ARCHIVED => Err(conflict("project_archived", &p.id.0)),
        DELETED => Err(conflict("project_deleted", &p.id.0)),
        _ => Err(conflict("project_closed", &p.id.0)),
    }
}

/// The root a work item's history belongs to: its feature for a feature task, itself otherwise.
pub(crate) fn history_root(w: &StoredWork) -> Id {
    match (&w.parent_feature_id, w.kind == FEATURE) {
        (Some(f), false) => f.clone(),
        _ => w.id.clone(),
    }
}

/// Appends one attributed work history event on the item's root and project.
pub(crate) fn event(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    w: &StoredWork,
    kind: &str,
    payload: Value,
) -> Result<(), StorageError> {
    let mut payload = payload;
    if let Some(map) = payload.as_object_mut() {
        map.insert(
            "principalKind".into(),
            json!(format!("{:?}", ctx.principal_kind())),
        );
        map.insert("workId".into(), json!(w.id.as_str()));
    }
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id,
        actor: ctx.principal().clone(),
        project_id: Some(w.project_id.clone()),
        root_id: Some(crate::protocol::RootId(history_root(w))),
        operation_id: None,
        aggregate_kind: WORK_AGGREGATE.into(),
        aggregate_id: w.id.as_str().to_owned(),
        aggregate_revision: w.revision.max(1),
        kind: kind.into(),
        payload,
    })?;
    Ok(())
}

/// A set of IDs in ID order; duplicates are refused as `kind`.
pub(crate) fn id_set(v: &[Id], kind: &'static str) -> R<Vec<Id>> {
    let keys: BTreeSet<&str> = v.iter().map(Id::as_str).collect();
    if keys.len() != v.len() {
        return Err(invalid(kind, None));
    }
    let mut out = v.to_vec();
    out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(out)
}
/// The repositories `repos` (a set) checked as members of `project`.
pub(crate) fn member_repositories(
    tx: &mut dyn WriteTransaction,
    project: &ProjectId,
    repos: &[Id],
) -> R<Vec<Id>> {
    let set = id_set(repos, "repository_ids")?;
    let members = tx.projects().members(project)?;
    for r in &set {
        if !members.contains(r) {
            return Err(not_found("repository", r).into());
        }
    }
    Ok(set)
}
/// Whether an artifact reference resolves for this invocation in `project`: the project's own
/// artifact or one shared with it. A workspace artifact the project cannot see is refused,
/// even for a workspace invocation that sees it, since the project's agents read it there.
pub(crate) fn check_artifact(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    project: &ProjectId,
    a: &ArtifactRef,
) -> R<()> {
    use crate::services::sessions::transcript::reference_resolves;
    // Visible in the work's project (its own artifact or one shared with it), whatever scope
    // the caller works in: the project's agents read it there.
    if reference_resolves(tx, ctx, Some(project), a)? {
        return Ok(());
    }
    Err(not_found("artifact", &a.artifact_id).into())
}
/// A non-blank title.
pub(crate) fn require_title(t: &str) -> R<()> {
    if t.trim().is_empty() {
        return Err(invalid("title", None));
    }
    Ok(())
}
/// The project's status `id` (the default workflow is seeded first if the project has none).
pub(crate) fn status_of(
    tx: &mut dyn WriteTransaction,
    project: &ProjectId,
    id: &Id,
) -> R<(StoredStatus, Vec<StoredStatus>)> {
    let all = workflow::ensure_workflow(tx, project)?;
    match all.iter().find(|s| &s.id == id) {
        Some(s) => Ok((s.clone(), all)),
        None => Err(not_found("status", id).into()),
    }
}

/// The work service: the one implementation of features, issues and task cards.
pub struct WorkService {
    ids: Arc<dyn IdSource>,
    runs: Arc<ControlService>,
    staged: Option<Arc<dyn plans::StagedPlans>>,
    clock: Option<Arc<dyn crate::ports::Clock>>,
}

impl WorkService {
    /// A service whose unlink pauses go through `runs` (the runtime's one control service).
    pub fn new(ids: Arc<dyn IdSource>, runs: Arc<ControlService>) -> Self {
        Self {
            ids,
            runs,
            staged: None,
            clock: None,
        }
    }
    /// Reads staged atomic plans (artifact content) through `staged`.
    pub fn with_staged_plans(mut self, staged: Arc<dyn plans::StagedPlans>) -> Self {
        self.staged = Some(staged);
        self
    }
    /// Supplies the host clock for current lease inspection; no wire argument can supply time.
    pub fn with_clock(mut self, clock: Arc<dyn crate::ports::Clock>) -> Self {
        self.clock = Some(clock);
        self
    }
    pub(crate) fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }
    pub(crate) fn runs(&self) -> &ControlService {
        &self.runs
    }
}

impl QueryHandler for WorkService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        use crate::protocol::results::OperationResult as O;
        use items::Family;
        let answered = match query.input() {
            OperationInput::Workflows(wops::Input::List(i)) => self
                .list_workflow(snapshot, ctx, i)
                .map(|(v, s)| (O::WorkflowsList { value: v }, s)),
            OperationInput::NextWork(crate::operations::next_work::Input::InspectClaim(i)) => {
                self.inspect_claim(snapshot, ctx, i)
            }
            OperationInput::NextWork(crate::operations::next_work::Input::InspectQueue(i)) => {
                self.inspect_queue(snapshot, ctx, i)
            }
            OperationInput::Features(fops::Input::List(i)) => self
                .list(snapshot, ctx, Family::Features, &items::list_args(i))
                .map(|(v, s)| (O::FeaturesList { value: v }, s)),
            OperationInput::Issues(iops::Input::List(i)) => self
                .list(snapshot, ctx, Family::Issues, &items::list_args(i))
                .map(|(v, s)| (O::IssuesList { value: v }, s)),
            OperationInput::Tasks(tops::Input::List(i)) => self
                .list(snapshot, ctx, Family::Tasks, &items::list_args(i))
                .map(|(v, s)| (O::TasksList { value: v }, s)),
            OperationInput::Features(fops::Input::Get(i)) => self
                .get(snapshot, ctx, Family::Features, &items::get_args(i))
                .map(|(v, s)| (O::FeaturesGet { value: v }, s)),
            OperationInput::Issues(iops::Input::Get(i)) => self
                .get(snapshot, ctx, Family::Issues, &items::get_args(i))
                .map(|(v, s)| (O::IssuesGet { value: v }, s)),
            OperationInput::Tasks(tops::Input::Get(i)) => self
                .get(snapshot, ctx, Family::Tasks, &items::get_args(i))
                .map(|(v, s)| (O::TasksGet { value: v }, s)),
            _ => Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "operation",
                None,
            )),
        };
        let (result, scope) = answered.map_err(|f| query_fault(f, "work", self.correlation()))?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(scope)),
            result,
        })
    }
}

impl CommandHandler for WorkService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let outcome = match command.input() {
            OperationInput::Workflows(i) => match i {
                wops::Input::CreateStatus(i) => self.create_status(tx, ctx, i),
                wops::Input::RenameStatus(i) => self.rename_status(tx, ctx, i),
                wops::Input::Order(i) => self.order_statuses(tx, ctx, i),
                wops::Input::ReassignAndRemove(i) => self.reassign_and_remove_status(tx, ctx, i),
                _ => Err(refuse(
                    DomainErrorCode::UnsupportedCapability,
                    "operation",
                    None,
                )),
            },
            OperationInput::NextWork(i) => match i {
                crate::operations::next_work::Input::ClaimProjectQueue(i) => self.allocate_project(
                    tx,
                    ctx,
                    i,
                    command.identity(),
                    command.schema_revision().0,
                ),
                crate::operations::next_work::Input::ClaimFeatureTask(i) => {
                    self.allocate_task(tx, ctx, i, command.identity(), command.schema_revision().0)
                }
                crate::operations::next_work::Input::ReleaseClaim(i) => {
                    self.release_allocation(tx, ctx, i)
                }
                _ => Err(refuse(
                    DomainErrorCode::UnsupportedCapability,
                    "operation",
                    None,
                )),
            },
            OperationInput::Features(i) => match i {
                fops::Input::Create(i) => {
                    self.create_features(tx, ctx, i, command.schema_revision().0)
                }
                fops::Input::EditPlan(i) => self.edit_plan(tx, ctx, i),
                fops::Input::Prioritize(i) => self.prioritize_features(tx, ctx, i),
                fops::Input::Dependencies(i) => self.feature_dependencies(tx, ctx, i),
                fops::Input::RequestPlanApproval(i) => self.request_plan_approval(tx, ctx, i),
                fops::Input::Edit(i) => self.edit_feature(tx, ctx, i, command.schema_revision().0),
                _ => Err(refuse(
                    DomainErrorCode::UnsupportedCapability,
                    "operation",
                    None,
                )),
            },
            OperationInput::Issues(i) => match i {
                iops::Input::Create(i) => self.create_issue(tx, ctx, i),
                iops::Input::Edit(i) => self.edit_issue(tx, ctx, i),
                iops::Input::Prioritize(i) => self.prioritize_issues(tx, ctx, i),
                _ => Err(refuse(
                    DomainErrorCode::UnsupportedCapability,
                    "operation",
                    None,
                )),
            },
            OperationInput::Tasks(i) => match i {
                tops::Input::Create(i) => self.create_task(tx, ctx, i),
                tops::Input::Edit(i) => self.edit_task(tx, ctx, i),
                tops::Input::Prioritize(i) => self.prioritize_tasks(tx, ctx, i),
                tops::Input::Move(i) => self.move_task(tx, ctx, i),
                tops::Input::Assign(i) => self.assign(tx, ctx, i),
                tops::Input::DependencyLinks(i) => self.task_dependencies(tx, ctx, i),
                tops::Input::AcceptanceEvidence(i) => self.acceptance_evidence(tx, ctx, i),
                tops::Input::SyncPlan(i) => self.sync_plan(tx, ctx, i, command.schema_revision().0),
                tops::Input::SetFeature(i) => self.set_feature(tx, ctx, i),
                _ => Err(refuse(
                    DomainErrorCode::UnsupportedCapability,
                    "operation",
                    None,
                )),
            },
            _ => Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "operation",
                None,
            )),
        };
        decided(tx, outcome)
    }
}
