//! Real retained bytes, gateway decisions and transaction rollback; no external providers.
use super::*;
use devworks_core::protocol::{DomainErrorCode, ProjectId};

fn project(f: &Fx, name: &str) -> ProjectId {
    let project = ProjectId(id(name));
    let p = project.clone();
    let name = name.to_owned();
    write(&*f.store, move |tx| {
        tx.projects().insert_project(&StoredProject {
            id: p,
            name,
            description: String::new(),
            lifecycle: ACTIVE.into(),
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 0,
            archive_stop_pending: false,
        })
    })
    .unwrap();
    project
}
fn limit(f: &Fx, project: &ProjectId, bytes: i64) {
    let scope = ConfigScope::Project(project.clone());
    write(&*f.store, move |tx| {
        let key = devworks_core::settings::catalog::ARTIFACT_QUOTA_KEY;
        let schema = devworks_core::settings::catalog::descriptor(key)
            .unwrap()
            .schema_ref();
        let expected = tx
            .configuration()
            .current(&scope, "settings")?
            .map_or(1, |r| r.revision);
        tx.configuration().append_revision(
            &scope,
            "settings",
            expected,
            1,
            json!({"values":{key:{"schema":schema,"value":bytes}}}),
        )?;
        Ok(())
    })
    .unwrap();
}
fn import(f: &Fx, slug: &str, parent: &[&str], body: &str, resources: usize) -> StoredConvention {
    let mut value = manifest(resources);
    value["slug"] = json!(slug);
    value["parentPath"] = json!(parent);
    value["markdown"] = json!({"text":body,"byteLength":body.len().to_string(),"sha256":devworks_core::fingerprint::sha256_hex(body.as_bytes())});
    let input = f.source(&serde_json::to_vec(&value).unwrap());
    let operation = f.admit(&input, &format!("import-{slug}"));
    let completed = f.dispatch(&operation);
    assert_eq!(
        completed.state,
        OperationState::ObservedComplete,
        "{completed:?}"
    );
    read(&*f.store, |r| {
        let draft = r.conventions().convention_import(&operation)?.unwrap();
        r.conventions().convention(
            &draft.convention_id,
            None,
            &[KnowledgeScope::Project(f.ctx.project_id().unwrap().clone())],
        )
    })
    .unwrap()
}
fn command(f: &Fx, key: &str, body: serde_json::Value) -> DomainDecision {
    let authority = HostAuthority {
        store: &*f.store,
        clock: &*f.h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        store: &*f.store,
        authority: &authority,
        ids: &*f.h.ids,
    };
    let operation = body["kind"].as_str().unwrap();
    let frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,"mode":"Command","requestKey":key,"body":body});
    match UseCases::new()
        .with_conventions(f.catalog.clone())
        .command(&gateway, &f.ctx, operation, frame.to_string().as_bytes())
        .unwrap()
    {
        CommandReply::Recorded { receipt, .. } => receipt.decision,
        other => panic!("expected actual durable decision: {other:?}"),
    }
}
fn capacity(decision: DomainDecision, destination: &ProjectId) {
    let DomainDecision::Refused { fault } = decision else {
        panic!("destination quota must refuse retention: {decision:?}");
    };
    assert_eq!(fault.code, DomainErrorCode::CapacityWait, "{fault:?}");
    assert_eq!(fault.target_id, Some(destination.0.clone()));
}
#[derive(Debug, PartialEq)]
struct RetainedState {
    conventions: Vec<StoredConvention>,
    resources: Vec<StoredConventionResource>,
    artifacts: [Vec<StoredArtifact>; 2],
    usage: [i64; 2],
    revisions: [i64; 2],
    index_revisions: [i64; 2],
}
fn retained(f: &Fx, source: &ProjectId, destination: &ProjectId) -> RetainedState {
    read(&*f.store, |r| {
        let scopes = vec![
            KnowledgeScope::Project(source.clone()),
            KnowledgeScope::Project(destination.clone()),
        ];
        Ok(RetainedState {
            conventions: r.conventions().conventions(
                &ConventionQuery {
                    scopes: scopes.clone(),
                    ..Default::default()
                },
                None,
                500,
            )?,
            resources: r.conventions().convention_resources(
                &ConventionResourceQuery {
                    scopes: scopes.clone(),
                    convention_id: None,
                    convention_revision: None,
                    lifecycles: vec!["Active".into(), "Retired".into(), "Deleted".into()],
                },
                None,
                500,
            )?,
            artifacts: [
                r.artifacts().list(source, &[], None, 500)?,
                r.artifacts().list(destination, &[], None, 500)?,
            ],
            usage: [
                r.artifacts().project_usage(source)?,
                r.artifacts().project_usage(destination)?,
            ],
            revisions: [
                r.conventions().convention_scope_revision(&scopes[0])?,
                r.conventions().convention_scope_revision(&scopes[1])?,
            ],
            index_revisions: [
                r.conventions()
                    .convention_index_source_revision(&scopes[0])?,
                r.conventions()
                    .convention_index_source_revision(&scopes[1])?,
            ],
        })
    })
}
fn move_body(owner: &StoredConvention, destination: &ProjectId) -> serde_json::Value {
    json!({"kind":"conventions.move","conventionId":owner.id,"expectedRevision":owner.revision.to_string(),
        "scope":devworks_core::services::conventions::model::scope(&owner.scope),
        "destination":{"kind":"Project","projectId":destination}})
}

#[test]
fn destination_quota_rolls_back_a_cross_project_subtree_and_resources_after_a_copied_prefix() {
    let mut f = Fx::new();
    let source = project(&f, "quota-source");
    let destination = project(&f, "quota-destination");
    f.ctx = f.ctx.with_lineage(Some(source.clone()), None, None);
    let owner = import(&f, "parent", &[], "# Actual\n\0body", 1);
    let child = import(
        &f,
        "child",
        &["parent"],
        "# Child\nadditional distinct body",
        1,
    );
    // Root body (14) and attachment (11) fit. The later child must refuse after the
    // root owner/resources have already been tentatively revised inside this same UoW.
    limit(&f, &destination, 25);
    f.ctx = f.ctx.with_lineage(None, None, None);
    let before = retained(&f, &source, &destination);
    assert_eq!(before.conventions.len(), 2);
    assert_eq!(before.resources.len(), 2);
    assert!(before.artifacts[1].is_empty());
    capacity(
        command(&f, "quota-subtree-move", move_body(&owner, &destination)),
        &destination,
    );
    assert_eq!(
        retained(&f, &source, &destination),
        before,
        "no copied artifact, owner/resource revision, scope bump or source mutation may survive"
    );
    assert_eq!(
        read(&*f.store, |r| r.conventions().convention(
            &child.id,
            None,
            &[KnowledgeScope::Project(source.clone())]
        ))
        .unwrap(),
        child
    );
}

#[test]
fn foreign_scope_attachment_uses_destination_quota_and_duplicate_digest_costs_no_more_bytes() {
    let mut f = Fx::new();
    let source = project(&f, "upload-source");
    let destination = project(&f, "attachment-destination");
    f.ctx = f.ctx.with_lineage(Some(destination.clone()), None, None);
    let body = "# Existing destination body";
    let owner = import(&f, "attachment-owner", &[], body, 0);
    f.ctx = f.ctx.with_lineage(Some(source.clone()), None, None);
    let upload = f.source(b"new foreign attachment bytes");
    let duplicate = f.source(body.as_bytes());
    // The real workspace Human can see this project upload. Its unrelated workspace
    // configuration is unlimited; the target project's current configuration must win.
    f.ctx = f.ctx.with_lineage(None, None, None);
    let usage = read(&*f.store, |r| r.artifacts().project_usage(&destination));
    limit(&f, &destination, usage + upload.byte_length.get() - 1);
    let before = retained(&f, &source, &destination);
    let edit = |content: &ArtifactRef| {
        json!({"kind":"conventions.edit","scope":{"kind":"Project","projectId":destination},
        "conventionId":owner.id,"expectedRevision":owner.revision.to_string(),"content":owner.content_ref,"title":"Changed only if the full edit commits",
        "resources":[{"kind":"Attach","name":"Foreign upload","content":content}]})
    };
    capacity(
        command(&f, "quota-foreign-attach", edit(&upload)),
        &destination,
    );
    assert_eq!(
        retained(&f, &source, &destination),
        before,
        "failed attachment must preserve owner, uploads and all references"
    );
    limit(&f, &destination, usage);
    let duplicate_before = read(&*f.store, |r| {
        r.artifacts().artifact(&duplicate.artifact_id)
    })
    .unwrap();
    assert!(matches!(
        command(&f, "quota-duplicate-attach", edit(&duplicate)),
        DomainDecision::Applied { .. }
    ));
    assert_eq!(
        read(&*f.store, |r| r.artifacts().project_usage(&destination)),
        usage
    );
    let after = retained(&f, &source, &destination);
    assert_eq!(after.resources.len(), 1);
    assert_eq!(
        after.resources[0].content_ref["sha256"],
        json!(duplicate.sha256)
    );
    assert_ne!(
        after.resources[0].content_ref["artifactId"],
        json!(duplicate.artifact_id)
    );
    assert_eq!(
        read(&*f.store, |r| r
            .artifacts()
            .artifact(&duplicate.artifact_id))
        .unwrap(),
        duplicate_before
    );
}

#[test]
fn cross_project_move_reuses_destination_distinct_blobs_at_exact_quota() {
    let mut f = Fx::new();
    let source = project(&f, "duplicate-move-source");
    let destination = project(&f, "duplicate-move-destination");
    f.ctx = f.ctx.with_lineage(Some(source.clone()), None, None);
    let body = "# Actual\n\0body";
    let owner = import(&f, "duplicate-move", &[], body, 1);
    f.ctx = f.ctx.with_lineage(Some(destination.clone()), None, None);
    f.source(body.as_bytes());
    f.source(b"\0\x01\xffresource");
    let usage = read(&*f.store, |r| r.artifacts().project_usage(&destination));
    assert_eq!(usage, 25);
    limit(&f, &destination, usage);
    f.ctx = f.ctx.with_lineage(None, None, None);
    let decision = command(&f, "quota-duplicate-move", move_body(&owner, &destination));
    assert!(
        matches!(decision, DomainDecision::Applied { .. }),
        "{decision:?}"
    );
    assert_eq!(
        read(&*f.store, |r| r.artifacts().project_usage(&destination)),
        usage
    );
    let moved = read(&*f.store, |r| {
        r.conventions().convention(
            &owner.id,
            None,
            &[KnowledgeScope::Project(destination.clone())],
        )
    })
    .unwrap();
    assert_eq!(moved.revision, owner.revision + 1);
    assert_ne!(moved.content_ref, owner.content_ref);
    assert_eq!(
        moved.content_ref.as_ref().unwrap()["sha256"],
        owner.content_ref.as_ref().unwrap()["sha256"]
    );
    let state = retained(&f, &source, &destination);
    assert_eq!(state.resources.len(), 1);
    let resource: ArtifactRef =
        serde_json::from_value(state.resources[0].content_ref.clone()).unwrap();
    assert_eq!(
        read(&*f.store, |r| r.artifacts().artifact(&resource.artifact_id))
            .unwrap()
            .scope,
        ArtifactScope::Project(destination)
    );
}
