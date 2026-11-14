//! A principal's authorized reach: which projects' records it may act on.
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::storage::{AuthorityRead, StorageError};
use crate::protocol::ProjectId;

/// The principal's authorized reach: the user, the host and unconfined principals act
/// workspace-wide; a confined principal only in its bound project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reach {
    Workspace,
    Project(ProjectId),
}

/// The principal's reach from its current host binding. `None`: a non-human principal without
/// an active host binding has no reach, and every caller refuses it (fails closed).
pub fn reach(
    authority: &dyn AuthorityRead,
    ctx: &InvocationContext,
) -> Result<Option<Reach>, StorageError> {
    if matches!(
        ctx.principal_kind(),
        PrincipalKind::Human | PrincipalKind::Host
    ) {
        return Ok(Some(Reach::Workspace));
    }
    Ok(match authority.principal(ctx.principal())? {
        Some(b) if b.active => Some(b.project_id.map_or(Reach::Workspace, Reach::Project)),
        _ => None,
    })
}
