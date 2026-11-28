//! Drive the native Documents tab over the application's real SQLite and dispatcher.
mod common;

use common::*;
use devworks_core::{
    operations::{
        OperationInput,
        documents::{ImportInput, Input},
    },
    protocol::{
        data::{Citation, DataScope},
        records::DocumentResult,
        results::OperationResult,
    },
};
use devworks_desktop::{
    documents_ops, documents_screen::Change, frame, ops, route::Route, shell::Shell,
};
use eframe::egui::accesskit::Role;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use std::sync::Arc;

fn open_tab(h: &mut Harness<'_, Shell>) {
    h.state_mut().navigate(Route::Knowledge);
    settle(h);
    click(h, Role::Button, "Documents");
    assert!(h.state().knowledge.documents.list.loaded);
}

fn type_search(h: &mut Harness<'_, Shell>, text: &str) {
    h.get_by_role_and_label(Role::TextInput, "Search documents")
        .focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, "Search documents")
        .type_text(text);
    h.step();
    click(h, Role::Button, "Search documents");
}

fn wait_for_index(h: &mut Harness<'_, Shell>) {
    step_until(h, "the exact version has a published index", |shell| {
        shell
            .knowledge
            .documents
            .detail
            .as_ref()
            .and_then(|d| d.index_activity.as_ref())
            .is_some_and(|activity| {
                activity
                    .chunk_count
                    .as_ref()
                    .is_some_and(|count| count.get() > 0)
            })
    });
}

/// Waits out a background access re-check: while it runs the detail pane shows only
/// "Checking access to retained sources…" (DW-463), so its text and controls are absent.
fn wait_for_access(h: &mut Harness<'_, Shell>) {
    step_until(
        h,
        "the document detail is shown after its access check",
        |shell| {
            let docs = &shell.knowledge.documents;
            !docs.validating_access && docs.detail.is_some()
        },
    );
    h.step();
}

fn resolve(b: &dyn devworks_desktop::backend::Backend, citation: Citation) -> String {
    let request = frame::build(
        b,
        frame::Negotiate::Latest,
        Some(OperationInput::Documents(Input::ResolveCitation(
            devworks_core::operations::documents::ResolveCitationInput { citation },
        ))),
    );
    let (OperationResult::DocumentsResolveCitation { value }, _) =
        ops::query_result(b.query("documents.resolve_citation", &request)).unwrap()
    else {
        panic!("real exact citation result");
    };
    value.text.0
}

#[test]
fn document_dialog_index_search_replace_and_remove_preserve_exact_evidence() {
    let root = Root::new("documents-panel");
    let engine = start(&root);
    let backend = engine.backend();
    let first = root.file(
        "observations.md",
        b"# Greenhouse\n\nThe original botanical register.",
    );
    let updated = root.file("updated.md", b"# Greenhouse\n\nNew inventory observations.");
    let script = Script::default();
    script
        .files
        .lock()
        .unwrap()
        .extend([vec![first], vec![updated]]);
    let mut h = harness(shell(&engine, &root, script), 1440.0, 900.0);
    settle(&mut h);
    open_tab(&mut h);
    assert!(has_text(&h, "No documents in this scope."));
    click(&mut h, Role::Button, "Import document…");
    assert!(has_text(&h, "Document imported."));
    assert!(has_text(&h, "Original file · observations.md"));
    assert!(has_text(&h, "Vision route · Unavailable"));
    let original = h
        .state()
        .knowledge
        .documents
        .selected
        .clone()
        .expect("exact imported version");
    assert_eq!(h.state().knowledge.documents.list.items.len(), 1);
    click(&mut h, Role::Button, "View original text");
    assert!(has_text(&h, "The original botanical register."));
    click(&mut h, Role::Button, "Index document");
    wait_for_index(&mut h);
    let activity = h
        .state()
        .knowledge
        .documents
        .detail
        .as_ref()
        .unwrap()
        .index_activity
        .as_ref()
        .unwrap();
    assert!(activity.indexed_at_ms.is_some());
    assert_eq!(activity.chunk_count.as_ref().unwrap().get(), 1);
    assert!(activity.attempts.0.is_empty());

    type_search(&mut h, "botanical");
    let result = h.state().knowledge.documents.results.as_ref().unwrap();
    assert_eq!(result.passages.len(), 1);
    let passage = result.passages[0].clone();
    let devworks_core::protocol::data::Locator::Text {
        first_line,
        last_line,
    } = &passage.citation.locator
    else {
        panic!("Markdown locator");
    };
    let label = if first_line == last_line {
        format!("Open citation · Line {first_line}")
    } else {
        format!("Open citation · Lines {first_line}–{last_line}")
    };
    click(&mut h, Role::Button, &label);
    assert!(has_text(&h, "Exact citation"));
    assert_eq!(
        h.state()
            .knowledge
            .documents
            .citation
            .as_ref()
            .unwrap()
            .text
            .0,
        passage.text
    );

    click(&mut h, Role::Button, "Replace source…");
    let docs = &h.state().knowledge.documents;
    let replacement = docs.selected.as_ref().unwrap();
    assert_eq!(replacement.artifact_id, original.artifact_id);
    assert_ne!(replacement.version_id, original.version_id);
    let detail = docs.detail.as_ref().unwrap();
    assert_eq!(detail.retained_version_count.get(), 2);
    assert_eq!(resolve(&*backend, passage.citation.clone()), passage.text);

    click(&mut h, Role::Button, "Retry indexing");
    wait_for_index(&mut h);
    click(&mut h, Role::Button, "Disable retrieval");
    assert!(
        !h.state()
            .knowledge
            .documents
            .detail
            .as_ref()
            .unwrap()
            .retrieval_enabled
    );
    click(&mut h, Role::Button, "Remove index");
    assert!(has_text(&h, "Index removed."));
    assert_eq!(resolve(&*backend, passage.citation), passage.text);
    let exact = h.state().knowledge.documents.selected.clone().unwrap();
    assert_eq!(
        ops::read_artifact_bytes(&*backend, &exact).unwrap(),
        b"# Greenhouse\n\nNew inventory observations."
    );
}

#[test]
fn dropped_source_is_shared_and_moved_by_the_real_scope_controls() {
    let root = Root::new("documents-scope");
    let engine = start(&root);
    let backend = engine.backend();
    let project = |name| {
        ops::project_changed(backend.command(
            "projects.create",
            &ops::project_create(&*backend, name, "", &[]),
        ))
        .unwrap()
    };
    let a = project("Archive A");
    let b = project("Archive B");
    let dropped = root.file("audit.txt", b"Retained transfer evidence.");
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    h.state_mut()
        .select_project(Some(a.id.0.as_str().to_owned()));
    open_tab(&mut h);
    h.state_mut().chat.coordinator_open = true;
    h.input_mut().dropped_files.push(Arc::new(Dropped(dropped)));
    settle(&mut h);
    h.state_mut().chat.coordinator_open = false;
    settle(&mut h);
    let exact = h
        .state()
        .knowledge
        .documents
        .selected
        .clone()
        .expect("dropped original");
    click(&mut h, Role::Button, "Index document");
    wait_for_index(&mut h);
    wait_for_access(&mut h);
    assert!(has_text(&h, "Owner · Archive A"));
    // Pick while the popup is open: a settle here can run a refresh that re-checks access,
    // which hides the picker for a frame and closes its popup.
    wait_for_access(&mut h);
    h.get_by_role_and_label(Role::ComboBox, "Destination project")
        .click();
    h.step();
    h.step();
    h.query_all_by_label("Archive B")
        .find(|n| n.accesskit_node().role() != Role::ComboBox)
        .expect("Archive B in the destination picker")
        .click();
    settle(&mut h);
    click(&mut h, Role::Button, "Share document");
    assert!(has_text(&h, "Document shared."));
    assert!(has_text(&h, "Shared with Archive B"));

    h.state_mut()
        .select_project(Some(b.id.0.as_str().to_owned()));
    settle(&mut h);
    assert_eq!(
        h.state().knowledge.documents.list.items[0].id,
        exact.artifact_id
    );
    assert_eq!(
        h.state().knowledge.documents.list.items[0]
            .current_version
            .as_ref()
            .unwrap()
            .sha256,
        exact.sha256
    );

    h.state_mut()
        .select_project(Some(a.id.0.as_str().to_owned()));
    settle(&mut h);
    h.get_by_label("audit.txt").click();
    settle(&mut h);
    let before_move = h.state().knowledge.documents.detail.clone().unwrap();
    let stale_disable = || {
        documents_ops::change(
            &*backend,
            Change::Disable,
            before_move.clone(),
            exact.clone(),
            None,
        )
    };
    click(&mut h, Role::Button, "Move document");
    assert!(
        ops::command_result(backend.command_in_project(
            &a.id,
            "documents.disable_retrieval",
            &stale_disable(),
        ))
        .is_err()
    );
    assert!(
        ops::command_result(backend.command("documents.disable_retrieval", &stale_disable()))
            .is_err()
    );
    let read = documents_ops::read(&*backend, exact.clone(), None);
    assert!(ops::query_result(backend.query_in_project(&a.id, "documents.read", &read)).is_err());
    assert!(ops::query_result(backend.query_in_project(&b.id, "documents.read", &read)).is_ok());
    assert!(has_text(&h, "Document moved."));
    assert!(h.state().knowledge.documents.list.items.is_empty());
    h.state_mut()
        .select_project(Some(b.id.0.as_str().to_owned()));
    settle(&mut h);
    let moved = &h.state().knowledge.documents.list.items[0];
    assert_eq!(moved.id, exact.artifact_id);
    assert!(
        matches!(&moved.scope, devworks_core::protocol::data::DataScope::Project { project_id } if project_id == &b.id)
    );
    assert_eq!(
        ops::read_artifact_bytes(&*backend, &exact).unwrap(),
        b"Retained transfer evidence."
    );

    let command = documents_ops::read(&*backend, exact, None);
    let (OperationResult::DocumentsRead { value }, _) =
        ops::query_result(backend.query("documents.read", &command)).unwrap()
    else {
        panic!("native document read");
    };
    assert!(
        matches!(value, devworks_core::protocol::records::DocumentResult::Current(value)
        if value.index_activity.as_ref().and_then(|a| a.chunk_count.as_ref()).is_some())
    );
}

#[test]
fn refresh_discards_cached_content_moved_out_of_general() {
    let root = Root::new("documents-cached-scope");
    let engine = start(&root);
    let backend = engine.backend();
    let target = ops::project_changed(backend.command(
        "projects.create",
        &ops::project_create(&*backend, "Project custody", "", &[]),
    ))
    .unwrap();
    let script = Script::default();
    let file = root.file("workspace.txt", b"Workspace-retained forest evidence.");
    script.files.lock().unwrap().push(vec![file]);
    let mut h = harness(shell(&engine, &root, script), 1440.0, 900.0);
    settle(&mut h);
    open_tab(&mut h);
    click(&mut h, Role::Button, "Import document…");
    click(&mut h, Role::Button, "Index document");
    wait_for_index(&mut h);
    let docs = &h.state().knowledge.documents;
    let detail = docs.detail.clone().unwrap();
    let exact = docs.selected.clone().unwrap();
    type_search(&mut h, "forest");
    click(&mut h, Role::Button, "Open citation · Line 1");
    click(&mut h, Role::Button, "View original text");
    assert!(h.state().knowledge.documents.citation.is_some());
    assert!(h.state().knowledge.documents.original.is_some());

    let change = documents_ops::change(&*backend, Change::Move, detail, exact, Some(target.id));
    ops::command_result(backend.command("documents.move", &change)).unwrap();
    h.state_mut().refresh_visible();
    settle(&mut h);
    let docs = &h.state().knowledge.documents;
    assert!(docs.list.items.is_empty());
    assert!(docs.selected.is_none() && docs.citation.is_none() && docs.original.is_none());
    assert!(docs.results.as_ref().unwrap().passages.is_empty());
    assert!(!has_text(&h, "Workspace-retained forest evidence."));
}

#[test]
fn refresh_keeps_retained_versions_loaded_beyond_the_first_page() {
    let root = Root::new("documents-version-pages");
    let engine = start(&root);
    let backend = engine.backend();
    let script = Script::default();
    let file = root.file("versions.txt", b"Exact retained version history.");
    script.files.lock().unwrap().push(vec![file]);
    let mut h = harness(shell(&engine, &root, script), 1440.0, 1550.0);
    settle(&mut h);
    open_tab(&mut h);
    click(&mut h, Role::Button, "Import document…");
    let mut detail = h.state().knowledge.documents.detail.clone().unwrap();
    let source = detail.selected_version.clone().unwrap();
    for _ in 0..20 {
        let frame = frame::build(
            &*backend,
            frame::Negotiate::Latest,
            Some(OperationInput::Documents(Input::Import(ImportInput {
                scope: DataScope::Workspace,
                title: detail.title,
                source: source.clone(),
                replace_document_id: Some(detail.id),
                expected_revision: Some(detail.revision),
            }))),
        );
        let OperationResult::DocumentsImport {
            value: DocumentResult::Current(next),
        } = ops::command_result(backend.command("documents.import", &frame)).unwrap()
        else {
            panic!("replacement must return the retained document");
        };
        detail = next;
    }
    h.state_mut().refresh_visible();
    settle(&mut h);
    let page = |shell: &Shell| {
        let detail = shell.knowledge.documents.detail.as_ref().unwrap();
        (
            detail.retained_versions.0.len(),
            detail.next_version_cursor.is_some(),
        )
    };
    assert_eq!(page(h.state()), (20, true));
    click(&mut h, Role::Button, "More versions");
    assert_eq!(page(h.state()), (21, false));
    h.state_mut().refresh_visible();
    settle(&mut h);
    assert_eq!(page(h.state()), (21, false));
}

#[test]
fn a_lost_document_source_acknowledgement_is_confirmed_on_restart() {
    let root = Root::new("documents-source-receipt");
    let engine = start(&root);
    let real = engine.backend();
    let lossy = LoseAcks::over(real.clone(), "artifacts.save", 4);
    let file = root.file("receipt.txt", b"One exact imported copy.");
    let script = Script::default();
    script.files.lock().unwrap().push(vec![file]);
    let mut h = harness(
        shell_with(Ok(lossy.clone()), &root, script, None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    open_tab(&mut h);
    click(&mut h, Role::Button, "Import document…");
    assert!(has_text(
        &h,
        "The source save is not confirmed. Use Confirm outcomes."
    ));
    assert!(h.state().knowledge.documents.list.items.is_empty());
    let original_keys = lossy.request_keys();
    assert_eq!(original_keys.len(), 4);
    h.state_mut().on_exit();
    let pending = std::fs::read(root.memory()).unwrap();
    drop(h);

    let mut h = harness(
        shell_with(Ok(lossy.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    h.state_mut().navigate(Route::Knowledge);
    settle(&mut h);
    if h.state().route_memory(Route::Knowledge).tab != devworks_desktop::documents_screen::TAB {
        click(&mut h, Role::Button, "Documents");
    }
    assert_eq!(h.state().knowledge.documents.list.items.len(), 1);
    let actual = &h.state().knowledge.documents.list.items[0];
    assert_eq!(actual.retained_version_count.get(), 1);
    let source = actual.current_version.as_ref().unwrap();
    assert_eq!(
        ops::read_artifact_bytes(&*real, source).unwrap(),
        b"One exact imported copy."
    );
    let imported = actual.id.clone();
    h.state_mut().on_exit();
    drop(h);
    // The import committed, but a crash can leave the earlier source-save frame on disk.
    std::fs::write(root.memory(), pending).unwrap();
    let mut h = harness(
        shell_with(Ok(lossy.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    h.state_mut().navigate(Route::Knowledge);
    settle(&mut h);
    let documents = &h.state().knowledge.documents;
    assert_eq!(documents.list.items.len(), 1);
    assert_eq!(documents.list.items[0].id, imported);
    assert_eq!(documents.list.items[0].retained_version_count.get(), 1);
    let replay = lossy.request_keys();
    assert!(replay.len() > original_keys.len() + 1);
    assert!(replay.iter().all(|key| key == &original_keys[0]));
}
