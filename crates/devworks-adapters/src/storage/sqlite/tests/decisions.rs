//! Native decision records, immutable reviewed content and per-user notification persistence.
use super::{Harness, T0, through};
use crate::storage::sqlite::{MIGRATION_0027_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::context::PrincipalKind;
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id, ProjectId};
use serde_json::json;
use std::sync::{Arc, Barrier};
fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn actor(s: &str) -> ActorId {
    ActorId(id(s))
}
fn project() -> ProjectId {
    ProjectId(id("project"))
}
fn seed(h: &Harness) -> SqliteStore {
    let s = h.store();
    run_write(&s, WriteOptions::default(), |tx| {
        for (name, kind) in [
            ("human-a", PrincipalKind::Human),
            ("human-b", PrincipalKind::Human),
            ("agent", PrincipalKind::Agent),
        ] {
            tx.actors().register(&actor(name), kind, name)?;
        }
        tx.projects().insert_project(&StoredProject {
            id: project(),
            name: "Project".into(),
            description: String::new(),
            lifecycle: "Active".into(),
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 0,
            archive_stop_pending: false,
        })?;
        tx.work().insert_work(&StoredWork {
            id: id("feature"),
            project_id: project(),
            kind: FEATURE.into(),
            parent_feature_id: None,
            status_id: None,
            title: "Feature".into(),
            priority: 0,
            manual_order: 0,
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            created_by: None,
            updated_by: None,
            search_key: "feature".into(),
            caller_ref: None,
            completion: None,
            brief: json!({}),
        })?;
        Ok(())
    })
    .unwrap();
    s
}
fn approval(name: &str) -> StoredApproval {
    StoredApproval {
        id: id(name),
        scope: ConfigScope::Project(project()),
        revision: 1,
        state: "Pending".into(),
        operation_kind: "SavedOperation".into(),
        target_id: id("feature"),
        target_revision: 1,
        content_digest: "a".repeat(64),
        scope_digest: "b".repeat(64),
        summary: "Review exact operation".into(),
        artifact: None,
        payload: json!({"kind":"SavedOperation","operation":"jobs.create","input":{"title":"Exact job"},"originalActor":"agent"}),
        author: actor("agent"),
        author_kind: PrincipalKind::Agent,
        created_at_ms: T0,
        decided_by: None,
        decided_at_ms: None,
        rationale: None,
        restore_epoch: 0,
    }
}
fn put(s: &SqliteStore, a: StoredApproval, expected: Option<i64>) -> Result<(), StorageError> {
    run_write(s, WriteOptions::default(), move |tx| {
        tx.decisions().put_approval(&a, expected)
    })
}
fn approve(s: &SqliteStore, name: &str) -> StoredApproval {
    let mut a = approval(name);
    put(s, a.clone(), None).unwrap();
    a.revision = 2;
    a.state = "Approved".into();
    a.decided_by = Some(actor("human-a"));
    a.decided_at_ms = Some(T0);
    a.rationale = Some("Reviewed exact contents".into());
    put(s, a.clone(), Some(1)).unwrap();
    a
}
fn notification(name: &str, user: &str, source: &str) -> StoredNotification {
    StoredNotification {
        id: id(name),
        revision: 1,
        user_id: actor(user),
        project_id: Some(project()),
        root_id: Some(devworks_core::protocol::RootId(id("feature"))),
        decision_id: None,
        category: "Completion".into(),
        priority: 1,
        title: "Verified completion".into(),
        body: "Complete with evidence".into(),
        source_id: source.into(),
        links: json!([{"kind":"work","targetId":"feature"}]),
        requires_input: false,
        read: false,
        acknowledged: false,
        created_at_ms: T0,
        updated_at_ms: T0,
    }
}
fn put_notification(
    s: &SqliteStore,
    n: StoredNotification,
    e: Option<i64>,
) -> Result<(), StorageError> {
    run_write(s, WriteOptions::default(), move |tx| {
        tx.decisions().put_notification(&n, e)
    })
}
fn filter(user: &str) -> NotificationFilter {
    NotificationFilter {
        user_id: actor(user),
        scope: ConfigScope::Workspace,
        categories: vec![],
        read: None,
        unacknowledged_only: false,
        completion_only: false,
    }
}
fn plan(name: &str) -> StoredProposal {
    StoredProposal {
        id: id(name),
        feature_id: id("feature"),
        revision: 1,
        plan_revision: 1,
        plan_digest: "a".repeat(64),
        state: "Pending".into(),
        requested_by: actor("human-a"),
        requested_at_ms: T0,
        decided_by: None,
        decided_at_ms: None,
        summary: "Exact plan".into(),
        reviewed_snapshot: Some(json!({"draft":{"content":"original plan"},"digest":"original"})),
    }
}

#[test]
fn migration_27_preserves_legacy_absence_and_captures_new_plan_snapshots() {
    let m = MIGRATIONS.iter().find(|m| m.version == 27).unwrap();
    assert_eq!(m.checksum(), MIGRATION_0027_SHA256);
    assert_eq!((m.minimum_reader, m.minimum_writer), (8, 27));
    assert!(!m.rebuilds_tables);
    let h = Harness::new();
    let old = SqliteStore::open_with(h.config(), &through(26)).unwrap();
    old.with_writer_connection(|c|{let db=super::super::conn::db;
        db(c.execute("INSERT INTO projects(id,title,lifecycle,revision,scope_revision,created_at) VALUES('project','Project','Active',1,1,?1)",[T0]))?;
        db(c.execute("INSERT INTO work_items(id,project_id,kind,title,priority,manual_order,revision,scope_revision,created_at,brief_json) VALUES('feature','project','feature','Feature',0,0,1,1,?1,'{}')",[T0]))?;
        db(c.execute("INSERT INTO plan_proposals(id,feature_id,revision,plan_revision,plan_digest,state,requested_by,requested_at,summary) VALUES('legacy','feature',1,1,?1,'Pending','host',?2,'legacy')",rusqlite::params!["a".repeat(64),T0]))?;Ok(())
    }).unwrap();
    drop(old);
    let s = h.store();
    run_write(&s, WriteOptions::default(), |tx| {
        tx.actors()
            .register(&actor("human-a"), PrincipalKind::Human, "Human")?;
        tx.work().put_proposal(&plan("new"), None)
    })
    .unwrap();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        assert!(
            r.work()
                .proposal(&id("legacy"))?
                .unwrap()
                .reviewed_snapshot
                .is_none()
        );
        assert_eq!(
            r.work().proposal(&id("new"))?.unwrap().reviewed_snapshot,
            plan("new").reviewed_snapshot
        );
        Ok(())
    })
    .unwrap();
    run_write(&s, WriteOptions::default(), |tx| {
        tx.work().put_feature_plan(
            &StoredFeaturePlan {
                feature_id: id("feature"),
                revision: 1,
                approved: None,
                approved_by: None,
                approved_at_ms: None,
                pending: Some(json!({"draft":"replacement"})),
                pending_revision: Some(1),
            },
            None,
        )?;
        let mut changed = plan("new");
        changed.revision = 2;
        changed.state = "Superseded".into();
        changed.reviewed_snapshot = Some(json!({"draft":"forged"}));
        assert!(tx.work().put_proposal(&changed, Some(1)).is_err());
        Ok(())
    })
    .unwrap();
    drop(s);
    let legacy = SqliteStore::open_with(h.config(), &through(26)).unwrap();
    assert!(matches!(legacy.info().access, AccessMode::ReadOnly { .. }));
    drop(legacy);
    let s = h.store();
    assert!(s.integrity_check().unwrap().problems.is_empty());
}

#[test]
fn approval_sources_merge_owners_with_exclusive_tied_cursors_and_scope() {
    let h = Harness::new();
    let s = seed(&h);
    put(&s, approval("shared"), None).unwrap();
    run_write(&s, WriteOptions::default(), |tx| {
        tx.configuration().stage(NewProposal {
            id: id("shared"),
            scope: ConfigScope::Project(project()),
            namespace: "settings".into(),
            base_revision: 1,
            author: actor("agent"),
            body: json!({"entries":[]}),
        })?;
        tx.work().put_proposal(&plan("shared"), None)?;
        tx.configuration().stage(NewProposal {
            id: id("workspace"),
            scope: ConfigScope::Workspace,
            namespace: "settings".into(),
            base_revision: 1,
            author: actor("agent"),
            body: json!({"entries":[]}),
        })?;
        Ok(())
    })
    .unwrap();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        let f = ApprovalSourceFilter {
            scope: ConfigScope::Project(project()),
            state: Some("Pending".into()),
        };
        let first = r.decisions().list_approval_sources(&f, None, 2)?;
        assert_eq!(first.len(), 2);
        let tail = first.last().unwrap();
        let position = ApprovalSourcePosition {
            created_at_ms: tail.created_at_ms,
            id: tail.id.clone(),
            source: tail.source,
        };
        let second = r
            .decisions()
            .list_approval_sources(&f, Some(&position), 2)?;
        assert_eq!(second.len(), 1);
        let mut all = first;
        all.extend(second);
        let sources = all
            .iter()
            .map(|r| r.source)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(sources.len(), 3);
        assert!(all.iter().all(|v| v.id == id("shared")));
        assert_eq!(
            r.decisions()
                .list_approval_sources(
                    &ApprovalSourceFilter {
                        scope: ConfigScope::Workspace,
                        state: None
                    },
                    None,
                    500
                )?
                .len(),
            4
        );
        assert!(r.decisions().list_approval_sources(&f, None, 0).is_err());
        Ok(())
    })
    .unwrap();
}

#[test]
fn immutable_approval_and_human_audit_bind_exact_actor_revision_and_snapshot() {
    let h = Harness::new();
    let s = seed(&h);
    let pending = approval("proposal");
    put(&s, pending.clone(), None).unwrap();
    let mut forged = pending.clone();
    forged.revision = 2;
    forged.state = "Approved".into();
    forged.decided_by = Some(actor("agent"));
    forged.decided_at_ms = Some(T0);
    assert!(matches!(
        put(&s, forged, Some(1)),
        Err(StorageError::ScopeDenied)
    ));
    let mut decided = pending.clone();
    decided.revision = 2;
    decided.state = "Approved".into();
    decided.decided_by = Some(actor("human-a"));
    decided.decided_at_ms = Some(T0);
    let mut changed = decided.clone();
    changed.payload = json!({"changed":true});
    assert!(matches!(
        put(&s, changed, Some(1)),
        Err(StorageError::InvalidRecord(_))
    ));
    assert!(matches!(
        put(&s, decided.clone(), Some(2)),
        Err(StorageError::RevisionConflict(_))
    ));
    put(&s, decided.clone(), Some(1)).unwrap();
    let audit = StoredHumanDecision {
        source: ApprovalSource::Generic,
        proposal_id: id("proposal"),
        reviewed_proposal_revision: 1,
        decision: "Approved".into(),
        rationale: Some("Reviewed".into()),
        actor: actor("human-a"),
        at_ms: T0,
        content_digest: decided.content_digest.clone(),
        scope_digest: decided.scope_digest.clone(),
        reviewed_snapshot: pending.payload,
    };
    let invalid = audit.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        let mut other = invalid;
        other.actor = actor("human-b");
        assert!(tx.decisions().insert_human_decision(&other).is_err());
        Ok(())
    })
    .unwrap();
    let saved = audit.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.decisions().insert_human_decision(&saved)
    })
    .unwrap();
    let saved = audit.clone();
    assert!(matches!(
        run_write(&s, WriteOptions::default(), move |tx| tx
            .decisions()
            .insert_human_decision(&saved)),
        Err(StorageError::Duplicate { .. })
    ));
    s.with_writer_connection(|c| {
        assert!(
            c.execute("UPDATE human_decisions SET reviewed_snapshot_json='{}'", [])
                .is_err()
        );
        Ok(())
    })
    .unwrap();
    drop(s);
    let s = h.store();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        assert_eq!(
            r.decisions()
                .human_decision(ApprovalSource::Generic, &id("proposal"))?,
            Some(audit)
        );
        assert_eq!(r.decisions().approval(&id("proposal"))?, Some(decided));
        Ok(())
    })
    .unwrap();
}

#[test]
fn notification_recipient_is_host_bound_human_not_every_registered_actor() {
    let h = Harness::new();
    let s = seed(&h);
    read_snapshot(&s, &ReadOptions::default(), |r| {
        assert!(r.decisions().notification_users()?.is_empty());
        Ok(())
    })
    .unwrap();
    run_write(&s, WriteOptions::default(), |tx| {
        assert!(
            tx.decisions()
                .bind_notification_user(&actor("agent"))
                .is_err()
        );
        assert!(
            tx.decisions()
                .bind_notification_user(&actor("host"))
                .is_err()
        );
        tx.decisions().bind_notification_user(&actor("human-a"))
    })
    .unwrap();
    drop(s);
    let s = h.store();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        assert_eq!(r.decisions().notification_users()?, vec![actor("human-a")]);
        Ok(())
    })
    .unwrap();
}

#[test]
fn notification_read_is_separate_from_acknowledgment_and_actor_isolation_survives_restart() {
    let h = Harness::new();
    let s = seed(&h);
    put(&s, approval("linked-decision"), None).unwrap();
    let mut a = notification("a", "human-a", "verified-event");
    a.decision_id = Some(id("linked-decision"));
    put_notification(&s, a.clone(), None).unwrap();
    let b = notification("b", "human-b", "verified-event");
    put_notification(&s, b.clone(), None).unwrap();
    let mut duplicate = a.clone();
    duplicate.id = id("duplicate");
    assert!(matches!(
        put_notification(&s, duplicate, None),
        Err(StorageError::Duplicate { .. })
    ));
    read_snapshot(&s, &ReadOptions::default(), |r| {
        assert!(
            r.decisions()
                .notification(&actor("human-b"), &id("a"))?
                .is_none()
        );
        assert_eq!(
            r.decisions().notifications(&filter("human-a"), None, 500)?,
            vec![a.clone()]
        );
        Ok(())
    })
    .unwrap();
    a.revision = 2;
    a.read = true;
    put_notification(&s, a.clone(), Some(1)).unwrap();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        let mut cards = filter("human-a");
        cards.completion_only = true;
        cards.unacknowledged_only = true;
        assert_eq!(r.decisions().notifications(&cards, None, 500)?.len(), 1);
        Ok(())
    })
    .unwrap();
    a.revision = 3;
    a.acknowledged = true;
    put_notification(&s, a.clone(), Some(2)).unwrap();
    let mut undo = a.clone();
    undo.revision = 4;
    undo.acknowledged = false;
    assert!(matches!(
        put_notification(&s, undo, Some(3)),
        Err(StorageError::InvalidRecord(_))
    ));
    drop(s);
    let s = h.store();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        assert_eq!(
            r.decisions().notification(&actor("human-a"), &id("a"))?,
            Some(a)
        );
        let mut cards = filter("human-a");
        cards.completion_only = true;
        cards.unacknowledged_only = true;
        assert!(r.decisions().notifications(&cards, None, 500)?.is_empty());
        assert_eq!(
            r.decisions().notification(&actor("human-b"), &id("b"))?,
            Some(b)
        );
        assert_eq!(
            r.decisions()
                .approval(&id("linked-decision"))?
                .unwrap()
                .state,
            "Pending"
        );
        Ok(())
    })
    .unwrap();
}

#[test]
fn notification_concurrent_dedup_and_tied_paging_are_bounded() {
    let h = Harness::new();
    let s = Arc::new(seed(&h));
    let barrier = Arc::new(Barrier::new(3));
    let handles = ["a", "b"]
        .into_iter()
        .map(|name| {
            let s = s.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                put_notification(&s, notification(name, "human-a", "source"), None)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let results = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    put_notification(&s, notification("c", "human-a", "other"), None).unwrap();
    read_snapshot(s.as_ref(), &ReadOptions::default(), |r| {
        let first = r.decisions().notifications(&filter("human-a"), None, 1)?;
        assert_eq!(first[0].id, id("c"));
        let position = NotificationPosition {
            created_at_ms: first[0].created_at_ms,
            id: first[0].id.clone(),
        };
        assert_eq!(
            r.decisions()
                .notifications(&filter("human-a"), Some(&position), 1)?
                .len(),
            1
        );
        assert!(
            r.decisions()
                .notifications(&filter("human-a"), None, MAX_PAGE + 1)
                .is_err()
        );
        Ok(())
    })
    .unwrap();
}

fn resume() -> StoredApprovalResume {
    StoredApprovalResume {
        approval_id: id("saved"),
        revision: 1,
        state: "Pending".into(),
        principal: actor("agent"),
        principal_kind: PrincipalKind::Agent,
        invocation: json!({"projectId":"project","rootId":"feature","originalActor":"agent","restoreEpoch":0}),
        operation: "jobs.create".into(),
        schema_revision: 3,
        input: json!({"title":"Exact job"}),
        request_key: id("deterministic-request"),
        request_digest: "c".repeat(64),
        grant_id: Some(id("internal-grant")),
        created_at_ms: T0,
        updated_at_ms: T0,
        result: None,
    }
}
#[test]
fn private_resume_preserves_original_actor_exact_request_and_unknown_replay_identity() {
    let h = Harness::new();
    let s = seed(&h);
    approve(&s, "saved");
    run_write(&s, WriteOptions::default(), |tx| {
        tx.authority().issue_grant(NewAuthorityGrant {
            id: id("internal-grant"),
            actor: actor("agent"),
            kind: GrantKind::Operation,
            issued_by: actor("human-a"),
            binding: json!({"operation":"jobs.create","requestDigest":"exact"}),
            expires_at_ms: None,
            uses: Some(1),
        })?;
        tx.decisions().put_approval_resume(&resume(), None)
    })
    .unwrap();
    let mut wrong = resume();
    wrong.revision = 2;
    wrong.state = "Unknown".into();
    wrong.principal = actor("human-a");
    wrong.principal_kind = PrincipalKind::Human;
    assert!(matches!(
        run_write(&s, WriteOptions::default(), move |tx| tx
            .decisions()
            .put_approval_resume(&wrong, Some(1))),
        Err(StorageError::ScopeDenied)
    ));
    let mut pending = resume();
    pending.revision = 2;
    pending.state = "Unknown".into();
    pending.result = Some(json!({"outcome":"CommitUnknown"}));
    let saved = pending.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.decisions().put_approval_resume(&saved, Some(1))
    })
    .unwrap();
    drop(s);
    let s = h.store();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        assert_eq!(
            r.decisions().pending_approval_resumes(None, 500)?,
            vec![pending.clone()]
        );
        Ok(())
    })
    .unwrap();
    let mut changed = pending.clone();
    changed.revision = 3;
    changed.state = "Admitted".into();
    changed.request_key = id("new-retry-key");
    assert!(matches!(
        run_write(&s, WriteOptions::default(), move |tx| tx
            .decisions()
            .put_approval_resume(&changed, Some(2))),
        Err(StorageError::InvalidRecord(_))
    ));
    pending.revision = 3;
    pending.state = "Admitted".into();
    pending.result = Some(json!({"operationId":"native-operation"}));
    let saved = pending.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.decisions().put_approval_resume(&saved, Some(2))
    })
    .unwrap();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        assert!(
            r.decisions()
                .pending_approval_resumes(None, 500)?
                .is_empty()
        );
        assert_eq!(r.decisions().approval_resume(&id("saved"))?, Some(pending));
        Ok(())
    })
    .unwrap();
}

#[test]
fn legal_large_source_snapshot_is_retained_without_a_new_one_mib_truncation() {
    let h = Harness::new();
    let s = seed(&h);
    let mut a = approval("large");
    a.operation_kind = "FindingRisk".into();
    a.payload =
        json!({"kind":"FindingRisk","plan":{"content":"x".repeat(MAX_DECISION_JSON_BYTES+1024)}});
    put(&s, a.clone(), None).unwrap();
    a.revision = 2;
    a.state = "Approved".into();
    a.decided_by = Some(actor("human-a"));
    a.decided_at_ms = Some(T0);
    put(&s, a.clone(), Some(1)).unwrap();
    let audit = StoredHumanDecision {
        source: ApprovalSource::Generic,
        proposal_id: a.id.clone(),
        reviewed_proposal_revision: 1,
        decision: "Approved".into(),
        rationale: Some("Exact reviewed source".into()),
        actor: actor("human-a"),
        at_ms: T0,
        content_digest: a.content_digest.clone(),
        scope_digest: a.scope_digest.clone(),
        reviewed_snapshot: a.payload.clone(),
    };
    let saved = audit.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.decisions().insert_human_decision(&saved)
    })
    .unwrap();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        assert_eq!(r.decisions().approval(&id("large"))?, Some(a));
        assert_eq!(
            r.decisions()
                .human_decision(ApprovalSource::Generic, &id("large"))?,
            Some(audit)
        );
        Ok(())
    })
    .unwrap();
}

#[test]
fn pending_resume_id_cursor_reaches_later_work_behind_waiting_rows() {
    let h = Harness::new();
    let s = seed(&h);
    for name in ["resume-a", "resume-b", "resume-c"] {
        let mut a = approval(name);
        a.author = actor("human-a");
        a.author_kind = PrincipalKind::Human;
        a.payload = json!({"kind":"SavedOperation","originalActor":"human-a"});
        put(&s, a.clone(), None).unwrap();
        a.revision = 2;
        a.state = "Approved".into();
        a.decided_by = Some(actor("human-a"));
        a.decided_at_ms = Some(T0);
        put(&s, a, Some(1)).unwrap();
        let mut row = resume();
        row.approval_id = id(name);
        row.principal = actor("human-a");
        row.principal_kind = PrincipalKind::Human;
        row.grant_id = None;
        row.request_key = id(&format!("key-{name}"));
        row.invocation = json!({"originalActor":"human-a","projectId":"project"});
        run_write(&s, WriteOptions::default(), move |tx| {
            tx.decisions().put_approval_resume(&row, None)
        })
        .unwrap();
    }
    read_snapshot(&s, &ReadOptions::default(), |r| {
        let first = r.decisions().pending_approval_resumes(None, 1)?;
        assert_eq!(first[0].approval_id, id("resume-a"));
        let next = r
            .decisions()
            .pending_approval_resumes(Some(&first[0].approval_id), 1)?;
        assert_eq!(next[0].approval_id, id("resume-b"));
        let last = r
            .decisions()
            .pending_approval_resumes(Some(&next[0].approval_id), 1)?;
        assert_eq!(last[0].approval_id, id("resume-c"));
        assert!(
            r.decisions()
                .pending_approval_resumes(Some(&last[0].approval_id), 1)?
                .is_empty()
        );
        Ok(())
    })
    .unwrap();
}

#[test]
fn project_tombstone_preserves_decision_history_and_does_not_remove_notification_cards() {
    let h = Harness::new();
    let s = seed(&h);
    let a = approve(&s, "retained");
    let n = notification("card", "human-a", "verified-retained");
    put_notification(&s, n.clone(), None).unwrap();
    for (state, expected) in [("Archived", 1), ("Deleted", 2)] {
        run_write(&s, WriteOptions::default(), move |tx| {
            let mut p = tx.projects().project(&project())?.unwrap();
            p.lifecycle = state.into();
            p.revision = expected + 1;
            p.archived_at_ms = Some(T0);
            if state == "Deleted" {
                p.deleted_at_ms = Some(T0);
            }
            tx.projects().update_project(&p, expected)
        })
        .unwrap();
    }
    read_snapshot(&s, &ReadOptions::default(), |r| {
        assert_eq!(r.decisions().approval(&a.id)?, Some(a.clone()));
        assert_eq!(
            r.decisions().notification(&actor("human-a"), &n.id)?,
            Some(n.clone())
        );
        Ok(())
    })
    .unwrap();
}
