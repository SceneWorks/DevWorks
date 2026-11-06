//! Exact composed graph reads. The caller supplies already-authorized repository/view bindings;
//! the storage owner validates every immutable binding before filtering or expanding rows.
use super::{StorageError, StoredEdge, StoredNode, StoredUnit};
use crate::protocol::Id;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const GRAPH_MAX_PAGE: u16 = 500;
pub const GRAPH_MAX_VIEWS: usize = 128;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphViewBinding {
    pub repository_id: Id,
    pub view_id: Id,
    pub view_digest: String,
}
/// Exactly one view per repository, never an implicit workspace or published-view fallback.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphScope {
    pub views: Vec<GraphViewBinding>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNodeIdentity {
    pub repository_id: Id,
    pub unit_key: String,
    pub node_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphPosition {
    pub repository_id: Id,
    pub unit_key: String,
    /// Empty for a unit page; exact node/edge ID for the corresponding row page.
    pub item_id: String,
}
#[derive(Clone, Debug, PartialEq)]
pub struct GraphPage<T> {
    pub items: Vec<T>,
    /// A page boundary, never a claim that the selected index has complete source coverage.
    pub has_more: bool,
}
#[derive(Clone, Debug, PartialEq)]
pub struct GraphUnit {
    pub repository_id: Id,
    pub view_id: Id,
    pub unit: StoredUnit,
}
#[derive(Clone, Debug, PartialEq)]
pub struct GraphNode {
    pub repository_id: Id,
    pub view_id: Id,
    pub generation_id: Id,
    pub content_digest: String,
    pub node: StoredNode,
}
#[derive(Clone, Debug, PartialEq)]
pub struct GraphEdge {
    pub repository_id: Id,
    pub view_id: Id,
    pub generation_id: Id,
    pub content_digest: String,
    pub edge: StoredEdge,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphNodeFilter {
    /// SQL LIKE pattern over name/qualified name (% and _ wildcards); no interpolated SQL.
    pub pattern: Option<String>,
    pub kinds: Vec<String>,
    pub unit_key: Option<String>,
    pub node_id: Option<String>,
    /// Only explicit false annotations matching this exact content AND view are excluded.
    pub exclude_untrusted: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphEdgeFilter {
    pub kinds: Vec<String>,
    /// Empty means no endpoint restriction; nonempty sets are exact composite identities.
    pub source_ids: Vec<GraphNodeIdentity>,
    pub target_ids: Vec<GraphNodeIdentity>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphCounts {
    pub units: u64,
    pub nodes: u64,
    pub edges: u64,
    pub node_kinds: BTreeMap<String, u64>,
    pub edge_kinds: BTreeMap<String, u64>,
}

pub trait GraphRead {
    fn graph_units(
        &self,
        scope: &GraphScope,
        after: Option<&GraphPosition>,
        limit: u16,
    ) -> Result<GraphPage<GraphUnit>, StorageError>;
    fn graph_nodes(
        &self,
        scope: &GraphScope,
        filter: &GraphNodeFilter,
        after: Option<&GraphPosition>,
        limit: u16,
    ) -> Result<GraphPage<GraphNode>, StorageError>;
    fn graph_node(
        &self,
        scope: &GraphScope,
        identity: &GraphNodeIdentity,
    ) -> Result<Option<GraphNode>, StorageError>;
    /// Empty filters page every visible raw edge, including unresolved ones. Core linking may
    /// rebind inherited edges against this same composition before reporting/expanding them.
    fn graph_edges(
        &self,
        scope: &GraphScope,
        filter: &GraphEdgeFilter,
        after: Option<&GraphPosition>,
        limit: u16,
    ) -> Result<GraphPage<GraphEdge>, StorageError>;
    /// Exact composed-row counts, not coverage inference and never a capped scan presented as total.
    fn graph_counts(&self, scope: &GraphScope) -> Result<GraphCounts, StorageError>;
}
