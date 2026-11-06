//! `ArtifactService`: the local artifact store behind `artifacts.save`, `.list`,
//! `.read`, `.preview`, `.export`, `.reference`, `.delete` and `.tag` (ART-01..05), registered through
//! [`crate::services::UseCases::with_artifacts`]. The UI, agent tools, packages and MCP reach the
//! same use cases through the gateway; there is no second implementation of any rule.
//!
//! # Publication (AD 24)
//!
//! A save is published in this order, so a crash at any point is reconcilable:
//!
//! 1. [`CommandHandler::prepare`] (after admission and current authority, outside any write
//!    transaction; skipped for replay-only authority and stopped by cancellation or the command's
//!    deadline) resolves the host source handle ([`ArtifactSources`]) and opens it with the
//!    verified no-follow reader;
//! 2. it journals a staging operation (`artifacts.stage_file`, committed as `Dispatching`) in its
//!    own small write, through a preparation port limited to the journal and artifact records;
//! 3. the file port streams the bytes in bounded chunks into `staging/` while hashing them,
//!    checks the caller's expected digest and length, flushes them, renames them to their
//!    content-addressed location in `artifacts/` and flushes every directory involved;
//! 4. [`CommandHandler::decide`] — inside the gateway's unit of work, atomically with the receipt
//!    — records the blob, the artifact and its immutable version, origin links, provenance and
//!    history, and completes the staging operation.
//!
//! A crash before step 4 leaves a journaled operation and at most a staged or unreferenced file;
//! [`ArtifactService::recover`] fails the operation and collects the files. Committed metadata
//! never names a file that was not durably published; a file without metadata is never visible.
//! When the process survives but step 4 does not commit (a refusal such as the in-transaction
//! quota check, a replay, a denial, a rollback or an unknown commit outcome),
//! [`CommandHandler::settle`] resolves the staging operation at once under its attempt fence
//! ([`ArtifactService::abandon`]); a resolution that cannot be written is retried later in the
//! same boot, and nothing is resolved twice.
//!
//! **Generations make removal exact.** Every publication of new bytes writes a generation file
//! `<digest>.<staging operation ID>` that is never written again; the blob row names the one
//! generation readers use, and reuse keeps an existing generation. Every removal names an exact
//! `(digest, generation)`, so a stale remover can never delete a newer incarnation:
//!
//! * a pending removal is claimed as a compare-and-set on (state, generation) recording its owner,
//!   the file of that generation is removed, and the outcome is recorded under the same owner;
//!   a claim another holder left (a crash, or an outcome that could not be written) is adopted by
//!   compare-and-set on (generation, owner) by the next collection or recovery;
//! * **retire on claim:** a generation ever claimed for removal is permanently dead. A failed
//!   removal leaves it `Retired` (still named, retried by the next collection or recovery),
//!   never back to a reusable state; staging never offers it for reuse and a save never keeps
//!   it, so a late or concurrent removal of a claimed generation can only remove dead bytes;
//! * a staging operation's own generation is removed only once the operation is resolved and the
//!   row does not name it — from then on nothing can ever bind it (an operation binds its
//!   generation only in the transaction that resolves it);
//! * a save that finds its digest claimed, or a reused generation changed since it was verified,
//!   refuses as `Busy` and a retry publishes a fresh generation.
//!
//! This is what makes recovery safe to run while commands are admitted and concurrently with
//! itself, collection and abandonment.
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
//! # Scopes and retention
//!
//! An artifact is owned by a project or by the workspace ([`ArtifactScope`]). A workspace artifact
//! (a projectless receipt's reply, a workspace attachment) has an owning principal and no
//! project; it follows the same visibility, deletion, retention and quota rules as a project
//! artifact, with the workspace as its scope: its audience is its owner, the user, the host and
//! the unconfined workspace coordinator ([`Viewer`]), and a project sees it only once it is
//! shared with that project. Workspace usage is measured against the workspace-level storage
//! limit. From schema revision 3, `artifacts.save` and `artifacts.list` name the workspace
//! (`workspace: true`) instead of a project: only a workspace invocation of a principal whose
//! authorized scope is the workspace may ([`WORKSPACE_SCOPE`]), a saved workspace artifact is
//! owned by its saver, and the listing returns what the principal is in the audience of.
//! `artifacts.tag` replaces an existing artifact's tag set under its revision (never a receipt's
//! reply).
//!
//! A receipt's durable typed reply ([`replies`]) is a [`ArtifactRetention::Receipt`] artifact:
//! system-owned, immutable and retained while its receipt is retained, because the shared
//! contracts keep command identity and its recorded decision as correctness records, never
//! disposable logs. `artifacts.delete` refuses it (`Conflict`, [`RECEIPT_RETAINED`]) and it is
//! never shared, moved or replaced, so explicit user deletion under ART-05 (which covers the
//! user's own work outputs) can never break a receipt. Only the receipt's principal (with the
//! user, the host and the workspace coordinator) sees it, so another principal's receipts are
//! not disclosed, and project listings omit it. Because a receipt identity has no project, a
//! reply is judged by the viewer's current authorized scope (workspace-wide, or a confined
//! principal's binding project), not by the invocation's lineage project ([`visible_in`]). Its bytes are system-owned: they never count
//! toward the scope's storage limit (which measures the user's own work outputs) and are
//! reported separately in the storage status. `artifacts.reference` on a reply is allowed on
//! purpose: it only links the immutable version to work, and changes neither its content nor
//! its retention.
//!
//! # Access (ART-04)
//!
//! Current authority (role, selected tools, reviewer read-only, project confinement) is the
//! gateway's [`crate::ports::AuthorityPort`]. On top of it, every use case checks scope: an
//! invocation bound to a project sees only artifacts that project owns or is shared; a workspace
//! invocation sees every project's artifacts and the workspace artifacts its principal may see;
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
pub mod evidence;
pub mod host;
mod index_source;
pub mod lifecycle;
pub mod observed;
pub mod preview;
pub mod replies;

use crate::authority::reads::{ReadRefusal, ReadRequest, VerifiedReader};
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, artifacts as ops};
use crate::ports::files::{ArtifactFiles, ArtifactSources, FileFault, SourceOrigin, SourceRefusal};
use crate::ports::storage::{
    ArtifactLink, ArtifactRetention, ArtifactScope, ArtifactState, AttemptBinding, AttemptFence,
    AuthorityRead, BlobState, ConfigScope, ConfigurationRead, LinkPurpose, ListPosition, NewEvent,
    NewOperation, ReadSnapshot, StorageError, StoredArtifact, StoredVersion, SubjectKey,
    Unavailable, WriteTransaction,
};
use crate::ports::{
    Clock, CommandHandler, Decided, HandlerFailure, IdKind, IdSource, Preparation, Prepared,
    QueryAnswer, QueryHandler,
};
use crate::protocol::data::{
    ArtifactCommand, ArtifactDeletion, ArtifactQuery, ArtifactSource, ArtifactTags, Coverage,
    CoverageState, DataScope,
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
use crate::registry::SchemaRevision;
use crate::settings::catalog::{ARTIFACT_MAX_FILE_KEY, ARTIFACT_QUOTA_KEY, AgentRole, descriptor};
use crate::settings::model::{chain, current_chain};
use lifecycle::{Readers, StreamPurpose, Streams};
use serde_json::{Value, json};
use std::sync::Arc;

/// The operations [`ArtifactService`] is registered for.
pub const OPERATIONS: [&str; 8] = [
    "artifacts.save",
    "artifacts.list",
    "artifacts.read",
    "artifacts.preview",
    "artifacts.export",
    "artifacts.reference",
    "artifacts.delete",
    "artifacts.tag",
];
/// History aggregate of an artifact (`aggregate_id` = artifact ID). Events carry the owning
/// project and, where there is one, the origin or referencing work root, so work history
/// navigates to its artifacts and an artifact's history navigates back (HISTORY-01/02).
pub const ARTIFACT_AGGREGATE: &str = "artifact";
/// History aggregate of an artifact's reference set (`aggregate_revision` = reference revision).
pub const REFERENCES_AGGREGATE: &str = "artifact_references";
/// History aggregate of an artifact's visibility scope (`aggregate_revision` = scope revision).
pub const SCOPE_AGGREGATE: &str = "artifact_scope";
/// History aggregate of an artifact's exports (ordered by commit; no artifact revision changes).
pub const EXPORT_AGGREGATE: &str = "artifact_export";
/// History aggregate of a managed blob's physical lifecycle (`aggregate_id` = digest).
pub const BLOB_AGGREGATE: &str = "artifact_blob";
/// Journal capability of a staged file publication.
pub use crate::ports::storage::STAGE_CAPABILITY;
/// Largest inline text window of one `artifacts.read` page.
pub const READ_WINDOW: usize = 65536;
/// Fault target kind of a delete refused because live references outside the caller's
/// authorized scope remain (`SharedReferencesRemain`; nothing changed).
pub const SHARED_REFERENCES_REMAIN: &str = "SharedReferencesRemain";
/// Fault target kind of a save refused by the project's storage limit (nothing was deleted).
pub const QUOTA_TARGET: &str = "artifact_storage_quota";
/// Fault target kind of a delete, share, move or replacement refused because the artifact is a
/// receipt's durable reply, retained while its receipt is retained (nothing changed).
pub const RECEIPT_RETAINED: &str = "artifact_receipt_retained";

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
        FileFault::Io(e) | FileFault::Unsupported(e) | FileFault::Source(e) => {
            StorageError::unavailable(Unavailable::Io, e)
        }
        FileFault::Mismatch => StorageError::unavailable(Unavailable::Io, "source changed"),
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
    /// Whether the scope covers an artifact owner scope.
    fn covers_scope(&self, scope: &ArtifactScope) -> bool {
        match scope {
            ArtifactScope::Project(p) => self.covers(p),
            ArtifactScope::Workspace => *self == Self::Workspace,
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

/// Who is looking at artifacts: the invocation, the principal's current authorized scope from
/// its host binding (`None` once the binding is inactive, or when a non-human principal has no
/// binding at all), and whether it is the unconfined
/// workspace coordinator.
#[derive(Clone, Debug)]
pub struct Viewer<'a> {
    ctx: &'a InvocationContext,
    coordinator: bool,
    authorized: Option<AuthorizedScope>,
}
impl<'a> Viewer<'a> {
    pub fn resolve(
        authority: &dyn AuthorityRead,
        ctx: &'a InvocationContext,
    ) -> Result<Self, StorageError> {
        if matches!(
            ctx.principal_kind(),
            PrincipalKind::Human | PrincipalKind::Host
        ) {
            return Ok(Self {
                ctx,
                coordinator: false,
                authorized: Some(AuthorizedScope::Workspace),
            });
        }
        Ok(match authority.principal(ctx.principal())? {
            Some(b) if !b.active => Self {
                ctx,
                coordinator: false,
                authorized: None,
            },
            Some(b) => Self {
                ctx,
                coordinator: ctx.principal_kind() == PrincipalKind::Agent
                    && b.kind == PrincipalKind::Agent
                    && b.role == Some(AgentRole::WorkspaceCoordinator)
                    && b.project_id.is_none(),
                authorized: Some(
                    b.project_id
                        .map_or(AuthorizedScope::Workspace, AuthorizedScope::Project),
                ),
            },
            // A non-human principal with no host binding has no authorized scope: fail closed.
            None => Self {
                ctx,
                coordinator: false,
                authorized: None,
            },
        })
    }
    /// The workspace-level audience of what `owner` owns: the owner itself, the user, the host
    /// and the unconfined workspace coordinator.
    fn sees_owned(&self, owner: Option<&crate::protocol::ActorId>) -> bool {
        matches!(
            self.ctx.principal_kind(),
            PrincipalKind::Human | PrincipalKind::Host
        ) || self.coordinator
            || owner == Some(self.ctx.principal())
    }
}

/// Whether an invocation may see `artifact`. A project-bound invocation sees what its project
/// owns or was shared; a workspace invocation sees every project's artifacts and the workspace
/// artifacts its principal belongs to the audience of. A receipt reply is judged by its
/// principal instead (see [`visible_in`]).
pub(crate) fn visible(
    read: &dyn crate::ports::storage::ArtifactRead,
    viewer: &Viewer<'_>,
    artifact: &StoredArtifact,
) -> Result<bool, StorageError> {
    visible_in(read, viewer, artifact, viewer.ctx.project_id())
}

/// [`visible`] for work of `project` (`None`: the workspace) rather than the invocation's own
/// project, for references a session or a work item records.
///
/// A receipt reply is not judged by the invocation's lineage project: a receipt identity is
/// `(actor, scope key, request key)` and has no project, so its principal may look it up from
/// any context. It is visible to its receipt's audience (the receipt's principal, the user, the
/// host, the unconfined workspace coordinator) within that principal's current authorized scope:
/// workspace-wide for the user, the host and unconfined principals; a confined principal's
/// binding project otherwise (owned or shared). An inactive binding sees none.
pub(crate) fn visible_in(
    read: &dyn crate::ports::storage::ArtifactRead,
    viewer: &Viewer<'_>,
    artifact: &StoredArtifact,
    project: Option<&ProjectId>,
) -> Result<bool, StorageError> {
    if artifact.retention == ArtifactRetention::Receipt {
        if !viewer.sees_owned(artifact.owner.as_ref()) {
            return Ok(false);
        }
        return Ok(match &viewer.authorized {
            None => false,
            Some(AuthorizedScope::Workspace) => true,
            Some(AuthorizedScope::Project(p)) => {
                artifact.scope.project() == Some(p) || read.shares(&artifact.id)?.contains(p)
            }
        });
    }
    Ok(match (project, &artifact.scope) {
        (None, ArtifactScope::Project(_)) => true,
        (None, ArtifactScope::Workspace) => viewer.sees_owned(artifact.owner.as_ref()),
        (Some(p), ArtifactScope::Project(owner)) => {
            owner == p || read.shares(&artifact.id)?.contains(p)
        }
        // A host-published repository text is readable where its repository is: by every
        // project that has the repository as a member (CODE-07; no share to maintain).
        (Some(p), ArtifactScope::Workspace) => {
            read.shares(&artifact.id)?.contains(p)
                || read.repository_text_readers(&artifact.id)?.contains(p)
        }
    })
}

/// Shares the artifact of `reference` with `project` for a workspace-authorized caller that
/// sees it at workspace level and names it in that project's work (a project's initial plan
/// names content written before the project existed: its agents must read it there). A live,
/// explicitly retained artifact only; the share is recorded like `ArtifactShared` of the share
/// path. Returns whether it was shared (nothing changes otherwise).
pub(crate) fn share_named_in(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    reference: &ArtifactRef,
    project: &ProjectId,
) -> Result<bool, StorageError> {
    let viewer = Viewer::resolve(tx.authority(), ctx)?;
    if viewer.authorized != Some(AuthorizedScope::Workspace) {
        return Ok(false);
    }
    let Some(mut artifact) = tx.artifacts().artifact(&reference.artifact_id)? else {
        return Ok(false);
    };
    if artifact.state != ArtifactState::Live
        || artifact.retention != ArtifactRetention::Explicit
        || retained_help(&artifact, &tx.store().host_actor)
        || artifact.scope.project() == Some(project)
        || !visible_in(tx.artifacts(), &viewer, &artifact, None)?
        || !tx.artifacts().add_share(&artifact.id, project)?
    {
        return Ok(false);
    }
    artifact.scope_revision += 1;
    artifact.reference_revision += 1;
    tx.artifacts().update_artifact(&artifact)?;
    event(
        tx,
        ctx,
        &artifact,
        None,
        None,
        "ArtifactShared",
        json!({"projectId": project.0.as_str(), "scopeRevision": artifact.scope_revision}),
    )?;
    Ok(true)
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

/// The schema revision that introduced the workspace scope of saves and listings.
pub const WORKSPACE_SINCE: SchemaRevision = SchemaRevision(3);
/// Fault target kind of a workspace save or listing outside the caller's authorized scope.
pub const WORKSPACE_SCOPE: &str = "workspace_scope";

/// The owner scope a save or listing names. The workspace scope (revision 3) is reachable only
/// from a workspace invocation (no project, no work lineage) of a principal whose current
/// authorized scope is the workspace: the user, the host and unconfined principals. A
/// project-confined principal, or a work invocation, is refused (`PermissionDenied`).
pub(crate) fn named_scope(
    authority: &dyn AuthorityRead,
    ctx: &InvocationContext,
    project: Option<&ProjectId>,
) -> Result<ArtifactScope, Failure> {
    match project {
        Some(p) => {
            require_project(ctx, p)?;
            Ok(ArtifactScope::Project(p.clone()))
        }
        None => {
            let workspace_invocation = ctx.project_id().is_none() && ctx.root_id().is_none();
            let viewer = Viewer::resolve(authority, ctx)?;
            if !workspace_invocation || viewer.authorized != Some(AuthorizedScope::Workspace) {
                return Err(
                    Refusal::new(DomainErrorCode::PermissionDenied, WORKSPACE_SCOPE, None).into(),
                );
            }
            Ok(ArtifactScope::Workspace)
        }
    }
}

/// The live artifact `id` if this invocation may see it (otherwise `NotFound`).
fn visible_artifact(
    read: &dyn crate::ports::storage::ArtifactRead,
    viewer: &Viewer<'_>,
    id: &Id,
) -> Result<StoredArtifact, Failure> {
    match read.artifact(id)? {
        Some(a) if a.state == ArtifactState::Live && visible(read, viewer, &a)? => Ok(a),
        _ => Err(not_found("artifact", id).into()),
    }
}

/// A receipt's reply is never deleted, shared, moved or replaced through the artifact use cases.
fn retained_help(artifact: &StoredArtifact, host: &crate::protocol::ActorId) -> bool {
    artifact.provenance["principalKind"] == "Host"
        && artifact.provenance["producedBy"].as_str() == Some(host.0.as_str())
        && artifact.provenance["derived"]["kind"] == "HelpTopic"
}

fn require_explicit(
    artifact: &StoredArtifact,
    host: &crate::protocol::ActorId,
) -> Result<(), Refusal> {
    if retained_help(artifact, host) {
        return Err(Refusal::new(
            DomainErrorCode::Conflict,
            "help_retained",
            Some(artifact.id.clone()),
        ));
    }
    match artifact.retention {
        ArtifactRetention::Explicit => Ok(()),
        ArtifactRetention::Receipt => Err(Refusal::new(
            DomainErrorCode::Conflict,
            RECEIPT_RETAINED,
            Some(artifact.id.clone()),
        )),
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

/// Visible storage status of one scope (ART-04/05): usage against its limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageStatus {
    /// Bytes of explicitly retained artifacts: what the limit measures.
    pub used_bytes: i64,
    /// Bytes of the scope's system-owned receipt replies, shown separately: they never count
    /// toward the limit and are never refused by it.
    pub receipt_reply_bytes: i64,
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

/// The schema revision that introduced artifact tags (ART-02).
pub const TAGS_SINCE: SchemaRevision = SchemaRevision(2);

/// The wire record of `a` at version `v` for a caller that negotiated `revision`. Tags are read
/// (and their stored form validated) only for a caller that can receive them.
fn tested_repository(
    v: &StoredVersion,
    revision: SchemaRevision,
) -> Result<Option<crate::protocol::records::TestedRepositoryRevision>, StorageError> {
    if revision < SchemaRevision(4) {
        return Ok(None);
    }
    let source = v
        .source
        .pointer("/source/testedRepository")
        .map(|value| {
            serde_json::from_value::<crate::protocol::records::TestedRepositoryRevision>(
                value.clone(),
            )
            .map_err(|_| StorageError::InvalidRecord("stored tested repository revision".into()))
        })
        .transpose()?;
    if let Some(source) = &source {
        crate::protocol::support::Check::check(source).map_err(StorageError::InvalidRecord)?;
    }
    Ok(source)
}
fn record(
    read: &dyn crate::ports::storage::ArtifactRead,
    a: &StoredArtifact,
    v: &StoredVersion,
    revision: SchemaRevision,
) -> Result<ArtifactRecord, StorageError> {
    let references = read
        .links(&a.id)?
        .iter()
        .filter(|l| l.purpose == LinkPurpose::Reference)
        .count()
        + read.shares(&a.id)?.len();
    let tags = if revision >= TAGS_SINCE {
        Some(
            ArtifactTags::new(read.tags(&a.id)?)
                .map_err(|e| StorageError::InvalidRecord(format!("stored artifact tags: {e}")))?,
        )
    } else {
        None
    };
    Ok(ArtifactRecord {
        tested_repository: tested_repository(v, revision)?,
        artifact: version_ref(v),
        name: label(&a.title),
        created_at_ms: counter(a.created_at_ms),
        scope: match &a.scope {
            ArtifactScope::Project(p) => DataScope::Project {
                project_id: p.clone(),
            },
            ArtifactScope::Workspace => DataScope::Workspace,
        },
        reference_count: counter(references as i64),
        tags,
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
/// Records that a committed transcript entry of `session` names `reference` (ART-02/05): in the
/// same transaction as the entry, the version gains a `session` reference link and the
/// artifact's reference revision advances (once per session and version), with an attributed
/// `ArtifactReferenced` event. A deletion decided at the earlier reference revision is then a
/// `RevisionConflict`, so referenced evidence is never removed by a stale decision. The caller
/// has already resolved the reference ([`crate::services::sessions::transcript::reference_resolves`]).
pub(crate) fn reference_from_transcript(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    reference: &ArtifactRef,
    session: &Id,
    root: Option<RootId>,
) -> Result<(), StorageError> {
    reference_from_host_owner(tx, ctx, reference, &SubjectKey::session(session), root)
}

/// Records an already validated host-owned reference in its producing unit of work. The caller
/// must establish the exact immutable version and owning subject before invoking this helper.
/// This is not a public reference operation or an authority grant.
pub(crate) fn reference_from_host_owner(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    reference: &ArtifactRef,
    subject: &SubjectKey,
    root: Option<RootId>,
) -> Result<(), StorageError> {
    let added = tx.artifacts().add_link(&ArtifactLink {
        version_id: reference.version_id.clone(),
        subject: subject.clone(),
        purpose: LinkPurpose::Reference,
    })?;
    if !added {
        return Ok(());
    }
    let mut artifact = tx
        .artifacts()
        .artifact(&reference.artifact_id)?
        .ok_or_else(|| StorageError::NotFound {
            kind: "artifact",
            id: reference.artifact_id.as_str().to_owned(),
        })?;
    artifact.reference_revision += 1;
    tx.artifacts().update_artifact(&artifact)?;
    event(
        tx,
        ctx,
        &artifact,
        root,
        (subject.kind == "operation").then(|| OperationId(subject.id.clone())),
        "ArtifactReferenced",
        json!({
            "versionId": reference.version_id.as_str(), "subjectKind": subject.kind,
            "subjectId": subject.id.as_str(), "referenceRevision": artifact.reference_revision,
        }),
    )?;
    Ok(())
}

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
    // Each event belongs to the aggregate of the revision dimension it advances, so no
    // aggregate ever records the same revision twice.
    let (aggregate_kind, aggregate_revision) = match kind {
        "ArtifactReferenced" | "ArtifactProjectDetached" => {
            (REFERENCES_AGGREGATE, artifact.reference_revision)
        }
        "ArtifactShared" => (SCOPE_AGGREGATE, artifact.scope_revision),
        // An export changes no artifact revision: its own aggregate orders by commit.
        "ArtifactExported" => (EXPORT_AGGREGATE, tx.committed_sequence()? + 1),
        _ => (ARTIFACT_AGGREGATE, artifact.revision),
    };
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id,
        actor: ctx.principal().clone(),
        project_id: artifact.scope.project().cloned(),
        root_id: root,
        operation_id: operation,
        aggregate_kind: aggregate_kind.into(),
        aggregate_id: artifact.id.as_str().to_owned(),
        aggregate_revision,
        kind: kind.into(),
        payload,
    })
}

// ---------------------------------------------------------------------------------------------
// Staging (preparation)

/// One source to stage for `scope`, with the caller's expected identity.
#[derive(Clone, Copy, Debug)]
pub struct StageRequest<'a> {
    pub scope: &'a ArtifactScope,
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
    /// The generation holding the bytes: this operation's own new generation (its ID), or the
    /// existing generation it verified and reused.
    pub generation: String,
    pub reused: bool,
    pub relative_path: String,
    pub origin: SourceOrigin,
    /// The resolved source handle (provenance).
    pub source: Value,
    /// The staging attempt's journal fence: only this attempt's owner resolves the operation.
    pub fence: AttemptFence,
}
impl StagedFile {
    pub fn staging_ref(&self) -> StagingRef {
        StagingRef {
            operation_id: self.operation_id.clone(),
            fence: self.fence.clone(),
            digest: self.digest.clone(),
            byte_count: self.byte_count,
        }
    }
}

/// What resolving a journaled staging operation needs. The only file a staging operation can
/// own is its own generation `<digest>.<operation ID>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagingRef {
    pub operation_id: OperationId,
    pub fence: AttemptFence,
    pub digest: String,
    pub byte_count: i64,
}

/// Wraps a source so a cancelled or expired command stops between chunks.
struct Stoppable<'a> {
    inner: &'a mut dyn std::io::Read,
    env: &'a Preparation<'a>,
    clock: &'a dyn Clock,
}
impl std::io::Read for Stoppable<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.env.stopped(self.clock.now_ms().get()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "stopped",
            ));
        }
        self.inner.read(buf)
    }
}

/// Fault target kind of a save refused because the existing managed file generation for its
/// digest is corrupt. The file is quarantined; a new request publishes a fresh generation.
pub const INTEGRITY_TARGET: &str = "artifact_blob_corrupt";

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
    /// Staging resolutions that could not be written when they arose; retried by the next
    /// recovery or collection in this boot.
    pub(crate) pending: std::sync::Mutex<Vec<(StagingRef, Value)>>,
    /// Generations this service is removing right now (best-effort coordination only).
    pub(crate) removing: std::sync::Mutex<std::collections::HashSet<(String, String)>>,
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
            pending: std::sync::Mutex::new(Vec::new()),
            removing: std::sync::Mutex::new(std::collections::HashSet::new()),
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
            scope,
            source,
            expected_digest,
            expected_bytes,
            request_digest,
        } = *request;
        let stopped = || env.stopped(self.clock.now_ms().get());
        if stopped() {
            return Err(StorageError::Cancelled);
        }
        let digest = String::from(expected_digest.clone());
        let limits = env.read(|s| {
            let max = setting_i64(s.configuration(), None, ARTIFACT_MAX_FILE_KEY);
            let quota = setting_i64(s.configuration(), scope.project(), ARTIFACT_QUOTA_KEY);
            let row = s.artifacts().blob(&digest)?;
            let usage = usage(s.artifacts(), scope)?;
            Ok((max, quota, row, usage))
        })?;
        let (max, quota, row, usage) = limits;
        let new_bytes = row.is_none();
        // An existing Present (or pending-removal) generation is reused after verification; any
        // other state publishes a new generation.
        // A retired or removed generation is never offered for reuse.
        let reuse = row
            .filter(|b| matches!(b.state, BlobState::Present | BlobState::UnlinkPending))
            .map(|b| b.generation);
        let (max, quota) = match (max, quota) {
            (Ok(m), Ok(q)) => (m, q),
            (Err(Failure::Refused(r)), _) | (_, Err(Failure::Refused(r))) => {
                return Ok(Staging::Refused(r));
            }
            (Err(Failure::Storage(e)), _) | (_, Err(Failure::Storage(e))) => return Err(e),
        };
        let too_large = || {
            Refusal::new(
                DomainErrorCode::InvalidArgument,
                "artifact_file_limit",
                None,
            )
        };
        if expected_bytes > max {
            return Ok(Staging::Refused(too_large()));
        }
        // Bytes no blob holds yet always add to usage: refuse before reading or staging them.
        if quota > 0 && new_bytes && usage.saturating_add(expected_bytes) > quota {
            return Ok(Staging::Refused(quota_refusal(scope)));
        }
        let resolved = match self.sources.resolve(ctx, source, scope.project()) {
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
        if stopped() {
            return Err(StorageError::Cancelled);
        }
        let opened = self.reader.open(&ReadRequest {
            root: &resolved.root,
            relative: &resolved.relative,
            max_bytes: max as u64,
            allow_hard_links: false,
        });
        let mut file = match opened {
            Ok(f) => f.reader,
            Err(r) => return Ok(Staging::Refused(read_refusal(r)?)),
        };
        let payload = json!({
            "digest": digest,
            "byteCount": expected_bytes,
            "requestDigest": request_digest,
            "projectId": scope.project().map(|p| p.0.as_str()),
            "source": source_json(source),
        });
        let mut reader = Stoppable {
            inner: &mut *file,
            env,
            clock: &*self.clock,
        };
        let (staging, published) = self.journal_and_publish(
            env,
            &mut reader,
            &digest,
            expected_bytes,
            reuse.as_deref(),
            payload,
        )?;
        let (operation, fence) = (staging.operation_id.clone(), staging.fence.clone());
        let published = match published {
            Ok(p) => p,
            Err(fault) => {
                // The failure stays visible on the journaled operation; if that write cannot be
                // made either, the resolution is retried later in this boot.
                self.abandon_or_defer(env, &staging, json!({"failed": format!("{fault:?}")}));
                return match fault {
                    FileFault::Source(_) if stopped() => Err(StorageError::Cancelled),
                    FileFault::Mismatch => Ok(Staging::Refused(
                        Refusal::new(DomainErrorCode::SourceChanged, "artifact_source", None)
                            .retry(RetryAdvice::RefreshAndNewRequest),
                    )),
                    FileFault::Corrupt(_) => {
                        // The reused generation is corrupt: keep it as evidence in quarantine
                        // (never claimed or unlinked). A new request then publishes a fresh
                        // generation; if the file cannot even be moved aside, retrying cannot
                        // succeed.
                        let retry =
                            match reuse.as_deref().map(|g| self.files.quarantine(&digest, g)) {
                                Some(Ok(())) => RetryAdvice::RefreshAndNewRequest,
                                _ => RetryAdvice::Never,
                            };
                        Ok(Staging::Refused(
                            Refusal::new(
                                DomainErrorCode::StorageUnavailable,
                                INTEGRITY_TARGET,
                                Id::new(digest.clone()).ok(),
                            )
                            .retry(retry),
                        ))
                    }
                    f => Err(file_failure(f)),
                };
            }
        };
        if stopped() {
            self.abandon_or_defer(env, &staging, json!({"cancelled": true}));
            return Err(StorageError::Cancelled);
        }
        Ok(Staging::Staged(StagedFile {
            operation_id: operation,
            digest,
            byte_count: expected_bytes,
            generation: published.generation,
            reused: published.reused,
            relative_path: published.relative_path,
            origin: resolved.origin,
            source: source_json(source),
            fence,
        }))
    }

    /// Journals a staging operation (`Dispatching` in its own small write) and publishes `source`
    /// as that operation's new generation of `digest` (or verifies and reuses `reuse`). The
    /// publication outcome is returned with the staging reference; on a failure the caller
    /// resolves the operation.
    fn journal_and_publish(
        &self,
        env: &Preparation<'_>,
        source: &mut dyn std::io::Read,
        digest: &str,
        expected_bytes: i64,
        reuse: Option<&str>,
        payload: Value,
    ) -> Result<
        (
            StagingRef,
            Result<crate::ports::files::PublishedBlob, FileFault>,
        ),
        StorageError,
    > {
        let args = sha256_hex(payload.to_string().as_bytes());
        let (operation, fence) = env.write(move |tx| {
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
        let staging = StagingRef {
            operation_id: operation.clone(),
            fence,
            digest: digest.to_owned(),
            byte_count: expected_bytes,
        };
        let published = self.files.publish(
            &format!("{}.part", operation.0.as_str()),
            source,
            digest,
            expected_bytes as u64,
            reuse,
            operation.0.as_str(),
        );
        Ok((staging, published))
    }

    /// A scope's storage status for the Settings/Artifacts views: a project's against its limit,
    /// the workspace's against the workspace-level limit.
    pub fn storage_status(
        &self,
        snapshot: &dyn ReadSnapshot,
        scope: &ArtifactScope,
    ) -> Result<StorageStatus, Failure> {
        let used = usage(snapshot.artifacts(), scope)?;
        let quota = setting_i64(
            snapshot.configuration(),
            scope.project(),
            ARTIFACT_QUOTA_KEY,
        )?;
        let max = setting_i64(snapshot.configuration(), None, ARTIFACT_MAX_FILE_KEY)?;
        Ok(StorageStatus {
            used_bytes: used,
            receipt_reply_bytes: snapshot.artifacts().receipt_usage(scope.project())?,
            quota_bytes: quota,
            max_file_bytes: max,
            at_limit: quota > 0 && used >= quota,
        })
    }
}

fn quota_refusal(scope: &ArtifactScope) -> Refusal {
    Refusal::new(
        DomainErrorCode::CapacityWait,
        QUOTA_TARGET,
        scope.project().map(|p| p.0.clone()),
    )
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

/// Binds a staged generation in the blob row, inside the publishing transaction. A generation
/// is bound only when the row names it already (reuse, revival of a pending removal) or names
/// none that is still in place (first publication, after removal, or healing a generation whose
/// file is gone). A claim in progress, or a reused generation that changed or was removed since
/// it was verified, refuses as `Busy` (retry publishes a fresh generation).
fn put_staged_blob(
    tx: &mut dyn WriteTransaction,
    files: &dyn ArtifactFiles,
    staged: &StagedFile,
) -> Result<String, StorageError> {
    let (digest, ours, n) = (&staged.digest, &staged.generation, staged.byte_count);
    let bind = |tx: &mut dyn WriteTransaction, generation: &str| {
        let path = files
            .managed_path(digest, generation)
            .map_err(file_failure)?;
        tx.artifacts()
            .put_blob(digest, n, generation, &path)
            .map(|b| b.generation)
    };
    match tx.artifacts().blob(digest)? {
        None if staged.reused => Err(StorageError::Busy),
        None => bind(tx, ours),
        Some(b) if b.state == BlobState::Unlinking => Err(StorageError::Busy),
        // Only a never-claimed generation is ever bound again; a claimed (retired or removed)
        // one is permanently dead.
        Some(b) if &b.generation == ours => match b.state {
            BlobState::Present | BlobState::UnlinkPending => bind(tx, ours),
            _ => Err(StorageError::Busy),
        },
        Some(b) if matches!(b.state, BlobState::Unlinked | BlobState::Retired) => {
            if staged.reused {
                Err(StorageError::Busy)
            } else {
                bind(tx, ours)
            }
        }
        Some(b) => {
            // The row names another live generation (a concurrent save published its own).
            if files
                .present(digest, &b.generation, n as u64)
                .map_err(file_failure)?
            {
                // Keep theirs (reviving a pending removal); our own generation is an orphan
                // removed once this operation is resolved.
                return bind(tx, &b.generation);
            }
            // Theirs is missing or has the wrong length: tell the two apart before healing,
            // so corrupt bytes are kept as evidence instead of being swept away later.
            match files.inspect(digest, &b.generation) {
                Ok(Some(_)) => bind(tx, &b.generation),
                Ok(None) if !staged.reused => bind(tx, ours),
                Err(FileFault::Corrupt(_)) if !staged.reused => {
                    files
                        .quarantine(digest, &b.generation)
                        .map_err(file_failure)?;
                    bind(tx, ours)
                }
                Ok(None) | Err(FileFault::Corrupt(_)) => Err(StorageError::Busy),
                Err(f) => Err(file_failure(f)),
            }
        }
    }
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
    generation: String,
) -> StoredVersion {
    StoredVersion {
        id: tx.new_id(IdKind::ArtifactVersion),
        artifact_id: artifact.clone(),
        version_no,
        blob_digest: staged.digest.clone(),
        byte_count: staged.byte_count,
        blob_generation: generation,
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

/// Bytes a scope holds.
fn usage(
    read: &dyn crate::ports::storage::ArtifactRead,
    scope: &ArtifactScope,
) -> Result<i64, StorageError> {
    match scope {
        ArtifactScope::Project(p) => read.project_usage(p),
        ArtifactScope::Workspace => read.workspace_usage(),
    }
}

fn check_quota(tx: &mut dyn WriteTransaction, scope: &ArtifactScope) -> Result<(), Failure> {
    let quota = setting_i64(tx.configuration(), scope.project(), ARTIFACT_QUOTA_KEY)?;
    if quota > 0 && usage(tx.artifacts(), scope)? > quota {
        return Err(Refusal::new(
            DomainErrorCode::CapacityWait,
            QUOTA_TARGET,
            scope.project().map(|p| p.0.clone()),
        )
        .into());
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
            tags,
            ..
        } = &req.command
        else {
            return Err(unsupported().into());
        };
        let tags = tags.as_ref().map_or(&[][..], |t| &t.0[..]);
        // Admission admits exactly one scope; the workspace only from revision 3.
        let scope = named_scope(tx.authority(), ctx, project_id.as_ref())?;
        if let ArtifactScope::Project(p) = &scope
            && !tx.artifacts().project_exists(p)?
        {
            return Err(not_found("project", &p.0).into());
        }
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
        let staged = match command.prepared::<Staging>() {
            Some(Staging::Staged(s)) => s.clone(),
            Some(Staging::Refused(r)) => return Err(r.clone().into()),
            None => {
                return Err(
                    StorageError::InvalidRecord("artifact save was not staged".into()).into(),
                );
            }
        };
        let generation = put_staged_blob(tx, &*self.files, &staged)?;
        let now = tx.now_ms();
        let artifact_id = tx.new_id(IdKind::Artifact);
        let mut artifact = StoredArtifact {
            id: artifact_id.clone(),
            origin_project_id: scope.project().cloned(),
            origin_work_id: ctx.root_id().cloned(),
            title: title.clone(),
            revision: 1,
            // A workspace artifact is owned by the principal that saved it.
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
                "sourceKind": staged.source.get("kind"),
                "repositoryId": staged.origin.repository_id.as_ref().map(Id::as_str),
                "repositoryRevision": staged.origin.repository_revision,
                "originalName": staged.origin.original_name,
            }),
        };
        tx.artifacts().insert_artifact(&artifact)?;
        tx.artifacts().set_tags(&artifact_id, tags)?;
        let version = new_version(tx, ctx, &artifact_id, 1, media_type, &staged, generation);
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
                "byteCount": staged.byte_count, "mediaType": media_type, "title": title,
                "tags": tags, "source": staged.source, "repositoryId": staged.origin.repository_id.as_ref().map(Id::as_str),
                "repositoryRevision": staged.origin.repository_revision,
            }),
        )?;
        let value = record(
            tx.artifacts(),
            &artifact,
            &version,
            command.schema_revision(),
        )?;
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
        let viewer = Viewer::resolve(tx.authority(), ctx)?;
        let mut artifact = visible_artifact(tx.artifacts(), &viewer, artifact_id)?;
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
        let mut projects: Vec<ProjectId> = artifact.scope.project().cloned().into_iter().collect();
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

    /// `artifacts.tag`: replaces the tag set of an existing artifact (ART-02), reusing the tag
    /// bounds and set normalization of saves (an empty set clears it). The caller must see the
    /// artifact and act for its owner scope: for a project artifact, a principal authorized for
    /// that project invoked from it (or from the workspace); for a workspace artifact, a member of
    /// its audience with the workspace as its authorized scope. Seeing an artifact through a
    /// share never grants it: a project it is only shared with does not own its metadata. The expected artifact revision
    /// guards concurrent edits; a changed set advances it and records `ArtifactTagsChanged`, an
    /// equal set is `NoChange`. A receipt's reply is never retagged (`Conflict`): it is an
    /// immutable correctness record, never listed, so tags would neither find it nor belong to
    /// the user's own outputs.
    fn tag(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        input: &ops::TagInput,
    ) -> Result<DomainDecision, Failure> {
        let viewer = Viewer::resolve(tx.authority(), ctx)?;
        let mut artifact = visible_artifact(tx.artifacts(), &viewer, &input.artifact_id)?;
        require_explicit(&artifact, &tx.store().host_actor)?;
        // Only the owner scope edits metadata; seeing an artifact through a share never does.
        let owns = match &artifact.scope {
            // A workspace artifact: a member of its audience (its owner, the user, the host, the
            // unconfined coordinator) acting with the workspace as its authorized scope.
            ArtifactScope::Workspace => {
                viewer.authorized == Some(AuthorizedScope::Workspace)
                    && viewer.sees_owned(artifact.owner.as_ref())
            }
            // A project artifact: a principal authorized for the owner project, invoked from it
            // or from the workspace (never from a project it is only shared with).
            ArtifactScope::Project(owner) => {
                viewer.authorized.as_ref().is_some_and(|a| a.covers(owner))
                    && ctx.project_id().is_none_or(|p| p == owner)
            }
        };
        if !owns {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "artifact",
                Some(artifact.id.clone()),
            )
            .into());
        }
        if input.expected_revision.get() != artifact.revision {
            return Err(conflict(
                RevisionDimension::Artifact,
                "artifact",
                &artifact.id,
                &input.expected_revision,
                artifact.revision,
            ));
        }
        let summary = |artifact: &StoredArtifact, changed| MutationSummary {
            id: artifact.id.clone(),
            revision: rev(artifact.revision),
            changed,
        };
        let before = tx.artifacts().tags(&artifact.id)?;
        if before == input.tags.0 {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::ArtifactsTag {
                    value: summary(&artifact, false),
                },
            });
        }
        tx.artifacts().set_tags(&artifact.id, &input.tags.0)?;
        artifact.revision += 1;
        tx.artifacts().update_artifact(&artifact)?;
        event(
            tx,
            ctx,
            &artifact,
            artifact.origin_work_id.clone(),
            None,
            "ArtifactTagsChanged",
            json!({"previous": before, "tags": input.tags.0, "revision": artifact.revision}),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::ArtifactsTag {
                value: summary(&artifact, true),
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
        let viewer = Viewer::resolve(tx.authority(), ctx)?;
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
                let mut artifact = visible_artifact(tx.artifacts(), &viewer, artifact_id)?;
                let version = live_version(tx.artifacts(), &artifact, version_id)?;
                require_explicit(&artifact, &tx.store().host_actor)?;
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
                let mut artifact = visible_artifact(tx.artifacts(), &viewer, artifact_id)?;
                require_explicit(&artifact, &tx.store().host_actor)?;
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
                let mut artifact = visible_artifact(tx.artifacts(), &viewer, artifact_id)?;
                require_explicit(&artifact, &tx.store().host_actor)?;
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
                if artifact.scope.project() == Some(project_id) {
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

    /// Global deletion needs authority over the owner scope and every project with a live
    /// reference; otherwise it is `SharedReferencesRemain` with no other scope disclosed, and
    /// nothing changes. A workspace artifact's owner scope is covered only by workspace-wide
    /// authority.
    fn authorize_reference_set(
        &self,
        tx: &mut dyn WriteTransaction,
        artifact: &StoredArtifact,
        scope: &AuthorizedScope,
    ) -> Result<(), Failure> {
        let projects = tx.artifacts().shares(&artifact.id)?;
        if scope.covers_scope(&artifact.scope) && projects.iter().all(|p| scope.covers(p)) {
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

/// Binds a list cursor to its scope and tag filter (a changed filter invalidates the cursor). A
/// workspace listing depends on who lists it, so its cursor binds the principal too.
fn list_binding(scope: &ListScope<'_>, ctx: &InvocationContext, tags: &[String]) -> String {
    let mut basis = match scope {
        ListScope::Project(p) => format!("artifacts.list|{}", p.0.as_str()),
        ListScope::Workspace => format!("artifacts.list|workspace|{}", ctx.principal().0.as_str()),
    };
    for tag in tags {
        basis.push('|');
        basis.push_str(&sha256_hex(tag.as_bytes()));
    }
    sha256_hex(basis.as_bytes())[..16].to_owned()
}

/// The scope an `artifacts.list` page covers.
enum ListScope<'a> {
    Project(&'a ProjectId),
    /// Schema revision 3: the workspace artifacts the principal belongs to the audience of.
    Workspace,
}

/// One `artifacts.list` page request, read at the negotiated `revision`.
struct Listing<'a> {
    scope: ListScope<'a>,
    tags: &'a [String],
    revision: SchemaRevision,
    limit: u16,
    cursor: Option<&'a str>,
}

impl ArtifactService {
    fn list(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        listing: Listing<'_>,
    ) -> Result<OperationResult, Failure> {
        let Listing {
            scope,
            tags,
            revision,
            limit,
            cursor,
        } = listing;
        let read = snapshot.artifacts();
        // The workspace audience: the user, the host and the unconfined coordinator list every
        // workspace artifact; any other unconfined principal lists the ones it owns.
        let owner = match &scope {
            ListScope::Project(project) => {
                require_project(ctx, project)?;
                if !read.project_exists(project)? {
                    return Err(not_found("project", &project.0).into());
                }
                None
            }
            ListScope::Workspace => {
                named_scope(snapshot.authority(), ctx, None)?;
                let viewer = Viewer::resolve(snapshot.authority(), ctx)?;
                (!viewer.sees_owned(None)).then(|| ctx.principal().clone())
            }
        };
        let binding = list_binding(&scope, ctx, tags);
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
        let page = limit.saturating_add(1).min(501);
        let mut rows = match &scope {
            ListScope::Project(project) => read.list(project, tags, after.as_ref(), page)?,
            ListScope::Workspace => {
                read.list_workspace(owner.as_ref(), tags, after.as_ref(), page)?
            }
        };
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
            items.push(record(read, a, &v, revision)?);
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
        let viewer = Viewer::resolve(snapshot.authority(), ctx)?;
        let artifact = visible_artifact(read, &viewer, artifact_id)?;
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
            .read_range(
                &v.blob_digest,
                &v.blob_generation,
                v.byte_count as u64,
                offset,
                max,
            )
            .map_err(|_| not_found("artifact_file", &v.id).into())
    }

    fn preview_of(&self, v: &StoredVersion) -> Result<(preview::Preview, ArtifactRef), Failure> {
        let pid = preview::preview_id(v.id.as_str());
        // The derived cache is disposable: a missing or unreadable entry is rebuilt from the
        // verified source, and a failed cache write only means it is rebuilt next time.
        let cached = self
            .files
            .read_derived(&pid, preview::PREVIEW_BYTES + 256)
            .ok()
            .flatten()
            .and_then(|b| preview::decode_cached(&b));
        let p = match cached {
            Some(p) => p,
            None => {
                let head = self.blob_range(v, 0, preview::PREVIEW_BYTES)?;
                let p = preview::build(&v.media_type, &head, v.byte_count as u64);
                let _ = self.files.write_derived(&pid, &preview::encode_cached(&p));
                p
            }
        };
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
                        tested_repository: tested_repository(&version, query.schema_revision())?,
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
            .present(
                &version.blob_digest,
                &version.blob_generation,
                version.byte_count as u64,
            )
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
                tested_repository: tested_repository(&version, query.schema_revision())?,
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
            .present(
                &version.blob_digest,
                &version.blob_generation,
                version.byte_count as u64,
            )
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
            ArtifactQuery::List {
                project_id, tags, ..
            } => {
                let tags = tags.as_ref().map_or(&[][..], |t| &t.0[..]);
                self.list(
                    snapshot,
                    ctx,
                    Listing {
                        // Admission admits exactly one scope; the workspace only from revision 3.
                        scope: project_id
                            .as_ref()
                            .map_or(ListScope::Workspace, ListScope::Project),
                        tags,
                        revision: query.schema_revision(),
                        limit: request.limit,
                        cursor,
                    },
                )
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
        let replay = env.read(|s| Ok(s.receipts().lookup(command.identity())?.is_some()))?;
        if replay {
            return Ok(None);
        }
        // Scope refusals are decided (and recorded) by `decide`; stage nothing for them.
        let scope = match env.read(|s| Ok(named_scope(s.authority(), ctx, project_id.as_ref())))? {
            Ok(scope) => scope,
            Err(Failure::Refused(r)) => return Ok(Some(Prepared::new(Staging::Refused(r)))),
            Err(Failure::Storage(e)) => return Err(e),
        };
        let staging = self.stage(
            env,
            ctx,
            &StageRequest {
                scope: &scope,
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
    /// unknown commit outcome is resolved the same way: the fenced resolution re-reads the
    /// durable record and completes the operation if its version did commit.
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
        let committed = match reply {
            Some(crate::protocol::CommandReply::Recorded {
                receipt, replayed, ..
            }) => !replayed && !matches!(receipt.decision, DomainDecision::Refused { .. }),
            _ => false,
        };
        if committed {
            // A concurrent save's generation was kept: ours is an orphan of a now resolved
            // operation and can be removed (no one can ever bind it).
            if !staged.reused {
                let (digest, own) = (staged.digest.clone(), staged.operation_id.clone());
                let bound = env.read(|s| {
                    Ok(s.artifacts()
                        .blob(&digest)?
                        .is_some_and(|b| b.generation == own.0.as_str()))
                });
                if matches!(bound, Ok(false)) {
                    let _ = self.files.unlink(&staged.digest, own.0.as_str());
                }
            }
            return;
        }
        {
            // An unknown commit outcome is settled the same way: the resolution re-reads the
            // durable record under the fence, completing the operation when its version did
            // commit and failing it otherwise. A resolution that cannot be written is retried
            // later in this boot.
            self.abandon_or_defer(
                env,
                &staged.staging_ref(),
                json!({"abandoned": "the save was not committed"}),
            );
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
            OperationInput::Artifacts(ops::Input::Tag(input)) => {
                self.tag(tx, ctx, input).map(|decision| Decided {
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
