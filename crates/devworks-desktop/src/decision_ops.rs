//! Native decision/notification frames. Records and eligibility come from the shared services.
use crate::backend::{Backend, screen_slot};
use crate::frame::{self, Negotiate};
use crate::ops::{self, Failure};
use devworks_core::gateway::{CommandOutcome, QueryOutcome};
use devworks_core::operations::{OperationInput as In, approvals, notifications};
use devworks_core::protocol::ProjectId;
use devworks_core::protocol::data::DataScope;
use devworks_core::protocol::records::{
    ApprovalViewRecord, KnowledgeReviewItem, NotificationConfig, NotificationRecord, PageRequest,
    ProposalDecision,
};
use devworks_core::protocol::results::OperationResult;
use devworks_core::protocol::support::BoundedList;

/// The requests the approvals and notification views wait for. The names are persisted (see
/// [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DecisionsSlot {
    ApprovalsList,
    ApprovalInspect(String),
    ApprovalRefresh(String),
    ApprovalContent(String),
    ApprovalDecision(String),
    ReviewQueue,
    NotificationsList,
    CompletionList,
    NotificationRead(String),
    NotificationChange(String),
    NotificationSettings,
    NotificationSettingsSave,
    OsNotificationPermission,
}
screen_slot!(DecisionsSlot, Decisions);

/// The frame of an approvals or notifications request (at the newest revision). A request that
/// did not convert (`None`) is no bytes, which the gateway rejects like any malformed request.
pub fn query(b: &dyn Backend, input: impl Into<Option<In>>) -> Vec<u8> {
    frame::build(b, Negotiate::Latest, input.into())
}
pub fn command(b: &dyn Backend, input: impl Into<Option<In>>) -> Vec<u8> {
    frame::build(b, Negotiate::Latest, input.into())
}
fn page(cursor: Option<&str>) -> Option<PageRequest> {
    frame::page(ops::PAGE_LIMIT.into(), cursor)
}
/// `approvals.list` of a scope, narrowed to one `state` when given.
pub fn approvals_list(
    project: Option<&str>,
    state: Option<&str>,
    cursor: Option<&str>,
) -> Option<In> {
    Some(In::Approvals(approvals::Input::List(
        approvals::ListInput {
            scope: frame::scope(project)?,
            page: page(cursor)?,
            state: frame::lift(state.map(frame::text))?,
        },
    )))
}
/// `approvals.review_queue` of a project. The retained informational queue permits at most 50
/// per page.
pub fn review_queue(project: &str, cursor: Option<&str>) -> Option<In> {
    Some(In::Approvals(approvals::Input::ReviewQueue(
        approvals::ReviewQueueInput {
            project_id: Some(ProjectId(frame::id(project)?)),
            page: frame::page(50, cursor)?,
        },
    )))
}
pub fn approval_inspect(proposal: &str) -> Option<In> {
    Some(In::Approvals(approvals::Input::Inspect(
        approvals::InspectInput {
            proposal_id: frame::id(proposal)?,
        },
    )))
}
/// `approvals.record_human_decision` of the exact content of `p`.
pub fn decision(p: &ApprovalViewRecord, approve: bool, rationale: &str) -> Option<In> {
    Some(In::Approvals(approvals::Input::RecordHumanDecision(
        approvals::RecordHumanDecisionInput {
            proposal_id: p.id.clone(),
            expected_revision: p.revision.clone(),
            content_digest: p.content_digest.clone(),
            decision: if approve {
                ProposalDecision::Approve
            } else {
                ProposalDecision::Reject
            },
            rationale: frame::text(rationale)?,
        },
    )))
}
/// `notifications.list` of the completion notices still unacknowledged (workspace scope).
pub fn completions(cursor: Option<&str>) -> Option<In> {
    Some(In::Notifications(notifications::Input::List(
        notifications::ListInput {
            scope: DataScope::Workspace,
            page: page(cursor)?,
            unacknowledged_only: true,
            categories: None,
            read: None,
            completion_only: Some(true),
        },
    )))
}
/// `notifications.list` of a scope, narrowed to unread notices and one category when given.
pub fn notifications_list(
    project: Option<&str>,
    unacknowledged_only: bool,
    unread_only: bool,
    category: &str,
    cursor: Option<&str>,
) -> Option<In> {
    let category = category.trim();
    Some(In::Notifications(notifications::Input::List(
        notifications::ListInput {
            scope: frame::scope(project)?,
            page: page(cursor)?,
            unacknowledged_only,
            categories: if category.is_empty() {
                None
            } else {
                Some(BoundedList(vec![frame::text(category)?]))
            },
            read: unread_only.then_some(false),
            completion_only: None,
        },
    )))
}
pub fn notification_read(notification: &str) -> Option<In> {
    Some(In::Notifications(notifications::Input::Read(
        notifications::ReadInput {
            notification_id: frame::id(notification)?,
        },
    )))
}
/// `notifications.mark_read` when `read` is given, else `notifications.acknowledge`, at the
/// record's revision.
pub fn notification_change(record: &NotificationRecord, read: Option<bool>) -> In {
    let (notification_id, expected_revision) = (record.id.clone(), record.revision.clone());
    In::Notifications(match read {
        Some(read) => notifications::Input::MarkRead(notifications::MarkReadInput {
            notification_id,
            expected_revision,
            read,
        }),
        None => notifications::Input::Acknowledge(notifications::AcknowledgeInput {
            notification_id,
            expected_revision,
        }),
    })
}
pub fn settings_query() -> In {
    In::Notifications(notifications::Input::SettingsQuery(
        notifications::SettingsQueryInput {},
    ))
}
pub fn settings_save(revision: &str, config: NotificationConfig) -> Option<In> {
    Some(In::Notifications(notifications::Input::SettingsCommand(
        notifications::SettingsCommandInput {
            expected_revision: frame::text(revision)?,
            config,
        },
    )))
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
