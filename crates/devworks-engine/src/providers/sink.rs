//! Trusted native output persistence. Provider bytes are data, never a storage callback.
use devworks_core::{
    context::InvocationContext,
    ports::{IdKind, storage::*},
    protocol::{Counter, EventId, Id, Protocol, Revision, provider::*},
    providers::{
        PreparedRequest,
        decode::{DecodedFrame, DecodedToolCall},
    },
    services::{
        artifacts::{ArtifactService, version_ref},
        sessions::{
            transcript::{BatchOutcome, SessionBatch, SessionWriter},
            turn::{self, TurnAssembler},
        },
        usage::{
            accounting::{self, AttemptEnd},
            host::{AccountingWriteFence, OwnedInferenceSink},
        },
    },
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

/// Host-only completed result. Embeddings have their own durable vector artifacts; they never
/// masquerade as assistant text or alter a conversation pin.
#[derive(Clone, Debug, Default)]
pub struct ProviderResult {
    pub vector_artifacts: Vec<ArtifactRef>,
    pub result_artifact: Option<ArtifactRef>,
    pub outcome: Option<AttemptEnd>,
}
pub type ResultChannel = Arc<Mutex<ProviderResult>>;

pub struct ProviderSink {
    store: Arc<dyn ApplicationStore>,
    artifacts: Arc<ArtifactService>,
    context: InvocationContext,
    prepared: PreparedRequest,
    assembler: TurnAssembler,
    sequence: i64,
    embedding: Option<(Id, u32, Vec<ArtifactRef>)>,
    vectors: BTreeMap<u64, Vec<f32>>,
    result: ResultChannel,
}
impl ProviderSink {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        artifacts: Arc<ArtifactService>,
        context: InvocationContext,
        prepared: PreparedRequest,
        embedding: Option<(Id, u32, Vec<ArtifactRef>)>,
        result: ResultChannel,
    ) -> Self {
        Self {
            store,
            artifacts,
            context,
            prepared,
            assembler: TurnAssembler::new(),
            sequence: 0,
            embedding,
            vectors: BTreeMap::new(),
            result,
        }
    }
    fn event(
        &mut self,
        attempt: &StoredInferenceAttempt,
        body: ProviderBody,
        fence: &AccountingWriteFence,
    ) -> Result<(), StorageError> {
        let mut assembler = self.assembler.clone();
        let sequence = self.sequence + 1;
        assembler
            .accept(&ProviderEvent {
                protocol: Protocol::Draft1,
                request_id: self.prepared.identity.request_id.clone(),
                attempt_id: self.prepared.identity.attempt_id.clone(),
                binding: self.prepared.identity.binding.clone(),
                sequence: Revision::new(sequence).map_err(invalid)?,
                event: body,
            })
            .map_err(|_| invalid("invalid normalized provider turn"))?;
        let bodies = assembler.take();
        self.append(attempt, bodies, fence)?;
        self.assembler = assembler;
        self.sequence = sequence;
        Ok(())
    }
    fn append(
        &self,
        attempt: &StoredInferenceAttempt,
        bodies: Vec<TranscriptBody>,
        fence: &AccountingWriteFence,
    ) -> Result<(), StorageError> {
        if bodies.is_empty() {
            return Ok(());
        }
        let store = self.store.clone();
        let context = self.context.clone();
        let attempt = attempt.clone();
        let epoch = self.prepared.identity.binding.context_epoch.get();
        fence.write(store.as_ref(), move |tx| {
            validate(tx, &context, &attempt)?;
            let session = attempt
                .session_id
                .as_ref()
                .ok_or(StorageError::ScopeDenied)?;
            for chunk in bodies.chunks(128) {
                let current = tx
                    .sessions()
                    .session(session)?
                    .ok_or(StorageError::ScopeDenied)?;
                let batch_id = tx.new_id(IdKind::TranscriptBatch);
                let batch = turn::batch_of(
                    session,
                    batch_id,
                    epoch,
                    current.next_sequence,
                    chunk.to_vec(),
                    || tx.new_id(IdKind::TranscriptEntry),
                );
                match SessionWriter::append_in(tx, &context, &SessionBatch::entries(batch))? {
                    BatchOutcome::Accepted(_) | BatchOutcome::Replayed(_) => {}
                    BatchOutcome::Refused(_) => return Err(StorageError::ScopeDenied),
                }
            }
            Ok(())
        })
    }
    fn artifact(
        &self,
        attempt: &StoredInferenceAttempt,
        bytes: &[u8],
        title: &str,
        media_type: &str,
        fence: &AccountingWriteFence,
    ) -> Result<ArtifactRef, StorageError> {
        let service = self.artifacts.clone();
        let store = self.store.clone();
        let context = self.context.clone();
        let attempt = attempt.clone();
        let title = title.to_owned();
        let media_type = media_type.to_owned();
        fence.publication(|| {
            // Existing artifact staging journals and file publication run off the DB writer.
            let staged=service.stage_host_bytes(store.as_ref(),bytes,json!({"kind":"ProviderOutput","requestId":attempt.request_id,"attemptId":attempt.attempt_id}))?
                .map_err(|_|invalid("provider artifact staging failed"))?;
            let pending=staged.clone(); let service_in=service.clone();
            let committed=run_write(store.as_ref(),WriteOptions::default(),move|tx| {
                validate(tx,&context,&attempt)?;
                let scope=attempt.project_id.clone().map_or(ArtifactScope::Workspace,ArtifactScope::Project);
                let version=service_in.commit_host_bytes_in_scope(tx,&pending, devworks_core::services::artifacts::host::HostArtifactPublication {title:&title,media_type:&media_type,provenance:json!({"requestId":attempt.request_id,"attemptId":attempt.attempt_id,"actorId":attempt.actor_id,"sessionId":attempt.session_id,"workId":attempt.work_id,"dispatch":accounting::snapshot(&attempt)?.dispatch}),
                    scope,owner:attempt.actor_id.clone()})?;
                let reference=version_ref(&version);
                let event=EventId(tx.new_id(IdKind::Event));
                tx.history().append(NewEvent {id:event,actor:attempt.actor_id.clone(),project_id:attempt.project_id.clone(),root_id:attempt.execution_root_id.clone(),operation_id:Some(attempt.operation_id.clone()),
                    aggregate_kind:"provider_output".into(),aggregate_id:attempt.attempt_id.as_str().into(),aggregate_revision:1,kind:"ProviderArtifactRecorded".into(),payload:json!({"artifact":reference,"mediaType":media_type,"requestId":attempt.request_id})})?;
                Ok(reference)
            });
            service.settle_host_bytes(store.as_ref(),&staged,committed.is_ok());
            committed
        })
    }
    fn call(
        &mut self,
        attempt: &StoredInferenceAttempt,
        call: DecodedToolCall,
        fence: &AccountingWriteFence,
    ) -> Result<(), StorageError> {
        let tool = self
            .prepared
            .tools
            .iter()
            .find(|t| t.wire_name == call.name)
            .ok_or(StorageError::ScopeDenied)?
            .clone();
        let arguments = self.artifact(
            attempt,
            call.arguments_raw.as_bytes(),
            "Provider tool arguments",
            "application/json",
            fence,
        )?;
        let bytes =
            serde_json::to_vec(&call.native_item).map_err(|_| invalid("native tool JSON"))?;
        let native_item = self.artifact(
            attempt,
            &bytes,
            "Provider tool item",
            "application/json",
            fence,
        )?;
        self.event(
            attempt,
            ProviderBody::ToolCallComplete {
                tool_call: CompleteToolCall {
                    call: ToolCallKey {
                        attempt_id: self.prepared.identity.attempt_id.clone(),
                        native_call_id: call.native_call_id,
                        ordinal: Counter::new(
                            i64::try_from(call.ordinal).map_err(|_| invalid("tool ordinal"))?,
                        )
                        .map_err(invalid)?,
                    },
                    capability_id: tool.capability_id,
                    input_schema: tool.input_schema,
                    arguments,
                    native_item,
                },
            },
            fence,
        )
    }
}
fn invalid(message: impl Into<String>) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
/// Every publication rechecks captured authority and execution ownership in its commit unit.
fn validate(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    attempt: &StoredInferenceAttempt,
) -> Result<(), StorageError> {
    let bound = accounting::snapshot(attempt)?;
    if &tx.store().store_id != ctx.store_id()
        || tx.store().restore_epoch != ctx.restore_epoch().get()
        || tx.store().boot_id.as_ref() != Some(&bound.admitted_boot_id)
        || ctx.principal() != &attempt.actor_id
        || ctx.project_id() != attempt.project_id.as_ref()
        || tx.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
    {
        return Err(StorageError::ScopeDenied);
    }
    let current = tx
        .usage()
        .inference_attempt(&attempt.attempt_id)?
        .ok_or(StorageError::ScopeDenied)?;
    if current.state != "Dispatched"
        || current.request_id != attempt.request_id
        || current.actor_id != attempt.actor_id
        || current.pricing_snapshot != attempt.pricing_snapshot
    {
        return Err(StorageError::ScopeDenied);
    }
    if let Some(revision) = bound.principal_revision
        && !tx
            .authority()
            .principal(&attempt.actor_id)?
            .is_some_and(|p| p.active && p.revision == revision)
    {
        return Err(StorageError::ScopeDenied);
    }
    if let Some(digest) = &bound.policy_digest
        && &devworks_core::authority::policy::resolve_policy(
            tx.configuration(),
            bound.authority_project_id.as_ref(),
        )?
        .digest()
            != digest
    {
        return Err(StorageError::ScopeDenied);
    }
    if let Some(session) = &attempt.session_id {
        let current = tx
            .sessions()
            .session(session)?
            .ok_or(StorageError::ScopeDenied)?;
        if current.closed()
            || Some(current.context_epoch) != bound.context_epoch
            || ctx.session_id() != Some(session)
        {
            return Err(StorageError::ScopeDenied);
        }
    }
    if let Some(proof) = &bound.dispatch {
        let assignment = match &proof.assignment_id {
            Some(id) => tx.runs().assignment(id)?,
            None => devworks_core::services::runs::coordinator(tx.runs(), &proof.root_id)?,
        }
        .ok_or(StorageError::ScopeDenied)?;
        let activation = tx
            .runs()
            .activation(&proof.execution_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if assignment.generation != proof.assignment_generation
            || assignment.execution_id.as_ref() != Some(&proof.execution_id)
            || assignment.root_id != proof.root_id
            || !assignment.state.is_live()
            || activation.owner_epoch != proof.owner_epoch
            || activation.boot_id != bound.admitted_boot_id
            || !matches!(
                activation.phase,
                ActivationPhase::Claimed | ActivationPhase::Running
            )
            || tx
                .runs()
                .root_control(&proof.root_id)?
                .is_some_and(|r| r.desired == DesiredExecution::Paused)
        {
            return Err(StorageError::ScopeDenied);
        }
        if let Some(session) = &attempt.session_id
            && !tx.runs().session_turn(session)?.is_some_and(|t| {
                t.turn_id == proof.turn_id
                    && t.execution_id == proof.execution_id
                    && t.owner_epoch == proof.owner_epoch
            })
        {
            return Err(StorageError::ScopeDenied);
        }
    }
    Ok(())
}
impl OwnedInferenceSink for ProviderSink {
    fn accept(
        &mut self,
        attempt: &StoredInferenceAttempt,
        frame: DecodedFrame,
        fence: &AccountingWriteFence,
    ) -> Result<(), StorageError> {
        match frame {
            DecodedFrame::TextDelta { block_index, text } => {
                if self.embedding.is_some() {
                    return Err(invalid("embedding response contains generation text"));
                }
                for chunk in turn::chunks(&text) {
                    self.event(
                        attempt,
                        ProviderBody::TextDelta {
                            block_index: Counter::new(
                                i64::try_from(block_index).map_err(|_| invalid("block index"))?,
                            )
                            .map_err(invalid)?,
                            text: chunk.into(),
                        },
                        fence,
                    )?;
                }
            }
            DecodedFrame::ToolCallComplete { call } => self.call(attempt, call, fence)?,
            DecodedFrame::Native { observation } => {
                self.artifact(
                    attempt,
                    observation.raw_json.as_bytes(),
                    "Provider native observation",
                    "application/json",
                    fence,
                )?;
                if let Some(value) = observation.continuation {
                    let bytes = serde_json::to_vec(&value)
                        .map_err(|_| invalid("native continuation JSON"))?;
                    let artifact = self.artifact(
                        attempt,
                        &bytes,
                        "Provider native continuation",
                        "application/json",
                        fence,
                    )?;
                    self.event(
                        attempt,
                        ProviderBody::NativeContinuation {
                            binding: self.prepared.identity.binding.clone(),
                            artifact,
                        },
                        fence,
                    )?;
                }
            }
            DecodedFrame::Usage { observation } => {
                self.artifact(
                    attempt,
                    observation.raw_json.as_bytes(),
                    "Provider usage observation",
                    "application/json",
                    fence,
                )?;
            }
            DecodedFrame::Embedding { index, vector } => {
                let Some((_, dimensions, inputs)) = &self.embedding else {
                    return Err(invalid("unexpected embedding"));
                };
                if index >= inputs.len() as u64
                    || vector.len() != *dimensions as usize
                    || vector.iter().any(|v| !v.is_finite())
                    || self.vectors.insert(index, vector).is_some()
                {
                    return Err(invalid("invalid embedding batch"));
                }
            }
            DecodedFrame::Identity {
                model,
                generation_id,
            } => {
                let bytes =
                    serde_json::to_vec(&json!({"model":model,"generationId":generation_id}))
                        .map_err(|_| invalid("identity JSON"))?;
                self.artifact(
                    attempt,
                    &bytes,
                    "Provider reported identity",
                    "application/json",
                    fence,
                )?;
            }
            DecodedFrame::Error { error } => {
                let bytes=serde_json::to_vec(&json!({"kind":format!("{:?}",error.kind),"code":error.code,"message":error.message})).map_err(|_|invalid("error JSON"))?;
                self.artifact(
                    attempt,
                    &bytes,
                    "Provider error observation",
                    "application/json",
                    fence,
                )?;
            }
            DecodedFrame::Heartbeat => {}
            DecodedFrame::Terminal { .. } => {
                return Err(invalid("terminal must follow physical transport closure"));
            }
        }
        Ok(())
    }
    fn finish(
        &mut self,
        attempt: &StoredInferenceAttempt,
        outcome: AttemptEnd,
        fence: &AccountingWriteFence,
    ) -> Result<(), StorageError> {
        if let Some((space, dimensions, inputs)) = &self.embedding {
            if outcome == AttemptEnd::Completed {
                if self.vectors.len() != inputs.len()
                    || !(0..inputs.len() as u64).all(|i| self.vectors.contains_key(&i))
                {
                    return Err(invalid("embedding population incomplete"));
                }
                let mut artifacts = Vec::new();
                for vector in self.vectors.values() {
                    let bytes: Vec<_> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
                    artifacts.push(self.artifact(
                        attempt,
                        &bytes,
                        "Provider embedding vector",
                        "application/vnd.devworks.float32-le",
                        fence,
                    )?);
                }
                let bytes=serde_json::to_vec(&json!({"version":1,"kind":"EmbeddingResult","spaceId":space,"dimensions":dimensions,"requestId":attempt.request_id,"attemptId":attempt.attempt_id,
                    "binding":self.prepared.identity.binding,"inputs":inputs,"vectors":artifacts})).map_err(|_|invalid("embedding result JSON"))?;
                let result = self.artifact(
                    attempt,
                    &bytes,
                    "Provider embedding result",
                    "application/vnd.devworks.embedding-result+json",
                    fence,
                )?;
                let mut channel = self.result.lock().unwrap_or_else(|p| p.into_inner());
                channel.vector_artifacts = artifacts;
                channel.result_artifact = Some(result);
                channel.outcome = Some(outcome);
            } else {
                self.result
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .outcome = Some(outcome);
            }
            return Ok(());
        }
        let terminal = match outcome {
            AttemptEnd::Completed => TurnOutcome::Completed,
            AttemptEnd::ToolCallsReady => TurnOutcome::ToolCallsReady,
            AttemptEnd::Incomplete => TurnOutcome::Incomplete,
            AttemptEnd::Failed => TurnOutcome::Failed,
            AttemptEnd::Cancelled => TurnOutcome::Cancelled,
            AttemptEnd::UnknownTail => TurnOutcome::OutcomeUnknown,
        };
        self.event(
            attempt,
            ProviderBody::Terminal {
                outcome: terminal,
                native_stop_reason: None,
            },
            fence,
        )?;
        self.result
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .outcome = Some(outcome);
        Ok(())
    }
}
