//! Real SQLite allocation concurrency, source fences and workflow mutation transactions.
use super::{Harness, T0, through};
use crate::storage::sqlite::{MIGRATION_0026_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::ports::storage::contract::StoreHarness;
use devworks_core::ports::storage::*;
use devworks_core::protocol::{Id, ProjectId, RootId};
use serde_json::json;
use std::sync::{Arc, Barrier};

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn project() -> ProjectId {
    ProjectId(id("project"))
}
fn work(name: &str, kind: &str, parent: Option<&str>) -> StoredWork {
    StoredWork {
        id: id(name),
        project_id: project(),
        kind: kind.into(),
        parent_feature_id: parent.map(id),
        status_id: Some(id("ready")),
        title: name.into(),
        priority: 0,
        manual_order: 0,
        revision: 1,
        scope_revision: 1,
        created_at_ms: T0,
        updated_at_ms: T0,
        created_by: None,
        updated_by: None,
        search_key: name.into(),
        caller_ref: None,
        completion: None,
        brief: json!({}),
    }
}
fn seeded(h: &Harness) -> SqliteStore {
    let s = h.store();
    run_write(&s, WriteOptions::default(), |tx| {
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
        for name in ["ready", "destination"] {
            tx.work().insert_status(&StoredStatus {
                id: id(name),
                project_id: project(),
                name: name.into(),
                semantic: "Ready".into(),
                sort_order: if name == "ready" { 1 } else { 2 },
                revision: 1,
            })?;
        }
        tx.work().insert_work(&work("feature", "feature", None))?;
        tx.work().insert_work(&work("issue", "task", None))?;
        tx.work()
            .insert_work(&work("task-a", "task", Some("feature")))?;
        tx.work()
            .insert_work(&work("task-b", "task", Some("feature")))?;
        Ok(())
    })
    .unwrap();
    s
}
fn claim(s: &SqliteStore, name: &str, work_id: &str, feature: bool) -> StoredAllocationClaim {
    let is_task = work_id.starts_with("task-");
    StoredAllocationClaim {
        id: id(name),
        project_id: project(),
        root_id: RootId(id(if is_task { "feature" } else { work_id })),
        work_id: id(work_id),
        kind: if feature { "feature" } else { "task" }.into(),
        queue_key: if is_task {
            feature_queue(&id("feature"))
        } else {
            PROJECT_QUEUE.into()
        },
        owner_id: s.info().host_actor,
        session_id: None,
        ingress_namespace: id("ingress"),
        invocation_id: id(name),
        fence: 1,
        expires_at_ms: T0 + 60_000,
        state: "Active".into(),
        revision: 1,
        work_revision: 1,
        parent_feature_id: is_task.then(|| id("feature")),
        root_control_revision: 1,
        queue_revision: 1,
        feature_task_set_revision: feature.then_some(1),
        created_at_ms: T0,
        execution_id: None,
    }
}
fn insert(s: &SqliteStore, c: StoredAllocationClaim) -> Result<(), StorageError> {
    run_write(s, WriteOptions::default(), move |tx| {
        tx.work().insert_allocation_claim(&c)
    })
}
fn update(s: &SqliteStore, c: StoredAllocationClaim, expected: i64) -> Result<(), StorageError> {
    run_write(s, WriteOptions::default(), move |tx| {
        tx.work().update_allocation_claim(&c, expected)
    })
}
fn get(s: &SqliteStore, name: &str) -> StoredAllocationClaim {
    read_snapshot(s, &ReadOptions::default(), |r| {
        r.work().allocation_claim(&id(name))
    })
    .unwrap()
    .unwrap()
}

#[test]
fn migration_26_is_checksummed_and_closes_unaware_writers_without_rebuilding() {
    let migration = MIGRATIONS.iter().find(|m| m.version == 26).unwrap();
    assert_eq!(migration.checksum(), MIGRATION_0026_SHA256);
    assert_eq!(
        (migration.minimum_reader, migration.minimum_writer),
        (8, 26)
    );
    assert!(!migration.rebuilds_tables);
    let h = Harness::new();
    drop(SqliteStore::open_with(h.config(), &through(25)).unwrap());
    let s = seeded(&h);
    insert(&s, claim(&s, "claim", "issue", false)).unwrap();
    drop(s);
    let legacy = SqliteStore::open_with(h.config(), &through(25)).unwrap();
    assert!(matches!(legacy.info().access, AccessMode::ReadOnly { .. }));
    drop(legacy);
    let s = h.store();
    assert_eq!(get(&s, "claim").work_id, id("issue"));
    assert!(s.integrity_check().unwrap().problems.is_empty());
}

#[test]
fn concurrent_claims_have_one_project_owner_and_durable_invocation_identity() {
    let h = Harness::new();
    let s = Arc::new(seeded(&h));
    let barrier = Arc::new(Barrier::new(3));
    let handles = [("claim-a", "issue", false), ("claim-b", "feature", true)]
        .into_iter()
        .map(|(name, work, feature)| {
            let c = claim(&s, name, work, feature);
            let s = s.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                insert(&s, c)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(StorageError::Duplicate { .. })))
            .count(),
        1
    );
    let saved = read_snapshot(s.as_ref(), &ReadOptions::default(), |r| {
        r.work().allocation_claims(&project(), None, 500)
    })
    .unwrap();
    assert_eq!(saved.len(), 1);
    let saved = saved[0].clone();
    assert!(matches!(
        insert(&s, saved.clone()),
        Err(StorageError::Duplicate { .. })
    ));
    let replay = read_snapshot(s.as_ref(), &ReadOptions::default(), |r| {
        r.work()
            .allocation_by_invocation(&saved.ingress_namespace, &saved.invocation_id)
    })
    .unwrap();
    assert_eq!(replay, Some(saved));
}

#[test]
fn leases_expire_explicitly_and_old_owner_cannot_overwrite_new_claim() {
    let h = Harness::new();
    let s = seeded(&h);
    let original = claim(&s, "old", "issue", false);
    insert(&s, original.clone()).unwrap();
    let mut expired = original.clone();
    expired.state = "Expired".into();
    expired.revision = 2;
    assert!(matches!(
        update(&s, expired.clone(), 1),
        Err(StorageError::InvalidRecord(_))
    ));
    h.set_now(T0 + 60_001);
    update(&s, expired.clone(), 1).unwrap();
    let mut fresh = claim(&s, "new", "issue", false);
    fresh.fence = 2;
    fresh.expires_at_ms = T0 + 120_000;
    insert(&s, fresh).unwrap();
    let mut stale = original;
    stale.revision = 2;
    stale.state = "Released".into();
    assert!(matches!(
        update(&s, stale, 1),
        Err(StorageError::RevisionConflict(_))
    ));
    expired.revision = 3;
    expired.state = "Active".into();
    expired.expires_at_ms = T0 + 120_000;
    assert!(matches!(
        update(&s, expired, 2),
        Err(StorageError::InvalidRecord(_))
    ));
}

#[test]
fn claim_admission_binds_queue_work_task_set_association_and_control() {
    let h = Harness::new();
    let s = seeded(&h);
    for index in 0..5 {
        let mut invalid = claim(&s, &format!("bad-{index}"), "feature", true);
        match index {
            0 => invalid.queue_revision = 2,
            1 => invalid.work_revision = 2,
            2 => invalid.feature_task_set_revision = Some(2),
            3 => invalid.parent_feature_id = Some(id("feature")),
            _ => invalid.root_control_revision = 2,
        };
        assert!(matches!(
            insert(&s, invalid),
            Err(StorageError::InvalidRecord(_))
        ));
    }
    run_write(&s, WriteOptions::default(), |tx| {
        let mut control = StoredRootControl::initial(RootId(id("issue")));
        control.desired = DesiredExecution::Paused;
        tx.runs().insert_root_control(&control)
    })
    .unwrap();
    assert!(matches!(
        insert(&s, claim(&s, "paused", "issue", false)),
        Err(StorageError::InvalidRecord(_))
    ));
}

#[test]
fn membership_writes_invalidate_unstarted_claims_and_preserve_prior_snapshot() {
    let h = Harness::new();
    let s = seeded(&h);
    insert(&s, claim(&s, "claim", "issue", false)).unwrap();
    let change = |tx: &mut dyn WriteTransaction| {
        let mut w = tx.work().work(&id("issue"))?.unwrap();
        w.parent_feature_id = Some(id("feature"));
        w.revision += 1;
        tx.work().update_work(&w, 1)
    };
    run_write(&s, WriteOptions::default(), change).unwrap();
    let invalidated = get(&s, "claim");
    assert_eq!(
        (&*invalidated.state, invalidated.revision),
        ("Invalidated", 2)
    );
    assert_eq!(invalidated.parent_feature_id, None);
    let mut next = claim(&s, "linked", "task-a", false);
    next.work_id = id("issue");
    next.root_id = RootId(id("feature"));
    next.work_revision = 2;
    next.fence = 2;
    insert(&s, next).unwrap();
}

#[test]
fn independent_child_claims_page_exclusively_and_reordering_does_not_preempt() {
    let h = Harness::new();
    let s = seeded(&h);
    for (name, w) in [("claim-c", "task-a"), ("claim-a", "task-b")] {
        insert(&s, claim(&s, name, w, false)).unwrap();
    }
    read_snapshot(&s, &ReadOptions::default(), |r| {
        let first = r.work().allocation_claims(&project(), None, 1)?;
        assert_eq!(first[0].id, id("claim-a"));
        let next = r
            .work()
            .allocation_claims(&project(), Some(&first[0].id), 1)?;
        assert_eq!(next[0].id, id("claim-c"));
        for limit in [0, MAX_PAGE + 1] {
            assert!(r.work().allocation_claims(&project(), None, limit).is_err());
        }
        Ok(())
    })
    .unwrap();
    run_write(&s, WriteOptions::default(), |tx| {
        tx.work()
            .bump_queue(&project(), &feature_queue(&id("feature")))
    })
    .unwrap();
    let mut kept = get(&s, "claim-a");
    kept.revision += 1;
    kept.expires_at_ms += 1000;
    update(&s, kept, 1).unwrap();
    assert_eq!(get(&s, "claim-a").state, "Active");
}

#[test]
fn workflow_rename_order_and_occupied_removal_share_the_write_transaction() {
    let h = Harness::new();
    let s = seeded(&h);
    run_write(&s, WriteOptions::default(), |tx| {
        let mut status = tx
            .work()
            .statuses(&project())?
            .into_iter()
            .find(|s| s.id == id("ready"))
            .unwrap();
        status.name = "Queued".into();
        status.sort_order = 3;
        status.revision = 2;
        tx.work().update_status(&status, 1)?;
        assert!(matches!(
            tx.work().update_status(&status, 1),
            Err(StorageError::RevisionConflict(_))
        ));
        assert!(
            tx.work()
                .delete_status(&project(), &id("ready"), 2)
                .is_err()
        );
        Ok(())
    })
    .unwrap();
    let reassign = |tx: &mut dyn WriteTransaction| {
        for name in ["feature", "issue", "task-a", "task-b"] {
            let mut w = tx.work().work(&id(name))?.unwrap();
            w.status_id = Some(id("destination"));
            w.revision += 1;
            tx.work().update_work(&w, 1)?;
        }
        tx.work().delete_status(&project(), &id("ready"), 2)
    };
    run_write(&s, WriteOptions::default(), move |tx| {
        reassign(tx)?;
        Err::<(), _>(StorageError::Cancelled)
    })
    .unwrap_err();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        assert_eq!(
            r.work().work(&id("issue"))?.unwrap().status_id,
            Some(id("ready"))
        );
        assert_eq!(r.work().statuses(&project())?.len(), 2);
        Ok(())
    })
    .unwrap();
    run_write(&s, WriteOptions::default(), reassign).unwrap();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        assert_eq!(r.work().statuses(&project())?.len(), 1);
        assert_eq!(
            r.work().work(&id("issue"))?.unwrap().status_id,
            Some(id("destination"))
        );
        Ok(())
    })
    .unwrap();
}

fn activate(s: &SqliteStore, root: &str) -> StoredActivation {
    let activation = StoredActivation {
        id: id("execution"),
        root_id: RootId(id(root)),
        boot_id: s.info().boot_id.unwrap(),
        generation: 1,
        phase: ActivationPhase::Pending,
        purpose: ActivationPurpose::Implementation,
        owner: None,
        lease_until_ms: None,
        owner_epoch: 0,
        revision: 1,
        attempts: 0,
        ready_at_ms: T0,
        created_at_ms: T0,
        stop_cause: None,
        stop_requested_at_ms: None,
        stop_acknowledged_at_ms: None,
        stopped_at_ms: None,
        outcome: None,
        detail: None,
        plan_revision: None,
    };
    let saved = activation.clone();
    run_write(s, WriteOptions::default(), move |tx| {
        tx.runs().insert_activation(&saved)
    })
    .unwrap();
    activation
}
#[test]
fn transferred_execution_survives_expiry_membership_and_restart_without_duplicate_ownership() {
    let h = Harness::new();
    let s = seeded(&h);
    insert(&s, claim(&s, "claim", "issue", false)).unwrap();
    let mut activation = activate(&s, "issue");
    let mut transferred = get(&s, "claim");
    transferred.state = "Transferred".into();
    transferred.execution_id = Some(activation.id.clone());
    transferred.revision = 2;
    update(&s, transferred, 1).unwrap();
    run_write(&s, WriteOptions::default(), |tx| {
        let mut w = tx.work().work(&id("issue"))?.unwrap();
        w.parent_feature_id = Some(id("feature"));
        w.revision = 2;
        tx.work().update_work(&w, 1)
    })
    .unwrap();
    assert_eq!(get(&s, "claim").state, "Transferred");
    let mut rebound = get(&s, "claim");
    rebound.parent_feature_id = Some(id("feature"));
    rebound.work_revision = 2;
    rebound.revision = 3;
    update(&s, rebound.clone(), 2).unwrap();
    assert_eq!(
        get(&s, "claim").root_id,
        RootId(id("issue")),
        "original execution lineage is preserved"
    );
    h.set_now(T0 + 60_001);
    let mut expired = rebound.clone();
    expired.state = "Expired".into();
    expired.revision = 4;
    assert!(matches!(
        update(&s, expired, 3),
        Err(StorageError::InvalidRecord(_))
    ));
    let mut new = claim(&s, "another", "task-a", false);
    new.work_id = id("issue");
    new.work_revision = 2;
    new.fence = 2;
    new.expires_at_ms = T0 + 120_000;
    assert!(matches!(
        insert(&s, new.clone()),
        Err(StorageError::Duplicate { .. })
    ));
    drop(s);
    let s = h.store();
    assert_eq!(get(&s, "claim"), rebound);
    assert!(matches!(
        insert(&s, new),
        Err(StorageError::Duplicate { .. })
    ));
    activation.phase = ActivationPhase::Stopped;
    activation.outcome = Some(ActivationOutcome::Interrupted);
    activation.stopped_at_ms = Some(T0 + 60_001);
    activation.revision = 2;
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.runs().update_activation(&activation, 1)
    })
    .unwrap();
    let mut release = get(&s, "claim");
    release.state = "Released".into();
    release.revision = 4;
    update(&s, release, 3).unwrap();
}

#[test]
fn yielding_keeps_project_slot_until_stopped_then_allows_prerequisite_allocation() {
    let h = Harness::new();
    let s = seeded(&h);
    insert(&s, claim(&s, "claim", "feature", true)).unwrap();
    let mut activation = activate(&s, "feature");
    let mut transferred = get(&s, "claim");
    transferred.state = "Transferred".into();
    transferred.execution_id = Some(activation.id.clone());
    transferred.revision = 2;
    update(&s, transferred, 1).unwrap();
    let mut yielding = get(&s, "claim");
    yielding.state = "Yielding".into();
    yielding.revision = 3;
    update(&s, yielding, 2).unwrap();
    assert!(matches!(
        insert(&s, claim(&s, "prerequisite", "issue", false)),
        Err(StorageError::Duplicate { .. })
    ));
    let mut yielded = get(&s, "claim");
    yielded.state = "Yielded".into();
    yielded.revision = 4;
    assert!(matches!(
        update(&s, yielded.clone(), 3),
        Err(StorageError::InvalidRecord(_))
    ));
    activation.phase = ActivationPhase::Stopped;
    activation.outcome = Some(ActivationOutcome::Prevented);
    activation.stopped_at_ms = Some(T0);
    activation.revision = 2;
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.runs().update_activation(&activation, 1)
    })
    .unwrap();
    update(&s, yielded, 3).unwrap();
    insert(&s, claim(&s, "prerequisite", "issue", false)).unwrap();
    assert_eq!(
        get(&s, "claim").execution_id,
        Some(id("execution")),
        "yield retains coordinator lineage"
    );
}

#[test]
fn ownership_transfer_fences_the_previous_actor_and_immutable_request_identity() {
    let h = Harness::new();
    let s = seeded(&h);
    insert(&s, claim(&s, "claim", "issue", false)).unwrap();
    run_write(&s, WriteOptions::default(), |tx| {
        tx.actors().register(
            &devworks_core::protocol::ActorId(id("new-owner")),
            devworks_core::context::PrincipalKind::Host,
            "Other host",
        )
    })
    .unwrap();
    let mut changed = get(&s, "claim");
    changed.owner_id = devworks_core::protocol::ActorId(id("new-owner"));
    changed.revision = 2;
    assert!(matches!(
        update(&s, changed.clone(), 1),
        Err(StorageError::InvalidRecord(_))
    ));
    changed.fence = 2;
    update(&s, changed.clone(), 1).unwrap();
    assert_eq!(get(&s, "claim"), changed);
    changed.revision = 3;
    changed.invocation_id = id("different-request");
    assert!(matches!(
        update(&s, changed, 2),
        Err(StorageError::InvalidRecord(_))
    ));
}
