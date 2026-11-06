//! Exact saved package operations use the ordinary inspector and current package policy.
//! Reading a review never publishes files, drains helpers, trusts a publisher or activates code.
use super::*;
use crate::admission::DecodedRequest;
use crate::ports::packages::InspectedBundle;
use crate::ports::storage::ConfigScope;
use crate::services::approvals::OwnerPreview;
use crate::services::approvals::owner_preview::artifact_binding;

pub(super) type BundleInspection = Result<(InspectedBundle, Value), (String, String, Value)>;
pub(super) struct PackageProposalDecision<'a> {
    pub proposal_id: &'a Id,
    pub expected_revision: i64,
    pub digest: &'a str,
    pub approve: bool,
    pub rationale: &'a str,
}
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
        let preview = env.read(|s| {
            Ok((|| {
                let artifact = inspected
                    .as_ref()
                    .map(|(_, _, r)| artifact_binding(s.artifacts(), r))
                    .transpose()?;
                self.package_approval(s.packages(), ctx, request, inspected.as_ref(), artifact)
            })())
        })??;
        let candidate = preview
            .binding
            .get("candidate")
            .filter(|v| !v.is_null())
            .map(|v| serde_json::from_value::<Inspection>(v.clone()))
            .transpose()
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        let bundle = preview
            .binding
            .pointer("/source/bundle")
            .or_else(|| preview.binding.pointer("/version/provenance/source"))
            .filter(|v| !v.is_null())
            .map(|v| serde_json::from_value::<ArtifactRef>(v.clone()))
            .transpose()
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        let body = match (candidate, bundle) {
            (Some(candidate), Some(bundle)) => {
                env.read(|s| Ok(self.review_facts(s.packages(), &candidate, &bundle)))??
            }
            _ => json!({"binding":preview.binding,"reviewed":preview.reviewed}),
        };
        let large_facts = serde_json::to_vec(&body)
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?
            .len()
            >= 32 * 1024;
        let large_review = serde_json::to_vec(&preview.reviewed)
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?
            .len()
            >= 256 * 1024;
        if !large_facts && !large_review {
            return Ok(preview);
        }
        let reference = self.publish_review_facts(ctx, request, env, &body)?;
        let compact = env.read(|s| {
            Ok(self.compact_review(s.artifacts(), preview.clone(), &reference, &body))
        })??;
        if serde_json::from_value::<crate::protocol::support::DynamicJson>(compact.reviewed.clone())
            .is_ok()
        {
            return Ok(compact);
        }
        let body = json!({"binding":preview.binding,"reviewed":preview.reviewed});
        let reference = self.publish_review_facts(ctx, request, env, &body)?;
        env.read(|s| Ok(self.compact_review(s.artifacts(), preview, &reference, &body)))?
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
        let mut preview = OwnerPreview {
            scope: ctx
                .project_id()
                .cloned()
                .map_or(ConfigScope::Workspace, ConfigScope::Project),
            target_id: Id::new(&package_id).map_err(|_| invalid("package_id"))?,
            target_revision: prior.as_ref().map_or(1, |p| p.revision),
            summary: format!("Review {} for {}", request.descriptor.name, package_id),
            reviewed: json!({"operation":request.descriptor.name,"input":request.normalized,"inspection":inspected_json,
                "source":source,"current":package,"storedVersion":stored_version,"proposal":proposal,"compatibility":compatibility,"artifact":source.as_ref().map(|s|&s["bundle"])}),
            binding: json!({"registryRevision":registry,"package":package,"version":stored_version,"proposal":proposal,"artifact":artifact,"candidate":inspected_json,"source":source}),
        };
        if preview.reviewed["artifact"].is_null() {
            preview
                .reviewed
                .as_object_mut()
                .expect("owner preview")
                .remove("artifact");
        }
        Ok(preview)
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
        let expanded = self.expand_review(tx, ctx, preview)?;
        let preview = &expanded;
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

impl PackageService {
    /// Actual dispatch validates the full approved owner snapshot before any business write.
    /// Normal owner preparation has just re-read/verified the signed bundle; receipt minting
    /// below is limited to that exact inspected version and never establishes publisher trust.
    pub(super) fn reviewed_package_binding(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
    ) -> Result<Option<Value>, Failure> {
        let Some(binding) =
            crate::services::approvals::owner_preview::approved_binding(tx, ctx, command)?
        else {
            return Ok(None);
        };
        let binding = self.expand_review_binding(tx, ctx, &binding)?;
        let control = self
            .approval_control
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        crate::services::approvals::owner_preview::recheck_control(tx, ctx, control.as_deref())?;
        let input = match command.input() {
            OperationInput::Addons(ops::Input::Configure(i)) => {
                serde_json::to_value(i).map_err(|e| StorageError::InvalidRecord(e.to_string()))?
            }
            input => match lifecycle(input) {
                Some((_, cmd)) => json!({"command":cmd}),
                None => return Err(invalid("saved_package_operation").into()),
            },
        };
        let request = crate::services::approvals::requests::decode(
            command.descriptor().name,
            command.schema_revision().0,
            &input,
            true,
        )
        .map_err(|_| invalid("saved_package_operation"))?;
        if request.fingerprint.as_str() != command.fingerprint().as_str() {
            return Err(invalid("saved_package_operation").into());
        }
        let inspected = command.prepared::<Prep>().and_then(|p| match p {
            Prep::Bundle(b) => Some(b.as_ref()),
            _ => None,
        });
        let source = inspected
            .map(|b| {
                let i = b.outcome.as_ref().map_err(|_| invalid("package_bundle"))?;
                Ok::<_, Failure>((i.clone(), b.provenance.clone(), b.bundle.clone()))
            })
            .transpose()?;
        let artifact = source
            .as_ref()
            .map(|(_, _, reference)| artifact_binding(tx.artifacts(), reference))
            .transpose()?;
        let current =
            self.package_approval(tx.packages(), ctx, &request, source.as_ref(), artifact)?;
        if current.binding != binding {
            return Err(Refusal::new(
                DomainErrorCode::SourceChanged,
                "saved_package_operation",
                Some(current.target_id),
            )
            .into());
        }
        Ok(Some(binding))
    }

    pub(super) fn approve_reviewed_version(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        binding: &Value,
        inspection: &Inspection,
    ) -> Result<Value, Failure> {
        let evidence = ctx.reviewed_operation().ok_or_else(|| {
            Refusal::new(
                DomainErrorCode::PermissionDenied,
                "package_version_approval",
                None,
            )
        })?;
        if !matches!(evidence.operation(), "addons.install" | "addons.update")
            || *binding != self.expand_review_binding(tx, ctx, evidence.owner_binding())?
            || binding["candidate"]
                != serde_json::to_value(inspection)
                    .map_err(|e| StorageError::InvalidRecord(e.to_string()))?
        {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "package_version_approval",
                None,
            )
            .into());
        }
        let checked = crate::services::approvals::owner_preview::approved_binding_for(
            tx,
            ctx,
            evidence.operation(),
            evidence.schema_revision(),
            evidence.request_digest(),
        )?;
        if checked
            .as_ref()
            .map(|b| self.expand_review_binding(tx, ctx, b))
            .transpose()?
            .as_ref()
            != Some(binding)
        {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "package_version_approval",
                None,
            )
            .into());
        }
        let now = tx.now_ms();
        let mut receipt = json!({"kind":"Person","by":evidence.approver(),"atMs":now,"approvalId":evidence.approval_id(),
            "manifestDigest":inspection.manifest_digest,"bundleDigest":inspection.bundle_digest,"signerKey":inspection.signer_key});
        if let Some(proposal_id) = binding
            .pointer("/proposal/id")
            .and_then(Value::as_str)
            .and_then(|s| Id::new(s).ok())
        {
            let proposal = tx
                .packages()
                .proposal(&proposal_id)?
                .ok_or_else(|| not_found("package_proposal", proposal_id.as_str()))?;
            if proposal.state != "Pending"
                || proposal.manifest_digest != inspection.manifest_digest
                || proposal.bundle_digest != inspection.bundle_digest
                || proposal.revision != binding["proposal"]["revision"].as_i64().unwrap_or(-1)
            {
                return Err(conflict("package_proposal", proposal_id.as_str()).into());
            }
            let mut approved = proposal.clone();
            approved.state = "Approved".into();
            approved.decided_by = Some(evidence.approver().clone());
            approved.decided_at_ms = Some(now);
            approved.decision = Some(
                json!({"decision":"Approve","approvalId":evidence.approval_id(),"by":evidence.approver()}),
            );
            tx.packages()
                .update_proposal(&approved, proposal.revision)?;
            receipt["proposalId"] = json!(proposal_id);
        }
        Ok(receipt)
    }
}

impl PackageService {
    pub(crate) fn decide_approval_source(
        &self,
        tx: &mut dyn WriteTransaction,
        human: &InvocationContext,
        _request: &DecodedRequest,
        preview: &OwnerPreview,
        approve: bool,
    ) -> Result<(), Failure> {
        let expanded = self.expand_review_mode(tx.artifacts(), preview, approve)?;
        let preview = &expanded;
        let Some(id) = preview
            .binding
            .pointer("/proposal/id")
            .and_then(Value::as_str)
            .and_then(|s| Id::new(s).ok())
        else {
            return Ok(());
        };
        let Some(source) = tx.packages().proposal(&id)? else {
            if !approve {
                return Ok(());
            }
            return Err(not_found("package_proposal", id.as_str()).into());
        };
        let revision = preview.binding["proposal"]["revision"]
            .as_i64()
            .ok_or_else(|| invalid("package_proposal"))?;
        let Some(version) = tx
            .packages()
            .package_version(&source.package_id, &source.manifest_digest)?
        else {
            if !approve {
                return Ok(());
            }
            return Err(not_found("package_version", &source.package_id).into());
        };
        if source.revision != revision
            || source.state != "Pending"
            || version_binding(&version) != preview.binding["version"]
        {
            if !approve {
                return Ok(());
            }
            return Err(
                Refusal::new(DomainErrorCode::SourceChanged, "package_proposal", Some(id)).into(),
            );
        }
        if human.principal_kind() != PrincipalKind::Human
            || tx.actors().actor_kind(human.principal())? != Some(PrincipalKind::Human)
        {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "package_proposal",
                Some(source.id),
            )
            .into());
        }
        if !approve {
            self.decide_package_proposal_in(
                tx,
                human,
                PackageProposalDecision {
                    proposal_id: &source.id,
                    expected_revision: source.revision,
                    digest: &source.manifest_digest,
                    approve: false,
                    rationale: "Rejected through the exact global approval review",
                },
            )?
            .map_err(Failure::from)?;
        }
        // Approval itself deliberately writes no version receipt. The saved command retains
        // its original actor and must pass current root holds/authority before the owner mints
        // that receipt in its actual install/update unit of work. retry_pending cannot bypass
        // a paused root merely because its human decision was recorded.
        Ok(())
    }
}

/// Build a source-reference request from an actual staged owner proposal. No caller's summary,
/// claimed signer or fabricated inspection can become the reviewed package contents.
pub fn draft_from_package_proposal(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    proposal: &StoredPackageProposal,
    version: &StoredPackageVersion,
) -> Result<Option<crate::operations::approvals::ProposeInput>, Failure> {
    draft_from_package_source(tx.packages(), ctx, proposal, version)
}
fn draft_from_package_source(
    read: &dyn PackageRegistryRead,
    ctx: &InvocationContext,
    proposal: &StoredPackageProposal,
    version: &StoredPackageVersion,
) -> Result<Option<crate::operations::approvals::ProposeInput>, Failure> {
    if proposal.state != "Pending" {
        return Ok(None);
    }
    if proposal.package_id != version.package_id
        || proposal.manifest_digest != version.manifest_digest
        || proposal.bundle_digest != version.bundle_digest
        || version.phase == VersionPhase::Rejected
    {
        return Err(Refusal::new(
            DomainErrorCode::SourceChanged,
            "package_proposal",
            Some(proposal.id.clone()),
        )
        .into());
    }
    let package = read
        .package(&proposal.package_id)?
        .ok_or_else(|| not_found("package", &proposal.package_id))?;
    let bundle: ArtifactRef = serde_json::from_value(proposal.summary["source"].clone())
        .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
    let decoded = source_review_request(read, &package.package_id, &bundle)?;
    let operation = decoded.descriptor.name;
    let input = decoded.normalized.clone();
    let scope = ctx.project_id().map_or(
        json!({"kind":"Workspace"}),
        |id| json!({"kind":"Project","projectId":id}),
    );
    let input=serde_json::from_value(json!({"scope":scope,"operationKind":operation,
        "target":{"id":proposal.package_id,"revision":rev(package.revision.max(1))},
        "contentDigest":decoded.fingerprint.as_str(),"summary":format!("Review {} version {}",proposal.package_id,proposal.version),
        "artifact":bundle,"payload":{"kind":"SavedOperation","schemaRevision":"1","input":input}}))
        .map_err(|e|StorageError::InvalidRecord(e.to_string()))?;
    Ok(Some(input))
}
/// The same owner-selected install/update request is used by source staging and publication.
pub(super) fn source_review_request(
    read: &dyn PackageRegistryRead,
    package_id: &str,
    bundle: &ArtifactRef,
) -> Result<DecodedRequest, Failure> {
    let package = read.package(package_id)?;
    let registry = read.registry_revision()?;
    let (operation, command) = if package.as_ref().is_some_and(|p| p.active_digest.is_some()) {
        (
            "addons.update",
            json!({"kind":"Update","packageId":package_id,"bundle":bundle,"expectedRegistryRevision":rev(registry)}),
        )
    } else {
        (
            "addons.install",
            json!({"kind":"Install","bundle":bundle,"expectedRegistryRevision":rev(registry)}),
        )
    };
    crate::services::approvals::requests::decode(operation, 1, &json!({"command":command}), true)
        .map_err(|_| invalid("package_approval_request").into())
}

impl PackageService {
    pub(super) fn stage_package_review(
        &self,
        tx: &mut dyn WriteTransaction,
        _ctx: &InvocationContext,
        proposal: &StoredPackageProposal,
        prepared_facts: Option<&(ArtifactRef, Value)>,
    ) -> Result<(), Failure> {
        let Some(source_context) = package_source_context(tx, proposal)? else {
            return Ok(());
        };
        let ctx = &source_context;
        let version = tx
            .packages()
            .package_version(&proposal.package_id, &proposal.manifest_digest)?
            .ok_or_else(|| not_found("package_version", &proposal.package_id))?;
        let Some(input) = draft_from_package_proposal(tx, ctx, proposal, &version)? else {
            return Ok(());
        };
        let crate::protocol::records::ApprovalContent::SavedOperation {
            schema_revision,
            input: args,
        } = input.payload.as_ref().expect("owner draft payload")
        else {
            unreachable!("saved package source")
        };
        let request = crate::services::approvals::requests::decode(
            &input.operation_kind.0,
            schema_revision.0.parse().unwrap_or(0),
            &args.0,
            true,
        )
        .map_err(|_| invalid("package_approval_request"))?;
        if !crate::services::approvals::validate_authority(tx, ctx, &request)? {
            return Ok(());
        }
        let inspection = policy::inspection(&version).ok_or_else(|| invalid("package_version"))?;
        let bundle: ArtifactRef = serde_json::from_value(proposal.summary["source"].clone())
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        let artifact = artifact_binding(tx.artifacts(), &bundle)?;
        let source = (inspection, artifact["provenance"].clone(), bundle);
        let mut preview =
            self.package_approval(tx.packages(), ctx, &request, Some(&source), Some(artifact))?;
        if let Some((reference, body)) = prepared_facts {
            let current_facts = self.review_facts(tx.packages(), &source.0, &source.2)?;
            if &current_facts != body {
                return Err(Refusal::new(
                    DomainErrorCode::SourceChanged,
                    "package_review_facts",
                    Some(proposal.id.clone()),
                )
                .into());
            }
            preview = self.compact_review(tx.artifacts(), preview, reference, body)?;
        }
        // Repeated source reconciliation must not create another human decision for one exact
        // package proposal. It may be Pending, Approved-awaiting-readmission, or terminal.
        let mut after = None;
        loop {
            let rows = tx.decisions().list_approval_sources(
                &crate::ports::storage::ApprovalSourceFilter {
                    scope: preview.scope.clone(),
                    state: None,
                },
                after.as_ref(),
                500,
            )?;
            let authority = crate::services::approvals::authority_binding(tx, ctx)?;
            let digest =
                crate::services::approvals::scope_digest(&preview.binding, Some(&authority));
            for row in &rows {
                if row.source == crate::ports::storage::ApprovalSource::Generic
                    && let Some(existing) = tx.decisions().approval(&row.id)?
                {
                    // Source identity lives in the retained owner facts, which may be compacted
                    // into either review envelope. Reconciliation must also recognize obsolete
                    // evidence generations so that their pending reviews can be superseded.
                    let historical =
                        crate::services::approvals::review_evidence::expand_historical(
                            &OwnerPreview {
                                scope: existing.scope.clone(),
                                target_id: existing.target_id.clone(),
                                target_revision: existing.target_revision,
                                summary: existing.summary.clone(),
                                reviewed: existing.payload["reviewed"].clone(),
                                binding: existing.payload["binding"].clone(),
                            },
                        )?;
                    let owner = self.expand_review_mode(tx.artifacts(), &historical, false)?;
                    if owner
                        .binding
                        .pointer("/proposal/id")
                        .and_then(Value::as_str)
                        != Some(proposal.id.as_str())
                    {
                        continue;
                    }
                    if matches!(existing.state.as_str(), "Pending" | "Approved")
                        && existing.content_digest == request.fingerprint.as_str()
                        && existing.scope_digest == digest
                    {
                        return Ok(());
                    }
                    if existing.state == "Pending" {
                        let mut superseded = existing.clone();
                        superseded.revision += 1;
                        superseded.state = "Superseded".into();
                        tx.decisions()
                            .put_approval(&superseded, Some(existing.revision))?;
                    }
                }
            }
            if rows.len() < 500 {
                break;
            }
            let last = rows.last().expect("full page");
            after = Some(crate::ports::storage::ApprovalSourcePosition {
                created_at_ms: last.created_at_ms,
                id: last.id.clone(),
                source: last.source,
            });
        }
        crate::services::approvals::stage_saved(tx, ctx, &input, &request, &preview)?;
        Ok(())
    }
}

pub(super) fn package_source_context(
    tx: &mut dyn WriteTransaction,
    proposal: &StoredPackageProposal,
) -> Result<Option<InvocationContext>, Failure> {
    let Some(saved) = proposal.origin.get("sourceContext") else {
        // Existing owner proposals remain visible, but cannot attest an unrecorded invocation.
        return Ok(None);
    };
    let kind = tx
        .actors()
        .actor_kind(&proposal.originator)?
        .ok_or_else(|| {
            Refusal::new(
                DomainErrorCode::NotFound,
                "package_proposal_author",
                Some(proposal.originator.0.clone()),
            )
        })?;
    if proposal.origin["kind"].as_str() != Some(kind_name(kind)) {
        return Err(Refusal::new(
            DomainErrorCode::SourceChanged,
            "package_proposal_author",
            Some(proposal.originator.0.clone()),
        )
        .into());
    }
    match crate::services::approvals::restore_source_context(
        tx.store(),
        &proposal.originator,
        kind,
        saved,
    ) {
        Ok(context) => Ok(Some(context)),
        Err(StorageError::InvalidRecord(_)) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

impl PackageService {
    /// Bootstrap the current owner proposal inventory through the same source-reference
    /// producer as addons.propose. Old pending work is visible without running or approving it.
    /// Each source gets its own bounded unit of work; no arbitrary host authority is inherited.
    pub fn sync_pending_reviews(&self) -> Result<usize, StorageError> {
        let mut after = None;
        let mut count = 0usize;
        loop {
            let page = crate::ports::storage::read_snapshot(
                &*self.store,
                &crate::ports::storage::ReadOptions::default(),
                |s| s.packages().packages(after.as_deref(), 500),
            )?;
            let more = page.len() == 500;
            for package in &page {
                let Some(digest) = &package.pending_digest else {
                    continue;
                };
                let package_id = package.package_id.clone();
                let digest = digest.clone();
                // Capture and publish immutable review facts before acquiring the writer.
                let prepared = crate::ports::storage::read_snapshot(
                    &*self.store,
                    &crate::ports::storage::ReadOptions::default(),
                    |s| {
                        let Some(source) = s
                            .packages()
                            .proposal_for(&package_id, &digest)?
                            .filter(|p| p.state == "Pending")
                        else {
                            return Ok(None);
                        };
                        let Some(saved) = source.origin.get("sourceContext") else {
                            return Ok(None);
                        };
                        let kind = s.actors().actor_kind(&source.originator)?.ok_or_else(|| {
                            StorageError::InvalidRecord("package source actor missing".into())
                        })?;
                        let Ok(context) = crate::services::approvals::restore_source_context(
                            &self.store.info(),
                            &source.originator,
                            kind,
                            saved,
                        ) else {
                            return Ok(None);
                        };
                        let version = s
                            .packages()
                            .package_version(&package_id, &digest)?
                            .ok_or_else(|| {
                                StorageError::InvalidRecord("package source version missing".into())
                            })?;
                        let Some(input) =
                            draft_from_package_source(s.packages(), &context, &source, &version)
                                .map_err(review_failure)?
                        else {
                            return Ok(None);
                        };
                        let Some(crate::protocol::records::ApprovalContent::SavedOperation {
                            schema_revision,
                            input: args,
                        }) = input.payload
                        else {
                            return Ok(None);
                        };
                        let request = crate::services::approvals::requests::decode(
                            &input.operation_kind.0,
                            schema_revision.0.parse().unwrap_or(0),
                            &args.0,
                            true,
                        )
                        .map_err(|_| {
                            StorageError::InvalidRecord("package review request invalid".into())
                        })?;
                        if !crate::services::approvals::prepare_authority(s, &context, &request)? {
                            return Ok(None);
                        };
                        let inspection = policy::inspection(&version).ok_or_else(|| {
                            StorageError::InvalidRecord("package inspection missing".into())
                        })?;
                        let bundle: ArtifactRef =
                            serde_json::from_value(source.summary["source"].clone())
                                .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
                        if !review_source_available(s.artifacts(), &bundle)? {
                            return Ok(None);
                        }
                        let facts = self
                            .review_facts(s.packages(), &inspection, &bundle)
                            .map_err(review_failure)?;
                        Ok(Some((source.id, source.revision, context, request, facts)))
                    },
                )?;
                let Some((source_id, source_revision, context, request, facts)) = prepared else {
                    continue;
                };
                let prepared_facts = if serde_json::to_vec(&facts)
                    .map_err(|e| StorageError::InvalidRecord(e.to_string()))?
                    .len()
                    >= 32 * 1024
                {
                    let reference = match self.publish_review_facts(
                        &context,
                        &request,
                        &Preparation::new(&*self.store, None, None),
                        &facts,
                    ) {
                        Ok(reference) => reference,
                        // A source can lose authority after the read-side check. It remains
                        // pending; one revoked source must not abort application bootstrap.
                        Err(Failure::Storage(StorageError::ScopeDenied)) => continue,
                        Err(Failure::Refused(refusal))
                            if refusal.code == DomainErrorCode::PermissionDenied =>
                        {
                            continue;
                        }
                        Err(failure) => return Err(review_failure(failure)),
                    };
                    Some((reference, facts))
                } else {
                    None
                };
                let service = self.clone_ports();
                let produced = run_write(&*self.store, WriteOptions::default(), move |tx| {
                    let Some(source) = tx
                        .packages()
                        .proposal_for(&package_id, &digest)?
                        .filter(|p| p.state == "Pending")
                    else {
                        return Ok(false);
                    };
                    if source.id != source_id || source.revision != source_revision {
                        return Ok(false);
                    };
                    let bundle: ArtifactRef =
                        serde_json::from_value(source.summary["source"].clone())
                            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
                    // The source may disappear after preparation. Check before staging can
                    // supersede any old review, within the same writer snapshot as staging.
                    if !review_source_available(tx.artifacts(), &bundle)? {
                        return Ok(false);
                    }
                    let context = package_source_context(tx, &source).map_err(|e| match e {
                        Failure::Storage(e) => e,
                        Failure::Refused(r) => StorageError::InvalidRecord(format!(
                            "pending package review refused: {:?} {}",
                            r.code, r.target_kind
                        )),
                    })?;
                    let Some(context) = context else {
                        return Ok(false);
                    };
                    service
                        .stage_package_review(tx, &context, &source, prepared_facts.as_ref())
                        .map_err(|e| match e {
                            Failure::Storage(e) => e,
                            Failure::Refused(r) => StorageError::InvalidRecord(format!(
                                "pending package review refused: {:?} {}",
                                r.code, r.target_kind
                            )),
                        })?;
                    Ok(true)
                })?;
                count += usize::from(produced);
            }
            after = page.last().map(|p| p.package_id.clone());
            if !more {
                return Ok(count);
            }
        }
    }
}

fn review_source_available(
    read: &dyn crate::ports::storage::ArtifactRead,
    reference: &ArtifactRef,
) -> Result<bool, StorageError> {
    match artifact_binding(read, reference) {
        Ok(_) => Ok(true),
        // A retained proposal is still inspectable/rejectable when its immutable bundle is
        // unavailable. This is non-actionable inventory, not evidence of store corruption.
        Err(Failure::Refused(refusal))
            if refusal.code == DomainErrorCode::SourceChanged
                && refusal.target_kind == "approval_artifact" =>
        {
            Ok(false)
        }
        Err(failure) => Err(review_failure(failure)),
    }
}

fn review_failure(failure: Failure) -> StorageError {
    match failure {
        Failure::Storage(e) => e,
        Failure::Refused(r) => StorageError::InvalidRecord(format!(
            "package review refused: {:?} {}",
            r.code, r.target_kind
        )),
    }
}
