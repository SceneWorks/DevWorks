//! Revision-bound convention indexing. Pure source chunking and ranking are shared by the
//! durable index publisher and query preparation; neither performs inference or storage I/O.
pub mod chunker;
pub mod rank;
