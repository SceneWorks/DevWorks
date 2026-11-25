//! Current project and repository reach shared by query and authority checks.
use crate::context::InvocationContext;
use crate::kernel::failure::{Failure, not_found};
use crate::ports::storage::{DELETED, ProjectRead, StoredProject, StoredRepository};
use crate::protocol::{Id, ProjectId};

/// The project `id` if this invocation may see it (any lifecycle; otherwise `NotFound`).
pub fn visible_project(
    read: &dyn ProjectRead,
    ctx: &InvocationContext,
    id: &ProjectId,
) -> Result<StoredProject, Failure> {
    match read.project(id)? {
        Some(p) if project_visible(ctx, Some(&p.id)) => Ok(p),
        _ => Err(not_found("project", &id.0).into()),
    }
}

/// The registered, not deleted repository `id` if this invocation may see it: a workspace
/// invocation sees every registration, a project invocation only its project's members.
pub fn visible_repository(
    read: &dyn ProjectRead,
    ctx: &InvocationContext,
    id: &Id,
) -> Result<StoredRepository, Failure> {
    let found = match read.repository(id)? {
        Some(r) if r.lifecycle != DELETED => match ctx.project_id() {
            None => Some(r),
            Some(p) => read.members(p)?.contains(&r.id).then_some(r),
        },
        _ => None,
    };
    found.ok_or_else(|| not_found("repository", id).into())
}

/// Whether the invocation may see a session of `project` (`None`: a workspace session).
pub(crate) fn project_visible(ctx: &InvocationContext, project: Option<&ProjectId>) -> bool {
    match ctx.project_id() {
        None => true,
        Some(p) => project == Some(p),
    }
}
