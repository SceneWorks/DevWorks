//! Durable typed replies of command receipts (HISTORY-01/02, ART-01/02, STORE-03).
//!
//! The recorded `ReceiptObservation` rule requires a `Recorded` observation to carry the
//! receipt's command, committed sequence, immutable reply artifact and reply schema. Files and
//! SQLite cannot commit in one transaction, so publication is split:
//!
//! 1. **Intent, with the receipt.** The gateway's command unit of work commits a durable queue
//!    delivery ([`record_intent`], consumer [`REPLY_CONSUMER`]) for the receipt's
//!    `CommandDecided` event together with the receipt, its history and the domain changes. A
//!    receipt never exists without its publication intent (migration 8 adds the intents of
//!    receipts an earlier build recorded).
//! 2. **Publication, after commit.** The durable delivery dispatcher claims the delivery and runs
//!    [`ReceiptReplies::publish`] outside any transaction under the claim's fence: the reply bytes
//!    are rendered from the stored receipt and written as a new generation under a journaled
//!    artifact staging operation (the artifact store's own publication protocol), then one write
//!    re-checks the claim fence and cancellation, records the blob, the immutable artifact
//!    version, the reply record and the history event, completes the staging operation and
//!    settles the delivery — all or nothing.
//! 3. **Recovery.** A failure before that write leaves at most a journaled staging operation and
//!    an unreferenced generation, which the staging resolution (at once, or the artifact store's
//!    recovery after a crash) fails and collects; the unsettled delivery is retried with durable,
//!    capped backoff in this boot and redelivered after a restart. A transient fault never makes
//!    it terminal (the engine's reply consumer has no attempt budget), so every recorded receipt
//!    eventually gets its reply. Publication is idempotent per receipt: the reply record is
//!    unique per receipt, command and artifact, and a publisher that finds the reply already
//!    recorded reuses it and creates nothing. Only a receipt this build cannot type is held
//!    (another contract or schema revision, an unregistered operation, a decision that cannot be
//!    redacted), and only an intent that cannot be honoured at all fails.
//!
//! **What a caller sees** ([`recorded_observation`]): the `Recorded` observation once published;
//! while the delivery is ready or claimed, pending (retry the same query); while it is held or
//! failed, held behind its delivery, which an operator resolves (`queue.retry`/`queue.reconcile`);
//! once it was cancelled or dismissed without a reply (or no intent exists), abandoned — the
//! reply will never be published, and no retry is advised.
//!
//! **Reply bytes** are the canonical (RFC 8785 JCS) JSON of the reply the command returned when it
//! was recorded, `CommandReply::Recorded { receipt, replayed: false }`, built from the stored
//! receipt, whose decision the gateway redacted before storing it; nothing is re-derived or
//! re-run. The decision is redacted again when the bytes are rendered (defence in depth for a
//! receipt an earlier build stored, including backfilled ones); one that cannot be redacted is
//! held, never published.
//!
//! The media type is `application/json`; the reply schema ([`reply_schema`]) names the
//! operation's binding at the receipt's stored schema revision (`devworks.reply/<operation>` at
//! that revision) with the digest of the schema document combining the generic command reply
//! envelope and that operation's result schema, both at that revision. A receipt recorded under
//! another contract or a revision this build does not register is held with its cause rather
//! than typed by a guess.
//!
//! **Owner scope.** The reply belongs to the project its decision was recorded in — taken from the
//! receipt's own `CommandDecided` event, which the delivery must name, never from the delivery's
//! payload — or to the workspace for a projectless receipt; either way its owning principal is
//! the receipt's actor and it is retained with the receipt ([`ArtifactRetention::Receipt`]; see
//! the module documentation of [`super`] for the deletion and visibility rules). A use case
//! cannot enqueue a reply intent: the gateway's handler transaction refuses it.
//!
//! **History.** Publication is recorded as a `ReceiptReplyPublished` event of the receipt's own
//! aggregate (`receipt_reply`, keyed by the command), attributed to the host, with no project or
//! work root, so it never enters a project or work feed where another principal's receipt
//! metadata would be disclosed. It names only the reply artifact version and its digest.
//!
//! **Cost.** Per recorded command: one queue row in the command's own transaction (no extra
//! commit), then, after commit and off the command's path, two write transactions (staging
//! journal; publication) and one small immutable file written through a flushed staging file,
//! an atomic rename and directory flushes. Measured by the ignored `measure_reply_overhead`
//! test (SQLite adapter reply tests, release build, macOS APFS with `synchronous=FULL`): a
//! command took about 18 ms, its reply's publication about 64 ms of write-owner and flush time
//! (roughly three flushed commits), a reply was about 400 bytes, and artifact recovery over 200
//! replies took about 12 ms. Publication is deliberately not batched: each reply stays an
//! independent, idempotent unit whose failure affects no other, and the dispatcher's
//! per-consumer in-flight cap keeps a burst from taking every worker; batching several replies
//! per write is the lever if write-owner time becomes the bottleneck. Files live in the
//! content-addressed store's 256 shard directories (`artifacts/sha256/<2 hex>/`): about 4 000
//! entries per shard per million receipts, which the supported file systems list efficiently;
//! boot recovery's file sweep and verification grow linearly with the number of retained
//! files, replies included.
use super::{
    ArtifactService, StagedFile, StagingRef, Viewer, put_staged_blob, version_ref, visible,
};
use crate::context::ReceiptIdentity;
use crate::fingerprint::sha256_hex;
use crate::ports::Preparation;
use crate::ports::files::{ArtifactFiles, FileFault, SourceOrigin};
use crate::ports::storage::{
    ApplicationStore, ArtifactRetention, ArtifactScope, ArtifactState, BlobState, LedgerEntry,
    NewEvent, NewQueueEntry, QueueEntry, QueueFence, QueueState, ReadSnapshot, ReceiptReply,
    Settlement, StorageError, StoredArtifact, StoredVersion, WriteOptions, WriteTransaction,
    run_write,
};
use crate::ports::{IdKind, StoredReceipt};
use crate::protocol::provider::{ArtifactRef, Digest, QualifiedId, SchemaRef};
use crate::protocol::records::{ReceiptLookupState, ReceiptObservation};
use crate::protocol::results::DomainCommandReply;
use crate::protocol::results::DomainDecision;
use crate::protocol::support::Revisioned;
use crate::protocol::{
    ActorId, CONTRACT, Check, CommandId, CommandReply, EventId, Id, ProjectId, Protocol,
    RequestKey, Revision,
};
use crate::registry::{OperationDescriptor, SchemaRevision};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Durable delivery consumer of receipt reply publication.
pub const REPLY_CONSUMER: &str = "receipt_replies";
/// Media type of every reply artifact.
pub const REPLY_MEDIA_TYPE: &str = "application/json";
/// Namespace of reply schema IDs (`devworks.reply/<operation>`).
pub const REPLY_SCHEMA_NAMESPACE: &str = "devworks.reply";
/// History aggregate of a receipt's reply publication (`aggregate_id` = command ID).
pub const REPLY_AGGREGATE: &str = "receipt_reply";

/// Records the publication intent of a receipt inside the command's unit of work: one unordered
/// delivery for the receipt's `CommandDecided` event. Called only by the gateway, right after it
/// recorded the receipt.
pub(crate) fn record_intent(
    tx: &mut dyn WriteTransaction,
    identity: &ReceiptIdentity,
    command: &CommandId,
    event: &EventId,
) -> Result<(), StorageError> {
    let id = tx.new_id(IdKind::QueueEntry);
    let ready_at_ms = tx.now_ms();
    tx.queue().enqueue(NewQueueEntry {
        id,
        consumer: REPLY_CONSUMER.into(),
        event_id: event.clone(),
        // Not root-scoped: a reply is bookkeeping of a decision already made, so its delivery is
        // never held for root eligibility, and it carries no operation (nothing external runs).
        root_id: None,
        operation_id: None,
        partition: None,
        priority: 0,
        ready_at_ms,
        payload: json!({
            "actorId": identity.actor_id().0.as_str(),
            "scopeKey": identity.scope_key().as_str(),
            "requestKey": identity.request_key().0.as_str(),
            "commandId": command.0.as_str(),
        }),
    })?;
    Ok(())
}

/// The publication intent a delivery carries.
#[derive(Clone, Debug)]
struct Intent {
    identity: ReceiptIdentity,
    command_id: CommandId,
}
impl Intent {
    fn parse(payload: &Value) -> Option<Self> {
        let id = |k: &str| payload.get(k)?.as_str().and_then(|s| Id::new(s).ok());
        let scope_key = payload.get("scopeKey")?.as_str()?.to_owned();
        Some(Self {
            identity: ReceiptIdentity::recorded(
                ActorId(id("actorId")?),
                scope_key,
                RequestKey(id("requestKey")?),
            ),
            command_id: CommandId(id("commandId")?),
        })
    }
}

/// The reply schema reference of `descriptor`'s binding at `revision` (a receipt's stored
/// revision): `devworks.reply/<operation>` at that revision, with the SHA-256 of the canonical
/// schema document (the contract, the generic command reply envelope and the operation's result
/// schema, both at that revision). `None` when the operation does not register `revision` or its
/// name cannot form a qualified schema ID.
pub fn reply_schema(
    descriptor: &OperationDescriptor,
    revision: SchemaRevision,
) -> Option<SchemaRef> {
    if !descriptor.supports(revision) {
        return None;
    }
    let schema_id = format!("{REPLY_SCHEMA_NAMESPACE}/{}", descriptor.name);
    let qualified = QualifiedId::try_from(schema_id.clone()).ok()?;
    let reply = crate::registry::schema_at(schemars::schema_for!(DomainCommandReply), revision);
    let document = json!({
        "$id": schema_id,
        "contract": CONTRACT,
        "revision": revision.0,
        "reply": serde_json::to_value(reply).ok()?,
        "result": serde_json::to_value(descriptor.result_schema(revision)).ok()?,
    });
    let canonical = crate::jcs::canonicalize(&document).ok()?;
    Some(SchemaRef {
        schema_id: qualified,
        revision: Revision::new(i64::from(revision.0)).ok()?,
        sha256: Digest(sha256_hex(canonical.as_bytes())),
    })
}

/// The canonical reply bytes of a stored receipt: the `Recorded` reply it was returned as, at its
/// stored schema revision, with its decision redacted again (an earlier build may have stored it
/// unredacted). The gateway stores the decision already projected onto the negotiated revision
/// and replays it under that projection; projecting again here is idempotent and keeps the bytes
/// exactly what a replay returns.
pub fn reply_bytes(stored: &StoredReceipt) -> Result<Vec<u8>, String> {
    let mut receipt = stored.receipt.clone();
    receipt.decision = crate::authority::redact::redact_decision(receipt.decision)?;
    if let DomainDecision::Applied { result } | DomainDecision::NoChange { result } =
        &mut receipt.decision
    {
        result.retain_revision(stored.schema_revision.0);
    }
    let reply: DomainCommandReply = CommandReply::Recorded {
        protocol: Protocol::Draft1,
        receipt,
        replayed: false,
    };
    let value = serde_json::to_value(&reply).map_err(|e| e.to_string())?;
    crate::jcs::canonicalize(&value)
        .map(String::into_bytes)
        .map_err(|e| e.to_string())
}

/// What one publication attempt established.
#[derive(Clone, Debug)]
pub enum ReplyOutcome {
    /// Published now, and the delivery settled.
    Published(ArtifactRef),
    /// The reply had already been published (and is reused); the delivery settled.
    Existing(ArtifactRef),
    /// The claim is no longer this publisher's (recovered, settled elsewhere or cancelled):
    /// nothing was published.
    Stale,
    /// Transient: nothing visible was published; retry the delivery.
    Retry(String),
    /// The receipt cannot be typed by this build (another contract or schema revision): hold the
    /// delivery with its cause.
    Hold(String),
    /// The intent cannot be honoured (unreadable, or its receipt does not exist).
    Poison(String),
}

enum Committed {
    Published(ArtifactRef),
    Existing(ArtifactRef),
    Stale,
}

/// The receipt reply publisher (see the module documentation).
pub struct ReceiptReplies {
    artifacts: Arc<ArtifactService>,
    schemas: Mutex<HashMap<(&'static str, SchemaRevision), SchemaRef>>,
}

impl ReceiptReplies {
    pub fn new(artifacts: Arc<ArtifactService>) -> Self {
        Self {
            artifacts,
            schemas: Mutex::new(HashMap::new()),
        }
    }

    fn schema(
        &self,
        descriptor: &'static OperationDescriptor,
        revision: SchemaRevision,
    ) -> Option<SchemaRef> {
        let mut cache = self.schemas.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(s) = cache.get(&(descriptor.name, revision)) {
            return Some(s.clone());
        }
        let s = reply_schema(descriptor, revision)?;
        cache.insert((descriptor.name, revision), s.clone());
        Some(s)
    }

    /// Publishes the reply of the receipt a claimed [`REPLY_CONSUMER`] delivery names, outside
    /// any transaction, and settles the delivery in the same write that records the reply.
    pub fn publish(&self, store: &dyn ApplicationStore, claim: &QueueEntry) -> ReplyOutcome {
        match self.try_publish(store, claim) {
            Ok(outcome) => outcome,
            Err(e) => ReplyOutcome::Retry(e.to_string()),
        }
    }

    fn try_publish(
        &self,
        store: &dyn ApplicationStore,
        claim: &QueueEntry,
    ) -> Result<ReplyOutcome, StorageError> {
        let Some(fence) = claim.fence() else {
            return Ok(ReplyOutcome::Stale);
        };
        if claim.consumer != REPLY_CONSUMER {
            return Ok(ReplyOutcome::Poison("not a receipt reply delivery".into()));
        }
        let Some(intent) = Intent::parse(&claim.payload) else {
            return Ok(ReplyOutcome::Poison("unreadable publication intent".into()));
        };
        let env = Preparation::new(store, None, None);
        let identity = intent.identity.clone();
        let event_id = claim.event_id.clone();
        let (existing, entry, event) = env.read(|s| {
            Ok((
                s.receipts().reply(&identity)?,
                s.receipts().lookup(&identity)?,
                s.history().event(&event_id)?,
            ))
        })?;
        // The delivery must name its receipt's own decision; the reply's owner scope is that
        // decision's project, never something the delivery claims.
        let decides = |e: &crate::ports::storage::EventRecord| {
            e.kind == "CommandDecided"
                && e.aggregate_kind == "command"
                && e.aggregate_id == intent.command_id.0.as_str()
                && &e.actor == intent.identity.actor_id()
        };
        let Some(event) = event.filter(decides) else {
            return Ok(ReplyOutcome::Poison(
                "the delivery does not name its receipt's decision".into(),
            ));
        };
        if existing.is_some() {
            return self.commit_existing(store, claim, fence, intent);
        }
        let stored = match entry {
            None => return Ok(ReplyOutcome::Poison("the receipt does not exist".into())),
            Some(LedgerEntry::Incompatible { reason, .. }) => {
                return Ok(ReplyOutcome::Hold(format!(
                    "incompatible receipt: {reason}"
                )));
            }
            Some(LedgerEntry::Current(stored)) => stored,
        };
        if stored.receipt.command_id != intent.command_id {
            return Ok(ReplyOutcome::Poison(
                "the intent names another command than its receipt".into(),
            ));
        }
        let Some(descriptor) = crate::registry::find(&stored.operation) else {
            return Ok(ReplyOutcome::Hold(format!(
                "operation {} is not registered by this build",
                stored.operation
            )));
        };
        if stored.contract != CONTRACT || !descriptor.supports(stored.schema_revision) {
            return Ok(ReplyOutcome::Hold(format!(
                "receipt recorded under {} schema revision {}; this build registers {:?}",
                stored.contract,
                stored.schema_revision.0,
                descriptor.revisions.iter().map(|r| r.0).collect::<Vec<_>>()
            )));
        }
        let Some(schema) = self.schema(descriptor, stored.schema_revision) else {
            return Ok(ReplyOutcome::Hold(format!(
                "operation {} has no qualified reply schema",
                stored.operation
            )));
        };
        let bytes = match reply_bytes(&stored) {
            Ok(b) => b,
            Err(e) => return Ok(ReplyOutcome::Hold(format!("unrenderable reply: {e}"))),
        };
        let digest = sha256_hex(&bytes);
        let staged = match self.stage(&env, &bytes, &digest, &intent)? {
            Ok(staged) => staged,
            Err(cause) => return Ok(ReplyOutcome::Retry(cause)),
        };
        let files = self.artifacts.files.clone();
        let (c, s, operation) = (claim.clone(), staged.clone(), stored.operation.clone());
        let project = event.project_id;
        let committed = run_write(store, WriteOptions::default(), move |tx| {
            commit(
                tx,
                &c,
                &fence,
                &intent,
                project.as_ref(),
                &operation,
                &schema,
                &s,
                &*files,
            )
        });
        let staging = staged.staging_ref();
        match committed {
            Ok(Committed::Published(r)) => {
                self.remove_unbound(&env, &staged);
                Ok(ReplyOutcome::Published(r))
            }
            Ok(Committed::Existing(r)) => {
                self.resolve(&env, &staging, "the reply was already published");
                Ok(ReplyOutcome::Existing(r))
            }
            Ok(Committed::Stale) => {
                self.resolve(&env, &staging, "the delivery claim was lost");
                Ok(ReplyOutcome::Stale)
            }
            // A resolution re-reads the durable record under the staging fence: completed if the
            // version did commit (an unknown commit outcome that succeeded), failed otherwise.
            Err(e) => {
                self.resolve(&env, &staging, "the reply metadata was not committed");
                Ok(ReplyOutcome::Retry(e.to_string()))
            }
        }
    }

    /// Stages the reply bytes as a new generation (or verifies and reuses the generation already
    /// holding them) under a journaled staging operation. `Ok(Err(cause))` is a publication
    /// failure already resolved on the journal.
    fn stage(
        &self,
        env: &Preparation<'_>,
        bytes: &[u8],
        digest: &str,
        intent: &Intent,
    ) -> Result<Result<StagedFile, String>, StorageError> {
        let d = digest.to_owned();
        let reuse = env
            .read(|s| s.artifacts().blob(&d))?
            .filter(|b| matches!(b.state, BlobState::Present | BlobState::UnlinkPending))
            .map(|b| b.generation);
        let source = json!({"kind": "ReceiptReply", "commandId": intent.command_id.0.as_str()});
        let payload = json!({
            "digest": digest,
            "byteCount": bytes.len(),
            "source": source,
        });
        let (staging, published) = self.artifacts.journal_and_publish(
            env,
            &mut std::io::Cursor::new(bytes),
            digest,
            bytes.len() as i64,
            reuse.as_deref(),
            payload,
        )?;
        let published = match published {
            Ok(p) => p,
            Err(fault) => {
                self.resolve(env, &staging, "the reply file was not published");
                if let (FileFault::Corrupt(_), Some(g)) = (&fault, reuse.as_deref()) {
                    // Kept as evidence; the retry publishes a fresh generation.
                    let _ = self.artifacts.files.quarantine(digest, g);
                }
                return Ok(Err(format!("reply file publication failed: {fault:?}")));
            }
        };
        Ok(Ok(StagedFile {
            operation_id: staging.operation_id.clone(),
            digest: digest.to_owned(),
            byte_count: bytes.len() as i64,
            generation: published.generation,
            reused: published.reused,
            relative_path: published.relative_path,
            origin: SourceOrigin::default(),
            source,
            fence: staging.fence,
        }))
    }

    /// The reply was already published: settle the delivery under its fence.
    fn commit_existing(
        &self,
        store: &dyn ApplicationStore,
        claim: &QueueEntry,
        fence: QueueFence,
        intent: Intent,
    ) -> Result<ReplyOutcome, StorageError> {
        let c = claim.clone();
        let committed = run_write(store, WriteOptions::default(), move |tx| {
            if !claimed(tx, &c, &fence)? {
                return Ok(Committed::Stale);
            }
            settle_existing(tx, &c, &fence, &intent)
        })?;
        Ok(match committed {
            Committed::Existing(r) | Committed::Published(r) => ReplyOutcome::Existing(r),
            Committed::Stale => ReplyOutcome::Stale,
        })
    }

    /// Resolves a staging operation that did not publish metadata (or remembers it for the
    /// artifact store's next recovery in this boot).
    fn resolve(&self, env: &Preparation<'_>, staging: &StagingRef, why: &str) {
        self.artifacts
            .abandon_or_defer(env, staging, json!({"abandoned": why}));
    }

    /// After a commit that kept another live generation of the same bytes, this resolved
    /// operation's own generation is an orphan nothing can bind: remove it.
    fn remove_unbound(&self, env: &Preparation<'_>, staged: &StagedFile) {
        if staged.reused {
            return;
        }
        let (digest, own) = (staged.digest.clone(), staged.operation_id.0.clone());
        let bound = env.read(|s| {
            Ok(s.artifacts()
                .blob(&digest)?
                .is_some_and(|b| b.generation == own.as_str()))
        });
        if matches!(bound, Ok(false)) {
            let _ = self.artifacts.files.unlink(&staged.digest, own.as_str());
        }
    }
}

/// Whether `claim` is still this publisher's live claim with no cancellation requested.
fn claimed(
    tx: &mut dyn WriteTransaction,
    claim: &QueueEntry,
    fence: &QueueFence,
) -> Result<bool, StorageError> {
    let boot = tx.store().boot_id.clone();
    Ok(tx.queue().entry(&claim.id)?.is_some_and(|e| {
        e.state == QueueState::Claimed
            && e.fence().as_ref() == Some(fence)
            && e.claim_boot_id == boot
            && e.cancellation_epoch == claim.cancellation_epoch
            && !e.cancel_requested
    }))
}

fn reference(
    tx: &mut dyn WriteTransaction,
    reply: &ReceiptReply,
) -> Result<ArtifactRef, StorageError> {
    let version =
        tx.artifacts()
            .version(&reply.version_id)?
            .ok_or_else(|| StorageError::NotFound {
                kind: "artifact_version",
                id: reply.version_id.as_str().to_owned(),
            })?;
    Ok(version_ref(&version))
}

fn settle_existing(
    tx: &mut dyn WriteTransaction,
    claim: &QueueEntry,
    fence: &QueueFence,
    intent: &Intent,
) -> Result<Committed, StorageError> {
    let Some(reply) = tx.receipts().reply(&intent.identity)? else {
        return Err(StorageError::NotFound {
            kind: "receipt_reply",
            id: intent.command_id.0.as_str().to_owned(),
        });
    };
    let r = reference(tx, &reply)?;
    tx.queue().settle(&claim.id, fence, Settlement::Completed)?;
    Ok(Committed::Existing(r))
}

/// The publishing write: fence, idempotency, blob, artifact, version, reply record, history and
/// settlement, all or nothing.
#[allow(clippy::too_many_arguments)]
fn commit(
    tx: &mut dyn WriteTransaction,
    claim: &QueueEntry,
    fence: &QueueFence,
    intent: &Intent,
    project: Option<&ProjectId>,
    operation: &str,
    schema: &SchemaRef,
    staged: &StagedFile,
    files: &dyn ArtifactFiles,
) -> Result<Committed, StorageError> {
    if !claimed(tx, claim, fence)? {
        return Ok(Committed::Stale);
    }
    if tx.receipts().reply(&intent.identity)?.is_some() {
        return settle_existing(tx, claim, fence, intent);
    }
    let generation = put_staged_blob(tx, files, staged)?;
    let scope = match project {
        Some(p) if tx.artifacts().project_exists(p)? => ArtifactScope::Project(p.clone()),
        _ => ArtifactScope::Workspace,
    };
    let host = tx.store().host_actor.clone();
    let owner = intent.identity.actor_id().clone();
    let now = tx.now_ms();
    let artifact_id = tx.new_id(IdKind::Artifact);
    let version_id = tx.new_id(IdKind::ArtifactVersion);
    let title = format!("{operation} reply");
    let mut artifact = StoredArtifact {
        id: artifact_id.clone(),
        origin_project_id: scope.project().cloned(),
        origin_work_id: None,
        title: title.clone(),
        revision: 1,
        scope: scope.clone(),
        owner: Some(owner.clone()),
        retention: ArtifactRetention::Receipt,
        scope_revision: 1,
        reference_revision: 1,
        current_version_id: None,
        state: ArtifactState::Live,
        created_at_ms: now,
        provenance: json!({
            "producedBy": host.0.as_str(),
            "principalKind": "Host",
            "sourceKind": "ReceiptReply",
            "receiptActor": owner.0.as_str(),
            "commandId": intent.command_id.0.as_str(),
            "operation": operation,
        }),
    };
    tx.artifacts().insert_artifact(&artifact)?;
    let version = StoredVersion {
        id: version_id.clone(),
        artifact_id: artifact_id.clone(),
        version_no: 1,
        blob_digest: staged.digest.clone(),
        byte_count: staged.byte_count,
        blob_generation: generation,
        media_type: REPLY_MEDIA_TYPE.into(),
        created_at_ms: now,
        deleted_at_ms: None,
        source: json!({
            "source": staged.source,
            "operationId": staged.operation_id.0.as_str(),
            "producedBy": host.0.as_str(),
            "principalKind": "Host",
        }),
    };
    tx.artifacts().insert_version(&version)?;
    artifact.current_version_id = Some(version_id.clone());
    tx.artifacts().update_artifact(&artifact)?;
    super::complete_operation(
        tx,
        &staged.operation_id,
        json!({"artifactId": artifact_id.as_str(), "versionId": version_id.as_str()}),
    )?;
    // The receipt's own aggregate, outside every project and work feed (see the module docs).
    let event = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: event,
        actor: host,
        project_id: None,
        root_id: None,
        operation_id: Some(staged.operation_id.clone()),
        aggregate_kind: REPLY_AGGREGATE.into(),
        aggregate_id: intent.command_id.0.as_str().to_owned(),
        aggregate_revision: 1,
        kind: "ReceiptReplyPublished".into(),
        payload: json!({
            "artifactId": artifact_id.as_str(), "versionId": version_id.as_str(),
            "digest": staged.digest, "byteCount": staged.byte_count,
            "mediaType": REPLY_MEDIA_TYPE, "principalKind": "Host",
        }),
    })?;
    tx.receipts().record_reply(
        &intent.identity,
        &ReceiptReply {
            command_id: intent.command_id.clone(),
            artifact_id,
            version_id,
            schema: serde_json::to_value(schema)
                .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
            published_at_ms: now,
        },
    )?;
    tx.queue().settle(&claim.id, fence, Settlement::Completed)?;
    Ok(Committed::Published(version_ref(&version)))
}

/// Why a recorded receipt's observation cannot be published to this caller now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservationGap {
    /// The reply is not published yet: its delivery is ready or being published.
    Pending,
    /// Its delivery is held for reconciliation or failed: an operator resolves it (the delivery
    /// named here) before publication can continue.
    Held(Id),
    /// It will never be published: its delivery was cancelled or dismissed without a reply, or
    /// no publication intent exists.
    Abandoned,
    /// The caller can no longer see the reply (its scope or authority changed).
    Denied,
}

/// The rules-valid `Recorded` observation of a stored receipt, once its reply is published and
/// still visible to the caller (access is rechecked on this snapshot before it is published).
pub(crate) fn recorded_observation(
    snapshot: &dyn ReadSnapshot,
    viewer: &Viewer<'_>,
    identity: &ReceiptIdentity,
    stored: &StoredReceipt,
) -> Result<Result<ReceiptObservation, ObservationGap>, StorageError> {
    let Some(reply) = snapshot.receipts().reply(identity)? else {
        return Ok(Err(publication_state(snapshot, stored)?));
    };
    let read = snapshot.artifacts();
    let Some(artifact) = read.artifact(&reply.artifact_id)? else {
        return Err(StorageError::InvalidRecord(
            "a receipt reply names a missing artifact".into(),
        ));
    };
    if artifact.state != ArtifactState::Live || !visible(read, viewer, &artifact)? {
        return Ok(Err(ObservationGap::Denied));
    }
    let version = read
        .version(&reply.version_id)?
        .filter(|v| v.artifact_id == artifact.id && v.live())
        .ok_or_else(|| {
            StorageError::InvalidRecord("a receipt reply names a missing version".into())
        })?;
    let schema: SchemaRef = serde_json::from_value(reply.schema.clone())
        .map_err(|e| StorageError::InvalidRecord(format!("stored reply schema: {e}")))?;
    let observation = ReceiptObservation {
        request_key: identity.request_key().clone(),
        command_id: Some(stored.receipt.command_id.clone()),
        committed_sequence: Some(stored.receipt.committed_sequence.clone()),
        operation_id: stored.receipt.operation_id.clone(),
        reply_artifact: Some(version_ref(&version)),
        reply_schema: Some(schema),
        state: ReceiptLookupState::Recorded,
    };
    observation.check().map_err(StorageError::InvalidRecord)?;
    Ok(Ok(observation))
}

/// Where an unpublished reply stands, from its receipt's decision event and that event's
/// publication delivery.
fn publication_state(
    snapshot: &dyn ReadSnapshot,
    stored: &StoredReceipt,
) -> Result<ObservationGap, StorageError> {
    let decision = snapshot.history().page(
        &crate::ports::storage::EventFilter::Aggregate {
            kind: "command".into(),
            id: stored.receipt.command_id.0.as_str().to_owned(),
        },
        0,
        1,
    )?;
    let Some(event) = decision.events.first() else {
        return Ok(ObservationGap::Abandoned);
    };
    Ok(
        match snapshot.queue().delivery(REPLY_CONSUMER, &event.id)? {
            None => ObservationGap::Abandoned,
            Some(e) => match e.state {
                QueueState::Ready | QueueState::Claimed => ObservationGap::Pending,
                QueueState::Reconciling | QueueState::Failed => ObservationGap::Held(e.id),
                QueueState::Completed | QueueState::Cancelled | QueueState::Dismissed => {
                    ObservationGap::Abandoned
                }
            },
        },
    )
}
