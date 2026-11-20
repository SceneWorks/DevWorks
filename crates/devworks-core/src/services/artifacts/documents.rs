//! Document-owned versions reuse already published bytes and the artifact scope authority.
use super::*;
use crate::ports::storage::ArtifactRead;
use crate::protocol::data::DataScope;

/// Reserved, immutable provenance set by the document import writer.
pub(crate) fn is_document(artifact: &StoredArtifact) -> bool {
    artifact.provenance["managedDocument"].as_bool() == Some(true)
        && artifact.retention == ArtifactRetention::Explicit
}

/// Check the requested corpus scope against current invocation and project authority.
pub(crate) fn document_scope(
    authority: &dyn AuthorityRead,
    ctx: &InvocationContext,
    requested: &DataScope,
) -> Result<ArtifactScope, Failure> {
    let authorized = authorized_scope(authority, ctx)?;
    match requested {
        DataScope::Project { project_id } => {
            require_project(ctx, project_id)?;
            if matches!(&authorized, AuthorizedScope::Project(allowed) if allowed != project_id) {
                return Err(not_found("project", &project_id.0).into());
            }
            Ok(ArtifactScope::Project(project_id.clone()))
        }
        DataScope::Workspace
            if authorized == AuthorizedScope::Workspace
                && ctx.project_id().is_none()
                && ctx.root_id().is_none() =>
        {
            Ok(ArtifactScope::Workspace)
        }
        DataScope::Workspace => {
            Err(Refusal::new(DomainErrorCode::PermissionDenied, WORKSPACE_SCOPE, None).into())
        }
    }
}

pub(crate) fn active_project(
    projects: &dyn crate::ports::storage::ProjectRead,
    id: &ProjectId,
) -> Result<(), Failure> {
    if projects
        .project(id)?
        .is_none_or(|project| project.lifecycle != "Active")
    {
        return Err(not_found("project", &id.0).into());
    }
    Ok(())
}

/// Resolve a managed document in the invocation's current artifact audience.
pub(crate) fn document(
    read: &dyn ArtifactRead,
    authority: &dyn AuthorityRead,
    ctx: &InvocationContext,
    id: &Id,
) -> Result<StoredArtifact, Failure> {
    let viewer = Viewer::resolve(authority, ctx)?;
    document_for(read, &viewer, id)
}

pub(crate) fn document_for(
    read: &dyn ArtifactRead,
    viewer: &Viewer<'_>,
    id: &Id,
) -> Result<StoredArtifact, Failure> {
    let project = match &viewer.authorized {
        Some(AuthorizedScope::Project(project)) => Some(project),
        Some(AuthorizedScope::Workspace) => viewer.ctx.project_id(),
        None => return Err(StorageError::ScopeDenied.into()),
    };
    if let Some(artifact) = read.artifact(id)?
        && artifact.state == ArtifactState::Live
        && is_document(&artifact)
        && visible_in(read, viewer, &artifact, project)?
    {
        return Ok(artifact);
    }
    Err(not_found("document", id).into())
}

pub(crate) fn document_list(
    snapshot: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    scope: &ArtifactScope,
    current: Option<bool>,
    after: Option<&ListPosition>,
    limit: u16,
) -> Result<Vec<StoredArtifact>, StorageError> {
    let viewer = Viewer::resolve(snapshot.authority(), ctx)?;
    let actor = (!viewer.sees_owned(None)).then_some(ctx.principal());
    snapshot
        .artifacts()
        .document_artifacts(scope, actor, current, after, limit)
}

/// Append a live managed version using the source's already journaled blob generation.
///
/// The source is rechecked inside the import receipt's writer. A new live version owns its
/// own digest reference, so later deletion of the original source artifact cannot remove it.
pub(crate) fn import_document(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    scope: &ArtifactScope,
    reference: &ArtifactRef,
    title: &str,
    replacement: Option<(&Id, &Revision)>,
) -> Result<StoredArtifact, Failure> {
    let viewer = Viewer::resolve(tx.authority(), ctx)?;
    let source = visible_artifact(tx.artifacts(), &viewer, &reference.artifact_id)?;
    require_explicit(&source, &tx.store().host_actor)?;
    if !visible_in(tx.artifacts(), &viewer, &source, scope.project())? {
        return Err(not_found("artifact", &reference.artifact_id).into());
    }
    let original = live_version(tx.artifacts(), &source, &reference.version_id)?;
    if original.blob_digest != String::from(reference.sha256.clone())
        || original.byte_count != reference.byte_length.get()
    {
        return Err(not_found("artifact_version", &reference.version_id).into());
    }
    let blob = tx
        .artifacts()
        .blob(&original.blob_digest)?
        .filter(|blob| {
            blob.state == BlobState::Present && blob.generation == original.blob_generation
        })
        .ok_or_else(|| not_found("artifact_version", &reference.version_id))?;

    let (mut artifact, version_no) = if let Some((id, expected)) = replacement {
        let artifact = visible_artifact(tx.artifacts(), &viewer, id)?;
        if !is_document(&artifact) || &artifact.scope != scope {
            return Err(not_found("document", id).into());
        }
        if expected.get() != artifact.revision {
            return Err(conflict(
                RevisionDimension::Artifact,
                "document",
                id,
                expected,
                artifact.revision,
            ));
        }
        let version_no = tx
            .artifacts()
            .version_history(id, None, false, 1)?
            .first()
            .map_or(1, |last| last.version_no + 1);
        (artifact, version_no)
    } else {
        let artifact = StoredArtifact {
            id: tx.new_id(IdKind::Artifact),
            origin_project_id: scope.project().cloned(),
            origin_work_id: ctx.root_id().cloned(),
            title: title.to_owned(),
            revision: 1,
            scope: scope.clone(),
            owner: matches!(scope, ArtifactScope::Workspace).then(|| ctx.principal().clone()),
            retention: ArtifactRetention::Explicit,
            scope_revision: 1,
            reference_revision: 1,
            current_version_id: None,
            state: ArtifactState::Live,
            created_at_ms: tx.now_ms(),
            provenance: json!({
                "managedDocument": true, "producedBy": ctx.principal().0.as_str(),
                "principalKind": ctx.principal_kind().as_str(), "invocationId": ctx.invocation_id(),
                "sessionId": ctx.session_id(),
            }),
        };
        tx.artifacts().insert_artifact(&artifact)?;
        (artifact, 1)
    };
    let version = StoredVersion {
        id: tx.new_id(IdKind::ArtifactVersion),
        artifact_id: artifact.id.clone(),
        version_no,
        blob_digest: blob.digest,
        byte_count: blob.byte_count,
        blob_generation: blob.generation,
        media_type: original.media_type,
        created_at_ms: tx.now_ms(),
        deleted_at_ms: None,
        source: json!({
            "kind": "ManagedDocumentImport", "importedFrom": reference,
            "originalName": original.source.get("originalName"),
            "originalSource": original.source.get("source"),
            "producedBy": ctx.principal().0.as_str(), "sessionId": ctx.session_id(),
        }),
    };
    tx.artifacts().insert_version(&version)?;
    for link in origin_links(ctx, &version.id) {
        tx.artifacts().add_link(&link)?;
    }
    if let Some(root) = &artifact.origin_work_id
        && Some(root) != ctx.root_id()
    {
        tx.artifacts().add_link(&ArtifactLink {
            version_id: version.id.clone(),
            subject: SubjectKey::root(root),
            purpose: LinkPurpose::Origin,
        })?;
    }
    artifact.title = title.to_owned();
    artifact.current_version_id = Some(version.id.clone());
    artifact.revision += i64::from(replacement.is_some());
    tx.artifacts().update_artifact(&artifact)?;
    check_quota(tx, scope)?;
    event(
        tx,
        ctx,
        &artifact,
        artifact.origin_work_id.clone(),
        None,
        "ArtifactVersionPublished",
        json!({"versionId":version.id,"versionNo":version_no,"digest":version.blob_digest,
            "byteCount":version.byte_count,"mediaType":version.media_type,"source":version.source}),
    )?;
    Ok(artifact)
}

/// Change the artifact's actual owning project, preserving explicit shares and its origin.
pub(crate) fn move_document(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    mut artifact: StoredArtifact,
    target: &ProjectId,
    expected: &Revision,
) -> Result<(StoredArtifact, bool), Failure> {
    if authorized_scope(tx.authority(), ctx)? != AuthorizedScope::Workspace {
        return Err(Refusal::new(DomainErrorCode::PermissionDenied, "project", None).into());
    }
    if expected.get() != artifact.scope_revision {
        return Err(conflict(
            RevisionDimension::Scope,
            "artifact_scope",
            &artifact.id,
            expected,
            artifact.scope_revision,
        ));
    }
    active_project(tx.projects(), target)?;
    if artifact.scope.project() == Some(target) {
        return Ok((artifact, false));
    }
    let previous = artifact.scope.project().cloned();
    artifact.scope = ArtifactScope::Project(target.clone());
    artifact.owner = None;
    artifact.scope_revision += 1;
    artifact.reference_revision += 1;
    tx.artifacts().remove_share(&artifact.id, target)?;
    tx.artifacts().update_artifact(&artifact)?;
    check_quota(tx, &artifact.scope)?;
    event(
        tx,
        ctx,
        &artifact,
        None,
        None,
        "ArtifactMoved",
        json!({"previousProjectId":previous,"projectId":target,"scopeRevision":rev(artifact.scope_revision)}),
    )?;
    Ok((artifact, true))
}
