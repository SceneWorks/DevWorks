//! Listing, reading, creating and editing work; workflow moves, assignment, acceptance evidence,
//! priority and queue order, and feature membership.
use super::model::{self, Detail, Extras, Noted, WorkBrief, active_of, link_id, verified};
use super::workflow::{self, DONE, semantic_of};
use super::{
    R, WorkService, check_artifact, check_revision, conflict, event, history_root, id_set, invalid,
    member_repositories, not_found, open_project, require_title, stale, status_of, visible_card,
    visible_feature, visible_project,
};
use crate::context::InvocationContext;
use crate::operations::{features as fops, issues as iops, tasks as tops};
use crate::ports::IdKind;
use crate::ports::storage::StopCause;
use crate::ports::storage::{
    ANY_QUEUE, BlockerFilter, FEATURE, MANUAL, NewEvent, PROJECT_QUEUE, ReadSnapshot, StorageError,
    StoredWork, TASK, VERIFIED, WORKFLOW_QUEUE, WorkFilter, WorkPosition, WorkRead, WorkSelection,
    WriteTransaction, feature_queue,
};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{
    AcceptanceCriterion, CriterionState, DetailLevel, FeaturePatch, HandoffRecord, MutationSummary,
    PageRequest, QueuePlacement, QueueSide, TouchRef, WorkComment, WorkItemType, WorkLink,
    WorkRecord,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Label, Page};
use crate::protocol::{EventId, Id, ProjectId, RevisionDimension, RootId};
use crate::services::cursor::CursorBinding;
use crate::services::runs::{control_of, ensure_control};
use crate::services::sessions::{Failure, rev};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

/// Which entry point a request came through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Features,
    Issues,
    Tasks,
}
impl Family {
    fn op(self, suffix: &str) -> String {
        match self {
            Self::Features => format!("features.{suffix}"),
            Self::Issues => format!("issues.{suffix}"),
            Self::Tasks => format!("tasks.{suffix}"),
        }
    }
}

/// A listing request of any family.
pub struct ListArgs {
    pub project_id: ProjectId,
    pub page: PageRequest,
    pub status_ids: Vec<Id>,
    pub repository_ids: Vec<Id>,
    pub text: Option<String>,
    pub full: bool,
}
/// A read request of any family.
pub struct GetArgs {
    pub work_id: Id,
    pub full: bool,
    pub detail_page: Option<PageRequest>,
}
macro_rules! args {
    ($($t:ty),*) => {$(
        impl From<&$t> for ListArgs {
            fn from(i: &$t) -> Self {
                Self {
                    project_id: i.project_id.clone(),
                    page: i.page.clone(),
                    status_ids: i.status_ids.0.clone(),
                    repository_ids: i.repository_ids.0.clone(),
                    text: i.text.as_ref().map(|t| t.0.clone()),
                    full: matches!(i.detail_level, DetailLevel::Full),
                }
            }
        }
    )*};
}
args!(fops::ListInput, iops::ListInput, tops::ListInput);
macro_rules! get_args {
    ($($t:ty),*) => {$(
        impl From<&$t> for GetArgs {
            fn from(i: &$t) -> Self {
                Self {
                    work_id: i.work_id.clone(),
                    full: matches!(i.detail_level, DetailLevel::Full),
                    detail_page: i.detail_page.clone(),
                }
            }
        }
    )*};
}
get_args!(fops::GetInput, iops::GetInput, tops::GetInput);
pub fn list_args<T>(i: &T) -> ListArgs
where
    for<'a> ListArgs: From<&'a T>,
{
    ListArgs::from(i)
}
pub fn get_args<T>(i: &T) -> GetArgs
where
    for<'a> GetArgs: From<&'a T>,
{
    GetArgs::from(i)
}

/// The issue view of a card: the same identity, tagged `Issue` whatever its membership (the
/// tag is a view and never establishes membership; `parentId` carries it).
pub(crate) fn issue_view(mut r: WorkRecord) -> WorkRecord {
    r.kind = crate::protocol::records::WorkKind::Issue;
    r
}

/// The live activation of the item when it is (or was) a root of its own.
fn live_execution(read: &dyn ReadSnapshot, w: &StoredWork) -> Result<Option<Id>, StorageError> {
    Ok(read
        .runs()
        .live_activation(&RootId(w.id.clone()))?
        .map(|a| a.id))
}
/// The wire record of `w` read from a snapshot.
pub(crate) fn snapshot_record(
    read: &dyn ReadSnapshot,
    w: &StoredWork,
    detail: Detail,
) -> Result<WorkRecord, StorageError> {
    let (readiness, pending_readiness) = if w.kind == FEATURE {
        (
            super::readiness::assessment(read.work(), w)?,
            super::readiness::pending_assessment(read.work(), w)?,
        )
    } else {
        (None, None)
    };
    let live = live_execution(read, w)?;
    model::record(
        read.work(),
        w,
        Extras {
            live_execution: live,
            readiness,
            pending_readiness,
            detail,
        },
    )
}
/// The wire record of `w` read inside a write transaction.
pub(crate) fn tx_record(tx: &mut dyn WriteTransaction, w: &StoredWork) -> R<WorkRecord> {
    let live = tx
        .runs()
        .live_activation(&RootId(w.id.clone()))?
        .map(|a| a.id);
    let (readiness, pending_readiness) = if w.kind == FEATURE {
        (
            super::readiness::assessment(tx.work(), w)?,
            super::readiness::pending_assessment(tx.work(), w)?,
        )
    } else {
        (None, None)
    };
    Ok(model::record(
        tx.work(),
        w,
        Extras {
            live_execution: live,
            readiness,
            pending_readiness,
            detail: Detail::Summary,
        },
    )?)
}

fn position(p: &str) -> Result<WorkPosition, StorageError> {
    let mut it = p.splitn(4, ':');
    let (Some(a), Some(b), Some(c), Some(d)) = (it.next(), it.next(), it.next(), it.next()) else {
        return Err(StorageError::CursorExpired);
    };
    let n = |s: &str| s.parse::<i64>().map_err(|_| StorageError::CursorExpired);
    Ok(WorkPosition {
        priority: n(a)?,
        manual_order: n(b)?,
        created_at_ms: n(c)?,
        id: Id::new(d).map_err(|_| StorageError::CursorExpired)?,
    })
}
fn position_text(p: &WorkPosition) -> String {
    format!(
        "{}:{}:{}:{}",
        p.priority,
        p.manual_order,
        p.created_at_ms,
        p.id.as_str()
    )
}

/// A full-detail page of `w`'s comments and handoffs from `cursor`. The cursor is bound to
/// the item and its revision (a change between pages expires it) and to its family's `get`.
fn detail_page(
    ctx: &InvocationContext,
    family: Family,
    w: &StoredWork,
    cursor: Option<&str>,
    limit: usize,
) -> R<Detail> {
    let binding = CursorBinding::new(ctx, &family.op("get"), &[w.id.as_str()], w.revision);
    let offset = match cursor {
        None => 0,
        Some(c) => binding
            .decode(c)?
            .parse::<usize>()
            .map_err(|_| StorageError::CursorExpired)?,
    };
    let brief = WorkBrief::of(w)?;
    let total = brief.comments.len().max(brief.handoffs.len());
    let end = offset.saturating_add(limit);
    Ok(Detail::Full {
        offset,
        limit,
        next: (end < total).then(|| binding.encode(&end.to_string())),
    })
}

/// The completion a card records in a status of `semantic` with `acceptance`.
pub(crate) fn completion_for(semantic: &str, acceptance: &[AcceptanceCriterion]) -> Option<String> {
    (semantic == DONE).then(|| {
        if verified(acceptance) {
            VERIFIED.to_owned()
        } else {
            MANUAL.to_owned()
        }
    })
}

/// Evidence IDs are recorded history links (evidence, review, outcome, Git or pull request
/// links) of the same project.
pub(crate) fn check_evidence(
    tx: &mut dyn WriteTransaction,
    project: &ProjectId,
    ids: &[Id],
) -> R<()> {
    const KINDS: [&str; 5] = ["evidence", "review", "outcome", "git", "pull_request"];
    for e in ids {
        match tx.sessions().link(e)? {
            Some(l) if &l.project_id == project && KINDS.contains(&l.kind.as_str()) => {}
            _ => return Err(not_found("evidence", e).into()),
        }
    }
    Ok(())
}

/// The repositories a card of `feature` may name: the feature's active approved repository
/// scope, or any project member before its first approval.
pub(crate) fn feature_scope(read: &dyn WorkRead, feature: &StoredWork) -> R<Option<Vec<Id>>> {
    let slot = read.feature_plan(&feature.id)?;
    Ok(active_of(slot.as_ref().and_then(|s| s.approved.as_ref()))
        .map(|a| a.draft.repository_ids.0.clone()))
}
fn check_scope(scope: &Option<Vec<Id>>, repos: &[Id], feature: &Id) -> R<()> {
    if let Some(allowed) = scope
        && repos.iter().any(|r| !allowed.contains(r))
    {
        return Err(conflict("plan_scope", feature));
    }
    Ok(())
}
/// A feature open to new members: not in a finished (Done or Closed) status.
pub(crate) fn open_feature(tx: &mut dyn WriteTransaction, f: &StoredWork) -> R<()> {
    let statuses = tx.work().statuses(&f.project_id)?;
    if f.status_id
        .as_ref()
        .and_then(|s| semantic_of(&statuses, s))
        .is_some_and(workflow::finished)
    {
        return Err(conflict("feature_closed", &f.id));
    }
    Ok(())
}

/// Advances the queues an ordering change touched (and the project's any-order revision).
pub(crate) fn bump(tx: &mut dyn WriteTransaction, project: &ProjectId, queues: &[String]) -> R<()> {
    for q in queues {
        tx.work().bump_queue(project, q)?;
    }
    tx.work().bump_queue(project, ANY_QUEUE)?;
    Ok(())
}

/// What a new card is.
pub(crate) struct NewCard<'a> {
    pub project: &'a ProjectId,
    pub feature: Option<&'a Id>,
    pub title: &'a str,
    pub description: &'a str,
    pub repositories: &'a [Id],
    pub status: Option<&'a Id>,
    pub priority: i64,
    pub caller_ref: Option<Id>,
    pub acceptance: Vec<AcceptanceCriterion>,
    pub task_type: Option<WorkItemType>,
    pub labels: Vec<Label>,
}

/// A planning card's normalized patch (TaskPatch and IssuePatch).
struct CardPatch {
    task_type: Option<WorkItemType>,
    clear_task_type: bool,
    labels: Option<Vec<Label>>,
    title: Option<String>,
    description: Option<String>,
    status_id: Option<Id>,
    repository_ids: Option<Vec<Id>>,
    goals: Option<Vec<String>>,
    acceptance: Option<Vec<AcceptanceCriterion>>,
    inherits: Option<Vec<Id>>,
    touches: Option<Vec<TouchRef>>,
    add_links: Option<Vec<WorkLink>>,
    remove_link_ids: Option<Vec<Id>>,
    add_comment: Option<WorkComment>,
    handoff: Option<HandoffRecord>,
    acceptance_report: Option<ArtifactRef>,
    priority: Option<i32>,
}

/// Metadata changes share the normal work revision and history. Label spelling and order
/// are authored values; uniqueness uses the same case-insensitive identity as allocation.
pub(crate) fn apply_metadata(
    brief: &mut WorkBrief,
    task_type: Option<&WorkItemType>,
    clear_task_type: bool,
    labels: Option<&[Label]>,
    changed: &mut Vec<&'static str>,
) -> R<bool> {
    if clear_task_type && task_type.is_some() {
        return Err(invalid("task_type", None));
    }
    let mut touched = false;
    if clear_task_type || task_type.is_some() {
        let next = if clear_task_type {
            None
        } else {
            task_type.cloned()
        };
        if serde_json::to_value(&brief.task_type).ok() != serde_json::to_value(&next).ok() {
            brief.task_type = next;
            changed.push("taskType");
            touched = true;
        }
    }
    if let Some(labels) = labels {
        let mut seen = BTreeSet::new();
        if labels.len() > 128
            || labels
                .iter()
                .any(|l| l.0.trim().is_empty() || !seen.insert(l.0.to_lowercase()))
        {
            return Err(invalid("labels", None));
        }
        if !brief
            .labels
            .iter()
            .map(|l| &l.0)
            .eq(labels.iter().map(|l| &l.0))
        {
            brief.labels = labels.to_vec();
            changed.push("labels");
            touched = true;
        }
    }
    Ok(touched)
}

impl WorkService {
    // -----------------------------------------------------------------------------------------
    // Reads

    pub(crate) fn list(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        family: Family,
        i: &ListArgs,
    ) -> R<(Page<WorkRecord>, i64)> {
        let p = visible_project(read.projects(), ctx, &i.project_id)?;
        let selection = match family {
            Family::Features => WorkSelection::Features,
            Family::Issues => WorkSelection::Standalone,
            Family::Tasks => WorkSelection::Tasks,
        };
        let status_ids = id_set(&i.status_ids, "status_ids")?;
        let repository_ids = id_set(&i.repository_ids, "repository_ids")?;
        let text = i.text.as_deref().map(model::fold).filter(|t| !t.is_empty());
        let filter = WorkFilter {
            project: p.id.clone(),
            selection,
            status_ids,
            repository_ids,
            text,
        };
        // A page is bound to the query, its filter, the project's scope revision and its order
        // revision: a reprioritization, reorder, insertion or membership change between pages
        // expires the cursor instead of skipping or repeating items.
        let order = read.work().queue_revision(&p.id, ANY_QUEUE)?;
        let statuses: Vec<&str> = filter.status_ids.iter().map(Id::as_str).collect();
        let repos: Vec<&str> = filter.repository_ids.iter().map(Id::as_str).collect();
        let op = family.op("list");
        let binding = CursorBinding::new(
            ctx,
            &op,
            &[
                p.id.0.as_str(),
                &statuses.join(","),
                &repos.join(","),
                filter.text.as_deref().unwrap_or(""),
                &order.to_string(),
                if i.full { "full" } else { "summary" },
            ],
            p.scope_revision,
        );
        let after = i
            .page
            .cursor
            .as_ref()
            .map(|c| binding.decode(&c.0).and_then(|s| position(&s)))
            .transpose()?;
        let limit = (i.page.limit.get() as usize).min(usize::from(crate::ports::storage::MAX_PAGE));
        let rows = read.work().works(&filter, after.as_ref(), limit as u16)?;
        let has_more = rows.len() == limit
            && !read
                .work()
                .works(&filter, Some(&WorkPosition::of(&rows[limit - 1])), 1)?
                .is_empty();
        let next = has_more.then(|| {
            BoundedText(binding.encode(&position_text(&WorkPosition::of(&rows[limit - 1]))))
        });
        let items = rows
            .iter()
            .map(|w| {
                // At full detail an item whose comments or handoffs exceed one page carries the
                // detail cursor its family's `get` continues from (never "more" without one).
                let detail = if i.full {
                    detail_page(ctx, family, w, None, 500)?
                } else {
                    Detail::Summary
                };
                snapshot_record(read, w, detail)
                    .map(|r| match family {
                        Family::Issues => issue_view(r),
                        _ => r,
                    })
                    .map_err(Failure::from)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok((
            Page {
                items: BoundedList(items),
                next_cursor: next,
                has_more,
            },
            p.scope_revision,
        ))
    }

    pub(crate) fn get(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        family: Family,
        i: &GetArgs,
    ) -> R<(WorkRecord, i64)> {
        let w = match family {
            Family::Features => visible_feature(read.work(), ctx, &i.work_id)?,
            _ => visible_card(read.work(), ctx, &i.work_id)?,
        };
        let p = visible_project(read.projects(), ctx, &w.project_id)?;
        let detail = match (&i.detail_page, i.full) {
            (None, false) => Detail::Summary,
            (page, _) => {
                let limit = page
                    .as_ref()
                    .map_or(500, |p| p.limit.get() as usize)
                    .min(500);
                let cursor = page
                    .as_ref()
                    .and_then(|p| p.cursor.as_ref())
                    .map(|c| c.0.clone());
                detail_page(ctx, family, &w, cursor.as_deref(), limit)?
            }
        };
        let record = snapshot_record(read, &w, detail)?;
        let record = match family {
            Family::Issues => issue_view(record),
            _ => record,
        };
        Ok((record, p.scope_revision))
    }

    // -----------------------------------------------------------------------------------------
    // Creation

    /// Inserts a card (a feature task or a standalone issue) and returns it.
    pub(crate) fn insert_card(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        c: NewCard<'_>,
    ) -> R<StoredWork> {
        require_title(c.title)?;
        if i32::try_from(c.priority).is_err() {
            return Err(invalid("priority", None));
        }
        let repos = member_repositories(tx, c.project, c.repositories)?;
        if let Some(f) = c.feature {
            let feature = visible_feature(tx.work(), ctx, f)?;
            if &feature.project_id != c.project {
                return Err(not_found("feature", f).into());
            }
            open_feature(tx, &feature)?;
            let scope = feature_scope(tx.work(), &feature)?;
            check_scope(&scope, &repos, f)?;
        }
        let status = match c.status {
            Some(s) => status_of(tx, c.project, s)?.0,
            None => workflow::default_status(tx, c.project)?,
        };
        let queue = match c.feature {
            Some(f) => feature_queue(f),
            None => PROJECT_QUEUE.to_owned(),
        };
        let now = tx.now_ms();
        let mut brief = WorkBrief {
            description: c.description.to_owned(),
            acceptance: c.acceptance,
            ..WorkBrief::default()
        };
        apply_metadata(
            &mut brief,
            c.task_type.as_ref(),
            false,
            Some(&c.labels),
            &mut Vec::new(),
        )?;
        let w = StoredWork {
            id: tx.new_id(IdKind::Link),
            project_id: c.project.clone(),
            kind: TASK.into(),
            parent_feature_id: c.feature.cloned(),
            status_id: Some(status.id.clone()),
            title: c.title.to_owned(),
            priority: c.priority,
            manual_order: tx.work().next_rank(c.project, &queue)?,
            revision: 1,
            scope_revision: 1,
            created_at_ms: now,
            updated_at_ms: now,
            created_by: Some(ctx.principal().clone()),
            updated_by: Some(ctx.principal().clone()),
            search_key: model::search_key(c.title, c.description),
            caller_ref: c.caller_ref,
            completion: completion_for(&status.semantic, &brief.acceptance),
            brief: brief.value(),
        };
        tx.work().insert_work(&w)?;
        tx.work().set_work_repositories(&w.id, &repos)?;
        bump(tx, c.project, &[queue])?;
        event(
            tx,
            ctx,
            &w,
            "WorkCreated",
            json!({
                "kind": "task",
                "featureId": c.feature.map(Id::as_str),
                "title": w.title,
                "statusId": status.id.as_str(),
                "priority": w.priority,
                "repositoryIds": repos.iter().map(Id::as_str).collect::<Vec<_>>(),
            }),
        )?;
        if let Some(f) = c.feature {
            self.notify_feature(tx, ctx, f, &w, "WorkLinkedToFeature")?;
        }
        Ok(w)
    }

    pub(crate) fn create_task(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &tops::CreateInput,
    ) -> R<DomainDecision> {
        open_project(tx, ctx, &i.project_id)?;
        let w = self.insert_card(
            tx,
            ctx,
            NewCard {
                project: &i.project_id,
                feature: i.parent_id.as_ref().map(|p| &p.0),
                title: &i.title.0,
                description: &i.description.0,
                repositories: &i.repository_ids.0,
                status: Some(&i.status_id),
                priority: i64::from(i.priority),
                caller_ref: None,
                acceptance: Vec::new(),
                task_type: i.task_type.clone(),
                labels: i.labels.as_ref().map(|l| l.0.clone()).unwrap_or_default(),
            },
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::TasksCreate {
                value: tx_record(tx, &w)?,
            },
        })
    }

    pub(crate) fn create_issue(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &iops::CreateInput,
    ) -> R<DomainDecision> {
        open_project(tx, ctx, &i.project_id)?;
        let w = self.insert_card(
            tx,
            ctx,
            NewCard {
                project: &i.project_id,
                feature: i.feature_id.as_ref().map(|p| &p.0),
                title: &i.title.0,
                description: &i.description.0,
                repositories: &i.repository_ids.0,
                status: Some(&i.status_id),
                priority: i64::from(i.priority),
                caller_ref: None,
                acceptance: Vec::new(),
                task_type: i.task_type.clone(),
                labels: i.labels.as_ref().map(|l| l.0.clone()).unwrap_or_default(),
            },
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::IssuesCreate {
                value: issue_view(tx_record(tx, &w)?),
            },
        })
    }

    /// A durable notice on the feature's history that `w` joined or left its delivery (the
    /// feature coordinator reads its root history; the delegation item delivers it to its inbox).
    pub(crate) fn notify_feature(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        feature: &Id,
        w: &StoredWork,
        kind: &str,
    ) -> R<()> {
        let Some(f) = tx.work().work(feature)? else {
            return Ok(());
        };
        let id = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id,
            actor: ctx.principal().clone(),
            project_id: Some(f.project_id.clone()),
            root_id: Some(RootId(f.id.clone())),
            operation_id: None,
            aggregate_kind: super::WORK_AGGREGATE.into(),
            aggregate_id: f.id.as_str().to_owned(),
            aggregate_revision: f.revision.max(1),
            kind: kind.into(),
            payload: json!({
                "workId": w.id.as_str(),
                "title": w.title,
                "principalKind": format!("{:?}", ctx.principal_kind()),
                "notifyCoordinator": true,
            }),
        })?;
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Edits

    fn edit_card(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        id: &Id,
        expected: i64,
        patch: CardPatch,
    ) -> R<(StoredWork, bool)> {
        let before = visible_card(tx.work(), ctx, id)?;
        check_revision(&before, expected)?;
        open_project(tx, ctx, &before.project_id)?;
        let mut after = before.clone();
        let mut brief = WorkBrief::of(&before)?;
        let mut changed: Vec<&str> = Vec::new();
        let mut queues: Vec<String> = Vec::new();
        let metadata_changed = apply_metadata(
            &mut brief,
            patch.task_type.as_ref(),
            patch.clear_task_type,
            patch.labels.as_deref(),
            &mut changed,
        )?;
        if let Some(t) = &patch.title {
            require_title(t)?;
            if t != &before.title {
                after.title = t.clone();
                changed.push("title");
            }
        }
        if let Some(d) = &patch.description
            && d != &brief.description
        {
            brief.description = d.clone();
            changed.push("description");
        }
        if let Some(r) = &patch.repository_ids {
            let repos = member_repositories(tx, &before.project_id, r)?;
            if let Some(f) = &before.parent_feature_id
                && let Some(feature) = tx.work().work(f)?
            {
                let scope = feature_scope(tx.work(), &feature)?;
                check_scope(&scope, &repos, f)?;
            }
            if repos != tx.work().work_repositories(&before.id)? {
                tx.work().set_work_repositories(&before.id, &repos)?;
                changed.push("repositoryIds");
            }
        }
        if let Some(g) = &patch.goals
            && g != &brief.goals
        {
            brief.goals = g.clone();
            changed.push("goals");
        }
        if let Some(a) = &patch.acceptance {
            let next = merge_acceptance(&brief.acceptance, a)?;
            let evidence: Vec<Id> = next
                .iter()
                .flat_map(|c| c.evidence_ids.0.iter().cloned())
                .collect();
            check_evidence(tx, &before.project_id, &evidence)?;
            if serde_json::to_value(&next).ok() != serde_json::to_value(&brief.acceptance).ok() {
                brief.acceptance = next;
                changed.push("acceptance");
            }
        }
        if let Some(inh) = &patch.inherits {
            let inh = id_set(inh, "inherits_requirement_ids")?;
            let Some(f) = &before.parent_feature_id else {
                return Err(conflict("inherits_without_feature", &before.id));
            };
            let reqs: Vec<Id> = tx
                .work()
                .feature_plan(f)?
                .and_then(|s| active_of(s.approved.as_ref()))
                .map(|a| {
                    a.draft
                        .requirements
                        .0
                        .iter()
                        .map(|r| r.id.clone())
                        .collect()
                })
                .unwrap_or_default();
            if let Some(unknown) = inh.iter().find(|r| !reqs.contains(r)) {
                return Err(not_found("requirement", unknown).into());
            }
            if inh != brief.inherits {
                brief.inherits = inh;
                changed.push("inheritsRequirementIds");
            }
        }
        if let Some(t) = &patch.touches {
            let members = tx.projects().members(&before.project_id)?;
            if let Some(bad) = t.iter().find(|x| !members.contains(&x.repository_id)) {
                return Err(not_found("repository", &bad.repository_id).into());
            }
            brief.touches = t.clone();
            changed.push("touches");
        }
        apply_links(
            tx,
            ctx,
            &before,
            &mut brief,
            &patch.add_links,
            &patch.remove_link_ids,
            &mut changed,
        )?;
        if let Some(c) = &patch.add_comment {
            add_comment(tx, ctx, &before, &mut brief, c)?;
            changed.push("comments");
        }
        if let Some(h) = &patch.handoff {
            if brief.handoffs.iter().any(|x| x.item.id == h.id) {
                return Err(conflict("duplicate_handoff", &h.id));
            }
            if brief.handoffs.len() >= super::MAX_NOTES {
                return Err(conflict("handoff_limit", &before.id));
            }
            for a in &h.evidence.0 {
                check_artifact(tx, ctx, &before.project_id, a)?;
            }
            brief.handoffs.push(Noted {
                item: h.clone(),
                actor: ctx.principal().clone(),
                at_ms: tx.now_ms(),
            });
            changed.push("handoffs");
        }
        if let Some(a) = &patch.acceptance_report {
            check_artifact(tx, ctx, &before.project_id, a)?;
            brief.acceptance_report = Some(a.clone());
            changed.push("acceptanceReport");
        }
        if let Some(p) = patch.priority
            && i64::from(p) != before.priority
        {
            // A direct priority patch is the same use case as bulk prioritization: it advances
            // the owner queue revision (its rank is kept).
            after.priority = i64::from(p);
            changed.push("priority");
            queues.push(before.queue());
        }
        let statuses_changed = if let Some(s) = &patch.status_id
            && Some(s) != before.status_id.as_ref()
        {
            // A patch carries no workflow revision, so it may change the status only while the
            // project's workflow is the one every client knows (the default, revision 1); once
            // the workflow changed, a status change goes through tasks.move, which checks it.
            if tx
                .work()
                .queue_revision(&before.project_id, WORKFLOW_QUEUE)?
                != 1
            {
                return Err(conflict("use_tasks_move", &before.id));
            }
            let (status, _) = status_of(tx, &before.project_id, s)?;
            after.status_id = Some(status.id.clone());
            changed.push("statusId");
            if before.parent_feature_id.is_some() {
                queues.push(before.queue());
            }
            Some(status)
        } else {
            None
        };
        if changed.is_empty() {
            return Ok((before, false));
        }
        if metadata_changed && !queues.contains(&before.queue()) {
            queues.push(before.queue());
        }
        let statuses = tx.work().statuses(&before.project_id)?;
        let semantic = after
            .status_id
            .as_ref()
            .and_then(|s| semantic_of(&statuses, s))
            .unwrap_or("")
            .to_owned();
        after.completion = completion_for(&semantic, &brief.acceptance);
        if statuses_changed.is_some() {
            if semantic == DONE {
                brief.completed_by = Some(ctx.principal().clone());
                brief.completed_at_ms = Some(tx.now_ms());
            } else {
                brief.completed_by = None;
                brief.completed_at_ms = None;
            }
        }
        after.search_key = model::search_key(&after.title, &brief.description);
        after.brief = brief.value();
        after.revision = before.revision + 1;
        after.updated_at_ms = tx.now_ms();
        after.updated_by = Some(ctx.principal().clone());
        tx.work().update_work(&after, before.revision)?;
        if !queues.is_empty() {
            super::items::bump(tx, &before.project_id, &queues)?;
        }
        event(
            tx,
            ctx,
            &after,
            "WorkEdited",
            json!({"changed": changed, "completion": after.completion}),
        )?;
        Ok((after, true))
    }

    pub(crate) fn edit_task(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &tops::EditInput,
    ) -> R<DomainDecision> {
        let c = &i.changes;
        let patch = CardPatch {
            task_type: c.task_type.clone(),
            clear_task_type: c.clear_task_type.unwrap_or(false),
            labels: c.labels.as_ref().map(|l| l.0.clone()),
            title: c.title.as_ref().map(|t| t.0.clone()),
            description: c.description.as_ref().map(|t| t.0.clone()),
            status_id: c.status_id.clone(),
            repository_ids: c.repository_ids.as_ref().map(|r| r.0.clone()),
            goals: c
                .goals
                .as_ref()
                .map(|g| g.0.iter().map(|x| x.0.clone()).collect()),
            acceptance: c.acceptance.as_ref().map(|a| a.0.clone()),
            inherits: c.inherits_requirement_ids.as_ref().map(|r| r.0.clone()),
            touches: c.touches.as_ref().map(|t| t.0.clone()),
            add_links: c.add_links.as_ref().map(|l| l.0.clone()),
            remove_link_ids: c.remove_link_ids.as_ref().map(|l| l.0.clone()),
            add_comment: c.add_comment.clone(),
            handoff: c.handoff.clone(),
            acceptance_report: c.acceptance_report.clone(),
            priority: c.priority,
        };
        let (w, changed) = self.edit_card(tx, ctx, &i.work_id, i.expected_revision.get(), patch)?;
        let value = tx_record(tx, &w)?;
        let result = OperationResult::TasksEdit { value };
        Ok(if changed {
            DomainDecision::Applied { result }
        } else {
            DomainDecision::NoChange { result }
        })
    }

    pub(crate) fn edit_issue(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &iops::EditInput,
    ) -> R<DomainDecision> {
        let c = &i.changes;
        let patch = CardPatch {
            task_type: c.task_type.clone(),
            clear_task_type: c.clear_task_type.unwrap_or(false),
            labels: c.labels.as_ref().map(|l| l.0.clone()),
            title: c.title.as_ref().map(|t| t.0.clone()),
            description: c.description.as_ref().map(|t| t.0.clone()),
            status_id: c.status_id.clone(),
            repository_ids: c.repository_ids.as_ref().map(|r| r.0.clone()),
            goals: c
                .goals
                .as_ref()
                .map(|g| g.0.iter().map(|x| x.0.clone()).collect()),
            acceptance: c.acceptance.as_ref().map(|a| a.0.clone()),
            inherits: None,
            touches: c.touches.as_ref().map(|t| t.0.clone()),
            add_links: c.add_links.as_ref().map(|l| l.0.clone()),
            remove_link_ids: c.remove_link_ids.as_ref().map(|l| l.0.clone()),
            add_comment: c.add_comment.clone(),
            handoff: c.handoff.clone(),
            acceptance_report: c.acceptance_report.clone(),
            priority: c.priority,
        };
        let (w, changed) = self.edit_card(tx, ctx, &i.work_id, i.expected_revision.get(), patch)?;
        let value = issue_view(tx_record(tx, &w)?);
        let result = OperationResult::IssuesEdit { value };
        Ok(if changed {
            DomainDecision::Applied { result }
        } else {
            DomainDecision::NoChange { result }
        })
    }

    pub(crate) fn edit_feature(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &fops::EditInput,
        schema_revision: u32,
    ) -> R<DomainDecision> {
        let before = visible_feature(tx.work(), ctx, &i.work_id)?;
        check_revision(&before, i.expected_revision.get())?;
        open_project(tx, ctx, &before.project_id)?;
        let c: &FeaturePatch = &i.changes;
        let mut after = before.clone();
        let mut brief = WorkBrief::of(&before)?;
        let mut changed: Vec<&str> = Vec::new();
        let metadata_changed = apply_metadata(
            &mut brief,
            None,
            false,
            c.labels.as_ref().map(|l| l.0.as_slice()),
            &mut changed,
        )?;
        let approved = tx
            .work()
            .feature_plan(&before.id)?
            .is_some_and(|s| s.approved.is_some());
        if let Some(t) = &c.title {
            require_title(&t.0)?;
            if t.0 != before.title {
                after.title = t.0.clone();
                changed.push("title");
            }
        }
        if let Some(d) = &c.description
            && d.0 != brief.description
        {
            brief.description = d.0.clone();
            changed.push("description");
        }
        let status_changed = if let Some(s) = &c.status_id
            && Some(s) != before.status_id.as_ref()
        {
            let (status, _) = status_of(tx, &before.project_id, s)?;
            after.status_id = Some(status.id);
            changed.push("statusId");
            true
        } else {
            false
        };
        if let Some(results) = &c.acceptance_evidence {
            if schema_revision < 2 {
                return Err(invalid("acceptance_evidence", None));
            }
            record_feature_results(tx, &before, &mut brief, &results.0)?;
            changed.push("acceptance");
        }
        // An approved feature's effective scope changes only through its pending plan.
        if let Some(r) = &c.repository_ids {
            let repos = member_repositories(tx, &before.project_id, &r.0)?;
            if repos != tx.work().work_repositories(&before.id)? {
                if approved {
                    return Err(conflict("plan_scope", &before.id));
                }
                tx.work().set_work_repositories(&before.id, &repos)?;
                changed.push("repositoryIds");
            }
        }
        if let Some(g) = &c.goals {
            let goals: Vec<String> = g.0.iter().map(|x| x.0.clone()).collect();
            if goals != brief.goals {
                if approved {
                    return Err(conflict("plan_scope", &before.id));
                }
                brief.goals = goals;
                changed.push("goals");
            }
        }
        apply_links(
            tx,
            ctx,
            &before,
            &mut brief,
            &c.add_links.as_ref().map(|l| l.0.clone()),
            &c.remove_link_ids.as_ref().map(|l| l.0.clone()),
            &mut changed,
        )?;
        if let Some(cm) = &c.add_comment {
            add_comment(tx, ctx, &before, &mut brief, cm)?;
            changed.push("comments");
        }
        let mut readiness_changed = false;
        if let Some(change) = &c.readiness {
            if schema_revision < 2 {
                return Err(invalid("readiness", None));
            }
            super::readiness::apply_change(tx, ctx, &before, change)?;
            readiness_changed = true;
        }
        if changed.is_empty() && !readiness_changed {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::FeaturesEdit {
                    value: tx_record(tx, &before)?,
                },
            });
        }
        // A feature in a Done status is Verified only when its active acceptance is met with
        // evidence; a Done label alone is Manual and satisfies no dependent.
        let semantic = {
            let statuses = tx.work().statuses(&before.project_id)?;
            after
                .status_id
                .as_ref()
                .and_then(|s| semantic_of(&statuses, s))
                .unwrap_or("")
                .to_owned()
        };
        after.completion = feature_completion(tx, &before.id, &semantic, &brief)?;
        if status_changed {
            if semantic == DONE {
                brief.completed_by = Some(ctx.principal().clone());
                brief.completed_at_ms = Some(tx.now_ms());
            } else {
                brief.completed_by = None;
                brief.completed_at_ms = None;
            }
        }
        after.search_key = model::search_key(&after.title, &brief.description);
        after.brief = brief.value();
        after.revision = before.revision + 1;
        after.updated_at_ms = tx.now_ms();
        after.updated_by = Some(ctx.principal().clone());
        tx.work().update_work(&after, before.revision)?;
        if metadata_changed {
            bump(tx, &before.project_id, &[before.queue()])?;
        }
        if readiness_changed {
            changed.push("readiness");
        }
        event(tx, ctx, &after, "WorkEdited", json!({"changed": changed}))?;
        Ok(DomainDecision::Applied {
            result: OperationResult::FeaturesEdit {
                value: tx_record(tx, &after)?,
            },
        })
    }

    // -----------------------------------------------------------------------------------------
    // Workflow, assignment and acceptance

    pub(crate) fn move_task(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &tops::MoveInput,
    ) -> R<DomainDecision> {
        let before = visible_card(tx.work(), ctx, &i.task_id)?;
        check_revision(&before, i.expected_revision.get())?;
        open_project(tx, ctx, &before.project_id)?;
        let workflow_revision = tx
            .work()
            .queue_revision(&before.project_id, WORKFLOW_QUEUE)?;
        if workflow_revision != i.expected_workflow_revision.get() {
            return Err(stale(
                RevisionDimension::Workflow,
                "workflow",
                &before.project_id.0,
                i.expected_workflow_revision.get(),
                workflow_revision,
            ));
        }
        let (status, _) = status_of(tx, &before.project_id, &i.status_id)?;
        if before.status_id.as_ref() == Some(&status.id) {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::TasksMove {
                    value: tx_record(tx, &before)?,
                },
            });
        }
        let mut brief = WorkBrief::of(&before)?;
        let mut after = before.clone();
        after.status_id = Some(status.id.clone());
        after.completion = completion_for(&status.semantic, &brief.acceptance);
        if status.semantic == DONE {
            brief.completed_by = Some(ctx.principal().clone());
            brief.completed_at_ms = Some(tx.now_ms());
        } else {
            brief.completed_by = None;
            brief.completed_at_ms = None;
        }
        after.brief = brief.value();
        after.revision = before.revision + 1;
        after.updated_at_ms = tx.now_ms();
        after.updated_by = Some(ctx.principal().clone());
        tx.work().update_work(&after, before.revision)?;
        // A feature's task-set revision covers its cards' workflow status (a re-plan closes the
        // unfinished cards it leaves out, so it must not act on a status it did not see).
        bump(tx, &before.project_id, &[before.queue()])?;
        super::allocation::invalidate_work(tx, ctx, &before.id)?;
        // A workflow move never changes execution: no pause, start, stop or completion of a run.
        event(
            tx,
            ctx,
            &after,
            "WorkMoved",
            json!({
                "fromStatusId": before.status_id.as_ref().map(Id::as_str),
                "toStatusId": status.id.as_str(),
                "semantic": status.semantic,
                "completion": after.completion,
            }),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::TasksMove {
                value: tx_record(tx, &after)?,
            },
        })
    }

    pub(crate) fn assign(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &tops::AssignInput,
    ) -> R<DomainDecision> {
        let before = visible_card(tx.work(), ctx, &i.task_id)?;
        check_revision(&before, i.expected_revision.get())?;
        open_project(tx, ctx, &before.project_id)?;
        let mut brief = WorkBrief::of(&before)?;
        let (assignment, agent) = match (&i.assignment_id, i.clear_assignment) {
            (Some(a), false) => {
                let Some(stored) = tx.runs().assignment(a)? else {
                    return Err(not_found("assignment", &a.0).into());
                };
                // The assignment is this card's, under its current root (its feature, or itself),
                // and still live.
                let root = history_root(&before);
                if stored.work_id.0 != before.id || stored.root_id.0 != root {
                    return Err(conflict("assignment_scope", &a.0));
                }
                if !stored.state.is_live() {
                    return Err(conflict("assignment_ended", &a.0));
                }
                let agent = tx
                    .sessions()
                    .session(&stored.session_id)?
                    .map(|s| s.agent_id.0);
                (Some(a.clone()), agent)
            }
            (None, true) => (None, None),
            _ => return Err(invalid("assignment", None)),
        };
        if brief.assignment_id == assignment {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::TasksAssign {
                    value: tx_record(tx, &before)?,
                },
            });
        }
        let previous = brief.assignment_id.clone();
        brief.assignment_id = assignment.clone();
        brief.assigned_agent_id = agent;
        let mut after = before.clone();
        after.brief = brief.value();
        after.revision = before.revision + 1;
        after.updated_at_ms = tx.now_ms();
        after.updated_by = Some(ctx.principal().clone());
        tx.work().update_work(&after, before.revision)?;
        event(
            tx,
            ctx,
            &after,
            "WorkAssigned",
            json!({
                "assignmentId": assignment.as_ref().map(|a| a.0.as_str()),
                "previousAssignmentId": previous.as_ref().map(|a| a.0.as_str()),
            }),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::TasksAssign {
                value: tx_record(tx, &after)?,
            },
        })
    }

    pub(crate) fn acceptance_evidence(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &tops::AcceptanceEvidenceInput,
    ) -> R<DomainDecision> {
        let before = visible_card(tx.work(), ctx, &i.task_id)?;
        check_revision(&before, i.expected_revision.get())?;
        open_project(tx, ctx, &before.project_id)?;
        let mut brief = WorkBrief::of(&before)?;
        let ids: Vec<Id> = i.criteria.0.iter().map(|c| c.id.clone()).collect();
        id_set(&ids, "criteria")?;
        let mut changes = Vec::new();
        for c in &i.criteria.0 {
            let Some(stored) = brief.acceptance.iter_mut().find(|s| s.id == c.id) else {
                return Err(not_found("criterion", &c.id).into());
            };
            // Evidence binds the criterion as written: its text cannot change here.
            if stored.text.0 != c.text.0 {
                return Err(conflict("criterion_text", &c.id));
            }
            id_set(&c.evidence_ids.0, "evidence_ids")?;
            check_evidence(tx, &before.project_id, &c.evidence_ids.0)?;
            stored.state = c.state.clone();
            stored.evidence_ids = c.evidence_ids.clone();
            changes.push(json!({
                "id": c.id.as_str(),
                "state": c.state,
                "evidence": c.evidence_ids.0.iter().map(Id::as_str).collect::<Vec<_>>(),
                // Met without evidence is a manual judgment, never verified acceptance.
                "verified": matches!(c.state, CriterionState::Met) && !c.evidence_ids.0.is_empty(),
            }));
        }
        let statuses = tx.work().statuses(&before.project_id)?;
        let semantic = before
            .status_id
            .as_ref()
            .and_then(|s| semantic_of(&statuses, s))
            .unwrap_or("")
            .to_owned();
        let mut after = before.clone();
        after.completion = completion_for(&semantic, &brief.acceptance);
        if serde_json::to_value(&brief.acceptance).ok()
            == WorkBrief::of(&before)
                .ok()
                .and_then(|b| serde_json::to_value(&b.acceptance).ok())
        {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::TasksAcceptanceEvidence {
                    value: tx_record(tx, &before)?,
                },
            });
        }
        after.brief = brief.value();
        after.revision = before.revision + 1;
        after.updated_at_ms = tx.now_ms();
        after.updated_by = Some(ctx.principal().clone());
        tx.work().update_work(&after, before.revision)?;
        event(
            tx,
            ctx,
            &after,
            "AcceptanceRecorded",
            json!({"criteria": changes, "completion": after.completion}),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::TasksAcceptanceEvidence {
                value: tx_record(tx, &after)?,
            },
        })
    }

    // -----------------------------------------------------------------------------------------
    // Priority and queue order

    /// Reorders one queue under its expected revision: the selected items (unique current
    /// members) take `priority` when one is given and move to the front of their priority
    /// bucket in the given order; every other item keeps its priority and relative order.
    #[allow(clippy::too_many_arguments)]
    fn reorder(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        project: &ProjectId,
        queue: &str,
        member: &dyn Fn(&StoredWork) -> bool,
        expected: i64,
        priority: Option<i32>,
        order: Order<'_>,
    ) -> R<MutationSummary> {
        open_project(tx, ctx, project)?;
        let current = tx.work().queue_revision(project, queue)?;
        if current != expected {
            return Err(stale(
                RevisionDimension::Queue,
                "queue",
                &project.0,
                expected,
                current,
            ));
        }
        if let Order::Ids(ordered) = &order {
            id_set(ordered, "ordered_ids")?;
        }
        let mut items: Vec<StoredWork> = Vec::new();
        let selection = if queue == PROJECT_QUEUE {
            vec![WorkSelection::Features, WorkSelection::Standalone]
        } else {
            let f = queue.strip_prefix("feature:").unwrap_or("");
            vec![WorkSelection::FeatureTasks(
                Id::new(f).map_err(|_| invalid("feature_id", None))?,
            )]
        };
        for s in selection {
            let filter = WorkFilter {
                project: project.clone(),
                selection: s,
                status_ids: Vec::new(),
                repository_ids: Vec::new(),
                text: None,
            };
            let mut after: Option<WorkPosition> = None;
            loop {
                let page = tx.work().works(&filter, after.as_ref(), 500)?;
                let more = page.len() == 500;
                after = page.last().map(WorkPosition::of);
                items.extend(page);
                if !more {
                    break;
                }
            }
        }
        // A placement is the whole bucket in its current order with one item moved next to its
        // anchor: the same reordering as a full ordered list, in one bounded request.
        let placed: Vec<Id>;
        let ordered: &[Id] = match &order {
            Order::Ids(ids) => ids,
            Order::Placement(m) => {
                placed = placement_order(tx, project, &items, member, priority, m)?;
                &placed
            }
        };
        let index: BTreeMap<&str, usize> = ordered
            .iter()
            .enumerate()
            .map(|(n, id)| (id.as_str(), n))
            .collect();
        // Placements already validated their two endpoints. Their computed order also includes
        // the other kinds in a mixed project bucket so those positions remain undisturbed.
        let supplied = match &order {
            Order::Ids(ids) => *ids,
            Order::Placement(_) => &[],
        };
        for id in supplied {
            match items.iter().find(|w| &w.id == id) {
                Some(w) if member(w) => {}
                Some(w) => return Err(conflict("queue_member", &w.id)),
                None => {
                    // Not in this queue: invisible or another project is NotFound; a card of
                    // another queue of the project is a conflict.
                    return match tx.work().work(id)? {
                        Some(w) if &w.project_id == project => Err(conflict("queue_member", id)),
                        _ => Err(not_found("work", id).into()),
                    };
                }
            }
        }
        let mut changed = false;
        for w in items.iter_mut() {
            if index.contains_key(w.id.as_str())
                && let Some(p) = priority
                && w.priority != i64::from(p)
            {
                let before = w.clone();
                w.priority = i64::from(p);
                w.revision = before.revision + 1;
                w.updated_at_ms = tx.now_ms();
                w.updated_by = Some(ctx.principal().clone());
                tx.work().update_work(w, before.revision)?;
                changed = true;
            }
        }
        items.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| {
                    let ka = index.get(a.id.as_str()).copied().unwrap_or(usize::MAX);
                    let kb = index.get(b.id.as_str()).copied().unwrap_or(usize::MAX);
                    ka.cmp(&kb)
                })
                .then(a.manual_order.cmp(&b.manual_order))
                .then(a.created_at_ms.cmp(&b.created_at_ms))
                .then(a.id.as_str().cmp(b.id.as_str()))
        });
        for (n, w) in items.iter().enumerate() {
            let rank = n as i64 + 1;
            if w.manual_order != rank {
                tx.work().set_rank(&w.id, rank)?;
                changed = true;
            }
        }
        let revision = if changed {
            bump(tx, project, &[queue.to_owned()])?;
            let revision = tx.work().queue_revision(project, queue)?;
            for w in items.iter().filter(|w| index.contains_key(w.id.as_str())) {
                event(
                    tx,
                    ctx,
                    w,
                    "WorkPrioritized",
                    json!({
                        "queue": queue,
                        "queueRevision": revision,
                        "priority": w.priority,
                        "position": index.get(w.id.as_str()),
                    }),
                )?;
            }
            let id = EventId(tx.new_id(IdKind::Event));
            tx.history().append(NewEvent {
                id,
                actor: ctx.principal().clone(),
                project_id: Some(project.clone()),
                root_id: None,
                operation_id: None,
                aggregate_kind: super::QUEUE_AGGREGATE.into(),
                aggregate_id: project.0.as_str().to_owned(),
                aggregate_revision: revision,
                kind: "QueueReordered".into(),
                payload: json!({
                    "queue": queue,
                    "priority": priority,
                    "orderedIds": match &order {
                        Order::Ids(_) => json!(ordered.iter().map(Id::as_str).collect::<Vec<_>>()),
                        Order::Placement(_) => serde_json::Value::Null,
                    },
                    "placement": match &order {
                        Order::Placement(m) => json!({"workId": m.work_id.as_str(),
                            "anchorId": m.anchor_id.as_str(), "side": format!("{:?}", m.side)}),
                        Order::Ids(_) => serde_json::Value::Null,
                    },
                    "principalKind": format!("{:?}", ctx.principal_kind()),
                }),
            })?;
            revision
        } else {
            current
        };
        Ok(MutationSummary {
            id: project.0.clone(),
            revision: rev(revision),
            changed,
        })
    }

    fn summary(changed: bool, result: OperationResult) -> DomainDecision {
        if changed {
            DomainDecision::Applied { result }
        } else {
            DomainDecision::NoChange { result }
        }
    }

    pub(crate) fn prioritize_tasks(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &tops::PrioritizeInput,
    ) -> R<DomainDecision> {
        let (queue, feature) = match &i.feature_id {
            Some(f) => {
                let feature = visible_feature(tx.work(), ctx, &f.0)?;
                if feature.project_id != i.project_id {
                    return Err(not_found("feature", &f.0).into());
                }
                (feature_queue(&f.0), Some(f.0.clone()))
            }
            None => (PROJECT_QUEUE.to_owned(), None),
        };
        let member = move |w: &StoredWork| {
            w.kind != FEATURE && w.parent_feature_id.as_ref() == feature.as_ref()
        };
        let m = self.reorder(
            tx,
            ctx,
            &i.project_id,
            &queue,
            &member,
            i.expected_queue_revision.get(),
            Some(i.priority),
            Order::of(i.ordered_task_ids.as_ref(), i.placement.as_ref())?,
        )?;
        Ok(Self::summary(
            m.changed,
            OperationResult::TasksPrioritize { value: m },
        ))
    }

    pub(crate) fn prioritize_issues(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &iops::PrioritizeInput,
    ) -> R<DomainDecision> {
        let member = |w: &StoredWork| w.kind != FEATURE && w.parent_feature_id.is_none();
        let m = self.reorder(
            tx,
            ctx,
            &i.project_id,
            PROJECT_QUEUE,
            &member,
            i.expected_queue_revision.get(),
            None,
            Order::of(i.ordered_work_ids.as_ref(), i.placement.as_ref())?,
        )?;
        Ok(Self::summary(
            m.changed,
            OperationResult::IssuesPrioritize { value: m },
        ))
    }

    pub(crate) fn prioritize_features(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &fops::PrioritizeInput,
    ) -> R<DomainDecision> {
        let member = |w: &StoredWork| w.kind == FEATURE;
        let m = self.reorder(
            tx,
            ctx,
            &i.project_id,
            PROJECT_QUEUE,
            &member,
            i.expected_queue_revision.get(),
            None,
            Order::of(i.ordered_work_ids.as_ref(), i.placement.as_ref())?,
        )?;
        Ok(Self::summary(
            m.changed,
            OperationResult::FeaturesPrioritize { value: m },
        ))
    }

    // -----------------------------------------------------------------------------------------
    // Membership

    pub(crate) fn set_feature(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &tops::SetFeatureInput,
    ) -> R<DomainDecision> {
        let before = visible_card(tx.work(), ctx, &i.task_id)?;
        check_revision(&before, i.expected_revision.get())?;
        open_project(tx, ctx, &before.project_id)?;
        let target = match (&i.feature_id, i.clear_feature) {
            (Some(f), false) => {
                let feature = visible_feature(tx.work(), ctx, &f.0)?;
                if feature.project_id != before.project_id {
                    return Err(not_found("feature", &f.0).into());
                }
                open_feature(tx, &feature)?;
                // A card joins within the feature's approved repository scope.
                let scope = feature_scope(tx.work(), &feature)?;
                let repos = tx.work().work_repositories(&before.id)?;
                check_scope(&scope, &repos, &f.0)?;
                Some(f.0.clone())
            }
            (None, true) => None,
            _ => return Err(invalid("feature", None)),
        };
        if target == before.parent_feature_id {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::TasksSetFeature {
                    value: tx_record(tx, &before)?,
                },
            });
        }
        super::allocation::invalidate_work(tx, ctx, &before.id)?;
        let mut after = before.clone();
        after.parent_feature_id = target.clone();
        let queue = after.queue();
        after.manual_order = tx.work().next_rank(&before.project_id, &queue)?;
        after.revision = before.revision + 1;
        after.updated_at_ms = tx.now_ms();
        after.updated_by = Some(ctx.principal().clone());
        tx.work().update_work(&after, before.revision)?;
        bump(tx, &before.project_id, &[before.queue(), queue])?;
        // Clearing membership installs a durable human pause on the card: its work stops and
        // stays held until an explicit, authorized Continue (relinking never clears it). Linking
        // and direct transfers keep every existing hold and create none.
        let pause = if target.is_none() {
            let root = crate::ports::storage::WorkRoot {
                id: RootId(after.id.clone()),
                project_id: after.project_id.clone(),
                kind: TASK.into(),
                title: after.title.clone(),
                revision: after.revision,
                scope_revision: after.scope_revision,
                parent_feature_id: None,
                created_at_ms: after.created_at_ms,
            };
            let control = ensure_control(tx, &root.id)?;
            let (_, hold) = self.runs().pause_root(
                tx,
                ctx.principal(),
                &root,
                &control,
                StopCause::Pause,
                "removed from its feature: work stays paused until it is continued",
            )?;
            Some(hold.id)
        } else {
            None
        };
        // A card linked while its new feature's stop is still pending is covered by that stop:
        // the feature's Stop reaches every linked card, and settles only once it quiesced.
        let stop_extended = match &target {
            Some(f) => self.runs().card_linked(
                tx,
                ctx.principal(),
                &RootId(f.clone()),
                &RootId(after.id.clone()),
            )?,
            None => false,
        };
        let kept: Vec<String> = crate::services::runs::active_blockers(
            tx.runs(),
            &BlockerFilter::RootLevel(RootId(after.id.clone())),
        )?
        .iter()
        .filter(|b| Some(&b.id) != pause.as_ref())
        .map(|b| b.id.as_str().to_owned())
        .collect();
        let control = control_of(tx.runs(), &RootId(after.id.clone()))?;
        event(
            tx,
            ctx,
            &after,
            "FeatureMembershipChanged",
            json!({
                "fromFeatureId": before.parent_feature_id.as_ref().map(Id::as_str),
                "toFeatureId": target.as_ref().map(Id::as_str),
                "transfer": before.parent_feature_id.is_some() && target.is_some(),
                "pauseHoldId": pause.as_ref().map(Id::as_str),
                "featureStopExtended": stop_extended,
                "keptHoldIds": kept,
                "controlRevision": control.revision,
                "deliveryAuthorizedWithoutPlanApproval": target.is_some(),
            }),
        )?;
        if let Some(f) = &before.parent_feature_id {
            // A requirement only this card covered has lost its delivery: an open readiness
            // finding of that requirement (its coverage is reconciled in the next draft saved).
            if let Some(feature) = tx.work().work(f)? {
                super::readiness::coverage_lost(tx, ctx, &feature, &after.id)?;
            }
            self.notify_feature(tx, ctx, f, &after, "WorkLeftFeature")?;
        }
        if let Some(f) = &target {
            self.notify_feature(tx, ctx, f, &after, "WorkLinkedToFeature")?;
        }
        Ok(DomainDecision::Applied {
            result: OperationResult::TasksSetFeature {
                value: tx_record(tx, &after)?,
            },
        })
    }
}

/// The completion of a feature in a status of `semantic`: Verified when its active acceptance
/// (criteria and tests) is met with evidence, Manual otherwise; none outside a Done status.
pub(crate) fn feature_completion(
    tx: &mut dyn WriteTransaction,
    feature: &Id,
    semantic: &str,
    brief: &WorkBrief,
) -> R<Option<String>> {
    if semantic != DONE {
        return Ok(None);
    }
    let active = active_of(
        tx.work()
            .feature_plan(feature)?
            .as_ref()
            .and_then(|s| s.approved.as_ref()),
    );
    Ok(Some(if model::feature_verified(active.as_ref(), brief) {
        VERIFIED.to_owned()
    } else {
        MANUAL.to_owned()
    }))
}

/// Records results of a feature's active-plan acceptance criteria or tests: each names one
/// criterion by ID at its approved text, and its evidence IDs are recorded history links of the
/// project. A criterion met without evidence stays a manual judgment.
fn record_feature_results(
    tx: &mut dyn WriteTransaction,
    feature: &StoredWork,
    brief: &mut WorkBrief,
    results: &[AcceptanceCriterion],
) -> R<()> {
    let ids: Vec<Id> = results.iter().map(|c| c.id.clone()).collect();
    id_set(&ids, "acceptance_evidence")?;
    let Some(active) = tx
        .work()
        .feature_plan(&feature.id)?
        .and_then(|s| active_of(s.approved.as_ref()))
    else {
        return Err(conflict("no_active_plan", &feature.id));
    };
    for r in results {
        let in_acceptance = active.draft.acceptance.0.iter().find(|c| c.id == r.id);
        let in_tests = active
            .draft
            .acceptance_tests
            .0
            .iter()
            .find(|c| c.id == r.id);
        let (approved, list) = match (in_acceptance, in_tests) {
            (Some(c), None) => (c, &mut brief.acceptance),
            (None, Some(c)) => (c, &mut brief.acceptance_tests),
            (Some(_), Some(_)) => return Err(invalid("ambiguous_criterion", Some(&r.id))),
            (None, None) => return Err(not_found("criterion", &r.id).into()),
        };
        if approved.text.0 != r.text.0 {
            return Err(conflict("criterion_text", &r.id));
        }
        id_set(&r.evidence_ids.0, "evidence_ids")?;
        check_evidence(tx, &feature.project_id, &r.evidence_ids.0)?;
        list.retain(|x| x.id != r.id);
        list.push(r.clone());
    }
    Ok(())
}

/// The acceptance criteria after an edit: a criterion keeping its ID and text keeps its stored
/// state and evidence (retained behavior); a new or reworded criterion takes what was given.
fn merge_acceptance(
    stored: &[AcceptanceCriterion],
    given: &[AcceptanceCriterion],
) -> R<Vec<AcceptanceCriterion>> {
    let ids: Vec<Id> = given.iter().map(|c| c.id.clone()).collect();
    id_set(&ids, "acceptance")?;
    Ok(given
        .iter()
        .map(|g| match stored.iter().find(|s| s.id == g.id) {
            Some(s) if s.text.0 == g.text.0 => s.clone(),
            _ => g.clone(),
        })
        .collect())
}

fn apply_links(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    w: &StoredWork,
    brief: &mut WorkBrief,
    add: &Option<Vec<WorkLink>>,
    remove: &Option<Vec<Id>>,
    changed: &mut Vec<&str>,
) -> R<()> {
    if let Some(remove) = remove {
        for id in remove {
            let before = brief.links.len();
            brief.links.retain(|l| &link_id(l) != id);
            if brief.links.len() == before {
                return Err(not_found("link", id).into());
            }
        }
        if !remove.is_empty() {
            changed.push("links");
        }
    }
    if let Some(add) = add {
        for l in add {
            let targets = usize::from(l.target_id.is_some())
                + usize::from(l.url.is_some())
                + usize::from(l.artifact.is_some());
            if targets != 1 {
                return Err(invalid("link", None));
            }
            if let Some(a) = &l.artifact {
                check_artifact(tx, ctx, &w.project_id, a)?;
            }
            // A link is a reference only: it never changes membership or grants access.
            let id = link_id(l);
            if brief.links.iter().any(|x| link_id(x) == id) {
                return Err(conflict("duplicate_link", &id));
            }
            if brief.links.len() >= 128 {
                return Err(conflict("link_limit", &w.id));
            }
            brief.links.push(l.clone());
        }
        if !add.is_empty() && !changed.contains(&"links") {
            changed.push("links");
        }
    }
    Ok(())
}

fn add_comment(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    w: &StoredWork,
    brief: &mut WorkBrief,
    c: &WorkComment,
) -> R<()> {
    if brief.comments.iter().any(|x| x.item.id == c.id) {
        return Err(conflict("duplicate_comment", &c.id));
    }
    if brief.comments.len() >= super::MAX_NOTES {
        return Err(conflict("comment_limit", &w.id));
    }
    for a in &c.evidence.0 {
        check_artifact(tx, ctx, &w.project_id, a)?;
    }
    brief.comments.push(Noted {
        item: c.clone(),
        actor: ctx.principal().clone(),
        at_ms: tx.now_ms(),
    });
    Ok(())
}

/// How a prioritization orders its queue: a list of IDs placed first in their bucket, or one
/// placement (schema revision 2) — exactly one of them.
pub(crate) enum Order<'a> {
    Ids(&'a [Id]),
    Placement(&'a QueuePlacement),
}
impl<'a> Order<'a> {
    fn of(
        ids: Option<&'a BoundedList<Id, 1, 128>>,
        placement: Option<&'a QueuePlacement>,
    ) -> R<Self> {
        match (ids, placement) {
            (Some(ids), None) => Ok(Order::Ids(&ids.0)),
            (None, Some(m)) => Ok(Order::Placement(m)),
            _ => Err(invalid("placement", None)),
        }
    }
}

/// The bucket of a placement's anchor in its current queue order, with the moved item taken out
/// and put immediately before or after the anchor. Both must be members of the queue; the
/// anchor's priority is the bucket (a task prioritization names it: they must agree), and an
/// item of another bucket moves into it only where its priority is set (tasks).
fn placement_order(
    tx: &mut dyn WriteTransaction,
    project: &ProjectId,
    items: &[StoredWork],
    member: &dyn Fn(&StoredWork) -> bool,
    priority: Option<i32>,
    m: &QueuePlacement,
) -> R<Vec<Id>> {
    if m.work_id == m.anchor_id {
        return Err(invalid("placement", Some(&m.work_id)));
    }
    let find = |tx: &mut dyn WriteTransaction, id: &Id| -> R<StoredWork> {
        match items.iter().find(|w| &w.id == id) {
            Some(w) if member(w) => Ok(w.clone()),
            Some(w) => Err(conflict("queue_member", &w.id)),
            None => match tx.work().work(id)? {
                Some(w) if &w.project_id == project => Err(conflict("queue_member", id)),
                _ => Err(not_found("work", id).into()),
            },
        }
    };
    let moved = find(tx, &m.work_id)?;
    let anchor = find(tx, &m.anchor_id)?;
    let bucket = anchor.priority;
    if let Some(p) = priority
        && i64::from(p) != bucket
    {
        return Err(conflict("priority_bucket", &m.anchor_id));
    }
    if priority.is_none() && moved.priority != bucket {
        return Err(conflict("priority_bucket", &m.work_id));
    }
    let mut order: Vec<&StoredWork> = items
        .iter()
        .filter(|w| w.priority == bucket && w.id != moved.id)
        .collect();
    order.sort_by(|a, b| {
        a.manual_order
            .cmp(&b.manual_order)
            .then(a.created_at_ms.cmp(&b.created_at_ms))
            .then(a.id.as_str().cmp(b.id.as_str()))
    });
    let at = order
        .iter()
        .position(|w| w.id == anchor.id)
        .expect("the anchor is in its bucket");
    let at = match m.side {
        QueueSide::Before => at,
        QueueSide::After => at + 1,
    };
    let mut ids: Vec<Id> = order.iter().map(|w| w.id.clone()).collect();
    ids.insert(at, moved.id.clone());
    Ok(ids)
}
