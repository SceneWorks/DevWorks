use super::{properties, text, yes};
use crate::ports::storage::*;
use crate::protocol::Id;
use crate::services::graph::{dataset, read::Reader, scan};
use crate::services::sessions::Failure;
use serde_json::{Value, json};
use std::collections::BTreeMap;

type LinkKey = (String, String, String);
type DegreeKey = dataset::Key;
type Ranked = (Vec<Value>, Vec<Value>, Vec<Value>, Option<u64>, Vec<Value>);
#[derive(Default)]
pub(super) struct Aggregate {
    degrees: BTreeMap<DegreeKey, (u64, u64, u64)>,
    links: BTreeMap<LinkKey, (u64, Option<u8>)>,
    complete: bool,
}
impl Aggregate {
    pub fn scan(reader: &mut Reader<'_>) -> Result<Self, Failure> {
        let mut out = Self::default();
        let mut unmapped = false;
        let scope = reader.scope().clone();
        scan::edges(
            reader.read,
            &scope,
            &reader.selected.layers,
            &mut reader.budget,
            |edge, budget| {
                let Ok(edge) = edge else {
                    unmapped = true;
                    return Ok(true);
                };
                let kind = &edge.raw.edge.kind;
                if edge.source.repository_id != edge.target.repository_id {
                    let key = (
                        edge.source.repository_id.as_str().to_owned(),
                        edge.target.repository_id.as_str().to_owned(),
                        kind.clone(),
                    );
                    if !out.links.contains_key(&key)
                        && !budget.retain(key.0.len() + key.1.len() + key.2.len() + 64)
                    {
                        return Ok(false);
                    }
                    let entry = out.links.entry(key).or_default();
                    entry.0 += 1;
                    let p = &edge.raw.edge.payload;
                    let band = p
                        .get("confidenceBand")
                        .or_else(|| p.get("confidence_band"))
                        .or_else(|| p.get("properties").and_then(|p| p.get("confidence_band")))
                        .and_then(Value::as_str)
                        .map(str::to_lowercase);
                    let band = band
                        .as_deref()
                        .and_then(|s| ["low", "medium", "high"].iter().position(|v| *v == s))
                        .map(|v| v as u8);
                    if let Some(band) = band {
                        entry.1 = Some(entry.1.map_or(band, |old| old.min(band)));
                    }
                }
                if edge.source.repository_id == edge.target.repository_id
                    && ["CALLS", "IMPLEMENTS", "INHERITS", "REFERENCES_PACKAGE"]
                        .contains(&kind.as_str())
                {
                    let key = dataset::key(&edge.target);
                    if !out.degrees.contains_key(&key)
                        && !budget.retain(key.0.len() + key.1.len() + key.2.len() + 80)
                    {
                        return Ok(false);
                    }
                    let entry = out.degrees.entry(key).or_default();
                    match kind.as_str() {
                        "CALLS" => entry.0 += 1,
                        "REFERENCES_PACKAGE" => entry.2 += 1,
                        _ => entry.1 += 1,
                    }
                }
                Ok(true)
            },
        )?;
        if unmapped && !reader.gaps.contains(&"unresolved_aggregate_endpoint") {
            reader.gaps.push("unresolved_aggregate_endpoint");
        }
        out.complete = reader.budget.exhausted.is_none() && !unmapped;
        Ok(out)
    }
    pub fn links(&self) -> Vec<Value> {
        self.links.iter().map(|((source,target,kind),(count,band))| {
            let mut value=json!({"sourceRepositoryId":source,"targetRepositoryId":target,"edgeKind":kind,"count":count.to_string()});
            if let Some(band)=band { value["confidenceBand"]=json!(["low","medium","high"][*band as usize]); }
            value
        }).collect()
    }
    pub fn links_for(&self, repository: &Id) -> (Vec<Value>, Vec<Value>) {
        let rows = self.links();
        (
            rows.iter()
                .filter(|v| v["targetRepositoryId"] == json!(repository))
                .cloned()
                .collect(),
            rows.into_iter()
                .filter(|v| v["sourceRepositoryId"] == json!(repository))
                .collect(),
        )
    }
    pub fn ranked(&mut self, reader: &mut Reader<'_>, repository: &Id) -> Result<Ranked, Failure> {
        let scope = reader.scoped_repository(repository)?;
        let mut hotspots = Vec::new();
        let mut supertypes = Vec::new();
        let mut packages = Vec::new();
        let mut ecosystems = BTreeMap::<String, u64>::new();
        let mut package_count = 0u64;
        for ((repo, unit, node), (calls, types, refs)) in &self.degrees {
            if repo != repository.as_str() {
                continue;
            }
            if !reader.budget.visit() {
                break;
            }
            let identity = GraphNodeIdentity {
                repository_id: Id::new(repo.clone()).expect("captured id"),
                unit_key: unit.clone(),
                node_id: node.clone(),
            };
            let Some(raw) = reader.read.graph().graph_node(&scope, &identity)? else {
                continue;
            };
            let props = properties(&raw);
            let name = &raw.node.name;
            let qualified = raw
                .node
                .payload
                .get("qualifiedName")
                .and_then(Value::as_str)
                .unwrap_or(name);
            if *calls >= 3 && ["Method", "Function"].contains(&raw.node.kind.as_str()) {
                hotspots.push(json!({"name":name,"qualifiedName":qualified,"fanIn":calls.to_string(),"reason":"High fan-in method"}));
            }
            if *types > 0 {
                supertypes.push(json!({"name":name,"qualifiedName":qualified,"nodeKind":raw.node.kind,"subtypeCount":types.to_string(),"external":raw.node.kind=="ExternalSymbol" || qualified.starts_with("swift-external:") || yes(&props["scip_external"])}));
            }
            if *refs > 0
                && ["Package", "NuGetPackage"].contains(&raw.node.kind.as_str())
                && !yes(&props["is_definition"])
            {
                let ecosystem = text(props, "ecosystem")
                    .or_else(|| text(props, "package_type"))
                    .unwrap_or_else(|| {
                        if qualified.starts_with("npm:") {
                            "npm"
                        } else if raw.node.kind == "NuGetPackage" {
                            "nuget"
                        } else {
                            "unknown"
                        }
                        .into()
                    });
                *ecosystems.entry(ecosystem.clone()).or_default() += 1;
                package_count += 1;
                let mut package =
                    json!({"name":name,"ecosystem":ecosystem,"dependentCount":refs.to_string()});
                if let Some(version) = ["version", "version_requirement", "revision", "branch"]
                    .into_iter()
                    .find_map(|k| text(props, k))
                {
                    package["version"] = json!(version);
                }
                packages.push(package);
            }
            // Only bounded top rows survive, while package population/ecosystem totals continue.
            top(&mut hotspots, "fanIn", 10);
            top(&mut supertypes, "subtypeCount", 10);
            top(&mut packages, "dependentCount", 25);
        }
        let complete = self.complete && reader.budget.exhausted.is_none();
        if !complete {
            ecosystems.clear();
        }
        Ok((
            hotspots,
            supertypes,
            packages,
            complete.then_some(package_count),
            ecosystems
                .into_iter()
                .map(|(name, count)| json!({"name":name,"count":count.to_string()}))
                .collect(),
        ))
    }
}
fn top(rows: &mut Vec<Value>, count: &str, limit: usize) {
    rows.sort_by(|a, b| {
        let number = |v: &Value| {
            v[count]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0)
        };
        number(b)
            .cmp(&number(a))
            .then_with(|| a["ecosystem"].as_str().cmp(&b["ecosystem"].as_str()))
            .then_with(|| {
                a["name"]
                    .as_str()
                    .unwrap_or("")
                    .to_lowercase()
                    .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
            })
            .then_with(|| a["version"].as_str().cmp(&b["version"].as_str()))
    });
    rows.truncate(limit);
}

pub(super) fn kind_counts(
    reader: &mut Reader<'_>,
    scope: &GraphScope,
) -> Result<Vec<Value>, Failure> {
    let mut after = None;
    let mut rows = Vec::new();
    loop {
        let page =
            reader
                .read
                .graph()
                .graph_kinds(scope, GraphElementKind::Node, after.as_deref(), 32)?;
        for row in page.items {
            if !reader.budget.visit() {
                return Ok(rows);
            }
            after = Some(row.kind.clone());
            rows.push(json!({"name":row.kind,"count":row.count.to_string()}));
        }
        if !page.has_more {
            break;
        }
    }
    rows.sort_by(|a, b| {
        b["count"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .cmp(&a["count"].as_str().and_then(|s| s.parse::<u64>().ok()))
            .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });
    Ok(rows)
}
fn repository_properties(
    reader: &mut Reader<'_>,
    scope: &GraphScope,
) -> Result<Option<Value>, Failure> {
    let mut found = None;
    scan::nodes(
        reader.read,
        scope,
        &GraphNodeFilter {
            kinds: vec!["Repository".into()],
            ..Default::default()
        },
        &mut reader.budget,
        |raw, _| {
            found = Some(properties(&raw).clone());
            Ok(false)
        },
    )?;
    Ok(found)
}
pub(super) fn repository_summary(
    reader: &mut Reader<'_>,
    scope: &GraphScope,
) -> Result<(Option<String>, Option<String>), Failure> {
    let props = repository_properties(reader, scope)?.unwrap_or(Value::Null);
    Ok((text(&props, "summary"), text(&props, "confidence")))
}
pub(super) fn rust_semantic(reader: &mut Reader<'_>, scope: &GraphScope) -> Result<Value, Failure> {
    let props = repository_properties(reader, scope)?.unwrap_or(Value::Null);
    let mut outcomes = props.get("rustSemanticOutcomes").cloned();
    if outcomes.is_none() {
        for view in &reader.selected.layers {
            if !scope.views.iter().any(|b| b.view_id == view.view.view_id) {
                continue;
            }
            for id in &view.layers {
                if !reader.budget.visit() {
                    break;
                }
                if let Some(generation) = reader.read.indexing().generation(id)?
                    && let Some(value) = generation.manifest.get("rustSemanticOutcomes")
                {
                    outcomes = Some(value.clone());
                    break;
                }
            }
        }
    }
    let Some(outcomes) = outcomes else {
        return Ok(json!({"status":"not_analyzed","treeSitterGroups":[]}));
    };
    let Some(groups) = outcomes["groups"].as_array() else {
        return Ok(json!({"status":"not_analyzed","treeSitterGroups":[]}));
    };
    let mut fallback = Vec::new();
    for group in groups {
        if group["outcome"] == "semantic" {
            continue;
        }
        let mut row = json!({"index":group["index"].as_u64().unwrap_or(0).to_string(),"crates":group.get("crates").cloned().unwrap_or(json!([])),"outcome":group["outcome"]});
        for name in ["failureCode", "detailCode"] {
            if let Some(value) = group.get(name).filter(|v| !v.is_null()) {
                row[name] = value.clone();
            }
        }
        fallback.push(row);
    }
    let mut value = json!({"status":if fallback.is_empty(){"semantic"}else if fallback.len()==groups.len(){"fallback"}else{"partial"},"groupCount":groups.len().to_string(),"treeSitterGroups":fallback});
    if let Some(manifest) = outcomes.get("manifest").filter(|v| !v.is_null()) {
        value["manifest"] = manifest.clone();
    }
    for name in ["semanticGroups", "fallbackGroups", "skippedGroups"] {
        if let Some(count) = outcomes.get(name).and_then(Value::as_u64) {
            value[name] = json!(count.to_string());
        }
    }
    Ok(value)
}
