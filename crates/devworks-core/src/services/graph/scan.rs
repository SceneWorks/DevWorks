//! Bounded, whole-unit-masked graph iteration. Expansion never selects another repository.
use super::budget::Budget;
use crate::ports::storage::*;
use crate::services::indexing::views::ViewLayers;
use crate::services::sessions::Failure;

const READ_PAGE: u16 = 32;

pub(super) fn nodes(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
    filter: &GraphNodeFilter,
    budget: &mut Budget,
    mut visit: impl FnMut(GraphNode, &mut Budget) -> Result<bool, Failure>,
) -> Result<(), Failure> {
    let mut after = None;
    loop {
        let page = read
            .graph()
            .graph_nodes(scope, filter, after.as_ref(), READ_PAGE)?;
        let more = page.has_more;
        for node in page.items {
            if !budget.visit() {
                return Ok(());
            }
            after = Some(GraphPosition {
                repository_id: node.repository_id.clone(),
                unit_key: node.node.unit_key.clone(),
                item_id: node.node.node_id.clone(),
            });
            if !visit(node, budget)? {
                return Ok(());
            }
        }
        if !more {
            return Ok(());
        }
    }
}

/// Resolves an endpoint inside its captured scope. A missing unit annotation is answered by
/// exact node identity only when unique, never by choosing the first colliding unit.
pub(super) fn endpoint(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
    repository: &crate::protocol::Id,
    unit: Option<&str>,
    node: &str,
    budget: &mut Budget,
) -> Result<Option<GraphNodeIdentity>, Failure> {
    if !scope.views.iter().any(|v| &v.repository_id == repository) {
        return Ok(None);
    }
    if let Some(unit) = unit {
        if !budget.visit() {
            return Ok(None);
        }
        let identity = GraphNodeIdentity {
            repository_id: repository.clone(),
            unit_key: unit.into(),
            node_id: node.into(),
        };
        return Ok(read.graph().graph_node(scope, &identity)?.map(|_| identity));
    }
    let local = GraphScope {
        views: scope
            .views
            .iter()
            .filter(|v| &v.repository_id == repository)
            .cloned()
            .collect(),
    };
    let filter = GraphNodeFilter {
        node_id: Some(node.into()),
        ..Default::default()
    };
    let mut found = None;
    let mut ambiguous = false;
    nodes(read, &local, &filter, budget, |candidate, _| {
        if found.is_some() {
            ambiguous = true;
            return Ok(false);
        }
        found = Some(GraphNodeIdentity {
            repository_id: candidate.repository_id,
            unit_key: candidate.node.unit_key,
            node_id: candidate.node.node_id,
        });
        Ok(true)
    })?;
    Ok(if ambiguous { None } else { found })
}

pub(super) struct Edge {
    pub raw: GraphEdge,
    pub source: GraphNodeIdentity,
    pub target: GraphNodeIdentity,
}

/// Both the re-evaluated result and its immutable producing-layer endpoint evidence.
/// Removed-file attribution must not rediscover an old target from today's baseline.
pub(super) struct Unmapped {
    pub raw: GraphEdge,
    pub original: GraphEdge,
}

/// Raw edges are streamed before endpoint filtering: inherited names may have rebound in
/// this composition, and SQL's old target index must not hide those incoming references.
pub(super) fn edges(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
    layers: &[ViewLayers],
    budget: &mut Budget,
    mut visit: impl FnMut(Result<Edge, Unmapped>, &mut Budget) -> Result<bool, Failure>,
) -> Result<(), Failure> {
    let mut after = None;
    let mut sessions =
        std::collections::HashMap::<String, crate::services::indexing::worker::ViewLinker>::new();
    loop {
        let page = read.graph().graph_edges(
            scope,
            &GraphEdgeFilter::default(),
            after.as_ref(),
            READ_PAGE,
        )?;
        let more = page.has_more;
        for mut raw in page.items {
            if !budget.visit() {
                return Ok(());
            }
            after = Some(GraphPosition {
                repository_id: raw.repository_id.clone(),
                unit_key: raw.edge.unit_key.clone(),
                item_id: raw.edge.edge_id.clone(),
            });
            let Some(view) = layers.iter().find(|v| v.view.view_id == raw.view_id) else {
                continue;
            };
            let original = raw.clone();
            if view.layers.len() > 1 {
                // The shared index linker retains language/receiver/import ambiguity rules.
                let unit_key = raw.edge.unit_key.clone();
                let linked = super::linking::link(
                    read.indexing(),
                    view,
                    sessions
                        .entry(view.view.view_id.as_str().to_owned())
                        .or_default(),
                    &unit_key,
                    vec![raw.edge],
                    budget,
                )?;
                let Some(linked) = linked else {
                    return Ok(());
                };
                let Some(edge) = linked.into_iter().next() else {
                    continue;
                };
                raw.edge = edge;
            }
            let payload = &raw.edge.payload;
            let repository = |key: &str| match payload.get(key) {
                None => Some(raw.repository_id.clone()),
                Some(value) => serde_json::from_value(value.clone()).ok(),
            };
            let (Some(source_repo), Some(target_repo)) = (
                repository("sourceRepositoryId"),
                repository("targetRepositoryId"),
            ) else {
                if !visit(Err(Unmapped { raw, original }), budget)? {
                    return Ok(());
                }
                continue;
            };
            if ["sourceUnit", "targetUnit"]
                .iter()
                .any(|key| payload.get(*key).is_some_and(|value| !value.is_string()))
            {
                if !visit(Err(Unmapped { raw, original }), budget)? {
                    return Ok(());
                }
                continue;
            }
            let source = endpoint(
                read,
                scope,
                &source_repo,
                payload
                    .get("sourceUnit")
                    .and_then(serde_json::Value::as_str),
                &raw.edge.source_node_id,
                budget,
            )?;
            let target = endpoint(
                read,
                scope,
                &target_repo,
                payload
                    .get("targetUnit")
                    .and_then(serde_json::Value::as_str),
                &raw.edge.target_node_id,
                budget,
            )?;
            let linked = match (source, target) {
                (Some(source), Some(target)) => Ok(Edge {
                    raw,
                    source,
                    target,
                }),
                _ => Err(Unmapped { raw, original }),
            };
            if !visit(linked, budget)? {
                return Ok(());
            }
        }
        if !more {
            return Ok(());
        }
    }
}
