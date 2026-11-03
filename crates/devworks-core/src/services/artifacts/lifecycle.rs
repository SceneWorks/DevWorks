//! Artifact lifecycle outside the seven request use cases: stream handles and their
//! re-authorized use, the separately authorized export destination write, physical collection
//! of tombstoned blobs, crash recovery of staging, and the transaction-scoped seams the
//! documents and backup items build on (share, append a version, resolve an exact version, pin
//! retained blobs).
use super::{
    ArtifactService, AuthorizedScope, BLOB_AGGREGATE, Failure, Refusal, StagedFile, StagingRef,
    authorized_scope, check_quota, complete_operation, conflict, event, file_failure, new_version,
    origin_links, put_staged_blob, rev, visible, visible_artifact,
};
use crate::authority::destructive::{
    self, BlockReason, DestructiveKind, EffectPlan, EntryKind, FilesystemResolver, InputNeeded,
    IntendedScope, PreflightVerdict, ProtectedRoots,
};
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::{Fingerprint, sha256_hex};
use crate::gateway::AdmittedQuery;
use crate::operations::OperationInput;
use crate::ports::Preparation;
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
use std::collections::HashMap;
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
    /// The blob generation the stream reads (the one the row named when it was opened).
    pub generation: String,
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
    /// Unknown handle, another principal's handle, a version deleted since it was issued, or a
    /// stored file that is missing, corrupt or unreadable when opened (the retained
    /// GetAttachment behavior reports an unreadable stored file as not found).
    NotFound,
    Expired,
    /// Current authority no longer allows the operation that issued it.
    Denied(AuthorityDenial),
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
    /// Present blobs whose file is missing: an integrity problem that is reported, never
    /// repaired by changing metadata.
    pub missing: Vec<String>,
    /// Corrupt digest files (wrong digest, even with the right length) moved into quarantine.
    pub quarantined: Vec<String>,
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

/// Resolves one staging operation of an earlier boot inside its own write: re-read under the
/// transaction, completed when its version committed, otherwise failed. `None` when it changed
/// since it was listed.
fn resolve_stale(
    tx: &mut dyn WriteTransaction,
    listed: OperationRecord,
) -> Result<Option<(OperationId, bool)>, StorageError> {
    let Some(op) = tx.operations().operation(&listed.id)? else {
        return Ok(None);
    };
    if crate::ports::storage::is_terminal(op.state) || op.revision != listed.revision {
        return Ok(None);
    }
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
    Ok(Some((op.id, published)))
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
                return Ok(None);
            };
            if a.state != ArtifactState::Live || !visible(read, ctx, &a)? {
                return Ok(None);
            }
            Ok(read
                .version(&grant.version_id)?
                .filter(|v| v.live() && v.blob_digest == grant.digest)
                .map(|v| v.blob_generation))
        })
        .map_err(StreamRefusal::Storage)?;
        let Some(generation) = live else {
            return Err(StreamRefusal::NotFound);
        };
        let reader = self
            .files
            .open(&grant.digest, &generation, grant.byte_count as u64)
            .map_err(|_| StreamRefusal::NotFound)?;
        Ok(ArtifactStream {
            grant,
            generation,
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
                &stream.generation,
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

    /// Physically removes blob generations no live version names, skipping those a backup pins
    /// or a reader holds, then finishes any claim another holder left. Each removal claims its
    /// exact generation in the store first (`Unlinking`, owned by this pass), removes only that
    /// generation's file, and records the outcome under the same ownership; a failure is
    /// visible and retried. It first retries staging resolutions deferred earlier in this boot.
    pub fn collect(&self, store: &dyn ApplicationStore) -> Result<CollectReport, StorageError> {
        self.retry_pending(&Preparation::new(store, None, None));
        let owner = self.claimant(store);
        let mut report = CollectReport::default();
        let mut after: Option<String> = None;
        loop {
            let page = read_snapshot(store, &ReadOptions::default(), |s| {
                s.artifacts()
                    .blobs_in(BlobState::UnlinkPending, after.as_deref(), 500)
            })?;
            let more = page.len() == 500;
            after = page.last().map(|b| b.digest.clone());
            let (readers, me) = (self.readers.clone(), owner.clone());
            let (claimed, deferred_pinned, deferred_reading) =
                run_write(store, WriteOptions::default(), move |tx| {
                    let (mut claimed, mut pinned, mut reading) =
                        (Vec::new(), Vec::new(), Vec::new());
                    for b in page {
                        let Some(current) = tx.artifacts().blob(&b.digest)? else {
                            continue;
                        };
                        if current.state != BlobState::UnlinkPending
                            || tx.artifacts().live_references(&b.digest)? > 0
                        {
                            continue;
                        }
                        if tx.artifacts().pinned(&b.digest)? {
                            pinned.push(b.digest);
                        } else if readers.holds(&b.digest) {
                            reading.push(b.digest);
                        } else if tx
                            .artifacts()
                            .claim_blob(&b.digest, &current.generation, &me)?
                        {
                            claimed.push((b.digest, current.generation));
                        }
                    }
                    Ok((claimed, pinned, reading))
                })?;
            report.deferred_pinned.extend(deferred_pinned);
            report.deferred_reading.extend(deferred_reading);
            let (done, failed) = self.finish_claims(store, &owner, claimed)?;
            report.unlinked.extend(done);
            report.failed.extend(failed);
            if !more {
                break;
            }
        }
        report.unlinked.extend(self.adopt_claims(store, &owner)?);
        Ok(report)
    }

    /// A claim owner identity: this boot plus a fresh nonce, so claims of earlier boots and of
    /// other passes are recognizable.
    fn claimant(&self, store: &dyn ApplicationStore) -> String {
        let boot = store.info().boot_id.map(|b| b.as_str().to_owned());
        format!(
            "{}.{}",
            boot.unwrap_or_else(|| "readonly".into()),
            self.ids.next_id(IdKind::Correlation).as_str()
        )
    }

    /// Removes the files of claims `owner` holds (exact generations) and records each outcome
    /// under that ownership: `Unlinked`, or back to `UnlinkPending` with the visible error. A
    /// claim adopted by another holder meanwhile is left to it (the removal of the exact
    /// generation was idempotent).
    fn finish_claims(
        &self,
        store: &dyn ApplicationStore,
        owner: &str,
        claimed: Vec<(String, String)>,
    ) -> Result<Unlinked, StorageError> {
        let mut outcomes = Vec::new();
        for (digest, generation) in claimed {
            let error = self
                .files
                .unlink(&digest, &generation)
                .err()
                .map(|f| format!("{f:?}"));
            outcomes.push((digest, generation, error));
        }
        let host = store.info().host_actor;
        let me = owner.to_owned();
        run_write(store, WriteOptions::default(), move |tx| {
            let (mut done, mut failed) = (Vec::new(), Vec::new());
            for (digest, generation, error) in outcomes {
                if !tx
                    .artifacts()
                    .finish_claim(&digest, &generation, &me, error.as_deref())?
                {
                    continue;
                }
                match error {
                    None => {
                        blob_event(
                            tx,
                            &host,
                            &digest,
                            "ArtifactBlobUnlinked",
                            json!({"generation": generation}),
                        )?;
                        done.push(digest);
                    }
                    Some(error) => {
                        blob_event(
                            tx,
                            &host,
                            &digest,
                            "ArtifactBlobUnlinkFailed",
                            json!({"generation": generation, "error": error}),
                        )?;
                        failed.push((digest, error));
                    }
                }
            }
            Ok((done, failed))
        })
    }

    /// Finishes every `Unlinking` claim another holder left (an earlier boot that crashed
    /// between claim and outcome, or a pass of this boot that could not record its outcome).
    /// Adoption is a compare-and-set on (generation, owner) as listed, so a claim that changed
    /// since — finished, or replaced by a newer generation's claim — is never taken over.
    /// Returns the digests whose removal this pass finished.
    fn adopt_claims(
        &self,
        store: &dyn ApplicationStore,
        owner: &str,
    ) -> Result<Vec<String>, StorageError> {
        let mut finished = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = read_snapshot(store, &ReadOptions::default(), |s| {
                s.artifacts()
                    .blobs_in(BlobState::Unlinking, after.as_deref(), 500)
            })?;
            let more = page.len() == 500;
            after = page.last().map(|b| b.digest.clone());
            let me = owner.to_owned();
            let adopted = run_write(store, WriteOptions::default(), move |tx| {
                let mut adopted = Vec::new();
                for b in page {
                    let Some(from) = b.claim_owner.as_deref() else {
                        continue;
                    };
                    if from != me
                        && tx
                            .artifacts()
                            .adopt_claim(&b.digest, &b.generation, from, &me)?
                    {
                        adopted.push((b.digest, b.generation));
                    }
                }
                Ok(adopted)
            })?;
            let (done, _) = self.finish_claims(store, owner, adopted)?;
            finished.extend(done);
            if !more {
                break;
            }
        }
        Ok(finished)
    }

    /// Reconciles staging and managed files. Safe while commands are admitted, and safe to run
    /// concurrently with itself: it resolves only staging operations of earlier boots (and
    /// resolutions this service deferred), and it removes only generations no one can bind.
    ///
    /// * Interrupted staging operations of earlier boots are completed (metadata committed) or
    ///   failed (it never did); a failed operation's own generation is removed.
    /// * A staged file is removed only when its operation is resolved (or absent: the journal
    ///   entry always commits before its file exists).
    /// * A generation file is an orphan when its blob row does not name it and its staging
    ///   operation is resolved (or absent): nothing can ever bind it again, so it is removed.
    /// * Claims other holders left are adopted and finished.
    /// * Present generations are verified: a missing file is reported; a corrupt one (even with
    ///   the right length) is quarantined and reported. Metadata is never changed to hide either.
    pub fn recover(&self, store: &dyn ApplicationStore) -> Result<RecoveryReport, StorageError> {
        let env = Preparation::new(store, None, None);
        self.retry_pending(&env);
        let owner = self.claimant(store);
        let boot = store.info().boot_id;
        let mut report = RecoveryReport::default();
        let mut after: Option<OperationId> = None;
        loop {
            let page = read_snapshot(store, &ReadOptions::default(), |s| {
                let ids = s.artifacts().unresolved_staging(after.as_ref(), 500)?;
                let mut records = Vec::with_capacity(ids.len());
                for id in &ids {
                    if let Some(r) = s.operations().operation(id)? {
                        records.push(r);
                    }
                }
                Ok((ids, records))
            })?;
            let (ids, records) = page;
            for op in records.into_iter().filter(|o| {
                o.latest_attempt()
                    .is_none_or(|a| Some(&a.boot_id) != boot.as_ref())
            }) {
                // A failed operation's own generation is removed by the sweep below: from the
                // moment it is resolved nothing can bind it.
                let resolved = run_write(store, WriteOptions::default(), move |tx| {
                    resolve_stale(tx, op)
                })?;
                match resolved {
                    Some((id, true)) => report.completed.push(id),
                    Some((id, false)) => report.failed.push(id),
                    None => {}
                }
            }
            match ids.last() {
                Some(last) if ids.len() == 500 => after = Some(last.clone()),
                _ => break,
            }
        }
        for name in self.files.staged().map_err(file_failure)? {
            let op = name
                .strip_suffix(".part")
                .and_then(|id| Id::new(id).ok())
                .map(OperationId);
            let resolved = match &op {
                None => false,
                Some(op) => read_snapshot(store, &ReadOptions::default(), |s| {
                    Ok(s.operations()
                        .operation(op)?
                        .is_none_or(|o| crate::ports::storage::is_terminal(o.state)))
                })?,
            };
            if resolved {
                self.files.remove_staged(&name).map_err(file_failure)?;
                report.removed_staging.push(name);
            }
        }
        for (digest, generation) in self.files.stored_files().map_err(file_failure)? {
            if self.remove_dead_generation(store, &digest, &generation)?
                && !report.removed_orphans.contains(&digest)
            {
                report.removed_orphans.push(digest);
            }
        }
        report.finished_unlinks = self.adopt_claims(store, &owner)?;
        let mut after: Option<String> = None;
        loop {
            let page = read_snapshot(store, &ReadOptions::default(), |s| {
                s.artifacts()
                    .blobs_in(BlobState::Present, after.as_deref(), 500)
            })?;
            for b in &page {
                match self.files.inspect(&b.digest, &b.generation) {
                    Ok(Some(len)) if len == b.byte_count as u64 => {}
                    Ok(None) => report.missing.push(b.digest.clone()),
                    Ok(Some(_)) | Err(FileFault::Corrupt(_)) => {
                        self.files
                            .quarantine(&b.digest, &b.generation)
                            .map_err(file_failure)?;
                        report.quarantined.push(b.digest.clone());
                    }
                    Err(_) => report.missing.push(b.digest.clone()),
                }
            }
            match page.last() {
                Some(last) if page.len() == 500 => after = Some(last.digest.clone()),
                _ => break,
            }
        }
        Ok(report)
    }

    /// Removes `generation`'s file of `digest` when nothing can ever bind it: the blob row does
    /// not name it (or names it only as already removed) and its staging operation — whose ID
    /// the generation is — is resolved or absent. Both facts come from one snapshot, and an
    /// operation binds its generation only in the transaction that resolves it, so the answer
    /// cannot change afterwards. Called for files listed on disk; returns whether it was removed.
    fn remove_dead_generation(
        &self,
        store: &dyn ApplicationStore,
        digest: &str,
        generation: &str,
    ) -> Result<bool, StorageError> {
        let dead = read_snapshot(store, &ReadOptions::default(), |s| {
            let named = s
                .artifacts()
                .blob(digest)?
                .is_some_and(|b| b.generation == generation && b.state != BlobState::Unlinked);
            if named {
                return Ok(false);
            }
            let Ok(id) = Id::new(generation) else {
                return Ok(true);
            };
            Ok(s.operations()
                .operation(&OperationId(id))?
                .is_none_or(|o| crate::ports::storage::is_terminal(o.state)))
        })?;
        if !dead {
            return Ok(false);
        }
        self.files
            .unlink(digest, generation)
            .map_err(file_failure)?;
        Ok(true)
    }

    /// Resolves a staging operation whose metadata was not committed, under the staging
    /// attempt's journal fence and in one write: a still-`Dispatching` operation becomes
    /// `ObservedComplete` if its version did commit (an unknown commit outcome that succeeded)
    /// and `Failed` otherwise. Only after that commits is its own generation removed — the
    /// operation is resolved, so nothing can bind that generation any more — together with its
    /// staged file. Returns `false` when there was nothing to resolve (already resolved, for
    /// example by recovery, or never journaled under this fence); calling it again is a no-op.
    pub fn abandon(
        &self,
        env: &Preparation<'_>,
        staged: &StagingRef,
        reason: Value,
    ) -> Result<bool, StorageError> {
        let s = staged.clone();
        let resolved = env.write(move |tx| {
            let Some(rec) = tx.operations().operation(&s.operation_id)? else {
                return Ok(None);
            };
            if rec.state != OperationState::Dispatching
                || rec.latest_attempt().map(|a| a.fence()).as_ref() != Some(&s.fence)
            {
                return Ok(None);
            }
            let committed = tx
                .artifacts()
                .version_by_operation(&s.operation_id)?
                .is_some();
            let (to, observation) = if committed {
                (
                    OperationState::ObservedComplete,
                    json!({"reconciled": "the save had committed"}),
                )
            } else {
                (OperationState::Failed, reason)
            };
            tx.operations().transition(
                &s.operation_id,
                rec.revision,
                Some(&s.fence),
                to,
                Some(observation),
            )?;
            Ok(Some(committed))
        })?;
        let Some(committed) = resolved else {
            return Ok(false);
        };
        let own = staged.operation_id.0.as_str();
        self.files
            .remove_staged(&format!("{own}.part"))
            .map_err(file_failure)?;
        let bound = env.read(|s| {
            Ok(s.artifacts()
                .blob(&staged.digest)?
                .is_some_and(|b| b.generation == own))
        })?;
        if !committed && !bound {
            self.files
                .unlink(&staged.digest, own)
                .map_err(file_failure)?;
        }
        Ok(true)
    }

    /// Resolves `staged` now, or remembers it for the next [`Self::recover`] /
    /// [`Self::collect`] in this boot when the resolution itself cannot be written.
    pub(super) fn abandon_or_defer(
        &self,
        env: &Preparation<'_>,
        staged: &StagingRef,
        reason: Value,
    ) {
        if self.abandon(env, staged, reason.clone()).is_err() {
            self.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push((staged.clone(), reason));
        }
    }

    /// Retries deferred resolutions; those that still fail stay deferred.
    fn retry_pending(&self, env: &Preparation<'_>) {
        let pending = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|p| p.into_inner()));
        for (staged, reason) in pending {
            self.abandon_or_defer(env, &staged, reason);
        }
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
        let generation = put_staged_blob(tx, &*self.files, staged)?;
        let next = tx
            .artifacts()
            .versions(artifact_id)?
            .iter()
            .map(|v| v.version_no)
            .max()
            .unwrap_or(0)
            + 1;
        let version = new_version(tx, ctx, artifact_id, next, media_type, staged, generation);
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
