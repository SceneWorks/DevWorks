//! Query-local bounded materializations for algorithms needing a composed adjacency set.
use super::{budget::Budget, scan};
use crate::ports::storage::*;
use crate::services::indexing::views::ViewLayers;
use crate::services::sessions::Failure;

pub(super) type Key = (String, String, String);

pub(super) fn key(identity: &GraphNodeIdentity) -> Key {
    (
        identity.repository_id.as_str().into(),
        identity.unit_key.clone(),
        identity.node_id.clone(),
    )
}

pub(super) fn identity(node: &GraphNode) -> GraphNodeIdentity {
    GraphNodeIdentity {
        repository_id: node.repository_id.clone(),
        unit_key: node.node.unit_key.clone(),
        node_id: node.node.node_id.clone(),
    }
}

pub(super) fn node_key(node: &GraphNode) -> Key {
    key(&identity(node))
}

pub(super) fn nodes(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
    filter: &GraphNodeFilter,
    budget: &mut Budget,
) -> Result<Vec<GraphNode>, Failure> {
    let mut rows = Vec::new();
    scan::nodes(read, scope, filter, budget, |node, budget| {
        let bytes = std::mem::size_of::<GraphNode>()
            + node.node.unit_key.len()
            + node.node.node_id.len()
            + node.node.name.len()
            + node.node.kind.len()
            + node.node.payload.to_string().len();
        if !budget.retain(bytes) {
            return Ok(false);
        }
        rows.push(node);
        Ok(true)
    })?;
    Ok(rows)
}

pub(super) struct Edges {
    pub linked: Vec<scan::Edge>,
    pub unmapped: Vec<scan::Unmapped>,
}

pub(super) fn edges(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
    layers: &[ViewLayers],
    kinds: &[&str],
    budget: &mut Budget,
) -> Result<Edges, Failure> {
    let mut linked = Vec::new();
    let mut unmapped = Vec::new();
    scan::edges(read, scope, layers, kinds, budget, |edge, budget| {
        let raw = match &edge {
            Ok(edge) => &edge.raw,
            Err(unmapped) => &unmapped.raw,
        };
        let original_bytes = match &edge {
            Ok(_) => 0,
            Err(e) => {
                e.original.edge.payload.to_string().len()
                    + e.original.edge.unit_key.len()
                    + e.original.edge.target_node_id.len()
                    + e.original.edge.source_node_id.len()
            }
        };
        let bytes = original_bytes
            + std::mem::size_of::<scan::Edge>()
            + raw.edge.unit_key.len()
            + raw.edge.edge_id.len()
            + raw.edge.source_node_id.len()
            + raw.edge.target_node_id.len()
            + raw.edge.payload.to_string().len();
        if !budget.retain(bytes) {
            return Ok(false);
        }
        match edge {
            Ok(edge) => linked.push(edge),
            Err(raw) => unmapped.push(raw),
        }
        Ok(true)
    })?;
    Ok(Edges { linked, unmapped })
}

pub(super) fn low_node(node: &GraphNode) -> bool {
    node.node.payload["properties"]
        .get("confidence")
        .or_else(|| node.node.payload.get("confidence"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| value.eq_ignore_ascii_case("low"))
}
