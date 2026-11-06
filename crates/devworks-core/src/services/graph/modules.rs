//! Retained .NET project ownership: the deepest captured project directory wins.
//! All discovery is inside the exact composition; no filesystem/project-file reads occur here.
use super::{budget::Budget, scan};
use crate::ports::storage::*;
use crate::services::sessions::Failure;

pub(super) struct Module {
    repository: String,
    directory: String,
    name: String,
}

pub(super) struct Modules {
    entries: Vec<Module>,
    complete: bool,
}

impl Modules {
    pub fn load(
        read: &dyn ReadSnapshot,
        scope: &GraphScope,
        budget: &mut Budget,
    ) -> Result<Self, Failure> {
        let mut modules = Vec::new();
        scan::nodes(
            read,
            scope,
            &GraphNodeFilter {
                kinds: vec!["DotnetProject".into()],
                ..Default::default()
            },
            budget,
            |node, budget| {
                let name = node.node.payload["properties"]["dotnet_project"]
                    .as_str()
                    .unwrap_or(&node.node.name);
                let directory = node
                    .node
                    .unit_key
                    .rsplit_once('/')
                    .map_or("", |(parent, _)| parent)
                    .to_lowercase();
                if !budget.retain(node.repository_id.as_str().len() + directory.len() + name.len())
                {
                    return Ok(false);
                }
                modules.push(Module {
                    repository: node.repository_id.as_str().into(),
                    directory,
                    name: name.into(),
                });
                Ok(true)
            },
        )?;
        Ok(Self {
            entries: modules,
            complete: budget.exhausted.is_none(),
        })
    }

    /// A tied directory with distinct project names is ambiguous, rather than dependent on
    /// enumeration order. The caller retains a coverage gap and never guesses membership.
    pub fn owner<'a>(&'a self, node: &'a GraphNode) -> (Option<&'a str>, bool) {
        if let Some(explicit) = node.node.payload["properties"]["dotnet_project"].as_str() {
            return (Some(explicit), false);
        }
        if node.node.kind == "Repository" {
            return (None, false);
        }
        if !self.complete {
            return (None, true);
        }
        let path = node.node.unit_key.trim_start_matches('/').to_lowercase();
        let mut best: Option<&Module> = None;
        let mut ambiguous = false;
        for module in &self.entries {
            if module.repository != node.repository_id.as_str()
                || !(module.directory.is_empty()
                    || path == module.directory
                    || path
                        .strip_prefix(&module.directory)
                        .is_some_and(|tail| tail.starts_with('/')))
            {
                continue;
            }
            match best {
                None => {
                    best = Some(module);
                    ambiguous = false;
                }
                Some(previous) if module.directory.len() > previous.directory.len() => {
                    best = Some(module);
                    ambiguous = false;
                }
                Some(previous)
                    if module.directory.len() == previous.directory.len()
                        && module.name != previous.name =>
                {
                    ambiguous = true
                }
                _ => {}
            }
        }
        (
            if ambiguous {
                None
            } else {
                best.map(|m| m.name.as_str())
            },
            ambiguous,
        )
    }
}
