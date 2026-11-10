//! The actual owned producer, SQLite accounting, current connection owner, artifacts and
//! SessionWriter. Controlled transport bytes exercise native protocols without model execution.
mod common;
use common::*;
use devworks_adapters::fs::{
    NativeFilesystem, artifacts::NativeArtifactFiles, sources::HostArtifactSources,
};
use devworks_adapters::index::{NativeSourceTree, treesitter::TreeSitterParser};
use devworks_core::authority::service::bind_principal;
use devworks_core::{
    context::{InvocationContext, PrincipalKind},
    ports::{
        IdKind, IdSource,
        http::{Entropy, HttpRequest, NoNetwork},
        provider_http::*,
        secrets::{Secret, SecretKey, SecretStore, SessionSecrets},
        storage::*,
        usage::{PriceQuery, PriceResolver},
    },
    protocol::{ActorId, AttemptId, Counter, Id, Protocol, Revision, provider::*},
    providers::{
        capabilities::{Capabilities, EmbeddingDimensions, Support},
        context::*,
        materialize::SchemaDocuments,
    },
    services::{
        UseCases,
        artifacts::{ArtifactService, version_ref},
        router::{
            InferenceRouter, Purpose, RequestAdmission,
            accounting::{AttemptSelection, ReportCategory},
            connections::ConnectionService,
        },
        runs::ControlService,
        usage::{
            accounting::{AttemptEnd, AttemptRequest},
            host::DriveOutcome,
            meter::InputSemantics,
            pricing::PricingContext,
        },
    },
};
use devworks_engine::{
    providers::{
        driver::{EmbeddingRegistration, InstructionTargets, NativeProviders, ProviderComposition},
        schemas::RegisteredSchemas,
    },
    usage::UsageRuntime,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
struct EntropyFixture;
impl Entropy for EntropyFixture {
    fn fill(&self, out: &mut [u8]) {
        out.fill(7);
    }
}
struct Script {
    status: u16,
    body: VecDeque<Vec<u8>>,
    content_type: &'static str,
    requests: Arc<Mutex<Vec<HttpRequest>>>,
    closed: Arc<AtomicBool>,
    reads: Arc<AtomicUsize>,
    after_open: Option<Arc<dyn Fn() + Send + Sync>>,
    headers: Vec<(String, String)>,
    close_gate: Option<Arc<(Mutex<bool>, std::sync::Condvar)>>,
    closing: Arc<tokio::sync::Notify>,
    wait_for_cancel: bool,
    waiting: Arc<tokio::sync::Notify>,
}
impl Script {
    fn sse(body: &str) -> Self {
        Self {
            status: 200,
            body: VecDeque::from([body.as_bytes().to_vec()]),
            content_type: "text/event-stream",
            requests: Default::default(),
            closed: Default::default(),
            reads: Default::default(),
            after_open: None,
            headers: vec![],
            close_gate: None,
            closing: Default::default(),
            wait_for_cancel: false,
            waiting: Default::default(),
        }
    }
    fn aggregate(body: &str) -> Self {
        let mut s = Self::sse(body);
        s.content_type = "application/json";
        s
    }
}
struct ScriptHttp(Mutex<Script>);
impl std::ops::Deref for ScriptHttp {
    type Target = Mutex<Script>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl StreamingHttp for ScriptHttp {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        c: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        if c.is_cancelled() {
            return Err(TransportError {
                phase: SendPhase::NotSent,
                kind: TransportFailure::Cancelled,
            });
        }
        let mut script = self.lock().unwrap();
        script.requests.lock().unwrap().push(request.clone());
        if let Some(f) = &script.after_open {
            f();
        }
        Ok(Box::new(Stream {
            head: ResponseHead {
                status: script.status,
                headers: std::iter::once(("content-type".into(), script.content_type.into()))
                    .chain(script.headers.clone())
                    .collect(),
            },
            body: std::mem::take(&mut script.body),
            closed: script.closed.clone(),
            reads: script.reads.clone(),
            close_gate: script.close_gate.clone(),
            closing: script.closing.clone(),
            wait_for_cancel: script.wait_for_cancel,
            waiting: script.waiting.clone(),
        }))
    }
}
struct Stream {
    head: ResponseHead,
    body: VecDeque<Vec<u8>>,
    closed: Arc<AtomicBool>,
    reads: Arc<AtomicUsize>,
    close_gate: Option<Arc<(Mutex<bool>, std::sync::Condvar)>>,
    closing: Arc<tokio::sync::Notify>,
    wait_for_cancel: bool,
    waiting: Arc<tokio::sync::Notify>,
}
struct Close(Arc<AtomicBool>);
impl StreamCloser for Close {
    fn close(&self) -> Result<(), TransportError> {
        self.0.store(true, Ordering::SeqCst);
        Ok(())
    }
}
impl HttpStream for Stream {
    fn head(&self) -> &ResponseHead {
        &self.head
    }
    fn closer(&self) -> Arc<dyn StreamCloser> {
        Arc::new(Close(self.closed.clone()))
    }
    fn read(&mut self, out: &mut [u8], cancel: &Cancellation) -> Result<usize, TransportError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let Some(mut bytes) = self.body.pop_front() else {
            if self.wait_for_cancel {
                self.waiting.notify_one();
                let signal = (Mutex::new(()), std::sync::Condvar::new());
                while !cancel.is_cancelled() {
                    drop(
                        signal
                            .1
                            .wait_timeout(
                                signal.0.lock().unwrap(),
                                std::time::Duration::from_millis(2),
                            )
                            .unwrap(),
                    );
                }
                return Err(TransportError {
                    phase: SendPhase::AfterSend,
                    kind: TransportFailure::Cancelled,
                });
            }
            return Ok(0);
        };
        let n = bytes.len().min(out.len());
        out[..n].copy_from_slice(&bytes[..n]);
        if n < bytes.len() {
            let rest = bytes.split_off(n);
            self.body.push_front(rest);
        }
        Ok(n)
    }
    fn close(&mut self) -> Result<(), TransportError> {
        self.closing.notify_one();
        if let Some(gate) = &self.close_gate {
            let mut ready = gate.0.lock().unwrap();
            while !*ready {
                ready = gate.1.wait(ready).unwrap();
            }
        }
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}
struct Fx {
    h: Harness,
    store: Arc<dyn ApplicationStore>,
    artifacts: Arc<ArtifactService>,
    connections: Arc<ConnectionService>,
    usage: Arc<UsageRuntime>,
    driver: Arc<NativeProviders>,
    context: InvocationContext,
    http: Arc<ScriptHttp>,
    profile: String,
    model: Id,
    source_fences: Mutex<Vec<devworks_core::ports::AuthorityFence>>,
    instruction_targets: Mutex<Option<InstructionTargets>>,
}
impl Fx {
    fn new(profile: &str, script: Script, embedding: bool) -> Self {
        Self::with_access(
            profile,
            script,
            embedding,
            Arc::new(NoNetwork),
            Arc::new(SessionSecrets::new()),
        )
    }
    fn with_access(
        profile: &str,
        script: Script,
        embedding: bool,
        access_http: Arc<dyn devworks_core::ports::http::HttpTransport>,
        secrets: Arc<dyn SecretStore>,
    ) -> Self {
        let h = Harness::new();
        let store = h.open();
        let context =
            principal(store.as_ref(), "host", PrincipalKind::Host).with_session(id("session"));
        let artifacts = Arc::new(ArtifactService::new(
            Arc::new(NativeArtifactFiles::new(
                h.root.clone(),
                h.root.with_extension("cache"),
            )),
            Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
            Arc::new(NativeFilesystem::new()),
            h.clock(),
            h.ids.clone(),
        ));
        let oauth = matches!(
            profile,
            "CodexSubscriptionResponses" | "ClaudeSubscriptionMessages"
        );
        let keyed = !matches!(profile, "CompatibleChat" | "Embedding");
        if keyed {
            use devworks_core::services::router::connections::auth::{Credential, Tokens};
            let credential = if oauth {
                Credential::OAuth(Tokens {
                    access: Secret::new("actual-fixture-access"),
                    refresh: Secret::new("fixture-refresh"),
                    expires_at_ms: T0 + 86_400_000,
                    account: Some("fixture-account".into()),
                })
            } else {
                Credential::ApiKey(Secret::new("actual-fixture-key"))
            };
            secrets
                .put(
                    &SecretKey::new(store.info().store_id.0.as_str(), id("credential")),
                    &credential.to_secret(),
                )
                .unwrap();
        }
        let connections = Arc::new(ConnectionService::new(
            h.ids.clone(),
            h.clock(),
            store.clone(),
            access_http,
            secrets,
            Arc::new(EntropyFixture),
        ));
        let model = if embedding {
            devworks_core::services::router::connections::discovered_model_id(
                "Embedding",
                &id("connection"),
                "fixture-model",
            )
        } else {
            id("model")
        };
        let m = model.clone();
        let p = profile.to_owned();
        write(store.as_ref(), move |tx| {
            let restore_epoch=tx.store().restore_epoch;
            tx.router().put_connection(
                &StoredConnection {
                    id: id("connection"),
                    name: "Explicit configured fixture".into(),
                    profile: p.clone(),
                    endpoint: if keyed {"https://provider.test/v1"}else{"http://localhost:11434/v1"}.into(),
                    enabled: true,
                    local_free: true,
                    open_router: None,
                    credential_ref: keyed.then(||id("credential")),
                    credential_revision: i64::from(keyed),
                    account_revision: 1,
                    refresh_operation_id: None,
                    credential_epoch: keyed.then_some(restore_epoch),
                    lifecycle: "Connected".into(),
                    state_reason: None,
                    account_digest: oauth.then(||devworks_core::services::router::connections::profiles::account_digest("fixture-account")),
                    access_expires_at_ms: None,
                    risk: (p=="ClaudeSubscriptionMessages").then(||StoredRiskAcceptance {notice_sha256:devworks_core::services::router::connections::profiles::risk_notice_sha256(),binding:devworks_core::services::router::connections::profiles::risk_binding(&p,"https://provider.test/v1",Some(&devworks_core::services::router::connections::profiles::account_digest("fixture-account"))),restore_epoch,accepted_by:"fixture-user".into(),accepted_at_ms:T0}),
                    discovered: Some(vec![DiscoveredModel {
                        id: "fixture-model".into(),
                        context_tokens: Some(10000),
                    }]),
                    discovered_at_ms: Some(T0),
                    healthy: Some(true),
                    checked_at_ms: Some(T0),
                    revision: 1,
                    created_at_ms: T0,
                    updated_at_ms: T0,
                },
                None,
            )?;
            if !embedding {
                tx.router().put_route_model(
                    &StoredRouteModel {
                        id: m,
                        display_name: "Fixture model".into(),
                        endpoint_profile: p,
                        upstream_model_id: "fixture-model".into(),
                        enabled: true,
                        efforts: vec![],
                        image_input: false,
                        tools: true,
                        context_tokens: Some(10000),
                        connection_id: Some(id("connection")),
                        position: 1,
                        revision: 1,
                        updated_at_ms: T0,
                    },
                    None,
                )?;
            }
            tx.sessions().insert_session(&StoredSession {
                id: id("session"),
                conversation_id: id("conversation"),
                agent_id: ActorId(id("host")),
                project_id: None,
                root_id: None,
                title: "Fixture session".into(),
                context_epoch: 0,
                revision: 1,
                created_by: ActorId(id("host")),
                created_at_ms: T0,
                closed_at_ms: None,
                closed_by: None,
                next_sequence: 1,
                checkpoint_sequence: 0,
            })
        })
        .unwrap();
        let router = Arc::new(InferenceRouter::new(
            h.ids.clone(),
            h.clock(),
            connections.clone(),
        ));
        let usage = UsageRuntime::new(
            store.clone(),
            Arc::new(ControlService::new(h.ids.clone())),
            Some(router.clone()),
        );
        let cases = UseCases::new()
            .with_router(router)
            .with_connections(connections.clone())
            .with_sessions(Arc::new(
                devworks_core::services::sessions::SessionService::new(
                    h.ids.clone(),
                    store.clone(),
                ),
            ));
        let schemas = Arc::new(RegisteredSchemas::new(
            store.clone(),
            cases,
            None,
            artifacts.clone(),
        ));
        let http = Arc::new(ScriptHttp(Mutex::new(script)));
        let mut config = ProviderComposition::new(http.clone());
        if embedding {
            let binding = NativeBinding {
                connection_id: id("connection"),
                account_revision: Revision::new(1).unwrap(),
                endpoint_profile: EndpointProfile::Embedding,
                model_id: model.clone(),
                context_epoch: Counter::new(0).unwrap(),
            };
            let mut capabilities = Capabilities::unknown(binding);
            capabilities.embeddings = Support::Supported;
            capabilities.embedding_dimensions = EmbeddingDimensions::Fixed(2);
            config.embedding_spaces.push(EmbeddingRegistration {
                space: devworks_core::protocol::data::EmbeddingSpace {
                    space_id: id("space"),
                    model_digest: "a".repeat(64).try_into().unwrap(),
                    dimensions: 2,
                    metric: devworks_core::protocol::data::Metric::Cosine,
                },
                connection_id: id("connection"),
                upstream_model_id: "fixture-model".into(),
                capabilities,
            });
        }
        let driver = NativeProviders::new(
            store.clone(),
            artifacts.clone(),
            connections.clone(),
            usage.clone(),
            schemas,
            config,
        );
        Self {
            h,
            store,
            artifacts,
            connections,
            usage,
            driver,
            context,
            http,
            profile: profile.into(),
            model,
            source_fences: Mutex::new(Vec::new()),
            instruction_targets: Mutex::new(None),
        }
    }
    fn bytes(&self, bytes: &[u8]) -> ArtifactRef {
        let staged = self
            .artifacts
            .stage_host_bytes(self.store.as_ref(), bytes, json!({"fixture":"input"}))
            .unwrap()
            .unwrap();
        let file = staged.clone();
        let service = self.artifacts.clone();
        let version = write(self.store.as_ref(), move |tx| {
            service.commit_host_bytes(
                tx,
                &file,
                "Fixture input",
                "application/json",
                json!({"fixture":true}),
            )
        })
        .unwrap();
        self.artifacts
            .settle_host_bytes(self.store.as_ref(), &staged, true);
        version_ref(&version)
    }
    fn generation(&self, tools: Vec<ToolSchema>) -> InferenceRequest {
        let context = ProviderContext {
            version: 1,
            instructions: vec![TextBlock {
                text: "Fixture instructions".into(),
                cache: None,
            }],
            instruction_stamps: vec![],
            messages: vec![Message {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: "fixture".into(),
                    cache: None,
                }],
            }],
            parameters: GenerationParameters {
                response_mode: Some(
                    if self.http.lock().unwrap().content_type == "application/json" {
                        devworks_core::providers::ResponseMode::Aggregate
                    } else {
                        devworks_core::providers::ResponseMode::Streaming
                    },
                ),
                ..Default::default()
            },
        };
        self.request(InferenceInput::Generation {
            session_id: id("session"),
            context: self.bytes(&serde_json::to_vec(&context).unwrap()),
            context_epoch: Counter::new(0).unwrap(),
            tools,
            response_headroom_tokens: Revision::new(10).unwrap(),
            requires_images: false,
            requires_parallel_tools: false,
        })
    }
    fn request(&self, input: InferenceInput) -> InferenceRequest {
        let embedding = matches!(input, InferenceInput::Embedding { .. });
        InferenceRequest {
            options: InferenceOptions {
                selection: ModelSelection::Model {
                    model_id: self.model.clone(),
                },
                effort: None,
                output_schema: None,
                deadline_ms: 2000,
            },
            authority_revision: Revision::new(1).unwrap(),
            control_generation: Revision::new(1).unwrap(),
            instruction_revision: Revision::new(1).unwrap(),
            cancellation_revision: Revision::new(1).unwrap(),
            protocol: Protocol::Draft1,
            request_id: id("request"),
            purpose: if embedding {
                InferencePurpose::Embedding
            } else {
                InferencePurpose::Conversation
            },
            attribution: Attribution::Workspace {
                session_id: id("session"),
            },
            input,
        }
    }
    async fn produce(
        &self,
        request: InferenceRequest,
    ) -> Result<devworks_engine::providers::driver::ProviderInvocation, StorageError> {
        self.produce_priced(
            request,
            PricingContext {
                input_semantics: InputSemantics::Unknown,
                cache_read_possible: false,
                cache_write_possible: false,
                cache_write_ttl_ms: None,
            },
        )
        .await
    }
    async fn produce_priced(
        &self,
        request: InferenceRequest,
        pricing: PricingContext,
    ) -> Result<devworks_engine::providers::driver::ProviderInvocation, StorageError> {
        self.produce_priced_modalities(request, pricing, vec!["Text".into()])
            .await
    }
    async fn produce_priced_modalities(
        &self,
        request: InferenceRequest,
        pricing: PricingContext,
        modalities: Vec<String>,
    ) -> Result<devworks_engine::providers::driver::ProviderInvocation, StorageError> {
        self.produce_dimensions(request, pricing, modalities, None, None, false)
            .await
    }
    async fn produce_dimensions(
        &self,
        request: InferenceRequest,
        pricing: PricingContext,
        modalities: Vec<String>,
        service_tier: Option<String>,
        processing_region: Option<String>,
        embedding_session: bool,
    ) -> Result<devworks_engine::providers::driver::ProviderInvocation, StorageError> {
        self.produce_dimensions_cancellable(
            request,
            pricing,
            modalities,
            service_tier,
            processing_region,
            (embedding_session, None),
        )
        .await
    }
    async fn produce_dimensions_cancellable(
        &self,
        request: InferenceRequest,
        pricing: PricingContext,
        modalities: Vec<String>,
        service_tier: Option<String>,
        processing_region: Option<String>,
        auxiliary: (bool, Option<Cancellation>),
    ) -> Result<devworks_engine::providers::driver::ProviderInvocation, StorageError> {
        let (embedding_session, cancellation) = auxiliary;
        self.driver
            .produce_cancellable(
                self.native_invocation(
                    request,
                    pricing,
                    modalities,
                    service_tier,
                    processing_region,
                    embedding_session,
                ),
                cancellation,
            )
            .await
    }
    fn native_invocation(
        &self,
        request: InferenceRequest,
        pricing: PricingContext,
        modalities: Vec<String>,
        service_tier: Option<String>,
        processing_region: Option<String>,
        embedding_session: bool,
    ) -> devworks_engine::providers::driver::NativeInvocation {
        let embedding = matches!(request.input, InferenceInput::Embedding { .. });
        let (selection, query) = if embedding {
            self.driver.embedding_selection(&id("space")).unwrap()
        } else {
            (
                AttemptSelection::Conversation {
                    intent: devworks_core::protocol::records::RouteIntent::Model {
                        model_id: self.model.clone(),
                    },
                    effort: devworks_core::protocol::records::EffortName::Default,
                    required_capabilities: vec![],
                    pin: None,
                },
                PriceQuery {
                    catalog_model_id: self.model.clone(),
                    provider_model_id: Some("fixture-model".into()),
                    connection_id: id("connection"),
                    endpoint_profile: self.profile.clone(),
                    access_mode: "Api".into(),
                    service_tier,
                    input_tokens: Some(10),
                    cache_write_ttl_ms: None,
                    processing_region,
                    required_modalities: modalities,
                },
            )
        };
        let quote = read(self.store.as_ref(), |s| {
            devworks_core::services::prices::CatalogPriceResolver.quote(s, &query)
        });
        let expected_epoch = match &request.input {
            InferenceInput::Generation { context_epoch, .. } => Some(context_epoch.get()),
            InferenceInput::Embedding { .. } if embedding_session => Some(
                read(self.store.as_ref(), |s| {
                    s.sessions().session(&id("session"))
                })
                .unwrap()
                .context_epoch,
            ),
            InferenceInput::Embedding { .. } => None,
        };
        let attempt = AttemptRequest {
            admission_key: id("admission"),
            request_id: request.request_id.clone(),
            actor_id: self.context.principal().clone(),
            work_id: None,
            job_run_id: None,
            role: "Host".into(),
            query,
            quote,
            price_generation: read(self.store.as_ref(), |s| s.usage().price_revision()),
            override_revision: 1,
            pricing,
            selection,
            input_tokens: 10,
            output_headroom: 10,
            unit_headroom: vec![],
            expected_context_epoch: expected_epoch,
            dispatch: None,
            cache_diagnostics: vec![],
        };
        let logical = RequestAdmission {
            request_id: request.request_id.clone(),
            purpose: if embedding {
                Purpose::Embedding
            } else if matches!(request.purpose, InferencePurpose::Compaction) {
                Purpose::Compaction
            } else {
                Purpose::Conversation
            },
            session_id: if embedding && !embedding_session {
                None
            } else {
                Some(id("session"))
            },
            project_id: None,
            root_id: None,
            model_id: self.model.clone(),
            connection_id: id("connection"),
            pin_id: None,
            args_digest: "a".repeat(64),
        };
        let source_fences = self.source_fences.lock().unwrap().clone();
        let instruction_targets = self.instruction_targets.lock().unwrap().clone();
        devworks_engine::providers::driver::NativeInvocation {
            source_fences,
            instruction_targets,
            context: self.context.clone(),
            logical,
            category: ReportCategory::Assistant,
            attempt,
            request,
        }
    }
    fn transcript(&self) -> Vec<StoredEntry> {
        read(self.store.as_ref(), |s| {
            s.sessions().entries(&id("session"), 0, 500)
        })
    }
}

fn tool_runtime_composition(f: &Fx) -> devworks_engine::Composition {
    let connections = Arc::new(ConnectionService::new(
        f.h.ids.clone(),
        f.h.clock(),
        f.store.clone(),
        Arc::new(NoNetwork),
        Arc::new(SessionSecrets::new()),
        Arc::new(EntropyFixture),
    ));
    let router = Arc::new(InferenceRouter::new(
        f.h.ids.clone(),
        f.h.clock(),
        connections.clone(),
    ));
    let cases = UseCases::new()
        .with_router(router)
        .with_connections(connections)
        .with_sessions(Arc::new(
            devworks_core::services::sessions::SessionService::new(
                f.h.ids.clone(),
                f.store.clone(),
            ),
        ));
    let mut config = devworks_engine::RuntimeConfig::new(id("tool-dispatcher"));
    config.providers = Some(ProviderComposition::new(f.http.clone()));
    config.collect_interval = std::time::Duration::from_secs(3600);
    devworks_engine::Composition {
        store: f.store.clone(),
        clock: f.h.clock(),
        ids: f.h.ids.clone(),
        authority: Arc::new(Authority),
        artifacts: f.artifacts.clone(),
        resources: Arc::new(devworks_core::services::resources::ResourceService::new(
            f.h.ids.clone(),
            f.h.clock(),
        )),
        process_host: Arc::new(devworks_adapters::process::NativeProcessHost),
        runs: devworks_engine::RunComposition::new(Arc::new(ControlService::new(f.h.ids.clone()))),
        power: Arc::new(devworks_adapters::power::NativePower),
        diagnostics: Arc::new(
            devworks_core::services::diagnostics::DiagnosticService::new(
                f.store.clone(),
                f.h.ids.clone(),
                f.h.clock(),
                None,
            ),
        ),
        git: None,
        browser: None,
        packages: None,
        use_cases: cases,
        consumers: vec![],
        indexing: None,
        config,
    }
}

#[test]
fn native_generation_persists_text_usage_and_exact_receipt_replay_without_second_transport() {
    runtime().block_on(async{
    let f=Fx::new("CompatibleChat",Script::sse("data: {\"model\":\"fixture-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n"),false);
    let request=f.generation(vec![]);let outcome=f.produce(request.clone()).await.unwrap();assert_eq!(outcome.result.outcome,Some(AttemptEnd::Completed));assert_eq!(f.transcript().iter().filter(|r|r.kind=="AssistantAttempt").count(),1);assert_eq!(f.transcript()[0].body["visibleText"],"hello");
    let attempt=match outcome.accounting{DriveOutcome::Settled(balance)=>{assert!(balance.local_quiescent);balance.attempt_id},other=>panic!("{other:?}")};
    assert_eq!(outcome.result.attempt_id, Some(AttemptId(attempt.clone())));
    assert_eq!(outcome.result.turn_start_sequence, Some(0));
    assert!(outcome.result.source_context.is_some());
    assert!(outcome.result.instruction_targets.is_none());
    let binding = outcome.result.binding.as_ref().expect("durable provider binding");
    assert_eq!(binding.connection_id, id("connection"));
    assert_eq!(binding.context_epoch.get(), 0);
    let observations=read(f.store.as_ref(),|s|s.usage().usage_observations(&attempt,None,64));assert_eq!(observations.len(),1);
    let replay=f.produce(request).await.unwrap();assert!(matches!(replay.accounting,DriveOutcome::AlreadyDispatched));assert_eq!(replay.result.attempt_id,Some(AttemptId(attempt)));assert_eq!(replay.result.binding.unwrap(),binding.clone());assert_eq!(replay.result.turn_start_sequence,Some(0));assert_eq!(replay.result.source_context.as_ref().unwrap().sha256.as_str(),outcome.result.source_context.as_ref().unwrap().sha256.as_str());assert!(replay.result.instruction_targets.is_none());assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(),1);assert!(f.http.lock().unwrap().closed.load(Ordering::SeqCst));
});
}

#[test]
fn fresh_native_request_uses_published_context_head_before_transport() {
    runtime().block_on(async {
        let wire = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let f = Fx::new("CompatibleChat", Script::sse(wire), false);
        let mut request = f.generation(vec![]);
        let source = match &request.input {
            InferenceInput::Generation { context, .. } => context,
            _ => unreachable!(),
        };
        let bytes = f
            .artifacts
            .read_exact(f.store.as_ref(), &f.context, source, 1024 * 1024)
            .unwrap()
            .0;
        let prompt: ProviderContext = serde_json::from_slice(&bytes).unwrap();
        let binding = NativeBinding {
            connection_id: id("connection"),
            account_revision: Revision::new(1).unwrap(),
            endpoint_profile: EndpointProfile::CompatibleChat,
            model_id: f.model.clone(),
            context_epoch: Counter::new(0).unwrap(),
        };
        let app = devworks_engine::boot(tool_runtime_composition(&f))
            .await
            .unwrap();
        let first = app
            .publish_initial_native_context(&f.context, &id("session"), &binding, &prompt)
            .unwrap();
        let invoke = |request| {
            f.native_invocation(
                request,
                PricingContext {
                    input_semantics: InputSemantics::Unknown,
                    cache_read_possible: false,
                    cache_write_possible: false,
                    cache_write_ttl_ms: None,
                },
                vec!["Text".into()],
                None,
                None,
                false,
            )
        };
        assert!(app
            .produce_and_dispatch_native_turn(invoke(request.clone()), None)
            .await
            .is_err());
        assert!(f.http.lock().unwrap().requests.lock().unwrap().is_empty());
        if let InferenceInput::Generation { context, .. } = &mut request.input {
            *context = first;
        }
        let progress = app
            .produce_and_dispatch_native_turn(invoke(request), None)
            .await
            .unwrap();
        assert_eq!(progress.provider.result.outcome, Some(AttemptEnd::Completed));
        assert!(progress.continuation.is_some());
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
        app.shutdown().await;
    });
}

#[test]
fn initial_native_context_is_durable_idempotent_and_bound_to_first_prompt() {
    runtime().block_on(async {
        let f = Fx::new("CompatibleChat", Script::sse("data: [DONE]\n\n"), false);
        let request = f.generation(vec![]);
        let source = match &request.input {
            InferenceInput::Generation { context, .. } => context,
            _ => unreachable!(),
        };
        let bytes = f
            .artifacts
            .read_exact(f.store.as_ref(), &f.context, source, 1024 * 1024)
            .unwrap()
            .0;
        let prompt: ProviderContext = serde_json::from_slice(&bytes).unwrap();
        let binding = NativeBinding {
            connection_id: id("connection"),
            account_revision: Revision::new(1).unwrap(),
            endpoint_profile: EndpointProfile::CompatibleChat,
            model_id: f.model.clone(),
            context_epoch: Counter::new(0).unwrap(),
        };
        let app = devworks_engine::boot(tool_runtime_composition(&f))
            .await
            .unwrap();
        assert!(
            app.recover_native_context_head(&f.context, &id("session"))
                .unwrap()
                .is_none()
        );
        let first = app
            .publish_initial_native_context(&f.context, &id("session"), &binding, &prompt)
            .unwrap();
        let retry = app
            .publish_initial_native_context(&f.context, &id("session"), &binding, &prompt)
            .unwrap();
        assert_eq!(retry.version_id, first.version_id);
        let head = read(f.store.as_ref(), |snapshot| {
            snapshot.sessions().provider_context_head(&id("session"))
        })
        .unwrap();
        assert_eq!(head.kind, "ProviderInitialContext");
        assert_eq!(head.revision, 1);
        assert_eq!(head.observed_sequence, 0);
        let (recovered, recovered_binding, cursor) = app
            .recover_native_context_head(&f.context, &id("session"))
            .unwrap()
            .unwrap();
        assert_eq!(recovered.version_id, first.version_id);
        assert_eq!(recovered_binding, binding);
        assert_eq!(cursor, 0);
        let mut different_prompt = prompt.clone();
        different_prompt.messages.push(Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "different request".into(),
                cache: None,
            }],
        });
        assert!(
            app.publish_initial_native_context(
                &f.context,
                &id("session"),
                &binding,
                &different_prompt,
            )
            .is_err()
        );
        app.shutdown().await;
        let restarted = devworks_engine::boot(tool_runtime_composition(&f))
            .await
            .unwrap();
        assert_eq!(
            restarted
                .recover_native_context_head(&f.context, &id("session"))
                .unwrap()
                .unwrap()
                .0
                .version_id,
            first.version_id,
        );
        assert!(f.http.lock().unwrap().requests.lock().unwrap().is_empty());
        restarted.shutdown().await;
    });
}

#[test]
fn empty_completed_turn_keeps_a_cursor_and_replays_without_transport() {
    runtime().block_on(async {
        let wire = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let f = Fx::new("CompatibleChat", Script::sse(wire), false);
        let request = f.generation(vec![]);
        let previous = match &request.input {
            InferenceInput::Generation { context, .. } => context.clone(),
            _ => unreachable!(),
        };
        let produced = f.produce(request.clone()).await.unwrap();
        assert_eq!(produced.result.outcome, Some(AttemptEnd::Completed));
        assert_eq!(produced.result.turn_start_sequence, Some(0));
        assert!(f.transcript().is_empty());
        let app = devworks_engine::boot(tool_runtime_composition(&f)).await.unwrap();
        let invoke = |request| f.native_invocation(
            request,
            PricingContext {
                input_semantics: InputSemantics::Unknown,
                cache_read_possible: false,
                cache_write_possible: false,
                cache_write_ttl_ms: None,
            },
            vec!["Text".into()],
            None,
            None,
            false,
        );
        let first = app.produce_and_dispatch_native_turn(invoke(request.clone()), None).await.unwrap();
        assert!(matches!(first.provider.accounting, DriveOutcome::AlreadyDispatched));
        let continuation = first.continuation.unwrap();
        let head = read(f.store.as_ref(), |snapshot| {
            snapshot.sessions().provider_context_head(&id("session"))
        }).unwrap();
        assert_eq!(head.context_epoch, 0);
        assert_eq!(head.observed_sequence, 0);
        assert_eq!(head.attempt_id, Some(produced.result.attempt_id.as_ref().unwrap().0.clone()));
        assert_eq!(head.artifact, json!(continuation));
        assert_eq!(head.revision, 1);
        let old = f.artifacts.read_exact(f.store.as_ref(), &f.context, &previous, 1024 * 1024).unwrap().0;
        let next = f.artifacts.read_exact(f.store.as_ref(), &f.context, &continuation, 1024 * 1024).unwrap().0;
        assert_eq!(next, old);
        let replay = app.produce_and_dispatch_native_turn(invoke(request), None).await.unwrap();
        assert_eq!(replay.continuation.unwrap().sha256, continuation.sha256);
        assert_eq!(read(f.store.as_ref(), |snapshot| snapshot.sessions().provider_context_head(&id("session"))).unwrap().revision, 1);
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
        app.shutdown().await;
        let recovered = devworks_engine::boot(tool_runtime_composition(&f)).await.unwrap();
        let (artifact, binding, cursor) = recovered.recover_native_context_head(&f.context, &id("session")).unwrap().unwrap();
        assert_eq!(artifact.sha256, continuation.sha256);
        assert_eq!(binding.context_epoch.get(), 0);
        assert_eq!(cursor, 0);
        recovered.shutdown().await;
    });
}

#[test]
fn provider_epoch_publishes_responses_context_without_old_cache_hints() {
    runtime().block_on(async {
        let f = Fx::new("AnthropicMessages", Script::sse("data: [DONE]\n\n"), false);
        let request = f.generation(vec![]);
        let reference = match &request.input {
            InferenceInput::Generation { context, .. } => context,
            _ => unreachable!(),
        };
        let mut context: ProviderContext = serde_json::from_slice(
            &f.artifacts
                .read_exact(f.store.as_ref(), &f.context, reference, 1024 * 1024)
                .unwrap()
                .0,
        )
        .unwrap();
        context.instructions[0].cache =
            Some(devworks_core::providers::context::CacheBreakpoint { ttl: None });
        if let ContentBlock::Text { cache, .. } = &mut context.messages[0].content[0] {
            *cache = Some(devworks_core::providers::context::CacheBreakpoint { ttl: None });
        }
        let source = f.bytes(&serde_json::to_vec(&context).unwrap());
        let app = devworks_engine::boot(tool_runtime_composition(&f))
            .await
            .unwrap();
        let previous = NativeBinding {
            connection_id: id("connection"),
            account_revision: Revision::new(1).unwrap(),
            endpoint_profile: EndpointProfile::AnthropicMessages,
            model_id: f.model.clone(),
            context_epoch: Counter::new(0).unwrap(),
        };
        let next = NativeBinding {
            connection_id: id("responses-connection"),
            endpoint_profile: EndpointProfile::OpenAiResponses,
            context_epoch: Counter::new(1).unwrap(),
            ..previous.clone()
        };
        let published = app
            .change_provider_epoch(&f.context, &id("session"), &source, &previous, &next)
            .unwrap();
        let converted: ProviderContext = serde_json::from_slice(
            &f.artifacts
                .read_exact(f.store.as_ref(), &f.context, &published, 1024 * 1024)
                .unwrap()
                .0,
        )
        .unwrap();
        assert!(converted.instructions[0].cache.is_none());
        assert!(matches!(
            converted.messages[0].content[0],
            ContentBlock::Text { cache: None, .. }
        ));
        let epoch = read(f.store.as_ref(), |s| s.sessions().epoch(&id("session"), 1)).unwrap();
        assert_eq!(epoch.reason, "ProviderChange");
        app.shutdown().await;
        let restarted = devworks_engine::boot(tool_runtime_composition(&f))
            .await
            .unwrap();
        let (source, binding, cursor) = restarted
            .recover_native_context_head(&f.context, &id("session"))
            .unwrap()
            .unwrap();
        assert_eq!(source.sha256, published.sha256);
        assert_eq!(binding, next);
        assert_eq!(cursor, epoch.sequence);
        restarted.shutdown().await;
    });
}

#[test]
fn unhandled_external_tool_is_refused_before_native_transport() {
    runtime().block_on(async {
        let f = Fx::new("CompatibleChat", Script::sse("data: [DONE]\n\n"), false);
        let mut tool = f
            .driver
            .schemas()
            .builtin(
                "sessions.get",
                devworks_core::registry::Mode::Query,
                devworks_core::registry::SchemaRevision(1),
                "external",
            )
            .unwrap();
        for capability in ["fixture/tool", "devworks/foreign"] {
            tool.capability_id = capability.to_string().try_into().unwrap();
            assert!(matches!(
                f.produce(f.generation(vec![tool.clone()])).await,
                Err(StorageError::StorageUnavailable {
                    cause: Unavailable::UnsupportedRuntime,
                    ..
                })
            ));
        }
        assert!(f.http.lock().unwrap().requests.lock().unwrap().is_empty());
    });
}

#[test]
fn native_generation_refuses_unreserved_response_headroom() {
    runtime().block_on(async {
        let f = Fx::new("CompatibleChat", Script::sse("data: [DONE]\n\n"), false);
        let request = f.generation(vec![]);
        let mut invocation = f.native_invocation(
            request,
            PricingContext {
                input_semantics: InputSemantics::Unknown,
                cache_read_possible: false,
                cache_write_possible: false,
                cache_write_ttl_ms: None,
            },
            vec!["Text".into()],
            None,
            None,
            false,
        );
        invocation.attempt.output_headroom = 9;
        assert!(matches!(
            f.driver.produce_cancellable(invocation, None).await,
            Err(StorageError::ScopeDenied)
        ));
        assert!(f.http.lock().unwrap().requests.lock().unwrap().is_empty());
    });
}
#[test]
fn native_generation_attributes_observed_context_epoch_change_to_usage() {
    runtime().block_on(async {
        let f = Fx::new("CompatibleChat", Script::sse("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n"), false);
        let mut request = f.generation(vec![]);
        let InferenceInput::Generation { context, context_epoch, .. } = &mut request.input else {
            unreachable!()
        };
        let published = context.clone();
        *context_epoch = Counter::new(1).unwrap();
        let ctx = f.context.clone();
        write(f.store.as_ref(), move |tx| {
            use devworks_core::services::sessions::{transcript::{BatchOutcome, SessionBatch, SessionWriter}, turn};
            let session = tx.sessions().session(&id("session"))?.unwrap();
            let batch = turn::batch_of(&id("session"), tx.new_id(IdKind::TranscriptBatch),
                1, session.next_sequence,
                vec![TranscriptBody::ContextEpochChanged {
                    previous_epoch: Counter::new(0).unwrap(),
                    next_epoch: Counter::new(1).unwrap(),
                    reason: ContextChangeReason::InstructionAdoption,
                    context: published,
                }],
                || tx.new_id(IdKind::TranscriptEntry));
            assert!(matches!(SessionWriter::append_in(tx, &ctx, &SessionBatch::entries(batch))?,
                BatchOutcome::Accepted(_)));
            Ok(())
        }).unwrap();
        let result = f.produce(request).await.unwrap();
        let DriveOutcome::Settled(balance) = result.accounting else { panic!("attempt did not settle") };
        let diagnostics = read(f.store.as_ref(), |s| {
            let attempt = s.usage().inference_attempt(&balance.attempt_id)?.unwrap();
            Ok(devworks_core::services::usage::accounting::snapshot(&attempt)?.cache_diagnostics)
        });
        assert_eq!(diagnostics.len(), 3);
        assert_eq!(diagnostics[0].kind.0, "ContextEpochChanged");
        assert_eq!(diagnostics[0].detail.as_ref().unwrap().0, "InstructionAdoption");
        assert!(!diagnostics[0].inferred);
        assert_eq!(diagnostics[1].kind.0, "InstructionRevision");
        assert_eq!(diagnostics[1].source_revision.as_ref().unwrap().get(), 1);
        assert!(!diagnostics[1].inferred);
        assert_eq!(diagnostics[2].kind.0, "RequestedToolAliasDigest");
        assert_eq!(diagnostics[2].detail.as_ref().unwrap().0,
            devworks_core::fingerprint::sha256_hex(b"[]"));
        assert!(!diagnostics[2].inferred);
    });
}
#[test]
fn compaction_result_is_artifact_only_and_does_not_enter_session_transcript() {
    runtime().block_on(async {
        let wire = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Retain the active plan.\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let f = Fx::new("CompatibleChat", Script::sse(wire), false);
        let mut request = f.generation(vec![]);
        request.purpose = InferencePurpose::Compaction;
        let source = match &request.input {
            InferenceInput::Generation { context, .. } => context.clone(),
            _ => unreachable!(),
        };
        let result = f.produce(request.clone()).await.unwrap();
        assert_eq!(result.result.outcome, Some(AttemptEnd::Completed));
        assert!(result.result.turn_start_sequence.is_none());
        let summary = result.result.result_artifact.expect("summary artifact");
        let (bytes, provenance) = f.artifacts.read_exact(f.store.as_ref(), &f.context, &summary,
            1024 * 1024).unwrap();
        assert_eq!(bytes, b"Retain the active plan.");
        assert_eq!(provenance["derived"]["sourceContext"], json!(source));
        assert!(f.transcript().is_empty());
        let replay = f.produce(request).await.unwrap();
        assert!(matches!(replay.accounting, DriveOutcome::AlreadyDispatched));
        assert_eq!(replay.result.result_artifact.unwrap().version_id, summary.version_id);
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
        assert!(f.transcript().is_empty());
        let tool = f.driver.schemas().builtin("sessions.get", devworks_core::registry::Mode::Query,
            devworks_core::registry::SchemaRevision(1), "get_session").unwrap();
        let mut with_tools = f.generation(vec![tool]);
        with_tools.request_id = id("compaction-with-tools");
        with_tools.purpose = InferencePurpose::Compaction;
        assert!(f.produce(with_tools).await.is_err());
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
        assert!(f.transcript().is_empty());
        let mut mislabeled = f.generation(vec![]);
        mislabeled.request_id = id("mislabeled-compaction");
        mislabeled.purpose = InferencePurpose::Compaction;
        let mut invocation = f.native_invocation(mislabeled, PricingContext {
            input_semantics: InputSemantics::Unknown, cache_read_possible: false,
            cache_write_possible: false, cache_write_ttl_ms: None,
        }, vec!["Text".into()], None, None, false);
        invocation.logical.purpose = Purpose::Conversation;
        assert!(f.driver.produce_cancellable(invocation, None).await.is_err());
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
        let app = devworks_engine::boot(tool_runtime_composition(&f)).await.unwrap();
        let mut wrong_lane = f.generation(vec![]);
        wrong_lane.request_id = id("compaction-through-turn");
        wrong_lane.purpose = InferencePurpose::Compaction;
        let invocation = f.native_invocation(wrong_lane, PricingContext {
            input_semantics: InputSemantics::Unknown, cache_read_possible: false,
            cache_write_possible: false, cache_write_ttl_ms: None,
        }, vec!["Text".into()], None, None, false);
        assert!(matches!(app.produce_and_dispatch_native_turn(invocation, None).await,
            Err(StorageError::ScopeDenied)));
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
        assert!(f.transcript().is_empty());
    });
}
#[test]
fn reviewer_agent_context_head_routes_with_its_existing_model_pin() {
    runtime().block_on(async {
        let f = Fx::new("CompatibleChat", Script::aggregate("{}"), false);
        let human = principal(f.store.as_ref(), "route-owner", PrincipalKind::Human);
        let agent = principal(f.store.as_ref(), "routed-agent", PrincipalKind::Agent)
            .with_session(id("agent-session"));
        write(f.store.as_ref(), {
            let model = f.model.clone();
            move |tx| {
                bind_principal(
                    tx,
                    &human,
                    NewPrincipalBinding {
                        actor: ActorId(id("routed-agent")),
                        kind: PrincipalKind::Agent,
                        role: Some(devworks_core::settings::catalog::AgentRole::CodeReviewer),
                        project_id: None,
                        tool_families: None,
                        bound_by: ActorId(id("route-owner")),
                    },
                    None,
                )
                .map_err(|error| StorageError::InvalidRecord(format!("binding: {error:?}")))?;
                tx.sessions().insert_session(&StoredSession {
                    id: id("agent-session"),
                    conversation_id: id("agent-conversation"),
                    agent_id: ActorId(id("routed-agent")),
                    project_id: None,
                    root_id: None,
                    title: "Agent route".into(),
                    context_epoch: 0,
                    revision: 1,
                    created_by: ActorId(id("route-owner")),
                    created_at_ms: T0,
                    closed_at_ms: None,
                    closed_by: None,
                    next_sequence: 1,
                    checkpoint_sequence: 0,
                })?;
                tx.router().put_session_pin(
                    &StoredPin {
                        session_id: id("agent-session"),
                        pin_id: id("agent-pin"),
                        revision: 1,
                        model_id: model,
                        connection_id: id("connection"),
                        upstream_model_id: Some("fixture-model".into()),
                        endpoint_profile: Some("CompatibleChat".into()),
                        identity_origin: "Resolved".into(),
                        intent: serde_json::to_string(
                            &devworks_core::protocol::records::RouteIntent::Model {
                                model_id: id("model"),
                            },
                        )
                        .unwrap(),
                        requested_effort: "Default".into(),
                        effective_effort: "Default".into(),
                        required_capabilities: vec![],
                        replaced_pin_id: None,
                        reason: "Initial".into(),
                        updated_at_ms: T0,
                    },
                    None,
                )?;
                Ok(())
            }
        })
        .unwrap();
        let app = devworks_engine::boot(tool_runtime_composition(&f))
            .await
            .unwrap();
        let route = devworks_engine::agent_turns::route_context_head(
            &app,
            &agent,
            &id("agent-session"),
            0,
            None,
            true,
        )
        .unwrap();
        assert_eq!(route.model_id, f.model);
        assert_eq!(route.pin_id, id("agent-pin"));
        assert_eq!(
            read(f.store.as_ref(), |snapshot| {
                snapshot.router().session_pin(&id("agent-session"))
            })
            .unwrap()
            .required_capabilities,
            ["tools"]
        );
        let replay = devworks_engine::agent_turns::route_context_head(
            &app,
            &agent,
            &id("agent-session"),
            0,
            None,
            true,
        )
        .unwrap();
        assert_eq!(replay.pin_id, route.pin_id);
        app.shutdown().await;
    });
}

#[test]
fn shared_settings_tool_choice_is_rechecked_for_agent_schema_materialization() {
    let f = Fx::new("CompatibleChat", Script::aggregate("{}"), false);
    let human = principal(f.store.as_ref(), "tool-selector", PrincipalKind::Human);
    assert!(f.driver.schemas().selected_builtins(&human).is_err());
    let agent = principal(f.store.as_ref(), "selected-worker", PrincipalKind::Agent);
    let binding = NewPrincipalBinding {
        actor: ActorId(id("selected-worker")),
        kind: PrincipalKind::Agent,
        role: Some(devworks_core::settings::catalog::AgentRole::Worker),
        project_id: None,
        tool_families: None,
        bound_by: ActorId(id("tool-selector")),
    };
    write(f.store.as_ref(), move |tx| {
        bind_principal(tx, &human, binding, None)
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    })
    .unwrap();
    let alias = f
        .driver
        .schemas()
        .builtin(
            "sessions.get",
            devworks_core::registry::Mode::Query,
            devworks_core::registry::SchemaRevision(1),
            "get_session",
        )
        .unwrap();
    assert!(f.driver.schemas().tool(&agent, &alias).is_ok());
    let initial_tools = f.driver.schemas().selected_builtins(&agent).unwrap();
    assert!(initial_tools.iter().any(|tool| {
        tool.capability_id.as_str() == "devworks-query/sessions.get"
            && tool.wire_name == "sessions_get_query"
    }));
    assert!(
        !initial_tools
            .iter()
            .any(|tool| tool.capability_id.as_str() == "devworks-query/agents.history")
    );
    assert_eq!(
        serde_json::to_value(&initial_tools).unwrap(),
        serde_json::to_value(f.driver.schemas().selected_builtins(&agent).unwrap()).unwrap()
    );
    let descriptor = devworks_core::settings::catalog::descriptor("tools.overrides").unwrap();
    let schema = descriptor.schema_ref();
    write(f.store.as_ref(), move |tx| {
        tx.configuration().append_revision(
            &ConfigScope::Workspace,
            "settings",
            1,
            1,
            json!({"values": {"tools.overrides": {"schema":schema,
                "value":{"devworks/sessions.get":false}}}}),
        )?;
        Ok(())
    })
    .unwrap();
    assert!(f.driver.schemas().tool(&agent, &alias).is_err());
    assert!(
        !f.driver
            .schemas()
            .selected_builtins(&agent)
            .unwrap()
            .iter()
            .any(|tool| tool.capability_id.as_str() == "devworks-query/sessions.get")
    );
    let policy = read(f.store.as_ref(), |s| {
        devworks_core::authority::policy::resolve_policy(s.configuration(), None)
    });
    assert!(!policy.tool_selected("devworks/sessions.get", true));
    let visible = read(f.store.as_ref(), |s| {
        let binding = s.authority().principal(agent.principal())?.unwrap();
        let operation = devworks_core::registry::find("sessions.get").unwrap();
        let selected = operation
            .binding(devworks_core::registry::Mode::Query)
            .unwrap();
        Ok(
            devworks_core::authority::policy::registered_tool_visibility(
                &agent,
                Some(&binding),
                &policy,
                operation,
                selected,
                selected.effect,
            ),
        )
    });
    assert!(matches!(
        visible,
        devworks_core::authority::Verdict::Deny(
            devworks_core::authority::DenialReason::ToolNotSelected
        )
    ));
    for name in ["agents.history", "agents.events"] {
        let hidden = read(f.store.as_ref(), |s| {
            let binding = s.authority().principal(agent.principal())?.unwrap();
            let operation = devworks_core::registry::find(name).unwrap();
            let selected = operation
                .binding(devworks_core::registry::Mode::Query)
                .unwrap();
            Ok(
                devworks_core::authority::policy::registered_tool_visibility(
                    &agent,
                    Some(&binding),
                    &policy,
                    operation,
                    selected,
                    selected.effect,
                ),
            )
        });
        assert!(matches!(
            hidden,
            devworks_core::authority::Verdict::Deny(
                devworks_core::authority::DenialReason::HumanOnly
            )
        ));
    }
    let receive_visibility = read(f.store.as_ref(), |s| {
        let binding = s.authority().principal(agent.principal())?.unwrap();
        let operation = devworks_core::registry::find("agents.receive").unwrap();
        let selected = operation
            .binding(devworks_core::registry::Mode::Command)
            .unwrap();
        Ok(
            devworks_core::authority::policy::registered_tool_visibility(
                &agent,
                Some(&binding),
                &policy,
                operation,
                selected,
                selected.effect,
            ),
        )
    });
    assert!(matches!(
        receive_visibility,
        devworks_core::authority::Verdict::Allow
    ));
    write(f.store.as_ref(), |tx| {
        let mut session = tx.sessions().session(&id("session"))?.unwrap();
        session.id = id("agent-session");
        session.conversation_id = id("agent-conversation");
        session.agent_id = ActorId(id("selected-worker"));
        tx.sessions().insert_session(&session)
    })
    .unwrap();
    let mut request = f.generation(vec![]);
    if let InferenceInput::Generation { session_id, .. } = &mut request.input {
        *session_id = id("agent-session");
    }
    let mut invocation = f.native_invocation(
        request,
        PricingContext {
            input_semantics: InputSemantics::Unknown,
            cache_read_possible: false,
            cache_write_possible: false,
            cache_write_ttl_ms: None,
        },
        vec!["Text".into()],
        None,
        None,
        false,
    );
    invocation.context = f.context.clone().with_session(id("agent-session"));
    assert!(
        runtime()
            .block_on(f.driver.produce_cancellable(invocation, None))
            .is_err()
    );
    assert!(f.http.lock().unwrap().requests.lock().unwrap().is_empty());
    let selected_agent = agent.with_session(id("agent-session"));
    let query = json!({"mode":"Query", "protocol":devworks_core::protocol::CONTRACT,
        "queryId":"selected-tool-check", "body":{"kind":"sessions.get",
            "sessionId":"agent-session"}});
    let authority = devworks_core::authority::HostAuthority {
        store: f.store.as_ref(),
        clock: f.h.clock.as_ref(),
        protected_branches: &[],
    };
    let gateway = devworks_core::gateway::Gateway {
        authority: &authority,
        store: f.store.as_ref(),
        ids: f.h.ids.as_ref(),
    };
    let selected_outcome = UseCases::new()
        .with_sessions(Arc::new(
            devworks_core::services::sessions::SessionService::new(
                f.h.ids.clone(),
                f.store.clone(),
            ),
        ))
        .query(
            &gateway,
            &selected_agent,
            "sessions.get",
            query.to_string().as_bytes(),
        );
    assert!(
        matches!(
            &selected_outcome,
            Ok(devworks_core::protocol::results::DomainQueryReply::Rejected { fault, .. })
                if fault.code == devworks_core::protocol::DomainErrorCode::PermissionDenied
        ),
        "{selected_outcome:?}"
    );
    let app = runtime()
        .block_on(devworks_engine::boot(tool_runtime_composition(&f)))
        .unwrap();
    let foreign = f.context.clone().with_session(id("agent-session"));
    assert!(
        app.dispatch_pending_tool_calls(&foreign, &id("agent-session"))
            .is_err()
    );
    runtime().block_on(app.shutdown());
}
#[test]
fn native_replay_keeps_tool_definitions_stable_across_discovery_order() {
    runtime().block_on(async {
        let wire = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let f = Fx::new("CompatibleChat", Script::sse(wire), false);
        let get = f.driver.schemas().builtin(
            "sessions.get", devworks_core::registry::Mode::Query,
            devworks_core::registry::SchemaRevision(1), "get_session",
        ).unwrap();
        let list = f.driver.schemas().builtin(
            "sessions.list", devworks_core::registry::Mode::Query,
            devworks_core::registry::SchemaRevision(1), "list_sessions",
        ).unwrap();
        let first = f.generation(vec![list, get]);
        let mut reversed = first.clone();
        if let InferenceInput::Generation { tools, .. } = &mut reversed.input {
            tools.reverse();
        }
        let first_outcome = f.produce(first).await.unwrap();
        let DriveOutcome::Settled(balance) = first_outcome.accounting else {
            panic!("first request did not settle");
        };
        let diagnostics = read(f.store.as_ref(), |s| {
            let attempt = s.usage().inference_attempt(&balance.attempt_id)?.unwrap();
            Ok(devworks_core::services::usage::accounting::snapshot(&attempt)?.cache_diagnostics)
        });
        assert_eq!(diagnostics[0].kind.0, "InstructionRevision");
        assert_eq!(diagnostics[1].kind.0, "RequestedToolAliasDigest");
        assert_eq!(diagnostics[1].detail.as_ref().unwrap().0.len(), 64);
        assert!(matches!(f.produce(reversed).await.unwrap().accounting,
            DriveOutcome::AlreadyDispatched));
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
    });
}
#[test]
fn positive_epoch_accepts_host_continuation_but_rejects_unrelated_copy() {
    runtime().block_on(async {
        let wire = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let f = Fx::new("CompatibleChat", Script::sse(wire), false);
        let mut first = f.generation(vec![]);
        let InferenceInput::Generation { context, context_epoch, .. } = &mut first.input else {
            unreachable!()
        };
        let root = context.clone();
        *context_epoch = Counter::new(1).unwrap();
        let ctx = f.context.clone();
        write(f.store.as_ref(), move |tx| {
            use devworks_core::services::sessions::{transcript::{BatchOutcome, SessionBatch, SessionWriter}, turn};
            let session = tx.sessions().session(&id("session"))?.unwrap();
            let batch = turn::batch_of(&id("session"), tx.new_id(IdKind::TranscriptBatch),
                1, session.next_sequence,
                vec![TranscriptBody::ContextEpochChanged {
                    previous_epoch: Counter::new(0).unwrap(),
                    next_epoch: Counter::new(1).unwrap(),
                    reason: ContextChangeReason::InstructionAdoption,
                    context: root,
                }], || tx.new_id(IdKind::TranscriptEntry));
            assert!(matches!(SessionWriter::append_in(tx, &ctx, &SessionBatch::entries(batch))?,
                BatchOutcome::Accepted(_)));
            Ok(())
        }).unwrap();
        let app = devworks_engine::boot(tool_runtime_composition(&f)).await.unwrap();
        let invoke = |request| f.native_invocation(request, PricingContext {
            input_semantics: InputSemantics::Unknown, cache_read_possible: false,
            cache_write_possible: false, cache_write_ttl_ms: None,
        }, vec!["Text".into()], None, None, false);
        let first_result = app.produce_and_dispatch_native_turn(invoke(first), None).await.unwrap();
        let descendant = first_result.continuation.expect("host continuation");
        let bytes = f.artifacts.read_exact(f.store.as_ref(), &f.context, &descendant,
            1024 * 1024).unwrap().0;

        let mut unrelated = f.generation(vec![]);
        unrelated.request_id = id("unrelated-epoch-context");
        if let InferenceInput::Generation { context, context_epoch, .. } = &mut unrelated.input {
            *context = f.bytes(&bytes);
            *context_epoch = Counter::new(1).unwrap();
        }
        assert!(app.produce_and_dispatch_native_turn(invoke(unrelated), None).await.is_err());
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);

        f.http.lock().unwrap().body = VecDeque::from([wire.as_bytes().to_vec()]);
        let mut second = f.generation(vec![]);
        second.request_id = id("second-epoch-request");
        if let InferenceInput::Generation { context, context_epoch, .. } = &mut second.input {
            *context = descendant;
            *context_epoch = Counter::new(1).unwrap();
        }
        let second_result = app.produce_and_dispatch_native_turn(invoke(second), None).await.unwrap();
        assert_eq!(second_result.provider.result.outcome, Some(AttemptEnd::Completed));
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 2);
        app.shutdown().await;
    });
}
#[test]
fn embedding_actual_discovery_fixed_dimensions_durable_vectors_and_no_assistant_turn() {
    runtime().block_on(async{
    let f=Fx::new("Embedding",Script::aggregate(r#"{"model":"fixture-model","data":[{"index":1,"embedding":[0.3,0.4]},{"index":0,"embedding":[0.1,0.2]}],"usage":{"prompt_tokens":2,"total_tokens":2}}"#),true);
    let request=f.request(InferenceInput::Embedding{embedding_space_id:id("space"),dimensions:2,inputs:vec![f.bytes(b"one"),f.bytes(b"two")]});let out=f.produce(request).await.unwrap();assert_eq!(out.result.vector_artifacts.len(),2);assert!(out.result.result_artifact.is_some());assert!(f.transcript().is_empty());
    let bytes=f.artifacts.read_exact(f.store.as_ref(),&f.context,&out.result.vector_artifacts[0],1024).unwrap().0;assert_eq!(bytes,[0.1_f32.to_le_bytes(),0.2_f32.to_le_bytes()].concat());
    assert!(serde_json::from_slice::<Value>(f.http.lock().unwrap().requests.lock().unwrap()[0].body.as_ref().unwrap()).unwrap().get("dimensions").is_none());
});
}
#[test]
fn malformed_embedding_population_and_incomplete_tools_never_publish_results() {
    runtime().block_on(async {
        for body in [
            r#"{"data":[{"index":0,"embedding":[0.1,0.2]}]}"#,
            r#"{"data":[{"index":0,"embedding":[0.1,0.2]},{"index":0,"embedding":[0.3,0.4]}]}"#,
            r#"{"data":[{"index":0,"embedding":[0.1]},{"index":1,"embedding":[0.3]}]}"#,
        ] {
            let f = Fx::new("Embedding", Script::aggregate(body), true);
            let request = f.request(InferenceInput::Embedding {
                embedding_space_id: id("space"),
                dimensions: 2,
                inputs: vec![f.bytes(b"one"), f.bytes(b"two")],
            });
            let outcome = f.produce(request).await;
            assert!(
                (outcome.is_err()
                    || outcome
                        .as_ref()
                        .is_ok_and(|o| o.result.vector_artifacts.is_empty()))
            );
            assert!(f.transcript().is_empty());
        }
    });
}

#[test]
fn successful_native_end_closes_without_waiting_for_http_eof_and_keeps_tool_arguments_exact() {
    runtime().block_on(async{
    let f=Fx::new("CompatibleChat",Script::sse("data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_actual\",\"type\":\"function\",\"function\":{\"name\":\"list_sessions\",\"arguments\":\"{ \\\"limit\\\" : 1 }\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4}}\n\ndata: [DONE]\n\n"),false);
    let tool=f.driver.schemas().builtin("sessions.list",devworks_core::registry::Mode::Query,devworks_core::registry::SchemaRevision(1),"list_sessions").unwrap();
    let outcome=f.produce(f.generation(vec![tool])).await.unwrap();assert_eq!(outcome.result.outcome,Some(AttemptEnd::ToolCallsReady));
    assert_eq!(f.http.lock().unwrap().reads.load(Ordering::SeqCst),1,"native end must not wait for HTTP EOF");
    let rows=f.transcript();let recorded=rows.iter().find(|r|r.kind=="ToolCallRecorded").unwrap();assert_eq!(recorded.body["toolCall"]["call"]["nativeCallId"],"call_actual");
    let arguments:ArtifactRef=serde_json::from_value(recorded.body["toolCall"]["arguments"].clone()).unwrap();
    assert_eq!(f.artifacts.read_exact(f.store.as_ref(),&f.context,&arguments,1024).unwrap().0,b"{ \"limit\" : 1 }");
});
}

#[test]
fn native_tool_broker_keeps_independent_results_and_recovery_has_no_second_response() {
    runtime().block_on(async {
        let event = json!({"choices":[{"index":0,"delta":{"content":"Checking the requested sessions.","tool_calls":[
            {"index":0,"id":"bad","type":"function","function":{"name":"get_session","arguments":"{\"unexpected\":true}"}},
            {"index":1,"id":"good","type":"function","function":{"name":"get_session","arguments":"{\"sessionId\":\"session\"}"}},
            {"index":2,"id":"missing","type":"function","function":{"name":"get_session","arguments":"{\"sessionId\":\"absent\"}"}}
        ]},"finish_reason":null}]});
        let wire = format!("data: {event}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n");
        let f = Fx::new("CompatibleChat", Script::sse(&wire), false);
        let tool = f.driver.schemas().builtin("sessions.get", devworks_core::registry::Mode::Query,
            devworks_core::registry::SchemaRevision(1), "get_session").unwrap();
        let request = f.generation(vec![tool]);
        let previous = match &request.input {
            InferenceInput::Generation { context, .. } => context.clone(),
            _ => unreachable!(),
        };
        let produced = f.produce(request.clone()).await.unwrap();
        assert_eq!(produced.result.outcome, Some(AttemptEnd::ToolCallsReady));
        let mut skipped = f.generation(vec![]);
        skipped.request_id = id("new-request-before-tool-results");
        let held = f.produce(skipped).await;
        assert!(matches!(held, Err(ref error) if format!("{error:?}").contains("unanswered tool calls")));
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
        let attempt = match produced.accounting {
            DriveOutcome::Settled(balance) => AttemptId(balance.attempt_id),
            other => panic!("{other:?}"),
        };

        let app = devworks_engine::boot(tool_runtime_composition(&f)).await.unwrap();
        let binding = NativeBinding {
            connection_id: id("connection"), account_revision: Revision::new(1).unwrap(),
            endpoint_profile: EndpointProfile::CompatibleChat, model_id: f.model.clone(),
            context_epoch: Counter::new(0).unwrap(),
        };
        let next_binding = NativeBinding { connection_id: id("next-connection"),
            endpoint_profile: EndpointProfile::AnthropicMessages,
            context_epoch: Counter::new(1).unwrap(), ..binding.clone() };
        assert!(app.resolved_tool_turn(&f.context, &id("session"), &attempt, 0).is_err());
        assert!(app.change_provider_epoch(&f.context, &id("session"), &previous,
            &binding, &next_binding).is_err());
        let revision = read(f.store.as_ref(), |s| Ok(s.sessions().session(&id("session"))?.unwrap().revision));
        let close = json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":1,
            "requestKey":"close-pending-tool-turn","body":{"kind":"sessions.end",
            "sessionId":"session","expectedRevision":revision.to_string()}});
        let close_reply = app.command(&f.context, "sessions.end", &serde_json::to_vec(&close).unwrap()).unwrap();
        assert!(matches!(close_reply, devworks_core::protocol::CommandReply::Recorded { receipt, .. }
            if matches!(receipt.decision, devworks_core::protocol::results::DomainDecision::Refused { ref fault }
                if fault.target_kind.as_ref().is_some_and(|kind| kind.0 == "unresolved_tool_calls"))));
        let foreign = f.context.clone().with_session(id("other-session"));
        assert!(matches!(app.dispatch_pending_tool_calls(&foreign, &id("session")),
            Err(StorageError::ScopeDenied)));
        let progress = app.produce_and_dispatch_native_turn(
            f.native_invocation(request.clone(), PricingContext {
                input_semantics: InputSemantics::Unknown,
                cache_read_possible: false,
                cache_write_possible: false,
                cache_write_ttl_ms: None,
            }, vec!["Text".into()], None, None, false),
            None,
        ).await.unwrap();
        assert!(matches!(progress.provider.accounting, DriveOutcome::AlreadyDispatched));
        let continuation_ref = progress.continuation.expect("published native tool continuation");
        let published: ProviderContext = serde_json::from_slice(
            &f.artifacts.read_exact(f.store.as_ref(), &f.context, &continuation_ref,
                1024 * 1024).unwrap().0).unwrap();
        assert_eq!(published.messages.len(), 3);
        let replay = app.produce_and_dispatch_native_turn(
            f.native_invocation(request, PricingContext {
                input_semantics: InputSemantics::Unknown,
                cache_read_possible: false,
                cache_write_possible: false,
                cache_write_ttl_ms: None,
            }, vec!["Text".into()], None, None, false),
            None,
        ).await.unwrap();
        assert!(matches!(replay.provider.accounting, DriveOutcome::AlreadyDispatched));
        assert!(replay.tool_dispatches.is_empty());
        assert_eq!(replay.continuation.unwrap().sha256.as_str(), continuation_ref.sha256.as_str());
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
        let dispatched = progress.tool_dispatches;
        assert_eq!(dispatched.len(), 3);
        assert!(dispatched.iter().all(|d| matches!(d.outcome, Ok(devworks_core::services::sessions::transcript::BatchOutcome::Accepted(_)))));
        let rows = f.transcript();
        let replies: Vec<_> = rows.iter().filter(|e| e.kind == "ToolResponseRecorded").collect();
        assert_eq!(replies.len(), 3);
        let reply = |native: &str| replies.iter().find(|e| e.body["call"]["nativeCallId"] == native).unwrap();
        assert_eq!(reply("bad").body["outcome"]["kind"], "Failed");
        assert_eq!(reply("good").body["outcome"]["kind"], "Completed");
        assert_eq!(reply("missing").body["outcome"]["kind"], "Failed");
        assert_eq!(reply("missing").body["outcome"]["code"], "domain_NotFound");
        let ordered = app.resolved_tool_turn(&f.context, &id("session"), &attempt, 0).unwrap();
        assert_eq!(ordered.iter().map(|r| r.call.call.native_call_id.as_str()).collect::<Vec<_>>(),
            vec!["bad", "good", "missing"]);
        let mut continuation = app.tool_continuation_context(
            &f.context, &id("session"), &attempt, 0, &binding, &previous).unwrap();
        assert_eq!(continuation.messages.len(), 3);
        assert!(matches!(&continuation.messages[1].content[0], ContentBlock::Text { text, .. }
            if text == "Checking the requested sessions."));
        let assistant_calls: Vec<_> = continuation.messages[1].content.iter().filter_map(|block| {
            if let ContentBlock::ToolCall { call_id, .. } = block { Some(call_id.as_str()) } else { None }
        }).collect();
        assert_eq!(assistant_calls, vec!["bad", "good", "missing"]);
        let result_calls: Vec<_> = continuation.messages[2].content.iter().filter_map(|block| {
            if let ContentBlock::ToolResult { call_id, .. } = block { Some(call_id.as_str()) } else { None }
        }).collect();
        assert_eq!(result_calls, assistant_calls);
        let result_errors: Vec<_> = continuation.messages[2].content.iter().filter_map(|block| {
            if let ContentBlock::ToolResult { is_error, .. } = block { Some(*is_error) } else { None }
        }).collect();
        assert_eq!(result_errors, vec![true, false, true]);
        let mut foreign_context: ProviderContext = serde_json::from_slice(
            &f.artifacts.read_exact(f.store.as_ref(), &f.context, &previous, 1024 * 1024).unwrap().0
        ).unwrap();
        foreign_context.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Native {
                binding: NativeBinding { model_id: id("foreign-model"), ..binding.clone() },
                artifact: previous.clone(),
            }],
        });
        let foreign = f.bytes(&serde_json::to_vec(&foreign_context).unwrap());
        assert!(app.tool_continuation_context(
            &f.context, &id("session"), &attempt, 0, &binding, &foreign).is_err());
        continuation.messages.push(Message { role: MessageRole::Assistant,
            content: vec![ContentBlock::Native { binding: binding.clone(), artifact: previous.clone() }] });
        let source = f.bytes(&serde_json::to_vec(&continuation).unwrap());
        let converted_ref = app.change_provider_epoch(&f.context, &id("session"), &source,
            &binding, &next_binding).unwrap();
        let converted_bytes = f.artifacts.read_exact(f.store.as_ref(), &f.context,
            &converted_ref, 1024 * 1024).unwrap().0;
        let converted: ProviderContext = serde_json::from_slice(&converted_bytes).unwrap();
        assert_eq!(converted.messages.len(), 3);
        assert_eq!(f.transcript().iter().filter(|entry| entry.kind == "ContextEpochChanged").count(), 1);
        let epoch = read(f.store.as_ref(), |s| s.sessions().epoch(&id("session"), 1)).unwrap();
        assert_eq!(epoch.reason, "ProviderChange");
        let sources = read(f.store.as_ref(), |s| s.sessions().context_sources(&id("session"), 1, 0, 10));
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].kind, "provider_context");
        assert_eq!(sources[0].disposition, "Included");
        assert_eq!(sources[1].kind, "provider_native");
        assert_eq!(sources[1].disposition, "Omitted");
        let reference: ArtifactRef = serde_json::from_value(reply("good").body["outcome"]["artifacts"][0].clone()).unwrap();
        let bytes = f.artifacts.read_exact(f.store.as_ref(), &f.context, &reference, 1024 * 1024).unwrap().0;
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["kind"], "Result");
        let mut accepted_call: CompleteToolCall = serde_json::from_value(
            rows.iter().find(|entry| entry.kind == "ToolCallRecorded").unwrap().body["toolCall"].clone()
        ).unwrap();
        accepted_call.call.native_call_id = "accepted-operation-call".into();
        accepted_call.call.ordinal = Counter::new(3).unwrap();
        let accepted_key = accepted_call.call.clone();
        let operation_id = devworks_core::protocol::OperationId(id("accepted-operation"));
        let operation_for_write = operation_id.clone();
        let context_for_write = f.context.clone();
        write(f.store.as_ref(), move |tx| {
            use devworks_core::ports::IdKind;
            use devworks_core::services::sessions::{transcript::{BatchOutcome, SessionBatch, SessionWriter}, turn};
            tx.operations().record_intent(NewOperation {
                id: operation_for_write.clone(), root_id: None, assignment_id: None,
                capability: "fixture/accepted".into(), schema_version: 1,
                args_digest: "a".repeat(64), payload: json!({}),
            })?;
            let session = tx.sessions().session(&id("session"))?.unwrap();
            let batch = turn::batch_of(&id("session"), tx.new_id(IdKind::TranscriptBatch),
                session.context_epoch, session.next_sequence,
                vec![TranscriptBody::ToolCallRecorded { tool_call: accepted_call },
                    TranscriptBody::ToolResponseRecorded { call: accepted_key,
                        outcome: ToolOutcome::OperationAccepted {
                            operation_id: operation_for_write, artifacts: vec![],
                        },
                    }],
                || tx.new_id(IdKind::TranscriptEntry));
            assert!(matches!(SessionWriter::append_in(tx, &context_for_write,
                &SessionBatch::entries(batch))?, BatchOutcome::Accepted(_)));
            Ok(())
        }).unwrap();
        let initial_response = f.transcript().into_iter().find(|entry|
            entry.kind == "ToolResponseRecorded"
                && entry.body["call"]["nativeCallId"] == "accepted-operation-call"
        ).unwrap().body;
        let close_with = |key: &str| {
            let revision = read(f.store.as_ref(), |s| Ok(s.sessions().session(&id("session"))?.unwrap().revision));
            json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":1,
                "requestKey":key,"body":{"kind":"sessions.end",
                "sessionId":"session","expectedRevision":revision.to_string()}})
        };
        let close = close_with("close-active-tool-operation");
        let reply = app.command(&f.context, "sessions.end", &serde_json::to_vec(&close).unwrap()).unwrap();
        assert!(matches!(reply, devworks_core::protocol::CommandReply::Recorded { receipt, .. }
            if matches!(receipt.decision, devworks_core::protocol::results::DomainDecision::Refused { ref fault }
                if fault.target_kind.as_ref().is_some_and(|kind| kind.0 == "unresolved_tool_operations"))));
        let foreign_session = f.context.clone().with_session(id("other-session"));
        assert!(app.reconcile_tool_operations(&foreign_session, &id("session")).is_err());
        assert!(app.record_tool_operation_update(&foreign_session, &id("session"), &operation_id).is_err());
        let reconciled = app.reconcile_tool_operations(&f.context, &id("session")).unwrap();
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].operation_id, operation_id);
        assert!(matches!(&reconciled[0].outcome, Ok(Some(_))));
        assert!(app.record_tool_operation_update(&f.context, &id("session"), &operation_id).unwrap().is_none());
        assert_eq!(f.transcript().iter().filter(|entry| entry.kind == "OperationUpdate").count(), 1);
        let retained_response = f.transcript().into_iter().find(|entry|
            entry.kind == "ToolResponseRecorded"
                && entry.body["call"]["nativeCallId"] == "accepted-operation-call"
        ).unwrap().body;
        assert_eq!(retained_response, initial_response);
        let append_synthetic_call = |native: &str, ordinal: i64, started: bool| {
            use devworks_core::services::sessions::{transcript::{BatchOutcome, SessionBatch, SessionWriter}, turn};
            let mut call: CompleteToolCall = serde_json::from_value(
                rows.iter().find(|entry| entry.kind == "ToolCallRecorded"
                    && entry.body["toolCall"]["call"]["nativeCallId"] == "good")
                    .unwrap().body["toolCall"].clone()
            ).unwrap();
            call.call.native_call_id = native.into();
            call.call.ordinal = Counter::new(ordinal).unwrap();
            let key = call.call.clone();
            let key_for_write = key.clone();
            let ctx = f.context.clone();
            write(f.store.as_ref(), move |tx| {
                let session = tx.sessions().session(&id("session"))?.unwrap();
                let mut bodies = vec![TranscriptBody::ToolCallRecorded { tool_call: call }];
                if started {
                    bodies.push(TranscriptBody::ToolDispatchStarted { call: key_for_write });
                }
                let batch = turn::batch_of(&id("session"), tx.new_id(IdKind::TranscriptBatch),
                    session.context_epoch, session.next_sequence, bodies,
                    || tx.new_id(IdKind::TranscriptEntry));
                assert!(matches!(SessionWriter::append_in(tx, &ctx, &SessionBatch::entries(batch))?,
                    BatchOutcome::Accepted(_)));
                Ok(())
            }).unwrap();
            key
        };
        let cancelled = append_synthetic_call("cancelled-before-dispatch", 4, false);
        assert!(matches!(app.cancel_tool_call_before_dispatch(&f.context, &id("session"), &cancelled),
            Ok(devworks_core::services::sessions::transcript::BatchOutcome::Accepted(_))));
        assert!(f.transcript().iter().any(|entry| entry.kind == "ToolResponseRecorded"
            && entry.body["call"]["nativeCallId"] == "cancelled-before-dispatch"
            && entry.body["outcome"]["kind"] == "CancelledBeforeDispatch"));
        let started = append_synthetic_call("started-before-cancel", 5, true);
        assert!(app.cancel_tool_call_before_dispatch(&f.context, &id("session"), &started).is_err());
        let recovered = app.dispatch_pending_tool_calls(&f.context, &id("session")).unwrap();
        assert_eq!(recovered.len(), 1);
        assert!(matches!(&recovered[0].outcome,
            Ok(devworks_core::services::sessions::transcript::BatchOutcome::Accepted(_))));
        assert_eq!(f.transcript().iter().filter(|entry| entry.kind == "ToolDispatchStarted"
            && entry.body["call"]["nativeCallId"] == "started-before-cancel").count(), 1);
        app.shutdown().await;
        let restarted = devworks_engine::boot(tool_runtime_composition(&f)).await.unwrap();
        assert!(restarted.dispatch_pending_tool_calls(&f.context, &id("session")).unwrap().is_empty());
        assert_eq!(f.transcript().iter().filter(|e| e.kind == "ToolResponseRecorded").count(), 6);
        assert!(restarted.record_tool_operation_update(&f.context, &id("session"), &operation_id).unwrap().is_none());
        let operation_for_write = operation_id.clone();
        write(f.store.as_ref(), move |tx| {
            use devworks_core::protocol::OperationState;
            let operation = tx.operations().operation(&operation_for_write)?.unwrap();
            tx.operations().transition(&operation_for_write, operation.revision, None,
                OperationState::CancelledBeforeDispatch, None)?;
            Ok(())
        }).unwrap();
        let close = close_with("close-unrecorded-tool-operation");
        let reply = restarted.command(&f.context, "sessions.end", &serde_json::to_vec(&close).unwrap()).unwrap();
        assert!(matches!(reply, devworks_core::protocol::CommandReply::Recorded { receipt, .. }
            if matches!(receipt.decision, devworks_core::protocol::results::DomainDecision::Refused { ref fault }
                if fault.target_kind.as_ref().is_some_and(|kind| kind.0 == "unresolved_tool_operations"))));
        assert!(restarted.record_tool_operation_update(&f.context, &id("session"), &operation_id).unwrap().is_some());
        let close = close_with("close-settled-tool-operation");
        let reply = restarted.command(&f.context, "sessions.end", &serde_json::to_vec(&close).unwrap()).unwrap();
        assert!(matches!(reply, devworks_core::protocol::CommandReply::Recorded { receipt, .. }
            if matches!(receipt.decision, devworks_core::protocol::results::DomainDecision::Applied { .. })));
        restarted.shutdown().await;
    });
}

#[test]
fn fresh_native_tool_turn_publishes_context_and_holds_on_user_redirection() {
    runtime().block_on(async {
        let event = json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"call-one","type":"function","function":{
                "name":"get_session","arguments":"{\"sessionId\":\"session\"}"}}
        ]},"finish_reason":null}]});
        let wire = format!("data: {event}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n");
        let f = Fx::new("CompatibleChat", Script::sse(&wire), false);
        let tool = f.driver.schemas().builtin("sessions.get", devworks_core::registry::Mode::Query,
            devworks_core::registry::SchemaRevision(1), "get_session").unwrap();
        let request = f.generation(vec![tool]);
        let previous = match &request.input {
            InferenceInput::Generation { context, .. } => context.clone(),
            _ => unreachable!(),
        };
        let app = devworks_engine::boot(tool_runtime_composition(&f)).await.unwrap();
        let invoke = |request| f.native_invocation(request, PricingContext {
            input_semantics: InputSemantics::Unknown,
            cache_read_possible: false,
            cache_write_possible: false,
            cache_write_ttl_ms: None,
        }, vec!["Text".into()], None, None, false);
        let progress = app.produce_and_dispatch_native_turn(invoke(request.clone()), None).await.unwrap();
        assert!(matches!(progress.provider.accounting, DriveOutcome::Settled(_)));
        assert_eq!(progress.provider.result.outcome, Some(AttemptEnd::ToolCallsReady));
        assert_eq!(progress.tool_dispatches.len(), 1);
        assert!(progress.tool_dispatches[0].outcome.is_ok());
        let continuation = progress.continuation.expect("published continuation");
        let bytes = f.artifacts.read_exact(f.store.as_ref(), &f.context, &continuation,
            1024 * 1024).unwrap().0;
        let material: ProviderContext = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(material.messages.len(), 3);
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);

        let ctx = f.context.clone();
        write(f.store.as_ref(), move |tx| {
            use devworks_core::services::sessions::{transcript::{BatchOutcome, SessionBatch, SessionWriter}, turn};
            let session = tx.sessions().session(&id("session"))?.unwrap();
            let batch = turn::batch_of(&id("session"), tx.new_id(IdKind::TranscriptBatch),
                session.context_epoch, session.next_sequence,
                vec![TranscriptBody::UserMessage {
                    content: vec![UserContent::Text { text: "Change direction".into() }],
                }],
                || tx.new_id(IdKind::TranscriptEntry));
            assert!(matches!(SessionWriter::append_in(tx, &ctx, &SessionBatch::entries(batch))?,
                BatchOutcome::Accepted(_)));
            Ok(())
        }).unwrap();
        let result = &progress.provider.result;
        assert!(app.tool_continuation_context(&f.context, &id("session"),
            result.attempt_id.as_ref().unwrap(), result.turn_start_sequence.unwrap(),
            result.binding.as_ref().unwrap(), &previous).is_err());
        assert!(app.produce_and_dispatch_native_turn(invoke(request), None).await.is_err());
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
        app.shutdown().await;
    });
}

#[test]
fn completed_native_turn_publishes_replayable_assistant_context() {
    runtime().block_on(async {
        let f = Fx::new("CompatibleChat", Script::sse("data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"retained native state\",\"content\":\"Answer\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"), false);
        let app = devworks_engine::boot(tool_runtime_composition(&f)).await.unwrap();
        let request = f.generation(vec![]);
        let invoke = |request| f.native_invocation(request, PricingContext {
            input_semantics: InputSemantics::Unknown, cache_read_possible: false,
            cache_write_possible: false, cache_write_ttl_ms: None,
        }, vec!["Text".into()], None, None, false);
        let progress = app.produce_and_dispatch_native_turn(invoke(request.clone()), None).await.unwrap();
        assert_eq!(progress.provider.result.outcome, Some(AttemptEnd::Completed));
        assert!(progress.tool_dispatches.is_empty());
        let source = progress.continuation.expect("completed turn continuation");
        let material: ProviderContext = serde_json::from_slice(&f.artifacts.read_exact(
            f.store.as_ref(), &f.context, &source, 1024 * 1024).unwrap().0).unwrap();
        assert_eq!(material.messages.len(), 2);
        assert!(material.messages[1].content.iter().any(|block|
            matches!(block, ContentBlock::Text { text, .. } if text == "Answer")));
        assert!(material.messages[1].content.iter().any(|block|
            matches!(block, ContentBlock::Native { .. })));
        let replay = app.produce_and_dispatch_native_turn(invoke(request.clone()), None).await.unwrap();
        assert!(matches!(replay.provider.accounting, DriveOutcome::AlreadyDispatched));
        assert_eq!(replay.continuation.unwrap().sha256.as_str(), source.sha256.as_str());
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
        f.http.lock().unwrap().body = VecDeque::from([
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Second answer\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_vec(),
        ]);
        let mut next = f.generation(vec![]);
        next.request_id = id("next-completed-request");
        if let InferenceInput::Generation { context, .. } = &mut next.input {
            *context = source.clone();
        }
        let second = app.produce_and_dispatch_native_turn(invoke(next), None).await.unwrap();
        assert_eq!(second.provider.result.outcome, Some(AttemptEnd::Completed));
        let second_context: ProviderContext = serde_json::from_slice(&f.artifacts.read_exact(
            f.store.as_ref(), &f.context, &second.continuation.unwrap(), 1024 * 1024).unwrap().0).unwrap();
        assert_eq!(second_context.messages.len(), 3);
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 2);
        assert!(app.produce_and_dispatch_native_turn(invoke(request), None).await.is_err());
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 2);
        app.shutdown().await;
    });
}

#[test]
fn later_operation_revisions_append_to_native_context_once_per_cursor() {
    runtime().block_on(async {
        use devworks_core::services::sessions::{transcript::{BatchOutcome, SessionBatch, SessionWriter}, turn};
        let event = json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"call-one","type":"function","function":{
                "name":"get_session","arguments":"{\"sessionId\":\"session\"}"}}
        ]},"finish_reason":null}]});
        let wire = format!("data: {event}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n");
        let f = Fx::new("CompatibleChat", Script::sse(&wire), false);
        let tool = f.driver.schemas().builtin("sessions.get", devworks_core::registry::Mode::Query,
            devworks_core::registry::SchemaRevision(1), "get_session").unwrap();
        let request = f.generation(vec![tool]);
        assert_eq!(f.produce(request.clone()).await.unwrap().result.outcome,
            Some(AttemptEnd::ToolCallsReady));
        let mut accepted_call: CompleteToolCall = serde_json::from_value(f.transcript().iter()
            .find(|entry| entry.kind == "ToolCallRecorded").unwrap().body["toolCall"].clone()).unwrap();
        accepted_call.call.native_call_id = "accepted-operation-call".into();
        accepted_call.call.ordinal = Counter::new(1).unwrap();
        let operation_id = devworks_core::protocol::OperationId(id("accepted-operation"));
        let ctx = f.context.clone();
        let operation_for_write = operation_id.clone();
        write(f.store.as_ref(), move |tx| {
            tx.operations().record_intent(NewOperation {
                id: operation_for_write.clone(), root_id: None, assignment_id: None,
                capability: "fixture/accepted".into(), schema_version: 1,
                args_digest: "a".repeat(64), payload: json!({}),
            })?;
            let session = tx.sessions().session(&id("session"))?.unwrap();
            let key = accepted_call.call.clone();
            let batch = turn::batch_of(&id("session"), tx.new_id(IdKind::TranscriptBatch),
                session.context_epoch, session.next_sequence,
                vec![TranscriptBody::ToolCallRecorded { tool_call: accepted_call },
                    TranscriptBody::ToolResponseRecorded { call: key,
                        outcome: ToolOutcome::OperationAccepted {
                            operation_id: operation_for_write, artifacts: vec![],
                        } }],
                || tx.new_id(IdKind::TranscriptEntry));
            assert!(matches!(SessionWriter::append_in(tx, &ctx, &SessionBatch::entries(batch))?,
                BatchOutcome::Accepted(_)));
            Ok(())
        }).unwrap();
        let app = devworks_engine::boot(tool_runtime_composition(&f)).await.unwrap();
        // The operation may advance before the first continuation is published.
        assert!(app.record_tool_operation_update(&f.context, &id("session"), &operation_id)
            .unwrap().is_some());
        let invoke = f.native_invocation(request, PricingContext {
            input_semantics: InputSemantics::Unknown, cache_read_possible: false,
            cache_write_possible: false, cache_write_ttl_ms: None,
        }, vec!["Text".into()], None, None, false);
        let progress = app.produce_and_dispatch_native_turn(invoke, None).await.unwrap();
        assert!(matches!(progress.provider.accounting, DriveOutcome::AlreadyDispatched));
        let source = progress.continuation.unwrap();
        let first = app.refresh_native_tool_context(&f.context, &id("session"), &source).unwrap();
        assert_eq!(first.version_id, source.version_id);
        let material: ProviderContext = serde_json::from_slice(&f.artifacts.read_exact(
            f.store.as_ref(), &f.context, &first, 1024 * 1024).unwrap().0).unwrap();
        assert_eq!(material.messages.len(), 4);
        assert!(matches!(&material.messages[3].content[0], ContentBlock::Text { text, .. }
            if text.contains("Tool operation update:") && text.contains("accepted-operation")));
        let replay = app.refresh_native_tool_context(&f.context, &id("session"), &first).unwrap();
        assert_eq!(replay.version_id, first.version_id);
        let prior_binding = progress.provider.result.binding.unwrap();
        app.shutdown().await;
        let app = devworks_engine::boot(tool_runtime_composition(&f)).await.unwrap();
        let operation_for_write = operation_id.clone();
        write(f.store.as_ref(), move |tx| {
            let operation = tx.operations().operation(&operation_for_write)?.unwrap();
            tx.operations().begin_attempt(&operation_for_write, operation.revision,
                AttemptBinding::default())?;
            Ok(())
        }).unwrap();
        assert!(app.record_tool_operation_update(&f.context, &id("session"), &operation_id)
            .unwrap().is_some());
        let next_binding = NativeBinding {
            connection_id: id("next-connection"),
            endpoint_profile: EndpointProfile::AnthropicMessages,
            context_epoch: Counter::new(1).unwrap(),
            ..prior_binding.clone()
        };
        let changed = app.change_provider_epoch(&f.context, &id("session"), &first,
            &prior_binding, &next_binding).unwrap();
        let changed_context: ProviderContext = serde_json::from_slice(&f.artifacts.read_exact(
            f.store.as_ref(), &f.context, &changed, 1024 * 1024).unwrap().0).unwrap();
        assert_eq!(changed_context.messages.len(), 5);
        assert!(matches!(&changed_context.messages[4].content[0], ContentBlock::Text { text, .. }
            if text.contains("Preflighted")));
        let operation_for_write = operation_id.clone();
        write(f.store.as_ref(), move |tx| {
            use devworks_core::protocol::OperationState;
            let operation = tx.operations().operation(&operation_for_write)?.unwrap();
            let fence = operation.latest_attempt().unwrap().fence();
            tx.operations().transition(&operation_for_write, operation.revision, Some(&fence),
                OperationState::CancelledBeforeDispatch, None)?;
            Ok(())
        }).unwrap();
        let second = app.refresh_native_tool_context(&f.context, &id("session"), &changed).unwrap();
        let head = read(f.store.as_ref(), |snapshot| snapshot.sessions().provider_context_head(&id("session"))).unwrap();
        assert_eq!(head.kind, "ProviderOperationUpdateContext");
        assert_eq!(head.context_epoch, 1);
        assert_eq!(head.artifact, json!(second));
        assert_eq!(app.refresh_native_tool_context(&f.context, &id("session"), &changed).unwrap().version_id, second.version_id);
        let (recovered, recovered_binding, cursor) = app.recover_native_context_head(&f.context, &id("session")).unwrap().unwrap();
        assert_eq!(recovered.version_id, second.version_id);
        assert_eq!(recovered_binding, next_binding);
        assert_eq!(cursor, head.observed_sequence);
        app.shutdown().await;
        let app = devworks_engine::boot(tool_runtime_composition(&f)).await.unwrap();
        assert_eq!(app.recover_native_context_head(&f.context, &id("session")).unwrap().unwrap().0.version_id, second.version_id);
        let material: ProviderContext = serde_json::from_slice(&f.artifacts.read_exact(
            f.store.as_ref(), &f.context, &second, 1024 * 1024).unwrap().0).unwrap();
        assert_eq!(material.messages.len(), 6);
        assert_eq!(f.transcript().iter().filter(|entry| entry.kind == "OperationUpdate").count(), 3);
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
        let ctx = f.context.clone();
        write(f.store.as_ref(), move |tx| {
            let session = tx.sessions().session(&id("session"))?.unwrap();
            let batch = turn::batch_of(&id("session"), tx.new_id(IdKind::TranscriptBatch),
                session.context_epoch, session.next_sequence,
                vec![TranscriptBody::UserMessage {
                    content: vec![UserContent::Text { text: "Redirect".into() }],
                }], || tx.new_id(IdKind::TranscriptEntry));
            assert!(matches!(SessionWriter::append_in(tx, &ctx, &SessionBatch::entries(batch))?,
                BatchOutcome::Accepted(_)));
            Ok(())
        }).unwrap();
        let redirected = app.refresh_native_tool_context(&f.context, &id("session"), &second).unwrap();
        let redirected_context: ProviderContext = serde_json::from_slice(&f.artifacts.read_exact(
            f.store.as_ref(), &f.context, &redirected, 1024 * 1024).unwrap().0).unwrap();
        assert!(matches!(&redirected_context.messages.last().unwrap().content[0],
            ContentBlock::Text { text, .. } if text == "Redirect"));
        assert_eq!(app.refresh_native_tool_context(&f.context, &id("session"), &redirected)
            .unwrap().version_id, redirected.version_id);
        devworks_core::services::sessions::notice::append_screen_context(
            f.store.as_ref(), &f.artifacts, &f.context, &id("session"),
            "overview", "Current screen: Overview",
        ).unwrap();
        let noticed = app.refresh_native_tool_context(&f.context, &id("session"), &redirected).unwrap();
        let noticed_context: ProviderContext = serde_json::from_slice(&f.artifacts.read_exact(
            f.store.as_ref(), &f.context, &noticed, 1024 * 1024).unwrap().0).unwrap();
        assert!(matches!(&noticed_context.messages.last().unwrap().content[0],
            ContentBlock::Text { text, .. } if text == "Current screen: Overview"));
        app.shutdown().await;
    });
}

#[test]
fn cancelled_native_tool_dispatch_records_only_predispatch_responses() {
    runtime().block_on(async {
        use devworks_core::services::sessions::transcript::BatchOutcome;
        let wire = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"cancelled-call\",\"type\":\"function\",\"function\":{\"name\":\"get_session\",\"arguments\":\"{\\\"sessionId\\\":\\\"session\\\"}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n";
        let f = Fx::new("CompatibleChat", Script::sse(wire), false);
        let tool = f.driver.schemas().builtin("sessions.get", devworks_core::registry::Mode::Query,
            devworks_core::registry::SchemaRevision(1), "get_session").unwrap();
        assert_eq!(f.produce(f.generation(vec![tool])).await.unwrap().result.outcome,
            Some(AttemptEnd::ToolCallsReady));
        let app = devworks_engine::boot(tool_runtime_composition(&f)).await.unwrap();
        let token = Cancellation::new();
        token.cancel();
        let dispatches = app.dispatch_pending_tool_calls_cancellable(
            &f.context, &id("session"), Some(&token)).unwrap();
        assert_eq!(dispatches.len(), 1);
        assert!(matches!(&dispatches[0].outcome, Ok(BatchOutcome::Accepted(_))));
        assert_eq!(f.transcript().iter().filter(|entry| entry.kind == "ToolDispatchStarted").count(), 0);
        assert!(f.transcript().iter().any(|entry| entry.kind == "ToolResponseRecorded"
            && entry.body["outcome"]["kind"] == "CancelledBeforeDispatch"));
        assert!(app.dispatch_pending_tool_calls(&f.context, &id("session")).unwrap().is_empty());
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
        app.shutdown().await;
    });
}

#[test]
fn native_provider_holds_changed_repository_guidance_before_transport() {
    runtime().block_on(async {
        use devworks_core::ports::indexing::SourceTree;
        for fault in ["edited", "wrong_target", "missing_target_set"] {
            let mut f = Fx::new("CompatibleChat", Script::sse("data: [DONE]\n\n"), false);
            let root = f.h.root.join("guidance-repo");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("AGENTS.md"), "first guidance").unwrap();
            let root = NativeSourceTree::new().canonical_dir(&root).unwrap();
            let location = root.to_string_lossy().into_owned();
            write(f.store.as_ref(), move |tx| {
                tx.projects().insert_repository(&StoredRepository {
                    id: id("guidance-repo"),
                    location,
                    name: "Guidance".into(),
                    remote_url: None,
                    primary: true,
                    lifecycle: "Active".into(),
                    languages: vec![],
                    revision: 1,
                    registered_at_ms: 1,
                })
            })
            .unwrap();
            let source = Arc::new(NativeSourceTree::new());
            let schemas = Arc::new(RegisteredSchemas::new(
                f.store.clone(),
                UseCases::new(),
                None,
                f.artifacts.clone(),
            ));
            f.driver = NativeProviders::new(
                f.store.clone(),
                f.artifacts.clone(),
                f.connections.clone(),
                f.usage.clone(),
                schemas,
                ProviderComposition::new(f.http.clone())
                    .with_repository_source(f.store.clone(), source.clone()),
            );
            let mut request = f.generation(vec![]);
            if let InferenceInput::Generation { context, .. } = &mut request.input {
                let bytes = f
                    .artifacts
                    .read_exact(f.store.as_ref(), &f.context, context, 1024 * 1024)
                    .unwrap()
                    .0;
                let base: ProviderContext = serde_json::from_slice(&bytes).unwrap();
                let bundle = devworks_core::providers::instructions::resolve(
                    source.as_ref(),
                    &root,
                    id("guidance-repo"),
                    &["README.md".into()],
                )
                .unwrap();
                let bound =
                    devworks_core::providers::instructions::bind_context(base, &[bundle]).unwrap();
                *context = f.bytes(&serde_json::to_vec(&bound).unwrap());
            }
            let targets = if fault == "edited" {
                std::fs::write(root.join("AGENTS.md"), "changed before send").unwrap();
                vec!["README.md".into()]
            } else {
                vec!["src/other.rs".into()]
            };
            if fault != "missing_target_set" {
                *f.instruction_targets.lock().unwrap() = Some(vec![(id("guidance-repo"), targets)]);
            }
            let result = f.produce(request).await;
            if fault == "missing_target_set" {
                assert!(matches!(result, Err(ref error) if format!("{error:?}").contains("target selection")), "{}", result.err().map_or_else(|| "success".into(), |e| format!("{e:?}")));
            } else {
                assert!(result.is_err());
            }
            assert_eq!(f.http.lock().unwrap().reads.load(Ordering::SeqCst), 0);
            assert!(f.transcript().is_empty());
            f.usage.stop();
            f.usage.drain().await;
        }
    });
}

#[test]
fn native_tool_dispatch_rechecks_recorded_guidance_before_first_effect() {
    runtime().block_on(async {
        use devworks_core::ports::indexing::SourceTree;
        use devworks_core::services::indexing::IndexSettings;
        use devworks_engine::{IndexComposition, indexing::IndexingConfig};

        for started in [false, true] {
            let wire = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_actual\",\"type\":\"function\",\"function\":{\"name\":\"get_session\",\"arguments\":\"{\\\"sessionId\\\":\\\"session\\\"}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n";
            let mut f = Fx::new("CompatibleChat", Script::sse(wire), false);
            let root = f.h.root.join("guidance-repo");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("AGENTS.md"), "first guidance").unwrap();
            let root = NativeSourceTree::new().canonical_dir(&root).unwrap();
            let location = root.to_string_lossy().into_owned();
            write(f.store.as_ref(), move |tx| tx.projects().insert_repository(&StoredRepository {
                id: id("guidance-repo"), location, name: "Guidance".into(), remote_url: None,
                primary: true, lifecycle: "Active".into(), languages: vec![], revision: 1,
                registered_at_ms: 1,
            })).unwrap();
            let source = Arc::new(NativeSourceTree::new());
            let tool = f.driver.schemas().builtin("sessions.get", devworks_core::registry::Mode::Query,
                devworks_core::registry::SchemaRevision(1), "get_session").unwrap();
            let schemas = f.driver.schemas().clone();
            f.driver = NativeProviders::new(
                f.store.clone(), f.artifacts.clone(), f.connections.clone(), f.usage.clone(),
                schemas, ProviderComposition::new(f.http.clone())
                    .with_repository_source(f.store.clone(), source.clone()),
            );
            let mut request = f.generation(vec![tool]);
            let original_context = match &request.input {
                InferenceInput::Generation { context, .. } => context.clone(),
                _ => unreachable!(),
            };
            let base: ProviderContext = serde_json::from_slice(
                &f.artifacts.read_exact(f.store.as_ref(), &f.context, &original_context,
                    1024 * 1024).unwrap().0).unwrap();
            let targets = vec!["README.md".into()];
            let bundle = devworks_core::providers::instructions::resolve(
                source.as_ref(), &root, id("guidance-repo"), &targets,
            ).unwrap();
            let bound = devworks_core::providers::instructions::bind_context(base, &[bundle]).unwrap();
            if let InferenceInput::Generation { context, .. } = &mut request.input {
                *context = f.bytes(&serde_json::to_vec(&bound).unwrap());
            }
            let bound_context = match &request.input {
                InferenceInput::Generation { context, .. } => context.clone(),
                _ => unreachable!(),
            };
            *f.instruction_targets.lock().unwrap() = Some(vec![(id("guidance-repo"), targets)]);
            let produced = f.produce(request).await.unwrap();
            assert_eq!(produced.result.outcome, Some(AttemptEnd::ToolCallsReady));
            assert_eq!(produced.result.source_context.as_ref().unwrap().sha256.as_str(),
                bound_context.sha256.as_str());
            assert_eq!(produced.result.instruction_targets.as_ref().unwrap().len(), 1);

            let mut composition = tool_runtime_composition(&f);
            composition.indexing = Some(IndexComposition {
                parser: Arc::new(TreeSitterParser::new()), source,
                fs: Arc::new(NativeFilesystem::new()), settings: IndexSettings::default(),
                config: IndexingConfig { discover_at_boot: false, ..IndexingConfig::default() },
                enrichers: vec![],
            });
            let app = devworks_engine::boot(composition).await.unwrap();
            std::fs::write(root.join("AGENTS.md"), "changed before tool dispatch").unwrap();
            let held = app.dispatch_pending_tool_calls(&f.context, &id("session")).unwrap();
            assert_eq!(held.len(), 1);
            assert!(held[0].outcome.is_err());
            assert!(f.transcript().iter().all(|entry| entry.kind != "ToolDispatchStarted"
                && entry.kind != "ToolResponseRecorded"));
            if started {
                use devworks_core::services::sessions::{transcript::{BatchOutcome, SessionBatch, SessionWriter}, turn};
                let call: CompleteToolCall = serde_json::from_value(f.transcript().iter()
                    .find(|entry| entry.kind == "ToolCallRecorded").unwrap().body["toolCall"].clone()).unwrap();
                let key = call.call;
                let ctx = f.context.clone();
                write(f.store.as_ref(), move |tx| {
                    let session = tx.sessions().session(&id("session"))?.unwrap();
                    let batch = turn::batch_of(&id("session"), tx.new_id(IdKind::TranscriptBatch),
                        session.context_epoch, session.next_sequence,
                        vec![TranscriptBody::ToolDispatchStarted { call: key }],
                        || tx.new_id(IdKind::TranscriptEntry));
                    assert!(matches!(SessionWriter::append_in(tx, &ctx, &SessionBatch::entries(batch))?,
                        BatchOutcome::Accepted(_)));
                    Ok(())
                }).unwrap();
            } else {
                std::fs::write(root.join("AGENTS.md"), "first guidance").unwrap();
            }
            let recovered = app.dispatch_pending_tool_calls(&f.context, &id("session")).unwrap();
            assert_eq!(recovered.len(), 1);
            assert!(recovered[0].outcome.is_ok());
            assert_eq!(f.transcript().iter().filter(|entry| entry.kind == "ToolResponseRecorded").count(), 1);
            app.shutdown().await;
            f.usage.stop();
            f.usage.drain().await;
        }
    });
}

#[test]
fn incomplete_native_call_never_enters_dispatchable_session_history() {
    runtime().block_on(async{
    let f=Fx::new("CompatibleChat",Script::sse("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\",\"tool_calls\":[{\"index\":0,\"id\":\"call_actual\",\"type\":\"function\",\"function\":{\"name\":\"list_sessions\",\"arguments\":\"{\"}}]},\"finish_reason\":null}]}\n\n"),false);
    let tool=f.driver.schemas().builtin("sessions.list",devworks_core::registry::Mode::Query,devworks_core::registry::SchemaRevision(1),"list_sessions").unwrap();
    let out=f.produce(f.generation(vec![tool])).await.unwrap();assert_ne!(out.result.outcome,Some(AttemptEnd::ToolCallsReady));let rows=f.transcript();assert!(rows.iter().all(|r|r.kind!="ToolCallRecorded"));assert!(rows.iter().any(|r|r.body.get("interrupted")==Some(&json!(true))));
});
}

#[test]
fn protocol_profiles_inject_only_their_current_access_credentials() {
    runtime().block_on(async{
    let messages=r#"{"id":"msg_actual","model":"fixture-model","content":[{"type":"text","text":"messages"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":2}}"#;
    let responses=r#"{"id":"resp_actual","model":"fixture-model","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"responses"}]}],"usage":{"input_tokens":3,"output_tokens":2}}"#;
    for profile in ["AnthropicMessages","ClaudeSubscriptionMessages","OpenAiResponses"] {
        let f=Fx::new(profile,Script::aggregate(if profile=="OpenAiResponses"{responses}else{messages}),false);
        let out=f.produce(f.generation(vec![])).await.unwrap();assert_eq!(out.result.outcome,Some(AttemptEnd::Completed),"{profile}");
        let http=f.http.lock().unwrap();let requests=http.requests.lock().unwrap();let sent=&requests[0];
        if profile=="AnthropicMessages" {assert_eq!(sent.header_value("x-api-key"),Some("actual-fixture-key"));assert!(sent.header_value("Authorization").is_none());}
        else {assert_eq!(sent.header_value("Authorization"),Some(if profile=="OpenAiResponses"{"Bearer actual-fixture-key"}else{"Bearer actual-fixture-access"}));assert!(sent.header_value("x-api-key").is_none());}
    }
});
}

#[test]
fn non_success_http_body_cannot_release_tools_and_known_retry_is_persisted() {
    runtime().block_on(async{
    let mut script=Script::aggregate(r#"{"error":{"code":"rate_limit_error","message":"later","resets_in_seconds":12},"usage":{"prompt_tokens":7,"completion_tokens":0}}"#);script.status=429;script.headers.push(("retry-after".into(),"10".into()));
    let f=Fx::new("CompatibleChat",script,false);let out=f.produce(f.generation(vec![])).await.unwrap();assert_eq!(out.result.outcome,Some(AttemptEnd::Failed));assert_eq!(out.result.retry_at_ms,Some(T0+12_000));assert!(out.result.faults.iter().any(|s|s.contains("Cooldown")));
    assert!(f.transcript().iter().all(|r|r.kind!="ToolCallRecorded"));let cooldown=read(f.store.as_ref(),|s|s.router().cooldowns());assert_eq!(cooldown.iter().find(|c|c.model_id==id("model")).unwrap().until_ms,T0+12_000);
});
}

#[test]
fn shutdown_fence_rejects_late_content_after_transport_close_outlives_deadline() {
    runtime().block_on(async{
    let mut script=Script::sse("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"late\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n");let gate=Arc::new((Mutex::new(false),std::sync::Condvar::new()));script.close_gate=Some(gate.clone());let closing=script.closing.clone();
    let f=Arc::new(Fx::new("CompatibleChat",script,false));let request=f.generation(vec![]);let worker=f.clone();let producing=tokio::spawn(async move{worker.produce(request).await});closing.notified().await;
    let drained=f.usage.drain_until(tokio::time::Instant::now()+std::time::Duration::from_millis(5)).await.unwrap();assert!(!drained.unclosed.is_empty());let before=read(f.store.as_ref(),|s|Ok(s.stamp().committed_sequence));
    *gate.0.lock().unwrap()=true;gate.1.notify_all();let _=producing.await.unwrap();assert_eq!(read(f.store.as_ref(),|s|Ok(s.stamp().committed_sequence)),before);assert!(f.transcript().is_empty());
});
}

#[test]
fn cancelled_owned_http_retains_unknown_tail_usage_and_request_marker_after_local_close() {
    runtime().block_on(async{
    let mut script=Script::sse("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"prefix\"},\"finish_reason\":null}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1}}\n\n");script.wait_for_cancel=true;let waiting=script.waiting.clone();let f=Arc::new(Fx::new("CompatibleChat",script,false));let request=f.generation(vec![]);let worker=f.clone();let producing=tokio::spawn(async move{worker.produce(request).await});waiting.notified().await;f.usage.stop();
    let out=producing.await.unwrap().unwrap();let DriveOutcome::Settled(balance)=out.accounting else{panic!("not settled")};assert!(balance.local_quiescent&&balance.unknown_tail&&balance.incomplete);assert!(devworks_core::services::usage::accounting::summary(&balance).unwrap().cancellation_requested);assert_eq!(out.result.outcome,Some(AttemptEnd::UnknownTail));assert!(f.http.lock().unwrap().closed.load(Ordering::SeqCst));assert!(f.transcript().iter().any(|r|r.body.get("interrupted")==Some(&json!(true))));
});
}

impl Fx {
    fn configure_connection(&self, action: Value) {
        let user = principal(self.store.as_ref(), "native-user", PrincipalKind::Human);
        let authority = devworks_core::authority::HostAuthority {
            store: self.store.as_ref(),
            clock: self.h.clock.as_ref(),
            protected_branches: &[],
        };
        let gateway = devworks_core::gateway::Gateway {
            authority: &authority,
            store: self.store.as_ref(),
            ids: self.h.ids.as_ref(),
        };
        let frame = json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":2,"requestKey":self.h.ids.next_id(IdKind::Command),"body":{"kind":"router.configure","action":action}});
        let reply = UseCases::new()
            .with_connections(self.connections.clone())
            .command(
                &gateway,
                &user,
                "router.configure",
                &serde_json::to_vec(&frame).unwrap(),
            )
            .unwrap();
        assert!(
            matches!(reply,devworks_core::protocol::CommandReply::Recorded{receipt,..} if matches!(receipt.decision,devworks_core::protocol::results::DomainDecision::Applied{..})),
            "connection configuration did not apply"
        );
    }
}
#[test]
fn actual_connection_endpoint_aba_and_public_replacement_cannot_relabel_native_continuations() {
    runtime().block_on(async {
use devworks_core::providers::materialize::{OwnedMaterials,Materials};
let f=Fx::new("CompatibleChat",Script::sse("data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"retained actual native reasoning\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"),false);
assert_eq!(f.produce(f.generation(vec![])).await.unwrap().result.outcome,Some(AttemptEnd::Completed));let rows=f.transcript();let actual=rows.iter().find(|r|r.kind=="NativeItem").expect("actual response must produce a bound native artifact");let binding:NativeBinding=serde_json::from_value(actual.body["binding"].clone()).unwrap();let native:ArtifactRef=serde_json::from_value(actual.body["artifact"].clone()).unwrap();
let request_for=|request_id:&str,binding:NativeBinding,artifact:ArtifactRef|{let c=ProviderContext{version:1,instructions:vec![],instruction_stamps:vec![],messages:vec![Message{role:MessageRole::Assistant,content:vec![ContentBlock::Native{binding,artifact}]}],parameters:Default::default()};let mut request=f.request(InferenceInput::Generation{session_id:id("session"),context:f.bytes(&serde_json::to_vec(&c).unwrap()),context_epoch:Counter::new(0).unwrap(),tools:vec![],response_headroom_tokens:Revision::new(10).unwrap(),requires_images:false,requires_parallel_tools:false});request.request_id=id(request_id);request};
f.http.lock().unwrap().body=VecDeque::from([b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"accepted\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_vec()]);assert_eq!(f.produce(request_for("good-native",binding.clone(),native.clone())).await.unwrap().result.outcome,Some(AttemptEnd::Completed));assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(),2);
// Public version publication preserves artifact creation provenance but attributes these new
// bytes to the Human; checking only artifact-level provenance would wrongly qualify them.
let human=principal(f.store.as_ref(),"native-editor",PrincipalKind::Human);let staged=f.artifacts.stage_host_bytes(f.store.as_ref(),br#"{"reasoning_content":"replacement bytes"}"#,json!({"source":"replacement fixture"})).unwrap().unwrap();let pending=staged.clone();let service=f.artifacts.clone();let artifact=native.artifact_id.clone();let version=write(f.store.as_ref(),move|tx|{let row=tx.artifacts().artifact(&artifact)?.unwrap();service.publish_version(tx,&human,&artifact,&Revision::new(row.revision).unwrap(),"application/json",&pending).map_err(|e|StorageError::InvalidRecord(format!("{e:?}")))}).unwrap();f.artifacts.settle_host_bytes(f.store.as_ref(),&staged,true);assert_eq!(version.source["principalKind"],"Human");let replacement=version_ref(&version);
let schemas=f.driver.schemas();let materials=OwnedMaterials{store:f.store.as_ref(),artifacts:f.artifacts.as_ref(),context:&f.context,schemas:schemas.as_ref()};assert!(materials.native(&replacement,&binding,65536).is_err());assert!(materials.native(&native,&binding,65536).is_ok());assert!(f.produce(request_for("human-version",binding.clone(),replacement)).await.is_err());
for endpoint in ["http://localhost:11434/v2","http://localhost:11434/v1"] {let current=read(f.store.as_ref(),|s|s.router().connection(&id("connection"))).unwrap();f.configure_connection(json!({"kind":"Connection","connectionId":"connection","expectedRevision":current.revision.to_string(),"name":"Fixture","profile":"CompatibleChat","endpoint":endpoint}));}
assert_eq!(read(f.store.as_ref(),|s|s.router().connection(&id("connection"))).unwrap().account_revision,3);assert!(f.produce(request_for("after-aba",binding.clone(),native.clone())).await.is_err());let mut relabeled=binding;relabeled.account_revision=Revision::new(3).unwrap();assert!(f.produce(request_for("relabel-after-aba",relabeled,native)).await.is_err());assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(),2,"neither a public version nor forged current binding may reach native HTTP");
});
}

#[path = "providers/auth_lifecycle.rs"]
mod auth_lifecycle;

#[path = "providers/effort_and_prices.rs"]
mod effort_and_prices;
#[test]
fn codex_aggregate_consumer_still_uses_native_sse_and_semantic_end_closure() {
    runtime().block_on(async{
let f=Fx::new("CodexSubscriptionResponses",Script::sse("event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"model\":\"fixture-model\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n"),false);let mut request=f.generation(vec![]);if let InferenceInput::Generation{context,..}=&mut request.input{let bytes=f.artifacts.read_exact(f.store.as_ref(),&f.context,context,8*1024*1024).unwrap().0;let mut c:ProviderContext=serde_json::from_slice(&bytes).unwrap();c.parameters.response_mode=Some(devworks_core::providers::ResponseMode::Aggregate);*context=f.bytes(&serde_json::to_vec(&c).unwrap());}let result=f.produce(request).await.unwrap();assert_eq!(result.result.outcome,Some(AttemptEnd::Completed));let script=f.http.lock().unwrap();assert!(script.closed.load(Ordering::SeqCst));assert_eq!(script.reads.load(Ordering::SeqCst),1);let requests=script.requests.lock().unwrap();let body:Value=serde_json::from_slice(requests[0].body.as_ref().unwrap()).unwrap();assert_eq!(body["stream"],true);assert!(requests[0].headers.iter().any(|(n,v)|n.eq_ignore_ascii_case("accept")&&v.as_str()=="text/event-stream"));
});
}
#[test]
fn malformed_suffix_in_same_transport_chunk_preserves_preceding_usage_without_releasing_calls() {
    runtime().block_on(async {
        let mut script = Script::sse(
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\n",
        );
        script
            .body
            .front_mut()
            .unwrap()
            .extend_from_slice(&[255, b'\n', b'\n']);
        let f = Fx::new("CompatibleChat", script, false);
        let result = f.produce(f.generation(vec![])).await.unwrap();
        let DriveOutcome::Settled(balance) = result.accounting else {
            panic!("incomplete stream not settled")
        };
        assert!(balance.incomplete);
        let rows = read(f.store.as_ref(), |s| {
            s.usage().usage_observations(&balance.attempt_id, None, 64)
        });
        assert_eq!(rows.len(), 1);
        let summary = devworks_core::services::usage::accounting::summary(&balance).unwrap();
        assert_eq!(summary.meter.quantities.input_tokens, Some(7));
        assert_eq!(summary.meter.quantities.output_tokens, Some(2));
        assert!(
            !f.transcript()
                .iter()
                .any(|row| row.kind == "ToolCallRecorded")
        );
    });
}
#[test]
fn aggregate_complete_tool_is_not_dispatchable_until_actual_transport_close() {
    runtime().block_on(async{
let mut script=Script::aggregate(r#"{"model":"fixture-model","choices":[{"index":0,"message":{"content":null,"tool_calls":[{"id":"actual-id","type":"function","function":{"name":"list_sessions","arguments":"{}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":1}}"#);let gate=Arc::new((Mutex::new(false),std::sync::Condvar::new()));script.close_gate=Some(gate.clone());let closing=script.closing.clone();let f=Arc::new(Fx::new("CompatibleChat",script,false));let tool=f.driver.schemas().builtin("sessions.list",devworks_core::registry::Mode::Query,devworks_core::registry::SchemaRevision(1),"list_sessions").unwrap();let request=f.generation(vec![tool]);let owner=f.clone();let task=tokio::spawn(async move{owner.produce(request).await});closing.notified().await;assert!(!f.transcript().iter().any(|row|row.kind=="ToolCallRecorded"));assert!(!f.http.lock().unwrap().closed.load(Ordering::SeqCst));*gate.0.lock().unwrap()=true;gate.1.notify_all();let result=task.await.unwrap().unwrap();assert_eq!(result.result.outcome,Some(AttemptEnd::ToolCallsReady));assert!(f.transcript().iter().any(|row|row.kind=="ToolCallRecorded"));
});
}

#[test]
fn session_bound_embedding_uses_actual_epoch_and_artifact_channel_without_assistant_turn() {
    runtime().block_on(async{
let f=Fx::new("Embedding",Script::aggregate(r#"{"model":"fixture-model","data":[{"index":0,"embedding":[0.1,0.2]}],"usage":{"prompt_tokens":1,"total_tokens":1}}"#),true);write(f.store.as_ref(),|tx|{let mut session=tx.sessions().session(&id("session"))?.unwrap();session.context_epoch=3;session.revision+=1;tx.sessions().update_session(&session)}).unwrap();let request=f.request(InferenceInput::Embedding{embedding_space_id:id("space"),dimensions:2,inputs:vec![f.bytes(b"actual session-bound embedding")]});let result=f.produce_dimensions(request,PricingContext{input_semantics:InputSemantics::Unknown,cache_read_possible:false,cache_write_possible:false,cache_write_ttl_ms:None},vec!["Text".into()],None,None,true).await.unwrap();assert_eq!(result.result.outcome,Some(AttemptEnd::Completed));assert!(f.transcript().is_empty());let manifest=f.artifacts.read_exact(f.store.as_ref(),&f.context,result.result.result_artifact.as_ref().unwrap(),65536).unwrap().0;let body:Value=serde_json::from_slice(&manifest).unwrap();assert_eq!(body["binding"]["contextEpoch"],"3");let DriveOutcome::Settled(balance)=result.accounting else{panic!("not settled")};let row=read(f.store.as_ref(),|s|s.usage().inference_attempt(&balance.attempt_id)).unwrap();assert_eq!(row.session_id,Some(id("session")));assert_eq!(devworks_core::services::usage::accounting::snapshot(&row).unwrap().context_epoch,Some(3));
});
}
#[test]
fn native_http_fault_categories_and_entitlement_precedence_survive_into_recorded_result() {
    runtime().block_on(async{
for (status,kind) in [(401,"Authentication"),(402,"Provider"),(403,"Authentication"),(404,"ModelUnavailable"),(408,"Unreachable"),(429,"Cooldown"),(503,"Overloaded")] {
let mut script=Script::aggregate(r#"{"error":{"message":"provider refused"}}"#);script.status=status;let f=Fx::new("OpenRouterChat",script,false);let result=f.produce(f.generation(vec![])).await.unwrap();assert_eq!(result.result.outcome,Some(AttemptEnd::Failed));assert!(result.result.faults.contains(&format!("{kind}:http_{status}")),"{:?}",result.result.faults);
}
let f=Fx::new("CodexSubscriptionResponses",Script::sse("event: error\ndata: {\"type\":\"error\",\"error\":{\"code\":\"usage_not_included\",\"message\":\"plan has no model access despite rate limit wording\"}}\n\n"),false);let result=f.produce(f.generation(vec![])).await.unwrap();assert!(result.result.faults.iter().any(|s|s=="ModelUnavailable:usage_not_included"));assert!(read(f.store.as_ref(),|s|s.router().cooldowns()).is_empty());
});
}

#[path = "providers/auto_routes.rs"]
mod auto_routes;
#[path = "providers/content_order.rs"]
mod content_order;
#[path = "providers/subscription_limits.rs"]
mod subscription_limits;

#[test]
fn external_auxiliary_cancel_reaches_the_actual_producer_without_stopping_other_sources() {
    runtime().block_on(async {
        let mut script = Script::sse("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"prefix\"},\"finish_reason\":null}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":1}}\n\n");
        script.wait_for_cancel = true;
        let waiting = script.waiting.clone();
        let f = Arc::new(Fx::new("CompatibleChat", script, false));
        let request = f.generation(vec![]);
        let cancellation = Cancellation::new();
        let worker = f.clone();
        let token = cancellation.clone();
        let producing = tokio::spawn(async move {
            worker.produce_dimensions_cancellable(request,
                PricingContext { input_semantics: InputSemantics::Unknown,
                    cache_read_possible: false, cache_write_possible: false,
                    cache_write_ttl_ms: None }, vec!["Text".into()], None, None, (false, Some(token))).await
        });
        waiting.notified().await;
        cancellation.cancel();
        let outcome = producing.await.unwrap().unwrap();
        let DriveOutcome::Settled(balance) = outcome.accounting else { panic!("not settled") };
        assert!(balance.local_quiescent && balance.unknown_tail);
        assert!(devworks_core::services::usage::accounting::summary(&balance).unwrap().cancellation_requested);
        assert!(outcome.result.vector_artifacts.is_empty());
        assert!(f.http.lock().unwrap().closed.load(Ordering::SeqCst));
        assert!(!read(f.store.as_ref(), |s| s.usage().usage_observations(&balance.attempt_id,None,64)).is_empty());
        {
            let mut script = f.http.lock().unwrap();
            script.wait_for_cancel = false;
            script.body = VecDeque::from([b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"other source\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_vec()]);
        }
        let mut second = f.generation(vec![]); second.request_id = id("independent-request");
        let second = f.produce(second).await.unwrap();
        assert_eq!(second.result.outcome,Some(AttemptEnd::Completed));
        let before = Cancellation::new(); before.cancel();
        let f = Fx::new("Embedding", Script::aggregate("{}"), true);
        let request = f.request(InferenceInput::Embedding {
            embedding_space_id:id("space"), dimensions:2, inputs:vec![f.bytes(b"never sent")],
        });
        let result = f.produce_dimensions_cancellable(request,
            PricingContext{input_semantics:InputSemantics::Unknown,cache_read_possible:false,
                cache_write_possible:false,cache_write_ttl_ms:None}, vec!["Text".into()],None,None,(false,Some(before))).await;
        assert!(matches!(result,Err(StorageError::Cancelled)));
        assert!(f.http.lock().unwrap().requests.lock().unwrap().is_empty());
        assert!(read(f.store.as_ref(), |s| s.router().route_request(&id("request"))).is_none());
    });
}

struct SourceGateSecrets {
    inner: SessionSecrets,
    reads: AtomicUsize,
    at: usize,
    change: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
impl SecretStore for SourceGateSecrets {
    fn name(&self) -> &'static str {
        "controlled source-authority gate"
    }
    fn put(
        &self,
        key: &SecretKey,
        value: &Secret,
    ) -> Result<(), devworks_core::ports::secrets::SecretError> {
        self.inner.put(key, value)
    }
    fn get(
        &self,
        key: &SecretKey,
    ) -> Result<Option<Secret>, devworks_core::ports::secrets::SecretError> {
        if self.reads.fetch_add(1, Ordering::SeqCst) + 1 == self.at
            && let Some(change) = self.change.lock().unwrap().take()
        {
            change();
        }

        self.inner.get(key)
    }
    fn delete(&self, key: &SecretKey) -> Result<bool, devworks_core::ports::secrets::SecretError> {
        self.inner.delete(key)
    }
    fn list(&self, store: &str) -> Result<Vec<Id>, devworks_core::ports::secrets::SecretError> {
        self.inner.list(store)
    }
}
#[test]
fn original_auxiliary_authority_is_fenced_at_admission_and_after_credential_wait() {
    runtime().block_on(async {
        for at in [1,2] {
            let secrets=Arc::new(SourceGateSecrets{inner:SessionSecrets::new(),reads:AtomicUsize::new(0),at,change:Mutex::new(None)});
            let f=Fx::with_access("OpenAiResponses",Script::aggregate(r#"{"id":"resp_actual","model":"fixture-model","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"actual"}]}],"usage":{"input_tokens":3,"output_tokens":1}}"#),false,Arc::new(NoNetwork),secrets.clone());
            let policy=read(f.store.as_ref(),|s|devworks_core::authority::policy::resolve_policy(s.configuration(),None));
            *f.source_fences.lock().unwrap()=vec![devworks_core::ports::AuthorityFence::Policy{project_id:None,digest:policy.digest()}];
            let store=f.store.clone();
            *secrets.change.lock().unwrap()=Some(Box::new(move||{
                write(store.as_ref(),|tx|{
                    let key=devworks_core::authority::policy::ACCESS_PROFILE_KEY;
                    let descriptor=devworks_core::settings::catalog::descriptor(key).unwrap();
                    tx.configuration().append_revision(&ConfigScope::Workspace,"settings",1,1,
                        json!({"values":{key:{"schema":descriptor.schema_ref(),"value":"projectScope"}}})).map(|_|())
                }).unwrap();
            }));
            let result=f.produce(f.generation(vec![])).await;
            assert!(result.is_err() || result.as_ref().is_ok_and(|r|r.result.outcome!=Some(AttemptEnd::Completed)));
            assert!(f.http.lock().unwrap().requests.lock().unwrap().is_empty(),"revoked source cannot reach HTTP after source-authority phase {at}");
            let route=read(f.store.as_ref(),|s|s.router().route_request(&id("request")));
            if at==1 {assert!(route.is_none(),"no logical native intent is published under changed source authority");}
            else {assert!(route.is_some(),"actual reservation exists before late credential gate; it must settle honestly");}
            assert_ne!(read(f.store.as_ref(),|s|devworks_core::authority::policy::resolve_policy(s.configuration(),None)).digest(),policy.digest());
        }
    });
}

#[path = "providers/embedding_qualification.rs"]
mod embedding_qualification;

#[path = "providers/convention_search.rs"]
mod convention_search;

#[path = "providers/selection_refresh.rs"]
mod selection_refresh;

#[path = "providers/convention_index_compatibility.rs"]
mod convention_index_compatibility;
