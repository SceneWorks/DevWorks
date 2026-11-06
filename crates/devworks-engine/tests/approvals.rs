//! Trusted native human decisions and durable readmission through the real gateway/SQLite
//! store. Saved commands retain their original Agent identity. No inference or native job runs.
mod common;
use common::*;
use devworks_adapters::fs::NativeFilesystem;
use devworks_adapters::fs::artifacts::NativeArtifactFiles;
use devworks_adapters::fs::sources::HostArtifactSources;
use devworks_adapters::power::NativePower;
use devworks_adapters::process::NativeProcessHost;
use devworks_core::authority::HostAuthority;
use devworks_core::authority::service::bind_principal;
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::gateway::{AdmittedCommand, CommandOutcome};
use devworks_core::ports::storage::*;
use devworks_core::ports::{
    AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization, Clock, CommandHandler,
    Decided, HandlerFailure, IdKind, IdSource,
};
use devworks_core::protocol::results::{DomainCommandReply, DomainDecision};
use devworks_core::protocol::{ActorId, Id, ProjectId, RootId};
use devworks_core::registry::SchemaRevision;
use devworks_core::services::{
    UseCases, artifacts::ArtifactService, diagnostics::DiagnosticService,
    resources::ResourceService, runs::ControlService, sessions::SessionService,
};
use devworks_core::settings::catalog::AgentRole;
use devworks_engine::{Composition, RunComposition, Runtime, RuntimeConfig, boot};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

struct NativeAuthority {
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn Clock>,
}
impl AuthorityPort for NativeAuthority {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.clock,
            protected_branches: &[],
        }
        .authorize(ctx, request)
    }
}
struct Maintenance;
impl CommandHandler for Maintenance {
    fn decide(
        &self,
        _: &InvocationContext,
        _: &AdmittedCommand,
        _: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        panic!("enabling a manual job must not execute its capability")
    }
}
fn composition(h: &Harness, store: Arc<dyn ApplicationStore>) -> Composition {
    let mut config = RuntimeConfig::new(id("approval-dispatcher"));
    config.approval_resume_interval = Duration::from_secs(3600);
    let mut use_cases =
        UseCases::new().with_sessions(Arc::new(SessionService::new(h.ids.clone(), store.clone())));
    use_cases.register(
        "help.generate_capability_docs",
        None,
        Some(Arc::new(Maintenance)),
    );
    Composition {
        store: store.clone(),
        clock: h.clock(),
        ids: h.ids.clone(),
        authority: Arc::new(NativeAuthority {
            store: store.clone(),
            clock: h.clock(),
        }),
        artifacts: Arc::new(ArtifactService::new(
            Arc::new(NativeArtifactFiles::new(
                h.root.clone(),
                h.root.with_extension("cache"),
            )),
            Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
            Arc::new(NativeFilesystem::new()),
            h.clock(),
            h.ids.clone(),
        )),
        resources: Arc::new(ResourceService::new(h.ids.clone(), h.clock())),
        process_host: Arc::new(NativeProcessHost),
        runs: RunComposition::new(Arc::new(ControlService::new(h.ids.clone()))),
        power: Arc::new(NativePower),
        diagnostics: Arc::new(DiagnosticService::new(
            store,
            h.ids.clone(),
            h.clock(),
            None,
        )),
        browser: None,
        git: None,
        packages: None,
        use_cases,
        consumers: vec![],
        indexing: None,
        config,
    }
}
fn principals(store: &dyn ApplicationStore) -> (InvocationContext, InvocationContext) {
    let user = principal(store, "user", PrincipalKind::Human);
    let agent = principal(store, "agent", PrincipalKind::Agent);
    if read(store, |s| s.authority().principal(agent.principal())).is_none() {
        let u = user.clone();
        let actor = agent.principal().clone();
        write(store, move |tx| {
            bind_principal(
                tx,
                &u,
                NewPrincipalBinding {
                    actor,
                    kind: PrincipalKind::Agent,
                    role: Some(AgentRole::WorkspaceCoordinator),
                    project_id: None,
                    tool_families: None,
                    bound_by: u.principal().clone(),
                },
                None,
            )
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
        })
        .unwrap();
    }
    let recipient = user.principal().clone();
    write(store, move |tx| {
        tx.decisions().bind_notification_user(&recipient)
    })
    .unwrap();
    (user, agent)
}
fn frame(h: &Harness, op: &str, revision: u32, mut body: Value) -> Vec<u8> {
    body["kind"] = json!(op);
    serde_json::to_vec(
        &json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":revision,
        "requestKey":h.ids.next_id(IdKind::Link),"body":body}),
    )
    .unwrap()
}
fn command(
    h: &Harness,
    app: &Runtime,
    ctx: &InvocationContext,
    op: &str,
    revision: u32,
    body: Value,
) -> DomainCommandReply {
    app.command(ctx, op, &frame(h, op, revision, body)).unwrap()
}
fn value(reply: DomainCommandReply) -> Value {
    match reply {
        devworks_core::protocol::CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied { result } | DomainDecision::NoChange { result } => {
                serde_json::to_value(result).unwrap()["value"].clone()
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}
fn draft(
    h: &Harness,
    app: &Runtime,
    agent: &InvocationContext,
    title: &str,
    project: bool,
) -> Value {
    let schema = devworks_core::services::jobs::argument_schema(
        "help.generate_capability_docs",
        SchemaRevision(1),
    )
    .unwrap();
    let scope = if project {
        json!({"kind":"Project","projectId":"project-a"})
    } else {
        json!({"kind":"Workspace"})
    };
    value(command(
        h,
        app,
        agent,
        "jobs.create",
        2,
        json!({"scope":scope,"title":title,"schedule":{"kind":"Manual"},
        "definition":{"kind":"Native","capabilityId":"help-generate_capability_docs","arguments":{"schema":schema,"value":{"registryRevision":"1","expectedHelpRevision":"1"}}},"enabled":false}),
    ))
}
fn propose_enable(
    h: &Harness,
    app: &Runtime,
    agent: &InvocationContext,
    job: &Value,
    project: bool,
) -> Value {
    let input = json!({"jobId":job["id"],"expectedRevision":job["revision"]});
    let decoded =
        devworks_core::services::approvals::requests::decode("jobs.enable", 2, &input, true)
            .unwrap();
    let scope = if project {
        json!({"kind":"Project","projectId":"project-a"})
    } else {
        json!({"kind":"Workspace"})
    };
    value(command(
        h,
        app,
        agent,
        "approvals.propose",
        3,
        json!({"scope":scope,"operationKind":"jobs.enable",
        "target":{"id":job["id"],"revision":job["revision"]},"contentDigest":decoded.fingerprint.as_str(),"summary":"Enable reviewed draft",
        "payload":{"kind":"SavedOperation","schemaRevision":"2","input":input}}),
    ))
}
fn decision(h: &Harness, proposal: &Value) -> Vec<u8> {
    frame(
        h,
        "approvals.record_human_decision",
        3,
        json!({"proposalId":proposal["id"],"expectedRevision":proposal["revision"],
        "contentDigest":proposal["contentDigest"],"decision":"Approve","rationale":"Enable this exact reviewed manual job"}),
    )
}
fn resume(store: &dyn ApplicationStore, proposal: &Value) -> StoredApprovalResume {
    read(store, |s| {
        s.decisions()
            .approval_resume(&id(proposal["id"].as_str().unwrap()))
    })
    .unwrap()
}
fn enabled(store: &dyn ApplicationStore, job: &Value) -> bool {
    read(store, |s| s.jobs().job(&id(job["id"].as_str().unwrap())))
        .unwrap()
        .enabled
}

#[test]
fn only_dedicated_native_decisions_readmit_the_saved_command_as_its_original_agent() {
    let h = Harness::new();
    runtime().block_on(async{
        let store=h.open();let (user,agent)=principals(&*store);
        let app=boot(composition(&h,store.clone())).await.unwrap();
        let job=draft(&h,&app,&agent,"Reviewed job",false);
        let proposal=propose_enable(&h,&app,&agent,&job,false);
        let generic=app.command(&user,"approvals.record_human_decision",&decision(&h,&proposal)).unwrap();
        assert!(!matches!(generic,devworks_core::protocol::CommandReply::Recorded{receipt,..}if matches!(receipt.decision,DomainDecision::Applied{..})),"generic command cannot manufacture a native interaction");
        assert!(!enabled(&*store,&job));
        let raw=decision(&h,&proposal);
        value(app.record_human_decision(&user,&raw).unwrap());
        app.approval_resumes().tick().await.unwrap();
        assert!(enabled(&*store,&job));
        let saved=resume(&*store,&proposal);assert_eq!(saved.state,"Admitted");assert_eq!(saved.principal_kind,PrincipalKind::Agent);assert_eq!(saved.principal,agent.principal().clone());
        // An exact native interaction replay and another catch-up pass do not re-execute it.
        value(app.record_human_decision(&user,&raw).unwrap());app.approval_resumes().tick().await.unwrap();
        assert_eq!(resume(&*store,&proposal),saved);
        let events=read(&*store,|s|s.history().page(&EventFilter::Aggregate{kind:"job".into(),id:job["id"].as_str().unwrap().into()},0,500));
        let enabled:Vec<_>=events.events.iter().filter(|e|e.kind=="JobEnabled").collect();assert_eq!(enabled.len(),1);assert_eq!(enabled[0].actor,*agent.principal());
        app.shutdown().await;
    });
}

#[test]
fn approved_operations_wait_for_independent_pause_without_minting_a_refusal_receipt() {
    let h = Harness::new();
    runtime().block_on(async {
        let store = h.open();
        seed_root(&h, "root");
        rusqlite::Connection::open(h.root.join("workspace.sqlite"))
            .unwrap()
            .execute("UPDATE work_items SET kind='issue' WHERE id='root'", [])
            .unwrap();
        let (user, agent) = principals(&*store);
        let agent = agent.with_lineage(
            Some(ProjectId(id("project-a"))),
            Some(RootId(id("root"))),
            None,
        );
        let app = boot(composition(&h, store.clone())).await.unwrap();
        let job = draft(&h, &app, &agent, "Paused reviewed job", true);
        let proposal = propose_enable(&h, &app, &agent, &job, true);
        let pause=serde_json::to_vec(&json!({"protocol":"1.0-draft.1","requestKey":h.ids.next_id(IdKind::Link),
            "command":{"kind":"runs.pause","rootId":"root","expectedControlRevision":"1"}})).unwrap();
        value(app.command(&user,"runs.pause",&pause).unwrap());
        value(
            app.record_human_decision(&user, &decision(&h, &proposal))
                .unwrap(),
        );
        let pass = app.approval_resumes().tick().await.unwrap();
        assert!(pass.waiting >= 1);
        assert!(!enabled(&*store, &job));
        let pending = resume(&*store, &proposal);
        assert_eq!(pending.state, "Pending");
        assert!(pending.result.is_none());
        let control=read(&*store,|s|s.runs().root_control(&RootId(id("root")))).unwrap();
        assert_eq!(control.desired,DesiredExecution::Paused,"approval did not clear the pause");
        let holds=read(&*store,|s|s.runs().blockers(&BlockerFilter::RootLevel(RootId(id("root"))),None,500));
        let covered:Vec<_>=holds.iter().filter(|h|h.cause.manual_resume()).map(|h|json!({"holdId":h.id,"revision":h.revision.to_string()})).collect();
        let mut passes=app.approval_resumes().watch();passes.borrow_and_update();
        let continue_frame=serde_json::to_vec(&json!({"protocol":"1.0-draft.1","requestKey":h.ids.next_id(IdKind::Link),
            "command":{"kind":"runs.continue","rootId":"root","expectedControlRevision":control.revision.to_string(),"coveredHolds":covered}})).unwrap();
        value(app.command(&user,"runs.continue",&continue_frame).unwrap());
        within("Continue wakes approval readmission",async{while !enabled(&*store,&job){passes.changed().await.unwrap();}}).await;
        app.approval_resumes().tick().await.unwrap();
        assert!(enabled(&*store, &job));
        assert_eq!(resume(&*store, &proposal).request_key, pending.request_key);
        app.shutdown().await;
    });
}

#[test]
fn approved_pending_outbox_is_readmitted_once_after_restart_and_stops_with_runtime() {
    let h = Harness::new();
    runtime().block_on(async {
        let store = h.open();
        let (user, agent) = principals(&*store);
        let app = boot(composition(&h, store.clone())).await.unwrap();
        app.approval_resumes().drain().await;
        let job = draft(&h, &app, &agent, "Restart reviewed job", false);
        let proposal = propose_enable(&h, &app, &agent, &job, false);
        value(
            app.record_human_decision(&user, &decision(&h, &proposal))
                .unwrap(),
        );
        let saved = resume(&*store, &proposal);
        assert_eq!(saved.state, "Pending");
        assert!(!enabled(&*store, &job));
        app.shutdown().await;
        drop(app);
        drop(store);
        let store = h.open();
        principals(&*store);
        let app = boot(composition(&h, store.clone())).await.unwrap();
        let mut passes = app.approval_resumes().watch();
        within("boot approval readmission", async {
            while resume(&*store, &proposal).state != "Admitted" {
                passes.changed().await.unwrap();
            }
        })
        .await;
        assert!(enabled(&*store, &job));
        let admitted = resume(&*store, &proposal);
        assert_eq!(admitted.request_key, saved.request_key);
        assert_eq!(admitted.principal_kind, PrincipalKind::Agent);
        app.shutdown().await;
        assert_eq!(app.approval_resumes().tick().await.unwrap().visited, 0);
        assert_eq!(resume(&*store, &proposal), admitted);
    });
}

struct LostReply {
    runtime: Arc<Runtime>,
    first: std::sync::atomic::AtomicBool,
    observations: std::sync::Mutex<Vec<(ActorId, PrincipalKind, Vec<u8>)>>,
}
impl devworks_engine::approvals::ResumeAdmission for LostReply {
    fn readmit(
        &self,
        context: &InvocationContext,
        operation: &str,
        raw: &[u8],
        options: WriteOptions,
    ) -> CommandOutcome {
        self.observations.lock().unwrap().push((
            context.principal().clone(),
            context.principal_kind(),
            raw.to_vec(),
        ));
        let outcome = self.runtime.command_with(context, operation, raw, options);
        if self.first.swap(false, std::sync::atomic::Ordering::SeqCst) {
            assert!(matches!(
                outcome,
                Ok(devworks_core::protocol::CommandReply::Recorded { .. })
            ));
            let request = serde_json::from_slice::<Value>(raw).unwrap();
            return Ok(
                devworks_core::protocol::CommandReply::CommandStatusUnknown {
                    protocol: devworks_core::protocol::Protocol::Draft1,
                    request_key: devworks_core::protocol::RequestKey(id(request["requestKey"]
                        .as_str()
                        .unwrap())),
                    correlation_id: devworks_core::protocol::CorrelationId(id(
                        "lost-approval-reply",
                    )),
                },
            );
        }
        assert!(
            matches!(
                outcome,
                Ok(devworks_core::protocol::CommandReply::Recorded { replayed: true, .. })
            ),
            "retry must resolve the original gateway receipt"
        );
        outcome
    }
}
#[test]
fn lost_admission_reply_reuses_the_original_agent_frame_and_receipt() {
    let h = Harness::new();
    runtime().block_on(async {
        let store = h.open();
        let (user, agent) = principals(&*store);
        let app = Arc::new(boot(composition(&h, store.clone())).await.unwrap());
        app.approval_resumes().drain().await;
        let job = draft(&h, &app, &agent, "Lost-reply reviewed job", false);
        let proposal = propose_enable(&h, &app, &agent, &job, false);
        value(
            app.record_human_decision(&user, &decision(&h, &proposal))
                .unwrap(),
        );
        let admission = Arc::new(LostReply {
            runtime: app.clone(),
            first: std::sync::atomic::AtomicBool::new(true),
            observations: std::sync::Mutex::new(vec![]),
        });
        let driver = devworks_engine::approvals::ResumeDispatcher::new(
            store.clone(),
            app.approvals().clone(),
            admission.clone(),
        );
        driver.tick().await.unwrap();
        assert!(enabled(&*store, &job));
        assert_eq!(resume(&*store, &proposal).state, "Unknown");
        driver.tick().await.unwrap();
        assert_eq!(resume(&*store, &proposal).state, "Admitted");
        let seen = admission.observations.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], seen[1]);
        assert_eq!(seen[0].0, *agent.principal());
        assert_eq!(seen[0].1, PrincipalKind::Agent);
        drop(seen);
        driver.drain().await;
        app.shutdown().await;
    });
}

struct CancelAtAdmission {
    runtime: Arc<Runtime>,
    started: tokio::sync::Notify,
    gate: std::sync::Mutex<bool>,
    release: std::sync::Condvar,
}
impl devworks_engine::approvals::ResumeAdmission for CancelAtAdmission {
    fn readmit(
        &self,
        context: &InvocationContext,
        operation: &str,
        raw: &[u8],
        options: WriteOptions,
    ) -> CommandOutcome {
        self.started.notify_one();
        let mut open = self.gate.lock().unwrap();
        while !*open {
            open = self.release.wait(open).unwrap();
        }
        drop(open);
        assert!(
            options
                .cancellation
                .as_ref()
                .is_some_and(Cancellation::is_cancelled)
        );
        self.runtime.command_with(context, operation, raw, options)
    }
}
#[test]
fn cancellation_drains_the_owned_admission_and_keeps_the_saved_operation_retryable() {
    let h = Harness::new();
    runtime().block_on(async {
        let store = h.open();
        let (user, agent) = principals(&*store);
        let app = Arc::new(boot(composition(&h, store.clone())).await.unwrap());
        app.approval_resumes().drain().await;
        let job = draft(&h, &app, &agent, "Cancelled reviewed job", false);
        let proposal = propose_enable(&h, &app, &agent, &job, false);
        value(
            app.record_human_decision(&user, &decision(&h, &proposal))
                .unwrap(),
        );
        let saved = resume(&*store, &proposal);
        let admission = Arc::new(CancelAtAdmission {
            runtime: app.clone(),
            started: tokio::sync::Notify::new(),
            gate: std::sync::Mutex::new(false),
            release: std::sync::Condvar::new(),
        });
        let driver = devworks_engine::approvals::ResumeDispatcher::new(
            store.clone(),
            app.approvals().clone(),
            admission.clone(),
        );
        let running = driver.clone();
        let task = tokio::spawn(async move { running.tick().await });
        within("saved operation at admission", admission.started.notified()).await;
        driver.stop();
        *admission.gate.lock().unwrap() = true;
        admission.release.notify_all();
        driver.drain().await;
        task.await.unwrap().unwrap();
        assert!(!enabled(&*store, &job));
        assert!(matches!(
            resume(&*store, &proposal).state.as_str(),
            "Pending" | "Unknown"
        ));
        assert_eq!(resume(&*store, &proposal).request_key, saved.request_key);
        // A fresh runtime owns a new cancellation token and catches up the original key.
        drop(driver);
        drop(admission);
        app.shutdown().await;
        drop(app);
        drop(store);
        let store = h.open();
        principals(&*store);
        let app = boot(composition(&h, store.clone())).await.unwrap();
        app.approval_resumes().tick().await.unwrap();
        assert!(enabled(&*store, &job));
        assert_eq!(resume(&*store, &proposal).request_key, saved.request_key);
        app.shutdown().await;
    });
}
