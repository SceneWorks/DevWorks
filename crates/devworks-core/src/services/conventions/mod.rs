//! Versioned authored conventions: one scoped owner for native UI, agents and scheduled
//! indexing. Convention data never grants authority or replaces explicit user instructions.
pub mod indexing;
pub mod model;

pub const OPERATIONS: [&str; 13] = [
    "conventions.list",
    "conventions.read",
    "conventions.edit",
    "conventions.version",
    "conventions.applicability",
    "conventions.retire",
    "conventions.index",
    "conventions.import",
    "conventions.delete",
    "conventions.move",
    "conventions.resources",
    "conventions.delete_resource",
    "conventions.search",
];
