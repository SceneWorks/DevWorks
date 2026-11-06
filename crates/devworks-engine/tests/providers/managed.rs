//! Actual managed run gates win over unchanged originating-producer wire counters.
use super::*;
use devworks_adapters::fs::{
    NativeFilesystem, artifacts::NativeArtifactFiles, sources::HostArtifactSources,
};
use devworks_core::{
    ports::{
        http::{Entropy, NoNetwork},
        provider_http::*,
        secrets::{Secret, SecretError, SecretKey, SecretStore, SessionSecrets},
    },
    protocol::provider::*,
    services::{
        artifacts::{ArtifactService, host::HostArtifactPublication, version_ref},
        router::connections::ConnectionService,
    },
};
use devworks_engine::providers::{
    driver::{NativeProviders, ProviderComposition},
    schemas::RegisteredSchemas,
};
struct EntropyFixture;
impl Entropy for EntropyFixture {
    fn fill(&self, bytes: &mut [u8]) {
        bytes.fill(1);
    }
}
struct CredentialGate {
    values: SessionSecrets,
    armed: AtomicBool,
    entered: tokio::sync::Notify,
    open: Mutex<bool>,
    changed: Condvar,
}
impl CredentialGate {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
impl SecretStore for CredentialGate {
    fn name(&self) -> &'static str {
        "controlled session fixture"
    }
    fn put(&self, key: &SecretKey, value: &Secret) -> Result<(), SecretError> {
        self.values.put(key, value)
    }
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            let mut open = self.open.lock().unwrap();
            while !*open {
                open = self.changed.wait(open).unwrap();
            }
        }
        self.values.get(key)
    }
    fn delete(&self, key: &SecretKey) -> Result<bool, SecretError> {
        self.values.delete(key)
    }
    fn list(&self, n: &str) -> Result<Vec<Id>, SecretError> {
        self.values.list(n)
    }
}
struct CountHttp(AtomicUsize);
impl StreamingHttp for CountHttp {
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
fn managed_redirect_pause_principal_and_session_changes_before_send_defeat_unchanged_wire_generations()
 {
    runtime().block_on(within("native managed send fences",async {
for changed in ["baseline","redirect","pause","principal","session"] {
let f=Arc::new(Fx::new());let _=router(&f);applied(&f.start(&f.user,"root-f",1,1));let activation=f.launch("native-provider-owner");
let started=f.command(&f.agent,"sessions.start",json!({"mode":"Command","protocol":PROTOCOL,"requestKey":f.key(),"body":{"kind":"sessions.start","scope":{"kind":"Project","projectId":"project-a"},"rootId":"root-f","title":"Native managed context"}}));let v=serde_json::to_value(started).unwrap();let session=id(v["receipt"]["decision"]["result"]["value"]["sessionId"].as_str().or_else(||v["receipt"]["decision"]["result"]["value"]["id"].as_str()).unwrap_or_else(||panic!("Agent-owned session: {v}")));
let a_session=session.clone();let assignment=f.tx(move|svc,tx|{let host=tx.store().host_actor.clone();Ok(svc.register_assignment(tx,&host,NewRunAssignment{root:RootId(id("root-f")),work:RootId(id("root-f")),parent:None,session:a_session,scope_key:None})?.unwrap())});let own=assignment.clone();let active=activation.clone();let turn=f.tx(move|svc,tx|svc.begin_turn(tx,&own.session_id,&own.id,&active.id,active.owner_epoch).map(|r|r.unwrap()));let context=f.agent.clone().with_lineage(Some(ProjectId(id("project-a"))),Some(RootId(id("root-f"))),None).with_session(session.clone());
let credentials=Arc::new(CredentialGate{values:SessionSecrets::new(),armed:AtomicBool::new(false),entered:tokio::sync::Notify::new(),open:Mutex::new(false),changed:Condvar::new()});credentials.put(&SecretKey::new(f.store.info().store_id.0.as_str(),id("native-current-key")),&devworks_core::services::router::connections::auth::Credential::ApiKey(Secret::new("actual-scoped-fixture-key")).to_secret()).unwrap();f.tx(|_,tx|{let mut row=tx.router().connection(&id("usage-connection"))?.unwrap();let expected=row.revision;row.credential_ref=Some(id("native-current-key"));row.credential_revision=1;row.credential_epoch=Some(tx.store().restore_epoch);tx.router().put_connection(&row,Some(expected)).map(|_|())});
let connection_service=Arc::new(ConnectionService::new(f.h.ids.clone(),f.h.clock(),f.store.clone(),Arc::new(NoNetwork),credentials.clone(),Arc::new(EntropyFixture)));let selected_router=Arc::new(InferenceRouter::new(f.h.ids.clone(),f.h.clock(),connection_service.clone()));let usage=UsageRuntime::new(f.store.clone(),f.service.clone(),Some(selected_router.clone()));
let artifacts=Arc::new(ArtifactService::new(Arc::new(NativeArtifactFiles::new(f.h.root.clone(),f.h.root.with_extension("cache"))),Arc::new(HostArtifactSources::new(f.h.clock(),f.h.ids.clone())),Arc::new(NativeFilesystem::new()),f.h.clock(),f.h.ids.clone()));let material=serde_json::to_vec(&devworks_core::providers::context::ProviderContext{version:1,instructions:vec![],messages:vec![devworks_core::providers::context::Message{role:devworks_core::providers::context::MessageRole::User,content:vec![devworks_core::providers::context::ContentBlock::Text{text:"Actual scoped pre-redirect context".into(),cache:None}]}],parameters:Default::default()}).unwrap();let staged=artifacts.stage_host_bytes(f.store.as_ref(),&material,json!({"context":"managed"})).unwrap().unwrap();let pending=staged.clone();let service=artifacts.clone();let version=write(f.store.as_ref(),move|tx|service.commit_host_bytes_in_scope(tx,&pending,HostArtifactPublication {title:"Actual scoped context",media_type:"application/json",provenance:json!({"context":"managed"}),scope:devworks_core::ports::storage::ArtifactScope::Project(ProjectId(id("project-a"))),owner:ActorId(id("agent-a"))})).unwrap();artifacts.settle_host_bytes(f.store.as_ref(),&staged,true);
let cases=f.use_cases.clone().with_router(selected_router).with_connections(connection_service.clone());let schemas=Arc::new(RegisteredSchemas::new(f.store.clone(),cases,None,artifacts.clone()));let http=Arc::new(CountHttp(AtomicUsize::new(0)));let driver=NativeProviders::new(f.store.clone(),artifacts, connection_service,usage.clone(),schemas,ProviderComposition::new(http.clone()));
                let query = PriceQuery {
                    catalog_model_id: id("usage-model"),
                    provider_model_id: Some("native-metadata-source".into()),
                    connection_id: id("usage-connection"),
                    endpoint_profile: "CompatibleChat".into(),
                    access_mode: "host-derived".into(),
                    service_tier: None,
                    input_tokens: Some(10),
                    cache_write_ttl_ms: None,
                    processing_region: None,
                    required_modalities: vec!["Text".into()],
                };
                let request = AttemptRequest {
                    admission_key: id("owned-admission"),
                    request_id: id("owned-request"),
                    actor_id: context.principal().clone(),
                    work_id: Some(assignment.work_id.0.clone()),
                    job_run_id: None,
                    role: "Worker".into(),
                    query,
                    quote: PriceQuote {
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
                    dispatch: Some(DispatchIdentity {
                        root_id: RootId(id("root-f")),
                        execution_id: activation.id,
                        owner_epoch: activation.owner_epoch,
                        assignment_id: None,
                        assignment_generation: assignment.generation,
                        turn_id: turn.turn_id,
                        implementation: false,
                    }),
                    cache_diagnostics: vec![],
                };
                let logical = RequestAdmission {
                    request_id: id("owned-request"),
                    purpose: Purpose::Conversation,
                    session_id: Some(assignment.session_id.clone()),
                    project_id: Some(ProjectId(id("project-a"))),
                    root_id: Some(assignment.work_id.clone()),
                    model_id: id("usage-model"),
                    connection_id: id("usage-connection"),
                    pin_id: None,
                    args_digest: "a".repeat(64),
                };

let inference=InferenceRequest{protocol:devworks_core::protocol::Protocol::Draft1,request_id:logical.request_id.clone(),purpose:InferencePurpose::Conversation,attribution:Attribution::Feature{project_id:ProjectId(id("project-a")),root_id:RootId(id("root-f")),assignment_id:None,job_run_id:None},input:InferenceInput::Generation{session_id:session.clone(),context:version_ref(&version),context_epoch:devworks_core::protocol::Counter::new(0).unwrap(),tools:vec![],response_headroom_tokens:Revision::new(10).unwrap(),requires_images:false,requires_parallel_tools:false},options:InferenceOptions{selection:ModelSelection::Model{model_id:id("usage-model")},effort:None,output_schema:None,deadline_ms:2000},authority_revision:Revision::new(1).unwrap(),control_generation:Revision::new(1).unwrap(),instruction_revision:Revision::new(1).unwrap(),cancellation_revision:Revision::new(1).unwrap()};let original=serde_json::to_value(&inference).unwrap();
credentials.armed.store(true,Ordering::SeqCst);let run=tokio::spawn(async move{driver.produce(context,logical,ReportCategory::Assistant,request,inference).await});credentials.entered.notified().await;
match changed {
"redirect"=>{applied(&f.redirect(&f.user,"root-f","A newer instruction must be incorporated before another dispatch"));},
"pause"=>{let stop=f.stop(&f.user,"root-f");assert!(matches!(decision(&stop),DomainDecision::Accepted{..}));},
"principal"=>{let actor=ActorId(id("agent-a"));let user=f.user.clone();f.tx(move|_,tx|{let current=tx.authority().principal(&actor)?.unwrap();devworks_core::authority::service::revoke_principal(tx,&user,&actor,current.revision).map(|_|()).map_err(|e|StorageError::InvalidRecord(format!("{e:?}")))});},
"session"=>{let current=read(f.store.as_ref(),|s|s.sessions().session(&session)).unwrap();applied(&f.command(&f.user,"sessions.end",json!({"mode":"Command","protocol":PROTOCOL,"requestKey":f.key(),"body":{"kind":"sessions.end","sessionId":session,"expectedRevision":current.revision.to_string()}})));},
_=>{}
}
credentials.release();let result=run.await.unwrap();let error=result.err();if changed=="baseline"{assert_eq!(http.0.load(Ordering::SeqCst),1,"legitimate managed context must reach the controlled transport: {error:?}");}else{assert_eq!(http.0.load(Ordering::SeqCst),0,"{changed} must win over originating generation fields: {error:?}");}assert_eq!(original["authorityRevision"],"1");assert_eq!(original["instructionRevision"],"1");usage.stop();usage.drain().await;
}
}));
}
