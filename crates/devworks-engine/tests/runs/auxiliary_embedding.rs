//! Actual managed auxiliary production. All vectors are recorded protocol fixtures.
use super::*;
use devworks_adapters::fs::{
    NativeFilesystem, artifacts::NativeArtifactFiles, sources::HostArtifactSources,
};
use devworks_core::{
    ports::{
        IdKind,
        http::{Entropy, NoNetwork},
        provider_http::*,
        secrets::{Secret, SecretKey, SecretStore, SessionSecrets},
    },
    protocol::{EventId, data::Metric},
    providers::capabilities::EmbeddingDimensions,
    services::{
        artifacts::ArtifactService,
        conventions::indexing::{
            IndexService,
            embedding::{
                ConventionEmbeddings, EmbeddingFailure, EmbeddingOrigin, EmbeddingRequest,
            },
            preparation::InputPreparation,
        },
        router::{
            connections::{ConnectionService, auth::Credential},
            embedding_spaces::{self, EmbeddingSpaceService, ProbePreparation},
        },
    },
};
use devworks_engine::{
    conventions::embedding::NativeEmbeddings,
    providers::{
        driver::{NativeProviders, ProviderComposition},
        schemas::RegisteredSchemas,
    },
};
struct FixedEntropy;
impl Entropy for FixedEntropy {
    fn fill(&self, b: &mut [u8]) {
        b.fill(1);
    }
}
struct RecordedHttp(std::sync::atomic::AtomicUsize);
struct RecordedStream {
    head: ResponseHead,
    body: std::io::Cursor<Vec<u8>>,
    closed: Arc<AtomicBool>,
}
struct RecordedCloser(Arc<AtomicBool>);
impl StreamCloser for RecordedCloser {
    fn close(&self) -> Result<(), TransportError> {
        self.0.store(true, Ordering::SeqCst);
        Ok(())
    }
}
impl HttpStream for RecordedStream {
    fn head(&self) -> &ResponseHead {
        &self.head
    }
    fn closer(&self) -> Arc<dyn StreamCloser> {
        Arc::new(RecordedCloser(self.closed.clone()))
    }
    fn read(&mut self, b: &mut [u8], c: &Cancellation) -> Result<usize, TransportError> {
        if c.is_cancelled() {
            return Err(TransportError {
                phase: SendPhase::AfterSend,
                kind: TransportFailure::Cancelled,
            });
        }
        std::io::Read::read(&mut self.body, b).map_err(|_| TransportError {
            phase: SendPhase::AfterSend,
            kind: TransportFailure::Protocol,
        })
    }
    fn close(&mut self) -> Result<(), TransportError> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}
impl StreamingHttp for RecordedHttp {
    fn open(
        &self,
        r: &devworks_core::ports::http::HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let body: Value = serde_json::from_slice(r.body.as_ref().unwrap()).unwrap();
        Ok(Box::new(RecordedStream{head:ResponseHead{status:200,headers:vec![("content-type".into(),"application/json".into())]},body:std::io::Cursor::new(serde_json::to_vec(&json!({"model":body["model"],"data":[{"index":0,"embedding":[0.25,0.75]}],"usage":{"prompt_tokens":7,"total_tokens":7}})).unwrap()),closed:Arc::new(AtomicBool::new(false))}))
    }
}
fn source(ctx: &InvocationContext) -> Value {
    json!({"version":1,"storeId":ctx.store_id(),"restoreEpoch":ctx.restore_epoch(),"invocationContext":{"principal":ctx.principal(),"kind":"Human","invocation":ctx.invocation_id(),"namespace":ctx.ingress_namespace(),"session":ctx.session_id(),"project":ctx.project_id(),"root":ctx.root_id(),"assignment":ctx.assignment_id(),"jobRun":ctx.job_run_id(),"capability":ctx.capability_registration(),"policyRevision":ctx.policy_revision(),"worktree":null}})
}
#[test]
fn managed_conventions_only_agent_embeddings_reuse_actual_turn_and_refuse_missing_turn() {
    let f = Fx::new();
    let _ = router(&f);
    applied(&f.start(&f.user, "root-f", 1, 1));
    let activation = f.launch("actual-embedding-owner");
    let result=f.command(&f.agent,"sessions.start",json!({"mode":"Command","protocol":PROTOCOL,"requestKey":f.key(),"body":{"kind":"sessions.start","scope":{"kind":"Project","projectId":"project-a"},"rootId":"root-f","title":"Actual auxiliary embedding"}}));
    let value = serde_json::to_value(result).unwrap();
    let session = id(value["receipt"]["decision"]["result"]["value"]["sessionId"]
        .as_str()
        .or_else(|| value["receipt"]["decision"]["result"]["value"]["id"].as_str())
        .unwrap());
    let assigned = session.clone();
    let assignment = f.tx(move |service, tx| {
        let host = tx.store().host_actor.clone();
        Ok(service
            .register_assignment(
                tx,
                &host,
                NewRunAssignment {
                    root: rid("root-f"),
                    work: rid("root-f"),
                    parent: None,
                    session: assigned,
                    scope_key: None,
                },
            )?
            .unwrap())
    });
    let ctx = f
        .agent
        .clone()
        .with_lineage(Some(ProjectId(id("project-a"))), Some(rid("root-f")), None)
        .with_session(session);
    let secrets = Arc::new(SessionSecrets::new());
    secrets
        .put(
            &SecretKey::new(f.store.info().store_id.0.as_str(), id("aux-key")),
            &Credential::ApiKey(Secret::new("actual-fixture-key")).to_secret(),
        )
        .unwrap();
    f.tx(|_, tx| {
        let mut connection = tx.router().connection(&id("usage-connection"))?.unwrap();
        connection.id = id("aux-embedding-connection");
        connection.profile = "Embedding".into();
        connection.discovered = Some(vec![DiscoveredModel {
            id: "native-metadata-source".into(),
            context_tokens: None,
        }]);
        connection.credential_ref = Some(id("aux-key"));
        connection.credential_revision = 1;
        connection.credential_epoch = Some(tx.store().restore_epoch);
        connection.revision = 1;
        tx.router().put_connection(&connection, None).map(|_| ())
    });
    let connections = Arc::new(ConnectionService::new(
        f.h.ids.clone(),
        f.h.clock(),
        f.store.clone(),
        Arc::new(NoNetwork),
        secrets,
        Arc::new(FixedEntropy),
    ));
    let selected_router = Arc::new(InferenceRouter::new(
        f.h.ids.clone(),
        f.h.clock(),
        connections.clone(),
    ));
    let usage = UsageRuntime::new(
        f.store.clone(),
        f.service.clone(),
        Some(selected_router.clone()),
    );
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
    let schemas = Arc::new(RegisteredSchemas::new(
        f.store.clone(),
        f.use_cases
            .clone()
            .with_connections(connections.clone())
            .with_router(selected_router),
        None,
        artifacts.clone(),
    ));
    let http = Arc::new(RecordedHttp(std::sync::atomic::AtomicUsize::new(0)));
    let driver = NativeProviders::new(
        f.store.clone(),
        artifacts.clone(),
        connections,
        usage.clone(),
        schemas,
        ProviderComposition::new(http.clone()),
    );
    let spaces = Arc::new(EmbeddingSpaceService::new(f.store.clone()));
    let user = f.user.clone();
    let operation=write(f.store.as_ref(),move|tx|{
        let c=tx.router().connection(&id("aux-embedding-connection"))?.unwrap();let identity=embedding_spaces::embedding_identity(&c,"native-metadata-source",2,&EmbeddingDimensions::Fixed(2),Metric::Cosine)?;
        let candidate=StoredEmbeddingCandidate{id:id("managed-candidate"),name:"Recorded native space".into(),space_id:identity.space.space_id,connection_id:c.id.clone(),account_revision:c.account_revision,account_digest:c.account_digest.clone(),endpoint:c.endpoint.clone(),endpoint_digest:devworks_core::fingerprint::sha256_hex(c.endpoint.as_bytes()),upstream_model_id:"native-metadata-source".into(),dimension_mode:"Native".into(),dimensions:2,metric:"Cosine".into(),element_type:"Float32".into(),encoder_config:identity.encoder_config,configuration_digest:identity.configuration_digest,model_digest:identity.model_digest,created_by:user.principal().clone(),created_at_ms:tx.now_ms()};tx.router().insert_embedding_candidate(&candidate,c.revision)?;
        let op=OperationId(id("managed-space-probe"));let delivery=id("managed-space-delivery");let probe=StoredEmbeddingProbe{operation_id:op.clone(),delivery_id:delivery.clone(),request_id:id("managed-probe-request"),admission_key:id("managed-probe-admission"),candidate_revision:1,store_id:user.store_id().clone(),restore_epoch:user.restore_epoch().get(),actor_id:user.principal().clone(),scope:devworks_core::protocol::data::DataScope::Workspace,connection_revision:c.revision,source_context:source(&user),authority_fences:vec![]};
        tx.operations().record_intent(NewOperation{id:op.clone(),root_id:None,assignment_id:None,capability:"router.configure".into(),schema_version:1,args_digest:"b".repeat(64),payload:json!({"kind":embedding_spaces::PROBE_KIND,"candidateId":candidate.id,"configurationDigest":candidate.configuration_digest,"probe":probe})})?;
        let event=EventId(tx.new_id(IdKind::Event));tx.history().append(NewEvent{id:event.clone(),actor:user.principal().clone(),project_id:user.project_id().cloned(),root_id:None,operation_id:Some(op.clone()),aggregate_kind:"embedding_space".into(),aggregate_id:candidate.id.as_str().to_owned(),aggregate_revision:2,kind:"EmbeddingQualificationRequested".into(),payload:json!({"candidateId":candidate.id})})?;
        let now=tx.now_ms();tx.queue().enqueue(NewQueueEntry{id:delivery,consumer:embedding_spaces::EFFECT_CONSUMER.into(),event_id:event,root_id:None,operation_id:Some(op.clone()),partition:None,priority:0,ready_at_ms:now,payload:json!({"kind":embedding_spaces::PROBE_KIND})})?;
        tx.router().start_embedding_qualification(&candidate.id,1,&probe)?;let journal=tx.operations().begin_attempt(&op,1,AttemptBinding::default())?;tx.operations().transition(&op,2,Some(&journal.fence()),OperationState::Dispatching,None)
    }).unwrap();
    let native = NativeEmbeddings::new(
        f.store.clone(),
        artifacts.clone(),
        driver.clone(),
        spaces.clone(),
        f.h.clock(),
    );
    // Keep the actual provider owner alive; the embedding port deliberately holds it weakly.
    // NativeProviders is held by this local value exactly as Runtime holds it in production.
    let ProbePreparation::Ready(prepared) = spaces.prepare_probe(&operation).unwrap() else {
        panic!("ready");
    };
    native.execute_probe(*prepared, false).unwrap();
    let selected = spaces.clone();
    let op = operation.id.clone();
    write(f.store.as_ref(), move |tx| {
        let actual = tx.operations().operation(&op)?.unwrap();
        tx.operations().transition(
            &op,
            actual.revision,
            actual.latest_attempt().map(|a| a.fence()).as_ref(),
            OperationState::ObservedComplete,
            None,
        )?;
        selected.complete_probe(tx, &op)?;
        let q = tx
            .router()
            .embedding_qualification(&id("managed-candidate"))?
            .unwrap();
        tx.router()
            .select_embedding_candidate(1, Some((&id("managed-candidate"), q.revision)))?;
        Ok(())
    })
    .unwrap();
    let actor = ctx.principal().clone();
    write(f.store.as_ref(), move |tx| {
        let p = tx.authority().principal(&actor)?.unwrap();
        tx.authority().bind_principal(
            NewPrincipalBinding {
                actor: p.actor,
                kind: p.kind,
                role: p.role,
                project_id: p.project_id,
                tool_families: Some(vec!["conventions".into()]),
                bound_by: p.bound_by,
            },
            Some(p.revision),
        )?;
        Ok(())
    })
    .unwrap();
    let config = native.configured_space(&ctx).unwrap();
    let space = config.space.unwrap();
    let registered = space.clone();
    write(f.store.as_ref(), move |tx| {
        tx.conventions().put_convention_embedding_space(&registered)
    })
    .unwrap();
    let index = IndexService::new(f.store.clone(), artifacts, f.h.ids.clone(), f.h.clock());
    let origin = EmbeddingOrigin::Query {
        query_id: id("actual-managed-query"),
        fingerprint: "d".repeat(64),
        scope_digest: "e".repeat(64),
    };
    let token = Cancellation::new();
    let inputs = index
        .prepare_embedding_inputs(InputPreparation {
            context: &ctx,
            origin: &origin,
            embedding_space: &space.id,
            configuration_source_digest: &config.source_digest,
            texts: &["actual scoped query"],
            fences: &[],
            cancellation: &token,
            deadline_ms: None,
        })
        .unwrap();
    let request = EmbeddingRequest {
        request_id: inputs.request_id,
        origin,
        space,
        configuration_source_digest: config.source_digest,
        preparation_operation: inputs.operation_id,
        input_refs: inputs.input_refs,
        fences: vec![],
        cancellation: token,
        deadline_ms: None,
    };
    assert!(matches!(
        native.embed(&ctx, request.clone()),
        Err(EmbeddingFailure::Unavailable(_))
    ));
    assert_eq!(
        http.0.load(Ordering::SeqCst),
        1,
        "missingturn mustnotcallHTTP"
    );
    let own = assignment.clone();
    let running = activation.clone();
    let turn = f.tx(move |service, tx| {
        service
            .begin_turn(
                tx,
                &own.session_id,
                &own.id,
                &running.id,
                running.owner_epoch,
            )
            .map(|t| t.unwrap())
    });
    let result = native.embed(&ctx, request).unwrap();
    let attempt = read(f.store.as_ref(), |s| {
        s.usage().inference_attempt(&result.attempt_id)
    })
    .unwrap();
    let admitted = devworks_core::services::usage::accounting::snapshot(&attempt).unwrap();
    assert_eq!(attempt.actor_id, *ctx.principal());
    assert_eq!(attempt.work_id, Some(id("root-f")));
    assert_eq!(attempt.purpose, "Embedding");
    let dispatch = admitted.dispatch.unwrap();
    assert_eq!(dispatch.turn_id, turn.turn_id);
    assert_eq!(dispatch.assignment_id, Some(assignment.id));
    assert_eq!(dispatch.execution_id, activation.id);
    assert_eq!(http.0.load(Ordering::SeqCst), 2);
    let balance = read(f.store.as_ref(), |s| {
        s.usage().attempt_balance(&result.attempt_id)
    })
    .unwrap();
    assert_eq!(
        devworks_core::services::usage::accounting::summary(&balance)
            .unwrap()
            .meter
            .quantities
            .input_tokens,
        Some(7)
    );
    assert!(balance.local_quiescent && !balance.unknown_tail);
    let ending_session = ctx.session_id().unwrap().clone();
    let ending_turn = turn.turn_id;
    f.tx(move |service, tx| service.end_turn(tx, &ending_session, &ending_turn))
        .unwrap();
    let after_config = native.configured_space(&ctx).unwrap();
    let after_space = after_config.space.unwrap();
    let after_origin = EmbeddingOrigin::Query {
        query_id: id("query-after-owned-turn"),
        fingerprint: "f".repeat(64),
        scope_digest: "e".repeat(64),
    };
    let after_token = Cancellation::new();
    let after_inputs = index
        .prepare_embedding_inputs(InputPreparation {
            context: &ctx,
            origin: &after_origin,
            embedding_space: &after_space.id,
            configuration_source_digest: &after_config.source_digest,
            texts: &["no current turn"],
            fences: &[],
            cancellation: &after_token,
            deadline_ms: None,
        })
        .unwrap();
    let after_request = EmbeddingRequest {
        request_id: after_inputs.request_id,
        origin: after_origin,
        space: after_space,
        configuration_source_digest: after_config.source_digest,
        preparation_operation: after_inputs.operation_id,
        input_refs: after_inputs.input_refs,
        fences: vec![],
        cancellation: after_token,
        deadline_ms: None,
    };
    assert!(matches!(
        native.embed(&ctx, after_request),
        Err(EmbeddingFailure::Unavailable(_))
    ));
    assert_eq!(
        http.0.load(Ordering::SeqCst),
        2,
        "ending the real turn removes compute ownership"
    );
    native.stop();
    usage.stop();
}
