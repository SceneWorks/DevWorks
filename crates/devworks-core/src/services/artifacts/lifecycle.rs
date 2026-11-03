//! Artifact lifecycle outside the seven request use cases: stream handles and their
//! re-authorized use, the separately authorized export destination write, physical collection
//! of tombstoned blobs, crash recovery of staging, and the transaction-scoped seams the
//! documents and backup items build on (share, append a version, resolve an exact version, pin
//! retained blobs).
use super::{
    ArtifactService, AuthorizedScope, BLOB_AGGREGATE, Failure, Refusal, STAGE_CAPABILITY,
    StagedFile, authorized_scope, check_quota, complete_operation, conflict, event, file_failure,
    new_version, origin_links, put_staged_blob, rev, visible, visible_artifact,
};
use crate::authority::destructive::{
    self, BlockReason, DestructiveKind, EffectPlan, EntryKind, FilesystemResolver, InputNeeded,
    IntendedScope, PreflightVerdict, ProtectedRoots,
};
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::{Fingerprint, sha256_hex};
use crate::gateway::AdmittedQuery;
use crate::operations::OperationInput;
use crate::ports::files::FileFault;
use crate::ports::storage::{
    ApplicationStore, ArtifactLink, ArtifactState, BlobRecord, BlobState, NewEvent,
    OperationRecord, ReadOptions, ReadSnapshot, StorageError, StoredArtifact, StoredVersion,
    SubjectKey, WriteOptions, WriteTransaction, read_snapshot, run_write,
};
use crate::ports::{AuthorityDenial, AuthorityPort, AuthorityRequest, Clock, IdKind};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::MutationSummary;
use crate::protocol::shared::Subject;
use crate::protocol::{
    ActorId, DomainErrorCode, EventId, Id, OperationId, OperationState, ProjectId, Revision,
    RevisionDimension,
};
use crate::registry::{ModeBinding, OperationDescriptor};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Lifetime of a stream handle (engineering default).
pub const STREAM_LIFETIME_MS: i64 = 300_000;

// ---------------------------------------------------------------------------------------------
// Stream handles

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamPurpose {
    /// Binary content of `artifacts.read`.
    Read,
    /// The byte stream of `artifacts.export` (writing it anywhere is a separate permission).
    Export,
}

/// An issued stream handle. Not bearer authority: only its principal may use it, and every use
/// re-authorizes the operation that issued it and re-checks the version is still live.
#[derive(Clone, Debug)]
pub struct StreamGrant {
    pub stream_id: Id,
    pub principal: ActorId,
    pub purpose: StreamPurpose,
    pub artifact_id: Id,
    pub version_id: Id,
    pub digest: String,
    pub byte_count: i64,
    pub media_type: String,
    pub suggested_name: String,
    pub expires_at_ms: i64,
    descriptor: &'static OperationDescriptor,
    binding: &'static ModeBinding,
    input: OperationInput,
    fingerprint: Fingerprint,
}

/// Issued stream handles (process memory: a handle never outlives the process that issued it).
#[derive(Default)]
pub struct Streams {
    grants: Mutex<HashMap<String, StreamGrant>>,
    serial: AtomicU64,
}
impl Streams {
    pub(super) fn issue(
        &self,
        clock: &dyn Clock,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        artifact: &StoredArtifact,
        version: &StoredVersion,
        purpose: StreamPurpose,
    ) -> StreamGrant {
        let now = clock.now_ms().get();
        let n = self.serial.fetch_add(1, Ordering::SeqCst);
        let basis = format!(
            "{}|{}|{}|{n}|{now}",
            query.query_id().0.as_str(),
            version.id.as_str(),
            ctx.principal().0.as_str()
        );
        let stream_id = Id::new(format!("st-{}", &sha256_hex(basis.as_bytes())[..48])).expect("id");
        let grant = StreamGrant {
            stream_id: stream_id.clone(),
            principal: ctx.principal().clone(),
            purpose,
            artifact_id: artifact.id.clone(),
            version_id: version.id.clone(),
            digest: version.blob_digest.clone(),
            byte_count: version.byte_count,
            media_type: version.media_type.clone(),
            suggested_name: suggested_name(artifact, version),
            expires_at_ms: now.saturating_add(STREAM_LIFETIME_MS),
            descriptor: query.descriptor(),
            binding: query.binding(),
            input: query.input().clone(),
            fingerprint: query.fingerprint().clone(),
        };
        let mut grants = self.grants.lock().unwrap_or_else(|p| p.into_inner());
        grants.retain(|_, g| g.expires_at_ms > now);
        grants.insert(stream_id.as_str().to_owned(), grant.clone());
        grant
    }
    fn get(&self, id: &Id) -> Option<StreamGrant> {
        self.grants
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id.as_str())
            .cloned()
    }
}

/// Active readers per blob digest. The collector never removes a blob a reader holds.
#[derive(Clone, Default)]
pub struct Readers(Arc<Mutex<HashMap<String, usize>>>);
impl Readers {
    fn acquire(&self, digest: &str) -> ReaderGuard {
        *self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(digest.to_owned())
            .or_default() += 1;
        ReaderGuard {
            readers: self.clone(),
            digest: digest.to_owned(),
        }
    }
    pub fn holds(&self, digest: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(digest)
            .is_some_and(|n| *n > 0)
    }
}
/// Releases its reader hold on drop.
pub struct ReaderGuard {
    readers: Readers,
    digest: String,
}
impl Drop for ReaderGuard {
    fn drop(&mut self) {
        let mut map = self.readers.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = map.get_mut(&self.digest) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.digest);
            }
        }
    }
}

/// An open, verified artifact byte stream holding a reader on its blob.
pub struct ArtifactStream {
    pub grant: StreamGrant,
    reader: Box<dyn Read + Send>,
    _hold: ReaderGuard,
}
impl Read for ArtifactStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buf)
    }
}

#[derive(Debug)]
pub enum StreamRefusal {
    /// Unknown handle, another principal's handle, or a version deleted since it was issued.
    NotFound,
    Expired,
    /// Current authority no longer allows the operation that issued it.
    Denied(AuthorityDenial),
    /// The stored file is missing, corrupt or unreadable.
    Unavailable(FileFault),
    Storage(StorageError),
}

/// A safe file name suggestion: the original name when the host recorded one, else the title,
/// with path separators and control characters replaced.
pub fn suggested_name(artifact: &StoredArtifact, version: &StoredVersion) -> String {
    let original = version
        .source
        .get("originalName")
        .and_then(Value::as_str)
        .or_else(|| {
            artifact
                .provenance
                .get("originalName")
                .and_then(Value::as_str)
        });
    let raw = original.unwrap_or(&artifact.title);
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .take(256)
        .collect();
    let trimmed = cleaned.trim().trim_start_matches('.').to_owned();
    if trimmed.is_empty() {
        "artifact".into()
    } else {
        trimmed
    }
}

/// Why a destination write was refused.
#[derive(Debug)]
pub enum ExportRefusal {
    Stream(StreamRefusal),
    /// Choosing an arbitrary filesystem destination is the user's own action.
    HumanOnly,
    Blocked(BlockReason),
    NeedsInput(InputNeeded),
    /// The destination exists and overwrite was not explicitly requested.
    DestinationExists,
    NotAFile,
    Write(FileFault),
    Storage(StorageError),
}

/// Removed digests and (digest, failure) pairs of one removal pass.
type Unlinked = (Vec<String>, Vec<(String, String)>);

/// Outcome of physical collection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CollectReport {
    pub unlinked: Vec<String>,
    pub deferred_pinned: Vec<String>,
    pub deferred_reading: Vec<String>,
    /// Removal failures: visible on the blob and in history, retried by the next collection.
    pub failed: Vec<(String, String)>,
}

/// Outcome of staging recovery.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Interrupted staging operations whose metadata had committed.
    pub completed: Vec<OperationId>,
    /// Interrupted staging operations whose metadata never committed.
    pub failed: Vec<OperationId>,
    pub removed_staging: Vec<String>,
    /// Digest files without metadata (never visible) that were removed.
    pub removed_orphans: Vec<String>,
    /// Interrupted physical removals that were finished.
    pub finished_unlinks: Vec<String>,
    /// Present blobs whose file is missing or has the wrong length: an integrity problem that is
    /// reported, never repaired by changing metadata.
    pub missing: Vec<String>,
}

/// Resolution of one exact version reference (citation seam).
#[derive(Clone, Debug, PartialEq)]
pub enum VersionResolution {
    Available(StoredVersion),
    /// Explicitly deleted; the tombstone keeps its provenance. Never redirected to another
    /// version.
    Unavailable {
        version: StoredVersion,
        deleted_at_ms: i64,
    },
    /// Not visible, unknown, or the reference's digest/length does not match that version.
    NotFound,
}

/// An artifact with its versions, links and shares (Artifacts view and navigation).
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactDetail {
    pub artifact: StoredArtifact,
    pub versions: Vec<StoredVersion>,
    pub links: Vec<ArtifactLink>,
    pub shares: Vec<ProjectId>,
}

fn blob_event(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    digest: &str,
    kind: &str,
    payload: Value,
) -> Result<(), StorageError> {
    let id = EventId(tx.new_id(IdKind::Event));
    let revision = tx.committed_sequence()? + 1;
    tx.history().append(NewEvent {
        id,
        actor: actor.clone(),
        project_id: None,
        root_id: None,
        operation_id: None,
        aggregate_kind: BLOB_AGGREGATE.into(),
        aggregate_id: digest.to_owned(),
        aggregate_revision: revision,
        kind: kind.into(),
        payload,
    })?;
    Ok(())
}

fn unresolved_staging(s: &dyn ReadSnapshot) -> Result<Vec<OperationRecord>, StorageError> {
    Ok(s.operations()
        .unresolved(500)?
        .into_iter()
        .filter(|o| o.capability == STAGE_CAPABILITY)
        .collect())
}

impl ArtifactService {
    /// Opens a stream handle for its principal after re-authorizing the operation that issued
    /// it and re-checking that the version is still live and visible. The returned stream holds
    /// a reader on the blob until dropped.
    pub fn open_stream(
        &self,
        authority: &dyn AuthorityPort,
        store: &dyn ApplicationStore,
        ctx: &InvocationContext,
        stream_id: &Id,
    ) -> Result<ArtifactStream, StreamRefusal> {
        let grant = self.streams.get(stream_id).ok_or(StreamRefusal::NotFound)?;
        if &grant.principal != ctx.principal() {
            return Err(StreamRefusal::NotFound);
        }
        if self.clock.now_ms().get() >= grant.expires_at_ms {
            return Err(StreamRefusal::Expired);
        }
        authority
            .authorize(
                ctx,
                &AuthorityRequest {
                    descriptor: grant.descriptor,
                    binding: grant.binding,
                    effect: grant.binding.effect,
                    action: None,
                    input: &grant.input,
                    fingerprint: &grant.fingerprint,
                },
            )
            .map_err(StreamRefusal::Denied)?;
        // Hold the reader before the liveness check: a deletion that commits after this check
        // finds the reader and defers removal; one that committed before fails the check.
        let hold = self.readers.acquire(&grant.digest);
        let live = read_snapshot(store, &ReadOptions::default(), |s| {
            let read = s.artifacts();
            let Some(a) = read.artifact(&grant.artifact_id)? else {
                return Ok(false);
            };
            if a.state != ArtifactState::Live || !visible(read, ctx, &a)? {
                return Ok(false);
            }
            Ok(read
                .version(&grant.version_id)?
                .is_some_and(|v| v.live() && v.blob_digest == grant.digest))
        })
        .map_err(StreamRefusal::Storage)?;
        if !live {
            return Err(StreamRefusal::NotFound);
        }
        let reader = self
            .files
            .open(&grant.digest, grant.byte_count as u64)
            .map_err(StreamRefusal::Unavailable)?;
        Ok(ArtifactStream {
            grant,
            reader,
            _hold: hold,
        })
    }

    /// Writes an export stream to a filesystem destination the user chose. Separately authorized
    /// from the stream itself: the stream is re-authorized as in [`Self::open_stream`], the
    /// destination write is the user's own trusted local action (never an agent's), and the
    /// destination passes the destructive/target preflight (an overwrite is a `Truncate` of the
    /// destination). An existing destination is replaced only with explicit `overwrite`.
    #[allow(clippy::too_many_arguments)]
    pub fn write_export(
        &self,
        authority: &dyn AuthorityPort,
        store: &dyn ApplicationStore,
        resolver: &dyn FilesystemResolver,
        app_data_root: Option<&str>,
        ctx: &InvocationContext,
        stream_id: &Id,
        destination: &str,
        overwrite: bool,
    ) -> Result<(), ExportRefusal> {
        let stream = self
            .open_stream(authority, store, ctx, stream_id)
            .map_err(ExportRefusal::Stream)?;
        if stream.grant.purpose != StreamPurpose::Export {
            return Err(ExportRefusal::Stream(StreamRefusal::NotFound));
        }
        if ctx.principal_kind() != PrincipalKind::Human {
            return Err(ExportRefusal::HumanOnly);
        }
        let repositories = read_snapshot(store, &ReadOptions::default(), |s| {
            s.authority().registered_repositories()
        })
        .map_err(ExportRefusal::Storage)?;
        let roots = ProtectedRoots::new(repositories, Vec::new(), app_data_root.map(str::to_owned));
        let plan = EffectPlan {
            kind: DestructiveKind::Truncate,
            targets: vec![destination.to_owned()],
            working_directory: None,
            scope: IntendedScope::Ordinary,
        };
        let decision = destructive::preflight(resolver, &roots, &plan);
        let target = match (&decision.verdict, decision.allowed_targets()) {
            (PreflightVerdict::Allowed, Some([t])) => t.clone(),
            (PreflightVerdict::Blocked(r), _) => return Err(ExportRefusal::Blocked(r.clone())),
            (PreflightVerdict::NeedsInput(n), _) => return Err(ExportRefusal::NeedsInput(*n)),
            _ => return Err(ExportRefusal::NeedsInput(InputNeeded::UnresolvedTarget)),
        };
        match target.entry_kind() {
            EntryKind::Missing => {}
            EntryKind::File if overwrite => {}
            EntryKind::File => return Err(ExportRefusal::DestinationExists),
            EntryKind::Symlink | EntryKind::ReparsePoint => {
                return Err(ExportRefusal::Blocked(BlockReason::LinkEscape));
            }
            EntryKind::Directory | EntryKind::Other => return Err(ExportRefusal::NotAFile),
        }
        // A new file inside the application data root is still an application-data change.
        if let (Some(app), Some(parent)) = (app_data_root, target.path().parent())
            && let Ok(app) = resolver.resolve(app, None)
        {
            let app = app.target.unwrap_or(app.entry);
            if parent.starts_with(&app.path) {
                return Err(ExportRefusal::Blocked(BlockReason::ApplicationData));
            }
        }
        self.files
            .export(
                &stream.grant.digest,
                stream.grant.byte_count as u64,
                Path::new(target.path()),
                overwrite,
            )
            .map_err(|f| match f {
                FileFault::Exists => ExportRefusal::DestinationExists,
                f => ExportRefusal::Write(f),
            })?;
        let (ctx, grant, path) = (
            ctx.clone(),
            stream.grant.clone(),
            target.path().to_string_lossy().into_owned(),
        );
        run_write(store, WriteOptions::default(), move |tx| {
            if let Some(a) = tx.artifacts().artifact(&grant.artifact_id)? {
                event(
                    tx,
                    &ctx,
                    &a,
                    None,
                    None,
                    "ArtifactExported",
                    json!({"versionId": grant.version_id.as_str(), "destination": path, "overwrite": overwrite}),
                )?;
            }
            Ok(())
        })
        .map_err(ExportRefusal::Storage)?;
        Ok(())
    }

    /// Physically removes blobs no live version names, skipping those a backup pins or a reader
    /// holds. Claim (`Unlinking`) commits before the file is removed and the outcome after, so a
    /// crash in between is finished by [`Self::recover`]; a failure is visible and retried.
    pub fn collect(&self, store: &dyn ApplicationStore) -> Result<CollectReport, StorageError> {
        let pending = read_snapshot(store, &ReadOptions::default(), |s| {
            s.artifacts().blobs_in(BlobState::UnlinkPending, None, 500)
        })?;
        let readers = self.readers.clone();
        let (claimed, mut report) = run_write(store, WriteOptions::default(), move |tx| {
            let mut report = CollectReport::default();
            let mut claimed = Vec::new();
            for b in pending {
                let current = tx.artifacts().blob(&b.digest)?;
                if current.as_ref().map(|c| c.state) != Some(BlobState::UnlinkPending)
                    || tx.artifacts().live_references(&b.digest)? > 0
                {
                    continue;
                }
                if tx.artifacts().pinned(&b.digest)? {
                    report.deferred_pinned.push(b.digest);
                } else if readers.holds(&b.digest) {
                    report.deferred_reading.push(b.digest);
                } else {
                    tx.artifacts().set_blob_state(
                        &b.digest,
                        BlobState::UnlinkPending,
                        BlobState::Unlinking,
                        None,
                    )?;
                    claimed.push(b.digest);
                }
            }
            Ok((claimed, report))
        })?;
        let (done, failed) = self.unlink_claimed(store, claimed)?;
        report.unlinked = done;
        report.failed = failed;
        Ok(report)
    }

    fn unlink_claimed(
        &self,
        store: &dyn ApplicationStore,
        claimed: Vec<String>,
    ) -> Result<Unlinked, StorageError> {
        let mut done = Vec::new();
        let mut failed = Vec::new();
        for digest in claimed {
            match self.files.unlink(&digest) {
                Ok(()) => done.push(digest),
                Err(f) => failed.push((digest, format!("{f:?}"))),
            }
        }
        let host = store.info().host_actor;
        let (d, f) = (done.clone(), failed.clone());
        run_write(store, WriteOptions::default(), move |tx| {
            for digest in &d {
                tx.artifacts().set_blob_state(
                    digest,
                    BlobState::Unlinking,
                    BlobState::Unlinked,
                    None,
                )?;
                blob_event(tx, &host, digest, "ArtifactBlobUnlinked", json!({}))?;
            }
            for (digest, error) in &f {
                tx.artifacts().set_blob_state(
                    digest,
                    BlobState::Unlinking,
                    BlobState::UnlinkPending,
                    Some(error),
                )?;
                blob_event(
                    tx,
                    &host,
                    digest,
                    "ArtifactBlobUnlinkFailed",
                    json!({"error": error}),
                )?;
            }
            Ok(())
        })?;
        Ok((done, failed))
    }

    /// Reconciles staging after a restart: interrupted staging operations of earlier boots are
    /// completed (metadata committed) or failed (it never did); staged files and digest files
    /// without metadata that no in-flight operation of this boot names are removed; interrupted
    /// physical removals are finished; Present blobs with a missing file are reported.
    pub fn recover(&self, store: &dyn ApplicationStore) -> Result<RecoveryReport, StorageError> {
        let boot = store.info().boot_id;
        let ops = read_snapshot(store, &ReadOptions::default(), unresolved_staging)?;
        let (stale, in_flight): (Vec<_>, Vec<_>) = ops.into_iter().partition(|o| {
            o.latest_attempt()
                .is_none_or(|a| Some(&a.boot_id) != boot.as_ref())
        });
        let keep_staging: HashSet<String> = in_flight
            .iter()
            .map(|o| format!("{}.part", o.id.0.as_str()))
            .collect();
        let keep_digests: HashSet<String> = in_flight
            .iter()
            .filter_map(|o| {
                o.payload
                    .get("digest")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect();
        let mut report = RecoveryReport::default();
        let resolved = run_write(store, WriteOptions::default(), move |tx| {
            let mut out = Vec::new();
            for op in stale {
                let published = tx.artifacts().version_by_operation(&op.id)?.is_some();
                let to = if published && op.state != OperationState::Preflighted {
                    OperationState::ObservedComplete
                } else {
                    OperationState::Failed
                };
                let fence = op.latest_attempt().map(|a| a.fence());
                tx.operations().transition(
                    &op.id,
                    op.revision,
                    fence.as_ref(),
                    to,
                    Some(json!({"recovered": if published { "metadata committed" } else { "metadata never committed" }})),
                )?;
                out.push((op.id, published));
            }
            Ok(out)
        })?;
        for (id, published) in resolved {
            if published {
                report.completed.push(id);
            } else {
                report.failed.push(id);
            }
        }
        for name in self.files.staged().map_err(file_failure)? {
            if !keep_staging.contains(&name) {
                self.files.remove_staged(&name).map_err(file_failure)?;
                report.removed_staging.push(name);
            }
        }
        let on_disk = self.files.stored_digests().map_err(file_failure)?;
        let orphans = read_snapshot(store, &ReadOptions::default(), |s| {
            let mut out = Vec::new();
            for d in on_disk {
                if !keep_digests.contains(&d) && s.artifacts().blob(&d)?.is_none() {
                    out.push(d);
                }
            }
            Ok(out)
        })?;
        for d in orphans {
            self.files.unlink(&d).map_err(file_failure)?;
            report.removed_orphans.push(d);
        }
        let interrupted = read_snapshot(store, &ReadOptions::default(), |s| {
            s.artifacts().blobs_in(BlobState::Unlinking, None, 500)
        })?;
        let (done, _) =
            self.unlink_claimed(store, interrupted.into_iter().map(|b| b.digest).collect())?;
        report.finished_unlinks = done;
        let mut after: Option<String> = None;
        loop {
            let page = read_snapshot(store, &ReadOptions::default(), |s| {
                s.artifacts()
                    .blobs_in(BlobState::Present, after.as_deref(), 500)
            })?;
            for b in &page {
                if !self
                    .files
                    .present(&b.digest, b.byte_count as u64)
                    .unwrap_or(false)
                {
                    report.missing.push(b.digest.clone());
                }
            }
            match page.last() {
                Some(last) if page.len() == 500 => after = Some(last.digest.clone()),
                _ => break,
            }
        }
        Ok(report)
    }

    /// Removes every derived (rebuildable) payload; retained blobs are untouched.
    pub fn collect_derived(&self) -> Result<u64, FileFault> {
        self.files.collect_derived()
    }

    /// Appends a new immutable version to an existing artifact from a file staged with
    /// [`ArtifactService::stage`] (the documents replacement path). Earlier versions are kept
    /// and keep resolving exactly; the origin links of the artifact are carried to the new
    /// version, so reuse never loses them. Runs inside the caller's unit of work.
    pub fn publish_version(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        artifact_id: &Id,
        expected_revision: &Revision,
        media_type: &str,
        staged: &StagedFile,
    ) -> Result<StoredVersion, Failure> {
        let mut artifact = visible_artifact(tx.artifacts(), ctx, artifact_id)?;
        if expected_revision.get() != artifact.revision {
            return Err(conflict(
                RevisionDimension::Artifact,
                "artifact",
                artifact_id,
                expected_revision,
                artifact.revision,
            ));
        }
        put_staged_blob(tx, &*self.files, staged)?;
        let next = tx
            .artifacts()
            .versions(artifact_id)?
            .iter()
            .map(|v| v.version_no)
            .max()
            .unwrap_or(0)
            + 1;
        let version = new_version(tx, ctx, artifact_id, next, media_type, staged);
        tx.artifacts().insert_version(&version)?;
        let mut links = origin_links(ctx, &version.id);
        if let Some(root) = &artifact.origin_work_id {
            links.push(ArtifactLink {
                version_id: version.id.clone(),
                subject: SubjectKey::root(root),
                purpose: crate::ports::storage::LinkPurpose::Origin,
            });
        }
        for link in links {
            tx.artifacts().add_link(&link)?;
        }
        artifact.current_version_id = Some(version.id.clone());
        artifact.revision += 1;
        tx.artifacts().update_artifact(&artifact)?;
        check_quota(tx, &artifact.current_project_id)?;
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
                "versionId": version.id.as_str(), "versionNo": next, "digest": staged.digest,
                "byteCount": staged.byte_count, "mediaType": media_type, "source": staged.source,
            }),
        )?;
        Ok(version)
    }

    /// Shares an artifact with another project (the documents share path). Both the source and
    /// destination project must be inside the principal's authorized scope.
    pub fn share(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        artifact_id: &Id,
        target: &ProjectId,
        expected_scope_revision: &Revision,
    ) -> Result<MutationSummary, Failure> {
        let mut artifact = visible_artifact(tx.artifacts(), ctx, artifact_id)?;
        if expected_scope_revision.get() != artifact.scope_revision {
            return Err(conflict(
                RevisionDimension::Scope,
                "artifact_scope",
                artifact_id,
                expected_scope_revision,
                artifact.scope_revision,
            ));
        }
        let scope = authorized_scope(tx.authority(), ctx)?;
        if scope != AuthorizedScope::Workspace {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "project",
                Some(target.0.clone()),
            )
            .into());
        }
        if !tx.artifacts().project_exists(target)? {
            return Err(
                Refusal::new(DomainErrorCode::NotFound, "project", Some(target.0.clone())).into(),
            );
        }
        if target == &artifact.current_project_id
            || !tx.artifacts().add_share(artifact_id, target)?
        {
            return Ok(MutationSummary {
                id: artifact_id.clone(),
                revision: rev(artifact.scope_revision),
                changed: false,
            });
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
            json!({"projectId": target.0.as_str(), "scopeRevision": artifact.scope_revision}),
        )?;
        Ok(MutationSummary {
            id: artifact_id.clone(),
            revision: rev(artifact.scope_revision),
            changed: true,
        })
    }

    /// Resolves an exact version reference (for example an old citation). A replaced version
    /// still resolves to itself; a deleted one is `Unavailable` with its tombstone; a reference
    /// whose digest or length differs from the version it names is `NotFound`.
    pub fn resolve_version(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        reference: &ArtifactRef,
    ) -> Result<VersionResolution, StorageError> {
        let read = snapshot.artifacts();
        let Some(a) = read.artifact(&reference.artifact_id)? else {
            return Ok(VersionResolution::NotFound);
        };
        if !visible(read, ctx, &a)? {
            return Ok(VersionResolution::NotFound);
        }
        let Some(v) = read.version(&reference.version_id)? else {
            return Ok(VersionResolution::NotFound);
        };
        if v.artifact_id != a.id
            || v.blob_digest != String::from(reference.sha256.clone())
            || v.byte_count != reference.byte_length.get()
        {
            return Ok(VersionResolution::NotFound);
        }
        Ok(match v.deleted_at_ms {
            None => VersionResolution::Available(v),
            Some(at) => VersionResolution::Unavailable {
                version: v,
                deleted_at_ms: at,
            },
        })
    }

    /// An artifact's versions (tombstones included), links and shares, if visible.
    pub fn detail(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        artifact_id: &Id,
    ) -> Result<Option<ArtifactDetail>, StorageError> {
        let read = snapshot.artifacts();
        let Some(a) = read.artifact(artifact_id)? else {
            return Ok(None);
        };
        if !visible(read, ctx, &a)? {
            return Ok(None);
        }
        Ok(Some(ArtifactDetail {
            versions: read.versions(artifact_id)?,
            links: read.links(artifact_id)?,
            shares: read.shares(artifact_id)?,
            artifact: a,
        }))
    }

    /// Navigation from work to its artifacts: visible links pointing at `subject`.
    pub fn linked(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        subject: &Subject,
    ) -> Result<Vec<(StoredArtifact, ArtifactLink)>, StorageError> {
        let read = snapshot.artifacts();
        let mut out = Vec::new();
        for link in read.linked_to(&SubjectKey::of(subject))? {
            let Some(v) = read.version(&link.version_id)? else {
                continue;
            };
            if let Some(a) = read.artifact(&v.artifact_id)?
                && visible(read, ctx, &a)?
            {
                out.push((a, link));
            }
        }
        Ok(out)
    }
}

/// Reserves the backup deletion barrier: pins every retained blob (not yet unlinked) for
/// `holder` and returns the retained-file manifest (digest, managed path, length). Pinned blobs
/// are never physically removed until [`release_backup`].
pub fn pin_for_backup(
    tx: &mut dyn WriteTransaction,
    holder: &Id,
) -> Result<Vec<BlobRecord>, StorageError> {
    let mut manifest = Vec::new();
    for state in [BlobState::Present, BlobState::UnlinkPending] {
        let mut after: Option<String> = None;
        loop {
            let page = tx.artifacts().blobs_in(state, after.as_deref(), 500)?;
            for b in &page {
                tx.artifacts().pin(holder, &b.digest)?;
            }
            let more = page.len() == 500;
            after = page.last().map(|b| b.digest.clone());
            manifest.extend(page);
            if !more {
                break;
            }
        }
    }
    Ok(manifest)
}

/// Releases a backup's deletion barrier.
pub fn release_backup(tx: &mut dyn WriteTransaction, holder: &Id) -> Result<i64, StorageError> {
    tx.artifacts().release_pins(holder)
}
