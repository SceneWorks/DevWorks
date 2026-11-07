//! Current guidance visibility uses the existing Knowledge scope policy, not caller IDs.
use crate::context::InvocationContext;
use crate::ports::storage::{AuthorityRead, KnowledgeScope, ReadSnapshot, WriteTransaction};
use crate::protocol::Id;
use crate::protocol::data::DataScope;
use crate::services::knowledge::{self, Scoped};
use crate::services::sessions::{Failure, Refusal, not_found};

pub(crate) fn requested(
    authority: &dyn AuthorityRead,
    ctx: &InvocationContext,
    explicit: Option<&DataScope>,
) -> Result<DataScope, Failure> {
    if let Some(scope) = explicit {
        return Ok(scope.clone());
    }
    if let Some(project_id) = ctx.project_id() {
        return Ok(DataScope::Project {
            project_id: project_id.clone(),
        });
    }
    Ok(match knowledge::reach(authority, ctx)? {
        knowledge::Reach::Workspace => DataScope::Workspace,
        knowledge::Reach::Project(project_id) => DataScope::Project { project_id },
    })
}

pub(crate) fn scope(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    explicit: Option<&DataScope>,
    write: bool,
) -> Result<Scoped, Failure> {
    let requested = requested(read.authority(), ctx, explicit)?;
    knowledge::resolve_scope(read.artifacts(), read.authority(), ctx, &requested, write)
}

pub(crate) fn scope_in(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    explicit: Option<&DataScope>,
    write: bool,
) -> Result<Scoped, Failure> {
    let requested = requested(tx.authority(), ctx, explicit)?;
    let reach = knowledge::reach(tx.authority(), ctx)?;
    knowledge::resolve_scope_with(&reach, ctx, &requested, write, |project| {
        tx.artifacts().project_exists(project)
    })
}

pub(crate) fn writable(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    actual: &KnowledgeScope,
) -> Result<(), Failure> {
    if knowledge::writable(&knowledge::reach(tx.authority(), ctx)?, actual) {
        Ok(())
    } else {
        Err(Refusal::new(
            crate::protocol::DomainErrorCode::PermissionDenied,
            "convention_scope",
            None,
        )
        .into())
    }
}

pub(crate) fn owner(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    id: &Id,
    revision: Option<i64>,
    explicit: Option<&DataScope>,
) -> Result<crate::ports::storage::StoredConvention, Failure> {
    let scoped = scope(read, ctx, explicit, false)?;
    read.conventions()
        .convention(id, revision, &scoped.readable)?
        .ok_or_else(|| not_found("convention", id).into())
}
