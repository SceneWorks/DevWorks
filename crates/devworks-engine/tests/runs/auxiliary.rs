//! Actual stored coordinator turns supply auxiliary ownership; context counters cannot mint it.
use super::*;
use devworks_core::services::usage::auxiliary::{self, AuxiliaryError, AuxiliaryUnavailable};
#[test]
fn auxiliary_ownership_comes_from_the_current_turn_without_beginning_another_dispatch() {
    let f = Fx::new();
    applied(&f.start(&f.user, "root-f", 1, 1));
    let activation = f.launch("actual-auxiliary-supervisor");
    let started=f.command(&f.agent,"sessions.start",json!({"mode":"Command","protocol":PROTOCOL,"requestKey":f.key(),"body":{
        "kind":"sessions.start","scope":{"kind":"Project","projectId":"project-a"},"rootId":"root-f","title":"Actual auxiliary session"}}));
    let value = serde_json::to_value(started).unwrap();
    let session = id(value["receipt"]["decision"]["result"]["value"]["sessionId"]
        .as_str()
        .or_else(|| value["receipt"]["decision"]["result"]["value"]["id"].as_str())
        .unwrap());
    let assigned_session = session.clone();
    let assignment = f.tx(move |svc, tx| {
        let host = tx.store().host_actor.clone();
        Ok(svc
            .register_assignment(
                tx,
                &host,
                NewRunAssignment {
                    root: rid("root-f"),
                    work: rid("root-f"),
                    parent: None,
                    session: assigned_session,
                    scope_key: None,
                },
            )?
            .unwrap())
    });
    let context = f
        .agent
        .clone()
        .with_lineage(Some(ProjectId(id("project-a"))), Some(rid("root-f")), None)
        .with_session(session.clone());
    let capture = || {
        read(f.store.as_ref(), |s| {
            Ok(auxiliary::ownership(s, &f.store.info(), &context))
        })
    };
    assert!(matches!(
        capture(),
        Err(AuxiliaryError::Unavailable(
            AuxiliaryUnavailable::NoCurrentManagedInferenceOwner
        ))
    ));
    let owner = assignment.clone();
    let running = activation.clone();
    let turn = f.tx(move |svc, tx| {
        svc.begin_turn(
            tx,
            &owner.session_id,
            &owner.id,
            &running.id,
            running.owner_epoch,
        )
        .map(|r| r.unwrap())
    });
    let before = read(f.store.as_ref(), |s| Ok(s.stamp().clone()));
    let captured = capture().unwrap();
    assert_eq!(captured.work_id, Some(assignment.work_id.0));
    assert_eq!(captured.role, "Worker");
    let proof = captured.dispatch.unwrap();
    assert_eq!(proof.execution_id, activation.id);
    assert_eq!(proof.owner_epoch, turn.owner_epoch);
    assert_eq!(proof.assignment_id, Some(assignment.id));
    assert_eq!(proof.assignment_generation, assignment.generation);
    assert_eq!(proof.turn_id, turn.turn_id);
    assert_eq!(
        proof.implementation,
        activation.purpose == ActivationPurpose::Implementation
    );
    assert_eq!(
        read(f.store.as_ref(), |s| Ok(s.stamp().clone())),
        before,
        "capturing ownership never dispatches or changes controls"
    );
    let stop_session = session.clone();
    let turn_id = turn.turn_id;
    f.tx(move |svc, tx| svc.end_turn(tx, &stop_session, &turn_id))
        .unwrap();
    assert!(matches!(
        capture(),
        Err(AuxiliaryError::Unavailable(
            AuxiliaryUnavailable::NoCurrentManagedInferenceOwner
        ))
    ));
    let human = f.user.clone().with_session(session);
    let manual = read(f.store.as_ref(), |s| {
        Ok(auxiliary::ownership(s, &f.store.info(), &human))
    })
    .unwrap();
    assert!(
        manual.dispatch.is_none(),
        "manual Human execution never borrows an Agent activation"
    );
    assert_eq!(manual.work_id, Some(id("root-f")));
}
