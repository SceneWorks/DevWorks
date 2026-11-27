//! Recorded checkout identity and authorization shared by ingress and indexing.
use crate::context::{BoundWorktree, InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::kernel::work_brief::WorkBrief;
use crate::ports::storage::*;
use crate::protocol::Id;

/// Paths at least this long keep the Windows verbatim prefix (the file APIs need it beyond the
/// classic 260-character limit).
pub const VERBATIM_MIN: usize = 248;

/// The identity form of a native absolute path (a registration location or a worktree path),
/// so a path reaching the indexer from the file system (`canonicalize`: `\\?\C:\x`) and one from
/// Git's output (`C:/x`, `//?/C:/x`) compare equal. On Windows: `/` becomes `\`, a verbatim
/// prefix is dropped while the path is shorter than [`VERBATIM_MIN`] (`\\?\UNC\server\share`
/// becomes `\\server\share`), the drive letter is upper case and a trailing separator is
/// removed (except at a drive root). Elsewhere the path is unchanged. `windows` is the target
/// platform (`cfg!(windows)`), a parameter so every platform tests the Windows rules.
pub fn native_location(raw: &str, windows: bool) -> String {
    if !windows {
        return raw.to_owned();
    }
    let mut s = raw.replace('/', "\\");
    if let Some(rest) = s.strip_prefix("\\\\?\\UNC\\") {
        if rest.len() + 2 < VERBATIM_MIN {
            s = format!("\\\\{rest}");
        }
    } else if let Some(rest) = s.strip_prefix("\\\\?\\")
        && rest.as_bytes().get(1) == Some(&b':')
        && rest.len() < VERBATIM_MIN
    {
        s = rest.to_owned();
    }
    // The drive letter: `c:\x` and `C:\x` name one directory.
    let drive_at = if s.starts_with("\\\\?\\") { 4 } else { 0 };
    if s.as_bytes().get(drive_at + 1) == Some(&b':')
        && let Some(c) = s.get(drive_at..drive_at + 1)
    {
        let upper = c.to_ascii_uppercase();
        s.replace_range(drive_at..drive_at + 1, &upper);
    }
    while s.len() > 3 && s.ends_with('\\') && !s.ends_with(":\\") {
        s.pop();
    }
    s
}

/// The stable worktree ID of a checkout path.
pub fn worktree_id(repository: &Id, path: &str) -> Id {
    let d = sha256_hex(format!("{}\u{0}{path}", repository.as_str()).as_bytes());
    Id::new(format!("wt-{}", &d[..32])).expect("valid")
}

pub(crate) fn applies(ctx: &InvocationContext) -> bool {
    ctx.principal_kind() == PrincipalKind::Agent && ctx.assignment_id().is_some()
}

pub(crate) fn indexed_id(tree: &StoredGitWorktree) -> Id {
    worktree_id(
        &tree.repository_id,
        &native_location(&tree.path, cfg!(windows)),
    )
}

pub(crate) fn owned(
    git: &dyn GitRead,
    ctx: &InvocationContext,
) -> Result<Option<StoredGitWorktree>, StorageError> {
    let Some(bound) = ctx.worktree().filter(|_| applies(ctx)) else {
        return Ok(None);
    };
    Ok(git.git_worktree(bound.id())?.filter(|tree| {
        !tree.removed
            && tree.pending_operation_id.is_none()
            && &tree.owner == ctx.principal()
            && tree.owner_root_id.as_ref() == ctx.root_id()
            && tree.owner_assignment_id.as_ref() == ctx.assignment_id()
            && &tree.repository_id == bound.repository_id()
            && &tree.work_id == bound.work_id()
            && tree.ownership_revision == bound.ownership_revision()
    }))
}

fn live(
    assignment: Option<StoredAssignment>,
    session: Option<StoredSession>,
    ctx: &InvocationContext,
) -> bool {
    assignment
        .zip(session)
        .is_some_and(|(assignment, session)| live_assignment(&assignment, &session, ctx))
}

/// Check exact custody and live lineage against the snapshot that will publish a query.
pub(crate) fn live_custody(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
) -> Result<bool, StorageError> {
    let Some((assignment, session)) = ctx.assignment_id().zip(ctx.session_id()) else {
        return Ok(false);
    };
    let Some(tree) = owned(read.git(), ctx)? else {
        return Ok(false);
    };
    let work = read.work().work(&tree.work_id)?;
    Ok(live(
        read.runs().assignment(assignment)?,
        read.sessions().session(session)?,
        ctx,
    ) && work.is_some_and(|work| Some(&work.project_id) == ctx.project_id())
        && read
            .work()
            .work_repositories(&tree.work_id)?
            .contains(&tree.repository_id)
        && read
            .projects()
            .repository_projects(&tree.repository_id)?
            .iter()
            .any(|project| Some(project) == ctx.project_id()))
}

pub(crate) fn worktree_for_write(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
) -> Result<Option<StoredGitWorktree>, StorageError> {
    let Some((assignment, session)) = ctx.assignment_id().zip(ctx.session_id()) else {
        return Ok(None);
    };
    let Some(tree) = owned(tx.git(), ctx)? else {
        return Ok(None);
    };
    if !live(
        tx.runs().assignment(assignment)?,
        tx.sessions().session(session)?,
        ctx,
    ) || tx
        .work()
        .work(&tree.work_id)?
        .is_none_or(|work| Some(&work.project_id) != ctx.project_id())
        || !tx
            .work()
            .work_repositories(&tree.work_id)?
            .contains(&tree.repository_id)
        || !tx
            .projects()
            .repository_projects(&tree.repository_id)?
            .iter()
            .any(|project| Some(project) == ctx.project_id())
    {
        return Ok(None);
    }
    Ok(Some(tree))
}

pub(crate) fn refresh_target(
    git: &dyn GitRead,
    ctx: &InvocationContext,
    repository: &Id,
    requested: Option<&Id>,
    has_ref: bool,
) -> Result<Option<Id>, StorageError> {
    let Some(tree) = owned(git, ctx)? else {
        return Ok(None);
    };
    let indexed = indexed_id(&tree);
    Ok((!has_ref
        && &tree.repository_id == repository
        && requested.is_some_and(|id| id == &indexed || id == &tree.id))
    .then_some(indexed))
}

pub(crate) fn status_target(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    repository: Option<&Id>,
    overlay: Option<&Id>,
) -> Result<(Id, Option<StoredOverlay>), StorageError> {
    let Some(tree) = owned(read.git(), ctx)? else {
        return Err(StorageError::ScopeDenied);
    };
    if repository.is_some_and(|id| id != &tree.repository_id) {
        return Err(StorageError::ScopeDenied);
    }
    let current = read
        .indexing()
        .overlay_for(&tree.repository_id, &indexed_id(&tree))?;
    if overlay.is_some_and(|id| !current.as_ref().is_some_and(|own| &own.id == id)) {
        return Err(StorageError::ScopeDenied);
    }
    Ok((tree.repository_id, current))
}

pub(crate) fn live_assignment(
    assignment: &StoredAssignment,
    session: &StoredSession,
    ctx: &InvocationContext,
) -> bool {
    assignment.state.is_live()
        && Some(&assignment.root_id) == ctx.root_id()
        && Some(&assignment.session_id) == ctx.session_id()
        && !session.closed()
        && &session.agent_id == ctx.principal()
        && session.project_id.as_ref() == ctx.project_id()
        && session.root_id.as_ref() == ctx.root_id()
}

/// Operation targets narrow current custody. A target never grants custody by itself.
#[derive(Default)]
pub struct AssignmentCheckout<'a> {
    pub repository: Option<&'a Id>,
    pub worktree: Option<&'a Id>,
    pub any_for_visibility: bool,
}

/// Resolve a normal launcher context from current assignment custody, without trusting tool input.
pub fn assignment_worktree(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    target: AssignmentCheckout<'_>,
) -> Result<Option<InvocationContext>, StorageError> {
    let Some(assignment_id) = ctx
        .assignment_id()
        .filter(|_| ctx.principal_kind() == PrincipalKind::Agent && ctx.worktree().is_none())
    else {
        return Ok(None);
    };
    if read.stamp().store_id != *ctx.store_id()
        || read.stamp().restore_epoch != ctx.restore_epoch().get()
    {
        return Err(StorageError::ScopeDenied);
    }
    let Some(assignment) = read.runs().assignment(assignment_id)? else {
        return Err(StorageError::ScopeDenied);
    };
    let Some(session) = read.sessions().session(&assignment.session_id)? else {
        return Err(StorageError::ScopeDenied);
    };
    if !live_assignment(&assignment, &session, ctx) {
        return Err(StorageError::ScopeDenied);
    }
    let work = read
        .work()
        .work(&assignment.work_id.0)?
        .ok_or(StorageError::ScopeDenied)?;
    let brief = WorkBrief::of(&work)?;
    if ctx.project_id() != Some(&work.project_id)
        || (work.id != assignment.root_id.0
            && work.parent_feature_id.as_ref() != Some(&assignment.root_id.0))
        || brief
            .assigned_agent_id
            .as_ref()
            .is_some_and(|id| id != &ctx.principal().0)
        || brief
            .assignment_id
            .as_ref()
            .is_some_and(|id| id != assignment_id)
    {
        return Err(StorageError::ScopeDenied);
    }
    let members = read.projects().members(&work.project_id)?;
    let repositories = read.work().work_repositories(&work.id)?;
    let mut after = None;
    let mut unique = None;
    loop {
        let page = read.git().git_worktrees(&work.id, after.as_ref(), 64)?;
        for row in &page {
            if row.removed
                || &row.owner != ctx.principal()
                || row.owner_root_id.as_ref() != ctx.root_id()
                || row.owner_assignment_id.as_ref() != ctx.assignment_id()
                || !members.contains(&row.repository_id)
                || !repositories.contains(&row.repository_id)
                || target
                    .repository
                    .is_some_and(|repository| repository != &row.repository_id)
                || target
                    .worktree
                    .is_some_and(|id| id != &row.id && id != &indexed_id(row))
            {
                continue;
            }
            if unique.is_some() {
                return Ok(None);
            }
            unique = Some(BoundWorktree::resolved(
                row.id.clone(),
                row.repository_id.clone(),
                row.work_id.clone(),
                row.ownership_revision,
            ));
            if target.any_for_visibility {
                return Ok(unique.map(|worktree| ctx.clone().bind_worktree(worktree)));
            }
        }
        if page.len() < 64 {
            break;
        }
        after = page.last().map(|row| row.id.clone());
    }
    Ok(unique.map(|worktree| ctx.clone().bind_worktree(worktree)))
}
