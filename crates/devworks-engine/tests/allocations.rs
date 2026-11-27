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
        Self::over(h, store)
    }
    fn over(h: Harness, store: Arc<dyn ApplicationStore>) -> Self {
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
            .with_work(Arc::new(
                WorkService::new(h.ids.clone(), control.clone()).with_clock(h.clock()),
            ));
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
    fn command3(&self, op: &str, body: Value) -> CommandReply {
        let mut frame = self.frame(op, body);
        frame["schemaRevision"] = json!(3);
        self.send(&self.user, op, &frame)
    }
    fn next3(&self, mut filters: Value) -> CommandReply {
        filters["projectId"] = json!("project");
        filters["expectedQueueRevision"] = json!(self.queue(None).to_string());
        filters["leaseMs"] = json!("1000");
        self.command3("next_work.claim_project_queue", filters)
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
    assert_eq!(
        stored.state,
        devworks_core::ports::storage::ClaimState::Invalidated
    );
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
    let service = f.control.clone();
    write(&*f.store, move |tx| {
        let a = service
            .claim_activation(tx, &id("supervisor"))?
            .expect("resumed activation");
        service
            .start_activation(tx, &a.id, a.owner_epoch)
            .map(|r| r.expect("resumed"))
    })
    .unwrap();
    let transferred = read(&*f.store, |s| {
        s.work()
            .allocation_claim(&id(resumed["id"].as_str().unwrap()))
    })
    .unwrap();
    assert_eq!(transferred.session_id.as_ref(), Some(&session));
    assert_eq!(
        transferred.owner_id,
        read(&*f.store, |s| s.sessions().session(&session))
            .unwrap()
            .agent_id
    );
    refused(f.command(
        "next_work.release_claim",
        json!({"claimId":resumed["id"],"expectedFence":resumed["fence"]}),
    ));
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

#[test]
fn reprioritization_preserves_a_reservation_and_fresh_start_rechecks_its_current_work() {
    let f = Fx::new();
    let selected = f.issue("selected", 5);
    let _ = f.issue("other", 1);
    let before = claim(f.claim());
    applied(f.command(
        "issues.edit",
        json!({"workId":selected,"expectedRevision":"1","changes":{"priority":0}}),
    ));
    let service = f.control.clone();
    write(&*f.store, move |tx| {
        allocation::reconcile_project(tx, &service, &ProjectId(id("project")))
    })
    .unwrap();
    let after = read(&*f.store, |s| {
        s.work()
            .allocation_claim(&id(before["id"].as_str().unwrap()))
    })
    .unwrap();
    assert_eq!(
        after.state,
        devworks_core::ports::storage::ClaimState::Active
    );
    assert_eq!(after.fence.to_string(), before["fence"]);
    assert_eq!(after.owner_id, f.user.principal().clone());
    f.start(&selected);
    let transferred = read(&*f.store, |s| s.work().allocation_claim(&after.id)).unwrap();
    assert_eq!(
        transferred.state,
        devworks_core::ports::storage::ClaimState::Transferred
    );
}

#[test]
fn an_eligible_item_after_the_first_storage_page_is_not_reported_as_no_work() {
    let f = Fx::new();
    write(&*f.store, |tx| {
        devworks_core::services::work::workflow::ensure_workflow(tx, &ProjectId(id("project")))?;
        for n in 0..=500 {
            let name = format!("item-{n:04}");
            tx.work().insert_work(&StoredWork {
                id: id(&name),
                project_id: ProjectId(id("project")),
                kind: TASK.into(),
                parent_feature_id: None,
                status_id: Some(id(if n == 500 {
                    "project-ready"
                } else {
                    "project-backlog"
                })),
                title: name.clone(),
                priority: 1000 - i64::from(n),
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
            })?;
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(claim(f.claim())["workId"], "item-0500");
}

#[test]
fn a_completed_task_can_release_its_claim_while_its_feature_coordinator_keeps_running() {
    let f = Fx::new();
    f.seed("feature", None, "feature", 1);
    f.seed("task-a", Some("feature"), "task", 2);
    f.seed("task-b", Some("feature"), "task", 1);
    f.scope.approve(&RootId(id("feature")), 1, &[]);
    f.start(&id("feature"));
    let coordinator_session = f.session(&id("feature"));
    let worker_session = f.session(&id("feature"));
    let service = f.control.clone();
    let sid = coordinator_session.clone();
    let (activation, coordinator) = write(&*f.store, move |tx| {
        let a = service.claim_activation(tx, &id("supervisor"))?.unwrap();
        let a = service.start_activation(tx, &a.id, a.owner_epoch)?.unwrap();
        let actor = tx.store().host_actor.clone();
        let coordinator = service
            .register_assignment(
                tx,
                &actor,
                devworks_core::services::runs::substrate::NewRunAssignment {
                    root: RootId(id("feature")),
                    work: RootId(id("feature")),
                    parent: None,
                    session: sid,
                    scope_key: None,
                },
            )?
            .unwrap();
        Ok((a, coordinator))
    })
    .unwrap();
    let selected=claim(f.command("next_work.claim_feature_task",json!({"rootId":"feature","expectedTaskSetRevision":f.queue(Some("feature")).to_string(),"leaseMs":"1000"})));
    assert_eq!(selected["workId"], "task-a");
    let service = f.control.clone();
    let sid = worker_session.clone();
    let parent = coordinator.id.clone();
    let worker = write(&*f.store, move |tx| {
        let actor = tx.store().host_actor.clone();
        service
            .register_assignment(
                tx,
                &actor,
                devworks_core::services::runs::substrate::NewRunAssignment {
                    root: RootId(id("feature")),
                    work: RootId(id("task-a")),
                    parent: Some(parent),
                    session: sid,
                    scope_key: None,
                },
            )
            .map(|r| r.unwrap())
    })
    .unwrap();
    let service = f.control.clone();
    write(&*f.store, move |tx| {
        let actor = tx.store().host_actor.clone();
        service
            .set_assignment_state(
                tx,
                &actor,
                &worker.id,
                worker.revision,
                AssignmentState::Completed,
                None,
            )
            .map(|r| r.unwrap())
    })
    .unwrap();
    applied(f.command("tasks.move",json!({"taskId":"task-a","expectedRevision":"1","statusId":"project-done","expectedWorkflowRevision":"1"})));
    let transferred = read(&*f.store, |s| {
        s.work()
            .allocation_claim(&id(selected["id"].as_str().unwrap()))
    })
    .unwrap();
    assert_eq!(&transferred.owner_id, f.user.principal());
    let context = f.user.clone().with_session(worker_session);
    applied(f.send(
        &context,
        "next_work.release_claim",
        &f.frame(
            "next_work.release_claim",
            json!({"claimId":transferred.id,"expectedFence":transferred.fence.to_string()}),
        ),
    ));
    assert_eq!(
        read(&*f.store, |s| s.runs().activation(&activation.id))
            .unwrap()
            .phase,
        ActivationPhase::Running
    );
    assert_eq!(claim(f.command("next_work.claim_feature_task",json!({"rootId":"feature","expectedTaskSetRevision":f.queue(Some("feature")).to_string(),"leaseMs":"1000"})))["workId"],"task-b");
}

#[test]
fn restart_quiescence_releases_old_project_ownership_but_keeps_manual_continue_holds() {
    let f = Fx::new();
    let interrupted = f.issue("interrupted", 5);
    let independent = f.issue("independent", 1);
    let reserved = claim(f.claim());
    f.start(&interrupted);
    let session = f.session(&interrupted);
    let service = f.control.clone();
    let sid = session.clone();
    let root = RootId(interrupted.clone());
    write(&*f.store, move |tx| {
        let a = service
            .claim_activation(tx, &id("old-supervisor"))?
            .unwrap();
        let a = service.start_activation(tx, &a.id, a.owner_epoch)?.unwrap();
        let actor = tx.store().host_actor.clone();
        service
            .register_assignment(
                tx,
                &actor,
                devworks_core::services::runs::substrate::NewRunAssignment {
                    root: root.clone(),
                    work: root,
                    parent: None,
                    session: sid,
                    scope_key: None,
                },
            )?
            .unwrap();
        Ok(a)
    })
    .unwrap();
    let Fx {
        h,
        store,
        cases,
        control,
        scope,
        user,
        keys: _,
    } = f;
    drop(cases);
    drop(control);
    drop(scope);
    drop(store);
    let store = h.open();
    let scope = Arc::new(ScriptedScope::new());
    let control = Arc::new(ControlService::with_scope(h.ids.clone(), scope.clone()));
    let cases = UseCases::new()
        .with_runs(control.clone())
        .with_sessions(Arc::new(
            devworks_core::services::sessions::SessionService::new(h.ids.clone(), store.clone()),
        ))
        .with_work(Arc::new(
            WorkService::new(h.ids.clone(), control.clone()).with_clock(h.clock()),
        ));
    let f = Fx {
        h,
        store,
        cases,
        control,
        scope,
        user,
        keys: AtomicUsize::new(1000),
    };
    let service = f.control.clone();
    write(&*f.store, move |tx| {
        service.recover_boot(
            tx,
            &devworks_core::services::runs::recovery::NoLifecycleHooks,
        )
    })
    .unwrap();
    let service = f.control.clone();
    write(&*f.store, move |tx| {
        allocation::reconcile_project(tx, &service, &ProjectId(id("project")))
    })
    .unwrap();
    assert_eq!(
        read(&*f.store, |s| s
            .work()
            .allocation_claim(&id(reserved["id"].as_str().unwrap())))
        .unwrap()
        .state,
        devworks_core::ports::storage::ClaimState::Released
    );
    assert!(
        !read(&*f.store, |s| s.sessions().session(&session))
            .unwrap()
            .closed()
    );
    assert!(
        read(&*f.store, |s| s.runs().blockers(
            &BlockerFilter::RootLevel(RootId(interrupted.clone())),
            None,
            500
        ))
        .iter()
        .any(|b| b.cause == HoldCause::RestartContinue)
    );
    assert_eq!(claim(f.claim())["workId"], json!(independent));
}

fn outcome(reply: CommandReply) -> Value {
    serde_json::to_value(applied(reply)).unwrap()["value"].clone()
}

#[test]
fn source_filters_secondary_keys_and_typed_no_work_are_durable_without_allocating_twice() {
    let f = Fx::new();
    let bug = f.issue("bug", 2);
    let task = f.issue("task", 9);
    applied(f.command3("issues.edit",json!({"workId":bug,"expectedRevision":"1","changes":{"taskType":"Bug","labels":["Urgent"]}})));
    applied(f.command3("issues.edit",json!({"workId":task,"expectedRevision":"1","changes":{"taskType":"Task","labels":["Urgent"]}})));
    let first = outcome(
        f.next3(json!({"taskType":"Bug","label":" URGENT ","idempotencyKey":"bug-selection"})),
    );
    assert_eq!(first["kind"], "Claimed");
    assert_eq!(first["claim"]["workId"], json!(bug));
    assert_eq!(first["reused"], false);
    let _ = f.issue("later", 100); // advance queue; secondary-key replay still finds the first allocation
    let replay = outcome(
        f.next3(json!({"taskType":"Bug","label":"urgent","idempotencyKey":"bug-selection"})),
    );
    assert_eq!(replay["claim"]["id"], first["claim"]["id"]);
    assert_eq!(replay["reused"], true);
    refused(f.next3(json!({"taskType":"Task","label":"urgent","idempotencyKey":"bug-selection"})));
    let another = outcome(f.next3(json!({"another":true,"taskType":"Task","label":"urgent"})));
    assert_eq!(another["claim"]["workId"], json!(task));
    assert_ne!(another["claim"]["id"], first["claim"]["id"]);
    let none = outcome(
        f.next3(json!({"another":true,"label":"absent","idempotencyKey":"empty-selection"})),
    );
    assert_eq!(none["kind"], "NoEligible");
    assert_eq!(none["reason"], "filter");
    assert_eq!(none["reused"], false);
    let replay = outcome(
        f.next3(json!({"another":true,"label":"absent","idempotencyKey":"empty-selection"})),
    );
    assert_eq!(replay["kind"], "NoEligible");
    assert_eq!(replay["reused"], true);
    applied(f.command(
        "next_work.release_claim",
        json!({"claimId":first["claim"]["id"],"expectedFence":first["claim"]["fence"]}),
    ));
    refused(f.next3(json!({"taskType":"Bug","label":"urgent","idempotencyKey":"bug-selection"})));
}

#[test]
fn an_additional_reservation_cannot_start_until_the_running_root_quiesces() {
    let f = Fx::new();
    let a = f.issue("a", 2);
    let b = f.issue("b", 1);
    let first = outcome(f.next3(json!({})));
    assert_eq!(first["claim"]["workId"], json!(a));
    f.start(&a);
    let second = outcome(f.next3(json!({"another":true})));
    assert_eq!(second["claim"]["workId"], json!(b));
    let control = read(&*f.store, |s| control_of(s.runs(), &RootId(b.clone())));
    let reply = f.command("runs.start",json!({"rootId":b,"expectedWorkRevision":"1","expectedControlRevision":control.revision.to_string()}));
    assert!(
        matches!(&reply, CommandReply::Recorded { receipt, .. }
            if matches!(&receipt.decision, DomainDecision::Refused { fault }
                if fault.code == devworks_core::protocol::DomainErrorCode::Conflict
                    && fault.target_kind.as_ref().is_some_and(|kind| kind.0 == "project_execution_owned"))),
        "{reply:?}"
    );
    assert!(read(&*f.store, |s| s.runs().live_activation(&RootId(b))).is_none());
}

#[test]
fn a_manual_start_of_an_unclaimed_root_runs_beside_a_claimed_root() {
    let f = Fx::new();
    let a = f.issue("claimed", 2);
    let b = f.issue("manual", 1);
    let reserved = outcome(f.next3(json!({})));
    assert_eq!(reserved["claim"]["workId"], json!(a));
    f.start(&a);
    let claim_id = id(reserved["claim"]["id"].as_str().unwrap());
    assert_eq!(
        read(&*f.store, |s| s.work().allocation_claim(&claim_id))
            .unwrap()
            .state,
        ClaimState::Transferred
    );
    f.start(&b);
    read(&*f.store, |s| {
        assert!(s.runs().live_activation(&RootId(a))?.is_some());
        assert!(s.runs().live_activation(&RootId(b))?.is_some());
        Ok(())
    });
}

fn start_reply(f: &Fx, root: &Id) -> CommandReply {
    let (control, revision) = read(&*f.store, |s| {
        Ok((
            control_of(s.runs(), &RootId(root.clone()))?,
            s.work().work(root)?.unwrap().revision,
        ))
    });
    f.command("runs.start",json!({"rootId":root,"expectedWorkRevision":revision.to_string(),"expectedControlRevision":control.revision.to_string()}))
}

fn execution_owned(reply: &CommandReply) -> bool {
    matches!(reply, CommandReply::Recorded { receipt, .. }
        if matches!(&receipt.decision, DomainDecision::Refused { fault }
            if fault.code == devworks_core::protocol::DomainErrorCode::Conflict
                && fault.target_kind.as_ref().is_some_and(|kind| kind.0 == "project_execution_owned")))
}

#[test]
fn a_yielding_claim_without_a_live_activation_still_excludes_claimed_but_not_manual_starts() {
    let f = Fx::new();
    let a = f.issue("yielding", 5);
    let b = f.issue("claimed", 3);
    let prerequisite = f.issue("prerequisite", 1);
    let manual = f.issue("manual", 0);
    let first = outcome(f.next3(json!({})));
    assert_eq!(first["claim"]["workId"], json!(a));
    f.start(&a);
    applied(f.command(
        "tasks.dependency_links",
        json!({"taskId":a,"expectedRevision":"1","replacement":{"edges":[
            {"predecessor":prerequisite,"successor":a}
        ]}}),
    ));
    let control = f.control.clone();
    write(&*f.store, move |tx| {
        allocation::reconcile_project(tx, &control, &ProjectId(id("project")))
    })
    .unwrap();
    let first_id = id(first["claim"]["id"].as_str().unwrap());
    read(&*f.store, |s| {
        assert_eq!(
            s.work().allocation_claim(&first_id)?.unwrap().state,
            ClaimState::Yielding
        );
        assert!(s.runs().live_activation(&RootId(a.clone()))?.is_none());
        Ok(())
    });
    let second = outcome(f.next3(json!({"another":true})));
    assert_eq!(second["claim"]["workId"], json!(b));
    let reply = start_reply(&f, &b);
    assert!(execution_owned(&reply), "{reply:?}");
    assert!(read(&*f.store, |s| s.runs().live_activation(&RootId(b))).is_none());
    f.start(&manual);
    assert!(read(&*f.store, |s| s.runs().live_activation(&RootId(manual))).is_some());
}

#[test]
fn a_feature_queue_claim_on_a_child_task_does_not_make_its_feature_claimed() {
    let f = Fx::new();
    let a = f.issue("claimed", 5);
    f.seed("feature-b", None, "feature", 0);
    f.seed("b-task", Some("feature-b"), "task", 0);
    f.scope.approve(&RootId(id("feature-b")), 1, &[]);
    let first = outcome(f.next3(json!({})));
    assert_eq!(first["claim"]["workId"], json!(a));
    f.start(&a);
    let child = claim(f.command("next_work.claim_feature_task",json!({"rootId":"feature-b","expectedTaskSetRevision":f.queue(Some("feature-b")).to_string(),"leaseMs":"30000"})));
    assert_eq!(child["workId"], "b-task");
    read(&*f.store, |s| {
        let child = s
            .work()
            .allocation_claim(&id(child["id"].as_str().unwrap()))?
            .unwrap();
        assert_eq!(child.root_id, RootId(id("feature-b")));
        assert_ne!(child.queue_key, PROJECT_QUEUE);
        assert!(child.state.is_live());
        assert!(
            s.runs()
                .live_activation(&RootId(id("feature-b")))?
                .is_none()
        );
        Ok(())
    });
    f.start(&id("feature-b"));
    read(&*f.store, |s| {
        assert!(s.runs().live_activation(&RootId(a))?.is_some());
        assert!(
            s.runs()
                .live_activation(&RootId(id("feature-b")))?
                .is_some()
        );
        Ok(())
    });
}

#[test]
fn automatic_project_claim_uses_priority_and_start_requires_that_exact_claim_and_archive_epoch() {
    let _native_case = native_runtime_case();
    let f = Fx::new();
    let recovering = f.issue("recovering", 1);
    applied(f.command(
        "tasks.move",
        json!({"taskId":recovering, "expectedRevision":"1",
            "statusId":"project-in-progress","expectedWorkflowRevision":"1"}),
    ));
    let fresh = f.issue("fresh", 100);
    let other_project = ProjectId(id("different-project"));
    let project = ProjectId(id("project"));
    let ingress = devworks_core::context::HostIngress::new(f.store.as_ref());
    let context = ingress
        .keep_working_context(&f.user, project.clone(), 0, id("automatic"))
        .unwrap();
    let foreign = ingress
        .keep_working_context(&f.user, project.clone(), 0, id("foreign"))
        .unwrap();
    let different = ingress
        .keep_working_context(&f.user, other_project, 0, id("different"))
        .unwrap();
    let select = |ctx: &InvocationContext| {
        let mut frame = f.frame("next_work.claim_project_queue", json!({
            "projectId":"project","expectedQueueRevision":f.queue(None).to_string(),"leaseMs":"1000"
        }));
        frame["schemaRevision"] = json!(3);
        f.send(ctx, "next_work.claim_project_queue", &frame)
    };
    refused(select(&different));
    let selected = outcome(select(&context));
    assert_eq!(selected["claim"]["workId"], json!(fresh));
    let start = |ctx: &InvocationContext, root: &Id| {
        let (control, revision) = read(&*f.store, |s| {
            Ok((
                control_of(s.runs(), &RootId(root.clone()))?,
                s.work().work(root)?.unwrap().revision,
            ))
        });
        f.send(ctx, "runs.start", &f.frame("runs.start", json!({
            "rootId":root,"expectedWorkRevision":revision.to_string(),"expectedControlRevision":control.revision.to_string()
        })))
    };
    refused(start(&foreign, &fresh));
    applied(f.command(
        "tasks.dependency_links",
        json!({"taskId":fresh,"expectedRevision":"1","replacement":{"edges":[
            {"predecessor":recovering,"successor":fresh}
        ]}}),
    ));
    let blocked_start = start(&context, &fresh);
    assert!(
        matches!(blocked_start, CommandReply::Recorded { receipt, .. }
        if matches!(&receipt.decision, DomainDecision::Refused { fault }
            if fault.code == devworks_core::protocol::DomainErrorCode::LeaseLost))
    );
    let reselected = outcome(select(&context));
    assert_eq!(reselected["claim"]["workId"], json!(recovering));
    let archive_project = project.clone();
    write(&*f.store, move |tx| {
        let mut record = tx.projects().project(&archive_project)?.unwrap();
        let expected = record.revision;
        record.archive_epoch += 1;
        record.revision += 1;
        tx.projects().update_project(&record, expected)
    })
    .unwrap();
    refused(start(&context, &recovering));
    assert!(
        read(&*f.store, |s| s
            .runs()
            .live_activation(&RootId(recovering.clone())))
        .is_none()
    );
}

#[test]
fn an_automatic_reservation_leaves_an_independent_manual_execution_slot() {
    let _native_case = native_runtime_case();
    let f = Fx::new();
    let a = f.issue("automatic", 3);
    let b = f.issue("manual", 2);
    let c = f.issue("still-ready", 1);
    let context = devworks_core::context::HostIngress::new(f.store.as_ref())
        .keep_working_context(&f.user, ProjectId(id("project")), 0, id("automatic"))
        .unwrap();
    let automatic = |operation, body| {
        let mut frame = f.frame(operation, body);
        if operation == "next_work.claim_project_queue" {
            frame["schemaRevision"] = json!(3);
        }
        f.send(&context, operation, &frame)
    };
    let project_queue = |another| {
        json!({"projectId":"project","expectedQueueRevision":f.queue(None).to_string(),
            "leaseMs":"30000","another":another})
    };
    let start = |root: &Id| {
        let control = read(&*f.store, |s| control_of(s.runs(), &RootId(root.clone())));
        json!({"rootId":root,"expectedWorkRevision":"1","expectedControlRevision":control.revision.to_string()})
    };

    let selected = outcome(automatic(
        "next_work.claim_project_queue",
        project_queue(false),
    ));
    assert_eq!(selected["claim"]["workId"], json!(a));
    applied(automatic("runs.start", start(&a)));
    let selected = outcome(f.next3(json!({})));
    assert_eq!(selected["claim"]["workId"], json!(b));
    f.start(&b);
    let selected = outcome(automatic(
        "next_work.claim_project_queue",
        project_queue(true),
    ));
    assert_eq!(selected["claim"]["workId"], json!(c));
    refused(automatic("runs.start", start(&c)));
    read(&*f.store, |s| {
        assert!(s.runs().live_activation(&RootId(a))?.is_some());
        assert!(s.runs().live_activation(&RootId(b))?.is_some());
        assert!(s.runs().live_activation(&RootId(c))?.is_none());
        Ok(())
    });
}

#[test]
fn recovery_precedes_fresh_priority_and_returns_previous_owner_handoff_and_native_activity() {
    let f = Fx::new();
    let recovering = f.issue("recovering", 1);
    let first = outcome(f.next3(json!({})));
    applied(f.command(
        "next_work.release_claim",
        json!({"claimId":first["claim"]["id"],"expectedFence":first["claim"]["fence"]}),
    ));
    applied(f.command("tasks.move",json!({"taskId":recovering,"expectedRevision":"1","statusId":"project-in-progress","expectedWorkflowRevision":"1"})));
    let fresh = f.issue("fresh", 100);
    let recovered = outcome(f.next3(json!({})));
    assert_eq!(recovered["kind"], "Recovered");
    assert_eq!(recovered["claim"]["workId"], json!(recovering));
    assert_ne!(recovered["claim"]["workId"], json!(fresh));
    assert_eq!(
        recovered["recovery"]["previousOwnerId"],
        json!(f.user.principal())
    );
    assert_eq!(recovered["recovery"]["lastActivity"]["kind"], "WorkMoved");
    assert_eq!(
        recovered["recovery"]["lastActivity"]["workId"],
        json!(recovering)
    );
}

#[test]
fn approved_unassessed_work_can_assess_but_cannot_implement_and_unapproved_roots_are_skipped() {
    let f = Fx::new();
    f.seed("unapproved", None, "feature", 100);
    f.seed("assess", None, "feature", 50);
    f.seed("assess-task", Some("assess"), "task", 1);
    let _ = f.issue("fallback", 1);
    let root = RootId(id("assess"));
    f.scope.approve(&root, 1, &[]);
    f.scope.set_readiness(
        &root,
        None,
        devworks_core::services::runs::scope::Readiness::Missing,
    );
    assert_eq!(claim(f.claim())["workId"], "assess");
    f.start(&root.0);
    let pending = read(&*f.store, |s| s.runs().live_activation(&root)).unwrap();
    assert_eq!(pending.purpose, ActivationPurpose::Planning);
    let service = f.control.clone();
    let activation = write(&*f.store, move |tx| {
        let a = service.claim_activation(tx, &id("supervisor"))?.unwrap();
        service
            .start_activation(tx, &a.id, a.owner_epoch)
            .map(|r| r.unwrap())
    })
    .unwrap();
    let request = devworks_core::services::runs::substrate::DispatchRequest {
        root: root.clone(),
        execution_id: activation.id,
        owner_epoch: activation.owner_epoch,
        target: devworks_core::services::runs::Target::Root,
        purpose: ActivationPurpose::Implementation,
    };
    let service = f.control.clone();
    let r = request.clone();
    let result = write(&*f.store, move |tx| service.begin_dispatch(tx, &r)).unwrap();
    assert!(matches!(
        result,
        Err(devworks_core::services::runs::substrate::Fenced::Blocked(_))
    ));
    f.scope.set_readiness(
        &root,
        None,
        devworks_core::services::runs::scope::Readiness::Current {
            assessment_revision: 2,
        },
    );
    let service = f.control.clone();
    assert!(
        write(&*f.store, move |tx| service.begin_dispatch(tx, &request))
            .unwrap()
            .is_ok()
    );
}

#[test]
fn locally_blocked_features_allocate_independent_tasks_and_dispatch_rechecks_new_scope_findings() {
    let f = Fx::new();
    f.seed("feature", None, "feature", 1);
    f.seed("blocked", Some("feature"), "task", 10);
    f.seed("independent", Some("feature"), "task", 1);
    write(&*f.store, |tx| {
        for (work, key) in [("blocked", "E1"), ("independent", "E2")] {
            let mut w = tx.work().work(&id(work))?.unwrap();
            w.brief = json!({"inherits":[key]});
            w.revision += 1;
            tx.work().update_work(&w, 1)?;
        }
        Ok(())
    })
    .unwrap();
    let root = RootId(id("feature"));
    f.scope.approve(&root, 1, &[("E1", "one"), ("E2", "two")]);
    f.scope.set_readiness(
        &root,
        Some("E1"),
        devworks_core::services::runs::scope::Readiness::Blocked {
            finding: "needs-answer".into(),
        },
    );
    assert_eq!(claim(f.claim())["workId"], "feature");
    f.start(&root.0);
    let sid = f.session(&root.0);
    let selected=claim(f.command("next_work.claim_feature_task",json!({"rootId":"feature","expectedTaskSetRevision":f.queue(Some("feature")).to_string(),"leaseMs":"1000"})));
    assert_eq!(selected["workId"], "independent");
    let service = f.control.clone();
    let session = sid.clone();
    let (activation, assignment) = write(&*f.store, move |tx| {
        let a = service.claim_activation(tx, &id("supervisor"))?.unwrap();
        let a = service.start_activation(tx, &a.id, a.owner_epoch)?.unwrap();
        let actor = tx.store().host_actor.clone();
        let coordinator = service
            .register_assignment(
                tx,
                &actor,
                devworks_core::services::runs::substrate::NewRunAssignment {
                    root: RootId(id("feature")),
                    work: RootId(id("feature")),
                    parent: None,
                    session: session.clone(),
                    scope_key: None,
                },
            )?
            .unwrap();
        let task = service
            .register_assignment(
                tx,
                &actor,
                devworks_core::services::runs::substrate::NewRunAssignment {
                    root: RootId(id("feature")),
                    work: RootId(id("independent")),
                    parent: Some(coordinator.id),
                    session,
                    scope_key: Some("E2".into()),
                },
            )?
            .unwrap();
        Ok((a, task))
    })
    .unwrap();
    f.scope.set_readiness(
        &root,
        Some("E2"),
        devworks_core::services::runs::scope::Readiness::Blocked {
            finding: "new-material-evidence".into(),
        },
    );
    let service = f.control.clone();
    let request = devworks_core::services::runs::substrate::DispatchRequest {
        root,
        execution_id: activation.id,
        owner_epoch: activation.owner_epoch,
        target: devworks_core::services::runs::Target::Assignment(assignment.id),
        purpose: ActivationPurpose::Implementation,
    };
    assert!(matches!(
        write(&*f.store, move |tx| service.begin_dispatch(tx, &request)).unwrap(),
        Err(devworks_core::services::runs::substrate::Fenced::Blocked(_))
    ));
}

impl Fx {
    fn label(&self, work: &Id, revision: i64) {
        applied(self.command3("issues.edit",json!({"workId":work,"expectedRevision":revision.to_string(),"changes":{"labels":["Urgent"]}})));
    }
    fn pause(&self, work: &Id) {
        let c = read(&*self.store, |s| {
            control_of(s.runs(), &RootId(work.clone()))
        });
        applied(self.command(
            "runs.pause",
            json!({"rootId":work,"expectedControlRevision":c.revision.to_string()}),
        ));
    }
    /// An open blocking question to the project's human about `work`.
    fn ask(&self, work: &str) {
        rusqlite::Connection::open(self.h.root.join("workspace.sqlite"))
            .unwrap()
            .execute(
                "INSERT INTO agent_messages(id,project_id,channel,message_class,body,attachments_json,\
                 state,blocking,revision,created_at,from_actor_id,target_item_id,to_human) \
                 VALUES(?1,'project','Human','Question','Which way?','[]','Open',1,1,0,?2,?3,1)",
                rusqlite::params![
                    format!("question-{work}"),
                    self.user.principal().0.as_str(),
                    work
                ],
            )
            .unwrap();
    }
    fn next_task(&self, feature: &str) -> Value {
        outcome(self.command3("next_work.claim_feature_task",json!({"rootId":feature,"expectedTaskSetRevision":self.queue(Some(feature)).to_string(),"leaseMs":"1000"})))
    }
}

#[test]
fn no_eligible_work_reports_each_cause_that_held_ready_work_back() {
    let f = Fx::new();
    let paused = f.issue("paused", 9);
    f.label(&paused, 1);
    f.pause(&paused);
    let prerequisite = f.issue("prerequisite", 1);
    applied(f.command("tasks.move",json!({"taskId":prerequisite,"expectedRevision":"1","statusId":"project-backlog","expectedWorkflowRevision":"1"})));
    let dependent = f.issue("dependent", 8);
    applied(f.command("tasks.dependency_links",json!({"taskId":dependent,"expectedRevision":"1","replacement":{"edges":[{"predecessor":prerequisite,"successor":dependent}]}})));
    let revision = read(&*f.store, |s| s.work().work(&dependent))
        .unwrap()
        .revision;
    f.label(&dependent, revision);
    let unlabelled = f.issue("unlabelled", 7);
    for (feature, priority) in [("unplanned", 6), ("unready", 5)] {
        f.seed(feature, None, "feature", priority);
        write(&*f.store, move |tx| {
            let mut w = tx.work().work(&id(feature))?.unwrap();
            w.brief = json!({"labels":["Urgent"]});
            w.revision += 1;
            tx.work().update_work(&w, 1)
        })
        .unwrap();
    }
    let unready = RootId(id("unready"));
    f.scope.approve(&unready, 1, &[]);
    f.scope.set_readiness(
        &unready,
        None,
        devworks_core::services::runs::scope::Readiness::Blocked {
            finding: "needs-answer".into(),
        },
    );
    let none = outcome(f.next3(json!({"label":"urgent"})));
    assert_eq!(none["kind"], "NoEligible");
    assert_eq!(none["reason"], "pause,prerequisite,plan,readiness,filter");
    let questioned = f.issue("questioned", 4);
    f.label(&questioned, 1);
    f.ask(questioned.as_str());
    let none = outcome(f.next3(json!({"label":"urgent"})));
    assert_eq!(
        none["reason"],
        "pause,prerequisite,plan,readiness,question,filter"
    );
    let any = outcome(f.next3(json!({})));
    assert_eq!(any["claim"]["workId"], json!(unlabelled));
}

#[test]
fn a_blocking_question_holds_the_work_it_names_and_not_independent_work() {
    let f = Fx::new();
    f.seed("feature", None, "feature", 1);
    f.seed("asked", Some("feature"), "task", 10);
    f.seed("independent", Some("feature"), "task", 1);
    let root = RootId(id("feature"));
    f.scope.approve(&root, 1, &[]);
    f.ask("asked");
    assert_eq!(claim(f.claim())["workId"], "feature");
    f.start(&root.0);
    let _ = f.session(&root.0);
    let first = f.next_task("feature");
    assert_eq!(first["claim"]["workId"], "independent");
    applied(f.command(
        "next_work.release_claim",
        json!({"claimId":first["claim"]["id"],"expectedFence":first["claim"]["fence"]}),
    ));
    f.ask("feature");
    let none = f.next_task("feature");
    assert_eq!(none["kind"], "NoEligible");
    assert_eq!(none["reason"], "question");
}

#[path = "allocations/handoff.rs"]
mod handoff;

/// A next_work claim reads the live claims and actionable work: it costs the same behind 10 or
/// 100 completed issues, each with a released claim.
#[test]
fn a_next_work_claim_costs_the_same_behind_tenfold_completed_history() {
    let h = Harness::new();
    let counted = h.open_counted();
    let f = Fx::over(h, counted.store.clone());
    let target = f.issue("target", 1);
    let db = rusqlite::Connection::open(f.h.root.join("workspace.sqlite")).unwrap();
    let mut done = 0;
    let mut complete = |to: usize| {
        for n in done..to {
            let name = format!("done-{n:04}");
            f.seed(&name, None, TASK, 10);
            db.execute(
                "UPDATE work_items SET status_id='project-done' WHERE id=?1",
                [&name],
            )
            .unwrap();
            db.execute(
                "INSERT INTO allocation_claims(id,project_id,root_id,work_id,kind,queue_key,owner_id,ingress_namespace,invocation_id,fence,expires_at,state,revision,work_revision,root_control_revision,queue_revision,created_at) VALUES(?1,'project',?2,?2,'task','project','user','history',?1,1,0,'Released',2,1,1,1,0)",
                [format!("released-{n:04}"), name],
            )
            .unwrap();
        }
        done = to;
    };
    let measure = || {
        let (reply, work) = counted.count(|| f.claim());
        let claimed = claim(reply);
        assert_eq!(claimed["workId"], json!(target));
        applied(f.command(
            "next_work.release_claim",
            json!({"claimId": claimed["id"], "expectedFence": claimed["fence"]}),
        ));
        work
    };
    complete(10);
    let small = measure();
    complete(100);
    let large = measure();
    assert_no_growth("next_work claim", small, large);
}

impl Fx {
    /// The item's record as `tasks.get` (or `features.get`) answers it.
    fn record(&self, op: &str, work: &str) -> devworks_core::protocol::records::WorkRecord {
        let frame = json!({"mode":"Query","protocol":"1.0-draft.1","schemaRevision":1,
            "queryId":"record","body":{"kind":op,"workId":work,"detailLevel":"Summary"}});
        match self
            .cases
            .query(
                &self.gateway(),
                &self.user,
                op,
                frame.to_string().as_bytes(),
            )
            .unwrap()
        {
            devworks_core::protocol::results::DomainQueryReply::Result { result, .. } => {
                match result {
                    OperationResult::TasksGet { value }
                    | OperationResult::FeaturesGet { value } => value,
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn a_task_record_carries_its_live_assignment_and_counts_as_worked_only_while_it_lives() {
    let f = Fx::new();
    f.seed("feature", None, "feature", 1);
    f.seed("task-a", Some("feature"), "task", 2);
    f.seed("task-b", Some("feature"), "task", 1);
    f.scope.approve(&RootId(id("feature")), 1, &[]);
    f.start(&id("feature"));
    let coordinator_session = f.session(&id("feature"));
    let worker_session = f.session(&id("feature"));
    let service = f.control.clone();
    let (activation, coordinator, worker) = write(&*f.store, move |tx| {
        let a = service.claim_activation(tx, &id("supervisor"))?.unwrap();
        let a = service.start_activation(tx, &a.id, a.owner_epoch)?.unwrap();
        let actor = tx.store().host_actor.clone();
        let register = |tx: &mut dyn WriteTransaction, work: &str, parent, session| {
            service
                .register_assignment(
                    tx,
                    &actor,
                    devworks_core::services::runs::substrate::NewRunAssignment {
                        root: RootId(id("feature")),
                        work: RootId(id(work)),
                        parent,
                        session,
                        scope_key: None,
                    },
                )
                .map(|r| r.unwrap())
        };
        let coordinator = register(tx, "feature", None, coordinator_session)?;
        let worker = register(tx, "task-a", Some(coordinator.id.clone()), worker_session)?;
        Ok((a, coordinator, worker))
    })
    .unwrap();
    // The worked task runs under its feature's activation and carries its own assignment; the
    // feature carries its coordinator's; the other task has no attempt.
    let task = f.record("tasks.get", "task-a");
    let attempts: Vec<_> = task
        .attempts
        .0
        .iter()
        .map(|a| (a.id.clone(), a.assignment_id.clone()))
        .collect();
    assert_eq!(attempts, [(activation.id.clone(), Some(worker.id.clone()))]);
    assert_eq!(task.attempt_count.get(), 1);
    let feature = f.record("features.get", "feature");
    assert_eq!(
        feature.attempts.0[0].assignment_id.as_ref(),
        Some(&coordinator.id)
    );
    assert!(f.record("tasks.get", "task-b").attempts.0.is_empty());
    // The board counts items with an attempt as worked: the feature and its worked task.
    let worked = ["feature", "task-a", "task-b"]
        .iter()
        .map(|w| {
            f.record(
                if *w == "feature" {
                    "features.get"
                } else {
                    "tasks.get"
                },
                w,
            )
        })
        .filter(|r| !r.attempts.0.is_empty())
        .count();
    assert_eq!(worked, 2);
    // The card records that assignment, and clears it again.
    let OperationResult::TasksAssign { value } = applied(f.command(
        "tasks.assign",
        json!({"taskId":"task-a","expectedRevision":task.revision,
            "assignmentId":worker.id,"clearAssignment":false}),
    )) else {
        panic!()
    };
    assert!(value.assigned_agent_id.is_some());
    let OperationResult::TasksAssign { value } = applied(f.command(
        "tasks.assign",
        json!({"taskId":"task-a","expectedRevision":value.revision,"clearAssignment":true}),
    )) else {
        panic!()
    };
    assert!(value.assigned_agent_id.is_none());
    // An ended assignment is no longer an attempt.
    let service = f.control.clone();
    write(&*f.store, move |tx| {
        let actor = tx.store().host_actor.clone();
        service
            .set_assignment_state(
                tx,
                &actor,
                &worker.id,
                worker.revision,
                AssignmentState::Completed,
                None,
            )
            .map(|r| r.unwrap())
    })
    .unwrap();
    assert!(f.record("tasks.get", "task-a").attempts.0.is_empty());
}
