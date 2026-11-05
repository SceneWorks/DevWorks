//! Native recorded grading: terminal conversations and stopped coordinator/worker activations
//! are independently eligible; keyset catch-up survives restart and never evaluates live work.
mod common;
use common::*;
use devworks_adapters::fs::NativeFilesystem;
use devworks_adapters::fs::artifacts::NativeArtifactFiles;
use devworks_adapters::fs::sources::HostArtifactSources;
use devworks_adapters::power::NativePower;
use devworks_adapters::process::NativeProcessHost;
use devworks_core::ports::storage::*;
use devworks_core::protocol::records::EvalSuiteSnapshot;
use devworks_core::protocol::{ActorId, AssignmentId, ProjectId, RootId};
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::diagnostics::DiagnosticService;
use devworks_core::services::resources::ResourceService;
use devworks_core::services::runs::ControlService;
use devworks_core::services::{UseCases, evals::EvalService};
use devworks_engine::evals::EvalSupervisor;
use devworks_engine::{Composition, RunComposition, RuntimeConfig, boot};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

fn suite() -> EvalSuiteSnapshot {
    serde_json::from_value(
        json!({"id":"suite","revision":1,"name":"Recorded proxy", "checks":[{
            "id":"check","ruleRef":"recorded text", "measurement":"Proxy",
            "predicate":{"kind":"TranscriptContains","entryKind":"UserMessage","text":"hello"}
        }]}),
    )
    .unwrap()
}
fn composition(
    h: &Harness,
    store: Arc<dyn ApplicationStore>,
    suites: Vec<EvalSuiteSnapshot>,
) -> Composition {
    let mut config = RuntimeConfig::new(id("eval-dispatcher"));
    config.automatic_eval_suites = suites;
    config.eval_interval = Duration::from_secs(3600);
    Composition {
        store: store.clone(),
        clock: h.clock(),
        ids: h.ids.clone(),
        authority: Arc::new(Authority),
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
        use_cases: UseCases::new(),
        consumers: vec![],
        indexing: None,
        config,
    }
}
fn session(
    store: &dyn ApplicationStore,
    name: &str,
    closed: bool,
    root: Option<&str>,
) -> StoredSession {
    let s = StoredSession {
        id: id(name),
        conversation_id: id(&format!("conversation-{name}")),
        agent_id: ActorId(id("host")),
        project_id: root.map(|_| ProjectId(id("project"))),
        root_id: root.map(|r| RootId(id(r))),
        title: name.into(),
        context_epoch: 0,
        revision: 1,
        created_by: ActorId(id("host")),
        created_at_ms: T0,
        closed_at_ms: closed.then_some(T0),
        closed_by: closed.then(|| ActorId(id("host"))),
        next_sequence: 2,
        checkpoint_sequence: 0,
    };
    let out = s.clone();
    write(store, move |tx| {
        tx.sessions().insert_session(&s)?;
        tx.sessions().insert_entry(&StoredEntry {
            session_id: s.id.clone(),
            sequence: 1,
            entry_id: id(&format!("entry-{name}", name = s.id.as_str())),
            batch_id: id("batch"),
            context_epoch: 0,
            actor: s.agent_id,
            kind: "UserMessage".into(),
            created_at_ms: T0,
            body: json!({"kind":"UserMessage","content":[{"kind":"Text","text":"hello"}]}),
        })
    })
    .unwrap();
    out
}
fn reports(store: &dyn ApplicationStore, name: &str) -> Vec<StoredEvalRun> {
    read(store, |s| s.evals().eval_runs(&id(name), None, 500))
}
fn supervisor(h: &Harness, store: Arc<dyn ApplicationStore>) -> Arc<EvalSupervisor> {
    EvalSupervisor::new(
        store,
        Arc::new(EvalService::new(h.ids.clone(), h.clock())),
        vec![suite()],
    )
}
fn seed_work(h: &Harness) {
    let db = rusqlite::Connection::open(h.root.join("workspace.sqlite")).unwrap();
    db.execute_batch("INSERT INTO projects(id,title,lifecycle,revision,scope_revision,created_at) VALUES('project','P','Active',1,1,0);
        INSERT INTO work_items VALUES('root','project','feature',NULL,NULL,NULL,'Root',1,0,1,1,1,'{}');
        INSERT INTO work_items VALUES('worker','project','task','root',NULL,NULL,'Worker',1,0,1,1,1,'{}');").unwrap();
}
fn activation(store: &dyn ApplicationStore, phase: ActivationPhase) -> StoredActivation {
    write(store, move |tx| {
        let boot = tx.store().boot_id.clone().unwrap();
        let a = StoredActivation {
            id: id("activation"),
            root_id: RootId(id("root")),
            boot_id: boot,
            generation: 1,
            phase,
            purpose: ActivationPurpose::Implementation,
            owner: None,
            lease_until_ms: None,
            owner_epoch: 0,
            revision: 1,
            attempts: 0,
            ready_at_ms: T0,
            created_at_ms: T0,
            stop_cause: None,
            stop_requested_at_ms: None,
            stop_acknowledged_at_ms: None,
            stopped_at_ms: (phase == ActivationPhase::Stopped).then_some(T0),
            outcome: (phase == ActivationPhase::Stopped).then_some(ActivationOutcome::Completed),
            detail: None,
            plan_revision: None,
        };
        tx.runs().insert_activation(&a)?;
        Ok(a)
    })
    .unwrap()
}
fn assignment(store: &dyn ApplicationStore, name: &str, worker: bool, state: AssignmentState) {
    let name = name.to_owned();
    write(store, move |tx| {
        tx.runs().insert_assignment(&StoredAssignment {
            id: AssignmentId(id(&format!("asg-{name}"))),
            root_id: RootId(id("root")),
            work_id: RootId(id(if worker { "worker" } else { "root" })),
            parent_id: worker.then(|| AssignmentId(id("asg-coordinator"))),
            session_id: id(&name),
            execution_id: Some(id("activation")),
            state,
            generation: 1,
            slot_held: false,
            revision: 1,
            checkpoint: None,
            scope_key: None,
            scope_digest: None,
            plan_revision: None,
            instruction_revision: 0,
            created_at_ms: T0,
        })
    })
    .unwrap();
}

#[test]
fn boot_and_restart_catch_up_idempotently_without_grading_active_conversations() {
    let h = Harness::new();
    runtime().block_on(async {
        let store = h.open();
        session(&*store, "closed", true, None);
        session(&*store, "active", false, None);
        let app = boot(composition(&h, store.clone(), vec![suite()]))
            .await
            .unwrap();
        // Synchronize on the durable outcome of the boot-started job, not a time delay.
        within("boot eval", async {
            while reports(&*store, "closed").is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let original = reports(&*store, "closed");
        assert_eq!(original.len(), 1);
        assert_eq!(original[0].report["outcome"], "passed");
        app.evals().tick().await.unwrap();
        assert_eq!(reports(&*store, "closed"), original);
        assert!(reports(&*store, "active").is_empty());
        app.shutdown().await;
        session(&*store, "after-shutdown", true, None);
        assert_eq!(app.evals().tick().await.unwrap(), 0);
        assert!(reports(&*store, "after-shutdown").is_empty());
        drop(app);
        drop(store);
        let store = h.open();
        let restarted = boot(composition(&h, store.clone(), vec![suite()]))
            .await
            .unwrap();
        within("restart eval", async {
            while reports(&*store, "after-shutdown").is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert_eq!(reports(&*store, "closed"), original);
        assert!(reports(&*store, "active").is_empty());
        restarted.shutdown().await;
    });
}

#[test]
fn stopped_activation_grades_open_coordinator_and_worker_and_live_activation_does_not() {
    let h = Harness::new();
    runtime().block_on(async {
        let store = h.open();
        seed_work(&h);
        session(&*store, "coordinator", false, Some("root"));
        session(&*store, "worker-session", false, Some("root"));
        let a = activation(&*store, ActivationPhase::Running);
        assignment(&*store, "coordinator", false, AssignmentState::Waiting);
        assignment(&*store, "worker-session", true, AssignmentState::Waiting);
        let evals = supervisor(&h, store.clone());
        assert_eq!(evals.tick().await.unwrap(), 0);
        write(&*store, move |tx| {
            let mut a = a;
            a.phase = ActivationPhase::Stopped;
            a.revision += 1;
            a.stopped_at_ms = Some(T0);
            a.outcome = Some(ActivationOutcome::Completed);
            tx.runs().update_activation(&a, 1)
        })
        .unwrap();
        assert_eq!(evals.tick().await.unwrap(), 2);
        assert_eq!(reports(&*store, "coordinator").len(), 1);
        assert_eq!(reports(&*store, "worker-session").len(), 1);
        assert!(
            !read(&*store, |s| s.sessions().session(&id("worker-session")))
                .unwrap()
                .closed()
        );
        evals.drain().await;
    });
}

#[test]
fn complete_keyset_sweep_reaches_sessions_older_than_one_page() {
    let h = Harness::new();
    runtime().block_on(async {
        let store = h.open();
        for n in 0..130 {
            session(&*store, &format!("session-{n:03}"), true, None);
        }
        let evals = supervisor(&h, store.clone());
        assert_eq!(evals.tick().await.unwrap(), 130);
        for n in 0..130 {
            assert_eq!(reports(&*store, &format!("session-{n:03}")).len(), 1);
        }
        evals.drain().await;
    });
}

#[test]
fn unconfigured_runtime_does_not_grade_recorded_sessions() {
    let h = Harness::new();
    runtime().block_on(async {
        let store = h.open();
        session(&*store, "closed", true, None);
        let app = boot(composition(&h, store.clone(), vec![])).await.unwrap();
        assert_eq!(app.evals().tick().await.unwrap(), 0);
        assert!(reports(&*store, "closed").is_empty());
        app.shutdown().await;
    });
}
