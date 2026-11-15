//! The edges at one node, read through the store's endpoint indexes instead of the full scan,
//! and resolved by the scan's own endpoint rules. `None` wherever only the scan can answer.
use super::{
    budget::Budget,
    read::Reader,
    scan::{self, Edge, Resolved},
};
use crate::ports::storage::*;
use crate::protocol::data::Direction;
use crate::services::indexing::views::ViewLayers;
use crate::services::sessions::Failure;
use std::collections::BTreeMap;

const READ_PAGE: u16 = 128;

/// Whether an overlay view is in scope: unmapped inbound counts then need every unresolved edge
/// of the scope (an edge's original target unit, or a `targetName` matched without case), which
/// only the scan reads.
pub(super) fn overlay(reader: &Reader<'_>) -> bool {
    reader
        .selected
        .layers
        .iter()
        .any(|view| super::scope::overlay(&view.view))
}

/// The resolved edges of `scope` with `node` at the end `direction` follows (source for
/// `Forward`, target for `Reverse`, either for `Both`), in the scan's order, as the scan would
/// keep them for `kinds` (empty: every kind). `None` when the scan must answer: an overlay view in
/// scope has no stored links yet, or `node` takes part in a package reference, whose derived
/// dependency is decided across every package of the scope.
///
/// Every edge that can resolve to `node` stores its node ID at that end, or (an edge into another
/// repository) names its qualified name; the endpoint indexes return those whatever their unit,
/// and the scan's endpoint rules decide each one, including a unique node-ID match for an
/// endpoint without a unit.
pub(super) fn around(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
    layers: &[ViewLayers],
    node: &GraphNodeIdentity,
    direction: &Direction,
    kinds: &[&str],
    budget: &mut Budget,
) -> Result<Option<Vec<Edge>>, Failure> {
    let stored = scan::stored_kinds(kinds);
    let packages = stored.is_empty() || stored.iter().any(|kind| kind == "REFERENCES_PACKAGE");
    let record = read.graph().graph_node(scope, node)?;
    if packages && record.as_ref().is_some_and(|n| n.node.kind == "Package") {
        return Ok(None);
    }
    let mut probes = Vec::new();
    if !matches!(direction, Direction::Reverse) {
        probes.push((
            GraphEdgeEnd::Source,
            GraphEndpointKey::NodeId,
            node.node_id.clone(),
        ));
    }
    if !matches!(direction, Direction::Forward) {
        probes.push((
            GraphEdgeEnd::Target,
            GraphEndpointKey::NodeId,
            node.node_id.clone(),
        ));
        if let Some(qualified) = record
            .as_ref()
            .and_then(|n| n.node.payload["qualifiedName"].as_str())
        {
            probes.push((
                GraphEdgeEnd::Target,
                GraphEndpointKey::TargetQualifiedName,
                qualified.to_owned(),
            ));
        }
    }
    // Candidates are resolved as they are read, as the scan resolves its stream: a budget that
    // runs out keeps the edges already found (reported partial by the caller).
    let mut found = BTreeMap::new();
    let mut seen = std::collections::BTreeSet::new();
    'probes: for (end, key, value) in probes {
        let filter = GraphCandidateFilter {
            end,
            key,
            values: vec![value],
            kinds: stored.clone(),
        };
        let mut after = None;
        loop {
            let Some(page) =
                read.graph()
                    .graph_candidate_edges(scope, &filter, after.as_ref(), READ_PAGE)?
            else {
                return Ok(None);
            };
            for raw in page.items {
                if packages && raw.edge.kind == "REFERENCES_PACKAGE" {
                    return Ok(None);
                }
                // The scan's order: repository, unit, edge.
                let order = (
                    raw.repository_id.as_str().to_owned(),
                    raw.edge.unit_key.clone(),
                    raw.edge.edge_id.clone(),
                );
                after = Some(GraphPosition {
                    repository_id: raw.repository_id.clone(),
                    unit_key: order.1.clone(),
                    item_id: order.2.clone(),
                });
                if !seen.insert(order.clone()) {
                    continue;
                }
                if !budget.visit() {
                    break 'probes;
                }
                // A stored view link is already composed: no edge is relinked here.
                let edge = match scan::resolve(read, scope, layers, None, raw, budget)? {
                    Resolved::Edge(Ok(edge)) => edge,
                    Resolved::Skip | Resolved::Edge(Err(_)) => continue,
                    Resolved::Stop => break 'probes,
                };
                let from = !matches!(direction, Direction::Reverse) && edge.source == *node;
                let to = !matches!(direction, Direction::Forward) && edge.target == *node;
                if !from && !to {
                    continue;
                }
                if !budget.retain(
                    std::mem::size_of::<Edge>()
                        + edge.raw.edge.unit_key.len()
                        + edge.raw.edge.edge_id.len()
                        + edge.raw.edge.source_node_id.len()
                        + edge.raw.edge.target_node_id.len()
                        + edge.raw.edge.payload.to_string().len(),
                ) {
                    break 'probes;
                }
                found.insert(order, edge);
            }
            if !page.has_more {
                break;
            }
        }
    }
    Ok(Some(found.into_values().collect()))
}

/// An edge at a node with both resolved endpoints, as the parity tests compare it.
pub type ResolvedEdge = (GraphEdge, GraphNodeIdentity, GraphNodeIdentity);

fn test_budget() -> Budget {
    Budget::new(&crate::protocol::data::QueryLimits {
        max_results: 500,
        max_visited: 1_000_000,
        deadline_ms: 300_000,
    })
}

/// The edges [`around`] finds at `node`, or `None` where the graph reads scan instead.
#[doc(hidden)]
pub fn indexed_incident(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
    layers: &[ViewLayers],
    node: &GraphNodeIdentity,
    direction: &Direction,
) -> Result<Option<Vec<ResolvedEdge>>, Failure> {
    let mut budget = test_budget();
    let found = around(read, scope, layers, node, direction, &[], &mut budget)?;
    if let Some(reason) = budget.exhausted {
        return Err(StorageError::InvalidRecord(reason.into()).into());
    }
    Ok(found.map(|edges| {
        edges
            .into_iter()
            .map(|e| (e.raw, e.source, e.target))
            .collect()
    }))
}

/// The edges the full scan resolves at `node`, in its order, derived package dependencies
/// included: the reference [`indexed_incident`] is checked against.
#[doc(hidden)]
pub fn scanned_incident(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
    layers: &[ViewLayers],
    node: &GraphNodeIdentity,
    direction: &Direction,
) -> Result<Vec<ResolvedEdge>, Failure> {
    let mut budget = test_budget();
    let edges = super::dataset::edges(read, scope, layers, &[], &mut budget)?;
    if let Some(reason) = budget.exhausted {
        return Err(StorageError::InvalidRecord(reason.into()).into());
    }
    Ok(edges
        .linked
        .into_iter()
        .filter(|e| {
            (!matches!(direction, Direction::Reverse) && e.source == *node)
                || (!matches!(direction, Direction::Forward) && e.target == *node)
        })
        .map(|e| (e.raw, e.source, e.target))
        .collect())
}
