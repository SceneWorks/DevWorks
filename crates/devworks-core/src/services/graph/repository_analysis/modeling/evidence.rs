//! Bounded graph relationships and first-party source for an explicitly requested model stage.
use super::*;
use crate::ports::storage::StoredNode;
use crate::services::artifacts::ArtifactService;
use crate::services::graph::repository_analysis::{
    capture::CapturedBytes,
    security::{Category, scanner},
};

const MAX_SOURCE_BLOCKS: usize = 128;

pub(super) struct ProjectNode {
    pub identity: GraphNodeIdentity,
    pub name: String,
    pub kind: String,
    pub span: Option<(usize, usize)>,
}

impl ProjectNode {
    pub fn from(node: &StoredNode, identity: GraphNodeIdentity) -> Self {
        let span = &node.payload["span"];
        let lines = span["startLine"]
            .as_u64()
            .zip(span["endLine"].as_u64())
            .filter(|(start, end)| *start > 0 && end >= start)
            .and_then(|(start, end)| {
                Some((usize::try_from(start).ok()?, usize::try_from(end).ok()?))
            });
        Self {
            identity,
            name: node.name.clone(),
            kind: node.kind.clone(),
            span: lines,
        }
    }
}

pub(super) type Outbound = BTreeMap<(String, String), Vec<(String, String)>>;

pub(super) fn relationships(
    read: &dyn ReadSnapshot,
    selected: &Selection,
    budget: &mut Budget,
) -> Result<Outbound, Failure> {
    let mut edges = Outbound::new();
    scan::edges(
        read,
        &selected.graph_scope(),
        &selected.layers,
        &[],
        budget,
        |edge, budget| {
            if let Ok(edge) = edge {
                let key = (
                    edge.source.repository_id.as_str().to_owned(),
                    edge.source.node_id,
                );
                let target = edge.target.node_id;
                let kind = edge.raw.edge.kind;
                if !budget.retain(target.len() + kind.len() + key.0.len() + key.1.len() + 64) {
                    return Ok(false);
                }
                edges.entry(key).or_default().push((kind, target));
            }
            Ok(true)
        },
    )?;
    Ok(edges)
}

fn owned_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    !scanner::sensitive(&path)
        && !scanner::nonproduction(&path)
        && !path.split('/').any(|part| {
            matches!(
                part,
                "vendor" | "third_party" | "third-party" | "node_modules" | "generated" | "dist"
            )
        })
        && path.rsplit_once('.').is_some_and(|(_, extension)| {
            matches!(
                extension,
                "c" | "h"
                    | "cc"
                    | "cpp"
                    | "hpp"
                    | "cs"
                    | "rs"
                    | "swift"
                    | "ts"
                    | "tsx"
                    | "js"
                    | "jsx"
                    | "py"
                    | "go"
                    | "rb"
                    | "java"
                    | "kt"
                    | "php"
            )
        })
}

fn high_signal(project: &str, node: &ProjectNode, outbound: usize) -> bool {
    let name = node.name.to_ascii_lowercase();
    let firmware = node
        .identity
        .unit_key
        .rsplit_once('.')
        .is_some_and(|(_, ext)| matches!(ext, "c" | "h" | "cc" | "cpp" | "hpp"));
    if firmware {
        owned_path(&node.identity.unit_key) || outbound >= 3
    } else {
        project.to_ascii_lowercase().contains(".services")
            || name.ends_with("controller")
            || name.ends_with("consumer")
            || matches!(node.kind.as_str(), "Route" | "Service")
            || outbound >= 5
    }
}

pub(super) struct SourceEvidence<'a> {
    pub read: &'a dyn ReadSnapshot,
    pub artifacts: &'a ArtifactService,
    pub selected: &'a Selection,
    pub outbound: &'a Outbound,
    pub all: bool,
    pub redactor: &'a Redactor,
    pub budget: &'a mut Budget,
    pub captured: BTreeMap<String, CapturedBytes>,
}

impl SourceEvidence<'_> {
    pub fn append(
        &mut self,
        repository: &Id,
        project: &str,
        nodes: &[ProjectNode],
        text: &mut String,
    ) -> Result<bool, Failure> {
        let layers = self
            .selected
            .layers
            .iter()
            .find(|layers| &layers.view.repository_id == repository)
            .ok_or_else(selection::unavailable)?;
        let degree = |node: &ProjectNode| {
            self.outbound
                .get(&(
                    repository.as_str().to_owned(),
                    node.identity.node_id.clone(),
                ))
                .map_or(0, Vec::len)
        };
        let mut nodes: Vec<_> = nodes
            .iter()
            .filter(|node| {
                node.span.is_some()
                    && owned_path(&node.identity.unit_key)
                    && (self.all || high_signal(project, node, degree(node)))
            })
            .collect();
        nodes.sort_by_key(|node| {
            (
                std::cmp::Reverse(high_signal(project, node, degree(node))),
                std::cmp::Reverse(degree(node)),
            )
        });
        let mut clipped = nodes.len() > MAX_SOURCE_BLOCKS;
        let mut files = BTreeMap::new();
        for node in nodes.into_iter().take(MAX_SOURCE_BLOCKS) {
            if !self.budget.visit() {
                return Ok(true);
            }
            let path = &node.identity.unit_key;
            if !files.contains_key(path) {
                let captured = self
                    .captured
                    .entry(repository.as_str().to_owned())
                    .or_default();
                let safe =
                    match captured.source(self.read, self.artifacts, layers, path, self.budget)? {
                        Ok((_, source)) => {
                            let mut findings = Vec::new();
                            scanner::scan(path, &source, self.budget, &mut findings);
                            if self.budget.exhausted.is_some() {
                                return Ok(true);
                            }
                            (!findings
                                .iter()
                                .any(|finding| finding.category == Category::Secret)
                                && self.redactor.redact_text(&source) == source
                                && self.budget.retain(source.len()))
                            .then_some(source)
                        }
                        Err(_) => {
                            clipped = true;
                            None
                        }
                    };
                files.insert(path, safe);
            }
            let Some(source) = files[path].as_ref() else {
                continue;
            };
            let Some((start, end)) = node.span else {
                continue;
            };
            // Match the captured span's inclusive line boundary. Keep each excerpt an inert JSON value.
            let lines = source
                .lines()
                .skip(start - 1)
                .take(end - start + 1)
                .map(str::trim_end)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();
            if lines.is_empty() {
                continue;
            }
            let entry = json!({
                "sourceNode": node.identity.node_id,
                "capture": "exact view",
                "source": lines.join("\n"),
            })
            .to_string();
            if !append(text, &entry) {
                clipped = true;
            }
        }
        Ok(clipped)
    }
}

pub(super) fn append(text: &mut String, entry: &str) -> bool {
    if SYSTEM.len() + text.len() + entry.len() + 1 > MAX_PROMPT_BYTES {
        return false;
    }
    text.push_str(entry);
    text.push('\n');
    true
}
