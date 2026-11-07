use super::*;
use devworks_core::protocol::OperationState;
fn draft(
    tx: &mut dyn WriteTransaction,
    name: &str,
) -> Result<StoredConventionImport, StorageError> {
    let source = artifact(
        tx,
        &format!("{name}-source"),
        &KnowledgeScope::Workspace,
        "portable manifest",
    )?;
    let op = OperationId(id(&format!("{name}-import")));
    tx.operations().record_intent(NewOperation {
        id: op.clone(),
        root_id: None,
        assignment_id: None,
        capability: "conventions.import".into(),
        schema_version: 3,
        args_digest: "a".repeat(64),
        payload: json!({"source":source}),
    })?;
    let row = StoredConventionImport {
        operation_id: op,
        convention_id: id(name),
        scope: KnowledgeScope::Workspace,
        author_id: ActorId(id("host")),
        source_ref: source,
        expected_scope_revision: tx
            .conventions()
            .convention_scope_revision(&KnowledgeScope::Workspace)?,
        revision: 1,
        phase: "Queued".into(),
        checkpoint: json!({}),
        processed_resources: 0,
        total_resources: None,
        error: None,
        created_at_ms: T0,
        updated_at_ms: T0,
    };
    tx.conventions().begin_convention_import(&row)
}
fn capture(
    tx: &mut dyn WriteTransaction,
    d: &StoredConventionImport,
    n: i64,
) -> Result<StoredConventionImportArtifact, StorageError> {
    let name = format!("{}-entry-{n}", d.convention_id.as_str());
    let bytes = format!("actual bytes {n}");
    let digest = sha256_hex(bytes.as_bytes());
    let path = format!("blobs/{name}");
    let blob =
        tx.artifacts()
            .put_blob(&digest, bytes.len() as i64, &format!("blob-{name}"), &path)?;
    let op = OperationId(id(&format!("stage-{name}")));
    let kind = if n == 0 { "Markdown" } else { "Resource" };
    let key = (n != 0).then(|| format!("key-{n}"));
    tx.operations().record_intent(NewOperation{id:op.clone(),root_id:None,assignment_id:None,capability:STAGE_CAPABILITY.into(),schema_version:1,args_digest:digest.clone(),payload:json!({"digest":digest,"byteCount":bytes.len(),"source":{"kind":"ConventionImportSource","operationId":d.operation_id,"conventionId":d.convention_id,"ordinal":n,"entryKind":kind,"resourceKey":key}})})?;
    let attempt = tx
        .operations()
        .begin_attempt(&op, 1, AttemptBinding::default())?;
    tx.operations().transition(
        &op,
        2,
        Some(&attempt.fence()),
        OperationState::Dispatching,
        None,
    )?;
    let row = StoredConventionImportArtifact {
        operation_id: d.operation_id.clone(),
        ordinal: n,
        kind: kind.into(),
        resource_id: (n != 0).then(|| id(&format!("resource-{name}"))),
        resource_key: key,
        resource_name: (n != 0).then(|| format!("Name {n}")),
        resource_lifecycle: (n != 0).then(|| "Active".into()),
        resource_media_type: (n != 0).then(|| "\0".into()),
        artifact: StoredArtifact {
            id: id(&name),
            origin_project_id: None,
            origin_work_id: None,
            title: name.clone(),
            revision: 1,
            scope: ArtifactScope::Workspace,
            owner: Some(d.author_id.clone()),
            retention: ArtifactRetention::Explicit,
            scope_revision: 1,
            reference_revision: 1,
            current_version_id: None,
            state: ArtifactState::Live,
            created_at_ms: T0,
            provenance: json!({"principalKind":"Host","producedBy":tx.store().host_actor}),
        },
        version: StoredVersion {
            id: id(&format!("version-{name}")),
            artifact_id: id(&name),
            version_no: 1,
            blob_digest: digest,
            byte_count: bytes.len() as i64,
            blob_generation: blob.generation,
            media_type: "application/octet-stream".into(),
            created_at_ms: T0,
            deleted_at_ms: None,
            source: json!({"operationId":op}),
        },
        staging_operation_id: op.clone(),
        staging_fence: attempt.fence(),
        managed_path: path,
    };
    tx.conventions().stage_convention_import_artifact(&row)?;
    tx.operations().transition(
        &op,
        3,
        Some(&attempt.fence()),
        OperationState::ObservedComplete,
        None,
    )?;
    Ok(row)
}
fn ready(
    tx: &mut dyn WriteTransaction,
    d: &StoredConventionImport,
    body: &StoredConventionImportArtifact,
    count: i64,
) -> Result<StoredConventionImport, StorageError> {
    let reference = json!({"artifactId":body.artifact.id,"versionId":body.version.id,"sha256":body.version.blob_digest,"byteLength":body.version.byte_count.to_string(),"mediaType":body.version.media_type});
    let mut row = record(d.convention_id.as_str(), d.scope.clone(), Some(reference));
    row.provenance = json!({"kind":"Imported","actorId":"host","source":d.source_ref,"schema":{"schemaId":"devworks.conventions/portable","revision":"1","sha256":"a".repeat(64)},"importedAtMs":T0.to_string()});
    let updated = tx
        .conventions()
        .set_convention_import_metadata(&d.operation_id, &row, None)?;
    tx.conventions().set_convention_import_phase(
        &d.operation_id,
        updated.revision,
        "Ready",
        Some(count),
        None,
        None,
    )
}
#[test]
fn private_import_pages_publish_atomically_reopen_and_reject_stale_counts() {
    let h = Harness::new();
    let store = h.store();
    let d = run_write(&store, WriteOptions::default(), |tx| {
        let d = draft(tx, "streamed")?;
        let body = capture(tx, &d, 0)?;
        for n in 1..=501 {
            capture(tx, &d, n)?;
        }
        let d = ready(tx, &d, &body, 501)?;
        assert!(tx.artifacts().artifact(&body.artifact.id)?.is_none());
        assert!(
            tx.conventions()
                .convention(&d.convention_id, None, &[KnowledgeScope::Workspace])?
                .is_none()
        );
        assert_eq!(
            tx.artifacts().live_references(&body.version.blob_digest)?,
            1
        );
        Ok(d)
    })
    .unwrap();
    assert_eq!(
        read(&store, |r| r.conventions().convention_import_artifacts(
            &d.operation_id,
            None,
            500
        ))
        .len(),
        500
    );
    assert_eq!(
        read(&store, |r| r.conventions().convention_import_artifacts(
            &d.operation_id,
            Some(499),
            500
        ))
        .len(),
        2
    );
    let rejected = d.clone();
    assert!(
        run_write(&store, WriteOptions::default(), move |tx| tx
            .conventions()
            .publish_convention_import(
                &rejected.operation_id,
                rejected.revision,
                rejected.expected_scope_revision,
                500
            ))
        .is_err()
    );
    assert!(read(&store, |r| r.artifacts().artifact(&id("streamed-entry-0"))).is_none());
    h.raw().execute_batch("CREATE TRIGGER fail_convention_import BEFORE INSERT ON convention_heads WHEN NEW.id='streamed' BEGIN SELECT RAISE(ABORT,'injected publication failure'); END;").unwrap();
    let failing = d.clone();
    run_write(&store, WriteOptions::default(), move |tx| {
        assert!(
            tx.conventions()
                .publish_convention_import(
                    &failing.operation_id,
                    failing.revision,
                    failing.expected_scope_revision,
                    501
                )
                .is_err()
        );
        assert!(
            tx.artifacts().artifact(&id("streamed-entry-0"))?.is_none(),
            "method savepoint rolls public artifact insert back even if caller commits outer UoW"
        );
        artifact(
            tx,
            "outer-transaction-survived",
            &KnowledgeScope::Workspace,
            "committed after refused import",
        )?;
        Ok(())
    })
    .unwrap();
    h.raw()
        .execute_batch("DROP TRIGGER fail_convention_import")
        .unwrap();
    let publishing = d.clone();
    run_write(&store, WriteOptions::default(), move |tx| {
        tx.conventions().publish_convention_import(
            &publishing.operation_id,
            publishing.revision,
            publishing.expected_scope_revision,
            501,
        )?;
        Ok(())
    })
    .unwrap();
    drop(store);
    let store = h.store();
    assert_eq!(
        read(&store, |r| r
            .conventions()
            .convention_import(&d.operation_id))
        .unwrap()
        .phase,
        "Published"
    );
    let q = ConventionResourceQuery {
        scopes: vec![KnowledgeScope::Workspace],
        convention_id: Some(d.convention_id),
        ..Default::default()
    };
    let first = read(&store, |r| {
        r.conventions().convention_resources(&q, None, 500)
    });
    assert_eq!(first.len(), 500);
    assert_eq!(first[0].media_type, "\0");
    assert_eq!(
        read(&store, |r| r.conventions().convention_resources(
            &q,
            Some(&first.last().unwrap().id),
            500
        ))
        .len(),
        1
    );
}
#[test]
fn cancelled_import_cleanup_is_bounded_and_never_exposes_private_artifacts() {
    let h = Harness::new();
    let store = h.store();
    run_write(&store, WriteOptions::default(), |tx| {
        let d = draft(tx, "abandoned")?;
        let a = capture(tx, &d, 0)?;
        let b = capture(tx, &d, 1)?;
        let d = tx
            .conventions()
            .convention_import(&d.operation_id)?
            .unwrap();
        tx.conventions().set_convention_import_phase(
            &d.operation_id,
            d.revision,
            "Cancelled",
            None,
            None,
            None,
        )?;
        tx.operations().transition(
            &d.operation_id,
            1,
            None,
            OperationState::CancelledBeforeDispatch,
            None,
        )?;
        assert!(
            !tx.conventions()
                .discard_convention_import_artifacts(&d.operation_id, 1)?
        );
        assert!(
            tx.conventions()
                .discard_convention_import_artifacts(&d.operation_id, 1)?
        );
        for row in [a, b] {
            assert!(tx.artifacts().artifact(&row.artifact.id)?.is_none());
            assert_eq!(tx.artifacts().live_references(&row.version.blob_digest)?, 0);
        }
        assert!(tx.artifacts().artifact(&id("abandoned-source"))?.is_some());
        Ok(())
    })
    .unwrap();
}
