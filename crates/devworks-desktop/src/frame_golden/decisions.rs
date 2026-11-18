//! Sample frames of the Approvals and Notifications requests: lists, inspections, the human
//! decision, notification changes and the notification settings.
use super::{Silent, from};
use crate::decision_ops as dops;
use crate::ops;
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::{
    ApprovalViewRecord, NotificationConfig, NotificationRecord,
};
use serde_json::json;

pub fn frames() -> Vec<(&'static str, Vec<u8>)> {
    let b = &Silent;
    let digest = "d".repeat(64);
    let approval: ApprovalViewRecord = from(json!({"id":"proposal-a","revision":"4",
        "contentDigest":digest,"state":"Pending","operationKind":"plan.approve",
        "targetId":"feature-a","targetRevision":"2","summary":"Plan"}));
    let notification: NotificationRecord =
        from(json!({"id":"note-a","revision":"5","title":"Done",
        "body":"Finished","acknowledged":false,"createdAtMs":"10"}));
    let artifact: ArtifactRef = from(json!({"artifactId":"artifact-a","versionId":"version-a",
        "sha256":"a".repeat(64),"byteLength":"12","mediaType":"text/plain"}));
    let config: NotificationConfig =
        from(json!({"desktopEnabled":true,"kinds":["approval","run"]}));
    vec![
        (
            "decisions_approvals_pending",
            dops::query(b, dops::approvals_list(None, Some("Pending"), None)),
        ),
        (
            "decisions_approvals_history",
            dops::query(
                b,
                dops::approvals_list(Some("project-a"), Some("Approved"), Some("c1")),
            ),
        ),
        (
            "decisions_approvals_all",
            dops::query(b, dops::approvals_list(None, None, None)),
        ),
        (
            "decisions_review_queue",
            dops::query(b, dops::review_queue("project-a", Some("c1"))),
        ),
        (
            "decisions_inspect",
            dops::query(b, dops::approval_inspect(approval.id.as_str())),
        ),
        (
            "decisions_notifications_completion",
            dops::query(b, dops::completions(Some("c1"))),
        ),
        (
            "decisions_notifications_history",
            dops::query(
                b,
                dops::notifications_list(Some("project-a"), false, false, "", None),
            ),
        ),
        (
            "decisions_notifications_filtered",
            dops::query(
                b,
                dops::notifications_list(None, true, true, " approval ", None),
            ),
        ),
        (
            "decisions_notification_read",
            dops::query(b, dops::notification_read(notification.id.as_str())),
        ),
        (
            "decisions_decide_approve",
            dops::command(b, dops::decision(&approval, true, "Looks right")),
        ),
        (
            "decisions_decide_reject",
            dops::command(b, dops::decision(&approval, false, "Not now")),
        ),
        (
            "decisions_notification_acknowledge",
            dops::command(b, dops::notification_change(&notification, None)),
        ),
        (
            "decisions_notification_mark_read",
            dops::command(b, dops::notification_change(&notification, Some(true))),
        ),
        (
            "decisions_artifact_read_cursor",
            ops::artifact_read_after(b, &artifact, Some("c1")),
        ),
        (
            "decisions_settings_query",
            dops::query(b, dops::settings_query()),
        ),
        (
            "decisions_settings_save",
            dops::command(b, dops::settings_save("3", config)),
        ),
    ]
}
