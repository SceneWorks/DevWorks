//! Bounded graph neighborhood and path operators over one immutable composition.
use super::{
    dataset::{self, Key},
    model,
    read::{Position, Reader},
    scan,
};
use crate::ports::storage::*;
use crate::protocol::{
    data::{Direction, GraphResult},
    records::*,
};
use crate::services::sessions::Failure;
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
        .ok_or_else(|| crate::services::sessions::not_found("node", id))?;
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
            value: model::wire(
                json!({"node":node,"neighbors":[],"incoming":[],"outgoing":[],"crossRepository":[],"relationshipsComplete":false}),
            )?,
        });
    }
    if reader.position.edge_property.is_some() || reader.position.offset > 0 {
        reader.next = None;
    }
    let edges = reader.raw_edges()?;
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
        value: model::wire(
            json!({"node":node,"neighbors":neighbors.into_values().collect::<Vec<_>>(),"incoming":incoming,"outgoing":outgoing,"crossRepository":cross,"relationshipsComplete":reader.next.is_none()&&reader.budget.exhausted.is_none()&&!reader.gaps.contains(&"graph_visited_limit")&&reader.selected.pin.unavailable_repositories.is_empty()&&unmapped.is_none_or(|count|count==0)}),
        )?,
    })
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
            selected_node_id: selected.map(|id| model::wire(json!(id))).transpose()?,
        },
    })
}

pub(super) struct Walk<'a> {
    pub starts: &'a [GraphNode],
    pub kinds: &'a [&'a str],
    pub direction: Direction,
    pub depth: u16,
}
struct Step {
    identity: GraphNodeIdentity,
    depth: u16,
    parent: GraphNodeIdentity,
    edge: scan::Edge,
}
fn walk(reader: &mut Reader<'_>, config: Walk<'_>) -> Result<(Vec<Step>, dataset::Edges), Failure> {
    let edges = reader.filtered_edges(config.kinds)?;
    let mut queue = VecDeque::new();
    let mut visited = BTreeSet::new();
    let mut steps = Vec::new();
    for start in config.starts {
        queue.push_back((dataset::identity(start), 0));
        visited.insert((dataset::node_key(start), 0));
    }
    while let Some((current, depth)) = queue.pop_front() {
        if depth >= config.depth {
            continue;
        }
        for edge in &edges.linked {
            if !reader.budget.visit() {
                break;
            }
            if !config.kinds.is_empty() && !config.kinds.contains(&edge.raw.edge.kind.as_str()) {
                continue;
            }
            let mut targets = Vec::new();
            if !matches!(config.direction, Direction::Reverse) && edge.source == current {
                targets.push(&edge.target);
            }
            if !matches!(config.direction, Direction::Forward) && edge.target == current {
                targets.push(&edge.source);
            }
            for target in targets {
                let next = depth + 1;
                let key = (
                    dataset::key(target),
                    next,
                    dataset::key(&current),
                    edge.raw.edge.edge_id.clone(),
                );
                if !reader
                    .budget
                    .retain(std::mem::size_of::<Step>() + edge.raw.edge.payload.to_string().len())
                {
                    break;
                }
                steps.push(Step {
                    identity: target.clone(),
                    depth: next,
                    parent: current.clone(),
                    edge: edge.clone(),
                });
                if visited.insert((key.0, next)) {
                    queue.push_back((target.clone(), next));
                }
            }
        }
        if reader.budget.exhausted.is_some() {
            break;
        }
    }
    Ok((steps, edges))
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
        let entry = model::wire(
            json!({"node":reader.node(&node,None)?.0,"depth":step.depth.to_string(),"parentId":step.parent.node_id,"edgeKind":step.edge.raw.edge.kind,"edgeId":step.edge.raw.edge.edge_id}),
        )?;
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
    let (steps, all) = walk(
        reader,
        Walk {
            starts: &seeds,
            kinds: &kinds,
            direction,
            depth,
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
    for (index, step) in steps.iter().enumerate().skip(reader.position.offset) {
        if edges.len() >= reader.limit.min(250) {
            reader.next = Some(Position {
                offset: index,
                ..Default::default()
            });
            break;
        }
        if !seen.insert((
            step.edge.raw.repository_id.as_str().to_owned(),
            step.edge.raw.edge.unit_key.clone(),
            step.edge.raw.edge.edge_id.clone(),
        )) {
            continue;
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
    let (steps, all) = walk(
        reader,
        Walk {
            starts: &starts,
            kinds: &["CALLS", "HTTP_CALLS"],
            direction,
            depth: selection.depth.get() as u16,
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
            value: model::wire(json!({"nodes":[],"edges":[],"lowConfidenceCount":"0"}))?,
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
    let edges = reader.filtered_edges(kinds)?;
    let unmapped = super::unmapped::count(
        reader,
        &edges.unmapped,
        &super::unmapped::identities(&starts),
        Some(selection),
        kinds,
    )?;
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    for edge in &edges.linked {
        if keys.contains(&dataset::key(&edge.target))
            && kinds.contains(&edge.raw.edge.kind.as_str())
            && seen.insert(dataset::key(&edge.source))
        {
            let node = raw(reader, &edge.source)?;
            rows.push((node, edge));
        }
    }
    let mut result = Vec::new();
    for (index, (node, edge)) in rows.iter().enumerate().skip(reader.position.offset) {
        if result.len() >= reader.limit {
            reader.next = Some(Position {
                offset: index,
                ..Default::default()
            });
            break;
        }
        let item = model::wire(
            json!({"node":reader.node(node,None)?.0,"edgeKind":edge.raw.edge.kind,"lowConfidence":dataset::low_node(node)}),
        )?;
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
        },
    )?;
    let (outbound, all) = walk(
        reader,
        Walk {
            starts: &starts,
            kinds,
            direction: Direction::Forward,
            depth: 4,
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
            let value: CodeTraversalEntry = model::wire(
                json!({"node":reader.node(&node,None)?.0,"depth":step.depth.to_string(),"parentId":step.parent.node_id,"edgeKind":step.edge.raw.edge.kind,"edgeId":step.edge.raw.edge.edge_id}),
            )?;
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
    let mut value = json!({"modelName":starts.first().map(|n|n.node.name.as_str()).unwrap_or_else(||match selection{CodeNodeSelection::Name{name,..}=>name.0.as_str(),_=>""}),"producers":producers,"consumers":consumers,"crossRepository":links,"relatedNodes":related.into_values().collect::<Vec<_>>(),"lowConfidenceCount":low.to_string()});
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
