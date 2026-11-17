//! Notification publication shared by every service that raises a card (settings and approvals
//! included). The notification service owns reads and acknowledgement; this module only writes.
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::storage::*;
use crate::protocol::records::NotificationLink;
use crate::protocol::support::{BoundedText, Check};
use crate::protocol::{Id, ProjectId, RootId};

pub const APPROVAL: &str = "Approval";

/// A source owns this content. Repeating a stable source never rewrites content, read state or
/// dismissal; a new authoritative occurrence needs a new source identity.
pub struct NotificationDraft {
    pub source: String,
    pub category: String,
    pub decision_id: Option<Id>,
    pub project_id: Option<ProjectId>,
    pub root_id: Option<RootId>,
    pub title: String,
    pub body: String,
    pub links: Vec<NotificationLink>,
    pub requires_input: bool,
    pub priority: i64,
}
fn limited(value: &str, characters: usize) -> String {
    value.chars().take(characters).collect()
}
pub(crate) fn wire_source(source: &str) -> Id {
    Id::new(source).unwrap_or_else(|_| {
        Id::new(format!("source-{}", sha256_hex(source.as_bytes()))).expect("digest identifier")
    })
}
pub fn publish(
    tx: &mut dyn WriteTransaction,
    draft: &NotificationDraft,
) -> Result<(), StorageError> {
    if draft.source.is_empty()
        || draft.source.len() > 512
        || draft.category.is_empty()
        || draft.category.len() > 128
        || draft.title.trim().is_empty()
        || draft.links.len() > 128
    {
        return Err(StorageError::InvalidRecord(
            "invalid notification source/content bounds".into(),
        ));
    }
    for link in &draft.links {
        link.check().map_err(StorageError::InvalidRecord)?;
    }
    let users = tx.decisions().notification_users()?;
    if users.len() > 1 {
        return Err(StorageError::InvalidRecord(
            "notification audience is not singular".into(),
        ));
    }
    for user in users {
        if tx
            .decisions()
            .notification_by_source(&user, &draft.source)?
            .is_some()
        {
            continue;
        }
        let now = tx.now_ms();
        let row = StoredNotification {
            id: tx.new_id(IdKind::Event),
            revision: 1,
            user_id: user,
            project_id: draft.project_id.clone(),
            root_id: draft.root_id.clone(),
            decision_id: draft.decision_id.clone(),
            category: draft.category.clone(),
            priority: draft.priority,
            title: limited(&draft.title, 256),
            body: limited(&draft.body, 65_536),
            source_id: draft.source.clone(),
            links: serde_json::to_value(&draft.links)
                .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
            requires_input: draft.requires_input,
            read: false,
            acknowledged: false,
            created_at_ms: now,
            updated_at_ms: now,
        };
        tx.decisions().put_notification(&row, None)?;
    }
    Ok(())
}
pub fn notify_approval(
    tx: &mut dyn WriteTransaction,
    source: ApprovalSource,
    id: &Id,
    project: Option<&ProjectId>,
    title: &str,
    body: &str,
) -> Result<(), StorageError> {
    publish(
        tx,
        &NotificationDraft {
            source: format!("approval-{}-{}", source.as_str(), id.as_str()),
            category: APPROVAL.into(),
            decision_id: Some(id.clone()),
            project_id: project.cloned(),
            root_id: None,
            title: title.into(),
            body: body.into(),
            links: vec![NotificationLink {
                kind: BoundedText("Approval".into()),
                target_id: Some(id.clone()),
                url: None,
                artifact: None,
                project_id: project.cloned(),
            }],
            requires_input: true,
            priority: 1,
        },
    )
}
