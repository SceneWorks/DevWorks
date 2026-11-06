//! Exact input resolution through existing artifact ownership; no I/O inside a write transaction.
use super::{PreparedTool, ProviderError};
use crate::protocol::provider::{ArtifactRef, NativeBinding, SchemaRef, ToolSchema};
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
    /// Resolves current capability/schema/alias binding and the owning tool description.
    fn tool(
        &self,
        _context: &InvocationContext,
        _alias: &ToolSchema,
    ) -> Result<PreparedTool, ProviderError> {
        Err(ProviderError::MaterialUnavailable)
    }
}
pub trait Materials {
    fn artifact(&self, reference: &ArtifactRef, max_bytes: u64) -> Result<Vec<u8>, ProviderError>;
    /// Native continuation bytes require their original immutable host/version binding.
    fn native(
        &self,
        _reference: &ArtifactRef,
        _binding: &NativeBinding,
        _max_bytes: u64,
    ) -> Result<Vec<u8>, ProviderError> {
        Err(ProviderError::ForeignNative)
    }
    fn schema(&self, reference: &SchemaRef) -> Result<Value, ProviderError>;
    fn tool(&self, _alias: &ToolSchema) -> Result<PreparedTool, ProviderError> {
        Err(ProviderError::MaterialUnavailable)
    }
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
    fn native(
        &self,
        reference: &ArtifactRef,
        binding: &NativeBinding,
        max_bytes: u64,
    ) -> Result<Vec<u8>, ProviderError> {
        let (bytes, provenance) = self
            .artifacts
            .read_exact(self.store, self.context, reference, max_bytes)
            .map_err(|_| ProviderError::MaterialUnavailable)?;
        let host = self.store.info().host_actor.0.as_str().to_owned();
        let original: NativeBinding =
            serde_json::from_value(provenance["derived"]["nativeBinding"].clone())
                .map_err(|_| ProviderError::ForeignNative)?;
        if original != *binding
            || provenance["principalKind"] != "Host"
            || provenance["producedBy"].as_str() != Some(&host)
            || provenance["derived"]["kind"] != "ProviderNativeContinuation"
        {
            return Err(ProviderError::ForeignNative);
        }
        let version = crate::ports::storage::read_snapshot(
            self.store,
            &crate::ports::storage::ReadOptions::default(),
            |s| s.artifacts().version(&reference.version_id),
        )
        .map_err(|_| ProviderError::MaterialUnavailable)?
        .ok_or(ProviderError::MaterialUnavailable)?;
        if !version.live()
            || version.artifact_id != reference.artifact_id
            || version.blob_digest != reference.sha256.as_str()
            || version.byte_count != reference.byte_length.get()
            || version.media_type != reference.media_type
            || version.source["principalKind"] != "Host"
            || version.source["producedBy"].as_str() != Some(&host)
        {
            return Err(ProviderError::ForeignNative);
        }
        Ok(bytes)
    }
    fn tool(&self, alias: &ToolSchema) -> Result<PreparedTool, ProviderError> {
        self.schemas.tool(self.context, alias)
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
        .map(|alias| {
            let tool = materials.tool(alias)?;
            if tool.wire_name != alias.wire_name
                || tool.capability_id.as_str() != alias.capability_id.as_str()
                || tool.input_schema.schema_id.as_str() != alias.input_schema.schema_id.as_str()
                || tool.input_schema.revision != alias.input_schema.revision
                || tool.input_schema.sha256 != alias.input_schema.sha256
            {
                return Err(ProviderError::InvalidInput("tool_registration_binding"));
            }
            verify_schema(&alias.input_schema, &tool.schema)?;
            Ok(tool)
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

/// Records the actual reads, including refs nested in context/native/tool-result artifacts.
pub(crate) struct TrackedMaterials<'a> {
    pub source: &'a dyn Materials,
    pub references: std::cell::RefCell<Vec<ArtifactRef>>,
}
impl TrackedMaterials<'_> {
    fn remember(
        &self,
        reference: &ArtifactRef,
        max: u64,
        bytes: Vec<u8>,
    ) -> Result<Vec<u8>, ProviderError> {
        reference.validate().map_err(ProviderError::InvalidInput)?;
        if bytes.len() as u64 > max {
            return Err(ProviderError::TooLarge);
        }
        if reference.byte_length.get() as u64 != bytes.len() as u64
            || crate::fingerprint::sha256_hex(&bytes) != reference.sha256.as_str()
        {
            return Err(ProviderError::MaterialUnavailable);
        }
        let mut refs = self.references.borrow_mut();
        if !refs.iter().any(|r| {
            r.artifact_id == reference.artifact_id
                && r.version_id == reference.version_id
                && r.sha256 == reference.sha256
                && r.byte_length == reference.byte_length
                && r.media_type == reference.media_type
        }) {
            refs.push(reference.clone());
        }
        Ok(bytes)
    }
}
impl Materials for TrackedMaterials<'_> {
    fn artifact(&self, reference: &ArtifactRef, max: u64) -> Result<Vec<u8>, ProviderError> {
        reference.validate().map_err(ProviderError::InvalidInput)?;
        self.remember(reference, max, self.source.artifact(reference, max)?)
    }
    fn native(
        &self,
        reference: &ArtifactRef,
        binding: &NativeBinding,
        max: u64,
    ) -> Result<Vec<u8>, ProviderError> {
        reference.validate().map_err(ProviderError::InvalidInput)?;
        self.remember(reference, max, self.source.native(reference, binding, max)?)
    }
    fn schema(&self, reference: &SchemaRef) -> Result<Value, ProviderError> {
        self.source.schema(reference)
    }
    fn tool(&self, alias: &ToolSchema) -> Result<PreparedTool, ProviderError> {
        self.source.tool(alias)
    }
}
