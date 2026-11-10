//! A sample made from a feature agent's tool call is admitted against that feature's budget.
use super::*;
use devworks_adapters::fs::{
    NativeFilesystem, artifacts::NativeArtifactFiles, sources::HostArtifactSources,
};
use devworks_core::ports::mcp::{SamplingCall, SamplingMessage};
use devworks_core::ports::{
    AuthorityFence, http::NoNetwork, provider_http::*, secrets::SessionSecrets,
};
use devworks_core::protocol::provider::ToolCallKey;
use devworks_core::protocol::records::FeatureBudgetInput;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::mcp::SamplingGrant;
use devworks_core::services::router::connections::ConnectionService;
use devworks_core::services::usage::budget;
use devworks_engine::mcp_sampling::McpSampling;
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

fn call(ordinal: u8) -> SamplingCall {
    SamplingCall {
        ordinal,
        messages: vec![SamplingMessage {
            assistant: false,
            text: "Summarize the change".into(),
        }],
        system_prompt: None,
        max_tokens: 64,
        temperature: None,
        stop_sequences: vec![],
    }
}

#[test]
fn feature_budget_refuses_a_sample_before_transport_and_bills_no_attempt() {
    let _native_case = native_runtime_case();
    runtime().block_on(within("sampling feature budget", async {
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
            "title":"Sampling feature session"}}),
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
            native_call_id: "call-sample".into(),
            ordinal: devworks_core::protocol::Counter::new(0).unwrap(),
        };
        let payload = json!({"principal":"agent-a","principalKind":"Agent",
            "sessionId":session,"call":key});
        let pinned = session.clone();
        let (journal, principal) = f.tx(move |_, tx| {
            tx.router().put_session_pin(
                &StoredPin {
                    session_id: pinned,
                    pin_id: id("sampling-pin"),
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
            let operation = OperationId(id("mcp-tool-call"));
            let intent = tx.operations().record_intent(NewOperation {
                id: operation.clone(),
                root_id: Some(RootId(id("root-f"))),
                assignment_id: None,
                capability: "mcp.tool".into(),
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
            let binding = tx.authority().principal(&ActorId(id("agent-a")))?.unwrap();
            Ok((attempt.id.0, binding.revision))
        });
        let grant = SamplingGrant {
            context: f
                .agent
                .clone()
                .with_lineage(
                    Some(ProjectId(id("project-a"))),
                    Some(RootId(id("root-f"))),
                    None,
                )
                .with_session(session.clone()),
            operation_id: OperationId(id("mcp-tool-call")),
            attempt_id: journal,
            call: key,
            fences: vec![AuthorityFence::Principal {
                actor: ActorId(id("agent-a")),
                revision: principal,
            }],
        };
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
        let sampling = McpSampling::new(f.store.clone(), artifacts, driver.clone(), router);

        // Without a cap the owner reaches the provider transport once.
        assert!(sampling.sample(&grant, &call(1), None).await.is_err());
        assert_eq!(http.0.load(Ordering::SeqCst), 1);
        let attempts = |f: &Fx| {
            read(&*f.store, |s| {
                s.usage()
                    .inference_attempts(&UsageAttemptFilter::default(), None, 64)
            })
            .len()
        };
        assert_eq!(attempts(&f), 1);

        // A reached cap holds the feature: admission refuses the sample before transport.
        limit(
            &f,
            FeatureBudgetInput::Limit {
                micro_usd: devworks_core::protocol::Counter::new(0).unwrap(),
            },
        );
        let planned = sampling.plan(&grant, &call(2)).expect("planned sample");
        assert_eq!(planned.attempt.work_id, Some(id("root-f")));
        let produced = driver.produce_cancellable(planned, None).await.unwrap();
        let refusal = format!("{:?}", produced.accounting);
        assert!(
            matches!(produced.accounting, DriveOutcome::Refused(_))
                && refusal.contains("BudgetContinue"),
            "{refusal}"
        );
        assert!(sampling.sample(&grant, &call(3), None).await.is_err());
        assert_eq!(http.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            attempts(&f),
            1,
            "no usage attempt is billed for a refused sample"
        );
        usage.stop();
        usage.drain().await;
    }));
}
