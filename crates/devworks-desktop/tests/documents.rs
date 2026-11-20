//! Drive the native Documents tab over the application's real SQLite and dispatcher.
mod common;

use common::*;
use devworks_core::{
    operations::{OperationInput, documents::Input},
    protocol::{data::Citation, results::OperationResult},
};
use devworks_desktop::{documents_ops, frame, ops, route::Route, shell::Shell};
use eframe::egui::accesskit::Role;
use egui_kittest::{Harness, kittest::Queryable};
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
    h.input_mut().dropped_files.push(Arc::new(Dropped(dropped)));
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
    h.get_by_role_and_label(Role::ComboBox, "Destination project")
        .click();
    settle(&mut h);
    h.get_by_label("Archive B").click();
    settle(&mut h);
    click(&mut h, Role::Button, "Share document");
    assert!(has_text(&h, "Document shared."));

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
    click(&mut h, Role::Button, "Move document");
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
    let replay = lossy.request_keys();
    assert!(replay.len() > original_keys.len());
    assert!(replay.iter().all(|key| key == &original_keys[0]));
}
