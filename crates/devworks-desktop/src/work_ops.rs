//! Typed frames of the work operations the Features, board, Issues and Overview screens bind
//! (DW-12), and the decoding of their replies. Every frame is the same wire envelope an agent
//! tool submits; nothing here decides anything the work service decides.
use crate::backend::Backend;
use crate::ops::{
    Failure, PAGE_LIMIT, PROTOCOL, at_revision, command_frame, command_result, query_frame,
    query_result, unexpected,
};
use devworks_core::gateway::{CommandOutcome, QueryOutcome};
use devworks_core::protocol::records::{MutationSummary, ProposalRecord, RootSnapshot, WorkRecord};
use devworks_core::protocol::results::OperationResult;
use devworks_core::protocol::{Cause, Event};
use serde_json::{Value, json};

/// Work records carry readiness and their queue/workflow revisions from schema revision 2.
pub const WORK_REVISION: u64 = 2;

/// A page of `features.list`, `issues.list` or `tasks.list` of `project` (schema revision 2).
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
/// `features.get` or `tasks.get` of one item (schema revision 2: readiness is included).
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
const REVISION_ONE: [&str; 4] = [
    "features.prioritize",
    "issues.prioritize",
    "tasks.prioritize",
    "features.request_plan_approval",
];
/// A work command frame (the same frame an agent tool submits): schema revision 2 where the
/// result is a work record (readiness and queue revisions included), revision 1 otherwise.
pub fn work_command(backend: &dyn Backend, operation: &str, body: Value) -> Vec<u8> {
    let frame = command_frame(backend, operation, body);
    if REVISION_ONE.contains(&operation) {
        frame
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
/// A page of the project change feed (`history.events` revision 2 with `projectId`).
pub fn project_events(backend: &dyn Backend, project: &str, after: Option<&str>) -> Vec<u8> {
    let mut query = json!({"kind": "history.events", "projectId": project, "limit": 200});
    if let Some(a) = after {
        query["after"] = json!(a);
    }
    json!({"protocol": PROTOCOL, "queryId": backend.token("q"), "schemaRevision": 2,
        "query": query})
    .to_string()
    .into_bytes()
}
/// Events of a feed page, its continuation and whether more exist.
pub fn events_page(outcome: QueryOutcome) -> Result<(Vec<Event>, Option<String>, bool), Failure> {
    match query_result(outcome)? {
        (OperationResult::HistoryEvents { value }, _) => Ok((
            value.items.0,
            value.next_cursor.map(|c| c.0),
            value.has_more,
        )),
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
