//! Owner-derived saved-operation review. A caller supplies the selected request; only its
//! existing business owner can attest current targets, effective contents and validation.
use crate::admission::DecodedRequest;
use crate::context::InvocationContext;
use crate::ports::Preparation;
use crate::ports::storage::WriteTransaction;
use crate::protocol::DomainErrorCode;
use crate::services::approvals::{OwnerPreview, SavedOperationOwners};
use crate::services::desktop::DesktopService;
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
    desktop: Option<Arc<DesktopService>>,
    instructions: Option<Arc<crate::services::instructions::InstructionService>>,
    evidence: Option<(
        Arc<dyn crate::ports::storage::ApplicationStore>,
        Arc<crate::services::artifacts::ArtifactService>,
    )>,
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
            desktop: None,
            instructions: None,
            evidence: None,
        }
    }
    pub fn with_evidence(
        mut self,
        store: Arc<dyn crate::ports::storage::ApplicationStore>,
        artifacts: Arc<crate::services::artifacts::ArtifactService>,
    ) -> Self {
        self.evidence = Some((store, artifacts));
        self
    }
    pub fn with_control(self, control: Arc<crate::services::runs::ControlService>) -> Self {
        if let Some(git) = &self.git {
            git.set_approval_control(control.clone());
        }
        if let Some(packages) = &self.packages {
            packages.set_approval_control(control);
        }
        self
    }
    pub fn with_desktop(mut self, desktop: Option<Arc<DesktopService>>) -> Self {
        self.desktop = desktop;
        self
    }
    pub fn with_instructions(
        mut self,
        instructions: Option<Arc<crate::services::instructions::InstructionService>>,
    ) -> Self {
        self.instructions = instructions;
        self
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
            "desktop.request_control" => self.desktop.is_some(),
            "skills.rollback" | "roles.rollback" => self.instructions.is_some(),
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
        let preview = match request.descriptor.family {
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
            "desktop" => self
                .desktop
                .as_ref()
                .ok_or_else(unsupported)?
                .approval_preview(ctx, request, env),
            "skills" | "roles" => self
                .instructions
                .as_ref()
                .ok_or_else(unsupported)?
                .approval_preview(ctx, request, env),
            _ => Err(unsupported()),
        }?;
        if !crate::services::approvals::review_evidence::needed(&preview) {
            return Ok(preview);
        }
        let (store, artifacts) = self.evidence.as_ref().ok_or_else(unsupported)?;
        let document = crate::services::approvals::review_evidence::document(&preview);
        let reference = publish_review_evidence(
            artifacts,
            &**store,
            ctx,
            env,
            request,
            (
                "SavedOperationReviewEvidence",
                "Complete saved-operation owner review",
            ),
            &document,
        )?;
        env.read(|s| {
            Ok(crate::services::approvals::review_evidence::seal(
                s.artifacts(),
                &preview,
                &reference,
            ))
        })?
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
        let expanded =
            crate::services::approvals::review_evidence::expand(tx.artifacts(), preview)?;
        let preview = &expanded;
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
            "desktop" => self
                .desktop
                .as_ref()
                .ok_or_else(unsupported)?
                .validate_approval_preview(tx, ctx, request, preview),
            "skills" | "roles" => self
                .instructions
                .as_ref()
                .ok_or_else(unsupported)?
                .validate_approval(tx, ctx, request, preview),
            _ => Err(unsupported()),
        }
    }
    fn decide_source(
        &self,
        tx: &mut dyn WriteTransaction,
        human: &InvocationContext,
        request: &DecodedRequest,
        preview: &OwnerPreview,
        approve: bool,
    ) -> Result<(), Failure> {
        if matches!(request.descriptor.family, "skills" | "roles") {
            return self
                .instructions
                .as_ref()
                .ok_or_else(unsupported)?
                .decide_approval(tx, human, request, preview, approve);
        }
        if request.descriptor.family != "addons" {
            return Ok(());
        }
        let expanded = if approve {
            crate::services::approvals::review_evidence::expand(tx.artifacts(), preview)?
        } else {
            crate::services::approvals::review_evidence::expand_historical(preview)?
        };
        let preview = &expanded;
        if request.descriptor.family == "addons" {
            return self
                .packages
                .as_ref()
                .ok_or_else(unsupported)?
                .decide_approval_source(tx, human, request, preview, approve);
        }
        Ok(())
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
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    command: &crate::gateway::AdmittedCommand,
) -> Result<Option<serde_json::Value>, Failure> {
    if let Some(evidence) = ctx.reviewed_operation() {
        let denied = || {
            Refusal::new(
                DomainErrorCode::PermissionDenied,
                "reviewed_operation",
                Some(evidence.approval_id().clone()),
            )
        };
        let outbox = tx
            .decisions()
            .approval_resume(evidence.approval_id())?
            .ok_or_else(denied)?;
        if !matches!(outbox.state.as_str(), "Pending" | "Unknown")
            || outbox.principal != *ctx.principal()
            || outbox.principal_kind != ctx.principal_kind()
            || outbox.operation != command.descriptor().name
            || outbox.schema_revision != i64::from(command.schema_revision().0)
            || outbox.request_digest != command.fingerprint().as_str()
            || outbox.request_key != command.identity().request_key().0
            || !outbox.grant_id.as_ref().is_some_and(|id| {
                command
                    .authorization()
                    .authority_grants()
                    .iter()
                    .any(|grant| &grant.grant_id == id)
            })
        {
            return Err(denied().into());
        }
    }
    let binding = approved_binding_for(
        tx,
        ctx,
        command.descriptor().name,
        command.schema_revision().0,
        command.fingerprint().as_str(),
    )?;
    if binding
        .as_ref()
        .is_some_and(|value| value.get("reviewEnvelope").is_some())
    {
        let evidence = ctx.reviewed_operation().expect("verified reviewed context");
        let proposal = tx
            .decisions()
            .approval(evidence.approval_id())?
            .ok_or_else(unsupported)?;
        let preview = OwnerPreview {
            scope: proposal.scope,
            target_id: proposal.target_id,
            target_revision: proposal.target_revision,
            summary: proposal.summary,
            reviewed: proposal.payload["reviewed"].clone(),
            binding: binding.expect("verified binding"),
        };
        return Ok(Some(
            crate::services::approvals::review_evidence::expand(tx.artifacts(), &preview)?.binding,
        ));
    }
    Ok(binding)
}

pub(crate) fn approved_binding_for(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    operation: &str,
    schema_revision: u32,
    request_digest: &str,
) -> Result<Option<serde_json::Value>, Failure> {
    use crate::context::PrincipalKind;
    use crate::ports::storage::ApprovalSource;
    let Some(evidence) = ctx.reviewed_operation() else {
        return Ok(None);
    };
    let denied = || {
        Refusal::new(
            DomainErrorCode::PermissionDenied,
            "reviewed_operation",
            Some(evidence.approval_id().clone()),
        )
    };
    if evidence.operation() != operation
        || evidence.schema_revision() != schema_revision
        || evidence.request_digest() != request_digest
    {
        return Err(denied().into());
    }
    let proposal = tx
        .decisions()
        .approval(evidence.approval_id())?
        .ok_or_else(denied)?;
    let audit = tx
        .decisions()
        .human_decision(ApprovalSource::Generic, evidence.approval_id())?
        .ok_or_else(denied)?;
    let scope_digest = crate::services::approvals::scope_digest(
        evidence.owner_binding(),
        proposal.payload.get("authority"),
    );
    if proposal.state != "Approved"
        || proposal.author != *ctx.principal()
        || proposal.author_kind != ctx.principal_kind()
        || proposal.operation_kind != evidence.operation()
        || proposal.content_digest != evidence.request_digest()
        || proposal.payload["binding"] != *evidence.owner_binding()
        || crate::services::approvals::authority_binding(tx, ctx)? != proposal.payload["authority"]
        || proposal.scope_digest != scope_digest
        || proposal.decided_by.as_ref() != Some(evidence.approver())
        || proposal.restore_epoch != tx.store().restore_epoch
        || proposal.restore_epoch != ctx.restore_epoch().get()
        || audit.actor != *evidence.approver()
        || audit.decision != "Approved"
        || audit.content_digest != proposal.content_digest
        || audit.scope_digest != proposal.scope_digest
        || audit.reviewed_proposal_revision.checked_add(1) != Some(proposal.revision)
        || tx.actors().actor_kind(evidence.approver())? != Some(PrincipalKind::Human)
    {
        return Err(denied().into());
    }
    Ok(Some(evidence.owner_binding().clone()))
}

/// Full immutable source material is paged through the ordinary artifact owner; previews retain
/// compact effect bindings rather than truncating source data to a protocol JSON bound.
pub(crate) fn publish_review_evidence(
    artifacts: &crate::services::artifacts::ArtifactService,
    store: &dyn crate::ports::storage::ApplicationStore,
    ctx: &InvocationContext,
    env: &Preparation<'_>,
    request: &DecodedRequest,
    presentation: (&str, &str),
    value: &serde_json::Value,
) -> Result<crate::protocol::provider::ArtifactRef, Failure> {
    use crate::services::artifacts::evidence::{Evidence, EvidenceFailure};
    use serde_json::json;
    let (kind, title) = presentation;
    let selected = (
        request.descriptor.name,
        request.revision.0,
        request.normalized.clone(),
        request.fingerprint.as_str().to_owned(),
    );
    let bytes = crate::jcs::canonicalize(value)
        .unwrap_or_else(|_| value.to_string())
        .into_bytes();
    let digest = crate::fingerprint::sha256_hex(&bytes);
    let tags = vec![
        "owner-review-evidence".to_owned(),
        kind.to_owned(),
        digest.clone(),
    ];
    let existing = env.read(|snapshot| {
        let mut after = None;
        loop {
            let artifacts = match ctx.project_id() {
                Some(project) => snapshot.artifacts().list(
                    project,
                    Some(ctx.principal()),
                    &tags,
                    after.as_ref(),
                    500,
                )?,
                None => snapshot.artifacts().list_workspace(
                    Some(ctx.principal()),
                    &tags,
                    after.as_ref(),
                    500,
                )?,
            };
            for artifact in &artifacts {
                if let Some(version) = artifact
                    .current_version_id
                    .as_ref()
                    .map(|id| snapshot.artifacts().version(id))
                    .transpose()?
                    .flatten()
                    && version.live()
                    && version.blob_digest == digest
                {
                    let reference = crate::services::artifacts::version_ref(&version);
                    if artifact_binding(snapshot.artifacts(), &reference).is_ok() {
                        return Ok(Some(reference));
                    }
                }
            }
            if artifacts.len() < 500 {
                return Ok(None);
            }
            let last = artifacts.last().expect("full artifact page");
            after = Some(crate::ports::storage::ListPosition {
                created_at_ms: last.created_at_ms,
                id: last.id.clone(),
            });
        }
    })?;
    if let Some(reference) = existing {
        return Ok(reference);
    }
    let evidence = Evidence {
        scope: ctx.project_id().cloned().map_or(
            crate::ports::storage::ArtifactScope::Workspace,
            crate::ports::storage::ArtifactScope::Project,
        ),
        producer: ctx.principal().clone(),
        principal_kind: format!("{:?}", ctx.principal_kind()),
        agent_session: ctx.session_id().cloned(),
        root: ctx.root_id().cloned(),
        assignment: ctx.assignment_id().cloned(),
        title: title.into(),
        media_type: "application/json".into(),
        tags,
        source: json!({"kind":kind,"sha256":digest}),
    };
    let context = ctx.clone();
    let cancellation = env.cancellation.cloned();
    let deadline = env.deadline_ms;
    artifacts
        .publish_evidence(store, evidence, &bytes, move |tx, _| {
            if cancellation.as_ref().is_some_and(|c| c.is_cancelled())
                || deadline.is_some_and(|d| tx.now_ms() >= d)
            {
                return Err(crate::ports::storage::StorageError::Cancelled);
            }
            let request = crate::services::approvals::requests::decode(
                selected.0,
                selected.1,
                &selected.2,
                true,
            )
            .map_err(|_| {
                crate::ports::storage::StorageError::InvalidRecord(
                    "owner review request no longer decodes".into(),
                )
            })?;
            if request.fingerprint.as_str() != selected.3
                || !crate::services::approvals::validate_authority(tx, &context, &request)?
            {
                return Err(crate::ports::storage::StorageError::ScopeDenied);
            }
            crate::services::artifacts::authorized_scope(tx.authority(), &context)
                .map_err(|_| crate::ports::storage::StorageError::ScopeDenied)?;
            Ok(())
        })
        .map(|(reference, ())| reference)
        .map_err(|e| match e {
            EvidenceFailure::Storage(e) => Failure::Storage(e),
            EvidenceFailure::Refused(r) => {
                Refusal::new(r.code, "commit_review_evidence", None).into()
            }
        })
}

/// Actual readmission, not recording a human decision, is fenced by current independent work
/// holds inside the owner's serialized unit of work. Busy rolls back without a receipt or
/// grant spend, so the same saved command can be retried after an authorized Continue.
pub(crate) fn recheck_control(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    control: Option<&crate::services::runs::ControlService>,
) -> Result<(), Failure> {
    let Some(id) = ctx.root_id() else {
        return Ok(());
    };
    let root = tx
        .sessions()
        .work_root(id)?
        .ok_or_else(|| Refusal::new(DomainErrorCode::NotFound, "root", Some(id.0.clone())))?;
    let target = ctx
        .assignment_id()
        .map_or(crate::services::runs::Target::Root, |id| {
            crate::services::runs::Target::Assignment(id.clone())
        });
    let control = control.ok_or_else(|| {
        Refusal::new(
            DomainErrorCode::UnsupportedCapability,
            "reviewed_control",
            Some(id.0.clone()),
        )
    })?;
    if !control
        .evaluate(
            tx.runs(),
            &root,
            &target,
            crate::ports::storage::ActivationPurpose::Implementation,
        )?
        .eligible
    {
        return Err(crate::ports::storage::StorageError::Busy.into());
    }
    Ok(())
}
