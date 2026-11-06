//! Exact composed graph storage on real SQLite; no providers or learned inference.
use super::{Harness, T0, through};
use crate::storage::sqlite::{AccessMode, MIGRATION_0031_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id};
use serde_json::json;
use std::sync::{Arc, Barrier};
fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn read<T>(s: &SqliteStore, f: impl FnOnce(&dyn ReadSnapshot) -> Result<T, StorageError>) -> T {
    read_snapshot(s, &ReadOptions::default(), f).unwrap()
}
fn write<T: Send + 'static>(
    s: &SqliteStore,
    f: impl FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
) -> T {
    run_write(s, WriteOptions::default(), f).unwrap()
}
fn repository(tx: &mut dyn WriteTransaction, repo: &str) -> Result<(), StorageError> {
    tx.projects().insert_repository(&StoredRepository {
        id: id(repo),
        location: format!("/src/{repo}"),
        name: repo.into(),
        remote_url: None,
        primary: true,
        lifecycle: ACTIVE.into(),
        languages: vec![],
        revision: 1,
        registered_at_ms: T0,
    })
}
fn generation(
    tx: &mut dyn WriteTransaction,
    name: &str,
    repo: &str,
    layer: &str,
) -> Result<(), StorageError> {
    tx.indexing().insert_generation(&StoredGeneration {
        id: id(name),
        repository_id: id(repo),
        layer: layer.into(),
        state: GEN_STAGING.into(),
        source_digest: "a".repeat(40),
        watermark: 0,
        manifest: json!({}),
    })
}
fn node(key: &str, name: &str, kind: &str) -> StoredNode {
    StoredNode {
        unit_key: key.into(),
        node_id: "shared".into(),
        kind: kind.into(),
        name: name.into(),
        payload: json!({"qualifiedName":name,"provenance":{"extractor":"fixture"}}),
    }
}
fn unit(g: &str, key: &str) -> StoredUnit {
    StoredUnit {
        generation_id: id(g),
        unit_key: key.into(),
        content_digest: sha256_hex(key.as_bytes()),
        action: UNIT_REPLACE.into(),
        payload: json!({"mode":"Structural"}),
    }
}
fn seal(tx: &mut dyn WriteTransaction, name: &str) -> Result<(), StorageError> {
    let mut g = tx.indexing().generation(&id(name))?.unwrap();
    g.state = GEN_SEALED.into();
    g.watermark = tx.indexing().units(&g.id, None, 500)?.len() as i64;
    tx.indexing().update_generation(&g)
}
fn view(
    tx: &mut dyn WriteTransaction,
    name: &str,
    repo: &str,
    base: &str,
    branch: Option<&str>,
) -> Result<GraphViewBinding, StorageError> {
    let v = StoredView {
        view_id: id(name),
        repository_id: id(repo),
        view_digest: sha256_hex(name.as_bytes()),
        baseline_id: id(base),
        branch_id: branch.map(id),
        worktree_id: None,
        coverage: if branch.is_some() {
            json!({"state":"Partial","head":"f".repeat(40)})
        } else {
            json!({"state":"Partial","commit":"a".repeat(40)})
        },
    };
    tx.indexing().insert_view(&v)?;
    if branch.is_none() {
        tx.indexing().publish(&id(repo), &v.view_id, 0)?;
    }
    Ok(GraphViewBinding {
        repository_id: v.repository_id,
        view_id: v.view_id,
        view_digest: v.view_digest,
    })
}
struct Fx {
    h: Harness,
    s: Arc<SqliteStore>,
    scope: GraphScope,
    baseline: GraphScope,
}
impl Fx {
    fn new() -> Self {
        let h = Harness::new();
        let s = Arc::new(h.store());
        let (scope, baseline) = write(&s, |tx| {
            repository(tx, "repo-a")?;
            repository(tx, "repo-b")?;
            generation(tx, "ga", "repo-a", LAYER_BASELINE)?;
            for (key, name, kind) in [
                ("a.rs", "old", "Function"),
                ("b.rs", "target", "Class"),
                ("gone.rs", "gone", "Class"),
                ("z.rs", "z", "Function"),
            ] {
                let edges = if key == "a.rs" {
                    vec![StoredEdge {
                        unit_key: key.into(),
                        edge_id: "masked-edge".into(),
                        source_node_id: "shared".into(),
                        target_node_id: "shared".into(),
                        kind: "CALLS".into(),
                        payload: json!({"targetUnit":"b.rs","resolution":"Resolved"}),
                    }]
                } else if key == "z.rs" {
                    vec![
                        StoredEdge {
                            unit_key: key.into(),
                            edge_id: "deleted-target".into(),
                            source_node_id: "shared".into(),
                            target_node_id: "shared".into(),
                            kind: "CALLS".into(),
                            payload: json!({"targetUnit":"gone.rs","resolution":"Resolved"}),
                        },
                        StoredEdge {
                            unit_key: key.into(),
                            edge_id: "cross-repo".into(),
                            source_node_id: "shared".into(),
                            target_node_id: "shared".into(),
                            kind: "CALLS".into(),
                            payload: json!({"targetRepositoryId":"repo-b","targetUnit":"a.rs","resolution":"Resolved"}),
                        },
                    ]
                } else {
                    vec![]
                };
                tx.indexing()
                    .put_unit(&unit("ga", key), &[node(key, name, kind)], &edges)?;
            }
            seal(tx, "ga")?;
            let baseline = view(tx, "va-base", "repo-a", "ga", None)?;
            generation(tx, "gb", "repo-a", LAYER_BRANCH)?;
            tx.indexing()
                .put_unit(&unit("gb", "a.rs"), &[node("a.rs", "new", "Function")], &[])?;
            let mut deleted = unit("gb", "gone.rs");
            deleted.action = UNIT_DELETE.into();
            tx.indexing().put_unit(&deleted, &[], &[])?;
            seal(tx, "gb")?;
            let a = view(tx, "va", "repo-a", "ga", Some("gb"))?;
            tx.indexing().put_overlay(
                &StoredOverlay {
                    id: id("overlay-a"),
                    repository_id: id("repo-a"),
                    worktree_id: None,
                    worktree_path: None,
                    state: OVERLAY_READY.into(),
                    revision: 1,
                    base_generation_id: Some(id("ga")),
                    base_oid: Some("a".repeat(40)),
                    head_oid: Some("f".repeat(40)),
                    head_ref: None,
                    capture_digest: None,
                    branch_generation_id: Some(id("gb")),
                    worktree_generation_id: None,
                    view_id: Some(id("va")),
                    run_id: None,
                    reason: None,
                    created_at_ms: T0,
                    updated_at_ms: T0,
                    expires_at_ms: T0 + 10_000,
                },
                None,
            )?;
            generation(tx, "foreign", "repo-b", LAYER_BASELINE)?;
            let mut foreign = (0..507)
                .map(|n| {
                    let mut row = node("a.rs", "foreign", "Foreign");
                    row.node_id = format!("node-{n:04}");
                    row
                })
                .collect::<Vec<_>>();
            foreign.push(node("a.rs", "foreign-same-id", "Foreign"));
            tx.indexing()
                .put_unit(&unit("foreign", "a.rs"), &foreign, &[])?;
            seal(tx, "foreign")?;
            let b = view(tx, "vb", "repo-b", "foreign", None)?;
            Ok((
                GraphScope { views: vec![a, b] },
                GraphScope {
                    views: vec![baseline],
                },
            ))
        });
        Self {
            h,
            s,
            scope,
            baseline,
        }
    }
}
fn key(repo: &str, unit: &str) -> GraphNodeIdentity {
    GraphNodeIdentity {
        repository_id: id(repo),
        unit_key: unit.into(),
        node_id: "shared".into(),
    }
}
fn scoped_a(f: &Fx) -> GraphScope {
    GraphScope {
        views: vec![f.scope.views[0].clone()],
    }
}

#[test]
fn graph31_masking_scope_composite_identity_and_pages_precede_caps() {
    let f = Fx::new();
    let a = scoped_a(&f);
    let counts = read(&f.s, |r| r.graph().graph_counts(&a));
    assert_eq!((counts.units, counts.nodes, counts.edges), (3, 3, 2));
    let kinds = read(&f.s, |r| {
        r.graph().graph_kinds(&a, GraphElementKind::Node, None, 1)
    });
    assert!(kinds.has_more);
    assert_eq!(kinds.items[0].kind, "Class");
    let after = kinds.items[0].kind.clone();
    assert!(
        !read(&f.s, |r| r.graph().graph_kinds(
            &a,
            GraphElementKind::Node,
            Some(&after),
            1
        ))
        .has_more
    );
    assert!(
        read(&f.s, |r| r.graph().graph_nodes(
            &a,
            &GraphNodeFilter {
                pattern: Some("%foreign%".into()),
                ..Default::default()
            },
            None,
            1
        ))
        .items
        .is_empty()
    );
    assert_eq!(
        read(&f.s, |r| r.graph().graph_node(&a, &key("repo-a", "a.rs")))
            .unwrap()
            .node
            .name,
        "new"
    );
    assert!(
        read(&f.s, |r| r
            .graph()
            .graph_node(&a, &key("repo-a", "gone.rs")))
        .is_none()
    );
    assert!(
        read_snapshot(&*f.s, &ReadOptions::default(), |r| r
            .graph()
            .graph_node(&a, &key("repo-b", "a.rs")))
        .is_err()
    );
    let edges = read(&f.s, |r| {
        r.graph()
            .graph_edges(&a, &GraphEdgeFilter::default(), None, 500)
    });
    assert!(!edges.has_more);
    assert_eq!(edges.items.len(), 2);
    assert!(edges.items.iter().all(|e| e.edge.edge_id != "masked-edge"));
    let cross = read(&f.s, |r| {
        r.graph().graph_edges(
            &f.scope,
            &GraphEdgeFilter {
                target_ids: vec![key("repo-b", "a.rs")],
                ..Default::default()
            },
            None,
            1,
        )
    });
    assert_eq!(cross.items.len(), 1);
    assert_eq!(cross.items[0].edge.edge_id, "cross-repo");
    let mut after = None;
    let mut all = vec![];
    loop {
        let p = read(&f.s, |r| {
            r.graph()
                .graph_nodes(&f.scope, &GraphNodeFilter::default(), after.as_ref(), 100)
        });
        assert!(p.items.len() <= 100);
        if let Some(n) = p.items.last() {
            after = Some(GraphPosition {
                repository_id: n.repository_id.clone(),
                unit_key: n.node.unit_key.clone(),
                item_id: n.node.node_id.clone(),
            });
        }
        all.extend(p.items);
        if !p.has_more {
            break;
        }
    }
    assert_eq!(all.len(), 511);
    let distinct = all
        .iter()
        .map(|n| {
            (
                n.repository_id.as_str(),
                n.node.unit_key.as_str(),
                n.node.node_id.as_str(),
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(distinct.len(), all.len());
    let mut wrong = a.clone();
    wrong.views[0].view_digest = "f".repeat(64);
    assert!(
        read_snapshot(&*f.s, &ReadOptions::default(), |r| r
            .graph()
            .graph_counts(&wrong))
        .is_err()
    );
    assert!(
        read_snapshot(&*f.s, &ReadOptions::default(), |r| r.graph().graph_nodes(
            &a,
            &GraphNodeFilter::default(),
            None,
            0
        ))
        .is_err()
    );
}

#[test]
fn graph31_trust_is_view_content_bound_revisioned_and_concurrent_cas_has_one_winner() {
    let f = Fx::new();
    let a = scoped_a(&f);
    let target = key("repo-a", "b.rs");
    let content = read(&f.s, |r| r.graph().graph_node(&a, &target))
        .unwrap()
        .content_digest;
    assert_eq!(read(&f.s, |r| r.graph().graph_trust_revision(&a)), 1);
    let record = StoredGraphTrust {
        identity: target.clone(),
        view_id: a.views[0].view_id.clone(),
        view_digest: a.views[0].view_digest.clone(),
        content_digest: content.clone(),
        revision: 2,
        trusted: false,
        updated_by: ActorId(id("host")),
        updated_at_ms: T0,
    };
    let row = record.clone();
    let scope = a.clone();
    write(&f.s, move |tx| tx.graph().put_graph_trust(&scope, &row, 1));
    assert!(read(&f.s, |r| r.graph().graph_trust_revision(&a)) > 1);
    assert_eq!(
        read(&f.s, |r| r.graph().graph_trust_revision(&f.baseline)),
        1
    );
    assert!(
        read(&f.s, |r| r.graph().graph_trust(
            &f.baseline,
            &target,
            &content
        ))
        .is_none()
    );
    assert_eq!(
        read(&f.s, |r| r.graph().graph_nodes(
            &a,
            &GraphNodeFilter {
                exclude_untrusted: true,
                ..Default::default()
            },
            None,
            500
        ))
        .items
        .len(),
        2
    );
    let mut bad = record.clone();
    bad.content_digest = "wrong".into();
    bad.revision = 3;
    let scope = a.clone();
    assert!(
        run_write(&*f.s, WriteOptions::default(), move |tx| tx
            .graph()
            .put_graph_trust(&scope, &bad, 2))
        .is_err()
    );
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = vec![];
    for trusted in [true, false] {
        let s = f.s.clone();
        let gate = barrier.clone();
        let scope = a.clone();
        let mut row = record.clone();
        row.revision = 3;
        row.trusted = trusted;
        workers.push(std::thread::spawn(move || {
            gate.wait();
            run_write(&*s, WriteOptions::default(), move |tx| {
                tx.graph().put_graph_trust(&scope, &row, 2)
            })
        }));
    }
    barrier.wait();
    assert_eq!(
        workers
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(Result::is_ok)
            .count(),
        1
    );
    assert_eq!(
        read(&f.s, |r| r.graph().graph_trust(&a, &target, &content))
            .unwrap()
            .revision,
        3
    );
}
fn pin(f: &Fx, name: &str) -> StoredGraphReadPin {
    StoredGraphReadPin {
        id: id(name),
        owner_namespace: "actual-owner".into(),
        store_id: f.s.info().store_id,
        restore_epoch: f.s.info().restore_epoch,
        scope_digest: "a".repeat(64),
        intent_digest: "b".repeat(64),
        scope: f.scope.clone(),
        view_witnesses: vec![
            GraphViewWitness {
                view_id: id("va"),
                source_revision: 1,
                overlay_id: Some(id("overlay-a")),
                expires_at_ms: Some(T0 + 10_000),
            },
            GraphViewWitness {
                view_id: id("vb"),
                source_revision: 1,
                overlay_id: None,
                expires_at_ms: None,
            },
        ],
        unavailable_repositories: vec![],
        revision: 1,
        created_at_ms: T0,
        expires_at_ms: T0 + 1000,
    }
}
#[test]
fn graph31_pin_renewal_ownership_expiry_reopen_and_gc_are_atomic() {
    let f = Fx::new();
    let p = pin(&f, "lease");
    let row = p.clone();
    write(&f.s, move |tx| tx.graph().acquire_graph_pin(&row));
    for v in &p.scope.views {
        assert_eq!(read(&f.s, |r| r.indexing().live_pins(&v.view_id, T0)), 1);
    }
    let held_view = p.scope.views[0].view_id.clone();
    assert!(
        run_write(&*f.s, WriteOptions::default(), move |tx| tx
            .indexing()
            .remove_view(&held_view))
        .is_err()
    );
    assert!(
        run_write(&*f.s, WriteOptions::default(), |tx| {
            let mut g = tx.indexing().generation(&id("gb"))?.unwrap();
            g.state = GEN_RETIRED.into();
            tx.indexing().update_generation(&g)
        })
        .is_err()
    );
    assert!(
        run_write(&*f.s, WriteOptions::default(), |tx| tx
            .graph()
            .renew_graph_pin(&id("lease"), "wrong-owner", 1, T0 + 2000))
        .is_err()
    );
    write(&f.s, |tx| {
        view(tx, "va-new", "repo-a", "ga", Some("gb"))?;
        let mut o = tx.indexing().overlay(&id("overlay-a"))?.unwrap();
        o.revision = 2;
        o.view_id = Some(id("va-new"));
        o.expires_at_ms = T0 + 20_000;
        tx.indexing().put_overlay(&o, Some(1))
    });
    assert!(
        read(&f.s, |r| r.graph().graph_retirable_views(T0, None, 10))
            .items
            .is_empty()
    );
    let mut old_new = pin(&f, "new-pin-for-old");
    old_new.created_at_ms = T0;
    assert!(
        run_write(&*f.s, WriteOptions::default(), move |tx| tx
            .graph()
            .acquire_graph_pin(&old_new))
        .is_err()
    );
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = vec![];
    for _ in 0..2 {
        let s = f.s.clone();
        let gate = barrier.clone();
        workers.push(std::thread::spawn(move || {
            gate.wait();
            run_write(&*s, WriteOptions::default(), |tx| {
                tx.graph()
                    .renew_graph_pin(&id("lease"), "actual-owner", 1, T0 + 2000)
            })
        }));
    }
    barrier.wait();
    assert_eq!(
        workers
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(Result::is_ok)
            .count(),
        1
    );
    let persisted = read(&f.s, |r| r.graph().graph_pin(&id("lease"))).unwrap();
    assert_eq!(persisted.revision, 2);
    assert_eq!(persisted.view_witnesses[0].source_revision, 1);
    let Fx { h, s, .. } = f;
    drop(s);
    let s = h.store();
    assert_eq!(
        read(&s, |r| r.graph().graph_pin(&id("lease"))),
        Some(persisted)
    );
    h.clock
        .0
        .store(T0 + 2000, std::sync::atomic::Ordering::SeqCst);
    assert!(
        run_write(&s, WriteOptions::default(), |tx| tx
            .graph()
            .renew_graph_pin(&id("lease"), "actual-owner", 2, T0 + 3000))
        .is_err()
    );
    let candidates = read(&s, |r| r.graph().graph_retirable_views(T0 + 2000, None, 1));
    assert_eq!(candidates.items.len(), 1);
    assert_eq!(candidates.items[0].view_id, id("va"));
    assert!(!candidates.has_more);
    write(&s, |tx| tx.indexing().remove_view(&id("va")));
    assert!(read(&s, |r| r.graph().graph_pin(&id("lease"))).is_none());
    assert_eq!(read(&s, |r| r.indexing().live_pins(&id("vb"), T0)), 0);
}
#[test]
fn graph31_source_is_private_immutable_shared_inherited_and_backup_retained() {
    let h = Harness::new();
    let s = h.store();
    let digest = sha256_hex(b"abc");
    let sha = digest.clone();
    write(&s, move |tx| {
        repository(tx, "repo")?;
        generation(tx, "source-a", "repo", LAYER_BASELINE)?;
        tx.artifacts()
            .put_blob(&sha, 3, "blob-gen", "artifacts/abc")?;
        for name in ["a.rs", "b.rs"] {
            let capture = StoredGraphSourceCapture {
                id: id(&format!("capture-{}", name.replace(".rs", ""))),
                repository_id: id("repo"),
                unit_key: name.into(),
                source_digest: "c".repeat(40),
                blob_digest: sha.clone(),
                blob_generation: "blob-gen".into(),
                byte_count: 3,
                created_at_ms: T0,
            };
            tx.graph().insert_graph_source_capture(&capture)?;
            let mut u = unit("source-a", name);
            u.content_digest = capture.source_digest;
            u.payload["sourceCapture"] = json!({"id":capture.id,"sha256":sha,"byteCount":"3"});
            tx.indexing()
                .put_unit(&u, &[node(name, name, "Function")], &[])?;
        }
        seal(tx, "source-a")?;
        generation(tx, "source-b", "repo", LAYER_BASELINE)?;
        tx.indexing().inherit_units(
            &id("source-a"),
            &id("source-b"),
            &["a.rs".into(), "b.rs".into()],
        )?;
        seal(tx, "source-b")?;
        Ok(())
    });
    assert_eq!(read(&s, |r| r.artifacts().live_references(&digest)), 2);
    assert_eq!(
        h.raw()
            .query_row("SELECT count(*) FROM artifacts", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        h.raw()
            .query_row("SELECT count(*) FROM graph_source_bindings", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        4
    );
    let manifest = write(&s, |tx| {
        devworks_core::services::artifacts::lifecycle::pin_for_backup(tx, &id("backup"))
    });
    assert_eq!(manifest.len(), 1);
    assert_eq!(manifest[0].digest, digest);
    assert!(read(&s, |r| r.artifacts().pinned(&digest)));
    let digest2 = digest.clone();
    assert!(
        run_write(&s, WriteOptions::default(), move |tx| {
            let mut row = tx.graph().graph_source_capture(&id("capture-a"))?.unwrap();
            row.blob_digest = digest2;
            row.byte_count = 4;
            tx.graph().insert_graph_source_capture(&row)
        })
        .is_err()
    );
    write(&s, |tx| {
        let mut g = tx.indexing().generation(&id("source-a"))?.unwrap();
        g.state = GEN_RETIRED.into();
        tx.indexing().update_generation(&g)?;
        tx.indexing().reclaim_generation(&g.id, 50_000).map(|_| ())
    });
    assert_eq!(read(&s, |r| r.artifacts().live_references(&digest)), 2);
    drop(s);
    let s = h.store();
    assert_eq!(
        read(&s, |r| r.graph().graph_source_capture(&id("capture-a")))
            .unwrap()
            .source_digest,
        "c".repeat(40)
    );
    write(&s, |tx| {
        let mut g = tx.indexing().generation(&id("source-b"))?.unwrap();
        g.state = GEN_RETIRED.into();
        tx.indexing().update_generation(&g)
    });
    loop {
        write(&s, |tx| {
            tx.indexing().reclaim_generation(&id("source-b"), 1)
        });
        let refs = read(&s, |r| r.artifacts().live_references(&digest));
        if refs == 1 {
            assert_eq!(
                read(&s, |r| r.artifacts().blob(&digest)).unwrap().state,
                BlobState::Present
            );
        }
        if refs == 0 {
            break;
        }
    }
    assert!(read(&s, |r| r.graph().graph_source_capture(&id("capture-a"))).is_none());
    assert_eq!(
        read(&s, |r| r.artifacts().blob(&digest)).unwrap().state,
        BlobState::UnlinkPending
    );
    assert!(read(&s, |r| r.artifacts().pinned(&digest)));
}
#[test]
fn graph31_capture_binding_fault_rolls_back_and_migration_is_additive() {
    let m = MIGRATIONS.iter().find(|m| m.version == 31).unwrap();
    assert_eq!(m.checksum(), MIGRATION_0031_SHA256);
    assert_eq!((m.minimum_reader, m.minimum_writer), (8, 31));
    assert!(!m.rebuilds_tables);
    let h = Harness::new();
    let s = h.store();
    write(&s, |tx| {
        repository(tx, "repo")?;
        generation(tx, "g", "repo", LAYER_BASELINE)
    });
    let result = run_write(&s, WriteOptions::default(), |tx| {
        let sha = sha256_hex(b"abc");
        tx.artifacts().put_blob(&sha, 3, "generation", "path")?;
        tx.graph()
            .insert_graph_source_capture(&StoredGraphSourceCapture {
                id: id("capture"),
                repository_id: id("repo"),
                unit_key: "a.rs".into(),
                source_digest: "x".into(),
                blob_digest: sha.clone(),
                blob_generation: "generation".into(),
                byte_count: 3,
                created_at_ms: T0,
            })?;
        let mut u = unit("g", "a.rs");
        u.content_digest = "wrong".into();
        u.payload["sourceCapture"] = json!({"id":"capture","sha256":sha,"byteCount":"3"});
        tx.indexing()
            .put_unit(&u, &[node("a.rs", "n", "Class")], &[])
    });
    assert!(result.is_err());
    assert!(read(&s, |r| r.graph().graph_source_capture(&id("capture"))).is_none());
    assert!(read(&s, |r| r.indexing().unit(&id("g"), "a.rs")).is_none());
    drop(s);
    let old = SqliteStore::open_with(h.config(), &through(30)).unwrap();
    assert!(matches!(
        old.compatibility().unwrap().access,
        AccessMode::ReadOnly { .. }
    ));
    drop(old);
    let s = h.store();
    assert!(s.integrity_check().unwrap().problems.is_empty());
}

#[test]
fn graph31_pin_binding_failures_write_nothing_and_broken_child_is_never_a_valid_lease() {
    let f = Fx::new();
    for mutation in 0..4 {
        let mut record = pin(&f, &format!("denied-{mutation}"));
        match mutation {
            0 => record.expires_at_ms = T0 + GRAPH_PIN_TTL_MS + 1,
            1 => record.view_witnesses[0].source_revision = 999,
            2 => record.view_witnesses.clear(),
            _ => record.scope.views[0].view_digest = "e".repeat(64),
        }
        assert!(
            run_write(&*f.s, WriteOptions::default(), move |tx| tx
                .graph()
                .acquire_graph_pin(&record))
            .is_err()
        );
    }
    assert_eq!(
        f.h.raw()
            .query_row("SELECT count(*) FROM graph_read_pins", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        f.h.raw()
            .query_row("SELECT count(*) FROM index_pins", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let record = pin(&f, "valid");
    write(&f.s, move |tx| tx.graph().acquire_graph_pin(&record));
    f.s.with_writer_connection(|conn|super::super::conn::db(conn.execute("UPDATE index_pins SET lease_until=lease_until+1 WHERE id=(SELECT index_pin_id FROM graph_read_pin_views WHERE pin_id='valid' LIMIT 1)",[])).map(|_|())).unwrap();
    assert!(
        read_snapshot(&*f.s, &ReadOptions::default(), |r| r
            .graph()
            .graph_pin(&id("valid")))
        .is_err()
    );
    write(&f.s, |tx| {
        tx.graph().invalidate_graph_pins_for_view(&id("va"))
    });
    assert_eq!(
        f.h.raw()
            .query_row("SELECT count(*) FROM graph_read_pins", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        f.h.raw()
            .query_row("SELECT count(*) FROM index_pins", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn graph31_directory_and_opaque_legacy_identity_can_be_annotated_without_fabricated_sha() {
    let h = Harness::new();
    let s = h.store();
    let scope = write(&s, |tx| {
        repository(tx, "directory-repo")?;
        generation(tx, "directory-generation", "directory-repo", LAYER_BASELINE)?;
        let mut u = unit("directory-generation", "/");
        u.content_digest = "actual-inventory-identity".into();
        tx.indexing()
            .put_unit(&u, &[node("/", "Root", "Folder")], &[])?;
        seal(tx, "directory-generation")?;
        Ok(GraphScope {
            views: vec![view(
                tx,
                "directory-view",
                "directory-repo",
                "directory-generation",
                None,
            )?],
        })
    });
    let target = key("directory-repo", "/");
    let current = read(&s, |r| r.graph().graph_node(&scope, &target)).unwrap();
    assert_eq!(current.content_digest, "actual-inventory-identity");
    let row = StoredGraphTrust {
        identity: target.clone(),
        view_id: scope.views[0].view_id.clone(),
        view_digest: scope.views[0].view_digest.clone(),
        content_digest: current.content_digest,
        revision: 2,
        trusted: false,
        updated_by: ActorId(id("host")),
        updated_at_ms: T0,
    };
    let scoped = scope.clone();
    write(&s, move |tx| tx.graph().put_graph_trust(&scoped, &row, 1));
    assert!(
        read(&s, |r| r.graph().graph_nodes(
            &scope,
            &GraphNodeFilter {
                exclude_untrusted: true,
                ..Default::default()
            },
            None,
            1
        ))
        .items
        .is_empty()
    );
}

fn endless_fixture() -> (Harness, SqliteStore, GraphScope) {
    let h = Harness::tuned(|c| {
        c.read_connections = 1;
        c.read_wait = std::time::Duration::from_secs(30);
    });
    let s = h.store();
    let scope = write(&s, |tx| {
        repository(tx, "repo")?;
        generation(tx, "g", "repo", LAYER_BASELINE)?;
        tx.indexing()
            .put_unit(&unit("g", "a.rs"), &[node("a.rs", "n", "Function")], &[])?;
        seal(tx, "g")?;
        Ok(GraphScope {
            views: vec![view(tx, "v", "repo", "g", None)?],
        })
    });
    // A controlled SQL fault fixture creates an actually unbounded statement behind the real
    // GraphRead port. It allocates no source corpus and exposes no raw SQL API to core callers.
    s.with_writer_connection(|conn|super::super::conn::db(conn.execute_batch("ALTER TABLE graph_nodes RENAME TO graph_nodes_fixture; CREATE VIEW graph_nodes AS WITH RECURSIVE forever(n) AS (VALUES(0) UNION ALL SELECT n+1 FROM forever) SELECT 'g' generation_id,'a.rs' unit_key,CAST(n AS TEXT) node_id,'Function' kind,'n' name,'{}' payload_json FROM forever;"))).unwrap();
    (h, s, scope)
}
fn reader_has_no_leftover_hook(s: &SqliteStore) {
    // Reuse the same one-entry pool WITHOUT installing a new read callback. A leaked expired
    // callback would interrupt this finite statement after its first VM progress interval.
    let lease = s.lease(None).unwrap();
    let conn = lease.conn.as_ref().unwrap();
    assert!(conn.is_autocommit());
    let sum:i64=conn.query_row("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<5000) SELECT sum(x) FROM n",[],|row|row.get(0)).unwrap();
    assert_eq!(sum, 12_502_500);
}
fn restore_endless(s: &SqliteStore) {
    s.with_writer_connection(|conn| {
        super::super::conn::db(conn.execute_batch(
            "DROP VIEW graph_nodes; ALTER TABLE graph_nodes_fixture RENAME TO graph_nodes;",
        ))
    })
    .unwrap();
}
#[test]
fn graph31_inflight_sql_cancel_deadline_and_panic_clear_hooks_before_pool_reuse() {
    for deadline in [false, true] {
        let (_h, s, scope) = endless_fixture();
        let cancel = Cancellation::new();
        let (other, rx) = std::sync::mpsc::sync_channel(0);
        let worker = if deadline {
            None
        } else {
            let token = cancel.clone();
            Some(std::thread::spawn(move || {
                rx.recv().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(20));
                token.cancel();
            }))
        };
        let options = ReadOptions {
            cancellation: Some(cancel),
            deadline_ms: deadline.then_some(T0 + 50),
        };
        let started = std::time::Instant::now();
        let result = read_snapshot(&s, &options, |r| {
            if !deadline {
                other.send(()).unwrap();
            }
            r.graph().graph_counts(&scope)
        });
        assert!(matches!(result, Err(StorageError::Cancelled)), "{result:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        if let Some(worker) = worker {
            worker.join().unwrap();
        }
        reader_has_no_leftover_hook(&s);
        restore_endless(&s);
        assert_eq!(read(&s, |r| r.graph().graph_counts(&scope)).nodes, 1);
        let token = Cancellation::new();
        let during = token.clone();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<(), _> = read_snapshot(
                &s,
                &ReadOptions {
                    cancellation: Some(token),
                    deadline_ms: None,
                },
                |_| {
                    during.cancel();
                    panic!("native read cleanup fixture")
                },
            );
        }));
        assert!(panic.is_err());
        reader_has_no_leftover_hook(&s);
        assert_eq!(read(&s, |r| r.graph().graph_counts(&scope)).nodes, 1);
    }
}
#[test]
fn graph31_read_deadline_also_bounds_waiting_for_the_only_reader() {
    let h = Harness::tuned(|c| {
        c.read_connections = 1;
        c.read_wait = std::time::Duration::from_secs(30);
    });
    let s = Arc::new(h.store());
    let next = s.clone();
    read(&s, move |_| {
        let waiter = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let result = read_snapshot(
                &*next,
                &ReadOptions {
                    cancellation: None,
                    deadline_ms: Some(T0 + 20),
                },
                |_| Ok(()),
            );
            assert!(matches!(result, Err(StorageError::Cancelled)));
            assert!(start.elapsed() < std::time::Duration::from_secs(2));
        });
        waiter.join().unwrap();
        Ok(())
    });
    assert!(read_snapshot(&*s, &ReadOptions::default(), |_| Ok(())).is_ok());
}

#[test]
fn graph31_unavailable_selection_stays_immutable_when_repository_becomes_indexed_and_reopens() {
    let h = Harness::new();
    let s = h.store();
    let scope = write(&s, |tx| {
        repository(tx, "a")?;
        repository(tx, "b")?;
        generation(tx, "a-generation", "a", LAYER_BASELINE)?;
        tx.indexing().put_unit(
            &unit("a-generation", "a.rs"),
            &[node("a.rs", "A", "Class")],
            &[],
        )?;
        seal(tx, "a-generation")?;
        Ok(GraphScope {
            views: vec![view(tx, "a-view", "a", "a-generation", None)?],
        })
    });
    let pin = StoredGraphReadPin {
        id: id("partial-selection"),
        owner_namespace: "owner".into(),
        store_id: s.info().store_id,
        restore_epoch: s.info().restore_epoch,
        scope_digest: "a".repeat(64),
        intent_digest: "b".repeat(64),
        scope: scope.clone(),
        view_witnesses: vec![GraphViewWitness {
            view_id: id("a-view"),
            source_revision: 1,
            overlay_id: None,
            expires_at_ms: None,
        }],
        unavailable_repositories: vec![GraphUnavailableRepository {
            repository_id: id("b"),
            reason: "NotIndexedAtSelection".into(),
        }],
        revision: 1,
        created_at_ms: T0,
        expires_at_ms: T0 + 1000,
    };
    let copy = pin.clone();
    write(&s, move |tx| tx.graph().acquire_graph_pin(&copy));
    write(&s, |tx| {
        generation(tx, "b-generation", "b", LAYER_BASELINE)?;
        tx.indexing().put_unit(
            &unit("b-generation", "b.rs"),
            &[node("b.rs", "B", "Class")],
            &[],
        )?;
        seal(tx, "b-generation")?;
        view(tx, "b-view", "b", "b-generation", None).map(|_| ())
    });
    let before = read(&s, |r| r.graph().graph_pin(&id("partial-selection"))).unwrap();
    assert_eq!(before, pin);
    assert_eq!(read(&s, |r| r.graph().graph_counts(&before.scope)).nodes, 1);
    assert_eq!(read(&s, |r| r.indexing().live_pins(&id("b-view"), T0)), 0);
    let renewed = write(&s, |tx| {
        tx.graph()
            .renew_graph_pin(&id("partial-selection"), "owner", 1, T0 + 2000)
    });
    assert_eq!(
        renewed.unavailable_repositories,
        pin.unavailable_repositories
    );
    assert_eq!(renewed.scope, pin.scope);
    let mut overlapping = pin.clone();
    overlapping.id = id("bad-partial");
    overlapping.unavailable_repositories[0].repository_id = id("a");
    assert!(
        run_write(&s, WriteOptions::default(), move |tx| tx
            .graph()
            .acquire_graph_pin(&overlapping))
        .is_err()
    );
    drop(s);
    let s = h.store();
    assert_eq!(
        read(&s, |r| r.graph().graph_pin(&id("partial-selection"))),
        Some(renewed)
    );
    h.clock
        .0
        .store(T0 + 2000, std::sync::atomic::Ordering::SeqCst);
    let expired = read(&s, |r| r.graph().graph_expired_pins(T0 + 2000, None, 1));
    assert!(!expired.has_more);
    assert_eq!(expired.items.len(), 1);
    let row = expired.items[0].clone();
    write(&s, move |tx| {
        tx.graph()
            .retire_graph_pin(&row.id, &row.owner_namespace, row.revision)
    });
    assert!(read(&s, |r| r.graph().graph_pin(&id("partial-selection"))).is_none());
    assert_eq!(read(&s, |r| r.indexing().live_pins(&id("a-view"), T0)), 0);
}
