//! Explicit, persistent embedding identities. Identity alone never qualifies a route.
mod identity;
pub use identity::{EmbeddingIdentity, embedding_identity};
mod binding;
pub use binding::{
    PreparedEmbeddingProbe, ProbeOutcome, ProbePreparation, QualifiedEmbeddingBinding,
    ResolvedEmbeddingConfiguration,
};
mod resolve;
pub use resolve::{resolve_selected_in, resolve_selected_read};
mod service;
pub use service::{EFFECT_CONSUMER, EmbeddingSpaceService, PROBE_KIND, PROBE_TEXT};
mod evidence;
pub use evidence::{NativeProbeResult, VerifiedProbeEvidence};

mod inputs;

mod commands;

mod outcomes;
mod query;
pub use outcomes::ProbeOutcomePage;
