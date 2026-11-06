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
    let edges = reader.raw_edges()?;
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
    let mut emitted = 0;
    for (index, edge) in incident.iter().enumerate().skip(offset) {
        if emitted >= reader.limit.min(249) {
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
        let projected_edge = model::edge(edge)?;
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
        emitted += 1;
    }
    Ok(GraphResult::NodeDetail {
        value: model::wire(
            json!({"node":node,"neighbors":neighbors.into_values().collect::<Vec<_>>(),"incoming":incoming,"outgoing":outgoing,"crossRepository":cross,"relationshipsComplete":reader.next.is_none()&&reader.budget.exhausted.is_none()&&reader.selected.pin.unavailable_repositories.is_empty()}),
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
                std::cmp::Reverse(match n.node.kind.as_str() {
                    "Method" => 0,
                    "Class" => 1,
                    _ => 2,
                }),
                n.node.node_id.clone(),
            )
        };
        span(a).cmp(&span(b))
    });
    if let Some(line) = &selection.line {
        let line = line
            .0
            .parse::<u64>()
            .map_err(|_| crate::ports::storage::StorageError::CursorExpired)?;
        let containing: Vec<_> = rows
            .iter()
            .filter(|n| {
                n.node.payload["span"]["startLine"]
                    .as_u64()
                    .is_some_and(|s| s <= line)
                    && n.node.payload["span"]["endLine"]
                        .as_u64()
                        .is_some_and(|e| line <= e)
            })
            .cloned()
            .collect();
        rows = if containing.is_empty() {
            rows.into_iter()
                .filter(|n| n.node.kind == "File")
                .take(1)
                .collect()
        } else {
            containing
        };
    }
    let nodes = reader.page_nodes(&rows)?;
    Ok(GraphResult::CodeNodes {
        value: model::wire(json!({"nodes":nodes}))?,
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
    let edges = reader.raw_edges()?;
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
    let (steps, _) = walk(
        reader,
        Walk {
            starts: &seeds,
            kinds: &kinds,
            direction,
            depth,
        },
    )?;
    graph_steps(reader, &steps)
}
fn graph_steps(reader: &mut Reader<'_>, steps: &[Step]) -> Result<GraphResult, Failure> {
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
        let edge = model::edge(&step.edge)?;
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
    }
    let low = edges
        .iter()
        .filter(|e| e.confidence.as_ref().is_some_and(|c| c.get() < 0.5))
        .count();
    Ok(GraphResult::CodeGraph {
        value: model::wire(
            json!({"nodes":nodes.into_values().collect::<Vec<_>>(),"edges":edges,"lowConfidenceCount":low.to_string()}),
        )?,
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
    let (steps, _) = walk(
        reader,
        Walk {
            starts: &starts,
            kinds: &["CALLS", "HTTP_CALLS"],
            direction,
            depth: selection.depth.get() as u16,
        },
    )?;
    let entries = entries(reader, &steps)?;
    let low = entries
        .iter()
        .filter(|e| {
            e.node
                .confidence
                .as_ref()
                .is_some_and(|v| v.0.eq_ignore_ascii_case("low"))
        })
        .count();
    let start = starts
        .first()
        .map(|n| reader.node(n, None).map(|n| n.0))
        .transpose()?;
    Ok(GraphResult::CallTrace {
        value: model::wire(
            json!({"start":start,"entries":entries,"lowConfidenceCount":low.to_string()}),
        )?,
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
        return graph_steps(reader, &[]);
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
    graph_steps(reader, &path)
}

pub(super) fn related(
    reader: &mut Reader<'_>,
    selection: &CodeNodeSelection,
    publishers: bool,
) -> Result<GraphResult, Failure> {
    let starts = reader.targets(selection)?;
    let keys: BTreeSet<_> = starts.iter().map(dataset::node_key).collect();
    let edges = reader.raw_edges()?;
    let kinds = if publishers {
        &["PUBLISHES", "CALLS"][..]
    } else {
        &["CONSUMES", "CALLS", "HTTP_CALLS", "USES_TYPE", "HANDLES"][..]
    };
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
            json!({"node":reader.node(node,None)?.0,"edgeKind":edge.raw.edge.kind,"lowConfidence":dataset::low_edge(edge)}),
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
    Ok(GraphResult::Related {
        value: model::wire(json!({"nodes":result,"lowConfidenceCount":low.to_string()}))?,
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
    // One outer offset covers both directions and all exact cross-repository edges.
    let mut combined = inbound;
    let split = combined.len();
    combined.extend(outbound);
    let entries = entries(reader, &combined)?;
    let offset = reader.position.offset;
    let producer_count = split.saturating_sub(offset).min(entries.len());
    let producers = entries[..producer_count].to_vec();
    let consumers = entries[producer_count..].to_vec();
    let model_node = starts
        .first()
        .map(|n| reader.node(n, None).map(|n| n.0))
        .transpose()?;
    let name = starts
        .first()
        .map(|n| n.node.name.clone())
        .unwrap_or_default();
    let mut cross = Vec::new();
    let mut related = BTreeMap::new();
    if let Some(start) = starts.first() {
        for edge in all.linked.iter().filter(|e| {
            e.source.repository_id != e.target.repository_id
                && (e.source.repository_id == start.repository_id
                    || e.target.repository_id == start.repository_id)
        }) {
            if cross.len() >= 249 {
                reader.gaps.push("lineage_cross_repository_limit");
                break;
            }
            let a = raw(reader, &edge.source)?;
            let b = raw(reader, &edge.target)?;
            let projected = model::edge(edge)?;
            let na = reader.node(&a, None)?.0;
            let nb = reader.node(&b, None)?.0;
            if !reader.retain_wire(&(&projected, &na, &nb))? {
                reader.gaps.push("lineage_cross_repository_limit");
                break;
            }
            related.insert(dataset::node_key(&a), na);
            related.insert(dataset::node_key(&b), nb);
            cross.push(projected);
        }
    }
    let low = producers
        .iter()
        .chain(&consumers)
        .filter(|e| {
            e.node
                .confidence
                .as_ref()
                .is_some_and(|v| v.0.eq_ignore_ascii_case("low"))
        })
        .count();
    Ok(GraphResult::Lineage {
        value: model::wire(
            json!({"modelName":name,"model":model_node,"producers":producers,"consumers":consumers,"crossRepository":cross,"relatedNodes":related.into_values().collect::<Vec<_>>(),"lowConfidenceCount":low.to_string()}),
        )?,
    })
}
