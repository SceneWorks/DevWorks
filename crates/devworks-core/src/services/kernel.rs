//! Helpers every service shares: the typed failure and refusal, the decision of a use-case
//! outcome, bounded-value constructors, and the project and session visibility rules. They
//! began in the sessions service and live here so no service depends on another to use them.
use crate::context::{InvocationContext, PrincipalKind};
pub use crate::kernel::failure::{Failure, Refusal};
pub(crate) use crate::kernel::failure::{not_found, rev};
pub(crate) use crate::kernel::project_visibility::project_visible;
use crate::ports::IdKind;
use crate::ports::storage::{SessionRead, StoredSession, WorkRoot, WriteTransaction};
use crate::ports::{Decided, HandlerFailure};
use crate::protocol::results::DomainDecision;
use crate::protocol::support::BoundedText;
use crate::protocol::{
    CorrelationId, Counter, DomainErrorCode, DomainFault, Id, ProjectId, RootId,
};

/// The query fault of a failure; a storage error maps through the shared storage fault.
pub(crate) fn query_fault(
    f: Failure,
    kind: &'static str,
    correlation: CorrelationId,
) -> DomainFault {
    f.query_fault(kind, correlation)
}

/// The command decision of a use-case outcome.
pub(crate) fn decided(
    tx: &mut dyn WriteTransaction,
    outcome: Result<DomainDecision, Failure>,
) -> Result<Decided, HandlerFailure> {
    match outcome {
        Ok(decision) => Ok(Decided {
            decision,
            operation_id: None,
        }),
        Err(Failure::Refused(r)) => Ok(Decided {
            decision: DomainDecision::Refused {
                fault: r.fault(CorrelationId(tx.new_id(IdKind::Correlation))),
            },
            operation_id: None,
        }),
        Err(Failure::Storage(e)) => Err(HandlerFailure::Storage(e)),
    }
}

pub(crate) fn counter(n: i64) -> Counter {
    Counter::new(n.max(0)).expect("non-negative counter")
}
pub(crate) fn label(s: &str, fallback: &str) -> BoundedText<1, 256> {
    let t: String = s.chars().take(256).collect();
    BoundedText(if t.is_empty() { fallback.into() } else { t })
}
pub(crate) fn content(s: &str) -> BoundedText<0, 65536> {
    BoundedText(s.chars().take(65536).collect())
}
/// A request naming `project` must match the invocation's project, when it has one.
pub(crate) fn require_project(ctx: &InvocationContext, project: &ProjectId) -> Result<(), Refusal> {
    match ctx.project_id() {
        Some(p) if p != project => Err(Refusal::new(
            DomainErrorCode::PermissionDenied,
            "project",
            Some(project.0.clone()),
        )),
        _ => Ok(()),
    }
}
/// The work root `id` if this invocation may see it (otherwise `NotFound`).
pub(crate) fn visible_root(
    snapshot: &dyn crate::ports::storage::SessionRead,
    ctx: &InvocationContext,
    id: &RootId,
) -> Result<WorkRoot, Failure> {
    match snapshot.work_root(id)? {
        Some(w) if project_visible(ctx, Some(&w.project_id)) => Ok(w),
        _ => Err(not_found("root", &id.0).into()),
    }
}
pub(crate) fn visible_session(
    read: &dyn crate::ports::storage::SessionRead,
    ctx: &InvocationContext,
    id: &Id,
) -> Result<StoredSession, Failure> {
    let owner = bound_agent_session(read, ctx)?;
    let session = project_session(read, ctx, id)?;
    if owner.as_ref().is_none_or(|own| {
        session.id == own.id
            || (own.root_id.is_some()
                && session.root_id == own.root_id
                && session.project_id == own.project_id)
    }) {
        Ok(session)
    } else {
        Err(not_found("session", id).into())
    }
}

pub(crate) fn project_session(
    read: &dyn SessionRead,
    ctx: &InvocationContext,
    id: &Id,
) -> Result<StoredSession, Failure> {
    match read.session(id)? {
        Some(session) if project_visible(ctx, session.project_id.as_ref()) => Ok(session),
        _ => Err(not_found("session", id).into()),
    }
}

pub(crate) fn bound_agent_session(
    read: &dyn SessionRead,
    ctx: &InvocationContext,
) -> Result<Option<StoredSession>, Failure> {
    if ctx.principal_kind() != PrincipalKind::Agent {
        return Ok(None);
    }
    let Some(id) = ctx.session_id() else {
        return Ok(None);
    };
    let own = read.session(id)?.ok_or_else(|| not_found("session", id))?;
    if own.agent_id != *ctx.principal()
        || !project_visible(ctx, own.project_id.as_ref())
        || ctx
            .root_id()
            .is_some_and(|root| own.root_id.as_ref() != Some(root))
    {
        return Err(not_found("session", id).into());
    }
    Ok(Some(own))
}
