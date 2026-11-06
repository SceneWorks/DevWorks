//! Trusted native output persistence. Provider bytes are data, never a storage callback.
use devworks_core::{
    context::InvocationContext,
    ports::{IdKind, storage::*},
    protocol::{Counter, EventId, Protocol, Revision, provider::*},
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
use serde_json::{Value, json};
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
    pub faults: Vec<String>,
    pub retry_at_ms: Option<i64>,
}
pub type ResultChannel = Arc<Mutex<ProviderResult>>;

const MAX_CONTENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_CONTENT_ITEMS: usize = 65536;

enum ArtifactKind {
    Evidence,
    NativeContinuation,
}
pub struct ProviderSink {
    store: Arc<dyn ApplicationStore>,
    artifacts: Arc<ArtifactService>,
    context: InvocationContext,
    prepared: PreparedRequest,
    assembler: TurnAssembler,
    /// Private content slots use provider item/sub-block positions, independently of when
    /// completion evidence becomes releasable. No placeholder enters the transcript.
    content: BTreeMap<(u64, u64), Vec<ProviderBody>>,
    content_bytes: usize,
    content_items: usize,
    live: Option<Arc<super::live::LiveTurns>>,
    live_session: Option<devworks_core::protocol::Id>,
    last_sequence: Option<i64>,
    sequence: i64,
    embedding: Option<(
        devworks_core::protocol::data::EmbeddingSpace,
        Vec<ArtifactRef>,
    )>,
    vectors: BTreeMap<u64, Vec<f32>>,
    result: ResultChannel,
    limit: devworks_core::providers::limits::LimitReading,
    retry_at: Option<i64>,
    http_status: u16,
    entitlement_refused: bool,
    limited: bool,
    access: Option<devworks_core::providers::ProviderAccessBasis>,
    evidence: Vec<Value>,
    evidence_bytes: usize,
}
impl ProviderSink {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        artifacts: Arc<ArtifactService>,
        context: InvocationContext,
        prepared: PreparedRequest,
        embedding: Option<(
            devworks_core::protocol::data::EmbeddingSpace,
            Vec<ArtifactRef>,
        )>,
        result: ResultChannel,
    ) -> Self {
        Self {
            store,
            artifacts,
            context,
            prepared,
            assembler: TurnAssembler::positioned(),
            content: BTreeMap::new(),
            content_bytes: 0,
            content_items: 0,
            live: None,
            live_session: None,
            last_sequence: None,
            sequence: 0,
            embedding,
            vectors: BTreeMap::new(),
            result,
            limit: Default::default(),
            retry_at: None,
            http_status: 0,
            entitlement_refused: false,
            limited: false,
            access: None,
            evidence: Vec::new(),
            evidence_bytes: 0,
        }
    }
    pub(crate) fn with_live(mut self, live: Arc<super::live::LiveTurns>) -> Self {
        self.live = Some(live);
        self
    }
    fn evidence(
        &mut self,
        attempt: &StoredInferenceAttempt,
        value: Value,
        fence: &AccountingWriteFence,
    ) -> Result<(), StorageError> {
        self.evidence_bytes = self.evidence_bytes.saturating_add(value.to_string().len());
        self.evidence.push(value);
        if self.evidence_bytes >= 1024 * 1024 {
            self.flush_evidence(attempt, fence)?;
        }
        Ok(())
    }
    fn flush_evidence(
        &mut self,
        attempt: &StoredInferenceAttempt,
        fence: &AccountingWriteFence,
    ) -> Result<(), StorageError> {
        if self.evidence.is_empty() {
            return Ok(());
        }
        let bytes=serde_json::to_vec(&json!({"version":1,"requestId":attempt.request_id,"attemptId":attempt.attempt_id,"binding":self.prepared.identity.binding,"observations":self.evidence})).map_err(|_|invalid("provider evidence JSON"))?;
        self.artifact(
            attempt,
            &bytes,
            "Provider evidence batch",
            "application/vnd.devworks.provider-evidence+json",
            ArtifactKind::Evidence,
            fence,
        )?;
        self.evidence.clear();
        self.evidence_bytes = 0;
        Ok(())
    }
    fn stage_content(
        &mut self,
        position: (u64, u64),
        body: ProviderBody,
    ) -> Result<(), StorageError> {
        // The decoder independently bounds the complete raw response to 32 MiB and 65536
        // observations. Bound this private representation too, including generated refs.
        let bytes = serde_json::to_vec(&body)
            .map_err(|_| invalid("provider content JSON"))?
            .len();
        if self.content_items >= MAX_CONTENT_ITEMS
            || self.content_bytes.saturating_add(bytes) > MAX_CONTENT_BYTES
        {
            return Err(invalid("private provider turn exceeds content bound"));
        }
        self.content_bytes += bytes;
        self.content_items += 1;
        self.content.entry(position).or_default().push(body);
        Ok(())
    }

    fn position(&self, ordinal: u64, part: u64, kind: u64) -> (u64, u64) {
        if matches!(
            self.prepared.profile,
            EndpointProfile::CompatibleChat | EndpointProfile::OpenRouterChat
        ) {
            // Chat exposes a single reasoning object, then assistant content and native calls.
            (kind, if kind == 2 { ordinal } else { part })
        } else {
            (ordinal, part)
        }
    }

    fn event(
        &mut self,
        attempt: &StoredInferenceAttempt,
        body: ProviderBody,
        fence: &AccountingWriteFence,
    ) -> Result<(), StorageError> {
        let sequence = self.sequence + 1;
        self.assembler
            .accept(&ProviderEvent {
                protocol: Protocol::Draft1,
                request_id: self.prepared.identity.request_id.clone(),
                attempt_id: self.prepared.identity.attempt_id.clone(),
                binding: self.prepared.identity.binding.clone(),
                sequence: Revision::new(sequence).map_err(invalid)?,
                event: body,
            })
            .map_err(|_| invalid("invalid normalized provider turn"))?;
        let bodies = self.assembler.take();
        if let Some(sequence) = self.append(attempt, bodies, fence)? {
            self.last_sequence = Some(sequence);
        }
        self.sequence = sequence;
        Ok(())
    }
    fn append(
        &self,
        attempt: &StoredInferenceAttempt,
        bodies: Vec<TranscriptBody>,
        fence: &AccountingWriteFence,
    ) -> Result<Option<i64>, StorageError> {
        if bodies.is_empty() {
            return Ok(None);
        }
        let store = self.store.clone();
        let context = self.context.clone();
        let attempt = attempt.clone();
        let epoch = self.prepared.identity.binding.context_epoch.get();
        let binding = self.prepared.identity.binding.clone();
        fence.write(store.as_ref(), move |tx| {
            validate(tx, &context, &attempt)?;
            validate_binding(tx, &binding)?;
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
            Ok(Some(
                tx.sessions()
                    .session(session)?
                    .ok_or(StorageError::ScopeDenied)?
                    .last_durable_sequence(),
            ))
        })
    }
    fn artifact(
        &self,
        attempt: &StoredInferenceAttempt,
        bytes: &[u8],
        title: &str,
        media_type: &str,
        kind: ArtifactKind,
        fence: &AccountingWriteFence,
    ) -> Result<ArtifactRef, StorageError> {
        let service = self.artifacts.clone();
        let store = self.store.clone();
        let context = self.context.clone();
        let attempt = attempt.clone();
        let title = title.to_owned();
        let media_type = media_type.to_owned();
        let binding = self.prepared.identity.binding.clone();
        let artifact_kind = match kind {
            ArtifactKind::Evidence => "ProviderEvidence",
            ArtifactKind::NativeContinuation => "ProviderNativeContinuation",
        };
        fence.publication(|| {
            // Existing artifact staging journals and file publication run off the DB writer.
            let staged=service.stage_host_bytes(store.as_ref(),bytes,json!({"kind":"ProviderOutput","requestId":attempt.request_id,"attemptId":attempt.attempt_id}))?
                .map_err(|_|invalid("provider artifact staging failed"))?;
            let pending=staged.clone(); let service_in=service.clone();
            let committed=run_write(store.as_ref(),WriteOptions::default(),move|tx| {
                validate(tx,&context,&attempt)?;
                validate_binding(tx,&binding)?;
                let scope=attempt.project_id.clone().map_or(ArtifactScope::Workspace,ArtifactScope::Project);
                let version=service_in.commit_host_bytes_in_scope(tx,&pending, devworks_core::services::artifacts::host::HostArtifactPublication {title:&title,media_type:&media_type,provenance:json!({"kind":artifact_kind,"nativeBinding":binding,"requestId":attempt.request_id,"attemptId":attempt.attempt_id,"actorId":attempt.actor_id,"sessionId":attempt.session_id,"workId":attempt.work_id,"dispatch":accounting::snapshot(&attempt)?.dispatch}),
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
            ArtifactKind::Evidence,
            fence,
        )?;
        let bytes =
            serde_json::to_vec(&call.native_item).map_err(|_| invalid("native tool JSON"))?;
        let native_item = self.artifact(
            attempt,
            &bytes,
            "Provider tool item",
            "application/json",
            ArtifactKind::Evidence,
            fence,
        )?;
        let position = self.position(call.ordinal, 0, 2);
        self.stage_content(
            position,
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
        )
    }
}
fn invalid(message: impl Into<String>) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
pub(crate) fn validate_binding(
    tx: &mut dyn WriteTransaction,
    binding: &NativeBinding,
) -> Result<(), StorageError> {
    let current = tx
        .router()
        .connection(&binding.connection_id)?
        .ok_or(StorageError::ScopeDenied)?;
    if current.account_revision != binding.account_revision.get()
        || current.profile != format!("{:?}", binding.endpoint_profile)
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}

/// Every publication rechecks captured authority and execution ownership in its commit unit.
pub(crate) fn validate(
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
    fn response_head(
        &mut self,
        attempt: &StoredInferenceAttempt,
        head: devworks_core::providers::ProviderResponseHead,
        fence: &AccountingWriteFence,
    ) -> Result<(), StorageError> {
        let store = self.store.clone();
        let current = attempt.clone();
        let context = self.context.clone();
        let now = fence.write(store.as_ref(), move |tx| {
            validate(tx, &context, &current)?;
            Ok(tx.now_ms())
        })?;
        self.http_status = head.status;
        self.limit =
            devworks_core::providers::limits::headers(&self.prepared.profile, &head.headers, now);
        self.retry_at = head
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("retry-after"))
            .and_then(|(_, v)| devworks_core::providers::limits::retry_after(now, v));
        self.evidence(
            attempt,
            json!({"kind":"host_http_response","status":head.status,"headers":head.headers}),
            fence,
        )?;
        self.access = Some(head.access);
        Ok(())
    }

    fn accept(
        &mut self,
        attempt: &StoredInferenceAttempt,
        frame: DecodedFrame,
        fence: &AccountingWriteFence,
    ) -> Result<(), StorageError> {
        match frame {
            DecodedFrame::TextDelta {
                block_index,
                content_index,
                text,
            } => {
                if self.embedding.is_some() {
                    return Err(invalid("embedding response contains generation text"));
                }
                let position = self.position(block_index, content_index, 1);
                for chunk in turn::chunks(&text) {
                    self.stage_content(
                        position,
                        ProviderBody::TextDelta {
                            block_index: Counter::new(
                                i64::try_from(block_index).map_err(|_| invalid("block index"))?,
                            )
                            .map_err(invalid)?,
                            text: chunk.into(),
                        },
                    )?;
                }
                if let (Some(live), Some(session)) = (&self.live, &attempt.session_id) {
                    let store = self.store.clone();
                    let context = self.context.clone();
                    let current = attempt.clone();
                    let live = live.clone();
                    let session = session.clone();
                    let preview = text.clone();
                    let binding = self.prepared.identity.binding.clone();
                    let epoch = binding.context_epoch.get();
                    fence.publication(|| {
                        run_write(store.as_ref(), WriteOptions::default(), move |tx| {
                            validate(tx, &context, &current)?;
                            validate_binding(tx, &binding)?;
                            Ok(())
                        })?;
                        live.text(
                            &session,
                            &self.prepared.identity.attempt_id,
                            epoch,
                            &preview,
                        )
                    })?;
                    self.live_session = attempt.session_id.clone();
                }
            }
            DecodedFrame::ToolCallComplete { call } => self.call(attempt, call, fence)?,
            DecodedFrame::Native { observation } => {
                if observation.value.get("error").is_some()
                    || observation.value.get("detail").is_some()
                {
                    let store = self.store.clone();
                    let current = attempt.clone();
                    let context = self.context.clone();
                    let now = fence.write(store.as_ref(), move |tx| {
                        validate(tx, &context, &current)?;
                        Ok(tx.now_ms())
                    })?;
                    self.retry_at =
                        devworks_core::providers::limits::body_reset(&observation.value, now)
                            .or(self.retry_at);
                }
                self.evidence(attempt,json!({"kind":observation.kind,"ordinal":observation.ordinal,"rawJson":observation.raw_json,"replay":format!("{:?}",observation.replay)}),fence)?;
                if let Some(value) = observation.continuation {
                    let bytes = serde_json::to_vec(&value)
                        .map_err(|_| invalid("native continuation JSON"))?;
                    let artifact = self.artifact(
                        attempt,
                        &bytes,
                        "Provider native continuation",
                        "application/json",
                        ArtifactKind::NativeContinuation,
                        fence,
                    )?;
                    let position = self.position(
                        observation
                            .ordinal
                            .ok_or_else(|| invalid("native content position"))?,
                        0,
                        0,
                    );
                    self.stage_content(
                        position,
                        ProviderBody::NativeContinuation {
                            binding: self.prepared.identity.binding.clone(),
                            artifact,
                        },
                    )?;
                }
            }
            DecodedFrame::Usage { observation } => {
                self.evidence(attempt,json!({"kind":"usage","rawJson":observation.raw_json,"inputSemantics":format!("{:?}",observation.input_semantics),"validationErrors":observation.validation_errors}),fence)?;
            }
            DecodedFrame::Embedding { index, vector } => {
                let Some((space, inputs)) = &self.embedding else {
                    return Err(invalid("unexpected embedding"));
                };
                if index >= inputs.len() as u64
                    || vector.len() != space.dimensions as usize
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
                self.evidence(attempt,json!({"kind":"reported_identity","rawJson":String::from_utf8(bytes).map_err(|_|invalid("identity UTF8"))?}),fence)?;
            }
            DecodedFrame::Error { error } => {
                self.entitlement_refused |= matches!(
                    error.kind,
                    devworks_core::providers::decode::DecodeErrorKind::ModelUnavailable
                );
                self.limited |= matches!(
                    error.kind,
                    devworks_core::providers::decode::DecodeErrorKind::Cooldown
                        | devworks_core::providers::decode::DecodeErrorKind::Overloaded
                );
                self.result
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .faults
                    .push(format!(
                        "{:?}:{}",
                        error.kind,
                        error.code.as_deref().unwrap_or("provider_error")
                    ));

                let bytes=serde_json::to_vec(&json!({"kind":format!("{:?}",error.kind),"code":error.code,"message":error.message})).map_err(|_|invalid("error JSON"))?;
                self.evidence(attempt,json!({"kind":"error","rawJson":String::from_utf8(bytes).map_err(|_|invalid("error UTF8"))?}),fence)?;
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
        self.finish_output(attempt, outcome, fence)?;
        let result = self
            .result
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let context = self.context.clone();
        let current = attempt.clone();
        let store = self.store.clone();
        fence.write(store.as_ref(),move|tx| {
            validate(tx,&context,&current)?;
            let event=EventId(tx.new_id(IdKind::Event));
            tx.history().append(NewEvent {id:event,actor:current.actor_id,project_id:current.project_id,root_id:current.execution_root_id,operation_id:Some(current.operation_id),aggregate_kind:"provider_result".into(),aggregate_id:current.attempt_id.as_str().into(),aggregate_revision:1,kind:"ProviderResultRecorded".into(),
                payload:json!({"outcome":result.outcome.map(|o|o.label()),"vectorArtifacts":result.vector_artifacts,"resultArtifact":result.result_artifact,"faults":result.faults,"retryAtMs":result.retry_at_ms})})?;
            Ok(())
        })
    }
}

impl ProviderSink {
    fn finish_output(
        &mut self,
        attempt: &StoredInferenceAttempt,
        outcome: AttemptEnd,
        fence: &AccountingWriteFence,
    ) -> Result<(), StorageError> {
        if let Some(access) = self.access.as_ref().filter(|a| {
            !self.entitlement_refused
                && matches!(self.http_status, 401..=403)
                && !(self.http_status == 401 && a.oauth)
        }) {
            let store = self.store.clone();
            let current = attempt.clone();
            let context = self.context.clone();
            let access = access.clone();
            let status = self.http_status;
            fence.write(store.as_ref(), move |tx| {
                validate(tx, &context, &current)?;
                devworks_core::services::router::connections::record_inference_rejection(
                    tx, &access, status,
                )
                .map(|_| ())
            })?;
        }
        self.flush_evidence(attempt, fence)?;
        if !self.entitlement_refused
            && (self.limit.exhausted || self.limited || self.http_status == 429)
        {
            let reset = self.limit.reset_at_ms.or(self.retry_at);
            if let Some(until_ms) = reset {
                let store = self.store.clone();
                let current = attempt.clone();
                let context = self.context.clone();
                fence.write(store.as_ref(), move |tx| {
                    validate(tx, &context, &current)?;
                    if until_ms > tx.now_ms() {
                        let now = tx.now_ms();
                        tx.router().record_cooldown(&StoredCooldown {
                            model_id: current.catalog_model_id,
                            until_ms,
                            reason: "RateLimited".into(),
                            observed_at_ms: now,
                        })?;
                    }
                    Ok(())
                })?;
                self.result
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .retry_at_ms = Some(until_ms);
            }
        }
        if let Some((space, inputs)) = &self.embedding {
            let dimensions = space.dimensions;
            if outcome == AttemptEnd::Completed {
                if self.vectors.len() != inputs.len()
                    || !(0..inputs.len() as u64).all(|i| self.vectors.contains_key(&i))
                {
                    return Err(invalid("embedding population incomplete"));
                }
                let vector_bytes: Vec<Vec<u8>> = self
                    .vectors
                    .values()
                    .map(|vector| vector.iter().flat_map(|v| v.to_le_bytes()).collect())
                    .collect();
                let descriptors:Vec<_>=vector_bytes.iter().enumerate().map(|(index,bytes)|json!({"index":index,"sha256":devworks_core::fingerprint::sha256_hex(bytes),"byteCount":bytes.len()})).collect();
                let manifest=serde_json::to_vec(&json!({"version":1,"kind":"EmbeddingResult","space":space,"dimensions":dimensions,"requestId":attempt.request_id,"attemptId":attempt.attempt_id,
                    "binding":self.prepared.identity.binding,"registration":accounting::snapshot(attempt)?.selection,"inputs":inputs,"vectors":descriptors})).map_err(|_|invalid("embedding result JSON"))?;
                let mut bytes = vector_bytes;
                bytes.push(manifest);
                let service = self.artifacts.clone();
                let store = self.store.clone();
                let context = self.context.clone();
                let current = attempt.clone();
                let binding = self.prepared.identity.binding.clone();
                let mut published=fence.publication(|| {
                    let mut staged=Vec::new();
                    for bytes in &bytes {
                        match service.stage_host_bytes(store.as_ref(),bytes,json!({"kind":"EmbeddingResult","requestId":current.request_id,"attemptId":current.attempt_id}))? {
                            Ok(file)=>staged.push(file),
                            Err(_)=>{for file in &staged {service.settle_host_bytes(store.as_ref(),file,false);}return Err(invalid("embedding staging failed"));}
                        }
                    }
                    let pending=staged.clone();let native=service.clone();
                    let committed=run_write(store.as_ref(),WriteOptions::default(),move|tx| {
                        validate(tx,&context,&current)?;
                        validate_binding(tx,&binding)?;
                        let mut references=Vec::new();
                        for (index,file) in pending.iter().enumerate() {
                            let version=native.commit_host_bytes_in_scope(tx,file,devworks_core::services::artifacts::host::HostArtifactPublication {
                                title:if index+1==pending.len(){"Provider embedding result"}else{"Provider embedding vector"},
                                media_type:if index+1==pending.len(){"application/vnd.devworks.embedding-result+json"}else{"application/vnd.devworks.float32-le"},
                                provenance:json!({"requestId":current.request_id,"attemptId":current.attempt_id,"inputIndex":index,"binding":binding}),
                                scope:current.project_id.clone().map_or(ArtifactScope::Workspace,ArtifactScope::Project),owner:current.actor_id.clone(),
                            })?;
                            references.push(version_ref(&version));
                        }
                        let event=EventId(tx.new_id(IdKind::Event));
                        tx.history().append(NewEvent {id:event,actor:current.actor_id,project_id:current.project_id,root_id:current.execution_root_id,operation_id:Some(current.operation_id),aggregate_kind:"provider_output".into(),aggregate_id:current.attempt_id.as_str().into(),aggregate_revision:1,
                            kind:"ProviderEmbeddingResultRecorded".into(),payload:json!({"requestId":current.request_id,"attemptId":current.attempt_id,"binding":binding,"artifacts":references})})?;
                        Ok(references)
                    });
                    for file in &staged {service.settle_host_bytes(store.as_ref(),file,committed.is_ok());}
                    committed
                })?;
                let result = published
                    .pop()
                    .ok_or_else(|| invalid("missing embedding manifest"))?;
                let artifacts = published;
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
        let successful = matches!(outcome, AttemptEnd::Completed | AttemptEnd::ToolCallsReady);
        for (block, (_, bodies)) in std::mem::take(&mut self.content).into_iter().enumerate() {
            for mut body in bodies {
                if !successful && !matches!(body, ProviderBody::TextDelta { .. }) {
                    continue;
                }
                if let ProviderBody::TextDelta { block_index, .. } = &mut body {
                    *block_index = Counter::new(
                        i64::try_from(block).map_err(|_| invalid("content position"))?,
                    )
                    .map_err(invalid)?;
                }
                self.event(attempt, body, fence)?;
            }
        }
        self.event(
            attempt,
            ProviderBody::Terminal {
                outcome: terminal,
                native_stop_reason: None,
            },
            fence,
        )?;
        if let (Some(live), Some(session), Some(sequence)) =
            (&self.live, &self.live_session, self.last_sequence)
        {
            fence.publication(|| {
                live.finalize(session, &self.prepared.identity.attempt_id, sequence);
                Ok(())
            })?;
        }
        self.result
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .outcome = Some(outcome);
        Ok(())
    }
}

impl Drop for ProviderSink {
    fn drop(&mut self) {
        if let (Some(live), Some(session)) = (&self.live, &self.live_session) {
            live.discard(session, &self.prepared.identity.attempt_id);
        }
    }
}
