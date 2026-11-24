//! Retained, exact-view repository analysis.
mod capture;
mod commands;
mod communities;
mod community;
mod deterministic;
mod documentation;
mod http;
pub(super) mod links;
mod modeling;
mod observation;
mod paid;
mod phase_store;
mod queries;
mod read;
mod recovery;
mod security;
mod selection;
mod service;
mod symbols;
mod vitals;
mod work;

pub use commands::{CONSUMER, QueueHook};
pub use documentation::DOCS_CONSUMER;
pub(crate) use documentation::synthesis_request;
pub(crate) use documentation::{DocumentationDraft, DocumentationEvidence};
pub use modeling::{
    AnalysisModel, AnalysisModelInput, MAX_OUTPUT_TOKENS, Observation as ModelObservation,
    ProviderAnalysis,
};
pub use service::{AnalysisService, COMMANDS, Execution, QUERIES, authorize_model_request};
mod auto;
pub use auto::INDEX_CONSUMER;
mod retention;

pub use crate::kernel::analysis::{dispatch_binding_in, dispatch_binding_snapshot};
