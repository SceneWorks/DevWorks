//! Resolve explicit SCIP and npm export identities within the selected composition.
use super::super::{budget::Budget, dataset, scan};
use super::{http, links::Link};
use crate::ports::storage::{
    GraphNode, GraphNodeFilter, GraphNodeIdentity, GraphScope, ReadSnapshot,
};
use crate::services::kernel::Failure;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const DEFINITION_KINDS: &[&str] = &[
    "Component",
    "Class",
    "Constructor",
    "Function",
    "Method",
    "Property",
    "Interface",
    "Enum",
    "Struct",
    "Namespace",
    "Module",
    "Trait",
    "Protocol",
    "Type",
    "TypeAlias",
    "Union",
    "Field",
    "StaticField",
    "StaticProperty",
    "Constant",
    "StaticMethod",
    "AbstractMethod",
    "TraitMethod",
    "Symbol",
];

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Symbol {
    Scip(String),
    Npm {
        package: String,
        import: String,
        export: String,
        member: String,
    },
}
impl Symbol {
    fn bytes(&self) -> usize {
        match self {
            Self::Scip(scip) => scip.len(),
            Self::Npm {
                package,
                import,
                export,
                member,
            } => package.len() + import.len() + export.len() + member.len(),
        }
    }
}

struct Reference {
    source: GraphNodeIdentity,
    kind: String,
    identity: Symbol,
}

#[derive(Default)]
pub(super) struct References {
    refs: Vec<Reference>,
}

impl References {
    pub(super) fn record(
        &mut self,
        edge: &scan::Edge,
        read: &dyn ReadSnapshot,
        scope: &GraphScope,
        budget: &mut Budget,
    ) -> Result<bool, Failure> {
        let kind = edge.raw.edge.kind.as_str();
        if !matches!(
            kind,
            "CALLS" | "USES_TYPE" | "IMPLEMENTS" | "RENDERS" | "INJECTS"
        ) {
            return Ok(true);
        }
        if !budget.visit() {
            return Ok(false);
        }
        let Some(target) = read.graph().graph_node(scope, &edge.target)? else {
            return Ok(true);
        };
        if target.node.kind != "ExternalSymbol" {
            return Ok(true);
        }
        let props = &target.node.payload["properties"];
        let identity = if matches!(kind, "CALLS" | "USES_TYPE" | "IMPLEMENTS")
            && http::string(props, "source") == Some("scip")
            && http::boolean(props, "scip_external")
        {
            http::nonempty(props, "scip_symbol").map(|symbol| Symbol::Scip(symbol.into()))
        } else if matches!(kind, "CALLS" | "USES_TYPE" | "RENDERS" | "INJECTS")
            && target.node.payload["qualifiedName"]
                .as_str()
                .is_some_and(|name| name.starts_with("npm-symbol:"))
        {
            npm(props)
        } else {
            None
        };
        let Some(identity) = identity else {
            return Ok(true);
        };
        if !budget
            .retain(identity.bytes() + edge.source.unit_key.len() + edge.source.node_id.len() + 192)
        {
            return Ok(false);
        }
        self.refs.push(Reference {
            source: edge.source.clone(),
            kind: kind.into(),
            identity,
        });
        Ok(true)
    }

    pub(super) fn resolve(
        self,
        read: &dyn ReadSnapshot,
        scope: &GraphScope,
        budget: &mut Budget,
        mut retain: impl FnMut(Link, &mut Budget) -> bool,
    ) -> Result<(), Failure> {
        if self.refs.is_empty() || budget.exhausted.is_some() {
            return Ok(());
        }
        let wanted: BTreeSet<_> = self
            .refs
            .iter()
            .map(|reference| &reference.identity)
            .collect();
        let mut definitions = BTreeMap::<Symbol, Vec<GraphNodeIdentity>>::new();
        scan::nodes(
            read,
            scope,
            &GraphNodeFilter {
                kinds: DEFINITION_KINDS
                    .iter()
                    .map(|kind| (*kind).to_owned())
                    .collect(),
                ..Default::default()
            },
            budget,
            |node, budget| {
                for identity in supported(&node)
                    .into_iter()
                    .filter(|identity| wanted.contains(identity))
                {
                    if !budget.retain(
                        identity.bytes() + node.node.unit_key.len() + node.node.node_id.len() + 192,
                    ) {
                        return Ok(false);
                    }
                    let entry = definitions.entry(identity).or_default();
                    let node = dataset::identity(&node);
                    if !entry.contains(&node) {
                        entry.push(node);
                    }
                }
                Ok(true)
            },
        )?;
        if budget.exhausted.is_some() {
            return Ok(());
        }
        for reference in self.refs {
            if !budget.visit() {
                break;
            }
            let found = definitions
                .get(&reference.identity)
                .map_or(&[][..], Vec::as_slice);
            let rust = matches!(reference.identity, Symbol::Scip(_));
            let mut candidates = found
                .iter()
                .filter(|node| !rust || node.repository_id != reference.source.repository_id);
            let first = candidates.next();
            let (target, diagnostic) = match (first, candidates.next()) {
                (Some(_), Some(_)) => (
                    None,
                    Some(if rust {
                        "AmbiguousScipSymbol"
                    } else {
                        "AmbiguousNpmExport"
                    }),
                ),
                (Some(target), None) if target.repository_id == reference.source.repository_id => {
                    continue;
                }
                (Some(target), None) => (Some(target.clone()), None),
                _ => (
                    None,
                    Some(if rust {
                        "ScipDefinitionUnavailable"
                    } else {
                        "NpmExportUnavailable"
                    }),
                ),
            };
            let inferred = target.is_some();
            let link = Link {
                source: reference.source,
                target,
                kind: reference.kind,
                inferred,
                confidence: if inferred {
                    if rust { 0.95 } else { 0.9 }
                } else {
                    0.0
                },
                matched_by: if rust {
                    "ScipSymbol".into()
                } else {
                    "NpmExport".into()
                },
                diagnostic: diagnostic.map(str::to_owned),
            };
            if !retain(link, budget) {
                break;
            }
        }
        Ok(())
    }
}

fn npm(props: &Value) -> Option<Symbol> {
    let package = http::nonempty(props, "package_name")?;
    let import = http::nonempty(props, "import_path")?;
    let export = http::nonempty(props, "export_name")?;
    if import != package
        && !import
            .strip_prefix(package)
            .is_some_and(|rest| rest.starts_with('/'))
    {
        return None;
    }
    Some(Symbol::Npm {
        package: package.into(),
        import: import.into(),
        export: export.into(),
        member: http::string(props, "member_path").unwrap_or("").into(),
    })
}

fn supported(node: &GraphNode) -> Vec<Symbol> {
    let props = &node.node.payload["properties"];
    let mut definitions = Vec::new();
    if http::string(props, "source") == Some("scip")
        && !http::boolean(props, "scip_external")
        && let Some(name) = http::nonempty(props, "scip_symbol")
    {
        definitions.push(Symbol::Scip(name.into()));
    }
    let aliases = &props["npm_exports"];
    let parsed;
    let aliases = if let Some(text) = aliases.as_str() {
        parsed = serde_json::from_str::<Value>(text).ok();
        parsed.as_ref().and_then(Value::as_array)
    } else {
        aliases.as_array()
    };
    if let Some(aliases) = aliases {
        definitions.extend(aliases.iter().filter_map(npm));
    }
    definitions
}

pub(super) fn has_export(node: &GraphNode) -> bool {
    DEFINITION_KINDS.contains(&node.node.kind.as_str()) && !supported(node).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn export_identity_keeps_subpath_name_and_member_and_rejects_other_package_prefixes() {
        let identity = |path, member| {
            npm(&json!({
                "package_name":"@example/lib", "import_path":path, "export_name":"result", "member_path":member
            }))
        };
        assert!(identity("@example/lib-sibling", "").is_none());
        assert_ne!(
            identity("@example/lib", "member"),
            identity("@example/lib", "")
        );
        assert_ne!(
            identity("@example/lib", ""),
            identity("@example/lib/subpath", "")
        );
    }
}
