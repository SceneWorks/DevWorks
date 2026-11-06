//! Read-only aggregates of the selected composition. Indexed facts are never model analyses.
//! Whole-store counts use the store's exact composed aggregate; ranked rows use one shared
//! bounded scan and disclose its actual budget/coverage through the enclosing graph page.
mod aggregate;
mod module_analysis;
mod schema;

use super::{
    model,
    read::{Metadata, Reader},
};
use crate::ports::storage::*;
use crate::protocol::DomainErrorCode;
use crate::protocol::{
    Id,
    data::{DataScope, GraphResult},
};
use crate::services::sessions::{Failure, Refusal};
use serde_json::{Value, json};

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
    let mut repositories = Vec::new();
    let mut after = None;
    loop {
        let page = metadata.read.projects().repositories(
            &RepositoryFilter {
                project: match metadata.scope {
                    DataScope::Project { project_id } => Some(project_id.clone()),
                    _ => None,
                },
                ..Default::default()
            },
            after.as_ref(),
            32,
        )?;
        if page.is_empty() {
            break;
        }
        for row in &page {
            after = Some(row.id.clone());
            if repository.is_some_and(|id| id != &row.id) {
                continue;
            }
            // The same visibility seam is used even for metadata-only bootstrap responses.
            super::scope::authorize(metadata.read, metadata.context, metadata.scope, &row.id)?;
            if repositories.len() == 500 {
                return Err(Refusal::new(
                    DomainErrorCode::ViewUnavailable,
                    "overview_metadata_page",
                    None,
                )
                .into());
            }
            let mut value = json!({"repositoryId":row.id,"name":row.name,"foundational":false});
            if row.languages.len() == 1 {
                value["language"] = json!(row.languages[0]);
            }
            repositories.push(value);
        }
        if page.len() < 32 {
            break;
        }
    }
    let mut value = json!({"repositories":repositories,"dependencies":[],"stamp":metadata.stamp,
        "graphCoverage":{"state":"Unavailable"},"unavailableReason":"graph_not_indexed","internalModules":[]});
    if let Some(reader) = reader.as_mut() {
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
        value["graphCoverage"] = json!({"state": if reader.budget.exhausted.is_some() || !reader.gaps.is_empty() || !reader.selected.pin.unavailable_repositories.is_empty() { "Partial" } else { "Complete" }});
        value
            .as_object_mut()
            .expect("object")
            .remove("unavailableReason");
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
            .is_some_and(|s| s.eq_ignore_ascii_case("true"))
    })
}
fn reference(node: &GraphNode) -> Value {
    json!({"repositoryId":node.repository_id,"viewId":node.view_id,"unitKey":node.node.unit_key,"nodeId":node.node.node_id})
}

/// One signed outer continuation addresses a deterministic sequence of report sections.
/// Aggregate totals stay independent of the displayed section rows.
fn page_sections(
    reader: &mut Reader<'_>,
    value: &mut Value,
    sections: &[&str],
) -> Result<(), Failure> {
    let mut at = 0usize;
    let mut emitted = 0usize;
    let mut stopped = false;
    for name in sections {
        let rows = value
            .pointer_mut(&format!("/{name}"))
            .and_then(Value::as_array_mut)
            .expect("report section");
        let mut page = Vec::new();
        for row in std::mem::take(rows) {
            let index = at;
            at += 1;
            if index < reader.position.offset || stopped {
                continue;
            }
            if emitted == reader.limit || !reader.retain_wire(&row)? {
                reader.next = Some(super::read::Position {
                    offset: index,
                    ..Default::default()
                });
                stopped = true;
                continue;
            }
            emitted += 1;
            page.push(row);
        }
        *rows = page;
    }
    if reader.position.offset > at {
        return Err(StorageError::CursorExpired.into());
    }
    Ok(())
}
