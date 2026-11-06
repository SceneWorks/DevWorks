use super::*;
fn space() -> StoredConventionEmbeddingSpace {
    StoredConventionEmbeddingSpace {
        id: "recorded-space".into(),
        provider: "recorded-provider".into(),
        model: "recorded-model".into(),
        configuration_digest: "c".repeat(64),
        dimensions: 2,
        metric: "Cosine".into(),
        element_type: "Float32".into(),
    }
}
#[test]
fn scoped_current_generation_ranking_uses_full_corpus_and_exact_vectors() {
    let h = Harness::new();
    let store = h.store();
    let (row, generation) = run_write(&store, WriteOptions::default(), |tx| {
        tx.conventions().put_convention_embedding_space(&space())?;
        let row = record(
            "rules",
            KnowledgeScope::Workspace,
            Some(artifact(
                tx,
                "rules-body",
                &KnowledgeScope::Workspace,
                "alpha beta",
            )?),
        );
        tx.conventions().create_convention(&row)?;
        let generation = index(tx, &row, "index-rules", Some("recorded-space"))?;
        tx.conventions().stage_convention_index(&generation)?;
        tx.conventions().stage_convention_index(&generation)?;
        let a = chunk(&generation, 0, "alpha", 10, Some(vec![1.0, 0.0]));
        let mut wrong = chunk(&generation, 1, "beta", 1, Some(vec![0.0, 1.0, 0.0]));
        assert!(
            tx.conventions()
                .stage_convention_chunks(&generation.generation_id, &[a.clone(), wrong.clone()])
                .is_err()
        );
        assert!(
            tx.conventions()
                .convention_chunks(&generation.generation_id, None, 500)?
                .is_empty(),
            "failed batch leaves no partial chunk"
        );
        wrong.vector = Some(vec![0.0, 1.0]);
        tx.conventions().stage_convention_chunks(
            &generation.generation_id,
            &[
                a.clone(),
                wrong,
                chunk(&generation, 2, "alpha", 1, Some(vec![1.0, 1.0])),
            ],
        )?;
        tx.conventions()
            .stage_convention_chunks(&generation.generation_id, &[a.clone()])?;
        let mut modified = a;
        modified.text = "tampered".into();
        assert!(
            tx.conventions()
                .stage_convention_chunks(&generation.generation_id, &[modified])
                .is_err()
        );
        assert!(
            tx.conventions()
                .publish_convention_index(&generation.generation_id, 1, None, 2)
                .is_err()
        );
        assert!(
            tx.conventions()
                .convention_index_head(&row.id, None)?
                .is_none()
        );
        assert_eq!(
            tx.conventions()
                .convention_lexical_statistics(
                    &query(vec![KnowledgeScope::Workspace]),
                    &["alpha".into()]
                )?
                .documents,
            0,
            "drafts are not searchable"
        );
        tx.conventions()
            .publish_convention_index(&generation.generation_id, 1, None, 3)?;
        let other = project(tx, "private")?;
        let hidden = record(
            "hidden",
            other.clone(),
            Some(artifact(tx, "hidden-body", &other, "alpha")?),
        );
        tx.conventions().create_convention(&hidden)?;
        let hg = index(tx, &hidden, "hidden-index", Some("recorded-space"))?;
        tx.conventions().stage_convention_index(&hg)?;
        tx.conventions().stage_convention_chunks(
            &hg.generation_id,
            &[chunk(&hg, 0, "alpha", 100, Some(vec![1.0, 0.0]))],
        )?;
        tx.conventions()
            .publish_convention_index(&hg.generation_id, 1, None, 1)?;
        Ok((row, generation))
    })
    .unwrap();
    let q = query(vec![KnowledgeScope::Workspace]);
    let stats = read(&store, |r| {
        r.conventions()
            .convention_lexical_statistics(&q, &["alpha".into(), "absent".into()])
    });
    assert_eq!((stats.documents, stats.total_tokens), (3, 12));
    assert_eq!(
        stats.document_frequencies,
        vec![
            ConventionTerm {
                term: "absent".into(),
                frequency: 0
            },
            ConventionTerm {
                term: "alpha".into(),
                frequency: 2
            }
        ]
    );
    let weighted = [ConventionWeightedTerm {
        term: "alpha".into(),
        idf: (1.0_f64 + (3.0 - 2.0 + 0.5) / (2.0 + 0.5)).ln(),
    }];
    let ranked = read(&store, |r| {
        r.conventions().convention_ranked_chunks(
            &q,
            &weighted,
            4.0,
            Some("recorded-space"),
            Some(&[1.0, 0.0]),
            2,
        )
    });
    assert_eq!(ranked.len(), 2);
    assert_eq!(ranked[0].chunk.index, 0);
    assert!((ranked[0].score - 1.0).abs() < 1e-6);
    assert_eq!(ranked[0].chunk.convention_id, row.id);
    assert!(ranked[1].dense_score > 0.7);
    let lexical = read(&store, |r| {
        r.conventions()
            .convention_ranked_chunks(&q, &weighted, 4.0, None, None, 1)
    });
    assert!(
        (lexical[0].score - 0.35).abs() < 1e-12,
        "lexical fallback retains frozen .35 weight"
    );
    let p1 = read(&store, |r| {
        r.conventions()
            .convention_chunks(&generation.generation_id, None, 1)
    });
    let p2 = read(&store, |r| {
        r.conventions()
            .convention_chunks(&generation.generation_id, Some(p1[0].index), 1)
    });
    assert_eq!((p1[0].index, p2[0].index), (0, 1));
    assert!(
        read_snapshot(&store, &ReadOptions::default(), |r| r
            .conventions()
            .convention_ranked_chunks(
                &q,
                &weighted,
                4.0,
                Some("recorded-space"),
                Some(&[0.0, 0.0]),
                1
            ))
        .is_err()
    );
    let mut filtered = q.clone();
    filtered.applicability = vec!["TypeScript".into()];
    assert_eq!(
        read(&store, |r| r
            .conventions()
            .convention_lexical_statistics(&filtered, &["alpha".into()]))
        .documents,
        0
    );
    let saved = read(&store, |r| {
        r.conventions().convention_index_head(&row.id, None)
    })
    .unwrap();
    assert_eq!(saved.status, "Ready");
    let saved_index = generation.clone();
    assert!(
        run_write(&store, WriteOptions::default(), move |tx| tx
            .conventions()
            .discard_convention_index_chunks(
                &saved_index.generation_id,
                &saved_index.operation_id,
                1
            ))
        .is_err()
    );
    drop(store);
    let store = SqliteStore::open(h.config()).unwrap();
    assert_eq!(
        read(&store, |r| r.conventions().convention_chunks(
            &generation.generation_id,
            None,
            1
        ))[0]
            .vector,
        Some(vec![1.0, 0.0])
    );
}
#[test]
fn source_revision_and_operation_cancellation_fence_publication_and_cleanup_is_bounded() {
    let h = Harness::new();
    let store = h.store();
    let (row, generation) = run_write(&store, WriteOptions::default(), |tx| {
        let row = record(
            "rules",
            KnowledgeScope::Workspace,
            Some(artifact(tx, "body", &KnowledgeScope::Workspace, "alpha")?),
        );
        tx.conventions().create_convention(&row)?;
        let generation = index(tx, &row, "stale-index", None)?;
        tx.conventions().stage_convention_index(&generation)?;
        tx.conventions().stage_convention_chunks(
            &generation.generation_id,
            &[
                chunk(&generation, 0, "alpha", 1, None),
                chunk(&generation, 1, "beta", 1, None),
            ],
        )?;
        Ok((row, generation))
    })
    .unwrap();
    let stale = generation.clone();
    let revised = next(&row);
    run_write(&store, WriteOptions::default(), move |tx| {
        tx.conventions().revise_convention(&revised, 1)?;
        assert!(
            tx.conventions()
                .publish_convention_index(&stale.generation_id, 1, None, 2)
                .is_err()
        );
        assert!(
            tx.conventions()
                .publish_convention_index(&stale.generation_id, 2, None, 2)
                .is_err()
        );
        Ok(())
    })
    .unwrap();
    assert_eq!(
        read(&store, |r| r
            .conventions()
            .convention_index_generation(&generation.generation_id))
        .unwrap()
        .status,
        "Stale"
    );
    let op = generation.operation_id.clone();
    run_write(&store, WriteOptions::default(), move |tx| {
        tx.operations()
            .transition(
                &op,
                1,
                None,
                devworks_core::protocol::OperationState::CancelledBeforeDispatch,
                None,
            )
            .map(|_| ())
    })
    .unwrap();
    let saved_index = generation.clone();
    run_write(&store, WriteOptions::default(), move |tx| {
        assert!(!tx.conventions().discard_convention_index_chunks(
            &saved_index.generation_id,
            &saved_index.operation_id,
            1
        )?);
        assert!(tx.conventions().discard_convention_index_chunks(
            &saved_index.generation_id,
            &saved_index.operation_id,
            1
        )?);
        Ok(())
    })
    .unwrap();
    assert!(
        read(&store, |r| r.conventions().convention_chunks(
            &generation.generation_id,
            None,
            500
        ))
        .is_empty()
    );
    assert!(
        read(&store, |r| r
            .conventions()
            .convention_index_generation(&generation.generation_id))
        .is_some(),
        "immutable source/operation provenance survives bounded payload cleanup"
    );
}
#[test]
fn embedding_input_replay_keeps_winner_refs_and_exact_actor_scope_space_and_bytes() {
    let h = Harness::new();
    let store = h.store();
    let row = run_write(&store, WriteOptions::default(), |tx| {
        let scope = project(tx, "inputs-project")?;
        tx.conventions().put_convention_embedding_space(&space())?;
        let first = artifact(tx, "input-1", &scope, "exact input")?;
        let digest = sha256_hex(
            devworks_core::jcs::canonicalize(&json!([{"sha256":first["sha256"],"byteCount":11}]))
                .unwrap()
                .as_bytes(),
        );
        let row = StoredConventionEmbeddingInputs {
            request_id: id("embedding-request"),
            actor_id: tx.store().host_actor.clone(),
            project_id: scope.project().cloned(),
            context_digest: "d".repeat(64),
            embedding_space: "recorded-space".into(),
            input_digest: digest,
            input_refs: vec![first],
            created_at_ms: T0,
        };
        let saved = tx.conventions().insert_convention_embedding_inputs(&row)?;
        let mut candidate = row.clone();
        candidate.input_refs = vec![artifact(tx, "input-2", &scope, "exact input")?];
        assert_eq!(
            tx.conventions()
                .insert_convention_embedding_inputs(&candidate)?,
            saved,
            "equivalent staged UUID cannot change an already admitted request frame"
        );
        candidate.context_digest = "e".repeat(64);
        assert!(
            tx.conventions()
                .insert_convention_embedding_inputs(&candidate)
                .is_err()
        );
        candidate = row.clone();
        candidate.project_id = None;
        assert!(
            tx.conventions()
                .insert_convention_embedding_inputs(&candidate)
                .is_err()
        );
        candidate = row.clone();
        candidate.input_digest = "a".repeat(64);
        assert!(
            tx.conventions()
                .insert_convention_embedding_inputs(&candidate)
                .is_err()
        );
        let mut changed = space();
        changed.model = "another model".into();
        assert!(
            tx.conventions()
                .put_convention_embedding_space(&changed)
                .is_err()
        );
        Ok(saved)
    })
    .unwrap();
    drop(store);
    let store = SqliteStore::open(h.config()).unwrap();
    assert_eq!(
        read(&store, |r| r
            .conventions()
            .convention_embedding_inputs(&row.request_id)),
        Some(row)
    );
}
