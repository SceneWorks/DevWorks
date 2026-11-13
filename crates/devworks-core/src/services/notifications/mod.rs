//! Persistent per-user notifications. Reading, read state and explicit acknowledgment are
//! separate from the owning approval, run and work records; none of these operations resumes
//! work or decides an approval. Producers publish in their own state transition transaction.
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, notifications as ops};
use crate::ports::storage::*;
use crate::ports::{
    CommandHandler, Decided, HandlerFailure, IdKind, IdSource, QueryAnswer, QueryHandler,
};
use crate::protocol::data::DataScope;
use crate::protocol::records::{
    NotificationConfig, NotificationLink, NotificationRecord, NotificationSettingsRecord,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Check, Page};
use crate::protocol::{
    ActorId, DomainErrorCode, DomainFault, EventId, Id, ProjectId, RevisionDimension, RootId,
};
use crate::services::cursor::{self, CursorBinding};
use crate::services::sessions::{
    Failure, Refusal, content, counter, decided, label, not_found, project_visible, query_fault,
    rev,
};
use serde_json::json;
use std::sync::Arc;

type R<T> = Result<T, Failure>;
pub const OPERATIONS: [&str; 5] = [
    "notifications.list",
    "notifications.read",
    "notifications.acknowledge",
    "notifications.settings",
    "notifications.mark_read",
];
pub const COMPLETION: &str = "Completion";
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
fn wire_source(source: &str) -> Id {
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
/// Only an actual transition into verified feature/standalone-issue completion produces a card.
/// A manual Done or a feature task finishing does not claim feature/issue acceptance.
pub fn verified_transition(
    tx: &mut dyn WriteTransaction,
    before: &StoredWork,
    after: &StoredWork,
) -> Result<(), StorageError> {
    if before.completion.as_deref() == Some(VERIFIED)
        || after.completion.as_deref() != Some(VERIFIED)
        || (!after.is_feature() && after.parent_feature_id.is_some())
    {
        return Ok(());
    }
    let statuses = tx.work().statuses(&after.project_id)?;
    if !after
        .status_id
        .as_ref()
        .and_then(|id| crate::services::work::workflow::semantic_of(&statuses, id))
        .is_some_and(|s| s == crate::services::work::workflow::DONE)
    {
        return Ok(());
    }
    let kind = if after.is_feature() {
        "Feature"
    } else {
        "Issue"
    };
    publish(
        tx,
        &NotificationDraft {
            source: format!(
                "verified-completion-{}-{}",
                after.id.as_str(),
                after.revision
            ),
            category: COMPLETION.into(),
            decision_id: None,
            project_id: Some(after.project_id.clone()),
            root_id: Some(RootId(after.id.clone())),
            title: format!("{kind} complete: {}", after.title),
            body: "Acceptance is supported by verified evidence.".into(),
            links: vec![NotificationLink {
                kind: BoundedText(kind.into()),
                target_id: Some(after.id.clone()),
                url: None,
                artifact: None,
                project_id: Some(after.project_id.clone()),
            }],
            requires_input: false,
            priority: 0,
        },
    )
}
fn namespace(user: &ActorId) -> String {
    let plain = format!("notifications:{}", user.0.as_str());
    if plain.len() <= 128 {
        plain
    } else {
        format!("notifications:{}", sha256_hex(user.0.as_str().as_bytes()))
    }
}
pub fn preferences(
    read: &dyn ConfigurationRead,
    user: &ActorId,
) -> Result<NotificationSettingsRecord, StorageError> {
    match read.current(&ConfigScope::Workspace, &namespace(user))? {
        None => Ok(NotificationSettingsRecord {
            revision: rev(1),
            config: NotificationConfig {
                desktop_enabled: true,
                kinds: BoundedList(vec![]),
            },
        }),
        Some(row) => {
            let config: NotificationConfig = serde_json::from_value(row.body).map_err(|_| {
                StorageError::unavailable(Unavailable::Corrupt, "invalid notification settings")
            })?;
            config.check().map_err(StorageError::InvalidRecord)?;
            Ok(NotificationSettingsRecord {
                revision: rev(row.revision),
                config,
            })
        }
    }
}
fn eligible(
    decisions: &dyn DecisionRead,
    configuration: &dyn ConfigurationRead,
    work: &dyn WorkRead,
    user: &ActorId,
    row: &StoredNotification,
) -> Result<bool, StorageError> {
    if &row.user_id != user
        || !row.requires_input
        || row.acknowledged
        || !decisions.notification_users()?.contains(user)
    {
        return Ok(false);
    }
    let config = preferences(configuration, user)?.config;
    if !config.desktop_enabled
        || (!config.kinds.0.is_empty() && !config.kinds.0.iter().any(|k| k.0 == row.category))
    {
        return Ok(false);
    }
    let Some(id) = &row.decision_id else {
        return Ok(false);
    };
    if row.source_id == format!("approval-Settings-{}", id.as_str()) {
        return Ok(configuration
            .proposal(id)?
            .is_some_and(|p| p.state == ProposalState::Pending));
    }
    if row.source_id == format!("approval-FeaturePlan-{}", id.as_str()) {
        return Ok(work.proposal(id)?.is_some_and(|p| p.state == "Pending"));
    }
    if row.source_id == format!("approval-Generic-{}", id.as_str()) {
        return Ok(decisions
            .approval(id)?
            .is_some_and(|p| p.state == "Pending"));
    }
    Ok(false)
}
pub fn os_eligible(
    read: &dyn ReadSnapshot,
    user: &ActorId,
    row: &StoredNotification,
) -> Result<bool, StorageError> {
    eligible(
        read.decisions(),
        read.configuration(),
        read.work(),
        user,
        row,
    )
}
pub fn os_eligible_tx(
    tx: &mut dyn WriteTransaction,
    user: &ActorId,
    row: &StoredNotification,
) -> Result<bool, StorageError> {
    // Distinct short reads avoid aliasing mutable transaction repository borrows.
    if &row.user_id != user
        || !row.requires_input
        || row.acknowledged
        || !tx.decisions().notification_users()?.contains(user)
    {
        return Ok(false);
    }
    let config = preferences(tx.configuration(), user)?.config;
    if !config.desktop_enabled
        || (!config.kinds.0.is_empty() && !config.kinds.0.iter().any(|k| k.0 == row.category))
    {
        return Ok(false);
    }
    let Some(id) = &row.decision_id else {
        return Ok(false);
    };
    if row.source_id == format!("approval-Settings-{}", id.as_str()) {
        return Ok(tx
            .configuration()
            .proposal(id)?
            .is_some_and(|p| p.state == ProposalState::Pending));
    }
    if row.source_id == format!("approval-FeaturePlan-{}", id.as_str()) {
        return Ok(tx
            .work()
            .proposal(id)?
            .is_some_and(|p| p.state == "Pending"));
    }
    if row.source_id == format!("approval-Generic-{}", id.as_str()) {
        return Ok(tx
            .decisions()
            .approval(id)?
            .is_some_and(|p| p.state == "Pending"));
    }
    Ok(false)
}
fn record(row: &StoredNotification) -> Result<NotificationRecord, StorageError> {
    let links = serde_json::from_value(row.links.clone()).map_err(|_| {
        StorageError::unavailable(Unavailable::Corrupt, "invalid notification links")
    })?;
    Ok(NotificationRecord {
        id: row.id.clone(),
        revision: rev(row.revision),
        title: label(&row.title, "Notification"),
        body: content(&row.body),
        decision_id: row.decision_id.clone(),
        root_id: row.root_id.clone(),
        acknowledged: row.acknowledged,
        created_at_ms: counter(row.created_at_ms),
        category: Some(label(&row.category, "Notification")),
        project_id: row.project_id.clone(),
        read: Some(row.read),
        source_id: Some(wire_source(&row.source_id)),
        links: Some(links),
        requires_input: Some(row.requires_input),
    })
}
fn visible(read: &dyn DecisionRead, ctx: &InvocationContext, id: &Id) -> R<StoredNotification> {
    read.notification(ctx.principal(), id)?
        .filter(|n| project_visible(ctx, n.project_id.as_ref()))
        .ok_or_else(|| not_found("notification", id).into())
}
pub struct NotificationService {
    ids: Arc<dyn IdSource>,
}
impl NotificationService {
    pub fn new(ids: Arc<dyn IdSource>) -> Self {
        Self { ids }
    }
    fn list(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::ListInput,
    ) -> R<(Page<NotificationRecord>, i64)> {
        let (scope, revision) = match &i.scope {
            DataScope::Workspace if ctx.project_id().is_none() => (ConfigScope::Workspace, 1),
            DataScope::Workspace => return Err(StorageError::ScopeDenied.into()),
            DataScope::Project { project_id } => {
                let p = crate::services::projects::visible_project(
                    snapshot.projects(),
                    ctx,
                    project_id,
                )?;
                (ConfigScope::Project(project_id.clone()), p.scope_revision)
            }
        };
        let mut categories = i.categories.as_ref().map_or_else(Vec::new, |v| {
            v.0.iter().map(|s| s.0.clone()).collect::<Vec<_>>()
        });
        categories.sort();
        categories.dedup();
        let filter = NotificationFilter {
            user_id: ctx.principal().clone(),
            scope,
            categories,
            read: i.read,
            unacknowledged_only: i.unacknowledged_only,
            completion_only: i.completion_only.unwrap_or(false),
        };
        let binding = CursorBinding::new(
            ctx,
            "notifications.list",
            &[
                &serde_json::to_string(&i.scope).expect("scope"),
                &filter.categories.join(","),
                // The cursor is a keyset position, so it stays valid across unrelated commits.
                &format!(
                    "{:?}:{}:{}",
                    filter.read, filter.unacknowledged_only, filter.completion_only,
                ),
            ],
            revision,
        );
        let rows = cursor::page(
            &binding,
            &i.page,
            |after, n| {
                let after = after
                    .map(|p| {
                        let (time, id) = p.split_once(':').ok_or(StorageError::CursorExpired)?;
                        Ok::<_, StorageError>(NotificationPosition {
                            created_at_ms: time.parse().map_err(|_| StorageError::CursorExpired)?,
                            id: Id::new(id).map_err(|_| StorageError::CursorExpired)?,
                        })
                    })
                    .transpose()?;
                snapshot
                    .decisions()
                    .notifications(&filter, after.as_ref(), n)
            },
            |r| format!("{}:{}", r.created_at_ms, r.id.as_str()),
        )?;
        Ok((rows.try_into_page(|r| record(&r))?, revision))
    }
    fn mutate(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        id: &Id,
        expected: i64,
        read: Option<bool>,
        ack: bool,
    ) -> R<DomainDecision> {
        let old = visible(tx.decisions(), ctx, id)?;
        if old.revision != expected {
            return Err(StorageError::conflict(
                RevisionDimension::Work,
                "notification",
                id.clone(),
                rev(expected),
                rev(old.revision),
            )
            .into());
        }
        let mut next = old.clone();
        if let Some(read) = read {
            next.read = read;
        }
        if ack {
            next.acknowledged = true;
        }
        let result = |value| {
            if ack {
                OperationResult::NotificationsAcknowledge { value }
            } else {
                OperationResult::NotificationsMarkRead { value }
            }
        };
        if next == old {
            return Ok(DomainDecision::NoChange {
                result: result(record(&old)?),
            });
        }
        next.revision += 1;
        next.updated_at_ms = tx.now_ms();
        tx.decisions().put_notification(&next, Some(old.revision))?;
        let event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event,
            actor: ctx.principal().clone(),
            project_id: next.project_id.clone(),
            root_id: next.root_id.clone(),
            operation_id: None,
            aggregate_kind: "notification".into(),
            aggregate_id: next.id.as_str().into(),
            aggregate_revision: next.revision,
            kind: if ack {
                "NotificationAcknowledged"
            } else {
                "NotificationReadChanged"
            }
            .into(),
            payload: json!({"read":next.read,"acknowledged":next.acknowledged}),
        })?;
        Ok(DomainDecision::Applied {
            result: result(record(&next)?),
        })
    }
}
impl QueryHandler for NotificationService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let result: R<(OperationResult, i64)> = match query.input() {
            OperationInput::Notifications(ops::Input::List(i)) => self
                .list(snapshot, ctx, i)
                .map(|(value, scope)| (OperationResult::NotificationsList { value }, scope)),
            OperationInput::Notifications(ops::Input::Read(i)) => {
                visible(snapshot.decisions(), ctx, &i.notification_id).and_then(|r| {
                    Ok((
                        OperationResult::NotificationsRead { value: record(&r)? },
                        r.revision,
                    ))
                })
            }
            OperationInput::Notifications(ops::Input::SettingsQuery(_)) => {
                preferences(snapshot.configuration(), ctx.principal())
                    .map(|value| {
                        let revision = value.revision.get();
                        (OperationResult::NotificationsSettings { value }, revision)
                    })
                    .map_err(Into::into)
            }
            _ => {
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into())
            }
        };
        let (result, scope) = result.map_err(|f| {
            query_fault(
                f,
                "notification",
                crate::protocol::CorrelationId(self.ids.next_id(IdKind::Correlation)),
            )
        })?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(scope)),
            result,
        })
    }
}
impl CommandHandler for NotificationService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let result = match command.input() {
            OperationInput::Notifications(ops::Input::MarkRead(i)) => self.mutate(
                tx,
                ctx,
                &i.notification_id,
                i.expected_revision.get(),
                Some(i.read),
                false,
            ),
            OperationInput::Notifications(ops::Input::Acknowledge(i)) => self.mutate(
                tx,
                ctx,
                &i.notification_id,
                i.expected_revision.get(),
                None,
                true,
            ),
            OperationInput::Notifications(ops::Input::SettingsCommand(i)) => (|| {
                let old = preferences(tx.configuration(), ctx.principal())?;
                if old.revision != i.expected_revision {
                    return Err(StorageError::conflict(
                        RevisionDimension::Configuration,
                        "notification_settings",
                        ctx.principal().0.clone(),
                        i.expected_revision.clone(),
                        old.revision,
                    )
                    .into());
                }
                if serde_json::to_value(&old.config).expect("preferences")
                    == serde_json::to_value(&i.config).expect("preferences")
                {
                    return Ok(DomainDecision::NoChange {
                        result: OperationResult::NotificationsSettings { value: old },
                    });
                }
                let saved = tx.configuration().append_revision(
                    &ConfigScope::Workspace,
                    &namespace(ctx.principal()),
                    i.expected_revision.get(),
                    1,
                    serde_json::to_value(&i.config).expect("preferences"),
                )?;
                Ok(DomainDecision::Applied {
                    result: OperationResult::NotificationsSettings {
                        value: NotificationSettingsRecord {
                            revision: rev(saved.revision),
                            config: i.config.clone(),
                        },
                    },
                })
            })(),
            _ => {
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into())
            }
        };
        decided(tx, result)
    }
}
