use super::{
    dataset, model,
    read::{Position, Reader},
};
use crate::ports::storage::*;
use crate::protocol::Id;
use crate::protocol::data::GraphResult;
use crate::protocol::records::{
    CodeNodesResult, CodeSearchHit, CodeSearchMode, CodeSearchRepository, CodeSearchResult,
};
use crate::protocol::support::BoundedList;
use crate::services::sessions::Failure;
use serde_json::json;
use std::cmp::Ordering;

pub(super) struct Search<'a> {
    pub mode: CodeSearchMode,
    pub text: &'a str,
    pub repository: Option<&'a Id>,
    pub kinds: Vec<String>,
    pub module: Option<&'a str>,
}

/// Retained LIKE substring semantics (%), with the MCP's * spelling. `_` is one
/// character. The core matcher avoids SQLite's ASCII-only case folding on Unicode names.
fn matches_with(pattern: &str, text: &str, mut keep_going: impl FnMut() -> bool) -> bool {
    let pattern = format!("*{}*", pattern.to_lowercase().replace('%', "*"));
    let (p, t): (Vec<_>, Vec<_>) = (
        pattern.chars().collect(),
        text.to_lowercase().chars().collect(),
    );
    let (mut i, mut j, mut star, mut saved) = (0, 0, None, 0);
    let mut steps = 0usize;
    while j < t.len() {
        steps += 1;
        if steps.is_multiple_of(256) && !keep_going() {
            return false;
        }
        if i < p.len() && (p[i] == '_' || p[i] == t[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            i += 1;
            saved = j;
        } else if let Some(at) = star {
            saved += 1;
            j = saved;
            i = at + 1;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == '*' {
        i += 1;
    }
    i == p.len()
}

fn node_order(a: &GraphNode, b: &GraphNode) -> Ordering {
    a.node
        .name
        .encode_utf16()
        .cmp(b.node.name.encode_utf16())
        .then_with(|| dataset::node_key(a).cmp(&dataset::node_key(b)))
}

const UNIFIED_KINDS: &[&str] = &[
    "Class",
    "Interface",
    "Enum",
    "Struct",
    "Record",
    "Method",
    "Route",
    "Service",
    "Event",
    "Queue",
    "Exchange",
    "Job",
    "Table",
    "View",
    "StoredProcedure",
    "Component",
    "DotnetProject",
];

pub(super) fn query(
    reader: &mut Reader<'_>,
    query: Search<'_>,
    nodes_only: bool,
) -> Result<GraphResult, Failure> {
    let scope = match query.repository {
        Some(id) => reader.scoped_repository(id)?,
        None => reader.scope().clone(),
    };
    let mut rows = reader.raw_nodes(
        &scope,
        &GraphNodeFilter {
            kinds: query.kinds,
            ..Default::default()
        },
    )?;
    let mut selected = Vec::new();
    for node in rows.drain(..) {
        if !reader.budget.visit() {
            break;
        }
        if !matches_with(query.text, &node.node.name, || reader.budget.visit()) {
            continue;
        }
        if matches!(query.mode, CodeSearchMode::Unified)
            && !UNIFIED_KINDS.contains(&node.node.kind.as_str())
        {
            continue;
        }
        if let Some(module) = query.module
            && !reader.module_matches(&node, module)?
        {
            continue;
        }
        selected.push(node);
    }
    let wildcard = query.text.contains(['%', '*']);
    // Trust is an exact view/content annotation. Keep untrusted results visible, as retained.
    let mut trust = std::collections::HashMap::new();
    if matches!(query.mode, CodeSearchMode::RankedNodes) && wildcard {
        for node in &selected {
            if !reader.budget.visit() {
                break;
            }
            let identity = dataset::identity(node);
            let annotation =
                reader
                    .read
                    .graph()
                    .graph_trust(&scope, &identity, &node.content_digest)?;
            let score = node.node.payload["trustScore"].as_f64().unwrap_or(0.5);
            trust.insert(
                dataset::node_key(node),
                (annotation.is_some_and(|a| !a.trusted), score),
            );
        }
    }
    let needle = query.text.to_lowercase();
    let exact = |n: &GraphNode| n.node.name.to_lowercase() == needle;
    selected.sort_by(|a, b| {
        let exact_order = if matches!(query.mode, CodeSearchMode::NodeList) {
            Ordering::Equal
        } else {
            exact(b).cmp(&exact(a))
        };
        exact_order
            .then_with(|| {
                if matches!(query.mode, CodeSearchMode::RankedNodes) && wildcard {
                    let a = trust
                        .get(&dataset::node_key(a))
                        .copied()
                        .unwrap_or((false, 0.5));
                    let b = trust
                        .get(&dataset::node_key(b))
                        .copied()
                        .unwrap_or((false, 0.5));
                    a.0.cmp(&b.0).then_with(|| b.1.total_cmp(&a.1))
                } else {
                    Ordering::Equal
                }
            })
            .then_with(|| node_order(a, b))
    });
    if matches!(query.mode, CodeSearchMode::Unified) && query.text.trim().is_empty() {
        selected.clear();
    }
    let low = selected.iter().filter(|n| dataset::low_node(n)).count() as u64;
    if nodes_only {
        let total = if reader.budget.exhausted.is_none() {
            Some(model::count(selected.len() as u64)?)
        } else {
            None
        };
        let nodes = reader.page_nodes(&selected)?;
        return Ok(GraphResult::CodeNodes {
            value: CodeNodesResult {
                nodes: BoundedList(nodes),
                total,
                low_confidence_count: model::count(low)?,
                selected_node_id: None,
            },
        });
    }
    enum Hit {
        Node(usize),
        Repository(CodeSearchRepository),
    }
    let mut hits: Vec<(String, bool, Hit)> = selected
        .iter()
        .enumerate()
        .map(|(i, n)| (n.node.name.clone(), false, Hit::Node(i)))
        .collect();
    if matches!(query.mode, CodeSearchMode::Unified) && !query.text.trim().is_empty() {
        for binding in &scope.views {
            if !reader.budget.visit() {
                break;
            }
            if let Some(repository) = reader.read.projects().repository(&binding.repository_id)?
                && matches_with(query.text, &repository.name, || reader.budget.visit())
            {
                let language = if repository.languages.len() == 1 {
                    Some(model::wire(json!(repository.languages[0]))?)
                } else {
                    None
                };
                hits.push((
                    repository.name.clone(),
                    true,
                    Hit::Repository(CodeSearchRepository {
                        repository_id: repository.id,
                        name: model::wire(json!(repository.name))?,
                        description: None,
                        language,
                        framework: None,
                        foundational: false,
                    }),
                ));
            }
        }
        hits.sort_by(|a, b| {
            (b.0.to_lowercase() == query.text.to_lowercase())
                .cmp(&(a.0.to_lowercase() == query.text.to_lowercase()))
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| a.0.encode_utf16().cmp(b.0.encode_utf16()))
        });
    }
    if reader.position.offset > hits.len() {
        return Err(StorageError::CursorExpired.into());
    }
    let total = if reader.budget.exhausted.is_none() {
        Some(model::count(hits.len() as u64)?)
    } else {
        None
    };
    let mut out = Vec::new();
    for (index, (_, _, hit)) in hits.into_iter().enumerate().skip(reader.position.offset) {
        if out.len() == reader.limit {
            reader.next = Some(Position {
                offset: index,
                ..Default::default()
            });
            break;
        }
        let hit = match hit {
            Hit::Node(i) => CodeSearchHit::Node {
                node: reader.node(&selected[i], None)?.0,
            },
            Hit::Repository(repository) => CodeSearchHit::Repository { repository },
        };
        if !reader.retain_wire(&hit)? {
            reader.next = Some(Position {
                offset: index,
                ..Default::default()
            });
            break;
        }
        out.push(hit);
    }
    Ok(GraphResult::CodeSearch {
        value: CodeSearchResult {
            mode: query.mode,
            items: BoundedList(out),
            total,
            low_confidence_count: model::count(low)?,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn matches(pattern: &str, text: &str) -> bool {
        matches_with(pattern, text, || true)
    }
    #[test]
    fn name_patterns_retain_substrings_wildcards_and_unicode() {
        assert!(matches("helper", "run_helper_async"));
        assert!(matches("get%user", "GET active USER"));
        assert!(matches("get*user", "GET active USER"));
        assert!(matches("café", "CafÉHandler"));
        assert!(matches("a_b", "a世b"));
        assert!(!matches("a_b", "ab"));
    }
}
