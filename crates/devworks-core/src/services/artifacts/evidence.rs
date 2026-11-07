//! Host-produced evidence artifacts and exact-version reads for host services (DW-047).
//!
//! **Evidence** is bytes a host service observed on behalf of a principal — a browser
//! accessibility snapshot, a screenshot, a batch of console or network notifications, what an
//! input did — published as an ordinary, explicitly retained artifact of the producing scope
//! (UIQA-03). It follows the artifact store's own publication protocol (a journaled staging
//! operation, a flushed content-addressed generation, then one write recording the blob, the
//! artifact, its immutable version, origin links and history); the caller's own records (for
//! example an observation index row) commit in that same write, so evidence is either fully
//! recorded or invisible and collectible. The scope's maximum file size and storage limit apply:
//! a refusal is returned, never a partial artifact.
//!
//! **Exact reads** open one immutable version a principal may see, checking the reference's
//! digest and length against the version, holding a reader on the blob while it is read (the
//! collector never removes a blob a reader holds). They back the browser's explicit file grants:
//! only an exact artifact the invocation may see is ever materialized for a page.
use super::lifecycle::ReaderGuard;
use super::{
    ArtifactService, Failure, Refusal, StagedFile, Viewer, complete_operation, put_staged_blob,
    setting_i64, usage, version_ref, visible,
};
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::ports::files::SourceOrigin;
use crate::ports::storage::{
    ApplicationStore, ArtifactLink, ArtifactRetention, ArtifactScope, ArtifactState, BlobState,
    LinkPurpose, NewEvent, ReadOptions, StorageError, StoredArtifact, StoredVersion, SubjectKey,
    WriteOptions, WriteTransaction, read_snapshot, run_write,
};
use crate::ports::{IdKind, Preparation};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::{ActorId, AssignmentId, DomainErrorCode, EventId, Id, RootId};
use crate::settings::catalog::{ARTIFACT_MAX_FILE_KEY, ARTIFACT_QUOTA_KEY};
use serde_json::{Value, json};
use std::io::Read;

/// One evidence publication.
#[derive(Clone, Debug)]
pub struct Evidence {
    pub scope: ArtifactScope,
    /// The principal the evidence was produced for (the owner of a workspace artifact).
    pub producer: ActorId,
    /// Its `PrincipalKind` name.
    pub principal_kind: String,
    pub agent_session: Option<Id>,
    pub root: Option<RootId>,
    pub assignment: Option<AssignmentId>,
    pub title: String,
    pub media_type: String,
    pub tags: Vec<String>,
    /// Source description recorded with the version (`{"kind": "BrowserScreenshot", ...}`).
    pub source: Value,
}

/// Why evidence was not published.
#[derive(Debug)]
pub enum EvidenceFailure {
    /// A limit or scope refusal (too large, storage limit reached, missing project).
    Refused(Refusal),
    Storage(StorageError),
}
impl From<StorageError> for EvidenceFailure {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}
impl From<Failure> for EvidenceFailure {
    fn from(f: Failure) -> Self {
        match f {
            Failure::Refused(r) => Self::Refused(r),
            Failure::Storage(e) => Self::Storage(e),
        }
    }
}

/// An open exact version and the reader hold on its blob.
pub struct ExactRead {
    pub reference: ArtifactRef,
    /// A safe single-component file name (the recorded original name, else the title).
    pub name: String,
    pub reader: Box<dyn Read + Send>,
    _hold: ReaderGuard,
}

impl ArtifactService {
    /// Publishes `bytes` as a new evidence artifact and runs `then` in the same write that
    /// records it (with the new version's reference), so the caller's index rows commit with
    /// the evidence or not at all.
    pub fn publish_evidence<T, F>(
        &self,
        store: &dyn ApplicationStore,
        evidence: Evidence,
        bytes: &[u8],
        then: F,
    ) -> Result<(ArtifactRef, T), EvidenceFailure>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn WriteTransaction, &ArtifactRef) -> Result<T, StorageError>
            + Send
            + 'static,
    {
        let env = Preparation::new(store, None, None);
        let digest = sha256_hex(bytes);
        let n = bytes.len() as i64;
        let scope = evidence.scope.clone();
        let d = digest.clone();
        let (max, quota, used, reuse) = env.read(move |s| {
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
            return Err(EvidenceFailure::Refused(Refusal::new(
                DomainErrorCode::InvalidArgument,
                "artifact_file_limit",
                None,
            )));
        }
        if quota > 0 && reuse.is_none() && used.saturating_add(n) > quota {
            return Err(EvidenceFailure::Refused(super::quota_refusal(
                &evidence.scope,
            )));
        }
        let payload = json!({"digest": digest, "byteCount": n,
            "projectId": evidence.scope.project().map(|p| p.0.as_str()), "source": evidence.source});
        let (staging, published) = self.journal_and_publish(
            &env,
            &mut std::io::Cursor::new(bytes),
            &digest,
            n,
            reuse.as_deref(),
            payload,
        )?;
        let published = match published {
            Ok(p) => p,
            Err(fault) => {
                self.abandon_or_defer(&env, &staging, json!({"failed": format!("{fault:?}")}));
                return Err(EvidenceFailure::Storage(super::file_failure(fault)));
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
            source: evidence.source.clone(),
            fence: staging.fence.clone(),
        };
        let files = self.files.clone();
        let s = staged.clone();
        let committed = run_write(store, WriteOptions::default(), move |tx| {
            // The storage limit is checked again with the new version in place, inside the
            // publishing transaction (as a save does): concurrent publications that each passed
            // the check above cannot together exceed it. Over the limit, nothing is kept.
            let savepoint = tx.savepoint()?;
            let r = commit(tx, &*files, &evidence, &s)?;
            match super::check_quota(tx, &evidence.scope) {
                Ok(()) => {}
                Err(Failure::Refused(refusal)) => {
                    tx.rollback_to(savepoint)?;
                    return Ok(Err(refusal));
                }
                Err(Failure::Storage(e)) => return Err(e),
            }
            tx.release(savepoint)?;
            let t = then(tx, &r)?;
            Ok(Ok((r, t)))
        });
        let committed = match committed {
            Ok(Ok(r)) => r,
            Ok(Err(refusal)) => {
                self.abandon_or_defer(
                    &env,
                    &staging,
                    json!({"abandoned": "the storage limit was reached"}),
                );
                return Err(EvidenceFailure::Refused(refusal));
            }
            Err(e) => {
                // Re-read under the staging fence: completed if the version did commit.
                self.abandon_or_defer(
                    &env,
                    &staging,
                    json!({"abandoned": "the evidence metadata was not committed"}),
                );
                return Err(e.into());
            }
        };
        if !staged.reused {
            self.remove_unbound_generation(&env, &staged);
        }
        Ok(committed)
    }

    /// After a commit that kept another live generation of the same bytes, this operation's own
    /// generation is an orphan nothing can bind: remove it.
    fn remove_unbound_generation(&self, env: &Preparation<'_>, staged: &StagedFile) {
        let (digest, own) = (staged.digest.clone(), staged.operation_id.0.clone());
        let bound = env.read(|s| {
            Ok(s.artifacts()
                .blob(&digest)?
                .is_some_and(|b| b.generation == own.as_str()))
        });
        if matches!(bound, Ok(false)) {
            let _ = self.files.unlink(&staged.digest, own.as_str());
        }
    }

    /// Opens the exact immutable version `reference` names, when `ctx` may see it now: the
    /// artifact is live and visible, the version is live and belongs to it, and the reference's
    /// digest, length and media type match. Anything else is `NotFound` (existence is not
    /// disclosed). The stream is bounded to the version's length.
    pub fn open_exact_version(
        &self,
        store: &dyn ApplicationStore,
        ctx: &InvocationContext,
        reference: &ArtifactRef,
    ) -> Result<ExactRead, Refusal> {
        let not_found = || {
            Refusal::new(
                DomainErrorCode::NotFound,
                "artifact_version",
                Some(reference.version_id.clone()),
            )
        };
        let hold = self.readers.acquire(&reference.sha256.0);
        let (aid, vid) = (reference.artifact_id.clone(), reference.version_id.clone());
        let found = read_snapshot(store, &ReadOptions::default(), |s| {
            let read = s.artifacts();
            let viewer = Viewer::resolve(s.authority(), ctx)?;
            let Some(a) = read.artifact(&aid)? else {
                return Ok(None);
            };
            if a.state != ArtifactState::Live || !visible(read, &viewer, &a)? {
                return Ok(None);
            }
            Ok(read
                .version(&vid)?
                .filter(|v| v.live() && v.artifact_id == a.id)
                .map(|v| (a, v)))
        })
        .map_err(|_| {
            Refusal::new(
                DomainErrorCode::StorageUnavailable,
                "artifact_version",
                None,
            )
            .retry(crate::protocol::RetryAdvice::SameCommand)
        })?;
        let Some((artifact, version)) = found else {
            return Err(not_found());
        };
        if version.blob_digest != reference.sha256.0
            || version.byte_count != reference.byte_length.get()
            || version.media_type != reference.media_type
        {
            return Err(not_found());
        }
        let reader = self
            .files
            .open(
                &version.blob_digest,
                &version.blob_generation,
                version.byte_count as u64,
            )
            .map_err(|_| not_found())?;
        Ok(ExactRead {
            reference: version_ref(&version),
            name: safe_name(&super::lifecycle::suggested_name(&artifact, &version)),
            reader,
            _hold: hold,
        })
    }
}

/// A single safe path component: no separators, no leading dot, no control characters, at most
/// 120 characters; empty becomes `file`.
pub fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .take(120)
        .collect();
    let trimmed = cleaned
        .trim()
        .trim_start_matches('.')
        .trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        "file".into()
    } else {
        trimmed.to_owned()
    }
}

/// The publishing write: blob, artifact, version, origin links, quota and history.
fn commit(
    tx: &mut dyn WriteTransaction,
    files: &dyn crate::ports::files::ArtifactFiles,
    e: &Evidence,
    staged: &StagedFile,
) -> Result<ArtifactRef, StorageError> {
    if let ArtifactScope::Project(p) = &e.scope
        && !tx.artifacts().project_exists(p)?
    {
        return Err(StorageError::NotFound {
            kind: "project",
            id: p.0.as_str().to_owned(),
        });
    }
    let generation = put_staged_blob(tx, files, staged)?;
    let now = tx.now_ms();
    let artifact_id = tx.new_id(IdKind::Artifact);
    let host = tx.store().host_actor.clone();
    let mut artifact = StoredArtifact {
        id: artifact_id.clone(),
        origin_project_id: e.scope.project().cloned(),
        origin_work_id: e.root.clone(),
        title: e.title.chars().take(256).collect(),
        revision: 1,
        owner: match &e.scope {
            ArtifactScope::Workspace => Some(e.producer.clone()),
            ArtifactScope::Project(_) => None,
        },
        scope: e.scope.clone(),
        retention: ArtifactRetention::Explicit,
        scope_revision: 1,
        reference_revision: 1,
        current_version_id: None,
        state: ArtifactState::Live,
        created_at_ms: now,
        provenance: json!({
            "producedBy": e.producer.0.as_str(),
            "principalKind": e.principal_kind,
            "sessionId": e.agent_session.as_ref().map(Id::as_str),
            "assignmentId": e.assignment.as_ref().map(|a| a.0.as_str()),
            "recordedBy": host.0.as_str(),
            "sourceKind": e.source.get("kind"),
            "source": e.source,
        }),
    };
    tx.artifacts().insert_artifact(&artifact)?;
    tx.artifacts().set_tags(&artifact_id, &e.tags)?;
    let version = StoredVersion {
        id: tx.new_id(IdKind::ArtifactVersion),
        artifact_id: artifact_id.clone(),
        version_no: 1,
        blob_digest: staged.digest.clone(),
        byte_count: staged.byte_count,
        blob_generation: generation,
        media_type: e.media_type.clone(),
        created_at_ms: now,
        deleted_at_ms: None,
        source: json!({
            "source": e.source,
            "operationId": staged.operation_id.0.as_str(),
            "producedBy": e.producer.0.as_str(),
            "principalKind": e.principal_kind,
            "sessionId": e.agent_session.as_ref().map(Id::as_str),
        }),
    };
    tx.artifacts().insert_version(&version)?;
    artifact.current_version_id = Some(version.id.clone());
    tx.artifacts().update_artifact(&artifact)?;
    if let Some(root) = &e.root {
        tx.artifacts().add_link(&ArtifactLink {
            version_id: version.id.clone(),
            subject: SubjectKey::root(root),
            purpose: LinkPurpose::Origin,
        })?;
    }
    if let Some(a) = &e.assignment {
        tx.artifacts().add_link(&ArtifactLink {
            version_id: version.id.clone(),
            subject: SubjectKey {
                kind: "assignment",
                id: a.0.clone(),
            },
            purpose: LinkPurpose::Origin,
        })?;
    }
    complete_operation(
        tx,
        &staged.operation_id,
        json!({"artifactId": artifact_id.as_str(), "versionId": version.id.as_str()}),
    )?;
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id,
        actor: e.producer.clone(),
        project_id: e.scope.project().cloned(),
        root_id: e.root.clone(),
        operation_id: Some(staged.operation_id.clone()),
        aggregate_kind: super::ARTIFACT_AGGREGATE.into(),
        aggregate_id: artifact_id.as_str().to_owned(),
        aggregate_revision: 1,
        kind: "ArtifactVersionPublished".into(),
        payload: json!({
            "versionId": version.id.as_str(), "versionNo": 1, "digest": staged.digest,
            "byteCount": staged.byte_count, "mediaType": e.media_type, "title": artifact.title,
            "tags": e.tags, "source": e.source,
        }),
    })?;
    Ok(version_ref(&version))
}
