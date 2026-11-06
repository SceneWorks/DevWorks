//! Actual captured module analyses and separately counted indexed facts.
use super::{properties, reference};
use crate::ports::storage::*;
use crate::services::graph::{modules::Modules, read::Reader, scan};
use crate::services::sessions::Failure;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Default)]
struct Observed {
    counts: BTreeMap<String, u64>,
    endpoints: Vec<Value>,
    services: Vec<Value>,
    tables: Vec<Value>,
    source: Option<Value>,
    indexed: Option<Value>,
}

pub(super) fn query(reader: &mut Reader<'_>, scope: &GraphScope) -> Result<Vec<Value>, Failure> {
    let owners = Modules::load(reader.read, scope, &mut reader.budget)?;
    let mut modules = BTreeMap::<(String, String), Observed>::new();
    let mut ambiguous_membership = false;
    scan::nodes(
        reader.read,
        scope,
        &GraphNodeFilter::default(),
        &mut reader.budget,
        |node, budget| {
            let (owner, ambiguous) = owners.owner(&node);
            if ambiguous {
                ambiguous_membership = true;
                return Ok(true);
            }
            let Some(name) = owner else {
                return Ok(true);
            };
            let key = (node.repository_id.as_str().to_owned(), name.to_owned());
            if !modules.contains_key(&key) && !budget.retain(key.0.len() + key.1.len() + 128) {
                return Ok(false);
            }
            let group = modules.entry(key).or_default();
            if !group.counts.contains_key(&node.node.kind)
                && !budget.retain(node.node.kind.len() + 64)
            {
                return Ok(false);
            }
            *group.counts.entry(node.node.kind.clone()).or_default() += 1;
            let refs = match node.node.kind.as_str() {
                "Route" => Some(&mut group.endpoints),
                "Service" => Some(&mut group.services),
                "Table" => Some(&mut group.tables),
                _ => None,
            };
            if let Some(refs) = refs {
                let value = reference(&node);
                if !budget.retain(value.to_string().len()) {
                    return Ok(false);
                }
                refs.push(value);
            }
            if node.node.kind == "DotnetProject" {
                group.indexed = Some(reference(&node));
                // This is a captured, typed producer seam. An absent row is NotAnalyzed;
                // another view's current analysis or today's application build is never substituted.
                if let Some(recorded) = properties(&node).get("analysis").filter(|v| v.is_object())
                {
                    if !budget.retain(recorded.to_string().len()) {
                        return Ok(false);
                    }
                    group.source = Some(recorded.clone());
                }
            }
            Ok(true)
        },
    )?;
    if ambiguous_membership && !reader.gaps.contains(&"ambiguous_internal_module") {
        reader.gaps.push("ambiguous_internal_module");
    }
    let complete = reader.budget.exhausted.is_none() && !ambiguous_membership;
    let indexed_complete = complete
        && reader
            .selected
            .layers
            .iter()
            .filter(|layer| scope.views.iter().any(|v| v.view_id == layer.view.view_id))
            .all(|layer| layer.view.coverage["state"] == "Complete");
    let mut out = Vec::new();
    for ((repo, name), group) in modules {
        let mut module = json!({"repositoryId":repo,"moduleName":name});
        if let Some(indexed) = group.indexed {
            module["indexedNode"] = indexed;
        }
        let mut value = json!({"module":module,"state":"NotAnalyzed","reason":"analysis_not_recorded",
            "observedNodeCounts":group.counts.into_iter().map(|(name,count)|json!({"name":name,"count":count.to_string()})).collect::<Vec<_>>(),
            "observedCoverage":{"state":if indexed_complete{"Complete"}else{"Partial"},"gaps":[],"moreGaps":false},
            "endpoints":drilldown("Route",group.endpoints,complete),"services":drilldown("Service",group.services,complete),"tables":drilldown("Table",group.tables,complete)});
        if let Some(source) = group.source {
            // Accept only the closed current typed record, including its actual provenance.
            // Invalid stored analysis is unavailable, never silently replaced with invented prose.
            value["recorded"] = source;
            value["state"] = json!("Recorded");
            value.as_object_mut().expect("object").remove("reason");
        }
        out.push(value);
    }
    Ok(out)
}
fn drilldown(kind: &str, nodes: Vec<Value>, complete: bool) -> Value {
    let mut value = json!({"nodeKind":kind,"nodes":nodes,"hasMore":false});
    if complete {
        value["count"] = json!(value["nodes"].as_array().expect("array").len().to_string());
    }
    value
}
