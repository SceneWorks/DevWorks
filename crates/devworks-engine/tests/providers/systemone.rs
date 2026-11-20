//! Decisions go through the native gateway, queue, router and usage ledger. HTTP uses scripted bytes.
use super::*;
use devworks_core::protocol::OperationId;
use devworks_core::protocol::records::DecisionCatalogModel;
use devworks_core::protocol::results::{
    DomainDecision, DomainQueryReply, OperationCompletion, OperationResult,
};
use devworks_core::protocol::shared::CommandReply;

const RESPONSE: &str = r#"{"model":"fixture-model","answers":{"category":{"type":"choice","choice":"code","probabilities":{"code":0.9,"other":0.1},"confidence":0.9}},"usage":{"input_tokens":7,"output_tokens":0}}"#;

fn media_decision_frame() -> Vec<u8> {
    let mut frame: Value =
        serde_json::from_slice(&request_frame("media-decision", Some("decision-a"))).unwrap();
    frame["schemaRevision"] = json!(4);
    let questions = frame["body"]["questions"].as_array_mut().unwrap();
    questions.extend([
        json!({"name":"found","question":{"kind":"Noul","instructions":"Is it present?"}}),
        json!({"name":"quality","question":{"kind":"Score","instructions":"How useful?","criteria":["low","high"]}}),
        json!({"name":"locate","question":{"kind":"Box","instructions":"Locate it","instructionParts":[
            {"type":"image_url","image_url":{"url":"data:image/png;base64,YQ=="}},
            {"type":"video_url","video_url":{"url":"data:video/mp4;base64,Yg=="}}
        ]}}),
    ]);
    serde_json::to_vec(&frame).unwrap()
}

fn request_frame(key: &str, exact: Option<&str>) -> Vec<u8> {
    let mut frame = json!({
        "mode":"Command", "protocol":"1.0-draft.1", "requestKey":key,
        "body":{"kind":"router.decide","state":{"prompt":"Classify this"},"questions":[{"name":"category","question":{"kind":"Choice","instructions":"Which?","criteria":[{"name":"code","description":"Writing code"},{"name":"other"}]}}],"images":[],"videos":[],"timeoutMs":"10000"}
    });
    if let Some(model) = exact {
        frame["body"]["modelId"] = json!(model);
        frame["body"]["expectedModelRevision"] = json!("1");
    } else {
        frame["schemaRevision"] = json!(4);
        frame["body"]["usePool"] = json!(true);
    }
    serde_json::to_vec(&frame).unwrap()
}

fn accepted(app: &devworks_engine::Runtime, caller: &InvocationContext, raw: &[u8]) -> OperationId {
    match app.command(caller, "router.decide", raw).unwrap() {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("expected a decision operation, got {other:?}"),
        },
        other => panic!("expected a decision receipt, got {other:?}"),
    }
}

fn catalog(
    app: &devworks_engine::Runtime,
    caller: &InvocationContext,
    include_disabled: bool,
) -> Vec<DecisionCatalogModel> {
    let frame = json!({"mode":"Query", "protocol":"1.0-draft.1", "schemaRevision":4,
        "queryId":"decision-catalog", "body":{"kind":"router.decision_models",
        "includeDisabled":include_disabled,"page":{"limit":20}}});
    match app
        .query(
            caller,
            "router.decision_models",
            &serde_json::to_vec(&frame).unwrap(),
        )
        .unwrap()
    {
        DomainQueryReply::Result {
            result: OperationResult::RouterDecisionModels { value },
            ..
        } => value.items.0,
        other => panic!("expected the decision catalog, got {other:?}"),
    }
}

async fn terminal(
    app: &devworks_engine::Runtime,
    caller: &InvocationContext,
    operation: &OperationId,
) -> OperationCompletion {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            if let Some(result) = app.systemone_completion(caller, operation).unwrap() {
                return result;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("decision queue deadline")
}

async fn completion(
    app: &devworks_engine::Runtime,
    caller: &InvocationContext,
    operation: &OperationId,
) -> devworks_core::protocol::records::DecisionResult {
    match terminal(app, caller, operation).await {
        OperationCompletion::Succeeded {
            result: OperationResult::RouterDecide { value },
            ..
        } => value,
        failed => panic!("expected completed decision, got {failed:?}"),
    }
}

#[test]
fn explicit_and_pooled_decisions_share_native_transport_replay_and_accounting() {
    runtime().block_on(async {
        let f = Fx::new("SystemOne", Script::aggregate(RESPONSE), true);
        f.http.lock().unwrap().repeat_body = Some(RESPONSE.as_bytes().to_vec());
        write(f.store.as_ref(), |tx| {
            let schema = devworks_core::providers::systemone::result_reference();
            let mut connection = tx.router().connection(&id("connection"))?.unwrap();
            connection.discovered.as_mut().unwrap().push(DiscoveredModel { id: "fixture-model-b".into(), context_tokens: None });
            tx.router().put_connection(&connection, Some(connection.revision))?;
            for id_name in ["decision-a", "decision-b"] {
                tx.router().put_decision_model(&StoredDecisionModel {
                    id: id(id_name), display_name: id_name.into(), connection_id: id("connection"),
                    upstream_model_id: if id_name == "decision-a" { "fixture-model" } else { "fixture-model-b" }.into(),
                    question_types: ["choice", "noul", "score", "box"].into_iter().map(str::to_owned).collect(),
                    accepts_images: id_name == "decision-a", accepts_videos: id_name == "decision-a", enabled: true,
                    extension_schemas: "[]".into(), result_schema: serde_json::to_string(&schema).unwrap(),
                    decision_policy: Some(json!({"poolMember":true,"maxInputBytes":"8192","maxQuestionCount":"4","maxBatchBytes":"16384"}).to_string()),
                    revision: 1, updated_at_ms: T0,
                }, None)?;
            }
            Ok(())
        }).unwrap();
        let compose = || {
            let mut comp = tool_runtime_composition(&f);
            comp.config.providers = Some(ProviderComposition::new(f.http.clone()).with_systemone_validator(Arc::new(devworks_adapters::packages::JsonSchemaValidator)));
            let credential = devworks_core::services::router::connections::auth::Credential::ApiKey(Secret::new("actual-fixture-key"));
            comp.application.connections.as_ref().unwrap().secrets.put(
                &SecretKey::new(f.store.info().store_id.0.as_str(), id("credential")), &credential.to_secret(),
            ).unwrap();
            comp
        };
        let app = devworks_engine::boot(compose()).await.unwrap();
        let caller = principal(f.store.as_ref(), "decision-user", PrincipalKind::Human);
        let explicit = request_frame("explicit-decision", Some("decision-b"));
        let op = accepted(&app, &caller, &explicit);
        let result = completion(&app, &caller, &op).await;
        assert_eq!(result.model_id, id("decision-b"));
        assert_eq!(result.output.value.0["answers"]["category"]["choice"], "code");
        assert_eq!(result.usage.items.0[0].input_tokens.get(), 7);
        assert_eq!(result.usage.items.0[0].amount_usd.as_ref().unwrap().0, "0");
        assert_eq!(accepted(&app, &caller, &explicit), op);
        assert_eq!(read(f.store.as_ref(), |read| read.router().decision_pool_cursor()), None);

        let first = request_frame("first-pool-decision", None);
        let op = accepted(&app, &caller, &first);
        assert_eq!(completion(&app, &caller, &op).await.model_id, id("decision-a"));
        app.shutdown().await;
        let app = devworks_engine::boot(compose()).await.unwrap();
        assert_eq!(accepted(&app, &caller, &first), op);
        assert_eq!(completion(&app, &caller, &op).await.model_id, id("decision-a"));

        let second = request_frame("second-pool-decision", None);
        let op = accepted(&app, &caller, &second);
        assert_eq!(completion(&app, &caller, &op).await.model_id, id("decision-b"));
        let (left, right) = std::thread::scope(|scope| {
            let left = scope.spawn(|| accepted(&app, &caller, &request_frame("pool-concurrent-left", None)));
            let right = scope.spawn(|| accepted(&app, &caller, &request_frame("pool-concurrent-right", None)));
            (left.join().unwrap(), right.join().unwrap())
        });
        let (left, right) = tokio::join!(
            completion(&app, &caller, &left),
            completion(&app, &caller, &right)
        );
        let mut allocated = [left.model_id, right.model_id];
        allocated.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        assert_eq!(allocated, [id("decision-a"), id("decision-b")]);
        assert_eq!(read(f.store.as_ref(), |read| read.router().decision_pool_cursor()), Some(id("decision-b")));
        assert!(read(f.store.as_ref(), |read| read.router().session_pin(&id("session"))).is_none());
        let refresh = |available| write(f.store.as_ref(), move |tx| {
            let mut connection = tx.router().connection(&id("connection"))?.unwrap();
            let mut models = vec![DiscoveredModel { id: "fixture-model-b".into(), context_tokens: None }];
            if available { models.push(DiscoveredModel { id: "fixture-model".into(), context_tokens: None }); }
            connection.discovered = Some(models);
            tx.router().put_connection(&connection, Some(connection.revision))?;
            Ok(())
        }).unwrap();
        refresh(false);
        assert_eq!(catalog(&app, &caller, false)[0].id, id("decision-b"));
        let retained = catalog(&app, &caller, true);
        assert_eq!(retained.len(), 2);
        assert_eq!(retained[0].id, id("decision-a"));
        assert!(retained[0].enabled);
        assert_eq!(retained[0].available, Some(false));
        let frames = app.command(&caller, "router.decide", &request_frame("missing-explicit-decision", Some("decision-a"))).unwrap();
        let CommandReply::Recorded { receipt, .. } = frames else { panic!("missing decision model was not durably refused") };
        assert!(matches!(receipt.decision, DomainDecision::Refused { .. }));
        assert!(read(f.store.as_ref(), |read| read.router().decision_model(&id("decision-a"))).unwrap().enabled);
        let op = accepted(&app, &caller, &request_frame("skip-missing", None));
        assert_eq!(completion(&app, &caller, &op).await.model_id, id("decision-b"));
        refresh(true);
        assert_eq!(catalog(&app, &caller, false).len(), 2);
        let op = accepted(&app, &caller, &request_frame("after-return", None));
        assert_eq!(completion(&app, &caller, &op).await.model_id, id("decision-a"));
        f.http.lock().unwrap().statuses.extend([402, 200]);
        let op = accepted(&app, &caller, &request_frame("fallback-after-billing", None));
        assert_eq!(completion(&app, &caller, &op).await.model_id, id("decision-a"));
        assert_eq!(read(f.store.as_ref(), |read| read.router().decision_pool_cursor()), Some(id("decision-b")));
        let result = app.command(&caller, "router.decide", &request_frame("billing-blocks-explicit", Some("decision-b"))).unwrap();
        let CommandReply::Recorded { receipt, .. } = result else { panic!("billing rejection should be recorded") };
        assert!(matches!(receipt.decision, DomainDecision::Refused { .. }));
        f.http.lock().unwrap().statuses.push_back(422);
        let raw = request_frame("invalid-at-provider", Some("decision-a"));
        let op = accepted(&app, &caller, &raw);
        let OperationCompletion::Failed { fault, .. } = terminal(&app, &caller, &op).await
        else { panic!("expected a typed failure for a provider-side input rejection") };
        assert_eq!(fault.code, devworks_core::protocol::DomainErrorCode::InvalidArgument);
        assert_eq!(accepted(&app, &caller, &raw), op);
        let other = principal(f.store.as_ref(), "other-decision-user", PrincipalKind::Human);
        assert!(app.systemone_completion(&other, &op).is_err());

        let mut media_response: Value = serde_json::from_str(RESPONSE).unwrap();
        media_response["answers"]["found"] = json!({"type":"noul","noul":0.85});
        media_response["answers"]["quality"] = json!({"type":"score","score":0.7,"confidence":0.7,
            "legend":{"0":"low","1":"high"},"probabilities":{"0":0.3,"1":0.7}});
        media_response["answers"]["locate"] = json!({"type":"box","box":{"left":0.2,"right":0.8,"top":0.1,"bottom":0.9},"confidence":0.8});
        f.http.lock().unwrap().repeat_body = Some(serde_json::to_vec(&media_response).unwrap());
        let op = accepted(&app, &caller, &media_decision_frame());
        let result = completion(&app, &caller, &op).await;
        assert_eq!(result.output.value.0, media_response);

        let waiting = {
            let mut script = f.http.lock().unwrap();
            script.wait_for_cancel = true;
            script.repeat_body = Some(RESPONSE.as_bytes().to_vec());
            script.waiting.clone()
        };
        let raw = request_frame("cancel-after-send", Some("decision-a"));
        let op = accepted(&app, &caller, &raw);
        tokio::time::timeout(std::time::Duration::from_secs(15), waiting.notified())
            .await.expect("scripted transport reached the send boundary");
        let pending = read(f.store.as_ref(), |read| read.operations().operation(&op)).unwrap();
        let pending: devworks_core::services::router::decisions::DecisionIntent =
            serde_json::from_value(pending.payload).unwrap();
        let delivery = pending.delivery_id;
        write(f.store.as_ref(), move |tx| tx.queue().cancel(&delivery)).unwrap();
        app.dispatcher().wakeup().wake();
        assert!(matches!(
            terminal(&app, &caller, &op).await,
            OperationCompletion::OutcomeUnknown { .. }
        ));
        assert_eq!(accepted(&app, &caller, &raw), op);
        app.shutdown().await;
        let requests = f.http.lock().unwrap().requests.clone();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 12);
        let mut models = Vec::new();
        for request in requests.iter() {
            assert_eq!(request.url, "https://provider.test/v1/systemone");
            assert_eq!(request.header_value("Authorization"), Some("Bearer actual-fixture-key"));
            let body: Value = serde_json::from_slice(request.body.as_ref().unwrap()).unwrap();
            models.push(body["model"].as_str().unwrap().to_owned());
            assert_eq!(body["questions"]["category"]["criteria"]["other"], Value::Null);
        }
        assert_eq!(models[..3], ["fixture-model-b", "fixture-model", "fixture-model-b"]);
        models[3..5].sort();
        assert_eq!(models[3..], ["fixture-model", "fixture-model-b", "fixture-model-b", "fixture-model", "fixture-model-b", "fixture-model", "fixture-model", "fixture-model", "fixture-model"]);
        let media_body: Value = serde_json::from_slice(requests[10].body.as_ref().unwrap()).unwrap();
        assert_eq!(media_body["questions"]["quality"]["criteria"], json!(["low","high"]));
        assert_eq!(media_body["questions"]["locate"]["instructions"][1]["image_url"]["url"], "data:image/png;base64,YQ==");
        assert_eq!(media_body["questions"]["locate"]["instructions"][2]["video_url"]["url"], "data:video/mp4;base64,Yg==");
    });
}
