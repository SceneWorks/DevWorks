use super::{
    ItemVersion, Recipient, Resolution, authorize_recipient, awaiting_human_change, resolve_message,
};
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::storage::{
    ActivationPhase, AssignmentState, SessionPosition, SessionScope, StorageError,
    StoredAgentDelivery, StoredAgentMessage, StoredAgentMessageEvent, StoredAssignment,
    StoredSession, WriteTransaction,
};
use crate::protocol::{Id, ProjectId, RootId};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageKind {
    Info,
    Question,
    Request,
    Blocker,
    Announcement,
}

impl MessageKind {
    fn name(self) -> &'static str {
        match self {
            Self::Info => "Info",
            Self::Question => "Question",
            Self::Request => "Request",
            Self::Blocker => "Blocker",
            Self::Announcement => "Announcement",
        }
    }
}

pub struct PostSpec {
    pub project: ProjectId,
    pub body: String,
    pub kind: MessageKind,
    pub item: Option<RootId>,
    pub feature: Option<RootId>,
    pub resource: Option<String>,
    pub to_session: Option<Id>,
    pub to_human: bool,
    pub blocking: bool,
    pub in_reply_to: Option<Id>,
    pub resolves: Option<Resolution>,
    pub idempotency_key: Option<String>,
}

pub struct PostedMessage {
    pub created: bool,
    pub message: StoredAgentMessage,
    pub recipients: usize,
    pub resolved: Option<StoredAgentMessage>,
    pub item_versions: Vec<ItemVersion>,
}

pub(super) fn session_project(
    tx: &mut dyn WriteTransaction,
    session: &crate::ports::storage::StoredSession,
) -> Result<Option<ProjectId>, StorageError> {
    let root_project = match &session.root_id {
        Some(root) => tx.sessions().work_root(root)?.map(|work| work.project_id),
        None => None,
    };
    if session.project_id.is_some() && root_project.is_some() && session.project_id != root_project
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(session.project_id.clone().or(root_project))
}

pub(super) fn active_assignment(
    tx: &mut dyn WriteTransaction,
    session: &StoredSession,
) -> Result<Option<StoredAssignment>, StorageError> {
    let Some(root) = &session.root_id else {
        return Ok(None);
    };
    let mut after = None;
    loop {
        let page = tx.runs().assignments(root, true, after.as_ref(), 500)?;
        if page.is_empty() {
            return Ok(None);
        }
        after = page.last().map(|assignment| assignment.id.clone());
        let len = page.len();
        for assignment in page {
            if assignment.session_id != session.id || assignment.state != AssignmentState::Active {
                continue;
            }
            let Some(execution) = &assignment.execution_id else {
                continue;
            };
            let Some(activation) = tx.runs().activation(execution)? else {
                continue;
            };
            if activation.phase == ActivationPhase::Running
                && tx.store().boot_id.as_ref() == Some(&activation.boot_id)
                && activation
                    .lease_until_ms
                    .is_some_and(|until| until > tx.now_ms())
            {
                return Ok(Some(assignment));
            }
        }
        if len < 500 {
            return Ok(None);
        }
    }
}

fn project_sessions(
    tx: &mut dyn WriteTransaction,
    project: &ProjectId,
) -> Result<Vec<(StoredSession, StoredAssignment)>, StorageError> {
    let mut after: Option<SessionPosition> = None;
    let mut sessions = Vec::new();
    loop {
        let page = tx
            .sessions()
            .sessions(&SessionScope::All, Some(false), after.as_ref(), 500)?;
        if page.is_empty() {
            break;
        }
        after = page.last().map(|session| SessionPosition {
            created_at_ms: session.created_at_ms,
            id: session.id.clone(),
        });
        let len = page.len();
        for session in page {
            if session_project(tx, &session)?.as_ref() == Some(project)
                && let Some(assignment) = active_assignment(tx, &session)?
            {
                sessions.push((session, assignment));
            }
        }
        if len < 500 {
            break;
        }
    }
    Ok(sessions)
}

fn recipients(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    spec: &PostSpec,
    item: &Option<RootId>,
    feature: &Option<RootId>,
    to_session: &Option<Id>,
    to_human: bool,
) -> Result<Vec<Recipient>, StorageError> {
    if to_human {
        return Ok(vec![Recipient::Human]);
    }
    if let Some(session) = to_session {
        return Ok(vec![Recipient::Session(session.clone())]);
    }
    let mut selected = Vec::new();
    for (session, assignment) in project_sessions(tx, &spec.project)? {
        if let Some(feature) = feature {
            let Some(work) = tx.sessions().work_root(&assignment.work_id)? else {
                continue;
            };
            if assignment.work_id != *feature && work.parent_feature_id.as_ref() != Some(feature) {
                continue;
            }
        } else if let Some(item) = item
            && assignment.work_id != *item
        {
            continue;
        }
        selected.push(session.id);
    }
    let mut seen = HashSet::new();
    Ok(selected
        .into_iter()
        .filter(|id| ctx.session_id() != Some(id) && seen.insert(id.clone()))
        .map(Recipient::Session)
        .collect())
}

/// Stores the retained message and current recipient copies together. The caller owns a single
/// write transaction; gateway admission and receipt replay enclose this command.
pub fn post_message(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    spec: &PostSpec,
) -> Result<PostedMessage, StorageError> {
    let body = spec.body.trim();
    if body.is_empty() || body.chars().count() > 8000 || body.contains('\0') {
        return Err(StorageError::InvalidRecord(
            "message body must be 1 to 8000 characters".into(),
        ));
    }
    if ctx.principal_kind() == PrincipalKind::Agent {
        let session = ctx.session_id().ok_or(StorageError::ScopeDenied)?;
        authorize_recipient(tx, ctx, &spec.project, &Recipient::Session(session.clone()))?;
    } else {
        authorize_recipient(tx, ctx, &spec.project, &Recipient::Human)?;
    }
    if let Some(item) = &spec.item {
        let root = tx
            .sessions()
            .work_root(item)?
            .ok_or(StorageError::ScopeDenied)?;
        if root.project_id != spec.project || root.kind == "feature" {
            return Err(StorageError::ScopeDenied);
        }
    }
    if let Some(feature) = &spec.feature {
        let root = tx
            .sessions()
            .work_root(feature)?
            .ok_or(StorageError::ScopeDenied)?;
        if root.project_id != spec.project || root.kind != "feature" {
            return Err(StorageError::ScopeDenied);
        }
    }
    if let Some(resource) = &spec.resource
        && (resource.is_empty() || resource.len() > 200 || resource.chars().any(char::is_control))
    {
        return Err(StorageError::InvalidRecord("invalid resource name".into()));
    }
    let exclusive_targets = usize::from(spec.feature.is_some())
        + usize::from(spec.resource.is_some())
        + usize::from(spec.to_session.is_some())
        + usize::from(spec.to_human);
    if exclusive_targets > 1
        || spec.item.is_some() && (spec.feature.is_some() || spec.resource.is_some())
    {
        return Err(StorageError::InvalidRecord(
            "incompatible message targets".into(),
        ));
    }
    if spec.blocking
        && (spec.kind != MessageKind::Question || !spec.to_human || spec.item.is_none())
    {
        return Err(StorageError::InvalidRecord(
            "blocking requires a human item question".into(),
        ));
    }
    if spec.blocking && ctx.principal_kind() == PrincipalKind::Agent {
        let own_root = ctx.root_id().ok_or(StorageError::ScopeDenied)?;
        let target = spec.item.as_ref().ok_or(StorageError::ScopeDenied)?;
        if target != own_root {
            let target_work = tx
                .sessions()
                .work_root(target)?
                .ok_or(StorageError::ScopeDenied)?;
            let own_work = tx
                .sessions()
                .work_root(own_root)?
                .ok_or(StorageError::ScopeDenied)?;
            if own_work.kind != "feature"
                || target_work.parent_feature_id.as_ref() != Some(own_root)
            {
                return Err(StorageError::ScopeDenied);
            }
        }
    }
    if spec.resolves.is_some() && spec.in_reply_to.is_none() {
        return Err(StorageError::InvalidRecord(
            "resolution requires in_reply_to".into(),
        ));
    }
    let key = match &spec.idempotency_key {
        Some(key)
            if key.is_empty() || key.len() > 128 || key.bytes().any(|c| c <= b' ' || c > b'~') =>
        {
            return Err(StorageError::InvalidRecord(
                "invalid idempotency key".into(),
            ));
        }
        Some(key) => Some(sha256_hex(
            format!("{}\0{key}", ctx.principal().0.as_str()).as_bytes(),
        )),
        None => None,
    };
    if let Some(key) = &key
        && let Some(existing) =
            tx.runs()
                .agent_message_by_key(&spec.project, ctx.principal(), key)?
    {
        let count = tx.runs().agent_recipient_count(&existing.id)?;
        return Ok(PostedMessage {
            created: false,
            message: existing,
            recipients: count,
            resolved: None,
            item_versions: Vec::new(),
        });
    }
    let mut item = spec.item.clone();
    let mut feature = spec.feature.clone();
    let mut resource = spec.resource.clone();
    let mut to_session = spec.to_session.clone();
    let mut to_human = spec.to_human;
    let explicit = item.is_some()
        || feature.is_some()
        || resource.is_some()
        || to_session.is_some()
        || to_human;
    let parent = match &spec.in_reply_to {
        Some(id) => {
            let parent = tx
                .runs()
                .agent_message(id)?
                .ok_or(StorageError::ScopeDenied)?;
            if parent.project_id != spec.project {
                return Err(StorageError::ScopeDenied);
            }
            if !explicit {
                item = parent.target_item_id.clone();
                if let Some(sender) = &parent.sender_session_id {
                    if ctx.session_id() != Some(sender) {
                        to_session = Some(sender.clone());
                    }
                } else if ctx.session_id().is_some() {
                    to_human = true;
                }
                if to_session.is_none() && !to_human {
                    if let Some(target) = &parent.target_session_id
                        && ctx.session_id() != Some(target)
                    {
                        to_session = Some(target.clone());
                    }
                    if to_session.is_none() {
                        if parent.to_human && ctx.session_id().is_some() {
                            to_human = true;
                        } else if parent.target_feature_id.is_some() {
                            feature = parent.target_feature_id.clone();
                        } else if parent.resource_name.is_some() {
                            resource = parent.resource_name.clone();
                        }
                    }
                }
            }
            Some(parent)
        }
        None => None,
    };
    if let Some(session_id) = &to_session {
        if ctx.session_id() == Some(session_id) {
            return Err(StorageError::InvalidRecord(
                "cannot message own session".into(),
            ));
        }
        let session = tx
            .sessions()
            .session(session_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if session_project(tx, &session)?.as_ref() != Some(&spec.project) {
            return Err(StorageError::ScopeDenied);
        }
    }
    let channel = if to_human {
        "Human"
    } else if to_session.is_some() {
        "Session"
    } else if feature.is_some() {
        "Feature"
    } else if resource.is_some() {
        "Resource"
    } else if item.is_some() {
        "Item"
    } else {
        "Project"
    };
    let routed = recipients(tx, ctx, spec, &item, &feature, &to_session, to_human)?;
    let was_blocked = if spec.blocking {
        item.as_ref()
            .map(|item| tx.runs().open_blocking_question(item))
            .transpose()?
    } else {
        None
    };
    let now = tx.now_ms();
    let message = StoredAgentMessage {
        id: tx.new_id(IdKind::Event),
        project_id: spec.project.clone(),
        root_id: item.clone(),
        channel: channel.into(),
        from_assignment_id: ctx.assignment_id().cloned(),
        to_assignment_id: None,
        message_class: spec.kind.name().into(),
        body: body.into(),
        attachments: serde_json::json!([]),
        in_reply_to_id: spec.in_reply_to.clone(),
        state: matches!(spec.kind, MessageKind::Question | MessageKind::Request)
            .then(|| "Open".into()),
        blocking: spec.blocking,
        idempotency_key: key,
        revision: 1,
        created_at_ms: now,
        resolved_at_ms: None,
        from_actor_id: ctx.principal().clone(),
        target_item_id: item,
        target_feature_id: feature,
        resource_name: resource,
        target_session_id: to_session,
        to_human,
        sender_session_id: ctx.session_id().cloned(),
        resolved_by_actor_id: None,
        resolved_by_session_id: None,
    };
    tx.runs().insert_agent_message(&message)?;
    for recipient in &routed {
        let delivery = StoredAgentDelivery {
            sequence: tx.runs().agent_delivery_head()? + 1,
            message_id: message.id.clone(),
            project_id: spec.project.clone(),
            recipient_assignment_id: None,
            recipient_human: matches!(recipient, Recipient::Human),
            state: "Pending".into(),
            revision: 1,
            delivered_at_ms: None,
            read_at_ms: None,
            recipient_session_id: match recipient {
                Recipient::Session(id) => Some(id.clone()),
                Recipient::Human => None,
            },
            handed_over: false,
            handed_to_session_id: None,
            resolved_at_ms: None,
        };
        tx.runs().insert_agent_delivery(&delivery)?;
    }
    let event_sequence = tx.runs().agent_event_head()? + 1;
    tx.runs()
        .insert_agent_message_event(&StoredAgentMessageEvent {
            sequence: event_sequence,
            project_id: spec.project.clone(),
            message_id: message.id.clone(),
            kind: "Posted".into(),
            actor_id: ctx.principal().clone(),
            recipient_assignment_id: None,
            recipient_session_id: None,
            recipient_human: false,
            created_at_ms: now,
        })?;
    let mut item_versions = Vec::new();
    if let (Some(item), Some(before)) = (message.target_item_id.as_ref(), was_blocked)
        && let Some(version) = awaiting_human_change(tx, ctx, item, before)?
    {
        item_versions.push(version);
    }
    let resolved = if let Some(resolution) = spec.resolves {
        let parent = parent.as_ref().expect("validated parent");
        let recipient = match ctx.session_id() {
            Some(id) => Recipient::Session(id.clone()),
            None => Recipient::Human,
        };
        let blocking_item = parent
            .blocking
            .then(|| parent.target_item_id.clone())
            .flatten();
        let before = blocking_item
            .as_ref()
            .map(|item| tx.runs().open_blocking_question(item))
            .transpose()?;
        let resolved = resolve_message(tx, ctx, &spec.project, &recipient, parent, resolution)?;
        if resolved.is_some()
            && let (Some(item), Some(before)) = (blocking_item.as_ref(), before)
            && let Some(version) = awaiting_human_change(tx, ctx, item, before)?
        {
            item_versions.push(version);
        }
        resolved.map(|(_, message)| message)
    } else {
        None
    };
    Ok(PostedMessage {
        created: true,
        message,
        recipients: routed.len(),
        resolved,
        item_versions,
    })
}
