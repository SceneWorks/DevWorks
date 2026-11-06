//! Actual SQLite Help publication, scope-free offline queries and bounded recovery. No inference.
use super::{Harness, T0, through};
use crate::storage::sqlite::{MIGRATION_0030_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::storage::*;
use devworks_core::protocol::{Id, OperationId, OperationState};
use serde_json::json;
use std::sync::Arc;

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn read<T>(s: &SqliteStore, f: impl FnOnce(&dyn ReadSnapshot) -> Result<T, StorageError>) -> T {
    read_snapshot(s, &ReadOptions::default(), f).unwrap()
}
fn claim(s: &SqliteStore, name: &str, revision: i64, registry: i64) -> OperationId {
    let op = OperationId(id(name));
    let saved = op.clone();
    let generation = format!("generation-{name}");
    run_write(s, WriteOptions::default(), move |tx| {
        tx.operations().record_intent(NewOperation {
            id: saved.clone(),
            root_id: None,
            assignment_id: None,
            capability: devworks_core::registry::find("help.generate_capability_docs")
                .unwrap()
                .name
                .into(),
            schema_version: 3,
            args_digest: "a".repeat(64),
            payload: json!({"generationId":generation}),
        })?;
        tx.help()
            .set_help_generation_operation(revision, registry, &saved)?;
        Ok(())
    })
    .unwrap();
    op
}
fn document(
    tx: &mut dyn WriteTransaction,
    generation: &str,
    name: &str,
    body: &str,
) -> Result<StoredHelpDocument, StorageError> {
    let artifact = id(&format!("artifact-{generation}-{name}"));
    let version = id(&format!("version-{generation}-{name}"));
    let digest = sha256_hex(body.as_bytes());
    let blob = match tx.artifacts().blob(&digest)? {
        Some(b) => b,
        None => tx.artifacts().put_blob(
            &digest,
            body.len() as i64,
            &format!("blob-{generation}-{name}"),
            &format!("blobs/{generation}-{name}"),
        )?,
    };
    let owner = tx.store().host_actor.clone();
    tx.artifacts().insert_artifact(&StoredArtifact {
        id: artifact.clone(),
        origin_project_id: None,
        origin_work_id: None,
        title: name.into(),
        revision: 1,
        scope: ArtifactScope::Workspace,
        owner: Some(owner),
        retention: ArtifactRetention::Explicit,
        scope_revision: 1,
        reference_revision: 1,
        current_version_id: None,
        state: ArtifactState::Live,
        created_at_ms: T0,
        provenance: json!({"generationId":generation}),
    })?;
    tx.artifacts().insert_version(&StoredVersion {
        id: version.clone(),
        artifact_id: artifact.clone(),
        version_no: 1,
        blob_digest: digest.clone(),
        byte_count: body.len() as i64,
        blob_generation: blob.generation,
        media_type: "text/markdown".into(),
        created_at_ms: T0,
        deleted_at_ms: None,
        source: json!({}),
    })?;
    Ok(StoredHelpDocument {
        topic: StoredHelpTopic {
            generation_id: id(generation),
            id: id(name),
            version: "1.0.0".into(),
            title: name.into(),
            content_digest: digest.clone(),
            content_ref: Some(
                json!({"artifactId":artifact.as_str(),"versionId":version.as_str(),"sha256":digest,"byteLength":body.len().to_string(),"mediaType":"text/markdown"}),
            ),
            availability: "Available".into(),
            reason: None,
            provenance: json!({"kind":"Bundled","appVersion":"1.0.0","contentDigest":digest}),
            related_operations: vec!["help.read".into()],
            screen_ids: vec!["help".into()],
            source_kind: "Bundled".into(),
            package_id: None,
            manifest_digest: None,
            character_count: body.chars().count() as i64,
            topic_revision: 1,
        },
        body: body.into(),
        search_text: format!("{name}\n{body}").to_lowercase(),
    })
}
fn missing(generation: &str, name: &str) -> StoredHelpDocument {
    StoredHelpDocument {
        topic: StoredHelpTopic {
            generation_id: id(generation),
            id: id(name),
            version: "1.0.0".into(),
            title: name.into(),
            content_digest: String::new(),
            content_ref: None,
            availability: "Missing".into(),
            reason: Some("Verified installed guide is absent".into()),
            provenance: serde_json::Value::Null,
            related_operations: vec![],
            screen_ids: vec!["packages".into()],
            source_kind: "Package".into(),
            package_id: Some(id("package-a")),
            manifest_digest: Some("f".repeat(64)),
            character_count: 0,
            topic_revision: 1,
        },
        body: String::new(),
        search_text: name.into(),
    }
}
fn publication(
    generation: &str,
    op: &OperationId,
    revision: i64,
    registry: i64,
    source: &str,
) -> StoredHelpHead {
    StoredHelpHead {
        revision,
        app_version: "1.0.0".into(),
        registry_revision: registry,
        source_digest: sha256_hex(source.as_bytes()),
        generation_id: Some(id(generation)),
        generation_operation_id: Some(op.clone()),
        updated_at_ms: T0,
    }
}
fn seed(s: &SqliteStore) -> StoredHelpHead {
    let op = claim(s, "initial-op", 1, 1);
    let new = publication("initial-generation", &op, 2, 1, "initial-source");
    run_write(s, WriteOptions::default(), move |tx| {
        let doc = document(tx, "initial-generation", "intro", "Original guide")?;
        tx.help().stage_help_document(&doc)?;
        tx.help().publish_help_generation(1, 1, Some(&op), &new, 1)
    })
    .unwrap()
}

#[test]
fn help30_migration_initial_sentinel_reopens_and_old_writer_is_read_only() {
    let m = MIGRATIONS.iter().find(|m| m.version == 30).unwrap();
    assert_eq!(m.checksum(), MIGRATION_0030_SHA256);
    assert_eq!((m.minimum_reader, m.minimum_writer), (8, 30));
    assert!(!m.rebuilds_tables);
    let h = Harness::new();
    drop(SqliteStore::open_with(h.config(), &through(29)).unwrap());
    let s = h.store();
    let head = read(&s, |r| r.help().help_head());
    assert_eq!(head.revision, 1);
    assert_eq!(head.registry_revision, 1);
    assert!(
        head.app_version.is_empty()
            && head.source_digest.is_empty()
            && head.generation_id.is_none()
            && head.generation_operation_id.is_none()
    );
    drop(s);
    let old = SqliteStore::open_with(h.config(), &through(29)).unwrap();
    assert!(matches!(old.info().access, AccessMode::ReadOnly { .. }));
    drop(old);
    let s = h.store();
    assert_eq!(read(&s, |r| r.help().help_head()), head);
}

#[test]
fn help_drafts_are_invisible_and_published_unicode_pages_filter_and_reopen() {
    let h = Harness::new();
    let s = h.store();
    let op = claim(&s, "publish-op", 1, 1);
    let body = format!("A🦀é\n100%_literal\n{}", "x".repeat(1_048_577));
    let expected = body.clone();
    run_write(&s,WriteOptions::default(),move|tx|{
        let a=document(tx,"large-generation","a-guide",&body)?;assert_eq!(tx.help().stage_help_document(&a)?,a.topic);assert_eq!(tx.help().stage_help_document(&a)?,a.topic);
        let mut b=document(tx,"large-generation","b-guide","Other package guide")?;b.topic.source_kind="Package".into();b.topic.package_id=Some(id("package-a"));b.topic.manifest_digest=Some("f".repeat(64));b.topic.screen_ids=vec!["packages".into()];b.topic.provenance=json!({"kind":"Package","packageId":"package-a","packageVersion":"1.0.0","manifestDigest":"f".repeat(64),"contentDigest":b.topic.content_digest});tx.help().stage_help_document(&b)?;
        tx.help().stage_help_document(&missing("large-generation","c-missing"))?;
        assert!(tx.help().help_topic(&id("large-generation"),&id("a-guide"))?.is_none());assert!(tx.help().help_topics(&id("large-generation"),&HelpFilter::default(),None,500)?.is_empty());assert!(tx.help().help_passage(&id("large-generation"),&id("a-guide"),0,2)?.is_none());Ok(())
    }).unwrap();
    assert_eq!(read(&s, |r| r.help().help_head()).revision, 1);
    let new = publication("large-generation", &op, 2, 1, "large-source");
    let saved = new.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.help().publish_help_generation(1, 1, Some(&op), &new, 3)
    })
    .unwrap();
    read(&s, |r| {
        let page =
            r.help()
                .help_topics(&id("large-generation"), &HelpFilter::default(), None, 1)?;
        assert_eq!(page[0].id, id("a-guide"));
        assert_eq!(page[0].character_count, expected.chars().count() as i64);
        assert_eq!(
            r.help().help_topics(
                &id("large-generation"),
                &HelpFilter::default(),
                Some(&page[0].id),
                1
            )?[0]
                .id,
            id("b-guide")
        );
        assert_eq!(
            r.help()
                .help_passage(&id("large-generation"), &id("a-guide"), 1, 2)?
                .as_deref(),
            Some("🦀é")
        );
        assert_eq!(
            r.help()
                .help_passage(
                    &id("large-generation"),
                    &id("a-guide"),
                    page[0].character_count - 3,
                    10
                )?
                .as_deref(),
            Some("xxx")
        );
        let literal = HelpFilter {
            query: Some("100%_literal".into()),
            ..Default::default()
        };
        assert_eq!(
            r.help()
                .help_topics(&id("large-generation"), &literal, None, 500)?
                .len(),
            1
        );
        let wildcard = HelpFilter {
            query: Some("100%_missing".into()),
            ..Default::default()
        };
        assert!(
            r.help()
                .help_topics(&id("large-generation"), &wildcard, None, 500)?
                .is_empty()
        );
        let packages = HelpFilter {
            screen_id: Some("packages".into()),
            package_id: Some(id("package-a")),
            ..Default::default()
        };
        assert_eq!(
            r.help()
                .help_topics(&id("large-generation"), &packages, None, 500)?
                .len(),
            2
        );
        let absent = r
            .help()
            .help_topic(&id("large-generation"), &id("c-missing"))?
            .unwrap();
        assert_eq!(absent.availability, "Missing");
        assert!(absent.content_ref.is_none() && absent.provenance.is_null());
        assert!(
            r.help()
                .help_passage(&id("large-generation"), &id("c-missing"), 0, 10)?
                .is_none()
        );
        for limit in [0, 501] {
            assert!(
                r.help()
                    .help_topics(&id("large-generation"), &HelpFilter::default(), None, limit)
                    .is_err()
            );
            assert!(r.help().help_drafts(None, limit).is_err());
        }
        for (offset, chars) in [
            (-1, 1),
            (i64::MAX, 1),
            (0, 0),
            (0, MAX_HELP_PASSAGE_CHARS + 1),
        ] {
            assert!(
                r.help()
                    .help_passage(&id("large-generation"), &id("a-guide"), offset, chars)
                    .is_err()
            );
        }
        Ok(())
    });
    drop(s);
    let s = h.store();
    assert_eq!(read(&s, |r| r.help().help_head()), saved);
    assert_eq!(
        read(&s, |r| r.help().help_passage(
            &id("large-generation"),
            &id("a-guide"),
            1,
            2
        ))
        .as_deref(),
        Some("🦀é")
    );
}

#[test]
fn help_publication_and_artifact_metadata_roll_back_together_and_history_stays_readable() {
    let h = Harness::new();
    let s = h.store();
    seed(&s);
    let op = claim(&s, "rollback-op", 2, 1);
    let prior = read(&s, |r| r.help().help_head());
    let new = publication("rollback-generation", &op, 3, 1, "new-source");
    let result: Result<(), StorageError> = run_write(&s, WriteOptions::default(), move |tx| {
        let doc = document(tx, "rollback-generation", "replacement", "Replacement")?;
        tx.help().stage_help_document(&doc)?;
        tx.help()
            .publish_help_generation(2, 1, Some(&op), &new, 1)?;
        Err(StorageError::Busy)
    });
    assert!(matches!(result, Err(StorageError::Busy)));
    read(&s, |r| {
        assert_eq!(r.help().help_head()?, prior);
        assert!(
            r.artifacts()
                .artifact(&id("artifact-rollback-generation-replacement"))?
                .is_none()
        );
        assert!(r.help().help_drafts(None, 500)?.is_empty());
        assert_eq!(
            r.help()
                .help_passage(&id("initial-generation"), &id("intro"), 0, 100)?
                .as_deref(),
            Some("Original guide")
        );
        Ok(())
    });
    let op = claim(&s, "replacement-op", 2, 1);
    let new = publication("replacement-generation", &op, 3, 1, "replacement-source");
    run_write(&s, WriteOptions::default(), move |tx| {
        let doc = document(tx, "replacement-generation", "intro", "New guide")?;
        tx.help().stage_help_document(&doc)?;
        tx.help().publish_help_generation(2, 1, Some(&op), &new, 1)
    })
    .unwrap();
    assert_eq!(
        read(&s, |r| r.help().help_passage(
            &id("initial-generation"),
            &id("intro"),
            0,
            100
        ))
        .as_deref(),
        Some("Original guide")
    );
    assert_eq!(read(&s, |r| r.help().help_head()).revision, 3);
    for sql in [
        "UPDATE help_topics SET title='tampered' WHERE generation_id='initial-generation'",
        "DELETE FROM help_topics WHERE generation_id='initial-generation'",
        "UPDATE help_topic_screens SET screen_id='elsewhere' WHERE generation_id='initial-generation'",
        "DELETE FROM help_topic_screens WHERE generation_id='initial-generation'",
        "UPDATE help_generations SET source_digest='changed' WHERE id='initial-generation'",
    ] {
        assert!(h.raw().execute(sql, []).is_err(), "{sql}");
    }
}

#[test]
fn help_head_registry_claim_and_complete_count_are_all_publication_fences() {
    let h = Harness::new();
    let s = h.store();
    let op = claim(&s, "fenced-op", 1, 1);
    let new = publication("fenced-generation", &op, 2, 1, "source");
    let save = new.clone();
    run_write(&s, WriteOptions::default(), |tx| {
        let doc = document(tx, "fenced-generation", "guide", "Guide")?;
        tx.help().stage_help_document(&doc)?;
        Ok(())
    })
    .unwrap();
    let incomplete = run_write(&s, WriteOptions::default(), {
        let op = op.clone();
        move |tx| tx.help().publish_help_generation(1, 1, Some(&op), &new, 2)
    });
    assert!(matches!(incomplete, Err(StorageError::InvalidRecord(_))));
    assert_eq!(read(&s, |r| r.help().help_head()).revision, 1);
    run_write(&s, WriteOptions::default(), |tx| {
        tx.packages().advance_registry(1)
    })
    .unwrap();
    let stale = run_write(&s, WriteOptions::default(), {
        let op = op.clone();
        let new = save.clone();
        move |tx| tx.help().publish_help_generation(1, 1, Some(&op), &new, 1)
    });
    assert!(matches!(stale, Err(StorageError::RevisionConflict(_))));
    let other = claim(&s, "other-op", 1, 2);
    let mut updated = save;
    updated.registry_revision = 2;
    let stale = run_write(&s, WriteOptions::default(), move |tx| {
        tx.help()
            .publish_help_generation(1, 2, Some(&op), &updated, 1)
    });
    assert!(matches!(stale, Err(StorageError::InvalidRecord(_))));
    let genhead = publication("other-generation", &other, 2, 2, "other");
    run_write(&s, WriteOptions::default(), move |tx| {
        let doc = document(tx, "other-generation", "guide", "Other guide")?;
        tx.help().stage_help_document(&doc)?;
        tx.help()
            .publish_help_generation(1, 2, Some(&other), &genhead, 1)
    })
    .unwrap();
    let stale = run_write(&s, WriteOptions::default(), |tx| {
        tx.help()
            .set_help_generation_operation(1, 2, &OperationId(id("other-op")))
    });
    assert!(matches!(stale, Err(StorageError::RevisionConflict(_))));
    assert_eq!(read(&s, |r| r.help().help_head()).revision, 2);
    assert!(
        read(&s, |r| r
            .help()
            .help_topic(&id("fenced-generation"), &id("guide")))
        .is_none()
    );
}

#[test]
fn help_noop_retains_content_revision_but_verifies_actual_staged_identity_and_cleans_in_pages() {
    let h = Harness::new();
    let s = h.store();
    let initial = seed(&s);
    let op = claim(&s, "noop-op", 2, 1);
    let mut same = initial.clone();
    same.generation_operation_id = Some(op.clone());
    same.revision = 3;
    let got = run_write(&s, WriteOptions::default(), {
        let op = op.clone();
        move |tx| tx.help().publish_help_generation(2, 1, Some(&op), &same, 1)
    })
    .unwrap();
    assert_eq!(got.revision, 2);
    assert_eq!(got.generation_id, initial.generation_id);
    assert_eq!(got.generation_operation_id, Some(op.clone()));
    assert_eq!(
        read(&s, |r| r
            .help()
            .help_generation_operation(&id("initial-generation"))),
        Some(OperationId(id("initial-op")))
    );
    assert_eq!(
        read(&s, |r| r
            .help()
            .help_generation_operation(&id("missing-generation"))),
        None
    );
    let mut staged_same = publication("same-generation", &op, 3, 1, "initial-source");
    staged_same.source_digest = initial.source_digest.clone();
    let same_op = op.clone();
    let identical = run_write(&s, WriteOptions::default(), move |tx| {
        let mut doc = document(tx, "same-generation", "intro", "Original guide")?;
        doc.topic.topic_revision = 2;
        tx.help().stage_help_document(&doc)?;
        tx.help()
            .publish_help_generation(2, 1, Some(&same_op), &staged_same, 1)
    })
    .unwrap();
    assert_eq!(identical.revision, 2);
    assert_eq!(identical.generation_id, initial.generation_id);
    assert_eq!(
        read(&s, |r| r
            .help()
            .help_topic(&id("initial-generation"), &id("intro")))
        .unwrap()
        .topic_revision,
        1
    );
    let mut changed = publication("failed-generation", &op, 3, 1, "initial-source");
    changed.source_digest = initial.source_digest;
    run_write(&s, WriteOptions::default(), |tx| {
        for name in ["a", "b", "c"] {
            let doc = document(tx, "failed-generation", name, "Different guide")?;
            tx.help().stage_help_document(&doc)?;
        }
        Ok(())
    })
    .unwrap();
    let refused = run_write(&s, WriteOptions::default(), {
        let op = op.clone();
        move |tx| {
            tx.help()
                .publish_help_generation(2, 1, Some(&op), &changed, 3)
        }
    });
    assert!(matches!(refused, Err(StorageError::InvalidRecord(_))));
    assert_eq!(read(&s, |r| r.help().help_head()).revision, 2);
    assert_eq!(read(&s, |r| r.help().help_drafts(None, 1)).len(), 1);
    let busy = run_write(&s, WriteOptions::default(), {
        let op = op.clone();
        move |tx| {
            tx.help()
                .discard_help_draft(&id("failed-generation"), &op, 2)
        }
    });
    assert!(matches!(busy, Err(StorageError::Busy)));
    let saved = op.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.operations().transition(
            &saved,
            1,
            None,
            OperationState::CancelledBeforeDispatch,
            None,
        )?;
        assert_eq!(
            tx.help()
                .staged_help_topics(&id("failed-generation"), None, 2)?
                .len(),
            2
        );
        assert!(
            !tx.help()
                .discard_help_draft(&id("failed-generation"), &saved, 2)?
        );
        assert_eq!(
            tx.help()
                .staged_help_topics(&id("failed-generation"), None, 500)?
                .len(),
            1
        );
        Ok(())
    })
    .unwrap();
    drop(s);
    let s = h.store();
    run_write(&s, WriteOptions::default(), move |tx| {
        assert!(
            tx.help()
                .discard_help_draft(&id("failed-generation"), &op, 2)?
        );
        assert!(
            tx.help()
                .discard_help_draft(&id("same-generation"), &op, 2)?
        );
        Ok(())
    })
    .unwrap();
    assert!(read(&s, |r| r.help().help_drafts(None, 500)).is_empty());
    assert_eq!(
        read(&s, |r| r.help().help_passage(
            &id("initial-generation"),
            &id("intro"),
            0,
            100
        ))
        .as_deref(),
        Some("Original guide")
    );
}

#[test]
fn help_native_concurrent_publish_has_one_cas_winner() {
    let h = Harness::new();
    let s = Arc::new(h.store());
    let op = claim(&s, "race-op", 1, 1);
    run_write(&*s, WriteOptions::default(), |tx| {
        for generation in ["race-a", "race-b"] {
            let doc = document(tx, generation, "guide", generation)?;
            tx.help().stage_help_document(&doc)?;
        }
        Ok(())
    })
    .unwrap();
    let results = std::thread::scope(|scope| {
        let jobs = ["race-a", "race-b"].map(|generation| {
            let s = s.clone();
            let op = op.clone();
            scope.spawn(move || {
                let new = publication(generation, &op, 2, 1, generation);
                run_write(&*s, WriteOptions::default(), move |tx| {
                    tx.help().publish_help_generation(1, 1, Some(&op), &new, 1)
                })
            })
        });
        jobs.into_iter()
            .map(|j| j.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(StorageError::RevisionConflict(_))))
            .count(),
        1
    );
    assert_eq!(read(&s, |r| r.help().help_head()).revision, 2);
}

#[test]
fn help_stage_requires_real_artifact_identity_and_immutable_document_bytes() {
    let h = Harness::new();
    let s = h.store();
    claim(&s, "identity-op", 1, 1);
    let doc = run_write(&s, WriteOptions::default(), |tx| {
        document(tx, "identity-generation", "guide", "Verified 🦀 guide")
    })
    .unwrap();
    let mut invalid = Vec::new();
    let mut wrong = doc.clone();
    wrong.body.push('!');
    invalid.push(wrong);
    let mut wrong = doc.clone();
    wrong.body.push('\0');
    invalid.push(wrong);
    let mut wrong = doc.clone();
    wrong.topic.character_count += 1;
    invalid.push(wrong);
    let mut wrong = doc.clone();
    wrong.topic.content_ref.as_mut().unwrap()["versionId"] = json!("nonexistent-version");
    invalid.push(wrong);
    let mut wrong = doc.clone();
    wrong.topic.content_ref.as_mut().unwrap()["byteLength"] = json!("1");
    invalid.push(wrong);
    let mut wrong = doc.clone();
    wrong.topic.provenance["contentDigest"] = json!("f".repeat(64));
    invalid.push(wrong);
    let mut wrong = doc.clone();
    wrong.topic.screen_ids.push("help".into());
    invalid.push(wrong);
    for wrong in invalid {
        assert!(matches!(
            run_write(&s, WriteOptions::default(), move |tx| {
                tx.help().stage_help_document(&wrong)
            }),
            Err(StorageError::InvalidRecord(_))
        ));
        assert!(read(&s, |r| r.help().help_drafts(None, 500)).is_empty());
    }
    let saved = doc.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.help().stage_help_document(&saved)
    })
    .unwrap();
    let mut changed = doc;
    changed.search_text.push_str(" changed");
    assert!(matches!(
        run_write(&s, WriteOptions::default(), move |tx| {
            tx.help().stage_help_document(&changed)
        }),
        Err(StorageError::Duplicate { .. })
    ));
    assert!(
        read(&s, |r| r
            .help()
            .help_topic(&id("identity-generation"), &id("guide")))
        .is_none()
    );
}
