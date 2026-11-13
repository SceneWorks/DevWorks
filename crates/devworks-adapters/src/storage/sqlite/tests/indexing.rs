//! Repository indexing records on the real SQLite store (DW-023): migration 19, and the
//! repository's invariants (compare-and-set revisions, one active run per target, staging-only
//! payload, idempotent batches, immutable sealed generations, the published pointer's
//! compare-and-set, bounded reclamation). The end-to-end service tests with real Git
//! repositories live in `tests/indexing.rs`.
use super::{Harness, T0, through};
use crate::storage::sqlite::{MIGRATION_0019_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id};
use serde_json::json;
use std::sync::Arc;

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn store(h: &Harness) -> Arc<dyn ApplicationStore> {
    Arc::new(h.store())
}
fn write<T: Send + 'static>(
    s: &Arc<dyn ApplicationStore>,
    f: impl FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
) -> Result<T, StorageError> {
    run_write(&**s, WriteOptions::default(), f)
}
fn registered(s: &Arc<dyn ApplicationStore>, repo: &str) {
    let r = StoredRepository {
        id: id(repo),
        location: format!("/src/{repo}"),
        name: repo.into(),
        remote_url: None,
        primary: true,
        lifecycle: ACTIVE.into(),
        languages: vec![],
        revision: 1,
        registered_at_ms: T0,
    };
    write(s, move |tx| tx.projects().insert_repository(&r)).unwrap();
}
fn run(id_: &str, repo: &str) -> StoredIndexRun {
    StoredIndexRun {
        id: id(id_),
        batch_id: id(id_),
        kind: RUN_INDEX.into(),
        repository_id: id(repo),
        project_id: None,
        overlay_id: None,
        state: RUN_QUEUED.into(),
        stage: "queued".into(),
        revision: 1,
        attempt: 0,
        max_attempts: 3,
        owner: None,
        lease_until_ms: None,
        cancel_requested_at_ms: None,
        config_revision: 1,
        config_digest: "a".repeat(64),
        source_oid: None,
        generation_id: None,
        completed_units: 0,
        total_units: None,
        output_view_id: None,
        error_code: None,
        error_message: None,
        deterministic: false,
        warnings: vec![],
        requested_by: ActorId(id("user")),
        created_at_ms: T0,
        started_at_ms: None,
        finished_at_ms: None,
        next_attempt_at_ms: T0,
    }
}
fn generation(id_: &str, repo: &str) -> StoredGeneration {
    StoredGeneration {
        id: id(id_),
        repository_id: id(repo),
        layer: LAYER_BASELINE.into(),
        state: GEN_STAGING.into(),
        source_digest: "b".repeat(40),
        watermark: 0,
        manifest: json!({"commit": "b".repeat(40)}),
    }
}
fn unit(g: &str, key: &str, digest: &str) -> StoredUnit {
    StoredUnit {
        generation_id: id(g),
        unit_key: key.into(),
        content_digest: digest.into(),
        action: UNIT_REPLACE.into(),
        payload: json!({"mode": "Structural"}),
    }
}
fn node(key: &str, n: &str) -> StoredNode {
    StoredNode {
        unit_key: key.into(),
        node_id: format!("n-{n}"),
        kind: "Function".into(),
        name: n.into(),
        payload: json!({"qualifiedName": n}),
    }
}

#[test]
fn migration_19_is_additive_and_keeps_floors_at_or_above_14() {
    let m = MIGRATIONS
        .iter()
        .find(|m| m.version == 19)
        .expect("migration 19");
    assert_eq!(
        m.checksum(),
        MIGRATION_0019_SHA256,
        "an applied migration was edited"
    );
    assert_eq!((m.minimum_reader, m.minimum_writer), (8, 19));
    assert!(m.minimum_writer >= 14);
    assert!(!m.rebuilds_tables);
    assert!(
        MIGRATIONS
            .iter()
            .filter(|e| e.version < m.version)
            .all(|e| e.minimum_reader <= m.minimum_reader && e.minimum_writer <= m.minimum_writer)
    );
    let h = Harness::new();
    let old = SqliteStore::open_with(h.config(), &through(14)).unwrap();
    // Graph rows and a registration written before migration 19 (positional migration 1 rows).
    old.with_writer_connection(|c| {
        super::super::conn::db(c.execute_batch(
            "INSERT INTO repositories VALUES('repo-old','/src/old',1);
             INSERT INTO index_generations VALUES('g-old','repo-old','Baseline','Sealed','x',1,'{}');
             INSERT INTO source_units VALUES('g-old','a.rs','d','Replace');
             INSERT INTO graph_edges VALUES('g-old','a.rs','e1','n1','n2','CALLS');",
        ))
    })
    .unwrap();
    drop(old);
    let s = SqliteStore::open_with(h.config(), &through(19)).unwrap();
    let report = s.compatibility().unwrap();
    assert_eq!(
        (
            report.store_schema_version,
            report.minimum_reader,
            report.minimum_writer
        ),
        (19, 8, 19)
    );
    let s: Arc<dyn ApplicationStore> = Arc::new(s);
    let (u, e) = read_snapshot(&*s, &ReadOptions::default(), |snap| {
        Ok((
            snap.indexing().unit(&id("g-old"), "a.rs")?,
            snap.indexing().edges(&id("g-old"), "a.rs")?,
        ))
    })
    .unwrap();
    assert_eq!(
        u.unwrap().payload,
        json!({}),
        "the defaulted payload column"
    );
    assert_eq!(e[0].payload, json!({}));
}

#[test]
fn runs_are_revisioned_one_active_per_target_and_final_once_finished() {
    let h = Harness::new();
    let s = store(&h);
    registered(&s, "repo-a");
    write(&s, |tx| tx.indexing().insert_run(&run("op-1", "repo-a"))).unwrap();
    // A second active run of the same baseline is refused.
    assert!(matches!(
        write(&s, |tx| tx.indexing().insert_run(&run("op-2", "repo-a"))),
        Err(StorageError::Duplicate { .. })
    ));
    let mut next = run("op-1", "repo-a");
    next.revision = 2;
    next.state = RUN_RUNNING.into();
    next.owner = Some("w".into());
    next.lease_until_ms = Some(T0 + 1);
    let n2 = next.clone();
    write(&s, move |tx| tx.indexing().update_run(&n2, 1)).unwrap();
    // A stale revision conflicts; an owner without a lease is invalid.
    let n3 = next.clone();
    assert!(matches!(
        write(&s, move |tx| tx.indexing().update_run(&n3, 1)),
        Err(StorageError::RevisionConflict(_))
    ));
    let mut bad = next.clone();
    bad.revision = 3;
    bad.lease_until_ms = None;
    assert!(write(&s, move |tx| tx.indexing().update_run(&bad, 2)).is_err());
    let mut done = next.clone();
    done.revision = 3;
    done.state = RUN_SUCCEEDED.into();
    done.owner = None;
    done.lease_until_ms = None;
    let d2 = done.clone();
    write(&s, move |tx| tx.indexing().update_run(&d2, 2)).unwrap();
    let mut again = done.clone();
    again.revision = 4;
    again.state = RUN_QUEUED.into();
    assert!(
        write(&s, move |tx| tx.indexing().update_run(&again, 3)).is_err(),
        "a finished run never changes again"
    );
    // Its target is free again; the claim listing sees the new queued run only.
    write(&s, |tx| tx.indexing().insert_run(&run("op-3", "repo-a"))).unwrap();
    let claimable = read_snapshot(&*s, &ReadOptions::default(), |snap| {
        snap.indexing().claimable_runs(T0, 10)
    })
    .unwrap();
    assert_eq!(
        claimable.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        vec!["op-3"]
    );
}

#[test]
fn generations_take_payload_only_while_staging_and_seal_immutably() {
    let h = Harness::new();
    let s = store(&h);
    registered(&s, "repo-a");
    write(&s, |tx| {
        tx.indexing()
            .insert_generation(&generation("g-1", "repo-a"))
    })
    .unwrap();
    let put = |s: &Arc<dyn ApplicationStore>, digest: &str, name: &str| {
        let (u, n) = (unit("g-1", "a.rs", digest), node("a.rs", name));
        write(s, move |tx| tx.indexing().put_unit(&u, &[n], &[]))
    };
    put(&s, "d1", "f").unwrap();
    // The same batch again is idempotent; different content under the same unit is a conflict.
    put(&s, "d1", "f").unwrap();
    assert!(matches!(
        put(&s, "d1", "g"),
        Err(StorageError::Duplicate { .. })
    ));
    // A seal checks the staged unit count.
    let mut sealed = generation("g-1", "repo-a");
    sealed.state = GEN_SEALED.into();
    sealed.watermark = 2;
    let wrong = sealed.clone();
    assert!(write(&s, move |tx| tx.indexing().update_generation(&wrong)).is_err());
    sealed.watermark = 1;
    let ok = sealed.clone();
    write(&s, move |tx| tx.indexing().update_generation(&ok)).unwrap();
    // Sealed content is immutable: no payload, no manifest change; it only retires.
    assert!(put(&s, "d2", "h").is_err());
    let mut changed = sealed.clone();
    changed.manifest = json!({"commit": "other"});
    assert!(write(&s, move |tx| tx.indexing().update_generation(&changed)).is_err());
    // A view of sealed layers publishes by compare-and-set only.
    let view = StoredView {
        view_id: id("v-1"),
        repository_id: id("repo-a"),
        view_digest: "c".repeat(64),
        baseline_id: id("g-1"),
        branch_id: None,
        worktree_id: None,
        coverage: json!({}),
    };
    let v = view.clone();
    write(&s, move |tx| tx.indexing().insert_view(&v)).unwrap();
    assert_eq!(
        write(&s, |tx| tx.indexing().publish(&id("repo-a"), &id("v-1"), 0)).unwrap(),
        1
    );
    assert!(matches!(
        write(&s, |tx| tx.indexing().publish(&id("repo-a"), &id("v-1"), 0)),
        Err(StorageError::RevisionConflict(_))
    ));
    // A view over a staging layer is refused.
    write(&s, |tx| {
        tx.indexing()
            .insert_generation(&generation("g-2", "repo-a"))
    })
    .unwrap();
    let mut over_staging = view.clone();
    over_staging.view_id = id("v-2");
    over_staging.view_digest = "d".repeat(64);
    over_staging.baseline_id = id("g-2");
    assert!(write(&s, move |tx| tx.indexing().insert_view(&over_staging)).is_err());
    // The published view is never removed; a referenced generation is never reclaimed.
    assert!(write(&s, |tx| tx.indexing().remove_view(&id("v-1"))).is_err());
    let mut retired = sealed.clone();
    retired.state = GEN_RETIRED.into();
    write(&s, move |tx| tx.indexing().update_generation(&retired)).unwrap();
    assert!(write(&s, |tx| tx.indexing().reclaim_generation(&id("g-1"), 10)).is_err());
    // An abandoned staging generation reclaims in bounded batches, then disappears.
    for i in 0..5 {
        let (u, n) = (
            unit("g-2", &format!("f{i}.rs"), "d"),
            node(&format!("f{i}.rs"), &format!("f{i}")),
        );
        write(&s, move |tx| tx.indexing().put_unit(&u, &[n], &[])).unwrap();
    }
    let first = write(&s, |tx| tx.indexing().reclaim_generation(&id("g-2"), 3)).unwrap();
    assert_eq!(first, 3);
    while write(&s, |tx| tx.indexing().reclaim_generation(&id("g-2"), 3)).unwrap() > 0 {}
    assert!(
        read_snapshot(&*s, &ReadOptions::default(), |snap| snap
            .indexing()
            .generation(&id("g-2")))
        .unwrap()
        .is_none()
    );
}

#[test]
fn source_configuration_and_identity_records_keep_their_invariants() {
    let h = Harness::new();
    let s = store(&h);
    registered(&s, "repo-a");
    let config = StoredSourceConfig {
        scope_key: WORKSPACE_SCOPE_KEY.into(),
        project_id: None,
        revision: 2,
        structural_languages: vec!["rust".into()],
        semantic_packages: vec![],
        exclude_patterns: vec!["**/gen/**".into()],
        max_file_bytes: 1000,
        updated_at_ms: T0,
    };
    let c = config.clone();
    write(&s, move |tx| tx.indexing().put_source_config(&c, 1)).unwrap();
    let stale = config.clone();
    assert!(matches!(
        write(&s, move |tx| tx.indexing().put_source_config(&stale, 1)),
        Err(StorageError::RevisionConflict(_))
    ));
    let read = read_snapshot(&*s, &ReadOptions::default(), |snap| {
        snap.indexing().source_config(WORKSPACE_SCOPE_KEY)
    })
    .unwrap()
    .unwrap();
    assert_eq!(read, config);
    let source = StoredRepositorySource {
        repository_id: id("repo-a"),
        section: WORKSPACE_SCOPE_KEY.into(),
        section_root: Some("/src".into()),
        location: "/src/repo-a".into(),
        state: SOURCE_PRESENT.into(),
        head_oid: Some("e".repeat(40)),
        head_ref: Some("refs/heads/main".into()),
        object_format: "Sha1".into(),
        remote_url: Some("https://user:token@github.com/acme/a.git".into()),
        default_ref: Some("refs/heads/develop".into()),
        revision: 1,
        observed_at_ms: T0,
    };
    let leaked = source.clone();
    assert!(
        write(&s, move |tx| tx.indexing().put_repository_source(&leaked)).is_err(),
        "a remote with credentials is never stored"
    );
    let mut bad_oid = source.clone();
    bad_oid.remote_url = None;
    bad_oid.head_oid = Some("XYZ".into());
    assert!(write(&s, move |tx| tx.indexing().put_repository_source(&bad_oid)).is_err());
    let mut ok = source.clone();
    ok.remote_url = Some("git@github.com:acme/a.git".into());
    let o = ok.clone();
    write(&s, move |tx| tx.indexing().put_repository_source(&o)).unwrap();
    let back = read_snapshot(&*s, &ReadOptions::default(), |snap| {
        snap.indexing().repository_source(&id("repo-a"))
    })
    .unwrap();
    assert_eq!(back, Some(ok));
}

#[test]
fn the_overlay_listing_bound_never_hides_a_live_overlay_behind_ended_records() {
    let h = Harness::new();
    let s = store(&h);
    registered(&s, "repo-a");
    let overlay = |n: u32, state: &str| StoredOverlay {
        id: id(&format!("overlay-{n:04}")),
        repository_id: id("repo-a"),
        worktree_id: None,
        worktree_path: None,
        state: state.into(),
        revision: 1,
        base_generation_id: None,
        base_oid: None,
        // Commits sort by number: the live overlay's commit sorts after every ended one.
        head_oid: Some(format!("{n:040x}")),
        head_ref: None,
        capture_digest: None,
        branch_generation_id: None,
        worktree_generation_id: None,
        view_id: None,
        run_id: None,
        reason: None,
        created_at_ms: T0,
        updated_at_ms: T0,
        expires_at_ms: T0,
    };
    let ended: Vec<StoredOverlay> = (0..300).map(|n| overlay(n, "Expired")).collect();
    let live = overlay(999, "Ready");
    let l = live.clone();
    write(&s, move |tx| {
        for o in ended.iter().chain([&l]) {
            tx.indexing().put_overlay(o, None)?;
        }
        Ok(())
    })
    .unwrap();
    let listed = read_snapshot(&*s, &ReadOptions::default(), |snap| {
        snap.indexing().overlays(&id("repo-a"))
    })
    .unwrap();
    assert_eq!(listed.len(), 256);
    assert_eq!(listed[0], live, "the live overlay is listed first");
}

#[test]
fn equal_timestamp_publications_follow_their_durable_revision_not_opaque_ids() {
    let h = Harness::new();
    let s = store(&h);
    registered(&s, "repo-a");
    registered(&s, "legacy");
    write(&s, |tx| {
        let mut g = generation("generation", "repo-a");
        tx.indexing().insert_generation(&g)?;
        g.state = GEN_SEALED.into();
        tx.indexing().update_generation(&g)?;
        for (revision, name) in [(1, "view-9"), (2, "view-10"), (3, "view-11")] {
            let view = StoredView {
                view_id: id(name),
                repository_id: id("repo-a"),
                view_digest: devworks_core::fingerprint::sha256_hex(name.as_bytes()),
                baseline_id: g.id.clone(),
                branch_id: None,
                worktree_id: None,
                coverage: json!({"sourceRevision":revision}),
            };
            tx.indexing().insert_view(&view)?;
            assert_eq!(
                tx.indexing()
                    .publish(&id("repo-a"), &view.view_id, revision - 1)?,
                revision
            );
            tx.projects().publish_index_view(&StoredIndexView {
                view_id: view.view_id,
                repository_id: id("repo-a"),
                source_revision: "commit".into(),
                coverage: "Complete".into(),
                published_at_ms: T0,
            })?;
        }
        for (name, at) in [("legacy-old", T0 - 1), ("legacy-a", T0), ("legacy-z", T0)] {
            tx.projects().publish_index_view(&StoredIndexView {
                view_id: id(name),
                repository_id: id("legacy"),
                source_revision: "legacy".into(),
                coverage: "Complete".into(),
                published_at_ms: at,
            })?;
        }
        Ok(())
    })
    .unwrap();
    drop(s);
    let s = store(&h);
    read_snapshot(&*s, &ReadOptions::default(), |read| {
        let names = |repo: &str| -> Result<Vec<String>, StorageError> {
            Ok(read
                .projects()
                .index_views(&id(repo))?
                .into_iter()
                .map(|v| v.view_id.as_str().to_owned())
                .collect())
        };
        assert_eq!(names("repo-a")?, ["view-11", "view-10", "view-9"]);
        assert_eq!(names("legacy")?, ["legacy-z", "legacy-a", "legacy-old"]);
        assert_eq!(read.indexing().live_pins(&id("view-9"), T0)?, 0);
        Ok(())
    })
    .unwrap();
}
