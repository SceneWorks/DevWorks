//! Host-observed content published as ordinary artifacts (the fetched pages of `web.fetch`).
//!
//! The bytes follow the artifact store's own publication protocol (see `publish.rs`): the scope's
//! file limit and storage limit are checked first, a staging operation is journaled and the bytes are flushed to
//! their content-addressed generation, then **one** write records the blob, the artifact, its
//! immutable version, origin links, tags, history and the caller's own records (`then`), and
//! completes the staging operation. A refusal or failure of that write resolves the staging
//! operation at once (or at the next recovery), so observed content is either fully recorded or
//! invisible and collectible.
//!
//! That write is the publication's linearization point. It first re-checks the invocation's
//! authorization (store and restore epoch, every authority fence — the principal's binding
//! revision and activity, the access policy digest — and every grant), so a principal revoked
//! or re-bound, or a policy narrowed, while the content was being produced publishes nothing
//! (`PermissionDenied`, [`AUTHORITY_CHANGED`]). The scope is resolved from current authority
//! there too. Everything it records sits behind one savepoint, rolled back before a typed
//! refusal (the file or storage limit) is returned, so a refusal commits nothing.
use super::publish::Gate;
use super::{
    ArtifactService, Failure, Refusal, StagedFile, complete_operation, event, named_scope,
    new_version, not_found, origin_links, put_staged_blob, setting_i64, version_ref,
};
use crate::context::InvocationContext;
use crate::ports::storage::{
    ApplicationStore, ArtifactRetention, ArtifactScope, ArtifactState, StorageError,
    StoredArtifact, SubjectKey, WriteTransaction,
};
use crate::ports::{Authorization, IdKind};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::{DomainErrorCode, Id, ProjectId};
use crate::settings::catalog::ARTIFACT_MAX_FILE_KEY;
use serde_json::{Value, json};

/// One observed publication.
#[derive(Clone, Debug)]
pub struct Observed {
    /// The owner scope the caller named: a project, or the workspace (`None`).
    pub project: Option<ProjectId>,
    pub title: String,
    pub media_type: String,
    pub tags: Vec<String>,
    /// Source description recorded with the version and its history (`{"kind": "WebFetch", ...}`).
    pub source: Value,
    /// Restrict a project artifact to the producing actor and privileged viewers.
    pub actor_audience: bool,
}

/// Why observed content was not published.
#[derive(Debug)]
pub enum ObservedFailure {
    /// A limit or scope refusal (too large, storage limit reached, project or scope refused).
    Refused(Refusal),
    Storage(StorageError),
}
impl From<StorageError> for ObservedFailure {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}
impl From<Failure> for ObservedFailure {
    fn from(f: Failure) -> Self {
        match f {
            Failure::Refused(r) => Self::Refused(r),
            Failure::Storage(e) => Self::Storage(e),
        }
    }
}

/// Fault target kind of a publication refused because the invocation's authority changed while
/// its content was being produced (nothing was recorded).
pub const AUTHORITY_CHANGED: &str = "authority_changed";

impl ArtifactService {
    /// Publishes `bytes` as a new explicitly retained artifact of the scope the invocation names
    /// and runs `then` in the same write that records it, with the new version's reference, so
    /// the caller's records commit with the content or not at all.
    pub fn publish_observed<T, F>(
        &self,
        store: &dyn ApplicationStore,
        ctx: &InvocationContext,
        authorization: &Authorization,
        observed: Observed,
        bytes: &[u8],
        then: F,
    ) -> Result<(ArtifactRef, T), ObservedFailure>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn WriteTransaction, &ArtifactRef) -> Result<T, StorageError>
            + Send
            + 'static,
    {
        let scope = observed
            .project
            .clone()
            .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
        let source = observed.source.clone();
        let (owned_ctx, gate_ctx) = (ctx.clone(), ctx.clone());
        let gate = Gate::Authorization(gate_ctx, authorization.clone());
        Ok(self.publish_bytes(
            store,
            scope,
            &source,
            gate,
            bytes,
            move |tx, files, staged| record_observed(tx, files, &owned_ctx, &observed, staged),
            then,
        )?)
    }

    /// Publish actor-scoped output while preserving ordinary explicit artifact retention.
    pub fn publish_actor_observed<T, F>(
        &self,
        store: &dyn ApplicationStore,
        ctx: &InvocationContext,
        authorization: &Authorization,
        mut observed: Observed,
        bytes: &[u8],
        then: F,
    ) -> Result<(ArtifactRef, T), ObservedFailure>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn WriteTransaction, &ArtifactRef) -> Result<T, StorageError>
            + Send
            + 'static,
    {
        observed.actor_audience = true;
        self.publish_observed(store, ctx, authorization, observed, bytes, then)
    }
}

/// Records the staged bytes as a new artifact inside the publishing write. The scope is
/// resolved from current authority here.
fn record_observed(
    tx: &mut dyn WriteTransaction,
    files: &dyn crate::ports::files::ArtifactFiles,
    ctx: &InvocationContext,
    observed: &Observed,
    staged: &StagedFile,
) -> Result<ArtifactRef, Failure> {
    let scope = named_scope(tx.authority(), ctx, observed.project.as_ref())?;
    if let ArtifactScope::Project(p) = &scope
        && !tx.artifacts().project_exists(p)?
    {
        return Err(not_found("project", &p.0).into());
    }
    // Origin work outside the owning project is not linked (a refusal, as a save refuses it).
    if let (Some(root), ArtifactScope::Project(p)) = (ctx.root_id(), &scope)
        && !tx.artifacts().subject_exists(p, &SubjectKey::root(root))?
    {
        return Err(Refusal::new(
            DomainErrorCode::InvalidArgument,
            "origin_work",
            Some(root.0.clone()),
        )
        .into());
    }
    // The limits are decided again in the committing write: a setting saved since the
    // pre-check applies (the storage limit is checked below, after the bytes are counted).
    if staged.byte_count > setting_i64(tx.configuration(), None, ARTIFACT_MAX_FILE_KEY)? {
        return Err(Refusal::new(
            DomainErrorCode::InvalidArgument,
            "artifact_file_limit",
            None,
        )
        .into());
    }
    let generation = put_staged_blob(tx, files, staged)?;
    let now = tx.now_ms();
    let artifact_id = tx.new_id(IdKind::Artifact);
    let mut artifact = StoredArtifact {
        id: artifact_id.clone(),
        origin_project_id: scope.project().cloned(),
        origin_work_id: ctx.root_id().cloned(),
        title: observed.title.clone(),
        revision: 1,
        owner: match &scope {
            ArtifactScope::Workspace => Some(ctx.principal().clone()),
            ArtifactScope::Project(_) => observed.actor_audience.then(|| ctx.principal().clone()),
        },
        scope,
        retention: ArtifactRetention::Explicit,
        scope_revision: 1,
        reference_revision: 1,
        current_version_id: None,
        state: ArtifactState::Live,
        created_at_ms: now,
        provenance: json!({
            "producedBy": ctx.principal().0.as_str(),
            "principalKind": ctx.principal_kind().as_str().to_string(),
            "sessionId": ctx.session_id().map(Id::as_str),
            "assignmentId": ctx.assignment_id().map(|a| a.0.as_str()),
            "invocationId": ctx.invocation_id().as_str(),
            "sourceKind": observed.source.get("kind"),
            "trust": observed.source.get("trust"),
        }),
    };
    tx.artifacts().insert_artifact(&artifact)?;
    let mut tags = observed.tags.clone();
    tags.sort();
    tags.dedup();
    tx.artifacts().set_tags(&artifact_id, &tags)?;
    let version = new_version(
        tx,
        ctx,
        &artifact_id,
        1,
        &observed.media_type,
        staged,
        generation,
    );
    tx.artifacts().insert_version(&version)?;
    artifact.current_version_id = Some(version.id.clone());
    tx.artifacts().update_artifact(&artifact)?;
    for link in origin_links(ctx, &version.id) {
        tx.artifacts().add_link(&link)?;
    }
    complete_operation(
        tx,
        &staged.operation_id,
        json!({"artifactId": artifact_id.as_str(), "versionId": version.id.as_str()}),
    )?;
    event(
        tx,
        ctx,
        &artifact,
        artifact.origin_work_id.clone(),
        Some(staged.operation_id.clone()),
        "ArtifactVersionPublished",
        json!({
            "versionId": version.id.as_str(), "versionNo": 1, "digest": staged.digest,
            "byteCount": staged.byte_count, "mediaType": observed.media_type,
            "title": observed.title, "tags": tags, "source": observed.source,
        }),
    )?;
    Ok(version_ref(&version))
}
