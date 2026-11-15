//! Work items record the principal that created them, and `history.work_lineage` shows that
//! creator as the structured actor of their rows.
use super::*;
use devworks_core::protocol::records::{ActorAttribution, LineageRecord};
use devworks_core::protocol::support::HistoryPage;

fn lineage(fx: &Fx, root: &Id, revision: Option<u32>) -> HistoryPage<LineageRecord> {
    let reply = fx.query_at(
        &fx.user(),
        "history.work_lineage",
        json!({"rootId": root.as_str(), "page": {"limit": 50}}),
        revision,
    );
    match reply {
        DomainQueryReply::Result {
            result: OperationResult::HistoryWorkLineage { value },
            ..
        } => value,
        other => panic!("{other:?}"),
    }
}
fn row<'a>(page: &'a HistoryPage<LineageRecord>, work: &Id) -> &'a LineageRecord {
    page.items
        .0
        .iter()
        .find(|r| &r.related_id == work)
        .unwrap_or_else(|| panic!("no lineage row for {work:?}"))
}
fn assert_created_by(page: &HistoryPage<LineageRecord>, work: &Id, who: &str) {
    let r = row(page, work);
    assert_eq!(r.actor_id, Some(actor(who)), "{work:?}");
    assert!(
        matches!(r.actor_attribution, Some(ActorAttribution::Recorded)),
        "{work:?}: {:?}",
        r.actor_attribution
    );
}

#[test]
fn work_created_by_an_agent_or_a_person_shows_that_creator_on_its_lineage_rows() {
    let fx = Fx::new();
    let p = fx.project("Creators", &[]);
    let by_person = fx.feature(&p, "Person feature", &[]);
    let by_agent = match applied(fx.command(
        &fx.agent(),
        "features.create",
        json!({"projectId": p.0.as_str(), "creation": {"kind": "Single", "title": "Agent feature",
            "description": "", "repositoryIds": [], "statusId": Fx::status(&p, "backlog"),
            "priority": 0}}),
    )) {
        OperationResult::FeaturesCreate { value } => value.items.0[0].clone(),
        other => panic!("{other:?}"),
    };
    // Cards of each feature by the other principal, and an issue the agent files.
    let person_task = fx.task(&fx.user(), &p, Some(&by_agent), "Person task", 0);
    let agent_task = fx.task(&fx.agent(), &p, Some(&by_person), "Agent task", 0);
    let agent_issue = work_of(applied(fx.command(
        &fx.agent(),
        "issues.create",
        json!({"projectId": p.0.as_str(), "title": "Agent issue", "description": "",
            "repositoryIds": [], "statusId": Fx::status(&p, "backlog"), "priority": 0,
            "featureId": by_agent.id.as_str()}),
    )));

    // A feature's lineage names the creator of each child card.
    let agent_feature = lineage(&fx, &by_agent.id, Some(3));
    assert_created_by(&agent_feature, &person_task.id, "user-a");
    assert_created_by(&agent_feature, &agent_issue.id, "agent-w");
    let person_feature = lineage(&fx, &by_person.id, Some(3));
    assert_created_by(&person_feature, &agent_task.id, "agent-w");

    // A card's lineage names the creator of its parent feature.
    assert_created_by(
        &lineage(&fx, &agent_task.id, Some(3)),
        &by_person.id,
        "user-a",
    );
    assert_created_by(
        &lineage(&fx, &person_task.id, Some(3)),
        &by_agent.id,
        "agent-w",
    );

    // Revision 2 carries the same actor without the revision-3 attribution state; revision 1
    // carries neither.
    let r2 = row(&lineage(&fx, &by_agent.id, Some(2)), &agent_issue.id).clone();
    assert_eq!(r2.actor_id, Some(actor("agent-w")));
    assert!(r2.actor_attribution.is_none());
    let r1 = row(&lineage(&fx, &by_agent.id, None), &agent_issue.id).clone();
    assert!(r1.actor_id.is_none() && r1.actor_attribution.is_none());

    // The stored creator is the creating principal, and an edit by someone else keeps it.
    let edited = fx.edit(&fx.user(), &agent_issue, json!({"title": "Edited issue"}));
    let _ = applied(edited);
    let issue_id = agent_issue.id.clone();
    let stored = fx.read(move |s| s.work().work(&issue_id)).unwrap();
    assert_eq!(stored.created_by, Some(actor("agent-w")));
    assert_eq!(stored.updated_by, Some(actor("user-a")));
    assert_created_by(
        &lineage(&fx, &by_agent.id, Some(3)),
        &agent_issue.id,
        "agent-w",
    );
}
