//! Real Engine/Tokio destruction while the actual native producer is preparing OAuth access.
use super::*;
use devworks_core::{
    context::{HostIngress, PrincipalKind},
    ports::{
        http::{HttpError, HttpResponse, HttpTransport},
        provider_http::{
            HttpStream, SendPhase, StreamLimits, StreamingHttp, TransportError, TransportFailure,
        },
        secrets::{Secret, SecretError, SecretKey, SecretStore, SessionSecrets},
        storage::*,
        usage::*,
    },
    protocol::{
        Counter, Id, Protocol, Revision,
        provider::*,
        records::{EffortName, RouteIntent},
    },
    providers::context::{GenerationParameters, ProviderContext},
    services::{
        artifacts::version_ref,
        router::{
            Purpose, RequestAdmission,
            accounting::{AttemptSelection, ReportCategory},
            connections::auth::{Credential, Tokens},
        },
        usage::{accounting::AttemptRequest, meter::InputSemantics, pricing::PricingContext},
    },
};
use std::sync::{
    Condvar, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
fn id(v: &str) -> Id {
    Id::new(v).unwrap()
}
struct Gate {
    entered: mpsc::Sender<()>,
    released: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn wait(&self) {
        let _ = self.entered.send(());
        let mut open = self.released.lock().unwrap();
        while !*open {
            open = self.changed.wait(open).unwrap();
        }
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct Refresh {
    gate: Option<Arc<Gate>>,
    calls: AtomicUsize,
}
impl HttpTransport for Refresh {
    fn send(&self, r: &devworks_core::ports::http::HttpRequest) -> Result<HttpResponse, HttpError> {
        assert!(r.url.contains("oauth/token"));
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            gate.wait();
        }
        Ok(HttpResponse {
            status: 200,
            body: br#"{"access_token":"rotated","refresh_token":"next-refresh","expires_in":3600}"#
                .to_vec(),
        })
    }
}
struct Secrets {
    inner: SessionSecrets,
    gate: Arc<Gate>,
    armed: AtomicBool,
}
impl SecretStore for Secrets {
    fn name(&self) -> &'static str {
        "controlled session fixture"
    }
    fn put(&self, k: &SecretKey, v: &Secret) -> Result<(), SecretError> {
        if self.armed.load(Ordering::SeqCst) {
            self.gate.wait();
        }
        self.inner.put(k, v)
    }
    fn get(&self, k: &SecretKey) -> Result<Option<Secret>, SecretError> {
        self.inner.get(k)
    }
    fn delete(&self, k: &SecretKey) -> Result<bool, SecretError> {
        self.inner.delete(k)
    }
    fn list(&self, n: &str) -> Result<Vec<Id>, SecretError> {
        self.inner.list(n)
    }
}
struct NoInference(AtomicUsize);
impl StreamingHttp for NoInference {
    fn open(
        &self,
        _: &devworks_core::ports::http::HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(TransportError {
            phase: SendPhase::NotSent,
            kind: TransportFailure::InvalidRequest,
        })
    }
}
#[test]
fn actual_engine_drop_is_bounded_during_owned_oauth_http_and_started_secret_write() {
    run_case(false);
}
pub(super) fn run_case(writer_race: bool) {
    for external_secret in [false, true] {
        let root = std::env::temp_dir().join(format!(
            "dw19-refresh-drop-{}-{external_secret}-{writer_race}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let (entered, signal) = mpsc::channel();
        let gate = Arc::new(Gate {
            entered,
            released: Mutex::new(false),
            changed: Condvar::new(),
        });
        let secrets = Arc::new(Secrets {
            inner: SessionSecrets::new(),
            gate: gate.clone(),
            armed: AtomicBool::new(false),
        });
        let http = Arc::new(Refresh {
            gate: (!external_secret).then(|| gate.clone()),
            calls: AtomicUsize::new(0),
        });
        let inference = Arc::new(NoInference(AtomicUsize::new(0)));
        let mut config = DesktopConfig::new(&root);
        config.shutdown_bound = Duration::from_millis(30);
        config.connection_tick = None;
        config.indexing.discover_at_boot = false;
        config.shared_skills = None;
        config.secrets = Some(secrets.clone());
        config.http = Some(http.clone());
        config.providers =
            Some(devworks_engine::providers::driver::ProviderComposition::new(inference.clone()));
        let engine = Engine::start(config).unwrap();
        let app = engine.host.runtime.clone();
        let store = app.store().clone();
        let info = store.info();
        // This fixture measures the provider tail against an exact whole-store stamp.
        // Settle the independent real startup owners before applying its 30 ms shutdown
        // budget; an already-admitted queue settlement may otherwise outlive that budget.
        let startup_deadline = std::time::Instant::now() + Duration::from_secs(30);
        let accounting = app.accounting().watch();
        loop {
            // Use Help's real durable cancellation signal. This fixture does not need a
            // completed catalog, and maintenance retains this source-change attempt.
            let cancelled_failure = run_write(store.as_ref(), WriteOptions::default(), |tx| {
                let page = tx.queue().deliveries(
                    &DeliveryFilter {
                        consumer: Some("help_docs".into()),
                        operation_id: None,
                    },
                    None,
                    500,
                )?;
                assert!(
                    page.next.is_none(),
                    "the fresh fixture has one startup Help generation"
                );
                let mut cancelled_failure = false;
                for entry in page.entries {
                    if entry.state == QueueState::Failed
                        && entry.cancel_requested
                        && entry.last_error.as_deref()
                            == Some("Help generation was cancelled before catalog publication.")
                    {
                        let operation = entry.operation_id.as_ref().expect("Help has an operation");
                        let operation = tx.operations().operation(operation)?.unwrap();
                        cancelled_failure = operation.capability == "help.generate_capability_docs"
                            && operation.state == devworks_core::protocol::OperationState::Failed;
                    }
                    if !entry.cancel_requested
                        && matches!(entry.state, QueueState::Ready | QueueState::Claimed)
                    {
                        let operation = entry.operation_id.as_ref().expect("Help has an operation");
                        assert_eq!(
                            tx.operations().operation(operation)?.unwrap().capability,
                            "help.generate_capability_docs"
                        );
                        tx.queue().cancel(&entry.id)?;
                    }
                }
                Ok(cancelled_failure)
            })
            .unwrap();
            let (help, receipts, bootstrap, drafts) =
                read_snapshot(store.as_ref(), &ReadOptions::default(), |s| {
                    Ok((
                        s.queue().status("help_docs")?,
                        s.queue().status("receipt_replies")?,
                        s.usage().usage_bootstrap()?,
                        s.help().help_drafts(None, 32)?.len(),
                    ))
                })
                .unwrap();
            let accounted = accounting.borrow().as_ref().is_some_and(|pass| {
                pass.as_ref()
                    .is_ok_and(|p| p.indexing_complete && p.recovery_complete && !p.recovery_more)
            });
            // Dispatch can establish the running generation's cancellation as Failed;
            // that exact requested outcome is settled just like cancellation before dispatch.
            if (help.completed + help.cancelled > 0 || cancelled_failure)
                && help.ready == 0
                && help.claimed == 0
                && help.reconciling == 0
                && receipts.ready == 0
                && receipts.claimed == 0
                && receipts.reconciling == 0
                && bootstrap.phase == "Complete"
                && drafts == 0
                && accounted
            {
                break;
            }
            assert!(
                std::time::Instant::now() < startup_deadline,
                "independent startup owners did not settle: Help={help:?}; receipts={receipts:?}; bootstrap={bootstrap:?}; drafts={drafts}; accounted={accounted}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        secrets
            .put(
                &SecretKey::new(info.store_id.0.as_str(), id("shutdown-auth")),
                &Credential::OAuth(Tokens {
                    access: Secret::new("old-access"),
                    refresh: Secret::new("old-refresh"),
                    expires_at_ms: 0,
                    account: Some("fixture-account".into()),
                })
                .to_secret(),
            )
            .unwrap();
        run_write(store.as_ref(), WriteOptions::default(), |tx| {
            let restore_epoch = tx.store().restore_epoch;
            tx.router().put_connection(
                &StoredConnection {
                    id: id("usage-connection"),
                    name: "Explicit free metadata fixture".into(),
                    profile: "CodexSubscriptionResponses".into(),
                    endpoint: "https://provider.test/v1".into(),
                    enabled: true,
                    local_free: true,
                    open_router: None,
                    credential_ref: Some(id("shutdown-auth")),
                    credential_revision: 1,
                    account_revision: 1,
                    refresh_operation_id: None,
                    credential_epoch: Some(restore_epoch),
                    lifecycle: "Connected".into(),
                    state_reason: None,
                    account_digest: None,
                    access_expires_at_ms: None,
                    risk: None,
                    discovered: None,
                    discovered_at_ms: None,
                    healthy: None,
                    checked_at_ms: None,
                    revision: 1,
                    created_at_ms: 1,
                    updated_at_ms: 1,
                },
                None,
            )?;
            tx.router().put_route_model(
                &StoredRouteModel {
                    id: id("usage-model"),
                    display_name: "Native metadata source".into(),
                    endpoint_profile: "CodexSubscriptionResponses".into(),
                    upstream_model_id: "native-metadata-source".into(),
                    enabled: true,
                    efforts: vec!["Low".into(), "High".into()],
                    image_input: false,
                    tools: true,
                    context_tokens: Some(128000),
                    connection_id: Some(id("usage-connection")),
                    position: 1,
                    revision: 1,
                    updated_at_ms: 1,
                },
                None,
            )
        })
        .unwrap();
        run_write(store.as_ref(), WriteOptions::default(), |tx| {
            let actor = tx.store().host_actor.clone();
            tx.sessions().insert_session(&StoredSession {
                id: id("shutdown-session"),
                conversation_id: id("shutdown-conversation"),
                agent_id: actor.clone(),
                project_id: None,
                root_id: None,
                title: "Shutdown access preparation".into(),
                context_epoch: 0,
                revision: 1,
                created_by: actor,
                created_at_ms: 1,
                closed_at_ms: None,
                closed_by: None,
                next_sequence: 1,
                checkpoint_sequence: 0,
            })
        })
        .unwrap();
        let context = HostIngress::new(store.as_ref())
            .context(
                info.host_actor,
                PrincipalKind::Host,
                id("refresh-invocation"),
                id("native-refresh"),
                Counter::new(info.restore_epoch).unwrap(),
                Revision::new(1).unwrap(),
            )
            .with_session(id("shutdown-session"));
        let logical = RequestAdmission {
            request_id: id("replay-request"),
            purpose: Purpose::Conversation,
            session_id: Some(id("shutdown-session")),
            project_id: None,
            root_id: None,
            model_id: id("usage-model"),
            connection_id: id("usage-connection"),
            pin_id: None,
            args_digest: "a".repeat(64),
        };
        let mut request = AttemptRequest {
            admission_key: id("replay-admission"),
            request_id: logical.request_id.clone(),
            actor_id: context.principal().clone(),
            work_id: None,
            job_run_id: None,
            role: "Host".into(),
            query: PriceQuery {
                catalog_model_id: id("usage-model"),
                provider_model_id: Some("native-metadata-source".into()),
                connection_id: id("usage-connection"),
                endpoint_profile: "CodexSubscriptionResponses".into(),
                access_mode: "Subscription".into(),
                service_tier: None,
                input_tokens: Some(10),
                cache_write_ttl_ms: None,
                processing_region: None,
                required_modalities: vec!["Text".into()],
            },
            quote: PriceQuote {
                publication_provider: None,
                classification: CostClassification::LocalFree,
                connection_revision: Some(1),
                snapshot_id: None,
                price_revision: None,
                override_revision: None,
                currency: "USD".into(),
                rates: None,
                tariffs: vec![],
                untracked_reason: None,
            },
            price_generation: 1,
            override_revision: 1,
            pricing: PricingContext {
                input_semantics: InputSemantics::Inclusive,
                cache_read_possible: false,
                cache_write_possible: false,
                cache_write_ttl_ms: None,
            },
            selection: AttemptSelection::Conversation {
                intent: RouteIntent::Model {
                    model_id: id("usage-model"),
                },
                effort: EffortName::Default,
                required_capabilities: vec![],
                pin: None,
            },
            input_tokens: 10,
            output_headroom: 10,
            unit_headroom: vec![],
            expected_context_epoch: Some(0),
            dispatch: None,
            cache_diagnostics: vec![],
        };

        let artifacts = app.artifacts().clone();
        let material = serde_json::to_vec(&ProviderContext {
            version: 1,
            instructions: vec![],
            instruction_stamps: vec![],
            skill_stamp: None,
            messages: vec![devworks_core::providers::context::Message {
                role: devworks_core::providers::context::MessageRole::User,
                content: vec![devworks_core::providers::context::ContentBlock::Text {
                    text: "bounded OAuth preparation fixture".into(),
                    cache: None,
                }],
            }],
            parameters: GenerationParameters::default(),
        })
        .unwrap();
        let staged = artifacts
            .stage_host_bytes(
                store.as_ref(),
                &material,
                serde_json::json!({"test":"actual-native-preparation"}),
            )
            .unwrap()
            .unwrap();
        let pending = staged.clone();
        let service = artifacts.clone();
        let version = run_write(store.as_ref(), WriteOptions::default(), move |tx| {
            service.commit_host_bytes(
                tx,
                &pending,
                "Provider context",
                "application/json",
                serde_json::json!({"test":true}),
            )
        })
        .unwrap();
        artifacts.settle_host_bytes(store.as_ref(), &staged, true);
        request.quote = read_snapshot(store.as_ref(), &ReadOptions::default(), |s| {
            devworks_core::services::prices::CatalogPriceResolver.quote(s, &request.query)
        })
        .unwrap();
        let provider_request = InferenceRequest {
            protocol: Protocol::Draft1,
            request_id: logical.request_id.clone(),
            purpose: InferencePurpose::Conversation,
            attribution: Attribution::Workspace {
                session_id: id("shutdown-session"),
            },
            input: InferenceInput::Generation {
                session_id: id("shutdown-session"),
                context: version_ref(&version),
                context_epoch: Counter::new(0).unwrap(),
                tools: vec![],
                response_headroom_tokens: Revision::new(10).unwrap(),
                requires_images: false,
                requires_parallel_tools: false,
            },
            options: InferenceOptions {
                selection: ModelSelection::Model {
                    model_id: id("usage-model"),
                },
                effort: None,
                output_schema: None,
                deadline_ms: 2000,
            },
            authority_revision: Revision::new(1).unwrap(),
            control_generation: Revision::new(1).unwrap(),
            instruction_revision: Revision::new(1).unwrap(),
            cancellation_revision: Revision::new(1).unwrap(),
        };
        secrets.armed.store(external_secret, Ordering::SeqCst);
        let providers = app.providers().unwrap().clone();
        let preparing = app.accounting().clone();
        let job = engine.host.async_runtime.spawn(async move {
            providers
                .produce(
                    context,
                    logical,
                    ReportCategory::Other,
                    request,
                    provider_request,
                )
                .await
        });
        signal.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(
            read_snapshot(store.as_ref(), &ReadOptions::default(), |s| s
                .usage()
                .inference_attempts(&UsageAttemptFilter::default(), None, 64))
            .unwrap()
            .is_empty()
        );
        let configured =
            writer_race.then(|| super::provider_refresh_lock_tests::block_configure(&engine.host));
        let connections = engine.host.connections.clone();
        let (finished, done) = mpsc::channel();
        std::thread::spawn(move || {
            drop(engine);
            let _ = finished.send(());
        });
        if let Some(configured) = configured {
            // Wait on the real close flag, without occupying the test thread if an old
            // lifecycle read lock deadlocks behind the queued database writer.
            let (closed, observed) = mpsc::channel();
            std::thread::spawn(move || {
                while !connections.is_closed() {
                    std::thread::yield_now();
                }
                let _ = closed.send(());
            });
            let closing = observed.recv_timeout(Duration::from_secs(2));
            configured.release();
            assert!(
                closing.is_ok(),
                "connection close must not wait for a writer-held configure lifecycle read"
            );
            configured
                .finished
                .recv_timeout(Duration::from_secs(2))
                .expect("actual configure must retire after writer gate opens");
        }
        let closed = done.recv_timeout(Duration::from_secs(3));
        let current = read_snapshot(store.as_ref(), &ReadOptions::default(), |s| {
            s.router().connection(&id("usage-connection"))
        })
        .unwrap()
        .unwrap();
        let operation = read_snapshot(store.as_ref(), &ReadOptions::default(), |s| {
            s.operations()
                .operation(current.refresh_operation_id.as_ref().unwrap())
        })
        .unwrap()
        .unwrap();
        let before = read_snapshot(store.as_ref(), &ReadOptions::default(), |s| {
            Ok(s.stamp().committed_sequence)
        })
        .unwrap();
        gate.release();
        assert!(
            closed.is_ok(),
            "actual Engine/Tokio Drop must not await external OAuth/keychain completion: {closed:?}"
        );
        assert_eq!(current.lifecycle, "ReauthenticationRequired");
        assert_eq!(current.credential_revision, 1);
        assert_eq!(
            operation.state,
            devworks_core::protocol::OperationState::OutcomeUnknown
        );
        assert!(operation.payload["allocatedHandle"].as_str().is_some());
        let local = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        local.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), preparing.drain())
                .await
                .unwrap();
        });
        drop(job);
        assert_eq!(inference.0.load(Ordering::SeqCst), 0);
        assert_eq!(http.calls.load(Ordering::SeqCst), 1);
        let after = read_snapshot(store.as_ref(), &ReadOptions::default(), |s| {
            Ok(s.stamp().committed_sequence)
        })
        .unwrap();
        if after != before {
            let events = read_snapshot(store.as_ref(), &ReadOptions::default(), |s| {
                s.history().page(&EventFilter::All, before, 32)
            })
            .unwrap();
            eprintln!(
                "provider shutdown delta external_secret={external_secret} writer_race={writer_race}: before={before} after={after}; identities={:?}",
                events
                    .events
                    .iter()
                    .map(|e| (&e.kind, &e.aggregate_kind, &e.aggregate_id, &e.operation_id))
                    .collect::<Vec<_>>()
            );
        }
        assert_eq!(after, before);
        drop(app);
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }
}
