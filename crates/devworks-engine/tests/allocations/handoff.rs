use super::*;
use devworks_core::services::runs::substrate::{DispatchRequest, Fenced, NewRunAssignment};
use devworks_core::services::runs::{Target, root_assignments};

struct MovedWorker {
    f: Fx,
    task: Id,
    worker: StoredAssignment,
    session: Id,
    coordinator_b: StoredAssignment,
    activation_a: StoredActivation,
    activation_b: StoredActivation,
    old_turn: StoredTurn,
}
impl MovedWorker {
    fn new() -> Self {
        Self::new_with_child(false)
    }
    fn new_with_child(child: bool) -> Self {
        let f = Fx::new();
        for name in ["feature-a", "feature-b"] {
            f.seed(name, None, "feature", 1);
            f.scope.approve(
                &RootId(id(name)),
                1,
                &[(
                    "E1",
                    if name == "feature-a" {
                        "original-A-statement"
                    } else {
                        "different-B-statement"
                    },
                )],
            );
        }
        f.scope.set_readiness(
            &RootId(id("feature-b")),
            Some("E1"),
            devworks_core::services::runs::scope::Readiness::Blocked {
                finding: "unrelated-B-E1".into(),
            },
        );
        f.seed("moving", Some("feature-a"), "task", 1);
        write(&*f.store, |tx| {
            let mut w = tx.work().work(&id("moving"))?.unwrap();
            w.revision = 2;
            w.brief = json!({"inherits":["E1"]});
            tx.work().update_work(&w, 1)
        })
        .unwrap();
        f.start(&id("feature-a"));
        f.start(&id("feature-b"));
        let coordinator_a_session = f.session(&id("feature-a"));
        let coordinator_b_session = f.session(&id("feature-b"));
        let session = f.session(&id("feature-a"));
        let service = f.control.clone();
        let sid = session.clone();
        let (activation_a, activation_b, coordinator_b, worker, old_turn) =
            write(&*f.store, move |tx| {
                let mut activations = Vec::new();
                for _ in 0..2 {
                    let a = service.claim_activation(tx, &id("supervisor"))?.unwrap();
                    activations.push(service.start_activation(tx, &a.id, a.owner_epoch)?.unwrap());
                }
                let activation_a = activations
                    .iter()
                    .find(|a| a.root_id.0 == id("feature-a"))
                    .unwrap()
                    .clone();
                let activation_b = activations
                    .into_iter()
                    .find(|a| a.root_id.0 == id("feature-b"))
                    .unwrap();
                let actor = tx.store().host_actor.clone();
                let coordinator_a = service
                    .register_assignment(
                        tx,
                        &actor,
                        NewRunAssignment {
                            root: RootId(id("feature-a")),
                            work: RootId(id("feature-a")),
                            parent: None,
                            session: coordinator_a_session,
                            scope_key: None,
                        },
                    )?
                    .unwrap();
                let coordinator_b = service
                    .register_assignment(
                        tx,
                        &actor,
                        NewRunAssignment {
                            root: RootId(id("feature-b")),
                            work: RootId(id("feature-b")),
                            parent: None,
                            session: coordinator_b_session,
                            scope_key: None,
                        },
                    )?
                    .unwrap();
                let request = NewRunAssignment {
                    root: RootId(id("feature-a")),
                    work: RootId(id("moving")),
                    parent: Some(coordinator_a.id),
                    session: sid.clone(),
                    scope_key: Some("E1".into()),
                };
                let worker = if child {
                    service.register_child_assignment(tx, &actor, request)?
                } else {
                    service.register_assignment(tx, &actor, request)?
                }
                .unwrap();
                let worker = service
                    .set_assignment_state(
                        tx,
                        &actor,
                        &worker.id,
                        worker.revision,
                        AssignmentState::Active,
                        Some(json!({"progress":"halfway","position":3})),
                    )?
                    .unwrap();
                let turn = service
                    .begin_turn(
                        tx,
                        &sid,
                        &worker.id,
                        &activation_a.id,
                        activation_a.owner_epoch,
                    )?
                    .unwrap();
                Ok((activation_a, activation_b, coordinator_b, worker, turn))
            })
            .unwrap();
        Self {
            f,
            task: id("moving"),
            worker,
            session,
            coordinator_b,
            activation_a,
            activation_b,
            old_turn,
        }
    }
    fn change(&self, feature: Option<&str>) {
        let w = read(&*self.f.store, |s| s.work().work(&self.task)).unwrap();
        let mut body = json!({"taskId":self.task,"expectedRevision":w.revision.to_string(),"clearFeature":feature.is_none()});
        if let Some(feature) = feature {
            body["featureId"] = json!(feature);
        }
        applied(self.f.command("tasks.set_feature", body));
    }
    fn reconcile(&self) {
        let service = self.f.control.clone();
        write(&*self.f.store, move |tx| {
            allocation::reconcile_project(tx, &service, &ProjectId(id("project")))
        })
        .unwrap();
    }
    fn settle(&self) {
        let service = self.f.control.clone();
        let sid = self.session.clone();
        let turn = self.old_turn.turn_id.clone();
        write(&*self.f.store, move |tx| service.end_turn(tx, &sid, &turn))
            .unwrap()
            .unwrap();
    }
    fn assignment(&self) -> StoredAssignment {
        read(&*self.f.store, |s| s.runs().assignment(&self.worker.id)).unwrap()
    }
    fn dispatch(
        &self,
        activation: &StoredActivation,
    ) -> Result<devworks_core::services::runs::substrate::Admission, Fenced> {
        let service = self.f.control.clone();
        let request = DispatchRequest {
            root: activation.root_id.clone(),
            execution_id: activation.id.clone(),
            owner_epoch: activation.owner_epoch,
            target: Target::Assignment(self.worker.id.clone()),
            purpose: ActivationPurpose::Implementation,
        };
        write(&*self.f.store, move |tx| {
            service.begin_dispatch(tx, &request)
        })
        .unwrap()
    }
}

#[test]
fn transferred_child_remains_schedulable_until_destination_launch() {
    let m = MovedWorker::new_with_child(true);
    m.change(Some("feature-b"));
    m.settle();
    m.reconcile();
    let moved = m.assignment();
    assert_eq!(moved.root_id, RootId(id("feature-b")));
    assert_eq!(moved.execution_id, Some(m.activation_b.id));
    assert_eq!(moved.state, AssignmentState::Waiting);
    assert!(!moved.slot_held);
}

#[test]
fn detached_child_retires_before_a_new_issue_coordinator_materializes() {
    let m = MovedWorker::new_with_child(true);
    m.change(None);
    m.settle();
    m.reconcile();
    let retired = m.assignment();
    assert_eq!(retired.state, AssignmentState::Superseded);
    assert_eq!(retired.purpose, AssignmentPurpose::Child);
    assert_eq!(retired.root_id, RootId(id("feature-a")));
    assert_eq!(retired.checkpoint, m.worker.checkpoint);
    assert!(
        read(&*m.f.store, |s| root_assignments(
            s.runs(),
            &RootId(id("moving")),
            true
        ))
        .is_empty()
    );

    let holds = read(&*m.f.store, |s| {
        devworks_core::services::runs::active_blockers(
            s.runs(),
            &BlockerFilter::RootLevel(RootId(id("moving"))),
        )
    });
    let covered: Vec<_> = holds
        .iter()
        .filter(|hold| hold.cause.manual_resume())
        .map(|hold| json!({"holdId":hold.id,"revision":hold.revision.to_string()}))
        .collect();
    let root_control = read(&*m.f.store, |s| control_of(s.runs(), &RootId(id("moving"))));
    applied(m.f.command("runs.continue", json!({"rootId":"moving","expectedControlRevision":root_control.revision.to_string(),"coveredHolds":covered})));
    let control = m.f.control.clone();
    let activation = write(&*m.f.store, move |tx| {
        let claim = control.claim_activation(tx, &id("supervisor"))?.unwrap();
        control.start_activation(tx, &claim.id, claim.owner_epoch)
    })
    .unwrap()
    .unwrap();
    let host = principal(&*m.f.store, "detached-host", PrincipalKind::Host);
    let agents =
        devworks_core::services::agents::AgentService::new(m.f.h.ids.clone(), m.f.control.clone());
    let coordinator = write(&*m.f.store, move |tx| {
        agents
            .materialize_primary(tx, &host, &activation.id, activation.owner_epoch)
            .map_err(|error| StorageError::InvalidRecord(format!("primary refused: {error:?}")))
    })
    .unwrap();
    let primary = read(&*m.f.store, |s| {
        s.runs().assignment(&coordinator.assignment_id)
    })
    .unwrap();
    assert_eq!(primary.purpose, AssignmentPurpose::Primary);
    assert_eq!(primary.root_id, RootId(id("moving")));
    assert_eq!(primary.work_id, RootId(id("moving")));
    assert_ne!(primary.id, m.worker.id);
    assert_eq!(
        read(&*m.f.store, |s| s
            .runs()
            .agent_assignment_context(&primary.id))
        .unwrap()
        .role_id,
        id("issueCoordinator")
    );
}

#[test]
fn moving_a_secondary_delegated_task_fences_the_worker_without_moving_its_primary_task() {
    let m = MovedWorker::new();
    m.f.seed("secondary", Some("feature-a"), "task", 1);
    write(&*m.f.store, |tx| {
        let mut task = tx.work().work(&id("secondary"))?.unwrap();
        task.revision = 2;
        task.brief = json!({"inherits":["E1"]});
        tx.work().update_work(&task, 1)
    })
    .unwrap();
    let worker = m.worker.id.clone();
    write(&*m.f.store, move |tx| {
        tx.runs()
            .insert_agent_assignment_context(&StoredAgentAssignmentContext {
                assignment_id: worker,
                project_id: ProjectId(id("project")),
                role_id: id("worker"),
                role_instructions: "Work on both tasks".into(),
                role_revisions: json!({}),
                brief: "Work on both tasks".into(),
                task_ids: json!(["moving", "secondary"]),
                source_checkpoint_id: None,
                created_at_ms: 1,
            })
    })
    .unwrap();
    let revision = read(&*m.f.store, |s| s.work().work(&id("secondary")))
        .unwrap()
        .revision;
    applied(m.f.command(
        "tasks.set_feature",
        json!({"taskId":"secondary","expectedRevision":revision.to_string(),"featureId":"feature-b","clearFeature":false}),
    ));
    assert_eq!(m.assignment().state, AssignmentState::Superseded);
    let secondary_holds = || {
        read(&*m.f.store, |s| {
            devworks_core::services::runs::active_blockers(
                s.runs(),
                &BlockerFilter::RootLevel(RootId(id("secondary"))),
            )
        })
    };
    assert!(secondary_holds().iter().any(|hold| {
        hold.origin_id == m.worker.id.0.as_str() && hold.clearance["membershipHandoff"] == true
    }));
    m.reconcile();
    assert!(
        !secondary_holds().is_empty(),
        "the old turn is still active"
    );
    m.settle();
    m.reconcile();
    assert!(
        secondary_holds().is_empty(),
        "quiescence releases the moved task"
    );
    let after = m.assignment();
    assert_eq!(after.state, AssignmentState::Superseded);
    assert_eq!(after.checkpoint, m.worker.checkpoint);
    assert_eq!(after.root_id, RootId(id("feature-a")));
    assert_eq!(after.work_id, RootId(id("moving")));
    assert_eq!(
        read(&*m.f.store, |s| s.work().work(&id("moving")))
            .unwrap()
            .parent_feature_id,
        Some(id("feature-a"))
    );
}

#[test]
fn a_split_delegated_scope_retires_instead_of_blocking_the_destination_forever() {
    let m = MovedWorker::new();
    m.f.seed("secondary", Some("feature-a"), "task", 1);
    let worker = m.worker.id.clone();
    write(&*m.f.store, move |tx| {
        tx.runs()
            .insert_agent_assignment_context(&StoredAgentAssignmentContext {
                assignment_id: worker,
                project_id: ProjectId(id("project")),
                role_id: id("worker"),
                role_instructions: "Work on both tasks".into(),
                role_revisions: json!({}),
                brief: "Work on both tasks".into(),
                task_ids: json!(["moving", "secondary"]),
                source_checkpoint_id: None,
                created_at_ms: 1,
            })
    })
    .unwrap();
    m.change(Some("feature-b"));
    let secondary_holds = || {
        read(&*m.f.store, |s| {
            devworks_core::services::runs::active_blockers(
                s.runs(),
                &BlockerFilter::RootLevel(RootId(id("secondary"))),
            )
        })
    };
    assert!(secondary_holds().iter().any(|hold| {
        hold.origin_id == m.worker.id.0.as_str() && hold.clearance["membershipHandoff"] == true
    }));
    m.reconcile();
    assert!(
        !secondary_holds().is_empty(),
        "the old turn is still active"
    );
    m.settle();
    m.reconcile();
    assert!(
        secondary_holds().is_empty(),
        "quiescence clears the secondary hold"
    );
    let retired = m.assignment();
    assert_eq!(retired.state, AssignmentState::Superseded);
    assert_eq!(retired.root_id, RootId(id("feature-a")));
    assert_eq!(retired.checkpoint, m.worker.checkpoint);
    assert!(
        read(&*m.f.store, |s| {
            devworks_core::services::runs::active_blockers(
                s.runs(),
                &BlockerFilter::RootLevel(RootId(id("moving"))),
            )
        })
        .is_empty()
    );
}

#[test]
fn a_feature_transfer_preserves_worker_session_and_checkpoint_and_fences_old_epoch_and_root() {
    let m = MovedWorker::new();
    m.change(Some("feature-b"));
    m.reconcile();
    assert_eq!(
        m.assignment().state,
        AssignmentState::Superseded,
        "old turn must quiesce"
    );
    assert!(m.dispatch(&m.activation_a).is_err());
    let replacement = m.f.session(&id("feature-b"));
    let service = m.f.control.clone();
    let parent = m.coordinator_b.id.clone();
    let result = write(&*m.f.store, move |tx| {
        let actor = tx.store().host_actor.clone();
        service.register_assignment(
            tx,
            &actor,
            NewRunAssignment {
                root: RootId(id("feature-b")),
                work: RootId(id("moving")),
                parent: Some(parent),
                session: replacement,
                scope_key: None,
            },
        )
    })
    .unwrap();
    assert!(
        matches!(result, Err(Fenced::Blocked(_))),
        "pending handoff owns the durable worker identity"
    );
    m.settle();
    m.reconcile();
    let moved = m.assignment();
    assert_eq!(moved.id, m.worker.id);
    assert_eq!(moved.session_id, m.session);
    assert_eq!(moved.root_id, RootId(id("feature-b")));
    assert_eq!(moved.parent_id, Some(m.coordinator_b.id.clone()));
    assert_eq!(moved.checkpoint, m.worker.checkpoint);
    assert_eq!(moved.state, AssignmentState::Active);
    assert!(moved.slot_held);
    let session = read(&*m.f.store, |s| s.sessions().session(&m.session)).unwrap();
    assert_eq!(session.root_id, Some(RootId(id("feature-b"))));
    assert_eq!(session.context_epoch, 1);
    let query = json!({"mode":"Query","protocol":"1.0-draft.1","schemaRevision":1,"queryId":"old-context","body":{"kind":"sessions.briefing","sessionId":m.session,"expectedContextEpoch":"0"}});
    let old =
        m.f.cases
            .query(
                &m.f.gateway(),
                &m.f.user,
                "sessions.briefing",
                query.to_string().as_bytes(),
            )
            .unwrap();
    assert!(!matches!(
        old,
        devworks_core::protocol::results::DomainQueryReply::Result { .. }
    ));
    assert!(m.dispatch(&m.activation_a).is_err());
    assert!(
        m.dispatch(&m.activation_b).is_ok(),
        "A.E1 must not be reinterpreted as unrelated blocked B.E1"
    );
    let record = read(&*m.f.store, |s| {
        devworks_core::services::work::model::record(
            s.work(),
            &s.work().work(&id("moving"))?.unwrap(),
            devworks_core::services::work::model::Extras {
                budget: None,
                live_execution: None,
                readiness: None,
                pending_readiness: None,
                detail: devworks_core::services::work::model::Detail::Summary,
            },
        )
    });
    assert!(record.inherits_requirement_ids.0.is_empty());
    let previous = record.previous_inheritance.unwrap();
    assert_eq!(previous.feature_id, id("feature-a"));
    assert_eq!(previous.requirement_ids.0, vec![id("E1")]);
    let service = m.f.control.clone();
    let sid = m.session.clone();
    let assignment = m.worker.id.clone();
    let a = m.activation_b.clone();
    write(&*m.f.store, move |tx| {
        let first = service
            .begin_turn(tx, &sid, &assignment, &a.id, a.owner_epoch)?
            .unwrap();
        assert!(
            service
                .begin_turn(tx, &sid, &assignment, &a.id, a.owner_epoch)?
                .is_err()
        );
        service.end_turn(tx, &sid, &first.turn_id)?.unwrap();
        Ok(())
    })
    .unwrap();
    assert_eq!(
        read(&*m.f.store, |s| root_assignments(
            s.runs(),
            &RootId(id("feature-b")),
            true
        ))
        .iter()
        .filter(|a| a.work_id.0 == m.task)
        .count(),
        1
    );
}

#[test]
fn returning_to_the_original_feature_fences_delayed_results_without_preempting_siblings() {
    use devworks_core::services::runs::substrate::{ResultAcceptance, ResultClaim};
    let m = MovedWorker::new();
    let admitted = m.dispatch(&m.activation_a).unwrap();
    let old = ResultClaim {
        assignment: m.worker.id.clone(),
        assignment_generation: admitted.assignment_generation.unwrap(),
        execution_id: m.activation_a.id.clone(),
        owner_epoch: m.activation_a.owner_epoch,
        plan_revision: admitted.plan_revision,
    };
    let sibling_session = m.f.session(&id("feature-a"));
    m.f.seed("sibling", Some("feature-a"), "task", 2);
    let service = m.f.control.clone();
    let parent = m.worker.parent_id.clone();
    let sibling = write(&*m.f.store, move |tx| {
        let actor = tx.store().host_actor.clone();
        service.register_assignment(
            tx,
            &actor,
            NewRunAssignment {
                root: RootId(id("feature-a")),
                work: RootId(id("sibling")),
                parent,
                session: sibling_session,
                scope_key: None,
            },
        )
    })
    .unwrap()
    .unwrap();
    m.change(Some("feature-b"));
    m.change(Some("feature-a"));
    m.reconcile();
    assert_eq!(m.assignment().state, AssignmentState::Superseded);
    m.settle();
    m.reconcile();
    let restored = m.assignment();
    assert_eq!(restored.root_id, m.worker.root_id);
    assert_eq!(restored.execution_id, m.worker.execution_id);
    assert_eq!(restored.checkpoint, m.worker.checkpoint);
    assert!(restored.generation > old.assignment_generation);
    assert_eq!(
        read(&*m.f.store, |s| s.runs().assignment(&sibling.id)).unwrap(),
        sibling
    );
    let accept = |claim: &ResultClaim| {
        let service = m.f.control.clone();
        let claim = claim.clone();
        write(&*m.f.store, move |tx| {
            let actor = tx.store().host_actor.clone();
            service.accept_result(tx, &actor, &claim)
        })
        .unwrap()
    };
    assert!(
        matches!(accept(&old), ResultAcceptance::Stale(reason) if reason.contains("generation"))
    );
    let fresh = m.dispatch(&m.activation_a).unwrap();
    assert_eq!(
        accept(&ResultClaim {
            assignment_generation: fresh.assignment_generation.unwrap(),
            plan_revision: fresh.plan_revision,
            ..old
        }),
        ResultAcceptance::Current
    );
    assert_eq!(
        read(&*m.f.store, |s| s.sessions().session(&m.session))
            .unwrap()
            .context_epoch,
        1
    );
}

#[test]
fn a_closed_session_retires_its_pending_handoff_and_allows_a_fresh_destination_worker() {
    let m = MovedWorker::new();
    let origin = m.worker.id.0.as_str().to_owned();
    write(&*m.f.store, move |tx| {
        let created_at_ms = tx.now_ms();
        tx.runs().insert_blocker(&StoredBlocker {
            id: id("independent-policy"),
            root_id: Some(RootId(id("moving"))),
            assignment_id: None,
            operation_id: None,
            cause: HoldCause::Policy,
            origin_id: origin,
            origin_revision: 999,
            state: BlockerState::Active,
            revision: 1,
            reason: "independent policy with the same origin identity".into(),
            clearance: json!({"manualPolicy":true}),
            created_at_ms,
            cleared_at_ms: None,
            cleared_by: None,
        })
    })
    .unwrap();
    m.change(Some("feature-b"));
    let session = read(&*m.f.store, |s| s.sessions().session(&m.session)).unwrap();
    applied(m.f.command(
        "sessions.end",
        json!({"sessionId":m.session,"expectedRevision":session.revision.to_string()}),
    ));
    m.reconcile();
    m.settle();
    m.reconcile();
    m.reconcile();
    let retired = m.assignment();
    assert_eq!(retired.state, AssignmentState::Superseded);
    assert_eq!(retired.checkpoint, m.worker.checkpoint);
    let replacement = m.f.session(&id("feature-b"));
    let service = m.f.control.clone();
    let parent = m.coordinator_b.id.clone();
    let new_worker = write(&*m.f.store, move |tx| {
        let actor = tx.store().host_actor.clone();
        service.register_assignment(
            tx,
            &actor,
            NewRunAssignment {
                root: RootId(id("feature-b")),
                work: RootId(id("moving")),
                parent: Some(parent),
                session: replacement,
                scope_key: None,
            },
        )
    })
    .unwrap()
    .unwrap();
    assert_ne!(new_worker.id, retired.id);
    assert_eq!(
        read(&*m.f.store, |s| s.runs().blocker(&id("independent-policy")))
            .unwrap()
            .state,
        BlockerState::Active
    );
    assert!(
        read(&*m.f.store, |s| s.sessions().session(&m.session))
            .unwrap()
            .closed()
    );
}

#[test]
fn an_uncertain_assignment_operation_without_a_queue_row_waits_without_poisoning_reconciliation() {
    let m = MovedWorker::new();
    let db = rusqlite::Connection::open(m.f.h.root.join("workspace.sqlite")).unwrap();
    db.execute("INSERT INTO operations(id,root_id,assignment_id,capability,schema_version,args_digest,payload_json,state,revision,created_at) VALUES('uncertain','feature-a',?1,'test',1,'digest','{}','NeedsInput',1,0)", [m.worker.id.0.as_str()]).unwrap();
    m.change(Some("feature-b"));
    m.settle();
    m.reconcile();
    assert_eq!(m.assignment().state, AssignmentState::Superseded);
    assert_eq!(
        read(&*m.f.store, |s| s.sessions().session(&m.session))
            .unwrap()
            .context_epoch,
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM queue_entries WHERE operation_id='uncertain'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    db.execute(
        "UPDATE operations SET state='Failed',revision=revision+1 WHERE id='uncertain'",
        [],
    )
    .unwrap();
    m.reconcile();
    assert_eq!(m.assignment().root_id, RootId(id("feature-b")));
    assert!(m.dispatch(&m.activation_b).is_ok());
}

#[test]
fn all_inherited_requirements_gate_dispatch_even_when_the_assignment_names_only_one() {
    let m = MovedWorker::new();
    let root = RootId(id("feature-a"));
    m.f.scope.approve(
        &root,
        1,
        &[("E1", "original-A-statement"), ("E2", "second-statement")],
    );
    write(&*m.f.store, |tx| {
        let mut w = tx.work().work(&id("moving"))?.unwrap();
        let revision = w.revision;
        w.brief = json!({"inherits":["E1","E2"]});
        w.revision += 1;
        tx.work().update_work(&w, revision)
    })
    .unwrap();
    assert!(m.dispatch(&m.activation_a).is_ok());
    m.f.scope.set_readiness(
        &root,
        Some("E2"),
        devworks_core::services::runs::scope::Readiness::Blocked {
            finding: "whole-E2-with-no-task-list".into(),
        },
    );
    assert!(matches!(
        m.dispatch(&m.activation_a),
        Err(Fenced::Blocked(_))
    ));
    m.f.seed("independent-e1", Some("feature-a"), "task", 1);
    write(&*m.f.store, |tx| {
        let mut w = tx.work().work(&id("independent-e1"))?.unwrap();
        w.brief = json!({"inherits":["E1"]});
        w.revision += 1;
        tx.work().update_work(&w, 1)
    })
    .unwrap();
    let claim = claim(m.f.command("next_work.claim_feature_task", json!({"rootId":"feature-a","expectedTaskSetRevision":m.f.queue(Some("feature-a")).to_string(),"leaseMs":"1000"})));
    assert_eq!(claim["workId"], "independent-e1");
}

#[test]
fn unlinking_and_relinking_preserves_the_worker_and_requires_explicit_clearance_of_its_pause() {
    let m = MovedWorker::new();
    m.change(None);
    m.settle();
    m.reconcile();
    let unlinked = m.assignment();
    assert_eq!(unlinked.root_id, RootId(m.task.clone()));
    assert_eq!(unlinked.parent_id, None);
    assert_eq!(unlinked.checkpoint, m.worker.checkpoint);
    assert_eq!(
        read(&*m.f.store, |s| control_of(s.runs(), &RootId(id("moving")))).desired,
        DesiredExecution::Paused
    );
    m.change(Some("feature-b"));
    m.reconcile();
    assert!(m.dispatch(&m.activation_b).is_err());
    assert_eq!(
        read(&*m.f.store, |s| control_of(s.runs(), &RootId(id("moving")))).desired,
        DesiredExecution::Paused
    );
    let holds = read(&*m.f.store, |s| {
        devworks_core::services::runs::active_blockers(
            s.runs(),
            &BlockerFilter::RootLevel(RootId(id("moving"))),
        )
    });
    let covered: Vec<_> = holds
        .iter()
        .filter(|b| b.cause.manual_resume())
        .map(|b| json!({"holdId":b.id,"revision":b.revision.to_string()}))
        .collect();
    let control = read(&*m.f.store, |s| control_of(s.runs(), &RootId(id("moving"))));
    applied(m.f.command("runs.continue",json!({"rootId":"moving","expectedControlRevision":control.revision.to_string(),"coveredHolds":covered})));
    m.reconcile();
    let relinked = m.assignment();
    assert_eq!(relinked.root_id, RootId(id("feature-b")));
    assert_eq!(relinked.session_id, m.worker.session_id);
    assert_eq!(relinked.checkpoint, m.worker.checkpoint);
    assert!(m.dispatch(&m.activation_b).is_ok());
    assert_eq!(
        read(&*m.f.store, |s| s.sessions().session(&m.session))
            .unwrap()
            .context_epoch,
        2
    );
}

#[test]
fn destination_capacity_and_budget_are_rechecked_before_a_transferred_workers_next_turn() {
    use devworks_core::services::runs::{HoldSubject, clear_blocker, raise_blocker};
    let m = MovedWorker::new();
    let limit = read(&*m.f.store, |s| {
        devworks_core::settings::service::max_subagents(
            s.configuration(),
            &ProjectId(id("project")),
        )
    });
    let mut occupants = Vec::new();
    for n in 0..limit {
        let name = format!("occupant-{n}");
        m.f.seed(&name, Some("feature-b"), "task", 1);
        let session = m.f.session(&id("feature-b"));
        let parent = m.coordinator_b.id.clone();
        let service = m.f.control.clone();
        occupants.push(
            write(&*m.f.store, move |tx| {
                let actor = tx.store().host_actor.clone();
                let worker = service
                    .register_assignment(
                        tx,
                        &actor,
                        NewRunAssignment {
                            root: RootId(id("feature-b")),
                            work: RootId(id(&name)),
                            parent: Some(parent),
                            session,
                            scope_key: None,
                        },
                    )?
                    .unwrap();
                Ok(service
                    .set_assignment_state(
                        tx,
                        &actor,
                        &worker.id,
                        worker.revision,
                        AssignmentState::Active,
                        None,
                    )?
                    .unwrap())
            })
            .unwrap(),
        );
    }
    m.change(Some("feature-b"));
    m.settle();
    m.reconcile();
    assert_eq!(m.assignment().state, AssignmentState::Waiting);
    assert!(!m.assignment().slot_held);
    let service = m.f.control.clone();
    let sid = m.session.clone();
    let assignment = m.worker.id.clone();
    let a = m.activation_b.clone();
    assert!(
        write(&*m.f.store, move |tx| service.begin_turn(
            tx,
            &sid,
            &assignment,
            &a.id,
            a.owner_epoch
        ))
        .unwrap()
        .is_err()
    );
    let service = m.f.control.clone();
    let freed = occupants.remove(0);
    let budget = write(&*m.f.store, move |tx| {
        let actor = tx.store().host_actor.clone();
        service
            .set_assignment_state(
                tx,
                &actor,
                &freed.id,
                freed.revision,
                AssignmentState::Completed,
                None,
            )?
            .unwrap();
        let root = tx.sessions().work_root(&RootId(id("feature-b")))?.unwrap();
        raise_blocker(
            tx,
            &actor,
            &root,
            HoldSubject::default(),
            HoldCause::BudgetContinue,
            "test-budget",
            1,
            "destination budget exhausted",
            json!({}),
        )
    })
    .unwrap();
    m.reconcile();
    assert_eq!(
        m.assignment().state,
        AssignmentState::Waiting,
        "free capacity cannot waive destination budget"
    );
    write(&*m.f.store, move |tx| {
        let actor = tx.store().host_actor.clone();
        let root = tx.sessions().work_root(&RootId(id("feature-b")))?.unwrap();
        clear_blocker(tx, &actor, &root, &budget, json!({"budgetApproved":true}))
    })
    .unwrap();
    m.reconcile();
    assert_eq!(m.assignment().state, AssignmentState::Active);
    assert!(m.assignment().slot_held);
    assert!(m.dispatch(&m.activation_b).is_ok());
}

#[test]
fn an_allocated_source_yields_and_the_missing_destination_coordinator_can_bootstrap_its_worker() {
    let f = Fx::new();
    for name in ["feature-a", "feature-b"] {
        f.seed(
            name,
            None,
            "feature",
            if name == "feature-a" { 9 } else { 1 },
        );
        f.scope.approve(&RootId(id(name)), 1, &[]);
    }
    f.seed("moving", Some("feature-a"), "task", 1);
    assert_eq!(claim(f.claim())["workId"], "feature-a");
    f.start(&id("feature-a"));
    let coordinator_session = f.session(&id("feature-a"));
    let worker_session = f.session(&id("feature-a"));
    let service = f.control.clone();
    let sid = worker_session.clone();
    let (activation, worker) = write(&*f.store, move |tx| {
        let a = service.claim_activation(tx, &id("supervisor"))?.unwrap();
        let a = service.start_activation(tx, &a.id, a.owner_epoch)?.unwrap();
        let actor = tx.store().host_actor.clone();
        let coordinator = service
            .register_assignment(
                tx,
                &actor,
                NewRunAssignment {
                    root: RootId(id("feature-a")),
                    work: RootId(id("feature-a")),
                    parent: None,
                    session: coordinator_session,
                    scope_key: None,
                },
            )?
            .unwrap();
        let worker = service
            .register_assignment(
                tx,
                &actor,
                NewRunAssignment {
                    root: RootId(id("feature-a")),
                    work: RootId(id("moving")),
                    parent: Some(coordinator.id),
                    session: sid,
                    scope_key: None,
                },
            )?
            .unwrap();
        Ok((a, worker))
    })
    .unwrap();
    applied(f.command("tasks.set_feature",json!({"taskId":"moving","expectedRevision":"1","featureId":"feature-b","clearFeature":false})));
    let service = f.control.clone();
    write(&*f.store, move |tx| {
        allocation::reconcile_project(tx, &service, &ProjectId(id("project")))
    })
    .unwrap();
    assert_eq!(
        read(&*f.store, |s| s.runs().activation(&activation.id))
            .unwrap()
            .phase,
        ActivationPhase::Stopping
    );
    let service = f.control.clone();
    let a = activation.clone();
    write(&*f.store, move |tx| {
        service.finish_activation(tx, &a.id, a.owner_epoch, ActivationOutcome::Completed, None)
    })
    .unwrap()
    .unwrap();
    let service = f.control.clone();
    write(&*f.store, move |tx| {
        allocation::reconcile_project(tx, &service, &ProjectId(id("project")))
    })
    .unwrap();
    assert_eq!(
        read(&*f.store, |s| s.runs().assignment(&worker.id))
            .unwrap()
            .state,
        AssignmentState::Superseded,
        "waiting for destination does not reactivate old worker"
    );
    assert_eq!(
        claim(f.claim())["workId"],
        "feature-b",
        "temporary routing hold must not deadlock target admission"
    );
    f.start(&id("feature-b"));
    let sid = f.session(&id("feature-b"));
    let service = f.control.clone();
    let target = write(&*f.store, move |tx| {
        let a = service.claim_activation(tx, &id("supervisor"))?.unwrap();
        let a = service.start_activation(tx, &a.id, a.owner_epoch)?.unwrap();
        let actor = tx.store().host_actor.clone();
        service
            .register_assignment(
                tx,
                &actor,
                NewRunAssignment {
                    root: RootId(id("feature-b")),
                    work: RootId(id("feature-b")),
                    parent: None,
                    session: sid,
                    scope_key: None,
                },
            )?
            .unwrap();
        allocation::reconcile_project(tx, &service, &ProjectId(id("project")))?;
        Ok(a)
    })
    .unwrap();
    let moved = read(&*f.store, |s| s.runs().assignment(&worker.id)).unwrap();
    assert_eq!(moved.root_id, RootId(id("feature-b")));
    assert_eq!(moved.session_id, worker_session);
    assert_eq!(moved.state, AssignmentState::Active);
    assert_eq!(moved.execution_id, Some(target.id));
}

#[test]
fn membership_preserves_origin_history_without_a_worker_and_allows_explicit_remapping() {
    let f = Fx::new();
    for (name, key) in [
        ("feature-a", "E1"),
        ("feature-b", "E2"),
        ("feature-c", "E9"),
    ] {
        f.seed(name, None, "feature", 1);
        f.scope
            .approve(&RootId(id(name)), 1, &[(key, "current requirement")]);
    }
    f.seed("moving", Some("feature-a"), "task", 1);
    write(&*f.store, |tx| {
        let mut work = tx.work().work(&id("moving"))?.unwrap();
        work.revision += 1;
        work.brief = json!({"inherits":["E1"]});
        tx.work().update_work(&work, 1)?;
        let approved_by = tx.store().host_actor.clone();
        let approved_at = tx.now_ms();
        let draft = json!({
            "content": {"artifactId":"plan-artifact", "versionId":"plan-version",
                "sha256":"a".repeat(64), "byteLength":"1", "mediaType":"text/plain"},
            "acceptance":[], "repositoryIds":[], "goals":[],
            "requirements":[{"id":"E2", "statement":"B requirement",
                "status":"Confirmed", "coveredByTaskIds":[]}],
            "acceptanceTests":[]
        });
        tx.work().put_feature_plan(
            &StoredFeaturePlan {
                feature_id: id("feature-b"),
                revision: 1,
                approved: Some(json!({"draft":draft, "digest":"b".repeat(64),
                "planRevision":1, "proposalId":"approval"})),
                approved_by: Some(approved_by),
                approved_at_ms: Some(approved_at),
                pending: None,
                pending_revision: None,
            },
            None,
        )
    })
    .unwrap();
    applied(f.command("tasks.set_feature",json!({"taskId":"moving","expectedRevision":"2","featureId":"feature-b","clearFeature":false})));
    let w = read(&*f.store, |s| s.work().work(&id("moving"))).unwrap();
    let brief = devworks_core::services::work::model::WorkBrief::of(&w).unwrap();
    assert!(brief.inherits.is_empty());
    assert_eq!(
        brief.previous_inheritance.unwrap().feature_id,
        id("feature-a")
    );
    let selected=claim(f.command("next_work.claim_feature_task",json!({"rootId":"feature-b","expectedTaskSetRevision":f.queue(Some("feature-b")).to_string(),"leaseMs":"1000"})));
    assert_eq!(
        selected["workId"], "moving",
        "missing B.E1 cannot become a permanent inherited blocker"
    );
    applied(f.command("tasks.edit",json!({"workId":"moving","expectedRevision":w.revision.to_string(),"changes":{"inheritsRequirementIds":["E2"]}})));
    let w = read(&*f.store, |s| s.work().work(&id("moving"))).unwrap();
    assert_eq!(
        devworks_core::services::work::model::WorkBrief::of(&w)
            .unwrap()
            .inherits,
        vec![id("E2")]
    );
    applied(f.command("tasks.set_feature",json!({"taskId":"moving","expectedRevision":w.revision.to_string(),"featureId":"feature-c","clearFeature":false})));
    let w = read(&*f.store, |s| s.work().work(&id("moving"))).unwrap();
    let brief = devworks_core::services::work::model::WorkBrief::of(&w).unwrap();
    assert!(brief.inherits.is_empty());
    let previous = brief.previous_inheritance.unwrap();
    assert_eq!(previous.feature_id, id("feature-b"));
    assert_eq!(previous.requirement_ids.0, vec![id("E2")]);
    let db = rusqlite::Connection::open(f.h.root.join("workspace.sqlite")).unwrap();
    let payloads:Vec<String>=db.prepare("SELECT payload_json FROM events WHERE aggregate_id='moving' AND kind='FeatureMembershipChanged' ORDER BY seq").unwrap().query_map([],|r|r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(payloads.len(), 2);
    let origins: Vec<Value> = payloads
        .iter()
        .map(|p| serde_json::from_str::<Value>(p).unwrap()["archivedInheritance"].clone())
        .collect();
    assert_eq!(
        origins[0],
        json!({"featureId":"feature-a","requirementIds":["E1"]})
    );
    assert_eq!(
        origins[1],
        json!({"featureId":"feature-b","requirementIds":["E2"]})
    );
}
