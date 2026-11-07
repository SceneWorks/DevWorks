//! Actual native gateway, metadata reader, durable worker and headless database controls.
//! Every credential facility is SessionSecrets; no OS keychain, inference or remote DB is used.
mod common;
use common::*;
use devworks_desktop::route::Route;
use eframe::egui::{self, accesskit::Role};
use egui_kittest::kittest::Queryable;
fn replace(
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    label: &str,
    text: &str,
) {
    h.get_by_role_and_label(Role::TextInput, label)
        .scroll_to_me();
    settle(h);
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.step();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    if text.is_empty() {
        h.key_press(egui::Key::Backspace);
    } else {
        h.get_by_role_and_label(Role::TextInput, label)
            .type_text(text);
    }
    settle(h);
}
#[test]
fn native_databases_create_sync_page_inspect_and_remove_only_indexed_metadata() {
    let _native_case = native_runtime_case();
    let root = Root::new("database-native-manager");
    let path = root.file("source.sqlite", b"");
    let source = rusqlite::Connection::open(&path).unwrap();
    source.execute_batch("CREATE TABLE parent(id INTEGER PRIMARY KEY, label TEXT DEFAULT 'structural_default'); CREATE TABLE child(id INTEGER, parent_id INT REFERENCES parent(id)); INSERT INTO parent VALUES(1,'SOURCE_ROW_SENTINEL_NEVER_INDEX');").unwrap();
    for n in 0..60 {
        source
            .execute_batch(&format!("CREATE TABLE additional_{n}(id INT)"))
            .unwrap();
    }
    drop(source);
    let engine = start(&root);
    let dialogs = Script::default();
    dialogs.files.lock().unwrap().push(vec![path.clone()]);
    let mut h = harness(shell(&engine, &root, dialogs), 1600.0, 1200.0);
    h.state_mut().navigate(Route::Databases);
    settle(&mut h);
    click(&mut h, Role::Button, "New database source");
    replace(&mut h, "Database source name", "Native metadata fixture");
    click(&mut h, Role::Button, "Choose database file");
    click(&mut h, Role::Button, "Save database source");
    step_until(&mut h, "native source save returned", |s| {
        !s.databases.busy && !s.databases.loading
    });
    assert!(
        h.state().databases.draft.is_none() && h.state().databases.connections.len() == 1,
        "source result: {:?}",
        h.state().databases.error
    );
    assert!(
        h.state().databases.error.is_none(),
        "{:?}",
        h.state().databases.error
    );
    click(&mut h, Role::Button, "Sync database metadata");
    step_until(&mut h, "metadata admission returned", |s| !s.databases.busy);
    assert!(
        h.state()
            .databases
            .notice
            .as_ref()
            .is_some_and(|v| v.contains("accepted")),
        "admission: notice={:?}; error={:?}; source={:?}",
        h.state().databases.notice,
        h.state().databases.error,
        h.state().databases.connections.first()
    );
    step_until(&mut h, "actual native metadata published", |s| {
        s.databases
            .connections
            .first()
            .is_some_and(|c| c.current_view_id.is_some() && c.active_operation_id.is_none())
    });
    click(&mut h, Role::Button, "Search schema objects");
    step_until(&mut h, "native first object page", |s| {
        !s.databases.read_loading && !s.databases.objects.is_empty()
    });
    assert!(
        h.state().databases.object_next.is_some(),
        "fixture exceeds one actual result page"
    );
    while h.state().databases.object_next.is_some() {
        let before = h.state().databases.objects.len();
        click(&mut h, Role::Button, "More schema objects");
        step_until(&mut h, "next exact schema page", |s| {
            !s.databases.read_loading
        });
        assert!(
            h.state().databases.objects.len() > before,
            "page result: count={}; error={:?}; cursor={:?}",
            h.state().databases.objects.len(),
            h.state().databases.error,
            h.state().databases.object_next
        );
    }
    let parent = h
        .state()
        .databases
        .objects
        .iter()
        .find(|o| {
            o.metadata.as_ref().is_some_and(|m| m.name.0 == "parent")
                && matches!(
                    o.kind,
                    devworks_core::protocol::records::MetadataObjectKind::Table
                )
        })
        .unwrap()
        .clone();
    let label = format!("{:?} {}", parent.kind, parent.qualified_name.0);
    click(&mut h, Role::Button, &label);
    click(&mut h, Role::Button, "Inspect schema relationships");
    step_until(&mut h, "native parent relationships", |s| {
        !s.databases.read_loading && !s.databases.relations.is_empty()
    });
    // A relationship endpoint deliberately installs an exact-object filter before children.
    let endpoint = format!("Inspect relationship target: {}", parent.qualified_name.0);
    h.query_all_by_role_and_label(Role::Button, &endpoint)
        .next()
        .expect("parent relationship target")
        .click_accesskit();
    settle(&mut h);
    step_until(&mut h, "exact relationship endpoint", |s| {
        !s.databases.read_loading && s.databases.object_id.as_deref() == Some(parent.id.as_str())
    });
    click(&mut h, Role::Button, "Inspect schema children");
    step_until(&mut h, "native table columns", |s| {
        !s.databases.read_loading
    });
    assert!(
        h.state()
            .databases
            .objects
            .iter()
            .any(|o| o.parent_id.as_ref() == Some(&parent.id))
    );
    assert!(
        !serde_json::to_string(&h.state().databases.objects)
            .unwrap()
            .contains("SOURCE_ROW_SENTINEL_NEVER_INDEX")
    );
    click(&mut h, Role::Button, "Remove database index");
    assert_eq!(
        h.state().databases.confirm,
        Some("index"),
        "index action: selected={:?}; busy={}; error={:?}",
        h.state().databases.selected,
        h.state().databases.busy,
        h.state().databases.error
    );
    click(&mut h, Role::Button, "Confirm database removal");
    step_until(&mut h, "native index removal", |s| {
        s.databases
            .connections
            .first()
            .is_some_and(|c| c.current_view_id.is_none() && c.active_operation_id.is_none())
            && !s.databases.busy
    });
    let source =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    assert_eq!(
        source
            .query_row("SELECT label FROM parent WHERE id=1", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        "SOURCE_ROW_SENTINEL_NEVER_INDEX"
    );
    click(&mut h, Role::Button, "Remove database source");
    assert_eq!(
        h.state().databases.confirm,
        Some("source"),
        "source action: selected={:?}; busy={}; error={:?}",
        h.state().databases.selected,
        h.state().databases.busy,
        h.state().databases.error
    );
    click(&mut h, Role::Button, "Confirm database removal");
    step_until(&mut h, "native source removal", |s| {
        s.databases.connections.is_empty() && !s.databases.busy
    });
    assert!(path.exists());
}
#[test]
fn database_draft_memory_preserves_configuration_without_password_bytes() {
    let _native_case = native_runtime_case();
    let root = Root::new("database-draft-memory");
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1200.0);
    h.state_mut().navigate(Route::Databases);
    settle(&mut h);
    click(&mut h, Role::Button, "New database source");
    let d = h.state_mut().databases.draft.as_mut().unwrap();
    d.config["name"] = serde_json::json!("Retained source");
    d.password = "fixture password never serialized".into();
    let memory = h.state().memory();
    let json = serde_json::to_string(&memory).unwrap();
    assert!(!json.contains("fixture password never serialized"));
    assert!(json.contains("Retained source"));
    h.state_mut().on_exit();
    drop(h);
    let mut restored = harness(shell(&engine, &root, Script::default()), 1600.0, 1200.0);
    restored.state_mut().navigate(Route::Databases);
    settle(&mut restored);
    let draft = restored.state().databases.draft.as_ref().unwrap();
    assert_eq!(draft.config["name"], "Retained source");
    assert!(draft.password.is_empty());
}

#[test]
fn native_database_sync_all_includes_sources_after_the_first_catalog_page() {
    use devworks_core::protocol::{CommandReply, results::DomainDecision};
    let _native_case = native_runtime_case();
    let root = Root::new("database-sync-all-pages");
    let path = root.file("shared-source.sqlite", b"");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE schema_fixture(id INTEGER PRIMARY KEY)")
        .unwrap();
    let engine = start(&root);
    let backend = engine.backend();
    for n in 0..101 {
        let config = serde_json::json!({"name":format!("Source{n:03}"),"engine":"Sqlite","endpoint":path.to_str().unwrap(),"catalogs":[],"schemas":[],"includePatterns":[],"excludePatterns":[],"writesEnabled":false,"enabled":true,"projectAccess":[],"workspaceExposed":false,"exposureMode":"SchemaOnly","authentication":"File"});
        let frame=serde_json::json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":5,"requestKey":backend.token("batch-source"),"body":{"kind":"schemas.configure","scope":{"kind":"Workspace"},"config":config}}).to_string().into_bytes();
        assert!(
            matches!(backend.command("schemas.configure",&frame).unwrap(),CommandReply::Recorded{receipt,..} if matches!(receipt.decision,DomainDecision::Applied{..}))
        );
    }
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1200.0);
    h.state_mut().navigate(Route::Databases);
    settle(&mut h);
    step_until(&mut h, "first source catalog page", |s| {
        s.databases.loaded && !s.databases.loading
    });
    assert_eq!(h.state().databases.connections.len(), 100);
    assert!(h.state().databases.next.is_some());
    click(&mut h, Role::Button, "Sync all enabled sources");
    step_until(&mut h, "all catalog pages admitted", |s| {
        !s.databases.batch_active && !s.databases.batch_scanning && !s.databases.busy
    });
    assert_eq!(
        h.state().databases.batch_submitted,
        101,
        "all enabled sources were captured before admissions changed inventory cursors; error={:?}; notice={:?}",
        h.state().databases.error,
        h.state().databases.notice
    );
    assert_eq!(
        h.state().databases.batch_failures,
        0,
        "{:?}",
        h.state().databases.error
    );
    assert!(
        h.state()
            .databases
            .notice
            .as_ref()
            .is_some_and(|s| s.contains("101"))
    );
    // This is admission evidence, not a fabricated assertion that all101 external reads finished.
    assert!(path.exists());
}

#[test]
fn native_database_inspector_explains_partial_object_metadata() {
    let _native_case = native_runtime_case();
    let root = Root::new("database-partial-object");
    let path = root.file("source.sqlite", b"");
    let source = rusqlite::Connection::open(&path).unwrap();
    source
        .execute_batch(&format!(
            "CREATE TABLE defaults(value TEXT DEFAULT '{}');",
            "x".repeat(6000)
        ))
        .unwrap();
    drop(source);
    let original = std::fs::read(&path).unwrap();
    let engine = start(&root);
    let dialogs = Script::default();
    dialogs.files.lock().unwrap().push(vec![path.clone()]);
    let mut h = harness(shell(&engine, &root, dialogs), 1600.0, 1200.0);
    h.state_mut().navigate(Route::Databases);
    settle(&mut h);
    click(&mut h, Role::Button, "New database source");
    replace(&mut h, "Database source name", "Partial metadata fixture");
    click(&mut h, Role::Button, "Choose database file");
    click(&mut h, Role::Button, "Save database source");
    step_until(&mut h, "partial fixture configured", |s| {
        !s.databases.busy && !s.databases.loading && s.databases.connections.len() == 1
    });
    click(&mut h, Role::Button, "Sync database metadata");
    step_until(&mut h, "bounded metadata published", |s| {
        s.databases.connections.first().is_some_and(|c| {
            c.current_view_id.is_some()
                && c.active_operation_id.is_none()
                && matches!(
                    c.coverage.as_ref().map(|v| &v.state),
                    Some(devworks_core::protocol::data::CoverageState::Partial)
                )
        })
    });
    click(&mut h, Role::Button, "Search schema objects");
    step_until(&mut h, "partial column listed", |s| {
        !s.databases.read_loading
            && s.databases
                .objects
                .iter()
                .any(|o| o.qualified_name.0 == "main.defaults.value")
    });
    click(&mut h, Role::Button, "Column main.defaults.value");
    assert!(
        h.query_by_label("Object coverage: Partial").is_some(),
        "selected={:?}; error={:?}; labels={:?}",
        h.state()
            .databases
            .selected_object
            .as_ref()
            .map(|o| (&o.qualified_name.0, &o.coverage.state)),
        h.state().databases.error,
        labels(&h, Role::Label)
    );
    assert!(
        h.query_by_label("Object coverage gap: sqlite_default_size_limit")
            .is_some()
    );
    let object = h.state().databases.selected_object.as_ref().unwrap();
    assert_eq!(
        object
            .metadata
            .as_ref()
            .unwrap()
            .default_value
            .as_ref()
            .unwrap()
            .0
            .chars()
            .count(),
        4096
    );
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[test]
fn native_database_key_utility_generates_32_bytes_without_persisting_or_reconfiguring() {
    use base64::Engine as _;
    let _native_case = native_runtime_case();
    let root = Root::new("database-key-utility");
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1200.0);
    h.state_mut().navigate(Route::Databases);
    settle(&mut h);
    click(&mut h, Role::Button, "Generate encryption key");
    let key = h
        .state()
        .databases
        .generated_key
        .as_ref()
        .expect("human utility result")
        .expose()
        .to_owned();
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&key)
            .unwrap()
            .len(),
        32
    );
    assert!(!format!("{:?}", h.state().databases.generated_key).contains(&key));
    assert!(h.query_by_label(&key).is_none(), "key bytes remain masked");
    h.get_by_role_and_label(Role::Button, "Copy generated encryption key")
        .click();
    let mut copied = false;
    for _ in 0..12 {
        h.step();
        copied |= h
            .output()
            .platform_output
            .commands
            .iter()
            .any(|command| matches!(command,egui::OutputCommand::CopyText(text) if text==&key));
        if copied {
            break;
        }
    }
    assert!(
        copied,
        "native clipboard command contains the exact generated key"
    );
    assert!(h.state().databases.connections.is_empty());
    assert!(
        !serde_json::to_string(&h.state().memory())
            .unwrap()
            .contains(&key)
    );
    click(&mut h, Role::Button, "Clear generated encryption key");
    assert!(h.state().databases.generated_key.is_none());
    click(&mut h, Role::Button, "Generate encryption key");
    h.state_mut().navigate(Route::Chat);
    settle(&mut h);
    assert!(h.state().databases.generated_key.is_none());
}

#[test]
fn native_database_background_inventory_failure_cannot_abort_an_admitted_sync_all_batch() {
    use devworks_core::protocol::{CommandReply, results::DomainDecision};
    let _native_case = native_runtime_case();
    let root = Root::new("database-sync-all-inventory-fault");
    let path = root.file("batch-source.sqlite", b"");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE original(id INT);")
        .unwrap();
    let engine = start(&root);
    let backend = engine.backend();
    for n in 0..3 {
        let frame = serde_json::json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":5,"requestKey":backend.token("fault-batch-source"),"body":{"kind":"schemas.configure","scope":{"kind":"Workspace"},"config":{"name":format!("FaultSource{n}"),"engine":"Sqlite","endpoint":path,"catalogs":[],"schemas":[],"includePatterns":[],"excludePatterns":[],"writesEnabled":false,"enabled":true,"projectAccess":[],"workspaceExposed":false,"exposureMode":"SchemaOnly","authentication":"File"}}}).to_string().into_bytes();
        assert!(
            matches!(backend.command("schemas.configure", &frame).unwrap(),CommandReply::Recorded{receipt,..} if matches!(receipt.decision,DomainDecision::Applied{..}))
        );
    }
    let held = Gate::holding_replies("schemas.refresh");
    let inventory = Gate::new("schemas.connections");
    let mut h = harness(
        shell_with(
            Ok(inventory.over(held.over(backend))),
            &root,
            Script::default(),
            None,
        ),
        1600.0,
        1200.0,
    );
    h.state_mut().navigate(Route::Databases);
    settle(&mut h);
    step_until(&mut h, "fault batch sources loaded", |s| {
        s.databases.connections.len() == 3 && !s.databases.loading
    });
    struct Release(Gate);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.open();
        }
    }
    let release = Release(held.clone());
    held.close();
    h.get_by_role_and_label(Role::Button, "Sync all enabled sources")
        .click_accesskit();
    step_until(
        &mut h,
        "first batch command awaiting acknowledgement",
        |s| s.databases.batch_submitted == 1 && s.databases.busy && !s.databases.batch_scanning,
    );
    step_until(&mut h, "inventory idle before fault", |s| {
        !s.databases.loading
    });
    inventory.fail_next_query();
    h.get_by_role_and_label(Role::Button, "Refresh databases")
        .click_accesskit();
    step_until(&mut h, "ordinary inventory read refused", |s| {
        !s.databases.loading && s.databases.error.is_some()
    });
    assert!(
        h.state().databases.batch_active,
        "inventory refresh cannot cancel the captured batch"
    );
    assert!(
        h.state().databases.busy,
        "the admitted command still awaits its receipt"
    );
    assert_eq!(h.state().databases.batch_remaining.len(), 2);
    drop(release);
    step_until(
        &mut h,
        "all captured batch commands admitted after read failure",
        |s| !s.databases.batch_active && !s.databases.busy,
    );
    assert_eq!(h.state().databases.batch_submitted, 3);
    assert_eq!(h.state().databases.batch_failures, 0);
    settle(&mut h);
}

#[test]
fn native_database_search_refreshes_a_missing_prepared_capture_before_reading_objects() {
    use devworks_core::protocol::{CommandReply, results::DomainDecision};
    let _native_case = native_runtime_case();
    let root = Root::new("database-search-fresh-capture");
    let path = root.file("capture-source.sqlite", b"");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE captured(id INT);")
        .unwrap();
    let engine = start(&root);
    let backend = engine.backend();
    let frame = serde_json::json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":5,"requestKey":backend.token("capture-source"),"body":{"kind":"schemas.configure","scope":{"kind":"Workspace"},"config":{"name":"Capture source","engine":"Sqlite","endpoint":path,"catalogs":[],"schemas":[],"includePatterns":[],"excludePatterns":[],"writesEnabled":false,"enabled":true,"projectAccess":[],"workspaceExposed":false,"exposureMode":"SchemaOnly","authentication":"File"}}}).to_string().into_bytes();
    assert!(
        matches!(backend.command("schemas.configure", &frame).unwrap(),CommandReply::Recorded{receipt,..} if matches!(receipt.decision,DomainDecision::Applied{..}))
    );
    let inventory = Gate::holding_replies("schemas.connections");
    let mut h = harness(
        shell_with(Ok(inventory.over(backend)), &root, Script::default(), None),
        1600.0,
        1200.0,
    );
    h.state_mut().navigate(Route::Databases);
    settle(&mut h);
    h.get_by_label("Capture source").click_accesskit();
    settle(&mut h);
    // A late source-status reply can reflow the selected-source controls.
    // Activate the native control by identity, as an accessibility client does.
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, "Sync database metadata")
        .click_accesskit();
    settle(&mut h);
    step_until(&mut h, "capture source published", |s| {
        s.databases
            .connections
            .first()
            .is_some_and(|c| c.read_view.is_some() && c.active_operation_id.is_none())
    });
    settle(&mut h);
    struct Release(Gate);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.open();
        }
    }
    let release = Release(inventory.clone());
    inventory.close();
    // Legal source projection when publication changes between lease preparation and read:
    // status sees the new head while no earlier capture can be attached to that head.
    h.state_mut().databases.connections[0].read_view = None;
    h.state_mut().databases.view = None;
    h.state_mut().databases.read_pin = None;
    h.get_by_role_and_label(Role::Button, "Search schema objects")
        .click_accesskit();
    h.step();
    assert!(
        h.state().databases.read_loading,
        "the explicit read intent waits for a fresh owner capture"
    );
    assert!(
        h.state().databases.error.is_none(),
        "a publication transition is not confirmed absence"
    );
    drop(release);
    step_until(&mut h, "fresh-capture object read", |s| {
        !s.databases.read_loading
            && s.databases
                .objects
                .iter()
                .any(|o| o.qualified_name.0 == "main.captured")
    });
    assert!(h.state().databases.read_pin.is_some());
    assert!(
        h.state().databases.error.is_none(),
        "{:?}",
        h.state().databases.error
    );
    settle(&mut h);
}

#[test]
fn native_database_boot_without_secret_facility_keeps_sqlite_metadata_and_native_fallback_accessible()
 {
    use devworks_core::ports::secrets::{Secret, SecretError, SecretKey, SecretStore};
    use devworks_core::protocol::{CommandReply, Id, results::DomainDecision};
    struct UnavailableMemory;
    impl SecretStore for UnavailableMemory {
        fn name(&self) -> &'static str {
            "Controlled unavailable memory store"
        }
        fn put(&self, _: &SecretKey, _: &Secret) -> Result<(), SecretError> {
            Err(SecretError::Unavailable(
                "controlled unavailable memory store".into(),
            ))
        }
        fn get(&self, _: &SecretKey) -> Result<Option<Secret>, SecretError> {
            Err(SecretError::Unavailable(
                "controlled unavailable memory store".into(),
            ))
        }
        fn delete(&self, _: &SecretKey) -> Result<bool, SecretError> {
            Err(SecretError::Unavailable(
                "controlled unavailable memory store".into(),
            ))
        }
        fn list(&self, _: &str) -> Result<Vec<Id>, SecretError> {
            Err(SecretError::Unavailable(
                "controlled unavailable memory store".into(),
            ))
        }
    }
    let _native_case = native_runtime_case();
    let root = Root::new("database-unavailable-secret-facility");
    let path = root.file("local.sqlite", b"");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE local_table(id INT);")
        .unwrap();
    let mut cfg = config(&root.0);
    cfg.secrets = Some(std::sync::Arc::new(UnavailableMemory));
    let engine = devworks_desktop::host::Engine::start(cfg)
        .expect("credential unavailability is a degraded capability");
    let backend = engine.backend();
    assert!(
        backend
            .stage_database_password(Secret::new("controlled-never-stored-value"))
            .is_err()
    );
    let frame = serde_json::json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":5,"requestKey":backend.token("unavailable-facility-source"),"body":{"kind":"schemas.configure","scope":{"kind":"Workspace"},"config":{"name":"Local without credentials","engine":"Sqlite","endpoint":path,"catalogs":[],"schemas":[],"includePatterns":[],"excludePatterns":[],"writesEnabled":false,"enabled":true,"projectAccess":[],"workspaceExposed":false,"exposureMode":"SchemaOnly","authentication":"File"}}}).to_string().into_bytes();
    assert!(
        matches!(backend.command("schemas.configure", &frame).unwrap(),CommandReply::Recorded{receipt,..} if matches!(receipt.decision,DomainDecision::Applied{..}))
    );
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1200.0);
    h.state_mut().navigate(Route::Databases);
    settle(&mut h);
    h.get_by_label("Local without credentials")
        .click_accesskit();
    settle(&mut h);
    click(&mut h, Role::Button, "Sync database metadata");
    step_until(&mut h, "credential-free SQLite still publishes", |s| {
        s.databases
            .connections
            .first()
            .is_some_and(|c| c.current_view_id.is_some() && c.active_operation_id.is_none())
    });
    click_nav(&mut h, "Settings");
    assert_eq!(
        h.state().route(),
        Route::Settings,
        "native Settings remains available for fallback selection"
    );
}

#[test]
fn native_database_switching_saved_network_source_to_sqlite_clears_network_custody_and_pending_input()
 {
    use devworks_core::ports::secrets::Secret;
    use devworks_core::protocol::{CommandReply, results::DomainDecision};
    let _native_case = native_runtime_case();
    let root = Root::new("database-engine-conversion");
    let path = root.file("converted.sqlite", b"");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE converted(id INT);")
        .unwrap();
    let engine = start(&root);
    let backend = engine.backend();
    let credential = backend
        .stage_database_password(Secret::new("controlled-saved-password"))
        .unwrap();
    let frame=serde_json::json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":5,"requestKey":backend.token("conversion-source"),"body":{"kind":"schemas.configure","scope":{"kind":"Workspace"},"config":{"name":"Convertible source","engine":"MySql","endpoint":"mysql://127.0.0.1:9/unused","credentialRef":credential,"catalogs":[],"schemas":[],"includePatterns":[],"excludePatterns":[],"writesEnabled":false,"enabled":true,"projectAccess":[],"workspaceExposed":false,"exposureMode":"SchemaOnly","authentication":"Password","username":"metadata_reader","tls":"LoopbackPlaintext"}}}).to_string().into_bytes();
    assert!(
        matches!(backend.command("schemas.configure",&frame).unwrap(),CommandReply::Recorded{receipt,..} if matches!(receipt.decision,DomainDecision::Applied{..}))
    );
    let dialogs = Script::default();
    dialogs.files.lock().unwrap().push(vec![path]);
    let mut h = harness(shell(&engine, &root, dialogs), 1600.0, 1200.0);
    h.state_mut().navigate(Route::Databases);
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, "Edit Convertible source")
        .click_accesskit();
    settle(&mut h);
    let draft = h.state_mut().databases.draft.as_mut().unwrap();
    draft.config["caCertificate"] = serde_json::json!({"artifactId":"copied-ca","versionId":"copied-ca-version","sha256":"a".repeat(64),"byteLength":"8","mediaType":"application/x-pem-file"});
    draft.password = "controlled-pending-password".into();
    draft.ca_file = Some(root.file("pending-ca.pem", b"never imported after switch"));
    click(&mut h, Role::ComboBox, "Database engine");
    h.get_by_label("Sqlite").click_accesskit();
    settle(&mut h);
    let draft = h.state().databases.draft.as_ref().unwrap();
    assert_eq!(draft.config["engine"], "Sqlite", "selected file engine");
    for key in ["credentialRef", "caCertificate", "tls", "username"] {
        assert!(
            draft.config.get(key).is_none(),
            "network-only {key} cleared"
        );
    }
    assert!(draft.password.is_empty());
    assert!(draft.ca_file.is_none());
    h.get_by_role_and_label(Role::Button, "Choose database file")
        .click_accesskit();
    settle(&mut h);
    assert_eq!(
        h.state().databases.draft.as_ref().unwrap().config["engine"],
        "Sqlite",
        "file chooser retains engine"
    );
    h.get_by_role_and_label(Role::Button, "Save database source")
        .click_accesskit();
    settle(&mut h);
    step_until(&mut h, "converted source saved", |s| {
        !s.databases.busy && s.databases.draft.is_none() && !s.databases.loading
    });
    // A late source-status reply can reflow the selected-source controls.
    // Activate the native control by identity, as an accessibility client does.
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, "Sync database metadata")
        .click_accesskit();
    settle(&mut h);
    step_until(
        &mut h,
        "converted SQLite publishes without network custody",
        |s| {
            s.databases
                .connections
                .first()
                .is_some_and(|c| c.current_view_id.is_some() && c.active_operation_id.is_none())
        },
    );
    assert!(
        h.state().databases.error.is_none(),
        "{:?}",
        h.state().databases.error
    );
}

#[test]
fn native_database_accessible_editor_control_survives_a_late_layout_change() {
    let root = Root::new("database-accessible-control");
    let mut h = harness(
        shell_with(
            Err("Controlled offline fixture".into()),
            &root,
            Script::default(),
            None,
        ),
        1500.0,
        1800.0,
    );
    h.state_mut().navigate(Route::Databases);
    h.state_mut().databases.draft = Some(devworks_desktop::databases_screen::Draft::new(None));
    h.state_mut().databases.tab = 2;
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, "Cancel database edit")
        .scroll_to_me();
    settle(&mut h);
    // A backend reply may reflow the form between its accessibility snapshot and input frame.
    h.get_by_role_and_label(Role::Button, "Cancel database edit")
        .click();
    h.state_mut().databases.notice = Some("Late native publication status\n".repeat(8));
    h.step();
    assert!(
        h.state().databases.draft.is_some(),
        "stale coordinates do not identify the intended control"
    );
    h.state_mut().databases.notice = None;
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, "Cancel database edit")
        .click_accesskit();
    h.state_mut().databases.notice = Some("Late native publication status\n".repeat(8));
    h.step();
    assert!(
        h.state().databases.draft.is_none(),
        "native accessible activation retains editor/control identity across late replies"
    );
}

#[test]
fn native_database_relationships_refreshes_a_missing_prepared_capture_before_reading_edges() {
    use devworks_core::protocol::{CommandReply, results::DomainDecision};
    let _native_case = native_runtime_case();
    let root = Root::new("database-relations-fresh-capture");
    let path = root.file("capture-source.sqlite", b"");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE captured(id INT);")
        .unwrap();
    let engine = start(&root);
    let backend = engine.backend();
    let frame = serde_json::json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":5,"requestKey":backend.token("capture-source"),"body":{"kind":"schemas.configure","scope":{"kind":"Workspace"},"config":{"name":"Capture source","engine":"Sqlite","endpoint":path,"catalogs":[],"schemas":[],"includePatterns":[],"excludePatterns":[],"writesEnabled":false,"enabled":true,"projectAccess":[],"workspaceExposed":false,"exposureMode":"SchemaOnly","authentication":"File"}}}).to_string().into_bytes();
    assert!(
        matches!(backend.command("schemas.configure", &frame).unwrap(),CommandReply::Recorded{receipt,..} if matches!(receipt.decision,DomainDecision::Applied{..}))
    );
    let inventory = Gate::holding_replies("schemas.connections");
    let mut h = harness(
        shell_with(Ok(inventory.over(backend)), &root, Script::default(), None),
        1600.0,
        1200.0,
    );
    h.state_mut().navigate(Route::Databases);
    settle(&mut h);
    h.get_by_label("Capture source").click_accesskit();
    settle(&mut h);
    // A late source-status reply can reflow the selected-source controls.
    // Activate the native control by identity, as an accessibility client does.
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, "Sync database metadata")
        .click_accesskit();
    settle(&mut h);
    step_until(&mut h, "capture source published", |s| {
        s.databases
            .connections
            .first()
            .is_some_and(|c| c.read_view.is_some() && c.active_operation_id.is_none())
    });
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, "Relationships")
        .click_accesskit();
    settle(&mut h);
    struct Release(Gate);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.open();
        }
    }
    let release = Release(inventory.clone());
    inventory.close();
    // Legal source projection when publication changes between lease preparation and read:
    // status sees the new head while no earlier capture can be attached to that head.
    h.state_mut().databases.connections[0].read_view = None;
    h.state_mut().databases.view = None;
    h.state_mut().databases.read_pin = None;
    h.get_by_role_and_label(Role::Button, "Load database relationships")
        .click_accesskit();
    h.step();
    assert!(
        h.state().databases.read_loading,
        "the explicit read intent waits for a fresh owner capture"
    );
    assert!(
        h.state().databases.error.is_none(),
        "a publication transition is not confirmed absence"
    );
    drop(release);
    step_until(&mut h, "fresh-capture relationship read", |s| {
        !s.databases.read_loading
            && s.databases
                .relations
                .iter()
                .any(|r| r.relation.0 == "HAS_COLUMN")
    });
    assert!(h.state().databases.read_pin.is_some());
    assert!(
        h.state().databases.error.is_none(),
        "{:?}",
        h.state().databases.error
    );
    settle(&mut h);
}

fn configured_source(
    backend: &dyn devworks_desktop::backend::Backend,
    config: serde_json::Value,
) -> devworks_core::protocol::records::SchemaConnection {
    use devworks_core::protocol::{
        CommandReply,
        results::{DomainDecision, OperationResult},
    };
    let frame=serde_json::json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":5,"requestKey":backend.token("review-source"),"body":{"kind":"schemas.configure","scope":{"kind":"Workspace"},"config":config}}).to_string();
    let CommandReply::Recorded { receipt, .. } = backend
        .command("schemas.configure", frame.as_bytes())
        .unwrap()
    else {
        panic!("configure receipt")
    };
    let DomainDecision::Applied {
        result: OperationResult::SchemasConfigure { value },
    } = receipt.decision
    else {
        panic!("configure applied")
    };
    value
}
#[test]
fn native_database_review_selecting_a_source_clears_foreign_endpoint_filter() {
    use devworks_core::protocol::{CommandReply, results::DomainDecision};
    let _native_case = native_runtime_case();
    let root = Root::new("source-endpoint-selection");
    let path = root.file("source.sqlite", b"");
    rusqlite::Connection::open(&path).unwrap().execute_batch("CREATE TABLE parent(id INTEGER PRIMARY KEY); CREATE TABLE child(id INT,parent_id INT REFERENCES parent(id));").unwrap();
    let engine = start(&root);
    let b = engine.backend();
    let mut ids = Vec::new();
    for name in ["Source A", "Source B"] {
        let source = configured_source(
            &*b,
            serde_json::json!({"name":name,"engine":"Sqlite","endpoint":path,"catalogs":[],"schemas":[],"includePatterns":[],"excludePatterns":[],"writesEnabled":false,"enabled":true,"projectAccess":[],"workspaceExposed":false,"exposureMode":"SchemaOnly","authentication":"File"}),
        );
        let frame=serde_json::json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":5,"requestKey":b.token("review-index"),"body":{"kind":"schemas.index","connectionId":source.id,"expectedRevision":source.revision}}).to_string();
        assert!(
            matches!(b.command("schemas.index",frame.as_bytes()).unwrap(),CommandReply::Recorded{receipt,..} if matches!(receipt.decision,DomainDecision::Accepted{..}))
        );
        ids.push(source.id);
    }
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1200.0);
    h.state_mut().navigate(Route::Databases);
    step_until(&mut h, "both source views published", |s| {
        s.databases.connections.len() == 2
            && s.databases
                .connections
                .iter()
                .all(|c| c.current_view_id.is_some() && c.active_operation_id.is_none())
    });
    settle(&mut h);
    h.get_by_role_and_label(Role::Button, "Source A")
        .click_accesskit();
    settle(&mut h);
    let parent = h
        .state()
        .databases
        .objects
        .iter()
        .find(|o| {
            o.metadata.as_ref().is_some_and(|m| m.name.0 == "parent")
                && matches!(
                    o.kind,
                    devworks_core::protocol::records::MetadataObjectKind::Table
                )
        })
        .unwrap()
        .clone();
    click(&mut h, Role::Button, "Table main.parent");
    click(&mut h, Role::Button, "Inspect schema relationships");
    h.query_all_by_role_and_label(Role::Button, "Inspect relationship target: main.parent")
        .next()
        .unwrap()
        .click_accesskit();
    settle(&mut h);
    assert_eq!(
        h.state().databases.object_id.as_deref(),
        Some(parent.id.as_str())
    );
    h.get_by_role_and_label(Role::Button, "Source B")
        .click_accesskit();
    settle(&mut h);
    let next = h
        .state()
        .databases
        .objects
        .iter()
        .find(|o| {
            o.metadata.as_ref().is_some_and(|m| m.name.0 == "parent")
                && matches!(
                    o.kind,
                    devworks_core::protocol::records::MetadataObjectKind::Table
                )
        })
        .expect("second source's actual parent is visible");
    assert_ne!(
        next.id, parent.id,
        "source identity is part of object identity"
    );
    assert_eq!(
        h.state().databases.selected.as_deref(),
        Some(ids[1].as_str())
    );
    assert!(h.state().databases.error.is_none());
}

struct PasswordMemory {
    inner: devworks_core::ports::secrets::SessionSecrets,
    writes: std::sync::Mutex<Vec<devworks_core::ports::secrets::SecretKey>>,
}
impl devworks_core::ports::secrets::SecretStore for PasswordMemory {
    fn name(&self) -> &'static str {
        "Controlled password memory"
    }
    fn put(
        &self,
        key: &devworks_core::ports::secrets::SecretKey,
        value: &devworks_core::ports::secrets::Secret,
    ) -> Result<(), devworks_core::ports::secrets::SecretError> {
        self.inner.put(key, value)?;
        self.writes.lock().unwrap().push(key.clone());
        Ok(())
    }
    fn get(
        &self,
        key: &devworks_core::ports::secrets::SecretKey,
    ) -> Result<
        Option<devworks_core::ports::secrets::Secret>,
        devworks_core::ports::secrets::SecretError,
    > {
        self.inner.get(key)
    }
    fn delete(
        &self,
        key: &devworks_core::ports::secrets::SecretKey,
    ) -> Result<bool, devworks_core::ports::secrets::SecretError> {
        self.inner.delete(key)
    }
    fn list(
        &self,
        namespace: &str,
    ) -> Result<Vec<devworks_core::protocol::Id>, devworks_core::ports::secrets::SecretError> {
        self.inner.list(namespace)
    }
}
fn corrected_password_save(edit: bool) {
    use devworks_core::ports::secrets::{Secret, SecretStore};
    let _native_case = native_runtime_case();
    let root = Root::new(if edit {
        "password-retry-edit"
    } else {
        "password-retry-create"
    });
    let secrets = std::sync::Arc::new(PasswordMemory {
        inner: devworks_core::ports::secrets::SessionSecrets::new(),
        writes: Default::default(),
    });
    let mut cfg = config(&root.0);
    cfg.secrets = Some(secrets.clone());
    let engine = devworks_desktop::host::Engine::start(cfg).unwrap();
    let b = engine.backend();
    let old = if edit {
        let old = b
            .stage_database_password(Secret::new("controlled-old-password"))
            .unwrap();
        configured_source(
            &*b,
            serde_json::json!({"name":"Password retry","engine":"MySql","endpoint":"mysql://127.0.0.1:9/unused","credentialRef":old,"catalogs":[],"schemas":[],"includePatterns":[],"excludePatterns":[],"writesEnabled":false,"enabled":true,"projectAccess":[],"workspaceExposed":false,"exposureMode":"SchemaOnly","authentication":"Password","username":"metadata_reader","tls":"LoopbackPlaintext"}),
        );
        Some(old)
    } else {
        None
    };
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1200.0);
    h.state_mut().navigate(Route::Databases);
    settle(&mut h);
    if edit {
        h.get_by_role_and_label(Role::Button, "Edit Password retry")
            .click_accesskit();
        settle(&mut h);
    } else {
        click(&mut h, Role::Button, "New database source");
        replace(&mut h, "Database source name", "Password retry");
        click(&mut h, Role::ComboBox, "Database engine");
        h.get_by_label("MySql").click_accesskit();
        settle(&mut h);
    }
    replace(&mut h, "Database username", "metadata_reader");
    replace(&mut h, "Database file or endpoint", "invalid-endpoint");
    replace(
        &mut h,
        "Database password (blank keeps saved password)",
        "controlled-replacement-password",
    );
    h.get_by_role_and_label(Role::Button, "Save database source")
        .click_accesskit();
    settle(&mut h);
    assert!(!h.state().databases.busy && h.state().databases.error.is_some());
    let key = secrets
        .writes
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|k| k.namespace.ends_with(":schema-sources"))
        .unwrap()
        .clone();
    let draft = h.state().databases.draft.as_ref().unwrap();
    assert!(draft.password.is_empty());
    assert_eq!(
        draft
            .config
            .get("credentialRef")
            .and_then(serde_json::Value::as_str),
        Some(key.handle.as_str()),
        "rejected save retains only the new opaque handle"
    );
    if let Some(old) = old {
        assert_ne!(old, key.handle);
    }
    assert!(
        !h.state()
            .databases
            .pending_memory()
            .to_string()
            .contains("controlled-replacement-password")
    );
    replace(
        &mut h,
        "Database file or endpoint",
        "mysql://127.0.0.1:9/unused",
    );
    h.get_by_role_and_label(Role::Button, "Save database source")
        .click_accesskit();
    settle(&mut h);
    assert!(
        h.state().databases.draft.is_none(),
        "{:?}",
        h.state().databases.error
    );
    assert_eq!(
        h.state().databases.connections[0].credential_ref.as_ref(),
        Some(&key.handle)
    );
    assert!(
        secrets
            .get(&key)
            .unwrap()
            .is_some_and(|v| v.expose() == "controlled-replacement-password")
    );
}
#[test]
fn native_database_review_rejected_new_source_keeps_staged_password() {
    corrected_password_save(false);
}
#[test]
fn native_database_review_rejected_edit_keeps_replacement_password() {
    corrected_password_save(true);
}
