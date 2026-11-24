//! Bounded, whole-unit-masked graph iteration. Expansion never selects another repository.
use super::budget::Budget;
use crate::ports::storage::*;
use crate::services::indexing::views::ViewLayers;
use crate::services::kernel::Failure;

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
    Ok(if ambiguous || budget.exhausted.is_some() {
        None
    } else {
        found
    })
}

fn qualified_endpoint(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
    repository: &crate::protocol::Id,
    payload: &serde_json::Value,
    budget: &mut Budget,
) -> Result<Option<GraphNodeIdentity>, Failure> {
    let local = GraphScope {
        views: scope
            .views
            .iter()
            .filter(|view| &view.repository_id == repository)
            .cloned()
            .collect(),
    };
    if local.views.is_empty() {
        return Ok(None);
    }
    let Some(qualified) = payload["targetQualifiedName"].as_str() else {
        return Ok(None);
    };
    if payload
        .get("targetKind")
        .is_some_and(|kind| !kind.is_string())
    {
        return Ok(None);
    }
    let filter = GraphNodeFilter {
        unit_key: payload["targetUnit"].as_str().map(str::to_owned),
        kinds: payload["targetKind"]
            .as_str()
            .map(|kind| vec![kind.to_owned()])
            .unwrap_or_default(),
        ..Default::default()
    };
    let mut found = None;
    let mut ambiguous = false;
    nodes(read, &local, &filter, budget, |candidate, _| {
        if candidate.node.payload["qualifiedName"].as_str() != Some(qualified) {
            return Ok(true);
        }
        if found.is_some() {
            ambiguous = true;
            return Ok(false);
        }
        found = Some(super::dataset::identity(&candidate));
        Ok(true)
    })?;
    Ok(if ambiguous || budget.exhausted.is_some() {
        None
    } else {
        found
    })
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

/// The stored edge kinds a read of `kinds` needs. The package reference is the retained source
/// of a derived dependency, not an endpoint filter: keep it whenever that derived relationship is
/// requested.
pub(super) fn stored_kinds(kinds: &[&str]) -> Vec<String> {
    let mut stored = kinds
        .iter()
        .map(|kind| (*kind).to_owned())
        .collect::<Vec<_>>();
    if kinds.contains(&"DEPENDS_ON") && !kinds.contains(&"REFERENCES_PACKAGE") {
        stored.push("REFERENCES_PACKAGE".into());
    }
    stored
}

/// One composed edge as the graph reads it.
pub(super) enum Resolved {
    /// Its view is not one of the selected layers.
    Skip,
    /// The budget ran out while relinking it.
    Stop,
    Edge(Result<Edge, Unmapped>),
}

type Linkers = std::collections::HashMap<String, crate::services::indexing::worker::ViewLinker>;

/// Resolves both endpoints of one composed edge inside `scope`. With `linkers`, an overlay
/// view's inherited edge is first relinked against that view; without, `raw` is already linked
/// (a stored view link).
pub(super) fn resolve(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
    layers: &[ViewLayers],
    linkers: Option<&mut Linkers>,
    mut raw: GraphEdge,
    budget: &mut Budget,
) -> Result<Resolved, Failure> {
    let Some(view) = layers.iter().find(|v| v.view.view_id == raw.view_id) else {
        return Ok(Resolved::Skip);
    };
    let original = raw.clone();
    if let Some(linkers) = linkers
        && view.layers.len() > 1
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
            linkers
                .entry(view.view.view_id.as_str().to_owned())
                .or_default(),
            &unit_key,
            vec![raw.edge],
            budget,
        )?;
        let Some(linked) = linked else {
            return Ok(Resolved::Stop);
        };
        let Some(edge) = linked.into_iter().next() else {
            return Ok(Resolved::Skip);
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
        return Ok(Resolved::Edge(Err(Unmapped { raw, original })));
    };
    if ["sourceUnit", "targetUnit"]
        .iter()
        .any(|key| payload.get(*key).is_some_and(|value| !value.is_string()))
    {
        return Ok(Resolved::Edge(Err(Unmapped { raw, original })));
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
    let target = if target_repo != raw.repository_id && payload.get("targetQualifiedName").is_some()
    {
        qualified_endpoint(read, scope, &target_repo, payload, budget)?
    } else {
        endpoint(
            read,
            scope,
            &target_repo,
            payload
                .get("targetUnit")
                .and_then(serde_json::Value::as_str),
            &raw.edge.target_node_id,
            budget,
        )?
    };
    Ok(Resolved::Edge(match (source, target) {
        (Some(source), Some(target)) => Ok(Edge {
            raw,
            source,
            target,
        }),
        _ => Err(Unmapped { raw, original }),
    }))
}

/// Raw edges are streamed before endpoint filtering: inherited names may have rebound in
/// this composition, and SQL's old target index must not hide those incoming references.
pub(super) fn edges(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
    layers: &[ViewLayers],
    kinds: &[&str],
    budget: &mut Budget,
    mut visit: impl FnMut(Result<Edge, Unmapped>, &mut Budget) -> Result<bool, Failure>,
) -> Result<(), Failure> {
    let mut after = None;
    let mut packages = None;
    let mut declared = std::collections::BTreeSet::new();
    let mut pinned = Vec::new();
    let mut derived_seen = std::collections::BTreeSet::new();
    let mut linkers = Linkers::new();
    let filter = GraphEdgeFilter {
        kinds: stored_kinds(kinds),
        ..Default::default()
    };
    loop {
        let page = read
            .graph()
            .graph_edges(scope, &filter, after.as_ref(), READ_PAGE)?;
        let more = page.has_more;
        for raw in page.items {
            if !budget.visit() {
                return Ok(());
            }
            after = Some(GraphPosition {
                repository_id: raw.repository_id.clone(),
                unit_key: raw.edge.unit_key.clone(),
                item_id: raw.edge.edge_id.clone(),
            });
            let linked = match resolve(read, scope, layers, Some(&mut linkers), raw, budget)? {
                Resolved::Skip => continue,
                Resolved::Stop => return Ok(()),
                Resolved::Edge(Err(unmapped)) => {
                    if !visit(Err(unmapped), budget)? {
                        return Ok(());
                    }
                    continue;
                }
                Resolved::Edge(Ok(edge)) => Ok::<_, Unmapped>(edge),
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

/// Stream the graph's stored relationships and the analysis linker's exact-view derivations.
pub(super) fn composed_edges(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
    layers: &[ViewLayers],
    kinds: &[&str],
    budget: &mut Budget,
    mut visit: impl FnMut(Result<Edge, Unmapped>, &mut Budget) -> Result<bool, Failure>,
) -> Result<(), Failure> {
    let mut cross_repository = std::collections::BTreeSet::new();
    let mut stopped = false;
    let mut derive = false;
    edges(read, scope, layers, kinds, budget, |edge, budget| {
        derive |= scope.views.len() > 1 && super::repository_analysis::links::can_derive(&edge);
        if let Ok(edge) = &edge
            && edge.source.repository_id != edge.target.repository_id
            && matches!(
                edge.raw.edge.kind.as_str(),
                "HTTP_CALLS"
                    | "PUBLISHES"
                    | "CALLS"
                    | "USES_TYPE"
                    | "IMPLEMENTS"
                    | "RENDERS"
                    | "INJECTS"
                    | "REFERENCES_PACKAGE"
            )
            && scope.views.len() > 1
        {
            let bytes = edge.source.unit_key.len()
                + edge.target.unit_key.len()
                + edge.source.node_id.len()
                + edge.target.node_id.len()
                + edge.raw.edge.kind.len()
                + 192;
            if !budget.retain(bytes) {
                return Ok(false);
            }
            cross_repository.insert((
                super::dataset::key(&edge.source),
                super::dataset::key(&edge.target),
                edge.raw.edge.kind.clone(),
            ));
        }
        stopped = !visit(edge, budget)?;
        Ok(!stopped)
    })?;
    if !derive || stopped || budget.exhausted.is_some() {
        return Ok(());
    }
    for edge in super::repository_analysis::links::graph_edges(
        read,
        scope,
        layers,
        kinds,
        budget,
        cross_repository,
    )? {
        if !visit(Ok(edge), budget)? {
            break;
        }
    }
    Ok(())
}

/// Every visible edge of `scope` as the graph operators read it today, an overlay view's
/// inherited edges relinked against that view; derived package dependencies are left out. The
/// reference the stored endpoint reads are checked against.
#[doc(hidden)]
pub fn scanned_edges(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
    layers: &[ViewLayers],
) -> Result<Vec<GraphEdge>, Failure> {
    let mut budget = Budget::new(&crate::protocol::data::QueryLimits {
        max_results: 500,
        max_visited: 1_000_000,
        deadline_ms: 300_000,
    });
    let mut out = Vec::new();
    edges(read, scope, layers, &[], &mut budget, |edge, _| {
        let raw = match edge {
            Ok(edge) => edge.raw,
            Err(unmapped) => unmapped.raw,
        };
        // A derived package dependency is not a stored edge (see `packages`).
        if !raw.edge.edge_id.starts_with("derived-") {
            out.push(raw);
        }
        Ok(true)
    })?;
    if let Some(reason) = budget.exhausted {
        return Err(StorageError::InvalidRecord(reason.into()).into());
    }
    Ok(out)
}
