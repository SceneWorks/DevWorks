//! The first provider input of a materialized agent turn comes from its durable assignment
//! context. The agent cannot replace its role text, project or work brief through tool arguments.
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::ports::storage::{
    ApplicationStore, AssignmentState, ReadOptions, StorageError, read_snapshot,
};
use devworks_core::protocol::records::AgentRecord;
use devworks_core::providers::context::{
    ContentBlock, GenerationParameters, Message, MessageRole, ProviderContext, TextBlock,
};

pub fn initial_context(
    store: &dyn ApplicationStore,
    ctx: &InvocationContext,
    agent: &AgentRecord,
) -> Result<ProviderContext, StorageError> {
    let (role, brief) = read_snapshot(store, &ReadOptions::default(), |snapshot| {
        let assignment = snapshot
            .runs()
            .assignment(&agent.assignment_id)?
            .ok_or(StorageError::ScopeDenied)?;
        let session = snapshot
            .sessions()
            .session(&assignment.session_id)?
            .ok_or(StorageError::ScopeDenied)?;
        let source = snapshot
            .runs()
            .agent_assignment_context(&assignment.id)?
            .ok_or(StorageError::ScopeDenied)?;
        if ctx.principal_kind() != PrincipalKind::Agent
            || snapshot.stamp().store_id != *ctx.store_id()
            || snapshot.stamp().restore_epoch != ctx.restore_epoch().get()
            || snapshot.actors().actor_kind(ctx.principal())? != Some(PrincipalKind::Agent)
            || ctx.session_id() != Some(&session.id)
            || ctx.assignment_id() != Some(&assignment.id)
            || ctx.root_id() != Some(&assignment.root_id)
            || ctx.project_id() != Some(&source.project_id)
            || session.agent_id != *ctx.principal()
            || session.closed()
            || session.context_epoch != 0
            || assignment.state != AssignmentState::Active
            || agent.root_id != assignment.root_id
            || agent.role_id != source.role_id
            || source.assignment_id != assignment.id
        {
            return Err(StorageError::ScopeDenied);
        }
        Ok((source.role_instructions, source.brief))
    })?;
    let context = ProviderContext {
        version: 1,
        instructions: if role.is_empty() {
            Vec::new()
        } else {
            vec![TextBlock {
                text: role,
                cache: None,
            }]
        },
        instruction_stamps: Vec::new(),
        messages: vec![Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: brief,
                cache: None,
            }],
        }],
        parameters: GenerationParameters::default(),
    };
    context
        .validate()
        .map_err(|e| StorageError::InvalidRecord(format!("agent provider context: {e:?}")))?;
    Ok(context)
}
