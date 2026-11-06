//! One current selection/runtime witness for queued publishers and retrieval cursors.
use super::{IndexService, embedding::EmbeddingConfiguration};
use crate::{
    context::InvocationContext,
    ports::storage::*,
    protocol::data::Metric,
    services::router::embedding_spaces::{
        self, QualifiedEmbeddingBinding, ResolvedEmbeddingConfiguration,
    },
};
pub fn stored_space(binding: &QualifiedEmbeddingBinding) -> StoredConventionEmbeddingSpace {
    StoredConventionEmbeddingSpace {
        id: binding.space().space_id.as_str().into(),
        provider: binding.connection().id.as_str().into(),
        model: binding.upstream_model_id().into(),
        configuration_digest: binding.configuration_digest().into(),
        dimensions: binding.space().dimensions,
        metric: format!("{:?}", binding.space().metric),
        element_type: "Float32".into(),
    }
}
pub fn resolved_configuration(
    resolved: &ResolvedEmbeddingConfiguration,
    available: bool,
) -> EmbeddingConfiguration {
    let source_digest = crate::fingerprint::sha256_hex(
        format!(
            "ConventionEmbeddingRuntimeV1:{available}:{}",
            resolved.source_digest()
        )
        .as_bytes(),
    );
    if !available {
        return EmbeddingConfiguration {
            source_digest,
            space: None,
            reason: Some("NoNativeEmbeddingRuntime".into()),
        };
    }
    let supported = resolved
        .binding()
        .filter(|b| matches!(b.space().metric, Metric::Cosine));
    EmbeddingConfiguration {
        source_digest,
        space: supported.map(stored_space),
        reason: if resolved.binding().is_some() && supported.is_none() {
            Some("UnsupportedMetric: Conventions use positive cosine similarity.".into())
        } else {
            resolved.reason().map(str::to_owned)
        },
    }
}
impl IndexService {
    pub fn configuration_read(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
    ) -> Result<EmbeddingConfiguration, StorageError> {
        let resolved = embedding_spaces::resolve_selected_read(read, ctx)?;
        Ok(resolved_configuration(&resolved, self.native_available()))
    }
    pub fn configuration_in(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
    ) -> Result<EmbeddingConfiguration, StorageError> {
        let resolved = embedding_spaces::resolve_selected_in(tx, ctx)?;
        Ok(resolved_configuration(&resolved, self.native_available()))
    }
    pub(super) fn native_available(&self) -> bool {
        !self.is_stopped()
            && self
                .embeddings
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
                .is_some_and(|p| p.is_available())
    }
}
