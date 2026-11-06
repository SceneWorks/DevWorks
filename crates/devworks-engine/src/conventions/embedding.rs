//! Actual provider-backed auxiliary embeddings. These are native producer invocations, not
//! vectors inferred from a configured name or a second inference outcome journal.
use crate::providers::{
    driver::{NativeInvocation, NativeProviders},
    sink::ProviderResult,
};
use devworks_core::{
    context::InvocationContext,
    fingerprint::sha256_hex,
    ports::{Clock, storage::*, usage::PriceResolver},
    protocol::{Id, OperationId, OperationState, Protocol, Revision, provider::*},
    services::{
        artifacts::{ArtifactService, host::HostArtifactPublication, version_ref},
        conventions::indexing::embedding::{
            ConventionEmbeddings, EmbeddedBatch, EmbeddingConfiguration, EmbeddingFailure,
            EmbeddingRequest,
        },
        router::{
            Purpose, RequestAdmission,
            accounting::ReportCategory,
            embedding_spaces::{
                EmbeddingSpaceService, NativeProbeResult, PROBE_TEXT, PreparedEmbeddingProbe,
            },
        },
        usage::{
            accounting::{self, AttemptEnd, AttemptRequest},
            auxiliary,
            meter::InputSemantics,
            pricing::PricingContext,
        },
    },
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::pin,
    sync::{Arc, Mutex, Weak, mpsc},
    task::{Context, Poll, Wake, Waker},
    time::{Duration, Instant},
};

const REQUEST_MS: u32 = 30_000;
struct Inner {
    store: Arc<dyn ApplicationStore>,
    artifacts: Arc<ArtifactService>,
    providers: Weak<NativeProviders>,
    spaces: Arc<EmbeddingSpaceService>,
    clock: Arc<dyn Clock>,
    closed: Arc<devworks_core::services::conventions::indexing::lifecycle::PublicationFence>,
    active: Mutex<BTreeMap<String, Cancellation>>,
}
#[derive(Clone)]
pub struct NativeEmbeddings {
    inner: Arc<Inner>,
}
struct Active {
    inner: Arc<Inner>,
    request: Id,
}
impl Drop for Active {
    fn drop(&mut self) {
        self.inner
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(self.request.as_str());
    }
}
fn unavailable(why: impl Into<String>) -> EmbeddingFailure {
    EmbeddingFailure::Unavailable(why.into())
}
fn invalid(why: &str) -> StorageError {
    StorageError::InvalidRecord(why.into())
}
fn digest(value: &Value) -> Result<String, StorageError> {
    Ok(sha256_hex(
        devworks_core::jcs::canonicalize(value)
            .map_err(|_| invalid("native embedding identity"))?
            .as_bytes(),
    ))
}
fn rev(value: i64) -> Result<Revision, StorageError> {
    Revision::new(value).map_err(|_| invalid("embedding revision"))
}

struct NativePlan<'a> {
    ctx: &'a InvocationContext,
    provider: &'a Arc<NativeProviders>,
    space: &'a Id,
    request: &'a Id,
    key: &'a Id,
    operation: &'a OperationId,
    inputs: Vec<ArtifactRef>,
    deadline_ms: u32,
}
struct Completed {
    attempt: StoredInferenceAttempt,
    result: ProviderResult,
}
impl NativeEmbeddings {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        artifacts: Arc<ArtifactService>,
        providers: Arc<NativeProviders>,
        spaces: Arc<EmbeddingSpaceService>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                artifacts,
                providers: Arc::downgrade(&providers),
                spaces,
                clock,
                closed: Arc::new(Default::default()),
                active: Mutex::new(BTreeMap::new()),
            }),
        }
    }
    fn providers(&self) -> Result<Arc<NativeProviders>, EmbeddingFailure> {
        self.inner
            .providers
            .upgrade()
            .ok_or_else(|| unavailable("NoNativeEmbeddingRuntime"))
    }
    /// Local metadata publication is drained; actual unclosed sources remain owned by the one
    /// native UsageRuntime, whose bounded shutdown/recovery already records that custody.
    pub fn stop(&self) {
        self.inner.closed.begin_close();
        for token in self
            .inner
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
        {
            token.cancel();
        }
        self.inner.closed.drain();
    }
    fn publication<T>(
        &self,
        work: impl FnOnce() -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        let _phase = self.inner.closed.enter()?;
        work()
    }

    fn start(&self, request: &Id, token: &Cancellation) -> Result<Active, EmbeddingFailure> {
        if self.inner.closed.is_closed() || token.is_cancelled() {
            return Err(EmbeddingFailure::Cancelled);
        }
        let mut active = self.inner.active.lock().unwrap_or_else(|p| p.into_inner());
        if active.contains_key(request.as_str()) {
            return Err(EmbeddingFailure::Unknown {
                request_id: request.clone(),
                reason: "The exact embedding request is already owned.".into(),
            });
        }
        if active.len() >= 32 {
            return Err(unavailable("Native embedding capacity is occupied."));
        }
        if self.inner.closed.is_closed() {
            return Err(EmbeddingFailure::Cancelled);
        }
        active.insert(request.as_str().to_owned(), token.clone());
        Ok(Active {
            inner: self.inner.clone(),
            request: request.clone(),
        })
    }
    /// Closed settled journal lookup only. A missing/unknown receipt never triggers another
    /// transport from observation; replay keeps the original request and admission identities.
    fn recorded(
        &self,
        ctx: &InvocationContext,
        request: &Id,
        key: &Id,
    ) -> Result<Option<Completed>, EmbeddingFailure> {
        let row = read_snapshot(self.inner.store.as_ref(), &ReadOptions::default(), |read| {
            let Some(route) = read.router().route_request(request)? else {
                return Ok(None);
            };
            let op = read
                .operations()
                .operation(&route.operation_id)?
                .ok_or(StorageError::ScopeDenied)?;
            for journal in &op.attempts {
                if let Some(attempt) = read.usage().inference_attempt(&journal.id.0)? {
                    if accounting::snapshot(&attempt)?.admission_key != *key {
                        continue;
                    }
                    if attempt.actor_id != *ctx.principal()
                        || attempt.request_id != *request
                        || attempt.operation_id != op.id
                    {
                        return Err(StorageError::ScopeDenied);
                    }
                    let balance = read
                        .usage()
                        .attempt_balance(&attempt.attempt_id)?
                        .ok_or(StorageError::ScopeDenied)?;
                    return Ok(Some((attempt, balance, op.state)));
                }
            }
            if op.state == OperationState::IntentRecorded
                && op.attempts.is_empty()
                && op.payload["actorId"] == json!(ctx.principal())
            {
                return Ok(None);
            }
            Err(StorageError::ScopeDenied)
        })?;
        let Some((attempt, balance, state)) = row else {
            return Ok(None);
        };
        if attempt.state != "Settled" || !balance.local_quiescent || balance.unknown_tail {
            return Err(EmbeddingFailure::Unknown {
                request_id: request.clone(),
                reason: "The exact native embedding outcome is not closed and complete.".into(),
            });
        }
        let result = self.providers()?.recorded_result(request, key)?;
        if state != OperationState::ObservedComplete
            || result.outcome != Some(AttemptEnd::Completed)
        {
            return Err(unavailable("Native embedding did not complete."));
        }
        Ok(Some(Completed { attempt, result }))
    }
    fn invocation(&self, plan: NativePlan<'_>) -> Result<NativeInvocation, EmbeddingFailure> {
        let NativePlan {
            ctx,
            provider,
            space,
            request,
            key,
            operation,
            inputs,
            deadline_ms,
        } = plan;
        let (selection, mut query) = provider.embedding_selection(space)?;
        // No tokenizer is claimed. The exact UTF-8 byte count is an engineering reservation
        // estimate, not a guaranteed token upper bound, while context-specific tariff matching remains unknown before actual usage.
        let headroom = inputs.iter().try_fold(0_i64, |n, r| {
            n.checked_add(r.byte_length.get())
                .ok_or_else(|| invalid("embedding headroom overflow"))
        })?;
        query.input_tokens = None;
        let info = self.inner.store.info();
        let (
            owner,
            quote,
            price_revision,
            override_revision,
            control_revision,
            instruction_revision,
        ) = read_snapshot(self.inner.store.as_ref(), &ReadOptions::default(), |read| {
            let owner = auxiliary::ownership(read, &info, ctx).map_err(|error| match error {
                auxiliary::AuxiliaryError::Storage(e) => e,
                _ => StorageError::ScopeDenied,
            })?;
            let control = ctx
                .root_id()
                .map(|root| read.runs().root_control(root))
                .transpose()?
                .flatten();
            Ok((
                owner,
                devworks_core::services::prices::CatalogPriceResolver.quote(read, &query)?,
                read.usage().price_revision()?,
                read.usage()
                    .price_override_revision(&query.catalog_model_id)?,
                control.as_ref().map_or(1, |c| c.revision),
                control
                    .as_ref()
                    .map_or(1, |c| c.instruction_revision.max(1)),
            ))
        })
        .map_err(|e| {
            if matches!(e, StorageError::ScopeDenied) {
                unavailable(
                    "NoCurrentManagedInferenceOwner or current inference authority unavailable",
                )
            } else {
                e.into()
            }
        })?;
        let inference = InferenceRequest {
            options: InferenceOptions {
                selection: ModelSelection::Model {
                    model_id: query.catalog_model_id.clone(),
                },
                effort: None,
                output_schema: None,
                deadline_ms,
            },
            authority_revision: ctx.policy_revision().clone(),
            control_generation: rev(control_revision)?,
            instruction_revision: rev(instruction_revision)?,
            cancellation_revision: rev(1)?,
            protocol: Protocol::Draft1,
            request_id: request.clone(),
            purpose: InferencePurpose::Embedding,
            attribution: Attribution::Service {
                operation_id: operation.clone(),
                project_id: ctx.project_id().cloned(),
            },
            input: InferenceInput::Embedding {
                embedding_space_id: space.clone(),
                dimensions: match &selection {
                    devworks_core::services::router::accounting::AttemptSelection::Embedding {
                        dimensions,
                        ..
                    } => *dimensions,
                    _ => return Err(unavailable("Invalid embedding selection")),
                },
                inputs,
            },
        };
        let logical = RequestAdmission {
            request_id: request.clone(),
            purpose: Purpose::Embedding,
            session_id: ctx.session_id().cloned(),
            project_id: ctx.project_id().cloned(),
            root_id: ctx.root_id().cloned(),
            model_id: query.catalog_model_id.clone(),
            connection_id: query.connection_id.clone(),
            pin_id: None,
            args_digest: digest(&json!(inference))?,
        };
        let attempt = AttemptRequest {
            admission_key: key.clone(),
            request_id: request.clone(),
            actor_id: ctx.principal().clone(),
            work_id: owner.work_id,
            job_run_id: owner.job_run_id,
            role: owner.role,
            query,
            quote,
            price_generation: price_revision,
            override_revision,
            pricing: PricingContext {
                input_semantics: InputSemantics::Unknown,
                cache_read_possible: false,
                cache_write_possible: false,
                cache_write_ttl_ms: None,
            },
            selection,
            input_tokens: headroom,
            output_headroom: 0,
            unit_headroom: vec![],
            expected_context_epoch: owner.expected_context_epoch,
            dispatch: owner.dispatch,
            cache_diagnostics: vec![],
        };
        Ok(NativeInvocation {
            source_fences: Vec::new(),
            context: ctx.clone(),
            logical,
            category: ReportCategory::Other,
            attempt,
            request: inference,
        })
    }
    fn produce(
        &self,
        ctx: &InvocationContext,
        provider: Arc<NativeProviders>,
        invocation: NativeInvocation,
        token: Cancellation,
        observe: bool,
        cancelled: impl Fn() -> bool,
    ) -> Result<Completed, EmbeddingFailure> {
        let request = invocation.request.request_id.clone();
        let key = invocation.attempt.admission_key.clone();
        if let Some(recorded) = self.recorded(ctx, &request, &key)? {
            return Ok(recorded);
        }
        if observe {
            return Err(EmbeddingFailure::NotStarted {
                request_id: request,
            });
        }
        if token.is_cancelled() {
            return Err(EmbeddingFailure::Cancelled);
        }
        // NativeProviders awaits its owned OS worker through an oneshot; no extra Tokio
        // runtime, source, ledger or learned execution is created by this synchronous bridge.
        struct Signal(std::thread::Thread);
        impl Wake for Signal {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.unpark();
            }
        }
        let signal = Waker::from(Arc::new(Signal(std::thread::current())));
        let mut cx = Context::from_waker(&signal);
        let mut future = pin!(provider.produce_cancellable(invocation, Some(token.clone())));
        loop {
            if cancelled() {
                token.cancel();
            }
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(result) => {
                    let invocation = result?;
                    match invocation.accounting {
                        devworks_core::services::usage::host::DriveOutcome::CancelledBeforeDispatch => return Err(EmbeddingFailure::Cancelled),
                        devworks_core::services::usage::host::DriveOutcome::Refused(_) => return Err(unavailable("Current inference preflight refused dispatch.")),
                        _=>{},
                    }
                    break;
                }
                Poll::Pending => std::thread::park_timeout(Duration::from_millis(100)),
            }
        }
        self.recorded(ctx, &request, &key)?
            .ok_or_else(|| EmbeddingFailure::Unknown {
                request_id: request,
                reason: "Native owner returned without a recorded result.".into(),
            })
    }
    fn probe_inputs(
        &self,
        p: &PreparedEmbeddingProbe,
    ) -> Result<Vec<ArtifactRef>, EmbeddingFailure> {
        let _phase = self.inner.closed.enter()?;
        if let Some(inputs) = self.inner.spaces.probe_inputs(p)? {
            return Ok(inputs);
        }
        if p.operation_state() != OperationState::Dispatching {
            return Err(EmbeddingFailure::Unknown {
                request_id: p.request_id().clone(),
                reason: "Probe input phase has no recorded exact references.".into(),
            });
        }
        let staged = self
            .publication(|| {
                self.inner.artifacts.stage_host_bytes(
                    self.inner.store.as_ref(),
                    PROBE_TEXT.as_bytes(),
                    self.inner.spaces.probe_input_marker(p),
                )
            })?
            .map_err(unavailable)?;
        let (prepared, files, artifacts, spaces) = (
            p.clone(),
            staged.clone(),
            self.inner.artifacts.clone(),
            self.inner.spaces.clone(),
        );
        let result = self.publication(|| {
            run_write(
                self.inner.store.as_ref(),
                WriteOptions::default(),
                move |tx| {
                    if let Some(inputs) = spaces.probe_inputs_in(tx, &prepared)? {
                        return Ok((inputs, false));
                    }
                    let ctx = prepared.context();
                    let version = artifacts.commit_host_bytes_in_scope(
                        tx,
                        &files,
                        HostArtifactPublication {
                            title: "Embedding qualification input",
                            media_type: "text/plain; charset=utf-8",
                            provenance: spaces.probe_input_marker(&prepared),
                            scope: ctx
                                .project_id()
                                .cloned()
                                .map_or(ArtifactScope::Workspace, ArtifactScope::Project),
                            owner: ctx.principal().clone(),
                        },
                    )?;
                    let inputs = vec![version_ref(&version)];
                    spaces.record_probe_inputs(tx, &prepared, &inputs)?;
                    Ok((inputs, true))
                },
            )
        });
        self.inner.artifacts.settle_host_bytes(
            self.inner.store.as_ref(),
            &staged,
            result.as_ref().is_ok_and(|(_, committed)| *committed),
        );
        result.map(|(refs, _)| refs).map_err(Into::into)
    }
    pub fn complete_probe(
        &self,
        tx: &mut dyn WriteTransaction,
        id: &OperationId,
    ) -> Result<(), StorageError> {
        let _phase = self.inner.closed.enter()?;
        self.inner.spaces.complete_probe(tx, id)
    }
    pub fn reconcile_probes(
        &self,
        after: Option<&Id>,
    ) -> Result<devworks_core::services::router::embedding_spaces::ProbeOutcomePage, StorageError>
    {
        self.publication(|| self.inner.spaces.reconcile_probe_outcomes(after))
    }
    pub fn execute_probe(
        &self,
        p: PreparedEmbeddingProbe,
        observe: bool,
    ) -> Result<(), EmbeddingFailure> {
        let token = Cancellation::new();
        let _active = self.start(p.request_id(), &token)?;
        let inputs = self.probe_inputs(&p)?;
        let provider = self.providers()?.for_embedding_probe(&p)?;
        let mut invocation = self.invocation(NativePlan {
            ctx: p.context(),
            provider: &provider,
            space: &p.space().space_id,
            request: p.request_id(),
            key: p.admission_key(),
            operation: p.operation_id(),
            inputs,
            deadline_ms: REQUEST_MS,
        })?;
        invocation.source_fences = p.authority_fences().to_vec();
        let completed = self.produce(p.context(), provider, invocation, token, observe, || {
            self.inner.spaces.probe_cancelled(&p).unwrap_or(true)
        })?;
        let result_artifact = completed
            .result
            .result_artifact
            .ok_or_else(|| unavailable("Native probe result manifest missing"))?;
        if completed.result.vector_artifacts.len() != 1 {
            return Err(unavailable("Native probe must have exactly one vector"));
        }
        let vector_artifact = completed
            .result
            .vector_artifacts
            .into_iter()
            .next()
            .expect("one vector");
        let result_bytes = self
            .inner
            .artifacts
            .read_exact(
                self.inner.store.as_ref(),
                p.context(),
                &result_artifact,
                1024 * 1024,
            )
            .map_err(|e| unavailable(format!("Probe result unavailable: {e:?}")))?
            .0;
        let vector_bytes = self
            .inner
            .artifacts
            .read_exact(
                self.inner.store.as_ref(),
                p.context(),
                &vector_artifact,
                u64::from(p.space().dimensions) * 4,
            )
            .map_err(|e| unavailable(format!("Probe vector unavailable: {e:?}")))?
            .0;
        let verified = self.inner.spaces.verify_probe_result(
            &p,
            NativeProbeResult {
                request_id: p.request_id().clone(),
                attempt_id: completed.attempt.attempt_id,
                result_artifact,
                vector_artifact,
                result_bytes,
                vector_bytes,
                reported_model: completed.attempt.provider_model_id,
            },
        )?;
        self.publication(|| {
            let inner = self.inner.clone();
            run_write(
                self.inner.store.as_ref(),
                WriteOptions::default(),
                move |tx| {
                    if inner.closed.is_closed() {
                        return Err(StorageError::Cancelled);
                    }
                    inner
                        .spaces
                        .persist_verified_probe_evidence_in(tx, verified)
                },
            )
        })?;
        Ok(())
    }
}
impl ConventionEmbeddings for NativeEmbeddings {
    fn is_available(&self) -> bool {
        !self.inner.closed.is_closed() && self.inner.providers.upgrade().is_some()
    }
    fn configured_space(
        &self,
        ctx: &InvocationContext,
    ) -> Result<EmbeddingConfiguration, EmbeddingFailure> {
        Ok(
            devworks_core::services::conventions::indexing::configuration::resolved_configuration(
                &self.inner.spaces.resolve_selected(ctx)?,
                self.is_available(),
            ),
        )
    }
    fn embed(
        &self,
        ctx: &InvocationContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddedBatch, EmbeddingFailure> {
        self.request(ctx, request, false)
    }
    fn observe(
        &self,
        ctx: &InvocationContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddedBatch, EmbeddingFailure> {
        self.request(ctx, request, true)
    }
}
impl NativeEmbeddings {
    fn request(
        &self,
        ctx: &InvocationContext,
        request: EmbeddingRequest,
        observe: bool,
    ) -> Result<EmbeddedBatch, EmbeddingFailure> {
        let worker = self.clone();
        let ctx = ctx.clone();
        let token = request.cancellation.clone();
        let request_id = request.request_id.clone();
        let deadline = request.deadline_ms;
        let started = Instant::now();
        let (send, receive) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("devworks-auxiliary-embedding".into())
            .spawn(move || {
                let result = worker.embed_owned(&ctx, request, observe);
                let _ = send.send(result);
            })
            .map_err(|e| {
                StorageError::unavailable(
                    Unavailable::UnsupportedRuntime,
                    format!("Embedding owner could not start: {e}"),
                )
            })?;
        loop {
            if token.is_cancelled()
                || deadline.is_some_and(|d| self.inner.clock.now_ms().get() >= d)
                || started.elapsed() >= Duration::from_millis(u64::from(REQUEST_MS))
                || self.inner.closed.is_closed()
            {
                token.cancel();
                return Err(EmbeddingFailure::Cancelled);
            }
            match receive.recv_timeout(Duration::from_millis(50)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(EmbeddingFailure::Unknown {
                        request_id,
                        reason: "The exact embedding owner ended without acknowledgement.".into(),
                    });
                }
            }
        }
    }
}
impl NativeEmbeddings {
    fn embed_owned(
        &self,
        ctx: &InvocationContext,
        request: EmbeddingRequest,
        observe: bool,
    ) -> Result<EmbeddedBatch, EmbeddingFailure> {
        let resolved = self.inner.spaces.resolve_selected(ctx)?;
        let binding = resolved.binding().ok_or_else(|| {
            unavailable(resolved.reason().unwrap_or("NoConfiguredEmbeddingRoute"))
        })?;
        if devworks_core::services::conventions::indexing::configuration::resolved_configuration(
            &resolved,
            self.is_available(),
        )
        .source_digest
            != request.configuration_source_digest
            || devworks_core::services::conventions::indexing::configuration::stored_space(binding)
                != request.space
        {
            return Err(unavailable(
                "Embedding configuration changed before dispatch.",
            ));
        }
        if !matches!(
            binding.space().metric,
            devworks_core::protocol::data::Metric::Cosine
        ) {
            return Err(unavailable("UnsupportedMetric"));
        }
        let provider = self.providers()?.for_qualified_embedding(ctx, binding)?;
        let token = request.cancellation.clone();
        let _active = self.start(&request.request_id, &token)?;
        let key = Id::new(format!(
            "cemb-admit-{}",
            &sha256_hex(request.request_id.as_str().as_bytes())[..40]
        ))
        .expect("digestID");
        let remaining = request
            .deadline_ms
            .map(|d| d.saturating_sub(self.inner.clock.now_ms().get()))
            .unwrap_or(i64::from(REQUEST_MS));
        if remaining <= 0 {
            return Err(EmbeddingFailure::Cancelled);
        }
        let timeout = remaining.min(i64::from(REQUEST_MS)) as u32;
        let start = Instant::now();
        let input_count = request.input_refs.len();
        let mut invocation = self.invocation(NativePlan {
            ctx,
            provider: &provider,
            space: &binding.space().space_id,
            request: &request.request_id,
            key: &key,
            operation: &request.preparation_operation,
            inputs: request.input_refs,
            deadline_ms: timeout,
        })?;
        invocation.source_fences = request.fences.clone();
        let completed = self.produce(ctx, provider, invocation, token.clone(), observe, || {
            token.is_cancelled() || start.elapsed() >= Duration::from_millis(u64::from(timeout))
        });
        if token.is_cancelled() {
            return Err(EmbeddingFailure::Cancelled);
        }
        let completed = completed?;
        if completed
            .attempt
            .provider_model_id
            .as_ref()
            .is_some_and(|id| id != binding.upstream_model_id())
        {
            return Err(unavailable(
                "Provider reported a different embedding model.",
            ));
        }
        if completed.result.vector_artifacts.len() != input_count {
            return Err(unavailable(
                "Native embedding vector population differs from exact inputs",
            ));
        }
        let mut vectors = Vec::with_capacity(completed.result.vector_artifacts.len());
        for reference in &completed.result.vector_artifacts {
            let bytes = self
                .inner
                .artifacts
                .read_exact(
                    self.inner.store.as_ref(),
                    ctx,
                    reference,
                    u64::from(request.space.dimensions) * 4,
                )
                .map_err(|e| unavailable(format!("Embedding result unavailable: {e:?}")))?
                .0;
            if bytes.len() != request.space.dimensions as usize * 4 {
                return Err(unavailable("Embedding dimension mismatch"));
            }
            let vector: Vec<_> = bytes
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().expect("fourbytes")))
                .collect();
            if vector.iter().any(|v| !v.is_finite())
                || vector
                    .iter()
                    .map(|v| f64::from(*v) * f64::from(*v))
                    .sum::<f64>()
                    == 0.0
            {
                return Err(unavailable("Invalid cosine vector"));
            }
            vectors.push(vector);
        }
        Ok(EmbeddedBatch {
            request_id: request.request_id,
            space: request.space,
            vectors,
            attempt_id: completed.attempt.attempt_id,
        })
    }
}
