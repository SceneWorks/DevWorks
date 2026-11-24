//! Exact view identity, retained read authority and stored graph projection.
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::kernel::{
    checkout,
    failure::{Failure, Refusal},
    project_visibility::{visible_project, visible_repository},
};
use crate::ports::storage::*;
use crate::protocol::{
    DomainErrorCode, Id,
    data::{DataScope, GitOid, ObjectFormat, RepositorySnapshot},
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// The layers of one view, highest first.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewLayers {
    pub view: StoredView,
    /// Generation IDs, highest layer first (worktree, branch, baseline).
    pub layers: Vec<Id>,
}

pub(crate) fn unavailable_index(id: &Id) -> Failure {
    Refusal::new(
        DomainErrorCode::ViewUnavailable,
        "index_view",
        Some(id.clone()),
    )
    .into()
}

/// The layers of `view` (it must exist and every layer must still be sealed).
pub fn layers(read: &dyn IndexRead, view: &Id) -> Result<ViewLayers, Failure> {
    let Some(v) = read.view(view)? else {
        return Err(unavailable_index(view));
    };
    let mut layers = Vec::new();
    for g in [
        v.worktree_id.as_ref(),
        v.branch_id.as_ref(),
        Some(&v.baseline_id),
    ]
    .into_iter()
    .flatten()
    {
        match read.generation(g)? {
            Some(stored) if stored.state == GEN_SEALED => layers.push(stored.id),
            _ => return Err(unavailable_index(view)),
        }
    }
    Ok(ViewLayers { view: v, layers })
}

pub(crate) fn unavailable(id: Option<Id>) -> Failure {
    Refusal::new(DomainErrorCode::ViewUnavailable, "graph_view", id).into()
}

pub(crate) fn retained(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    scope: &DataScope,
    pin: &StoredGraphReadPin,
    binding: &GraphViewBinding,
    now: i64,
) -> Result<ViewLayers, Failure> {
    let selected = layers(read.indexing(), &binding.view_id)?;
    authorize(read, ctx, scope, &selected.view.repository_id)?;
    authorize_worktree_view(read, ctx, &selected.view)?;
    let proof = pin
        .view_witnesses
        .iter()
        .find(|w| w.view_id == binding.view_id)
        .ok_or_else(|| unavailable(Some(pin.id.clone())))?;
    if let Some(id) = &proof.overlay_id
        && read
            .indexing()
            .overlay(id)?
            .is_none_or(|row| row.state == OVERLAY_UNAVAILABLE)
    {
        return Err(unavailable(Some(pin.id.clone())));
    }
    if selected.view.repository_id != binding.repository_id
        || selected.view.view_digest != binding.view_digest
        || proof.expires_at_ms.is_some_and(|expiry| now >= expiry)
    {
        return Err(unavailable(Some(pin.id.clone())));
    }
    Ok(selected)
}

/// Current durable custody is checked even for metadata-only status and retained pins.
pub(crate) fn owned(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
) -> Result<Option<StoredGitWorktree>, Failure> {
    let Some(bound) = ctx.worktree() else {
        return Ok(None);
    };
    let denied = || {
        Refusal::new(
            DomainErrorCode::PermissionDenied,
            "graph_owned_context",
            Some(bound.id().clone()),
        )
    };
    let tree = read.git().git_worktree(bound.id())?.ok_or_else(denied)?;
    let work = read.work().work(bound.work_id())?.ok_or_else(denied)?;
    if tree.removed
        || tree.id != *bound.id()
        || tree.repository_id != *bound.repository_id()
        || tree.work_id != *bound.work_id()
        || tree.ownership_revision != bound.ownership_revision()
        || ctx.project_id() != Some(&work.project_id)
        || !ctx.root_id().is_some_and(|root| {
            root.0 == work.id || work.parent_feature_id.as_ref() == Some(&root.0)
        })
        || !read
            .work()
            .work_repositories(&work.id)?
            .contains(bound.repository_id())
        || !read
            .projects()
            .members(&work.project_id)?
            .contains(bound.repository_id())
    {
        return Err(denied().into());
    }
    Ok(Some(tree))
}

pub(crate) fn authorize(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    scope: &DataScope,
    repository: &Id,
) -> Result<(), Failure> {
    visible_repository(read.projects(), ctx, repository)?;
    if let DataScope::Project { project_id } = scope {
        visible_project(read.projects(), ctx, project_id)?;
        if !read.projects().members(project_id)?.contains(repository) {
            return Err(Refusal::new(
                DomainErrorCode::NotFound,
                "repository",
                Some(repository.clone()),
            )
            .into());
        }
    }
    Ok(())
}

/// A caller-supplied overlay view or a retained pin must not select a sibling's local files.
pub(crate) fn authorize_worktree_view(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    view: &StoredView,
) -> Result<Option<Id>, Failure> {
    if !checkout::applies(ctx) {
        return Ok(None);
    }
    let worktree = view
        .coverage
        .get("worktreeId")
        .and_then(serde_json::Value::as_str);
    if worktree.is_none() && view.worktree_id.is_none() {
        return Ok(None);
    }
    if let Some(id) = checkout::owned(read.git(), ctx)?
        .filter(|tree| tree.repository_id == view.repository_id)
        .map(|tree| checkout::indexed_id(&tree))
        .filter(|id| Some(id.as_str()) == worktree)
    {
        return Ok(Some(id));
    }
    Err(Refusal::new(
        DomainErrorCode::PermissionDenied,
        "graph_owned_context",
        None,
    )
    .into())
}

pub(crate) fn owner(ctx: &InvocationContext) -> String {
    let worktree = ctx.worktree().map(|tree| {
        json!({
            "id": tree.id(),
            "workId": tree.work_id(),
            "repositoryId": tree.repository_id(),
            "ownershipRevision": tree.ownership_revision()
        })
    });
    sha256_hex(
        json!({
            "actor": ctx.principal(),
            "kind": ctx.principal_kind().as_str().to_string(),
            "project": ctx.project_id(),
            "root": ctx.root_id(),
            "assignment": ctx.assignment_id(),
            "session": ctx.session_id(),
            "ingress": ctx.ingress_namespace(),
            "worktree": worktree
        })
        .to_string()
        .as_bytes(),
    )
}

pub(crate) fn scope_digest(scope: &DataScope) -> String {
    sha256_hex(
        serde_json::to_string(scope)
            .expect("typed scope")
            .as_bytes(),
    )
}

pub(crate) fn check_pin(
    pin: &StoredGraphReadPin,
    ctx: &InvocationContext,
    requested_scope: &DataScope,
    now: i64,
) -> Result<(), Failure> {
    if pin.owner_namespace != owner(ctx)
        || pin.scope_digest != scope_digest(requested_scope)
        || pin.store_id != *ctx.store_id()
        || pin.restore_epoch != ctx.restore_epoch().get()
        || pin.expires_at_ms <= now
    {
        return Err(unavailable(Some(pin.id.clone())));
    }
    Ok(())
}

pub(crate) fn unrepresentable() -> Failure {
    Refusal::new(
        DomainErrorCode::ViewUnavailable,
        "graph_value_unrepresentable",
        None,
    )
    .into()
}

/// Validates a stored JSON document as the wire type `T`. Use it only where the input really is
/// a stored document; typed values go through [`text`], [`list`] and [`count`].
pub(crate) fn wire<T: DeserializeOwned>(value: Value) -> Result<T, Failure> {
    serde_json::from_value(value).map_err(|_| unrepresentable())
}

/// A wire scalar (bounded text, identifier, digest, kind name) built from typed text under the
/// same validation as the wire type's reader, with no JSON document in between.
pub(crate) fn text<T: DeserializeOwned>(value: &str) -> Result<T, Failure> {
    T::deserialize(serde::de::value::StrDeserializer::<serde::de::value::Error>::new(value))
        .map_err(|_| unrepresentable())
}

fn oid(text: &str) -> Result<GitOid, Failure> {
    let oid = GitOid {
        format: if text.len() == 64 {
            ObjectFormat::Sha256
        } else {
            ObjectFormat::Sha1
        },
        hex: text.into(),
    };
    oid.validate()
        .map_err(|_| Refusal::new(DomainErrorCode::ViewUnavailable, "graph_snapshot", None))?;
    Ok(oid)
}

pub(crate) fn snapshot(view: &StoredView) -> Result<RepositorySnapshot, Failure> {
    let base = view
        .coverage
        .get("base")
        .or_else(|| view.coverage.get("commit"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Refusal::new(
                DomainErrorCode::ViewUnavailable,
                "graph_snapshot",
                Some(view.view_id.clone()),
            )
        })?;
    let head = view
        .coverage
        .get("head")
        .and_then(Value::as_str)
        .unwrap_or(base);
    Ok(RepositorySnapshot {
        repository_id: view.repository_id.clone(),
        base: oid(base)?,
        head: oid(head)?,
        worktree_id: view
            .coverage
            .get("worktreeId")
            .filter(|v| !v.is_null())
            .map(|v| wire(v.clone()))
            .transpose()?,
        capture_digest: view
            .coverage
            .get("captureDigest")
            .filter(|v| !v.is_null())
            .map(|v| wire(v.clone()))
            .transpose()?,
    })
}
