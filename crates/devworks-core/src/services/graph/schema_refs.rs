//! The code that uses a database table or column: the `USES_TABLE` and `USES_COLUMN` edges into
//! the `DatabaseTable` nodes of that name, in the published graph view of every repository the
//! caller may read in its scope. A node is found through the qualified-name node index and its
//! uses through the indexed incident reads, so the work follows the uses, not the repositories'
//! size.
use crate::context::InvocationContext;
use crate::ports::storage::*;
use crate::protocol::Id;
use crate::protocol::data::DataScope;
use crate::services::indexing::views;
use crate::services::kernel::Failure;

/// One use of a table (or one of its columns) by a code definition, with its evidence.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CodeReference {
    pub repository_id: Id,
    pub view_id: Id,
    pub unit_key: String,
    pub node_id: String,
    pub node_name: String,
    pub node_kind: String,
    pub edge_id: String,
    pub edge_kind: String,
    pub table: String,
    pub column: Option<String>,
    pub extractor: String,
    pub line: u64,
    pub snippet: String,
    pub statement: Option<String>,
}

const NODE_PAGE: u16 = 500;

/// Every use of a table named one of `tables` (lowercased, `schema.table` or `table`), or of
/// `column` of it, in the repositories of `scope` that `ctx` may read, ordered by repository,
/// unit and edge.
pub(crate) fn code_references(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    scope: &DataScope,
    tables: &[String],
    column: Option<&str>,
) -> Result<Vec<CodeReference>, Failure> {
    let project = match scope {
        DataScope::Project { project_id } => Some(project_id.clone()),
        DataScope::Workspace => None,
    };
    let filter = RepositoryFilter {
        project,
        text: None,
        languages: Vec::new(),
        include_deleted: false,
    };
    let mut found = Vec::new();
    let mut after = None;
    loop {
        let repositories = read.projects().repositories(&filter, after.as_ref(), 128)?;
        let more = repositories.len() == 128;
        for repository in repositories {
            after = Some(repository.id.clone());
            // Only repositories the caller may read contribute; the others stay invisible.
            if super::scope::authorize(read, ctx, scope, &repository.id).is_err() {
                continue;
            }
            let Some(published) = read.indexing().published(&repository.id)? else {
                continue;
            };
            let layers = views::layers(read.indexing(), &published.view_id)?;
            in_view(read, &layers, tables, column, &mut found)?;
        }
        if !more {
            break;
        }
    }
    found.sort_by(|a, b| {
        (a.repository_id.as_str(), &a.unit_key, &a.edge_id).cmp(&(
            b.repository_id.as_str(),
            &b.unit_key,
            &b.edge_id,
        ))
    });
    Ok(found)
}

fn in_view(
    read: &dyn ReadSnapshot,
    layers: &views::ViewLayers,
    tables: &[String],
    column: Option<&str>,
    found: &mut Vec<CodeReference>,
) -> Result<(), Failure> {
    let repository = &layers.view.repository_id;
    let scope = GraphScope {
        views: vec![GraphViewBinding {
            repository_id: repository.clone(),
            view_id: layers.view.view_id.clone(),
            view_digest: layers.view.view_digest.clone(),
        }],
    };
    for table in tables {
        let qualified = format!("db-table:{table}");
        for generation in &layers.layers {
            // Each file naming the table has its own node: page through all of them.
            let mut after: Option<(String, String)> = None;
            loop {
                let page = read.indexing().nodes_qualified_after(
                    generation,
                    &qualified,
                    after.as_ref().map(|(u, n)| (u.as_str(), n.as_str())),
                    NODE_PAGE,
                )?;
                let more = page.len() == usize::from(NODE_PAGE);
                for node in page {
                    after = Some((node.unit_key.clone(), node.node_id.clone()));
                    // A unit counts in the layer that decides it: the highest one mentioning it.
                    let visible = views::resolve_unit(read.indexing(), layers, &node.unit_key)?
                        .is_some_and(|(g, _)| &g == generation);
                    if node.kind != "DatabaseTable" || !visible {
                        continue;
                    }
                    let target = GraphNodeIdentity {
                        repository_id: repository.clone(),
                        unit_key: node.unit_key.clone(),
                        node_id: node.node_id.clone(),
                    };
                    uses_of(read, &scope, layers, &target, table, column, found)?;
                }
                if !more {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn uses_of(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
    layers: &views::ViewLayers,
    target: &GraphNodeIdentity,
    table: &str,
    column: Option<&str>,
    found: &mut Vec<CodeReference>,
) -> Result<(), Failure> {
    let kind = if column.is_some() {
        "USES_COLUMN"
    } else {
        "USES_TABLE"
    };
    let filter = GraphIncidentFilter {
        end: GraphEdgeEnd::Target,
        nodes: vec![target.clone()],
        kinds: vec![kind.into()],
    };
    let mut after = None;
    loop {
        // A published view is one layer, whose stored edges are already its links.
        let Some(page) =
            read.graph()
                .graph_incident_edges(scope, &filter, after.as_ref(), NODE_PAGE)?
        else {
            return Ok(());
        };
        for edge in page.items {
            after = Some(GraphPosition {
                repository_id: edge.repository_id.clone(),
                unit_key: edge.edge.unit_key.clone(),
                item_id: edge.edge.edge_id.clone(),
            });
            let props = &edge.edge.payload["properties"];
            let text = |key: &str| props[key].as_str().map(str::to_owned);
            if column.is_some() && text("column").as_deref() != column {
                continue;
            }
            let source_unit = edge.edge.payload["sourceUnit"]
                .as_str()
                .unwrap_or(&edge.edge.unit_key)
                .to_owned();
            let source = read.graph().graph_node(
                scope,
                &GraphNodeIdentity {
                    repository_id: target.repository_id.clone(),
                    unit_key: source_unit.clone(),
                    node_id: edge.edge.source_node_id.clone(),
                },
            )?;
            let (node_name, node_kind) = source
                .map(|n| (n.node.name, n.node.kind))
                .unwrap_or_default();
            found.push(CodeReference {
                repository_id: target.repository_id.clone(),
                view_id: layers.view.view_id.clone(),
                unit_key: source_unit,
                node_id: edge.edge.source_node_id.clone(),
                node_name,
                node_kind,
                edge_id: edge.edge.edge_id.clone(),
                edge_kind: edge.edge.kind.clone(),
                table: table.to_owned(),
                column: text("column"),
                extractor: text("extractor").unwrap_or_default(),
                line: props["line"].as_u64().unwrap_or(0),
                snippet: text("snippet").unwrap_or_default(),
                statement: text("statement"),
            });
        }
        if !page.has_more {
            return Ok(());
        }
    }
}
