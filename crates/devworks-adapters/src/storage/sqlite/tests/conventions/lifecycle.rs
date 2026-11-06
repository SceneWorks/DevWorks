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
        tx.conventions()
            .stage_convention_chunks(&first.generation_id, &[chunk(&first, 0, "alpha", 1, None)])?;
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
        tx.conventions().stage_convention_chunks(
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
        read(&store, |r| r
            .conventions()
            .convention_index_head(&owner.id, None))
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
fn resource_index_survives_body_only_edit_but_title_and_resource_retirement_invalidate() {
    let h = Harness::new();
    let store = h.store();
    let (owner, attachment, res_index) = run_write(&store, WriteOptions::default(), |tx| {
        let scope = KnowledgeScope::Workspace;
        let owner = record(
            "rules",
            scope.clone(),
            Some(artifact(tx, "body", &scope, "alpha")?),
        );
        tx.conventions().create_convention(&owner)?;
        let attachment = StoredConventionResource {
            id: id("attachment"),
            convention_id: owner.id.clone(),
            revision: 1,
            convention_revision: 1,
            name: "Resource".into(),
            media_type: "text/markdown".into(),
            content_ref: artifact(tx, "resource", &scope, "beta")?,
            author_id: tx.store().host_actor.clone(),
            created_at_ms: T0,
            deleted_at_ms: None,
            lifecycle: "Active".into(),
        };
        tx.conventions().add_convention_resource(&attachment, 1)?;
        let mut generation = index(tx, &owner, "resource-index", None)?;
        generation.resource_id = Some(attachment.id.clone());
        generation.resource_revision = Some(1);
        generation.source_digest = attachment.content_ref["sha256"].as_str().unwrap().into();
        tx.conventions().stage_convention_index(&generation)?;
        tx.conventions().stage_convention_chunks(
            &generation.generation_id,
            &[chunk(&generation, 0, "beta", 1, None)],
        )?;
        tx.conventions()
            .publish_convention_index(&generation.generation_id, 1, None, 1)?;
        Ok((owner, attachment, generation))
    })
    .unwrap();
    let q = query(vec![KnowledgeScope::Workspace]);
    let body_changed = run_write(&store, WriteOptions::default(), {
        let owner = owner.clone();
        move |tx| {
            let mut row = next(&owner);
            row.content_ref = Some(artifact(tx, "new-body", &row.scope, "gamma")?);
            tx.conventions().revise_convention(&row, 1)?;
            Ok(row)
        }
    })
    .unwrap();
    assert_eq!(
        read(&store, |r| r
            .conventions()
            .convention_lexical_statistics(&q, &["beta".into()]))
        .documents,
        1
    );
    let title_changed = run_write(&store, WriteOptions::default(), move |tx| {
        let mut row = next(&body_changed);
        row.title = "New breadcrumb title".into();
        tx.conventions().revise_convention(&row, 2)?;
        Ok(row)
    })
    .unwrap();
    assert_eq!(
        read(&store, |r| r
            .conventions()
            .convention_lexical_statistics(&q, &["beta".into()]))
        .documents,
        0
    );
    let retired = run_write(&store, WriteOptions::default(), {
        let attachment = attachment.clone();
        let old_index = res_index.clone();
        move |tx| {
            let mut new_index = index(tx, &title_changed, "resource-index-new", None)?;
            new_index.resource_id = Some(attachment.id.clone());
            new_index.resource_revision = Some(1);
            new_index.source_digest = attachment.content_ref["sha256"].as_str().unwrap().into();
            tx.conventions().stage_convention_index(&new_index)?;
            tx.conventions().stage_convention_chunks(
                &new_index.generation_id,
                &[chunk(&new_index, 0, "beta", 1, None)],
            )?;
            tx.conventions().publish_convention_index(
                &new_index.generation_id,
                3,
                Some(&old_index.generation_id),
                1,
            )?;
            let row = next(&title_changed);
            tx.conventions().revise_convention(&row, 3)?;
            let mut resource = attachment;
            resource.revision = 2;
            resource.convention_revision = 4;
            resource.lifecycle = "Retired".into();
            tx.conventions()
                .revise_convention_resource(&resource, 1, 4)?;
            Ok(resource)
        }
    })
    .unwrap();
    assert_eq!(
        read(&store, |r| r
            .conventions()
            .convention_lexical_statistics(&q, &["beta".into()]))
        .documents,
        0
    );
    let history = ConventionResourceQuery {
        scopes: q.scopes,
        convention_id: Some(owner.id),
        convention_revision: Some(3),
        lifecycles: vec!["Active".into()],
    };
    assert_eq!(
        read(&store, |r| r
            .conventions()
            .convention_resources(&history, None, 500)),
        vec![attachment]
    );
    let current = ConventionResourceQuery {
        convention_revision: None,
        lifecycles: vec!["Retired".into()],
        ..history
    };
    assert_eq!(
        read(&store, |r| r
            .conventions()
            .convention_resources(&current, None, 500)),
        vec![retired]
    );
}
#[test]
fn breadcrumb_artifacts_bind_full_unicode_and_nul_bytes_to_exact_generation_at_publication() {
    let h = Harness::new();
    let store = h.store();
    run_write(&store,WriteOptions::default(),|tx| {
        let scope=KnowledgeScope::Workspace;let owner=record("rules",scope.clone(),Some(artifact(tx,"body",&scope,"alpha")?));tx.conventions().create_convention(&owner)?;
        let generation=index(tx,&owner,"path-generation",None)?;tx.conventions().stage_convention_index(&generation)?;let mut row=chunk(&generation,0,"alpha",1,None);row.section_path=format!("{}\0", "λ".repeat(65_537));
        assert!(tx.conventions().stage_convention_chunks(&generation.generation_id,&[row.clone()]).is_err(),"illegal inline path needs actual artifact fallback");
        let marker=json!({"principalKind":"Host","producedBy":"host","derived":{"kind":"ConventionSectionPath","generationId":generation.generation_id,"chunkId":row.id,"conventionId":owner.id,"operationId":generation.operation_id,"sourceDigest":generation.source_digest}});
        row.section_path_ref=Some(artifact_with_provenance(tx,"section-path",&scope,&row.section_path,marker)?);
        tx.conventions().stage_convention_chunks(&generation.generation_id,&[row.clone()])?;
        let saved=tx.conventions().convention_chunks(&generation.generation_id,None,1)?.remove(0);assert_eq!(saved.section_path,row.section_path);assert_eq!(saved.section_path_ref,row.section_path_ref);
        tx.artifacts().tombstone_version(&id("version-section-path"),T0+1)?;assert!(tx.conventions().publish_convention_index(&generation.generation_id,1,None,1).is_err(),"publication rechecks exact path artifact backing");assert!(tx.conventions().convention_index_head(&owner.id,None)?.is_none());Ok(())
    }).unwrap();
}
