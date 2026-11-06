use crate::ports::storage::*;
use crate::protocol::data::GraphResult;
use crate::services::graph::{
    model,
    read::{Metadata, Reader},
};
use crate::services::sessions::Failure;
use serde_json::{Value, json};
use std::collections::BTreeMap;

const DECLARED: &str = include_str!("declared-schema.md");
pub(super) fn query(
    metadata: &Metadata<'_>,
    mut reader: Option<&mut Reader<'_>>,
) -> Result<GraphResult, Failure> {
    let section = |start: &str, end: &str| {
        DECLARED
            .split(start)
            .nth(1)
            .and_then(|s| s.split(end).next())
            .unwrap_or("")
    };
    let names = |s: &str| {
        s.split([',', '\n'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let mut nodes = names(section("## Node Types", "## Edge Types"));
    let mut edges = names(section("## Edge Types", "## Key Properties"));
    let mut properties = Vec::<Value>::new();
    // Retain the complete source description as notes; expose the named property lists as
    // structured declarations as well. Extended prose stays exact in the declared document.
    let mut owner: Option<(String, String)> = None;
    let mut declarations = Vec::new();
    for line in section("## Key Properties", "## Swift (tree-sitter)").lines() {
        if let Some(line) = line.strip_prefix("- ") {
            if let Some(previous) = owner.take() {
                declarations.push(previous);
            }
            if let Some((kind, description)) = line.split_once(": ") {
                owner = Some((kind.to_owned(), description.to_owned()));
            }
        } else if let Some((_, description)) = owner.as_mut() {
            description.push(' ');
            description.push_str(line.trim());
        }
    }
    if let Some(previous) = owner {
        declarations.push(previous);
    }
    for (kind, description) in declarations {
        let owner = kind.split_whitespace().next().unwrap_or("Graph");
        for name in description
            .split(',')
            .map(str::trim)
            .take_while(|s| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        {
            if !name.is_empty() {
                properties.push(json!({"ownerKind":owner,"name":name,"description":description}));
            }
        }
    }
    let mut actual = BTreeMap::<(String, String), ()>::new();
    let mut observed = false;
    if let Some(reader) = reader.as_deref_mut() {
        let scope = reader.scope().clone();
        for (domain, rows) in [
            (GraphElementKind::Node, &mut nodes),
            (GraphElementKind::Edge, &mut edges),
        ] {
            let mut after = None;
            loop {
                let page = reader
                    .read
                    .graph()
                    .graph_kinds(&scope, domain, after.as_deref(), 32)?;
                for row in page.items {
                    if !reader.budget.visit() {
                        break;
                    }
                    after = Some(row.kind.clone());
                    rows.push(row.kind);
                }
                if !page.has_more || reader.budget.exhausted.is_some() {
                    break;
                }
            }
        }
        super::super::scan::nodes(
            reader.read,
            &scope,
            &GraphNodeFilter::default(),
            &mut reader.budget,
            |node, budget| {
                let props = super::properties(&node);
                if let Some(object) = props.as_object() {
                    for key in object.keys() {
                        let identity = (node.node.kind.clone(), key.clone());
                        if !actual.contains_key(&identity)
                            && !budget.retain(identity.0.len() + identity.1.len() + 32)
                        {
                            return Ok(false);
                        }
                        actual.insert(identity, ());
                    }
                }
                Ok(true)
            },
        )?;
        observed = true;
    }
    for ((kind, name), ()) in actual {
        properties.push(json!({"ownerKind":kind,"name":name,"description":"Property observed in the selected immutable composition; its presence does not establish semantic analysis coverage."}));
    }
    nodes.sort();
    nodes.dedup();
    edges.sort();
    edges.dedup();
    let mut value = json!({"nodeKinds":nodes,"edgeKinds":edges,"properties":properties,"notes":[DECLARED],"origin":if observed{"Observed"}else{"Declared"},"stamp":metadata.stamp});
    if !observed {
        value["unavailableReason"] = json!("graph_not_indexed");
    }
    if let Some(reader) = reader {
        super::page_sections(
            reader,
            &mut value,
            &["nodeKinds", "edgeKinds", "properties", "notes"],
        )?;
    } else {
        super::paging::metadata(
            metadata,
            &mut value,
            &["nodeKinds", "edgeKinds", "properties", "notes"],
        )?;
    }
    Ok(GraphResult::SchemaDetails {
        value: model::wire(value)?,
    })
}
