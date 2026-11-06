use super::*;

#[test]
fn withdrawing_pending_budget_is_revision_and_permission_fenced_and_omission_retains() {
    let fx = Fx::new();
    let p = fx.project("Budget draft", &["repo-1"]);
    let f = fx.feature(&p, "Withdraw", &["repo-1"]);
    let doc = fx.artifact(&p, 1);
    let plan = draft(&doc, &[("E1", "ship")], &["repo-1"]);
    let save = |who: &InvocationContext, revision: &Revision, extra: Value| {
        let mut body = json!({"featureId":f.id,"expectedRevision":revision,"plan":plan});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        fx.command_at(who, "features.edit_plan", body, Some(3))
    };
    let saved = work_of(applied(save(
        &fx.user(),
        &f.revision,
        json!({"budget":{"kind":"Limit","microUsd":"12500000"}}),
    )));
    let digest = saved.pending_plan.as_ref().unwrap().plan_digest.clone();
    let retained = work_of(unchanged(save(&fx.user(), &saved.revision, json!({}))));
    assert_eq!(retained.pending_plan.as_ref().unwrap().plan_digest, digest);
    assert!(retained.pending_plan.as_ref().unwrap().budget.is_some());
    let before = serde_json::to_value(&retained).unwrap();
    let stale = refused(save(
        &fx.user(),
        &f.revision,
        json!({"clearBudgetProposal":true}),
    ));
    assert_eq!(stale.code, DomainErrorCode::RevisionConflict);
    let outsider = fx.ctx("unbound-agent", PrincipalKind::Agent, Some(&p));
    assert!(matches!(
        save(
            &outsider,
            &retained.revision,
            json!({"clearBudgetProposal":true}),
        ),
        CommandReply::Rejected {
            code: devworks_core::protocol::Rejection::PermissionDenied,
            ..
        }
    ));
    let DomainQueryReply::Result {
        result: OperationResult::FeaturesGet { value: shown },
        ..
    } = fx.query_at(
        &fx.user(),
        "features.get",
        json!({"workId":f.id,"detailLevel":"Summary"}),
        Some(3),
    )
    else {
        panic!("current feature")
    };
    assert_eq!(serde_json::to_value(shown).unwrap(), before);
    let withdrawn = work_of(applied(save(
        &fx.user(),
        &retained.revision,
        json!({"clearBudgetProposal":true}),
    )));
    assert!(withdrawn.pending_plan.as_ref().unwrap().budget.is_none());
    assert_ne!(withdrawn.pending_plan.as_ref().unwrap().plan_digest, digest);
    assert_eq!(
        serde_json::to_value(&withdrawn.budget).unwrap(),
        serde_json::to_value(&retained.budget).unwrap()
    );
}
