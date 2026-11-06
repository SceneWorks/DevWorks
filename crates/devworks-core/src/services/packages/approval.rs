//! Exact saved package operations use the ordinary inspector and current package policy.
//! Reading a review never publishes files, drains helpers, trusts a publisher or activates code.
use super::*;
use crate::admission::DecodedRequest;
use crate::ports::packages::InspectedBundle;
use crate::ports::storage::ConfigScope;
use crate::services::approvals::OwnerPreview;
use crate::services::approvals::owner_preview::artifact_binding;

pub(super) type BundleInspection = Result<(InspectedBundle, Value), (String, String, Value)>;
impl PackageService {
    /// Shared by ordinary install/proposal preparation and approval review, before publication.
    pub(super) fn inspect_bundle_source(
        &self,
        ctx: &InvocationContext,
        bundle: &ArtifactRef,
    ) -> BundleInspection {
        let (bytes, provenance) = self
            .artifacts
            .read_exact(&*self.store, ctx, bundle, MAX_BUNDLE_BYTES)
            .map_err(|e| match e {
                ExactReadRefusal::NotFound => (
                    "BundleUnavailable".into(),
                    "The bundle artifact is not available".into(),
                    Value::Null,
                ),
                ExactReadRefusal::Unreadable => (
                    "BundleUnavailable".into(),
                    "The bundle artifact cannot be read".into(),
                    Value::Null,
                ),
                ExactReadRefusal::TooLarge { bytes, max } => (
                    "BundleTooLarge".into(),
                    format!("The bundle is {bytes} bytes; the limit is {max}"),
                    Value::Null,
                ),
                ExactReadRefusal::Storage(e) => {
                    ("BundleUnavailable".into(), format!("{e:?}"), Value::Null)
                }
            })?;
        let inspected = self
            .inspector
            .inspect(&bytes)
            .map_err(|e| (e.code().into(), e.detail().to_owned(), provenance.clone()))?;
        Ok((inspected, provenance))
    }
    pub(crate) fn approval_preview(
        &self,
        ctx: &InvocationContext,
        request: &DecodedRequest,
        env: &Preparation<'_>,
    ) -> Result<OwnerPreview, Failure> {
        let inspected = match lifecycle(&request.input).map(|(_, cmd)| cmd) {
            Some(AddonCommand::Install { bundle, .. } | AddonCommand::Update { bundle, .. }) => {
                let (inspected, provenance) =
                    self.inspect_bundle_source(ctx, bundle)
                        .map_err(|(code, _, _)| {
                            Refusal::new(
                                if code == "BundleUnavailable" {
                                    DomainErrorCode::NotFound
                                } else {
                                    DomainErrorCode::InvalidArgument
                                },
                                "package_bundle",
                                Some(bundle.version_id.clone()),
                            )
                        })?;
                Some((inspected.inspection, provenance, bundle.clone()))
            }
            _ => None,
        };
        env.read(|s| {
            Ok((|| {
                let artifact = inspected
                    .as_ref()
                    .map(|(_, _, reference)| artifact_binding(s.artifacts(), reference))
                    .transpose()?;
                self.package_approval(s.packages(), ctx, request, inspected.as_ref(), artifact)
            })())
        })?
    }

    fn package_approval(
        &self,
        read: &dyn PackageRegistryRead,
        ctx: &InvocationContext,
        request: &DecodedRequest,
        inspected: Option<&(Inspection, Value, ArtifactRef)>,
        artifact: Option<Value>,
    ) -> Result<OwnerPreview, Failure> {
        let (package_id, expected_registry, target_digest) =
            match lifecycle(&request.input).map(|(_, cmd)| cmd) {
                Some(AddonCommand::Install {
                    expected_registry_revision,
                    ..
                }) => {
                    let i = &inspected.ok_or_else(|| invalid("package_bundle"))?.0;
                    (
                        i.package_id.clone(),
                        Some(expected_registry_revision.get()),
                        Some(i.manifest_digest.clone()),
                    )
                }
                Some(AddonCommand::Update {
                    package_id,
                    expected_registry_revision,
                    ..
                }) => {
                    let i = &inspected.ok_or_else(|| invalid("package_bundle"))?.0;
                    if package_id.as_str() != i.package_id {
                        return Err(invalid("package_id").into());
                    }
                    (
                        package_id.as_str().into(),
                        Some(expected_registry_revision.get()),
                        Some(i.manifest_digest.clone()),
                    )
                }
                Some(AddonCommand::Enable {
                    package_id,
                    expected_registry_revision,
                }) => (
                    package_id.as_str().into(),
                    Some(expected_registry_revision.get()),
                    None,
                ),
                Some(AddonCommand::Rollback {
                    package_id,
                    target_manifest_digest,
                    expected_registry_revision,
                    ..
                }) => (
                    package_id.as_str().into(),
                    Some(expected_registry_revision.get()),
                    Some(target_manifest_digest.0.clone()),
                ),
                _ => match &request.input {
                    OperationInput::Addons(ops::Input::Configure(i)) => {
                        (i.package_id.as_str().into(), None, None)
                    }
                    _ => {
                        return Err(Refusal::new(
                            DomainErrorCode::UnsupportedCapability,
                            "saved_package_operation",
                            None,
                        )
                        .into());
                    }
                },
            };
        let registry = read.registry_revision()?;
        if let Some(expected) = expected_registry
            && registry != expected
        {
            return Err(registry_conflict(expected, registry).into());
        }
        let prior = read.package(&package_id)?;
        if matches!(
            &request.input,
            OperationInput::Addons(ops::Input::Install(_))
        ) {
            if prior.as_ref().is_some_and(|p| p.installed) {
                return Err(conflict("package", &package_id).into());
            }
            if prior.is_none() {
                Self::admit_identity(read, &package_id)?;
            }
        } else if prior.as_ref().is_none_or(|p| !p.installed) {
            return Err(not_found("package", &package_id).into());
        }
        let digest = target_digest.or_else(|| prior.as_ref().and_then(|p| p.active_digest.clone()));
        let version = digest
            .as_ref()
            .map(|d| read.package_version(&package_id, d))
            .transpose()?
            .flatten();
        if version
            .as_ref()
            .is_some_and(|v| v.phase == VersionPhase::Rejected)
        {
            return Err(conflict("package_version", &package_id).into());
        }
        if let OperationInput::Addons(ops::Input::Rollback(i)) = &request.input {
            let AddonCommand::Rollback {
                expected_state_generation,
                ..
            } = &i.command
            else {
                unreachable!("bound rollback")
            };
            let p = prior.as_ref().expect("existing package");
            if p.state_generation != expected_state_generation.get() {
                return Err(package_conflict(
                    &package_id,
                    RevisionDimension::Package,
                    expected_state_generation.get(),
                    p.state_generation,
                )
                .into());
            }
            if p.active_digest != digest
                && version
                    .as_ref()
                    .is_none_or(|v| v.phase != VersionPhase::Installed || v.approval.is_none())
            {
                return Err(not_found("package_version", &package_id).into());
            }
        }
        let candidate = inspected
            .map(|(i, _, _)| i.clone())
            .or_else(|| version.as_ref().and_then(policy::inspection));
        if let OperationInput::Addons(ops::Input::Configure(i)) = &request.input {
            let p = prior.as_ref().expect("existing package");
            if p.revision != i.expected_revision.get() {
                return Err(package_conflict(
                    &package_id,
                    RevisionDimension::Configuration,
                    i.expected_revision.get(),
                    p.revision,
                )
                .into());
            }
            self.check_approval_configuration(ctx, p, candidate.as_ref(), i)?;
        }
        let package = prior.as_ref().map(package_binding);
        let stored_version = version.as_ref().map(version_binding);
        let proposal = digest
            .as_ref()
            .map(|d| read.proposal_for(&package_id, d))
            .transpose()?
            .flatten();
        let proposal=proposal.map(|p|json!({"id":p.id,"revision":p.revision,"state":p.state,"originator":p.originator,
            "manifestDigest":p.manifest_digest,"bundleDigest":p.bundle_digest,"origin":p.origin,"summary":p.summary,"decision":p.decision}));
        let inspected_json = candidate
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        let compatibility = candidate
            .as_ref()
            .and_then(|i| policy::static_incompatibility(i, &self.platform));
        let source = inspected
            .map(|(_, provenance, bundle)| json!({"bundle":bundle,"provenance":provenance}));
        Ok(OwnerPreview {
            scope: ctx
                .project_id()
                .cloned()
                .map_or(ConfigScope::Workspace, ConfigScope::Project),
            target_id: Id::new(&package_id).map_err(|_| invalid("package_id"))?,
            target_revision: prior.as_ref().map_or(1, |p| p.revision),
            summary: format!("Review {} for {}", request.descriptor.name, package_id),
            reviewed: json!({"operation":request.descriptor.name,"input":request.normalized,"inspection":inspected_json,
                "source":source,"current":package,"storedVersion":stored_version,"proposal":proposal,"compatibility":compatibility}),
            binding: json!({"registryRevision":registry,"package":package,"version":stored_version,"proposal":proposal,"artifact":artifact}),
        })
    }
    fn check_approval_configuration(
        &self,
        ctx: &InvocationContext,
        p: &StoredPackage,
        inspection: Option<&Inspection>,
        i: &ops::ConfigureInput,
    ) -> Result<(), Failure> {
        let schema = serde_json::to_value(&i.config.schema)
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        let schema_id = schema["schemaId"].as_str().unwrap_or_default();
        let doc = if let Some(doc) = host_schema(schema_id) {
            if Some(&schema) != host_schema_ref(schema_id).as_ref() {
                return Err(Refusal::new(
                    DomainErrorCode::SchemaMismatch,
                    "package_configuration",
                    None,
                )
                .into());
            }
            if schema_id == UPDATE_POLICY_SCHEMA && ctx.principal_kind() != PrincipalKind::Human {
                return Err(Refusal::new(
                    DomainErrorCode::PermissionDenied,
                    "publisher_trust",
                    Id::new(&p.package_id).ok(),
                )
                .into());
            }
            doc
        } else {
            let active = inspection.ok_or_else(|| not_found("package_version", &p.package_id))?;
            if active.configuration_schema != schema {
                return Err(Refusal::new(
                    DomainErrorCode::SchemaMismatch,
                    "package_configuration",
                    None,
                )
                .into());
            }
            active
                .configuration_schema_document
                .clone()
                .ok_or_else(|| {
                    Refusal::new(
                        DomainErrorCode::UnsupportedCapability,
                        "package_configuration",
                        None,
                    )
                })?
        };
        self.validator
            .validate(&doc, &i.config.value.0)
            .map_err(|_| {
                Refusal::new(
                    DomainErrorCode::InvalidArgument,
                    "package_configuration",
                    None,
                )
            })?;
        Ok(())
    }
    pub(crate) fn validate_approval_preview(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        request: &DecodedRequest,
        preview: &OwnerPreview,
    ) -> Result<(), Failure> {
        let inspected = preview
            .reviewed
            .get("inspection")
            .filter(|v| !v.is_null())
            .map(|v| serde_json::from_value::<Inspection>(v.clone()))
            .transpose()
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        let source = preview
            .reviewed
            .get("source")
            .filter(|v| !v.is_null())
            .map(|v| {
                let bundle = serde_json::from_value::<ArtifactRef>(v["bundle"].clone())
                    .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
                let i = inspected.clone().ok_or_else(|| invalid("package_bundle"))?;
                Ok::<_, Failure>((i, v["provenance"].clone(), bundle))
            })
            .transpose()?;
        let artifact = source
            .as_ref()
            .map(|(_, _, bundle)| artifact_binding(tx.artifacts(), bundle))
            .transpose()?;
        let current =
            self.package_approval(tx.packages(), ctx, request, source.as_ref(), artifact)?;
        if current.binding != preview.binding
            || current.reviewed != preview.reviewed
            || current.target_id != preview.target_id
            || current.target_revision != preview.target_revision
            || current.scope != preview.scope
        {
            return Err(Refusal::new(
                DomainErrorCode::SourceChanged,
                "saved_package_operation",
                Some(preview.target_id.clone()),
            )
            .into());
        }
        Ok(())
    }
}
fn package_binding(p: &StoredPackage) -> Value {
    json!({"id":p.package_id,"revision":p.revision,"enabled":p.enabled,"installed":p.installed,
        "activeDigest":p.active_digest,"pendingDigest":p.pending_digest,"predecessorDigest":p.predecessor_digest,
        "stateGeneration":p.state_generation,"config":p.config,"configRevision":p.config_revision,"trust":p.trust,"failure":p.failure})
}
fn version_binding(v: &StoredPackageVersion) -> Value {
    json!({"manifestDigest":v.manifest_digest,"bundleDigest":v.bundle_digest,"version":v.version,"publisherId":v.publisher_id,
        "signerKey":v.signer_key,"inspection":v.inspection,"phase":v.phase.as_str(),"waits":v.waits,"approval":v.approval,"provenance":v.provenance})
}
