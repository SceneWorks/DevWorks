//! Native graph frames use the same scoped, pinned operations as agent tools.
use crate::{
    backend::{Backend, screen_slot},
    frame::{self, Negotiate},
    ops::{self, Failure},
};
use devworks_core::{
    gateway::QueryOutcome,
    operations::{OperationInput as In, code, repositories},
    protocol::{
        Id,
        data::{
            DataScope, GraphPage, GraphQuery, GraphReadRequest, IndexCommand, IndexCommandRequest,
            QueryLimits, ViewSelection,
        },
        records::CodeNode,
        results::OperationResult,
        support::{Content, Label},
    },
};

/// The graph requests the screen waits for. The names are persisted (see [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum GraphSlot {
    /// A graph read (read sequence).
    GraphRead(u64),
    /// A trust change of one node (node ID).
    GraphTrust(String),
    /// A scoped analysis report read.
    AnalysisRead(u64),
    /// One command slot per displayed graph scope.
    AnalysisCommand(String),
    /// An authorized report artifact preview.
    AnalysisEvidence(u64),
}
screen_slot!(GraphSlot, Graph);

pub const PAGE_SIZE: u16 = 50;

/// What a graph read asks for.
#[derive(Clone)]
pub enum Query {
    /// `repositories.nodes`: a repository's nodes, narrowed by kind and module.
    Nodes {
        repository_id: Id,
        node_kind: Option<Label>,
        internal_module: Option<Content>,
    },
    /// Any `code.*` read.
    Code(GraphQuery),
}

/// The frame of a graph read of `operation` over `scope`: the explicit `view`, or the current
/// view when it is `None`. An input that did not convert (`None`) is no bytes, which the gateway
/// rejects like any malformed request.
pub fn read(
    backend: &dyn Backend,
    operation: &str,
    scope: Option<DataScope>,
    view: Option<Option<ViewSelection>>,
    query: Option<Query>,
    cursor: Option<&str>,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        let (scope, view) = (scope?, view?);
        let (views, current) = match view {
            Some(view) => (Some(view), None),
            None => (None, Some(true)),
        };
        Some(match query? {
            Query::Nodes {
                repository_id,
                node_kind,
                internal_module,
            } => In::Repositories(repositories::Input::Nodes(repositories::NodesInput {
                repository_id,
                views,
                current,
                page: frame::page(u32::from(PAGE_SIZE), cursor)?,
                node_kind,
                internal_module,
                scope: Some(scope),
            })),
            Query::Code(query) => code_input(
                operation,
                GraphReadRequest {
                    protocol: frame::protocol(),
                    query_id: frame::query_id(backend)?,
                    scope,
                    view: views,
                    current,
                    query,
                    limits: QueryLimits {
                        max_results: PAGE_SIZE,
                        max_visited: 20000,
                        deadline_ms: 10000,
                    },
                    cursor: cursor.map(str::to_owned),
                },
            )?,
        })
    })
}

/// The `code.*` input of `operation` around `request`.
fn code_input(operation: &str, request: GraphReadRequest) -> Option<In> {
    use code::Input as C;
    Some(In::Code(match operation {
        "code.search" => C::Search(request),
        "code.architecture" => C::Architecture(request),
        "code.dependency" => C::Dependency(request),
        "code.impact" => C::Impact(request),
        "code.source" => C::Source(request),
        "code.traverse" => C::Traverse(request),
        "code.graph_scope_status" => C::GraphScopeStatus(request),
        "code.overview" => C::Overview(request),
        "code.nodes_by_file" => C::NodesByFile(request),
        "code.node_detail" => C::NodeDetail(request),
        "code.file_impact" => C::FileImpact(request),
        "code.graph_schema" => C::GraphSchema(request),
        "code.call_path" => C::CallPath(request),
        "code.data_lineage" => C::DataLineage(request),
        "code.consumers" => C::Consumers(request),
        "code.publishers" => C::Publishers(request),
        "code.snippet" => C::Snippet(request),
        "code.repository_dependencies" => C::RepositoryDependencies(request),
        _ => return None,
    }))
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

pub fn trust(
    backend: &dyn Backend,
    node: &CodeNode,
    scope: Option<DataScope>,
    pin: Option<Id>,
    trusted: bool,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Code(code::Input::SetTrust(IndexCommandRequest {
            protocol: frame::protocol(),
            request_key: frame::request_key(backend)?,
            command: IndexCommand::SetTrust {
                repository_id: node.repository_id.clone(),
                node_id: node.node_id.clone(),
                expected_revision: node.trust_revision.clone(),
                view_id: node.view_id.clone(),
                expected_content_digest: None,
                trusted,
                unit_key: Some(node.unit_key.clone()),
                expected_view_digest: Some(node.view_digest.clone()),
                expected_content_identity: Some(node.content_identity.clone()),
                scope: Some(scope?),
                pin_id: pin,
            },
        })))
    })
}
