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

#[derive(Clone)]
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
    let mut packages = None;
    let mut declared = std::collections::BTreeSet::new();
    let mut pinned = Vec::new();
    let mut derived_seen = std::collections::BTreeSet::new();
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
            if view.layers.len() > 1
                && raw
                    .edge
                    .payload
                    .get("targetRepositoryId")
                    .is_none_or(|id| id.as_str() == Some(raw.repository_id.as_str()))
            {
                // The shared index linker retains language/receiver/import ambiguity rules.
                let unit_key = raw.edge.unit_key.clone();
                let linked = super::linking::link(
                    read,
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
            let derived = if let Ok(edge) = &linked {
                if edge.raw.edge.kind == "REFERENCES_PACKAGE" {
                    if packages.is_none() {
                        packages = Some(super::packages::Packages::load(read, scope, budget)?);
                    }
                    packages
                        .as_ref()
                        .expect("loaded packages")
                        .link(read, scope, edge, budget)?
                } else {
                    None
                }
            } else {
                None
            };
            if !visit(linked, budget)? {
                return Ok(());
            }
            if let Some(derived) = derived {
                let identity = (
                    super::dataset::key(&derived.source),
                    super::dataset::key(&derived.target),
                );
                if derived_seen.contains(&identity) {
                    continue;
                }
                if !budget.retain(
                    identity.0.0.len()
                        + identity.0.1.len()
                        + identity.0.2.len()
                        + identity.1.0.len()
                        + identity.1.1.len()
                        + identity.1.2.len()
                        + 128,
                ) {
                    return Ok(());
                }
                derived_seen.insert(identity);
                let props = &derived.raw.edge.payload["properties"];
                if props["ecosystem"].as_str() == Some("swiftpm") {
                    let key = (
                        derived.source.repository_id.as_str().to_owned(),
                        super::dataset::key(&derived.target),
                    );
                    if props["dependency_kind"].as_str() == Some("pinned") {
                        if !budget.retain(
                            std::mem::size_of::<Edge>()
                                + derived.raw.edge.payload.to_string().len()
                                + derived.source.unit_key.len()
                                + derived.target.unit_key.len()
                                + derived.source.node_id.len()
                                + derived.target.node_id.len(),
                        ) {
                            return Ok(());
                        }
                        pinned.push((key, derived));
                        continue;
                    }
                    if !budget
                        .retain(key.0.len() + key.1.0.len() + key.1.1.len() + key.1.2.len() + 64)
                    {
                        return Ok(());
                    }
                    declared.insert(key);
                }
                if !visit(Ok(derived), budget)? {
                    return Ok(());
                }
            }
        }
        if !more {
            break;
        }
    }
    // A pin is a fallback logical dependency only when no selected declaration links it.
    // A cut scan returns above, so unseen declarations can never be treated as absent.
    for (key, edge) in pinned {
        if !declared.contains(&key) && !visit(Ok(edge), budget)? {
            break;
        }
    }
    Ok(())
}
