//! Immutable price quotations shared by host inference admission and pricing inspection.
mod resolver;
pub use resolver::CatalogPriceResolver;
pub use resolver::official_price_source;

mod inspection;
pub mod refresh;
mod service;
pub use service::{OPERATIONS, PriceService};
