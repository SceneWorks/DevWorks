//! Native producer composition. Only trusted host configuration can qualify capabilities/spaces.
use super::{
    schemas::RegisteredSchemas,
    sink::{ProviderResult, ProviderSink, ResultChannel, SourceProvenance},
    source::NativeProviderSource,
};
use crate::usage::UsageRuntime;
use devworks_core::{
    context::InvocationContext,
    fingerprint::sha256_hex,
    ports::{
        provider_http::{StreamLimits, StreamingHttp},
        storage::*,
    },
    protocol::{AttemptId, Counter, Id, Revision, provider::*},
    providers::{
        PreparedRequest, RequestIdentity,
        capabilities::{Capabilities, Support},
        encode::{self, PreparationLimits},
        materialize::OwnedMaterials,
    },
    services::{
        artifacts::ArtifactService,
        router::{RequestAdmission, accounting::ReportCategory, connections::ConnectionService},
        usage::{
            accounting::{AttemptEnd, AttemptRequest},
            host::{
                AccountingWriteFence, DriveOutcome, InferenceFrame, OwnedInferenceSink,
                OwnedInferenceSource,
            },
        },
    },
};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct ProviderComposition {
    pub http: Arc<dyn StreamingHttp>,
    /// Advanced controls require exact host-qualified model/profile metadata. Basic tool/image
    /// flags and efforts are intersected with the CURRENT registered catalog model facts.
    pub capabilities: Vec<Capabilities>,
    pub embedding_spaces: Vec<EmbeddingRegistration>,
    pub usage_semantics:
        std::collections::BTreeMap<String, devworks_core::services::usage::meter::InputSemantics>,
    pub(crate) instruction_guard: Option<Arc<crate::instruction_guard::InstructionGuard>>,
}
impl std::fmt::Debug for ProviderComposition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderComposition")
            .field("models", &self.capabilities.len())
            .field("embedding_spaces", &self.embedding_spaces.len())
            .finish_non_exhaustive()
    }
}
impl ProviderComposition {
    pub fn new(http: Arc<dyn StreamingHttp>) -> Self {
        Self {
            http,
            capabilities: Vec::new(),
            embedding_spaces: Vec::new(),
            usage_semantics: Default::default(),
            instruction_guard: None,
        }
    }
    /// Host-composed repository source used by direct native provider owners. Boot installs
    /// its registered indexing source automatically.
    pub fn with_repository_source(
        mut self,
        store: Arc<dyn ApplicationStore>,
        source: Arc<dyn devworks_core::ports::indexing::SourceTree>,
    ) -> Self {
        self.instruction_guard = Some(Arc::new(crate::instruction_guard::InstructionGuard::new(
            store,
            Some(source),
        )));
        self
    }
    pub(crate) fn with_instruction_guard(
        mut self,
        guard: Arc<crate::instruction_guard::InstructionGuard>,
    ) -> Self {
        self.instruction_guard = Some(guard);
        self
    }
}
#[derive(Clone, Debug)]
pub struct EmbeddingRegistration {
    pub space: devworks_core::protocol::data::EmbeddingSpace,
    pub connection_id: Id,
    pub upstream_model_id: String,
    pub capabilities: Capabilities,
}
impl EmbeddingRegistration {
    pub fn digest(&self) -> String {
        // Host-only immutable registration; all support facts (including fixed vs parameter
        // dimensions) participate. Nothing in the wire InferenceSpec can mint this registry.
        sha256_hex(format!("{:?}", self).as_bytes())
    }
}
pub struct NativeProviders {
    store: Arc<dyn ApplicationStore>,
    artifacts: Arc<ArtifactService>,
    connections: Arc<ConnectionService>,
    usage: Arc<UsageRuntime>,
    schemas: Arc<RegisteredSchemas>,
    composition: ProviderComposition,
    live: Arc<super::live::LiveTurns>,
}
/// Trusted native producer input. Cancellation is operational ownership, never wire authority.
pub type InstructionTargets = Vec<(Id, Vec<String>)>;
pub struct NativeInvocation {
    pub source_fences: Vec<devworks_core::ports::AuthorityFence>,
    /// Host-selected repository targets for this request. `Some` requires an exact stamped set;
    /// `None` is only for native callers that have no selected repository target yet.
    pub instruction_targets: Option<InstructionTargets>,
    pub context: InvocationContext,
    pub logical: RequestAdmission,
    pub category: ReportCategory,
    pub attempt: AttemptRequest,
    pub request: InferenceRequest,
    /// A text-only request made from inside one of this session's dispatching tool calls.
    pub nested: Option<NestedToolInference>,
}
/// The request's output is an artifact; it never enters the transcript or moves the context head.
#[derive(Clone)]
pub struct NestedToolInference {
    pub tool_operation: devworks_core::protocol::OperationId,
    pub call: ToolCallKey,
}
pub struct ProviderInvocation {
    pub accounting: DriveOutcome,
    pub result: ProviderResult,
}
pub(crate) struct NativeProduction {
    pub source_fences: Vec<devworks_core::ports::AuthorityFence>,
    pub context: InvocationContext,
    pub logical: RequestAdmission,
    pub category: ReportCategory,
    pub request: AttemptRequest,
    pub source: Box<dyn OwnedInferenceSource>,
    pub sink: Box<dyn OwnedInferenceSink>,
    pub preparation: NativePreparation,
    pub cancellation: Option<Cancellation>,
}
pub(crate) struct NativePreparation {
    owner: Arc<NativeProviders>,
    request: InferenceRequest,
}
pub(crate) struct QuoteRefresh {
    pub quote: devworks_core::ports::usage::PriceQuote,
    pub price_generation: i64,
    pub override_revision: i64,
    pub connection_revision: i64,
    pub native_effort: Option<devworks_core::providers::effort::NormalizedEffort>,
    pub recorded_selection: Option<devworks_core::services::router::accounting::AttemptSelection>,
}
impl NativePreparation {
    pub(crate) fn prepare(
        &self,
        ctx: &InvocationContext,
        logical: &RequestAdmission,
        request: &AttemptRequest,
    ) -> Result<Option<QuoteRefresh>, StorageError> {
        if ctx.principal() != &request.actor_id
            || logical.request_id != request.request_id
            || logical.connection_id != request.query.connection_id
            || logical.model_id != request.query.catalog_model_id
        {
            return Err(StorageError::ScopeDenied);
        }
        let store = self.owner.store.as_ref();
        let (replay, before) = read_snapshot(store, &ReadOptions::default(), |s| {
            let mut replay = None;
            if let Some(route) = s.router().route_request(&request.request_id)? {
                let op = s
                    .operations()
                    .operation(&route.operation_id)?
                    .ok_or(StorageError::ScopeDenied)?;
                if op.args_digest != logical.args_digest {
                    return Err(StorageError::ScopeDenied);
                }
                for journal in &op.attempts {
                    if let Some(row) = s.usage().inference_attempt(&journal.id.0)?
                        && devworks_core::services::usage::accounting::snapshot(&row)?.admission_key
                            == request.admission_key
                    {
                        let captured = devworks_core::services::usage::accounting::snapshot(&row)?;
                        let expected=match &captured.selection {devworks_core::services::router::accounting::AttemptSelection::NativeGeneration{route,..}=>route.as_ref(),selection=>selection};
                        if serde_json::to_value(expected).ok()
                            != serde_json::to_value(&request.selection).ok()
                        {
                            return Err(StorageError::ScopeDenied);
                        }
                        replay = Some(captured);
                        break;
                    }
                }
            }
            Ok((
                replay,
                s.router()
                    .connection(&request.query.connection_id)?
                    .ok_or(StorageError::ScopeDenied)?,
            ))
        })?;
        if let Some(captured) = replay {
            return Ok(Some(QuoteRefresh {
                connection_revision: captured
                    .quote
                    .connection_revision
                    .ok_or(StorageError::ScopeDenied)?,
                quote: captured.quote,
                price_generation: captured.price_generation,
                override_revision: captured.override_revision,
                native_effort: None,
                recorded_selection: Some(captured.selection),
            }));
        }
        let parameters = match &self.request.input {
            InferenceInput::Generation { context, .. } => {
                let (bytes, _) = self
                    .owner
                    .artifacts
                    .read_exact(store, ctx, context, 8 * 1024 * 1024)
                    .map_err(|_| StorageError::ScopeDenied)?;
                let material = serde_json::from_slice::<
                    devworks_core::providers::context::ProviderContext,
                >(&bytes)
                .map_err(|_| invalid("provider context"))?;
                material
                    .validate()
                    .map_err(|_| invalid("provider context"))?;
                Some(material.parameters)
            }
            _ => None,
        };
        self.owner
            .connections
            .access(&before.id, None)
            .map_err(|_| {
                StorageError::unavailable(
                    Unavailable::UnsupportedRuntime,
                    "current provider access unavailable",
                )
            })?;
        use devworks_core::ports::usage::PriceResolver;
        read_snapshot(store, &ReadOptions::default(), |s| {
            let current = s
                .router()
                .connection(&before.id)?
                .ok_or(StorageError::ScopeDenied)?;
            if current.account_revision != before.account_revision
                || current.endpoint != before.endpoint
                || current.profile != before.profile
                || current.profile != request.query.endpoint_profile
                || !current.enabled
            {
                return Err(StorageError::ScopeDenied);
            }
            let native_effort = if let Some(parameters) = &parameters {
                let context_epoch = match &self.request.input {
                    InferenceInput::Generation { context_epoch, .. } => context_epoch.clone(),
                    _ => Counter::new(0).expect("zero"),
                };
                let binding = NativeBinding {
                    connection_id: current.id.clone(),
                    account_revision: Revision::new(current.account_revision).map_err(invalid)?,
                    endpoint_profile: serde_json::from_value(serde_json::Value::String(
                        current.profile.clone(),
                    ))
                    .map_err(|_| invalid("endpoint profile"))?,
                    model_id: request.query.catalog_model_id.clone(),
                    context_epoch,
                };
                let model = s.router().route_model(&request.query.catalog_model_id)?;
                let upstream = super::wire_model::registered_upstream(
                    &current,
                    model.as_ref(),
                    &request.query,
                )?;
                let caps = self.owner.capabilities(
                    &self.request,
                    &binding,
                    &current,
                    model,
                    &upstream,
                    &request.selection,
                )?;
                Some(
                    devworks_core::providers::effort::normalize_effort(
                        parameters,
                        &self.request.options,
                        &caps,
                    )
                    .map_err(|_| invalid("native effort is not qualified"))?,
                )
            } else {
                None
            };
            Ok(Some(QuoteRefresh {
                quote: devworks_core::services::prices::CatalogPriceResolver
                    .quote(s, &request.query)?,
                price_generation: s.usage().price_revision()?,
                override_revision: s
                    .usage()
                    .price_override_revision(&request.query.catalog_model_id)?,
                connection_revision: current.revision,
                native_effort,
                recorded_selection: None,
            }))
        })
    }
}
impl NativeProviders {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        artifacts: Arc<ArtifactService>,
        connections: Arc<ConnectionService>,
        usage: Arc<UsageRuntime>,
        schemas: Arc<RegisteredSchemas>,
        composition: ProviderComposition,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            artifacts,
            connections,
            usage,
            schemas,
            composition,
            live: Arc::new(super::live::LiveTurns::default()),
        })
    }
    pub(crate) fn verify_instruction_material(
        &self,
        ctx: &InvocationContext,
        context: &devworks_core::protocol::provider::ArtifactRef,
        targets: Option<&[(Id, Vec<String>)]>,
    ) -> Result<(), StorageError> {
        let Some(guard) = &self.composition.instruction_guard else {
            return Ok(());
        };
        let bytes = self
            .artifacts
            .read_exact(self.store.as_ref(), ctx, context, 8 * 1024 * 1024)
            .map_err(|_| StorageError::ScopeDenied)?
            .0;
        let material: devworks_core::providers::context::ProviderContext =
            serde_json::from_slice(&bytes).map_err(|_| invalid("provider instruction context"))?;
        guard.verify(ctx, &material, targets)
    }
    fn verify_instruction_context(
        &self,
        ctx: &InvocationContext,
        request: &InferenceRequest,
        targets: Option<&[(Id, Vec<String>)]>,
    ) -> Result<(), StorageError> {
        if let InferenceInput::Generation { context, .. } = &request.input {
            self.verify_instruction_material(ctx, context, targets)
        } else {
            Ok(())
        }
    }
    /// Explicit host-qualified declarations, including Unknown support. Every request still
    /// intersects these with the CURRENT registered model/profile/account and current policy.
    pub fn capability_registrations(&self) -> &[Capabilities] {
        &self.composition.capabilities
    }
    pub fn embedding_registrations(&self) -> &[EmbeddingRegistration] {
        &self.composition.embedding_spaces
    }
    /// Trusted native UI read only. The host must authorize the session under its current
    /// user context before serving this preview; generic gateway/tools cannot publish it.
    pub fn text_preview(&self, session: &Id) -> Option<super::live::LiveTurn> {
        if self.usage.accounting_fence().is_closed() {
            self.live.clear();
            return None;
        }
        self.usage
            .accounting_fence()
            .publication(|| {
                let Some(preview) = self.live.snapshot(session) else {
                    return Ok(None);
                };
                let current = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
                    let Some(current) = read.sessions().session(session)? else {
                        return Ok(false);
                    };
                    let Some(attempt) = read.usage().inference_attempt(&preview.attempt_id.0)?
                    else {
                        return Ok(false);
                    };
                    Ok(!current.closed()
                        && current.context_epoch == preview.context_epoch
                        && attempt.session_id.as_ref() == Some(session)
                        && if preview.finalized_through.is_some() {
                            matches!(attempt.state.as_str(), "Dispatched" | "Settled")
                        } else {
                            attempt.state == "Dispatched"
                        })
                })
                .unwrap_or(false);
                if !current {
                    self.live.discard(session, &preview.attempt_id);
                    return Ok(None);
                }
                Ok(Some(preview))
            })
            .ok()
            .flatten()
    }
    pub fn schemas(&self) -> &Arc<RegisteredSchemas> {
        &self.schemas
    }
    pub async fn produce(
        self: &Arc<Self>,
        context: InvocationContext,
        logical: RequestAdmission,
        category: ReportCategory,
        attempt: AttemptRequest,
        request: InferenceRequest,
    ) -> Result<ProviderInvocation, StorageError> {
        self.produce_cancellable(
            NativeInvocation {
                source_fences: Vec::new(),
                instruction_targets: None,
                context,
                logical,
                category,
                attempt,
                request,
                nested: None,
            },
            None,
        )
        .await
    }
    /// An actual originating query/job cancellation joins the same owned producer token.
    /// Dropping the caller's future still never drops source custody or resends a request.
    pub async fn produce_cancellable(
        self: &Arc<Self>,
        invocation: NativeInvocation,
        cancellation: Option<Cancellation>,
    ) -> Result<ProviderInvocation, StorageError> {
        let NativeInvocation {
            source_fences,
            instruction_targets,
            context,
            mut logical,
            category,
            mut attempt,
            mut request,
            nested,
        } = invocation;
        if cancellation
            .as_ref()
            .is_some_and(Cancellation::is_cancelled)
        {
            return Err(StorageError::Cancelled);
        }
        request.validate().map_err(invalid)?;
        if let InferenceInput::Generation {
            response_headroom_tokens,
            ..
        } = &request.input
            && attempt.output_headroom < response_headroom_tokens.get()
        {
            // The router's reserve must cover the headroom actually sent to the endpoint.
            return Err(StorageError::ScopeDenied);
        }
        if let InferenceInput::Generation { tools, .. } = &mut request.input {
            tools.sort_by(|a, b| {
                a.capability_id
                    .as_str()
                    .cmp(b.capability_id.as_str())
                    .then_with(|| a.wire_name.cmp(&b.wire_name))
            });
            let selected_mcp = if tools
                .iter()
                .any(|tool| tool.capability_id.as_str().starts_with("mcp/"))
            {
                self.schemas.selected_mcp_tools(&context)?
            } else {
                Vec::new()
            };
            if tools.iter().any(|tool| {
                self.schemas
                    .builtin_binding(&tool.capability_id, &tool.input_schema)
                    .is_none()
                    && !selected_mcp.iter().any(|selected| {
                        selected.capability_id == tool.capability_id
                            && selected.wire_name == tool.wire_name
                            && selected.input_schema.schema_id == tool.input_schema.schema_id
                            && selected.input_schema.revision == tool.input_schema.revision
                            && selected.input_schema.sha256 == tool.input_schema.sha256
                    })
            }) {
                // Schema discovery alone does not make an external invocation durable. Only
                // currently selected MCP bindings have the journaled turn dispatcher.
                return Err(StorageError::unavailable(
                    Unavailable::UnsupportedRuntime,
                    "external tool dispatch is unavailable",
                ));
            }
        }
        let compaction = matches!(request.purpose, InferencePurpose::Compaction);
        let detached = compaction || nested.is_some();
        if compaction != (logical.purpose == devworks_core::services::router::Purpose::Compaction)
            || (detached
                && !matches!(&request.input, InferenceInput::Generation { tools, .. } if tools.is_empty()))
            || (compaction && nested.is_some())
        {
            return Err(StorageError::ScopeDenied);
        }
        let mut turn_start_sequence = None;
        let mut context_head_fence = None;
        if let InferenceInput::Generation {
            session_id,
            context: source,
            ..
        } = &request.input
        {
            let owner = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
                read.sessions()
                    .session(session_id)?
                    .map(|session| session.agent_id)
                    .ok_or(StorageError::ScopeDenied)
            })?;
            if owner != *context.principal() || context.session_id() != Some(session_id) {
                // A host-origin request cannot borrow an agent session to bypass its role and
                // shared tool selection before the provider sees a tool schema.
                return Err(StorageError::ScopeDenied);
            }
            if let Some(nested) = &nested {
                self.require_dispatching_call(&context, session_id, nested)?;
            } else if !compaction {
                let fence = crate::tool_turn::require_current_head_for_new_request(
                    self.store.as_ref(),
                    &context,
                    session_id,
                    &request.request_id,
                    source,
                )?;
                // Capture before dispatch. Even an empty completion must have a cursor, and a
                // concurrent redirect must be visible when continuation scans this turn.
                turn_start_sequence = Some(fence.sequence);
                context_head_fence = Some(fence);
            }
        }
        if let ModelSelection::Model { model_id } = &request.options.selection
            && model_id != &attempt.query.catalog_model_id
        {
            return Err(StorageError::ScopeDenied);
        }

        logical.args_digest = devworks_core::fingerprint::canonical_digest(
            &serde_json::to_value(&request).map_err(|_| invalid("provider request JSON"))?,
        )
        .map_err(|_| invalid("provider request canonical JSON"))?;
        if request.request_id != logical.request_id || request.request_id != attempt.request_id {
            return Err(StorageError::ScopeDenied);
        }
        if let InferenceInput::Generation { session_id, .. } = &request.input
            && nested.is_none()
            && !crate::tool_turn::pending_calls(self.store.as_ref(), &context, session_id)?
                .is_empty()
        {
            // A fresh generation cannot skip an earlier unanswered tool call by supplying a
            // context that omits it. The original request remains replayable so its durable
            // calls can be dispatched and reconciled after a restart.
            let replay = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
                let Some(route) = read.router().route_request(&request.request_id)? else {
                    return Ok(false);
                };
                let operation = read
                    .operations()
                    .operation(&route.operation_id)?
                    .ok_or(StorageError::ScopeDenied)?;
                for journal in &operation.attempts {
                    if let Some(stored) = read.usage().inference_attempt(&journal.id.0)?
                        && stored.request_id == request.request_id
                        && stored.session_id.as_ref() == Some(session_id)
                        && devworks_core::services::usage::accounting::snapshot(&stored)?
                            .admission_key
                            == attempt.admission_key
                    {
                        return Ok(true);
                    }
                }
                Ok(false)
            })?;
            if !replay {
                return Err(StorageError::unavailable(
                    Unavailable::UnsupportedRuntime,
                    "new generation waits for earlier unanswered tool calls",
                ));
            }
        }
        if let InferenceInput::Generation {
            session_id,
            context: request_context,
            context_epoch,
            ..
        } = &request.input
            && context_epoch.get() > 0
            && nested.is_none()
        {
            // The epoch is host-published provenance, not a guess from a cache miss. Record
            // its reason on the attributed attempt without copying prompt or tool material.
            let epoch = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
                read.sessions()
                    .epoch(session_id, context_epoch.get())?
                    .ok_or(StorageError::ScopeDenied)
            })?;
            let published: devworks_core::protocol::provider::ArtifactRef =
                serde_json::from_value(epoch.context)
                    .map_err(|error| invalid(error.to_string()))?;
            if published.artifact_id != request_context.artifact_id
                || published.version_id != request_context.version_id
                || published.sha256 != request_context.sha256
                || published.byte_length != request_context.byte_length
                || published.media_type != request_context.media_type
            {
                crate::context_epoch::verify_descendant(
                    self.store.as_ref(),
                    &self.artifacts,
                    &context,
                    session_id,
                    request_context,
                    &published,
                    context_epoch.get(),
                    &attempt.query.connection_id,
                    &attempt.query.catalog_model_id,
                    &attempt.query.endpoint_profile,
                )?;
            }
            if attempt.cache_diagnostics.len() >= 128 {
                return Err(StorageError::ScopeDenied);
            }
            attempt.cache_diagnostics.push(
                devworks_core::protocol::records::UsageCacheDiagnostic {
                    kind: devworks_core::protocol::support::BoundedText(
                        "ContextEpochChanged".into(),
                    ),
                    inferred: false,
                    detail: Some(devworks_core::protocol::support::BoundedText(epoch.reason)),
                    source_revision: Some(Revision::new(context_epoch.get()).map_err(invalid)?),
                },
            );
        }
        if let InferenceInput::Generation { tools, .. } = &request.input {
            if attempt.cache_diagnostics.len() < 128 {
                attempt.cache_diagnostics.push(
                    devworks_core::protocol::records::UsageCacheDiagnostic {
                        kind: devworks_core::protocol::support::BoundedText(
                            "InstructionRevision".into(),
                        ),
                        inferred: false,
                        detail: None,
                        source_revision: Some(request.instruction_revision.clone()),
                    },
                );
            }
            // This is the requested alias/schema-reference set, not a claim that the provider
            // reused its materialized prefix or reported a cache hit.
            let aliases = devworks_core::jcs::canonicalize(
                &serde_json::to_value(tools).map_err(|_| invalid("tool alias set"))?,
            )
            .map_err(|_| invalid("canonical tool alias set"))?;
            if attempt.cache_diagnostics.len() < 128 {
                attempt.cache_diagnostics.push(
                    devworks_core::protocol::records::UsageCacheDiagnostic {
                        kind: devworks_core::protocol::support::BoundedText(
                            "RequestedToolAliasDigest".into(),
                        ),
                        inferred: false,
                        detail: Some(devworks_core::protocol::support::BoundedText(sha256_hex(
                            aliases.as_bytes(),
                        ))),
                        source_revision: None,
                    },
                );
            }
        }
        use devworks_core::services::usage::meter::InputSemantics;
        attempt.pricing.input_semantics = self
            .composition
            .usage_semantics
            .get(attempt.query.catalog_model_id.as_str())
            .copied()
            .unwrap_or(match attempt.query.endpoint_profile.as_str() {
                "AnthropicMessages" | "ClaudeSubscriptionMessages" => InputSemantics::Disjoint,
                // The embedding codec's prompt_tokens is total input; this profile has no
                // cache sub-buckets to subtract. Actual usage remains provider-reported.
                "Embedding"
                | "OpenAiResponses"
                | "CodexSubscriptionResponses"
                | "OpenRouterChat" => InputSemantics::Inclusive,
                _ => InputSemantics::Unknown,
            });
        let shared = Arc::new(Mutex::new(None));
        let result = Arc::new(Mutex::new(ProviderResult::default()));
        let source = LazySource {
            owner: self.clone(),
            context: context.clone(),
            request: request.clone(),
            shared: shared.clone(),
            inner: None,
            source_fences: source_fences.clone(),
            context_head_fence,
            instruction_targets: instruction_targets.clone(),
        };
        let sink = LazySink {
            owner: self.clone(),
            context: context.clone(),
            request: request.clone(),
            instruction_targets,
            shared,
            result: result.clone(),
            turn_start_sequence,
            detached,
            inner: None,
        };
        let request_id = logical.request_id.clone();
        let admission_key = attempt.admission_key.clone();
        let accounting = self
            .usage
            .produce_native(NativeProduction {
                source_fences,
                cancellation,
                context,
                logical,
                category,
                request: attempt,
                source: Box::new(source),
                sink: Box::new(sink),
                preparation: NativePreparation {
                    owner: self.clone(),
                    request,
                },
            })
            .await?;
        let result = if matches!(accounting, DriveOutcome::AlreadyDispatched) {
            self.recorded_result(&request_id, &admission_key)?
        } else {
            result.lock().unwrap_or_else(|p| p.into_inner()).clone()
        };
        Ok(ProviderInvocation { accounting, result })
    }
    /// The nested request's tool call must still be dispatching for this principal and session.
    fn require_dispatching_call(
        &self,
        context: &InvocationContext,
        session_id: &Id,
        nested: &NestedToolInference,
    ) -> Result<(), StorageError> {
        let operation = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
            read.operations().operation(&nested.tool_operation)
        })?
        .ok_or(StorageError::ScopeDenied)?;
        let payload = &operation.payload;
        if operation.capability != "mcp.tool"
            || operation.state != devworks_core::protocol::OperationState::Dispatching
            || payload.get("principal") != Some(&serde_json::json!(context.principal()))
            || payload.get("sessionId") != Some(&serde_json::json!(session_id))
            || payload.get("call") != Some(&serde_json::json!(nested.call))
        {
            return Err(StorageError::ScopeDenied);
        }
        Ok(())
    }
    pub(crate) fn recorded_result(
        &self,
        request_id: &Id,
        admission_key: &Id,
    ) -> Result<ProviderResult, StorageError> {
        read_snapshot(self.store.as_ref(), &ReadOptions::default(), |s| {
            let route = s
                .router()
                .route_request(request_id)?
                .ok_or(StorageError::ScopeDenied)?;
            let operation = s
                .operations()
                .operation(&route.operation_id)?
                .ok_or(StorageError::ScopeDenied)?;
            for journal in &operation.attempts {
                let Some(attempt) = s.usage().inference_attempt(&journal.id.0)? else {
                    continue;
                };
                if devworks_core::services::usage::accounting::snapshot(&attempt)?.admission_key
                    != *admission_key
                {
                    continue;
                }
                let page = s.history().page(
                    &EventFilter::Aggregate {
                        kind: "provider_result".into(),
                        id: attempt.attempt_id.as_str().into(),
                    },
                    0,
                    1,
                )?;
                if let Some(event) = page.events.first() {
                    let body = &event.payload;
                    let outcome = match body["outcome"].as_str() {
                        Some("Completed") => Some(AttemptEnd::Completed),
                        Some("ToolCallsReady") => Some(AttemptEnd::ToolCallsReady),
                        Some("Incomplete") => Some(AttemptEnd::Incomplete),
                        Some("Failed") => Some(AttemptEnd::Failed),
                        Some("Cancelled") => Some(AttemptEnd::Cancelled),
                        Some("UnknownTail") => Some(AttemptEnd::UnknownTail),
                        _ => None,
                    };
                    return Ok(ProviderResult {
                        attempt_id: serde_json::from_value(body["attemptId"].clone())
                            .map_err(|_| invalid("stored provider attempt identity"))?,
                        binding: serde_json::from_value(body["binding"].clone())
                            .map_err(|_| invalid("stored provider binding"))?,
                        turn_start_sequence: body["turnStartSequence"].as_i64(),
                        source_context: serde_json::from_value(body["sourceContext"].clone())
                            .map_err(|_| invalid("stored provider source context"))?,
                        instruction_targets: serde_json::from_value(
                            body["instructionTargets"].clone(),
                        )
                        .map_err(|_| invalid("stored provider instruction targets"))?,
                        vector_artifacts: serde_json::from_value(body["vectorArtifacts"].clone())
                            .map_err(|_| invalid("stored provider vectors"))?,
                        result_artifact: serde_json::from_value(body["resultArtifact"].clone())
                            .map_err(|_| invalid("stored provider result"))?,
                        outcome,
                        faults: serde_json::from_value(body["faults"].clone())
                            .map_err(|_| invalid("stored provider fault"))?,
                        retry_at_ms: body["retryAtMs"].as_i64(),
                    });
                }
            }
            Ok(ProviderResult::default())
        })
    }
    /// Only closed core metadata can scope an ephemeral producer registration. The actual
    /// connection/access/current-route/accounting gates still run for every native attempt.
    pub(crate) fn for_qualified_embedding(
        &self,
        context: &InvocationContext,
        binding: &devworks_core::services::router::embedding_spaces::QualifiedEmbeddingBinding,
    ) -> Result<Arc<Self>, StorageError> {
        self.scoped_embedding(
            context,
            binding.connection(),
            binding.space(),
            binding.upstream_model_id(),
            binding.dimension_mode(),
        )
    }
    pub(crate) fn for_embedding_probe(
        &self,
        probe: &devworks_core::services::router::embedding_spaces::PreparedEmbeddingProbe,
    ) -> Result<Arc<Self>, StorageError> {
        self.scoped_embedding(
            probe.context(),
            probe.connection(),
            probe.space(),
            probe.upstream_model_id(),
            probe.dimension_mode(),
        )
    }
    fn scoped_embedding(
        &self,
        context: &InvocationContext,
        connection: &StoredConnection,
        space: &devworks_core::protocol::data::EmbeddingSpace,
        upstream: &str,
        dimensions: devworks_core::providers::capabilities::EmbeddingDimensions,
    ) -> Result<Arc<Self>, StorageError> {
        if connection.profile != "Embedding" {
            return Err(StorageError::ScopeDenied);
        }
        let epoch = match context.session_id() {
            Some(id) => read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
                Ok(read.sessions().session(id)?.map(|s| s.context_epoch))
            })?
            .ok_or(StorageError::ScopeDenied)?,
            None => 0,
        };
        let mut capabilities = Capabilities::unknown(NativeBinding {
            connection_id: connection.id.clone(),
            account_revision: Revision::new(connection.account_revision)
                .map_err(|_| invalid("embedding account revision"))?,
            endpoint_profile: EndpointProfile::Embedding,
            model_id: devworks_core::services::router::connections::discovered_model_id(
                "Embedding",
                &connection.id,
                upstream,
            ),
            context_epoch: Counter::new(epoch).map_err(|_| invalid("embedding context epoch"))?,
        });
        capabilities.embeddings = Support::Supported;
        capabilities.embedding_dimensions = dimensions;
        let mut composition = self.composition.clone();
        composition.embedding_spaces = vec![EmbeddingRegistration {
            space: space.clone(),
            connection_id: connection.id.clone(),
            upstream_model_id: upstream.into(),
            capabilities,
        }];
        Ok(Self::new(
            self.store.clone(),
            self.artifacts.clone(),
            self.connections.clone(),
            self.usage.clone(),
            self.schemas.clone(),
            composition,
        ))
    }
    /// Builds the internal admission selection from CURRENT discovery plus this host's explicit
    /// space registration. It cannot create a conversational model or mutate a session pin.
    pub fn embedding_selection(
        &self,
        space: &Id,
    ) -> Result<
        (
            devworks_core::services::router::accounting::AttemptSelection,
            devworks_core::ports::usage::PriceQuery,
        ),
        StorageError,
    > {
        let registration = self
            .composition
            .embedding_spaces
            .iter()
            .find(|r| &r.space.space_id == space)
            .ok_or(StorageError::ScopeDenied)?;
        let connection = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |s| {
            s.router().connection(&registration.connection_id)
        })?
        .ok_or(StorageError::ScopeDenied)?;
        if connection.lifecycle != "Connected"
            || registration.capabilities.binding.account_revision.get()
                != connection.account_revision
            || registration.capabilities.binding.connection_id != connection.id
            || registration.capabilities.binding.endpoint_profile != EndpointProfile::Embedding
            || !registration.capabilities.embeddings.supported()
        {
            return Err(StorageError::ScopeDenied);
        }
        if connection.profile != "Embedding"
            || !connection.enabled
            || !connection
                .discovered
                .as_ref()
                .is_some_and(|v| v.iter().any(|m| m.id == registration.upstream_model_id))
        {
            return Err(StorageError::ScopeDenied);
        }
        let model = devworks_core::services::router::connections::discovered_model_id(
            "Embedding",
            &connection.id,
            &registration.upstream_model_id,
        );
        if registration.capabilities.binding.model_id != model {
            return Err(StorageError::ScopeDenied);
        }
        Ok((
            devworks_core::services::router::accounting::AttemptSelection::Embedding {
                embedding_space_id: space.clone(),
                dimensions: registration.space.dimensions,
                upstream_model_id: registration.upstream_model_id.clone(),
                connection_revision: connection.revision,
                registration_digest: registration.digest(),
            },
            devworks_core::ports::usage::PriceQuery {
                catalog_model_id: model,
                provider_model_id: Some(registration.upstream_model_id.clone()),
                connection_id: connection.id,
                endpoint_profile: "Embedding".into(),
                access_mode: "Api".into(),
                service_tier: None,
                input_tokens: None,
                cache_write_ttl_ms: None,
                processing_region: None,
                required_modalities: vec!["Text".into()],
            },
        ))
    }
    fn capabilities(
        &self,
        request: &InferenceRequest,
        binding: &NativeBinding,
        connection: &StoredConnection,
        model: Option<StoredRouteModel>,
        upstream: &str,
        selection: &devworks_core::services::router::accounting::AttemptSelection,
    ) -> Result<Capabilities, StorageError> {
        let mut capabilities = self
            .composition
            .capabilities
            .iter()
            .find(|c| {
                c.binding.connection_id == binding.connection_id
                    && c.binding.account_revision == binding.account_revision
                    && c.binding.model_id == binding.model_id
                    && c.binding.endpoint_profile == binding.endpoint_profile
            })
            .cloned()
            .unwrap_or_else(|| Capabilities::unknown(binding.clone()));
        match &request.input {
            InferenceInput::Embedding {
                embedding_space_id,
                dimensions,
                ..
            } => {
                let registration = self
                    .composition
                    .embedding_spaces
                    .iter()
                    .find(|r| {
                        r.space.space_id == *embedding_space_id
                            && r.space.dimensions == *dimensions
                            && r.capabilities.binding.account_revision == binding.account_revision
                            && r.connection_id == binding.connection_id
                            && r.upstream_model_id == upstream
                    })
                    .ok_or(StorageError::ScopeDenied)?;
                let devworks_core::services::router::accounting::AttemptSelection::Embedding {
                    registration_digest,
                    ..
                } = selection
                else {
                    return Err(StorageError::ScopeDenied);
                };
                if registration_digest != &registration.digest() {
                    return Err(StorageError::ScopeDenied);
                }
                capabilities = registration.capabilities.clone();
            }
            InferenceInput::Generation { .. } => {
                let model = model.ok_or(StorageError::ScopeDenied)?;
                if !model.enabled
                    || model.endpoint_profile != connection.profile
                    || model.upstream_model_id != upstream
                {
                    return Err(StorageError::ScopeDenied);
                }
                capabilities.images = if model.image_input {
                    Support::Supported
                } else {
                    Support::Unsupported
                };
                capabilities.tools = if model.tools {
                    Support::Supported
                } else {
                    Support::Unsupported
                };
                let declared = model
                    .efforts
                    .iter()
                    .map(|e| e.to_ascii_lowercase())
                    .collect::<Vec<_>>();
                if capabilities.efforts.is_empty() {
                    use devworks_core::providers::capabilities::EffortEncoding;
                    for value in &declared {
                        if value == "default" {
                            continue;
                        }
                        let encoding = match connection.profile.as_str() {
                            "OpenAiResponses" | "CodexSubscriptionResponses" => {
                                Some(EffortEncoding::ResponsesReasoning {
                                    value: value.clone(),
                                })
                            }
                            "AnthropicMessages" | "ClaudeSubscriptionMessages" => {
                                Some(EffortEncoding::MessagesOutputConfig {
                                    value: value.clone(),
                                })
                            }
                            "OpenRouterChat" => Some(EffortEncoding::ReasoningEffort {
                                value: value.clone(),
                            }),
                            _ => None,
                        };
                        if let Some(encoding) = encoding {
                            capabilities.efforts.push(value.clone());
                            capabilities
                                .effort_encodings
                                .insert(value.clone(), encoding);
                        }
                    }
                } else {
                    capabilities
                        .efforts
                        .retain(|e| declared.contains(&e.to_ascii_lowercase()) || e == "none");
                }
            }
        }
        capabilities.binding = binding.clone();
        Ok(capabilities)
    }
    fn prepare(
        &self,
        context: &InvocationContext,
        request: &InferenceRequest,
        attempt: &StoredInferenceAttempt,
        targets: Option<&[(Id, Vec<String>)]>,
    ) -> Result<(PreparedRequest, StoredConnection), StorageError> {
        let bound = devworks_core::services::usage::accounting::snapshot(attempt)?;
        let (connection, model) =
            read_snapshot(self.store.as_ref(), &ReadOptions::default(), |s| {
                Ok((
                    s.router()
                        .connection(&attempt.connection_id)?
                        .ok_or(StorageError::ScopeDenied)?,
                    s.router().route_model(&attempt.catalog_model_id)?,
                ))
            })?;
        if bound.quote.connection_revision != Some(connection.revision)
            || connection.profile != attempt.endpoint_profile
        {
            return Err(StorageError::ScopeDenied);
        }
        let profile: EndpointProfile =
            serde_json::from_value(serde_json::Value::String(connection.profile.clone()))
                .map_err(|_| invalid("unqualified provider profile"))?;
        let context_epoch = match &request.input {
            InferenceInput::Generation {
                session_id,
                context_epoch,
                ..
            } => {
                if attempt.session_id.as_ref() != Some(session_id)
                    || bound.context_epoch != Some(context_epoch.get())
                {
                    return Err(StorageError::ScopeDenied);
                }
                context_epoch.clone()
            }
            InferenceInput::Embedding { .. } => {
                Counter::new(bound.context_epoch.unwrap_or(0)).map_err(invalid)?
            }
        };
        let binding = NativeBinding {
            connection_id: connection.id.clone(),
            account_revision: Revision::new(connection.account_revision).map_err(invalid)?,
            endpoint_profile: profile,
            model_id: attempt.catalog_model_id.clone(),
            context_epoch,
        };
        let upstream =
            super::wire_model::registered_upstream(&connection, model.as_ref(), &bound.query)?;
        let capabilities = self.capabilities(
            request,
            &binding,
            &connection,
            model,
            &upstream,
            &bound.selection,
        )?;
        let identity = RequestIdentity {
            request_id: attempt.request_id.clone(),
            attempt_id: AttemptId(attempt.attempt_id.clone()),
            binding,
            upstream_model: upstream,
        };
        let constraints = connection
            .open_router
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|_| invalid("current OpenRouter policy"))?;
        self.verify_instruction_context(context, request, targets)?;
        let materials = OwnedMaterials {
            store: self.store.as_ref(),
            artifacts: self.artifacts.as_ref(),
            context,
            schemas: self.schemas.as_ref(),
        };
        let prepared = encode::prepare(
            identity,
            request,
            &capabilities,
            &materials,
            constraints.as_ref(),
            &PreparationLimits {
                context_bytes: 8 * 1024 * 1024,
                artifact_bytes: 32 * 1024 * 1024,
                request_bytes: 32 * 1024 * 1024,
            },
        )
        .map_err(|_| invalid("provider request materialization refused"))?;
        Ok((prepared, connection))
    }
}
type SharedPrepared = Arc<Mutex<Option<PreparedRequest>>>;
struct LazySource {
    owner: Arc<NativeProviders>,
    context: InvocationContext,
    request: InferenceRequest,
    shared: SharedPrepared,
    inner: Option<NativeProviderSource>,
    source_fences: Vec<devworks_core::ports::AuthorityFence>,
    context_head_fence: Option<crate::tool_turn::ContextHeadFence>,
    instruction_targets: Option<InstructionTargets>,
}
impl OwnedInferenceSource for LazySource {
    fn start(&mut self, attempt: &StoredInferenceAttempt) -> Result<(), StorageError> {
        self.start_cancellable(attempt, &Cancellation::new())
    }
    fn start_cancellable(
        &mut self,
        attempt: &StoredInferenceAttempt,
        cancellation: &Cancellation,
    ) -> Result<(), StorageError> {
        let (prepared, connection) = self.owner.prepare(
            &self.context,
            &self.request,
            attempt,
            self.instruction_targets.as_deref(),
        )?;
        let limits = StreamLimits {
            max_request_bytes: 32 * 1024 * 1024,
            deadline_ms: u64::from(self.request.options.deadline_ms),
            idle_timeout_ms: u64::from(self.request.options.deadline_ms).min(60_000),
            max_response_bytes: 32 * 1024 * 1024,
            max_header_bytes: 65536,
            max_headers: 128,
        };
        let mut source = NativeProviderSource::new(
            prepared.clone(),
            self.owner.connections.clone(),
            self.owner.composition.http.clone(),
            limits,
            connection.endpoint,
            connection.credential_revision,
        );
        let owner = self.owner.clone();
        let context = self.context.clone();
        let captured = attempt.clone();
        let publication = prepared.clone();
        let expected_revision = connection.revision;
        let source_fences = self.source_fences.clone();
        let context_head_fence = self.context_head_fence.clone();
        let instruction_request = self.request.clone();
        let instruction_targets = self.instruction_targets.clone();
        source = source.with_send_gate(Box::new(move || {
            use devworks_core::providers::materialize::SchemaDocuments;
            owner.verify_instruction_context(
                &context,
                &instruction_request,
                instruction_targets.as_deref(),
            )?;
            for tool in &publication.tools {
                owner
                    .schemas
                    .tool(
                        &context,
                        &ToolSchema {
                            wire_name: tool.wire_name.clone(),
                            capability_id: tool.capability_id.clone(),
                            input_schema: tool.input_schema.clone(),
                        },
                    )
                    .map_err(|_| StorageError::ScopeDenied)?;
            }
            let context = context.clone();
            let captured = captured.clone();
            let publication = publication.clone();
            let source_fences = source_fences.clone();
            let context_head_fence = context_head_fence.clone();
            owner
                .usage
                .accounting_fence()
                .write(owner.store.as_ref(), move |tx| {
                    super::sink::validate(tx, &context, &captured)?;
                    if let Some(fence) = &context_head_fence {
                        fence.check(tx)?;
                    }
                    if !devworks_core::authority::fences_hold(tx, &source_fences)? {
                        return Err(StorageError::ScopeDenied);
                    }
                    let connection = tx
                        .router()
                        .connection(&captured.connection_id)?
                        .ok_or(StorageError::ScopeDenied)?;
                    // External aliases are refused before admission; an unrelated package
                    // registry edit cannot change this built-in-only request.
                    if connection.revision != expected_revision
                        || connection.account_revision
                            != publication.identity.binding.account_revision.get()
                        || !connection.enabled
                    {
                        return Err(StorageError::ScopeDenied);
                    }
                    for reference in &publication.artifact_refs {
                        if !devworks_core::services::sessions::transcript::reference_resolves(
                            tx,
                            &context,
                            captured.project_id.as_ref(),
                            reference,
                        )? {
                            return Err(StorageError::ScopeDenied);
                        }
                    }
                    Ok(())
                })
        }));
        *self.shared.lock().unwrap_or_else(|p| p.into_inner()) = Some(prepared);
        let result = source.start_cancellable(attempt, cancellation);
        self.inner = Some(source);
        result
    }
    fn start_failure_outcome(&self) -> AttemptEnd {
        self.inner
            .as_ref()
            .map_or(AttemptEnd::Failed, |s| s.start_failure_outcome())
    }
    fn next(&mut self, c: &Cancellation) -> Result<Option<InferenceFrame>, StorageError> {
        self.inner
            .as_mut()
            .ok_or(StorageError::ScopeDenied)?
            .next(c)
    }
    fn close(&mut self) -> Result<(), StorageError> {
        match self.inner.as_mut() {
            Some(s) => s.close(),
            None => Ok(()),
        }
    }
}
struct LazySink {
    owner: Arc<NativeProviders>,
    context: InvocationContext,
    request: InferenceRequest,
    instruction_targets: Option<InstructionTargets>,
    shared: SharedPrepared,
    result: ResultChannel,
    turn_start_sequence: Option<i64>,
    detached: bool,
    inner: Option<ProviderSink>,
}
impl LazySink {
    fn get(&mut self) -> Result<Option<&mut ProviderSink>, StorageError> {
        if self.inner.is_none()
            && let Some(prepared) = self
                .shared
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        {
            let embedding = match &self.request.input {
                InferenceInput::Embedding {
                    embedding_space_id,
                    inputs,
                    ..
                } => {
                    let registration = self
                        .owner
                        .composition
                        .embedding_spaces
                        .iter()
                        .find(|r| r.space.space_id == *embedding_space_id)
                        .ok_or(StorageError::ScopeDenied)?;
                    Some((registration.space.clone(), inputs.clone()))
                }
                _ => None,
            };
            let sink = ProviderSink::new(
                self.owner.store.clone(),
                self.owner.artifacts.clone(),
                self.context.clone(),
                prepared,
                SourceProvenance {
                    context: match &self.request.input {
                        InferenceInput::Generation { context, .. } => Some(context.clone()),
                        InferenceInput::Embedding { .. } => None,
                    },
                    instruction_targets: self.instruction_targets.clone(),
                },
                embedding,
                self.result.clone(),
            )
            .with_turn_start_sequence(self.turn_start_sequence);
            self.inner = Some(
                if matches!(self.request.purpose, InferencePurpose::Compaction) {
                    sink.with_compaction()
                } else if self.detached {
                    sink.with_sampled_text()
                } else {
                    sink.with_live(self.owner.live.clone())
                },
            );
        }
        Ok(self.inner.as_mut())
    }
}
impl OwnedInferenceSink for LazySink {
    fn response_head(
        &mut self,
        a: &StoredInferenceAttempt,
        h: devworks_core::providers::ProviderResponseHead,
        w: &AccountingWriteFence,
    ) -> Result<(), StorageError> {
        self.get()?
            .ok_or(StorageError::ScopeDenied)?
            .response_head(a, h, w)
    }

    fn accept(
        &mut self,
        a: &StoredInferenceAttempt,
        f: devworks_core::providers::decode::DecodedFrame,
        w: &AccountingWriteFence,
    ) -> Result<(), StorageError> {
        self.get()?
            .ok_or(StorageError::ScopeDenied)?
            .accept(a, f, w)
    }
    fn finish(
        &mut self,
        a: &StoredInferenceAttempt,
        o: AttemptEnd,
        w: &AccountingWriteFence,
    ) -> Result<(), StorageError> {
        match self.get()? {
            Some(s) => s.finish(a, o, w),
            None => Ok(()),
        }
    }
}
fn invalid(message: impl Into<String>) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
