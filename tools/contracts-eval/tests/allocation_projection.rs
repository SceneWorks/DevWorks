use devworks_contract_probe::domain_generated::AllocationSelection;
use devworks_contract_probe::domain_support::{Check, Revisioned};
use serde_json::{Value, json};

fn legacy() -> Value {
    json!({"id":"claim-a","workId":"work-a","ownerId":"actor-a","fence":"1","expiresAtMs":"100","state":"Held"})
}
fn claim() -> Value {
    let mut value = legacy();
    value.as_object_mut().unwrap().extend(
        json!({"workKind":"Feature","workRevision":"2","projectId":"project-a","rootId":"work-a","ownerSessionId":"session-a","queueRevision":"3"})
            .as_object().unwrap().clone(),
    );
    value
}

#[test]
fn old_receipt_claim_decodes_and_serializes_without_new_history() {
    let value = legacy();
    let decoded: AllocationSelection = serde_json::from_value(value.clone()).unwrap();
    decoded.check().unwrap();
    decoded.check_revision(1).unwrap();
    assert!(decoded.check_revision(3).is_err());
    assert_eq!(serde_json::to_value(decoded).unwrap(), value);
}

#[test]
fn claimed_and_recovered_results_project_to_the_exact_old_claim() {
    for kind in ["Claimed", "Recovered"] {
        let mut wire = json!({"kind":kind,"claim":claim(),"reused":kind == "Recovered"});
        if kind == "Recovered" {
            wire["recovery"] = json!({"links":[],"unmetAcceptance":[]});
        }
        let mut decoded: AllocationSelection = serde_json::from_value(wire.clone()).unwrap();
        decoded.check().unwrap();
        decoded.check_revision(3).unwrap();
        assert_eq!(serde_json::to_value(&decoded).unwrap(), wire);
        decoded.retain_revision(1);
        decoded.check_revision(1).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), legacy());
    }
}

#[test]
fn no_eligible_outcome_is_explicit_and_has_no_legacy_fake_claim() {
    let mut decoded: AllocationSelection = serde_json::from_value(
        json!({"kind":"NoEligible","reason":"all_candidates_blocked","reused":false}),
    )
    .unwrap();
    decoded.check().unwrap();
    decoded.check_revision(3).unwrap();
    decoded.retain_revision(1);
    assert!(decoded.check_revision(1).is_err());
    assert!(serde_json::to_value(decoded).unwrap().get("id").is_none());
}
