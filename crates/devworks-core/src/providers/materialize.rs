//! Exact input resolution through existing artifact ownership; no I/O inside a write transaction.
use super::{PreparedTool, ProviderError};
use crate::protocol::provider::{ArtifactRef, SchemaRef, ToolSchema};
use crate::services::artifacts::ArtifactService;
use crate::{context::InvocationContext, ports::storage::ApplicationStore};
use serde_json::Value;

/// Resolves a CURRENT registered built-in or installed-package schema for this exact ref/scope.
/// Implementations must reject inactive, unregistered, foreign or changed publications.
pub trait SchemaDocuments: Send + Sync {
    fn resolve(
        &self,
        context: &InvocationContext,
        reference: &SchemaRef,
    ) -> Result<Value, ProviderError>;
}
pub trait Materials {
    fn artifact(&self, reference: &ArtifactRef, max_bytes: u64) -> Result<Vec<u8>, ProviderError>;
    fn schema(&self, reference: &SchemaRef) -> Result<Value, ProviderError>;
}
/// Scope-bound adapter to the real ArtifactService. This object never grants artifact access.
pub struct OwnedMaterials<'a> {
    pub store: &'a dyn ApplicationStore,
    pub artifacts: &'a ArtifactService,
    pub context: &'a InvocationContext,
    pub schemas: &'a dyn SchemaDocuments,
}
impl Materials for OwnedMaterials<'_> {
    fn artifact(&self, reference: &ArtifactRef, max_bytes: u64) -> Result<Vec<u8>, ProviderError> {
        self.artifacts
            .read_exact(self.store, self.context, reference, max_bytes)
            .map(|v| v.0)
            .map_err(|_| ProviderError::MaterialUnavailable)
    }
    fn schema(&self, reference: &SchemaRef) -> Result<Value, ProviderError> {
        let schema = self.schemas.resolve(self.context, reference)?;
        verify_schema(reference, &schema)?;
        Ok(schema)
    }
}
pub fn tools(
    materials: &dyn Materials,
    aliases: &[ToolSchema],
) -> Result<Vec<PreparedTool>, ProviderError> {
    aliases
        .iter()
        .map(|t| {
            Ok(PreparedTool {
                wire_name: t.wire_name.clone(),
                capability_id: t.capability_id.clone(),
                input_schema: t.input_schema.clone(),
                schema: schema(materials, &t.input_schema)?,
                description: None,
            })
        })
        .collect()
}

/// Every material source, including an injected resolver, must present the exact registered ref.
pub fn schema(materials: &dyn Materials, reference: &SchemaRef) -> Result<Value, ProviderError> {
    let schema = materials.schema(reference)?;
    verify_schema(reference, &schema)?;
    Ok(schema)
}
fn verify_schema(reference: &SchemaRef, schema: &Value) -> Result<(), ProviderError> {
    let canonical =
        crate::jcs::canonicalize(schema).map_err(|_| ProviderError::InvalidInput("schema_json"))?;
    if crate::fingerprint::sha256_hex(canonical.as_bytes()) != reference.sha256.as_str() {
        return Err(ProviderError::InvalidInput("schema_digest"));
    }
    if !schema.is_object() {
        return Err(ProviderError::InvalidInput("schema_document"));
    }
    Ok(())
}
