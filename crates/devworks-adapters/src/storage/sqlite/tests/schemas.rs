//! Real metadata storage: visibility before paging, CAS, hidden staging and immutable witnesses.
use super::{Harness, T0};
use crate::storage::sqlite::{MIGRATION_0034_SHA256, MIGRATIONS};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id, OperationId, Revision, records::*};
use serde_json::json;
fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn source(name: &str, shared: bool) -> StoredSchemaSource {
    StoredSchemaSource{connection:serde_json::from_value(json!({"id":name,"revision":"1","name":name,"engine":"Sqlite","state":"Enabled","writesEnabled":false,"metadataOnly":true,"ownerId":"host","config":{"name":name,"engine":"Sqlite","endpoint":"/tmp/metadata-fixture.sqlite","catalogs":[],"schemas":[],"includePatterns":[],"excludePatterns":[],"writesEnabled":false,"enabled":true,"workspaceExposed":shared,"projectAccess":[{"projectId":"metadata-project","metadataEnabled":shared,"dataReadEnabled":false,"dataWriteEnabled":false}],"exposureMode":"SchemaOnly","authentication":"File"}})).unwrap(),owner_id:ActorId(id("host")),deleted:false,created_at_ms:T0,updated_at_ms:T0}
}
#[test]
fn source_visibility_precedes_keyset_limit_and_revision_cas_preserves_identity() {
    let h = Harness::new();
    let s = h.store();
    run_write(&s, WriteOptions::default(), |tx| {
        for name in ["a-private", "b-shared", "c-private", "d-shared"] {
            tx.schemas()
                .create_schema_source(&source(name, name.ends_with("shared")))?;
        }
        Ok(())
    })
    .unwrap();
    let project = SchemaVisibility {
        actor_id: ActorId(id("project-principal")),
        workspace: false,
        project_id: Some(id("metadata-project")),
        exposed_only: true,
    };
    let first = read_snapshot(&s, &ReadOptions::default(), |r| {
        r.schemas().schema_sources(&project, None, 1)
    })
    .unwrap();
    assert_eq!(first[0].connection.id, id("b-shared"));
    let second = read_snapshot(&s, &ReadOptions::default(), |r| {
        r.schemas()
            .schema_sources(&project, Some(&first[0].connection.id), 1)
    })
    .unwrap();
    assert_eq!(second[0].connection.id, id("d-shared"));
    let owner = SchemaVisibility {
        actor_id: ActorId(id("host")),
        workspace: true,
        project_id: None,
        exposed_only: false,
    };
    assert_eq!(
        read_snapshot(&s, &ReadOptions::default(), |r| r
            .schemas()
            .schema_sources(&owner, None, 500))
        .unwrap()
        .len(),
        4
    );
    let before = read_snapshot(&s, &ReadOptions::default(), |r| {
        r.schemas().schema_inventory_revision()
    })
    .unwrap();
    let mut changed = first[0].clone();
    changed.connection.revision = Revision::new(2).unwrap();
    changed.connection.config.as_mut().unwrap().project_access =
        Some(serde_json::from_value(json!([])).unwrap());
    let saved = changed.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.schemas().revise_schema_source(&saved, 1)
    })
    .unwrap();
    assert!(matches!(
        run_write(&s, WriteOptions::default(), move |tx| tx
            .schemas()
            .revise_schema_source(&changed, 1)),
        Err(StorageError::RevisionConflict(_))
    ));
    assert!(
        read_snapshot(&s, &ReadOptions::default(), |r| r
            .schemas()
            .schema_inventory_revision())
        .unwrap()
            > before
    );
    let remaining = read_snapshot(&s, &ReadOptions::default(), |r| {
        r.schemas().schema_sources(&project, None, 500)
    })
    .unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].connection.id, id("d-shared"));
}
#[test]
fn hidden_staging_seals_atomically_and_exact_publication_survives_index_removal() {
    let h = Harness::new();
    let s = h.store();
    let view = id("schema-view");
    let op = OperationId(id("schema-operation"));
    let source_id = id("schema-source");
    let op_for_create = op.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.schemas()
            .create_schema_source(&source("schema-source", false))?;
        tx.operations().record_intent(NewOperation {
            id: op_for_create.clone(),
            root_id: None,
            assignment_id: None,
            capability: "schemas.index".into(),
            schema_version: 5,
            args_digest: sha256_hex(b"metadata-fixture"),
            payload: json!({"recordedFixture":true}),
        })?;
        tx.schemas().create_schema_view(&StoredSchemaView {
            id: id("schema-view"),
            connection_id: id("schema-source"),
            source_revision: 1,
            operation_id: op_for_create,
            digest: sha256_hex(b"metadata-only"),
            sealed: false,
            created_at_ms: T0,
        })
    })
    .unwrap();
    let object:SchemaObject=serde_json::from_value(json!({"id":"object-table","viewId":"schema-view","kind":"Table","qualifiedName":"fixture.public.parent","coverage":{"state":"Complete","gaps":[],"moreGaps":false},"metadata":{"name":"parent","catalog":"fixture","schema":"public","columnNames":[],"referencedColumns":[],"parameters":[],"definitionSql":"CREATE TABLE parent(id INT)"}})).unwrap();
    let v = view.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.schemas().append_schema_rows(&v, &[object], &[])
    })
    .unwrap();
    assert!(
        read_snapshot(&s, &ReadOptions::default(), |r| r
            .schemas()
            .schema_view(&view))
        .unwrap()
        .is_none()
    );
    assert!(
        read_snapshot(&s, &ReadOptions::default(), |r| r.schemas().schema_objects(
            &view,
            &SchemaObjectFilter::default(),
            None,
            500
        ))
        .unwrap()
        .is_empty()
    );
    let result:IndexRunRecord=serde_json::from_value(json!({"id":"schema-operation","revision":"1","repositoryIds":[],"stage":"CompletedMetadata","state":"ObservedComplete","completedUnits":"1","totalUnits":"1","outputViews":["schema-view"],"diagnostics":[]})).unwrap();
    let witness = StoredSchemaPublication {
        operation_id: op.clone(),
        connection_id: source_id.clone(),
        result,
        created_at_ms: T0,
    };
    let v = view.clone();
    let saved = witness.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.schemas().seal_schema_view(&v)?;
        tx.schemas().record_schema_publication(&saved)
    })
    .unwrap();
    assert_eq!(
        read_snapshot(&s, &ReadOptions::default(), |r| r.schemas().schema_objects(
            &view,
            &SchemaObjectFilter::default(),
            None,
            500
        ))
        .unwrap()
        .len(),
        1
    );
    let v = view.clone();
    assert!(
        run_write(&s, WriteOptions::default(), move |tx| tx
            .schemas()
            .append_schema_rows(&v, &[], &[]))
        .is_err()
    );
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.schemas().remove_schema_views(&source_id)
    })
    .unwrap();
    assert!(
        read_snapshot(&s, &ReadOptions::default(), |r| r
            .schemas()
            .schema_view(&view))
        .unwrap()
        .is_none()
    );
    let recovered = read_snapshot(&s, &ReadOptions::default(), |r| {
        r.schemas().schema_publication(&op)
    })
    .unwrap()
    .unwrap();
    assert_eq!(
        serde_json::to_value(recovered.result).unwrap(),
        serde_json::to_value(witness.result).unwrap()
    );
    assert_eq!(
        sha256_hex(
            MIGRATIONS
                .iter()
                .find(|v| v.version == 34)
                .unwrap()
                .sql
                .as_bytes()
        ),
        MIGRATION_0034_SHA256
    );
}

fn schema_context(s: &super::SqliteStore, who: &str) -> devworks_core::context::InvocationContext {
    use devworks_core::context::{HostIngress, PrincipalKind};
    let info = s.info();
    HostIngress::new(s).context(
        ActorId(id(who)),
        PrincipalKind::Human,
        id("schema-ui"),
        id("native-ui"),
        devworks_core::protocol::Counter::new(info.restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    )
}
fn schema_query(
    h: &Harness,
    s: &super::SqliteStore,
    who: &str,
    operation: &str,
    mut body: serde_json::Value,
) -> devworks_core::protocol::results::DomainQueryReply {
    let authority = devworks_core::authority::HostAuthority {
        store: s,
        clock: &*h.clock,
        protected_branches: &[],
    };
    let gateway = devworks_core::gateway::Gateway {
        authority: &authority,
        store: s,
        ids: &*h.ids,
    };
    let service =
        devworks_core::services::schemas::SchemaService::new(h.ids.clone(), h.clock.clone());
    body["kind"] = json!(operation);
    let frame = json!({"protocol":"1.0-draft.1","mode":"Query","schemaRevision":5,"queryId":"schema-query","body":body});
    gateway
        .query(
            &schema_context(s, who),
            operation,
            frame.to_string().as_bytes(),
            &service,
        )
        .unwrap()
}
fn schema_command(
    h: &Harness,
    s: &super::SqliteStore,
    who: &str,
    key: &str,
    operation: &str,
    mut body: serde_json::Value,
) -> devworks_core::protocol::results::DomainDecision {
    let authority = devworks_core::authority::HostAuthority {
        store: s,
        clock: &*h.clock,
        protected_branches: &[],
    };
    let gateway = devworks_core::gateway::Gateway {
        authority: &authority,
        store: s,
        ids: &*h.ids,
    };
    let service = std::sync::Arc::new(devworks_core::services::schemas::SchemaService::new(
        h.ids.clone(),
        h.clock.clone(),
    ));
    body["kind"] = json!(operation);
    let frame = json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":5,"requestKey":key,"body":body});
    match gateway
        .command(
            &schema_context(s, who),
            operation,
            frame.to_string().as_bytes(),
            service,
        )
        .unwrap()
    {
        devworks_core::protocol::CommandReply::Recorded { receipt, .. } => receipt.decision,
        other => panic!("expected durable receipt: {other:?}"),
    }
}
fn schema_seed_humans(h: &Harness, s: &super::SqliteStore) {
    run_write(s, WriteOptions::default(), |tx| {
        for who in ["schema-owner", "schema-other"] {
            tx.actors().register(
                &ActorId(id(who)),
                devworks_core::context::PrincipalKind::Human,
                who,
            )?;
        }
        Ok(())
    })
    .unwrap();
    let raw = h.raw();
    for p in ["metadata-project", "other-project"] {
        raw.execute("INSERT INTO projects(id,title,lifecycle,revision,scope_revision,created_at) VALUES(?1,?1,'Active',1,1,?2)", rusqlite::params![p,T0]).unwrap();
    }
}
#[test]
fn native_gateway_source_configuration_replays_and_refuses_stale_or_foreign_edits() {
    use devworks_core::protocol::results::{DomainDecision, OperationResult};
    let h = Harness::new();
    let s = h.store();
    schema_seed_humans(&h, &s);
    let config = serde_json::to_value(source("draft", true).connection.config.unwrap()).unwrap();
    let body = json!({"scope":{"kind":"Workspace"},"config":config});
    let made = schema_command(
        &h,
        &s,
        "schema-owner",
        "create",
        "schemas.configure",
        body.clone(),
    );
    let DomainDecision::Applied {
        result: OperationResult::SchemasConfigure { value: created },
    } = made
    else {
        panic!("{made:?}")
    };
    assert_eq!(created.revision.get(), 1);
    assert!(created.metadata_only);
    let replay = schema_command(&h, &s, "schema-owner", "create", "schemas.configure", body);
    assert!(
        matches!(replay, DomainDecision::Applied { result: OperationResult::SchemasConfigure { value } } if value.id == created.id)
    );
    let mut edit = json!({"scope":{"kind":"Workspace"},"connectionId":created.id,"expectedRevision":"1","config":config});
    let foreign = schema_command(
        &h,
        &s,
        "schema-other",
        "foreign-edit",
        "schemas.configure",
        edit.clone(),
    );
    assert!(matches!(foreign, DomainDecision::Refused { .. }));
    edit["config"]["name"] = json!("Changed source");
    let changed = schema_command(
        &h,
        &s,
        "schema-owner",
        "edit",
        "schemas.configure",
        edit.clone(),
    );
    assert!(
        matches!(changed, DomainDecision::Applied { result: OperationResult::SchemasConfigure { value } } if value.revision.get() == 2 && value.name.0 == "Changed source")
    );
    let stale = schema_command(
        &h,
        &s,
        "schema-owner",
        "stale-edit",
        "schemas.configure",
        edit,
    );
    assert!(matches!(stale, DomainDecision::Refused { .. }));
    assert_eq!(
        read_snapshot(&s, &ReadOptions::default(), |r| r.schemas().schema_sources(
            &SchemaVisibility {
                actor_id: ActorId(id("schema-owner")),
                workspace: true,
                project_id: None,
                exposed_only: false
            },
            None,
            500
        ))
        .unwrap()
        .len(),
        1
    );
}
#[test]
fn native_gateway_shared_pages_redact_other_projects_and_expire_after_access_change() {
    use devworks_core::protocol::results::{DomainQueryReply, OperationResult};
    let h = Harness::new();
    let s = h.store();
    schema_seed_humans(&h, &s);
    run_write(&s, WriteOptions::default(), |tx| {
        for name in ["a-private", "b-shared", "c-private", "d-shared"] {
            let mut row = source(name, name.ends_with("shared"));
            if name.ends_with("shared") {
                row.connection.credential_ref = Some(id("private-password-handle"));
                let config = row.connection.config.as_mut().unwrap();
                config.credential_ref = Some(id("private-password-handle"));
                config.username = Some(serde_json::from_value(json!("private-login")).unwrap());
                config.project_access.as_mut().unwrap().0.push(serde_json::from_value(json!({"projectId":"other-project","metadataEnabled":true,"dataReadEnabled":true,"dataWriteEnabled":false})).unwrap());
            }
            tx.schemas().create_schema_source(&row)?;
        }
        Ok(())
    }).unwrap();
    let body =
        json!({"scope":{"kind":"Project","projectId":"metadata-project"},"page":{"limit":1}});
    let first = schema_query(&h, &s, "schema-other", "schemas.connections", body.clone());
    let DomainQueryReply::Result {
        result: OperationResult::SchemasConnections { value: first },
        ..
    } = first
    else {
        panic!("{first:?}")
    };
    assert_eq!(first.items.0[0].id, id("b-shared"));
    assert!(first.has_more);
    let record = &first.items.0[0];
    assert!(record.credential_ref.is_none());
    let config = record.config.as_ref().unwrap();
    assert!(config.credential_ref.is_none() && config.username.is_none());
    assert_eq!(config.project_access.as_ref().unwrap().0.len(), 1);
    assert_eq!(
        config.project_access.as_ref().unwrap().0[0].project_id,
        id("metadata-project")
    );
    let mut next = body.clone();
    next["page"]["cursor"] = json!(first.next_cursor.unwrap());
    let second = schema_query(&h, &s, "schema-other", "schemas.connections", next.clone());
    assert!(
        matches!(second, DomainQueryReply::Result { result: OperationResult::SchemasConnections { value }, .. } if !value.has_more && value.items.0[0].id == id("d-shared"))
    );
    run_write(&s, WriteOptions::default(), |tx| {
        let mut row = tx.schemas().schema_source(&id("d-shared"))?.unwrap();
        row.connection.revision = Revision::new(2).unwrap();
        row.connection.config.as_mut().unwrap().project_access =
            Some(serde_json::from_value(json!([])).unwrap());
        tx.schemas().revise_schema_source(&row, 1)
    })
    .unwrap();
    let stale = schema_query(&h, &s, "schema-other", "schemas.connections", next);
    assert!(
        matches!(stale, DomainQueryReply::Rejected { fault, .. } if fault.code == devworks_core::protocol::DomainErrorCode::CursorExpired)
    );
    let unpublished = schema_query(
        &h,
        &s,
        "schema-other",
        "schemas.objects",
        json!({"scope":{"kind":"Project","projectId":"metadata-project"},"views":{"kind":"SchemaPublished","connectionIds":["b-shared"]},"connectionId":"b-shared","kinds":[],"page":{"limit":10}}),
    );
    assert!(
        matches!(unpublished, DomainQueryReply::Rejected { fault, .. } if fault.code == devworks_core::protocol::DomainErrorCode::ViewUnavailable)
    );
}

#[test]
fn native_password_intake_binds_human_and_purpose_and_reconciles_only_unused_handles() {
    use devworks_core::{
        ports::secrets::{Secret, SecretKey, SecretStore, SessionSecrets},
        protocol::{
            CommandReply,
            results::{DomainDecision, OperationResult},
        },
        services::schemas::SchemaService,
    };
    let h = Harness::new();
    let s = std::sync::Arc::new(h.store());
    schema_seed_humans(&h, &s);
    let secrets = std::sync::Arc::new(SessionSecrets::new());
    let service = std::sync::Arc::new(
        SchemaService::new(h.ids.clone(), h.clock.clone())
            .with_credentials(s.clone(), secrets.clone()),
    );
    assert_eq!(service.reconcile_credential_orphans().unwrap(), 0);
    let owner = schema_context(&s, "schema-owner");
    let other = schema_context(&s, "schema-other");
    let password = "  exact metadata passphrase fixture  ";
    let handle = service
        .stage_password(&owner, Secret::new(password))
        .unwrap();
    let foreign = service
        .stage_password(&other, Secret::new("other fixture credential"))
        .unwrap();
    assert!(service.discard_password(&other, &handle).is_err());
    let discarded = service
        .stage_password(&owner, Secret::new("cancelled unsubmitted fixture"))
        .unwrap();
    service.discard_password(&owner, &discarded).unwrap();
    let namespace = format!("{}:schema-sources", s.info().store_id.0.as_str());
    assert!(
        secrets
            .get(&SecretKey::new(namespace.clone(), discarded))
            .unwrap()
            .is_none()
    );
    let authority = devworks_core::authority::HostAuthority {
        store: &*s,
        clock: &*h.clock,
        protected_branches: &[],
    };
    let gateway = devworks_core::gateway::Gateway {
        authority: &authority,
        store: &*s,
        ids: &*h.ids,
    };
    let mut config =
        serde_json::to_value(source("password-source", false).connection.config.unwrap()).unwrap();
    config["engine"] = json!("MySql");
    config["endpoint"] = json!("mysql://127.0.0.1:1/metadata_fixture");
    config["authentication"] = json!("Password");
    config["username"] = json!("metadata_reader");
    let command = |key: &str, handle: &Id| {
        let mut config = config.clone();
        config["credentialRef"] = json!(handle);
        let frame = json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":5,"requestKey":key,"body":{"kind":"schemas.configure","scope":{"kind":"Workspace"},"config":config}});
        match gateway
            .command(
                &owner,
                "schemas.configure",
                frame.to_string().as_bytes(),
                service.clone(),
            )
            .unwrap()
        {
            CommandReply::Recorded { receipt, .. } => receipt.decision,
            other => panic!("{other:?}"),
        }
    };
    assert!(matches!(
        command("foreign-password", &foreign),
        DomainDecision::Refused { .. }
    ));
    let router_handle = id("foreign-subsystem-handle");
    let router_key = SecretKey::new(s.info().store_id.0.as_str(), router_handle.clone());
    secrets
        .put(&router_key, &Secret::new("foreign subsystem fixture"))
        .unwrap();
    assert!(matches!(
        command("foreign-purpose", &router_handle),
        DomainDecision::Refused { .. }
    ));
    let created = command("password-create", &handle);
    assert!(
        matches!(created, DomainDecision::Applied { result: OperationResult::SchemasConfigure { value } } if value.credential_ref.as_ref() == Some(&handle))
    );
    assert!(service.discard_password(&owner, &handle).is_err());
    assert_eq!(
        secrets
            .get(&SecretKey::new(namespace.clone(), handle.clone()))
            .unwrap()
            .unwrap()
            .expose(),
        password
    );
    let text: String = h
        .raw()
        .query_row(
            "SELECT group_concat(body_json) FROM schema_sources",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!text.contains(password));
    assert!(
        service.reconcile_credential_orphans().is_err(),
        "startup cleanup is closed before command publication can race it"
    );
    drop(service);
    let restarted = SchemaService::new(h.ids.clone(), h.clock.clone())
        .with_credentials(s.clone(), secrets.clone());
    assert_eq!(restarted.reconcile_credential_orphans().unwrap(), 1);
    assert!(
        secrets
            .get(&SecretKey::new(namespace.clone(), foreign))
            .unwrap()
            .is_none()
    );
    assert!(
        secrets
            .get(&SecretKey::new(namespace, handle))
            .unwrap()
            .is_some()
    );
    assert!(secrets.get(&router_key).unwrap().is_some());
}
