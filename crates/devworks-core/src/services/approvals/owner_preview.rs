//! Owner-derived saved-operation review. A caller supplies the selected request; only its
//! existing business owner can attest current targets, effective contents and validation.
use crate::admission::DecodedRequest;
use crate::context::InvocationContext;
use crate::ports::Preparation;
use crate::ports::storage::WriteTransaction;
use crate::protocol::DomainErrorCode;
use crate::services::approvals::{OwnerPreview, SavedOperationOwners};
use crate::services::sessions::{Failure, Refusal};
use crate::services::{git::GitService, jobs::JobService, packages::PackageService};
use std::sync::Arc;

/// Composition registers actual business-owner instances. Missing owners never qualify an
/// operation; generic metadata or an arbitrary registered command is insufficient authority.
#[derive(Default)]
pub struct NativeOwnerPreviews {
    git: Option<Arc<GitService>>,
    packages: Option<Arc<PackageService>>,
    jobs: Option<Arc<JobService>>,
}
impl NativeOwnerPreviews {
    pub fn new(
        git: Option<Arc<GitService>>,
        packages: Option<Arc<PackageService>>,
        jobs: Option<Arc<JobService>>,
    ) -> Self {
        Self {
            git,
            packages,
            jobs,
        }
    }
}
fn unsupported() -> Failure {
    Refusal::new(
        DomainErrorCode::UnsupportedCapability,
        "saved_operation_owner",
        None,
    )
    .into()
}
impl SavedOperationOwners for NativeOwnerPreviews {
    fn supports(&self, operation: &str) -> bool {
        match operation {
            "git.worktree" | "git.commit" | "git.push" => self.git.is_some(),
            "addons.install" | "addons.update" | "addons.enable" | "addons.rollback"
            | "addons.configure" => self.packages.is_some(),
            "jobs.create" | "jobs.edit" | "jobs.enable" | "jobs.run_now" => self.jobs.is_some(),
            _ => false,
        }
    }
    fn prepare(
        &self,
        ctx: &InvocationContext,
        request: &DecodedRequest,
        env: &Preparation<'_>,
    ) -> Result<OwnerPreview, Failure> {
        if !self.supports(request.descriptor.name) {
            return Err(unsupported());
        }
        match request.descriptor.family {
            "git" => self
                .git
                .as_ref()
                .ok_or_else(unsupported)?
                .approval_preview(ctx, request, env),
            "addons" => self
                .packages
                .as_ref()
                .ok_or_else(unsupported)?
                .approval_preview(ctx, request, env),
            "jobs" => self
                .jobs
                .as_ref()
                .ok_or_else(unsupported)?
                .approval_preview(ctx, request, env),
            _ => Err(unsupported()),
        }
    }
    fn validate(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        request: &DecodedRequest,
        preview: &OwnerPreview,
    ) -> Result<(), Failure> {
        if !self.supports(request.descriptor.name) {
            return Err(unsupported());
        }
        match request.descriptor.family {
            "git" => self
                .git
                .as_ref()
                .ok_or_else(unsupported)?
                .validate_approval_preview(tx, ctx, request, preview),
            "addons" => self
                .packages
                .as_ref()
                .ok_or_else(unsupported)?
                .validate_approval_preview(tx, ctx, request, preview),
            "jobs" => self
                .jobs
                .as_ref()
                .ok_or_else(unsupported)?
                .validate_approval_preview(tx, ctx, request, preview),
            _ => Err(unsupported()),
        }
    }
}

/// Exact retained artifact identity shared by the owners' read and transaction checks. The
/// owner still applies its existing visibility/content rules; this never grants artifact access.
pub(crate) fn artifact_binding(
    read: &dyn crate::ports::storage::ArtifactRead,
    reference: &crate::protocol::provider::ArtifactRef,
) -> Result<serde_json::Value, Failure> {
    use crate::ports::storage::{ArtifactState, BlobState};
    use serde_json::json;
    let unavailable = || {
        Failure::from(Refusal::new(
            DomainErrorCode::SourceChanged,
            "approval_artifact",
            Some(reference.version_id.clone()),
        ))
    };
    let artifact = read
        .artifact(&reference.artifact_id)?
        .ok_or_else(unavailable)?;
    let version = read
        .version(&reference.version_id)?
        .ok_or_else(unavailable)?;
    let blob = read.blob(&version.blob_digest)?.ok_or_else(unavailable)?;
    if artifact.state != ArtifactState::Live
        || !version.live()
        || version.artifact_id != artifact.id
        || version.blob_digest != reference.sha256.0
        || version.byte_count != reference.byte_length.get()
        || version.media_type != reference.media_type
        || blob.state != BlobState::Present
        || blob.generation != version.blob_generation
    {
        return Err(unavailable());
    }
    Ok(
        json!({"reference":reference,"artifactRevision":artifact.revision,"scopeRevision":artifact.scope_revision,
        "scopeProject":artifact.scope.project(),"owner":artifact.owner,"generation":version.blob_generation,
        "source":version.source,"provenance":artifact.provenance}),
    )
}

/// Recheck the private host witness against the immutable generic proposal and human audit at
/// the owner's deciding transaction. The original actor remains unchanged. This is exact
/// operation consent, never a publisher-trust, role, pause or general native-execution bypass.
pub(crate) fn approved_binding(
    tx:&mut dyn WriteTransaction,ctx:&InvocationContext,command:&crate::gateway::AdmittedCommand,
)->Result<Option<serde_json::Value>,Failure> {
    use crate::context::PrincipalKind;
    use crate::ports::storage::ApprovalSource;
    let Some(evidence)=ctx.reviewed_operation() else {return Ok(None);};
    let denied=||Refusal::new(DomainErrorCode::PermissionDenied,"reviewed_operation",Some(evidence.approval_id().clone()));
    if evidence.operation()!=command.descriptor().name || evidence.schema_revision()!=command.schema_revision().0
        || evidence.request_digest()!=command.fingerprint().as_str() {return Err(denied().into());}
    let proposal=tx.decisions().approval(evidence.approval_id())?.ok_or_else(denied)?;
    let audit=tx.decisions().human_decision(ApprovalSource::Generic,evidence.approval_id())?.ok_or_else(denied)?;
    let scope_digest=crate::fingerprint::sha256_hex(crate::jcs::canonicalize(evidence.owner_binding())
        .map_err(|e|crate::ports::storage::StorageError::InvalidRecord(e.to_string()))?.as_bytes());
    if proposal.state!="Approved" || proposal.author!=*ctx.principal() || proposal.author_kind!=ctx.principal_kind()
        || proposal.operation_kind!=evidence.operation() || proposal.content_digest!=evidence.request_digest()
        || proposal.payload["binding"]!=*evidence.owner_binding() || proposal.scope_digest!=scope_digest
        || proposal.decided_by.as_ref()!=Some(evidence.approver()) || proposal.restore_epoch!=tx.store().restore_epoch
        || proposal.restore_epoch!=ctx.restore_epoch().get() || audit.actor!=*evidence.approver()
        || audit.decision!="Approved" || audit.content_digest!=proposal.content_digest || audit.scope_digest!=proposal.scope_digest
        || audit.reviewed_proposal_revision.checked_add(1)!=Some(proposal.revision)
        || tx.actors().actor_kind(evidence.approver())?!=Some(PrincipalKind::Human) {
        return Err(denied().into());
    }
    Ok(Some(evidence.owner_binding().clone()))
}
