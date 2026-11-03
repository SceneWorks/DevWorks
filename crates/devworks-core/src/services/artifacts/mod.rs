//! `ArtifactService`: the per-project local artifact store behind `artifacts.save`, `.list`,
//! `.read`, `.preview`, `.export`, `.reference` and `.delete` (ART-01..05), registered through
//! [`crate::services::UseCases::with_artifacts`]. The UI, agent tools, packages and MCP reach the
//! same use cases through the gateway; there is no second implementation of any rule.
//!
//! # Publication (AD 24)
//!
//! A save is published in this order, so a crash at any point is reconcilable:
//!
//! 1. [`CommandHandler::prepare`] (after admission and current authority, outside any write
//!    transaction) resolves the host source handle ([`ArtifactSources`]), reads it with the
//!    verified no-follow bounded reader and checks the caller's expected digest and length;
//! 2. it journals a staging operation (`artifacts.stage_file`, committed as `Dispatching`) in its
//!    own small write;
//! 3. the file port stages the bytes in `staging/`, flushes and verifies them, renames them to
//!    their content-addressed location in `artifacts/` and flushes the directory;
//! 4. [`CommandHandler::decide`] — inside the gateway's unit of work, atomically with the receipt
//!    — records the blob, the artifact and its immutable version, origin links, provenance and
//!    history, and completes the staging operation.
//!
//! A crash before step 4 leaves a journaled operation and at most a staged or unreferenced file;
//! [`ArtifactService::recover`] fails the operation and collects the files. Committed metadata
//! never names a file that was not durably published; a file without metadata is never visible.
//! When the process survives but step 4 does not commit (a refusal such as the in-transaction
//! quota check, a replay, a denial or a rollback), [`CommandHandler::settle`] resolves the
//! staging operation at once under its attempt fence ([`ArtifactService::abandon`]), so it never
//! waits for the next boot and is never resolved twice.
//!
//! # Versions, references and deletion (AD 59)
//!
//! Versions are immutable and never overwritten: a save always creates a new artifact, and the
//! document replacement path appends versions through [`ArtifactService::publish_version`]
//! (`documents.import` with `replaceDocumentId`). Exact old references keep resolving to their
//! own version ([`ArtifactService::resolve_version`]) until that version is explicitly deleted,
//! after which they report it unavailable with tombstone provenance — never another version.
//!
//! Deletion authorizes the full affected reference set (the owning project and every project it
//! is shared with); a principal confined to one project gets `SharedReferencesRemain` (a
//! `Conflict` refusal that changes nothing). It tombstones metadata first, so new reads fail as
//! soon as it commits, and marks a blob `UnlinkPending` only when no surviving live version of
//! any artifact needs it. [`ArtifactService::collect`] removes a pending blob's file later, and
//! only while no reader holds it and no backup pins it. Previews are a separate, disposable
//! derived class ([`ArtifactFiles::collect_derived`]).
//!
//! # Access (ART-04)
//!
//! Current authority (role, selected tools, reviewer read-only, project confinement) is the
//! gateway's [`crate::ports::AuthorityPort`]. On top of it, every use case checks project
//! scope: an invocation bound to a project sees only artifacts that project owns or is shared;
//! anything else is `NotFound`, so existence is not disclosed. Agents receive references and
//! bounded text windows or previews, never whole files in model context; binary content is an
//! expiring, principal-bound stream handle re-authorized at every use
//! ([`ArtifactService::open_stream`]). Writing an export to a filesystem destination is a
//! separately authorized host action ([`ArtifactService::write_export`]).
//!
//! # Retained CodeGraph behavior
//!
//! `WorkImportsController.GetAttachment` (`GET api/work/attachments/{id}`) maps to
//! `artifacts.read` of the imported attachment's artifact version: the content comes back with
//! its media type (text inline, otherwise a stream handle whose suggested name is the original
//! filename); an unknown attachment and a stored file that cannot be read are both `NotFound`
//! (the source's quirk: I/O failures were reported as not found), and an unavailable store is
//! `StorageUnavailable`.
pub mod lifecycle;
pub mod preview;

use crate::authority::reads::{ReadRefusal, ReadRequest, VerifiedReader};
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, artifacts as ops};
use crate::ports::files::{ArtifactFiles, ArtifactSources, FileFault, SourceOrigin, SourceRefusal};
use crate::ports::storage::{
    ArtifactLink, ArtifactState, AttemptBinding, AttemptFence, BlobState, ConfigScope,
    ConfigurationRead, LinkPurpose, ListPosition, NewEvent, NewOperation, ReadOptions,
    ReadSnapshot, StorageError, StoredArtifact, StoredVersion, SubjectKey, Unavailable,
    WriteOptions, WriteTransaction, read_snapshot, run_write,
};
use crate::ports::{
    Clock, CommandHandler, Decided, HandlerFailure, IdKind, IdSource, Preparation, Prepared,
    QueryAnswer, QueryHandler,
};
use crate::protocol::data::{
    ArtifactCommand, ArtifactDeletion, ArtifactQuery, ArtifactSource, Coverage, CoverageState,
    DataScope,
};
use crate::protocol::provider::{ArtifactRef, Digest};
use crate::protocol::records::{
    ArtifactExport, ArtifactPreview, ArtifactReadContent, ArtifactRecord, MutationSummary,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Page};
use crate::protocol::{
    CorrelationId, Counter, DomainErrorCode, DomainFault, EventId, Id, OperationId, OperationState,
    ProjectId, RetryAdvice, Revision, RevisionDimension, RootId,
};
use crate::settings::catalog::{ARTIFACT_MAX_FILE_KEY, ARTIFACT_QUOTA_KEY, descriptor};
use crate::settings::model::{chain, current_chain};
use lifecycle::{Readers, StreamPurpose, Streams};
use serde_json::{Value, json};
use std::sync::Arc;

/// The operations [`ArtifactService`] is registered for.
pub const OPERATIONS: [&str; 7] = [
    "artifacts.save",
    "artifacts.list",
    "artifacts.read",
    "artifacts.preview",
    "artifacts.export",
    "artifacts.reference",
    "artifacts.delete",
];
/// History aggregate of an artifact (`aggregate_id` = artifact ID). Events carry the owning
/// project and, where there is one, the origin or referencing work root, so work history
/// navigates to its artifacts and an artifact's history navigates back (HISTORY-01/02).
pub const ARTIFACT_AGGREGATE: &str = "artifact";
/// History aggregate of a managed blob's physical lifecycle (`aggregate_id` = digest).
pub const BLOB_AGGREGATE: &str = "artifact_blob";
/// Journal capability of a staged file publication.
pub const STAGE_CAPABILITY: &str = "artifacts.stage_file";
/// Largest inline text window of one `artifacts.read` page.
pub const READ_WINDOW: usize = 65536;
/// Fault target kind of a delete refused because live references outside the caller's
/// authorized scope remain (`SharedReferencesRemain`; nothing changed).
pub const SHARED_REFERENCES_REMAIN: &str = "SharedReferencesRemain";
/// Fault target kind of a save refused by the project's storage limit (nothing was deleted).
pub const QUOTA_TARGET: &str = "artifact_storage_quota";

// ---------------------------------------------------------------------------------------------
// Refusals

/// A typed refusal: a durable `Refused` decision for commands, a fault for queries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub code: DomainErrorCode,
    pub retry: RetryAdvice,
    pub target_kind: &'static str,
    pub target_id: Option<Id>,
}
impl Refusal {
    pub fn new(code: DomainErrorCode, target_kind: &'static str, target_id: Option<Id>) -> Self {
        Self {
            code,
            retry: RetryAdvice::Never,
            target_kind,
            target_id,
        }
    }
    fn retry(mut self, retry: RetryAdvice) -> Self {
        self.retry = retry;
        self
    }
    pub fn fault(self, correlation_id: CorrelationId) -> DomainFault {
        DomainFault {
            code: self.code,
            retry: self.retry,
            correlation_id,
            target_id: self.target_id,
            target_kind: Some(BoundedText(self.target_kind.to_owned())),
            revision_dimension: None,
            expected_revision: None,
            current_revision: None,
            blocker_ids: BoundedList(Vec::new()),
            diagnostic_id: None,
        }
    }
}
fn not_found(kind: &'static str, id: &Id) -> Refusal {
    Refusal::new(DomainErrorCode::NotFound, kind, Some(id.clone()))
}

/// A use-case failure: a typed refusal or a storage error (a revision conflict becomes the
/// gateway's durable `RevisionConflict`; anything else rolls the command back).
#[derive(Debug)]
pub enum Failure {
    Refused(Refusal),
    Storage(StorageError),
}
impl From<StorageError> for Failure {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}
impl From<Refusal> for Failure {
    fn from(r: Refusal) -> Self {
        Self::Refused(r)
    }
}
fn file_failure(f: FileFault) -> StorageError {
    match f {
        FileFault::DiskFull => {
            StorageError::unavailable(Unavailable::DiskFull, "artifact volume is full")
        }
        FileFault::Corrupt(e) => StorageError::unavailable(Unavailable::Corrupt, e),
        FileFault::NotFound => StorageError::unavailable(Unavailable::Io, "managed file missing"),
        FileFault::Exists => StorageError::unavailable(Unavailable::Io, "file exists"),
        FileFault::Io(e) | FileFault::Unsupported(e) => {
            StorageError::unavailable(Unavailable::Io, e)
        }
    }
}

fn rev(n: i64) -> Revision {
    Revision::new(n.max(1)).expect("positive revision")
}
fn counter(n: i64) -> Counter {
    Counter::new(n.max(0)).expect("non-negative counter")
}
fn conflict(
    dimension: RevisionDimension,
    kind: &'static str,
    id: &Id,
    expected: &Revision,
    current: i64,
) -> Failure {
    Failure::Storage(StorageError::conflict(
        dimension,
        kind,
        id.clone(),
        expected.clone(),
        rev(current),
    ))
}

// ---------------------------------------------------------------------------------------------
// Scope

/// Which projects' references the invoking principal may act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthorizedScope {
    Workspace,
    Project(ProjectId),
}
impl AuthorizedScope {
    fn covers(&self, project: &ProjectId) -> bool {
        match self {
            Self::Workspace => true,
            Self::Project(p) => p == project,
        }
    }
}

/// The principal's reference scope from its current host binding: the user and the host act
/// workspace-wide; a principal confined to a project (the authority port already refused any
/// invocation outside it) acts only on that project's references.
pub fn authorized_scope(
    authority: &dyn crate::ports::storage::AuthorityRead,
    ctx: &InvocationContext,
) -> Result<AuthorizedScope, StorageError> {
    if matches!(
        ctx.principal_kind(),
        PrincipalKind::Human | PrincipalKind::Host
    ) {
        return Ok(AuthorizedScope::Workspace);
    }
    let bound = authority.principal(ctx.principal())?;
    Ok(match bound.and_then(|b| b.project_id) {
        Some(p) => AuthorizedScope::Project(p),
        None => AuthorizedScope::Workspace,
    })
}

/// Whether an invocation may see `artifact`: a project-bound invocation sees what its project
/// owns or was shared; a workspace invocation sees every project's artifacts.
fn visible(
    read: &dyn crate::ports::storage::ArtifactRead,
    ctx: &InvocationContext,
    artifact: &StoredArtifact,
) -> Result<bool, StorageError> {
    Ok(match ctx.project_id() {
        None => true,
        Some(p) => &artifact.current_project_id == p || read.shares(&artifact.id)?.contains(p),
    })
}

/// A request naming `project` must match the invocation's project, when it has one.
fn require_project(ctx: &InvocationContext, project: &ProjectId) -> Result<(), Refusal> {
    match ctx.project_id() {
        Some(p) if p != project => Err(Refusal::new(
            DomainErrorCode::PermissionDenied,
            "project",
            Some(project.0.clone()),
        )),
        _ => Ok(()),
    }
}

/// The live artifact `id` if this invocation may see it (otherwise `NotFound`).
fn visible_artifact(
    read: &dyn crate::ports::storage::ArtifactRead,
    ctx: &InvocationContext,
    id: &Id,
) -> Result<StoredArtifact, Failure> {
    match read.artifact(id)? {
        Some(a) if a.state == ArtifactState::Live && visible(read, ctx, &a)? => Ok(a),
        _ => Err(not_found("artifact", id).into()),
    }
}

/// The live version `version_id` of `artifact`.
fn live_version(
    read: &dyn crate::ports::storage::ArtifactRead,
    artifact: &StoredArtifact,
    version_id: &Id,
) -> Result<StoredVersion, Failure> {
    match read.version(version_id)? {
        Some(v) if v.artifact_id == artifact.id && v.live() => Ok(v),
        _ => Err(not_found("artifact_version", version_id).into()),
    }
}

// ---------------------------------------------------------------------------------------------
// Settings

fn setting_i64(
    read: &dyn ConfigurationRead,
    project: Option<&ProjectId>,
    key: &str,
) -> Result<i64, Failure> {
    let scope = project.map_or(ConfigScope::Workspace, |p| ConfigScope::Project(p.clone()));
    let layers = current_chain(read, &scope)?;
    let d = descriptor(key).expect("catalog key");
    let entries = chain(d, &layers)
        .map_err(|_| Refusal::new(DomainErrorCode::SchemaMismatch, "setting_schema", None))?;
    Ok(entries
        .into_iter()
        .next()
        .and_then(|e| e.value.value.0.as_i64())
        .or_else(|| d.default_value().as_i64())
        .unwrap_or(0))
}

/// Visible storage status of one project (ART-04/05): usage against its limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageStatus {
    pub used_bytes: i64,
    /// 0 means no limit.
    pub quota_bytes: i64,
    pub max_file_bytes: i64,
    /// New saves are refused (nothing is deleted) while this holds.
    pub at_limit: bool,
}

// ---------------------------------------------------------------------------------------------
// Records

/// The artifact reference of one version.
pub fn version_ref(v: &StoredVersion) -> ArtifactRef {
    ArtifactRef {
        artifact_id: v.artifact_id.clone(),
        version_id: v.id.clone(),
        sha256: Digest(v.blob_digest.clone()),
        byte_length: counter(v.byte_count),
        media_type: v.media_type.clone(),
    }
}

/// The wire name of an artifact: its title, bounded to a `Label` (the stored title is kept whole).
fn label(title: &str) -> BoundedText<1, 256> {
    let t: String = title.chars().take(256).collect();
    BoundedText(if t.is_empty() { "artifact".into() } else { t })
}

fn record(
    read: &dyn crate::ports::storage::ArtifactRead,
    a: &StoredArtifact,
    v: &StoredVersion,
) -> Result<ArtifactRecord, StorageError> {
    let references = read
        .links(&a.id)?
        .iter()
        .filter(|l| l.purpose == LinkPurpose::Reference)
        .count()
        + read.shares(&a.id)?.len();
    Ok(ArtifactRecord {
        artifact: version_ref(v),
        name: label(&a.title),
        created_at_ms: counter(a.created_at_ms),
        scope: DataScope::Project {
            project_id: a.current_project_id.clone(),
        },
        reference_count: counter(references as i64),
    })
}

/// Digest binding a whole-artifact deletion to the exact live version set: SHA-256 of
/// `"<versionId>:<sha256>\n"` for every live version in publication order.
pub fn versions_digest(versions: &[StoredVersion]) -> String {
    let mut basis = String::new();
    for v in versions.iter().filter(|v| v.live()) {
        basis.push_str(&format!("{}:{}\n", v.id.as_str(), v.blob_digest));
    }
    sha256_hex(basis.as_bytes())
}

/// Appends one artifact history event inside the command's transaction.
fn event(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    artifact: &StoredArtifact,
    root: Option<RootId>,
    operation: Option<OperationId>,
    kind: &str,
    mut payload: Value,
) -> Result<i64, StorageError> {
    if let Some(map) = payload.as_object_mut() {
        map.insert(
            "principalKind".into(),
            json!(format!("{:?}", ctx.principal_kind())),
        );
        map.insert("invocationId".into(), json!(ctx.invocation_id().as_str()));
        if let Some(s) = ctx.session_id() {
            map.insert("sessionId".into(), json!(s.as_str()));
        }
    }
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id,
        actor: ctx.principal().clone(),
        project_id: Some(artifact.current_project_id.clone()),
        root_id: root,
        operation_id: operation,
        aggregate_kind: ARTIFACT_AGGREGATE.into(),
        aggregate_id: artifact.id.as_str().to_owned(),
        aggregate_revision: artifact.revision,
        kind: kind.into(),
        payload,
    })
}

// ---------------------------------------------------------------------------------------------
// Staging (preparation)

/// One source to stage for `project`, with the caller's expected identity.
#[derive(Clone, Copy, Debug)]
pub struct StageRequest<'a> {
    pub project: &'a ProjectId,
    pub source: &'a ArtifactSource,
    pub expected_digest: &'a Digest,
    pub expected_bytes: i64,
    /// Digest of the admitted request the staging belongs to (journal provenance).
    pub request_digest: &'a str,
}

/// The outcome of staging one source file.
#[derive(Clone, Debug)]
pub enum Staging {
    /// Refused before any file was written; `decide` records the refusal.
    Refused(Refusal),
    /// Durably published at its digest location under a journaled staging operation.
    Staged(StagedFile),
}

/// A source file published as an unreferenced blob, awaiting its metadata commit.
#[derive(Clone, Debug)]
pub struct StagedFile {
    pub operation_id: OperationId,
    pub digest: String,
    pub byte_count: i64,
    pub relative_path: String,
    pub origin: SourceOrigin,
    /// The resolved source handle (provenance).
    pub source: Value,
    /// The staging attempt's journal fence: only this attempt's owner resolves the operation.
    pub fence: AttemptFence,
}

fn source_json(source: &ArtifactSource) -> Value {
    serde_json::to_value(source).unwrap_or(Value::Null)
}

fn read_refusal(r: ReadRefusal) -> Result<Refusal, StorageError> {
    let source = "artifact_source";
    Ok(match r {
        ReadRefusal::NotFound => Refusal::new(DomainErrorCode::NotFound, source, None),
        ReadRefusal::TooLarge { .. } => Refusal::new(
            DomainErrorCode::InvalidArgument,
            "artifact_file_limit",
            None,
        ),
        ReadRefusal::Unsupported(_) => {
            Refusal::new(DomainErrorCode::UnsupportedTarget, source, None)
        }
        ReadRefusal::Io(e) => return Err(StorageError::unavailable(Unavailable::Io, e)),
        // Links, escapes, non-regular files, hard links, moved roots and swaps: refused.
        ReadRefusal::Escape
        | ReadRefusal::LinkRefused
        | ReadRefusal::NotRegularFile
        | ReadRefusal::HardLinked
        | ReadRefusal::RootChanged
        | ReadRefusal::Changed => Refusal::new(DomainErrorCode::PermissionDenied, source, None),
    })
}

// ---------------------------------------------------------------------------------------------
// The service

/// The artifact domain service.
pub struct ArtifactService {
    pub(crate) files: Arc<dyn ArtifactFiles>,
    pub(crate) sources: Arc<dyn ArtifactSources>,
    pub(crate) reader: Arc<dyn VerifiedReader>,
    pub(crate) clock: Arc<dyn Clock>,
    /// Correlation IDs for query faults and stream handles (commands allocate in their
    /// transaction).
    pub(crate) ids: Arc<dyn IdSource>,
    pub(crate) streams: Streams,
    pub(crate) readers: Readers,
}

impl ArtifactService {
    pub fn new(
        files: Arc<dyn ArtifactFiles>,
        sources: Arc<dyn ArtifactSources>,
        reader: Arc<dyn VerifiedReader>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
    ) -> Self {
        Self {
            files,
            sources,
            reader,
            clock,
            ids,
            streams: Streams::default(),
            readers: Readers::default(),
        }
    }

    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }

    /// Stages one source for a save into `project`: resolve, verified bounded read, digest and
    /// length check, journaled staging operation, then flushed atomic publication of the file.
    /// Reused by the documents replacement path. Refusals write nothing; an I/O or disk failure
    /// fails the journaled operation and is returned (the caller rejects without a receipt).
    pub fn stage(
        &self,
        env: &Preparation<'_>,
        ctx: &InvocationContext,
        request: &StageRequest<'_>,
    ) -> Result<Staging, StorageError> {
        let StageRequest {
            project,
            source,
            expected_digest,
            expected_bytes,
            request_digest,
        } = *request;
        let cancelled = || env.cancellation.is_some_and(|c| c.is_cancelled());
        if cancelled() {
            return Err(StorageError::Cancelled);
        }
        let digest = String::from(expected_digest.clone());
        let limits = read_snapshot(env.store, &ReadOptions::default(), |s| {
            let max = setting_i64(s.configuration(), None, ARTIFACT_MAX_FILE_KEY);
            let quota = setting_i64(s.configuration(), Some(project), ARTIFACT_QUOTA_KEY);
            let new_bytes = s.artifacts().blob(&digest)?.is_none();
            let usage = s.artifacts().project_usage(project)?;
            Ok((max, quota, new_bytes, usage))
        })?;
        let (max, quota, new_bytes, usage) = limits;
        let (max, quota) = match (max, quota) {
            (Ok(m), Ok(q)) => (m, q),
            (Err(Failure::Refused(r)), _) | (_, Err(Failure::Refused(r))) => {
                return Ok(Staging::Refused(r));
            }
            (Err(Failure::Storage(e)), _) | (_, Err(Failure::Storage(e))) => return Err(e),
        };
        if expected_bytes > max {
            return Ok(Staging::Refused(Refusal::new(
                DomainErrorCode::InvalidArgument,
                "artifact_file_limit",
                None,
            )));
        }
        // Bytes no blob holds yet always add to usage: refuse before reading or staging them.
        if quota > 0 && new_bytes && usage.saturating_add(expected_bytes) > quota {
            return Ok(Staging::Refused(quota_refusal(project)));
        }
        let resolved = match self.sources.resolve(ctx, source) {
            Ok(r) => r,
            Err(SourceRefusal::NotFound) => {
                return Ok(Staging::Refused(Refusal::new(
                    DomainErrorCode::NotFound,
                    "artifact_source",
                    None,
                )));
            }
            Err(SourceRefusal::PermissionDenied) => {
                return Ok(Staging::Refused(Refusal::new(
                    DomainErrorCode::PermissionDenied,
                    "artifact_source",
                    None,
                )));
            }
        };
        let read = self.reader.read(&ReadRequest {
            root: &resolved.root,
            relative: &resolved.relative,
            max_bytes: max as u64,
            allow_hard_links: false,
        });
        let bytes = match read {
            Ok(v) => v.bytes,
            Err(r) => return Ok(Staging::Refused(read_refusal(r)?)),
        };
        if bytes.len() as i64 != expected_bytes || sha256_hex(&bytes) != digest {
            return Ok(Staging::Refused(
                Refusal::new(DomainErrorCode::SourceChanged, "artifact_source", None)
                    .retry(RetryAdvice::RefreshAndNewRequest),
            ));
        }
        if cancelled() {
            return Err(StorageError::Cancelled);
        }
        let payload = json!({
            "digest": digest,
            "byteCount": expected_bytes,
            "requestDigest": request_digest,
            "projectId": project.0.as_str(),
            "source": source_json(source),
        });
        let args = sha256_hex(payload.to_string().as_bytes());
        let operation = run_write(env.store, WriteOptions::default(), move |tx| {
            let id = OperationId(tx.new_id(IdKind::Operation));
            tx.operations().record_intent(NewOperation {
                id: id.clone(),
                root_id: None,
                assignment_id: None,
                capability: STAGE_CAPABILITY.into(),
                schema_version: 1,
                args_digest: args,
                payload,
            })?;
            let attempt = tx
                .operations()
                .begin_attempt(&id, 1, AttemptBinding::default())?;
            tx.operations().transition(
                &id,
                2,
                Some(&attempt.fence()),
                OperationState::Dispatching,
                Some(json!({"step": "staging"})),
            )?;
            Ok((id, attempt.fence()))
        })?;
        let (operation, fence) = operation;
        let staging_name = format!("{}.part", operation.0.as_str());
        let published = match self.files.publish(&staging_name, &bytes, &digest) {
            Ok(p) => p,
            Err(fault) => {
                let diagnostic = format!("{fault:?}");
                // Best effort: the failure stays visible on the journaled operation; recovery
                // resolves it if this write cannot be made either.
                let _ = fail_operation(env.store, &operation, json!({"failed": diagnostic}));
                return Err(file_failure(fault));
            }
        };
        if cancelled() {
            let _ = fail_operation(env.store, &operation, json!({"cancelled": true}));
            return Err(StorageError::Cancelled);
        }
        Ok(Staging::Staged(StagedFile {
            operation_id: operation,
            digest,
            byte_count: expected_bytes,
            relative_path: published.relative_path,
            origin: resolved.origin,
            source: source_json(source),
            fence,
        }))
    }

    /// The project's storage status for the Settings/Artifacts views.
    pub fn storage_status(
        &self,
        snapshot: &dyn ReadSnapshot,
        project: &ProjectId,
    ) -> Result<StorageStatus, Failure> {
        let used = snapshot.artifacts().project_usage(project)?;
        let quota = setting_i64(snapshot.configuration(), Some(project), ARTIFACT_QUOTA_KEY)?;
        let max = setting_i64(snapshot.configuration(), None, ARTIFACT_MAX_FILE_KEY)?;
        Ok(StorageStatus {
            used_bytes: used,
            quota_bytes: quota,
            max_file_bytes: max,
            at_limit: quota > 0 && used >= quota,
        })
    }
}

fn quota_refusal(project: &ProjectId) -> Refusal {
    Refusal::new(
        DomainErrorCode::CapacityWait,
        QUOTA_TARGET,
        Some(project.0.clone()),
    )
}

/// Fails a journaled staging operation (from `Dispatching`) with an attributed observation.
pub(crate) fn fail_operation(
    store: &dyn crate::ports::storage::ApplicationStore,
    operation: &OperationId,
    observation: Value,
) -> Result<(), StorageError> {
    let operation = operation.clone();
    run_write(store, WriteOptions::default(), move |tx| {
        let Some(op) = tx.operations().operation(&operation)? else {
            return Ok(());
        };
        if crate::ports::storage::is_terminal(op.state) {
            return Ok(());
        }
        let fence = op.latest_attempt().map(|a| a.fence());
        tx.operations().transition(
            &operation,
            op.revision,
            fence.as_ref(),
            OperationState::Failed,
            Some(observation),
        )?;
        Ok(())
    })
}

/// Completes a staging operation inside the publishing transaction.
fn complete_operation(
    tx: &mut dyn WriteTransaction,
    operation: &OperationId,
    observation: Value,
) -> Result<(), StorageError> {
    let op = tx
        .operations()
        .operation(operation)?
        .ok_or_else(|| StorageError::NotFound {
            kind: "operation",
            id: operation.0.as_str().to_owned(),
        })?;
    if op.state != OperationState::Dispatching {
        return Err(StorageError::InvalidRecord(
            "the staging operation is no longer in flight".into(),
        ));
    }
    let fence = op.latest_attempt().map(|a| a.fence());
    tx.operations().transition(
        operation,
        op.revision,
        fence.as_ref(),
        OperationState::ObservedComplete,
        Some(observation),
    )?;
    Ok(())
}

/// Records a staged blob's metadata, refusing a blob the collector is removing (retry) or whose
/// file is no longer in place.
fn put_staged_blob(
    tx: &mut dyn WriteTransaction,
    files: &dyn ArtifactFiles,
    staged: &StagedFile,
) -> Result<(), StorageError> {
    if let Some(b) = tx.artifacts().blob(&staged.digest)? {
        let in_place = files
            .present(&staged.digest, staged.byte_count as u64)
            .map_err(file_failure)?;
        if b.state == BlobState::Unlinking || (b.state != BlobState::Present && !in_place) {
            return Err(StorageError::Busy);
        }
    } else if !files
        .present(&staged.digest, staged.byte_count as u64)
        .map_err(file_failure)?
    {
        // Another abandoned staging of the same bytes removed the unreferenced file after this
        // one reused it: never record metadata for a file that is not in place (retry).
        return Err(StorageError::Busy);
    }
    tx.artifacts()
        .put_blob(&staged.digest, staged.byte_count, &staged.relative_path)?;
    Ok(())
}

/// Origin links of a new version from the host lineage of the invocation.
fn origin_links(ctx: &InvocationContext, version: &Id) -> Vec<ArtifactLink> {
    let mut links = Vec::new();
    if let Some(root) = ctx.root_id() {
        links.push(ArtifactLink {
            version_id: version.clone(),
            subject: SubjectKey::root(root),
            purpose: LinkPurpose::Origin,
        });
    }
    if let Some(a) = ctx.assignment_id() {
        links.push(ArtifactLink {
            version_id: version.clone(),
            subject: SubjectKey {
                kind: "assignment",
                id: a.0.clone(),
            },
            purpose: LinkPurpose::Origin,
        });
    }
    links
}

fn new_version(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    artifact: &Id,
    version_no: i64,
    media_type: &str,
    staged: &StagedFile,
) -> StoredVersion {
    StoredVersion {
        id: tx.new_id(IdKind::ArtifactVersion),
        artifact_id: artifact.clone(),
        version_no,
        blob_digest: staged.digest.clone(),
        byte_count: staged.byte_count,
        media_type: media_type.to_owned(),
        created_at_ms: tx.now_ms(),
        deleted_at_ms: None,
        source: json!({
            "source": staged.source,
            "operationId": staged.operation_id.0.as_str(),
            "producedBy": ctx.principal().0.as_str(),
            "principalKind": format!("{:?}", ctx.principal_kind()),
            "sessionId": ctx.session_id().map(Id::as_str),
            "repositoryId": staged.origin.repository_id.as_ref().map(Id::as_str),
            "repositoryRevision": staged.origin.repository_revision,
            "originalName": staged.origin.original_name,
        }),
    }
}

fn check_quota(tx: &mut dyn WriteTransaction, project: &ProjectId) -> Result<(), Failure> {
    let quota = setting_i64(tx.configuration(), Some(project), ARTIFACT_QUOTA_KEY)?;
    if quota > 0 && tx.artifacts().project_usage(project)? > quota {
        return Err(quota_refusal(project).into());
    }
    Ok(())
}

impl ArtifactService {
    fn save(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
    ) -> Result<Decided, Failure> {
        let OperationInput::Artifacts(ops::Input::Save(req)) = command.input() else {
            unreachable!("dispatched by input");
        };
        let ArtifactCommand::Save {
            project_id,
            title,
            media_type,
            ..
        } = &req.command
        else {
            return Err(unsupported().into());
        };
        require_project(ctx, project_id)?;
        if !tx.artifacts().project_exists(project_id)? {
            return Err(not_found("project", &project_id.0).into());
        }
        if let Some(root) = ctx.root_id()
            && !tx
                .artifacts()
                .subject_exists(project_id, &SubjectKey::root(root))?
        {
            return Err(Refusal::new(
                DomainErrorCode::InvalidArgument,
                "origin_work",
                Some(root.0.clone()),
            )
            .into());
        }
        let staged = match command.prepared::<Staging>() {
            Some(Staging::Staged(s)) => s.clone(),
            Some(Staging::Refused(r)) => return Err(r.clone().into()),
            None => {
                return Err(
                    StorageError::InvalidRecord("artifact save was not staged".into()).into(),
                );
            }
        };
        put_staged_blob(tx, &*self.files, &staged)?;
        let now = tx.now_ms();
        let artifact_id = tx.new_id(IdKind::Artifact);
        let mut artifact = StoredArtifact {
            id: artifact_id.clone(),
            origin_project_id: project_id.clone(),
            origin_work_id: ctx.root_id().cloned(),
            title: title.clone(),
            revision: 1,
            current_project_id: project_id.clone(),
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
                "sourceKind": staged.source.get("kind"),
                "repositoryId": staged.origin.repository_id.as_ref().map(Id::as_str),
                "repositoryRevision": staged.origin.repository_revision,
                "originalName": staged.origin.original_name,
            }),
        };
        tx.artifacts().insert_artifact(&artifact)?;
        let version = new_version(tx, ctx, &artifact_id, 1, media_type, &staged);
        tx.artifacts().insert_version(&version)?;
        artifact.current_version_id = Some(version.id.clone());
        tx.artifacts().update_artifact(&artifact)?;
        for link in origin_links(ctx, &version.id) {
            tx.artifacts().add_link(&link)?;
        }
        check_quota(tx, project_id)?;
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
                "byteCount": staged.byte_count, "mediaType": media_type, "title": title,
                "source": staged.source, "repositoryId": staged.origin.repository_id.as_ref().map(Id::as_str),
                "repositoryRevision": staged.origin.repository_revision,
            }),
        )?;
        let value = record(tx.artifacts(), &artifact, &version)?;
        Ok(Decided {
            decision: DomainDecision::Applied {
                result: OperationResult::ArtifactsSave { value },
            },
            operation_id: Some(staged.operation_id),
        })
    }

    fn reference(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        command: &ArtifactCommand,
    ) -> Result<DomainDecision, Failure> {
        let ArtifactCommand::Reference {
            artifact_id,
            version_id,
            subject,
            expected_revision,
        } = command
        else {
            return Err(unsupported().into());
        };
        let mut artifact = visible_artifact(tx.artifacts(), ctx, artifact_id)?;
        live_version(tx.artifacts(), &artifact, version_id)?;
        if expected_revision.get() != artifact.reference_revision {
            return Err(conflict(
                RevisionDimension::Reference,
                "artifact_references",
                artifact_id,
                expected_revision,
                artifact.reference_revision,
            ));
        }
        let key = SubjectKey::of(subject);
        let mut projects = vec![artifact.current_project_id.clone()];
        projects.extend(tx.artifacts().shares(artifact_id)?);
        let mut found = false;
        for p in &projects {
            if ctx.project_id().is_none_or(|c| c == p) && tx.artifacts().subject_exists(p, &key)? {
                found = true;
                break;
            }
        }
        if !found {
            return Err(
                Refusal::new(DomainErrorCode::NotFound, "subject", Some(key.id.clone())).into(),
            );
        }
        let added = tx.artifacts().add_link(&ArtifactLink {
            version_id: version_id.clone(),
            subject: key.clone(),
            purpose: LinkPurpose::Reference,
        })?;
        if !added {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::ArtifactsReference {
                    value: MutationSummary {
                        id: artifact_id.clone(),
                        revision: rev(artifact.reference_revision),
                        changed: false,
                    },
                },
            });
        }
        artifact.reference_revision += 1;
        tx.artifacts().update_artifact(&artifact)?;
        let root = (key.kind == "root").then(|| RootId(key.id.clone()));
        event(
            tx,
            ctx,
            &artifact,
            root,
            None,
            "ArtifactReferenced",
            json!({
                "versionId": version_id.as_str(), "subjectKind": key.kind, "subjectId": key.id.as_str(),
                "referenceRevision": artifact.reference_revision,
            }),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::ArtifactsReference {
                value: MutationSummary {
                    id: artifact_id.clone(),
                    revision: rev(artifact.reference_revision),
                    changed: true,
                },
            },
        })
    }

    fn delete(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        deletion: &ArtifactDeletion,
    ) -> Result<DomainDecision, Failure> {
        let scope = authorized_scope(tx.authority(), ctx)?;
        let summary = |id: &Id, revision: i64, changed: bool| MutationSummary {
            id: id.clone(),
            revision: rev(revision),
            changed,
        };
        match deletion {
            ArtifactDeletion::DeleteVersion {
                artifact_id,
                version_id,
                expected_revision,
                expected_reference_revision,
            } => {
                let mut artifact = visible_artifact(tx.artifacts(), ctx, artifact_id)?;
                let version = live_version(tx.artifacts(), &artifact, version_id)?;
                self.check_revisions(&artifact, expected_revision, expected_reference_revision)?;
                self.authorize_reference_set(tx, &artifact, &scope)?;
                let now = tx.now_ms();
                tx.artifacts().tombstone_version(version_id, now)?;
                if artifact.current_version_id.as_ref() == Some(version_id) {
                    // Deleting the current version never reactivates an older one.
                    artifact.current_version_id = None;
                }
                artifact.revision += 1;
                let remaining = tx
                    .artifacts()
                    .versions(artifact_id)?
                    .iter()
                    .any(StoredVersion::live);
                if !remaining {
                    self.retire(tx, &mut artifact)?;
                }
                tx.artifacts().update_artifact(&artifact)?;
                let pending = release_blob(tx, &version.blob_digest)?;
                event(
                    tx,
                    ctx,
                    &artifact,
                    None,
                    None,
                    "ArtifactVersionDeleted",
                    json!({
                        "versionId": version_id.as_str(), "digest": version.blob_digest,
                        "physicalDeletePending": pending, "artifactDeleted": !remaining,
                    }),
                )?;
                Ok(DomainDecision::Applied {
                    result: OperationResult::ArtifactsDelete {
                        value: summary(artifact_id, artifact.revision, true),
                    },
                })
            }
            ArtifactDeletion::DeleteArtifact {
                artifact_id,
                expected_revision,
                expected_reference_revision,
                expected_versions_digest,
            } => {
                let mut artifact = visible_artifact(tx.artifacts(), ctx, artifact_id)?;
                self.check_revisions(&artifact, expected_revision, expected_reference_revision)?;
                let versions = tx.artifacts().versions(artifact_id)?;
                if versions_digest(&versions) != String::from(expected_versions_digest.clone()) {
                    // An unseen concurrent version is never swept into an earlier decision.
                    return Err(Refusal::new(
                        DomainErrorCode::Conflict,
                        "artifact_versions",
                        Some(artifact_id.clone()),
                    )
                    .retry(RetryAdvice::RefreshAndNewRequest)
                    .into());
                }
                self.authorize_reference_set(tx, &artifact, &scope)?;
                let now = tx.now_ms();
                let mut digests = Vec::new();
                for v in versions.iter().filter(|v| v.live()) {
                    tx.artifacts().tombstone_version(&v.id, now)?;
                    digests.push(v.blob_digest.clone());
                }
                artifact.current_version_id = None;
                artifact.revision += 1;
                self.retire(tx, &mut artifact)?;
                tx.artifacts().update_artifact(&artifact)?;
                let mut pending = Vec::new();
                for d in &digests {
                    if release_blob(tx, d)? {
                        pending.push(d.clone());
                    }
                }
                event(
                    tx,
                    ctx,
                    &artifact,
                    None,
                    None,
                    "ArtifactDeleted",
                    json!({"versions": digests.len(), "physicalDeletePending": pending}),
                )?;
                Ok(DomainDecision::Applied {
                    result: OperationResult::ArtifactsDelete {
                        value: summary(artifact_id, artifact.revision, true),
                    },
                })
            }
            ArtifactDeletion::DetachProject {
                artifact_id,
                project_id,
                expected_reference_revision,
            } => {
                let mut artifact = visible_artifact(tx.artifacts(), ctx, artifact_id)?;
                if expected_reference_revision.get() != artifact.reference_revision {
                    return Err(conflict(
                        RevisionDimension::Reference,
                        "artifact_references",
                        artifact_id,
                        expected_reference_revision,
                        artifact.reference_revision,
                    ));
                }
                if !scope.covers(project_id) {
                    return Err(Refusal::new(
                        DomainErrorCode::PermissionDenied,
                        "project",
                        Some(project_id.0.clone()),
                    )
                    .into());
                }
                if &artifact.current_project_id == project_id {
                    // Detach never deletes: the owning project's reference is removed only by an
                    // explicit deletion (or a documents move).
                    return Err(Refusal::new(
                        DomainErrorCode::Conflict,
                        "artifact_owner_project",
                        Some(project_id.0.clone()),
                    )
                    .into());
                }
                if !tx.artifacts().remove_share(artifact_id, project_id)? {
                    return Ok(DomainDecision::NoChange {
                        result: OperationResult::ArtifactsDelete {
                            value: summary(artifact_id, artifact.reference_revision, false),
                        },
                    });
                }
                artifact.reference_revision += 1;
                artifact.scope_revision += 1;
                tx.artifacts().update_artifact(&artifact)?;
                event(
                    tx,
                    ctx,
                    &artifact,
                    None,
                    None,
                    "ArtifactProjectDetached",
                    json!({"projectId": project_id.0.as_str(), "referenceRevision": artifact.reference_revision}),
                )?;
                Ok(DomainDecision::Applied {
                    result: OperationResult::ArtifactsDelete {
                        value: summary(artifact_id, artifact.reference_revision, true),
                    },
                })
            }
        }
    }

    fn check_revisions(
        &self,
        artifact: &StoredArtifact,
        expected: &Revision,
        expected_reference: &Revision,
    ) -> Result<(), Failure> {
        if expected.get() != artifact.revision {
            return Err(conflict(
                RevisionDimension::Artifact,
                "artifact",
                &artifact.id,
                expected,
                artifact.revision,
            ));
        }
        if expected_reference.get() != artifact.reference_revision {
            return Err(conflict(
                RevisionDimension::Reference,
                "artifact_references",
                &artifact.id,
                expected_reference,
                artifact.reference_revision,
            ));
        }
        Ok(())
    }

    /// Global deletion needs authority over every project with a live reference; otherwise it is
    /// `SharedReferencesRemain` with no other projects disclosed, and nothing changes.
    fn authorize_reference_set(
        &self,
        tx: &mut dyn WriteTransaction,
        artifact: &StoredArtifact,
        scope: &AuthorizedScope,
    ) -> Result<(), Failure> {
        let mut projects = vec![artifact.current_project_id.clone()];
        projects.extend(tx.artifacts().shares(&artifact.id)?);
        if projects.iter().all(|p| scope.covers(p)) {
            return Ok(());
        }
        Err(Refusal::new(
            DomainErrorCode::Conflict,
            SHARED_REFERENCES_REMAIN,
            Some(artifact.id.clone()),
        )
        .into())
    }

    /// Marks an artifact deleted and removes its (authorized) shares.
    fn retire(
        &self,
        tx: &mut dyn WriteTransaction,
        artifact: &mut StoredArtifact,
    ) -> Result<(), StorageError> {
        artifact.state = ArtifactState::Deleted;
        artifact.current_version_id = None;
        for p in tx.artifacts().shares(&artifact.id)? {
            tx.artifacts().remove_share(&artifact.id, &p)?;
        }
        artifact.reference_revision += 1;
        artifact.scope_revision += 1;
        Ok(())
    }
}

/// After a tombstone: a blob no live version names any more becomes `UnlinkPending` (returns
/// `true`); bytes another surviving version needs stay `Present`.
fn release_blob(tx: &mut dyn WriteTransaction, digest: &str) -> Result<bool, StorageError> {
    if tx.artifacts().live_references(digest)? > 0 {
        return Ok(false);
    }
    match tx.artifacts().blob(digest)? {
        Some(b) if b.state == BlobState::Present => {
            tx.artifacts().set_blob_state(
                digest,
                BlobState::Present,
                BlobState::UnlinkPending,
                None,
            )?;
            Ok(true)
        }
        Some(b) => Ok(b.state == BlobState::UnlinkPending),
        None => Ok(false),
    }
}

fn unsupported() -> Refusal {
    Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None)
}

// ---------------------------------------------------------------------------------------------
// Queries

/// Binds a list cursor to its project.
fn list_binding(project: &ProjectId) -> String {
    sha256_hex(format!("artifacts.list|{}", project.0.as_str()).as_bytes())[..16].to_owned()
}

impl ArtifactService {
    fn list(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        project: &ProjectId,
        limit: u16,
        cursor: Option<&str>,
    ) -> Result<OperationResult, Failure> {
        require_project(ctx, project)?;
        let read = snapshot.artifacts();
        if !read.project_exists(project)? {
            return Err(not_found("project", &project.0).into());
        }
        let binding = list_binding(project);
        let after = match cursor {
            None => None,
            Some(c) => {
                let parsed = c.rsplit_once(':').and_then(|(rest, b)| {
                    let (t, id) = rest.split_once(':')?;
                    (b == binding).then_some(())?;
                    Some(ListPosition {
                        created_at_ms: t.parse().ok()?,
                        id: Id::new(id).ok()?,
                    })
                });
                Some(parsed.ok_or(Failure::Storage(StorageError::CursorExpired))?)
            }
        };
        let mut rows = read.list(project, after.as_ref(), limit.saturating_add(1).min(501))?;
        let has_more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let mut items = Vec::with_capacity(rows.len());
        for a in &rows {
            let Some(vid) = &a.current_version_id else {
                continue;
            };
            let Some(v) = read.version(vid)? else {
                continue;
            };
            items.push(record(read, a, &v)?);
        }
        let next_cursor = match (has_more, rows.last()) {
            (true, Some(last)) => Some(BoundedText(format!(
                "{}:{}:{binding}",
                last.created_at_ms,
                last.id.as_str()
            ))),
            _ => None,
        };
        Ok(OperationResult::ArtifactsList {
            value: Page {
                items: BoundedList(items),
                next_cursor: next_cursor.clone(),
                has_more: next_cursor.is_some(),
            },
        })
    }

    /// The version a read/preview/export names (a derived preview ID resolves to its source).
    fn target(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        artifact_id: &Id,
        version_id: &Id,
    ) -> Result<(StoredArtifact, StoredVersion, bool), Failure> {
        let read = snapshot.artifacts();
        let artifact = visible_artifact(read, ctx, artifact_id)?;
        if version_id.as_str().starts_with("pv-") {
            let source = read
                .versions(artifact_id)?
                .into_iter()
                .find(|v| v.live() && preview::preview_id(v.id.as_str()) == version_id.as_str())
                .ok_or_else(|| Failure::from(not_found("artifact_version", version_id)))?;
            return Ok((artifact, source, true));
        }
        let version = live_version(read, &artifact, version_id)?;
        Ok((artifact, version, false))
    }

    /// Reads a range of a version's blob; any managed-file failure is `NotFound` (retained
    /// GetAttachment behavior: an unreadable stored file is reported as not found).
    fn blob_range(&self, v: &StoredVersion, offset: u64, max: usize) -> Result<Vec<u8>, Failure> {
        self.files
            .read_range(&v.blob_digest, v.byte_count as u64, offset, max)
            .map_err(|_| not_found("artifact_file", &v.id).into())
    }

    fn preview_of(&self, v: &StoredVersion) -> Result<(preview::Preview, ArtifactRef), Failure> {
        let head = self.blob_range(v, 0, preview::PREVIEW_BYTES)?;
        let p = preview::build(&v.media_type, &head, v.byte_count as u64);
        let pid = preview::preview_id(v.id.as_str());
        // The derived cache is disposable: a failed cache write only means it is rebuilt.
        let _ = self.files.write_derived(&pid, &p.bytes);
        let reference = ArtifactRef {
            artifact_id: v.artifact_id.clone(),
            version_id: Id::new(pid).expect("derived id"),
            sha256: Digest(sha256_hex(&p.bytes)),
            byte_length: counter(p.bytes.len() as i64),
            media_type: p.media_type.to_owned(),
        };
        Ok((p, reference))
    }

    fn read(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        artifact_id: &Id,
        version_id: &Id,
        cursor: Option<&str>,
    ) -> Result<OperationResult, Failure> {
        let (artifact, version, derived) = self.target(snapshot, ctx, artifact_id, version_id)?;
        let (bytes, reference, textual) = if derived {
            let (p, r) = self.preview_of(&version)?;
            (Some(p.bytes), r, true)
        } else {
            (
                None,
                version_ref(&version),
                preview::textual(&version.media_type),
            )
        };
        let total = reference.byte_length.get() as u64;
        let first = match cursor {
            None => 0,
            Some(c) => c
                .parse::<u64>()
                .ok()
                .filter(|o| *o <= total)
                .ok_or_else(|| Refusal::new(DomainErrorCode::InvalidArgument, "cursor", None))?,
        };
        if textual {
            let window = match &bytes {
                Some(b) => b[first as usize..]
                    .iter()
                    .take(READ_WINDOW)
                    .copied()
                    .collect(),
                None => self.blob_range(&version, first, READ_WINDOW)?,
            };
            if let Some(text) = preview::utf8_prefix(&window)
                && (!text.is_empty() || first == total)
            {
                let next = first + text.len() as u64;
                return Ok(OperationResult::ArtifactsRead {
                    value: ArtifactReadContent::Text {
                        artifact: reference,
                        first_byte: counter(first as i64),
                        next_byte: counter(next as i64),
                        content: BoundedText(text.to_owned()),
                        truncated: next < total,
                    },
                });
            }
        }
        // Binary (or not valid UTF-8): a principal-bound, expiring stream handle, issued only
        // while the stored file is in place.
        if !self
            .files
            .present(&version.blob_digest, version.byte_count as u64)
            .unwrap_or(false)
        {
            return Err(not_found("artifact_file", &version.id).into());
        }
        let grant = self.streams.issue(
            &*self.clock,
            ctx,
            query,
            &artifact,
            &version,
            StreamPurpose::Read,
        );
        Ok(OperationResult::ArtifactsRead {
            value: ArtifactReadContent::Binary {
                artifact: reference,
                stream_id: grant.stream_id,
                first_byte: counter(0),
                byte_length: counter(version.byte_count),
                expires_at_ms: counter(grant.expires_at_ms),
            },
        })
    }

    fn preview(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        artifact_id: &Id,
        version_id: &Id,
    ) -> Result<OperationResult, Failure> {
        let (_, version, derived) = self.target(snapshot, ctx, artifact_id, version_id)?;
        if derived {
            return Err(Refusal::new(
                DomainErrorCode::UnsupportedTarget,
                "artifact_preview",
                Some(version_id.clone()),
            )
            .into());
        }
        let (p, reference) = self.preview_of(&version)?;
        let state = match p.extent {
            preview::Extent::Complete => CoverageState::Complete,
            preview::Extent::Partial => CoverageState::Partial,
            preview::Extent::Unavailable => CoverageState::Unavailable,
        };
        Ok(OperationResult::ArtifactsPreview {
            value: ArtifactPreview {
                artifact: version_ref(&version),
                preview: reference,
                page: None,
                coverage: Coverage {
                    state,
                    gaps: Vec::new(),
                    more_gaps: false,
                },
            },
        })
    }

    fn export(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        artifact_id: &Id,
        version_id: &Id,
    ) -> Result<OperationResult, Failure> {
        let (artifact, version, derived) = self.target(snapshot, ctx, artifact_id, version_id)?;
        if derived {
            return Err(Refusal::new(
                DomainErrorCode::UnsupportedTarget,
                "artifact_preview",
                Some(version_id.clone()),
            )
            .into());
        }
        if !self
            .files
            .present(&version.blob_digest, version.byte_count as u64)
            .unwrap_or(false)
        {
            return Err(not_found("artifact_file", &version.id).into());
        }
        let grant = self.streams.issue(
            &*self.clock,
            ctx,
            query,
            &artifact,
            &version,
            StreamPurpose::Export,
        );
        Ok(OperationResult::ArtifactsExport {
            value: ArtifactExport {
                artifact: version_ref(&version),
                stream_id: grant.stream_id,
                expires_at_ms: counter(grant.expires_at_ms),
                suggested_name: BoundedText(lifecycle::suggested_name(&artifact, &version)),
            },
        })
    }

    fn query_failure(&self, f: Failure) -> DomainFault {
        match f {
            Failure::Refused(r) => r.fault(self.correlation()),
            Failure::Storage(e) => {
                let (code, retry) = match e {
                    StorageError::CursorExpired => (
                        DomainErrorCode::CursorExpired,
                        RetryAdvice::RefreshAndNewRequest,
                    ),
                    StorageError::Cancelled => (DomainErrorCode::Cancelled, RetryAdvice::Never),
                    _ => (
                        DomainErrorCode::StorageUnavailable,
                        RetryAdvice::SameCommand,
                    ),
                };
                Refusal::new(code, "artifact", None)
                    .retry(retry)
                    .fault(self.correlation())
            }
        }
    }
}

impl QueryHandler for ArtifactService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let request = match query.input() {
            OperationInput::Artifacts(
                ops::Input::List(r)
                | ops::Input::Read(r)
                | ops::Input::Preview(r)
                | ops::Input::Export(r),
            ) => r,
            _ => return Err(unsupported().fault(self.correlation())),
        };
        let cursor = request.cursor.as_deref();
        let answered = match &request.query {
            ArtifactQuery::List { project_id } => {
                self.list(snapshot, ctx, project_id, request.limit, cursor)
            }
            ArtifactQuery::Read {
                artifact_id,
                version_id,
            } => self.read(snapshot, ctx, query, artifact_id, version_id, cursor),
            ArtifactQuery::Preview {
                artifact_id,
                version_id,
            } => self.preview(snapshot, ctx, artifact_id, version_id),
            ArtifactQuery::Export {
                artifact_id,
                version_id,
            } => self.export(snapshot, ctx, query, artifact_id, version_id),
            // No catalog operation selects citation resolution (documents item).
            ArtifactQuery::ResolveCitation { .. } => Err(unsupported().into()),
        };
        let scope_revision = match &request.query {
            ArtifactQuery::Read { artifact_id, .. }
            | ArtifactQuery::Preview { artifact_id, .. }
            | ArtifactQuery::Export { artifact_id, .. } => snapshot
                .artifacts()
                .artifact(artifact_id)
                .ok()
                .flatten()
                .map_or(1, |a| a.scope_revision),
            _ => 1,
        };
        let result = answered.map_err(|f| self.query_failure(f))?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(scope_revision)),
            result,
        })
    }
}

impl CommandHandler for ArtifactService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        let OperationInput::Artifacts(ops::Input::Save(req)) = command.input() else {
            return Ok(None);
        };
        let ArtifactCommand::Save {
            project_id,
            source,
            expected_digest,
            expected_bytes,
            ..
        } = &req.command
        else {
            return Ok(None);
        };
        // A replay decides nothing new, so nothing is staged for it.
        let replay = read_snapshot(env.store, &ReadOptions::default(), |s| {
            Ok(s.receipts().lookup(command.identity())?.is_some())
        })?;
        if replay {
            return Ok(None);
        }
        // Scope refusals are decided (and recorded) by `decide`; stage nothing for them.
        if let Err(r) = require_project(ctx, project_id) {
            return Ok(Some(Prepared::new(Staging::Refused(r))));
        }
        let staging = self.stage(
            env,
            ctx,
            &StageRequest {
                project: project_id,
                source,
                expected_digest,
                expected_bytes: expected_bytes.get(),
                request_digest: command.fingerprint().as_str(),
            },
        )?;
        Ok(Some(Prepared::new(staging)))
    }

    /// A save whose staged file the unit of work did not commit — refused (for example by the
    /// in-transaction quota check), replayed, denied, rejected or rolled back — resolves its
    /// staging operation now instead of at the next boot ([`ArtifactService::abandon`]). An
    /// unknown commit outcome is left to recovery, which reads the durable record first.
    fn settle(
        &self,
        _ctx: &InvocationContext,
        prepared: Option<&Prepared>,
        reply: Option<&crate::protocol::results::DomainCommandReply>,
        env: &Preparation<'_>,
    ) {
        let Some(Staging::Staged(staged)) = prepared.and_then(|p| p.get::<Staging>()) else {
            return;
        };
        let committed_or_unknown = match reply {
            Some(crate::protocol::CommandReply::Recorded {
                receipt, replayed, ..
            }) => !replayed && !matches!(receipt.decision, DomainDecision::Refused { .. }),
            Some(crate::protocol::CommandReply::CommandStatusUnknown { .. }) => true,
            _ => false,
        };
        if !committed_or_unknown {
            // Best effort: a failure here leaves the operation to recovery, which is fenced the
            // same way, so nothing is ever resolved twice.
            let _ = self.abandon(env.store, staged);
        }
    }

    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let outcome = match command.input() {
            OperationInput::Artifacts(ops::Input::Save(_)) => self.save(tx, ctx, command),
            OperationInput::Artifacts(ops::Input::Reference(req)) => self
                .reference(tx, ctx, &req.command)
                .map(|decision| Decided {
                    decision,
                    operation_id: None,
                }),
            OperationInput::Artifacts(ops::Input::Delete(req)) => {
                self.delete(tx, ctx, &req.deletion).map(|decision| Decided {
                    decision,
                    operation_id: None,
                })
            }
            _ => Err(unsupported().into()),
        };
        match outcome {
            Ok(d) => Ok(d),
            Err(Failure::Refused(r)) => Ok(Decided {
                decision: DomainDecision::Refused {
                    fault: r.fault(CorrelationId(tx.new_id(IdKind::Correlation))),
                },
                operation_id: None,
            }),
            Err(Failure::Storage(e)) => Err(HandlerFailure::Storage(e)),
        }
    }
}
