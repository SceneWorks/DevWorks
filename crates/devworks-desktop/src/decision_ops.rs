//! Native decision/notification frames. Records and eligibility come from the shared services.
use crate::backend::Backend;
use crate::ops::{self, Failure};
use devworks_core::gateway::{CommandOutcome, QueryOutcome};
use devworks_core::protocol::records::{
    ApprovalViewRecord, KnowledgeReviewItem, NotificationRecord,
};
use devworks_core::protocol::results::OperationResult;
use serde_json::{Value, json};

pub fn query(b: &dyn Backend, op: &str, body: Value) -> Vec<u8> {
    ops::at_revision(ops::query_frame(b, op, body), 3)
}
pub fn command(b: &dyn Backend, op: &str, body: Value) -> Vec<u8> {
    ops::at_revision(ops::command_frame(b, op, body), 3)
}
pub fn scope(project: Option<&str>) -> Value {
    project.map_or(
        json!({"kind":"Workspace"}),
        |p| json!({"kind":"Project","projectId":p}),
    )
}
pub fn page(cursor: Option<&str>) -> Value {
    let mut page = json!({"limit":ops::PAGE_LIMIT});
    if let Some(c) = cursor {
        page["cursor"] = json!(c);
    }
    page
}
pub fn approvals(q: QueryOutcome) -> Result<(Vec<ApprovalViewRecord>, Option<String>), Failure> {
    match ops::query_result(q)?.0 {
        OperationResult::ApprovalsList { value } => {
            Ok((value.items.0, value.next_cursor.map(|x| x.0)))
        }
        other => Err(ops::unexpected("approvals.list", &other)),
    }
}
pub fn approval(q: QueryOutcome) -> Result<ApprovalViewRecord, Failure> {
    match ops::query_result(q)?.0 {
        OperationResult::ApprovalsInspect { value } => Ok(value),
        other => Err(ops::unexpected("approvals.inspect", &other)),
    }
}
pub fn decided(q: CommandOutcome) -> Result<ApprovalViewRecord, Failure> {
    match ops::command_result(q)? {
        OperationResult::ApprovalsRecordHumanDecision { value }
        | OperationResult::ApprovalsPropose { value } => Ok(value),
        other => Err(ops::unexpected("approval decision", &other)),
    }
}
pub fn notifications(
    q: QueryOutcome,
) -> Result<(Vec<NotificationRecord>, Option<String>), Failure> {
    match ops::query_result(q)?.0 {
        OperationResult::NotificationsList { value } => {
            Ok((value.items.0, value.next_cursor.map(|x| x.0)))
        }
        other => Err(ops::unexpected("notifications.list", &other)),
    }
}
pub fn notification(q: QueryOutcome) -> Result<NotificationRecord, Failure> {
    match ops::query_result(q)?.0 {
        OperationResult::NotificationsRead { value } => Ok(value),
        other => Err(ops::unexpected("notifications.read", &other)),
    }
}
pub fn notified(q: CommandOutcome) -> Result<NotificationRecord, Failure> {
    match ops::command_result(q)? {
        OperationResult::NotificationsAcknowledge { value }
        | OperationResult::NotificationsMarkRead { value } => Ok(value),
        other => Err(ops::unexpected("notification change", &other)),
    }
}
pub fn reviews(q: QueryOutcome) -> Result<(Vec<KnowledgeReviewItem>, Option<String>), Failure> {
    match ops::query_result(q)?.0 {
        OperationResult::ApprovalsReviewQueue { value } => {
            Ok((value.items.0, value.next_cursor.map(|x| x.0)))
        }
        other => Err(ops::unexpected("approvals.review_queue", &other)),
    }
}
