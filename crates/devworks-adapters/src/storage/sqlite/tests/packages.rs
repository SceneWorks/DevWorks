//! The package registry records of migration 20 on the real SQLite store: the registry head and
//! package records are compare-and-set, version identity is immutable, proposals record one
//! decision, and publishing a state generation fences the previous one while retaining its final
//! state. Migration 20 applies on a version-12 store.
use super::{Harness, through};
use crate::storage::sqlite::{MIGRATION_0020_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id};
use serde_json::json;

fn package(id: &str) -> StoredPackage {
    StoredPackage {
        package_id: id.into(),
        revision: 0,
        enabled: true,
        installed: true,
        active_digest: None,
        predecessor_digest: None,
        pending_digest: None,
        state_generation: 0,
        config: None,
        config_revision: 0,
        trust: None,
        failure: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    }
}
fn version(id: &str, digest: &str) -> StoredPackageVersion {
    StoredPackageVersion {
        package_id: id.into(),
        manifest_digest: digest.into(),
        bundle_digest: "b".repeat(64),
        version: "1.0.0".into(),
        publisher_id: "acme".into(),
        signer_key: Some("c".repeat(64)),
        inspection: json!({"packageId": id}),
        provenance: json!({"installedBy": "user-a"}),
        phase: VersionPhase::Staged,
        waits: vec!["Quiescence".into()],
        approval: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    }
}
fn generation(id: &str, g: i64, digest: &str) -> StoredStateGeneration {
    StoredStateGeneration {
        package_id: id.into(),
        generation: g,
        manifest_digest: digest.into(),
        schema_digest: "d".repeat(64),
        seeded_from: (g > 1).then_some(g - 1),
        seed_revision: 1,
        created_at_ms: 1,
    }
}

#[test]
fn registry_records_are_compare_and_set_and_versions_keep_their_identity() {
    let h = Harness::new();
    let s = h.store();
    let d = "a".repeat(64);
    run_write(&s, WriteOptions::default(), {
        let d = d.clone();
        move |tx| {
            assert_eq!(tx.packages().registry_revision()?, 1);
            assert_eq!(tx.packages().advance_registry(1)?, 2);
            assert!(matches!(
                tx.packages().advance_registry(1),
                Err(StorageError::RevisionConflict(_))
            ));
            let p = tx.packages().insert_package(&package("demo"))?;
            assert_eq!(p.revision, 1);
            assert!(matches!(
                tx.packages().insert_package(&package("demo")),
                Err(StorageError::Duplicate { .. })
            ));
            // A version refers to an existing package; its identity is unique.
            tx.packages().insert_version(&version("demo", &d))?;
            assert!(matches!(
                tx.packages().insert_version(&version("demo", &d)),
                Err(StorageError::Duplicate { .. })
            ));
            let mut p2 = p.clone();
            p2.active_digest = Some(d.clone());
            let p2 = tx.packages().update_package(&p2, 1)?;
            assert_eq!(p2.revision, 2);
            // A stale revision is a conflict naming the current one.
            match tx.packages().update_package(&p2, 1) {
                Err(StorageError::RevisionConflict(c)) => assert_eq!(c.current.get(), 2),
                other => panic!("{other:?}"),
            }
            // An uninstalled package can be neither enabled nor active.
            let mut bad = p2.clone();
            bad.installed = false;
            assert!(matches!(
                tx.packages().update_package(&bad, 2),
                Err(StorageError::InvalidRecord(_))
            ));
            tx.packages().update_version(
                "demo",
                &d,
                VersionPhase::Installed,
                &[],
                Some(&json!({"kind": "Person"})),
            )?;
            let v = tx.packages().package_version("demo", &d)?.unwrap();
            assert_eq!(
                (v.phase, v.waits.len(), v.version.as_str()),
                (VersionPhase::Installed, 0, "1.0.0")
            );
            assert!(matches!(
                tx.packages().update_version(
                    "demo",
                    &"f".repeat(64),
                    VersionPhase::Installed,
                    &[],
                    None
                ),
                Err(StorageError::NotFound { .. })
            ));
            Ok(())
        }
    })
    .unwrap();
}

#[test]
fn proposals_record_one_decision_and_generations_fence_and_retain_state() {
    let h = Harness::new();
    let s = h.store();
    let d = "a".repeat(64);
    run_write(&s, WriteOptions::default(), {
        let d = d.clone();
        move |tx| {
            tx.packages().insert_package(&package("demo"))?;
            tx.packages().insert_version(&version("demo", &d))?;
            let pr = StoredPackageProposal {
                id: Id::new("proposal-1").unwrap(),
                revision: 1,
                package_id: "demo".into(),
                manifest_digest: d.clone(),
                bundle_digest: "b".repeat(64),
                version: "1.0.0".into(),
                state: "Pending".into(),
                base_package_id: None,
                originator: ActorId(Id::new("agent-a").unwrap()),
                origin: json!({"kind": "Agent"}),
                summary: json!({"requestedAccess": []}),
                decided_by: None,
                decided_at_ms: None,
                decision: None,
                created_at_ms: 5,
            };
            tx.packages().insert_proposal(&pr)?;
            assert_eq!(tx.packages().proposal_for("demo", &d)?.unwrap().id, pr.id);
            let mut approved = pr.clone();
            approved.state = "Approved".into();
            approved.decided_by = Some(ActorId(Id::new("user-a").unwrap()));
            approved.decided_at_ms = Some(6);
            let a = tx.packages().update_proposal(&approved, 1)?;
            assert_eq!(a.revision, 2);
            assert!(matches!(
                tx.packages().update_proposal(&approved, 1),
                Err(StorageError::RevisionConflict(_))
            ));

            // Generation 1 with state, written by its handler; then generation 2.
            tx.packages().publish_generation(
                &generation("demo", 1, &d),
                Some(&json!({"n": 1})),
                1,
            )?;
            tx.packages()
                .compare_and_set_state("demo", 1, 1, json!({"n": 2}))?;
            tx.packages().publish_generation(
                &generation("demo", 2, &d),
                Some(&json!({"n": 2})),
                2,
            )?;
            // The retiring generation's final state is retained; the live state moved on.
            assert_eq!(
                tx.packages().generation_state("demo", 1)?.unwrap(),
                (2, json!({"n": 2}))
            );
            assert_eq!(
                tx.packages().generation_state("demo", 2)?.unwrap(),
                (2, json!({"n": 2}))
            );
            // A handler pinned to generation 1 is fenced.
            assert!(matches!(
                tx.packages().compare_and_set_state("demo", 1, 2, json!({})),
                Err(StorageError::LeaseLost { .. })
            ));
            // Generations only grow.
            assert!(matches!(
                tx.packages()
                    .publish_generation(&generation("demo", 2, &d), None, 1),
                Err(StorageError::LeaseLost { .. })
            ));
            assert_eq!(
                tx.packages()
                    .state_generation("demo", 2)?
                    .unwrap()
                    .seeded_from,
                Some(1)
            );
            Ok(())
        }
    })
    .unwrap();
}

#[test]
fn migration_20_adds_the_package_registry_to_a_version_19_store() {
    let m = MIGRATIONS
        .iter()
        .find(|m| m.version == 20)
        .expect("migration 20");
    assert_eq!(
        m.checksum(),
        MIGRATION_0020_SHA256,
        "an applied migration was edited"
    );
    assert_eq!((m.minimum_reader, m.minimum_writer), (8, 19));
    assert!(
        MIGRATIONS
            .iter()
            .filter(|e| e.version < 20)
            .all(|e| e.minimum_reader <= m.minimum_reader && e.minimum_writer <= m.minimum_writer)
    );
    let h = Harness::new();
    drop(SqliteStore::open_with(h.config(), &through(19)).unwrap());
    let s = h.store();
    let head = run_write(&s, WriteOptions::default(), |tx| {
        tx.packages().registry_revision()
    })
    .unwrap();
    assert_eq!(head, 1);
}

#[test]
fn a_packages_version_list_always_includes_its_newest_version() {
    let h = Harness::new();
    let s = h.store();
    run_write(&s, WriteOptions::default(), |tx| {
        tx.packages().insert_package(&package("many"))?;
        for n in 0..520i64 {
            let mut v = version("many", &format!("{n:064x}"));
            v.created_at_ms = n + 1;
            v.updated_at_ms = n + 1;
            tx.packages().insert_version(&v)?;
        }
        Ok(())
    })
    .unwrap();
    let all = run_write(&s, WriteOptions::default(), |tx| {
        tx.packages().package_versions("many")
    })
    .unwrap();
    assert_eq!(all.len(), 512);
    // The bound keeps the newest, oldest first: the last one is the newest version.
    assert_eq!(all.last().unwrap().manifest_digest, format!("{:064x}", 519));
    assert_eq!(all[0].manifest_digest, format!("{:064x}", 8));
}
