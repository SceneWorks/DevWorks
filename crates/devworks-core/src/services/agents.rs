//! Agent coordination domain rules shared by the native UI and tool gateway.
//! A message is stored with its recipient delivery and project event in one write unit.
pub mod retained;
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::OperationInput;
use crate::operations::agents as ops;
use crate::ports::storage::{
    ActivationPhase, ActorRead, AssignmentState, BlockerFilter, NewPrincipalBinding, ReadSnapshot,
    RunRead, StorageError, StoredAgentAssignmentContext, StoredAgentDelivery, StoredAgentMessage,
    StoredAgentMessageEvent, StoredAssignment, StoredSession, Unavailable, WriteTransaction,
};
use crate::ports::{
    CommandHandler, Decided, HandlerFailure, IdKind, IdSource, Preparation, QueryAnswer,
    QueryHandler,
};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{
    AgentMessage, AgentRecord, CheckpointRecord, MessageClass, MessageDirection, RetainedAckResult,
    RetainedActor, RetainedDeliveryState, RetainedEventPage, RetainedHistoryPage,
    RetainedInboxResult, RetainedItemVersion, RetainedMessage, RetainedMessageEvent,
    RetainedMessageKind, RetainedMessageLine, RetainedMessageResolution, RetainedMessageState,
    RetainedPostResult,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Page};
use crate::protocol::{
    ActorId, AssignmentId, CorrelationId, Counter, DomainErrorCode, DomainFault, Id,
    OperationState, ProjectId, Revision, RevisionDimension,
};
use crate::services::cursor::{self, CursorBinding};
use crate::services::runs::substrate::{Fenced, NewRunAssignment};
use crate::services::runs::{self, ControlService};
use crate::services::sessions::{Failure, Refusal, decided, query_fault, rev};
use crate::settings::catalog::AgentRole;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------------
// Refusals and storage faults
//
// A rule this service checks refuses with its own target kind. A stored row it cannot read is a
// storage fault (`Unavailable::Corrupt`), never the caller's invalid argument.

/// The caller's context, scope or authority does not permit this action on `kind`.
fn denied(kind: &'static str) -> Refusal {
    Refusal::new(DomainErrorCode::PermissionDenied, kind, None)
}

/// The request carries a value for `kind` that its rule does not accept.
fn invalid(kind: &'static str) -> Refusal {
    Refusal::new(DomainErrorCode::InvalidArgument, kind, None)
}

fn missing(kind: &'static str, id: &Id) -> Refusal {
    Refusal::new(DomainErrorCode::NotFound, kind, Some(id.clone()))
}

/// A stored row this service reads is malformed.
fn corrupt(what: &str) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, format!("stored {what}"))
}

/// The retained-message helpers report their own rules as scope and record errors. Their
/// malformed stored rows are `Unavailable::Corrupt`, so these two kinds are always a rule.
fn retained_rule(kind: &'static str) -> impl FnOnce(StorageError) -> Failure {
    move |error| match error {
        StorageError::ScopeDenied => denied(kind).into(),
        StorageError::InvalidRecord(_) => invalid(kind).into(),
        other => other.into(),
    }
}

/// The wire failure of a use-case outcome. A shared helper of another service that denies scope
/// or finds no row keeps its permission or not-found code, and an inbox cursor from another
/// caller restarts. Every other storage error, a corrupt row included, stays a storage fault.
fn failure(error: Failure) -> Failure {
    match error {
        Failure::Storage(StorageError::ScopeDenied) => denied("assignment").into(),
        Failure::Storage(StorageError::NotFound { kind, id }) => {
            Refusal::new(DomainErrorCode::NotFound, kind, Id::new(id).ok()).into()
        }
        Failure::Storage(StorageError::CursorExpired) => {
            Refusal::new(DomainErrorCode::CursorExpired, "agent_inbox_cursor", None)
                .retry(crate::protocol::RetryAdvice::RefreshAndNewRequest)
                .into()
        }
        other => other,
    }
}

fn assignment_scope(
    tx: &mut dyn WriteTransaction,
    recipient: &AssignmentId,
) -> Result<(crate::ports::storage::StoredAssignment, ProjectId), Failure> {
    let assignment = tx
        .runs()
        .assignment(recipient)?
        .ok_or_else(|| missing("assignment", &recipient.0))?;
    let root = tx
        .sessions()
        .work_root(&assignment.root_id)?
        .ok_or_else(|| missing("root", &assignment.root_id.0))?;
    Ok((assignment, root.project_id))
}

fn source_assignment(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    project: &ProjectId,
    root: &crate::protocol::RootId,
) -> Result<Option<AssignmentId>, Failure> {
    if ctx.project_id().is_some_and(|p| p != project) || ctx.root_id().is_some_and(|r| r != root) {
        return Err(denied("root").into());
    }
    if ctx.principal_kind() != PrincipalKind::Agent {
        return Ok(None);
    }
    let id = ctx.assignment_id().ok_or_else(|| denied("assignment"))?;
    let assignment = tx
        .runs()
        .assignment(id)?
        .ok_or_else(|| denied("assignment"))?;
    let session = tx
        .sessions()
        .session(&assignment.session_id)?
        .ok_or_else(|| denied("session"))?;
    if assignment.root_id != *root
        || assignment.state != AssignmentState::Active
        || ctx.session_id() != Some(&assignment.session_id)
        || session.agent_id != *ctx.principal()
        || session.closed()
    {
        return Err(denied("assignment").into());
    }
    let execution = assignment
        .execution_id
        .as_ref()
        .ok_or_else(|| denied("activation"))?;
    let activation = tx
        .runs()
        .activation(execution)?
        .ok_or_else(|| denied("activation"))?;
    if activation.root_id != *root
        || ctx.activation() != Some((execution, activation.owner_epoch))
        || activation.phase != ActivationPhase::Running
        || tx.store().boot_id.as_ref() != Some(&activation.boot_id)
        || !activation
            .lease_until_ms
            .is_some_and(|until| until > tx.now_ms())
    {
        return Err(denied("activation").into());
    }
    Ok(Some(id.clone()))
}

fn sender_can_read_attachment(
    tx: &mut dyn WriteTransaction,
    sender: Option<&AssignmentId>,
    root: &crate::protocol::RootId,
    reference: &ArtifactRef,
) -> Result<bool, Failure> {
    let Some(sender) = sender else {
        return Ok(true);
    };
    let assignment = tx
        .runs()
        .assignment(sender)?
        .ok_or_else(|| denied("assignment"))?;
    let coordinator = assignment.parent_id.is_none();
    let context = tx
        .runs()
        .agent_assignment_context(sender)?
        .ok_or_else(|| corrupt("assignment context"))?;
    if tx
        .artifacts()
        .artifact(&reference.artifact_id)?
        .is_some_and(|artifact| {
            artifact.origin_project_id.as_ref() == Some(&context.project_id)
                && (artifact.provenance["assignmentId"].as_str() == Some(sender.0.as_str())
                    || coordinator && artifact.origin_work_id.as_ref() == Some(root))
        })
    {
        return Ok(true);
    }
    let task_ids: Vec<Id> =
        serde_json::from_value(context.task_ids).map_err(|_| corrupt("assignment task scope"))?;
    for link in tx.artifacts().links(&reference.artifact_id)? {
        if link.purpose != crate::ports::storage::LinkPurpose::Reference
            || link.version_id != reference.version_id
        {
            continue;
        }
        if link.subject.kind == "root"
            && sender_scopes_root_subject(tx, &assignment, &task_ids, root, &link.subject.id)?
            || link.subject.kind == "session" && link.subject.id == assignment.session_id
            || link.subject.kind == "assignment"
                && (link.subject.id == sender.0
                    || coordinator
                        && tx
                            .runs()
                            .assignment(&AssignmentId(link.subject.id))?
                            .is_some_and(|a| a.root_id == *root))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn sender_scopes_root_subject(
    tx: &mut dyn WriteTransaction,
    assignment: &StoredAssignment,
    task_ids: &[Id],
    root: &crate::protocol::RootId,
    subject: &Id,
) -> Result<bool, StorageError> {
    if subject == &root.0 {
        return Ok(true);
    }
    if assignment.parent_id.is_none() {
        return Ok(tx
            .work()
            .work(subject)?
            .is_some_and(|work| work.parent_feature_id.as_ref() == Some(&root.0)));
    }
    if subject != &assignment.work_id.0 && !task_ids.contains(subject) {
        return Ok(false);
    }
    let current_root = tx.sessions().work_root(root)?;
    let current_task = tx
        .sessions()
        .work_root(&crate::protocol::RootId(subject.clone()))?;
    Ok(current_root
        .zip(current_task)
        .is_some_and(|(root_work, task)| {
            task.project_id == root_work.project_id
                && task.kind != "feature"
                && (if root_work.kind == "feature" {
                    task.parent_feature_id.as_ref() == Some(&root_work.id)
                } else {
                    task.id == root_work.id
                })
        }))
}

fn query_source_assignment(
    snapshot: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    root: &crate::protocol::RootId,
) -> Result<AssignmentId, Failure> {
    let id = ctx.assignment_id().ok_or_else(|| denied("assignment"))?;
    let assignment = snapshot
        .runs()
        .assignment(id)?
        .ok_or_else(|| denied("assignment"))?;
    let session = snapshot
        .sessions()
        .session(&assignment.session_id)?
        .ok_or_else(|| denied("session"))?;
    let execution = assignment
        .execution_id
        .as_ref()
        .ok_or_else(|| denied("activation"))?;
    let activation = snapshot
        .runs()
        .activation(execution)?
        .ok_or_else(|| denied("activation"))?;
    if assignment.root_id != *root
        || assignment.state != AssignmentState::Active
        || ctx.session_id() != Some(&assignment.session_id)
        || session.agent_id != *ctx.principal()
        || session.closed()
        || activation.root_id != *root
        || activation.phase != ActivationPhase::Running
        || ctx.activation() != Some((execution, activation.owner_epoch))
    {
        return Err(denied("assignment").into());
    }
    Ok(id.clone())
}

fn class_name(class: &MessageClass) -> &'static str {
    match class {
        MessageClass::Instruction => "Instruction",
        MessageClass::Status => "Status",
        MessageClass::Question => "Question",
        MessageClass::Evidence => "Evidence",
    }
}
fn class_of(name: &str) -> Result<MessageClass, StorageError> {
    match name {
        "Instruction" => Ok(MessageClass::Instruction),
        "Status" => Ok(MessageClass::Status),
        "Question" => Ok(MessageClass::Question),
        "Evidence" => Ok(MessageClass::Evidence),
        _ => Err(corrupt("assignment message class")),
    }
}
fn wire(
    message: &StoredAgentMessage,
    delivery: &StoredAgentDelivery,
) -> Result<AgentMessage, StorageError> {
    let to = message
        .to_assignment_id
        .clone()
        .ok_or_else(|| corrupt("assignment message recipient"))?;
    let attachments: Vec<ArtifactRef> = serde_json::from_value(message.attachments.clone())
        .map_err(|_| corrupt("message attachments"))?;
    Ok(AgentMessage {
        id: message.id.clone(),
        revision: rev(delivery.revision),
        from_assignment_id: message.from_assignment_id.clone(),
        to_assignment_id: to,
        message_class: class_of(&message.message_class)?,
        content: BoundedText(message.body.clone()),
        artifact_refs: BoundedList(attachments),
        acknowledged: matches!(delivery.state.as_str(), "Read" | "Answered" | "Declined"),
    })
}

fn fenced(error: Fenced) -> Failure {
    let code = match error {
        Fenced::NotFound(_) => DomainErrorCode::NotFound,
        Fenced::LeaseLost(_) | Fenced::StopRequested | Fenced::Blocked(_) => {
            DomainErrorCode::Conflict
        }
        Fenced::Invalid(_) => DomainErrorCode::InvalidArgument,
    };
    Refusal::new(code, "assignment", None).into()
}

fn agent_record(
    read: &dyn RunRead,
    assignment: &StoredAssignment,
) -> Result<AgentRecord, StorageError> {
    let context = read
        .agent_assignment_context(&assignment.id)?
        .ok_or_else(|| corrupt("assignment context"))?;
    let checkpoint = assignment
        .checkpoint
        .as_ref()
        .map(|value| {
            serde_json::from_value::<CheckpointRecord>(value.clone())
                .map(|record| record.content)
                .map_err(|_| corrupt("assignment checkpoint"))
        })
        .transpose()?;
    let state = match assignment.state {
        AssignmentState::Ready => OperationState::IntentRecorded,
        AssignmentState::Active => OperationState::Dispatching,
        AssignmentState::Waiting => OperationState::NeedsInput,
        AssignmentState::Completed => OperationState::ObservedComplete,
        AssignmentState::Failed => OperationState::Failed,
        AssignmentState::Superseded => OperationState::CancelledBeforeDispatch,
    };
    let mut blockers =
        runs::active_blockers(read, &BlockerFilter::RootLevel(assignment.root_id.clone()))?;
    blockers.extend(runs::active_blockers(
        read,
        &BlockerFilter::Assignment(assignment.id.clone()),
    )?);
    let blocker_ids = blockers.into_iter().map(|b| b.id).collect();
    Ok(AgentRecord {
        assignment_id: assignment.id.clone(),
        root_id: assignment.root_id.clone(),
        revision: rev(assignment.revision),
        role_id: context.role_id,
        state,
        checkpoint,
        blockers: BoundedList(blocker_ids),
    })
}

fn primary_brief(
    tx: &mut dyn WriteTransaction,
    root: &crate::ports::storage::WorkRoot,
) -> Result<String, Failure> {
    let work = tx.work().work(&root.id.0)?.ok_or_else(|| denied("work"))?;
    if work.project_id != root.project_id
        || work.revision != root.revision
        || !(work.kind == root.kind
            || root.kind == "issue" && work.kind == crate::ports::storage::TASK)
    {
        return Err(denied("work").into());
    }
    let brief = crate::services::work::model::WorkBrief::of(&work)?;
    let repositories = tx.work().work_repositories(&work.id)?;
    let prerequisites = tx.work().open_prerequisites(&work.id)?;
    let approved_plan = if root.kind == "feature" {
        tx.work()
            .feature_plan(&work.id)?
            .and_then(|slot| slot.approved)
            .map(|value| {
                serde_json::from_value::<crate::services::work::model::ActivePlan>(value)
                    .map_err(|_| corrupt("approved plan"))
            })
            .transpose()?
    } else {
        None
    };
    Ok(serde_json::to_string(&serde_json::json!({
        "workId": root.id,
        "projectId": root.project_id,
        "kind": root.kind,
        "revision": root.revision,
        "title": root.title,
        "description": brief.description,
        "goals": brief.goals,
        "acceptance": brief.acceptance,
        "repositories": repositories,
        "openPrerequisites": prerequisites,
        "approvedPlan": approved_plan,
    }))
    .map_err(|_| StorageError::InvalidRecord("coordinator brief serialization".into()))?)
}

fn link_unlinked_redirect_attachments(
    tx: &mut dyn WriteTransaction,
    host: &InvocationContext,
    root: &crate::protocol::RootId,
    project: &ProjectId,
    session: &Id,
) -> Result<(), Failure> {
    let mut after = 0;
    loop {
        let page = tx.runs().instructions(root, after, 500)?;
        let more = page.len() == 500;
        for instruction in page {
            after = instruction.seq;
            if instruction.session_id.as_ref() == Some(session) {
                continue;
            }
            let attachments: Vec<ArtifactRef> = serde_json::from_value(instruction.attachments)
                .map_err(|_| corrupt("redirect attachments"))?;
            for artifact in attachments {
                if !crate::services::sessions::transcript::reference_resolves(
                    tx,
                    host,
                    Some(project),
                    &artifact,
                )? || host.principal_kind() == PrincipalKind::Agent
                    && !sender_can_read_attachment(tx, host.assignment_id(), root, &artifact)?
                {
                    return Err(denied("attachment").into());
                }
                crate::services::artifacts::reference_from_host_owner(
                    tx,
                    host,
                    &artifact,
                    &crate::ports::storage::SubjectKey::session(session),
                    Some(root.clone()),
                )?;
            }
        }
        if !more {
            break;
        }
    }
    Ok(())
}

/// One service for the agent-family gateway; composition registers only qualified operations.
pub struct AgentService {
    ids: Arc<dyn IdSource>,
    runs: Arc<ControlService>,
}
impl AgentService {
    pub fn new(ids: Arc<dyn IdSource>, runs: Arc<ControlService>) -> Self {
        Self { ids, runs }
    }

    /// Materializes the root's one coordinator only after the host owns a running activation.
    /// A live primary is reused with its session and checkpoint; a child never takes its work.
    pub fn materialize_primary(
        &self,
        tx: &mut dyn WriteTransaction,
        host: &InvocationContext,
        activation_id: &crate::protocol::Id,
        owner_epoch: i64,
    ) -> Result<AgentRecord, Failure> {
        if host.principal_kind() != PrincipalKind::Host {
            return Err(Refusal::new(DomainErrorCode::PermissionDenied, "host", None).into());
        }
        let activation = tx.runs().activation(activation_id)?.ok_or_else(|| {
            Refusal::new(
                DomainErrorCode::NotFound,
                "activation",
                Some(activation_id.clone()),
            )
        })?;
        if activation.phase != ActivationPhase::Running || activation.owner_epoch != owner_epoch {
            return Err(Refusal::new(
                DomainErrorCode::Conflict,
                "activation",
                Some(activation_id.clone()),
            )
            .into());
        }
        let root = tx
            .sessions()
            .work_root(&activation.root_id)?
            .ok_or_else(|| denied("root"))?;
        if !runs::is_run_root(&root)
            || host.project_id().is_some_and(|p| p != &root.project_id)
            || host.root_id().is_some_and(|r| r != &root.id)
            || !self.runs.root_permits_dispatch(tx, &root.id)?
        {
            return Err(denied("root").into());
        }
        let role = if root.kind == "feature" {
            AgentRole::FeatureCoordinator
        } else {
            AgentRole::IssueCoordinator
        };
        let now = tx.now_ms();
        let assignment = if let Some(existing) = runs::coordinator(tx.runs(), &root.id)? {
            if existing.purpose != crate::ports::storage::AssignmentPurpose::Primary
                || existing.execution_id.as_ref() != Some(activation_id)
            {
                return Err(Refusal::new(
                    DomainErrorCode::Conflict,
                    "assignment",
                    Some(existing.id.0),
                )
                .into());
            }
            existing
        } else {
            let actor = ActorId(tx.new_id(IdKind::Link));
            tx.actors()
                .register(&actor, PrincipalKind::Agent, role.as_str())?;
            let session = StoredSession {
                id: tx.new_id(IdKind::Session),
                conversation_id: tx.new_id(IdKind::Conversation),
                agent_id: actor,
                project_id: Some(root.project_id.clone()),
                root_id: Some(root.id.clone()),
                title: format!("{}: {}", role.as_str(), root.title),
                context_epoch: 0,
                revision: 1,
                created_by: host.principal().clone(),
                created_at_ms: now,
                closed_at_ms: None,
                closed_by: None,
                next_sequence: 1,
                checkpoint_sequence: 0,
            };
            tx.sessions().insert_session(&session)?;
            self.runs
                .register_assignment(
                    tx,
                    host.principal(),
                    NewRunAssignment {
                        root: root.id.clone(),
                        work: root.id.clone(),
                        parent: None,
                        session: session.id,
                        scope_key: None,
                    },
                )?
                .map_err(fenced)?
        };
        let session = tx
            .sessions()
            .session(&assignment.session_id)?
            .ok_or_else(|| denied("session"))?;
        if session.closed()
            || session.root_id.as_ref() != Some(&root.id)
            || session.project_id.as_ref() != Some(&root.project_id)
            || tx.actors().actor_kind(&session.agent_id)? != Some(PrincipalKind::Agent)
        {
            return Err(denied("session").into());
        }
        if let Some(context) = tx.runs().agent_assignment_context(&assignment.id)? {
            if context.project_id != root.project_id || context.role_id.as_str() != role.as_str() {
                return Err(denied("assignment").into());
            }
        } else {
            let instructions = crate::settings::service::role_instructions(
                tx.configuration(),
                role,
                Some(&root.project_id),
            )
            .map_err(|error| match error {
                crate::settings::service::Failure::Storage(error) => Failure::Storage(error),
                crate::settings::service::Failure::Refused(refusal) => {
                    Refusal::new(refusal.code, refusal.target_kind, refusal.target_id).into()
                }
            })?;
            let brief = primary_brief(tx, &root)?;
            tx.runs()
                .insert_agent_assignment_context(&StoredAgentAssignmentContext {
                    assignment_id: assignment.id.clone(),
                    project_id: root.project_id.clone(),
                    role_id: crate::protocol::Id::new(role.as_str()).map_err(|_| denied("role_id"))?,
                    role_instructions: instructions.text,
                    role_revisions: serde_json::json!({"workspace":instructions.revisions.workspace,"project":instructions.revisions.project}),
                    brief,
                    task_ids: serde_json::json!([]),
                    source_checkpoint_id: None,
                    created_at_ms: now,
                })?;
        }
        // Redirects received before a coordinator existed have no transcript session link.
        // Materialization grants their validated versions to this coordinator's session.
        link_unlinked_redirect_attachments(tx, host, &root.id, &root.project_id, &session.id)?;
        bind_role_authority(tx, host, &session.agent_id, role, &root.project_id)?;
        let assignment = if assignment.state == AssignmentState::Active {
            assignment
        } else {
            self.runs
                .set_assignment_state(
                    tx,
                    host.principal(),
                    &assignment.id,
                    assignment.revision,
                    AssignmentState::Active,
                    None,
                )?
                .map_err(fenced)?
        };
        retained::handover_item(tx, &assignment.id)?;
        retained::handover_direct(tx, &session.id)?;
        Ok(agent_record(tx.runs(), &assignment)?)
    }

    /// A coordinator may finish only after its descendants have settled. The host checks the
    /// exact activation epoch again, so a late turn cannot finish a replacement coordinator.
    pub fn complete_primary(
        &self,
        tx: &mut dyn WriteTransaction,
        host: &InvocationContext,
        assignment_id: &AssignmentId,
        activation_id: &crate::protocol::Id,
        owner_epoch: i64,
    ) -> Result<(), Failure> {
        if host.principal_kind() != PrincipalKind::Host {
            return Err(Refusal::new(DomainErrorCode::PermissionDenied, "host", None).into());
        }
        let activation = tx
            .runs()
            .activation(activation_id)?
            .ok_or_else(|| denied("activation"))?;
        let assignment = tx
            .runs()
            .assignment(assignment_id)?
            .ok_or_else(|| denied("assignment"))?;
        if activation.phase != ActivationPhase::Running
            || activation.owner_epoch != owner_epoch
            || assignment.purpose != crate::ports::storage::AssignmentPurpose::Primary
            || assignment.execution_id.as_ref() != Some(activation_id)
            || assignment.root_id != activation.root_id
            || assignment.state != AssignmentState::Active
            || runs::root_assignments(tx.runs(), &assignment.root_id, true)?
                .iter()
                .any(|child| child.parent_id.is_some())
        {
            return Err(Refusal::new(
                DomainErrorCode::Conflict,
                "assignment",
                Some(assignment.id.0),
            )
            .into());
        }
        self.runs
            .set_assignment_state(
                tx,
                host.principal(),
                assignment_id,
                assignment.revision,
                AssignmentState::Completed,
                None,
            )?
            .map_err(fenced)?;
        Ok(())
    }

    /// A child turn reports completion only after its effects have settled; this transition
    /// checks current activation ownership and descendants, then releases its per-root slot.
    pub fn complete_child(
        &self,
        tx: &mut dyn WriteTransaction,
        host: &InvocationContext,
        assignment_id: &AssignmentId,
        activation_id: &crate::protocol::Id,
        owner_epoch: i64,
    ) -> Result<(), Failure> {
        if host.principal_kind() != PrincipalKind::Host {
            return Err(Refusal::new(DomainErrorCode::PermissionDenied, "host", None).into());
        }
        let activation = tx
            .runs()
            .activation(activation_id)?
            .ok_or_else(|| denied("activation"))?;
        let assignment = tx
            .runs()
            .assignment(assignment_id)?
            .ok_or_else(|| denied("assignment"))?;
        if activation.phase != ActivationPhase::Running
            || activation.owner_epoch != owner_epoch
            || assignment.purpose != crate::ports::storage::AssignmentPurpose::Child
            || assignment.execution_id.as_ref() != Some(activation_id)
            || assignment.root_id != activation.root_id
            || assignment.state != AssignmentState::Active
            || runs::root_assignments(tx.runs(), &assignment.root_id, true)?
                .iter()
                .any(|child| child.parent_id.as_ref() == Some(assignment_id))
        {
            return Err(Refusal::new(
                DomainErrorCode::Conflict,
                "assignment",
                Some(assignment.id.0),
            )
            .into());
        }
        self.runs
            .set_assignment_state(
                tx,
                host.principal(),
                assignment_id,
                assignment.revision,
                AssignmentState::Completed,
                None,
            )?
            .map_err(fenced)?;
        Ok(())
    }

    /// A child whose own turn finished may wait for descendants without occupying a root slot.
    /// Active plus an unheld slot is the durable marker; only this completed-turn path sets it.
    pub fn wait_for_descendants(
        &self,
        tx: &mut dyn WriteTransaction,
        host: &InvocationContext,
        assignment_id: &AssignmentId,
        activation_id: &crate::protocol::Id,
        owner_epoch: i64,
    ) -> Result<(), Failure> {
        if host.principal_kind() != PrincipalKind::Host {
            return Err(Refusal::new(DomainErrorCode::PermissionDenied, "host", None).into());
        }
        let activation = tx
            .runs()
            .activation(activation_id)?
            .ok_or_else(|| denied("activation"))?;
        let mut assignment = tx
            .runs()
            .assignment(assignment_id)?
            .ok_or_else(|| denied("assignment"))?;
        if activation.phase != ActivationPhase::Running
            || activation.owner_epoch != owner_epoch
            || assignment.purpose != crate::ports::storage::AssignmentPurpose::Child
            || assignment.execution_id.as_ref() != Some(activation_id)
            || assignment.root_id != activation.root_id
            || assignment.state != AssignmentState::Active
            || !runs::root_assignments(tx.runs(), &assignment.root_id, true)?
                .iter()
                .any(|child| child.parent_id.as_ref() == Some(assignment_id))
        {
            return Err(Refusal::new(
                DomainErrorCode::Conflict,
                "assignment",
                Some(assignment.id.0),
            )
            .into());
        }
        if assignment.slot_held {
            let expected = assignment.revision;
            assignment.slot_held = false;
            assignment.revision += 1;
            tx.runs().update_assignment(&assignment, expected)?;
        }
        Ok(())
    }

    /// Starts a durable child under the root's current activation. The child may investigate
    /// readiness before implementation; each later effect still needs its own dispatch admission.
    pub fn activate_child(
        &self,
        tx: &mut dyn WriteTransaction,
        host: &InvocationContext,
        assignment_id: &AssignmentId,
        activation_id: &crate::protocol::Id,
        owner_epoch: i64,
    ) -> Result<Result<AgentRecord, Fenced>, Failure> {
        if host.principal_kind() != PrincipalKind::Host {
            return Err(Refusal::new(DomainErrorCode::PermissionDenied, "host", None).into());
        }
        let assignment = tx
            .runs()
            .assignment(assignment_id)?
            .ok_or_else(|| denied("assignment"))?;
        if assignment.purpose != crate::ports::storage::AssignmentPurpose::Child
            || !matches!(
                assignment.state,
                AssignmentState::Ready | AssignmentState::Waiting
            )
            || assignment.execution_id.as_ref() != Some(activation_id)
        {
            return Ok(Err(Fenced::Invalid(
                "the child is not ready under this activation".into(),
            )));
        }
        if let Err(fenced) = self.runs.begin_dispatch(
            tx,
            &crate::services::runs::substrate::DispatchRequest {
                root: assignment.root_id.clone(),
                execution_id: activation_id.clone(),
                owner_epoch,
                target: crate::services::runs::Target::Assignment(assignment_id.clone()),
                purpose: crate::ports::storage::ActivationPurpose::Planning,
            },
        )? {
            return Ok(Err(fenced));
        }
        let current = tx
            .runs()
            .assignment(assignment_id)?
            .ok_or_else(|| denied("assignment"))?;
        if current.purpose != crate::ports::storage::AssignmentPurpose::Child
            || !matches!(
                current.state,
                AssignmentState::Ready | AssignmentState::Waiting
            )
            || current.execution_id.as_ref() != Some(activation_id)
        {
            return Ok(Err(Fenced::Invalid(
                "the child changed during activation admission".into(),
            )));
        }
        let parent = current
            .parent_id
            .as_ref()
            .ok_or_else(|| corrupt("child assignment parent"))?;
        if !tx
            .runs()
            .assignment(parent)?
            .is_some_and(|parent| parent.root_id == current.root_id && parent.state.is_live())
        {
            return Ok(Err(Fenced::Invalid("the child parent has ended".into())));
        }
        let active = match self.runs.set_assignment_state(
            tx,
            host.principal(),
            assignment_id,
            current.revision,
            AssignmentState::Active,
            None,
        )? {
            Ok(active) => active,
            Err(fenced) => return Ok(Err(fenced)),
        };
        let root = tx
            .sessions()
            .work_root(&active.root_id)?
            .ok_or_else(|| denied("root"))?;
        link_unlinked_redirect_attachments(
            tx,
            host,
            &root.id,
            &root.project_id,
            &active.session_id,
        )?;
        bind_child_authority(tx, host, assignment_id)?;
        retained::handover_item(tx, assignment_id)?;
        Ok(Ok(agent_record(tx.runs(), &active)?))
    }

    fn inspect(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::InspectInput,
    ) -> Result<(AgentRecord, i64), Failure> {
        let assignment = snapshot
            .runs()
            .assignment(&input.assignment_id)?
            .ok_or_else(|| missing("assignment", &input.assignment_id.0))?;
        let root = snapshot
            .sessions()
            .work_root(&assignment.root_id)?
            .ok_or_else(|| denied("root"))?;
        if ctx.project_id().is_some_and(|p| p != &root.project_id)
            || ctx.root_id().is_some_and(|r| r != &root.id)
        {
            return Err(denied("root").into());
        }
        if ctx.principal_kind() == PrincipalKind::Agent {
            query_source_assignment(snapshot, ctx, &root.id)?;
        }
        Ok((
            agent_record(snapshot.runs(), &assignment)?,
            root.scope_revision,
        ))
    }

    pub fn delegate(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        input: &ops::DelegateInput,
    ) -> Result<AgentRecord, Failure> {
        let root = tx.sessions().work_root(&input.root_id)?.ok_or_else(|| {
            Refusal::new(
                DomainErrorCode::NotFound,
                "root",
                Some(input.root_id.0.clone()),
            )
        })?;
        if !runs::is_run_root(&root) {
            return Err(
                Refusal::new(DomainErrorCode::InvalidArgument, "root", Some(root.id.0)).into(),
            );
        }
        let parent = source_assignment(tx, ctx, &root.project_id, &root.id)?
            .ok_or_else(|| Refusal::new(DomainErrorCode::PermissionDenied, "assignment", None))?;
        let control = runs::control_of(tx.runs(), &root.id)?;
        if control.revision != input.expected_control_revision.get() {
            return Err(StorageError::conflict(
                RevisionDimension::Control,
                "root",
                root.id.0.clone(),
                input.expected_control_revision.clone(),
                rev(control.revision),
            )
            .into());
        }
        if control.desired != crate::ports::storage::DesiredExecution::Enabled {
            return Err(Refusal::new(DomainErrorCode::Conflict, "root", Some(root.id.0)).into());
        }
        let role = AgentRole::parse(input.role_id.as_str()).ok_or_else(|| {
            Refusal::new(
                DomainErrorCode::InvalidArgument,
                "role_id",
                Some(input.role_id.clone()),
            )
        })?;
        let parent_context = tx
            .runs()
            .agent_assignment_context(&parent)?
            .ok_or_else(|| corrupt("assignment context"))?;
        let parent_role = AgentRole::parse(parent_context.role_id.as_str())
            .ok_or_else(|| corrupt("assignment role"))?;
        let allowed = matches!(role, AgentRole::Worker | AgentRole::CodeReviewer)
            && matches!(
                parent_role,
                AgentRole::FeatureCoordinator | AgentRole::IssueCoordinator | AgentRole::Worker
            );
        if !allowed {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "role_id",
                Some(input.role_id.clone()),
            )
            .into());
        }
        let role_id = crate::protocol::Id::new(role.as_str()).map_err(|_| denied("role_id"))?;
        if input.instruction.0.trim().is_empty() {
            return Err(Refusal::new(DomainErrorCode::InvalidArgument, "instruction", None).into());
        }
        if parent_role == AgentRole::Worker {
            let parent_assignment = tx
                .runs()
                .assignment(&parent)?
                .ok_or_else(|| denied("assignment"))?;
            let parent_tasks: Vec<crate::protocol::Id> =
                serde_json::from_value(parent_context.task_ids.clone())
                    .map_err(|_| corrupt("parent task scope"))?;
            if input.task_ids.0.is_empty() && parent_assignment.work_id != root.id {
                return Err(Refusal::new(DomainErrorCode::PermissionDenied, "task", None).into());
            }
            for task in &input.task_ids.0 {
                if task != &parent_assignment.work_id.0 && !parent_tasks.contains(task) {
                    return Err(Refusal::new(
                        DomainErrorCode::PermissionDenied,
                        "task",
                        Some(task.clone()),
                    )
                    .into());
                }
            }
        }
        for task in &input.task_ids.0 {
            let work = tx
                .sessions()
                .work_root(&crate::protocol::RootId(task.clone()))?
                .ok_or_else(|| {
                    Refusal::new(DomainErrorCode::NotFound, "task", Some(task.clone()))
                })?;
            let within_root = if root.kind == "feature" {
                work.kind != "feature" && work.parent_feature_id.as_ref() == Some(&root.id)
            } else {
                work.kind != "feature" && work.id == root.id
            };
            if work.project_id != root.project_id || !within_root {
                return Err(Refusal::new(
                    DomainErrorCode::PermissionDenied,
                    "task",
                    Some(task.clone()),
                )
                .into());
            }
        }
        if let Some(source) = &input.source_checkpoint_id {
            let assignment = tx
                .runs()
                .assignment(&parent)?
                .ok_or_else(|| denied("assignment"))?;
            let checkpoint: Option<CheckpointRecord> = assignment
                .checkpoint
                .map(|value| {
                    serde_json::from_value(value).map_err(|_| corrupt("parent checkpoint"))
                })
                .transpose()?;
            if checkpoint.as_ref().map(|c| &c.id) != Some(source) {
                return Err(Refusal::new(
                    DomainErrorCode::InvalidArgument,
                    "source_checkpoint_id",
                    Some(source.clone()),
                )
                .into());
            }
        }
        let instructions = crate::settings::service::role_instructions(
            tx.configuration(),
            role,
            Some(&root.project_id),
        )
        .map_err(|error| match error {
            crate::settings::service::Failure::Storage(error) => Failure::Storage(error),
            crate::settings::service::Failure::Refused(refusal) => {
                Refusal::new(refusal.code, refusal.target_kind, refusal.target_id).into()
            }
        })?;
        let actor = ActorId(tx.new_id(IdKind::Link));
        tx.actors()
            .register(&actor, PrincipalKind::Agent, role.as_str())?;
        let now = tx.now_ms();
        let session = StoredSession {
            id: tx.new_id(IdKind::Session),
            conversation_id: tx.new_id(IdKind::Conversation),
            agent_id: actor,
            project_id: Some(root.project_id.clone()),
            root_id: Some(root.id.clone()),
            title: format!("{}: {}", role.as_str(), root.title),
            context_epoch: 0,
            revision: 1,
            created_by: ctx.principal().clone(),
            created_at_ms: now,
            closed_at_ms: None,
            closed_by: None,
            next_sequence: 1,
            checkpoint_sequence: 0,
        };
        tx.sessions().insert_session(&session)?;
        retained::handover_direct(tx, &session.id)?;
        let assignment = self
            .runs
            .register_child_assignment(
                tx,
                ctx.principal(),
                NewRunAssignment {
                    root: root.id.clone(),
                    work: input
                        .task_ids
                        .0
                        .first()
                        .cloned()
                        .map(crate::protocol::RootId)
                        .unwrap_or_else(|| root.id.clone()),
                    parent: Some(parent),
                    session: session.id,
                    scope_key: None,
                },
            )?
            .map_err(fenced)?;
        tx.runs().insert_agent_assignment_context(&StoredAgentAssignmentContext {
            assignment_id:assignment.id.clone(),project_id:root.project_id.clone(),
            role_id,role_instructions:instructions.text,
            role_revisions:serde_json::json!({"workspace":instructions.revisions.workspace,"project":instructions.revisions.project}),
            brief:input.instruction.0.clone(),
            task_ids:serde_json::to_value(&input.task_ids.0).map_err(|_|StorageError::InvalidRecord("task references".into()))?,
            source_checkpoint_id:input.source_checkpoint_id.clone(),created_at_ms:now,
        })?;
        link_unlinked_redirect_attachments(
            tx,
            ctx,
            &root.id,
            &root.project_id,
            &assignment.session_id,
        )?;
        retained::handover_item(tx, &assignment.id)?;
        Ok(agent_record(tx.runs(), &assignment)?)
    }

    fn inbox(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::InboxInput,
    ) -> Result<(Page<AgentMessage>, i64), Failure> {
        let assignment = snapshot
            .runs()
            .assignment(&input.assignment_id)?
            .ok_or_else(|| missing("assignment", &input.assignment_id.0))?;
        let root = snapshot
            .sessions()
            .work_root(&assignment.root_id)?
            .ok_or_else(|| missing("root", &assignment.root_id.0))?;
        if ctx.project_id().is_some_and(|p| p != &root.project_id)
            || ctx.root_id().is_some_and(|r| r != &assignment.root_id)
            || ctx.principal_kind() == PrincipalKind::Agent
                && (ctx.assignment_id() != Some(&assignment.id)
                    || ctx.session_id() != Some(&assignment.session_id))
        {
            return Err(denied("assignment").into());
        }
        if ctx.principal_kind() == PrincipalKind::Agent {
            query_source_assignment(snapshot, ctx, &assignment.root_id)?;
        }
        let project = snapshot
            .projects()
            .project(&root.project_id)?
            .ok_or_else(|| denied("project"))?;
        let direction = match input.direction {
            MessageDirection::Inbound => "Inbound",
            MessageDirection::Outbound => "Outbound",
            MessageDirection::Both => "Both",
        };
        let binding = CursorBinding::new(
            ctx,
            "agents.inbox",
            &[
                assignment.id.0.as_str(),
                direction,
                if input.unacknowledged_only {
                    "unread"
                } else {
                    "all"
                },
            ],
            project.scope_revision,
        );
        let rows = cursor::page(
            &binding,
            &input.page,
            |before, n| {
                let before = before
                    .map(|s| crate::protocol::Id::new(s).map_err(|_| StorageError::CursorExpired))
                    .transpose()?;
                snapshot.runs().agent_assignment_thread(
                    &root.project_id,
                    &assignment.id,
                    direction,
                    input.unacknowledged_only,
                    before.as_ref(),
                    n,
                )
            },
            |(m, _)| m.id.as_str().to_owned(),
        )?;
        let page = rows.try_into_page(|(m, d)| {
            let delivery = d.unwrap_or(StoredAgentDelivery {
                sequence: 0,
                message_id: m.id.clone(),
                project_id: m.project_id.clone(),
                recipient_assignment_id: Some(assignment.id.clone()),
                recipient_session_id: None,
                recipient_human: false,
                state: "Pending".into(),
                revision: 1,
                delivered_at_ms: None,
                read_at_ms: None,
                resolved_at_ms: None,
                handed_over: false,
                handed_to_session_id: None,
            });
            wire(&m, &delivery)
        })?;
        Ok((page, project.scope_revision))
    }
}

fn retained_actor(
    actors: &dyn ActorRead,
    actor: &ActorId,
    session: Option<Id>,
) -> Result<RetainedActor, StorageError> {
    let stored = actors
        .actor(actor)?
        .ok_or_else(|| corrupt("message actor"))?;
    let kind = if session.is_some() {
        "session"
    } else {
        match stored.kind {
            PrincipalKind::Human => "user",
            PrincipalKind::Agent | PrincipalKind::Package | PrincipalKind::McpClient => "agent",
            PrincipalKind::Host | PrincipalKind::Schedule => "system",
        }
    };
    Ok(RetainedActor {
        name: BoundedText(stored.display_name),
        kind: BoundedText(kind.into()),
        session_id: session,
        harness: None,
        model: None,
        host: None,
    })
}

fn retained_message(
    actors: &dyn ActorRead,
    row: StoredAgentMessage,
) -> Result<RetainedMessage, StorageError> {
    let from = retained_actor(actors, &row.from_actor_id, row.sender_session_id.clone())?;
    let resolved_by = row
        .resolved_by_actor_id
        .as_ref()
        .map(|actor| retained_actor(actors, actor, row.resolved_by_session_id.clone()))
        .transpose()?;
    let kind = match row.message_class.as_str() {
        "Info" => RetainedMessageKind::Info,
        "Question" => RetainedMessageKind::Question,
        "Request" => RetainedMessageKind::Request,
        "Blocker" => RetainedMessageKind::Blocker,
        "Announcement" => RetainedMessageKind::Announcement,
        _ => {
            return Err(corrupt("retained message kind"));
        }
    };
    let state = row
        .state
        .as_deref()
        .map(|state| match state {
            "Open" => Ok(RetainedMessageState::Open),
            "Answered" => Ok(RetainedMessageState::Answered),
            "Declined" => Ok(RetainedMessageState::Declined),
            _ => Err(corrupt("retained message state")),
        })
        .transpose()?;
    Ok(RetainedMessage {
        id: row.id,
        project_id: row.project_id,
        channel: BoundedText(row.channel.to_ascii_lowercase()),
        kind,
        from_actor_id: row.from_actor_id,
        from_session_id: row.sender_session_id,
        from,
        body: BoundedText(row.body),
        created_at_ms: Counter::new(row.created_at_ms).map_err(|_| corrupt("message counter"))?,
        state,
        blocking: row.blocking,
        to_session_id: row.target_session_id,
        to_human: row.to_human,
        item_id: row.target_item_id,
        feature_id: row.target_feature_id,
        resource_name: row.resource_name.map(BoundedText),
        in_reply_to_id: row.in_reply_to_id,
        resolved_by_actor_id: row.resolved_by_actor_id,
        resolved_by,
        resolved_at_ms: row
            .resolved_at_ms
            .map(Counter::new)
            .transpose()
            .map_err(|_| corrupt("message counter"))?,
        delivery: None,
        handed_over: None,
    })
}

fn retained_summary(body: &str) -> String {
    let line = body.split('\n').next().unwrap_or_default().trim();
    if line.chars().count() <= 160 {
        line.to_owned()
    } else {
        format!("{}…", line.chars().take(159).collect::<String>())
    }
}

fn retained_line(
    message: RetainedMessage,
    delivery: Option<RetainedDeliveryState>,
    handed_over: bool,
) -> RetainedMessageLine {
    RetainedMessageLine {
        id: message.id,
        kind: message.kind,
        from: message.from.name,
        summary: BoundedText(retained_summary(&message.body.0)),
        created_at_ms: message.created_at_ms,
        from_session_id: message.from_session_id,
        item_id: message.item_id,
        state: message.state,
        blocking: message.blocking.then_some(true),
        delivery,
        handed_over: handed_over.then_some(true),
    }
}

fn retained_history(
    snapshot: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    input: &ops::HistoryInput,
) -> Result<(RetainedHistoryPage, i64), Failure> {
    let channel = input
        .channel
        .as_ref()
        .map(|value| match value.0.to_ascii_lowercase().as_str() {
            "item" => Ok("Item"),
            "project" => Ok("Project"),
            "feature" => Ok("Feature"),
            "resource" => Ok("Resource"),
            "session" => Ok("Session"),
            "human" => Ok("Human"),
            _ => Err(invalid("channel")),
        })
        .transpose()?;
    let limit = u16::try_from(input.limit.as_ref().map_or(50, Revision::get))
        .map_err(|_| invalid("limit"))?;
    let page = retained::history(
        snapshot,
        ctx,
        &retained::MessageHistoryQuery {
            project: input.project_id.clone(),
            channel: channel.map(str::to_owned),
            item: input.item_id.clone(),
            feature: input.feature_id.clone(),
            resource: input.resource_name.as_ref().map(|v| v.0.clone()),
            open_only: input.open_only.unwrap_or(false),
            to_human: input.to_human.unwrap_or(false),
            before: input.before.as_ref().map(Counter::get),
            limit,
        },
    )
    .map_err(retained_rule("agent_message"))?;
    let scope = snapshot
        .projects()
        .project(&input.project_id)?
        .ok_or_else(|| denied("project"))?
        .scope_revision;
    Ok((
        RetainedHistoryPage {
            items: BoundedList(
                page.items
                    .into_iter()
                    .map(|row| retained_message(snapshot.actors(), row))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            next_before: page
                .next_before
                .map(Counter::new)
                .transpose()
                .map_err(|_| corrupt("message counter"))?,
        },
        scope,
    ))
}

fn retained_events(
    snapshot: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    input: &ops::EventsInput,
) -> Result<(RetainedEventPage, i64), Failure> {
    let limit = u16::try_from(input.limit.as_ref().map_or(100, Revision::get))
        .map_err(|_| invalid("limit"))?;
    let page = retained::events(
        snapshot,
        ctx,
        &input.project_id,
        input.after.as_ref().map(Counter::get),
        limit,
    )
    .map_err(retained_rule("agent_message"))?;
    let mut items = Vec::with_capacity(page.items.len());
    for event in page.items {
        let row = snapshot.runs().agent_message(&event.message_id)?;
        if row.as_ref().is_some_and(|row| {
            !matches!(
                row.message_class.as_str(),
                "Info" | "Question" | "Request" | "Blocker" | "Announcement"
            )
        }) {
            continue;
        }
        let message = row
            .map(|row| retained_message(snapshot.actors(), row))
            .transpose()?;
        let actor_session = match event.kind.as_str() {
            "Posted" => message.as_ref().and_then(|m| m.from_session_id.clone()),
            "Answered" | "Declined" => message.as_ref().and_then(|m| {
                m.resolved_by
                    .as_ref()
                    .and_then(|actor| actor.session_id.clone())
            }),
            _ => event.recipient_session_id.clone(),
        };
        let delivery = if event.recipient_human {
            snapshot.runs().agent_delivery(&event.message_id, None)?
        } else if let Some(session) = event.recipient_session_id.as_ref() {
            snapshot
                .runs()
                .agent_session_delivery(&event.message_id, session)?
        } else {
            None
        };
        let delivery_state = delivery
            .as_ref()
            .map(|row| match row.state.as_str() {
                "Pending" => Ok(RetainedDeliveryState::Pending),
                "Delivered" => Ok(RetainedDeliveryState::Delivered),
                "Read" => Ok(RetainedDeliveryState::Read),
                "Answered" => Ok(RetainedDeliveryState::Answered),
                "Declined" => Ok(RetainedDeliveryState::Declined),
                _ => Err(corrupt("retained delivery state")),
            })
            .transpose()?;
        let line = message.map(|m| {
            retained_line(
                m,
                delivery_state,
                delivery.as_ref().is_some_and(|row| row.handed_over),
            )
        });
        let kind = if event.kind == "HandedOver" {
            "handed_over".to_owned()
        } else {
            event.kind.to_ascii_lowercase()
        };
        items.push(RetainedMessageEvent {
            sequence: Counter::new(event.sequence).map_err(|_| corrupt("message counter"))?,
            project_id: event.project_id,
            message_id: event.message_id,
            kind: BoundedText(kind),
            actor: retained_actor(snapshot.actors(), &event.actor_id, actor_session)?,
            created_at_ms: Counter::new(event.created_at_ms)
                .map_err(|_| corrupt("message counter"))?,
            recipient: if event.recipient_human {
                Some(BoundedText("human".into()))
            } else {
                event
                    .recipient_session_id
                    .map(|id| BoundedText(id.as_str().to_owned()))
            },
            message: line,
        });
    }
    let scope = snapshot
        .projects()
        .project(&input.project_id)?
        .ok_or_else(|| denied("project"))?
        .scope_revision;
    Ok((
        RetainedEventPage {
            items: BoundedList(items),
            next_sequence: Counter::new(page.next_sequence)
                .map_err(|_| corrupt("message counter"))?,
        },
        scope,
    ))
}

fn post_retained(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    input: &ops::PostInput,
) -> Result<RetainedPostResult, Failure> {
    if input.session_id.as_ref() != ctx.session_id() && input.session_id.is_some() {
        return Err(denied("session").into());
    }
    let project = if let Some(project) = input.project_id.as_ref().or(ctx.project_id()) {
        project.clone()
    } else if let Some(root) = input.item_id.as_ref().or(input.feature_id.as_ref()) {
        tx.sessions()
            .work_root(root)?
            .ok_or_else(|| denied("work"))?
            .project_id
    } else if let Some(session) = ctx.session_id() {
        tx.sessions()
            .session(session)?
            .ok_or_else(|| denied("session"))?
            .project_id
            .ok_or_else(|| denied("project"))?
    } else {
        return Err(invalid("project_id").into());
    };
    let kind = match input
        .message_kind
        .as_ref()
        .unwrap_or(&RetainedMessageKind::Info)
    {
        RetainedMessageKind::Info => retained::MessageKind::Info,
        RetainedMessageKind::Question => retained::MessageKind::Question,
        RetainedMessageKind::Request => retained::MessageKind::Request,
        RetainedMessageKind::Blocker => retained::MessageKind::Blocker,
        RetainedMessageKind::Announcement => retained::MessageKind::Announcement,
    };
    let resolves = input.resolves.as_ref().map(|state| match state {
        RetainedMessageResolution::Answered => retained::Resolution::Answered,
        RetainedMessageResolution::Declined => retained::Resolution::Declined,
    });
    let posted = retained::post_message(
        tx,
        ctx,
        &retained::PostSpec {
            project,
            body: input.body.0.clone(),
            kind,
            item: input.item_id.clone(),
            feature: input.feature_id.clone(),
            resource: input.resource_name.as_ref().map(|value| value.0.clone()),
            to_session: input.to_session_id.clone(),
            to_human: input.to_human.unwrap_or(false),
            blocking: input.blocking.unwrap_or(false),
            in_reply_to: input.in_reply_to_id.clone(),
            resolves,
            idempotency_key: input.idempotency_key.as_ref().map(|value| value.0.clone()),
        },
    )
    .map_err(retained_rule("agent_message"))?;
    Ok(RetainedPostResult {
        created: posted.created,
        message: retained_message(tx.actors(), posted.message)?,
        recipients: Counter::new(
            i64::try_from(posted.recipients)
                .map_err(|_| StorageError::InvalidRecord("too many retained recipients".into()))?,
        )
        .map_err(|e| StorageError::InvalidRecord(e.into()))?,
        resolved: posted
            .resolved
            .map(|row| retained_message(tx.actors(), row))
            .transpose()?,
        item_versions: (!posted.item_versions.is_empty()).then(|| {
            BoundedList(
                posted
                    .item_versions
                    .into_iter()
                    .map(|item| RetainedItemVersion {
                        key: item.key,
                        version: Revision::new(item.version).expect("positive work version"),
                    })
                    .collect(),
            )
        }),
    })
}

/// The delivery sequence a receive continues after (0: from the start of the inbox).
fn receive_after(
    ctx: &InvocationContext,
    input: &ops::ReceiveInput,
    project: &ProjectId,
    recipient: &retained::Recipient,
) -> Result<i64, StorageError> {
    input.cursor.as_ref().map_or(Ok(0), |cursor| {
        retained::parse_inbox_cursor(&retained::inbox_binding(ctx, project, recipient), &cursor.0)
    })
}

fn receive_selection(input: &ops::ReceiveInput) -> Result<(u16, u64), Refusal> {
    let limit = u16::try_from(input.limit.as_ref().map_or(10, Revision::get))
        .map_err(|_| invalid("limit"))?;
    if !(1..=100).contains(&limit) {
        return Err(invalid("limit"));
    }
    let wait = input.wait_seconds.unwrap_or(0);
    if !(0..=55).contains(&wait) {
        return Err(invalid("wait_seconds"));
    }
    Ok((limit, wait as u64))
}

/// The inbox a receive or ack reads: the calling session's, or a person's named project inbox.
fn inbox_owner(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    named: Option<&ProjectId>,
) -> Result<(ProjectId, retained::Recipient), Failure> {
    if let Some(session_id) = ctx.session_id() {
        let session = tx
            .sessions()
            .session(session_id)?
            .ok_or_else(|| denied("session"))?;
        let project = session.project_id.ok_or_else(|| denied("project"))?;
        if named.is_some_and(|named| named != &project) {
            return Err(denied("project").into());
        }
        Ok((project, retained::Recipient::Session(session_id.clone())))
    } else {
        let project = named.cloned().ok_or_else(|| invalid("project_id"))?;
        Ok((project, retained::Recipient::Human))
    }
}

fn receive_retained(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    input: &ops::ReceiveInput,
) -> Result<(RetainedInboxResult, bool), Failure> {
    if input.session_id.is_some() && input.session_id.as_ref() != ctx.session_id() {
        return Err(denied("session").into());
    }
    let (limit, _) = receive_selection(input)?;
    let (project, recipient) = inbox_owner(tx, ctx, input.project_id.as_ref())?;
    let after = receive_after(ctx, input, &project, &recipient)?;
    let page = retained::deliver_inbox(
        tx,
        ctx,
        &project,
        &recipient,
        after,
        input.include_read.unwrap_or(false),
        limit,
    )
    .map_err(|error| match error {
        // A cursor ahead of the inbox head cannot be one this inbox issued.
        StorageError::CursorExpired => invalid("cursor").into(),
        other => retained_rule("agent_inbox")(other),
    })?;
    let changed = !page.items.is_empty();
    let mut items = Vec::with_capacity(page.items.len());
    for (delivery, row) in page.items {
        let mut message = retained_message(tx.actors(), row)?;
        message.delivery = Some(match delivery.state.as_str() {
            "Pending" => RetainedDeliveryState::Pending,
            "Delivered" => RetainedDeliveryState::Delivered,
            "Read" => RetainedDeliveryState::Read,
            "Answered" => RetainedDeliveryState::Answered,
            "Declined" => RetainedDeliveryState::Declined,
            _ => return Err(corrupt("inbox delivery state").into()),
        });
        message.handed_over = delivery.handed_over.then_some(true);
        items.push(message);
    }
    Ok((
        RetainedInboxResult {
            items: BoundedList(items),
            next_cursor: BoundedText(retained::inbox_cursor(
                &retained::inbox_binding(ctx, &project, &recipient),
                page.next_sequence,
            )?),
        },
        changed,
    ))
}

fn ack_retained(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    input: &ops::AckInput,
) -> Result<RetainedAckResult, Failure> {
    if input.session_id.is_some() && input.session_id.as_ref() != ctx.session_id() {
        return Err(denied("session").into());
    }
    // An ack names message ids or an inbox cursor; only ids carry a resolution.
    if input.messages.is_some() == input.up_to.is_some()
        || (input.resolution.is_some() && input.messages.is_none())
    {
        return Err(invalid("ack_selection").into());
    }
    let (project, recipient) = inbox_owner(tx, ctx, input.project_id.as_ref())?;
    let selection = match (&input.messages, &input.up_to) {
        (Some(ids), None) => retained::AckSelection::Messages(ids.0.clone()),
        (None, Some(cursor)) => retained::AckSelection::Through(retained::parse_inbox_cursor(
            &retained::inbox_binding(ctx, &project, &recipient),
            &cursor.0,
        )?),
        _ => unreachable!("validated one ack selection"),
    };
    let resolution = input.resolution.as_ref().map(|value| match value {
        RetainedMessageResolution::Answered => retained::Resolution::Answered,
        RetainedMessageResolution::Declined => retained::Resolution::Declined,
    });
    let acked = retained::acknowledge(tx, ctx, &project, &recipient, selection, resolution)
        .map_err(|error| match error {
            // An `up_to` ahead of the inbox head cannot be a cursor this inbox issued.
            StorageError::CursorExpired => invalid("up_to").into(),
            other => retained_rule("agent_inbox")(other),
        })?;
    let count = acked.changed.len();
    let mut lines = Vec::new();
    for (delivery, row) in acked.changed.into_iter().take(50) {
        let message = retained_message(tx.actors(), row)?;
        let delivery_state = match delivery.state.as_str() {
            "Pending" => RetainedDeliveryState::Pending,
            "Delivered" => RetainedDeliveryState::Delivered,
            "Read" => RetainedDeliveryState::Read,
            "Answered" => RetainedDeliveryState::Answered,
            "Declined" => RetainedDeliveryState::Declined,
            _ => return Err(corrupt("inbox delivery state").into()),
        };
        lines.push(retained_line(
            message,
            Some(delivery_state),
            delivery.handed_over,
        ));
    }
    Ok(RetainedAckResult {
        acked: Counter::new(
            i64::try_from(count).map_err(|_| StorageError::InvalidRecord("ack count".into()))?,
        )
        .map_err(|e| StorageError::InvalidRecord(e.into()))?,
        messages: BoundedList(lines),
        item_versions: (!acked.item_versions.is_empty()).then(|| {
            BoundedList(
                acked
                    .item_versions
                    .into_iter()
                    .map(|item| RetainedItemVersion {
                        key: item.key,
                        version: Revision::new(item.version).expect("positive work version"),
                    })
                    .collect(),
            )
        }),
    })
}

/// The revision-one assignment send. Gateway receipt replay surrounds this transaction;
/// storage records and event sequence commit with that receipt.
pub fn post_assignment(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    input: &ops::MessagesInput,
) -> Result<AgentMessage, Failure> {
    let (recipient, project) = assignment_scope(tx, &input.recipient_assignment_id)?;
    if !recipient.state.is_live() {
        return Err(denied("assignment").into());
    }
    let sender = source_assignment(tx, ctx, &project, &recipient.root_id)?;
    if input.content.0.trim().is_empty() {
        return Err(invalid("content").into());
    }
    for attachment in &input.attachments.0 {
        if !crate::services::sessions::transcript::reference_resolves(
            tx,
            ctx,
            Some(&project),
            attachment,
        )? || !sender_can_read_attachment(tx, sender.as_ref(), &recipient.root_id, attachment)?
        {
            return Err(denied("attachment").into());
        }
    }
    // Delivery itself grants the exact immutable version to this recipient. Record that grant
    // with the message in the same transaction so an inbox reference is immediately readable.
    for attachment in &input.attachments.0 {
        crate::services::artifacts::reference_from_host_owner(
            tx,
            ctx,
            attachment,
            &crate::ports::storage::SubjectKey {
                kind: "assignment",
                id: recipient.id.0.clone(),
            },
            Some(recipient.root_id.clone()),
        )?;
    }
    let now = tx.now_ms();
    let message = StoredAgentMessage {
        id: tx.new_id(IdKind::Event),
        project_id: project.clone(),
        root_id: Some(recipient.root_id.clone()),
        channel: "Assignment".into(),
        from_assignment_id: sender,
        to_assignment_id: Some(recipient.id.clone()),
        message_class: class_name(&input.message_class).into(),
        body: input.content.0.clone(),
        attachments: serde_json::to_value(&input.attachments.0)
            .map_err(|_| StorageError::InvalidRecord("attachments".into()))?,
        in_reply_to_id: None,
        state: matches!(input.message_class, MessageClass::Question).then(|| "Open".into()),
        blocking: false,
        idempotency_key: None,
        revision: 1,
        created_at_ms: now,
        resolved_at_ms: None,
        from_actor_id: ctx.principal().clone(),
        target_item_id: None,
        target_feature_id: None,
        resource_name: None,
        target_session_id: None,
        to_human: false,
        sender_session_id: ctx.session_id().cloned(),
        resolved_by_actor_id: None,
        resolved_by_session_id: None,
    };
    let delivery = StoredAgentDelivery {
        sequence: tx.runs().agent_delivery_head()? + 1,
        message_id: message.id.clone(),
        project_id: project.clone(),
        recipient_assignment_id: Some(recipient.id.clone()),
        recipient_session_id: None,
        recipient_human: false,
        state: "Pending".into(),
        revision: 1,
        delivered_at_ms: None,
        read_at_ms: None,
        resolved_at_ms: None,
        handed_over: false,
        handed_to_session_id: None,
    };
    let event = StoredAgentMessageEvent {
        sequence: tx.runs().agent_event_head()? + 1,
        project_id: project,
        message_id: message.id.clone(),
        kind: "Posted".into(),
        actor_id: ctx.principal().clone(),
        recipient_assignment_id: Some(recipient.id),
        recipient_session_id: None,
        recipient_human: false,
        created_at_ms: now,
    };
    tx.runs().insert_agent_message(&message)?;
    tx.runs().insert_agent_delivery(&delivery)?;
    tx.runs().insert_agent_message_event(&event)?;
    Ok(wire(&message, &delivery)?)
}

/// Explicitly marks one assignment delivery read. Inbox observation is pure; only this command
/// advances delivery state, so a lost read reply can replay the same pending message.
pub fn acknowledge_assignment(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    input: &ops::AcknowledgeInput,
) -> Result<AgentMessage, Failure> {
    let (assignment, project) = assignment_scope(tx, &input.assignment_id)?;
    if !assignment.state.is_live()
        || ctx.principal_kind() != PrincipalKind::Agent
        || ctx.assignment_id() != Some(&assignment.id)
        || ctx.session_id() != Some(&assignment.session_id)
        || ctx.project_id().is_some_and(|p| p != &project)
    {
        return Err(denied("assignment").into());
    }
    let session = tx
        .sessions()
        .session(&assignment.session_id)?
        .ok_or_else(|| denied("session"))?;
    if session.closed() || session.agent_id != *ctx.principal() {
        return Err(denied("session").into());
    }
    let message = tx
        .runs()
        .agent_message(&input.message_id)?
        .ok_or_else(|| missing("agent_message", &input.message_id))?;
    if message.project_id != project || message.to_assignment_id.as_ref() != Some(&assignment.id) {
        return Err(denied("agent_message").into());
    }
    let mut delivery = tx
        .runs()
        .agent_delivery(&message.id, Some(&assignment.id))?
        .ok_or_else(|| corrupt("assignment message delivery"))?;
    if delivery.revision != input.expected_revision.get() {
        return Err(StorageError::conflict(
            RevisionDimension::Store,
            "agent_delivery",
            message.id.clone(),
            input.expected_revision.clone(),
            rev(delivery.revision),
        )
        .into());
    }
    if matches!(delivery.state.as_str(), "Read" | "Answered" | "Declined") {
        return Ok(wire(&message, &delivery)?);
    }
    delivery.state = "Read".into();
    delivery.revision += 1;
    delivery.delivered_at_ms.get_or_insert(tx.now_ms());
    delivery.read_at_ms = Some(tx.now_ms());
    tx.runs()
        .update_agent_delivery(&delivery, input.expected_revision.get())?;
    let event = StoredAgentMessageEvent {
        sequence: tx.runs().agent_event_head()? + 1,
        project_id: project,
        message_id: message.id.clone(),
        kind: "Read".into(),
        actor_id: ctx.principal().clone(),
        recipient_assignment_id: Some(assignment.id),
        recipient_session_id: None,
        recipient_human: false,
        created_at_ms: tx.now_ms(),
    };
    tx.runs().insert_agent_message_event(&event)?;
    Ok(wire(&message, &delivery)?)
}

/// A checkpoint is bound to the assignment's own transcript position and advances its revision.
/// It cannot attest to a future entry or replace a newer checkpoint.
pub fn checkpoint_assignment(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    input: &ops::CheckpointInput,
) -> Result<CheckpointRecord, Failure> {
    let (mut assignment, project) = assignment_scope(tx, &input.assignment_id)?;
    if !assignment.state.is_live()
        || ctx.assignment_id() != Some(&assignment.id)
        || ctx.session_id() != Some(&assignment.session_id)
        || ctx.project_id().is_some_and(|p| p != &project)
    {
        return Err(denied("assignment").into());
    }
    let session = tx
        .sessions()
        .session(&assignment.session_id)?
        .ok_or_else(|| denied("session"))?;
    if session.closed() || session.agent_id != *ctx.principal() {
        return Err(denied("session").into());
    }
    if assignment.revision != input.expected_revision.get() {
        return Err(StorageError::conflict(
            RevisionDimension::Work,
            "assignment",
            assignment.id.0.clone(),
            input.expected_revision.clone(),
            rev(assignment.revision),
        )
        .into());
    }
    // A checkpoint attests to a durable transcript entry, never a future one or an older one.
    let sequence = input.transcript_sequence.get();
    if sequence > session.last_durable_sequence() {
        return Err(invalid("transcript_sequence").into());
    }
    if let Some(previous) = &assignment.checkpoint {
        let previous: CheckpointRecord = serde_json::from_value(previous.clone())
            .map_err(|_| corrupt("assignment checkpoint"))?;
        if sequence < previous.transcript_sequence.get() {
            return Err(invalid("transcript_sequence").into());
        }
    }
    if !crate::services::sessions::transcript::reference_resolves(
        tx,
        ctx,
        Some(&project),
        &input.content,
    )? {
        return Err(denied("content").into());
    }
    let record = CheckpointRecord {
        id: tx.new_id(IdKind::Event),
        revision: rev(assignment.revision + 1),
        assignment_id: assignment.id.clone(),
        transcript_sequence: input.transcript_sequence.clone(),
        content: input.content.clone(),
    };
    assignment.checkpoint = Some(
        serde_json::to_value(&record)
            .map_err(|_| StorageError::InvalidRecord("checkpoint serialization".into()))?,
    );
    assignment.revision += 1;
    tx.runs()
        .update_assignment(&assignment, input.expected_revision.get())?;
    let root = tx
        .sessions()
        .work_root(&assignment.root_id)?
        .ok_or_else(|| missing("root", &assignment.root_id.0))?;
    crate::services::runs::record(
        tx,
        ctx.principal(),
        &root,
        None,
        "AgentAssignment",
        assignment.id.0.as_str(),
        assignment.revision,
        "AssignmentCheckpointed",
        serde_json::json!({"checkpointId":record.id.as_str(),"transcriptSequence":sequence}),
    )?;
    Ok(record)
}

fn bind_role_authority(
    tx: &mut dyn WriteTransaction,
    host: &InvocationContext,
    actor: &ActorId,
    role: AgentRole,
    project: &ProjectId,
) -> Result<(), Failure> {
    if host.principal_kind() != PrincipalKind::Host {
        return Err(Refusal::new(DomainErrorCode::PermissionDenied, "host", None).into());
    }
    if let Some(existing) = tx.authority().principal(actor)? {
        if existing.active
            && existing.kind == PrincipalKind::Agent
            && existing.role == Some(role)
            && existing.project_id.as_ref() == Some(project)
        {
            return Ok(());
        }
        return Err(Refusal::new(
            DomainErrorCode::PermissionDenied,
            "agent_authority",
            Some(actor.0.clone()),
        )
        .into());
    }
    crate::authority::service::bind_principal(
        tx,
        host,
        NewPrincipalBinding {
            actor: actor.clone(),
            kind: PrincipalKind::Agent,
            role: Some(role),
            project_id: Some(project.clone()),
            tool_families: None,
            bound_by: host.principal().clone(),
        },
        None,
    )
    .map_err(|error| match error {
        crate::authority::service::AdminError::Storage(error) => Failure::Storage(error),
        crate::authority::service::AdminError::Denied(_) => {
            Refusal::new(DomainErrorCode::PermissionDenied, "host", None).into()
        }
        crate::authority::service::AdminError::Invalid(_) => {
            Refusal::new(DomainErrorCode::InvalidArgument, "agent_authority", None).into()
        }
    })?;
    Ok(())
}

/// Trusted host materialization of a recorded child intent. An agent's delegate command cannot
/// mint this authority; the host supervisor calls this before launching the child session.
pub fn bind_child_authority(
    tx: &mut dyn WriteTransaction,
    host: &InvocationContext,
    assignment_id: &AssignmentId,
) -> Result<ActorId, Failure> {
    if host.principal_kind() != PrincipalKind::Host {
        return Err(Refusal::new(DomainErrorCode::PermissionDenied, "host", None).into());
    }
    let assignment = tx.runs().assignment(assignment_id)?.ok_or_else(|| {
        Refusal::new(
            DomainErrorCode::NotFound,
            "assignment",
            Some(assignment_id.0.clone()),
        )
    })?;
    if assignment.purpose != crate::ports::storage::AssignmentPurpose::Child
        || !assignment.state.is_live()
    {
        return Err(Refusal::new(
            DomainErrorCode::Conflict,
            "assignment",
            Some(assignment.id.0),
        )
        .into());
    }
    let root = tx
        .sessions()
        .work_root(&assignment.root_id)?
        .ok_or_else(|| denied("root"))?;
    let context = tx
        .runs()
        .agent_assignment_context(assignment_id)?
        .ok_or_else(|| corrupt("assignment context"))?;
    if context.project_id != root.project_id {
        return Err(denied("assignment").into());
    }
    let role =
        AgentRole::parse(context.role_id.as_str()).ok_or_else(|| corrupt("assignment role"))?;
    let session = tx
        .sessions()
        .session(&assignment.session_id)?
        .ok_or_else(|| denied("session"))?;
    let actor = session.agent_id;
    if tx.actors().actor_kind(&actor)? != Some(PrincipalKind::Agent) {
        return Err(denied("session").into());
    }
    bind_role_authority(tx, host, &actor, role, &root.project_id)?;
    Ok(actor)
}

impl QueryHandler for AgentService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let outcome = match query.input() {
            OperationInput::Agents(ops::Input::Inbox(input)) => self
                .inbox(snapshot, ctx, input)
                .map(|(value, scope)| (OperationResult::AgentsInbox { value }, scope)),
            OperationInput::Agents(ops::Input::Inspect(input)) => self
                .inspect(snapshot, ctx, input)
                .map(|(value, scope)| (OperationResult::AgentsInspect { value }, scope)),
            OperationInput::Agents(ops::Input::History(input)) => {
                retained_history(snapshot, ctx, input)
                    .map(|(value, scope)| (OperationResult::AgentsHistory { value }, scope))
            }
            OperationInput::Agents(ops::Input::Events(input)) => {
                retained_events(snapshot, ctx, input)
                    .map(|(value, scope)| (OperationResult::AgentsEvents { value }, scope))
            }
            _ => {
                return Err(Refusal::new(
                    DomainErrorCode::UnsupportedCapability,
                    "operation",
                    None,
                )
                .fault(CorrelationId(self.ids.next_id(IdKind::Correlation))));
            }
        };
        let (result, scope) = outcome.map_err(|e| {
            query_fault(
                failure(e),
                "assignment",
                CorrelationId(self.ids.next_id(IdKind::Correlation)),
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

impl CommandHandler for AgentService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<crate::ports::Prepared>, StorageError> {
        let OperationInput::Agents(ops::Input::Receive(input)) = command.input() else {
            return Ok(None);
        };
        let Ok((limit, wait)) = receive_selection(input) else {
            return Ok(None);
        };
        if wait == 0 {
            return Ok(None);
        }
        let deadline = Instant::now() + Duration::from_secs(wait);
        loop {
            if env.cancellation.is_some_and(|token| token.is_cancelled()) {
                return Err(StorageError::Cancelled);
            }
            let available = env.read(|snapshot| {
                if snapshot.receipts().lookup(command.identity())?.is_some() {
                    return Ok(true);
                }
                if input.session_id.is_some() && input.session_id.as_ref() != ctx.session_id() {
                    return Err(StorageError::ScopeDenied);
                }
                if snapshot.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind()) {
                    return Err(StorageError::ScopeDenied);
                }
                let (project, recipient) = if let Some(session_id) = ctx.session_id() {
                    if ctx.principal_kind() != PrincipalKind::Agent {
                        return Err(StorageError::ScopeDenied);
                    }
                    let session = snapshot
                        .sessions()
                        .session(session_id)?
                        .ok_or(StorageError::ScopeDenied)?;
                    if session.closed() || session.agent_id != *ctx.principal() {
                        return Err(StorageError::ScopeDenied);
                    }
                    let project = session.project_id.ok_or(StorageError::ScopeDenied)?;
                    if input
                        .project_id
                        .as_ref()
                        .is_some_and(|named| named != &project)
                    {
                        return Err(StorageError::ScopeDenied);
                    }
                    (project, retained::Recipient::Session(session_id.clone()))
                } else {
                    if ctx.principal_kind() != PrincipalKind::Human {
                        return Err(StorageError::ScopeDenied);
                    }
                    let project = input.project_id.clone().ok_or_else(|| {
                        StorageError::InvalidRecord("human inbox needs project".into())
                    })?;
                    (project, retained::Recipient::Human)
                };
                if ctx.project_id().is_some_and(|scope| scope != &project)
                    || snapshot.projects().project(&project)?.is_none()
                {
                    return Err(StorageError::ScopeDenied);
                }
                let after = receive_after(ctx, input, &project, &recipient)?;
                if after > snapshot.runs().agent_delivery_head()? {
                    return Err(StorageError::CursorExpired);
                }
                let rows = match recipient {
                    retained::Recipient::Session(ref session_id) => {
                        snapshot.runs().agent_session_inbox(
                            &project,
                            session_id,
                            after,
                            input.include_read.unwrap_or(false),
                            limit,
                        )?
                    }
                    retained::Recipient::Human => snapshot.runs().agent_inbox(
                        &project,
                        None,
                        after,
                        input.include_read.unwrap_or(false),
                        limit,
                    )?,
                };
                Ok(!rows.is_empty())
            });
            // A rule this check meets is refused by `decide` with its own target; only a
            // storage fault ends the wait here.
            let available = match available {
                Ok(available) => available,
                Err(
                    StorageError::ScopeDenied
                    | StorageError::InvalidRecord(_)
                    | StorageError::CursorExpired,
                ) => return Ok(None),
                Err(error) => return Err(error),
            };
            if available || Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(std::cmp::min(
                Duration::from_millis(500),
                deadline.saturating_duration_since(Instant::now()),
            ));
        }
    }

    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let result = match command.input() {
            OperationInput::Agents(ops::Input::Delegate(input)) => {
                let result = self
                    .delegate(tx, ctx, input)
                    .map(|value| DomainDecision::Applied {
                        result: OperationResult::AgentsDelegate { value },
                    });
                return decided(tx, result);
            }
            OperationInput::Agents(ops::Input::Messages(input)) => post_assignment(tx, ctx, input)
                .map(|value| DomainDecision::Applied {
                    result: OperationResult::AgentsMessages { value },
                }),
            OperationInput::Agents(ops::Input::Post(input)) => {
                post_retained(tx, ctx, input).map(|value| {
                    let result = OperationResult::AgentsPost {
                        value: value.clone(),
                    };
                    if value.created {
                        DomainDecision::Applied { result }
                    } else {
                        DomainDecision::NoChange { result }
                    }
                })
            }
            OperationInput::Agents(ops::Input::Receive(input)) => receive_retained(tx, ctx, input)
                .map(|(value, changed)| {
                    let result = OperationResult::AgentsReceive { value };
                    if changed {
                        DomainDecision::Applied { result }
                    } else {
                        DomainDecision::NoChange { result }
                    }
                }),
            OperationInput::Agents(ops::Input::Ack(input)) => {
                ack_retained(tx, ctx, input).map(|value| {
                    let changed = value.acked.get() > 0;
                    let result = OperationResult::AgentsAck { value };
                    if changed {
                        DomainDecision::Applied { result }
                    } else {
                        DomainDecision::NoChange { result }
                    }
                })
            }
            OperationInput::Agents(ops::Input::Acknowledge(input)) => {
                acknowledge_assignment(tx, ctx, input).map(|value| {
                    let result = OperationResult::AgentsAcknowledge {
                        value: value.clone(),
                    };
                    if value.acknowledged && value.revision == input.expected_revision {
                        DomainDecision::NoChange { result }
                    } else {
                        DomainDecision::Applied { result }
                    }
                })
            }
            OperationInput::Agents(ops::Input::Checkpoint(input)) => {
                checkpoint_assignment(tx, ctx, input).map(|value| DomainDecision::Applied {
                    result: OperationResult::AgentsCheckpoint { value },
                })
            }
            _ => {
                return decided(
                    tx,
                    Err(
                        Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None)
                            .into(),
                    ),
                );
            }
        };
        decided(tx, result.map_err(failure))
    }
}
