//! Real SQLite convention history/scope/index transactions. Vectors are recorded fixtures only.
use super::{Harness, T0, through};
use crate::storage::sqlite::{MIGRATION_0032_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id, OperationId, ProjectId};
use serde_json::{Value, json};
fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn read<T>(s: &SqliteStore, f: impl FnOnce(&dyn ReadSnapshot) -> Result<T, StorageError>) -> T {
    read_snapshot(s, &ReadOptions::default(), f).unwrap()
}
fn project(tx: &mut dyn WriteTransaction, name: &str) -> Result<KnowledgeScope, StorageError> {
    let p = ProjectId(id(name));
    tx.projects().insert_project(&StoredProject {
        id: p.clone(),
        name: name.into(),
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
    Ok(KnowledgeScope::Project(p))
}
fn artifact(
    tx: &mut dyn WriteTransaction,
    name: &str,
    scope: &KnowledgeScope,
    text: &str,
) -> Result<Value, StorageError> {
    artifact_with_provenance(tx, name, scope, text, json!({}))
}
fn artifact_with_provenance(
    tx: &mut dyn WriteTransaction,
    name: &str,
    scope: &KnowledgeScope,
    text: &str,
    provenance: Value,
) -> Result<Value, StorageError> {
    let digest = sha256_hex(text.as_bytes());
    let blob = match tx.artifacts().blob(&digest)? {
        Some(b) => b,
        None => tx.artifacts().put_blob(
            &digest,
            text.len() as i64,
            &format!("blob-{name}"),
            &format!("blobs/{name}"),
        )?,
    };
    let actor = tx.store().host_actor.clone();
    tx.artifacts().insert_artifact(&StoredArtifact {
        id: id(name),
        origin_project_id: scope.project().cloned(),
        origin_work_id: None,
        title: name.into(),
        revision: 1,
        scope: scope
            .project()
            .cloned()
            .map_or(ArtifactScope::Workspace, ArtifactScope::Project),
        owner: Some(actor),
        retention: ArtifactRetention::Explicit,
        scope_revision: 1,
        reference_revision: 1,
        current_version_id: None,
        state: ArtifactState::Live,
        created_at_ms: T0,
        provenance,
    })?;
    let version = id(&format!("version-{name}"));
    tx.artifacts().insert_version(&StoredVersion {
        id: version.clone(),
        artifact_id: id(name),
        version_no: 1,
        blob_digest: digest.clone(),
        byte_count: text.len() as i64,
        blob_generation: blob.generation,
        media_type: "text/markdown".into(),
        created_at_ms: T0,
        deleted_at_ms: None,
        source: json!({}),
    })?;
    Ok(
        json!({"artifactId":name,"versionId":version,"sha256":digest,"byteLength":text.len().to_string(),"mediaType":"text/markdown"}),
    )
}
fn record(name: &str, scope: KnowledgeScope, reference: Option<Value>) -> StoredConvention {
    StoredConvention {
        id: id(name),
        scope,
        revision: 1,
        slug: name.into(),
        title: format!("Title {name}"),
        parent_id: None,
        sort_order: 0,
        lifecycle: "Active".into(),
        content_ref: reference,
        raw_content_ref: None,
        applicability: vec!["Rust".into()],
        provenance: json!({"kind":"Authored","actorId":"host"}),
        author_id: ActorId(id("host")),
        created_at_ms: T0,
        updated_at_ms: T0,
        legacy_approved: false,
    }
}
fn query(scopes: Vec<KnowledgeScope>) -> ConventionQuery {
    ConventionQuery {
        scopes,
        ..Default::default()
    }
}
fn next(r: &StoredConvention) -> StoredConvention {
    StoredConvention {
        revision: r.revision + 1,
        updated_at_ms: r.updated_at_ms + 1,
        ..r.clone()
    }
}
fn index(
    tx: &mut dyn WriteTransaction,
    row: &StoredConvention,
    name: &str,
    space: Option<&str>,
) -> Result<StoredConventionIndex, StorageError> {
    let op = OperationId(id(&format!("operation-{name}")));
    tx.operations().record_intent(NewOperation {
        id: op.clone(),
        root_id: None,
        assignment_id: None,
        capability: "conventions.index".into(),
        schema_version: 3,
        args_digest: "a".repeat(64),
        payload: json!({"conventionId":row.id}),
    })?;
    Ok(StoredConventionIndex {
        generation_id: id(name),
        convention_id: row.id.clone(),
        convention_revision: row.revision,
        resource_id: None,
        resource_revision: None,
        operation_id: op,
        state: "Draft".into(),
        status: "Queued".into(),
        error: None,
        embedding_space: space.map(str::to_owned),
        source_digest: row.content_ref.as_ref().unwrap()["sha256"]
            .as_str()
            .unwrap()
            .into(),
        created_at_ms: T0,
        updated_at_ms: T0,
    })
}
fn chunk(
    index: &StoredConventionIndex,
    number: i64,
    word: &str,
    frequency: u32,
    vector: Option<Vec<f32>>,
) -> StoredConventionChunk {
    StoredConventionChunk {
        generation_id: index.generation_id.clone(),
        id: id(&format!("chunk-{}-{number}", index.generation_id.as_str())),
        convention_id: index.convention_id.clone(),
        convention_revision: index.convention_revision,
        resource_id: index.resource_id.clone(),
        resource_revision: index.resource_revision,
        index: number,
        section_path: "Section".into(),
        section_path_ref: None,
        text: std::iter::repeat_n(word, frequency as usize)
            .collect::<Vec<_>>()
            .join(" "),
        embedding_text: std::iter::repeat_n(word, frequency as usize)
            .collect::<Vec<_>>()
            .join(" "),
        token_count: i64::from(frequency),
        terms: vec![ConventionTerm {
            term: word.into(),
            frequency,
        }],
        vector,
    }
}
#[test]
fn conventions32_migration_snapshot_cas_scoped_paging_and_reopen() {
    let migration = MIGRATIONS.iter().find(|m| m.version == 32).unwrap();
    assert_eq!(migration.checksum(), MIGRATION_0032_SHA256);
    assert_eq!(
        (migration.minimum_reader, migration.minimum_writer),
        (8, 32)
    );
    let h = Harness::new();
    let old = SqliteStore::open_with(h.config(), &through(31)).unwrap();
    drop(old);
    let store = SqliteStore::open(h.config()).unwrap();
    let (scope, first) = run_write(&store, WriteOptions::default(), |tx| {
        let a = project(tx, "project-a")?;
        let b = project(tx, "project-b")?;
        tx.conventions()
            .create_convention(&record("a-hidden", b, None))?;
        let first = record("z-visible", a.clone(), None);
        tx.conventions().create_convention(&first)?;
        tx.conventions().bump_convention_scope(&a)?;
        Ok((a, first))
    })
    .unwrap();
    assert_eq!(
        read(&store, |r| r.conventions().conventions(
            &query(vec![scope.clone()]),
            None,
            1
        ))[0],
        first
    );
    let revision = next(&first);
    let copy = revision.clone();
    run_write(&store, WriteOptions::default(), move |tx| {
        tx.conventions().revise_convention(&copy, 1)
    })
    .unwrap();
    let bad = next(&revision);
    assert!(
        run_write(&store, WriteOptions::default(), move |tx| tx
            .conventions()
            .revise_convention(&bad, 1))
        .is_err()
    );
    assert_eq!(
        read(&store, |r| r.conventions().convention_versions(
            &first.id,
            std::slice::from_ref(&scope),
            None,
            1
        ))[0],
        revision
    );
    assert_eq!(
        read(&store, |r| r.conventions().convention_versions(
            &first.id,
            std::slice::from_ref(&scope),
            Some(2),
            1
        ))[0],
        first
    );
    assert!(
        read(&store, |r| r.conventions().conventions(
            &query(vec![scope.clone()]),
            Some(&first.position()),
            1
        ))
        .is_empty()
    );
    assert!(
        read_snapshot(&store, &ReadOptions::default(), |r| r
            .conventions()
            .conventions(&query(vec![scope.clone()]), None, 0))
        .is_err()
    );
    assert_eq!(
        read(&store, |r| r
            .conventions()
            .convention_scope_revision(&scope)),
        2
    );
    drop(store);
    let store = SqliteStore::open(h.config()).unwrap();
    assert_eq!(
        read(&store, |r| r.conventions().convention(
            &first.id,
            None,
            std::slice::from_ref(&scope)
        )),
        Some(revision)
    );
    let raw = rusqlite::Connection::open(h.config().data_root.join("workspace.sqlite")).unwrap();
    assert!(
        raw.execute("UPDATE convention_versions SET title='tamper'", [])
            .is_err()
    );
    assert!(raw.execute("DELETE FROM convention_versions", []).is_err());
}
#[test]
fn convention_resource_history_is_immutable_and_move_never_leaks_old_scope() {
    let h = Harness::new();
    let store = h.store();
    let (a, b, owner, resource) = run_write(&store, WriteOptions::default(), |tx| {
        let a = project(tx, "a")?;
        let b = project(tx, "b")?;
        let content = artifact(tx, "body-a", &a, "private a body")?;
        let owner = record("rules", a.clone(), Some(content));
        tx.conventions().create_convention(&owner)?;
        let resource = StoredConventionResource {
            id: id("attachment"),
            convention_id: owner.id.clone(),
            revision: 1,
            convention_revision: 1,
            name: "rules.md".into(),
            media_type: "text/markdown".into(),
            content_ref: artifact(tx, "attachment-a", &a, "private attachment")?,
            author_id: tx.store().host_actor.clone(),
            created_at_ms: T0,
            deleted_at_ms: None,
            lifecycle: "Active".into(),
        };
        tx.conventions().add_convention_resource(&resource, 1)?;
        Ok((a, b, owner, resource))
    })
    .unwrap();
    let copy = resource.clone();
    assert!(
        run_write(&store, WriteOptions::default(), move |tx| {
            let mut r = copy;
            r.id = id("late-attachment");
            tx.conventions().add_convention_resource(&r, 1)
        })
        .is_err(),
        "a later transaction cannot append associations into immutable owner revision1"
    );
    let (moved, updated) = run_write(&store, WriteOptions::default(), {
        let owner = owner.clone();
        let resource = resource.clone();
        let b = b.clone();
        move |tx| {
            let mut moved = next(&owner);
            moved.scope = b.clone();
            moved.content_ref = Some(artifact(tx, "body-b", &b, "new scope b body")?);
            tx.conventions().revise_convention(&moved, 1)?;
            let mut r = resource;
            r.revision = 2;
            r.convention_revision = 2;
            r.content_ref = artifact(tx, "attachment-b", &b, "new scope attachment")?;
            tx.conventions().revise_convention_resource(&r, 1, 2)?;
            Ok((moved, r))
        }
    })
    .unwrap();
    assert!(
        read(&store, |r| r.conventions().convention(
            &owner.id,
            Some(1),
            std::slice::from_ref(&b)
        ))
        .is_none()
    );
    assert!(
        read(&store, |r| r.conventions().convention(
            &owner.id,
            Some(1),
            std::slice::from_ref(&a)
        ))
        .is_none()
    );
    let rq = ConventionResourceQuery {
        scopes: vec![b.clone()],
        convention_id: Some(owner.id.clone()),
        ..Default::default()
    };
    assert_eq!(
        read(&store, |r| r
            .conventions()
            .convention_resources(&rq, None, 500)),
        vec![updated]
    );
    let history = ConventionResourceQuery {
        scopes: vec![a.clone(), b.clone()],
        convention_id: Some(owner.id.clone()),
        convention_revision: Some(1),
        ..Default::default()
    };
    assert_eq!(
        read(&store, |r| r
            .conventions()
            .convention_resources(&history, None, 500)),
        vec![resource]
    );
    let mut denied = history.clone();
    denied.scopes = vec![b];
    assert!(
        read(&store, |r| r
            .conventions()
            .convention_resources(&denied, None, 500))
        .is_empty()
    );
    assert_eq!(
        read(&store, |r| r.conventions().convention(
            &moved.id,
            None,
            &history.scopes
        )),
        Some(moved)
    );
}
#[test]
fn convention_hierarchy_slug_and_cas_races_refuse_atomically() {
    let h = Harness::new();
    let store = std::sync::Arc::new(h.store());
    let first = record("parent", KnowledgeScope::Workspace, None);
    let child = StoredConvention {
        parent_id: Some(first.id.clone()),
        ..record("child", KnowledgeScope::Workspace, None)
    };
    run_write(&*store, WriteOptions::default(), {
        let first = first.clone();
        let child = child.clone();
        move |tx| {
            tx.conventions().create_convention(&first)?;
            tx.conventions().create_convention(&child)
        }
    })
    .unwrap();
    let mut cycle = next(&first);
    cycle.parent_id = Some(child.id);
    assert!(
        run_write(&*store, WriteOptions::default(), move |tx| tx
            .conventions()
            .revise_convention(&cycle, 1))
        .is_err()
    );
    let collision = StoredConvention {
        id: id("other"),
        slug: first.slug.clone(),
        ..record("other", KnowledgeScope::Workspace, None)
    };
    assert!(
        run_write(&*store, WriteOptions::default(), move |tx| tx
            .conventions()
            .create_convention(&collision))
        .is_err()
    );
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let joins = (0..2)
        .map(|n| {
            let store = store.clone();
            let barrier = barrier.clone();
            let mut row = next(&first);
            row.title = format!("winner-{n}");
            std::thread::spawn(move || {
                barrier.wait();
                run_write(&*store, WriteOptions::default(), move |tx| {
                    tx.conventions().revise_convention(&row, 1)
                })
                .is_ok()
            })
        })
        .collect::<Vec<_>>();
    let winners = joins
        .into_iter()
        .map(|j| j.join().unwrap())
        .filter(|v| *v)
        .count();
    assert_eq!(winners, 1);
    assert_eq!(
        read(&store, |r| r.conventions().convention_versions(
            &first.id,
            &[KnowledgeScope::Workspace],
            None,
            500
        ))
        .len(),
        2
    );
}
#[path = "conventions/lifecycle.rs"]
mod lifecycle;
#[path = "conventions/index.rs"]
mod retrieval;

#[test]
fn source_metadata_limits_nul_and_organization_fuzzy_filters_precede_sql_paging() {
    let h = Harness::new();
    let store = h.store();
    run_write(&store, WriteOptions::default(), |tx| {
        for (key, title, sort) in [("x", "B", 0), ("y", "Z", -1), ("z", "A", 0)] {
            let mut row = record(key, KnowledgeScope::Workspace, None);
            row.title = title.into();
            row.sort_order = sort;
            tx.conventions().create_convention(&row)?;
        }
        let foreign = project(tx, "foreign")?;
        let mut hidden = record("a-foreign", foreign, None);
        hidden.title = "A".into();
        tx.conventions().create_convention(&hidden)?;
        let mut full = record("bounds", KnowledgeScope::Workspace, None);
        full.title = format!("{}\0", "λ".repeat(499));
        full.slug = format!("{}\0", "s".repeat(199));
        tx.conventions().create_convention(&full)?;
        let mut invalid = record("invalid-title", KnowledgeScope::Workspace, None);
        invalid.title = "λ".repeat(501);
        assert!(tx.conventions().create_convention(&invalid).is_err());
        invalid.title = "okay".into();
        invalid.slug = "s".repeat(201);
        assert!(tx.conventions().create_convention(&invalid).is_err());
        Ok(())
    })
    .unwrap();
    let mut q = query(vec![KnowledgeScope::Workspace]);
    q.order = ConventionOrder::Title;
    let first = read(&store, |r| r.conventions().conventions(&q, None, 1)).remove(0);
    assert_eq!(first.id, id("z"));
    let second = read(&store, |r| {
        r.conventions().conventions(&q, Some(&first.position()), 1)
    })
    .remove(0);
    assert_eq!(second.id, id("x"));
    q.order = ConventionOrder::Organization;
    let first = read(&store, |r| r.conventions().conventions(&q, None, 1)).remove(0);
    assert_eq!(first.id, id("y"));
    let second = read(&store, |r| {
        r.conventions().conventions(&q, Some(&first.position()), 1)
    })
    .remove(0);
    assert_eq!(second.id, id("z"));
    q.name_pattern = Some("%A%".into());
    let results = read(&store, |r| r.conventions().conventions(&q, None, 2));
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].id,
        id("z"),
        "fuzzy titleORslug never materializes hidden scope candidates"
    );
    q.name_pattern = Some(format!("{}_%", "s".repeat(199)));
    let result = read(&store, |r| r.conventions().conventions(&q, None, 2));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].slug.chars().count(), 200);
    q.name_pattern = Some(format!("%{}\0%", "λ".repeat(499)));
    let result = read(&store, |r| r.conventions().conventions(&q, None, 2));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].title.chars().count(), 500);
    q.name_pattern = Some("not-a-match_%".into());
    assert!(read(&store, |r| r.conventions().conventions(&q, None, 2)).is_empty());
}
