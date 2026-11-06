//! Native graph frames use the same scoped, pinned operations as agent tools.
use crate::{
    backend::Backend,
    ops::{self, Failure},
};
use devworks_core::{
    gateway::QueryOutcome,
    protocol::{data::GraphPage, records::CodeNode, results::OperationResult},
};
use serde_json::{Value, json};

pub const PAGE_SIZE: u16 = 50;

pub fn repository_nodes(
    backend: &dyn Backend,
    scope: Value,
    mut body: Value,
    view: Option<Value>,
    cursor: Option<&str>,
) -> Vec<u8> {
    body["scope"] = scope;
    match view {
        Some(view) => body["views"] = view,
        None => body["current"] = json!(true),
    }
    body["page"] = json!({"limit":PAGE_SIZE});
    if let Some(cursor) = cursor {
        body["page"]["cursor"] = json!(cursor);
    }
    ops::at_revision(ops::query_frame(backend, "repositories.nodes", body), 3)
}

pub fn read(
    backend: &dyn Backend,
    scope: Value,
    view: Option<Value>,
    query: Value,
    cursor: Option<&str>,
) -> Vec<u8> {
    let mut frame = json!({"protocol":ops::PROTOCOL,"schemaRevision":3,"queryId":backend.token("q"),
        "scope":scope,"query":query,"limits":{"maxResults":PAGE_SIZE,"maxVisited":20000,"deadlineMs":10000}});
    match view {
        Some(view) => frame["view"] = view,
        None => frame["current"] = json!(true),
    }
    if let Some(cursor) = cursor {
        frame["cursor"] = json!(cursor);
    }
    frame.to_string().into_bytes()
}

pub fn page(outcome: QueryOutcome) -> Result<GraphPage, Failure> {
    match ops::query_result(outcome)?.0 {
        OperationResult::CodeSearch { value }
        | OperationResult::CodeArchitecture { value }
        | OperationResult::CodeDependency { value }
        | OperationResult::CodeImpact { value }
        | OperationResult::CodeSource { value }
        | OperationResult::CodeTraverse { value }
        | OperationResult::CodeGraphScopeStatus { value }
        | OperationResult::CodeOverview { value }
        | OperationResult::CodeNodesByFile { value }
        | OperationResult::CodeNodeDetail { value }
        | OperationResult::CodeFileImpact { value }
        | OperationResult::CodeGraphSchema { value }
        | OperationResult::CodeCallPath { value }
        | OperationResult::CodeDataLineage { value }
        | OperationResult::CodeConsumers { value }
        | OperationResult::CodePublishers { value }
        | OperationResult::CodeSnippet { value }
        | OperationResult::CodeRepositoryDependencies { value }
        | OperationResult::RepositoriesNodes { value } => Ok(value),
        other => Err(ops::unexpected("Graph", &other)),
    }
}

pub fn trust(backend: &dyn Backend, node: &CodeNode, trusted: bool) -> Vec<u8> {
    json!({"protocol":ops::PROTOCOL,"schemaRevision":3,"requestKey":backend.token("rk"),
        "command":{"kind":"SetTrust","repositoryId":node.repository_id,"nodeId":node.node_id,
            "expectedRevision":node.trust_revision,"viewId":node.view_id,"unitKey":node.unit_key,
            "expectedViewDigest":node.view_digest,"expectedContentIdentity":node.content_identity,
            "trusted":trusted}})
    .to_string()
    .into_bytes()
}
