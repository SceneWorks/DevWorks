//! The analysis link phase reads a composed graph and never rewrites its layers.
use super::super::{budget::Budget, dataset, model, scan};
use super::{http, selection::Selection, symbols::References};
use crate::ports::storage::{
    GraphEdge, GraphNodeFilter, GraphNodeIdentity, GraphScope, ReadSnapshot, StoredEdge,
};
use crate::protocol::Id;
use crate::protocol::records::{
    AnalysisLinkEndpoint, AnalysisLinkEvidence, AnalysisMetricItem, MetricRecord,
};
use crate::protocol::support::BoundedText;
use crate::services::{indexing::views::ViewLayers, kernel::Failure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

const KINDS: &[&str] = &[
    "CALLS",
    "CONSUMES",
    "DEPENDS_ON",
    "HANDLES",
    "HTTP_CALLS",
    "IMPORTS",
    "PUBLISHES",
    "REFERENCES_PACKAGE",
    "USES_TYPE",
    "IMPLEMENTS",
    "RENDERS",
    "INJECTS",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct Link {
    pub source: GraphNodeIdentity,
    pub target: Option<GraphNodeIdentity>,
    pub kind: String,
    pub inferred: bool,
    pub confidence: f64,
    pub matched_by: String,
    pub diagnostic: Option<String>,
}

impl Link {
    pub(super) fn wire(
        &self,
        views: &BTreeMap<&str, &Id>,
    ) -> Result<AnalysisLinkEvidence, Failure> {
        fn endpoint(
            node: &GraphNodeIdentity,
            views: &BTreeMap<&str, &Id>,
        ) -> Result<AnalysisLinkEndpoint, Failure> {
            let view_id = views
                .get(node.repository_id.as_str())
                .ok_or_else(super::selection::unavailable)?;
            Ok(AnalysisLinkEndpoint {
                repository_id: node.repository_id.clone(),
                view_id: (*view_id).clone(),
                node_id: model::text(&node.node_id)?,
            })
        }
        Ok(AnalysisLinkEvidence {
            relationship: model::text(&self.kind)?,
            source: endpoint(&self.source, views)?,
            target: self
                .target
                .as_ref()
                .map(|node| endpoint(node, views))
                .transpose()?,
            // Unresolved evidence never copies a raw display hint from another repository.
            target_label: None,
            resolution: BoundedText(
                match (&self.target, self.inferred) {
                    (None, _) => "Unresolved",
                    (Some(_), true) => "Inferred",
                    (Some(_), false) => "Resolved",
                }
                .into(),
            ),
            confidence: model::wire(serde_json::json!(self.confidence))?,
            matched_by: model::text(&self.matched_by)?,
        })
    }

    pub(super) fn metric(
        &self,
        views: &BTreeMap<&str, &Id>,
    ) -> Result<AnalysisMetricItem, Failure> {
        let link = self.wire(views)?;
        Ok(AnalysisMetricItem {
            metric: MetricRecord {
                name: link.relationship.clone(),
                value: link.confidence.clone(),
                unit: BoundedText("confidence".into()),
                source_id: link.source.node_id.clone(),
                inferred: self.inferred,
            },
            repository_id: link.source.repository_id.clone(),
            view_id: link.source.view_id.clone(),
            path: None,
            computed_at_ms: None,
            namespace: None,
            language: None,
            language_indexed: None,
            history_coverage: None,
            link: Some(link),
            file_metrics: None,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct LinkReport {
    pub links: Vec<Link>,
    pub gaps: Vec<String>,
    pub visited: u32,
}

pub(super) fn build(
    read: &dyn ReadSnapshot,
    selection: &Selection,
    budget: &mut Budget,
) -> Result<LinkReport, Failure> {
    let mut report = build_for(read, &selection.graph_scope(), &selection.layers, budget)?;
    report.gaps.extend(
        selection
            .unavailable
            .iter()
            .map(|entry| entry.reason.clone()),
    );
    report.gaps.sort();
    report.gaps.dedup();
    Ok(report)
}

fn build_for(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
    layers: &[ViewLayers],
    budget: &mut Budget,
) -> Result<LinkReport, Failure> {
    let mut routes = Vec::new();
    scan::nodes(
        read,
        scope,
        &GraphNodeFilter {
            kinds: vec!["Route".into()],
            ..Default::default()
        },
        budget,
        |node, budget| {
            let Some(name) = http::string(&node.node.payload, "qualifiedName") else {
                return Ok(true);
            };
            let properties = &node.node.payload["properties"];
            if !budget
                .retain(node.node.unit_key.len() + name.len() + properties.to_string().len() + 256)
            {
                return Ok(false);
            }
            if let Some(route) = http::Route::from_node(dataset::identity(&node), name, properties)
            {
                routes.push(route);
            }
            Ok(true)
        },
    )?;
    let mut names = Vec::new();
    for view in &scope.views {
        let repository = read
            .projects()
            .repository(&view.repository_id)?
            .ok_or_else(super::selection::unavailable)?;
        names.push((repository.name, repository.id));
    }
    let names = http::Names::new(names);
    let mut roots = BTreeMap::<String, Option<GraphNodeIdentity>>::new();
    scan::nodes(
        read,
        scope,
        &GraphNodeFilter {
            kinds: vec!["Repository".into()],
            ..Default::default()
        },
        budget,
        |node, _| {
            roots
                .entry(node.repository_id.as_str().into())
                .and_modify(|found| *found = None)
                .or_insert_with(|| Some(dataset::identity(&node)));
            Ok(true)
        },
    )?;
    let mut links = Links::default();
    let mut messages = Messages::default();
    let mut references = References::default();
    scan::edges(read, scope, layers, KINDS, budget, |edge, budget| {
        if let Ok(resolved) = &edge
            && resolved.source.repository_id == resolved.target.repository_id
            && !references.record(resolved, read, scope, budget)?
        {
            return Ok(false);
        }
        let (raw, source, target) = match &edge {
            Ok(edge) => (
                &edge.raw,
                Some(edge.source.clone()),
                Some(edge.target.clone()),
            ),
            Err(edge) => (&edge.raw, None, None),
        };
        let kind = raw.edge.kind.as_str();
        let payload = &raw.edge.payload;
        let properties = &payload["properties"];
        let concrete = source.as_ref().zip(target.as_ref()).filter(|(from, to)| {
            from.repository_id != to.repository_id
                && !matches!(
                    http::string(payload, "resolution"),
                    Some("Unresolved") | Some("Ambiguous")
                )
        });

        if kind == "HTTP_CALLS" && http::boolean(properties, "binding_ambiguous") {
            if let Some(from) = caller(read, scope, raw, source, budget)? {
                return Ok(links.unresolved(from, kind, "AmbiguousRoute", budget));
            }
            return Ok(true);
        }
        if kind == "HTTP_CALLS"
            && http::boolean(properties, "gateway_call")
            && let Some(dto) =
                http::nonempty(properties, "request_dto").filter(|dto| !dto.starts_with("route:"))
        {
            let Some(from) = caller(read, scope, raw, source, budget)? else {
                return Ok(true);
            };
            let target = http::nonempty(properties, "service_name")
                .and_then(|name| names.service(name))
                .or_else(|| names.dto(dto));
            if let Some(repository) = target {
                if repository == &from.repository_id {
                    return Ok(true);
                }
                if let Some(to) = roots.get(repository.as_str()).and_then(Option::as_ref) {
                    return Ok(links.insert(
                        inferred(from, to.clone(), kind, "GatewayService", 0.7),
                        budget,
                    ));
                }
            }
            return Ok(links.unresolved(from, kind, "GatewayServiceUnavailable", budget));
        }
        if kind == "REFERENCES_PACKAGE"
            && let Some((from, to)) = source.as_ref().zip(target.as_ref())
            && from.repository_id == to.repository_id
            && let Some(package) = read.graph().graph_node(scope, to)?
            && package.node.kind == "NuGetPackage"
            && let Some(repository) = names.nuget(&package.node.name)
            && repository != &from.repository_id
        {
            if let Some(to) = roots.get(repository.as_str()).and_then(Option::as_ref) {
                return Ok(links.insert(
                    inferred(from.clone(), to.clone(), kind, "NuGetPackageName", 0.9),
                    budget,
                ));
            }
            return Ok(links.unresolved(from.clone(), kind, "NuGetRepositoryUnavailable", budget));
        }
        if kind == "HTTP_CALLS" && http::nonempty(properties, "url_pattern").is_some() {
            let Some(from) = caller(read, scope, raw, source, budget)? else {
                return Ok(true);
            };
            return Ok(
                match http::resolve(
                    &routes,
                    &names,
                    properties,
                    http::string(payload, "targetQualifiedName"),
                    budget,
                ) {
                    http::Match::Unique(route)
                        if route.node.repository_id != from.repository_id =>
                    {
                        links.insert(
                            Link {
                                source: from,
                                target: Some(route.node.clone()),
                                kind: kind.into(),
                                inferred: true,
                                confidence: 0.7,
                                matched_by: "HttpRoute".into(),
                                diagnostic: None,
                            },
                            budget,
                        )
                    }
                    // A definite local binding is not a cross-repository link.
                    http::Match::Unique(_) => true,
                    http::Match::Incomplete => false,
                    http::Match::Unresolved => {
                        links.unresolved(from, kind, "NoEligibleRoute", budget)
                    }
                    http::Match::Ambiguous => {
                        links.unresolved(from, kind, "AmbiguousRoute", budget)
                    }
                },
            );
        }
        if matches!(kind, "PUBLISHES" | "CONSUMES")
            && let Some(event) = http::nonempty(properties, "event_type")
                .or_else(|| http::nonempty(payload, "targetQualifiedName"))
            && let Some(from) = caller(read, scope, raw, source.clone(), budget)?
            && !messages.record(kind, event, from, budget)
        {
            return Ok(false);
        }
        if let Some((from, to)) = concrete {
            let resolution = http::string(payload, "resolution").unwrap_or("Resolved");
            let inferred = !resolution.eq_ignore_ascii_case("Resolved");
            let confidence = payload["confidence"]
                .as_f64()
                .filter(|value| (0.0..=1.0).contains(value))
                .unwrap_or(if inferred { 0.7 } else { 1.0 });
            let basis = http::nonempty(properties, "resolutionBasis")
                .filter(|value| value.len() <= 128 && !value.chars().any(char::is_control))
                .unwrap_or("GraphRelationship");
            let relationship = if resolution == "DerivedPackage" {
                "REFERENCES_PACKAGE"
            } else {
                kind
            };
            return Ok(links.insert(
                Link {
                    source: from.clone(),
                    target: Some(to.clone()),
                    kind: relationship.into(),
                    inferred,
                    confidence,
                    matched_by: basis.into(),
                    diagnostic: None,
                },
                budget,
            ));
        }
        let explicit_cross = http::nonempty(payload, "targetRepositoryId")
            .is_some_and(|id| id != raw.repository_id.as_str());
        // An absent target that may be outside the scope is a gap, not an exterior identity.
        if explicit_cross && let Some(from) = caller(read, scope, raw, source, budget)? {
            return Ok(links.unresolved(from, kind, "TargetUnavailable", budget));
        }
        Ok(true)
    })?;
    messages.link(&mut links, budget);
    references.resolve(read, scope, budget, |link, budget| {
        links.insert(link, budget)
    })?;
    let mut gaps = Vec::new();
    if let Some(exhausted) = budget.exhausted {
        gaps.push(exhausted.to_owned());
    }
    for layer in layers {
        for id in &layer.layers {
            let generation = read
                .indexing()
                .generation(id)?
                .ok_or_else(super::selection::unavailable)?;
            let (_, missing) =
                crate::services::indexing::status::manifest_coverage(&generation.manifest);
            gaps.extend(missing);
        }
    }
    gaps.sort();
    gaps.dedup();
    Ok(LinkReport {
        links: links.rows,
        gaps,
        visited: budget.visited,
    })
}

/// Use the same derivation in graph exploration. The graph read has already checked every
/// layer's authority and supplies its own work budget. Raw edges remain in the index.
pub(in crate::services::graph) fn graph_edges(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
    layers: &[ViewLayers],
    kinds: &[&str],
    budget: &mut Budget,
    existing: &[scan::Edge],
) -> Result<Vec<scan::Edge>, Failure> {
    if scope.views.len() < 2 || budget.exhausted.is_some() {
        return Ok(Vec::new());
    }
    let report = build_for(read, scope, layers, budget)?;
    if budget.exhausted.is_some() {
        return Ok(Vec::new());
    }
    let mut derived = Vec::new();
    let mut seen = BTreeSet::new();
    for edge in existing {
        if edge.source.repository_id == edge.target.repository_id {
            continue;
        }
        let pair = (
            dataset::key(&edge.source),
            dataset::key(&edge.target),
            edge.raw.edge.kind.as_str(),
        );
        if !budget.retain(
            pair.0.0.len()
                + pair.0.1.len()
                + pair.0.2.len()
                + pair.1.0.len()
                + pair.1.1.len()
                + pair.1.2.len()
                + pair.2.len()
                + 96,
        ) {
            return Ok(derived);
        }
        seen.insert(pair);
    }
    for link in &report.links {
        let Some(target) = &link.target else { continue };
        if !kinds.is_empty() && !kinds.contains(&link.kind.as_str()) {
            continue;
        }
        if !seen.insert((
            dataset::key(&link.source),
            dataset::key(target),
            link.kind.as_str(),
        )) {
            continue;
        }
        if !budget.visit()
            || !budget.retain(
                link.source.unit_key.len()
                    + target.unit_key.len()
                    + link.kind.len()
                    + link.matched_by.len()
                    + 512,
            )
        {
            break;
        }
        let from = read
            .graph()
            .graph_node(scope, &link.source)?
            .ok_or_else(super::selection::unavailable)?;
        let to = read
            .graph()
            .graph_node(scope, target)?
            .ok_or_else(super::selection::unavailable)?;
        let edge_id = format!(
            "derived-analysis-{}",
            super::selection::digest(&json!({
                "source": link.source, "target": target, "kind": link.kind,
                "fromView": from.view_id, "toView": to.view_id,
            }))
        );
        derived.push(scan::Edge {
            source: link.source.clone(),
            target: target.clone(),
            raw: GraphEdge {
                repository_id: from.repository_id,
                view_id: from.view_id,
                generation_id: from.generation_id,
                content_digest: from.content_digest,
                edge: StoredEdge {
                    unit_key: link.source.unit_key.clone(),
                    edge_id,
                    source_node_id: link.source.node_id.clone(),
                    target_node_id: target.node_id.clone(),
                    kind: link.kind.clone(),
                    payload: json!({
                        "targetRepositoryId": target.repository_id, "targetUnitKey": target.unit_key,
                        "resolution": if link.inferred { "Inferred" } else { "Resolved" },
                        "confidence": link.confidence,
                        "properties": { "resolutionBasis": link.matched_by, "targetViewId": to.view_id },
                    }),
                },
            },
        });
    }
    Ok(derived)
}

fn caller(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
    raw: &crate::ports::storage::GraphEdge,
    source: Option<GraphNodeIdentity>,
    budget: &mut Budget,
) -> Result<Option<GraphNodeIdentity>, Failure> {
    if source.is_some() {
        return Ok(source);
    }
    scan::endpoint(
        read,
        scope,
        &raw.repository_id,
        http::string(&raw.edge.payload, "sourceUnit"),
        &raw.edge.source_node_id,
        budget,
    )
}

fn inferred(
    source: GraphNodeIdentity,
    target: GraphNodeIdentity,
    kind: &str,
    matched_by: &str,
    confidence: f64,
) -> Link {
    Link {
        source,
        target: Some(target),
        kind: kind.into(),
        inferred: true,
        confidence,
        matched_by: matched_by.into(),
        diagnostic: None,
    }
}

type LinkKey = (dataset::Key, Option<dataset::Key>, String, Option<String>);

#[derive(Default)]
struct Links {
    identities: BTreeSet<LinkKey>,
    rows: Vec<Link>,
}

impl Links {
    fn insert(&mut self, link: Link, budget: &mut Budget) -> bool {
        let key = (
            dataset::key(&link.source),
            link.target.as_ref().map(dataset::key),
            link.kind.clone(),
            link.diagnostic.clone(),
        );
        if self.identities.contains(&key) {
            return true;
        }
        let size = key.0.0.len()
            + key.0.1.len()
            + key.0.2.len()
            + key
                .1
                .as_ref()
                .map_or(0, |target| target.0.len() + target.1.len() + target.2.len())
            + link.kind.len()
            + link.matched_by.len()
            + link.diagnostic.as_ref().map_or(0, String::len);
        if !budget.retain(size.saturating_mul(2) + 384) {
            return false;
        }
        self.identities.insert(key);
        self.rows.push(link);
        true
    }

    fn unresolved(
        &mut self,
        source: GraphNodeIdentity,
        kind: &str,
        cause: &str,
        budget: &mut Budget,
    ) -> bool {
        self.insert(
            Link {
                source,
                target: None,
                kind: kind.into(),
                inferred: false,
                confidence: 0.0,
                matched_by: cause.into(),
                diagnostic: Some(cause.into()),
            },
            budget,
        )
    }
}

#[derive(Default)]
struct Messages {
    publishers: BTreeMap<String, Vec<GraphNodeIdentity>>,
    consumers: BTreeMap<String, Vec<GraphNodeIdentity>>,
}

impl Messages {
    fn record(
        &mut self,
        kind: &str,
        event: &str,
        node: GraphNodeIdentity,
        budget: &mut Budget,
    ) -> bool {
        if !budget.retain(event.len() + node.node_id.len() + node.unit_key.len() + 192) {
            return false;
        }
        let groups = if kind == "PUBLISHES" {
            &mut self.publishers
        } else {
            &mut self.consumers
        };
        groups.entry(event.into()).or_default().push(node);
        true
    }

    fn link(self, links: &mut Links, budget: &mut Budget) {
        for (event, publishers) in self.publishers {
            let Some(consumers) = self.consumers.get(&event) else {
                continue;
            };
            for source in publishers {
                for target in consumers {
                    if !budget.visit() {
                        return;
                    }
                    if source.repository_id == target.repository_id {
                        continue;
                    }
                    if !links.insert(
                        Link {
                            source: source.clone(),
                            target: Some(target.clone()),
                            kind: "PUBLISHES".into(),
                            inferred: true,
                            confidence: 0.9,
                            matched_by: "MessageType".into(),
                            diagnostic: None,
                        },
                        budget,
                    ) {
                        return;
                    }
                }
            }
        }
    }
}

/// The community phase counts only observed cross-repository endpoints.
pub(super) fn relation<'a>(
    link: &'a Link,
    indexes: &BTreeMap<&str, usize>,
) -> Option<super::community::Relation<'a>> {
    Some(super::community::Relation {
        source: *indexes.get(link.source.repository_id.as_str())?,
        target: *indexes.get(link.target.as_ref()?.repository_id.as_str())?,
        kind: &link.kind,
    })
}
