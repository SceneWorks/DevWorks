//! The read seam of published views for graph search, source lookup and traversal (item 28):
//! view selection and layer masking (AD 23). Every function reads one snapshot and never
//! substitutes another view: a missing, expired or retired view is `ViewUnavailable`.
//!
//! A unit is resolved by the highest layer of the view that mentions its key — the captured
//! worktree, then the branch, then the baseline. A `Delete` in a higher layer hides the unit (and
//! everything it contributed) in every lower layer. An edge is visible only when its own unit is
//! visible; its target must also resolve in the same view, otherwise the reader reports it
//! unresolved instead of following it into a masked unit.
use crate::context::InvocationContext;
use crate::ports::storage::*;
use crate::protocol::DomainErrorCode;
use crate::protocol::Id;
use crate::protocol::data::ViewSelection;
use crate::services::projects::visible_repository;
use crate::services::sessions::{Failure, Refusal};

/// The layers of one view, highest first.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewLayers {
    pub view: StoredView,
    /// Generation IDs, highest layer first (worktree, branch, baseline).
    pub layers: Vec<Id>,
}

fn unavailable(id: &Id) -> Failure {
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
        return Err(unavailable(view));
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
            _ => return Err(unavailable(view)),
        }
    }
    Ok(ViewLayers { view: v, layers })
}

/// The views a selection names for `ctx`: the published baseline of each named repository, or
/// exact views (each visible to the invocation). An agent's own worktree view is selected with
/// [`worktree_view`].
pub fn select(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    selection: &ViewSelection,
) -> Result<Vec<ViewLayers>, Failure> {
    let mut out = Vec::new();
    match selection {
        ViewSelection::Published { repository_ids } => {
            for r in repository_ids {
                let repo = visible_repository(read.projects(), ctx, r)?;
                let Some(p) = read.indexing().published(&repo.id)? else {
                    return Err(Refusal::new(
                        DomainErrorCode::ViewUnavailable,
                        "repository_view",
                        Some(repo.id),
                    )
                    .into());
                };
                out.push(layers(read.indexing(), &p.view_id)?);
            }
        }
        ViewSelection::Exact { view_ids } => {
            for v in view_ids {
                let l = layers(read.indexing(), v)?;
                visible_repository(read.projects(), ctx, &l.view.repository_id)?;
                // An overlay view that expired or was retired is no longer current.
                if l.view.worktree_id.is_some() {
                    let current = read
                        .indexing()
                        .overlays(&l.view.repository_id)?
                        .into_iter()
                        .any(|o| {
                            o.view_id.as_ref() == Some(v)
                                && matches!(o.state.as_str(), OVERLAY_READY | OVERLAY_STALE)
                        });
                    if !current {
                        return Err(unavailable(v));
                    }
                }
                out.push(l);
            }
        }
        ViewSelection::Pinned { .. } => {
            return Err(
                Refusal::new(DomainErrorCode::UnsupportedCapability, "view_pin", None).into(),
            );
        }
        ViewSelection::SchemaPublished { .. } => {
            return Err(Refusal::new(DomainErrorCode::InvalidArgument, "views", None).into());
        }
    }
    Ok(out)
}

/// The ready view of one worktree (the agent's own view), never the baseline in its place.
pub fn worktree_view(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    repository: &Id,
    worktree: &Id,
    now_ms: i64,
) -> Result<ViewLayers, Failure> {
    visible_repository(read.projects(), ctx, repository)?;
    let Some(o) = read.indexing().overlay_for(repository, worktree)? else {
        return Err(Refusal::new(
            DomainErrorCode::ViewUnavailable,
            "worktree_view",
            Some(worktree.clone()),
        )
        .into());
    };
    let published = read
        .indexing()
        .published(repository)?
        .map(|p| read.indexing().view(&p.view_id))
        .transpose()?
        .flatten()
        .map(|v| v.baseline_id);
    let current = super::effective_config(read.indexing(), read.projects(), repository)?.digest;
    let moved = super::status::overlay_config_moved(read.indexing(), &o, &current)?;
    let state = super::status::effective_overlay_state(&o, published.as_ref(), now_ms, moved);
    match (state, &o.view_id) {
        (OVERLAY_READY, Some(v)) => layers(read.indexing(), v),
        _ => Err(Refusal::new(
            DomainErrorCode::ViewUnavailable,
            "worktree_view",
            Some(o.id),
        )
        .into()),
    }
}

/// The unit visible for `key` in the view and the layer holding it (`None`: absent or deleted).
pub fn resolve_unit(
    read: &dyn IndexRead,
    view: &ViewLayers,
    key: &str,
) -> Result<Option<(Id, StoredUnit)>, StorageError> {
    for g in &view.layers {
        if let Some(u) = read.unit(g, key)? {
            return Ok((u.action == UNIT_REPLACE).then(|| (g.clone(), u)));
        }
    }
    Ok(None)
}

/// The nodes visible for `key` in the view.
pub fn unit_nodes(
    read: &dyn IndexRead,
    view: &ViewLayers,
    key: &str,
) -> Result<Vec<StoredNode>, StorageError> {
    match resolve_unit(read, view, key)? {
        Some((g, _)) => read.nodes(&g, key),
        None => Ok(Vec::new()),
    }
}

/// The edges visible for `key` in the view, each with whether its target resolves in the same
/// view (`false`: report it unresolved).
pub fn unit_edges(
    read: &dyn IndexRead,
    view: &ViewLayers,
    key: &str,
) -> Result<Vec<(StoredEdge, bool)>, StorageError> {
    let Some((g, _)) = resolve_unit(read, view, key)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for e in read.edges(&g, key)? {
        let target_unit = e
            .payload
            .get("targetUnit")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let resolves = match target_unit {
            Some(t) => unit_nodes(read, view, &t)?
                .iter()
                .any(|n| n.node_id == e.target_node_id),
            None => false,
        };
        out.push((e, resolves));
    }
    Ok(out)
}

/// Every visible unit key of the view, by key (bounded paging over each layer).
pub fn visible_units(
    read: &dyn IndexRead,
    view: &ViewLayers,
    limit: usize,
) -> Result<Vec<String>, StorageError> {
    let mut keys = std::collections::BTreeMap::<String, bool>::new();
    // Lowest layer first so higher layers overwrite.
    for g in view.layers.iter().rev() {
        let mut after: Option<String> = None;
        loop {
            let page = read.units(g, after.as_deref(), MAX_PAGE)?;
            for u in &page {
                keys.insert(u.unit_key.clone(), u.action == UNIT_REPLACE);
            }
            if page.len() < MAX_PAGE as usize {
                break;
            }
            after = page.last().map(|u| u.unit_key.clone());
        }
    }
    Ok(keys
        .into_iter()
        .filter(|(_, v)| *v)
        .map(|(k, _)| k)
        .take(limit)
        .collect())
}
