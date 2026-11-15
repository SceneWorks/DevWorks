//! Exact composed graph reads, private source capture, and retention leases.
mod analysis;
mod budget;
mod cursor;
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
mod scan;
#[doc(hidden)]
pub use scan::scanned_edges;
mod scope;
mod search;

mod operators;
mod source;

mod service;
pub use service::{GraphService, OPERATIONS};

mod packages;

mod unmapped;
