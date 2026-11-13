//! Shared notification commands and real work-completion producers on native SQLite.
use super::*;
use devworks_core::services::notifications::{self as notices, NotificationService};
fn fixture() -> Fx {
    let mut fx = Fx::new();
    fx.cases = fx
        .cases
        .clone()
        .with_notifications(Arc::new(NotificationService::new(fx.h.ids.clone())));
    fx.write(|tx| {
        tx.actors()
            .register(&actor("user-b"), PrincipalKind::Human, "Other human")?;
        tx.decisions().bind_notification_user(&actor("user-a"))
    });
    fx
}
fn rows(fx: &Fx) -> Vec<StoredNotification> {
    fx.read(|s| {
        s.decisions().notifications(
            &NotificationFilter {
                user_id: actor("user-a"),
                scope: ConfigScope::Workspace,
                categories: vec![],
                read: None,
                unacknowledged_only: false,
                completion_only: false,
            },
            None,
            500,
        )
    })
}
fn source(fx: &Fx, p: &ProjectId) -> Id {
    source_named(fx, p, "notification-proposal")
}
fn source_named(fx: &Fx, p: &ProjectId, name: &'static str) -> Id {
    let p = p.clone();
    fx.write(move |tx| {
        let a = StoredApproval {
            id: id(name),
            scope: ConfigScope::Project(p.clone()),
            revision: 1,
            state: "Pending".into(),
            operation_kind: "SavedOperation".into(),
            target_id: id("target"),
            target_revision: 1,
            content_digest: "a".repeat(64),
            scope_digest: "b".repeat(64),
            summary: "Review exact operation".into(),
            artifact: None,
            payload: json!({"input":{}}),
            author: actor("agent-w"),
            author_kind: PrincipalKind::Agent,
            created_at_ms: tx.now_ms(),
            decided_by: None,
            decided_at_ms: None,
            rationale: None,
            restore_epoch: tx.store().restore_epoch,
        };
        tx.decisions().put_approval(&a, None)?;
        notices::notify_approval(
            tx,
            ApprovalSource::Generic,
            &a.id,
            Some(&p),
            "Review needed",
            "An exact proposal needs your decision",
        )?;
        Ok(a.id)
    })
}
fn query_value(fx: &Fx, ctx: &InvocationContext, op: &str, body: Value, revision: u32) -> Value {
    match fx.query_at(ctx, op, body, Some(revision)) {
        DomainQueryReply::Result { result, .. } => {
            serde_json::to_value(result).unwrap()["value"].clone()
        }
        other => panic!("{other:?}"),
    }
}
fn changed_value(reply: DomainCommandReply) -> Value {
    serde_json::to_value(applied(reply)).unwrap()["value"].clone()
}
fn list(fx: &Fx, body: Value) -> Value {
    query_value(fx, &fx.user(), "notifications.list", body, 3)
}

fn completions(fx: &Fx) -> Vec<StoredNotification> {
    rows(fx)
        .into_iter()
        .filter(|r| r.category == "Completion")
        .collect()
}

#[test]
fn read_read_state_and_acknowledgment_are_separate_per_user_and_persist() {
    let fx = fixture();
    let p = fx.project("Notifications", &[]);
    let proposal = source(&fx, &p);
    let original = rows(&fx).pop().unwrap();
    let read = query_value(
        &fx,
        &fx.user(),
        "notifications.read",
        json!({"notificationId":original.id}),
        3,
    );
    assert_eq!(read["read"], false);
    assert_eq!(read["acknowledged"], false);
    assert_eq!(rows(&fx)[0].revision, 1);
    let mark = changed_value(fx.command_at(
        &fx.user(),
        "notifications.mark_read",
        json!({"notificationId":original.id,"expectedRevision":"1","read":true}),
        Some(3),
    ));
    assert_eq!(mark["read"], true);
    assert_eq!(mark["acknowledged"], false);
    let ack = changed_value(fx.command(
        &fx.user(),
        "notifications.acknowledge",
        json!({"notificationId":original.id,"expectedRevision":"2"}),
    ));
    assert_eq!(ack["acknowledged"], true);
    assert_eq!(
        fx.read(|s| s.decisions().approval(&proposal))
            .unwrap()
            .state,
        "Pending"
    );
    assert!(
        list(
            &fx,
            json!({"scope":{"kind":"Workspace"},"page":{"limit":50},"unacknowledgedOnly":true})
        )["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let all = list(
        &fx,
        json!({"scope":{"kind":"Workspace"},"page":{"limit":50},"unacknowledgedOnly":false}),
    );
    assert_eq!(all["items"].as_array().unwrap().len(), 1);
    let other = fx.ctx("user-b", PrincipalKind::Human, None);
    assert!(
        query_value(
            &fx,
            &other,
            "notifications.list",
            json!({"scope":{"kind":"Workspace"},"page":{"limit":50},"unacknowledgedOnly":false}),
            3
        )["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        matches!(fx.query_at(&other,"notifications.read",json!({"notificationId":original.id}),Some(3)),DomainQueryReply::Rejected{fault,..}if fault.code==DomainErrorCode::NotFound)
    );
    let keep = rows(&fx)[0].clone();
    let p2 = p.clone();
    let proposal2 = proposal.clone();
    fx.write(move |tx| {
        notices::notify_approval(
            tx,
            ApprovalSource::Generic,
            &proposal2,
            Some(&p2),
            "Changed summary cannot replace original",
            "different",
        )
    });
    assert_eq!(rows(&fx), vec![keep.clone()]);
    let Fx { h, store, .. } = fx;
    drop(store);
    let reopened = h.store();
    let actual = read_snapshot(&reopened, &ReadOptions::default(), |s| {
        s.decisions().notification(&actor("user-a"), &keep.id)
    })
    .unwrap()
    .unwrap();
    assert_eq!(actual, keep);
}

#[test]
fn a_list_cursor_survives_an_unrelated_commit() {
    let fx = fixture();
    let p = fx.project("Paging", &[]);
    source_named(&fx, &p, "first-proposal");
    source_named(&fx, &p, "second-proposal");
    let body = |cursor: Option<&Value>| {
        let mut b =
            json!({"scope":{"kind":"Workspace"},"page":{"limit":1},"unacknowledgedOnly":false});
        if let Some(c) = cursor {
            b["page"]["cursor"] = c.clone();
        }
        b
    };
    let first = list(&fx, body(None));
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    assert_eq!(first["hasMore"], true);
    let before = fx.read(|s| Ok(s.stamp().committed_sequence));
    fx.project("Unrelated", &[]);
    assert!(fx.read(|s| Ok(s.stamp().committed_sequence)) > before);
    let second = list(&fx, body(Some(&first["nextCursor"])));
    let items = second["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_ne!(items[0]["id"], first["items"][0]["id"]);
    assert_eq!(second["hasMore"], false);
    assert!(second.get("nextCursor").is_none());
}

#[test]
fn canonical_preferences_round_trip_and_os_eligibility_rechecks_pending_source() {
    let fx = fixture();
    let p = fx.project("Settings", &[]);
    let proposal = source(&fx, &p);
    let row = rows(&fx)[0].clone();
    let prefs = query_value(&fx, &fx.user(), "notifications.settings", json!({}), 1);
    assert_eq!(prefs["revision"], "1");
    assert_eq!(prefs["config"], json!({"desktopEnabled":true,"kinds":[]}));
    let disabled = changed_value(fx.command(
        &fx.user(),
        "notifications.settings",
        json!({"expectedRevision":"1","config":{"desktopEnabled":false,"kinds":["Approval"]}}),
    ));
    assert_eq!(disabled["revision"], "2");
    assert_eq!(
        rows(&fx).len(),
        1,
        "OS preferences never remove persistent in-app input"
    );
    assert!(!fx.read(|s| notices::os_eligible(s, &actor("user-a"), &row)));
    let stale = fx.command(
        &fx.user(),
        "notifications.settings",
        json!({"expectedRevision":"1","config":{"desktopEnabled":true,"kinds":[]}}),
    );
    assert!(
        !matches!(stale,CommandReply::Recorded{receipt,..} if matches!(receipt.decision,DomainDecision::Applied{..}))
    );
    let saved = changed_value(fx.command(
        &fx.user(),
        "notifications.settings",
        json!({"expectedRevision":"2","config":{"desktopEnabled":true,"kinds":["Approval"]}}),
    ));
    assert_eq!(saved["revision"], "3");
    assert!(fx.read(|s| notices::os_eligible(s, &actor("user-a"), &row)));
    let r = row.clone();
    assert!(fx.write(move |tx| notices::os_eligible_tx(tx, &actor("user-a"), &r)));
    changed_value(fx.command_at(
        &fx.user(),
        "notifications.mark_read",
        json!({"notificationId":row.id,"expectedRevision":"1","read":true}),
        Some(3),
    ));
    let read = rows(&fx)[0].clone();
    assert!(
        fx.read(|s| notices::os_eligible(s, &actor("user-a"), &read)),
        "reading did not decide input"
    );
    let proposal_id = proposal.clone();
    fx.write(move |tx| {
        let mut a = tx.decisions().approval(&proposal_id)?.unwrap();
        a.revision += 1;
        a.state = "Approved".into();
        a.decided_by = Some(actor("user-a"));
        a.decided_at_ms = Some(tx.now_ms());
        a.rationale = Some("Exact decision".into());
        tx.decisions().put_approval(&a, Some(1))
    });
    assert!(!fx.read(|s| notices::os_eligible(s, &actor("user-a"), &read)));
    let r = read.clone();
    assert!(!fx.write(move |tx| notices::os_eligible_tx(tx, &actor("user-a"), &r)));
    let other = fx.ctx("user-b", PrincipalKind::Human, None);
    assert_eq!(
        query_value(&fx, &other, "notifications.settings", json!({}), 1)["config"]["desktopEnabled"],
        true
    );
}

#[test]
fn verified_issue_transition_creates_one_card_but_manual_done_and_linked_tasks_do_not() {
    let fx = fixture();
    let p = fx.project("Completion", &[]);
    let feature = fx.feature(&p, "Parent", &[]);
    for (index, parent) in [None, Some(&feature)].into_iter().enumerate() {
        let task = fx.task(&fx.user(), &p, parent, &format!("Card {index}"), 0);
        let task = work_of(applied(fx.edit(
            &fx.user(),
            &task,
            json!({"acceptance":[{"id":"C1","text":"passes","evidenceIds":[],"state":"Unmet"}]}),
        )));
        let done=work_of(applied(fx.command(&fx.user(),"tasks.move",json!({"taskId":task.id,"expectedRevision":task.revision,"statusId":Fx::status(&p,"done"),"expectedWorkflowRevision":"1"}))));
        assert_eq!(
            completions(&fx).len(),
            index,
            "manual Done did not announce verified completion"
        );
        let evidence = fx.evidence(&done.id);
        let verified=work_of(applied(fx.command(&fx.agent(),"tasks.acceptance_evidence",json!({"taskId":done.id,"expectedRevision":done.revision,"criteria":[{"id":"C1","text":"passes","evidenceIds":[evidence],"state":"Met"}]}))));
        assert_eq!(
            completions(&fx).len(),
            1,
            "a linked task is not a completed feature/issue"
        );
        if index == 0 {
            let card = completions(&fx)[0].clone();
            assert_eq!(card.root_id, Some(RootId(verified.id.clone())));
            assert_eq!(card.category, "Completion");
            assert!(!card.requires_input);
            let edited = work_of(applied(fx.edit(
                &fx.user(),
                &verified,
                json!({"title":"Still verified"}),
            )));
            assert_eq!(
                completions(&fx).len(),
                1,
                "later edits do not duplicate the transition"
            );
            changed_value(fx.command(
                &fx.user(),
                "notifications.acknowledge",
                json!({"notificationId":card.id,"expectedRevision":"1"}),
            ));
            query_value(
                &fx,
                &fx.user(),
                "notifications.read",
                json!({"notificationId":card.id}),
                3,
            );
            assert!(completions(&fx)[0].acknowledged);
            assert_eq!(
                fx.read(|s| s.work().work(&edited.id))
                    .unwrap()
                    .completion
                    .as_deref(),
                Some(VERIFIED)
            );
        }
    }
}

#[test]
fn verified_feature_acceptance_produces_a_completion_card_from_the_shared_work_operation() {
    let fx = fixture();
    let p = fx.project("Feature completion", &[]);
    let feature = fx.feature(&p, "Feature", &[]);
    let doc = fx.artifact(&p, 1);
    let feature = fx.plan(&feature, &doc, &[("E1", "deliver")]);
    let feature = fx.approve(&feature);
    let done=work_of(applied(fx.command_at(&fx.user(),"features.edit",json!({"workId":feature.id,"expectedRevision":feature.revision,"changes":{"statusId":Fx::status(&p,"done")}}),Some(2))));
    assert!(completions(&fx).is_empty());
    let evidence = fx.evidence(&done.id);
    let verified=work_of(applied(fx.command_at(&fx.agent(),"features.edit",json!({"workId":done.id,"expectedRevision":done.revision,"changes":{"acceptanceEvidence":[{"id":"A1","text":"works","evidenceIds":[evidence],"state":"Met"}]}}),Some(2))));
    assert_eq!(
        fx.read(|s| s.work().work(&verified.id))
            .unwrap()
            .completion
            .as_deref(),
        Some(VERIFIED)
    );
    assert_eq!(completions(&fx).len(), 1);
    assert_eq!(completions(&fx)[0].root_id, Some(RootId(verified.id)));
}

#[test]
fn producer_preserves_legal_multibyte_character_bounds_through_the_shared_read_gateway() {
    let fx = fixture();
    let title = "界".repeat(256);
    let body = "語".repeat(65_536);
    let expected_title = title.clone();
    let expected_body = body.clone();
    fx.write(move |tx| {
        notices::publish(
            tx,
            &notices::NotificationDraft {
                source: "unicode-source".into(),
                category: "Completion".into(),
                decision_id: None,
                project_id: None,
                root_id: None,
                title,
                body,
                links: vec![],
                requires_input: false,
                priority: 0,
            },
        )
    });
    let row = rows(&fx).pop().unwrap();
    assert_eq!(row.title, expected_title);
    assert_eq!(row.body, expected_body);
    let result = query_value(
        &fx,
        &fx.user(),
        "notifications.read",
        json!({"notificationId":row.id}),
        3,
    );
    assert_eq!(result["title"], expected_title);
    assert_eq!(result["body"], expected_body);
    assert!(!rows(&fx)[0].read);
    assert!(!rows(&fx)[0].acknowledged);
}
