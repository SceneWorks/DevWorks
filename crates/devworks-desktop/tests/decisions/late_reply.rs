use super::*;
use devworks_core::protocol::CommandReply;
use devworks_core::protocol::results::DomainDecision;
use devworks_desktop::backend::Slot;

struct Release(Gate);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.open();
    }
}

#[test]
fn another_proposals_late_approved_decision_preserves_the_selected_review() {
    late_decision(false);
}
#[test]
fn another_proposals_late_refused_decision_preserves_the_selected_review() {
    late_decision(true);
}
fn late_decision(stale: bool) {
    let root = Root::new(if stale {
        "late-refused-decision"
    } else {
        "late-applied-decision"
    });
    let engine = start(&root);
    let real = engine.backend();
    let feature_a = feature_plan(&*real, &root, "Decision A");
    let a = request(&*real, &feature_a);
    let feature_b = feature_plan(&*real, &root, "Review B");
    let b = request(&*real, &feature_b);
    // Hold only A's response after the real gateway decides it. Missing hints ensure that
    // the eventual decision handler itself must reconcile the inbox.
    let gate = Gate::holding_replies("approvals.record_human_decision").missing_hints();
    let mut h = harness(
        shell_with(Ok(gate.over(real.clone())), &root, Script::default(), None),
        1800.0,
        1400.0,
    );
    settle(&mut h);
    h.state_mut().navigate(Route::Approvals);
    settle(&mut h);
    press(&mut h, &format!("Inspect proposal {}", a.id.as_str()));
    rationale(&mut h, "The exact decision for proposal A.");
    if stale {
        let current = get(&*real, &feature_a);
        let mut draft =
            serde_json::to_value(&current.pending_plan.as_ref().unwrap().draft).unwrap();
        draft["goals"] = json!(["A revised after its exact inspection"]);
        wops::work_changed(real.command(
            "features.edit_plan",
            &wops::work_command(
                &*real,
                "features.edit_plan",
                json!({"featureId":feature_a.id,"expectedRevision":current.revision,"plan":draft}),
            ),
        ))
        .unwrap();
    }
    let _release = Release(gate.clone());
    gate.close();
    h.get_by_role_and_label(Role::Button, "Approve exact proposal")
        .click();
    step_until(&mut h, "A's decision is in flight", |s| {
        s.client()
            .unwrap()
            .is_pending(&Slot::ApprovalDecision(a.id.as_str().into()))
    });
    let frame = h
        .state()
        .client()
        .unwrap()
        .unconfirmed()
        .into_iter()
        .find(|c| c.slot == Slot::ApprovalDecision(a.id.as_str().into()))
        .unwrap()
        .frame()
        .to_vec();
    // Do not wait for all client workers: A's reply is deliberately held while the other
    // workers support inspecting B and editing its actual native rationale input.
    h.get_by_role_and_label(Role::Button, &format!("Inspect proposal {}", b.id.as_str()))
        .click();
    step_until(&mut h, "B is inspected while A is pending", |s| {
        s.decisions.selected.as_ref().is_some_and(|p| p.id == b.id)
    });
    let draft = "An unsent rationale belonging only to proposal B.";
    rationale(&mut h, draft);
    let inspected = serde_json::to_value(&h.state().decisions.selected).unwrap();
    assert_eq!(h.state().decisions.rationale, draft);
    assert!(
        h.state()
            .client()
            .unwrap()
            .is_pending(&Slot::ApprovalDecision(a.id.as_str().into()))
    );
    assert!(
        h.state().decisions.approvals.iter().any(|p| p.id == a.id),
        "the held response has not refreshed A's inbox row"
    );
    gate.open();
    settle(&mut h);
    assert_eq!(
        serde_json::to_value(&h.state().decisions.selected).unwrap(),
        inspected,
        "A's result cannot replace B's frozen inspection"
    );
    assert_eq!(h.state().decisions.rationale, draft);
    assert!(
        h.state().decisions.decision_error.is_none(),
        "A's refusal cannot become B's decision error"
    );
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, "Decision rationale")
            .accesskit_node()
            .value()
            .as_deref(),
        Some(draft)
    );
    assert!(
        !h.state()
            .client()
            .unwrap()
            .is_pending(&Slot::ApprovalDecision(a.id.as_str().into()))
    );
    assert!(
        get(&*real, &feature_b).approval.is_none(),
        "B was only inspected, never decided"
    );
    if stale {
        assert!(
            get(&*real, &feature_a).approval.is_none(),
            "A's stale target remains refused by the real owner"
        );
    } else {
        assert!(get(&*real, &feature_a).approval.is_some());
    }
    assert!(
        !h.state().decisions.approvals.iter().any(|p| p.id == a.id),
        "A's settled or superseded row must leave the refreshed pending inbox"
    );
    let CommandReply::Recorded {
        receipt,
        replayed: true,
        ..
    } = real.human_decision(&frame).unwrap()
    else {
        panic!("the UI's exact decision must already have a durable replayable receipt")
    };
    if stale {
        assert!(
            matches!(receipt.decision, DomainDecision::Refused { .. }),
            "{receipt:?}"
        );
    } else {
        assert!(
            matches!(receipt.decision, DomainDecision::Applied { .. }),
            "{receipt:?}"
        );
    }
}
