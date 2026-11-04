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
use egui_kittest::kittest::Queryable;
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
