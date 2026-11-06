use super::*;
use devworks_core::services::router::{AuxiliaryRequest, accounting::AttemptSelection};

fn register(f: &Fx, upstream: &str) {
    let upstream = upstream.to_owned();
    write(f.store.as_ref(), move |tx| {
        let mut model = tx.router().route_model(&id("model"))?.unwrap();
        model.upstream_model_id = upstream.clone();
        model.revision += 1;
        tx.router()
            .put_route_model(&model, Some(model.revision - 1))?;
        let mut connection = tx.router().connection(&id("connection"))?.unwrap();
        connection.local_free = false;
        connection.discovered = Some(vec![DiscoveredModel {
            id: upstream,
            context_tokens: Some(10000),
        }]);
        let revision = connection.revision;
        connection.revision += 1;
        tx.router().put_connection(&connection, Some(revision))
    })
    .unwrap();
}
fn attempt(
    f: &Fx,
    request: &InferenceRequest,
    intent: devworks_core::protocol::records::RouteIntent,
) -> (RequestAdmission, AttemptRequest) {
    let router = InferenceRouter::new(f.h.ids.clone(), f.h.clock(), f.connections.clone());
    let route = read(f.store.as_ref(), |s| {
        router.route_auxiliary(
            s,
            &AuxiliaryRequest {
                purpose: Purpose::Conversation,
                session_id: Some(id("session")),
                intent: intent.clone(),
                effort: devworks_core::protocol::records::EffortName::Default,
                required_capabilities: vec![],
            },
        )
    })
    .unwrap();
    let query = PriceQuery {
        catalog_model_id: route.model_id.clone(),
        provider_model_id: None,
        connection_id: route.connection_id.clone(),
        endpoint_profile: route.endpoint_profile,
        access_mode: "Api".into(),
        service_tier: None,
        input_tokens: Some(10),
        cache_write_ttl_ms: None,
        processing_region: None,
        required_modalities: vec!["Text".into()],
    };
    let quote = read(f.store.as_ref(), |s| {
        devworks_core::services::prices::CatalogPriceResolver.quote(s, &query)
    });
    let logical = RequestAdmission {
        request_id: request.request_id.clone(),
        purpose: Purpose::Conversation,
        session_id: Some(id("session")),
        project_id: None,
        root_id: None,
        model_id: route.model_id,
        connection_id: route.connection_id,
        pin_id: None,
        args_digest: "a".repeat(64),
    };
    let request = AttemptRequest {
        admission_key: id("auto-admission"),
        request_id: request.request_id.clone(),
        actor_id: f.context.principal().clone(),
        work_id: None,
        job_run_id: None,
        role: "Host".into(),
        query,
        quote,
        price_generation: read(f.store.as_ref(), |s| s.usage().price_revision()),
        override_revision: 1,
        pricing: PricingContext {
            input_semantics: InputSemantics::Inclusive,
            cache_read_possible: false,
            cache_write_possible: false,
            cache_write_ttl_ms: None,
        },
        selection: AttemptSelection::Conversation {
            intent,
            effort: devworks_core::protocol::records::EffortName::Default,
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
    (logical, request)
}
#[test]
fn canonical_auto_and_auto_beta_routes_send_registered_upstream_then_record_actual_model_and_cost_once()
 {
    runtime().block_on(async {
        for upstream in ["openrouter/auto","openrouter/auto-beta"] {
            let f=Fx::new("OpenRouterChat",Script::sse("data: {\"id\":\"native-generation\",\"model\":\"vendor/actual-served\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"actual reply\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"cost\":0.00000125}}\n\ndata: [DONE]\n\n"),false);
            register(&f,upstream);
            let input=f.generation(vec![]);
            let (logical,request)=attempt(&f,&input,devworks_core::protocol::records::RouteIntent::OpenRouterAuto{});
            assert_eq!(request.query.provider_model_id,None);
            let outcome=f.driver.produce(f.context.clone(),logical.clone(),ReportCategory::Assistant,request.clone(),input.clone()).await.unwrap();
            let DriveOutcome::Settled(balance)=outcome.accounting else{panic!("not settled")};
            assert_eq!(outcome.result.outcome,Some(AttemptEnd::Completed));
            assert_eq!(balance.priced_micro_usd,Some(1));
            assert_eq!(balance.classification,"ReportedCharge");
            let stored=read(f.store.as_ref(),|s|s.usage().inference_attempt(&balance.attempt_id)).unwrap();
            let bound=devworks_core::services::usage::accounting::snapshot(&stored).unwrap();
            assert_eq!(bound.query.provider_model_id,None);
            assert_eq!(stored.provider_model_id.as_deref(),Some("vendor/actual-served"));
            let requests=f.http.lock().unwrap().requests.clone();
            let wire:Value=serde_json::from_slice(requests.lock().unwrap()[0].body.as_deref().unwrap()).unwrap();
            assert_eq!(wire["model"],upstream);
            let replay=f.driver.produce(f.context.clone(),logical,ReportCategory::Assistant,request,input).await.unwrap();
            assert!(matches!(replay.accounting,DriveOutcome::AlreadyDispatched));
            assert_eq!(requests.lock().unwrap().len(),1);
            assert_eq!(read(f.store.as_ref(),|s|s.usage().inference_attempts(&UsageAttemptFilter::default(),None,64)).len(),1);
        }
    });
}
#[test]
fn unresolved_serving_identity_is_only_qualified_for_exact_openrouter_auto_profiles() {
    runtime().block_on(async {
        for (profile, upstream) in [
            ("CompatibleChat", "openrouter/auto"),
            ("OpenAiResponses", "openrouter/auto-beta"),
            ("OpenRouterChat", "openrouter/auto:beta"),
        ] {
            let f = Fx::new(profile, Script::aggregate("{}"), false);
            register(&f, upstream);
            let input = f.generation(vec![]);
            let (logical, request) = attempt(
                &f,
                &input,
                devworks_core::protocol::records::RouteIntent::Model {
                    model_id: f.model.clone(),
                },
            );
            assert!(
                f.driver
                    .produce(
                        f.context.clone(),
                        logical,
                        ReportCategory::Assistant,
                        request,
                        input
                    )
                    .await
                    .is_err()
            );
            assert!(f.http.lock().unwrap().requests.lock().unwrap().is_empty());
        }
    });
}
