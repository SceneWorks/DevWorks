//! Exact composed graph reads. The caller supplies already-authorized repository/view bindings;
//! the storage owner validates every immutable binding before filtering or expanding rows.
use super::{StorageError, StoredEdge, StoredNode, StoredUnit};
use crate::protocol::{ActorId, Id, StoreId};
use serde::{Deserialize, Serialize};

pub const GRAPH_MAX_PAGE: u16 = 500;
pub const GRAPH_MAX_VIEWS: usize = 256;
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
    /// Opaque StoredUnit content identity (may be a Git OID); not necessarily raw SHA-256.
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
    /// Literal target lookup: Unicode lowercase name equality/containment, or exact qualified name.
    pub name: Option<GraphNameFilter>,
    pub kinds: Vec<String>,
    pub unit_key: Option<String>,
    pub node_id: Option<String>,
    /// Only explicit false annotations matching this exact content AND view are excluded.
    pub exclude_untrusted: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphNameFilter {
    pub name: String,
    pub contains: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphEdgeFilter {
    /// Exact visible source unit; composition masking still precedes this filter.
    pub unit_key: Option<String>,
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
    fn graph_kinds(
        &self,
        scope: &GraphScope,
        domain: GraphElementKind,
        after: Option<&str>,
        limit: u16,
    ) -> Result<GraphPage<GraphKindCount>, StorageError>;
    fn graph_trust(
        &self,
        scope: &GraphScope,
        identity: &GraphNodeIdentity,
        content_digest: &str,
    ) -> Result<Option<StoredGraphTrust>, StorageError>;
    fn graph_trust_revision(&self, scope: &GraphScope) -> Result<i64, StorageError>;
    /// Expired current-view leases also need bounded cleanup; they are not retirable views.
    fn graph_expired_pins(
        &self,
        now_ms: i64,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<GraphPage<StoredGraphReadPin>, StorageError>;
    fn graph_pin(&self, id: &Id) -> Result<Option<StoredGraphReadPin>, StorageError>;
    fn graph_retirable_views(
        &self,
        now_ms: i64,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<GraphPage<super::StoredView>, StorageError>;
    fn graph_source_capture(
        &self,
        id: &Id,
    ) -> Result<Option<StoredGraphSourceCapture>, StorageError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphElementKind {
    Node,
    Edge,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphKindCount {
    pub kind: String,
    pub count: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredGraphTrust {
    pub identity: GraphNodeIdentity,
    pub view_id: Id,
    pub view_digest: String,
    pub content_digest: String,
    /// One is the implicit unannotated baseline; first authored revision is two.
    pub revision: i64,
    pub trusted: bool,
    pub updated_by: ActorId,
    pub updated_at_ms: i64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredGraphReadPin {
    pub id: Id,
    pub owner_namespace: String,
    pub store_id: StoreId,
    pub restore_epoch: i64,
    pub scope_digest: String,
    /// Exact owner/store/restore/scope/view-set binding; never operation or query filters.
    pub intent_digest: String,
    pub scope: GraphScope,
    pub view_witnesses: Vec<GraphViewWitness>,
    pub unavailable_repositories: Vec<GraphUnavailableRepository>,
    pub revision: i64,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphViewWitness {
    pub view_id: Id,
    pub source_revision: i64,
    pub overlay_id: Option<Id>,
    pub expires_at_ms: Option<i64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphUnavailableRepository {
    pub repository_id: Id,
    pub reason: String,
}
/// Private retained source bytes. No public artifact/version row is created; the blob is held
/// by generation/unit bindings, with the same reader, backup and physical-removal guards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredGraphSourceCapture {
    pub id: Id,
    pub repository_id: Id,
    pub unit_key: String,
    pub source_digest: String,
    pub blob_digest: String,
    pub blob_generation: String,
    pub byte_count: i64,
    pub created_at_ms: i64,
}
pub const GRAPH_PIN_TTL_MS: i64 = 600_000;
pub trait GraphRepository: GraphRead {
    fn put_graph_trust(
        &mut self,
        scope: &GraphScope,
        record: &StoredGraphTrust,
        expected_revision: i64,
    ) -> Result<(), StorageError>;
    fn acquire_graph_pin(&mut self, record: &StoredGraphReadPin) -> Result<(), StorageError>;
    fn renew_graph_pin(
        &mut self,
        id: &Id,
        owner_namespace: &str,
        expected_revision: i64,
        expires_at_ms: i64,
    ) -> Result<StoredGraphReadPin, StorageError>;
    fn retire_graph_pin(
        &mut self,
        id: &Id,
        owner_namespace: &str,
        expected_revision: i64,
    ) -> Result<(), StorageError>;
    fn invalidate_graph_pins_for_view(&mut self, view: &Id) -> Result<(), StorageError>;
    fn insert_graph_source_capture(
        &mut self,
        record: &StoredGraphSourceCapture,
    ) -> Result<(), StorageError>;
}
