use super::*;
use devworks_core::protocol::OperationState;
#[test]
fn capability_pages_do_not_starve_behind_unrelated_unknown_operations_and_reopen_truthfully() {
    let h = Harness::new();
    let store = h.store();
    write(&store, |tx| {
        for n in 0..505 {
            let id = OperationId(Id::new(format!("a-unrelated-{n:04}")).unwrap());
            tx.operations().record_intent(NewOperation {
                id: id.clone(),
                root_id: None,
                assignment_id: None,
                capability: "unrelated.wait".into(),
                schema_version: 1,
                args_digest: "fixture".into(),
                payload: json!({}),
            })?;
            let a = tx
                .operations()
                .begin_attempt(&id, 1, AttemptBinding::default())?;
            tx.operations().transition(
                &id,
                2,
                Some(&a.fence()),
                OperationState::Dispatching,
                None,
            )?;
            tx.operations().transition(
                &id,
                3,
                Some(&a.fence()),
                OperationState::OutcomeUnknown,
                None,
            )?;
        }
        for (n, cap) in [
            (0, "conventions.prepare"),
            (1, "conventions.prepare"),
            (2, "conventions.prepare"),
            (3, "conventions.prepare-other"),
        ] {
            tx.operations().record_intent(NewOperation {
                id: OperationId(Id::new(format!("z-owned-{n}")).unwrap()),
                root_id: None,
                assignment_id: None,
                capability: cap.into(),
                schema_version: 1,
                args_digest: "fixture".into(),
                payload: json!({"requestId":format!("actual-request-{n}")}),
            })?;
        }
        assert!(
            tx.operations()
                .unresolved(500)?
                .iter()
                .all(|o| o.capability == "unrelated.wait")
        );
        let page = tx.operations().operations_for_capability(
            "conventions.prepare",
            &[OperationState::IntentRecorded],
            None,
            2,
        )?;
        assert_eq!(
            page.iter().map(|o| o.id.0.as_str()).collect::<Vec<_>>(),
            vec!["z-owned-0", "z-owned-1"]
        );
        let next = tx.operations().operations_for_capability(
            "conventions.prepare",
            &[OperationState::IntentRecorded],
            Some(&page[1].id),
            2,
        )?;
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].payload["requestId"], "actual-request-2");
        assert!(
            tx.operations()
                .operations_for_capability("conventions.prepare", &[], None, 500)?
                .is_empty()
        );
        assert!(
            tx.operations()
                .operations_for_capability(
                    "conventions.prepare",
                    &[OperationState::IntentRecorded],
                    None,
                    0
                )
                .is_err()
        );
        assert!(
            tx.operations()
                .operations_for_capability(
                    "conventions.prepare",
                    &[OperationState::IntentRecorded],
                    None,
                    501
                )
                .is_err()
        );
        tx.operations().transition(
            &page[0].id,
            1,
            None,
            OperationState::CancelledBeforeDispatch,
            None,
        )?;
        Ok(())
    })
    .unwrap();
    drop(store);
    let store = h.store();
    let rows = read_snapshot(&store, &ReadOptions::default(), |r| {
        r.operations().operations_for_capability(
            "conventions.prepare",
            &[OperationState::IntentRecorded],
            None,
            500,
        )
    })
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id.0.as_str(), "z-owned-1");
    let cancelled = read_snapshot(&store, &ReadOptions::default(), |r| {
        r.operations().operations_for_capability(
            "conventions.prepare",
            &[OperationState::CancelledBeforeDispatch],
            None,
            500,
        )
    })
    .unwrap();
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].state, OperationState::CancelledBeforeDispatch);
    assert!(
        read_snapshot(&store, &ReadOptions::default(), |r| r
            .operations()
            .operations_for_capability(
                "conventions.prepare' OR 1=1 --",
                &[OperationState::OutcomeUnknown],
                None,
                500
            ))
        .unwrap()
        .is_empty(),
        "capability stays a bound parameter"
    );
}
