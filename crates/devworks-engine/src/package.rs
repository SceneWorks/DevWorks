//! Host side of the package `broker.commit_event` request (provider-session-package-contracts.md
//! "Atomic event handler commit").
//!
//! [`commit_event`] runs inside the delivery's write transaction. It validates the host-resolved
//! invocation binding, the claimed delivery, the expected subscription checkpoint, the pinned
//! state generation and the expected state revision, and every intent, before writing anything.
//! It then commits, in that one transaction: the state compare-and-set, one durable operation
//! intent plus outbox delivery per typed effect intent, the delivery's settlement (which advances
//! the subscription checkpoint) and the processing receipt holding the payload digest and the
//! exact accepted reply. External effects run later through ordinary operation admission;
//! accepting their intents is not their completion.
//!
//! Checkpoints ([`expected_checkpoint`]): an ordered subscription (a delivery with a partition)
//! uses its partition's contiguous checkpoint, so a commit is refused if another commit of the
//! same partition moved it. An unordered subscription has no order to protect, so its checkpoint
//! is per delivery: `0` until this delivery is processed, and the accepted reply carries the
//! delivery's own event sequence. A consumer-wide watermark would change whenever an unrelated
//! delivery settled (spurious conflicts) and could lie below the committed event.
//!
//! Cancellation: the invocation carries the claim's cancellation epoch. A delivery whose epoch
//! rose (or whose cancellation is otherwise requested) refuses a new commit with
//! [`CommitRefusal::Cancelled`] and nothing is written; the dispatcher settles it `Cancelled`.
//! Both decide with the same rule, `cancellation_applies`.
//!
//! Replay: an identical commit for an already processed `(package, subscription, event)` returns
//! the original receipt, checkpoint, state revision and operation IDs without writing; a changed
//! one is refused. A stale or revoked invocation cannot introduce a new commit, but may recover
//! an already committed reply. A crash before the transaction commits leaves no state, receipt,
//! intent or checkpoint; a crash after it reuses all four on replay.
use crate::store;
use devworks_core::fingerprint::sha256_hex;
use devworks_core::jcs;
use devworks_core::ports::IdKind;
use devworks_core::ports::storage::{
    NewEvent, NewOperation, NewQueueEntry, ProcessingRecord, QueueEntry, QueueFence, QueueRead,
    QueueState, Settlement, StorageError, WriteTransaction, cancellation_applies,
};
use devworks_core::protocol::{EventId, Id, OperationId};
use devworks_extension_protocol::package::{EventCommit, PackageRequest, RpcResult, RpcVersion};
use devworks_extension_protocol::scalars as wire;
use serde_json::{Value, json};

/// What the host resolved for one event invocation. Nothing here comes from the package.
#[derive(Clone, Debug)]
pub struct PackageInvocation {
    pub package_id: String,
    pub subscription_id: Id,
    pub process_session_id: String,
    pub invocation_id: String,
    pub invocation_token: String,
    /// The package state generation pinned for this invocation.
    pub state_generation: i64,
    /// The claimed delivery this invocation handles.
    pub delivery_id: Id,
    pub fence: QueueFence,
    /// The claim's cancellation epoch when the invocation started.
    pub cancellation_epoch: i64,
    /// Host actor that attributes the commit's history.
    pub actor: devworks_core::protocol::ActorId,
    /// Consumer that admits the commit's effect intents (the broker's outbox).
    pub broker_consumer: String,
}

/// Why a commit was refused. Nothing was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitRefusal {
    /// Frame bounds, duplicate intent keys or a non-relative HTTP target.
    Invalid(&'static str),
    /// The commit names a different process session, invocation, token, subscription or event
    /// than the host resolved for this invocation.
    BindingMismatch,
    /// The invocation's delivery is no longer claimed by it (stale or revoked).
    StaleInvocation,
    /// A cancellation was requested for the delivery after the invocation started.
    Cancelled,
    CheckpointConflict {
        expected: i64,
        current: i64,
    },
    GenerationMismatch {
        pinned: i64,
        current: Option<i64>,
    },
    StateConflict {
        expected: i64,
        current: i64,
    },
    /// A different commit was already processed for this event.
    ChangedReplay,
}

#[derive(Clone, Debug)]
pub enum CommitOutcome {
    /// `EventCommitAccepted` of a new commit (reply only after the transaction commits).
    Accepted(RpcResult),
    /// The original `EventCommitAccepted` of an identical earlier commit.
    Replayed(RpcResult),
    Refused(CommitRefusal),
}

fn invalid(m: impl Into<String>) -> StorageError {
    StorageError::InvalidRecord(m.into())
}

/// Digest of the commit's meaning, excluding the per-process session, invocation and token so a
/// restarted package process can replay the same commit.
pub fn payload_digest(commit: &EventCommit) -> Result<String, StorageError> {
    let mut v = serde_json::to_value(commit).map_err(|e| invalid(e.to_string()))?;
    if let Some(o) = v.as_object_mut() {
        o.remove("processSessionId");
        o.remove("invocationId");
        o.remove("invocationToken");
    }
    let canonical = jcs::canonicalize(&v).map_err(|e| invalid(format!("{e:?}")))?;
    Ok(sha256_hex(canonical.as_bytes()))
}

/// The checkpoint a new commit for `entry` must present (see the module documentation).
pub fn expected_checkpoint(queue: &dyn QueueRead, entry: &QueueEntry) -> Result<i64, StorageError> {
    match entry.partition.as_deref() {
        Some(p) => queue.checkpoint(&entry.consumer, Some(p)),
        None => Ok(0),
    }
}

fn wire_err<E: std::fmt::Debug>(e: E) -> StorageError {
    invalid(format!("wire value: {e:?}"))
}

fn accepted(record: &ProcessingRecord) -> Result<RpcResult, StorageError> {
    let ops = record
        .intents
        .as_array()
        .ok_or_else(|| invalid("processing intents"))?
        .iter()
        .map(|i| {
            let id = i["operationId"]
                .as_str()
                .ok_or_else(|| invalid("intent op"))?;
            Ok(wire::OperationId(
                wire::Id::try_from(id.to_owned()).map_err(wire_err)?,
            ))
        })
        .collect::<Result<Vec<_>, StorageError>>()?;
    Ok(RpcResult::EventCommitAccepted {
        processing_receipt: wire::Id::try_from(record.receipt_id.as_str().to_owned())
            .map_err(wire_err)?,
        checkpoint: wire::Counter::try_from(record.checkpoint.to_string()).map_err(wire_err)?,
        state_revision: wire::Revision::try_from(record.state_revision.to_string())
            .map_err(wire_err)?,
        operation_ids: ops,
    })
}

fn counter(c: &wire::Counter) -> Result<i64, StorageError> {
    String::from(c.clone())
        .parse()
        .map_err(|_| invalid("counter"))
}
fn rev(c: &wire::Revision) -> Result<i64, StorageError> {
    String::from(c.clone())
        .parse()
        .map_err(|_| invalid("revision"))
}

/// Commits one package event handler invocation atomically (see the module documentation).
pub fn commit_event(
    tx: &mut dyn WriteTransaction,
    inv: &PackageInvocation,
    commit: &EventCommit,
) -> Result<CommitOutcome, StorageError> {
    // Structural checks shared with the wire protocol (bounds, unique keys, relative targets).
    let frame = PackageRequest::CommitEvent {
        jsonrpc: RpcVersion::V2,
        id: "commit".into(),
        params: commit.clone(),
    };
    if let Err(e) = frame.validate() {
        return Ok(CommitOutcome::Refused(CommitRefusal::Invalid(e)));
    }
    let event_id = EventId(Id::new(commit.event_id.0.as_str()).map_err(invalid)?);
    if commit.subscription_id.as_str() != inv.subscription_id.as_str() {
        return Ok(CommitOutcome::Refused(CommitRefusal::BindingMismatch));
    }
    let digest = payload_digest(commit)?;

    // An already processed event replays its original reply, even for a stale invocation.
    if let Some(done) =
        tx.packages()
            .processing(&inv.package_id, &inv.subscription_id, &event_id)?
    {
        return Ok(if done.payload_digest == digest {
            CommitOutcome::Replayed(accepted(&done)?)
        } else {
            CommitOutcome::Refused(CommitRefusal::ChangedReplay)
        });
    }

    // A new commit needs the current invocation and its claimed delivery for this event.
    if commit.process_session_id.as_str() != inv.process_session_id
        || commit.invocation_id.as_str() != inv.invocation_id
        || commit.invocation_token.as_str() != inv.invocation_token
    {
        return Ok(CommitOutcome::Refused(CommitRefusal::BindingMismatch));
    }
    let Some(delivery) = tx.queue().entry(&inv.delivery_id)? else {
        return Ok(CommitOutcome::Refused(CommitRefusal::StaleInvocation));
    };
    if delivery.event_id != event_id {
        return Ok(CommitOutcome::Refused(CommitRefusal::BindingMismatch));
    }
    let boot = tx.store().boot_id.clone();
    if delivery.state != QueueState::Claimed
        || delivery.fence().as_ref() != Some(&inv.fence)
        || delivery.claim_boot_id != boot
    {
        return Ok(CommitOutcome::Refused(CommitRefusal::StaleInvocation));
    }
    // The dispatcher's rule: a request applies unless the operation may have dispatched.
    let requested =
        delivery.cancellation_epoch != inv.cancellation_epoch || delivery.cancel_requested;
    if requested && cancellation_applies(&*tx.operations(), &delivery)? {
        return Ok(CommitOutcome::Refused(CommitRefusal::Cancelled));
    }
    let partition = delivery.partition.clone();
    let checkpoint = expected_checkpoint(&*tx.queue(), &delivery)?;
    let expected_checkpoint = counter(&commit.expected_checkpoint)?;
    if expected_checkpoint != checkpoint {
        return Ok(CommitOutcome::Refused(CommitRefusal::CheckpointConflict {
            expected: expected_checkpoint,
            current: checkpoint,
        }));
    }
    let pinned = rev(&commit.state_generation)?;
    let state = tx.packages().package_state(&inv.package_id)?;
    if pinned != inv.state_generation || state.as_ref().map(|s| s.generation) != Some(pinned) {
        return Ok(CommitOutcome::Refused(CommitRefusal::GenerationMismatch {
            pinned,
            current: state.as_ref().map(|s| s.generation),
        }));
    }
    let current_revision = state.as_ref().map_or(0, |s| s.revision);
    let expected_revision = rev(&commit.expected_state_revision)?;
    if expected_revision != current_revision {
        return Ok(CommitOutcome::Refused(CommitRefusal::StateConflict {
            expected: expected_revision,
            current: current_revision,
        }));
    }

    // All checks passed: everything below commits together or not at all.
    let state_revision = match &commit.state_proposal {
        Some(proposal) => {
            let artifact = serde_json::to_value(proposal).map_err(|e| invalid(e.to_string()))?;
            tx.packages()
                .compare_and_set_state(
                    &inv.package_id,
                    pinned,
                    expected_revision,
                    json!({"artifact": artifact}),
                )?
                .revision
        }
        None => current_revision,
    };
    let aggregate = format!("{}/{}", inv.package_id, inv.subscription_id.as_str());
    let mut intents = Vec::with_capacity(commit.intents.len());
    for intent in &commit.intents {
        let call = serde_json::to_value(&intent.call).map_err(|e| invalid(e.to_string()))?;
        let kind = call["kind"].as_str().unwrap_or("call").to_ascii_lowercase();
        let args = jcs::canonicalize(&call).map_err(|e| invalid(format!("{e:?}")))?;
        let op = OperationId(tx.new_id(IdKind::Operation));
        tx.operations().record_intent(NewOperation {
            id: op.clone(),
            root_id: None,
            assignment_id: None,
            capability: format!("package.broker.{kind}"),
            schema_version: 1,
            args_digest: sha256_hex(args.as_bytes()),
            payload: json!({
                "packageId": inv.package_id,
                "subscriptionId": inv.subscription_id.as_str(),
                "eventId": event_id.0.as_str(),
                "actionKey": intent.action_key.as_str(),
                "call": call,
            }),
        })?;
        // One event per intent: a delivery is unique per (consumer, event).
        let intent_event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: intent_event.clone(),
            actor: inv.actor.clone(),
            project_id: None,
            root_id: None,
            operation_id: Some(op.clone()),
            aggregate_kind: "package_subscription".into(),
            aggregate_id: store::bounded(&aggregate),
            aggregate_revision: state_revision.max(1),
            kind: "PackageEffectIntentRecorded".into(),
            payload: json!({"actionKey": intent.action_key.as_str(), "sourceEvent": event_id.0.as_str()}),
        })?;
        let entry_id = tx.new_id(IdKind::QueueEntry);
        let ready_at_ms = tx.now_ms();
        tx.queue().enqueue(NewQueueEntry {
            id: entry_id,
            consumer: inv.broker_consumer.clone(),
            event_id: intent_event,
            root_id: None,
            operation_id: Some(op.clone()),
            partition: None,
            priority: 0,
            ready_at_ms,
            payload: json!({"packageId": inv.package_id, "actionKey": intent.action_key.as_str()}),
        })?;
        intents
            .push(json!({"actionKey": intent.action_key.as_str(), "operationId": op.0.as_str()}));
    }
    tx.queue()
        .settle(&inv.delivery_id, &inv.fence, Settlement::Completed)?;
    let checkpoint = match partition.as_deref() {
        Some(p) => tx.queue().checkpoint(&delivery.consumer, Some(p))?,
        None => delivery.event_sequence,
    };
    let record = ProcessingRecord {
        package_id: inv.package_id.clone(),
        subscription_id: inv.subscription_id.clone(),
        event_id: event_id.clone(),
        receipt_id: tx.new_id(IdKind::Command),
        payload_digest: digest,
        state_generation: pinned,
        state_revision,
        checkpoint,
        intents: Value::Array(intents),
    };
    tx.packages().record_processing(&record)?;
    let committed = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: committed,
        actor: inv.actor.clone(),
        project_id: None,
        root_id: None,
        operation_id: None,
        aggregate_kind: "package_subscription".into(),
        aggregate_id: store::bounded(&aggregate),
        aggregate_revision: state_revision.max(1),
        kind: "PackageEventCommitted".into(),
        payload: json!({
            "eventId": event_id.0.as_str(),
            "processingReceipt": record.receipt_id.as_str(),
            "checkpoint": checkpoint,
            "stateRevision": state_revision,
        }),
    })?;
    Ok(CommitOutcome::Accepted(accepted(&record)?))
}
