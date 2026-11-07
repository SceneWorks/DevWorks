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
    StoredSchemaSource{configuration_revision:1,connection:serde_json::from_value(json!({"id":name,"revision":"1","name":name,"engine":"Sqlite","state":"Enabled","writesEnabled":false,"metadataOnly":true,"ownerId":"host","config":{"name":name,"engine":"Sqlite","endpoint":"/tmp/metadata-fixture.sqlite","catalogs":[],"schemas":[],"includePatterns":[],"excludePatterns":[],"writesEnabled":false,"enabled":true,"workspaceExposed":shared,"projectAccess":[{"projectId":"metadata-project","metadataEnabled":shared,"dataReadEnabled":false,"dataWriteEnabled":false}],"exposureMode":"SchemaOnly","authentication":"File"}})).unwrap(),owner_id:ActorId(id("host")),deleted:false,created_at_ms:T0,updated_at_ms:T0}
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
    changed.configuration_revision += 1;
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
            configuration_revision: 1,
            coverage: serde_json::from_value(
                json!({"state":"Complete","gaps":[],"moreGaps":false}),
            )
            .unwrap(),
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
        row.configuration_revision += 1;
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

struct CountingMetadata(std::sync::atomic::AtomicUsize);
impl devworks_core::ports::database::MetadataReader for CountingMetadata {
    fn read(
        &self,
        request: devworks_core::ports::database::MetadataRequest,
    ) -> devworks_core::ports::database::MetadataFuture {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        devworks_core::ports::database::MetadataReader::read(
            &crate::database::NativeMetadataReader,
            request,
        )
    }
}
fn metadata_owner(
    h: &Harness,
    s: std::sync::Arc<super::SqliteStore>,
    reader: std::sync::Arc<dyn devworks_core::ports::database::MetadataReader>,
) -> std::sync::Arc<devworks_core::services::schemas::SchemaService> {
    let artifacts = std::sync::Arc::new(devworks_core::services::artifacts::ArtifactService::new(
        std::sync::Arc::new(crate::fs::artifacts::NativeArtifactFiles::new(
            h.root.0.clone(),
            h.root.0.join("cache"),
        )),
        std::sync::Arc::new(crate::fs::sources::HostArtifactSources::new(
            h.clock.clone(),
            h.ids.clone(),
        )),
        std::sync::Arc::new(crate::fs::NativeFilesystem::new()),
        h.clock.clone(),
        h.ids.clone(),
    ));
    std::sync::Arc::new(
        devworks_core::services::schemas::SchemaService::new(h.ids.clone(), h.clock.clone())
            .with_runtime(s, reader, artifacts),
    )
}
fn owner_command(
    h: &Harness,
    s: &super::SqliteStore,
    service: std::sync::Arc<devworks_core::services::schemas::SchemaService>,
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
    body["kind"] = json!(operation);
    let frame = json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":5,"requestKey":key,"body":body});
    match gateway
        .command(
            &schema_context(s, "schema-owner"),
            operation,
            frame.to_string().as_bytes(),
            service,
        )
        .unwrap()
    {
        devworks_core::protocol::CommandReply::Recorded { receipt, .. } => receipt.decision,
        other => panic!("{other:?}"),
    }
}
fn dispatch_metadata(
    s: &super::SqliteStore,
    operation: OperationId,
    unknown: bool,
) -> OperationRecord {
    run_write(s, WriteOptions::default(), move |tx| {
        let original = tx.operations().operation(&operation)?.unwrap();
        let a = tx.operations().begin_attempt(
            &operation,
            original.revision,
            AttemptBinding::default(),
        )?;
        let dispatched = tx.operations().transition(
            &operation,
            original.revision + 1,
            Some(&a.fence()),
            devworks_core::protocol::OperationState::Dispatching,
            None,
        )?;
        if unknown {
            tx.operations().transition(
                &operation,
                dispatched.revision,
                Some(&a.fence()),
                devworks_core::protocol::OperationState::OutcomeUnknown,
                None,
            )
        } else {
            Ok(dispatched)
        }
    })
    .unwrap()
}
#[test]
fn native_metadata_worker_recovers_staging_publishes_no_rows_and_observes_after_revocation() {
    use devworks_core::{
        protocol::results::{DomainDecision, OperationResult},
        services::schemas::indexing::SchemaOutcome,
    };
    let h = Harness::new();
    let s = std::sync::Arc::new(h.store());
    schema_seed_humans(&h, &s);
    let path = h.root.0.join("source-fixture.sqlite");
    rusqlite::Connection::open(&path).unwrap().execute_batch("CREATE TABLE parent(id INTEGER PRIMARY KEY, label TEXT DEFAULT 'catalog_default'); CREATE TABLE child(id INT, parent_id INT REFERENCES parent(id)); CREATE INDEX child_parent ON child(parent_id); INSERT INTO parent VALUES(1,'SOURCE_ROW_SENTINEL_NEVER_INDEX');").unwrap();
    let reader = std::sync::Arc::new(CountingMetadata(std::sync::atomic::AtomicUsize::new(0)));
    let owner = metadata_owner(&h, s.clone(), reader.clone());
    let mut config =
        serde_json::to_value(source("native-metadata", true).connection.config.unwrap()).unwrap();
    config["endpoint"] = json!(path.to_str().unwrap());
    let made = owner_command(
        &h,
        &s,
        owner.clone(),
        "worker-source",
        "schemas.configure",
        json!({"scope":{"kind":"Workspace"},"config":config}),
    );
    let DomainDecision::Applied {
        result: OperationResult::SchemasConfigure { value: source },
    } = made
    else {
        panic!("{made:?}")
    };
    let accepted = owner_command(
        &h,
        &s,
        owner.clone(),
        "worker-index",
        "schemas.index",
        json!({"connectionId":source.id,"expectedRevision":source.revision}),
    );
    let DomainDecision::Accepted { operation_id } = accepted else {
        panic!("{accepted:?}")
    };
    let operation = dispatch_metadata(&s, operation_id.clone(), true);
    let old = operation_id.clone();
    let connection = source.id.clone();
    run_write(&*s, WriteOptions::default(), move |tx| {
        tx.schemas().create_schema_view(&StoredSchemaView {
            id: id("abandoned-stage"),
            connection_id: connection,
            source_revision: 2,
            configuration_revision: 1,
            coverage: serde_json::from_value(
                json!({"state":"Complete","gaps":[],"moreGaps":false}),
            )
            .unwrap(),
            operation_id: old,
            digest: sha256_hex(b"abandoned-stage"),
            sealed: false,
            created_at_ms: T0,
        })
    })
    .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let completed = runtime
        .block_on(owner.run_metadata(&operation, &Cancellation::new()))
        .unwrap();
    let SchemaOutcome::Completed(result) = completed else {
        panic!("{completed:?}")
    };
    assert_eq!(reader.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    let current = read_snapshot(&*s, &ReadOptions::default(), |r| {
        r.schemas().schema_source(&source.id)
    })
    .unwrap()
    .unwrap();
    assert!(current.connection.active_operation_id.is_none());
    assert_eq!(current.connection.revision.get(), 3);
    let view = current.connection.current_view_id.clone().unwrap();
    let objects = read_snapshot(&*s, &ReadOptions::default(), |r| {
        r.schemas()
            .schema_objects(&view, &SchemaObjectFilter::default(), None, 500)
    })
    .unwrap();
    assert!(
        objects
            .iter()
            .any(|o| matches!(o.kind, MetadataObjectKind::Table))
    );
    assert!(
        objects
            .iter()
            .any(|o| matches!(o.kind, MetadataObjectKind::Column))
    );
    assert!(
        !serde_json::to_string(&objects)
            .unwrap()
            .contains("SOURCE_ROW_SENTINEL_NEVER_INDEX")
    );
    assert_eq!(
        h.raw()
            .query_row(
                "SELECT count(*) FROM schema_views WHERE sealed=0",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    let tested = owner_command(
        &h,
        &s,
        owner.clone(),
        "worker-test",
        "schemas.refresh",
        json!({"connectionId":source.id,"expectedRevision":current.connection.revision,"testOnly":true}),
    );
    let DomainDecision::Accepted {
        operation_id: test_id,
    } = tested
    else {
        panic!("{tested:?}")
    };
    let test_operation = dispatch_metadata(&s, test_id, false);
    let tested = runtime
        .block_on(owner.run_metadata(&test_operation, &Cancellation::new()))
        .unwrap();
    assert!(matches!(tested,SchemaOutcome::Completed(value) if value["outputViews"]==json!([])));
    let current = read_snapshot(&*s, &ReadOptions::default(), |r| {
        r.schemas().schema_source(&source.id)
    })
    .unwrap()
    .unwrap();
    assert_eq!(current.connection.current_view_id.as_ref(), Some(&view));
    assert_eq!(reader.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    // A source edit/removal cannot erase the immutable completion witness.
    let removed = owner_command(
        &h,
        &s,
        owner.clone(),
        "worker-remove",
        "schemas.configure",
        json!({"scope":{"kind":"Workspace"},"connectionId":source.id,"expectedRevision":current.connection.revision,"action":"Remove","config":config}),
    );
    assert!(matches!(removed, DomainDecision::Applied { .. }));
    let observed = runtime
        .block_on(owner.run_metadata(&operation, &Cancellation::new()))
        .unwrap();
    assert!(matches!(observed,SchemaOutcome::Completed(value) if value==result));
    assert_eq!(reader.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    owner.stop();
}

#[test]
fn native_metadata_source_revocation_cancels_queued_work_and_can_be_reenabled() {
    use devworks_core::protocol::results::{DomainDecision, OperationResult};
    let h = Harness::new();
    let s = std::sync::Arc::new(h.store());
    schema_seed_humans(&h, &s);
    let reader = std::sync::Arc::new(CountingMetadata(std::sync::atomic::AtomicUsize::new(0)));
    let owner = metadata_owner(&h, s.clone(), reader.clone());
    let config =
        serde_json::to_value(source("queued-metadata", true).connection.config.unwrap()).unwrap();
    let made = owner_command(
        &h,
        &s,
        owner.clone(),
        "queued-source",
        "schemas.configure",
        json!({"scope":{"kind":"Workspace"},"config":config}),
    );
    let DomainDecision::Applied {
        result: OperationResult::SchemasConfigure { value: source },
    } = made
    else {
        panic!("{made:?}")
    };
    let accepted = owner_command(
        &h,
        &s,
        owner.clone(),
        "queued-index",
        "schemas.index",
        json!({"connectionId":source.id,"expectedRevision":"1"}),
    );
    let DomainDecision::Accepted { operation_id } = accepted else {
        panic!("{accepted:?}")
    };
    let operation = read_snapshot(&*s, &ReadOptions::default(), |r| {
        r.operations().operation(&operation_id)
    })
    .unwrap()
    .unwrap();
    let delivery: Id = serde_json::from_value(operation.payload["deliveryId"].clone()).unwrap();
    let mut disabled = config.clone();
    disabled["enabled"] = json!(false);
    disabled["projectAccess"] = json!([]);
    let changed = owner_command(
        &h,
        &s,
        owner.clone(),
        "queued-disable",
        "schemas.configure",
        json!({"scope":{"kind":"Workspace"},"connectionId":source.id,"expectedRevision":"2","config":disabled}),
    );
    assert!(
        matches!(changed,DomainDecision::Applied{result:OperationResult::SchemasConfigure{value}} if value.active_operation_id.is_none() && value.revision.get()==3)
    );
    let cancelled = read_snapshot(&*s, &ReadOptions::default(), |r| r.queue().entry(&delivery))
        .unwrap()
        .unwrap();
    assert!(cancelled.cancel_requested || matches!(cancelled.state, QueueState::Cancelled));
    assert_eq!(reader.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    let restored = owner_command(
        &h,
        &s,
        owner.clone(),
        "queued-reenable",
        "schemas.configure",
        json!({"scope":{"kind":"Workspace"},"connectionId":source.id,"expectedRevision":"3","config":config}),
    );
    assert!(
        matches!(restored,DomainDecision::Applied{result:OperationResult::SchemasConfigure{value}} if value.active_operation_id.is_none() && value.revision.get()==4)
    );
}

#[test]
fn native_metadata_missing_target_is_failed_not_empty_and_releases_source_for_retry() {
    use devworks_core::{
        protocol::results::{DomainDecision, OperationResult},
        services::schemas::indexing::SchemaOutcome,
    };
    let h = Harness::new();
    let s = std::sync::Arc::new(h.store());
    schema_seed_humans(&h, &s);
    let reader = std::sync::Arc::new(CountingMetadata(std::sync::atomic::AtomicUsize::new(0)));
    let owner = metadata_owner(&h, s.clone(), reader.clone());
    let path = h.root.0.join("later-created.sqlite");
    let mut config =
        serde_json::to_value(source("missing-metadata", true).connection.config.unwrap()).unwrap();
    config["endpoint"] = json!(path.to_str().unwrap());
    let made = owner_command(
        &h,
        &s,
        owner.clone(),
        "missing-source",
        "schemas.configure",
        json!({"scope":{"kind":"Workspace"},"config":config}),
    );
    let DomainDecision::Applied {
        result: OperationResult::SchemasConfigure { value: source },
    } = made
    else {
        panic!("{made:?}")
    };
    let accepted = owner_command(
        &h,
        &s,
        owner.clone(),
        "missing-index",
        "schemas.index",
        json!({"connectionId":source.id,"expectedRevision":"1"}),
    );
    let DomainDecision::Accepted { operation_id } = accepted else {
        panic!("{accepted:?}")
    };
    let operation = dispatch_metadata(&s, operation_id, false);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let failed = runtime
        .block_on(owner.run_metadata(&operation, &Cancellation::new()))
        .unwrap();
    assert!(matches!(failed, SchemaOutcome::Failed(_)));
    let current = read_snapshot(&*s, &ReadOptions::default(), |r| {
        r.schemas().schema_source(&source.id)
    })
    .unwrap()
    .unwrap();
    assert!(current.connection.current_view_id.is_none() && current.connection.coverage.is_none());
    assert!(
        current.connection.active_operation_id.is_none() && current.connection.last_error.is_some()
    );
    assert!(
        !path.exists(),
        "read-only missing-target request cannot create a database"
    );
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE now_present(id INTEGER PRIMARY KEY)")
        .unwrap();
    let accepted = owner_command(
        &h,
        &s,
        owner.clone(),
        "missing-retry",
        "schemas.refresh",
        json!({"connectionId":source.id,"expectedRevision":current.connection.revision}),
    );
    let DomainDecision::Accepted { operation_id } = accepted else {
        panic!("{accepted:?}")
    };
    let operation = dispatch_metadata(&s, operation_id, false);
    assert!(matches!(
        runtime
            .block_on(owner.run_metadata(&operation, &Cancellation::new()))
            .unwrap(),
        SchemaOutcome::Completed(_)
    ));
    let current = read_snapshot(&*s, &ReadOptions::default(), |r| {
        r.schemas().schema_source(&source.id)
    })
    .unwrap()
    .unwrap();
    assert!(
        current.connection.current_view_id.is_some() && current.connection.last_error.is_none()
    );
}

#[test]
fn native_schema_pins_keep_captured_views_but_never_extend_expiry_or_access() {
    use devworks_core::{
        protocol::results::{DomainDecision, DomainQueryReply, OperationResult},
        services::schemas::indexing::SchemaOutcome,
    };
    use std::sync::{Arc, atomic::Ordering};
    let h = Harness::new();
    let s = Arc::new(h.store());
    schema_seed_humans(&h, &s);
    let path = h.root.0.join("retained-schema.sqlite");
    let target = rusqlite::Connection::open(&path).unwrap();
    target
        .execute_batch(
            "CREATE TABLE original(id INTEGER PRIMARY KEY); INSERT INTO original VALUES(7);",
        )
        .unwrap();
    let reader = Arc::new(CountingMetadata(std::sync::atomic::AtomicUsize::new(0)));
    let service = metadata_owner(&h, s.clone(), reader.clone());
    let mut config =
        serde_json::to_value(source("pin-source", true).connection.config.unwrap()).unwrap();
    config["endpoint"] = json!(path.to_str().unwrap());
    let DomainDecision::Applied {
        result: OperationResult::SchemasConfigure { value: created },
    } = owner_command(
        &h,
        &s,
        service.clone(),
        "pin-create",
        "schemas.configure",
        json!({"scope":{"kind":"Workspace"},"config":config}),
    )
    else {
        panic!("source creation");
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let index = |key: &str| {
        let current = read_snapshot(&*s, &ReadOptions::default(), |r| {
            r.schemas().schema_source(&created.id)
        })
        .unwrap()
        .unwrap();
        let DomainDecision::Accepted { operation_id } = owner_command(
            &h,
            &s,
            service.clone(),
            key,
            "schemas.index",
            json!({"connectionId":created.id,"expectedRevision":current.connection.revision}),
        ) else {
            panic!("index admission");
        };
        let operation = dispatch_metadata(&s, operation_id, false);
        assert!(matches!(
            runtime
                .block_on(service.run_metadata(&operation, &Cancellation::new()))
                .unwrap(),
            SchemaOutcome::Completed(_)
        ));
    };
    let query = |who: &str, op: &str, mut body: serde_json::Value| {
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
        body["kind"] = json!(op);
        let frame = json!({"protocol":"1.0-draft.1","mode":"Query","schemaRevision":5,"queryId":"retained-schema-query","body":body});
        gateway
            .query(
                &schema_context(&s, who),
                op,
                frame.to_string().as_bytes(),
                &*service,
            )
            .unwrap()
    };
    let catalog = |who: &str, scope: serde_json::Value| {
        let result = query(
            who,
            "schemas.connections",
            json!({"scope":scope,"page":{"limit":100}}),
        );
        let DomainQueryReply::Result {
            result: OperationResult::SchemasConnections { value },
            ..
        } = result
        else {
            panic!("{result:?}");
        };
        value
            .items
            .0
            .into_iter()
            .find(|row| row.id == created.id)
            .unwrap()
            .read_view
            .unwrap()
    };
    let objects = |who: &str, scope: serde_json::Value, pin: &Id, cursor: Option<&str>| {
        let mut body = json!({"scope":scope,"connectionId":created.id,"views":{"kind":"Pinned","pinId":pin},"kinds":[],"page":{"limit":1}});
        if let Some(cursor) = cursor {
            body["page"]["cursor"] = json!(cursor);
        }
        query(who, "schemas.objects", body)
    };
    index("pin-index-first");
    let first = catalog("schema-owner", json!({"kind":"Workspace"}));
    let repeated = catalog("schema-owner", json!({"kind":"Workspace"}));
    assert_eq!(first.pin_id, repeated.pin_id);
    assert_eq!(first.expires_at_ms, repeated.expires_at_ms);
    let before = objects(
        "schema-owner",
        json!({"kind":"Workspace"}),
        &first.pin_id,
        None,
    );
    let DomainQueryReply::Result {
        result: OperationResult::SchemasObjects { value: old_page },
        ..
    } = before
    else {
        panic!("{before:?}");
    };
    assert!(!old_page.items.0.is_empty());
    let cursor = old_page.next_cursor.unwrap();
    assert!(matches!(
        objects(
            "schema-other",
            json!({"kind":"Workspace"}),
            &first.pin_id,
            None
        ),
        DomainQueryReply::Rejected { .. }
    ));
    assert!(matches!(
        objects(
            "schema-owner",
            json!({"kind":"Project","projectId":"metadata-project"}),
            &first.pin_id,
            None
        ),
        DomainQueryReply::Rejected { .. }
    ));
    target.execute_batch("CREATE TABLE later(id INT);").unwrap();
    index("pin-index-second");
    let current = catalog("schema-owner", json!({"kind":"Workspace"}));
    assert_ne!(first.pin_id, current.pin_id);
    assert_ne!(
        first.schema_views[0].snapshot_id,
        current.schema_views[0].snapshot_id
    );
    let retained = objects(
        "schema-owner",
        json!({"kind":"Workspace"}),
        &first.pin_id,
        Some(&cursor.0),
    );
    assert!(
        matches!(
            retained,
            DomainQueryReply::Result {
                result: OperationResult::SchemasObjects { .. },
                ..
            }
        ),
        "{retained:?}"
    );
    let old = read_snapshot(&*s, &ReadOptions::default(), |r| {
        r.schemas().schema_objects(
            &first.schema_views[0].snapshot_id,
            &SchemaObjectFilter::default(),
            None,
            500,
        )
    })
    .unwrap();
    assert!(old.iter().all(|o| !o.qualified_name.0.contains("later")));
    h.clock.0.store(first.expires_at_ms.get(), Ordering::SeqCst);
    assert!(matches!(
        objects(
            "schema-owner",
            json!({"kind":"Workspace"}),
            &first.pin_id,
            None
        ),
        DomainQueryReply::Rejected { .. }
    ));
    let fresh = catalog("schema-owner", json!({"kind":"Workspace"}));
    assert_ne!(current.pin_id, fresh.pin_id);
    assert!(matches!(
        objects(
            "schema-owner",
            json!({"kind":"Workspace"}),
            &first.pin_id,
            None
        ),
        DomainQueryReply::Rejected { .. }
    ));
    // Current scope revocation wins even while a foreign reader's retained pin is live.
    let shared = catalog(
        "schema-other",
        json!({"kind":"Project","projectId":"metadata-project"}),
    );
    let current = read_snapshot(&*s, &ReadOptions::default(), |r| {
        r.schemas().schema_source(&created.id)
    })
    .unwrap()
    .unwrap();
    config["projectAccess"] = json!([]);
    config["workspaceExposed"] = json!(false);
    let edited = owner_command(
        &h,
        &s,
        service.clone(),
        "pin-revoke",
        "schemas.configure",
        json!({"scope":{"kind":"Workspace"},"connectionId":created.id,"expectedRevision":current.connection.revision,"config":config}),
    );
    assert!(matches!(edited, DomainDecision::Applied { .. }));
    assert!(matches!(
        objects(
            "schema-other",
            json!({"kind":"Project","projectId":"metadata-project"}),
            &shared.pin_id,
            None
        ),
        DomainQueryReply::Rejected { .. }
    ));
    assert!(matches!(
        objects(
            "schema-owner",
            json!({"kind":"Workspace"}),
            &fresh.pin_id,
            None
        ),
        DomainQueryReply::Rejected { .. }
    ));
    assert_eq!(
        reader.0.load(Ordering::SeqCst),
        2,
        "all drill-downs are local immutable reads"
    );
    assert_eq!(
        target
            .query_row("SELECT id FROM original", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        7
    );
    service.stop();
}
