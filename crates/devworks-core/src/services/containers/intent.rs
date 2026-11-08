//! Frozen, attributed command intent reconstructed only after the receipt commits.
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::AdmittedCommand;
use crate::operations::{OperationInput, containers as ops};
use crate::ports::storage::ApplicationStore;
use crate::protocol::{ActorId, AssignmentId, Id, OperationId, ProjectId, RootId};
use crate::services::git::context::SourceBinding;
use crate::services::sessions::{counter, rev};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ContainerCommand {
    Build(ops::BuildInput),
    Pull(ops::PullInput),
    Create(ops::CreateInput),
    Start(ops::StartInput),
    Exec(ops::ExecInput),
    Stop(ops::StopInput),
}

impl ContainerCommand {
    pub fn from_input(input: &OperationInput) -> Option<Self> {
        match input {
            OperationInput::Containers(ops::Input::Build(input)) => {
                Some(Self::Build(input.clone()))
            }
            OperationInput::Containers(ops::Input::Pull(input)) => Some(Self::Pull(input.clone())),
            OperationInput::Containers(ops::Input::Create(input)) => {
                Some(Self::Create(input.clone()))
            }
            OperationInput::Containers(ops::Input::Start(input)) => {
                Some(Self::Start(input.clone()))
            }
            OperationInput::Containers(ops::Input::Exec(input)) => Some(Self::Exec(input.clone())),
            OperationInput::Containers(ops::Input::Stop(input)) => Some(Self::Stop(input.clone())),
            _ => None,
        }
    }

    pub fn input(&self) -> OperationInput {
        OperationInput::Containers(match self {
            Self::Build(input) => ops::Input::Build(input.clone()),
            Self::Pull(input) => ops::Input::Pull(input.clone()),
            Self::Create(input) => ops::Input::Create(input.clone()),
            Self::Start(input) => ops::Input::Start(input.clone()),
            Self::Exec(input) => ops::Input::Exec(input.clone()),
            Self::Stop(input) => ops::Input::Stop(input.clone()),
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContainerIntent {
    actor: ActorId,
    kind: String,
    session: Option<Id>,
    project: Option<ProjectId>,
    root: Option<RootId>,
    assignment: Option<AssignmentId>,
    restore_epoch: i64,
    policy_revision: i64,
    authority_sources: Vec<Id>,
    fingerprint: String,
    source_binding: Option<SourceBinding>,
    pub command: ContainerCommand,
}

impl ContainerIntent {
    pub fn new(
        ctx: &InvocationContext,
        admitted: &AdmittedCommand,
        command: ContainerCommand,
        source_binding: Option<SourceBinding>,
    ) -> Self {
        Self {
            actor: ctx.principal().clone(),
            kind: format!("{:?}", ctx.principal_kind()),
            session: ctx.session_id().cloned(),
            project: ctx.project_id().cloned(),
            root: ctx.root_id().cloned(),
            assignment: ctx.assignment_id().cloned(),
            restore_epoch: ctx.restore_epoch().get(),
            policy_revision: ctx.policy_revision().get(),
            authority_sources: ctx.authority_sources().to_vec(),
            fingerprint: admitted.fingerprint().as_str().into(),
            source_binding,
            command,
        }
    }

    pub fn context(
        &self,
        store: &dyn ApplicationStore,
        operation: &OperationId,
    ) -> Option<InvocationContext> {
        let kind = match self.kind.as_str() {
            "Human" => PrincipalKind::Human,
            "Agent" => PrincipalKind::Agent,
            "Package" => PrincipalKind::Package,
            "McpClient" => PrincipalKind::McpClient,
            "Schedule" => PrincipalKind::Schedule,
            "Host" => PrincipalKind::Host,
            _ => return None,
        };
        let info = store.info();
        if info.restore_epoch != self.restore_epoch {
            return None;
        }
        let mut ctx = InvocationContext::new(
            self.actor.clone(),
            kind,
            operation.0.clone(),
            Id::new("container_effects").expect("static consumer"),
            info.store_id,
            counter(self.restore_epoch),
            rev(self.policy_revision),
        )
        .with_lineage(
            self.project.clone(),
            self.root.clone(),
            self.assignment.clone(),
        );
        if let Some(session) = &self.session {
            ctx = ctx.with_session(session.clone());
        }
        for source in &self.authority_sources {
            ctx = ctx.with_authority_source(source.clone());
        }
        if let Some(binding) = &self.source_binding {
            let bound = binding.context(store).ok()?.worktree()?.clone();
            ctx = ctx.bind_worktree(bound);
        }
        Some(ctx)
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}
