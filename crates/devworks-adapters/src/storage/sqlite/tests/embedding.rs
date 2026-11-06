//! Real SQLite invariants over recorded native facts. No provider call or inference execution.
use super::{Harness, T0};
use crate::storage::sqlite::{MIGRATION_0033_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::storage::*;
use devworks_core::protocol::data::DataScope;
use devworks_core::protocol::provider::{ArtifactRef, Digest};
use devworks_core::protocol::{Counter, EventId, Id, OperationId, OperationState};
use serde_json::json;

fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}
fn connection() -> StoredConnection {
    StoredConnection {
        id: id("embedding-connection"),
        name: "Explicit embedding connection".into(),
        profile: "Embedding".into(),
        endpoint: "https://embedding.invalid/v1".into(),
        enabled: true,
        local_free: false,
        open_router: None,
        credential_ref: None,
        credential_revision: 0,
        account_revision: 1,
        refresh_operation_id: None,
        credential_epoch: None,
        lifecycle: "Connected".into(),
        state_reason: None,
        account_digest: Some(sha256_hex(b"account")),
        access_expires_at_ms: None,
        risk: None,
        discovered: Some(vec![DiscoveredModel {
            id: "literal-model".into(),
            context_tokens: None,
        }]),
        discovered_at_ms: Some(T0),
        healthy: Some(true),
        checked_at_ms: Some(T0),
        revision: 1,
        created_at_ms: T0,
        updated_at_ms: T0,
    }
}
fn candidate(store: &SqliteStore, name: &str) -> StoredEmbeddingCandidate {
    let c = connection();
    let model_identity = json!({"version":1,"connectionId":c.id,"endpointProfile":"Embedding","endpoint":c.endpoint,"accountRevision":c.account_revision.to_string(),"accountDigest":c.account_digest,"upstreamModelId":"literal-model"});
    let model_digest = sha256_hex(
        devworks_core::jcs::canonicalize(&model_identity)
            .unwrap()
            .as_bytes(),
    );
    let encoder_config = json!({"version":1,"modelDigest":model_digest,"dimensions":2,"dimensionMode":"Native","metric":"Cosine","elementType":"Float32","encodingFormat":"float"});
    let configuration_digest = sha256_hex(
        devworks_core::jcs::canonicalize(&encoder_config)
            .unwrap()
            .as_bytes(),
    );
    StoredEmbeddingCandidate {
        id: id(name),
        name: "x".repeat(256),
        space_id: id(&format!("embspace-{configuration_digest}")),
        connection_id: c.id,
        account_revision: c.account_revision,
        account_digest: c.account_digest,
        endpoint_digest: sha256_hex(c.endpoint.as_bytes()),
        endpoint: c.endpoint,
        upstream_model_id: "literal-model".into(),
        dimension_mode: "Native".into(),
        dimensions: 2,
        metric: "Cosine".into(),
        element_type: "Float32".into(),
        encoder_config,
        configuration_digest,
        model_digest,
        created_by: store.info().host_actor,
        created_at_ms: T0,
    }
}
fn seeded(h: &Harness) -> SqliteStore {
    let s = h.store();
    run_write(&s, WriteOptions::default(), |tx| {
        tx.router().put_connection(&connection(), None)?;
        Ok(())
    })
    .unwrap();
    let c = candidate(&s, "candidate");
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.router().insert_embedding_candidate(&c, 1)
    })
    .unwrap();
    s
}
fn intent(tx: &mut dyn WriteTransaction, name: &str) -> Result<OperationRecord, StorageError> {
    tx.operations().record_intent(NewOperation {
        id: OperationId(id(name)),
        root_id: None,
        assignment_id: None,
        capability: "router.qualify_embedding_space".into(),
        schema_version: 3,
        args_digest: sha256_hex(name.as_bytes()),
        payload: json!({"recordedFixture":true}),
    })
}
fn start(s: &SqliteStore) -> AttemptFence {
    run_write(s, WriteOptions::default(), |tx| {
        let op = intent(tx, "qualification-operation")?;
        let event = EventId(id("qualification-event"));
        let actor = tx.store().host_actor.clone();
        tx.history().append(NewEvent {
            id: event.clone(),
            actor: actor.clone(),
            project_id: None,
            root_id: None,
            operation_id: Some(op.id.clone()),
            aggregate_kind: "embedding_space".into(),
            aggregate_id: "candidate".into(),
            aggregate_revision: 2,
            kind: "EmbeddingQualificationRequested".into(),
            payload: json!({}),
        })?;
        tx.queue().enqueue(NewQueueEntry {
            id: id("qualification-delivery"),
            consumer: "embedding-qualification".into(),
            event_id: event,
            root_id: None,
            operation_id: Some(op.id.clone()),
            partition: Some("candidate".into()),
            priority: 0,
            ready_at_ms: T0,
            payload: json!({}),
        })?;
        let probe = StoredEmbeddingProbe {
            operation_id: op.id.clone(),
            delivery_id: id("qualification-delivery"),
            request_id: id("native-request"),
            admission_key: id("native-admission"),
            candidate_revision: 1,
            store_id: tx.store().store_id.clone(),
            restore_epoch: tx.store().restore_epoch,
            actor_id: actor,
            scope: DataScope::Workspace,
            connection_revision: 1,
            source_context: json!({"version":1,"fixture":"original source context"}),
            authority_fences: vec![],
        };
        assert_eq!(
            tx.router()
                .start_embedding_qualification(&id("candidate"), 1, &probe)?
                .revision,
            2
        );
        tx.queue().claim_ready(&ClaimRequest {
            consumer: "embedding-qualification".into(),
            owner: id("fixture-worker"),
            lease_ms: 60000,
            limit: 1,
        })?;
        let attempt = tx
            .operations()
            .begin_attempt(&op.id, 1, AttemptBinding::default())?;
        tx.operations().transition(
            &op.id,
            2,
            Some(&attempt.fence()),
            OperationState::Dispatching,
            None,
        )?;
        Ok(attempt.fence())
    })
    .unwrap()
}
fn artifact(
    tx: &mut dyn WriteTransaction,
    name: &str,
    bytes: &[u8],
) -> Result<ArtifactRef, StorageError> {
    let digest = sha256_hex(bytes);
    let version_id = id(&format!("{name}-version"));
    let actor = tx.store().host_actor.clone();
    tx.artifacts().put_blob(
        &digest,
        bytes.len() as i64,
        name,
        &format!("{digest}.{name}"),
    )?;
    tx.artifacts().insert_artifact(&StoredArtifact {
        id: id(name),
        origin_project_id: None,
        origin_work_id: None,
        title: name.into(),
        revision: 1,
        scope: ArtifactScope::Workspace,
        owner: Some(actor),
        retention: ArtifactRetention::Explicit,
        scope_revision: 1,
        reference_revision: 1,
        current_version_id: None,
        state: ArtifactState::Live,
        created_at_ms: T0,
        provenance: json!({"recordedNativeFixture":true}),
    })?;
    tx.artifacts().insert_version(&StoredVersion {
        id: version_id.clone(),
        artifact_id: id(name),
        version_no: 1,
        blob_digest: digest.clone(),
        byte_count: bytes.len() as i64,
        blob_generation: name.into(),
        media_type: "application/octet-stream".into(),
        created_at_ms: T0,
        deleted_at_ms: None,
        source: json!({"kind":"Host","recordedNativeFixture":true}),
    })?;
    Ok(ArtifactRef {
        artifact_id: id(name),
        version_id,
        sha256: Digest::new(digest).unwrap(),
        byte_length: Counter::new(bytes.len() as i64).unwrap(),
        media_type: "application/octet-stream".into(),
    })
}
fn native_facts(s: &SqliteStore) -> StoredEmbeddingEvidence {
    run_write(s,WriteOptions::default(),|tx|{
        let native=intent(tx,"native-operation")?;
        let attempt=tx.operations().begin_attempt(&native.id,1,AttemptBinding::default())?;
        tx.router().record_route_request(&StoredRouteRequest{request_id:id("native-request"),operation_id:native.id.clone(),purpose:"Embedding".into(),session_id:None,project_id:None,model_id:id("candidate"),connection_id:id("embedding-connection"),pin_id:None,created_at_ms:T0})?;
        let mut usage=StoredInferenceAttempt{attempt_id:attempt.id.0.clone(),request_id:id("native-request"),operation_id:native.id.clone(),attempt_number:1,project_id:None,feature_id:None,work_id:None,assignment_id:None,execution_root_id:None,execution_id:None,session_id:None,job_run_id:None,actor_id:tx.store().host_actor.clone(),role:"EmbeddingProbe".into(),purpose:"Embedding".into(),catalog_model_id:id("candidate"),connection_id:id("embedding-connection"),endpoint_profile:"Embedding".into(),access_mode:"Custom".into(),provider_model_id:Some("literal-model".into()),pricing_snapshot:json!({"admission_key":"native-admission","quote":{"classification":"Untracked","connection_revision":1}}),state:"Admitted".into(),revision:1,created_at_ms:T0};
        tx.usage().insert_inference_attempt(&usage)?;
        usage.state="Dispatched".into();usage.revision=2;tx.usage().update_inference_attempt(&usage,1)?;
        usage.state="Settled".into();usage.revision=3;tx.usage().update_inference_attempt(&usage,2)?;
        tx.usage().put_attempt_balance(&StoredAttemptBalance{attempt_id:attempt.id.0.clone(),revision:1,priced_micro_usd:None,reserved_micro_usd:None,classification:"Untracked".into(),monetary_complete:false,incomplete:false,unknown_tail:false,local_quiescent:true,summary:json!({"outcome":"Completed"})},None)?;
        tx.operations().transition(&native.id,2,Some(&attempt.fence()),OperationState::Dispatching,None)?;
        tx.operations().transition(&native.id,3,Some(&attempt.fence()),OperationState::ObservedComplete,None)?;
        Ok(StoredEmbeddingEvidence{request_id:id("native-request"),native_operation_id:native.id,attempt_id:attempt.id.0,result_artifact:artifact(tx,"native-result",b"recorded manifest")?,vector_artifact:artifact(tx,"native-vector",&[0,0,128,63,0,0,0,64])?,reported_model:Some("literal-model".into()),registration_digest:sha256_hex(b"recorded registration"),connection_revision:1})
    }).unwrap()
}
fn stage(
    s: &SqliteStore,
    fence: AttemptFence,
    evidence: StoredEmbeddingEvidence,
) -> StoredEmbeddingQualification {
    run_write(s, WriteOptions::default(), move |tx| {
        tx.router().stage_embedding_probe_evidence(
            &id("candidate"),
            2,
            &OperationId(id("qualification-operation")),
            &fence,
            &evidence,
        )
    })
    .unwrap()
}
fn complete(s: &SqliteStore, fence: AttemptFence) -> StoredEmbeddingQualification {
    run_write(s, WriteOptions::default(), move |tx| {
        tx.operations().transition(
            &OperationId(id("qualification-operation")),
            3,
            Some(&fence),
            OperationState::ObservedComplete,
            None,
        )?;
        tx.router().finish_embedding_qualification(
            &id("candidate"),
            3,
            &OperationId(id("qualification-operation")),
            &fence,
            EmbeddingQualificationState::Qualified,
            None,
        )
    })
    .unwrap()
}

#[test]
fn sql33_preserves_prefix_and_initial_selection_is_explicit_cas() {
    assert_eq!(
        MIGRATIONS
            .iter()
            .find(|m| m.version == 33)
            .unwrap()
            .checksum(),
        MIGRATION_0033_SHA256
    );
    assert_eq!(
        MIGRATIONS.iter().map(|m| m.version).collect::<Vec<_>>(),
        (1..=33).collect::<Vec<_>>()
    );
    let h = Harness::new();
    let s = h.store();
    assert_eq!(
        read_snapshot(&s, &ReadOptions::default(), |r| r
            .router()
            .embedding_selection())
        .unwrap(),
        StoredEmbeddingSelection {
            revision: 1,
            candidate_id: None,
            space_id: None
        }
    );
    assert_eq!(
        run_write(&s, WriteOptions::default(), |tx| tx
            .router()
            .select_embedding_candidate(1, None))
        .unwrap()
        .revision,
        2
    );
    assert!(matches!(
        run_write(&s, WriteOptions::default(), |tx| tx
            .router()
            .select_embedding_candidate(1, None)),
        Err(StorageError::RevisionConflict(_))
    ));
}
#[test]
fn immutable_candidates_preserve_declared_names_and_paged_identity() {
    let h = Harness::new();
    let s = seeded(&h);
    let c = candidate(&s, "candidate");
    assert!(matches!(
        run_write(&s, WriteOptions::default(), move |tx| tx
            .router()
            .insert_embedding_candidate(&c, 1)),
        Err(StorageError::Duplicate { .. })
    ));
    let c = candidate(&s, "second");
    assert!(matches!(
        run_write(&s, WriteOptions::default(), move |tx| tx
            .router()
            .insert_embedding_candidate(&c, 2)),
        Err(StorageError::RevisionConflict(_))
    ));
    let mut c = candidate(&s, "second");
    c.name.push('x');
    assert!(
        run_write(&s, WriteOptions::default(), move |tx| tx
            .router()
            .insert_embedding_candidate(&c, 1))
        .is_err()
    );
    let template = candidate(&s, "template");
    run_write(&s, WriteOptions::default(), move |tx| {
        for n in 0..501 {
            let mut c = template.clone();
            c.id = id(&format!("page-{n:04}"));
            tx.router().insert_embedding_candidate(&c, 1)?;
        }
        Ok(())
    })
    .unwrap();
    let first = read_snapshot(&s, &ReadOptions::default(), |r| {
        r.router().embedding_candidates(None, 500)
    })
    .unwrap();
    assert_eq!(first.len(), 500);
    let tail = read_snapshot(&s, &ReadOptions::default(), |r| {
        r.router()
            .embedding_candidates(first.last().map(|c| &c.id), 500)
    })
    .unwrap();
    assert_eq!(tail.len(), 2);
    assert!(tail[0].id.as_str() > first.last().unwrap().id.as_str());
    assert!(
        read_snapshot(&s, &ReadOptions::default(), |r| r
            .router()
            .embedding_candidates(None, 501))
        .is_err()
    );
}
#[test]
fn native_evidence_is_staged_before_terminal_qualification_and_survives_restart() {
    let h = Harness::new();
    let s = seeded(&h);
    let fence = start(&s);
    let evidence = native_facts(&s);
    let f = fence.clone();
    assert!(
        run_write(&s, WriteOptions::default(), move |tx| tx
            .router()
            .finish_embedding_qualification(
                &id("candidate"),
                2,
                &OperationId(id("qualification-operation")),
                &f,
                EmbeddingQualificationState::Qualified,
                None
            ))
        .is_err()
    );
    assert_eq!(
        h.raw()
            .query_row("SELECT count(*) FROM embedding_spaces", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        stage(&s, fence.clone(), evidence).state,
        EmbeddingQualificationState::Qualifying
    );
    assert_eq!(
        complete(&s, fence).state,
        EmbeddingQualificationState::Qualified
    );
    let selected = run_write(&s, WriteOptions::default(), |tx| {
        tx.router()
            .select_embedding_candidate(1, Some((&id("candidate"), 4)))
    })
    .unwrap();
    assert!(selected.space_id.is_some());
    assert_eq!(
        h.raw()
            .query_row(
                "SELECT count(*) FROM router_embedding_qualifications",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        4
    );
    drop(s);
    let s = h.store();
    assert_eq!(
        read_snapshot(&s, &ReadOptions::default(), |r| r
            .router()
            .embedding_selection())
        .unwrap(),
        selected
    );
}
#[test]
fn stale_owners_and_changed_native_evidence_cannot_qualify() {
    let h = Harness::new();
    let s = seeded(&h);
    let fence = start(&s);
    let evidence = native_facts(&s);
    let mut wrong = evidence.clone();
    wrong.request_id = id("unrelated");
    let f = fence.clone();
    assert!(
        run_write(&s, WriteOptions::default(), move |tx| tx
            .router()
            .stage_embedding_probe_evidence(
                &id("candidate"),
                2,
                &OperationId(id("qualification-operation")),
                &f,
                &wrong
            ))
        .is_err()
    );
    let mut stale = fence.clone();
    stale.attempt_no += 1;
    let e = evidence.clone();
    assert!(matches!(
        run_write(&s, WriteOptions::default(), move |tx| tx
            .router()
            .stage_embedding_probe_evidence(
                &id("candidate"),
                2,
                &OperationId(id("qualification-operation")),
                &stale,
                &e
            )),
        Err(StorageError::LeaseLost { .. })
    ));
    stage(&s, fence.clone(), evidence.clone());
    let mut changed = evidence;
    changed.reported_model = Some("different".into());
    assert!(
        run_write(&s, WriteOptions::default(), move |tx| tx
            .router()
            .stage_embedding_probe_evidence(
                &id("candidate"),
                3,
                &OperationId(id("qualification-operation")),
                &fence,
                &changed
            ))
        .is_err()
    );
    assert_eq!(
        h.raw()
            .query_row("SELECT count(*) FROM embedding_spaces", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}
#[test]
fn source_changes_fence_completion_and_selection_without_relabelling_history() {
    let h = Harness::new();
    let s = seeded(&h);
    let fence = start(&s);
    let evidence = native_facts(&s);
    stage(&s, fence.clone(), evidence);
    complete(&s, fence);
    let previous = read_snapshot(&s, &ReadOptions::default(), |r| {
        r.router().embedding_candidate(&id("candidate"))
    })
    .unwrap()
    .unwrap();
    run_write(&s, WriteOptions::default(), |tx| {
        let mut c = tx
            .router()
            .connection(&id("embedding-connection"))?
            .unwrap();
        c.account_revision = 2;
        c.account_digest = Some(sha256_hex(b"replacement account"));
        tx.router().put_connection(&c, Some(1))?;
        Ok(())
    })
    .unwrap();
    assert!(
        run_write(&s, WriteOptions::default(), |tx| tx
            .router()
            .select_embedding_candidate(1, Some((&id("candidate"), 4))))
        .is_err()
    );
    assert_eq!(
        read_snapshot(&s, &ReadOptions::default(), |r| r
            .router()
            .embedding_candidate(&id("candidate")))
        .unwrap()
        .unwrap(),
        previous
    );
    assert_eq!(
        h.raw()
            .query_row("SELECT count(*) FROM embedding_spaces", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        read_snapshot(&s, &ReadOptions::default(), |r| r
            .router()
            .embedding_selection())
        .unwrap()
        .revision,
        1
    );
}
#[test]
fn unknown_probe_reconciles_same_identity_without_resend() {
    let h = Harness::new();
    let s = seeded(&h);
    let fence = start(&s);
    let evidence = native_facts(&s);
    let f = fence.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.operations().transition(
            &OperationId(id("qualification-operation")),
            3,
            Some(&f),
            OperationState::OutcomeUnknown,
            None,
        )?;
        tx.router().finish_embedding_qualification(
            &id("candidate"),
            2,
            &OperationId(id("qualification-operation")),
            &f,
            EmbeddingQualificationState::Unknown,
            Some("native outcome unknown"),
        )?;
        Ok(())
    })
    .unwrap();
    let old = read_snapshot(&s, &ReadOptions::default(), |r| {
        r.router().embedding_qualification(&id("candidate"))
    })
    .unwrap()
    .unwrap();
    let mut probe = old.probe.unwrap();
    probe.operation_id = OperationId(id("new-operation"));
    probe.candidate_revision = 3;
    assert!(
        run_write(&s, WriteOptions::default(), move |tx| {
            intent(tx, "new-operation")?;
            tx.router()
                .start_embedding_qualification(&id("candidate"), 3, &probe)
        })
        .is_err()
    );
    assert_eq!(
        h.raw()
            .query_row(
                "SELECT count(*) FROM operations WHERE id='new-operation'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    // Same-attempt recovery can stage verified native facts without allocating another request.
    drop(s);
    let s = h.store();
    let f = fence.clone();
    let staged = run_write(&s, WriteOptions::default(), move |tx| {
        tx.router().stage_embedding_probe_evidence(
            &id("candidate"),
            3,
            &OperationId(id("qualification-operation")),
            &f,
            &evidence,
        )
    })
    .unwrap();
    assert_eq!(staged.state, EmbeddingQualificationState::Unknown);
    assert_eq!(
        h.raw()
            .query_row("SELECT count(*) FROM embedding_spaces", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.operations().transition(
            &OperationId(id("qualification-operation")),
            4,
            Some(&fence),
            OperationState::ObservedComplete,
            None,
        )?;
        tx.router().finish_embedding_qualification(
            &id("candidate"),
            4,
            &OperationId(id("qualification-operation")),
            &fence,
            EmbeddingQualificationState::Qualified,
            None,
        )
    })
    .unwrap();
}

#[test]
fn restored_probe_authority_is_not_selection_authority() {
    let h = Harness::new();
    let s = seeded(&h);
    let fence = start(&s);
    let evidence = native_facts(&s);
    stage(&s, fence.clone(), evidence);
    complete(&s, fence);
    drop(s);
    h.raw()
        .execute("UPDATE store_meta SET restore_epoch=restore_epoch+1", [])
        .unwrap();
    let s = h.store();
    assert!(matches!(
        run_write(&s, WriteOptions::default(), |tx| tx
            .router()
            .select_embedding_candidate(1, Some((&id("candidate"), 4)))),
        Err(StorageError::ScopeDenied)
    ));
    assert_eq!(
        read_snapshot(&s, &ReadOptions::default(), |r| r
            .router()
            .embedding_selection())
        .unwrap()
        .revision,
        1
    );
}

#[test]
fn cosmetic_connection_change_preserves_qualification_but_selection_cas_still_applies() {
    let h = Harness::new();
    let s = seeded(&h);
    let fence = start(&s);
    let evidence = native_facts(&s);
    stage(&s, fence.clone(), evidence);
    complete(&s, fence);
    run_write(&s, WriteOptions::default(), |tx| {
        let mut c = tx
            .router()
            .connection(&id("embedding-connection"))?
            .unwrap();
        c.name = "Renamed".into();
        c.local_free = true;
        tx.router().put_connection(&c, Some(1))?;
        Ok(())
    })
    .unwrap();
    let selected = run_write(&s, WriteOptions::default(), |tx| {
        tx.router()
            .select_embedding_candidate(1, Some((&id("candidate"), 4)))
    })
    .unwrap();
    assert_eq!(selected.candidate_id, Some(id("candidate")));
    assert!(matches!(
        run_write(&s, WriteOptions::default(), |tx| tx
            .router()
            .select_embedding_candidate(2, Some((&id("candidate"), 3)))),
        Err(StorageError::RevisionConflict(_))
    ));
}

#[test]
fn existing_vector_identity_is_never_relabelled_by_qualification() {
    let h = Harness::new();
    let s = seeded(&h);
    let fence = start(&s);
    let evidence = native_facts(&s);
    stage(&s, fence.clone(), evidence);
    let c = candidate(&s, "candidate");
    h.raw().execute("INSERT INTO embedding_spaces(id,model_digest,dimensions,metric,element_type) VALUES(?1,?2,2,'L2','Float32')",rusqlite::params![c.space_id.as_str(),sha256_hex(b"older incompatible identity")]).unwrap();
    assert!(
        run_write(&s, WriteOptions::default(), move |tx| {
            tx.operations().transition(
                &OperationId(id("qualification-operation")),
                3,
                Some(&fence),
                OperationState::ObservedComplete,
                None,
            )?;
            tx.router().finish_embedding_qualification(
                &id("candidate"),
                3,
                &OperationId(id("qualification-operation")),
                &fence,
                EmbeddingQualificationState::Qualified,
                None,
            )
        })
        .is_err()
    );
    assert_eq!(
        h.raw()
            .query_row("SELECT metric FROM embedding_spaces", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        "L2"
    );
    assert_eq!(
        read_snapshot(&s, &ReadOptions::default(), |r| r
            .router()
            .embedding_qualification(&id("candidate")))
        .unwrap()
        .unwrap()
        .revision,
        3
    );
    assert_eq!(
        read_snapshot(&s, &ReadOptions::default(), |r| r
            .operations()
            .operation(&OperationId(id("qualification-operation"))))
        .unwrap()
        .unwrap()
        .state,
        OperationState::Dispatching
    );
}

#[test]
fn late_cancellation_preserves_accounted_native_result_without_qualifying() {
    for staged in [false, true] {
        let h = Harness::new();
        let s = seeded(&h);
        let fence = start(&s);
        let evidence = native_facts(&s);
        let revision = if staged {
            stage(&s, fence.clone(), evidence.clone());
            3
        } else {
            2
        };
        run_write(&s, WriteOptions::default(), |tx| {
            tx.queue().cancel(&id("qualification-delivery"))
        })
        .unwrap();
        let f = fence.clone();
        let e = evidence.clone();
        assert!(matches!(
            run_write(&s, WriteOptions::default(), move |tx| tx
                .router()
                .stage_embedding_probe_evidence(
                    &id("candidate"),
                    revision,
                    &OperationId(id("qualification-operation")),
                    &f,
                    &e
                )),
            Err(StorageError::Cancelled)
        ));
        let q = run_write(&s, WriteOptions::default(), move |tx| {
            tx.operations().transition(
                &OperationId(id("qualification-operation")),
                3,
                Some(&fence),
                OperationState::ObservedComplete,
                None,
            )?;
            assert!(matches!(
                tx.router().finish_embedding_qualification(
                    &id("candidate"),
                    revision,
                    &OperationId(id("qualification-operation")),
                    &fence,
                    EmbeddingQualificationState::Qualified,
                    None
                ),
                Err(StorageError::Cancelled)
            ));
            tx.router().finish_embedding_qualification(
                &id("candidate"),
                revision,
                &OperationId(id("qualification-operation")),
                &fence,
                EmbeddingQualificationState::Cancelled,
                Some("configuration intent cancelled after native completion"),
            )
        })
        .unwrap();
        assert_eq!(q.state, EmbeddingQualificationState::Cancelled);
        assert_eq!(
            h.raw()
                .query_row("SELECT count(*) FROM embedding_spaces", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            read_snapshot(&s, &ReadOptions::default(), |r| r
                .operations()
                .operation(&evidence.native_operation_id))
            .unwrap()
            .unwrap()
            .state,
            OperationState::ObservedComplete
        );
    }
}
