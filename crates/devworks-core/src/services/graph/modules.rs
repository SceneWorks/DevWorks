//! Retained .NET project ownership: the deepest captured project directory wins.
//! All discovery is inside the exact composition; no filesystem/project-file reads occur here.
use super::{budget::Budget, scan};
use crate::ports::storage::*;
use crate::services::kernel::Failure;

fn captured_directory(unit: &str) -> &str {
    unit.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// Each projected node's project owner, read once per repository and unit.
#[derive(Default)]
pub(super) struct Owners {
    read: std::collections::BTreeMap<(String, String), Modules>,
}

impl Owners {
    /// `node`'s owner and whether it is ambiguous, reading the projects with `load` the first
    /// time its unit is seen. Every unit read is charged to `budget`; `None` once that runs out
    /// (`graph_memory_limit`).
    pub fn owner(
        &mut self,
        node: &GraphNode,
        budget: &mut Budget,
        load: impl FnOnce(&mut Budget) -> Result<Modules, Failure>,
    ) -> Result<Option<(Option<String>, bool)>, Failure> {
        let key = (
            node.repository_id.as_str().to_owned(),
            node.node.unit_key.clone(),
        );
        if Modules::needed(node) && !self.read.contains_key(&key) {
            if !budget.retain(
                key.0.len() + key.1.len() + std::mem::size_of::<((String, String), Modules)>() + 64,
            ) {
                return Ok(None);
            }
            let modules = load(budget)?;
            self.read.insert(key.clone(), modules);
        }
        let none = Modules::default();
        let (owner, ambiguous) = self.read.get(&key).unwrap_or(&none).owner(node);
        Ok(Some((owner.map(str::to_owned), ambiguous)))
    }
}

/// Retains one project node; `false` when the memory budget is spent.
fn push(modules: &mut Vec<Module>, node: &GraphNode, budget: &mut Budget) -> bool {
    let name = node.node.payload["properties"]["dotnet_project"]
        .as_str()
        .unwrap_or(&node.node.name);
    let directory = captured_directory(&node.node.unit_key).to_owned();
    if !budget.retain(node.repository_id.as_str().len() + directory.len() + name.len()) {
        return false;
    }
    modules.push(Module {
        repository: node.repository_id.as_str().into(),
        directory,
        name: name.into(),
    });
    true
}

pub(super) struct Module {
    repository: String,
    directory: String,
    name: String,
}

pub(super) struct Modules {
    entries: Vec<Module>,
    complete: bool,
}

impl Default for Modules {
    /// No project: what [`Modules::owner`] uses for a node it needs none for.
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            complete: true,
        }
    }
}

impl Modules {
    /// Every project of `scope`.
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
            |node, budget| Ok(push(&mut modules, &node, budget)),
        )?;
        Ok(Self {
            entries: modules,
            complete: budget.exhausted.is_none(),
        })
    }

    /// The exact captured directory of a uniquely identified module.
    pub fn directory(&self, repository: &str, name: &str) -> Option<&str> {
        let mut matches = self
            .entries
            .iter()
            .filter(|module| module.repository == repository && module.name == name)
            .map(|module| module.directory.as_str());
        let first = matches.next()?;
        matches.all(|directory| directory == first).then_some(first)
    }

    /// Whether [`Modules::owner`] reads the projects for `node`: it names no project of its own
    /// and is not a repository.
    pub fn needed(node: &GraphNode) -> bool {
        node.node.payload["properties"]["dotnet_project"]
            .as_str()
            .is_none()
            && node.node.kind != "Repository"
    }

    /// The projects that can own the nodes of `node`'s unit: those of its repository whose
    /// directory is the repository root, the unit itself, or a directory on the unit's path.
    pub fn chain(
        read: &dyn ReadSnapshot,
        scope: &GraphScope,
        node: &GraphNode,
        budget: &mut Budget,
    ) -> Result<Self, Failure> {
        let local = GraphScope {
            views: scope
                .views
                .iter()
                .filter(|view| view.repository_id == node.repository_id)
                .cloned()
                .collect(),
        };
        // Each directory as the store indexes a project's unit: through its last `/`. A project
        // unit without a directory reads "" (or "/" when its key starts with one).
        let path = node.node.unit_key.trim_start_matches('/');
        let mut directories = std::collections::BTreeSet::from([String::new(), "/".to_owned()]);
        for (at, _) in path.match_indices('/') {
            directories.insert(format!("{}/", &path[..at]));
        }
        directories.insert(format!("{path}/"));
        let directories: Vec<String> = directories.into_iter().collect();
        let mut modules = Vec::new();
        // A read names at most GRAPH_MAX_PAGE directories: a deeper unit's are read in batches,
        // and ownership is decided over all of their projects together.
        'batches: for batch in directories.chunks(usize::from(GRAPH_MAX_PAGE)) {
            let mut after = None;
            while !local.views.is_empty() {
                let page = read
                    .graph()
                    .graph_project_nodes(&local, batch, after.as_ref(), 32)?;
                for node in page.items {
                    if !budget.visit() {
                        break 'batches;
                    }
                    after = Some(GraphPosition {
                        repository_id: node.repository_id.clone(),
                        unit_key: node.node.unit_key.clone(),
                        item_id: node.node.node_id.clone(),
                    });
                    if !push(&mut modules, &node, budget) {
                        break 'batches;
                    }
                }
                if !page.has_more {
                    break;
                }
            }
        }
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
        let path = node.node.unit_key.trim_start_matches('/');
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

/// Every node of `scope` checked against the unit-ranking reads the lookups replaced: by node
/// ID (alone, with its unit, and through [`GraphRead::graph_node`]), by unit, by kind, and its project owner
/// from its directory chain against every project of the scope. Returns how many nodes were
/// checked and a description of each difference (none when they agree).
#[doc(hidden)]
pub fn node_lookup_parity(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
) -> Result<(usize, Vec<String>), Failure> {
    let budget = || {
        Budget::new(&crate::protocol::data::QueryLimits {
            max_results: 500,
            max_visited: 1_000_000,
            deadline_ms: 300_000,
        })
    };
    let pages = |filter: &GraphNodeFilter| -> Result<Vec<GraphNode>, StorageError> {
        let (mut out, mut after) = (Vec::new(), None);
        loop {
            let page = read
                .graph()
                .graph_nodes(scope, filter, after.as_ref(), 500)?;
            after = page.items.last().map(|n: &GraphNode| GraphPosition {
                repository_id: n.repository_id.clone(),
                unit_key: n.node.unit_key.clone(),
                item_id: n.node.node_id.clone(),
            });
            out.extend(page.items);
            if !page.has_more {
                return Ok(out);
            }
        }
    };
    // The reference: every visible node, through the scan that ranks every unit row.
    let all = pages(&GraphNodeFilter::default())?;
    let mut differences = Vec::new();
    let ids: std::collections::BTreeSet<&str> =
        all.iter().map(|n| n.node.node_id.as_str()).collect();
    for id in ids {
        let expected: Vec<_> = all
            .iter()
            .filter(|n| n.node.node_id == id)
            .cloned()
            .collect();
        let found = pages(&GraphNodeFilter {
            node_id: Some(id.into()),
            ..Default::default()
        })?;
        if found != expected {
            differences.push(format!("node {id}: {found:?} != {expected:?}"));
        }
    }
    let units: std::collections::BTreeSet<&str> =
        all.iter().map(|n| n.node.unit_key.as_str()).collect();
    for unit in units {
        let expected: Vec<_> = all
            .iter()
            .filter(|n| n.node.unit_key == unit)
            .cloned()
            .collect();
        let found = pages(&GraphNodeFilter {
            unit_key: Some(unit.into()),
            ..Default::default()
        })?;
        if found != expected {
            differences.push(format!("unit {unit}: {found:?} != {expected:?}"));
        }
    }
    let kinds: std::collections::BTreeSet<&str> =
        all.iter().map(|n| n.node.kind.as_str()).collect();
    for kind in kinds {
        let expected: Vec<_> = all
            .iter()
            .filter(|n| n.node.kind == kind)
            .cloned()
            .collect();
        let (mut found, mut after) = (Vec::new(), None);
        loop {
            let page = read
                .graph()
                .graph_nodes_of_kind(scope, kind, after.as_ref(), 7)?;
            after = page.items.last().map(|n: &GraphNode| GraphPosition {
                repository_id: n.repository_id.clone(),
                unit_key: n.node.unit_key.clone(),
                item_id: n.node.node_id.clone(),
            });
            found.extend(page.items);
            if !page.has_more {
                break;
            }
        }
        if found != expected {
            differences.push(format!("kind {kind}: {found:?} != {expected:?}"));
        }
    }
    let mut every = budget();
    let projects = Modules::load(read, scope, &mut every)?;
    for node in &all {
        let identity = super::dataset::identity(node);
        let one = read.graph().graph_node(scope, &identity)?;
        let unit = pages(&GraphNodeFilter {
            node_id: Some(node.node.node_id.clone()),
            unit_key: Some(node.node.unit_key.clone()),
            ..Default::default()
        })?;
        if one.as_ref() != Some(node) || unit != [node.clone()] {
            differences.push(format!("node {identity:?}: {one:?}, {unit:?}"));
        }
        let mut chained = budget();
        let chain = Modules::chain(read, scope, node, &mut chained)?;
        let (expected, found) = (projects.owner(node), chain.owner(node));
        if expected != found {
            differences.push(format!("owner of {identity:?}: {found:?} != {expected:?}"));
        }
    }
    Ok((all.len(), differences))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Id;
    use serde_json::json;
    fn symbol(path: &str) -> GraphNode {
        GraphNode {
            repository_id: Id::new("repo").unwrap(),
            view_id: Id::new("view").unwrap(),
            generation_id: Id::new("generation").unwrap(),
            content_digest: "a".repeat(64),
            node: StoredNode {
                unit_key: path.into(),
                node_id: "symbol".into(),
                kind: "Class".into(),
                name: "Example".into(),
                payload: json!({"properties":{}}),
            },
        }
    }
    #[test]
    fn each_unit_read_for_its_owner_is_charged_until_the_memory_limit() {
        let mut budget = Budget::new(&crate::protocol::data::QueryLimits {
            max_results: 1,
            max_visited: 1_000_000,
            deadline_ms: 300_000,
        });
        // Leave 64 KiB of the 16 MiB bound: a few hundred units fill it.
        assert!(budget.retain(16 * 1024 * 1024 - 64 * 1024));
        let mut owners = Owners::default();
        let mut read = 0;
        let mut stopped = None;
        for unit in 0..100_000 {
            let node = symbol(&format!("src/unit_{unit}.cs"));
            match owners
                .owner(&node, &mut budget, |_| {
                    read += 1;
                    Ok(Modules::default())
                })
                .unwrap()
            {
                Some(owner) => assert_eq!(owner, (None, false)),
                None => {
                    stopped = Some(unit);
                    break;
                }
            }
        }
        let stopped = stopped.expect("the cache reaches the memory limit");
        assert!(stopped > 0 && stopped < 1_000, "{stopped}");
        assert_eq!(read, stopped);
        assert_eq!(budget.exhausted, Some("graph_memory_limit"));
    }

    #[test]
    fn captured_git_directory_spelling_distinguishes_project_owners() {
        let modules = Modules {
            entries: [
                ("Foo/Upper.csproj", "Upper"),
                ("foo/Lower.csproj", "Lower"),
                ("Foo/Nested/Deep.csproj", "Deep"),
            ]
            .into_iter()
            .map(|(path, name)| Module {
                repository: "repo".into(),
                directory: captured_directory(path).into(),
                name: name.into(),
            })
            .collect(),
            complete: true,
        };
        for (path, name) in [
            ("Foo/Example.cs", Some("Upper")),
            ("foo/Example.cs", Some("Lower")),
            ("Foo/Nested/Example.cs", Some("Deep")),
            ("FOO/Example.cs", None),
            ("Foobar/Example.cs", None),
        ] {
            let node = symbol(path);
            assert_eq!(modules.owner(&node), (name, false), "{path}");
        }
    }
}
