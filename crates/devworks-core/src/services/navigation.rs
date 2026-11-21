//! The currently visible coordinator conversation and its host-observed screen.
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::AdmittedQuery;
use crate::locks::RwLockExt;
use crate::operations::OperationInput;
use crate::ports::storage::{ReadSnapshot, StorageError};
use crate::ports::{QueryAnswer, QueryHandler};
use crate::protocol::records::ScreenContext;
use crate::protocol::results::OperationResult;
use crate::protocol::{CorrelationId, DomainErrorCode, DomainFault, Id, Revision};
use crate::settings::catalog::AgentRole;
use std::sync::RwLock;

#[derive(Default)]
pub struct NavigationContextService {
    state: RwLock<NavigationState>,
}

#[derive(Default)]
struct NavigationState {
    revision: i64,
    selection_key: String,
    visible: Option<(Id, ScreenContext)>,
}

impl NavigationContextService {
    /// Only the native host publishes this observation. There is no command to set it.
    pub fn publish(&self, mut visible: Option<(Id, ScreenContext)>, selection_key: &str) {
        let mut state = self.state.write_locked();
        let unchanged = match (&state.visible, &visible) {
            (None, None) => true,
            (Some((a, old)), Some((b, new))) => {
                a == b
                    && state.selection_key == selection_key
                    && old.screen_id == new.screen_id
                    && old.project_id == new.project_id
                    && old.root_id == new.root_id
                    && old.selected_ids.0 == new.selected_ids.0
                    && old.help_topic_id == new.help_topic_id
            }
            _ => false,
        };
        if unchanged {
            return;
        }
        state.revision = state.revision.saturating_add(1);
        state.selection_key = selection_key.to_owned();
        if let Some((_, screen)) = visible.as_mut() {
            screen.revision = Revision::new(state.revision).expect("positive screen revision");
        }
        state.visible = visible;
    }

    fn current(
        &self,
        ctx: &InvocationContext,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<Option<ScreenContext>, StorageError> {
        if ctx.project_id().is_some() || ctx.root_id().is_some() || ctx.assignment_id().is_some() {
            return Err(StorageError::ScopeDenied);
        }
        if ctx.principal_kind() == PrincipalKind::Agent
            && !snapshot
                .authority()
                .principal(ctx.principal())?
                .is_some_and(|binding| {
                    binding.active
                        && binding.role == Some(AgentRole::WorkspaceCoordinator)
                        && binding.project_id.is_none()
                })
        {
            return Err(StorageError::ScopeDenied);
        }
        let Some((id, screen)) = self.state.read_locked().visible.clone() else {
            return Ok(None);
        };
        let Some(session) = snapshot.sessions().session(&id)? else {
            return Ok(None);
        };
        if session.closed() || session.root_id.is_some() || session.project_id.is_some() {
            return Ok(None);
        }
        let owns = match ctx.principal_kind() {
            PrincipalKind::Human => ctx.principal() == &session.created_by,
            PrincipalKind::Agent => {
                ctx.session_id() == Some(&id) && ctx.principal() == &session.agent_id
            }
            _ => false,
        };
        if !owns {
            return Err(StorageError::ScopeDenied);
        }
        Ok((self.state.read_locked().revision == screen.revision.get()).then_some(screen))
    }
}

impl QueryHandler for NavigationContextService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let correlation = CorrelationId(ctx.invocation_id().clone());
        let fault =
            |code| super::kernel::Refusal::new(code, "screen", None).fault(correlation.clone());
        if !matches!(query.input(), OperationInput::Ui(_)) {
            return Err(fault(DomainErrorCode::UnsupportedCapability));
        }
        let screen = self
            .current(ctx, snapshot)
            .map_err(|error| {
                super::kernel::query_fault(error.into(), "screen", correlation.clone())
            })?
            .ok_or_else(|| fault(DomainErrorCode::ViewUnavailable))?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), screen.revision.clone()),
            result: OperationResult::UiCurrentScreen { value: screen },
        })
    }
}
