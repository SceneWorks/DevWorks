//! Real file metadata and native recurring jobs. No inference or OS credential access.
use super::*;
use devworks_core::ports::database::{
    MetadataFailure, MetadataFuture, MetadataReader, MetadataRequest,
};

#[derive(Default)]
struct Reader {
    calls: AtomicUsize,
    parked: Arc<AtomicBool>,
    stop_observed: Arc<AtomicBool>,
}
impl MetadataReader for Reader {
    fn read(&self, request: MetadataRequest) -> MetadataFuture {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n < 2 {
            return devworks_adapters::database::NativeMetadataReader.read(request);
        }
        let (parked, stopped) = (self.parked.clone(), self.stop_observed.clone());
        Box::pin(async move {
            parked.store(true, Ordering::SeqCst);
            while !request.cancellation.is_cancelled() {
                tokio::task::yield_now().await;
            }
            stopped.store(true, Ordering::SeqCst);
            Err(MetadataFailure::Cancelled)
        })
    }
}
fn source(app: &HelpApp, id: &Id) -> StoredSchemaSource {
    read(&*app.store, |s| s.schemas().schema_source(id)).unwrap()
}
fn definition(app: &HelpApp, connection: &Id) -> Value {
    let capability = app
        .runtime
        .jobs()
        .catalog()
        .capabilities()
        .into_iter()
        .find(|c| c.operation == "schemas.index")
        .expect("native schema sync qualified");
    json!({"kind":"Native","capabilityId":capability.capability_id,
        "arguments":{"schema":capability.schema,"value":{"connectionId":connection}}})
}
#[test]
fn recurring_schema_jobs_capture_current_private_source_without_borrowing_human_and_cancel_owned_child()
 {
    let _native_case = crate::common::native_runtime_case();
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let path = h.root.join("scheduled-target.sqlite");
    let target = rusqlite::Connection::open(&path).unwrap();
    target.execute_batch("CREATE TABLE original(id INTEGER PRIMARY KEY,label TEXT); INSERT INTO original VALUES(1,'SOURCE_ROW_SENTINEL_NEVER_INDEX');").unwrap();
    drop(target);
    let rt = runtime();
    let reader = Arc::new(Reader::default());
    let app = boot_help_with_metadata(
        &rt,
        &h,
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.with_extension("cache"),
        )),
        Some(devworks_engine::schemas::SchemaComposition {
            reader: reader.clone(),
            secrets: Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
        }),
    );
    let config = json!({"name":"Private scheduled source","engine":"Sqlite","endpoint":path,
        "catalogs":[],"schemas":[],"includePatterns":[],"excludePatterns":[],"writesEnabled":false,
        "enabled":true,"projectAccess":[],"workspaceExposed":false,"exposureMode":"SchemaOnly","authentication":"File"});
    let configured = value(app.command(
        "schemas.configure",
        json!({"scope":{"kind":"Workspace"},"config":config}),
        5,
    ));
    let connection = id(configured["id"]
        .as_str()
        .expect("configured source identity"));
    let (scheduler, native) = scheduler_for(&app, 0, "schemas.index");
    native.lose_committed.store(true, Ordering::SeqCst);
    let job = app.create(
        "Recurring private metadata",
        json!({"kind":"Interval","seconds":"3600"}),
        definition(&app, &connection),
        true,
    );
    h.advance(3_601_000);
    app.tick(&rt, &scheduler);
    let first = app.settle(&rt, &scheduler, &job);
    assert_eq!(first.status, "Succeeded");
    let frame1 = captured(&first);
    let after1 = source(&app, &connection);
    assert!(after1.connection.current_view_id.is_some());
    assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
    assert_eq!(frame1["body"]["expectedRevision"], "1");
    let frames = native.frames.lock().unwrap();
    assert!(frames.len() >= 2);
    assert_eq!(
        frames[0], frames[1],
        "lost acknowledgment repeats exact frame and invocation"
    );
    drop(frames);
    let inner = read(&*app.store, |s| {
        s.operations()
            .operation(first.inner_operation_id.as_ref().unwrap())
    })
    .unwrap();
    assert_eq!(inner.payload["actor"], json!(app.store.info().host_actor));
    assert_eq!(inner.payload["actorKind"], "Host");
    assert_eq!(
        inner.payload["sourceContext"]["invocationContext"]["jobRun"],
        json!(first.id)
    );
    let target = rusqlite::Connection::open(&path).unwrap();
    target
        .execute_batch("ALTER TABLE original ADD COLUMN added_by_fixture TEXT;")
        .unwrap();
    drop(target);
    h.advance(3_601_000);
    app.tick(&rt, &scheduler);
    let second = app.settle(&rt, &scheduler, &job);
    assert_eq!(second.status, "Succeeded");
    let frame2 = captured(&second);
    assert_eq!(
        frame2["body"]["expectedRevision"],
        json!(after1.connection.revision)
    );
    assert_ne!(frame1, frame2);
    assert_eq!(reader.calls.load(Ordering::SeqCst), 2);
    let after2 = source(&app, &connection);
    assert_ne!(
        after1.connection.current_view_id,
        after2.connection.current_view_id
    );
    let view = after2.connection.current_view_id.as_ref().unwrap();
    let objects = read(&*app.store, |s| {
        s.schemas()
            .schema_objects(view, &SchemaObjectFilter::default(), None, 500)
    });
    let serialized = serde_json::to_string(&objects).unwrap();
    assert!(serialized.contains("added_by_fixture"));
    assert!(!serialized.contains("SOURCE_ROW_SENTINEL_NEVER_INDEX"));
    // The schedule grants metadata indexing only; Host workspace discovery remains unchanged.
    let visibility = SchemaVisibility {
        actor_id: app.store.info().host_actor,
        workspace: true,
        project_id: None,
        exposed_only: false,
    };
    assert!(
        read(&*app.store, |s| s.schemas().schema_sources(
            &visibility,
            None,
            500
        ))
        .is_empty()
    );
    h.advance(3_601_000);
    app.tick(&rt, &scheduler);
    rt.block_on(within("owned schema read entered", async {
        while !reader.parked.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    }));
    let third = app.runs(&job).into_iter().max_by_key(|r| r.seq).unwrap();
    assert_eq!(third.status, "Running");
    assert!(third.inner_operation_id.is_some());
    let reply = app.command(
        "jobs.cancel",
        json!({"jobId":job,"runId":third.id,"expectedRevision":third.revision.to_string()}),
        2,
    );
    assert!(
        matches!(
            reply,
            CommandReply::Recorded {
                receipt: devworks_core::protocol::Receipt {
                    decision: DomainDecision::Accepted { .. },
                    ..
                },
                ..
            }
        ),
        "{reply:?}"
    );
    let cancelled = app.settle(&rt, &scheduler, &job);
    assert_eq!(cancelled.status, "Cancelled");
    assert!(reader.stop_observed.load(Ordering::SeqCst));
    assert_eq!(
        source(&app, &connection).connection.current_view_id,
        after2.connection.current_view_id
    );
    rt.block_on(app.runtime.shutdown());
}

#[test]
fn schema_schedule_rechecks_project_access_before_dispatch_and_later_authorized_run_uses_fresh_revision()
 {
    let _native_case = crate::common::native_runtime_case();
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let path = h.root.join("project-scheduled-target.sqlite");
    let target = rusqlite::Connection::open(&path).unwrap();
    target
        .execute_batch("CREATE TABLE project_table(id INTEGER PRIMARY KEY);")
        .unwrap();
    drop(target);
    let rt = runtime();
    let reader = Arc::new(Reader::default());
    let app = boot_help_with_metadata(
        &rt,
        &h,
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.with_extension("cache"),
        )),
        Some(devworks_engine::schemas::SchemaComposition {
            reader: reader.clone(),
            secrets: Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
        }),
    );
    let mut config = json!({"name":"Project scheduled source","engine":"Sqlite","endpoint":path,
        "catalogs":[],"schemas":[],"includePatterns":[],"excludePatterns":[],"writesEnabled":false,
        "enabled":true,"projectAccess":[{"projectId":"project-a","metadataEnabled":true,"dataReadEnabled":false,"dataWriteEnabled":false}],
        "workspaceExposed":false,"exposureMode":"SchemaOnly","authentication":"File"});
    let configured = value(app.command(
        "schemas.configure",
        json!({"scope":{"kind":"Workspace"},"config":config}),
        5,
    ));
    let connection = id(configured["id"].as_str().unwrap());
    let created = value(app.command(
        "jobs.create",
        json!({"scope":{"kind":"Project","projectId":"project-a"},"title":"Project schema sync",
        "schedule":{"kind":"Manual"},"definition":definition(&app,&connection),"enabled":true}),
        2,
    ));
    let job = id(created["id"].as_str().unwrap());
    let queued = value(app.command(
        "jobs.run_now",
        json!({"jobId":job,"expectedRevision":"1"}),
        2,
    ));
    let queued_id = id(queued["id"].as_str().unwrap());
    let before = read(&*app.store, |s| s.jobs().job_run(&queued_id)).unwrap();
    assert!(
        before.checkpoint.is_some(),
        "purpose grant commits with actual run"
    );
    config["projectAccess"] = json!([]);
    value(app.command("schemas.configure",json!({"connectionId":connection,"expectedRevision":"1","config":config,"scope":{"kind":"Workspace"}}),5));
    let (scheduler, _native) = scheduler_for(&app, 0, "schemas.index");
    app.tick(&rt, &scheduler);
    let failed = read(&*app.store, |s| s.jobs().job_run(&queued_id)).unwrap();
    assert_eq!(failed.status, "Failed");
    assert!(failed.inner_operation_id.is_none());
    assert!(
        failed
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("authorization changed"))
    );
    assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
    let source_before = source(&app, &connection);
    assert!(source_before.connection.current_view_id.is_none());
    config["projectAccess"] = json!([{"projectId":"project-a","metadataEnabled":true,"dataReadEnabled":false,"dataWriteEnabled":false}]);
    value(app.command("schemas.configure",json!({"connectionId":connection,"expectedRevision":source_before.connection.revision,"config":config,"scope":{"kind":"Workspace"}}),5));
    let current = source(&app, &connection);
    value(app.command(
        "jobs.run_now",
        json!({"jobId":job,"expectedRevision":"1"}),
        2,
    ));
    app.tick(&rt, &scheduler);
    let done = app.settle(&rt, &scheduler, &job);
    assert_eq!(done.status, "Succeeded");
    assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        captured(&done)["body"]["expectedRevision"],
        json!(current.connection.revision)
    );
    let inner = read(&*app.store, |s| {
        s.operations()
            .operation(done.inner_operation_id.as_ref().unwrap())
    })
    .unwrap();
    assert_eq!(inner.payload["actorKind"], "Host");
    assert_eq!(
        inner.payload["scope"],
        json!({"kind":"Project","projectId":"project-a"})
    );
    assert_eq!(
        inner.payload["sourceContext"]["invocationContext"]["project"],
        "project-a"
    );
    rt.block_on(app.runtime.shutdown());
}

#[test]
fn cancelling_ready_schema_child_clears_only_its_activity_and_allows_next_run() {
    let _native_case = crate::common::native_runtime_case();
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let path = h.root.join("ready-cancel-target.sqlite");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE original(id INTEGER PRIMARY KEY);")
        .unwrap();
    let rt = runtime();
    let reader = Arc::new(Reader::default());
    let app = boot_help_with_metadata(
        &rt,
        &h,
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.with_extension("cache"),
        )),
        Some(devworks_engine::schemas::SchemaComposition {
            reader: reader.clone(),
            secrets: Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
        }),
    );
    let configured = value(app.command("schemas.configure", json!({"scope":{"kind":"Workspace"},"config":{"name":"Queued cancel source","engine":"Sqlite","endpoint":path,
        "catalogs":[],"schemas":[],"includePatterns":[],"excludePatterns":[],"writesEnabled":false,"enabled":true,"projectAccess":[],"workspaceExposed":false,"exposureMode":"SchemaOnly","authentication":"File"}}), 5));
    let connection = id(configured["id"].as_str().unwrap());
    let (scheduler, _) = scheduler_for(&app, 0, "schemas.index");
    let job = app.create(
        "Ready metadata cancellation",
        json!({"kind":"Interval","seconds":"3600"}),
        definition(&app, &connection),
        true,
    );
    h.advance(3_601_000);
    app.tick(&rt, &scheduler);
    assert_eq!(app.settle(&rt, &scheduler, &job).status, "Succeeded");
    let published = source(&app, &connection);
    assert!(published.connection.current_view_id.is_some());
    assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
    // Stop queue admission before creating the next child, establishing Ready deterministically.
    app.runtime.dispatcher().stop();
    h.advance(3_601_000);
    app.tick(&rt, &scheduler);
    let ready = app.runs(&job).into_iter().max_by_key(|r| r.seq).unwrap();
    assert_eq!(ready.status, "Running");
    let operation = ready.inner_operation_id.as_ref().unwrap().clone();
    let delivery = read(&*app.store, |s| s.operations().operation(&operation))
        .unwrap()
        .payload["deliveryId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        read(&*app.store, |s| s.queue().entry(&id(&delivery)))
            .unwrap()
            .state,
        QueueState::Ready
    );
    assert_eq!(
        source(&app, &connection).connection.active_operation_id,
        Some(operation.0.clone())
    );
    app.command(
        "jobs.cancel",
        json!({"jobId":job,"runId":ready.id,"expectedRevision":ready.revision.to_string()}),
        2,
    );
    assert_eq!(app.settle(&rt, &scheduler, &job).status, "Cancelled");
    let after = source(&app, &connection);
    assert!(after.connection.active_operation_id.is_none());
    assert_eq!(
        after.connection.current_view_id,
        published.connection.current_view_id
    );
    assert_eq!(
        after.configuration_revision,
        published.configuration_revision
    );
    assert_eq!(
        read(&*app.store, |s| s.operations().operation(&operation))
            .unwrap()
            .state,
        OperationState::CancelledBeforeDispatch
    );
    assert_eq!(
        reader.calls.load(Ordering::SeqCst),
        1,
        "cancelled child never executes"
    );
    h.advance(3_601_000);
    app.tick(&rt, &scheduler);
    let next = app.runs(&job).into_iter().max_by_key(|r| r.seq).unwrap();
    assert!(next.seq > ready.seq);
    assert_eq!(next.status, "Running", "source is eligible for a later run");
    assert_ne!(next.inner_operation_id, Some(operation));
    assert_eq!(
        source(&app, &connection).connection.active_operation_id,
        next.inner_operation_id.as_ref().map(|op| op.0.clone())
    );
    app.command(
        "jobs.cancel",
        json!({"jobId":job,"runId":next.id,"expectedRevision":next.revision.to_string()}),
        2,
    );
    assert_eq!(app.settle(&rt, &scheduler, &job).status, "Cancelled");
    rt.block_on(app.runtime.shutdown());
}

#[test]
#[cfg(unix)]
fn separately_permitted_database_effect_runs_through_native_gateway_and_operation_journal() {
    use devworks_core::protocol::results::DomainQueryReply;
    let _case = crate::common::native_runtime_case();
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let outside = Harness::new();
    std::fs::create_dir_all(&outside.root).unwrap();
    let path = outside.root.join("db-target.sqlite");
    let target = rusqlite::Connection::open(&path).unwrap();
    target
        .execute_batch(
            "CREATE TABLE contacts (id INTEGER PRIMARY KEY, label TEXT, payload BLOB);
         INSERT INTO contacts VALUES(1,'VISIBLE_DATABASE_ROW',X'0102');",
        )
        .unwrap();
    let rt = runtime();
    let app = boot_help_with_database(
        &rt,
        &h,
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.with_extension("cache"),
        )),
        Some(devworks_engine::schemas::SchemaComposition {
            reader: Arc::new(devworks_adapters::database::NativeMetadataReader),
            secrets: Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
        }),
        Some(devworks_engine::schemas::DatabaseComposition {
            adapter: Arc::new(devworks_adapters::database::NativeDataAdapter::new(vec![
                h.root.clone(),
            ])),
            filesystem: Arc::new(NativeFilesystem::new()),
        }),
    );
    let available = |name, exposure, writes| {
        value(app.command("schemas.configure", json!({"scope":{"kind":"Workspace"},
            "config":{"name":name,"engine":"Sqlite","endpoint":path,"catalogs":[],"schemas":[],
                "includePatterns":[],"excludePatterns":[],"writesEnabled":writes,
                "enabled":true,"projectAccess":[],"workspaceExposed":false,"exposureMode":exposure,
                "authentication":"File"}
        }),5))
    };
    let enabled = available("Enabled connection", "ReadWrite", true);
    let disabled = available("Metadata only", "SchemaOnly", false);
    let read_data = |connection: &Value, text: &str| {
        let bytes = serde_json::to_vec(&json!({
            "protocol":PROTOCOL,"mode":"Query","schemaRevision":2,"queryId":"database-read",
            "body":{"kind":"database.read","connectionId":connection["id"],
                "expectedConnectionRevision":connection["revision"],"statement":text,
                "parameters":[],"rowLimit":100,"timeoutMs":"30000","includeSchemaDigest":true}
        }))
        .unwrap();
        app.runtime
            .query(&app.user(), "database.read", &bytes)
            .unwrap()
    };
    assert!(matches!(
        read_data(&disabled, "SELECT id FROM contacts"),
        DomainQueryReply::Rejected { .. }
    ));
    let reply = read_data(&enabled, "SELECT id,label,payload FROM contacts");
    let DomainQueryReply::Result {
        result: OperationResult::DatabaseRead { value: answer },
        ..
    } = reply
    else {
        panic!("authorized native database read: {reply:?}");
    };
    assert_eq!(answer.rows.0.len(), 1);
    assert!(answer.schema_digest.is_some());
    let artifact = match &answer.rows.0[0].cells.0[2] {
        devworks_core::protocol::records::DbScalar::Bytes { value } => value.clone(),
        _ => panic!("expected a scoped native artifact"),
    };
    let read_artifact = || {
        app.runtime
            .artifacts()
            .read_exact(app.store.as_ref(), &app.user(), &artifact, 100)
    };
    assert_eq!(read_artifact().unwrap().0, [1, 2]);

    let saved = value(app.command(
        "database.edit_sensitive_column_rules",
        json!({
            "expectedRevision":"1","edit":{"kind":"Upsert","rule":{
                "id":"deny-database-payload","table":"contacts","column":"payload","allow":false
            }}
        }),
        1,
    ));
    assert_eq!(saved["revision"], "2");
    assert!(read_artifact().is_err());
    assert!(matches!(
        read_data(&enabled, "SELECT payload AS alias FROM contacts"),
        DomainQueryReply::Rejected { .. }
    ));

    let response = app.command(
        "database.execute_write",
        json!({
            "connectionId":enabled["id"],"expectedConnectionRevision":enabled["revision"],
            "statement":"INSERT INTO contacts(id,label) VALUES(2,'PRIVATE_WRITE_MATERIAL')",
            "parameters":[],"timeoutMs":"5000"
        }),
        2,
    );
    let CommandReply::Recorded { receipt, .. } = response else {
        panic!("{response:?}")
    };
    let DomainDecision::Accepted { operation_id } = receipt.decision else {
        panic!("{:?}", receipt.decision)
    };
    rt.block_on(app.runtime.dispatcher().run_until_idle())
        .unwrap();
    let journal = read(app.store.as_ref(), |read| {
        read.operations().operation(&operation_id)
    })
    .unwrap();
    assert_eq!(
        journal.state,
        OperationState::ObservedComplete,
        "{:?}",
        journal.latest_attempt()
    );
    assert!(
        !journal
            .payload
            .to_string()
            .contains("PRIVATE_WRITE_MATERIAL")
    );
    assert!(
        journal
            .latest_attempt()
            .unwrap()
            .observations
            .iter()
            .any(|entry| entry["completed"] == true)
    );
    let observed: i64 = target
        .query_row("SELECT count(*) FROM contacts WHERE id=2", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(observed, 1);
    assert!(rt.block_on(app.runtime.shutdown()).reconciliation.is_ok());
}
