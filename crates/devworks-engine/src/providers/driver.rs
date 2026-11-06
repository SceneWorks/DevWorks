//! Native producer composition. Only trusted host configuration can qualify capabilities/spaces.
use super::{
    schemas::RegisteredSchemas,
    sink::{ProviderResult, ProviderSink, ResultChannel},
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
        }
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
}
pub struct ProviderInvocation {
    pub accounting: DriveOutcome,
    pub result: ProviderResult,
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
        })
    }
    pub fn schemas(&self) -> &Arc<RegisteredSchemas> {
        &self.schemas
    }
    pub async fn produce(
        self: &Arc<Self>,
        context: InvocationContext,
        mut logical: RequestAdmission,
        category: ReportCategory,
        mut attempt: AttemptRequest,
        request: InferenceRequest,
    ) -> Result<ProviderInvocation, StorageError> {
        request.validate().map_err(invalid)?;
        let canonical = devworks_core::jcs::canonicalize(
            &serde_json::to_value(&request).map_err(|_| invalid("provider request JSON"))?,
        )
        .map_err(|_| invalid("provider request canonical JSON"))?;
        logical.args_digest = sha256_hex(canonical.as_bytes());
        if request.request_id != logical.request_id || request.request_id != attempt.request_id {
            return Err(StorageError::ScopeDenied);
        }
        use devworks_core::services::usage::meter::InputSemantics;
        attempt.pricing.input_semantics = self
            .composition
            .usage_semantics
            .get(attempt.query.catalog_model_id.as_str())
            .copied()
            .unwrap_or(match attempt.query.endpoint_profile.as_str() {
                "AnthropicMessages" | "ClaudeSubscriptionMessages" => InputSemantics::Disjoint,
                "OpenAiResponses" | "CodexSubscriptionResponses" | "OpenRouterChat" => {
                    InputSemantics::Inclusive
                }
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
        };
        let sink = LazySink {
            owner: self.clone(),
            context: context.clone(),
            request,
            shared,
            result: result.clone(),
            inner: None,
        };
        let accounting = self
            .usage
            .produce_with_sink(
                context,
                logical,
                category,
                attempt,
                Box::new(source),
                Some(Box::new(sink)),
            )
            .await?;
        let result = result.lock().unwrap_or_else(|p| p.into_inner()).clone();
        Ok(ProviderInvocation { accounting, result })
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
    fn prepare(
        &self,
        context: &InvocationContext,
        request: &InferenceRequest,
        attempt: &StoredInferenceAttempt,
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
            InferenceInput::Embedding { .. } => Counter::new(0).expect("zero"),
        };
        let binding = NativeBinding {
            connection_id: connection.id.clone(),
            account_revision: Revision::new(connection.account_revision).map_err(invalid)?,
            endpoint_profile: profile,
            model_id: attempt.catalog_model_id.clone(),
            context_epoch,
        };
        let upstream = bound
            .query
            .provider_model_id
            .clone()
            .ok_or(StorageError::ScopeDenied)?;
        let mut capabilities = self
            .composition
            .capabilities
            .iter()
            .find(|c| {
                c.binding.connection_id == binding.connection_id
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
                            && r.connection_id == binding.connection_id
                            && r.upstream_model_id == upstream
                    })
                    .ok_or(StorageError::ScopeDenied)?;
                let devworks_core::services::router::accounting::AttemptSelection::Embedding {
                    registration_digest,
                    ..
                } = &bound.selection
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
        let registry_revision =
            read_snapshot(self.owner.store.as_ref(), &ReadOptions::default(), |s| {
                s.packages().registry_revision()
            })?;
        let (prepared, connection) = self.owner.prepare(&self.context, &self.request, attempt)?;
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
        source = source.with_send_gate(Box::new(move || {
            use devworks_core::providers::materialize::SchemaDocuments;
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
            owner
                .usage
                .accounting_fence()
                .write(owner.store.as_ref(), move |tx| {
                    super::sink::validate(tx, &context, &captured)?;
                    let connection = tx
                        .router()
                        .connection(&captured.connection_id)?
                        .ok_or(StorageError::ScopeDenied)?;
                    if connection.revision != expected_revision
                        || connection.account_revision
                            != publication.identity.binding.account_revision.get()
                        || !connection.enabled
                        || tx.packages().registry_revision()? != registry_revision
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
    shared: SharedPrepared,
    result: ResultChannel,
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
            self.inner = Some(ProviderSink::new(
                self.owner.store.clone(),
                self.owner.artifacts.clone(),
                self.context.clone(),
                prepared,
                embedding,
                self.result.clone(),
            ));
        }
        Ok(self.inner.as_mut())
    }
}
impl OwnedInferenceSink for LazySink {
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
