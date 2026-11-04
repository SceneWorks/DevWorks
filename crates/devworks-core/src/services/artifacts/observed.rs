//! Host-observed content published as ordinary artifacts (the fetched pages of `web.fetch`).
//!
//! The bytes follow the artifact store's own publication protocol: the scope's file limit and
//! storage limit are checked first, a staging operation is journaled and the bytes are flushed to
//! their content-addressed generation, then **one** write records the blob, the artifact, its
//! immutable version, origin links, tags, history and the caller's own records (`then`), and
//! completes the staging operation. A refusal or failure of that write resolves the staging
//! operation at once (or at the next recovery), so observed content is either fully recorded or
//! invisible and collectible. The scope is resolved from current authority inside that write,
//! so a principal whose binding changed meanwhile publishes nothing.
use super::{
    ArtifactService, Failure, Refusal, StagedFile, check_quota, complete_operation, event,
    named_scope, new_version, not_found, origin_links, put_staged_blob, quota_refusal, setting_i64,
    usage, version_ref,
};
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::ports::files::SourceOrigin;
use crate::ports::storage::{
    ApplicationStore, ArtifactRetention, ArtifactScope, ArtifactState, BlobState, StorageError,
    StoredArtifact, SubjectKey, WriteOptions, WriteTransaction, run_write,
};
use crate::ports::{IdKind, Preparation};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::{DomainErrorCode, Id, ProjectId};
use crate::settings::catalog::{ARTIFACT_MAX_FILE_KEY, ARTIFACT_QUOTA_KEY};
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

/// What the recording write established, or why it did not.
enum Recorded<T> {
    Done(ArtifactRef, T),
    Refused(Refusal),
}

impl ArtifactService {
    /// Publishes `bytes` as a new explicitly retained artifact of the scope the invocation names
    /// and runs `then` in the same write that records it, with the new version's reference, so
    /// the caller's records commit with the content or not at all.
    pub fn publish_observed<T, F>(
        &self,
        store: &dyn ApplicationStore,
        ctx: &InvocationContext,
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
        let env = Preparation::new(store, None, None);
        let digest = sha256_hex(bytes);
        let n = bytes.len() as i64;
        let (project, d) = (observed.project.clone(), digest.clone());
        let (max, quota, used, reuse) = env.read(move |s| {
            let scope = project.map_or(ArtifactScope::Workspace, ArtifactScope::Project);
            let max = setting_i64(s.configuration(), None, ARTIFACT_MAX_FILE_KEY);
            let quota = setting_i64(s.configuration(), scope.project(), ARTIFACT_QUOTA_KEY);
            let reuse = s
                .artifacts()
                .blob(&d)?
                .filter(|b| matches!(b.state, BlobState::Present | BlobState::UnlinkPending))
                .map(|b| b.generation);
            Ok((max, quota, usage(s.artifacts(), &scope)?, reuse))
        })?;
        let (max, quota) = (max?, quota?);
        if n > max {
            return Err(ObservedFailure::Refused(Refusal::new(
                DomainErrorCode::InvalidArgument,
                "artifact_file_limit",
                None,
            )));
        }
        let scope = observed
            .project
            .clone()
            .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
        if quota > 0 && reuse.is_none() && used.saturating_add(n) > quota {
            return Err(ObservedFailure::Refused(quota_refusal(&scope)));
        }
        let payload = json!({
            "digest": digest, "byteCount": n,
            "projectId": scope.project().map(|p| p.0.as_str()),
            "source": observed.source,
        });
        let mut reader = bytes;
        let (staging, published) =
            self.journal_and_publish(&env, &mut reader, &digest, n, reuse.as_deref(), payload)?;
        let published = match published {
            Ok(p) => p,
            Err(fault) => {
                self.abandon_or_defer(&env, &staging, json!({"failed": format!("{fault:?}")}));
                return Err(ObservedFailure::Storage(super::file_failure(fault)));
            }
        };
        let staged = StagedFile {
            operation_id: staging.operation_id.clone(),
            digest,
            byte_count: n,
            generation: published.generation,
            reused: published.reused,
            relative_path: published.relative_path,
            origin: SourceOrigin::default(),
            source: observed.source.clone(),
            fence: staging.fence.clone(),
        };
        let files = self.files.clone();
        let (owned_ctx, owned) = (ctx.clone(), staged.clone());
        let outcome = run_write(
            store,
            WriteOptions::default(),
            move |tx| match record_observed(tx, &*files, &owned_ctx, &observed, &owned) {
                Ok(reference) => {
                    let value = then(tx, &reference)?;
                    Ok(Recorded::Done(reference, value))
                }
                Err(Failure::Refused(r)) => Ok(Recorded::Refused(r)),
                Err(Failure::Storage(e)) => Err(e),
            },
        );
        match outcome {
            Ok(Recorded::Done(reference, value)) => {
                // A concurrent save kept its own generation: ours is an orphan of a now resolved
                // operation that nothing can bind any more.
                if !staged.reused {
                    let own = staged.operation_id.0.as_str().to_owned();
                    let d = staged.digest.clone();
                    let bound = env.read(move |s| {
                        Ok(s.artifacts().blob(&d)?.is_some_and(|b| b.generation == own))
                    });
                    if matches!(bound, Ok(false)) {
                        let _ = self
                            .files
                            .unlink(&staged.digest, staged.operation_id.0.as_str());
                    }
                }
                Ok((reference, value))
            }
            Ok(Recorded::Refused(r)) => {
                self.abandon_or_defer(&env, &staging, json!({"refused": r.target_kind}));
                Err(ObservedFailure::Refused(r))
            }
            Err(e) => {
                // An unknown commit outcome is resolved the same way: the resolution re-reads the
                // durable record under the fence and completes the operation if it committed.
                self.abandon_or_defer(&env, &staging, json!({"abandoned": format!("{e:?}")}));
                Err(ObservedFailure::Storage(e))
            }
        }
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
            ArtifactScope::Project(_) => None,
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
            "principalKind": format!("{:?}", ctx.principal_kind()),
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
    check_quota(tx, &artifact.scope)?;
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
