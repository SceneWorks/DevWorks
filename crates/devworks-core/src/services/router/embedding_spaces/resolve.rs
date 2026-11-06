use super::{QualifiedEmbeddingBinding, ResolvedEmbeddingConfiguration, embedding_identity};
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::ports::storage::*;
use crate::protocol::data::Metric;
use crate::protocol::provider::ArtifactRef;
use crate::protocol::{OperationState, StoreId};
use crate::providers::capabilities::EmbeddingDimensions;
use serde_json::{Value, json};

struct Selection {
    selected: StoredEmbeddingSelection,
    candidate: Option<StoredEmbeddingCandidate>,
    qualification: Option<StoredEmbeddingQualification>,
    connection: Option<StoredConnection>,
}
fn selection(read: &dyn RouterRead) -> Result<Selection, StorageError> {
    let selected = read.embedding_selection()?;
    let candidate = selected
        .candidate_id
        .as_ref()
        .map(|id| read.embedding_candidate(id))
        .transpose()?
        .flatten();
    let qualification = selected
        .candidate_id
        .as_ref()
        .map(|id| read.embedding_qualification(id))
        .transpose()?
        .flatten();
    let connection = candidate
        .as_ref()
        .map(|c| read.connection(&c.connection_id))
        .transpose()?
        .flatten();
    Ok(Selection {
        selected,
        candidate,
        qualification,
        connection,
    })
}

pub(super) fn candidate_space(
    c: &StoredEmbeddingCandidate,
) -> Result<crate::protocol::data::EmbeddingSpace, StorageError> {
    let dimensions = u32::try_from(c.dimensions)
        .map_err(|_| StorageError::InvalidRecord("embedding dimensions".into()))?;
    let metric = match c.metric.as_str() {
        "Cosine" => Metric::Cosine,
        "L2" => Metric::L2,
        _ => return Err(StorageError::InvalidRecord("embedding metric".into())),
    };
    if dimensions == 0
        || dimensions > 65536
        || !["Native", "Parameter"].contains(&c.dimension_mode.as_str())
        || c.element_type != "Float32"
    {
        return Err(StorageError::InvalidRecord(
            "embedding encoder identity".into(),
        ));
    }
    Ok(crate::protocol::data::EmbeddingSpace {
        space_id: c.space_id.clone(),
        model_digest: crate::protocol::provider::Digest(c.model_digest.clone()),
        dimensions,
        metric,
    })
}

pub(super) fn current_candidate(
    c: &StoredEmbeddingCandidate,
    connection: Option<&StoredConnection>,
) -> Result<Option<&'static str>, StorageError> {
    let Some(connection) = connection else {
        return Ok(Some("The configured embedding connection was removed."));
    };
    if connection.profile != "Embedding"
        || connection.account_revision != c.account_revision
        || connection.account_digest != c.account_digest
        || connection.endpoint != c.endpoint
    {
        return Ok(Some(
            "The embedding connection, account, or endpoint identity changed. Create and qualify a new space.",
        ));
    }
    let space = candidate_space(c)?;
    let mode = if c.dimension_mode == "Parameter" {
        EmbeddingDimensions::Parameter
    } else {
        EmbeddingDimensions::Fixed(space.dimensions)
    };
    let actual = embedding_identity(
        connection,
        &c.upstream_model_id,
        space.dimensions,
        &mode,
        space.metric,
    )?;
    if actual.model_digest != c.model_digest
        || actual.configuration_digest != c.configuration_digest
        || actual.encoder_config != c.encoder_config
        || actual.space.space_id != c.space_id
    {
        return Err(StorageError::unavailable(
            Unavailable::Corrupt,
            "embedding candidate semantic identity mismatch",
        ));
    }
    if !connection.enabled || connection.lifecycle != "Connected" {
        return Ok(Some(
            "The selected embedding connection is disabled or disconnected.",
        ));
    }
    if !connection
        .discovered
        .as_ref()
        .is_some_and(|models| models.iter().any(|model| model.id == c.upstream_model_id))
    {
        return Ok(Some(
            "The selected embedding model is not in the current complete inventory.",
        ));
    }
    Ok(None)
}

fn artifact_current(
    read: &dyn ArtifactRead,
    reference: &ArtifactRef,
) -> Result<bool, StorageError> {
    let Some(artifact) = read.artifact(&reference.artifact_id)? else {
        return Ok(false);
    };
    let Some(version) = read.version(&reference.version_id)? else {
        return Ok(false);
    };
    let blob = read.blob(&reference.sha256.0)?;
    Ok(artifact.state == ArtifactState::Live
        && version.live()
        && version.artifact_id == reference.artifact_id
        && version.blob_digest == reference.sha256.0
        && version.byte_count == reference.byte_length.get()
        && version.media_type == reference.media_type
        && blob.is_some_and(|b| {
            b.state == BlobState::Present && b.generation == version.blob_generation
        }))
}
fn evidence_current(
    read: &dyn ArtifactRead,
    q: Option<&StoredEmbeddingQualification>,
) -> Result<bool, StorageError> {
    let Some(evidence) = q.and_then(|q| q.evidence.as_ref()) else {
        return Ok(false);
    };
    Ok(artifact_current(read, &evidence.result_artifact)?
        && artifact_current(read, &evidence.vector_artifact)?)
}
fn principal_current(
    ctx: &InvocationContext,
    kind: Option<PrincipalKind>,
    principal: Option<&PrincipalBinding>,
) -> bool {
    if kind != Some(ctx.principal_kind()) {
        return false;
    }
    match ctx.principal_kind() {
        PrincipalKind::Human | PrincipalKind::Host => true,
        _ => principal.is_some_and(|p| {
            p.active
                && p.kind == ctx.principal_kind()
                && p.project_id
                    .as_ref()
                    .is_none_or(|project| ctx.project_id() == Some(project))
        }),
    }
}
fn authority_witness(
    ctx: &InvocationContext,
    principal: Option<&PrincipalBinding>,
    policy: &str,
) -> Value {
    let mut source_context = crate::services::approvals::save_source_context(ctx);
    // An invocation identifies this ingress, not a change to the caller's authority.
    // Keep the same stable context used by the paid preparation/custody owner.
    if let Some(invocation) = source_context["invocationContext"].as_object_mut() {
        invocation.remove("invocation");
    }
    json!({"sourceContext":source_context,"policy":policy,"principal":principal.map(|p|json!({"actor":p.actor,"kind":format!("{:?}",p.kind),"active":p.active,"revision":p.revision.to_string(),"projectId":p.project_id,"role":p.role.map(|r|r.as_str()),"tools":p.tool_families}))})
}
fn resolved(
    selection: Selection,
    authority: Value,
    authorized: bool,
    operation: Option<OperationRecord>,
    evidence_current: bool,
) -> Result<ResolvedEmbeddingConfiguration, StorageError> {
    let source = json!({"version":1,"selectionRevision":selection.selected.revision.to_string(),"candidateId":selection.selected.candidate_id,"spaceId":selection.selected.space_id,"candidate":selection.candidate.as_ref().map(|c|json!({"id":c.id,"configurationDigest":c.configuration_digest})),"qualification":selection.qualification.as_ref().map(|q|json!({"revision":q.revision.to_string(),"state":q.state,"probe":q.probe,"evidence":q.evidence})),"connectionRevision":selection.connection.as_ref().map(|c|c.revision.to_string()),"operation":operation.as_ref().map(|op|json!({"id":op.id,"revision":op.revision.to_string(),"state":op.state})),"evidenceCurrent":evidence_current,"authorityCurrent":authorized,"authority":authority});
    let source_digest = sha256_hex(
        crate::jcs::canonicalize(&source)
            .unwrap_or_else(|_| source.to_string())
            .as_bytes(),
    );
    let unavailable = |why: &str| ResolvedEmbeddingConfiguration {
        source_digest: source_digest.clone(),
        reason: Some(why.into()),
        binding: None,
    };
    if !authorized {
        return Ok(unavailable(
            "Current embedding metadata authority is unavailable.",
        ));
    }
    let Some(candidate) = selection.candidate else {
        return Ok(unavailable("No embedding space is explicitly selected."));
    };
    if selection.selected.space_id.as_ref() != Some(&candidate.space_id) {
        return Err(StorageError::unavailable(
            Unavailable::Corrupt,
            "embedding selection identity mismatch",
        ));
    }
    if let Some(reason) = current_candidate(&candidate, selection.connection.as_ref())? {
        return Ok(unavailable(reason));
    }
    let Some(q) = selection.qualification else {
        return Ok(unavailable(
            "The embedding space has no qualification record.",
        ));
    };
    if q.state != EmbeddingQualificationState::Qualified
        || !evidence_current
        || !operation.is_some_and(|o| o.state == OperationState::ObservedComplete)
    {
        return Ok(unavailable(
            "The selected embedding space has no current completed positive qualification.",
        ));
    }
    let space = candidate_space(&candidate)?;
    let connection = selection.connection.ok_or(StorageError::ScopeDenied)?;
    let binding = QualifiedEmbeddingBinding {
        candidate,
        qualification_revision: q.revision,
        connection,
        space,
        source_digest: source_digest.clone(),
    };
    Ok(ResolvedEmbeddingConfiguration {
        source_digest,
        reason: None,
        binding: Some(binding),
    })
}

pub(super) fn qualification_current_read(
    read: &dyn ReadSnapshot,
    q: Option<&StoredEmbeddingQualification>,
) -> Result<bool, StorageError> {
    let Some(probe) = q.and_then(|q| q.probe.as_ref()) else {
        return Ok(false);
    };
    let current = probe.store_id==read.stamp().store_id && probe.restore_epoch==read.stamp().restore_epoch
        && read.actors().actor_kind(&probe.actor_id)?==Some(PrincipalKind::Human)
        // This only rechecks original facts; it is never returned as a permission.
        && crate::authority::service::authorization_holds(read,&crate::ports::Authorization::direct().fenced_by(probe.authority_fences.clone()))?;
    Ok(current && evidence_current(read.artifacts(), q)?)
}

/// Metadata and a freshness witness; this does not authorize an inference or spend.
pub fn resolve_selected_read(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
) -> Result<ResolvedEmbeddingConfiguration, StorageError> {
    let choice = selection(read.router())?;
    let principal = read.authority().principal(ctx.principal())?;
    let valid = read.stamp().store_id == *ctx.store_id()
        && read.stamp().restore_epoch == ctx.restore_epoch().get()
        && principal_current(
            ctx,
            read.actors().actor_kind(ctx.principal())?,
            principal.as_ref(),
        );
    let policy =
        crate::authority::policy::resolve_policy(read.configuration(), ctx.project_id())?.digest();
    let operation = choice
        .qualification
        .as_ref()
        .and_then(|q| q.probe.as_ref())
        .map(|p| read.operations().operation(&p.operation_id))
        .transpose()?
        .flatten();
    let current = qualification_current_read(read, choice.qualification.as_ref())?;
    resolved(
        choice,
        authority_witness(ctx, principal.as_ref(), &policy),
        valid,
        operation,
        current,
    )
}

/// The same metadata rules against the writer's current snapshot; no nested reads or I/O.
pub fn resolve_selected_in(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
) -> Result<ResolvedEmbeddingConfiguration, StorageError> {
    let choice = selection(&*tx.router())?;
    let principal = tx.authority().principal(ctx.principal())?;
    let store_id: StoreId = tx.store().store_id.clone();
    let restore_epoch = tx.store().restore_epoch;
    let valid = store_id == *ctx.store_id()
        && restore_epoch == ctx.restore_epoch().get()
        && principal_current(
            ctx,
            tx.actors().actor_kind(ctx.principal())?,
            principal.as_ref(),
        );
    let policy =
        crate::authority::policy::resolve_policy(&*tx.configuration(), ctx.project_id())?.digest();
    let operation = choice
        .qualification
        .as_ref()
        .and_then(|q| q.probe.as_ref())
        .map(|p| tx.operations().operation(&p.operation_id))
        .transpose()?
        .flatten();
    let source_current = match choice.qualification.as_ref().and_then(|q| q.probe.as_ref()) {
        Some(probe) => {
            probe.store_id == tx.store().store_id
                && probe.restore_epoch == tx.store().restore_epoch
                && tx.actors().actor_kind(&probe.actor_id)? == Some(PrincipalKind::Human)
                && crate::authority::service::fences_hold(tx, &probe.authority_fences)?
        }
        None => false,
    };
    let current =
        source_current && evidence_current(&*tx.artifacts(), choice.qualification.as_ref())?;
    resolved(
        choice,
        authority_witness(ctx, principal.as_ref(), &policy),
        valid,
        operation,
        current,
    )
}
