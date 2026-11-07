//! Resolve repository authority before selecting or expanding any graph rows.
use crate::context::InvocationContext;
use crate::ports::storage::*;
use crate::protocol::data::{DataScope, ViewSelection};
use crate::protocol::{DomainErrorCode, Id};
use crate::services::indexing::{self, status, views};
use crate::services::projects::{visible_project, visible_repository};
use crate::services::sessions::{Failure, Refusal};
use std::collections::BTreeSet;

pub(super) fn unavailable(id: Option<Id>) -> Failure {
    Refusal::new(DomainErrorCode::ViewUnavailable, "graph_view", id).into()
}

pub(super) fn overlay(view: &StoredView) -> bool {
    view.coverage.get("head").is_some()
        || view.coverage.get("overlayId").is_some()
        || view.branch_id.is_some()
        || view.worktree_id.is_some()
}

/// Numeric publication revision is separate from the snapshot's Git object identity.
pub(super) fn witness(
    read: &dyn ReadSnapshot,
    view: &StoredView,
) -> Result<GraphViewWitness, Failure> {
    if let Some(revision) = view
        .coverage
        .get("sourceRevision")
        .and_then(serde_json::Value::as_i64)
        .filter(|v| *v > 0)
    {
        let (overlay_id, expiry) = if overlay(view) {
            (
                Some(
                    serde_json::from_value(view.coverage["overlayId"].clone())
                        .map_err(|_| unavailable(Some(view.view_id.clone())))?,
                ),
                Some(
                    view.coverage["expiresAtMs"]
                        .as_i64()
                        .ok_or_else(|| unavailable(Some(view.view_id.clone())))?,
                ),
            )
        } else {
            (None, None)
        };
        return Ok(GraphViewWitness {
            view_id: view.view_id.clone(),
            source_revision: revision,
            overlay_id,
            expires_at_ms: expiry,
        });
    }
    if overlay(view) {
        let matched = if let Some(worktree) = view
            .coverage
            .get("worktreeId")
            .and_then(|v| serde_json::from_value::<Id>(v.clone()).ok())
        {
            read.indexing()
                .overlay_for(&view.repository_id, &worktree)?
        } else if let Some(head) = view
            .coverage
            .get("head")
            .and_then(serde_json::Value::as_str)
        {
            read.indexing().overlay_at(&view.repository_id, head)?
        } else {
            None
        };
        let matched = matched
            .filter(|o| o.view_id.as_ref() == Some(&view.view_id))
            .ok_or_else(|| unavailable(Some(view.view_id.clone())))?;
        return Ok(GraphViewWitness {
            view_id: view.view_id.clone(),
            source_revision: matched.revision,
            overlay_id: Some(matched.id),
            expires_at_ms: Some(matched.expires_at_ms),
        });
    }
    if let Some(pointer) = read
        .indexing()
        .published(&view.repository_id)?
        .filter(|p| p.view_id == view.view_id)
    {
        return Ok(GraphViewWitness {
            view_id: view.view_id.clone(),
            source_revision: pointer.revision,
            overlay_id: None,
            expires_at_ms: None,
        });
    }
    let generation = read
        .indexing()
        .generation(&view.baseline_id)?
        .ok_or_else(|| unavailable(Some(view.view_id.clone())))?;
    if let (Some(parent), Some(run_id)) = (
        generation.manifest["parentPointerRevision"].as_i64(),
        generation.manifest["runId"].as_str(),
    ) {
        let run_id =
            Id::new(run_id.to_owned()).map_err(|_| unavailable(Some(view.view_id.clone())))?;
        if parent >= 0
            && let Some(revision) = parent.checked_add(1)
            && read.indexing().run(&run_id)?.is_some_and(|r| {
                r.state == RUN_SUCCEEDED && r.output_view_id.as_ref() == Some(&view.view_id)
            })
        {
            return Ok(GraphViewWitness {
                view_id: view.view_id.clone(),
                source_revision: revision,
                overlay_id: None,
                expires_at_ms: None,
            });
        }
    }
    Err(unavailable(Some(view.view_id.clone())))
}

pub(super) fn retained(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    scope: &DataScope,
    pin: &StoredGraphReadPin,
    binding: &GraphViewBinding,
    now: i64,
) -> Result<views::ViewLayers, Failure> {
    let selected = views::layers(read.indexing(), &binding.view_id)?;
    authorize(read, ctx, scope, &selected.view.repository_id)?;
    let proof = pin
        .view_witnesses
        .iter()
        .find(|w| w.view_id == binding.view_id)
        .ok_or_else(|| unavailable(Some(pin.id.clone())))?;
    if selected.view.repository_id != binding.repository_id
        || selected.view.view_digest != binding.view_digest
        || proof.expires_at_ms.is_some_and(|expiry| now >= expiry)
    {
        return Err(unavailable(Some(pin.id.clone())));
    }
    Ok(selected)
}

/// Current durable custody is checked even for metadata-only status and retained pins.
pub(super) fn owned(
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

pub(super) fn authorize(
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

/// Rechecks the effective expiry of an explicit overlay without substituting today's view.
/// Stale-but-retained exact views remain addressable; current selection requires Ready.
pub(super) fn exact(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    scope: &DataScope,
    id: &Id,
    now_ms: i64,
) -> Result<views::ViewLayers, Failure> {
    let layers = views::layers(read.indexing(), id)?;
    authorize(read, ctx, scope, &layers.view.repository_id)?;
    if overlay(&layers.view) {
        if let Some(expiry) = layers
            .view
            .coverage
            .get("expiresAtMs")
            .and_then(serde_json::Value::as_i64)
        {
            return if now_ms < expiry {
                Ok(layers)
            } else {
                Err(unavailable(Some(id.clone())))
            };
        }
        let repo = &layers.view.repository_id;
        let published = read
            .indexing()
            .published(repo)?
            .map(|p| read.indexing().view(&p.view_id))
            .transpose()?
            .flatten()
            .map(|v| v.baseline_id);
        let config = indexing::effective_config(read.indexing(), read.projects(), repo)?.digest;
        // Exact immutable provenance avoids the bounded overview listing: a 257th live
        // overlay must not become unavailable merely because it is outside that page.
        let overlay = if let Some(worktree) = layers
            .view
            .coverage
            .get("worktreeId")
            .and_then(|v| serde_json::from_value::<Id>(v.clone()).ok())
        {
            read.indexing().overlay_for(repo, &worktree)?
        } else if let Some(head) = layers
            .view
            .coverage
            .get("head")
            .and_then(serde_json::Value::as_str)
        {
            read.indexing().overlay_at(repo, head)?
        } else {
            None
        };
        let live = match overlay.filter(|o| o.view_id.as_ref() == Some(id)) {
            Some(overlay) => {
                let moved = status::overlay_config_moved(read.indexing(), &overlay, &config)?;
                matches!(
                    status::effective_overlay_state(&overlay, published.as_ref(), now_ms, moved),
                    OVERLAY_READY | OVERLAY_STALE
                )
            }
            None => false,
        };
        if !live {
            return Err(unavailable(Some(id.clone())));
        }
    }
    Ok(layers)
}

pub(super) struct Resolved {
    pub layers: Vec<views::ViewLayers>,
    pub unavailable: Vec<(Id, &'static str)>,
}

pub(super) fn resolve(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    scope: &DataScope,
    selection: Option<&ViewSelection>,
    now_ms: i64,
    current_worktree: Option<&Id>,
) -> Result<Resolved, Failure> {
    if matches!(scope, DataScope::Workspace) && ctx.project_id().is_some() {
        return Err(
            Refusal::new(DomainErrorCode::PermissionDenied, "workspace_scope", None).into(),
        );
    }
    let mut out = Vec::new();
    let mut missing = Vec::new();
    match selection {
        Some(ViewSelection::Exact { view_ids }) => {
            for id in view_ids {
                out.push(exact(read, ctx, scope, id, now_ms)?);
            }
        }
        Some(ViewSelection::Published { repository_ids }) => {
            for repo in repository_ids {
                authorize(read, ctx, scope, repo)?;
                let published = read
                    .indexing()
                    .published(repo)?
                    .ok_or_else(|| unavailable(Some(repo.clone())))?;
                out.push(exact(read, ctx, scope, &published.view_id, now_ms)?);
            }
        }
        None => {
            // The ingress-bound worktree is positive host custody, never a tool-supplied path.
            if let Some(worktree) = ctx.worktree() {
                owned(read, ctx)?;
                authorize(read, ctx, scope, worktree.repository_id())?;
                out.push(views::worktree_view(
                    read,
                    ctx,
                    worktree.repository_id(),
                    current_worktree.ok_or_else(|| unavailable(Some(worktree.id().clone())))?,
                    now_ms,
                )?);
            } else {
                let project = match scope {
                    DataScope::Project { project_id } => {
                        visible_project(read.projects(), ctx, project_id)?;
                        Some(project_id.clone())
                    }
                    DataScope::Workspace => ctx.project_id().cloned(),
                };
                let filter = RepositoryFilter {
                    project,
                    text: None,
                    languages: Vec::new(),
                    include_deleted: false,
                };
                let mut after = None;
                let mut last_selected: Option<Id> = None;
                'repositories: loop {
                    let rows = read.projects().repositories(&filter, after.as_ref(), 256)?;
                    let more = rows.len() == 256;
                    for repo in &rows {
                        authorize(read, ctx, scope, &repo.id)?;
                        if out.len() + missing.len() >= 256 {
                            // Reserve one real member as the stable scope-limit witness rather
                            // than inventing a repository ID or overflowing the pin's bound.
                            let marker = last_selected.as_ref().expect("a full selected scope");
                            out.retain(|v| &v.view.repository_id != marker);
                            missing.retain(|(id, _)| id != marker);
                            missing.push((marker.clone(), "ScopeEnumerationIncomplete"));
                            break 'repositories;
                        }
                        match read.indexing().published(&repo.id)? {
                            None => missing.push((repo.id.clone(), "NotIndexedAtSelection")),
                            Some(published) => {
                                match exact(read, ctx, scope, &published.view_id, now_ms) {
                                    Ok(view) => out.push(view),
                                    Err(Failure::Refused(r))
                                        if r.code == DomainErrorCode::ViewUnavailable =>
                                    {
                                        missing
                                            .push((repo.id.clone(), "ViewUnavailableAtSelection"))
                                    }
                                    Err(error) => return Err(error),
                                }
                            }
                        }
                        last_selected = Some(repo.id.clone());
                    }
                    after = rows.last().map(|r| r.id.clone());
                    if !more {
                        break;
                    }
                }
            }
        }
        Some(ViewSelection::Pinned { .. }) => return Err(unavailable(None)),
        Some(ViewSelection::SchemaPublished { .. }) => {
            return Err(Refusal::new(DomainErrorCode::InvalidArgument, "graph_view", None).into());
        }
    }
    if out.is_empty() {
        return Err(unavailable(None));
    }
    let mut repositories = BTreeSet::new();
    if out
        .iter()
        .any(|v| !repositories.insert(v.view.repository_id.as_str().to_owned()))
    {
        return Err(Refusal::new(
            DomainErrorCode::InvalidArgument,
            "duplicate_repository_view",
            None,
        )
        .into());
    }
    out.sort_by(|a, b| {
        a.view
            .repository_id
            .as_str()
            .cmp(b.view.repository_id.as_str())
    });
    Ok(Resolved {
        layers: out,
        unavailable: missing,
    })
}

/// The lease writer rechecks current authority and immutable view identity in its own unit.
/// This writes no business data; a read selection cannot retain a different, raced view.
pub(super) fn validate_in(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    scope: &DataScope,
    selected: &[views::ViewLayers],
    current: bool,
    retained: Option<&[GraphViewWitness]>,
) -> Result<(), Failure> {
    if ctx.store_id() != &tx.store().store_id
        || ctx.restore_epoch().get() != tx.store().restore_epoch
    {
        return Err(unavailable(None));
    }
    if let DataScope::Project { project_id } = scope {
        visible_project(tx.projects(), ctx, project_id)?;
    }
    for selected in selected {
        let repository = &selected.view.repository_id;
        visible_repository(tx.projects(), ctx, repository)?;
        if let DataScope::Project { project_id } = scope
            && !tx.projects().members(project_id)?.contains(repository)
        {
            return Err(unavailable(Some(repository.clone())));
        }
        if tx.indexing().view(&selected.view.view_id)?.as_ref() != Some(&selected.view) {
            return Err(unavailable(Some(selected.view.view_id.clone())));
        }
        for generation in &selected.layers {
            if !tx
                .indexing()
                .generation(generation)?
                .is_some_and(|g| g.state == GEN_SEALED)
            {
                return Err(unavailable(Some(selected.view.view_id.clone())));
            }
        }
        if !overlay(&selected.view) {
            if current
                && !tx
                    .indexing()
                    .published(repository)?
                    .is_some_and(|p| p.view_id == selected.view.view_id)
            {
                return Err(unavailable(Some(selected.view.view_id.clone())));
            }
            continue;
        }
        if !current
            && let Some(proof) = retained
                .and_then(|proofs| proofs.iter().find(|p| p.view_id == selected.view.view_id))
        {
            if proof.overlay_id.is_none()
                || proof
                    .expires_at_ms
                    .is_none_or(|expiry| expiry <= tx.now_ms())
            {
                return Err(unavailable(Some(selected.view.view_id.clone())));
            }
            continue;
        }
        if !current
            && let Some(expiry) = selected
                .view
                .coverage
                .get("expiresAtMs")
                .and_then(serde_json::Value::as_i64)
        {
            if tx.now_ms() >= expiry {
                return Err(unavailable(Some(selected.view.view_id.clone())));
            }
            continue;
        }
        let overlay = if let Some(worktree) = selected
            .view
            .coverage
            .get("worktreeId")
            .and_then(|v| serde_json::from_value::<Id>(v.clone()).ok())
        {
            tx.indexing().overlay_for(repository, &worktree)?
        } else if let Some(head) = selected
            .view
            .coverage
            .get("head")
            .and_then(serde_json::Value::as_str)
        {
            tx.indexing().overlay_at(repository, head)?
        } else {
            None
        };
        let overlay = overlay
            .filter(|o| o.view_id.as_ref() == Some(&selected.view.view_id))
            .ok_or_else(|| unavailable(Some(selected.view.view_id.clone())))?;
        let published = tx
            .indexing()
            .published(repository)?
            .map(|p| tx.indexing().view(&p.view_id))
            .transpose()?
            .flatten()
            .map(|v| v.baseline_id);
        let config = indexing::effective_config_tx(tx, repository)?.digest;
        let moved = status::overlay_config_moved(tx.indexing(), &overlay, &config)?;
        let state =
            status::effective_overlay_state(&overlay, published.as_ref(), tx.now_ms(), moved);
        if !matches!(state, OVERLAY_READY | OVERLAY_STALE) || current && state != OVERLAY_READY {
            return Err(unavailable(Some(selected.view.view_id.clone())));
        }
    }
    if let Some(bound) = ctx.worktree() {
        let tree = tx.git().git_worktree(bound.id())?.ok_or_else(|| {
            Refusal::new(
                DomainErrorCode::PermissionDenied,
                "graph_owned_context",
                Some(bound.id().clone()),
            )
        })?;
        if tree.removed
            || tree.ownership_revision != bound.ownership_revision()
            || tree.work_id != *bound.work_id()
            || tree.repository_id != *bound.repository_id()
        {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "graph_owned_context",
                Some(bound.id().clone()),
            )
            .into());
        }
    }
    Ok(())
}
