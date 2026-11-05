//! Facts behind automatic force-with-lease authority: only a known agent-owned task branch.
//! The witness is rechecked at the gateway commit/publication boundary.
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::operations::{OperationInput, git::Input};
use crate::ports::storage::*;
use crate::protocol::{
    Id,
    data::{GitCommand, PushMode},
};
fn digest(
    tree: &StoredGitWorktree,
    work: &StoredWork,
    repository: &StoredRepository,
    project: &StoredProject,
    work_member: bool,
    project_member: bool,
) -> Result<Option<String>, StorageError> {
    let brief = crate::services::work::model::WorkBrief::of(work)?;
    if brief
        .assigned_agent_id
        .as_ref()
        .is_some_and(|agent| agent != &tree.owner.0)
        || tree.removed
        || !matches!(work.kind.as_str(), TASK | "issue")
        || repository.lifecycle != ACTIVE
        || project.lifecycle != ACTIVE
        || !work_member
        || !project_member
    {
        return Ok(None);
    }
    Ok(Some(sha256_hex(
        serde_json::json!([
            tree,
            work.id,
            work.kind,
            work.revision,
            work.scope_revision,
            work.parent_feature_id,
            repository.id,
            repository.revision,
            project.id,
            project.scope_revision,
            project.lifecycle
        ])
        .to_string()
        .as_bytes(),
    )))
}
pub(super) fn witness(s: &dyn ReadSnapshot, id: &Id) -> Result<Option<String>, StorageError> {
    let Some(tree) = s.git().git_worktree(id)? else {
        return Ok(None);
    };
    let Some(work) = s.work().work(&tree.work_id)? else {
        return Ok(None);
    };
    let Some(repo) = s.projects().repository(&tree.repository_id)? else {
        return Ok(None);
    };
    let Some(project) = s.projects().project(&work.project_id)? else {
        return Ok(None);
    };
    digest(
        &tree,
        &work,
        &repo,
        &project,
        s.work().work_repositories(&work.id)?.contains(&repo.id),
        s.projects()
            .repository_projects(&repo.id)?
            .contains(&work.project_id),
    )
}
pub(super) fn witness_in(
    tx: &mut dyn WriteTransaction,
    id: &Id,
) -> Result<Option<String>, StorageError> {
    let Some(tree) = tx.git().git_worktree(id)? else {
        return Ok(None);
    };
    let Some(work) = tx.work().work(&tree.work_id)? else {
        return Ok(None);
    };
    let Some(repo) = tx.projects().repository(&tree.repository_id)? else {
        return Ok(None);
    };
    let Some(project) = tx.projects().project(&work.project_id)? else {
        return Ok(None);
    };
    let work_member = tx.work().work_repositories(&work.id)?.contains(&repo.id);
    let project_member = tx
        .projects()
        .repository_projects(&repo.id)?
        .contains(&work.project_id);
    digest(&tree, &work, &repo, &project, work_member, project_member)
}
pub(super) fn owned_force(
    s: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    input: &OperationInput,
) -> Result<Option<(Id, String)>, StorageError> {
    if ctx.principal_kind() != PrincipalKind::Agent {
        return Ok(None);
    }
    let OperationInput::Git(Input::Push(request)) = input else {
        return Ok(None);
    };
    let GitCommand::Push {
        worktree_id,
        target_ref,
        mode: PushMode::ForceWithLease,
        expected_remote: Some(_),
        ..
    } = &request.command
    else {
        return Ok(None);
    };
    let Some(tree) = s.git().git_worktree(worktree_id)? else {
        return Ok(None);
    };
    let Some(work) = s.work().work(&tree.work_id)? else {
        return Ok(None);
    };
    let brief = crate::services::work::model::WorkBrief::of(&work)?;
    if brief
        .assignment_id
        .as_ref()
        .is_some_and(|assignment| Some(assignment) != ctx.assignment_id())
        || &tree.owner != ctx.principal()
        || tree.branch != *target_ref
        || ctx.project_id() != Some(&work.project_id)
        || !ctx
            .root_id()
            .is_some_and(|r| r.0 == work.id || work.parent_feature_id.as_ref() == Some(&r.0))
    {
        return Ok(None);
    }
    Ok(witness(s, worktree_id)?.map(|d| (worktree_id.clone(), d)))
}
