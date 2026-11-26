//! Typed frames of the work operations the Features, board, Issues and Overview screens bind
//! (DW-12), and the decoding of their replies. Every frame is the same wire envelope an agent
//! tool submits; nothing here decides anything the work service decides.
use crate::backend::Backend;
use crate::frame::{self, Negotiate};
use crate::ops::{Failure, PAGE_LIMIT, command_result, query_result, unexpected};
use devworks_core::gateway::{CommandOutcome, QueryOutcome};
use devworks_core::operations::{
    OperationInput as In, agents, features, history, issues, next_work, runs, tasks, workflows,
};
use devworks_core::protocol::records::{
    AllocationClaimRecord, AllocationQueueSnapshot, AllocationSelection, DetailLevel,
    MutationSummary, ProposalRecord, RetainedAckResult, RetainedEventPage, RetainedHistoryPage,
    RetainedInboxResult, RetainedPostResult, RootSnapshot, StatusRecord, WorkRecord,
};
use devworks_core::protocol::results::OperationResult;
use devworks_core::protocol::support::BoundedList;
use devworks_core::protocol::{Cause, Event};
use devworks_core::protocol::{
    CommandRequest, HoldRef, ProjectId, Query, QueryRequest, RootCommand, RootId,
};

/// The work family a list or get request reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Features,
    Issues,
    Tasks,
}

/// The requests the work screens (Overview, Features and Issues) wait for. The names are
/// persisted (see [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum WorkSlot {
    KeepWorking(String),
    /// Every feature of a project (all pages, read in order).
    WorkFeatures(String),
    /// The complete authoritative workflow, fenced by load generation.
    WorkWorkflow(String, u64),
    WorkWorkflowCommand(String),
    /// A queue snapshot; a lease requests a claim after the read completes.
    AllocationQueue(String, Option<String>, Option<u64>),
    AllocationClaim(String, Option<String>),
    AllocationInspect(String),
    AllocationRelease(String),
    /// Every card (feature task or standalone issue) of a project.
    WorkCards(String),
    /// The project change feed (Activity) of a project.
    WorkActivity(String, Option<String>, u64),
    WorkMessagesHistory(String),
    WorkMessagesInbox(String),
    WorkMessagesEvents(String),
    WorkMessagePost(String),
    WorkMessageAck(String, String),
    /// One work item's current record (readiness included).
    WorkDetail(String),
    WorkGit(String),
    CodeReviews(String, bool),
    CodeReviewFindings(String, String, bool),
    CodeReviewFindingCommand(String, String),
    ReadinessHistory(String, u64),
    /// A page of one root's linked history: its lineage, evidence links and model and tool
    /// usage (the list request's generation).
    WorkLineage(String, u64),
    WorkEvidence(String, u64),
    WorkToolUsage(String, u64),
    WorkGitInspect(String, String),
    /// A change of one work item (one at a time per item).
    WorkCommand(String),
    /// A new feature, task or issue in a project.
    WorkCreate(String),
    /// A prioritization of one queue (`<project>/<queue>`).
    WorkOrder(String),
    /// A plan text stored as an artifact for a feature's pending plan.
    WorkPlanText(String),
    /// Project-owned decisions for requirements dropped by a plan save.
    WorkPlanDrop(String),
    /// A feature's plan draft save (`features.edit_plan`).
    WorkPlan(String),
    /// A root's run controls (inspect) and one Start/Pause/Continue.
    WorkInspect(String),
    WorkRun(String),
    /// The roots and running work of a project an archival would stop (features, issues).
    ArchiveCount(String, bool),
    /// Work opened from elsewhere (a completion card): read as a card (`false`) or a feature.
    WorkOpen(String, bool),
    GithubIssuePublish(String),
    GithubIssueLink(String),
    GithubPullChecks(String),
    GithubPullReviews(String),
}
crate::backend::screen_slot!(WorkSlot, Work);

pub fn message_history(backend: &dyn Backend, project: &str, before: Option<i64>) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Agents(agents::Input::History(agents::HistoryInput {
            project_id: ProjectId(frame::id(project)?),
            channel: None,
            item_id: None,
            feature_id: None,
            resource_name: None,
            open_only: None,
            to_human: None,
            before: frame::lift(before.map(frame::number))?,
            limit: Some(frame::number(100)?),
        })))
    })
}

pub fn message_history_page(outcome: QueryOutcome) -> Result<RetainedHistoryPage, Failure> {
    match query_result(outcome)? {
        (OperationResult::AgentsHistory { value }, _) => Ok(value),
        (other, _) => Err(unexpected("agents.history", &other)),
    }
}

pub fn message_inbox(backend: &dyn Backend, project: &str, cursor: Option<&str>) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Agents(agents::Input::Receive(agents::ReceiveInput {
            session_id: None,
            project_id: Some(ProjectId(frame::id(project)?)),
            cursor: frame::lift(cursor.map(frame::text))?,
            include_read: None,
            limit: Some(frame::number(100)?),
            wait_seconds: Some(0),
        })))
    })
}

pub fn message_inbox_page(outcome: CommandOutcome) -> Result<RetainedInboxResult, Failure> {
    match command_result(outcome)? {
        OperationResult::AgentsReceive { value } => Ok(value),
        other => Err(unexpected("agents.receive", &other)),
    }
}

/// `agents.post`. A post that did not convert (`None`) is sent as no bytes, which the gateway
/// rejects like any malformed request.
pub fn message_post(backend: &dyn Backend, input: impl Into<Option<In>>) -> Vec<u8> {
    frame::build(backend, Negotiate::Latest, input.into())
}

pub fn message_posted(outcome: CommandOutcome) -> Result<RetainedPostResult, Failure> {
    match command_result(outcome)? {
        OperationResult::AgentsPost { value } => Ok(value),
        other => Err(unexpected("agents.post", &other)),
    }
}

pub fn message_ack(
    backend: &dyn Backend,
    project: &str,
    message: &str,
    resolution: Option<&str>,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Agents(agents::Input::Ack(agents::AckInput {
            session_id: None,
            project_id: Some(ProjectId(frame::id(project)?)),
            messages: Some(BoundedList(vec![frame::id(message)?])),
            up_to: None,
            resolution: frame::lift(resolution.map(frame::text))?,
        })))
    })
}

pub fn message_acked(outcome: CommandOutcome) -> Result<RetainedAckResult, Failure> {
    match command_result(outcome)? {
        OperationResult::AgentsAck { value } => Ok(value),
        other => Err(unexpected("agents.ack", &other)),
    }
}

pub fn message_events(backend: &dyn Backend, project: &str, after: Option<i64>) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Agents(agents::Input::Events(agents::EventsInput {
            project_id: ProjectId(frame::id(project)?),
            after: frame::lift(after.map(frame::number))?,
            limit: Some(frame::number(100)?),
        })))
    })
}

pub fn message_events_page(outcome: QueryOutcome) -> Result<RetainedEventPage, Failure> {
    match query_result(outcome)? {
        (OperationResult::AgentsEvents { value }, _) => Ok(value),
        (other, _) => Err(unexpected("agents.events", &other)),
    }
}

/// A patch or request that sets nothing; the caller sets the members it changes.
pub(crate) fn empty<T: serde::de::DeserializeOwned>() -> Option<T> {
    serde_json::from_str("{}").ok()
}

/// A page of the list of `family` in `project`.
pub fn work_list(
    backend: &dyn Backend,
    family: Family,
    project: &str,
    cursor: Option<&str>,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        let project_id = ProjectId(frame::id(project)?);
        let page = frame::page(PAGE_LIMIT.into(), cursor)?;
        let (status_ids, repository_ids) = (BoundedList(Vec::new()), BoundedList(Vec::new()));
        let detail_level = DetailLevel::Summary;
        Some(match family {
            Family::Features => In::Features(features::Input::List(features::ListInput {
                project_id,
                page,
                status_ids,
                repository_ids,
                text: None,
                detail_level,
            })),
            Family::Issues => In::Issues(issues::Input::List(issues::ListInput {
                project_id,
                page,
                status_ids,
                repository_ids,
                text: None,
                detail_level,
            })),
            Family::Tasks => In::Tasks(tasks::Input::List(tasks::ListInput {
                project_id,
                page,
                status_ids,
                repository_ids,
                text: None,
                detail_level,
            })),
        })
    })
}
/// One page of work records and the cursor of the next page.
pub fn work_page(outcome: QueryOutcome) -> Result<(Vec<WorkRecord>, Option<String>), Failure> {
    match query_result(outcome)? {
        (
            OperationResult::FeaturesList { value }
            | OperationResult::IssuesList { value }
            | OperationResult::TasksList { value },
            _,
        ) => Ok((value.items.0, value.next_cursor.map(|c| c.0))),
        (other, _) => Err(unexpected("work list", &other)),
    }
}
/// The get of one item of `family` (schema revision 3, including readiness and metadata).
pub fn work_get(backend: &dyn Backend, family: Family, work: &str) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        let work_id = frame::id(work)?;
        let detail_level = DetailLevel::Summary;
        Some(match family {
            Family::Features => In::Features(features::Input::Get(features::GetInput {
                work_id,
                detail_level,
                detail_page: None,
            })),
            Family::Issues => In::Issues(issues::Input::Get(issues::GetInput {
                work_id,
                detail_level,
                detail_page: None,
            })),
            Family::Tasks => In::Tasks(tasks::Input::Get(tasks::GetInput {
                work_id,
                detail_level,
                detail_page: None,
            })),
        })
    })
}
pub fn work_record(outcome: QueryOutcome) -> Result<WorkRecord, Failure> {
    match query_result(outcome)? {
        (
            OperationResult::FeaturesGet { value }
            | OperationResult::IssuesGet { value }
            | OperationResult::TasksGet { value },
            _,
        ) => Ok(value),
        (other, _) => Err(unexpected("work get", &other)),
    }
}
/// A work command. Work records use the newest revision (authored metadata); queue placement
/// keeps revision 2 and plan-approval requests keep their original revision. A command that did
/// not convert (`None`) is sent as no bytes, which the gateway rejects like any malformed
/// request.
pub fn work_command(backend: &dyn Backend, input: impl Into<Option<In>>) -> Vec<u8> {
    let input = input.into();
    let negotiate = match input.as_ref().map(In::operation) {
        Some("features.prioritize" | "issues.prioritize" | "tasks.prioritize") => {
            Negotiate::pinned(2, "queue placement was written against schema revision 2")
        }
        Some("features.request_plan_approval") => Negotiate::pinned(
            1,
            "plan-approval requests keep the revision they were written against",
        ),
        _ => Negotiate::Latest,
    };
    frame::build(backend, negotiate, input)
}
/// The work record a work command answered (applied or unchanged).
pub fn work_changed(outcome: CommandOutcome) -> Result<WorkRecord, Failure> {
    match command_result(outcome)? {
        OperationResult::TasksCreate { value }
        | OperationResult::TasksEdit { value }
        | OperationResult::TasksMove { value }
        | OperationResult::TasksAssign { value }
        | OperationResult::TasksDependencyLinks { value }
        | OperationResult::TasksAcceptanceEvidence { value }
        | OperationResult::TasksSetFeature { value }
        | OperationResult::IssuesCreate { value }
        | OperationResult::IssuesEdit { value }
        | OperationResult::FeaturesEdit { value }
        | OperationResult::FeaturesEditPlan { value }
        | OperationResult::FeaturesDependencies { value } => Ok(value),
        OperationResult::FeaturesCreate { value } | OperationResult::TasksSyncPlan { value } => {
            value
                .items
                .0
                .into_iter()
                .next()
                .ok_or_else(|| Failure::Other("no work record was returned".into()))
        }
        other => Err(unexpected("work command", &other)),
    }
}
/// The queue change a prioritization answered.
pub fn work_ordered(outcome: CommandOutcome) -> Result<MutationSummary, Failure> {
    match command_result(outcome)? {
        OperationResult::FeaturesPrioritize { value }
        | OperationResult::IssuesPrioritize { value }
        | OperationResult::TasksPrioritize { value } => Ok(value),
        other => Err(unexpected("work order", &other)),
    }
}
/// The approval request `features.request_plan_approval` answered.
pub fn approval_requested(outcome: CommandOutcome) -> Result<ProposalRecord, Failure> {
    match command_result(outcome)? {
        OperationResult::FeaturesRequestPlanApproval { value } => Ok(value),
        other => Err(unexpected("features.request_plan_approval", &other)),
    }
}
/// Events one Activity read asks for.
pub const ACTIVITY_PAGE: u16 = 200;
/// Newest-first project or card decisions; older pages use the same scoped cursor.
pub fn history_events(
    backend: &dyn Backend,
    project: &str,
    root: Option<&str>,
    after: Option<&str>,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        let (root_id, project_id) = match root {
            Some(root) => (Some(frame::text::<RootId>(root)?), None),
            None => (None, Some(ProjectId(frame::id(project)?))),
        };
        Some(In::History(history::Input::Events(QueryRequest {
            protocol: frame::protocol(),
            query_id: frame::query_id(backend)?,
            query: Query::Events {
                root_id,
                project_id,
                newest_first: Some(true),
                after: after.map(str::to_owned),
                limit: ACTIVITY_PAGE,
            },
        })))
    })
}
#[cfg(any(test, feature = "test-support"))]
pub fn project_events(backend: &dyn Backend, project: &str, after: Option<&str>) -> Vec<u8> {
    history_events(backend, project, None, after)
}
/// Events of a newest-first page and its older-page cursor, present only while more exist.
pub fn events_page(outcome: QueryOutcome) -> Result<(Vec<Event>, Option<String>, bool), Failure> {
    match query_result(outcome)? {
        (OperationResult::HistoryEvents { value }, _) => {
            let resume = value.next_cursor.map(|c| c.0).filter(|_| value.has_more);
            Ok((value.items.0, resume, value.has_more))
        }
        (other, _) => Err(unexpected("history.events", &other)),
    }
}
/// `runs.inspect` of a root (its control revision and holds).
pub fn run_inspect(backend: &dyn Backend, root: &str) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Runs(runs::Input::Inspect(QueryRequest {
            protocol: frame::protocol(),
            query_id: frame::query_id(backend)?,
            query: Query::Root {
                root_id: frame::text(root)?,
            },
        })))
    })
}
pub fn run_snapshot(outcome: QueryOutcome) -> Result<RootSnapshot, Failure> {
    match query_result(outcome)? {
        (OperationResult::RunsInspect { value }, _) => Ok(value),
        (other, _) => Err(unexpected("runs.inspect", &other)),
    }
}
/// Whether a hold is one Continue clears (a manual-resume condition).
pub fn manual_hold(c: &Cause) -> bool {
    matches!(
        c,
        Cause::HumanPause | Cause::RestartContinue | Cause::BudgetContinue
    )
}
/// `runs.start`, `runs.pause` or `runs.continue` of a root at its inspected revisions.
pub fn run_control(
    backend: &dyn Backend,
    operation: &str,
    root: &WorkRecord,
    snapshot: &RootSnapshot,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        let root_id = RootId(root.id.clone());
        let expected_control_revision = snapshot.root.revision.clone();
        let request = |command| {
            Some(CommandRequest {
                protocol: frame::protocol(),
                request_key: frame::request_key(backend)?,
                command,
            })
        };
        Some(match operation {
            "runs.start" => In::Runs(runs::Input::Start(request(RootCommand::Start {
                root_id,
                expected_work_revision: root.revision.clone(),
                expected_control_revision,
            })?)),
            "runs.pause" => In::Runs(runs::Input::Pause(request(RootCommand::Pause {
                root_id,
                expected_control_revision,
            })?)),
            _ => In::Runs(runs::Input::Continue(request(RootCommand::Continue {
                root_id,
                expected_control_revision,
                covered_holds: snapshot
                    .blockers
                    .items
                    .iter()
                    .filter(|b| manual_hold(&b.cause))
                    .map(|b| HoldRef {
                        hold_id: b.hold_id.clone(),
                        revision: b.revision.clone(),
                    })
                    .collect(),
            })?)),
        })
    })
}
/// The root snapshot a run control command answered.
pub fn run_changed(outcome: CommandOutcome) -> Result<RootSnapshot, Failure> {
    match command_result(outcome)? {
        OperationResult::RunsStart { value }
        | OperationResult::RunsPause { value }
        | OperationResult::RunsContinue { value } => Ok(value),
        other => Err(unexpected("run control", &other)),
    }
}

/// Lines of linked history one page request asks for.
pub const LINKED_PAGE: u32 = 50;

/// A page of a root's linked history (`history.work_lineage`): its related work, its links and
/// its events, in committed order.
pub fn work_lineage(backend: &dyn Backend, root: &str, cursor: Option<&str>) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::History(history::Input::WorkLineage(
            history::WorkLineageInput {
                root_id: RootId(frame::id(root)?),
                page: frame::page(LINKED_PAGE, cursor)?,
            },
        )))
    })
}

pub fn work_lineage_result(
    outcome: QueryOutcome,
) -> Result<
    devworks_core::protocol::support::HistoryPage<devworks_core::protocol::records::LineageRecord>,
    Failure,
> {
    match query_result(outcome)? {
        (OperationResult::HistoryWorkLineage { value }, _) => Ok(value),
        (other, _) => Err(unexpected("history.work_lineage", &other)),
    }
}

/// Retained source-control associations use the shared history query, including removed paths.
pub fn git_links(backend: &dyn Backend, root: &str, cursor: Option<&str>) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::History(history::Input::GitLinks(
            history::GitLinksInput {
                root_id: frame::text(root)?,
                page: frame::page(200, cursor)?,
            },
        )))
    })
}
pub fn git_links_result(
    outcome: QueryOutcome,
) -> Result<
    devworks_core::protocol::support::HistoryPage<
        devworks_core::protocol::records::AttributedGitLink,
    >,
    Failure,
> {
    match query_result(outcome)? {
        (OperationResult::HistoryGitLinks { value }, _) => Ok(value),
        (other, _) => Err(unexpected("history.git_links", &other)),
    }
}

/// A page of a root's evidence links (`history.evidence_links`).
pub fn evidence_links(backend: &dyn Backend, root: &str, cursor: Option<&str>) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::History(history::Input::EvidenceLinks(
            history::EvidenceLinksInput {
                root_id: RootId(frame::id(root)?),
                page: frame::page(LINKED_PAGE, cursor)?,
            },
        )))
    })
}
pub fn evidence_links_result(
    outcome: QueryOutcome,
) -> Result<
    devworks_core::protocol::support::HistoryPage<devworks_core::protocol::records::EvidenceLink>,
    Failure,
> {
    match query_result(outcome)? {
        (OperationResult::HistoryEvidenceLinks { value }, _) => Ok(value),
        (other, _) => Err(unexpected("history.evidence_links", &other)),
    }
}

/// One fact of a root's usage page: a model request or a completed tool call.
#[derive(Clone, Debug)]
pub enum UsageRow {
    Model(devworks_core::protocol::records::UsageItem),
    Tool(devworks_core::protocol::records::ToolInvocation),
}
/// A page of a root's model and tool usage (`history.tool_usage`).
pub fn tool_usage(backend: &dyn Backend, root: &str, cursor: Option<&str>) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::History(history::Input::ToolUsage(
            history::ToolUsageInput {
                root_id: Some(RootId(frame::id(root)?)),
                project_id: None,
                workspace: None,
                page: frame::page(LINKED_PAGE, cursor)?,
            },
        )))
    })
}
/// The rows of a usage page in committed order and the cursor of the next one, present only
/// while more exist.
pub fn tool_usage_result(
    outcome: QueryOutcome,
) -> Result<(Vec<UsageRow>, Option<String>), Failure> {
    match query_result(outcome)? {
        (OperationResult::HistoryToolUsage { value }, _) => Ok(usage_rows(value)),
        (other, _) => Err(unexpected("history.tool_usage", &other)),
    }
}

/// A usage page's model requests and tool calls merged by their committed sequences. Pages
/// follow one another in that order, so appended pages stay ordered. A page without model
/// sequences (revision 1) carries no tool calls and keeps its wire order.
pub fn usage_rows(
    page: devworks_core::protocol::records::UsagePage,
) -> (Vec<UsageRow>, Option<String>) {
    let next = page.next_cursor.map(|c| c.0).filter(|_| page.has_more);
    let models = page.items.0;
    let sequences = page.item_sequences.map(|s| s.0).unwrap_or_default();
    let mut rows: Vec<(i64, UsageRow)> = if sequences.len() == models.len() {
        sequences
            .iter()
            .map(|s| s.get())
            .zip(models.into_iter().map(UsageRow::Model))
            .collect()
    } else {
        models
            .into_iter()
            .map(|m| (0, UsageRow::Model(m)))
            .collect()
    };
    rows.extend(
        page.tool_invocations
            .map(|t| t.0)
            .unwrap_or_default()
            .into_iter()
            .map(|t| (t.sequence.get(), UsageRow::Tool(t))),
    );
    // Stable: rows without a known sequence keep their wire order.
    rows.sort_by_key(|(sequence, _)| *sequence);
    (rows.into_iter().map(|(_, row)| row).collect(), next)
}

/// Project workflow columns, including the backend's edit/removal fences.
pub fn workflow_list(backend: &dyn Backend, project: &str, cursor: Option<&str>) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Workflows(workflows::Input::List(
            workflows::ListInput {
                project_id: ProjectId(frame::id(project)?),
                page: frame::page(PAGE_LIMIT.into(), cursor)?,
            },
        )))
    })
}
pub fn workflow_page(
    outcome: QueryOutcome,
) -> Result<(Vec<StatusRecord>, Option<String>), Failure> {
    match query_result(outcome)? {
        (OperationResult::WorkflowsList { value }, _) => {
            if value.has_more && value.next_cursor.is_none() {
                return Err(Failure::Other(
                    "workflow page has no continuation cursor".into(),
                ));
            }
            Ok((
                value.items.0,
                value.next_cursor.filter(|_| value.has_more).map(|c| c.0),
            ))
        }
        (other, _) => Err(unexpected("workflows.list", &other)),
    }
}
/// A workflow command; one that did not convert (`None`) is sent as no bytes.
pub fn workflow_command(backend: &dyn Backend, input: impl Into<Option<In>>) -> Vec<u8> {
    let input = input.into();
    let negotiate = match input.as_ref().map(In::operation) {
        Some("workflows.reassign_and_remove") => Negotiate::Initial,
        _ => Negotiate::Latest,
    };
    frame::build(backend, negotiate, input)
}
pub fn workflow_changed(outcome: CommandOutcome) -> Result<(), Failure> {
    match command_result(outcome)? {
        OperationResult::WorkflowsCreateStatus { .. }
        | OperationResult::WorkflowsRenameStatus { .. }
        | OperationResult::WorkflowsOrder { .. }
        | OperationResult::WorkflowsReassignAndRemove { .. } => Ok(()),
        other => Err(unexpected("workflow change", &other)),
    }
}
pub fn allocation_queue(backend: &dyn Backend, project: &str, feature: Option<&str>) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::NextWork(next_work::Input::InspectQueue(
            next_work::InspectQueueInput {
                project_id: ProjectId(frame::id(project)?),
                feature_id: frame::lift(feature.map(frame::id))?,
            },
        )))
    })
}
pub fn queue_snapshot(outcome: QueryOutcome) -> Result<AllocationQueueSnapshot, Failure> {
    match query_result(outcome)? {
        (OperationResult::NextWorkInspectQueue { value }, _) => Ok(value),
        (other, _) => Err(unexpected("next_work.inspect_queue", &other)),
    }
}
/// A work-allocation command; one that did not convert (`None`) is sent as no bytes.
pub fn allocation_command(backend: &dyn Backend, input: impl Into<Option<In>>) -> Vec<u8> {
    frame::build(backend, Negotiate::Latest, input.into())
}
pub fn allocation_inspect(backend: &dyn Backend, claim: &str) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::NextWork(next_work::Input::InspectClaim(
            next_work::InspectClaimInput {
                claim_id: frame::id(claim)?,
            },
        )))
    })
}
pub fn allocation_selection(outcome: CommandOutcome) -> Result<AllocationSelection, Failure> {
    match command_result(outcome)? {
        OperationResult::NextWorkClaimProjectQueue { value }
        | OperationResult::NextWorkClaimFeatureTask { value } => Ok(value),
        other => Err(unexpected("next work selection", &other)),
    }
}
pub fn allocation_released(outcome: CommandOutcome) -> Result<AllocationClaimRecord, Failure> {
    match command_result(outcome)? {
        OperationResult::NextWorkReleaseClaim { value } => Ok(value),
        other => Err(unexpected("next_work.release_claim", &other)),
    }
}
pub fn inspected_claim(outcome: QueryOutcome) -> Result<AllocationClaimRecord, Failure> {
    match query_result(outcome)? {
        (OperationResult::NextWorkInspectClaim { value }, _) => Ok(value),
        (other, _) => Err(unexpected("next_work.inspect_claim", &other)),
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;
    use serde_json::json;

    fn page(models: &[(&str, i64)], tools: &[(&str, i64)], more: bool) -> serde_json::Value {
        let item = |id: &str| {
            json!({"requestId": id, "purpose": "chat", "modelId": "model-a",
                "inputTokens": "1", "outputTokens": "1", "cacheReadTokens": "0",
                "cacheWriteTokens": "0", "priceKind": "Untracked", "priceOrigin": "Untracked",
                "incomplete": false})
        };
        let tool = |id: &str, sequence: i64| {
            json!({"sequence": sequence.to_string(), "callId": id, "sessionId": "session-a",
                "toolName": "fs.read", "outcome": "Completed", "actorId": "actor-a",
                "occurredAtMs": "1"})
        };
        let mut value = json!({
            "items": models.iter().map(|(id, _)| item(id)).collect::<Vec<_>>(),
            "hasMore": more,
            "itemSequences": models.iter().map(|(_, s)| s.to_string()).collect::<Vec<_>>(),
            "toolInvocations": tools.iter().map(|(id, s)| tool(id, *s)).collect::<Vec<_>>(),
        });
        if more {
            value["nextCursor"] = json!("next");
        }
        value
    }

    fn ids(rows: &[UsageRow]) -> Vec<&str> {
        rows.iter()
            .map(|r| match r {
                UsageRow::Model(m) => m.request_id.as_str(),
                UsageRow::Tool(t) => t.call_id.as_str(),
            })
            .collect()
    }

    #[test]
    fn usage_pages_list_model_requests_and_tool_calls_in_committed_order() {
        let first: devworks_core::protocol::records::UsagePage = serde_json::from_value(page(
            &[("m-1", 1), ("m-4", 4)],
            &[("t-2", 2), ("t-3", 3)],
            true,
        ))
        .unwrap();
        let second: devworks_core::protocol::records::UsagePage =
            serde_json::from_value(page(&[("m-6", 6)], &[("t-5", 5), ("t-7", 7)], false)).unwrap();
        let (mut rows, next) = usage_rows(first);
        assert_eq!(next.as_deref(), Some("next"));
        let (more, last) = usage_rows(second);
        assert_eq!(last, None);
        rows.extend(more);
        assert_eq!(
            ids(&rows),
            ["m-1", "t-2", "t-3", "m-4", "t-5", "m-6", "t-7"]
        );
    }
}
