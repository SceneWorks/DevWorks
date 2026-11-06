use super::*;
fn qualified(f: &mut Fx, levels: &[&str]) {
    let model = f.model.clone();
    let names = levels
        .iter()
        .filter(|n| **n != "none")
        .map(|n| {
            match *n {
                "high" => "High",
                "xhigh" => "XHigh",
                "max" => "Max",
                "minimal" => "Minimal",
                _ => "Low",
            }
            .to_string()
        })
        .collect::<Vec<_>>();
    write(f.store.as_ref(), move |tx| {
        let mut model = tx.router().route_model(&model)?.unwrap();
        let expected = model.revision;
        model.revision += 1;
        model.efforts = names;
        tx.router().put_route_model(&model, Some(expected))
    })
    .unwrap();
    let mut caps = Capabilities::unknown(NativeBinding {
        connection_id: id("connection"),
        account_revision: Revision::new(1).unwrap(),
        endpoint_profile: serde_json::from_value(json!(f.profile)).unwrap(),
        model_id: f.model.clone(),
        context_epoch: Counter::new(0).unwrap(),
    });
    caps.thinking = Support::Supported;
    caps.efforts = levels.iter().map(|n| n.to_string()).collect();
    for level in levels {
        caps.effort_encodings.insert(
            level.to_string(),
            devworks_core::providers::capabilities::EffortEncoding::ResponsesReasoning {
                value: level.to_string(),
            },
        );
    }
    let mut composition = ProviderComposition::new(f.http.clone());
    composition.capabilities.push(caps);
    f.driver = NativeProviders::new(
        f.store.clone(),
        f.artifacts.clone(),
        f.connections.clone(),
        f.usage.clone(),
        f.driver.schemas().clone(),
        composition,
    );
}
fn thinking(f: &Fx, thinking: Thinking) -> InferenceRequest {
    let mut request = f.generation(vec![]);
    if let InferenceInput::Generation { context, .. } = &mut request.input {
        let bytes = f
            .artifacts
            .read_exact(f.store.as_ref(), &f.context, context, 8 * 1024 * 1024)
            .unwrap()
            .0;
        let mut material: ProviderContext = serde_json::from_slice(&bytes).unwrap();
        material.parameters.thinking = Some(thinking);
        *context = f.bytes(&serde_json::to_vec(&material).unwrap());
    }
    request
}
#[test]
fn exact_declared_native_effort_matches_request_wire_and_accounting_before_admission() {
    runtime().block_on(async{
for (thinking_value,level) in [(Thinking::Enabled{budget_tokens:20000},"high"),(Thinking::Enabled{budget_tokens:40000},"xhigh"),(Thinking::Enabled{budget_tokens:70000},"max"),(Thinking::Disabled,"none")] {
let mut f=Fx::new("OpenAiResponses",Script::aggregate(r#"{"id":"response","model":"fixture-model","status":"completed","output":[],"usage":{"input_tokens":10,"output_tokens":2}}"#),false);qualified(&mut f,&[level]);let request=thinking(&f,thinking_value);let outcome=f.produce(request.clone()).await.unwrap();assert_eq!(outcome.result.outcome,Some(AttemptEnd::Completed));let rows=read(f.store.as_ref(),|s|s.usage().inference_attempts(&UsageAttemptFilter::default(),None,64));let captured=devworks_core::services::usage::accounting::snapshot(&rows[0]).unwrap();let AttemptSelection::NativeGeneration{requested_effort,effective_effort,..}=&captured.selection else {panic!("native effort metadata missing")};assert_eq!(requested_effort.as_deref(),Some(level));assert_eq!(effective_effort.as_deref(),Some(level));let sent:Value=serde_json::from_slice(f.http.lock().unwrap().requests.lock().unwrap()[0].body.as_ref().unwrap()).unwrap();assert_eq!(sent["reasoning"]["effort"],level);assert!(matches!(f.produce(request).await.unwrap().accounting,DriveOutcome::AlreadyDispatched));
}
let f=Fx::new("OpenAiResponses",Script::aggregate("{}"),false);assert!(f.produce(thinking(&f,Thinking::Enabled{budget_tokens:20000})).await.is_err());assert!(f.http.lock().unwrap().requests.lock().unwrap().is_empty());assert!(read(f.store.as_ref(),|s|s.usage().inference_attempts(&UsageAttemptFilter::default(),None,64)).is_empty());
});
}

fn prices(f: &Fx) {
    prices_for(f, "Standard", None)
}
fn prices_for(f: &Fx, tier: &str, region: Option<&str>) {
    write(f.store.as_ref(), |tx| {
        let mut c = tx.router().connection(&id("connection"))?.unwrap();
        let expected = c.revision;
        c.local_free = false;
        tx.router().put_connection(&c, Some(expected)).map(|_| ())
    })
    .unwrap();
    let user = principal(f.store.as_ref(), "price-human", PrincipalKind::Human);
    let tariff = |modality: &str, kind: &str, amount: &str, ttl: Option<i64>| {
        let mut row = json!({"upstreamModelId":"fixture-model","serviceTier":tier,"modality":modality,"chargeKind":kind,"unit":"Token","units":"1000000","amountUsd":amount});
        if let Some(region) = region {
            row["region"] = json!(region);
        }
        if let Some(ttl) = ttl {
            row["cacheTtlSeconds"] = json!(ttl.to_string());
        }
        row
    };
    let tariffs = vec![
        tariff("Text", "Input", "1", None),
        tariff("Text", "Output", "2", None),
        tariff("Text", "CacheRead", "0.5", None),
        tariff("Text", "CacheWrite", "1.25", Some(300)),
        tariff("Text", "CacheWrite", "1.5", Some(3600)),
        tariff("Audio", "Input", "3", None),
        tariff("Audio", "Output", "4", None),
    ];
    let raw=serde_json::to_vec(&json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":3,"requestKey":"native-price-override","body":{"kind":"router.price_override","modelId":f.model,"expectedRevision":"1","change":{"kind":"Set","tariffs":tariffs}}})).unwrap();
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
    let reply = UseCases::new()
        .with_prices(Arc::new(
            devworks_core::services::prices::PriceService::new(f.h.ids.clone(), f.h.clock(), false),
        ))
        .command(&gateway, &user, "router.price_override", &raw)
        .unwrap();
    assert!(
        matches!(reply,devworks_core::protocol::CommandReply::Recorded{receipt,..}
if matches!(receipt.decision,devworks_core::protocol::results::DomainDecision::Applied{..})),
        "actual Human override must persist"
    );
}
#[test]
fn native_observed_usage_uses_real_quotes_exact_cost_and_preserves_unknown_input_semantics() {
    runtime().block_on(async {
let messages="event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"fixture-model\",\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":4,\"cache_creation_input_tokens\":6,\"cache_creation\":{\"ephemeral_5m_input_tokens\":2,\"ephemeral_1h_input_tokens\":4}}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
for (profile,script,expected,complete,semantics) in [
("AnthropicMessages",Script::sse(messages),Some(25),true,InputSemantics::Disjoint),
("OpenAiResponses",Script::aggregate(r#"{"id":"r","model":"fixture-model","status":"completed","output":[],"usage":{"input_tokens":20,"output_tokens":2,"input_tokens_details":{"cached_tokens":4}}}"#),Some(22),true,InputSemantics::Inclusive),
("CompatibleChat",Script::aggregate(r#"{"model":"fixture-model","choices":[{"index":0,"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":20,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":4}}}"#),Some(6),false,InputSemantics::Unknown),
("OpenRouterChat",Script::aggregate(r#"{"model":"fixture-model","choices":[{"index":0,"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":20,"completion_tokens":2,"cost":0.00000125}}"#),Some(1),true,InputSemantics::Inclusive),
("OpenAiResponses",Script::aggregate(r#"{"id":"r","model":"fixture-model","status":"completed","output":[],"usage":{"input_tokens":10,"output_tokens":4,"input_tokens_details":{"cached_tokens":0,"audio_tokens":4},"output_tokens_details":{"audio_tokens":1}}}"#),Some(28),true,InputSemantics::Inclusive),
("OpenAiResponses",Script::aggregate(r#"{"id":"r","model":"fixture-model","status":"completed","output":[],"usage":{"input_tokens":10,"output_tokens":4,"input_tokens_details":{"cached_tokens":4,"audio_tokens":4},"output_tokens_details":{"audio_tokens":1}}}"#),Some(10),false,InputSemantics::Inclusive),
] {
let f=Fx::new(profile,script,false);prices(&f);let modalities=if f.http.lock().unwrap().body.front().is_some_and(|b|String::from_utf8_lossy(b).contains("audio_tokens")){vec!["Text".into(),"Audio".into()]}else{vec!["Text".into()]};let result=f.produce_priced_modalities(f.generation(vec![]),PricingContext{input_semantics:InputSemantics::Unknown,cache_read_possible:profile!="OpenRouterChat",cache_write_possible:profile=="AnthropicMessages",cache_write_ttl_ms:None},modalities).await.unwrap();let DriveOutcome::Settled(balance)=result.accounting else{panic!("price-native source not settled")};assert_eq!(balance.priced_micro_usd,expected,"{profile}: {balance:?}");assert_eq!(balance.monetary_complete,complete,"{profile}: {balance:?}");let attempt=read(f.store.as_ref(),|s|s.usage().inference_attempt(&balance.attempt_id)).unwrap();let captured=devworks_core::services::usage::accounting::snapshot(&attempt).unwrap();assert_eq!(captured.override_revision,2);assert_eq!(captured.pricing.input_semantics,semantics);assert_eq!(captured.quote.classification,devworks_core::ports::usage::CostClassification::ConfiguredEstimate);if profile=="OpenRouterChat"{let summary=devworks_core::services::usage::accounting::summary(&balance).unwrap();let cost=summary.reported_cost.unwrap();assert_eq!(cost.amount,"0.00000125");assert_eq!(cost.currency,"USD");assert_eq!(balance.classification,"ReportedCharge");}
if profile=="CompatibleChat"{assert!(devworks_core::services::usage::accounting::summary(&balance).unwrap().unknown_components.iter().any(|s|s.contains("input")));}
}
});
}

#[test]
fn actual_response_dimensions_qualify_immutable_estimates_and_paid_charge_semantics() {
    runtime().block_on(async {
for (profile,reported_tier,reported_region,quoted_region,paid,known) in [
("OpenAiResponses",Some("default"),None,None,false,true),
("OpenAiResponses",Some("priority"),None,None,false,false),
("OpenAiResponses",Some("auto"),None,None,false,false),
("OpenAiResponses",None,Some("eu"),Some("us"),false,false),
("OpenAiResponses",None,Some("eu"),None,false,true),
("OpenRouterChat",Some("priority"),Some("eu"),Some("us"),true,true),
("CodexSubscriptionResponses",Some("priority"),None,None,true,false),
] {
let mut usage=json!({"input_tokens":10,"output_tokens":2,"prompt_tokens":10,"completion_tokens":2});if let Some(t)=reported_tier{usage["service_tier"]=json!(t);}
if let Some(r)=reported_region{usage["processing_region"]=json!(r);}
if paid {usage["cost"]=json!("0.000002");usage["currency"]=json!("USD");}
let body=if profile=="OpenRouterChat" {json!({"model":"fixture-model","choices":[{"index":0,"message":{"content":"ok"},"finish_reason":"stop"}],"usage":usage})}else{json!({"id":"r","model":"fixture-model","status":"completed","output":[],"usage":usage})};
let script=if profile=="CodexSubscriptionResponses"{Script::sse(&format!("event: response.completed\ndata: {}\n\n",json!({"type":"response.completed","response":body})))}else{Script::aggregate(&body.to_string())};let f=Fx::new(profile,script,false);prices_for(&f,"Standard",quoted_region);
let out=f.produce_dimensions(f.generation(vec![]),PricingContext{input_semantics:InputSemantics::Unknown,cache_read_possible:false,cache_write_possible:false,cache_write_ttl_ms:None},vec!["Text".into()],None,quoted_region.map(str::to_string),false).await.unwrap();let DriveOutcome::Settled(balance)=out.accounting else{panic!("dimension-native source not settled")};assert_eq!(balance.monetary_complete,known,"{profile} {reported_tier:?}/{reported_region:?}: {balance:?}");if known {assert_eq!(balance.priced_micro_usd,Some(if paid{2}else{14}));}else{assert_eq!(balance.priced_micro_usd,None);assert_eq!(balance.classification,"Untracked");}
let captured=read(f.store.as_ref(),|s|s.usage().inference_attempt(&balance.attempt_id)).unwrap();let original=devworks_core::services::usage::accounting::snapshot(&captured).unwrap();assert_eq!(original.query.service_tier,None);assert_eq!(original.query.processing_region.as_deref(),quoted_region);assert_eq!(original.quote.classification,if profile=="CodexSubscriptionResponses"{devworks_core::ports::usage::CostClassification::SubscriptionApiEquivalent}else{devworks_core::ports::usage::CostClassification::ConfiguredEstimate});assert_eq!(original.quote.override_revision,Some(2));let summary=devworks_core::services::usage::accounting::summary(&balance).unwrap();assert_eq!(summary.reported_service_tier.as_deref(),reported_tier);assert_eq!(summary.reported_processing_region.as_deref(),reported_region);if profile=="CodexSubscriptionResponses"{assert_eq!(captured.access_mode,"Subscription");assert_ne!(balance.classification,"ReportedCharge");}
}
});
}

#[test]
fn response_dimension_mismatch_is_sticky_but_later_paid_usage_corrects_the_same_attempt() {
    runtime().block_on(async {
for paid in [false,true] {
let chunks=[("default",1),("priority",2),("default",3)].into_iter().map(|(tier,output)|{let mut usage=json!({"prompt_tokens":10,"completion_tokens":output,"service_tier":tier});if paid&&output==3{usage["cost"]=json!("0.000004");usage["currency"]=json!("USD");}format!("data: {}\n\n",json!({"model":"fixture-model","choices":[{"index":0,"delta":{},"finish_reason":if output==3{Some("stop")}else{None}}],"usage":usage}))}).collect::<String>()+"data: [DONE]\n\n";
let f=Fx::new("OpenRouterChat",Script::sse(&chunks),false);prices(&f);let result=f.produce(f.generation(vec![])).await.unwrap();let DriveOutcome::Settled(balance)=result.accounting else {panic!("dimension correction not settled")};let summary=devworks_core::services::usage::accounting::summary(&balance).unwrap();assert_eq!(summary.reported_service_tier.as_deref(),Some("default"));assert_eq!(summary.pricing_dimension_mismatches.len(),1);if paid{assert_eq!(balance.priced_micro_usd,Some(4));assert_eq!(balance.classification,"ReportedCharge");assert!(balance.monetary_complete);}else{assert_eq!(balance.priced_micro_usd,None);assert_eq!(balance.classification,"Untracked");assert!(!balance.monetary_complete);}let rows=read(f.store.as_ref(),|s|s.usage().inference_attempts(&UsageAttemptFilter::default(),None,64));assert_eq!(rows.len(),1);let observed=read(f.store.as_ref(),|s|s.usage().usage_observations(&balance.attempt_id,None,64));assert_eq!(observed.len(),3);
}
});
}
