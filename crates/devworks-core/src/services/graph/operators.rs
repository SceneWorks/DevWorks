//! Bounded graph neighborhood and path operators over one immutable composition.
use super::{
    budget::Budget,
    dataset::{self, Adjacency, Key},
    model,
    read::{Position, Reader},
    scan,
};
use crate::ports::storage::*;
use crate::protocol::{
    data::{Direction, GraphResult},
    records::*,
};
use crate::services::kernel::Failure;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

fn raw(reader: &Reader<'_>, id: &GraphNodeIdentity) -> Result<GraphNode, Failure> {
    reader
        .read
        .graph()
        .graph_node(reader.scope(), id)?
        .ok_or_else(|| super::scope::unavailable(Some(id.repository_id.clone())))
}

pub(super) fn detail(
    reader: &mut Reader<'_>,
    id: &crate::protocol::Id,
) -> Result<GraphResult, Failure> {
    let target = reader
        .find_id(id)?
        .ok_or_else(|| crate::services::kernel::not_found("node", id))?;
    let (node, property) = reader.node(&target, reader.position.property.clone().as_ref())?;
    if let Some(property) = property {
        reader.next = Some(Position {
            property: Some(property),
            ..reader.position.clone()
        });
    }
    if reader.next.is_some()
        && reader.position.edge_property.is_none()
        && reader.position.offset == 0
    {
        return Ok(GraphResult::NodeDetail {
            value: CodeNodeDetail {
                node,
                neighbors: model::list(Vec::new())?,
                incoming: model::list(Vec::new())?,
                outgoing: model::list(Vec::new())?,
                cross_repository: model::list(Vec::new())?,
                relationships_complete: false,
            },
        });
    }
    if reader.position.edge_property.is_some() || reader.position.offset > 0 {
        reader.next = None;
    }
    let edges = match indexed_incident(reader, &target)? {
        Some(linked) => dataset::Edges {
            linked,
            unmapped: Vec::new(),
        },
        None => reader.raw_edges()?,
    };
    let unmapped = super::unmapped::count(
        reader,
        &edges.unmapped,
        &[dataset::identity(&target)],
        None,
        &[],
    )?;
    let key = dataset::node_key(&target);
    let incident: Vec<_> = edges
        .linked
        .iter()
        .filter(|e| dataset::key(&e.source) == key || dataset::key(&e.target) == key)
        .collect();
    let mut neighbors = BTreeMap::new();
    let mut incoming = Vec::new();
    let mut outgoing = Vec::new();
    let mut cross = Vec::new();
    let offset = reader.position.offset;
    for (index, edge) in incident.iter().enumerate().skip(offset) {
        if index - offset >= reader.limit.min(249) {
            reader.next = Some(Position {
                offset: index,
                ..Default::default()
            });
            break;
        }
        let other = if dataset::key(&edge.source) == key {
            &edge.target
        } else {
            &edge.source
        };
        let other_node = raw(reader, other)?;
        let projected = reader.node(&other_node, None)?.0;
        let (projected_edge, edge_next) = model::edge_page(
            edge,
            if index == offset {
                reader.position.edge_property.as_ref()
            } else {
                None
            },
        )?;
        if !reader.retain_wire(&(&projected, &projected_edge))? {
            reader.next = Some(Position {
                offset: index,
                ..Default::default()
            });
            break;
        }
        neighbors.insert(dataset::key(other), projected);
        if dataset::key(&edge.source) == key {
            outgoing.push(projected_edge.clone());
        } else {
            incoming.push(projected_edge.clone());
        }
        if edge.source.repository_id != edge.target.repository_id {
            cross.push(projected_edge);
        }
        if let Some(property) = edge_next {
            reader.next = Some(Position {
                offset: index,
                edge_property: Some(property),
                ..Default::default()
            });
            break;
        }
    }
    Ok(GraphResult::NodeDetail {
        value: CodeNodeDetail {
            node,
            neighbors: model::list(neighbors.into_values().collect())?,
            incoming: model::list(incoming)?,
            outgoing: model::list(outgoing)?,
            cross_repository: model::list(cross)?,
            relationships_complete: reader.next.is_none()
                && reader.budget.exhausted.is_none()
                && !reader.gaps.contains(&"graph_visited_limit")
                && reader.selected.pin.unavailable_repositories.is_empty()
                && unmapped.is_none_or(|count| count == 0),
        },
    })
}

/// The edges at `node` through the endpoint indexes, where they answer exactly as the scan (see
/// [`super::incident::around`]), with the scan's visit reservation and gap. Without an overlay
/// view in scope there is no unmapped inbound count to read every unresolved edge for.
fn indexed_incident(
    reader: &mut Reader<'_>,
    node: &GraphNode,
) -> Result<Option<Vec<scan::Edge>>, Failure> {
    if super::incident::overlay(reader) {
        return Ok(None);
    }
    let identity = dataset::identity(node);
    let (read, selected) = (reader.read, reader.selected);
    let (found, partial) = reader.budget.reserve_visits(|budget| {
        super::incident::around(
            read,
            &selected.pin.scope,
            &selected.layers,
            &identity,
            &Direction::Both,
            &[],
            budget,
        )
    });
    let found = found?;
    if found.is_some() && partial && !reader.gaps.contains(&"graph_visited_limit") {
        reader.gaps.push("graph_visited_limit");
    }
    Ok(found)
}

pub(super) fn by_file(
    reader: &mut Reader<'_>,
    selection: &CodeFileSelection,
) -> Result<GraphResult, Failure> {
    let scope = reader.scoped_repository(&selection.repository_id)?;
    let mut rows = reader.raw_nodes(
        &scope,
        &GraphNodeFilter {
            unit_key: Some(selection.path.0.clone()),
            ..Default::default()
        },
    )?;
    rows.sort_by(|a, b| {
        let span = |n: &GraphNode| {
            let start = n.node.payload["span"]["startLine"].as_u64().unwrap_or(0);
            let end = n.node.payload["span"]["endLine"].as_u64().unwrap_or(start);
            (
                end.saturating_sub(start),
                match n.node.kind.as_str() {
                    "Method" => 0,
                    "Class" => 1,
                    _ => 2,
                },
                n.node.node_id.clone(),
            )
        };
        span(a).cmp(&span(b))
    });
    let selected = if let Some(line) = &selection.line {
        let line = line
            .0
            .parse::<u64>()
            .map_err(|_| StorageError::CursorExpired)?;
        rows.iter()
            .find(|n| {
                n.node.payload["span"]["startLine"]
                    .as_u64()
                    .is_some_and(|s| s <= line)
                    && n.node.payload["span"]["endLine"]
                        .as_u64()
                        .is_some_and(|e| line <= e)
            })
            .or_else(|| rows.iter().find(|n| n.node.kind == "File"))
            .or_else(|| rows.first())
            .map(|n| n.node.node_id.clone())
    } else {
        None
    };
    let total = if reader.budget.exhausted.is_none() {
        Some(model::count(rows.len() as u64)?)
    } else {
        None
    };
    let nodes = reader.page_nodes(&rows)?;
    let low = model::low_nodes(&nodes);
    Ok(GraphResult::CodeNodes {
        value: CodeNodesResult {
            nodes: crate::protocol::support::BoundedList(nodes),
            total,
            low_confidence_count: model::count(low as u64)?,
            selected_node_id: selected.map(|id| model::text(id.as_str())).transpose()?,
        },
    })
}

pub(super) struct Walk<'a> {
    pub starts: &'a [GraphNode],
    pub kinds: &'a [&'a str],
    pub direction: Direction,
    pub depth: u16,
    /// The caller reads the walk's complete edge set (unmapped inbound counts, cross-repository
    /// links), which only the scan has.
    pub complete: bool,
}
/// One traversal row: the node reached, its depth and the edge that led to it.
fn traversal_entry(
    reader: &mut Reader<'_>,
    node: &GraphNode,
    step: &Step,
) -> Result<CodeTraversalEntry, Failure> {
    Ok(CodeTraversalEntry {
        node: reader.node(node, None)?.0,
        depth: model::count(u64::from(step.depth))?,
        parent_id: Some(model::text(&step.parent.node_id)?),
        edge_kind: Some(model::text(&step.edge.raw.edge.kind)?),
        edge_id: Some(model::text(&step.edge.raw.edge.edge_id)?),
    })
}
struct Step {
    identity: GraphNodeIdentity,
    depth: u16,
    parent: GraphNodeIdentity,
    edge: scan::Edge,
}
fn walk(reader: &mut Reader<'_>, config: Walk<'_>) -> Result<(Vec<Step>, dataset::Edges), Failure> {
    if !config.complete
        && let Some(steps) = indexed_walk(reader, &config)?
    {
        let edges = dataset::Edges {
            linked: Vec::new(),
            unmapped: Vec::new(),
        };
        return Ok((steps, edges));
    }
    let edges = reader.filtered_edges(config.kinds)?;
    let starts = super::unmapped::identities(config.starts);
    let (adjacency, partial) = Adjacency::new(&edges.linked, &starts, &mut reader.budget);
    reader.note_index_limits(false, partial);
    let mut walk = BreadthFirst::new(starts);
    while let Some((current, depth)) = walk.next(config.depth, &mut reader.budget) {
        walk.expand(
            &mut reader.budget,
            &config,
            &current,
            depth,
            adjacency.around(&current, &config.direction),
        );
    }
    Ok((walk.steps, edges))
}

/// The walk over each reached node's own edges, read through the endpoint indexes.
/// If a node needs the composed scan, its result ordering matches this path.
fn indexed_walk(reader: &mut Reader<'_>, config: &Walk<'_>) -> Result<Option<Vec<Step>>, Failure> {
    let (read, selected) = (reader.read, reader.selected);
    let mut walk = BreadthFirst::new(config.starts.iter().map(dataset::identity));
    while let Some((current, depth)) = walk.next(config.depth, &mut reader.budget) {
        // As in the scan's materialization, preserve visits for following the edges already read.
        let ((edges, visit_partial), memory_partial) = reader.budget.reserve_retention(|budget| {
            budget.reserve_visits(|budget| {
                super::incident::around(
                    read,
                    &selected.pin.scope,
                    &selected.layers,
                    &current,
                    &config.direction,
                    config.kinds,
                    budget,
                )
            })
        });
        let Some(edges) = edges? else {
            return Ok(None);
        };
        reader.note_index_limits(visit_partial, memory_partial);
        walk.expand(&mut reader.budget, config, &current, depth, &edges);
    }
    Ok(Some(walk.steps))
}

/// Breadth-first discovery expands a node at its minimum distance, once. Every followed
/// relationship still contributes a step, even when it points to an already reached node.
struct BreadthFirst {
    queue: VecDeque<(GraphNodeIdentity, u16)>,
    visited: BTreeSet<Key>,
    steps: Vec<Step>,
}

impl BreadthFirst {
    fn new(starts: impl IntoIterator<Item = GraphNodeIdentity>) -> Self {
        let mut walk = Self {
            queue: VecDeque::new(),
            visited: BTreeSet::new(),
            steps: Vec::new(),
        };
        for start in starts {
            if walk.visited.insert(dataset::key(&start)) {
                walk.queue.push_back((start, 0));
            }
        }
        walk
    }

    fn next(&mut self, depth: u16, budget: &mut Budget) -> Option<(GraphNodeIdentity, u16)> {
        let next = self.queue.pop_front()?;
        (next.1 < depth && budget.check()).then_some(next)
    }

    fn expand<'e>(
        &mut self,
        budget: &mut Budget,
        config: &Walk<'_>,
        current: &GraphNodeIdentity,
        depth: u16,
        edges: impl IntoIterator<Item = &'e scan::Edge>,
    ) {
        for edge in edges {
            if !budget.check() {
                break;
            }
            if !config.kinds.is_empty() && !config.kinds.contains(&edge.raw.edge.kind.as_str()) {
                continue;
            }
            let target = if !matches!(config.direction, Direction::Reverse)
                && edge.source == *current
            {
                &edge.target
            } else if !matches!(config.direction, Direction::Forward) && edge.target == *current {
                &edge.source
            } else {
                continue;
            };
            if !budget.visit()
                || !budget
                    .retain(std::mem::size_of::<Step>() + edge.raw.edge.payload.to_string().len())
            {
                break;
            }
            let next = depth + 1;
            self.steps.push(Step {
                identity: target.clone(),
                depth: next,
                parent: current.clone(),
                edge: edge.clone(),
            });
            if self.visited.insert(dataset::key(target)) {
                self.queue.push_back((target.clone(), next));
            }
        }
    }
}
fn entries(reader: &mut Reader<'_>, steps: &[Step]) -> Result<Vec<CodeTraversalEntry>, Failure> {
    let mut rows = Vec::new();
    for (index, step) in steps.iter().enumerate().skip(reader.position.offset) {
        if rows.len() >= reader.limit.min(500) {
            reader.next = Some(Position {
                offset: index,
                ..Default::default()
            });
            break;
        }
        let node = raw(reader, &step.identity)?;
        let entry = traversal_entry(reader, &node, step)?;
        if !reader.retain_wire(&entry)? {
            reader.next = Some(Position {
                offset: index,
                ..Default::default()
            });
            break;
        }
        rows.push(entry);
    }
    Ok(rows)
}

pub(super) fn traverse(
    reader: &mut Reader<'_>,
    starts: &[crate::protocol::Id],
    direction: Direction,
    kinds: &[crate::protocol::Id],
    depth: u16,
) -> Result<GraphResult, Failure> {
    let mut seeds = Vec::new();
    for id in starts {
        if let Some(node) = reader.find_id(id)? {
            seeds.push(node);
        }
    }
    let kinds: Vec<_> = kinds.iter().map(|k| k.as_str()).collect();
    let complete = super::incident::overlay(reader);
    let (steps, all) = walk(
        reader,
        Walk {
            starts: &seeds,
            kinds: &kinds,
            direction,
            depth,
            complete,
        },
    )?;
    let identities: Vec<_> = seeds
        .iter()
        .map(dataset::identity)
        .chain(steps.iter().map(|step| step.identity.clone()))
        .collect();
    let unmapped = super::unmapped::count(reader, &all.unmapped, &identities, None, &kinds)?;
    graph_steps(reader, &steps, unmapped)
}
fn graph_steps(
    reader: &mut Reader<'_>,
    steps: &[Step],
    unmapped: Option<u64>,
) -> Result<GraphResult, Failure> {
    let mut nodes = BTreeMap::<Key, CodeNode>::new();
    let mut edges = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, step) in steps.iter().enumerate() {
        // Prefix identity is part of the logical traversal, including rows before this page.
        // Borrow stable step strings; page boundaries never reset edge deduplication.
        if !seen.insert((
            step.edge.raw.repository_id.as_str(),
            step.edge.raw.edge.unit_key.as_str(),
            step.edge.raw.edge.edge_id.as_str(),
        )) {
            continue;
        }
        if !reader.budget.retain(96) {
            break;
        }
        if index < reader.position.offset {
            continue;
        }
        if edges.len() >= reader.limit.min(250) {
            reader.next = Some(Position {
                offset: index,
                ..Default::default()
            });
            break;
        }
        let (edge, edge_next) = model::edge_page(
            &step.edge,
            if index == reader.position.offset {
                reader.position.edge_property.as_ref()
            } else {
                None
            },
        )?;
        let a = raw(reader, &step.edge.source)?;
        let b = raw(reader, &step.edge.target)?;
        let a_node = reader.node(&a, None)?.0;
        let b_node = reader.node(&b, None)?.0;
        if !reader.retain_wire(&(&edge, &a_node, &b_node))? {
            reader.next = Some(Position {
                offset: index,
                ..Default::default()
            });
            break;
        }
        nodes.insert(dataset::node_key(&a), a_node);
        nodes.insert(dataset::node_key(&b), b_node);
        edges.push(edge);
        if let Some(property) = edge_next {
            reader.next = Some(Position {
                offset: index,
                edge_property: Some(property),
                ..Default::default()
            });
            break;
        }
    }
    let low = model::low_nodes(nodes.values());
    let mut value = json!({"nodes":nodes.into_values().collect::<Vec<_>>(),"edges":edges,"lowConfidenceCount":low.to_string()});
    if let Some(count) = unmapped {
        value["unmappedInboundEdges"] = json!(count.to_string());
    }
    Ok(GraphResult::CodeGraph {
        value: model::wire(value)?,
    })
}

pub(super) fn calls(
    reader: &mut Reader<'_>,
    selection: &CodeCallSelection,
) -> Result<GraphResult, Failure> {
    let mut starts = reader.targets(&selection.target)?;
    starts.sort_by_key(|n| !matches!(n.node.kind.as_str(), "Method" | "Function"));
    starts.truncate(1);
    let direction = match selection.direction {
        CodeTraceDirection::Inbound => Direction::Reverse,
        CodeTraceDirection::Outbound => Direction::Forward,
        CodeTraceDirection::Both => Direction::Both,
    };
    let complete = super::incident::overlay(reader);
    let (steps, all) = walk(
        reader,
        Walk {
            starts: &starts,
            kinds: &["CALLS", "HTTP_CALLS"],
            direction,
            depth: selection.depth.get() as u16,
            complete,
        },
    )?;
    let identities: Vec<_> = starts
        .iter()
        .map(dataset::identity)
        .chain(steps.iter().map(|step| step.identity.clone()))
        .collect();
    let unmapped = super::unmapped::count(
        reader,
        &all.unmapped,
        &identities,
        Some(&selection.target),
        &["CALLS", "HTTP_CALLS"],
    )?;
    let entries = entries(reader, &steps)?;
    let start = starts
        .first()
        .map(|n| reader.node(n, None).map(|n| n.0))
        .transpose()?;
    let low = model::low_nodes(entries.iter().map(|entry| &entry.node).chain(start.iter()));
    let mut value = json!({"entries":entries,"lowConfidenceCount":low.to_string()});
    if let Some(start) = start {
        value["start"] = json!(start);
    }
    if let Some(count) = unmapped {
        value["unmappedInboundEdges"] = json!(count.to_string());
    }
    Ok(GraphResult::CallTrace {
        value: model::wire(value)?,
    })
}

pub(super) fn shortest(
    reader: &mut Reader<'_>,
    from: &crate::protocol::Id,
    to: &crate::protocol::Id,
    depth: u16,
) -> Result<GraphResult, Failure> {
    let Some(start) = reader.find_id(from)? else {
        return Ok(GraphResult::CodeGraph {
            value: CodeGraphResult {
                nodes: model::list(Vec::new())?,
                edges: model::list(Vec::new())?,
                low_confidence_count: model::count(0)?,
                unmapped_inbound_edges: None,
            },
        });
    };
    let target = reader.find_id(to)?;
    let (steps, _) = walk(
        reader,
        Walk {
            starts: &[start],
            kinds: &[],
            direction: Direction::Forward,
            depth,
            complete: false,
        },
    )?;
    let Some(target) = target else {
        return graph_steps(reader, &[], None);
    };
    let mut cursor = dataset::node_key(&target);
    let mut path = Vec::new();
    if let Some(mut index) = steps
        .iter()
        .position(|s| dataset::key(&s.identity) == cursor)
    {
        loop {
            let step = &steps[index];
            path.push(Step {
                identity: step.identity.clone(),
                depth: step.depth,
                parent: step.parent.clone(),
                edge: step.edge.clone(),
            });
            cursor = dataset::key(&step.parent);
            let Some(previous) = steps[..index]
                .iter()
                .rposition(|s| dataset::key(&s.identity) == cursor && s.depth + 1 == step.depth)
            else {
                break;
            };
            index = previous;
        }
    }
    path.reverse();
    graph_steps(reader, &path, None)
}

pub(super) fn related(
    reader: &mut Reader<'_>,
    selection: &CodeNodeSelection,
    publishers: bool,
) -> Result<GraphResult, Failure> {
    let starts = reader.targets(selection)?;
    let keys: BTreeSet<_> = starts.iter().map(dataset::node_key).collect();
    let kinds = if publishers {
        &["PUBLISHES", "CALLS"][..]
    } else {
        &["CONSUMES", "CALLS", "HTTP_CALLS", "USES_TYPE", "HANDLES"][..]
    };
    // Every edge kept below ends at a start. Without an overlay view in scope there is no
    // unmapped inbound count to read every unresolved edge for.
    let indexed = if super::incident::overlay(reader) {
        None
    } else {
        let identities = super::unmapped::identities(&starts);
        let (read, selected) = (reader.read, reader.selected);
        let (found, partial) = reader.budget.reserve_visits(|budget| {
            super::incident::around_all(
                read,
                &selected.pin.scope,
                &selected.layers,
                &identities,
                &Direction::Reverse,
                kinds,
                budget,
            )
        });
        let found = found?;
        if found.is_some() && partial && !reader.gaps.contains(&"graph_visited_limit") {
            reader.gaps.push("graph_visited_limit");
        }
        found
    };
    let edges = match indexed {
        Some(linked) => dataset::Edges {
            linked,
            unmapped: Vec::new(),
        },
        None => reader.filtered_edges(kinds)?,
    };
    let unmapped = super::unmapped::count(
        reader,
        &edges.unmapped,
        &super::unmapped::identities(&starts),
        Some(selection),
        kinds,
    )?;
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    for edge in &edges.linked {
        if !keys.contains(&dataset::key(&edge.target))
            || !kinds.contains(&edge.raw.edge.kind.as_str())
        {
            continue;
        }
        let identity = dataset::key(&edge.source);
        if seen.contains(&identity) {
            continue;
        }
        // Endpoint identities are bounded, unlike source payloads. Retain only this
        // deduplication prefix and hydrate a node after it enters the requested page.
        if !reader
            .budget
            .retain(identity.0.len() + identity.1.len() + identity.2.len() + 96)
        {
            break;
        }
        seen.insert(identity);
        let index = seen.len() - 1;
        if index < reader.position.offset {
            continue;
        }
        if result.len() >= reader.limit {
            reader.next = Some(Position {
                offset: index,
                ..Default::default()
            });
            break;
        }
        let node = raw(reader, &edge.source)?;
        let item = CodeRelatedNode {
            node: reader.node(&node, None)?.0,
            edge_kind: model::text(&edge.raw.edge.kind)?,
            low_confidence: dataset::low_node(&node),
        };
        if !reader.retain_wire(&item)? {
            reader.next = Some(Position {
                offset: index,
                ..Default::default()
            });
            break;
        }
        result.push(item);
    }
    let low = result
        .iter()
        .filter(|r: &&CodeRelatedNode| r.low_confidence)
        .count();
    let mut value = json!({"nodes":result,"lowConfidenceCount":low.to_string()});
    if let Some(count) = unmapped {
        value["unmappedInboundEdges"] = json!(count.to_string());
    }
    Ok(GraphResult::Related {
        value: model::wire(value)?,
    })
}

pub(super) fn lineage(
    reader: &mut Reader<'_>,
    selection: &CodeNodeSelection,
) -> Result<GraphResult, Failure> {
    let mut starts = reader.targets(selection)?;
    starts.sort_by_key(|n| !matches!(n.node.kind.as_str(), "Class" | "Record" | "Interface"));
    starts.truncate(1);
    let kinds = &["USES_TYPE", "PUBLISHES", "CONSUMES", "HTTP_CALLS"];
    let (inbound, _) = walk(
        reader,
        Walk {
            starts: &starts,
            kinds,
            direction: Direction::Reverse,
            depth: 4,
            complete: false,
        },
    )?;
    // Cross-repository links are every such edge of the start's repository, which only the
    // scan reads; a scope of one repository has none.
    let repositories: BTreeSet<_> = reader
        .scope()
        .views
        .iter()
        .map(|view| view.repository_id.as_str())
        .collect();
    let complete = super::incident::overlay(reader) || repositories.len() > 1;
    let (outbound, all) = walk(
        reader,
        Walk {
            starts: &starts,
            kinds,
            direction: Direction::Forward,
            depth: 4,
            complete,
        },
    )?;
    let identities: Vec<_> = starts
        .iter()
        .map(dataset::identity)
        .chain(
            inbound
                .iter()
                .chain(&outbound)
                .map(|step| step.identity.clone()),
        )
        .collect();
    let unmapped =
        super::unmapped::count(reader, &all.unmapped, &identities, Some(selection), kinds)?;
    let cross: Vec<_> = all
        .linked
        .iter()
        .filter(|edge| {
            edge.source.repository_id != edge.target.repository_id
                && starts.first().is_some_and(|start| {
                    edge.source.repository_id == start.repository_id
                        || edge.target.repository_id == start.repository_id
                })
        })
        .collect();
    let total = inbound.len() + outbound.len() + cross.len();
    let mut producers = Vec::new();
    let mut consumers = Vec::new();
    let mut links = Vec::new();
    let mut related = BTreeMap::new();
    for index in reader.position.offset..total {
        if index - reader.position.offset >= reader.limit.min(250) {
            reader.next = Some(Position {
                offset: index,
                ..Default::default()
            });
            break;
        }
        if index < inbound.len() + outbound.len() {
            let (step, producer) = if index < inbound.len() {
                (&inbound[index], true)
            } else {
                (&outbound[index - inbound.len()], false)
            };
            let node = raw(reader, &step.identity)?;
            let value = traversal_entry(reader, &node, step)?;
            if !reader.retain_wire(&value)? {
                reader.next = Some(Position {
                    offset: index,
                    ..Default::default()
                });
                break;
            }
            if producer {
                producers.push(value);
            } else {
                consumers.push(value);
            }
        } else {
            let edge = cross[index - inbound.len() - outbound.len()];
            let (edge_value, next) = model::edge_page(
                edge,
                if index == reader.position.offset {
                    reader.position.edge_property.as_ref()
                } else {
                    None
                },
            )?;
            let a = raw(reader, &edge.source)?;
            let b = raw(reader, &edge.target)?;
            let na = reader.node(&a, None)?.0;
            let nb = reader.node(&b, None)?.0;
            if !reader.retain_wire(&(&edge_value, &na, &nb))? {
                reader.next = Some(Position {
                    offset: index,
                    ..Default::default()
                });
                break;
            }
            related.insert(dataset::node_key(&a), na);
            related.insert(dataset::node_key(&b), nb);
            links.push(edge_value);
            if let Some(property) = next {
                reader.next = Some(Position {
                    offset: index,
                    edge_property: Some(property),
                    ..Default::default()
                });
                break;
            }
        }
    }
    let model_node = starts
        .first()
        .map(|node| reader.node(node, None).map(|value| value.0))
        .transpose()?;
    let low = model::low_nodes(
        producers
            .iter()
            .chain(&consumers)
            .map(|entry| &entry.node)
            .chain(model_node.iter()),
    );
    let mut value = json!({
        "modelName": starts
            .first()
            .map(|n| n.node.name.as_str())
            .unwrap_or_else(|| match selection {
                CodeNodeSelection::Name { name, .. } => name.0.as_str(),
                _ => "",
            }),
        "producers": producers,
        "consumers": consumers,
        "crossRepository": links,
        "relatedNodes": related.into_values().collect::<Vec<_>>(),
        "lowConfidenceCount": low.to_string()
    });
    if let Some(count) = unmapped {
        value["unmappedInboundEdges"] = json!(count.to_string());
    }
    if let Some(model) = model_node {
        value["model"] = json!(model);
    }
    Ok(GraphResult::Lineage {
        value: model::wire(value)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(name: &str) -> GraphNodeIdentity {
        GraphNodeIdentity {
            repository_id: crate::protocol::Id::new("repo").unwrap(),
            unit_key: "src/lib.rs".into(),
            node_id: name.into(),
        }
    }

    fn edge(id: &str, source: &str, target: &str, kind: &str) -> scan::Edge {
        let source = identity(source);
        let target = identity(target);
        scan::Edge {
            raw: GraphEdge {
                repository_id: source.repository_id.clone(),
                view_id: crate::protocol::Id::new("view").unwrap(),
                generation_id: crate::protocol::Id::new("generation").unwrap(),
                content_digest: "capture".into(),
                edge: StoredEdge {
                    unit_key: source.unit_key.clone(),
                    edge_id: id.into(),
                    source_node_id: source.node_id.clone(),
                    target_node_id: target.node_id.clone(),
                    kind: kind.into(),
                    payload: json!({}),
                },
            },
            source,
            target,
        }
    }

    #[test]
    fn adjacency_walk_charges_only_followed_edges_and_expands_cycles_once() {
        let edges = [
            edge("unrelated", "x", "y", "CALLS"),
            edge("ab", "a", "b", "CALLS"),
            edge("bc", "b", "c", "CALLS"),
            edge("ca", "c", "a", "CALLS"),
            edge("self", "a", "a", "CALLS"),
            edge("other-kind", "b", "d", "USES_TYPE"),
        ];
        let mut budget = Budget::new(&crate::protocol::data::QueryLimits {
            max_results: 50,
            max_visited: 4,
            deadline_ms: 10_000,
        });
        let config = Walk {
            starts: &[],
            kinds: &["CALLS"],
            direction: Direction::Forward,
            depth: 10,
            complete: true,
        };
        let (adjacency, partial) = Adjacency::new(&edges, &[identity("a")], &mut budget);
        assert!(!partial);
        assert_eq!(budget.visited, 0);
        let mut walk = BreadthFirst::new([identity("a"), identity("a")]);
        let mut expanded = Vec::new();
        while let Some((node, depth)) = walk.next(config.depth, &mut budget) {
            walk.expand(
                &mut budget,
                &config,
                &node,
                depth,
                adjacency.around(&node, &config.direction),
            );
            expanded.push(node.node_id);
        }
        assert_eq!(expanded, ["a", "b", "c"]);
        assert_eq!(
            walk.steps
                .iter()
                .map(|step| step.edge.raw.edge.edge_id.as_str())
                .collect::<Vec<_>>(),
            ["ab", "self", "bc", "ca"]
        );
        assert_eq!(budget.visited, 4);
        assert_eq!(budget.exhausted, None);
        assert_eq!(
            adjacency
                .around_all(
                    [&identity("a"), &identity("b")],
                    &Direction::Both,
                    &mut budget
                )
                .0
                .iter()
                .map(|edge| edge.raw.edge.edge_id.as_str())
                .collect::<Vec<_>>(),
            ["ab", "bc", "ca", "self", "other-kind"]
        );
        assert_eq!(budget.visited, 4);
    }

    #[test]
    fn fallback_keeps_a_seed_edge_when_the_auxiliary_index_is_cut_short() {
        // Simulate the remaining retention after an admitted scan with a large payload per edge.
        // The single useful edge is intentionally last, after thousands of unrelated endpoints.
        let mut budget = Budget::new(&crate::protocol::data::QueryLimits {
            max_results: 50,
            max_visited: 20_000,
            deadline_ms: 10_000,
        });
        assert!(budget.retain(super::super::budget::MAX_RETAINED_BYTES - 200 * 1024));
        let mut edges: Vec<_> = (0..2_000)
            .map(|i| {
                edge(
                    &format!("edge-{i}"),
                    &format!("left-{i}"),
                    &format!("right-{i}"),
                    "CALLS",
                )
            })
            .collect();
        edges.push(edge("short-path", "start", "target", "CALLS"));
        let starts = [identity("start")];
        let (adjacency, partial) = Adjacency::new(&edges, &starts, &mut budget);
        assert!(
            partial,
            "the other endpoints exceed the auxiliary index's memory share"
        );
        assert_eq!(
            budget.exhausted, None,
            "the retained prefix is still usable"
        );
        let config = Walk {
            starts: &[],
            kinds: &[],
            direction: Direction::Forward,
            depth: 3,
            complete: true,
        };
        let mut walk = BreadthFirst::new(starts.clone());
        while let Some((node, depth)) = walk.next(config.depth, &mut budget) {
            walk.expand(
                &mut budget,
                &config,
                &node,
                depth,
                adjacency.around(&node, &config.direction),
            );
        }
        assert_eq!(walk.steps.len(), 1);
        assert_eq!(walk.steps[0].identity, identity("target"));
        let (incident, partial) = adjacency.around_all(&starts, &Direction::Both, &mut budget);
        assert!(!partial);
        assert_eq!(incident[0].raw.edge.edge_id, "short-path");
        assert_eq!(budget.visited, 1);
        assert_eq!(budget.exhausted, None);
    }
}
