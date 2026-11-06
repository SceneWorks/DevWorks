//! Retained dependency impact. All identities and expansion remain inside one captured view set.
use super::{
    dataset::{self, Key},
    model,
    read::{Position, Reader},
    scan,
};
use crate::ports::storage::*;
use crate::protocol::Id;
use crate::protocol::data::GraphResult;
use crate::protocol::records::{
    CodeAffectedNode, CodeImpact, CodeImpactSummary, CodeNodeSelection, CodeRepositoryLink,
    CodeRisk,
};
use crate::services::sessions::Failure;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

const DEPENDENCIES: &[&str] = &[
    "CALLS",
    "HTTP_CALLS",
    "CONSUMES",
    "USES_TYPE",
    "INJECTS",
    "REGISTERS",
    "IMPLEMENTS",
    "INHERITS",
    "HANDLES",
    "REFERENCES_PACKAGE",
    "RENDERS",
    "SUBSCRIBES",
    "DEFINES_METHOD",
    "DEFINES",
];
const PREFERRED: &[&str] = &[
    "Method",
    "Function",
    "Class",
    "Interface",
    "Service",
    "Event",
    "Route",
];
const LOW_CONFIDENCE: &str =
    "Low-confidence node (tree-sitter fallback; relationships may be incomplete)";

struct Reached {
    raw: GraphNode,
    depth: u16,
    edge: String,
    origin: Id,
    risk: usize,
    factors: Vec<String>,
}

type Links = BTreeMap<(String, String), (String, u64)>;

pub(super) fn node_impact(
    reader: &mut Reader<'_>,
    selection: &CodeNodeSelection,
    depth: u16,
) -> Result<GraphResult, Failure> {
    let mut starts = reader.targets(selection)?;
    if let CodeNodeSelection::Name {
        name,
        repository_id,
    } = selection
    {
        if repository_id.is_some()
            && starts
                .iter()
                .any(|n| n.node.payload["qualifiedName"].as_str() == Some(&name.0))
        {
            starts.retain(|n| n.node.payload["qualifiedName"].as_str() == Some(&name.0));
        } else {
            if repository_id.is_none() {
                starts.truncate(10);
            }
            if starts
                .iter()
                .any(|n| PREFERRED.contains(&n.node.kind.as_str()))
            {
                starts.retain(|n| PREFERRED.contains(&n.node.kind.as_str()));
            } else {
                starts.truncate(1);
            }
        }
    }
    build(reader, starts, depth.clamp(1, 5), None, selection)
}

pub(super) fn file_impact(
    reader: &mut Reader<'_>,
    repository: &Id,
    path: &str,
    depth: u16,
) -> Result<GraphResult, Failure> {
    let scope = reader.scoped_repository(repository)?;
    let mut starts = reader.raw_nodes(
        &scope,
        &GraphNodeFilter {
            unit_key: Some(path.into()),
            ..Default::default()
        },
    )?;
    if starts
        .iter()
        .any(|n| !matches!(n.node.kind.as_str(), "File" | "Folder" | "Namespace"))
    {
        starts.retain(|n| !matches!(n.node.kind.as_str(), "File" | "Folder" | "Namespace"));
    }
    build(
        reader,
        starts,
        depth.clamp(1, 5),
        Some((repository, path)),
        &CodeNodeSelection::Name {
            name: model::wire(json!(path))?,
            repository_id: Some(repository.clone()),
        },
    )
}

fn raw_node(
    reader: &mut Reader<'_>,
    identity: &GraphNodeIdentity,
) -> Result<Option<GraphNode>, Failure> {
    if !reader.budget.visit() {
        return Ok(None);
    }
    let raw = reader.read.graph().graph_node(reader.scope(), identity)?;
    if let Some(raw) = &raw {
        let bytes = std::mem::size_of::<Reached>()
            + raw.node.payload.to_string().len()
            + raw.node.name.len()
            + raw.node.unit_key.len()
            + raw.node.node_id.len()
            + 1024;
        if !reader.budget.retain(bytes) {
            return Ok(None);
        }
    }
    Ok(raw)
}

/// Structural file/folder containers never inherit the dependency risk of their contents.
fn local_dependency(edge: &str, source: &GraphNode) -> bool {
    DEPENDENCIES.contains(&edge)
        && match edge {
            "DEFINES" => matches!(
                source.node.kind.as_str(),
                "Function" | "Method" | "Component"
            ),
            "DEFINES_METHOD" => matches!(
                source.node.kind.as_str(),
                "Function"
                    | "Method"
                    | "Constructor"
                    | "Delegate"
                    | "Component"
                    | "Class"
                    | "Interface"
                    | "Struct"
                    | "Record"
                    | "Enum"
            ),
            _ => true,
        }
}

fn classify(
    raw: &GraphNode,
    depth: u16,
    edge: &str,
    origin: &Id,
    module: Option<&str>,
) -> (usize, Vec<String>) {
    let mut score: i32 = match depth {
        1 => 1,
        2 => 2,
        _ => 3,
    };
    let mut factors = Vec::new();
    let cross = &raw.repository_id != origin;
    if cross {
        score -= 1;
        factors.push("Cross-repo dependency (harder to detect breakage)".into());
    }
    if matches!(edge, "CONSUMES" | "PUBLISHES") {
        score -= 1;
        factors.push(format!("Async edge ({edge}) — silent failures"));
    }
    if depth == 1
        && cross
        && matches!(
            edge,
            "HTTP_CALLS" | "CONSUMES" | "PUBLISHES" | "REFERENCES_PACKAGE"
        )
    {
        score = 0;
        factors.push("Direct cross-repo consumer".into());
    }
    let path = raw.node.unit_key.to_lowercase();
    if path.contains("test")
        || path.contains("spec")
        || module.is_some_and(|m| m.to_lowercase().contains("test"))
    {
        score += 1;
        factors.push("Test code (caught by CI)".into());
    }
    if factors.is_empty() {
        factors.push(format!("Hop {depth} dependent"));
    }
    if dataset::low_node(raw) {
        factors.push(LOW_CONFIDENCE.into());
    }
    (score.clamp(0, 3) as usize, factors)
}

fn kind(risk: usize) -> CodeRisk {
    match risk {
        0 => CodeRisk::Critical,
        1 => CodeRisk::High,
        2 => CodeRisk::Medium,
        _ => CodeRisk::Low,
    }
}

fn add_link(links: &mut Links, source: String, target: String, edge: &str, count: u64) {
    let value = links
        .entry((source, target))
        .or_insert_with(|| (edge.into(), 0));
    value.1 += count;
}

fn build(
    reader: &mut Reader<'_>,
    starts: Vec<GraphNode>,
    depth: u16,
    file: Option<(&Id, &str)>,
    selection: &CodeNodeSelection,
) -> Result<GraphResult, Failure> {
    let edges = reader.raw_edges()?;
    if !edges.unmapped.is_empty() {
        reader.gaps.push("unresolved_impact_edges");
    }
    let start_keys: BTreeSet<Key> = starts.iter().map(dataset::node_key).collect();
    let start_repositories: BTreeSet<String> = starts
        .iter()
        .map(|n| n.repository_id.as_str().to_owned())
        .collect();
    let mut reached = BTreeMap::<Key, Reached>::new();
    for raw in starts {
        reached.insert(
            dataset::node_key(&raw),
            Reached {
                origin: raw.repository_id.clone(),
                raw,
                depth: 0,
                edge: "CHANGED".into(),
                risk: 0,
                factors: vec!["Changed node".into()],
            },
        );
    }
    // Breadth-first expansion carries the same dependency origin through each local segment;
    // crossing a repository boundary starts a new segment at the depended-upon repository.
    for wave in 1..=depth {
        let mut candidates = BTreeMap::<Key, (GraphNode, String, Id)>::new();
        for edge in &edges.linked {
            if !reader.budget.visit() {
                break;
            }
            let cross = edge.source.repository_id != edge.target.repository_id;
            let (dependency, dependent) = if cross && edge.raw.edge.kind == "PUBLISHES" {
                (&edge.source, &edge.target)
            } else {
                (&edge.target, &edge.source)
            };
            let Some(prior) = reached
                .get(&dataset::key(dependency))
                .filter(|n| n.depth == wave - 1)
            else {
                continue;
            };
            let key = dataset::key(dependent);
            if reached.contains_key(&key) || candidates.contains_key(&key) {
                continue;
            }
            if !cross && !DEPENDENCIES.contains(&edge.raw.edge.kind.as_str()) {
                continue;
            }
            if !reader
                .budget
                .retain(key.0.len() + key.1.len() + key.2.len() + std::mem::size_of::<Reached>())
            {
                break;
            }
            let Some(raw) = raw_node(reader, dependent)? else {
                continue;
            };
            if !cross && !local_dependency(&edge.raw.edge.kind, &raw) {
                continue;
            }
            candidates.insert(
                key,
                (
                    raw,
                    edge.raw.edge.kind.clone(),
                    if cross {
                        dependency.repository_id.clone()
                    } else {
                        prior.origin.clone()
                    },
                ),
            );
        }
        if candidates.is_empty() {
            break;
        }
        for (key, (raw, edge, origin)) in candidates {
            let module = reader.internal_module(&raw)?;
            let (risk, factors) = classify(&raw, wave, &edge, &origin, module.as_deref());
            reached.insert(
                key,
                Reached {
                    raw,
                    depth: wave,
                    edge,
                    origin,
                    risk,
                    factors,
                },
            );
        }
    }
    let mut links = Links::new();
    let mut direct = BTreeSet::<String>::new();
    for edge in &edges.linked {
        if !reader.budget.visit() {
            break;
        }
        if edge.source.repository_id == edge.target.repository_id {
            continue;
        }
        let (dependency, dependent) = if edge.raw.edge.kind == "PUBLISHES" {
            (&edge.source, &edge.target)
        } else {
            (&edge.target, &edge.source)
        };
        if start_keys.contains(&dataset::key(dependency)) {
            direct.insert(dependent.repository_id.as_str().to_owned());
            add_link(
                &mut links,
                edge.source.repository_id.as_str().to_owned(),
                edge.target.repository_id.as_str().to_owned(),
                &edge.raw.edge.kind,
                1,
            );
        }
    }
    if let Some(start) = start_repositories.first() {
        for reached in reached.values().filter(|n| {
            n.depth > 0
                && !start_repositories.contains(n.raw.repository_id.as_str())
                && !direct.contains(n.raw.repository_id.as_str())
        }) {
            add_link(
                &mut links,
                start.clone(),
                reached.raw.repository_id.as_str().to_owned(),
                "TRANSITIVE",
                1,
            );
        }
    }
    if let Some((repository, path)) = file.filter(|_| !start_keys.is_empty()) {
        crate_dependents(reader, repository, path, depth, &edges.linked, &mut links)?;
    }
    let mut counts = [0u64; 4];
    let mut low = 0;
    let mut repositories = BTreeSet::new();
    for node in reached.values() {
        repositories.insert(node.raw.repository_id.as_str().to_owned());
        if dataset::low_node(&node.raw) {
            low += 1;
        }
        if node.depth > 0 {
            counts[node.risk] += 1;
        }
    }
    let cross_count = links
        .keys()
        .flat_map(|(s, t)| [s, t])
        .filter(|r| !start_repositories.contains(*r))
        .collect::<BTreeSet<_>>()
        .len();
    let unmapped = unmapped_count(reader, &edges.unmapped, &reached, file, selection);
    let mut changed = Vec::new();
    let mut affected = Vec::new();
    let mut cross_repository = Vec::new();
    let mut offset = 0;
    let mut emitted = 0;
    for changed_phase in [true, false] {
        for n in reached.values().filter(|n| (n.depth == 0) == changed_phase) {
            if offset < reader.position.offset {
                offset += 1;
                continue;
            }
            if emitted == reader.limit {
                reader.next = Some(Position {
                    offset,
                    ..Default::default()
                });
                break;
            }
            let (node, _) = reader.node(&n.raw, None)?;
            let mut factors = n.factors.clone();
            if n.depth == 0 && dataset::low_node(&n.raw) {
                factors.push(LOW_CONFIDENCE.into());
            }
            let value: CodeAffectedNode = model::wire(
                json!({"node":node,"depth":n.depth.to_string(),"edgeKind":n.edge,"risk":kind(n.risk),"riskFactors":factors,"lowConfidence":dataset::low_node(&n.raw)}),
            )?;
            if !reader.retain_wire(&value)? {
                reader.next = Some(Position {
                    offset,
                    ..Default::default()
                });
                break;
            }
            if changed_phase {
                changed.push(value)
            } else {
                affected.push(value)
            };
            emitted += 1;
            offset += 1;
        }
        if reader.next.is_some() {
            break;
        }
    }
    if reader.next.is_none() {
        for ((source, target), (edge, count)) in &links {
            if offset < reader.position.offset {
                offset += 1;
                continue;
            }
            if emitted == reader.limit {
                reader.next = Some(Position {
                    offset,
                    ..Default::default()
                });
                break;
            }
            let value: CodeRepositoryLink = model::wire(
                json!({"sourceRepositoryId":source,"targetRepositoryId":target,"edgeKind":edge,"count":count.to_string()}),
            )?;
            if !reader.retain_wire(&value)? {
                reader.next = Some(Position {
                    offset,
                    ..Default::default()
                });
                break;
            }
            cross_repository.push(value);
            emitted += 1;
            offset += 1;
        }
    }
    if reader.position.offset > reached.len() + links.len() {
        return Err(StorageError::CursorExpired.into());
    }
    if reader.budget.exhausted.is_some()
        || !edges.unmapped.is_empty()
        || !reader.selected.pin.unavailable_repositories.is_empty()
    {
        reader.gaps.push("impact_counts_incomplete");
    }
    let (language, language_indexed) = match file {
        Some((_, path)) => {
            let (language, indexed) = reader.file_language(path);
            (
                language.map(|v| model::wire(json!(v))).transpose()?,
                Some(indexed),
            )
        }
        None => (None, None),
    };
    Ok(GraphResult::Impact {
        value: CodeImpact {
            found: !start_keys.is_empty(),
            path: file.map(|(_, p)| model::wire(json!(p))).transpose()?,
            language,
            language_indexed,
            blast_radius: counts
                .iter()
                .position(|n| *n > 0)
                .map(kind)
                .unwrap_or(CodeRisk::None),
            changed: model::wire(json!(changed))?,
            affected: model::wire(json!(affected))?,
            cross_repository: model::wire(json!(cross_repository))?,
            summary: CodeImpactSummary {
                total_affected: model::count(counts.iter().sum())?,
                cross_repository_count: model::count(cross_count as u64)?,
                critical_count: model::count(counts[0])?,
                high_count: model::count(counts[1])?,
                medium_count: model::count(counts[2])?,
                low_count: model::count(counts[3])?,
                affected_repository_ids: model::wire(json!(repositories))?,
                low_confidence_count: model::count(low)?,
            },
            unmapped_inbound_edges: reader
                .selected
                .layers
                .iter()
                .any(|l| l.layers.len() > 1)
                .then(|| model::count(unmapped))
                .transpose()?,
        },
    })
}

fn crate_dependents(
    reader: &mut Reader<'_>,
    repository: &Id,
    path: &str,
    depth: u16,
    edges: &[scan::Edge],
    links: &mut Links,
) -> Result<(), Failure> {
    let manifest = if path.to_lowercase().ends_with(".rs") {
        "Cargo.toml"
    } else if path.to_lowercase().ends_with(".swift") {
        "Package.swift"
    } else {
        return Ok(());
    };
    let scope = reader.scoped_repository(repository)?;
    let mut directory = path.rsplit_once('/').map_or("", |(p, _)| p);
    let owner = loop {
        let path = if directory.is_empty() {
            manifest.into()
        } else {
            format!("{directory}/{manifest}")
        };
        let rows = reader.raw_nodes(
            &scope,
            &GraphNodeFilter {
                unit_key: Some(path),
                kinds: vec!["Package".into()],
                ..Default::default()
            },
        )?;
        let owner = rows.into_iter().find(|n| {
            let v = &n.node.payload["properties"]["is_definition"];
            v == true || v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("true"))
        });
        if owner.is_some() || directory.is_empty() || reader.budget.exhausted.is_some() {
            break owner;
        }
        directory = directory.rsplit_once('/').map_or("", |(p, _)| p);
    };
    let Some(owner) = owner else { return Ok(()) };
    let mut visited = BTreeSet::from([dataset::node_key(&owner)]);
    let mut frontier = visited.clone();
    let mut dependents = BTreeSet::new();
    for _ in 0..depth {
        let mut next = BTreeSet::new();
        for edge in edges {
            if !reader.budget.visit() {
                break;
            }
            if edge.raw.edge.kind != "REFERENCES_PACKAGE"
                || !frontier.contains(&dataset::key(&edge.target))
            {
                continue;
            }
            if edge.source.repository_id != edge.target.repository_id {
                dependents.insert((dataset::key(&edge.source), dataset::key(&edge.target)));
            }
            let key = dataset::key(&edge.source);
            if !visited.contains(&key)
                && reader
                    .budget
                    .retain(key.0.len() + key.1.len() + key.2.len() + 96)
            {
                visited.insert(key.clone());
                next.insert(key);
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }
    let mut additional = Links::new();
    for (source, target) in dependents {
        add_link(&mut additional, source.0, target.0, "REFERENCES_PACKAGE", 1);
    }
    for ((source, target), value) in additional {
        if !links.contains_key(&(source.clone(), target.clone()))
            && !links.contains_key(&(target.clone(), source.clone()))
        {
            links.insert((source, target), value);
        }
    }
    Ok(())
}

fn unmapped_count(
    reader: &mut Reader<'_>,
    edges: &[scan::Unmapped],
    reached: &BTreeMap<Key, Reached>,
    file: Option<(&Id, &str)>,
    selection: &CodeNodeSelection,
) -> u64 {
    if !reader.budget.retain(reached.len().saturating_mul(256)) {
        return 0;
    }
    let files: BTreeSet<_> = reached
        .values()
        .map(|n| (n.raw.repository_id.as_str(), n.raw.node.unit_key.as_str()))
        .chain(file.map(|(r, p)| (r.as_str(), p)))
        .collect();
    let mut sources = BTreeMap::<(&str, &str), Vec<&str>>::new();
    for node in reached.values() {
        sources
            .entry((
                node.raw.repository_id.as_str(),
                node.raw.node.node_id.as_str(),
            ))
            .or_default()
            .push(&node.raw.node.unit_key);
    }
    let mut count = 0;
    for unresolved in edges {
        if !reader.budget.visit() {
            break;
        }
        let edge = &unresolved.original;
        let source_repo = edge.edge.payload["sourceRepositoryId"]
            .as_str()
            .unwrap_or(edge.repository_id.as_str());
        let target_repo = edge.edge.payload["targetRepositoryId"]
            .as_str()
            .unwrap_or(edge.repository_id.as_str());
        if !reader
            .scope()
            .views
            .iter()
            .any(|v| v.repository_id.as_str() == source_repo)
            || !reader
                .scope()
                .views
                .iter()
                .any(|v| v.repository_id.as_str() == target_repo)
        {
            continue;
        }
        let source = sources
            .get(&(source_repo, edge.edge.source_node_id.as_str()))
            .is_some_and(|units| match edge.edge.payload["sourceUnit"].as_str() {
                Some(unit) => units.contains(&unit),
                None => units.len() == 1,
            });
        let same_file = edge.edge.payload["targetUnit"]
            .as_str()
            .is_some_and(|path| files.contains(&(target_repo, path)));
        let name = if let CodeNodeSelection::Name {
            name,
            repository_id,
        } = selection
        {
            repository_id
                .as_ref()
                .is_none_or(|r| r.as_str() == target_repo)
                && ["targetName", "targetQualifiedName"].iter().any(|key| {
                    edge.edge.payload[key]
                        .as_str()
                        .is_some_and(|n| n.eq_ignore_ascii_case(&name.0))
                })
        } else {
            false
        };
        if source || same_file || name {
            count += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    fn raw(repository: &str, path: &str, kind: &str) -> GraphNode {
        GraphNode {
            repository_id: Id::new(repository).unwrap(),
            view_id: Id::new("view").unwrap(),
            generation_id: Id::new("generation").unwrap(),
            content_digest: "a".repeat(64),
            node: StoredNode {
                unit_key: path.into(),
                node_id: "node".into(),
                kind: kind.into(),
                name: "symbol".into(),
                payload: json!({}),
            },
        }
    }
    #[test]
    fn retained_risk_boosts_demotion_and_factors_keep_their_order() {
        let origin = Id::new("origin").unwrap();
        let local = raw("origin", "src/main.rs", "Function");
        assert_eq!(
            classify(&local, 1, "CALLS", &origin, None),
            (1, vec!["Hop 1 dependent".into()])
        );
        assert_eq!(classify(&local, 2, "CALLS", &origin, None).0, 2);
        assert_eq!(classify(&local, 3, "CALLS", &origin, None).0, 3);
        assert_eq!(classify(&local, 1, "CONSUMES", &origin, None).0, 0);
        assert_eq!(
            classify(&local, 1, "CALLS", &origin, Some("App.Tests")).0,
            2
        );
        let mut cross = raw("consumer", "tests/caller.rs", "Function");
        cross.node.payload = json!({"confidence":"low"});
        let (risk, factors) = classify(&cross, 1, "PUBLISHES", &origin, None);
        assert_eq!(
            risk, 1,
            "test demotion applies after direct cross-repository Critical"
        );
        assert_eq!(
            factors,
            vec![
                "Cross-repo dependency (harder to detect breakage)",
                "Async edge (PUBLISHES) — silent failures",
                "Direct cross-repo consumer",
                "Test code (caught by CI)",
                LOW_CONFIDENCE
            ]
        );
    }
    #[test]
    fn structural_containers_never_receive_contents_dependency_impact() {
        assert!(!local_dependency(
            "DEFINES",
            &raw("repo", "src/a.rs", "File")
        ));
        assert!(!local_dependency(
            "DEFINES_METHOD",
            &raw("repo", "src/a.rs", "Namespace")
        ));
        assert!(local_dependency(
            "DEFINES",
            &raw("repo", "src/a.rs", "Function")
        ));
        assert!(local_dependency(
            "DEFINES_METHOD",
            &raw("repo", "src/a.rs", "Class")
        ));
        assert!(!local_dependency(
            "PUBLISHES",
            &raw("repo", "src/a.rs", "Function")
        ));
    }
}
