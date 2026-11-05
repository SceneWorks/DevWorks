//! Native next-work reservations and root handoff against SQLite; no provider execution.
mod common;
use common::*;
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::storage::*;
use devworks_core::protocol::results::DomainCommandReply as CommandReply;
use devworks_core::protocol::results::{DomainDecision, OperationResult};
use devworks_core::protocol::{Id, ProjectId, RootId};
use devworks_core::services::UseCases;
use devworks_core::services::runs::scope::ScriptedScope;
use devworks_core::services::runs::{ControlService, control_of};
use devworks_core::services::work::{WorkService, allocation};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Fx {
    h: Harness,
    store: Arc<dyn ApplicationStore>,
    cases: UseCases,
    control: Arc<ControlService>,
    scope: Arc<ScriptedScope>,
    user: InvocationContext,
    keys: AtomicUsize,
}
impl Fx {
    fn new() -> Self {
        let h = Harness::new();
        let store = h.open();
        rusqlite::Connection::open(h.root.join("workspace.sqlite")).unwrap().execute_batch("INSERT INTO projects(id,title,lifecycle,revision,scope_revision,created_at) VALUES('project','P','Active',1,1,0);").unwrap();
        let user = principal(&*store, "user", PrincipalKind::Human);
        let scope = Arc::new(ScriptedScope::new());
        let control = Arc::new(ControlService::with_scope(h.ids.clone(), scope.clone()));
        let cases = UseCases::new()
            .with_runs(control.clone())
            .with_sessions(Arc::new(
                devworks_core::services::sessions::SessionService::new(
                    h.ids.clone(),
                    store.clone(),
                ),
            ))
            .with_work(Arc::new(WorkService::new(h.ids.clone(), control.clone())));
        Self {
            h,
            store,
            cases,
            control,
            scope,
            user,
            keys: AtomicUsize::new(0),
        }
    }
    fn gateway(&self) -> Gateway<'_> {
        Gateway {
            authority: &Authority,
            store: &*self.store,
            ids: &*self.h.ids,
        }
    }
    fn frame(&self, op: &str, body: Value) -> Value {
        let mut body = body;
        body["kind"] = json!(op);
        if op.starts_with("runs.") {
            return json!({"protocol":"1.0-draft.1","requestKey":format!("key-{}",self.keys.fetch_add(1,Ordering::SeqCst)),"command":body});
        }
        json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":1,"requestKey":format!("key-{}",self.keys.fetch_add(1,Ordering::SeqCst)),"body":body})
    }
    fn send(&self, who: &InvocationContext, op: &str, frame: &Value) -> CommandReply {
        self.cases
            .command(&self.gateway(), who, op, frame.to_string().as_bytes())
            .unwrap()
    }
    fn command(&self, op: &str, body: Value) -> CommandReply {
        self.send(&self.user, op, &self.frame(op, body))
    }
    fn queue(&self, feature: Option<&str>) -> i64 {
        read(&*self.store, |s| {
            s.work().queue_revision(
                &ProjectId(id("project")),
                &feature.map_or_else(|| PROJECT_QUEUE.into(), |f| feature_queue(&id(f))),
            )
        })
    }
    fn claim(&self) -> CommandReply {
        self.command("next_work.claim_project_queue",json!({"projectId":"project","expectedQueueRevision":self.queue(None).to_string(),"leaseMs":"1000"}))
    }
    fn issue(&self, name: &str, priority: i32) -> Id {
        let out=applied(self.command("issues.create",json!({"projectId":"project","title":name,"description":"clear actionable issue","repositoryIds":[],"statusId":"project-ready","priority":priority})));
        let OperationResult::IssuesCreate { value } = out else {
            panic!()
        };
        value.id
    }
    fn seed(&self, name: &str, feature: Option<&str>, kind: &str, priority: i64) {
        let (name, parent, kind) = (name.to_owned(), feature.map(str::to_owned), kind.to_owned());
        write(&*self.store, move |tx| {
            devworks_core::services::work::workflow::ensure_workflow(
                tx,
                &ProjectId(id("project")),
            )?;
            tx.work().insert_work(&StoredWork {
                id: id(&name),
                project_id: ProjectId(id("project")),
                kind,
                parent_feature_id: parent.map(|p| id(&p)),
                status_id: Some(id("project-ready")),
                title: name.clone(),
                priority,
                manual_order: 0,
                revision: 1,
                scope_revision: 1,
                created_at_ms: 1,
                updated_at_ms: 1,
                created_by: None,
                updated_by: None,
                search_key: name,
                caller_ref: None,
                completion: None,
                brief: json!({}),
            })
        })
        .unwrap();
    }
    fn session(&self, work: &Id) -> Id {
        let result = serde_json::to_value(applied(self.command(
            "sessions.start",
            json!({"scope":{"kind":"Project","projectId":"project"},"rootId":work,"title":"Work"}),
        )))
        .unwrap();
        id(result["value"]["id"]
            .as_str()
            .or_else(|| result["value"]["sessionId"].as_str())
            .unwrap())
    }
    fn start(&self, work: &Id) {
        let w = read(&*self.store, |s| s.work().work(work)).unwrap();
        let root = RootId(work.clone());
        let c = read(&*self.store, |s| control_of(s.runs(), &root));
        applied(self.command("runs.start",json!({"rootId":work,"expectedWorkRevision":w.revision.to_string(),"expectedControlRevision":c.revision.to_string()})));
    }
}
fn applied(reply: CommandReply) -> OperationResult {
    match reply {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied { result } | DomainDecision::NoChange { result } => result,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}
fn claim(reply: CommandReply) -> Value {
    let result = serde_json::to_value(applied(reply)).unwrap();
    result["value"].clone()
}
fn refused(reply: CommandReply) {
    assert!(
        !matches!(reply,CommandReply::Recorded{receipt,..}if matches!(receipt.decision,DomainDecision::Applied{..}|DomainDecision::NoChange{..}))
    )
}

#[test]
fn priority_filters_blockers_before_ranking_and_replay_does_not_allocate_twice() {
    let f = Fx::new();
    let prerequisite = f.issue("prerequisite", 1);
    let blocked = f.issue("blocked", 100);
    applied(f.command("tasks.dependency_links",json!({"taskId":blocked,"expectedRevision":"1","replacement":{"edges":[{"predecessor":prerequisite,"successor":blocked}]}})));
    let frame=f.frame("next_work.claim_project_queue",json!({"projectId":"project","expectedQueueRevision":f.queue(None).to_string(),"leaseMs":"1000"}));
    let first = claim(f.send(&f.user, "next_work.claim_project_queue", &frame));
    assert_eq!(first["workId"], json!(prerequisite));
    let replay = claim(f.send(&f.user, "next_work.claim_project_queue", &frame));
    assert_eq!(replay, first);
    assert_eq!(
        read(&*f.store, |s| allocation::claims(
            s.work(),
            &ProjectId(id("project"))
        ))
        .len(),
        1
    );
    assert_eq!(
        claim(f.claim())["id"],
        first["id"],
        "same owner reuses its reservation"
    );
    let other = principal(&*f.store, "other", PrincipalKind::Human);
    refused(f.send(
        &other,
        "next_work.release_claim",
        &f.frame(
            "next_work.release_claim",
            json!({"claimId":first["id"],"expectedFence":first["fence"]}),
        ),
    ));
}

#[test]
fn status_and_membership_changes_fence_unstarted_reservations_and_pause_survives_relink() {
    let f = Fx::new();
    let issue = f.issue("issue", 2);
    let reserved = claim(f.claim());
    applied(f.command("tasks.move",json!({"taskId":issue,"expectedRevision":"1","statusId":"project-backlog","expectedWorkflowRevision":"1"})));
    let stored = read(&*f.store, |s| {
        s.work()
            .allocation_claim(&id(reserved["id"].as_str().unwrap()))
    })
    .unwrap();
    assert_eq!(stored.state, "Invalidated");
    f.seed("feature", None, "feature", 1);
    let w = read(&*f.store, |s| s.work().work(&issue)).unwrap();
    applied(f.command("tasks.set_feature",json!({"taskId":issue,"expectedRevision":w.revision.to_string(),"featureId":"feature","clearFeature":false})));
    let w = read(&*f.store, |s| s.work().work(&issue)).unwrap();
    applied(f.command(
        "tasks.set_feature",
        json!({"taskId":issue,"expectedRevision":w.revision.to_string(),"clearFeature":true}),
    ));
    let w = read(&*f.store, |s| s.work().work(&issue)).unwrap();
    applied(f.command("tasks.set_feature",json!({"taskId":issue,"expectedRevision":w.revision.to_string(),"featureId":"feature","clearFeature":false})));
    let root = RootId(issue);
    assert_eq!(
        read(&*f.store, |s| control_of(s.runs(), &root)).desired,
        DesiredExecution::Paused
    );
}

#[test]
fn blocked_root_yields_only_after_quiescence_and_returns_with_coordinator_lineage() {
    let f = Fx::new();
    f.seed("feature-a", None, "feature", 9);
    f.seed("a-task", Some("feature-a"), "task", 9);
    f.scope.approve(&RootId(id("feature-a")), 1, &[]);
    let prerequisite = f.issue("prerequisite", 1);
    let selected = claim(f.claim());
    assert_eq!(selected["workId"], "feature-a");
    f.start(&id("feature-a"));
    let session = f.session(&id("feature-a"));
    let service = f.control.clone();
    let session_for_assignment = session.clone();
    let coordinator = write(&*f.store, move |tx| {
        let actor = tx.store().host_actor.clone();
        service
            .register_assignment(
                tx,
                &actor,
                devworks_core::services::runs::substrate::NewRunAssignment {
                    root: RootId(id("feature-a")),
                    work: RootId(id("feature-a")),
                    parent: None,
                    session: session_for_assignment,
                    scope_key: None,
                },
            )
            .map(|result| result.expect("coordinator"))
    })
    .unwrap();
    let activation = read(&*f.store, |s| {
        s.runs().live_activation(&RootId(id("feature-a")))
    })
    .unwrap();
    let before = activation.id.clone();
    // A concurrent dependency edit makes the remaining task wait on a root in another queue.
    applied(f.command("tasks.dependency_links",json!({"taskId":"a-task","expectedRevision":"1","replacement":{"edges":[{"predecessor":prerequisite,"successor":"a-task"}]}})));
    let control = f.control.clone();
    write(&*f.store, move |tx| {
        allocation::reconcile_project(tx, &control, &ProjectId(id("project")))
    })
    .unwrap();
    let yielded = read(&*f.store, |s| s.runs().activation(&before)).unwrap();
    assert_eq!(yielded.phase, ActivationPhase::Stopped);
    // Pending activation never launched: another boundary verifies quiescence before release.
    let control = f.control.clone();
    write(&*f.store, move |tx| {
        allocation::reconcile_project(tx, &control, &ProjectId(id("project")))
    })
    .unwrap();
    let next = claim(f.claim());
    assert_eq!(next["workId"], json!(prerequisite));
    applied(f.command(
        "next_work.release_claim",
        json!({"claimId":next["id"],"expectedFence":next["fence"]}),
    ));
    let prereq = prerequisite.clone();
    write(&*f.store, move |tx| {
        let mut w = tx.work().work(&prereq)?.unwrap();
        let expected = w.revision;
        w.revision += 1;
        w.status_id = Some(id("project-done"));
        w.completion = Some(VERIFIED.into());
        tx.work().update_work(&w, expected)
    })
    .unwrap();
    let resumed = claim(f.claim());
    assert_eq!(resumed["workId"], "feature-a");
    f.start(&id("feature-a"));
    assert_eq!(
        read(&*f.store, |s| devworks_core::services::runs::coordinator(
            s.runs(),
            &RootId(id("feature-a"))
        ))
        .unwrap()
        .id,
        coordinator.id,
        "resuming retains coordinator/session lineage"
    );
    assert_ne!(
        read(&*f.store, |s| s
            .runs()
            .live_activation(&RootId(id("feature-a"))))
        .unwrap()
        .id,
        before
    );
    assert!(
        read(&*f.store, |s| s.runs().blockers(
            &BlockerFilter::Root(RootId(id("feature-a"))),
            None,
            500
        ))
        .is_empty(),
        "yield did not invent a manual pause"
    );
}

#[test]
fn an_open_conversation_without_a_current_owner_does_not_pin_an_expired_reservation() {
    let f = Fx::new();
    let issue = f.issue("issue", 1);
    let session = f.session(&issue);
    let context = f.user.clone().with_session(session.clone());
    let selected=claim(f.send(&context,"next_work.claim_project_queue",&f.frame("next_work.claim_project_queue",json!({"projectId":"project","expectedQueueRevision":f.queue(None).to_string(),"leaseMs":"1000"}))));
    f.h.advance(1001);
    assert!(
        !read(&*f.store, |s| s.sessions().session(&session))
            .unwrap()
            .closed()
    );
    let other = principal(&*f.store, "recovery", PrincipalKind::Human);
    let recovered=claim(f.send(&other,"next_work.claim_project_queue",&f.frame("next_work.claim_project_queue",json!({"projectId":"project","expectedQueueRevision":f.queue(None).to_string(),"leaseMs":"1000"}))));
    assert_eq!(recovered["workId"], json!(issue));
    assert_ne!(recovered["id"], selected["id"]);
    assert!(
        recovered["fence"].as_str().unwrap().parse::<i64>().unwrap()
            > selected["fence"].as_str().unwrap().parse::<i64>().unwrap()
    );
}

#[test]
fn only_a_current_fenced_turn_renews_a_session_reservation() {
    let f = Fx::new();
    let owner = f.issue("owner", 2);
    let work = f.issue("next", 1);
    f.start(&owner);
    let session = f.session(&owner);
    let service = f.control.clone();
    let sid = session.clone();
    let owned_root = RootId(owner.clone());
    let activation = write(&*f.store, move |tx| {
        let a = service
            .claim_activation(tx, &id("supervisor"))?
            .expect("activation");
        let a = service
            .start_activation(tx, &a.id, a.owner_epoch)?
            .expect("running");
        let actor = tx.store().host_actor.clone();
        let assignment = service
            .register_assignment(
                tx,
                &actor,
                devworks_core::services::runs::substrate::NewRunAssignment {
                    root: owned_root.clone(),
                    work: owned_root,
                    parent: None,
                    session: sid.clone(),
                    scope_key: None,
                },
            )?
            .expect("assignment");
        service
            .begin_turn(tx, &sid, &assignment.id, &a.id, a.owner_epoch)?
            .expect("turn");
        Ok(a)
    })
    .unwrap();
    let context = f.user.clone().with_session(session.clone());
    let reserved=claim(f.send(&context,"next_work.claim_project_queue",&f.frame("next_work.claim_project_queue",json!({"projectId":"project","expectedQueueRevision":f.queue(None).to_string(),"leaseMs":"1000"}))));
    assert_eq!(reserved["workId"], json!(work));
    f.h.advance(1001);
    let service = f.control.clone();
    write(&*f.store, move |tx| {
        allocation::reconcile_project(tx, &service, &ProjectId(id("project")))
    })
    .unwrap();
    let c = read(&*f.store, |s| {
        s.work()
            .allocation_claim(&id(reserved["id"].as_str().unwrap()))
    })
    .unwrap();
    assert_eq!(c.expires_at_ms, activation.lease_until_ms.unwrap());
    let service = f.control.clone();
    write(&*f.store, move |tx| {
        service
            .finish_activation(
                tx,
                &activation.id,
                activation.owner_epoch,
                ActivationOutcome::Completed,
                None,
            )
            .map(|result| result.expect("finish"))
    })
    .unwrap();
    assert!(
        !read(&*f.store, |s| s.sessions().session(&session))
            .unwrap()
            .closed()
    );
    f.h.advance(30_000);
    let other = principal(&*f.store, "recovery", PrincipalKind::Human);
    let recovered=claim(f.send(&other,"next_work.claim_project_queue",&f.frame("next_work.claim_project_queue",json!({"projectId":"project","expectedQueueRevision":f.queue(None).to_string(),"leaseMs":"1000"}))));
    assert_ne!(recovered["id"], reserved["id"]);
}
