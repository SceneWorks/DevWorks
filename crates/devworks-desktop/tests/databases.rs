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
        "all enabled sources were captured before admissions changed inventory cursors"
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
    assert!(h.query_by_label("Object coverage: Partial").is_some());
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
