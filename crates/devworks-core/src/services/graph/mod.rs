//! Exact composed graph reads, private source capture, and retention leases.
mod analysis;
mod budget;
mod cursor;
pub(crate) fn cursor_pin(cursor: &str) -> Option<crate::protocol::Id> {
    cursor::Cursors::pin(cursor).ok().flatten()
}
mod dataset;
mod impact;
mod incident;
#[doc(hidden)]
pub use incident::{
    ResolvedEdge, UnresolvedParity, indexed_incident, scanned_incident, unresolved_parity,
};
mod languages;
pub(crate) mod lease;
mod linking;
mod model;
mod modules;
#[doc(hidden)]
pub use modules::node_lookup_parity;
mod properties;
mod read;
pub mod repository_analysis;
mod scan;
pub(crate) mod schema_refs;
#[doc(hidden)]
pub use scan::scanned_edges;
mod scope;
mod search;

mod operators;
mod source;

mod service;
pub use service::{GraphService, OPERATIONS};
pub(crate) use service::{graph_input, target_repository};

mod packages;

mod unmapped;
