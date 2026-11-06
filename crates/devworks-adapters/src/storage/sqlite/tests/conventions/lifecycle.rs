use super::*;
#[test]
fn exact_artifact_version_witness_survives_new_latest_but_deleted_bytes_never_publish() {
    let h = Harness::new();
    let store = h.store();
    let (owner, first, second) = run_write(&store, WriteOptions::default(), |tx| {
        let scope = KnowledgeScope::Workspace;
        let a = artifact(tx, "body", &scope, "alpha")?;
        let owner = record("rules", scope, Some(a));
        tx.conventions().create_convention(&owner)?;
        let first = index(tx, &owner, "version-a-index", None)?;
        tx.conventions().stage_convention_index(&first)?;
        stage_chunks(
            tx,
            &first.generation_id,
            &[chunk(&first, 0, "alpha", 1, None)],
        )?;
        let digest = sha256_hex(b"beta");
        let blob = tx
            .artifacts()
            .put_blob(&digest, 4, "blob-beta", "blobs/beta")?;
        tx.artifacts().insert_version(&StoredVersion {
            id: id("body-version-2"),
            artifact_id: id("body"),
            version_no: 2,
            blob_digest: digest.clone(),
            byte_count: 4,
            blob_generation: blob.generation,
            media_type: "text/markdown".into(),
            created_at_ms: T0 + 1,
            deleted_at_ms: None,
            source: json!({}),
        })?;
        let mut artifact = tx.artifacts().artifact(&id("body"))?.unwrap();
        artifact.current_version_id = Some(id("body-version-2"));
        artifact.revision += 1;
        tx.artifacts().update_artifact(&artifact)?;
        tx.conventions()
            .publish_convention_index(&first.generation_id, 1, None, 1)?;
        let second = index(tx, &owner, "version-a-pending", None)?;
        tx.conventions().stage_convention_index(&second)?;
        stage_chunks(
            tx,
            &second.generation_id,
            &[chunk(&second, 0, "alpha", 1, None)],
        )?;
        tx.artifacts()
            .tombstone_version(&id("version-body"), T0 + 2)?;
        assert!(
            tx.conventions()
                .publish_convention_index(&second.generation_id, 1, Some(&first.generation_id), 1)
                .is_err(),
            "latest version2 cannot stand in for captured deleted version1"
        );
        let mut retired = next(&owner);
        retired.lifecycle = "Retired".into();
        tx.conventions().revise_convention(&retired, 1)?;
        Ok((owner, first, second))
    })
    .unwrap();
    let q = query(vec![KnowledgeScope::Workspace]);
    assert_eq!(
        read(&store, |r| r
            .conventions()
            .convention_lexical_statistics(&q, &["alpha".into()]))
        .documents,
        0
    );
    assert_eq!(
        read(&store, |r| r.conventions().convention(
            &owner.id,
            Some(1),
            &q.scopes
        ))
        .unwrap()
        .content_ref,
        owner.content_ref
    );
    assert_eq!(
        read(&store, |r| r.conventions().convention_index_head(&owner.id))
            .unwrap()
            .generation_id,
        first.generation_id
    );
    assert_eq!(
        read(&store, |r| r
            .conventions()
            .convention_index_generation(&second.generation_id))
        .unwrap()
        .status,
        "Stale"
    );
}
#[test]
fn attachment_only_revisions_reuse_exact_body_index_but_same_digest_new_version_does_not() {
    let h = Harness::new();
    let store = h.store();
    run_write(&store, WriteOptions::default(), |tx| {
        let scope = KnowledgeScope::Workspace;
        let body = artifact(tx, "body", &scope, "alpha")?;
        let owner = record("owner", scope.clone(), Some(body.clone()));
        tx.conventions().create_convention(&owner)?;
        let g = index(tx, &owner, "body-index", None)?;
        tx.conventions().stage_convention_index(&g)?;
        stage_chunks(tx, &g.generation_id, &[chunk(&g, 0, "alpha", 1, None)])?;
        tx.conventions()
            .publish_convention_index(&g.generation_id, 1, None, 1)?;
        let owner2 = next(&owner);
        tx.conventions().revise_convention(&owner2, 1)?;
        let resource = StoredConventionResource {
            id: id("attachment"),
            convention_id: owner.id.clone(),
            revision: 1,
            convention_revision: 2,
            name: "Attachment".into(),
            media_type: String::new(),
            content_ref: artifact(
                tx,
                "attachment-body",
                &scope,
                "attachment bytes never embedded",
            )?,
            author_id: owner.author_id.clone(),
            created_at_ms: T0,
            deleted_at_ms: None,
            lifecycle: "Active".into(),
        };
        tx.conventions().add_convention_resource(&resource, 2)?;
        assert_eq!(
            tx.conventions()
                .convention_index_status(&owner.id)?
                .unwrap()
                .status,
            "LexicalOnly"
        );
        assert_eq!(
            tx.conventions()
                .convention_lexical_statistics(&query(vec![scope.clone()]), &["alpha".into()])?
                .documents,
            1
        );
        assert_eq!(
            tx.conventions().convention_resource(
                &owner.id,
                &resource.id,
                None,
                std::slice::from_ref(&scope)
            )?,
            Some(resource)
        );
        let digest = body["sha256"].as_str().unwrap();
        let blob = tx.artifacts().blob(digest)?.unwrap();
        tx.artifacts().insert_version(&StoredVersion {
            id: id("same-bytes-new-version"),
            artifact_id: id("body"),
            version_no: 2,
            blob_digest: digest.into(),
            byte_count: 5,
            blob_generation: blob.generation,
            media_type: "text/markdown".into(),
            created_at_ms: T0 + 1,
            deleted_at_ms: None,
            source: json!({}),
        })?;
        let mut owner3 = next(&owner2);
        owner3.content_ref.as_mut().unwrap()["versionId"] = json!("same-bytes-new-version");
        tx.conventions().revise_convention(&owner3, 2)?;
        assert_eq!(
            tx.conventions()
                .convention_index_status(&owner.id)?
                .unwrap()
                .status,
            "Stale"
        );
        assert_eq!(
            tx.conventions()
                .convention_lexical_statistics(&query(vec![scope]), &["alpha".into()])?
                .documents,
            0
        );
        assert_eq!(
            tx.conventions()
                .convention_index_generation(&g.generation_id)?
                .unwrap()
                .source_ref,
            body
        );
        Ok(())
    })
    .unwrap();
}
fn post_section(
    tx: &mut dyn WriteTransaction,
    g: &Id,
    id: &Id,
    batch: i64,
    terms: &[ConventionTerm],
) -> Result<(), StorageError> {
    let digest = sha256_hex(
        devworks_core::jcs::canonicalize(&json!(terms))
            .unwrap()
            .as_bytes(),
    );
    tx.conventions()
        .stage_convention_section_terms(g, id, batch, &digest, terms)
}
#[test]
fn huge_heading_source_spans_shared_dictionary_exact_batches_and_compact_lexical_results() {
    let h = Harness::new();
    let store = h.store();
    let generation = run_write(&store, WriteOptions::default(), |tx| {
        tx.conventions()
            .put_convention_embedding_space(&super::retrieval::space())?;
        let heading = "q".repeat(8 * 1024 * 1024);
        let source = format!("# {heading}\r\n{}", "alpha\n".repeat(1000));
        let mut owner = record(
            "normalized",
            KnowledgeScope::Workspace,
            Some(artifact(tx, "source", &KnowledgeScope::Workspace, &source)?),
        );
        owner.title = "Root".into();
        tx.conventions().create_convention(&owner)?;
        let g = index(tx, &owner, "normalized-generation", Some("recorded-space"))?;
        tx.conventions().stage_convention_index(&g)?;
        let root = StoredConventionSection {
            generation_id: g.generation_id.clone(),
            id: id("root-section"),
            parent_id: None,
            heading: Some("Root".into()),
            level: 0,
            source_offset: None,
            source_length: None,
            token_count: 1,
        };
        tx.conventions().stage_convention_section(&root)?;
        post_section(
            tx,
            &g.generation_id,
            &root.id,
            0,
            &[ConventionTerm {
                term: "root".into(),
                frequency: 1,
            }],
        )?;
        let section = StoredConventionSection {
            generation_id: g.generation_id.clone(),
            id: id("heading-section"),
            parent_id: Some(root.id),
            heading: None,
            level: 1,
            source_offset: Some(2),
            source_length: Some(heading.len() as i64),
            token_count: 1,
        };
        tx.conventions().stage_convention_section(&section)?;
        let terms = [ConventionTerm {
            term: heading,
            frequency: 1,
        }];
        post_section(tx, &g.generation_id, &section.id, 0, &terms)?;
        post_section(tx, &g.generation_id, &section.id, 0, &terms)?;
        assert!(
            post_section(
                tx,
                &g.generation_id,
                &section.id,
                0,
                &[ConventionTerm {
                    term: "changed".into(),
                    frequency: 1
                }]
            )
            .is_err()
        );
        for n in 0..1000 {
            let mut c = chunk(&g, n, "alpha", 1, (n == 0).then(|| vec![1.0, 0.0]));
            c.section_id = section.id.clone();
            tx.conventions()
                .stage_convention_chunks(&g.generation_id, std::slice::from_ref(&c))?;
            let t = [ConventionTerm {
                term: "alpha".into(),
                frequency: 1,
            }];
            let digest = sha256_hex(
                devworks_core::jcs::canonicalize(&json!(t))
                    .unwrap()
                    .as_bytes(),
            );
            tx.conventions().stage_convention_chunk_terms(
                &g.generation_id,
                &c.id,
                0,
                &digest,
                &t,
            )?;
        }
        tx.conventions().set_convention_index_status(
            &g.generation_id,
            &g.operation_id,
            "Queued",
            "Indexing",
            Some("actual later embedding batch refused"),
        )?;
        tx.conventions()
            .publish_convention_index(&g.generation_id, 1, None, 1000)?;
        let saved = tx
            .conventions()
            .convention_index_status(&owner.id)?
            .unwrap();
        assert_eq!(saved.status, "LexicalOnly");
        assert_eq!(
            saved.error.as_deref(),
            Some("actual later embedding batch refused")
        );
        assert_eq!(
            tx.conventions().convention_chunk_count(&g.generation_id)?,
            1000
        );
        let stats = tx.conventions().convention_lexical_statistics(
            &query(vec![KnowledgeScope::Workspace]),
            &["alpha".into(), "root".into()],
        )?;
        assert_eq!((stats.documents, stats.total_tokens), (1000, 3000));
        assert!(
            stats
                .document_frequencies
                .iter()
                .all(|t| t.frequency == 1000)
        );
        let found = tx.conventions().convention_ranked_chunks(
            &query(vec![KnowledgeScope::Workspace]),
            &[ConventionWeightedTerm {
                term: "root".into(),
                idf: 1.0,
            }],
            3.0,
            Some("recorded-space"),
            Some(&[1.0, 0.0]),
            50,
        )?;
        assert_eq!(found.len(), 50);
        assert!(found.iter().all(|c| c.dense_score == 0.0
            && (c.score - 0.35).abs() < 1e-12
            && c.chunk.excerpt == "alpha"));
        let spans = tx
            .conventions()
            .convention_sections(&g.generation_id, None, 500)?;
        assert_eq!(spans.len(), 2);
        assert_eq!(
            spans
                .iter()
                .filter_map(|s| s.heading.as_ref())
                .map(String::len)
                .sum::<usize>(),
            4
        );
        assert!(
            tx.conventions()
                .convention_chunk(&g.generation_id, &id("chunk-normalized-generation-0"))?
                .unwrap()
                .vector
                .is_some(),
            "actual paid partial vector is retained, never reused for dense fallback"
        );
        Ok(g.generation_id)
    })
    .unwrap();
    let raw = h.raw();
    let stored: i64 = raw
        .query_row(
            "SELECT sum(length(cast(term AS BLOB))) FROM convention_terms WHERE generation_id=?1",
            [generation.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        stored,
        8 * 1024 * 1024 + 9,
        "large ancestor term stored once, not once/chunk"
    );
    drop(raw);
    drop(store);
    let store = h.store();
    assert_eq!(
        read(&store, |r| r.conventions().convention_sections(
            &generation,
            None,
            500
        ))
        .len(),
        2
    );
}
#[test]
fn additive_term_batches_require_complete_source_counts_and_replay_exactly() {
    let h = Harness::new();
    let store = h.store();
    run_write(&store, WriteOptions::default(), |tx| {
        let text = format!("# {}\nbody", "alpha ".repeat(750));
        let mut owner = record(
            "batch-owner",
            KnowledgeScope::Workspace,
            Some(artifact(
                tx,
                "batch-source",
                &KnowledgeScope::Workspace,
                &text,
            )?),
        );
        owner.title = "-".into();
        tx.conventions().create_convention(&owner)?;
        let g = index(tx, &owner, "batch-generation", None)?;
        tx.conventions().stage_convention_index(&g)?;
        let root = StoredConventionSection {
            generation_id: g.generation_id.clone(),
            id: id("batch-root"),
            parent_id: None,
            heading: Some("-".into()),
            level: 0,
            source_offset: None,
            source_length: None,
            token_count: 0,
        };
        tx.conventions().stage_convention_section(&root)?;
        let section = StoredConventionSection {
            generation_id: g.generation_id.clone(),
            id: id("batch-heading"),
            parent_id: Some(root.id),
            heading: None,
            level: 1,
            source_offset: Some(2),
            source_length: Some(4499),
            token_count: 750,
        };
        tx.conventions().stage_convention_section(&section)?;
        let mut c = chunk(&g, 0, "body", 1, None);
        c.section_id = section.id.clone();
        tx.conventions()
            .stage_convention_chunks(&g.generation_id, std::slice::from_ref(&c))?;
        let t = [ConventionTerm {
            term: "body".into(),
            frequency: 1,
        }];
        let digest = sha256_hex(
            devworks_core::jcs::canonicalize(&json!(t))
                .unwrap()
                .as_bytes(),
        );
        tx.conventions()
            .stage_convention_chunk_terms(&g.generation_id, &c.id, 0, &digest, &t)?;
        let first = [ConventionTerm {
            term: "alpha".into(),
            frequency: 500,
        }];
        post_section(tx, &g.generation_id, &section.id, 0, &first)?;
        post_section(tx, &g.generation_id, &section.id, 0, &first)?;
        assert!(
            tx.conventions()
                .publish_convention_index(&g.generation_id, 1, None, 1)
                .is_err()
        );
        let rest = [ConventionTerm {
            term: "alpha".into(),
            frequency: 250,
        }];
        post_section(tx, &g.generation_id, &section.id, 1, &rest)?;
        assert!(
            post_section(tx, &g.generation_id, &section.id, 2, &rest).is_err(),
            "cannot exceed actual declared tokens"
        );
        tx.conventions()
            .publish_convention_index(&g.generation_id, 1, None, 1)?;
        let stats = tx.conventions().convention_lexical_statistics(
            &query(vec![KnowledgeScope::Workspace]),
            &["alpha".into()],
        )?;
        assert_eq!((stats.documents, stats.total_tokens), (1, 751));
        assert_eq!(stats.document_frequencies[0].frequency, 1);
        Ok(())
    })
    .unwrap();
}
#[test]
fn deletion_cascades_resource_association_history_in_same_revision_transaction() {
    let h = Harness::new();
    let store = h.store();
    let (owner, resource) = run_write(&store, WriteOptions::default(), |tx| {
        let owner = record("deleting", KnowledgeScope::Workspace, None);
        tx.conventions().create_convention(&owner)?;
        let resource = StoredConventionResource {
            id: id("delete-resource"),
            convention_id: owner.id.clone(),
            revision: 1,
            convention_revision: 1,
            name: "Resource".into(),
            media_type: "\0".into(),
            content_ref: artifact(tx, "delete-bytes", &KnowledgeScope::Workspace, "retained")?,
            author_id: owner.author_id.clone(),
            created_at_ms: T0,
            deleted_at_ms: None,
            lifecycle: "Active".into(),
        };
        tx.conventions().add_convention_resource(&resource, 1)?;
        Ok((owner, resource))
    })
    .unwrap();
    let o = owner.clone();
    let r = resource.clone();
    run_write(&store, WriteOptions::default(), move |tx| {
        let mut deleted = next(&o);
        deleted.lifecycle = "Deleted".into();
        tx.conventions().revise_convention(&deleted, 1)?;
        let mut retired = r.clone();
        retired.revision = 2;
        retired.convention_revision = 2;
        retired.lifecycle = "Deleted".into();
        retired.deleted_at_ms = Some(T0 + 1);
        tx.conventions()
            .revise_convention_resource(&retired, 1, 2)?;
        assert!(tx.artifacts().artifact(&id("delete-bytes"))?.is_some());
        assert_eq!(
            tx.conventions().convention_resource(
                &o.id,
                &r.id,
                Some(1),
                &[KnowledgeScope::Workspace]
            )?,
            Some(r)
        );
        Ok(())
    })
    .unwrap();
    let stale = resource;
    assert!(
        run_write(&store, WriteOptions::default(), move |tx| {
            let mut r = stale;
            r.revision = 3;
            r.convention_revision = 2;
            r.lifecycle = "Deleted".into();
            r.deleted_at_ms = Some(T0 + 2);
            tx.conventions().revise_convention_resource(&r, 2, 2)
        })
        .is_err(),
        "cannot append to an already committed owner deletion revision"
    );
}
#[test]
fn small_heading_caches_are_exact_source_segments_once_and_large_or_nul_segments_keep_spans() {
    let h = Harness::new();
    let store = h.store();
    let (generation, source_bytes) = run_write(&store, WriteOptions::default(), |tx| {
        let large = "λ".repeat(32769);
        let source = format!(
            "# λ hello\r\nalpha\n## β two\r\nbeta\n# nul\0title\r\ngamma\n# {large}\r\ndelta"
        );
        let owner = record(
            "cache-owner",
            KnowledgeScope::Workspace,
            Some(artifact(
                tx,
                "cache-source",
                &KnowledgeScope::Workspace,
                &source,
            )?),
        );
        tx.conventions().create_convention(&owner)?;
        let g = index(tx, &owner, "cache-generation", None)?;
        tx.conventions().stage_convention_index(&g)?;
        let root = StoredConventionSection {
            generation_id: g.generation_id.clone(),
            id: id("cache-root"),
            parent_id: None,
            heading: Some(owner.title),
            level: 0,
            source_offset: None,
            source_length: None,
            token_count: 0,
        };
        tx.conventions().stage_convention_section(&root)?;
        for (n, title) in ["λ hello", "β two", "nul\0title", large.as_str()]
            .into_iter()
            .enumerate()
        {
            let start = source.find(title).unwrap();
            let end = start + title.len();
            let segment = source.get(start..end).unwrap();
            // This source-reader step is outside SQLite: use the exact original UTF-8 slice,
            // never the chunker's decorated '# heading' embedding segment.
            let cache = (segment.len() <= 64 * 1024
                && serde_json::from_value::<devworks_core::protocol::support::Content>(json!(
                    segment
                ))
                .is_ok())
            .then(|| segment.to_owned());
            let section = StoredConventionSection {
                generation_id: g.generation_id.clone(),
                id: id(&format!("cached-{n}")),
                parent_id: Some(root.id.clone()),
                heading: cache,
                level: 1,
                source_offset: Some(start as i64),
                source_length: Some(title.len() as i64),
                token_count: 0,
            };
            tx.conventions().stage_convention_section(&section)?;
            tx.conventions().stage_convention_section(&section)?;
            if n < 2 {
                assert_eq!(
                    tx.conventions()
                        .convention_section(&g.generation_id, &section.id)?
                        .unwrap()
                        .heading
                        .as_deref(),
                    Some(segment)
                );
            } else {
                assert!(section.heading.is_none());
                let mut illegal = section.clone();
                illegal.id = id(&format!("invalid-cache-{n}"));
                illegal.heading = Some(segment.into());
                assert!(tx.conventions().stage_convention_section(&illegal).is_err());
            }
        }
        let original = tx
            .conventions()
            .convention_section(&g.generation_id, &id("cached-0"))?
            .unwrap();
        let mut duplicate = original.clone();
        duplicate.id = id("duplicate-span");
        assert!(
            tx.conventions()
                .stage_convention_section(&duplicate)
                .is_err(),
            "same source heading cannot amplify through fresh section IDs"
        );
        let mut overlap = original;
        overlap.id = id("overlapping-span");
        overlap.source_offset = overlap.source_offset.map(|o| o + 1);
        overlap.source_length = overlap.source_length.map(|n| n - 1);
        overlap.heading = None;
        assert!(tx.conventions().stage_convention_section(&overlap).is_err());
        Ok((g.generation_id, source.len()))
    })
    .unwrap();
    let raw = h.raw();
    let cached:i64=raw.query_row("SELECT coalesce(sum(length(cast(heading AS BLOB))),0) FROM convention_sections WHERE generation_id=?1 AND level>0",[generation.as_str()],|r|r.get(0)).unwrap();
    assert_eq!(cached, "λ hello".len() as i64 + "β two".len() as i64);
    assert!(cached as usize <= source_bytes);
    drop(raw);
    drop(store);
    let store = h.store();
    let sections = read(&store, |r| {
        r.conventions().convention_sections(&generation, None, 500)
    });
    assert_eq!(sections.len(), 5);
    assert!(
        sections
            .iter()
            .filter(|s| s.level > 0)
            .all(|s| s.source_offset.is_some() && s.source_length.is_some())
    );
}
