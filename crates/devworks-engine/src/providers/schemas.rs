//! The current selected-operation and installed-package registry, never caller schema bytes.
use devworks_core::{
    authority::policy::{self, Verdict},
    context::InvocationContext,
    fingerprint::sha256_hex,
    ports::{packages::Inspection, storage::*},
    protocol::{
        Id, Revision,
        provider::{QualifiedId, SchemaRef, ToolSchema},
    },
    providers::{PreparedTool, ProviderError, materialize::SchemaDocuments},
    registry::{self, Mode, SchemaRevision},
    services::{
        UseCases,
        artifacts::ArtifactService,
        packages::{PackageService, policy as package_policy},
    },
};
use serde_json::{Value, json};
use std::{io::Read, sync::Arc};

pub struct RegisteredSchemas {
    store: Arc<dyn ApplicationStore>,
    cases: UseCases,
    packages: Option<Arc<PackageService>>,
    artifacts: Arc<ArtifactService>,
}
impl RegisteredSchemas {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        cases: UseCases,
        packages: Option<Arc<PackageService>>,
        artifacts: Arc<ArtifactService>,
    ) -> Self {
        Self {
            store,
            cases,
            packages,
            artifacts,
        }
    }
    /// Generates the exact same schema reference consumed by resolve/tool. No arbitrary ID is
    /// parsed into an operation, and no uncomposed operation enters this inventory. This is
    /// only a candidate alias: `SchemaDocuments::tool(context, alias)` separately qualifies
    /// the current principal/profile/selected operation before any provider receives it.
    pub fn builtin(
        &self,
        operation: &str,
        mode: Mode,
        revision: SchemaRevision,
        wire_name: &str,
    ) -> Result<ToolSchema, ProviderError> {
        let descriptor = registry::find(operation).ok_or(ProviderError::MaterialUnavailable)?;
        let selected = descriptor
            .binding(mode)
            .ok_or(ProviderError::MaterialUnavailable)?;
        if !descriptor.supports(revision) || !self.registered(operation, mode) {
            return Err(ProviderError::MaterialUnavailable);
        }
        let schema: Value = selected.input_schema(revision).to_value();
        let reference = schema_ref(operation, mode, revision, &schema)?;
        Ok(ToolSchema {
            wire_name: wire_name.into(),
            capability_id: QualifiedId::try_from(format!("devworks/{operation}"))
                .map_err(ProviderError::InvalidInput)?,
            input_schema: reference,
        })
    }
    fn registered(&self, op: &str, mode: Mode) -> bool {
        match mode {
            Mode::Query => self.cases.has_query(op),
            Mode::Command => self.cases.has_command(op),
        }
    }
    fn builtin_document(
        &self,
        ctx: &InvocationContext,
        reference: &SchemaRef,
    ) -> Result<
        Option<(
            Value,
            &'static registry::OperationDescriptor,
            &'static registry::ModeBinding,
        )>,
        ProviderError,
    > {
        for d in registry::all() {
            for selected in d.modes {
                if !self.registered(d.name, selected.mode) {
                    continue;
                }
                let revision = SchemaRevision(
                    u32::try_from(reference.revision.get())
                        .map_err(|_| ProviderError::MaterialUnavailable)?,
                );
                if !d.supports(revision) {
                    continue;
                }
                let schema: Value = selected.input_schema(revision).to_value();
                let expected = schema_ref(d.name, selected.mode, revision, &schema)?;
                if equal(&expected, reference) {
                    read_snapshot(self.store.as_ref(), &ReadOptions::default(), |s| {
                        current_context(s, ctx)?;
                        let principal = s.authority().principal(ctx.principal())?;
                        let effective =
                            policy::resolve_policy(s.configuration(), ctx.project_id())?;
                        if !matches!(
                            policy::registered_tool_visibility(
                                ctx,
                                principal.as_ref(),
                                &effective,
                                d,
                                selected,
                                selected.effect
                            ),
                            Verdict::Allow
                        ) {
                            return Err(StorageError::ScopeDenied);
                        }
                        Ok(())
                    })
                    .map_err(|_| ProviderError::MaterialUnavailable)?;
                    return Ok(Some((schema, d, selected)));
                }
            }
        }
        Ok(None)
    }
    fn package_inspection(
        &self,
        ctx: &InvocationContext,
        package: &str,
    ) -> Result<(Inspection, i64), ProviderError> {
        let service = self
            .packages
            .as_ref()
            .ok_or(ProviderError::MaterialUnavailable)?;
        if !self.cases.has_query("addons.catalog") {
            return Err(ProviderError::MaterialUnavailable);
        }
        let (record, version, revision) =
            read_snapshot(self.store.as_ref(), &ReadOptions::default(), |s| {
                package_context(s, ctx)?;
                let registry = service.registry(s.packages())?;
                let (record, version) = registry.get(package).ok_or(StorageError::ScopeDenied)?;
                let view = devworks_core::services::packages::view::package_record(
                    s.packages(),
                    &registry,
                    record,
                    service.platform(),
                    service.runtimes().as_ref(),
                    ctx.project_id().map(|p| p.0.as_str()),
                )?;
                if !matches!(
                    view.availability,
                    Some(devworks_core::protocol::records::PackageAvailability::Ready)
                ) {
                    return Err(StorageError::ScopeDenied);
                }
                Ok((
                    record.clone(),
                    version.clone().ok_or(StorageError::ScopeDenied)?,
                    s.packages().registry_revision()?,
                ))
            })
            .map_err(|_| ProviderError::MaterialUnavailable)?;
        let mut inspection =
            package_policy::inspection(&version).ok_or(ProviderError::MaterialUnavailable)?;
        if inspection.schema_assets.is_empty()
            || inspection
                .contributions
                .iter()
                .any(|c| c.descriptor.is_none())
        {
            // The legacy record still binds its original bundle. Reinspection never accepts a
            // caller substitute or weakens original visibility to obtain it.
            let source =
                version
                    .provenance
                    .get("source")
                    .cloned()
                    .ok_or(ProviderError::Unsupported(
                        "legacy package metadata needs reinstall",
                    ))?;
            let reference =
                serde_json::from_value(source).map_err(|_| ProviderError::MaterialUnavailable)?;
            let info = self.store.info();
            let metadata_context = devworks_core::context::HostIngress::new(self.store.as_ref())
                .context(
                    info.host_actor,
                    devworks_core::context::PrincipalKind::Host,
                    ctx.invocation_id().clone(),
                    Id::new("package-metadata-recovery").expect("static"),
                    devworks_core::protocol::Counter::new(info.restore_epoch)
                        .map_err(|_| ProviderError::MaterialUnavailable)?,
                    Revision::new(1).expect("positive"),
                );
            let bytes = self
                .artifacts
                .read_exact(
                    self.store.as_ref(),
                    &metadata_context,
                    &reference,
                    128 * 1024 * 1024,
                )
                .map_err(|_| {
                    ProviderError::Unsupported("legacy package source unavailable; reinstall")
                })?
                .0;
            if sha256_hex(&bytes) != version.bundle_digest {
                return Err(ProviderError::MaterialUnavailable);
            }
            inspection = service
                .inspect(&bytes)
                .map_err(|_| ProviderError::MaterialUnavailable)?;
            if inspection.manifest_digest != version.manifest_digest
                || inspection.package_id != record.package_id
            {
                return Err(ProviderError::MaterialUnavailable);
            }
        }
        service
            .files()
            .verify(package, &inspection.manifest_digest, &inspection.files)
            .map_err(|_| ProviderError::MaterialUnavailable)?;
        Ok((inspection, revision))
    }
    fn package_document(
        &self,
        ctx: &InvocationContext,
        reference: &SchemaRef,
    ) -> Result<Value, ProviderError> {
        let package = reference
            .schema_id
            .as_str()
            .split('/')
            .next()
            .ok_or(ProviderError::MaterialUnavailable)?;
        let (inspection, revision) = self.package_inspection(ctx, package)?;
        let asset = inspection
            .schema_assets
            .iter()
            .find(|a| serde_json::to_value(reference).ok().as_ref() == Some(&a.reference))
            .ok_or(ProviderError::MaterialUnavailable)?;
        let bytes = self.file(&inspection, &asset.path, 8 * 1024 * 1024)?;
        let value =
            serde_json::from_slice(&bytes).map_err(|_| ProviderError::MaterialUnavailable)?;
        let again = self.package_inspection(ctx, package)?;
        if again.1 != revision || again.0.manifest_digest != inspection.manifest_digest {
            return Err(ProviderError::MaterialUnavailable);
        }
        Ok(value)
    }
    fn file(&self, i: &Inspection, path: &str, max: u64) -> Result<Vec<u8>, ProviderError> {
        let expected = i
            .files
            .iter()
            .find(|f| f.path == path)
            .ok_or(ProviderError::MaterialUnavailable)?;
        let length = expected.byte_length;
        if length > max {
            return Err(ProviderError::TooLarge);
        }
        let service = self
            .packages
            .as_ref()
            .ok_or(ProviderError::MaterialUnavailable)?;
        let file = std::fs::File::open(service.files().path(
            &i.package_id,
            &i.manifest_digest,
            path,
        ))
        .map_err(|_| ProviderError::MaterialUnavailable)?;
        let mut bytes = Vec::new();
        file.take(length + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ProviderError::MaterialUnavailable)?;
        if bytes.len() as u64 != length || sha256_hex(&bytes) != expected.sha256 {
            return Err(ProviderError::MaterialUnavailable);
        }
        Ok(bytes)
    }
}
fn schema_ref(
    op: &str,
    mode: Mode,
    revision: SchemaRevision,
    schema: &Value,
) -> Result<SchemaRef, ProviderError> {
    let mode = match mode {
        Mode::Query => "query",
        Mode::Command => "command",
    };
    let canonical =
        devworks_core::jcs::canonicalize(schema).map_err(|_| ProviderError::MaterialUnavailable)?;
    Ok(SchemaRef {
        schema_id: QualifiedId::try_from(format!("devworks/{op}.{mode}.input"))
            .map_err(ProviderError::InvalidInput)?,
        revision: Revision::new(i64::from(revision.0)).map_err(ProviderError::InvalidInput)?,
        sha256: sha256_hex(canonical.as_bytes())
            .try_into()
            .map_err(ProviderError::InvalidInput)?,
    })
}
fn equal(a: &SchemaRef, b: &SchemaRef) -> bool {
    a.schema_id.as_str() == b.schema_id.as_str() && a.revision == b.revision && a.sha256 == b.sha256
}
fn current_context(s: &dyn ReadSnapshot, ctx: &InvocationContext) -> Result<(), StorageError> {
    if s.stamp().store_id != *ctx.store_id()
        || s.stamp().restore_epoch != ctx.restore_epoch().get()
        || s.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}
fn package_context(s: &dyn ReadSnapshot, ctx: &InvocationContext) -> Result<(), StorageError> {
    current_context(s, ctx)?;
    let d = registry::find("addons.catalog").ok_or(StorageError::ScopeDenied)?;
    let selected = d.binding(Mode::Query).ok_or(StorageError::ScopeDenied)?;
    let binding = s.authority().principal(ctx.principal())?;
    let effective = policy::resolve_policy(s.configuration(), ctx.project_id())?;
    if !matches!(
        policy::registered_tool_visibility(
            ctx,
            binding.as_ref(),
            &effective,
            d,
            selected,
            devworks_core::registry::EffectClass::External
        ),
        Verdict::Allow
    ) {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}

impl SchemaDocuments for RegisteredSchemas {
    fn resolve(
        &self,
        ctx: &InvocationContext,
        reference: &SchemaRef,
    ) -> Result<Value, ProviderError> {
        match self.builtin_document(ctx, reference)? {
            Some((schema, _, _)) => Ok(schema),
            None => self.package_document(ctx, reference),
        }
    }
    fn tool(
        &self,
        ctx: &InvocationContext,
        alias: &ToolSchema,
    ) -> Result<PreparedTool, ProviderError> {
        let (schema, description) = match self.builtin_document(ctx, &alias.input_schema)? {
            Some((schema, d, selected)) => {
                if alias.capability_id.as_str() != format!("devworks/{}", d.name) {
                    return Err(ProviderError::MaterialUnavailable);
                }
                let description = schema
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        format!(
                            "{} {} ({})",
                            selected.mode.as_str(),
                            d.name,
                            selected.input_type
                        )
                    });
                (schema, Some(description))
            }
            None => {
                if ctx.principal_kind() == devworks_core::context::PrincipalKind::Agent {
                    read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
                        current_context(read, ctx)?;
                        let selected =
                            policy::resolve_policy(read.configuration(), ctx.project_id())?
                                .tool_selected(alias.capability_id.as_str(), false);
                        if selected {
                            Ok(())
                        } else {
                            Err(StorageError::ScopeDenied)
                        }
                    })
                    .map_err(|_| ProviderError::MaterialUnavailable)?;
                }
                let package = alias
                    .capability_id
                    .as_str()
                    .split('/')
                    .next()
                    .ok_or(ProviderError::MaterialUnavailable)?;
                let (inspection, revision) = self.package_inspection(ctx, package)?;
                let contribution = inspection
                    .contributions
                    .iter()
                    .find(|c| c.kind == "Tool" && c.id == alias.capability_id.as_str())
                    .ok_or(ProviderError::MaterialUnavailable)?;
                let descriptor = contribution
                    .descriptor
                    .as_ref()
                    .ok_or(ProviderError::MaterialUnavailable)?;
                let canonical = devworks_core::jcs::canonicalize(descriptor)
                    .map_err(|_| ProviderError::MaterialUnavailable)?;
                if sha256_hex(canonical.as_bytes()) != contribution.contract_digest
                    || descriptor["meta"]["inputSchema"] != json!(alias.input_schema)
                {
                    return Err(ProviderError::MaterialUnavailable);
                }
                let schema = self.package_document(ctx, &alias.input_schema)?;
                let help = self.file(&inspection, &inspection.help_file, 1024 * 1024)?;
                let description =
                    String::from_utf8(help).map_err(|_| ProviderError::MaterialUnavailable)?;
                if self.package_inspection(ctx, package)?.1 != revision {
                    return Err(ProviderError::MaterialUnavailable);
                }
                (schema, Some(description))
            }
        };
        Ok(PreparedTool {
            wire_name: alias.wire_name.clone(),
            capability_id: alias.capability_id.clone(),
            input_schema: alias.input_schema.clone(),
            schema,
            description,
        })
    }
}
