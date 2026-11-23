//! Query-local bounded materializations for algorithms needing a composed adjacency set.
use super::{budget::Budget, scan};
use crate::ports::storage::*;
use crate::protocol::data::Direction;
use crate::services::indexing::views::ViewLayers;
use crate::services::kernel::Failure;
use std::collections::{BTreeMap, BTreeSet};

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
    scan::composed_edges(read, scope, layers, kinds, budget, |edge, budget| {
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

type Endpoint<'a> = (&'a str, &'a str, &'a str);

fn endpoint(identity: &GraphNodeIdentity) -> Endpoint<'_> {
    (
        identity.repository_id.as_str(),
        &identity.unit_key,
        &identity.node_id,
    )
}

#[derive(Default)]
struct Incident {
    incoming: Vec<usize>,
    outgoing: Vec<usize>,
}

/// Index a composed scan once. Package derivations and overlay masking still come from that
/// scan. Offsets preserve its ordering for shortest paths, matching ties and continuation.
pub(super) struct Adjacency<'a> {
    edges: &'a [scan::Edge],
    incident: BTreeMap<Endpoint<'a>, Incident>,
}

impl<'a> Adjacency<'a> {
    pub fn new(edges: &'a [scan::Edge], budget: &mut Budget) -> Self {
        let mut incident = BTreeMap::<_, Incident>::new();
        'edges: for (index, edge) in edges.iter().enumerate() {
            if !budget.check() {
                break;
            }
            for (identity, outgoing) in [(&edge.source, true), (&edge.target, false)] {
                let key = endpoint(identity);
                if !budget.retain(2 * std::mem::size_of::<usize>()) {
                    break 'edges;
                }
                let entry = match incident.entry(key) {
                    std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        if !budget.retain(std::mem::size_of::<(Endpoint<'_>, Incident)>() + 32) {
                            break 'edges;
                        }
                        entry.insert(Incident::default())
                    }
                };
                if outgoing {
                    entry.outgoing.push(index);
                } else {
                    entry.incoming.push(index);
                }
            }
        }
        Self { edges, incident }
    }

    fn offsets<'s>(
        &'s self,
        node: &'s GraphNodeIdentity,
        direction: &'s Direction,
    ) -> impl Iterator<Item = usize> + use<'s, 'a> {
        let entry = self.incident.get(&endpoint(node));
        let mut outgoing = entry
            .filter(|_| !matches!(direction, Direction::Reverse))
            .map_or(&[][..], |entry| entry.outgoing.as_slice())
            .iter()
            .copied()
            .peekable();
        let mut incoming = entry
            .filter(|_| !matches!(direction, Direction::Forward))
            .map_or(&[][..], |entry| entry.incoming.as_slice())
            .iter()
            .copied()
            .peekable();
        std::iter::from_fn(move || match (outgoing.peek(), incoming.peek()) {
            (Some(a), Some(b)) if a == b => {
                incoming.next();
                outgoing.next()
            }
            (Some(a), Some(b)) if a < b => outgoing.next(),
            (_, Some(_)) => incoming.next(),
            _ => outgoing.next(),
        })
    }

    pub fn around<'s>(
        &'s self,
        node: &'s GraphNodeIdentity,
        direction: &'s Direction,
    ) -> impl Iterator<Item = &'s scan::Edge> + use<'s, 'a> {
        self.offsets(node, direction)
            .map(move |index| &self.edges[index])
    }

    pub fn around_all<'n>(
        &self,
        nodes: impl IntoIterator<Item = &'n GraphNodeIdentity>,
        direction: &Direction,
        budget: &mut Budget,
    ) -> Vec<&scan::Edge> {
        let mut offsets = BTreeSet::new();
        'nodes: for node in nodes {
            for index in self.offsets(node, direction) {
                if !budget.check() {
                    break 'nodes;
                }
                if offsets.contains(&index) {
                    continue;
                }
                // Account for the ordered set and the returned slice of references.
                if !budget.retain(4 * std::mem::size_of::<usize>()) {
                    break 'nodes;
                }
                offsets.insert(index);
            }
        }
        offsets
            .into_iter()
            .map(move |index| &self.edges[index])
            .collect()
    }
}

pub(super) fn low_node(node: &GraphNode) -> bool {
    node.node.payload["properties"]
        .get("confidence")
        .or_else(|| node.node.payload.get("confidence"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| value.eq_ignore_ascii_case("low"))
}
