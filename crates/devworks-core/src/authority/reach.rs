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
        Some(binding) if ctx.principal_kind() == PrincipalKind::McpClient => {
            // A multi-project client acts in one granted project per request. Gateway admission
            // requires its live proof; explicit deferred owners recheck the serialized fence
            // before using a restored context.
            if binding.active && binding.kind == PrincipalKind::McpClient {
                ctx.project_id().cloned().map(Reach::Project)
            } else {
                None
            }
        }
        Some(b) if b.active => Some(b.project_id.map_or(Reach::Workspace, Reach::Project)),
        _ => None,
    })
}
