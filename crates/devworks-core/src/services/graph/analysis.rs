//! Read-only aggregates of the selected composition. Indexed facts are never model analyses.
//! Whole-store counts use the store's exact composed aggregate; ranked rows use one shared
//! bounded scan and disclose its actual budget/coverage through the enclosing graph page.
mod aggregate;
mod module_analysis;
mod paging;
mod schema;

use super::{
    model,
    read::{Metadata, Reader},
};
use crate::ports::storage::*;
use crate::protocol::{Id, data::GraphResult};
use crate::services::sessions::Failure;
use serde_json::{Value, json};

/// Only actual captured metadata from the selected repository view is returned.
pub(super) fn repository_metadata(
    reader: &mut Reader<'_>,
    repository: &Id,
) -> Result<Option<Value>, Failure> {
    let scope = reader.scoped_repository(repository)?;
    aggregate::repository_properties(reader, &scope)
}

pub(super) fn architecture(
    reader: &mut Reader<'_>,
    repository: &Id,
) -> Result<GraphResult, Failure> {
    let scope = reader.scoped_repository(repository)?;
    let counts = aggregate::kind_counts(reader, &scope)?;
    let mut aggregate = aggregate::Aggregate::scan(reader)?;
    let modules = module_analysis::query(reader, &scope)?;
    let (summary, confidence) = aggregate::repository_summary(reader, &scope)?;
    let (hotspots, supertypes, packages, package_count, ecosystems) =
        aggregate.ranked(reader, repository)?;
    let (inbound, outbound) = aggregate.links_for(repository);
    let mut value = json!({"repositoryId":repository,"nodeCounts":counts,"hotspots":hotspots,
        "supertypes":supertypes,"packages":packages,
        "packageCountsByEcosystem":ecosystems,"inbound":inbound,"outbound":outbound,"internalModules":modules});
    if let Some(count) = package_count {
        value["packageCount"] = json!(count.to_string());
    }
    if let Some(summary) = summary {
        value["summary"] = json!(summary);
    }
    if let Some(confidence) = confidence {
        value["confidence"] = json!(confidence);
    }
    page_sections(
        reader,
        &mut value,
        &[
            "nodeCounts",
            "hotspots",
            "inbound",
            "outbound",
            "packages",
            "packageCountsByEcosystem",
            "supertypes",
            "internalModules",
        ],
    )?;
    Ok(GraphResult::Architecture {
        value: model::wire(value)?,
    })
}

pub(super) fn dependencies(
    reader: &mut Reader<'_>,
    repository: &Id,
) -> Result<GraphResult, Failure> {
    let scope = reader.scoped_repository(repository)?;
    let aggregate = aggregate::Aggregate::scan(reader)?;
    let (inbound, outbound) = aggregate.links_for(repository);
    let rust = aggregate::rust_semantic(reader, &scope)?;
    let mut value = json!({"repositoryId":repository,"inbound":inbound,"outbound":outbound,"rustSemantic":rust});
    page_sections(
        reader,
        &mut value,
        &["inbound", "outbound", "rustSemantic/treeSitterGroups"],
    )?;
    Ok(GraphResult::Dependencies {
        value: model::wire(value)?,
    })
}

pub(super) fn overview(
    metadata: &Metadata<'_>,
    mut reader: Option<&mut Reader<'_>>,
    repository: Option<&Id>,
) -> Result<GraphResult, Failure> {
    if let Some(repository) = repository {
        super::scope::authorize(metadata.read, metadata.context, metadata.scope, repository)?;
    }
    let mut repositories = Vec::new();
    metadata.repositories(|row| {
        if repository.is_some_and(|id| id != &row.id) {
            return Ok(true);
        }
        let mut value = json!({"repositoryId":row.id,"name":row.name});
        if row.languages.len() == 1 {
            value["language"] = json!(row.languages[0]);
        }
        if !metadata.budget.borrow_mut().retain(value.to_string().len()) {
            return Ok(false);
        }
        repositories.push(value);
        Ok(true)
    })?;
    let mut value = json!({"repositories":repositories,"dependencies":[],"stamp":metadata.stamp,
        "graphCoverage":{"state":"Unavailable","gaps":[],"moreGaps":false},"unavailableReason":"graph_not_indexed","internalModules":[]});
    let graph_available = reader.as_ref().is_some_and(|reader| {
        repository.is_none_or(|id| {
            reader
                .scope()
                .views
                .iter()
                .any(|view| &view.repository_id == id)
        })
    });
    if graph_available {
        let reader = reader.as_mut().expect("selected repository view");
        reader.budget.absorb_usage(&metadata.budget.borrow());
        for row in value["repositories"].as_array_mut().expect("repositories") {
            let id: Id = model::wire(row["repositoryId"].clone())?;
            if reader
                .scope()
                .views
                .iter()
                .any(|binding| binding.repository_id == id)
            {
                let scope = reader.scoped_repository(&id)?;
                if let Some(recorded) = aggregate::repository_properties(reader, &scope)? {
                    if let Some(flag) = recorded
                        .get("foundational")
                        .or_else(|| recorded.get("isFoundational"))
                        .and_then(Value::as_bool)
                    {
                        row["foundational"] = json!(flag);
                    }
                    for (input, output) in
                        [("sourceGroup", "sourceGroup"), ("framework", "framework")]
                    {
                        if let Some(text) = recorded.get(input).and_then(Value::as_str) {
                            row[output] = json!(text);
                        }
                    }
                }
                if let Some(source) = reader.readme_source(&id)? {
                    row["readmeSource"] = serde_json::to_value(source).expect("typed source");
                }
            }
        }
        let scope = match repository {
            Some(id) => reader.scoped_repository(id)?,
            None => reader.scope().clone(),
        };
        let counts = reader.read.graph().graph_counts(&scope)?;
        let aggregate = aggregate::Aggregate::scan(reader)?;
        let links: Vec<_> = aggregate
            .links()
            .into_iter()
            .filter(|v| {
                repository.is_none_or(|id| {
                    v["sourceRepositoryId"] == json!(id) || v["targetRepositoryId"] == json!(id)
                })
            })
            .collect();
        value["dependencies"] = json!(links);
        value["internalModules"] = json!(module_analysis::query(reader, &scope)?);
        value["graphTotals"] =
            json!({"nodeCount":counts.nodes.to_string(),"edgeCount":counts.edges.to_string()});
        value["graphCoverage"] = json!({"state": if reader.budget.exhausted.is_some() || !reader.gaps.is_empty() || !reader.selected.pin.unavailable_repositories.is_empty() || reader.selected.layers.iter().any(|v|v.view.coverage["state"]!="Complete") { "Partial" } else { "Complete" },"gaps":[],"moreGaps":false});
        value
            .as_object_mut()
            .expect("object")
            .remove("unavailableReason");
        page_sections(
            reader,
            &mut value,
            &["repositories", "dependencies", "internalModules"],
        )?;
    } else {
        paging::metadata(metadata, &mut value, &["repositories"])?;
        if let Some(reader) = reader {
            reader.budget.absorb_usage(&metadata.budget.borrow());
            reader.next = metadata.next.borrow().clone();
        }
    }
    Ok(GraphResult::RepositoryOverview {
        value: model::wire(value)?,
    })
}

pub(super) fn schema(
    metadata: &Metadata<'_>,
    reader: Option<&mut Reader<'_>>,
) -> Result<GraphResult, Failure> {
    schema::query(metadata, reader)
}

fn properties(node: &GraphNode) -> &Value {
    node.node
        .payload
        .get("properties")
        .unwrap_or(&node.node.payload)
}
fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .filter(|v| !v.is_null())
        .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
        .filter(|s| !s.trim().is_empty())
}
fn yes(value: &Value) -> bool {
    value.as_bool().unwrap_or_else(|| {
        value
            .as_str()
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("true"))
    })
}
fn reference(node: &GraphNode) -> Value {
    json!({"repositoryId":node.repository_id,"viewId":node.view_id,"unitKey":node.node.unit_key,"nodeId":node.node.node_id})
}

fn page_sections(
    reader: &mut Reader<'_>,
    value: &mut Value,
    sections: &[&str],
) -> Result<(), Failure> {
    paging::sections(reader, value, sections)
}
