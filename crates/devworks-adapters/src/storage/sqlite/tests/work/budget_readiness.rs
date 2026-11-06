use super::*;

fn assess(
    fx: &Fx,
    feature: &WorkRecord,
    revision: &Revision,
    digest: &str,
    summary: &str,
) -> DomainCommandReply {
    let mut change = json!({"kind":"Assess","planRevision":revision,"planDigest":digest,"summary":summary,"findings":[]});
    if let Some(previous) = feature
        .readiness
        .as_ref()
        .or(feature.pending_readiness.as_ref())
    {
        change["expectedAssessmentRevision"] = json!(previous.revision);
    }
    fx.command_at(&fx.agent(),"features.edit",json!({"workId":feature.id,"expectedRevision":feature.revision,"changes":{"readiness":change}}),Some(3))
}

#[test]
fn budget_bound_readiness_uses_exact_pending_and_approved_tokens_independent_of_current_cap() {
    let fx = Fx::new();
    let project = fx.project("Readiness budget binding", &["repo-1"]);
    let feature = fx.feature(&project, "Exact readiness binding", &["repo-1"]);
    let doc = fx.artifact(&project, 1);
    let draft = draft(&doc, &[("E1", "ship")], &["repo-1"]);
    let saved=work_of(applied(fx.command_at(&fx.user(),"features.edit_plan",json!({"featureId":feature.id,"expectedRevision":feature.revision,"plan":draft,"budget":{"kind":"Limit","microUsd":"12500000"}}),Some(3))));
    let pending = saved.pending_plan.as_ref().unwrap();
    let exact = String::from(pending.plan_digest.clone().unwrap());
    let plain_digest = plan_digest(&serde_json::from_value(draft.clone()).unwrap());
    assert_ne!(exact, plain_digest);
    let plain = refused(assess(
        &fx,
        &saved,
        &pending.revision,
        &plain_digest,
        "wrong plan-only binding",
    ));
    assert_eq!(plain.code, DomainErrorCode::Conflict);
    let assessed = work_of(applied(assess(
        &fx,
        &saved,
        &pending.revision,
        &exact,
        "assessed pending budget-bound plan",
    )));
    assert!(
        !assessed.pending_readiness.as_ref().unwrap().ready,
        "pending scope does not authorize execution before approval"
    );
    assert_eq!(
        String::from(
            assessed
                .pending_readiness
                .as_ref()
                .unwrap()
                .plan_digest
                .clone()
        ),
        exact
    );

    let revised=work_of(applied(fx.command_at(&fx.user(),"features.edit_plan",json!({"featureId":assessed.id,"expectedRevision":assessed.revision,"plan":draft,"budget":{"kind":"Limit","microUsd":"25000000"}}),Some(3))));
    let current = revised.pending_plan.as_ref().unwrap();
    let current_digest = String::from(current.plan_digest.clone().unwrap());
    assert_ne!(current_digest, exact);
    let old = refused(assess(
        &fx,
        &revised,
        &current.revision,
        &exact,
        "stale budget proposal",
    ));
    assert_eq!(old.code, DomainErrorCode::Conflict);
    let assessed = work_of(applied(assess(
        &fx,
        &revised,
        &current.revision,
        &current_digest,
        "assessed replacement budget-bound plan",
    )));
    let pending = assessed.pending_plan.as_ref().unwrap();
    let proposal=match applied(fx.command_at(&fx.agent(),"features.request_plan_approval",json!({"featureId":assessed.id,"expectedRevision":assessed.revision,"planRevision":pending.revision,"planDigest":current_digest}),Some(1))) {
        OperationResult::FeaturesRequestPlanApproval{value}=>value,
        other=>panic!("{other:?}"),
    };
    let human = fx.user();
    let proposal_id = proposal.id;
    let approved = fx.write(move |tx| {
        decide_plan_approval(tx, &human, &proposal_id, 1, true)
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    });
    let approval = approved.approval.as_ref().unwrap();
    assert_eq!(String::from(approval.plan_digest.clone()), current_digest);
    assert_eq!(
        approved
            .budget
            .as_ref()
            .unwrap()
            .limit_micro_usd
            .as_ref()
            .unwrap()
            .get(),
        25_000_000
    );
    let active = work_of(applied(assess(
        &fx,
        &approved,
        &approval.plan_revision,
        &current_digest,
        "assessed approved budget-bound plan",
    )));
    assert!(active.readiness.as_ref().unwrap().ready);

    let changed=work_of(applied(fx.command_at(&fx.user(),"features.edit",json!({"workId":active.id,"expectedRevision":active.revision,"changes":{"budget":{"kind":"Limit","microUsd":"40000000"}}}),Some(3))));
    assert_eq!(
        changed
            .budget
            .as_ref()
            .unwrap()
            .limit_micro_usd
            .as_ref()
            .unwrap()
            .get(),
        40_000_000
    );
    let binding = changed.approval.as_ref().unwrap();
    assert_eq!(String::from(binding.plan_digest.clone()), current_digest);
    let assessed = work_of(applied(assess(
        &fx,
        &changed,
        &binding.plan_revision,
        &current_digest,
        "current cap is not the approved plan binding",
    )));
    assert!(assessed.readiness.as_ref().unwrap().ready);
    assert_eq!(
        String::from(assessed.readiness.as_ref().unwrap().plan_digest.clone()),
        current_digest
    );
    let plain = refused(assess(
        &fx,
        &assessed,
        &binding.plan_revision,
        &plain_digest,
        "plan-only digest still cannot authorize assessment",
    ));
    assert_eq!(plain.code, DomainErrorCode::Conflict);
}
