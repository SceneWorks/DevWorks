//! The actual owned producer, SQLite accounting, current connection owner, artifacts and
//! SessionWriter. Controlled transport bytes exercise native protocols without model execution.
mod common;
use common::*;
use devworks_adapters::fs::{
    NativeFilesystem, artifacts::NativeArtifactFiles, sources::HostArtifactSources,
};
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
    protocol::{ActorId, Counter, Id, Protocol, Revision, provider::*},
    providers::{
        capabilities::{Capabilities, EmbeddingDimensions, Support},
        context::*,
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
        driver::{EmbeddingRegistration, NativeProviders, ProviderComposition},
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
        let expected_epoch = if embedding {
            if embedding_session {
                Some(
                    read(self.store.as_ref(), |s| {
                        s.sessions().session(&id("session"))
                    })
                    .unwrap()
                    .context_epoch,
                )
            } else {
                None
            }
        } else {
            Some(0)
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
        self.driver
            .produce(
                self.context.clone(),
                logical,
                ReportCategory::Assistant,
                attempt,
                request,
            )
            .await
    }
    fn transcript(&self) -> Vec<StoredEntry> {
        read(self.store.as_ref(), |s| {
            s.sessions().entries(&id("session"), 0, 500)
        })
    }
}
#[test]
fn native_generation_persists_text_usage_and_exact_receipt_replay_without_second_transport() {
    runtime().block_on(async{
    let f=Fx::new("CompatibleChat",Script::sse("data: {\"model\":\"fixture-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n"),false);
    let request=f.generation(vec![]);let outcome=f.produce(request.clone()).await.unwrap();assert_eq!(outcome.result.outcome,Some(AttemptEnd::Completed));assert_eq!(f.transcript().iter().filter(|r|r.kind=="AssistantAttempt").count(),1);assert_eq!(f.transcript()[0].body["visibleText"],"hello");
    let attempt=match outcome.accounting{DriveOutcome::Settled(balance)=>{assert!(balance.local_quiescent);balance.attempt_id},other=>panic!("{other:?}")};
    let observations=read(f.store.as_ref(),|s|s.usage().usage_observations(&attempt,None,64));assert_eq!(observations.len(),1);
    let replay=f.produce(request).await.unwrap();assert!(matches!(replay.accounting,DriveOutcome::AlreadyDispatched));assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(),1);assert!(f.http.lock().unwrap().closed.load(Ordering::SeqCst));
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
let request_for=|request_id:&str,binding:NativeBinding,artifact:ArtifactRef|{let c=ProviderContext{version:1,instructions:vec![],messages:vec![Message{role:MessageRole::Assistant,content:vec![ContentBlock::Native{binding,artifact}]}],parameters:Default::default()};let mut request=f.request(InferenceInput::Generation{session_id:id("session"),context:f.bytes(&serde_json::to_vec(&c).unwrap()),context_epoch:Counter::new(0).unwrap(),tools:vec![],response_headroom_tokens:Revision::new(10).unwrap(),requires_images:false,requires_parallel_tools:false});request.request_id=id(request_id);request};
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
for (status,kind) in [(401,"Authentication"),(402,"Authentication"),(403,"Authentication"),(404,"ModelUnavailable"),(408,"Unreachable"),(429,"Cooldown"),(503,"Overloaded")] {
let mut script=Script::aggregate(r#"{"error":{"message":"provider refused"}}"#);script.status=status;let f=Fx::new("OpenRouterChat",script,false);let result=f.produce(f.generation(vec![])).await.unwrap();assert_eq!(result.result.outcome,Some(AttemptEnd::Failed));assert!(result.result.faults.contains(&format!("{kind}:http_{status}")),"{:?}",result.result.faults);
}
let f=Fx::new("CodexSubscriptionResponses",Script::sse("event: error\ndata: {\"type\":\"error\",\"error\":{\"code\":\"usage_not_included\",\"message\":\"plan has no model access despite rate limit wording\"}}\n\n"),false);let result=f.produce(f.generation(vec![])).await.unwrap();assert!(result.result.faults.iter().any(|s|s=="ModelUnavailable:usage_not_included"));assert!(read(f.store.as_ref(),|s|s.router().cooldowns()).is_empty());
});
}

#[path = "providers/auto_routes.rs"]
mod auto_routes;
