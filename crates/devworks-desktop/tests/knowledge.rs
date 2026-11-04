//! The Knowledge workspace over the real runtime, driven headlessly: manual creation, word and
//! tag search, entry detail with provenance, evidence and revisions, a lifecycle change and a
//! lesson that takes effect at once. The screen reaches the same `knowledge.*` operations an
//! agent tool does; nothing here runs inference.
mod common;

use common::*;
use devworks_desktop::knowledge_screen::{self, Change};
use devworks_desktop::ops;
use devworks_desktop::route::Route;
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::{NodeT, Queryable};
use serde_json::json;

fn type_into(
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    label: &str,
    text: &str,
) {
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, label)
        .type_text(text);
    h.step();
}

fn scope_label(h: &egui_kittest::Harness<'_, devworks_desktop::shell::Shell>) -> String {
    h.get_by_role_and_label(Role::ComboBox, "Knowledge scope")
        .value()
        .unwrap_or_default()
}

#[test]
fn the_scope_comes_from_visible_projects_and_follows_the_navigation() {
    let root = Root::new("knowledge-scope");
    let engine = start(&root);
    let backend = engine.backend();
    let made = ops::project_changed(backend.command(
        "projects.create",
        &ops::project_create(&*backend, "Engine", "", &[]),
    ))
    .unwrap();
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    h.state_mut().navigate(Route::Knowledge);
    settle(&mut h);
    assert_eq!(scope_label(&h), knowledge_screen::GENERAL);
    assert_eq!(
        h.state().knowledge.projects,
        vec![(made.id.0.as_str().to_owned(), "Engine".to_owned())],
        "the choices are the listed projects"
    );
    // Selecting the project in the navigation scopes Knowledge to it.
    h.state_mut()
        .select_project(Some(made.id.0.as_str().to_owned()));
    settle(&mut h);
    assert_eq!(scope_label(&h), "Engine");
    click(&mut h, Role::Button, "New entry");
    type_into(&mut h, "Entry text", "Engine builds with cargo");
    click(&mut h, Role::Button, "Save entry");
    assert!(has_text(&h, "Entry recorded."));
    let page = |scope: serde_json::Value| match ops::query_result(backend.query(
        "knowledge.search",
        &knowledge_screen::query(
            &*backend,
            "knowledge.search",
            json!({"scope": scope, "text": "cargo", "tags": [], "kinds": [],
                    "page": {"limit": 10}, "entityLimit": 10, "claimLimit": 10}),
        ),
    ))
    .unwrap()
    .0
    {
        devworks_core::protocol::results::OperationResult::KnowledgeSearch { value } => {
            value.items.0.len()
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(
        page(json!({"kind": "Project", "projectId": made.id.0.as_str()})),
        1
    );
    assert_eq!(
        page(json!({"kind": "Workspace"})),
        0,
        "it stays in the project"
    );
    // A project that is no longer listed is never a scope: general Knowledge is.
    assert!(h.state_mut().knowledge.set_projects(Vec::new()));
    assert!(h.state().knowledge.project.is_none());
    assert!(
        !h.state_mut()
            .knowledge
            .follow_navigation(Some("not-a-listed-project"))
    );
}

#[test]
fn a_file_is_imported_by_dialog_or_drop_after_an_exact_preview() {
    let root = Root::new("knowledge-import");
    let engine = start(&root);
    let file = root.file(
        "memory.json",
        serde_json::to_vec(
            &json!({"format": "devworks/knowledge-import", "candidates": [
            {"entry": {"kind": "decision", "summary": "Use SQLite for Knowledge"},
                "sourceFile": "MEMORY.md", "verification": "verified"},
            {"entry": {"kind": "fact", "summary": "My editor is vim"},
                "sourceFile": "MEMORY.md", "verification": "personal"}]}),
        )
        .unwrap()
        .as_slice(),
    );
    let script = Script::default();
    script.files.lock().unwrap().push(vec![file.clone()]);
    let mut h = harness(shell(&engine, &root, script), 1440.0, 900.0);
    settle(&mut h);
    h.state_mut().navigate(Route::Knowledge);
    settle(&mut h);
    // The file chooser stores the file as the user's artifact, then previews exactly.
    click(&mut h, Role::Button, "Import from file");
    assert!(
        has_text(&h, "Import memory.json"),
        "{:?}",
        h.state().knowledge.import
    );
    assert!(has_text(&h, "would store") && has_text(&h, "personal"));
    assert!(has_text(&h, "stores 1 of 2 candidates"));
    assert!(
        h.state().knowledge.items.is_empty(),
        "nothing is imported before Confirm"
    );
    click(&mut h, Role::Button, "Confirm import");
    assert!(has_text(&h, "Imported 1 entries from memory.json"));
    assert!(has_text(&h, "stored"));
    assert_eq!(h.state().knowledge.items.len(), 1);
    click(&mut h, Role::Button, "Close import");
    // Dropping the same file on the screen stores a new copy and previews it: the entry it
    // imported is now an active duplicate, so nothing would be imported twice.
    h.input_mut()
        .dropped_files
        .push(std::sync::Arc::new(Dropped(file)));
    settle(&mut h);
    assert!(has_text(&h, "Import memory.json"));
    assert!(has_text(&h, "duplicate"));
    assert!(has_text(&h, "stores 0 of 2 candidates"));
    click(&mut h, Role::Button, "Cancel import");
    assert!(h.state().knowledge.import.is_none());
}

#[test]
fn entries_are_created_searched_inspected_and_changed_through_the_shared_operations() {
    let root = Root::new("knowledge");
    let engine = start(&root);
    let backend = engine.backend();
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    h.state_mut().navigate(Route::Knowledge);
    settle(&mut h);
    assert!(has_text(&h, "No entries match"), "an empty store says so");
    assert!(
        has_text(&h, "Word search only"),
        "retrieval reports lexical-only mode, never vector search"
    );
    // Documents belong to their own item and say so.
    click(&mut h, Role::Button, "Documents");
    assert!(has_text(&h, "DW-35"));
    click(&mut h, Role::Button, "Entries");

    // Manual creation of a decision.
    click(&mut h, Role::Button, "New entry");
    type_into(&mut h, "Entry subject", "Storage");
    type_into(
        &mut h,
        "Entry text",
        "Use SQLite with FTS5 for Knowledge search",
    );
    type_into(&mut h, "Entry tags", "Storage, decisions");
    click(&mut h, Role::Button, "Save entry");
    assert!(
        has_text(&h, "Entry recorded."),
        "{:?}",
        h.state().knowledge.message
    );
    assert_eq!(h.state().knowledge.items.len(), 1);
    assert!(
        h.state().knowledge.draft.is_none(),
        "the draft is cleared once recorded"
    );

    // Word search (case-insensitive) and tag filter reach the same claim.
    type_into(&mut h, "Search Knowledge", "fts5 KNOWLEDGE");
    click(&mut h, Role::Button, "Search");
    assert_eq!(h.state().knowledge.items.len(), 1);
    type_into(&mut h, "Filter by tags", "missing-tag");
    click(&mut h, Role::Button, "Search");
    assert!(h.state().knowledge.items.is_empty());
    h.state_mut().knowledge.tags.clear();
    h.state_mut().knowledge.text.clear();
    click(&mut h, Role::Button, "Search");

    // Detail: provenance, evidence and revision history.
    let label = h.state().knowledge.items[0]
        .value
        .as_ref()
        .unwrap()
        .0
        .clone();
    h.get_by_label(&format!("Fact  ·  {label}")).click();
    settle(&mut h);
    assert_eq!(
        h.state().knowledge.selected.as_ref(),
        Some(&h.state().knowledge.items[0].id)
    );
    assert!(has_text(&h, "Origin User"), "the user is the origin");
    assert!(has_text(&h, "Revisions"));
    assert!(has_text(&h, "No source evidence recorded."));

    // A lifecycle change needs a reason, and is applied only when recorded.
    click(&mut h, Role::Button, Change::Retract.label());
    assert!(has_text(&h, "Retract needs a reason."));
    type_into(&mut h, "Change reason", "superseded by the vector plan");
    click(&mut h, Role::Button, Change::Retract.label());
    assert!(has_text(&h, "Retract recorded."));
    let page = ops::query_result(backend.query(
        "knowledge.search",
        &knowledge_screen::query(
            &*backend,
            "knowledge.search",
            json!({"scope": {"kind": "Workspace"}, "text": "sqlite", "tags": [], "kinds": [],
                "page": {"limit": 10}, "entityLimit": 10, "claimLimit": 10,
                "states": ["Retracted"]}),
        ),
    ))
    .unwrap();
    match page.0 {
        devworks_core::protocol::results::OperationResult::KnowledgeSearch { value } => {
            assert_eq!(
                value.items.0.len(),
                1,
                "the agent tool path sees the same change"
            );
        }
        other => panic!("{other:?}"),
    }

    // A lesson in the user's own words takes effect at once.
    click(&mut h, Role::Button, "New entry");
    h.state_mut().knowledge.draft.as_mut().unwrap().entry_kind = "Lesson";
    settle(&mut h);
    type_into(&mut h, "Entry text", "Run cargo fmt before committing");
    type_into(&mut h, "Lesson activities", "implementing");
    click(&mut h, Role::Button, "Save entry");
    assert!(has_text(&h, "Entry recorded."));
    h.state_mut().knowledge.lessons_only = true;
    click(&mut h, Role::Button, "Search");
    let items = &h.state().knowledge.items;
    assert_eq!(items.len(), 1);
    let lesson = items[0].lesson.as_ref().unwrap();
    assert_eq!(format!("{:?}", lesson.rule_state), "Effective");

    // Cleanup by source: previewed exactly, then deleted by the user.
    let recorded = ops::command_result(backend.command(
        "knowledge.record",
        &knowledge_screen::command(
            &*backend,
            "knowledge.record",
            json!({"scope": {"kind": "Workspace"}, "batch": {"entities": [{"clientId": "s",
                "label": "Run 9", "entityType": "run", "aliases": []}], "evidence": [],
                "claims": [{"kind": "Fact", "subjectId": "s", "predicate": "observed",
                    "value": "flaky test output", "confidence": 0.5, "evidenceIds": [],
                    "tags": [], "source": "test-run-9"}]}}),
        ),
    ));
    assert!(recorded.is_ok(), "{recorded:?}");
    h.state_mut().knowledge.lessons_only = false;
    h.get_by_label("Clean up by source").click();
    settle(&mut h);
    type_into(&mut h, "Cleanup source", "test-run-9");
    click(&mut h, Role::Button, "Preview deletion");
    assert!(
        has_text(&h, "tombstones 1 claims"),
        "the preview counts what it removes"
    );
    click(&mut h, Role::Button, "Delete previewed entries");
    assert!(has_text(&h, "Delete by source recorded."));
    assert!(h.state().knowledge.source_preview.is_none());
}

#[test]
fn an_unknown_write_blocks_new_saves_and_is_confirmed_once_at_the_next_launch() {
    let root = Root::new("knowledge-unknown");
    let engine = start(&root);
    let real = engine.backend();
    // More losses than automatic resubmissions: the outcome stays unknown.
    let lossy = LoseAcks::over(real.clone(), "knowledge.record", 4);
    let mut h = harness(
        shell_with(Ok(lossy.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    h.state_mut().navigate(Route::Knowledge);
    settle(&mut h);
    click(&mut h, Role::Button, "New entry");
    type_into(&mut h, "Entry subject", "Storage");
    type_into(&mut h, "Entry text", "Keep the unknown write");
    click(&mut h, Role::Button, "Save entry");
    assert!(
        has_text(&h, "the outcome is not confirmed yet"),
        "{:?}",
        h.state().knowledge.message
    );
    // Saves stay blocked: a new request (a fresh key) would replace the unknown one.
    assert!(h.state().knowledge.writing.is_some());
    let sent = lossy.request_keys().len();
    let save = h.query_by_role_and_label(Role::Button, "Save entry");
    assert!(save.is_none_or(|n| n.accesskit_node().is_disabled()));
    // Even if the screen's lock were lost, the slot itself refuses a replacement.
    h.state_mut().knowledge.writing = None;
    settle(&mut h);
    click(&mut h, Role::Button, "Save entry");
    assert!(
        has_text(&h, "Use Confirm outcomes first"),
        "{:?}",
        h.state().knowledge.message
    );
    assert!(h.state().knowledge.writing.is_some());
    assert_eq!(lossy.request_keys().len(), sent, "nothing new was sent");
    h.state_mut().on_exit();
    drop(h);
    // Relaunched offline (nothing can be confirmed): the lock is restored from memory.
    let mut h = harness(
        shell_with(Err("offline".into()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    assert!(
        h.state().knowledge.writing.is_some(),
        "saves stay blocked after an offline relaunch"
    );
    drop(h);
    // Relaunched while it still cannot be confirmed: the lock stays.
    let still = LoseAcks::over(real.clone(), "knowledge.record", 4);
    let mut h = harness(
        shell_with(Ok(still.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    assert!(
        h.state().knowledge.writing.is_some(),
        "saves stay blocked after a relaunch"
    );
    // Relaunched again: the exact frame is confirmed, and recorded once.
    let again = LoseAcks::over(real.clone(), "knowledge.record", 0);
    h.state_mut().on_exit();
    drop(h);
    let mut h = harness(
        shell_with(Ok(again.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    let mut keys = lossy.request_keys();
    keys.extend(still.request_keys());
    keys.extend(again.request_keys());
    assert!(keys.windows(2).all(|w| w[0] == w[1]), "{keys:?}");
    h.state_mut().navigate(Route::Knowledge);
    settle(&mut h);
    assert!(
        h.state().knowledge.writing.is_none(),
        "known now: saves resume"
    );
    let page = ops::query_result(real.query(
        "knowledge.search",
        &knowledge_screen::query(
            &*real,
            "knowledge.search",
            json!({"scope": {"kind": "Workspace"}, "text": "unknown write", "tags": [],
                "kinds": [], "page": {"limit": 10}, "entityLimit": 10, "claimLimit": 10}),
        ),
    ))
    .unwrap();
    match page.0 {
        devworks_core::protocol::results::OperationResult::KnowledgeSearch { value } => {
            assert_eq!(value.items.0.len(), 1, "recorded exactly once");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_completed_write_clears_only_the_form_it_submitted_and_only_if_unchanged() {
    let root = Root::new("knowledge-forms");
    let engine = start(&root);
    let real = engine.backend();
    // Entry saves reply only when the gate opens (the save commits first).
    let gate = Gate::holding_replies("knowledge.record");
    let verify_gate = Gate::holding_replies("knowledge.verify");
    let mut h = harness(
        shell_with(
            Ok(verify_gate.over(gate.over(real.clone()))),
            &root,
            Script::default(),
            None,
        ),
        1440.0,
        900.0,
    );
    settle(&mut h);
    h.state_mut().navigate(Route::Knowledge);
    settle(&mut h);
    click(&mut h, Role::Button, "New entry");
    type_into(&mut h, "Entry subject", "Storage");
    type_into(&mut h, "Entry text", "First entry");
    click(&mut h, Role::Button, "Save entry");
    assert!(has_text(&h, "Entry recorded."));
    assert!(
        h.state().knowledge.draft.is_none(),
        "the saved draft is cleared"
    );

    // An unrelated change leaves an open draft, a typed reason and typed tags alone.
    let label = h.state().knowledge.items[0]
        .value
        .as_ref()
        .unwrap()
        .0
        .clone();
    h.get_by_label(&format!("Fact  ·  {label}")).click();
    settle(&mut h);
    click(&mut h, Role::Button, "New entry");
    type_into(&mut h, "Entry text", "Unsaved thought");
    type_into(&mut h, "Change reason", "a reason not yet used");
    h.state_mut().knowledge.tag_edit = "kept".into();
    click(&mut h, Role::Button, Change::SaveTags.label());
    assert!(has_text(&h, "Save tags recorded."));
    assert_eq!(
        h.state().knowledge.draft.as_ref().map(|d| d.value.as_str()),
        Some("Unsaved thought")
    );
    assert_eq!(h.state().knowledge.reason, "a reason not yet used");
    // Tags typed while another write completes survive the entry's re-read.
    type_into(&mut h, "Edit tags", ", typed");
    type_into(&mut h, "Change reason", " (now used)");
    click(&mut h, Role::Button, Change::Verify.label());
    assert!(has_text(&h, "Verify recorded."));
    assert_eq!(h.state().knowledge.tag_edit, "kept, typed");
    assert!(
        h.state().knowledge.reason.is_empty(),
        "the reason Verify used is cleared"
    );
    assert!(
        h.state().knowledge.draft.is_some(),
        "Verify never used the draft"
    );

    // Text typed while its own save is pending is kept, not erased by the save's success.
    gate.close();
    h.get_by_role_and_label(Role::Button, "Save entry").click();
    step_until(&mut h, "the entry is being saved", |s| {
        s.knowledge.writing.is_some()
    });
    type_into(&mut h, "Entry text", " and more");
    gate.open();
    settle(&mut h);
    assert!(
        has_text(&h, "The edits made while it was saving stay in the form"),
        "{:?}",
        h.state().knowledge.message
    );
    assert_eq!(
        h.state().knowledge.draft.as_ref().map(|d| d.value.as_str()),
        Some("Unsaved thought and more")
    );

    // The same for a reason: one typed further while its change is pending stays.
    type_into(&mut h, "Change reason", "checked again");
    verify_gate.close();
    h.get_by_role_and_label(Role::Button, Change::Verify.label())
        .click();
    step_until(&mut h, "the verification is being recorded", |s| {
        s.knowledge.writing.is_some()
    });
    type_into(&mut h, "Change reason", ", twice");
    verify_gate.open();
    settle(&mut h);
    assert!(has_text(&h, "Verify recorded."));
    assert_eq!(h.state().knowledge.reason, "checked again, twice");
    h.state_mut().on_exit();
}

/// The stored entry `id` as listed.
fn listed(
    h: &egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    id: &devworks_core::protocol::Id,
) -> (String, Vec<String>) {
    let c = h
        .state()
        .knowledge
        .items
        .iter()
        .find(|c| c.id == *id)
        .unwrap_or_else(|| panic!("{id:?} is listed"));
    (
        c.value.as_ref().unwrap().0.clone(),
        c.tags.0.iter().map(|t| t.0.clone()).collect(),
    )
}

/// Saves the open draft while `gate` holds its reply, types `more` into it meanwhile, then
/// lets the reply arrive.
fn save_typing_meanwhile(
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    gate: &Gate,
    more: &str,
) {
    gate.close();
    h.get_by_role_and_label(Role::Button, "Save entry").click();
    step_until(h, "the entry is being saved", |s| {
        s.knowledge.writing.is_some()
    });
    type_into(h, "Entry text", more);
    gate.open();
    settle(h);
    assert!(
        has_text(h, "The edits made while it was saving stay in the form"),
        "{:?}",
        h.state().knowledge.message
    );
}

#[test]
fn an_entry_edited_while_saving_saves_again_onto_the_stored_revision() {
    let root = Root::new("knowledge-rebase");
    let engine = start(&root);
    let real = engine.backend();
    let record = Gate::holding_replies("knowledge.record");
    let revise = Gate::holding_replies("knowledge.revise");
    let mut h = harness(
        shell_with(
            Ok(revise.over(record.over(real.clone()))),
            &root,
            Script::default(),
            None,
        ),
        1440.0,
        900.0,
    );
    settle(&mut h);
    h.state_mut().navigate(Route::Knowledge);
    settle(&mut h);

    // A new entry edited while it saves: the later text edits the entry it created.
    click(&mut h, Role::Button, "New entry");
    type_into(&mut h, "Entry subject", "Storage");
    type_into(&mut h, "Entry text", "First entry");
    type_into(&mut h, "Entry tags", "Storage");
    save_typing_meanwhile(&mut h, &record, ", revised");
    let id = h
        .state()
        .knowledge
        .draft
        .as_ref()
        .unwrap()
        .revises
        .clone()
        .unwrap()
        .0;
    assert_eq!(h.state().knowledge.items.len(), 1);
    // The tags were not edited after the save submitted them: they are the stored ones now
    // (as the store folded them), the text keeps the later edit.
    let stored_tags = listed(&h, &id).1.join(", ");
    assert_ne!(stored_tags, "Storage");
    assert_eq!(
        h.state().knowledge.draft.as_ref().map(|d| d.tags.clone()),
        Some(stored_tags)
    );
    click(&mut h, Role::Button, "Save entry");
    assert!(
        has_text(&h, "Entry recorded."),
        "{:?}",
        h.state().knowledge.message
    );
    assert!(h.state().knowledge.draft.is_none());
    assert_eq!(h.state().knowledge.items.len(), 1, "never a second entry");
    assert_eq!(listed(&h, &id).0, "First entry, revised");

    // An edit of it, changed again while its own save runs, then saved again: the second save
    // applies at the stored revision and keeps the later edit.
    click(&mut h, Role::Button, "Edit");
    type_into(&mut h, "Entry text", " twice");
    save_typing_meanwhile(&mut h, &revise, " and more");
    click(&mut h, Role::Button, "Save entry");
    assert!(
        has_text(&h, "Entry recorded."),
        "{:?}",
        h.state().knowledge.message
    );
    assert!(h.state().knowledge.draft.is_none());
    assert_eq!(listed(&h, &id).0, "First entry, revised twice and more");

    // Another write of the entry while it is being edited: the field the user did not change
    // (tags) takes the stored value, the one they changed keeps theirs, and the save applies
    // without undoing the other write.
    click(&mut h, Role::Button, "Edit");
    type_into(&mut h, "Entry text", ", final");
    h.state_mut().knowledge.tag_edit = "fresh".into();
    click(&mut h, Role::Button, Change::SaveTags.label());
    assert!(has_text(&h, "Save tags recorded."));
    assert_eq!(
        h.state().knowledge.draft.as_ref().map(|d| d.tags.as_str()),
        Some("fresh")
    );
    click(&mut h, Role::Button, "Save entry");
    assert!(
        has_text(&h, "Entry recorded."),
        "{:?}",
        h.state().knowledge.message
    );
    let (text, tags) = listed(&h, &id);
    assert_eq!(text, "First entry, revised twice and more, final");
    assert!(tags.contains(&"fresh".to_owned()), "{tags:?}");
    h.state_mut().on_exit();
}

fn import_artifacts(backend: &dyn devworks_desktop::backend::Backend) -> usize {
    ops::artifacts(backend.query("artifacts.list", &ops::workspace_artifacts(backend)))
        .unwrap()
        .0
        .into_iter()
        .filter(|a| a.name.0 == "memory.json")
        .count()
}

#[test]
fn an_unconfirmed_import_upload_is_confirmed_with_its_exact_frame_never_stored_twice() {
    let root = Root::new("knowledge-import-unknown");
    let engine = start(&root);
    let real = engine.backend();
    let file = root.file(
        "memory.json",
        serde_json::to_vec(
            &json!({"format": "devworks/knowledge-import", "candidates": [
            {"entry": {"kind": "decision", "summary": "Use SQLite for Knowledge"},
                "sourceFile": "MEMORY.md", "verification": "verified"}]}),
        )
        .unwrap()
        .as_slice(),
    );
    // The save commits, but every acknowledgment is lost, through the automatic resubmissions.
    let lossy = LoseAcks::over(real.clone(), "artifacts.save", 4);
    let script = Script::default();
    script.files.lock().unwrap().push(vec![file.clone()]);
    let mut h = harness(
        shell_with(Ok(lossy.clone()), &root, script, None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    h.state_mut().navigate(Route::Knowledge);
    settle(&mut h);
    click(&mut h, Role::Button, "Import from file");
    assert!(
        has_text(&h, "not yet confirmed whether the file was stored"),
        "{:?}",
        h.state().knowledge.import
    );
    assert_eq!(import_artifacts(&*real), 1, "it did commit");
    // Neither a new import nor cancelling can replace it while it is unconfirmed.
    let import = h.query_by_role_and_label(Role::Button, "Import from file");
    assert!(import.is_none_or(|n| n.accesskit_node().is_disabled()));
    let keys = lossy.request_keys();

    // Closed and relaunched: the frame is kept and confirmed at the next launch, which
    // replays the recorded save (the same request key) and previews it.
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
    assert!(
        has_text(&h, "Import memory.json"),
        "{:?}",
        h.state().knowledge.import
    );
    let after = lossy.request_keys();
    assert!(after.len() > keys.len(), "it was resubmitted");
    assert!(
        after.iter().all(|k| *k == keys[0]),
        "every admission is the same request: {after:?}"
    );
    assert_eq!(import_artifacts(&*real), 1, "never stored twice");
    click(&mut h, Role::Button, "Confirm import");
    assert!(has_text(&h, "Imported 1 entries from memory.json"));
    h.state_mut().on_exit();
}

#[test]
fn an_unconfirmed_import_upload_is_confirmed_by_confirm_outcomes() {
    let root = Root::new("knowledge-import-confirm");
    let engine = start(&root);
    let real = engine.backend();
    let file = root.file(
        "memory.json",
        serde_json::to_vec(
            &json!({"format": "devworks/knowledge-import", "candidates": [
            {"entry": {"kind": "fact", "summary": "Knowledge imports confirm once"},
                "sourceFile": "MEMORY.md", "verification": "verified"}]}),
        )
        .unwrap()
        .as_slice(),
    );
    let lossy = LoseAcks::over(real.clone(), "artifacts.save", 4);
    let script = Script::default();
    script.files.lock().unwrap().push(vec![file]);
    let mut h = harness(
        shell_with(Ok(lossy.clone()), &root, script, None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    h.state_mut().navigate(Route::Knowledge);
    settle(&mut h);
    click(&mut h, Role::Button, "Import from file");
    assert!(has_text(
        &h,
        "not yet confirmed whether the file was stored"
    ));
    let first = lossy.request_keys()[0].clone();
    click(&mut h, Role::Button, "Confirm outcomes");
    assert!(
        has_text(&h, "Import memory.json"),
        "{:?}",
        h.state().knowledge.import
    );
    assert!(lossy.request_keys().iter().all(|k| *k == first));
    assert_eq!(import_artifacts(&*real), 1, "never stored twice");
    h.state_mut().on_exit();
}
