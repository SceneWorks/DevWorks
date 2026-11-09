//! Agent coordination domain rules shared by the native UI and tool gateway.
//! A message is stored with its recipient delivery and project event in one write unit.
pub mod retained;
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::OperationInput;
use crate::operations::agents as ops;
use crate::ports::storage::{
    ActivationPhase, AssignmentState, BlockerFilter, NewPrincipalBinding, ReadSnapshot, RunRead,
    StorageError, StoredAgentAssignmentContext, StoredAgentDelivery, StoredAgentMessage,
    StoredAgentMessageEvent, StoredAssignment, StoredSession, WriteTransaction,
};
use crate::ports::{
    CommandHandler, Decided, HandlerFailure, IdKind, IdSource, QueryAnswer, QueryHandler,
};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{
    AgentMessage, AgentRecord, CheckpointRecord, MessageClass, MessageDirection,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Page};
use crate::protocol::{
    ActorId, AssignmentId, CorrelationId, DomainErrorCode, DomainFault, OperationState, ProjectId,
    RevisionDimension,
};
use crate::services::cursor::CursorBinding;
use crate::services::runs::substrate::{Fenced, NewRunAssignment};
use crate::services::runs::{self, ControlService};
use crate::services::sessions::{Failure, Refusal, decided, query_fault, rev};
use crate::settings::catalog::AgentRole;
use std::sync::Arc;

fn denied() -> StorageError {
    StorageError::ScopeDenied
}

fn assignment_scope(
    tx: &mut dyn WriteTransaction,
    recipient: &AssignmentId,
) -> Result<(crate::ports::storage::StoredAssignment, ProjectId), StorageError> {
    let assignment = tx
        .runs()
        .assignment(recipient)?
        .ok_or_else(|| StorageError::NotFound {
            kind: "assignment",
            id: recipient.0.as_str().into(),
        })?;
    let root = tx
        .sessions()
        .work_root(&assignment.root_id)?
        .ok_or_else(|| StorageError::NotFound {
            kind: "root",
            id: assignment.root_id.0.as_str().into(),
        })?;
    Ok((assignment, root.project_id))
}

fn source_assignment(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    project: &ProjectId,
    root: &crate::protocol::RootId,
) -> Result<Option<AssignmentId>, StorageError> {
    if ctx.project_id().is_some_and(|p| p != project) || ctx.root_id().is_some_and(|r| r != root) {
        return Err(denied());
    }
    if ctx.principal_kind() != PrincipalKind::Agent {
        return Ok(None);
    }
    let id = ctx.assignment_id().ok_or_else(denied)?;
    let assignment = tx.runs().assignment(id)?.ok_or_else(denied)?;
    let session = tx
        .sessions()
        .session(&assignment.session_id)?
        .ok_or_else(denied)?;
    if assignment.root_id != *root
        || assignment.state != AssignmentState::Active
        || ctx.session_id() != Some(&assignment.session_id)
        || session.agent_id != *ctx.principal()
        || session.closed()
    {
        return Err(denied());
    }
    let execution = assignment.execution_id.as_ref().ok_or_else(denied)?;
    let activation = tx.runs().activation(execution)?.ok_or_else(denied)?;
    if activation.root_id != *root
        || activation.phase != ActivationPhase::Running
        || tx.store().boot_id.as_ref() != Some(&activation.boot_id)
        || !activation
            .lease_until_ms
            .is_some_and(|until| until > tx.now_ms())
    {
        return Err(denied());
    }
    Ok(Some(id.clone()))
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
        _ => Err(StorageError::InvalidRecord(
            "message is outside the assignment contract".into(),
        )),
    }
}
fn wire(
    message: &StoredAgentMessage,
    delivery: &StoredAgentDelivery,
) -> Result<AgentMessage, StorageError> {
    let to = message
        .to_assignment_id
        .clone()
        .ok_or_else(|| StorageError::InvalidRecord("assignment message has no recipient".into()))?;
    let attachments: Vec<ArtifactRef> = serde_json::from_value(message.attachments.clone())
        .map_err(|_| StorageError::InvalidRecord("invalid stored message attachments".into()))?;
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

fn failure(error: StorageError) -> Failure {
    match error {
        StorageError::ScopeDenied => {
            Refusal::new(DomainErrorCode::PermissionDenied, "assignment", None).into()
        }
        StorageError::NotFound { kind, id } => Refusal::new(
            DomainErrorCode::NotFound,
            kind,
            crate::protocol::Id::new(id).ok(),
        )
        .into(),
        StorageError::InvalidRecord(_) => {
            Refusal::new(DomainErrorCode::InvalidArgument, "agent_message", None).into()
        }
        other => other.into(),
    }
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
        .ok_or_else(|| StorageError::InvalidRecord("assignment has no role context".into()))?;
    let checkpoint = assignment
        .checkpoint
        .as_ref()
        .map(|value| {
            serde_json::from_value::<CheckpointRecord>(value.clone())
                .map(|record| record.content)
                .map_err(|_| StorageError::InvalidRecord("invalid stored checkpoint".into()))
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
            .ok_or_else(denied)?;
        if !runs::is_run_root(&root)
            || host.project_id().is_some_and(|p| p != &root.project_id)
            || host.root_id().is_some_and(|r| r != &root.id)
            || !self.runs.root_permits_dispatch(tx, &root.id)?
        {
            return Err(denied().into());
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
            .ok_or_else(denied)?;
        if session.closed()
            || session.root_id.as_ref() != Some(&root.id)
            || session.project_id.as_ref() != Some(&root.project_id)
            || tx.actors().actor_kind(&session.agent_id)? != Some(PrincipalKind::Agent)
        {
            return Err(denied().into());
        }
        if let Some(context) = tx.runs().agent_assignment_context(&assignment.id)? {
            if context.project_id != root.project_id || context.role_id.as_str() != role.as_str() {
                return Err(denied().into());
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
            tx.runs()
                .insert_agent_assignment_context(&StoredAgentAssignmentContext {
                    assignment_id: assignment.id.clone(),
                    project_id: root.project_id.clone(),
                    role_id: crate::protocol::Id::new(role.as_str()).map_err(|_| denied())?,
                    role_instructions: instructions.text,
                    role_revisions: serde_json::json!({"workspace":instructions.revisions.workspace,"project":instructions.revisions.project}),
                    brief: root.title,
                    task_ids: serde_json::json!([]),
                    source_checkpoint_id: None,
                    created_at_ms: now,
                })?;
        }
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
        agent_record(tx.runs(), &assignment).map_err(failure)
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
        let activation = tx.runs().activation(activation_id)?.ok_or_else(denied)?;
        let assignment = tx.runs().assignment(assignment_id)?.ok_or_else(denied)?;
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

    fn inspect(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::InspectInput,
    ) -> Result<(AgentRecord, i64), StorageError> {
        let assignment = snapshot
            .runs()
            .assignment(&input.assignment_id)?
            .ok_or_else(|| StorageError::NotFound {
                kind: "assignment",
                id: input.assignment_id.0.as_str().into(),
            })?;
        let root = snapshot
            .sessions()
            .work_root(&assignment.root_id)?
            .ok_or_else(denied)?;
        if ctx.project_id().is_some_and(|p| p != &root.project_id)
            || ctx.root_id().is_some_and(|r| r != &root.id)
        {
            return Err(denied());
        }
        if ctx.principal_kind() == PrincipalKind::Agent {
            let caller = ctx.assignment_id().ok_or_else(denied)?;
            let source = snapshot.runs().assignment(caller)?.ok_or_else(denied)?;
            let session = snapshot
                .sessions()
                .session(&source.session_id)?
                .ok_or_else(denied)?;
            if source.root_id != root.id
                || !source.state.is_live()
                || ctx.session_id() != Some(&source.session_id)
                || session.agent_id != *ctx.principal()
            {
                return Err(denied());
            }
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
        let parent = source_assignment(tx, ctx, &root.project_id, &root.id)
            .map_err(failure)?
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
        if input.instruction.0.trim().is_empty() {
            return Err(Refusal::new(DomainErrorCode::InvalidArgument, "instruction", None).into());
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
            let assignment = tx.runs().assignment(&parent)?.ok_or_else(denied)?;
            let checkpoint: Option<CheckpointRecord> = assignment
                .checkpoint
                .map(|value| {
                    serde_json::from_value(value).map_err(|_| {
                        StorageError::InvalidRecord("invalid parent checkpoint".into())
                    })
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
                    work: root.id.clone(),
                    parent: Some(parent),
                    session: session.id,
                    scope_key: None,
                },
            )?
            .map_err(fenced)?;
        tx.runs().insert_agent_assignment_context(&StoredAgentAssignmentContext {
            assignment_id:assignment.id.clone(),project_id:root.project_id,
            role_id:input.role_id.clone(),role_instructions:instructions.text,
            role_revisions:serde_json::json!({"workspace":instructions.revisions.workspace,"project":instructions.revisions.project}),
            brief:input.instruction.0.clone(),
            task_ids:serde_json::to_value(&input.task_ids.0).map_err(|_|StorageError::InvalidRecord("task references".into()))?,
            source_checkpoint_id:input.source_checkpoint_id.clone(),created_at_ms:now,
        })?;
        retained::handover_item(tx, &assignment.id)?;
        agent_record(tx.runs(), &assignment).map_err(failure)
    }

    fn inbox(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::InboxInput,
    ) -> Result<(Page<AgentMessage>, i64), StorageError> {
        let assignment = snapshot
            .runs()
            .assignment(&input.assignment_id)?
            .ok_or_else(|| StorageError::NotFound {
                kind: "assignment",
                id: input.assignment_id.0.as_str().into(),
            })?;
        let root = snapshot
            .sessions()
            .work_root(&assignment.root_id)?
            .ok_or_else(|| StorageError::NotFound {
                kind: "root",
                id: assignment.root_id.0.as_str().into(),
            })?;
        if ctx.project_id().is_some_and(|p| p != &root.project_id)
            || ctx.root_id().is_some_and(|r| r != &assignment.root_id)
            || ctx.principal_kind() == PrincipalKind::Agent
                && (ctx.assignment_id() != Some(&assignment.id)
                    || ctx.session_id() != Some(&assignment.session_id))
        {
            return Err(denied());
        }
        if ctx.principal_kind() == PrincipalKind::Agent {
            let session = snapshot
                .sessions()
                .session(&assignment.session_id)?
                .ok_or_else(denied)?;
            if session.agent_id != *ctx.principal() || !assignment.state.is_live() {
                return Err(denied());
            }
        }
        let project = snapshot
            .projects()
            .project(&root.project_id)?
            .ok_or_else(denied)?;
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
        let before = input
            .page
            .cursor
            .as_ref()
            .map(|c| {
                binding.decode(&c.0).and_then(|s| {
                    crate::protocol::Id::new(s).map_err(|_| StorageError::CursorExpired)
                })
            })
            .transpose()?;
        let limit = input.page.limit.get() as u16;
        let rows = snapshot.runs().agent_assignment_thread(
            &root.project_id,
            &assignment.id,
            direction,
            input.unacknowledged_only,
            before.as_ref(),
            limit,
        )?;
        let next = if rows.len() == usize::from(limit) {
            let last = rows.last().map(|(m, _)| &m.id);
            if snapshot
                .runs()
                .agent_assignment_thread(
                    &root.project_id,
                    &assignment.id,
                    direction,
                    input.unacknowledged_only,
                    last,
                    1,
                )?
                .is_empty()
            {
                None
            } else {
                last.map(|id| BoundedText(binding.encode(id.as_str())))
            }
        } else {
            None
        };
        let items = rows
            .into_iter()
            .map(|(m, d)| {
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
            })
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = next.is_some();
        Ok((
            Page {
                items: BoundedList(items),
                next_cursor: next,
                has_more,
            },
            project.scope_revision,
        ))
    }
}

/// The revision-one assignment send. Gateway receipt replay surrounds this transaction;
/// storage records and event sequence commit with that receipt.
pub fn post_assignment(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    input: &ops::MessagesInput,
) -> Result<AgentMessage, StorageError> {
    let (recipient, project) = assignment_scope(tx, &input.recipient_assignment_id)?;
    if !recipient.state.is_live() {
        return Err(denied());
    }
    let sender = source_assignment(tx, ctx, &project, &recipient.root_id)?;
    if input.content.0.trim().is_empty() {
        return Err(StorageError::InvalidRecord("empty message".into()));
    }
    for attachment in &input.attachments.0 {
        if !crate::services::sessions::transcript::reference_resolves(
            tx,
            ctx,
            Some(&project),
            attachment,
        )? {
            return Err(denied());
        }
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
    wire(&message, &delivery)
}

/// Explicitly marks one assignment delivery read. Inbox observation is pure; only this command
/// advances delivery state, so a lost read reply can replay the same pending message.
pub fn acknowledge_assignment(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    input: &ops::AcknowledgeInput,
) -> Result<AgentMessage, StorageError> {
    let (assignment, project) = assignment_scope(tx, &input.assignment_id)?;
    if !assignment.state.is_live()
        || ctx.principal_kind() != PrincipalKind::Agent
        || ctx.assignment_id() != Some(&assignment.id)
        || ctx.session_id() != Some(&assignment.session_id)
        || ctx.project_id().is_some_and(|p| p != &project)
    {
        return Err(denied());
    }
    let session = tx
        .sessions()
        .session(&assignment.session_id)?
        .ok_or_else(denied)?;
    if session.closed() || session.agent_id != *ctx.principal() {
        return Err(denied());
    }
    let message =
        tx.runs()
            .agent_message(&input.message_id)?
            .ok_or_else(|| StorageError::NotFound {
                kind: "agent_message",
                id: input.message_id.as_str().into(),
            })?;
    if message.project_id != project || message.to_assignment_id.as_ref() != Some(&assignment.id) {
        return Err(denied());
    }
    let mut delivery = tx
        .runs()
        .agent_delivery(&message.id, Some(&assignment.id))?
        .ok_or_else(denied)?;
    if delivery.revision != input.expected_revision.get() {
        return Err(StorageError::conflict(
            RevisionDimension::Store,
            "agent_delivery",
            message.id.clone(),
            input.expected_revision.clone(),
            rev(delivery.revision),
        ));
    }
    if matches!(delivery.state.as_str(), "Read" | "Answered" | "Declined") {
        return wire(&message, &delivery);
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
    wire(&message, &delivery)
}

/// A checkpoint is bound to the assignment's own transcript position and advances its revision.
/// It cannot attest to a future entry or replace a newer checkpoint.
pub fn checkpoint_assignment(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    input: &ops::CheckpointInput,
) -> Result<CheckpointRecord, StorageError> {
    let (mut assignment, project) = assignment_scope(tx, &input.assignment_id)?;
    if !assignment.state.is_live()
        || ctx.assignment_id() != Some(&assignment.id)
        || ctx.session_id() != Some(&assignment.session_id)
        || ctx.project_id().is_some_and(|p| p != &project)
    {
        return Err(denied());
    }
    let session = tx
        .sessions()
        .session(&assignment.session_id)?
        .ok_or_else(denied)?;
    if session.closed() || session.agent_id != *ctx.principal() {
        return Err(denied());
    }
    if assignment.revision != input.expected_revision.get() {
        return Err(StorageError::conflict(
            RevisionDimension::Work,
            "assignment",
            assignment.id.0.clone(),
            input.expected_revision.clone(),
            rev(assignment.revision),
        ));
    }
    let sequence = input.transcript_sequence.get();
    if sequence > session.last_durable_sequence() {
        return Err(StorageError::InvalidRecord(
            "checkpoint is ahead of the transcript".into(),
        ));
    }
    if let Some(previous) = &assignment.checkpoint {
        let previous: CheckpointRecord = serde_json::from_value(previous.clone())
            .map_err(|_| StorageError::InvalidRecord("invalid stored checkpoint".into()))?;
        if sequence < previous.transcript_sequence.get() {
            return Err(StorageError::InvalidRecord(
                "checkpoint sequence regressed".into(),
            ));
        }
    }
    if !crate::services::sessions::transcript::reference_resolves(
        tx,
        ctx,
        Some(&project),
        &input.content,
    )? {
        return Err(denied());
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
        .ok_or_else(denied)?;
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
        .ok_or_else(denied)?;
    let context = tx
        .runs()
        .agent_assignment_context(assignment_id)?
        .ok_or_else(denied)?;
    if context.project_id != root.project_id {
        return Err(denied().into());
    }
    let role = AgentRole::parse(context.role_id.as_str())
        .ok_or_else(|| StorageError::InvalidRecord("unknown stored assignment role".into()))?;
    let session = tx
        .sessions()
        .session(&assignment.session_id)?
        .ok_or_else(denied)?;
    let actor = session.agent_id;
    if tx.actors().actor_kind(&actor)? != Some(PrincipalKind::Agent) {
        return Err(denied().into());
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
