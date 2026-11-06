//! Typed frames of the work operations the Features, board, Issues and Overview screens bind
//! (DW-12), and the decoding of their replies. Every frame is the same wire envelope an agent
//! tool submits; nothing here decides anything the work service decides.
use crate::backend::Backend;
use crate::ops::{
    Failure, PAGE_LIMIT, PROTOCOL, at_revision, command_frame, command_result, query_frame,
    query_result, unexpected,
};
use devworks_core::gateway::{CommandOutcome, QueryOutcome};
use devworks_core::protocol::records::{
    AllocationClaimRecord, AllocationQueueSnapshot, AllocationSelection, MutationSummary,
    ProposalRecord, RootSnapshot, StatusRecord, WorkRecord,
};
use devworks_core::protocol::results::OperationResult;
use devworks_core::protocol::{Cause, Event};
use serde_json::{Value, json};

/// Work records include authored task types and labels from schema revision 3.
pub const WORK_REVISION: u64 = 3;

/// A page of `features.list`, `issues.list` or `tasks.list` of `project` (schema revision 3).
pub fn work_list(
    backend: &dyn Backend,
    operation: &str,
    project: &str,
    cursor: Option<&str>,
) -> Vec<u8> {
    let mut page = json!({"limit": PAGE_LIMIT});
    if let Some(c) = cursor {
        page["cursor"] = json!(c);
    }
    at_revision(
        query_frame(
            backend,
            operation,
            json!({"projectId": project, "page": page, "statusIds": [], "repositoryIds": [],
                "detailLevel": "Summary"}),
        ),
        WORK_REVISION,
    )
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
/// `features.get` or `tasks.get` of one item (schema revision 3, including readiness and metadata).
pub fn work_get(backend: &dyn Backend, operation: &str, work: &str) -> Vec<u8> {
    at_revision(
        query_frame(
            backend,
            operation,
            json!({"workId": work, "detailLevel": "Summary"}),
        ),
        WORK_REVISION,
    )
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
/// Operations whose result is not a work record negotiate only revision 1.
const REVISION_ONE: [&str; 1] = ["features.request_plan_approval"];
/// Work records use revision 3 (authored metadata); queue placement keeps revision 2 and
/// plan-approval requests retain their original revision. Saved unknown frames are never rebuilt.
pub fn work_command(backend: &dyn Backend, operation: &str, body: Value) -> Vec<u8> {
    let frame = command_frame(backend, operation, body);
    if REVISION_ONE.contains(&operation) {
        frame
    } else if [
        "features.prioritize",
        "issues.prioritize",
        "tasks.prioritize",
    ]
    .contains(&operation)
    {
        at_revision(frame, 2)
    } else {
        at_revision(frame, WORK_REVISION)
    }
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
/// A page of the project change feed (`history.events` revision 2 with `projectId`), oldest
/// first after `after` (the feed pages forward only).
pub fn project_events(backend: &dyn Backend, project: &str, after: Option<&str>) -> Vec<u8> {
    let mut query = json!({"kind": "history.events", "projectId": project, "limit": ACTIVITY_PAGE});
    if let Some(a) = after {
        query["after"] = json!(a);
    }
    json!({"protocol": PROTOCOL, "queryId": backend.token("q"), "schemaRevision": 2,
        "query": query})
    .to_string()
    .into_bytes()
}
/// Events of a feed page and where reading continues: the next page's cursor while more
/// exist, otherwise the checkpoint after everything read (newer events start there).
pub fn events_page(outcome: QueryOutcome) -> Result<(Vec<Event>, Option<String>, bool), Failure> {
    match query_result(outcome)? {
        (OperationResult::HistoryEvents { value }, _) => {
            let resume = if value.has_more {
                value.next_cursor.map(|c| c.0)
            } else {
                value.checkpoint.map(|c| c.0)
            };
            Ok((value.items.0, resume, value.has_more))
        }
        (other, _) => Err(unexpected("history.events", &other)),
    }
}
/// `runs.inspect` of a root (its control revision and holds).
pub fn run_inspect(backend: &dyn Backend, root: &str) -> Vec<u8> {
    json!({"protocol": PROTOCOL, "queryId": backend.token("q"),
        "query": {"kind": "runs.inspect", "rootId": root}})
    .to_string()
    .into_bytes()
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
    let control = snapshot.root.revision.as_str();
    let command = match operation {
        "runs.start" => json!({"kind": operation, "rootId": root.id,
            "expectedWorkRevision": root.revision, "expectedControlRevision": control}),
        "runs.pause" => json!({"kind": operation, "rootId": root.id,
            "expectedControlRevision": control}),
        _ => {
            let holds: Vec<Value> = snapshot
                .blockers
                .items
                .iter()
                .filter(|b| manual_hold(&b.cause))
                .map(|b| json!({"holdId": b.hold_id, "revision": b.revision}))
                .collect();
            json!({"kind": operation, "rootId": root.id,
                "expectedControlRevision": control, "coveredHolds": holds})
        }
    };
    json!({"protocol": PROTOCOL, "requestKey": backend.token("rk"), "command": command})
        .to_string()
        .into_bytes()
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

/// Retained source-control associations use the shared history query, including removed paths.
pub fn git_links(backend: &dyn Backend, root: &str, cursor: Option<&str>) -> Vec<u8> {
    let mut frame = json!({"protocol":PROTOCOL,"schemaRevision":3,"queryId":backend.token("q"),"mode":"Query",
        "body":{"kind":"history.git_links","rootId":root,"page":{"limit":200}}});
    if let Some(cursor) = cursor {
        frame["body"]["page"]["cursor"] = json!(cursor);
    }
    frame.to_string().into_bytes()
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

/// Project workflow columns, including the backend's edit/removal fences.
pub fn workflow_list(backend: &dyn Backend, project: &str, cursor: Option<&str>) -> Vec<u8> {
    let mut page = json!({"limit":PAGE_LIMIT});
    if let Some(cursor) = cursor {
        page["cursor"] = json!(cursor);
    }
    at_revision(
        query_frame(
            backend,
            "workflows.list",
            json!({"projectId":project,"page":page}),
        ),
        3,
    )
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
pub fn workflow_command(backend: &dyn Backend, operation: &str, body: Value) -> Vec<u8> {
    let frame = command_frame(backend, operation, body);
    if operation == "workflows.reassign_and_remove" {
        frame
    } else {
        at_revision(frame, 3)
    }
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
    let mut body = json!({"projectId":project});
    if let Some(feature) = feature {
        body["featureId"] = json!(feature);
    }
    at_revision(query_frame(backend, "next_work.inspect_queue", body), 3)
}
pub fn queue_snapshot(outcome: QueryOutcome) -> Result<AllocationQueueSnapshot, Failure> {
    match query_result(outcome)? {
        (OperationResult::NextWorkInspectQueue { value }, _) => Ok(value),
        (other, _) => Err(unexpected("next_work.inspect_queue", &other)),
    }
}
pub fn allocation_command(backend: &dyn Backend, operation: &str, body: Value) -> Vec<u8> {
    at_revision(command_frame(backend, operation, body), 3)
}
pub fn allocation_inspect(backend: &dyn Backend, claim: &str) -> Vec<u8> {
    at_revision(
        query_frame(backend, "next_work.inspect_claim", json!({"claimId":claim})),
        3,
    )
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
