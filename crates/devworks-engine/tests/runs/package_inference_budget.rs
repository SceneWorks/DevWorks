//! A package's brokered inference from a feature agent's `package.invoke` tool call is admitted
//! against that feature's budget: a reached cap refuses it before any provider request.
use super::*;
use devworks_adapters::fs::{
    NativeFilesystem, artifacts::NativeArtifactFiles, sources::HostArtifactSources,
};
use devworks_core::ports::{
    AuthorityFence, http::NoNetwork, provider_http::*, secrets::SessionSecrets,
};
use devworks_core::protocol::provider::ToolCallKey;
use devworks_core::protocol::records::FeatureBudgetInput;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::router::connections::ConnectionService;
use devworks_core::services::usage::budget;
use devworks_engine::mcp_sampling::McpSampling;
use devworks_engine::package_host::inference::{
    PackageInferenceRequest, grant_for, infer, person_grant,
};
use devworks_engine::providers::{
    driver::{NativeProviders, ProviderComposition},
    schemas::RegisteredSchemas,
};

struct Entropy;
impl devworks_core::ports::http::Entropy for Entropy {
    fn fill(&self, bytes: &mut [u8]) {
        bytes.fill(3);
    }
}
/// Counts transport opens; every open fails before anything is sent.
struct CountHttp(AtomicUsize);
impl StreamingHttp for CountHttp {
    fn open(
        &self,
        _: &HttpRequest,
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

fn limit(f: &Fx, input: FeatureBudgetInput) {
    f.tx(move |_, tx| {
        let work = tx.work().work(&id("root-f"))?.unwrap();
        budget::configure(tx, &work, &ActorId(id("user-a")), &input).map(|_| ())
    });
}

fn compose(
    f: &Fx,
    router: &Arc<devworks_core::services::router::InferenceRouter>,
) -> (
    Arc<ArtifactService>,
    Arc<UsageRuntime>,
    Arc<CountHttp>,
    Arc<NativeProviders>,
) {
    let artifacts = Arc::new(ArtifactService::new(
        Arc::new(NativeArtifactFiles::new(
            f.h.root.clone(),
            f.h.root.with_extension("cache"),
        )),
        Arc::new(HostArtifactSources::new(f.h.clock(), f.h.ids.clone())),
        Arc::new(NativeFilesystem::new()),
        f.h.clock(),
        f.h.ids.clone(),
    ));
    let connections = Arc::new(ConnectionService::new(
        f.h.ids.clone(),
        f.h.clock(),
        f.store.clone(),
        Arc::new(NoNetwork),
        Arc::new(SessionSecrets::new()),
        Arc::new(Entropy),
    ));
    let usage = UsageRuntime::new(f.store.clone(), f.service.clone(), Some(router.clone()));
    let schemas = Arc::new(RegisteredSchemas::new(
        f.store.clone(),
        f.use_cases.clone().with_router(router.clone()),
        None,
        artifacts.clone(),
    ));
    let http = Arc::new(CountHttp(AtomicUsize::new(0)));
    let driver = NativeProviders::new(
        f.store.clone(),
        artifacts.clone(),
        connections,
        usage.clone(),
        schemas,
        ProviderComposition::new(http.clone()),
    );
    (artifacts, usage, http, driver)
}

fn request(text: &str) -> PackageInferenceRequest {
    PackageInferenceRequest::parse(
        json!({"messages": [{"role": "user", "text": text}], "maxTokens": 64})
            .to_string()
            .as_bytes(),
    )
    .unwrap()
}

#[test]
fn feature_budget_refuses_package_inference_before_transport_and_bills_no_attempt() {
    let _native_case = native_runtime_case();
    runtime().block_on(within("package inference feature budget", async {
        let f = Fx::new();
        let router = router(&f);
        applied(&f.start(&f.user, "root-f", 1, 1));
        let activation = f.launch("native-provider-owner");
        let started = f.command(
            &f.agent,
            "sessions.start",
            json!({"mode":"Command",
            "protocol":PROTOCOL,"requestKey":f.key(),"body":{"kind":"sessions.start",
            "scope":{"kind":"Project","projectId":"project-a"},"rootId":"root-f",
            "title":"Package feature session"}}),
        );
        let value = serde_json::to_value(started).unwrap();
        let result = &value["receipt"]["decision"]["result"]["value"];
        let session = id(result["sessionId"]
            .as_str()
            .or(result["id"].as_str())
            .unwrap());
        let registered = session.clone();
        let assignment = f.tx(move |svc, tx| {
            let host = tx.store().host_actor.clone();
            Ok(svc
                .register_assignment(
                    tx,
                    &host,
                    NewRunAssignment {
                        root: RootId(id("root-f")),
                        work: RootId(id("root-f")),
                        parent: None,
                        session: registered,
                        scope_key: None,
                    },
                )?
                .unwrap())
        });
        let (own, active) = (assignment.clone(), activation.clone());
        f.tx(move |svc, tx| {
            svc.begin_turn(tx, &own.session_id, &own.id, &active.id, active.owner_epoch)
                .map(|turn| turn.unwrap())
        });
        let key = ToolCallKey {
            attempt_id: devworks_core::protocol::AttemptId(id("provider-attempt")),
            native_call_id: "call-package".into(),
            ordinal: devworks_core::protocol::Counter::new(0).unwrap(),
        };
        let fenced = f.tx(|_, tx| {
            Ok(tx
                .authority()
                .principal(&ActorId(id("agent-a")))?
                .unwrap()
                .revision)
        });
        let fences = vec![AuthorityFence::Principal {
            actor: ActorId(id("agent-a")),
            revision: fenced,
        }];
        // The dispatching tool call the package tool consumer journals for an agent's call.
        let payload = json!({"packageId":"summarizer","contributionId":"summarizer/run",
            "principal":"agent-a","principalKind":"Agent","sessionId":session,
            "projectId":"project-a","rootId":"root-f","assignmentId":null,"fences":fences,
            "call":key,"activation":null});
        let pinned = session.clone();
        f.tx(move |_, tx| {
            tx.router().put_session_pin(
                &StoredPin {
                    session_id: pinned,
                    pin_id: id("package-pin"),
                    revision: 1,
                    model_id: id("usage-model"),
                    connection_id: id("usage-connection"),
                    upstream_model_id: Some("native-metadata-source".into()),
                    endpoint_profile: Some("CompatibleChat".into()),
                    identity_origin: "Resolved".into(),
                    intent: serde_json::to_string(&RouteIntent::Model {
                        model_id: id("usage-model"),
                    })
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
            let operation = OperationId(id("package-tool-call"));
            let intent = tx.operations().record_intent(NewOperation {
                id: operation.clone(),
                root_id: Some(RootId(id("root-f"))),
                assignment_id: None,
                capability: "package.invoke".into(),
                schema_version: 1,
                args_digest: "a".repeat(64),
                payload,
            })?;
            let attempt = tx.operations().begin_attempt(
                &operation,
                intent.revision,
                AttemptBinding::default(),
            )?;
            let preflighted = tx.operations().operation(&operation)?.unwrap();
            tx.operations().transition(
                &operation,
                preflighted.revision,
                Some(&attempt.fence()),
                OperationState::Dispatching,
                None,
            )?;
            let _ = attempt;
            Ok(())
        });
        let grant = grant_for(&*f.store, &OperationId(id("package-tool-call")))
            .expect("the feature agent's tool call");
        let (artifacts, usage, http, driver) = compose(&f, &router);
        let run = |action: &'static str| {
            let (store, artifacts, driver, router, grant) = (
                f.store.clone(),
                artifacts.clone(),
                driver.clone(),
                router.clone(),
                grant.clone(),
            );
            async move {
                infer(
                    store,
                    artifacts,
                    driver,
                    router,
                    &grant,
                    action,
                    &request("Summarize"),
                    None,
                )
                .await
            }
        };

        // Without a cap the call reaches the provider transport once (the fixture transport
        // then fails before sending anything).
        assert!(run("first").await.is_err());
        assert_eq!(http.0.load(Ordering::SeqCst), 1);
        let attempts = |f: &Fx| {
            read(&*f.store, |s| {
                s.usage()
                    .inference_attempts(&UsageAttemptFilter::default(), None, 64)
            })
            .len()
        };
        assert_eq!(attempts(&f), 1);

        // A reached cap holds the feature: admission refuses the package's inference before
        // transport, against the feature the invoking agent works on.
        limit(
            &f,
            FeatureBudgetInput::Limit {
                micro_usd: devworks_core::protocol::Counter::new(0).unwrap(),
            },
        );
        let planned = McpSampling::for_package(
            f.store.clone(),
            artifacts.clone(),
            driver.clone(),
            router.clone(),
            "second".into(),
        )
        .plan(&grant, &request("Summarize").sampling_call().unwrap())
        .expect("planned package inference");
        assert_eq!(planned.attempt.work_id, Some(id("root-f")));
        let produced = driver.produce_cancellable(planned, None).await.unwrap();
        let refusal = format!("{:?}", produced.accounting);
        assert!(
            matches!(produced.accounting, DriveOutcome::Refused(_))
                && refusal.contains("BudgetContinue"),
            "{refusal}"
        );
        assert!(run("third").await.is_err());
        assert_eq!(http.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            attempts(&f),
            1,
            "no usage attempt is billed for refused package inference"
        );
        usage.stop();
        usage.drain().await;
    }));
}

#[test]
fn feature_budget_refuses_a_persons_package_inference_before_transport() {
    let _native_case = native_runtime_case();
    runtime().block_on(within("package inference person budget", async {
        let f = Fx::new();
        let router = router(&f);
        applied(&f.start(&f.user, "root-f", 1, 1));
        // The person's invocation has no session pin: it routes through the High tier pool.
        f.tx(|_, tx| {
            let key = "routing.tierHigh.members";
            let schema = devworks_core::settings::catalog::descriptor(key)
                .unwrap()
                .schema_ref();
            tx.configuration()
                .append_revision(
                    &devworks_core::ports::storage::ConfigScope::Workspace,
                    "settings",
                    1,
                    1,
                    json!({"values": {key: {"schema": schema, "value": ["usage-model"]}}}),
                )
                .map(|_| ())
        });
        let (artifacts, usage, http, driver) = compose(&f, &router);
        let person = f.user.clone().with_lineage(
            Some(devworks_core::protocol::ProjectId(id("project-a"))),
            Some(RootId(id("root-f"))),
            None,
        );
        let grant = person_grant(&*f.store, &person, &OperationId(id("person-invocation")))
            .expect("the person's grant for the feature's work item");
        let run = |action: &'static str| {
            let (store, artifacts, driver, router, grant) = (
                f.store.clone(),
                artifacts.clone(),
                driver.clone(),
                router.clone(),
                grant.clone(),
            );
            async move {
                infer(
                    store,
                    artifacts,
                    driver,
                    router,
                    &grant,
                    action,
                    &request("Summarize"),
                    None,
                )
                .await
            }
        };
        let attempts = |f: &Fx| {
            read(&*f.store, |s| {
                s.usage()
                    .inference_attempts(&UsageAttemptFilter::default(), None, 64)
            })
        };

        // Without a cap the call reaches the provider transport once (the fixture transport
        // then fails before sending anything), billed to the person against the feature.
        assert!(run("first").await.is_err());
        assert_eq!(http.0.load(Ordering::SeqCst), 1);
        let first = attempts(&f);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].actor_id, ActorId(id("user-a")));
        assert_eq!(first[0].work_id, Some(id("root-f")));

        limit(
            &f,
            FeatureBudgetInput::Limit {
                micro_usd: devworks_core::protocol::Counter::new(0).unwrap(),
            },
        );
        let planned = McpSampling::for_package(
            f.store.clone(),
            artifacts.clone(),
            driver.clone(),
            router.clone(),
            "second".into(),
        )
        .plan(&grant, &request("Summarize").sampling_call().unwrap())
        .expect("planned package inference");
        assert_eq!(planned.attempt.work_id, Some(id("root-f")));
        let produced = driver.produce_cancellable(planned, None).await.unwrap();
        let refusal = format!("{:?}", produced.accounting);
        assert!(
            matches!(produced.accounting, DriveOutcome::Refused(_))
                && refusal.contains("BudgetContinue"),
            "{refusal}"
        );
        assert!(run("third").await.is_err());
        assert_eq!(http.0.load(Ordering::SeqCst), 1);
        assert_eq!(attempts(&f).len(), 1);
        usage.stop();
        usage.drain().await;
    }));
}
