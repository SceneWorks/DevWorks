//! Closed core probe metadata, actual native codecs/source/sink/accounting and durable result.
//! Scripted response vectors are recorded values, never model execution.
use super::*;
use devworks_core::services::usage::accounting;
use devworks_core::{
    protocol::{
        OperationId, OperationState,
        data::{DataScope, Metric},
    },
    services::router::embedding_spaces::{self, EmbeddingSpaceService, ProbePreparation},
};
use devworks_engine::conventions::embedding::NativeEmbeddings;

fn source_context(ctx: &InvocationContext) -> Value {
    json!({"version":1,"storeId":ctx.store_id(),"restoreEpoch":ctx.restore_epoch(),"invocationContext":{
        "principal":ctx.principal(),"kind":ctx.principal_kind().as_str().to_string(),"invocation":ctx.invocation_id(),"namespace":ctx.ingress_namespace(),"session":ctx.session_id(),
        "project":ctx.project_id(),"root":ctx.root_id(),"assignment":ctx.assignment_id(),"jobRun":ctx.job_run_id(),"capability":ctx.capability_registration(),"policyRevision":ctx.policy_revision(),"worktree":null}})
}
pub(super) fn probe(f: &Fx) -> (Arc<EmbeddingSpaceService>, OperationRecord) {
    let human = principal(f.store.as_ref(), "probe-human", PrincipalKind::Human);
    let ctx = human.clone();
    let operation=write(f.store.as_ref(),move|tx|{
        let connection=tx.router().connection(&id("connection"))?.unwrap();
        let identity=embedding_spaces::embedding_identity(&connection,"fixture-model",2,&EmbeddingDimensions::Fixed(2),Metric::Cosine)?;
        let candidate=StoredEmbeddingCandidate{id:id("actual-candidate"),name:"Explicit human-selected candidate".into(),space_id:identity.space.space_id,connection_id:connection.id.clone(),
            account_revision:connection.account_revision,account_digest:connection.account_digest.clone(),endpoint:connection.endpoint.clone(),endpoint_digest:devworks_core::fingerprint::sha256_hex(connection.endpoint.as_bytes()),
            upstream_model_id:"fixture-model".into(),dimension_mode:"Native".into(),dimensions:2,metric:"Cosine".into(),element_type:"Float32".into(),encoder_config:identity.encoder_config,configuration_digest:identity.configuration_digest,model_digest:identity.model_digest,
            created_by:ctx.principal().clone(),created_at_ms:tx.now_ms()};
        tx.router().insert_embedding_candidate(&candidate,connection.revision)?;
        let op=OperationId(id("actual-probe-owner"));
        let delivery=id("actual-probe-delivery");
        let probe=StoredEmbeddingProbe{operation_id:op.clone(),delivery_id:delivery.clone(),request_id:id("actual-probe-request"),admission_key:id("actual-probe-admission"),candidate_revision:1,
            store_id:ctx.store_id().clone(),restore_epoch:ctx.restore_epoch().get(),actor_id:ctx.principal().clone(),scope:DataScope::Workspace,connection_revision:connection.revision,source_context:source_context(&ctx),authority_fences:vec![]};
        tx.operations().record_intent(NewOperation{id:op.clone(),root_id:None,assignment_id:None,capability:"router.configure".into(),schema_version:1,args_digest:"b".repeat(64),payload:json!({"kind":embedding_spaces::PROBE_KIND,"candidateId":candidate.id,"configurationDigest":candidate.configuration_digest,"probe":probe})})?;
        let event=common::event(tx,"ActualQualificationIntent")?;
        let now=tx.now_ms();
        tx.queue().enqueue(NewQueueEntry{id:delivery,consumer:embedding_spaces::EFFECT_CONSUMER.into(),event_id:event,root_id:None,operation_id:Some(op.clone()),partition:None,priority:0,ready_at_ms:now,payload:json!({"kind":embedding_spaces::PROBE_KIND})})?;
        tx.router().start_embedding_qualification(&candidate.id,1,&probe)?;
        let journal=tx.operations().begin_attempt(&op,1,AttemptBinding::default())?;
        tx.operations().transition(&op,2,Some(&journal.fence()),OperationState::Dispatching,None)
    }).unwrap();
    (
        Arc::new(EmbeddingSpaceService::new(f.store.clone())),
        operation,
    )
}
#[test]
fn fixed_probe_uses_actual_native_accounting_and_observation_never_resends() {
    let f = Fx::new(
        "Embedding",
        Script::aggregate(
            r#"{"model":"fixture-model","data":[{"index":0,"embedding":[0.25,0.75]}],"usage":{"prompt_tokens":3,"total_tokens":3}}"#,
        ),
        true,
    );
    let (service, operation) = probe(&f);
    let executor = NativeEmbeddings::new(
        f.store.clone(),
        f.artifacts.clone(),
        f.driver.clone(),
        service.clone(),
        f.h.clock(),
    );
    let ProbePreparation::Ready(prepared) = service.prepare_probe(&operation).unwrap() else {
        panic!("actual probe not ready")
    };
    let caller = prepared.context().clone();
    executor.execute_probe(*prepared.clone(), false).unwrap();
    let q = read(f.store.as_ref(), |s| {
        s.router().embedding_qualification(&id("actual-candidate"))
    })
    .unwrap();
    assert_eq!(
        q.state,
        EmbeddingQualificationState::Qualifying,
        "native completion stages evidence; dispatcher B owns qualification"
    );
    let evidence = q.evidence.unwrap();
    assert_eq!(evidence.request_id, id("actual-probe-request"));
    assert_ne!(evidence.native_operation_id, operation.id);
    let actual = read(f.store.as_ref(), |s| {
        s.usage().inference_attempt(&evidence.attempt_id)
    })
    .unwrap();
    assert_eq!(actual.actor_id, ActorId(id("probe-human")));
    assert_eq!(actual.operation_id, evidence.native_operation_id);
    assert_eq!(actual.purpose, "Embedding");
    let balance = read(f.store.as_ref(), |s| {
        s.usage().attempt_balance(&actual.attempt_id)
    })
    .unwrap();
    assert!(balance.local_quiescent && !balance.unknown_tail);
    assert_eq!(
        accounting::summary(&balance)
            .unwrap()
            .meter
            .quantities
            .input_tokens,
        Some(3)
    );
    executor.execute_probe(*prepared, true).unwrap();
    assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
    let service2 = service.clone();
    let id2 = operation.id.clone();
    write(f.store.as_ref(), move |tx| {
        let op = tx.operations().operation(&id2)?.unwrap();
        tx.operations().transition(
            &op.id,
            op.revision,
            op.latest_attempt().map(|a| a.fence()).as_ref(),
            OperationState::ObservedComplete,
            Some(json!({"actualNativeEvidenceStaged":true})),
        )?;
        service2.complete_probe(tx, &id2)
    })
    .unwrap();
    assert_eq!(
        read(f.store.as_ref(), |s| s
            .router()
            .embedding_qualification(&id("actual-candidate")))
        .unwrap()
        .state,
        EmbeddingQualificationState::Qualified
    );
    let sent = f.http.lock().unwrap();
    let requests = sent.requests.lock().unwrap();
    let body: Value = serde_json::from_slice(requests[0].body.as_ref().unwrap()).unwrap();
    assert_eq!(body["input"], json!([embedding_spaces::PROBE_TEXT]));
    assert!(
        body.get("dimensions").is_none(),
        "Native dimension mode preserves fixed-model request semantics"
    );
    drop(requests);
    drop(sent);
    write(f.store.as_ref(), |tx| {
        let q = tx
            .router()
            .embedding_qualification(&id("actual-candidate"))?
            .unwrap();
        tx.router()
            .select_embedding_candidate(1, Some((&id("actual-candidate"), q.revision)))?;
        Ok(())
    })
    .unwrap();
    use devworks_core::services::conventions::indexing::{
        IndexService,
        embedding::{ConventionEmbeddings, EmbeddingOrigin, EmbeddingRequest},
        preparation::InputPreparation,
    };
    let index = IndexService::new(
        f.store.clone(),
        f.artifacts.clone(),
        f.h.ids.clone(),
        f.h.clock(),
    );
    let configuration = executor.configured_space(&caller).unwrap();
    let space = configuration.space.unwrap();
    let registered_space = space.clone();
    write(f.store.as_ref(), move |tx| {
        tx.conventions()
            .put_convention_embedding_space(&registered_space)
    })
    .unwrap();
    let origin = EmbeddingOrigin::Query {
        query_id: id("actual-query"),
        fingerprint: "d".repeat(64),
        scope_digest: "e".repeat(64),
    };
    let token = Cancellation::new();
    let inputs = index
        .prepare_embedding_inputs(InputPreparation {
            context: &caller,
            origin: &origin,
            embedding_space: &space.id,
            configuration_source_digest: &configuration.source_digest,
            texts: &["actual query text"],
            fences: &[],
            cancellation: &token,
            deadline_ms: None,
        })
        .unwrap();
    let request = EmbeddingRequest {
        request_id: inputs.request_id,
        origin,
        space,
        configuration_source_digest: configuration.source_digest,
        preparation_operation: inputs.operation_id.clone(),
        input_refs: inputs.input_refs,
        fences: vec![],
        cancellation: token,
        deadline_ms: None,
    };
    assert!(matches!(executor.observe(&caller, request.clone()), Err(devworks_core::services::conventions::indexing::embedding::EmbeddingFailure::NotStarted { .. })));
    assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
    f.http.lock().unwrap().body.push_back(br#"{"model":"fixture-model","data":[{"index":0,"embedding":[0.25,0.75]}],"usage":{"prompt_tokens":3,"total_tokens":3}}"#.to_vec());
    let embedded = executor.embed(&caller, request.clone()).unwrap();
    assert_eq!(embedded.vectors, vec![vec![0.25, 0.75]]);
    let preparation = read(f.store.as_ref(), |s| {
        s.operations().operation(&inputs.operation_id)
    })
    .unwrap();
    assert_eq!(preparation.state, OperationState::ObservedComplete);
    assert_eq!(
        preparation.payload["sourceContext"]["invocationContext"]["principal"],
        json!(caller.principal())
    );
    executor.observe(&caller, request).unwrap();
    assert_eq!(
        f.http.lock().unwrap().requests.lock().unwrap().len(),
        2,
        "closed exact query is never resent"
    );
    assert!(f.transcript().is_empty());
}

#[test]
fn invalid_native_vectors_and_model_identity_never_qualify_but_keep_actual_usage() {
    for body in [
        r#"{"model":"fixture-model","data":[{"index":0,"embedding":[0,0]}],"usage":{"prompt_tokens":7,"total_tokens":7}}"#,
        r#"{"model":"fixture-model","data":[{"index":0,"embedding":[1]}],"usage":{"prompt_tokens":7,"total_tokens":7}}"#,
        r#"{"model":"different-model","data":[{"index":0,"embedding":[0.25,0.75]}],"usage":{"prompt_tokens":7,"total_tokens":7}}"#,
    ] {
        let f = Fx::new("Embedding", Script::aggregate(body), true);
        let (service, operation) = probe(&f);
        let executor = NativeEmbeddings::new(
            f.store.clone(),
            f.artifacts.clone(),
            f.driver.clone(),
            service.clone(),
            f.h.clock(),
        );
        let ProbePreparation::Ready(prepared) = service.prepare_probe(&operation).unwrap() else {
            panic!("ready");
        };
        assert!(executor.execute_probe(*prepared, false).is_err());
        let q = read(f.store.as_ref(), |s| {
            s.router().embedding_qualification(&id("actual-candidate"))
        })
        .unwrap();
        assert_ne!(q.state, EmbeddingQualificationState::Qualified);
        assert!(q.evidence.is_none());
        let native = read(f.store.as_ref(), |s| {
            let route = s
                .router()
                .route_request(&id("actual-probe-request"))?
                .unwrap();
            s.operations().operation(&route.operation_id)
        })
        .unwrap();
        let balance = read(f.store.as_ref(), |s| {
            s.usage()
                .attempt_balance(&native.latest_attempt().unwrap().id.0)
        })
        .unwrap();
        assert!(balance.local_quiescent);
        assert_eq!(
            accounting::summary(&balance)
                .unwrap()
                .meter
                .quantities
                .input_tokens,
            Some(7)
        );
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
    }
}

#[test]
fn probe_shutdown_after_send_retains_unknown_tail_and_observation_never_resends() {
    let mut script = Script::aggregate(r#"{"data":[{"index":0,"embedding":[0.25,0.75]}]}"#);
    script.wait_for_cancel = true;
    let waiting = script.waiting.clone();
    let f = Fx::new("Embedding", script, true);
    let (service, operation) = probe(&f);
    let executor = NativeEmbeddings::new(
        f.store.clone(),
        f.artifacts.clone(),
        f.driver.clone(),
        service.clone(),
        f.h.clock(),
    );
    let ProbePreparation::Ready(prepared) = service.prepare_probe(&operation).unwrap() else {
        panic!("ready");
    };
    let observer = *prepared.clone();
    let worker = executor.clone();
    let running = std::thread::spawn(move || worker.execute_probe(*prepared, false));
    common::runtime().block_on(common::within("probe sent", waiting.notified()));
    executor.stop();
    assert!(running.join().unwrap().is_err());
    assert!(f.http.lock().unwrap().closed.load(Ordering::SeqCst));
    let q = read(f.store.as_ref(), |s| {
        s.router().embedding_qualification(&id("actual-candidate"))
    })
    .unwrap();
    assert!(q.evidence.is_none());
    let fresh = NativeEmbeddings::new(
        f.store.clone(),
        f.artifacts.clone(),
        f.driver.clone(),
        service,
        f.h.clock(),
    );
    assert!(matches!(
        fresh.execute_probe(observer, true),
        Err(
            devworks_core::services::conventions::indexing::embedding::EmbeddingFailure::Unknown { .. }
        )
    ));
    assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
}

#[test]
fn probe_evidence_is_unavailable_until_physical_native_close() {
    let mut script = Script::aggregate(
        r#"{"model":"fixture-model","data":[{"index":0,"embedding":[0.25,0.75]}],"usage":{"prompt_tokens":7}}"#,
    );
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    script.close_gate = Some(gate.clone());
    let closing = script.closing.clone();
    let f = Fx::new("Embedding", script, true);
    let (service, operation) = probe(&f);
    let executor = NativeEmbeddings::new(
        f.store.clone(),
        f.artifacts.clone(),
        f.driver.clone(),
        service.clone(),
        f.h.clock(),
    );
    let ProbePreparation::Ready(prepared) = service.prepare_probe(&operation).unwrap() else {
        panic!("ready");
    };
    let running = std::thread::spawn(move || executor.execute_probe(*prepared, false));
    common::runtime().block_on(common::within("probe close gate", closing.notified()));
    let q = read(f.store.as_ref(), |s| {
        s.router().embedding_qualification(&id("actual-candidate"))
    })
    .unwrap();
    assert!(q.evidence.is_none());
    assert!(!f.http.lock().unwrap().closed.load(Ordering::SeqCst));
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    running.join().unwrap().unwrap();
    assert!(
        read(f.store.as_ref(), |s| s
            .router()
            .embedding_qualification(&id("actual-candidate")))
        .unwrap()
        .evidence
        .is_some()
    );
}
