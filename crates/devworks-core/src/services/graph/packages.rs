//! Package links derived only from definitions in the complete selected composition.
//! Structural manifests do not authorize cross-repository symbol-name guessing.
use super::{
    budget::Budget,
    dataset,
    scan::{self, Edge},
};
use crate::{ports::storage::*, services::sessions::Failure};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(super) struct Packages {
    definitions: Vec<GraphNode>,
    complete: bool,
}
fn field<'a>(node: &'a GraphNode, name: &str) -> Option<&'a str> {
    node.node.payload["properties"][name].as_str()
}
fn definition(node: &GraphNode) -> bool {
    let value = &node.node.payload["properties"]["is_definition"];
    value.as_bool() == Some(true)
        || value
            .as_str()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}
fn git_key(text: &str) -> String {
    let text = text
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_lowercase();
    text.strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"))
        .or_else(|| text.strip_prefix("ssh://"))
        .unwrap_or(&text)
        .replace("git@", "")
        .replace(':', "/")
}
impl Packages {
    pub fn load(
        read: &dyn ReadSnapshot,
        scope: &GraphScope,
        budget: &mut Budget,
    ) -> Result<Self, Failure> {
        let mut definitions = Vec::new();
        scan::nodes(
            read,
            scope,
            &GraphNodeFilter {
                kinds: vec!["Package".into()],
                ..Default::default()
            },
            budget,
            |node, budget| {
                if definition(&node) {
                    if !budget.retain(
                        node.node.payload.to_string().len() + std::mem::size_of::<GraphNode>(),
                    ) {
                        return Ok(false);
                    }
                    definitions.push(node);
                }
                Ok(true)
            },
        )?;
        Ok(Self {
            definitions,
            complete: budget.exhausted.is_none(),
        })
    }
    pub fn link(
        &self,
        read: &dyn ReadSnapshot,
        scope: &GraphScope,
        local: &Edge,
        budget: &mut Budget,
    ) -> Result<Option<Edge>, Failure> {
        if !self.complete || local.raw.edge.kind != "REFERENCES_PACKAGE" {
            return Ok(None);
        }
        let Some(dependency) = read.graph().graph_node(scope, &local.target)? else {
            return Ok(None);
        };
        if dependency.node.kind != "Package" || definition(&dependency) {
            return Ok(None);
        }
        let ecosystem = field(&dependency, "ecosystem");
        let name = field(&dependency, "package_name").unwrap_or(&dependency.node.name);
        let version = field(&dependency, "version");
        let eligible: Vec<_> = self
            .definitions
            .iter()
            .filter(|d| {
                d.repository_id != local.source.repository_id && field(d, "ecosystem") == ecosystem
            })
            .collect();
        let key = field(&dependency, "package_key");
        let mut candidates: Vec<_> = eligible
            .iter()
            .copied()
            .filter(|d| key.is_some_and(|key| field(d, "package_key") == Some(key)))
            .collect();
        let mut band = "high";
        if candidates.is_empty()
            && matches!(field(&dependency, "source_kind"), Some("git" | "remote"))
            && let Some(url) =
                field(&dependency, "remote_url").or_else(|| field(&dependency, "source"))
        {
            let key = git_key(url);
            for definition in &eligible {
                if !budget.visit() {
                    return Ok(None);
                }
                let repository = read.projects().repository(&definition.repository_id)?;
                if repository
                    .as_ref()
                    .and_then(|r| r.remote_url.as_ref())
                    .is_some_and(|url| git_key(url) == key)
                    && field(definition, "manifest_path").is_some_and(|path| !path.contains('/'))
                {
                    candidates.push(*definition);
                }
            }
        }
        if candidates.is_empty() && field(&dependency, "source_kind") == Some("path") {
            let names: Vec<_> = eligible
                .iter()
                .copied()
                .filter(|d| field(d, "package_name").unwrap_or(&d.node.name) == name)
                .collect();
            let external = field(&dependency, "external_path")
                .unwrap_or("")
                .replace('\\', "/")
                .to_lowercase();
            let segments: Vec<_> = external.split('/').collect();
            for definition in &names {
                if !budget.visit() {
                    return Ok(None);
                }
                if read
                    .projects()
                    .repository(&definition.repository_id)?
                    .is_some_and(|r| segments.contains(&r.name.to_lowercase().as_str()))
                {
                    candidates.push(*definition);
                }
            }
            if candidates.is_empty() && ecosystem != Some("swiftpm") {
                candidates = names;
            }
            band = "medium";
        }
        let repositories: BTreeSet<_> = candidates
            .iter()
            .map(|d| d.repository_id.as_str())
            .collect();
        if repositories.len() != 1 {
            return Ok(None);
        }
        if candidates.len() > 1 {
            let exact: Vec<_> = candidates
                .iter()
                .copied()
                .filter(|d| version.is_some() && field(d, "version") == version)
                .collect();
            if exact.len() == 1 {
                candidates = exact;
            } else {
                return Ok(None);
            }
        }
        let Some(target) = candidates.first() else {
            return Ok(None);
        };
        let mut raw = local.raw.clone();
        let target_id = dataset::identity(target);
        raw.edge.edge_id = format!(
            "derived-{}",
            crate::fingerprint::sha256_hex(
                format!(
                    "{}\0{}\0{}\0{}\0{}",
                    local.raw.repository_id.as_str(),
                    local.raw.edge.unit_key,
                    local.raw.edge.edge_id,
                    target.repository_id.as_str(),
                    target.node.node_id
                )
                .as_bytes()
            )
        );
        raw.edge.target_node_id = target.node.node_id.clone();
        let payload = raw
            .edge
            .payload
            .as_object_mut()
            .expect("stored edge object");
        payload.insert("targetRepositoryId".into(), json!(target.repository_id));
        payload.insert("targetUnit".into(), json!(target.node.unit_key));
        payload.insert("resolution".into(), json!("DerivedPackage"));
        payload.insert("confidenceBand".into(), json!(band));
        payload.insert(
            "confidence".into(),
            json!(if band == "high" { 0.9 } else { 0.6 }),
        );
        if !payload.get("properties").is_some_and(Value::is_object) {
            payload.insert("properties".into(), json!({}));
        }
        for key in ["ecosystem", "dependency_kind"] {
            if let Some(value) = dependency.node.payload["properties"].get(key) {
                payload["properties"][key] = value.clone();
            }
        }
        payload["properties"]["resolutionBasis"] = json!("selected-view-package-manifest");
        Ok(Some(Edge {
            raw,
            source: local.source.clone(),
            target: target_id,
        }))
    }
}
