//! The Knowledge graph from the desktop over the real runtime: claim and entity bundles, an
//! entity's edges, a subgraph, the whole graph, a graph query, one frontier step and a rendered
//! summary; relating two entries; lesson views; and the last write's read-back receipt. Each
//! is the same `knowledge.*` operation an agent tool submits.
mod common;

use common::*;
use devworks_core::protocol::results::OperationResult;
use devworks_desktop::backend::Backend;
use devworks_desktop::frame::fixture;
use devworks_desktop::knowledge_screen;
use devworks_desktop::ops;
use devworks_desktop::route::Route;
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::{NodeT, Queryable};
use serde_json::{Value, json};

type H = egui_kittest::Harness<'static, devworks_desktop::shell::Shell>;

fn type_into(h: &mut H, label: &str, text: &str) {
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, label)
        .type_text(text);
    h.step();
}

/// Picks `option` from the combo box labelled `combo`.
fn pick(h: &mut H, combo: &str, option: &str) {
    click(h, Role::ComboBox, combo);
    h.get_by_label(option).click();
    settle(h);
}

/// Records, as an agent tool does, three entities linked by two claims:
/// Storage uses SQLite, and SQLite provides FTS5 search.
fn record_graph(backend: &dyn Backend) -> (String, String) {
    record_graph_in(backend, json!({"kind": "Workspace"}))
}
fn record_graph_in(backend: &dyn Backend, scope: Value) -> (String, String) {
    let frame = knowledge_screen::command(
        backend,
        fixture::command_input(
            "knowledge.record",
            json!({"scope": scope, "batch": {"evidence": [], "entities": [
                {"clientId": "s", "label": "Storage", "entityType": "topic", "aliases": []},
                {"clientId": "d", "label": "SQLite", "entityType": "component", "aliases": []},
                {"clientId": "f", "label": "FTS5", "entityType": "feature", "aliases": []}],
              "claims": [
                {"kind": "Fact", "subjectId": "s", "predicate": "uses", "objectId": "d",
                 "value": "Storage uses SQLite", "confidence": 0.9, "evidenceIds": [], "tags": []},
                {"kind": "Fact", "subjectId": "d", "predicate": "provides", "objectId": "f",
                 "value": "SQLite provides FTS5 search", "confidence": 0.9, "evidenceIds": [],
                 "tags": []}]}}),
        ),
    );
    match ops::command_result(backend.command("knowledge.record", &frame)) {
        Ok(OperationResult::KnowledgeRecord { value }) => (
            value.claim_ids.0[0].as_str().to_owned(),
            value.claim_ids.0[1].as_str().to_owned(),
        ),
        other => panic!("{other:?}"),
    }
}

/// The agent path's claim bundle of `claim`, as JSON.
fn claim_bundle(backend: &dyn Backend, claim: &str) -> Value {
    let frame = knowledge_screen::query(
        backend,
        fixture::query_input(
            "knowledge.claim_bundle",
            json!({"scope": {"kind": "Workspace"}, "query": {"entityIds": [],
                "claimIds": [claim], "predicates": [], "tags": [], "states": [], "maxDepth": 1,
                "page": {"limit": 10}, "entityLimit": 10, "claimLimit": 10, "evidenceLimit": 10,
                "neighborLimit": 10, "includeSuperseded": false, "includeConflicts": true,
                "includeSupersessionChain": false, "includeEvidence": false}}),
        ),
    );
    match ops::query_result(backend.query("knowledge.claim_bundle", &frame)) {
        Ok((OperationResult::KnowledgeClaimBundle { value }, _)) => {
            serde_json::to_value(value).unwrap()
        }
        other => panic!("{other:?}"),
    }
}

fn open_knowledge(engine: &devworks_desktop::host::Engine, root: &Root) -> H {
    let mut h = harness(shell(engine, root, Script::default()), 1440.0, 1000.0);
    settle(&mut h);
    h.state_mut().navigate(Route::Knowledge);
    settle(&mut h);
    h
}

/// The graph view's title, once rendered.
fn graph_title(h: &H) -> Option<String> {
    h.state().knowledge.graph.view.as_ref().map(|v| v.title())
}

#[test]
fn a_person_opens_bundles_follows_the_graph_one_step_and_reads_its_summary() {
    let root = Root::new("knowledge-graph");
    let engine = start(&root);
    let backend = engine.backend();
    record_graph(&*backend);
    let mut h = open_knowledge(&engine, &root);
    assert_eq!(h.state().knowledge.claims.items.len(), 2);

    // The claim bundle of the selected entry: the claim with its subject and object entities.
    h.get_by_label("Fact  ·  Storage uses SQLite").click();
    settle(&mut h);
    click(&mut h, Role::Button, "Open claim bundle");
    assert!(has_text(&h, "Claim bundle: Storage uses SQLite"));
    assert!(has_text(&h, "Entities (2)"), "{:?}", graph_title(&h));
    assert!(
        h.query_by_role_and_label(Role::Link, "Open claim SQLite provides FTS5 search")
            .is_none(),
        "the other claim is not in this bundle"
    );

    // An entity bundle from the claim bundle: Storage's claims and neighbors.
    click(&mut h, Role::Link, "Open entity Storage");
    assert!(has_text(&h, "Entity bundle: Storage"));
    assert!(
        h.query_by_role_and_label(Role::Link, "Open entity FTS5")
            .is_none(),
        "FTS5 is two hops away"
    );

    // One frontier step reaches SQLite's claim and FTS5; what was shown stays.
    click(&mut h, Role::Button, "Expand one step");
    assert!(
        h.query_by_label("Expanded 1 step").is_some(),
        "one step, singular"
    );
    assert!(
        h.query_by_role_and_label(Role::Link, "Open claim SQLite provides FTS5 search")
            .is_some()
    );
    assert!(
        h.query_by_role_and_label(Role::Link, "Open entity FTS5")
            .is_some()
    );
    assert!(
        h.query_by_role_and_label(Role::Link, "Open claim Storage uses SQLite")
            .is_some(),
        "the bundle's rows stay"
    );

    // The summary of what is shown is published as an artifact and its text is read back.
    click(&mut h, Role::Button, "Render summary");
    let summary = h
        .state()
        .knowledge
        .graph
        .view
        .as_ref()
        .and_then(|v| v.summary.clone())
        .expect("the summary text is shown");
    assert!(summary.contains("Relevant Memory"), "{summary}");
    assert!(summary.contains("**FTS5** (feature)"), "{summary}");
    assert!(has_text(&h, "Relevant Memory"));

    // An entity's edges, a subgraph around what is shown, the whole graph and a graph query.
    click(&mut h, Role::Button, "Entity edges SQLite");
    assert_eq!(graph_title(&h).as_deref(), Some("Edges of SQLite"));
    assert!(has_text(&h, "Entries (2)"), "both edges of SQLite");
    click(&mut h, Role::Button, "Subgraph around this");
    assert_eq!(graph_title(&h).as_deref(), Some("Subgraph"));
    assert!(has_text(&h, "Entities (3)"));
    click(&mut h, Role::Button, "Whole graph");
    assert_eq!(graph_title(&h).as_deref(), Some("Whole graph"));
    assert!(has_text(&h, "Entities (3)"));
    type_into(&mut h, "Query the graph", "FTS5");
    click(&mut h, Role::Button, "Query graph");
    assert_eq!(graph_title(&h).as_deref(), Some("Graph query: FTS5"));
    assert!(has_text(&h, "Answer"), "the query's rendered text is shown");
    click(&mut h, Role::Button, "Close graph");
    assert!(h.state().knowledge.graph.view.is_none());
}

#[test]
fn a_person_relates_two_entries_and_the_relation_is_in_both_bundles() {
    let root = Root::new("knowledge-relate");
    let engine = start(&root);
    let backend = engine.backend();
    let (uses, provides) = record_graph(&*backend);
    let mut h = open_knowledge(&engine, &root);

    h.get_by_label("Fact  ·  Storage uses SQLite").click();
    settle(&mut h);
    assert!(has_text(&h, "No relations recorded."));
    let relate = h.get_by_role_and_label(Role::Button, "Relate entries");
    assert!(
        relate.accesskit_node().is_disabled(),
        "nothing is picked yet"
    );
    pick(&mut h, "Relation", "Supports");
    pick(&mut h, "Related entry", "SQLite provides FTS5 search");
    // While another Knowledge write is in flight the button is disabled.
    h.state_mut().knowledge.writing = Some("Entry".into());
    settle(&mut h);
    assert!(
        h.get_by_role_and_label(Role::Button, "Relate entries")
            .accesskit_node()
            .is_disabled()
    );
    h.state_mut().knowledge.writing = None;
    click(&mut h, Role::Button, "Relate entries");
    assert!(
        has_text(&h, "Relate recorded."),
        "{:?}",
        h.state().knowledge.message
    );
    assert!(
        has_text(&h, "Last change: Relate · complete"),
        "the write's receipt is read back: {:?}",
        h.state().knowledge.last_write
    );
    assert!(has_text(
        &h,
        "This entry supports SQLite provides FTS5 search"
    ));
    assert!(
        h.state().knowledge.relate_object.is_none(),
        "the pick is used"
    );

    // The relation is in the claim bundle of each entry, on screen and on the agent path.
    click(&mut h, Role::Button, "Open claim bundle");
    assert!(has_text(&h, "Claim bundle: Storage uses SQLite"));
    let line = "\"Storage uses SQLite\" supports \"SQLite provides FTS5 search\"";
    assert!(has_text(&h, line));
    h.get_by_label("Fact  ·  SQLite provides FTS5 search")
        .click();
    settle(&mut h);
    assert!(has_text(&h, "Storage uses SQLite supports this entry"));
    click(&mut h, Role::Button, "Open claim bundle");
    assert!(has_text(&h, "Claim bundle: SQLite provides FTS5 search"));
    assert!(has_text(&h, line));
    for claim in [&uses, &provides] {
        let bundle = claim_bundle(&*backend, claim);
        let relations = bundle["relations"].as_array().expect("relations");
        assert!(
            relations.iter().any(|r| r["subjectId"] == json!(uses)
                && r["objectId"] == json!(provides)
                && r["predicate"] == json!("supports")),
            "{bundle}"
        );
    }

    // A refusal reads in plain words: two facts are not lessons to merge.
    pick(&mut h, "Relation", "Same lesson as");
    pick(&mut h, "Related entry", "Storage uses SQLite");
    click(&mut h, Role::Button, "Relate entries");
    assert!(
        has_text(
            &h,
            "Relate was not recorded. Same lesson as and Unmerge from relate two lessons: \
             choose two lessons."
        ),
        "{:?}",
        h.state().knowledge.message
    );
}

#[test]
fn a_lesson_view_lists_tagged_lessons_by_view_and_rule_state() {
    let root = Root::new("knowledge-lessons");
    let engine = start(&root);
    let backend = engine.backend();
    record_graph(&*backend);
    let mut h = open_knowledge(&engine, &root);
    click(&mut h, Role::Button, "New entry");
    h.state_mut().knowledge.draft.as_mut().unwrap().entry_kind = "Lesson";
    settle(&mut h);
    type_into(&mut h, "Entry text", "Run cargo fmt before committing");
    type_into(&mut h, "Lesson activities", "implementing");
    click(&mut h, Role::Button, "Save entry");
    assert!(has_text(&h, "Entry recorded."));
    assert_eq!(h.state().knowledge.claims.items.len(), 3);

    click(&mut h, Role::CheckBox, "Lessons");
    pick(&mut h, "Lesson view", "All lessons");
    let items = &h.state().knowledge.claims.items;
    assert_eq!(items.len(), 1, "only the lesson");
    assert!(items[0].lesson.is_some());
    type_into(&mut h, "Lesson activity", "reviewing");
    click(&mut h, Role::Button, "Search");
    assert!(
        h.state().knowledge.claims.items.is_empty(),
        "it applies when implementing, not reviewing"
    );
    h.state_mut().knowledge.lesson_activity.clear();
    pick(&mut h, "Rule state", "Candidate");
    assert!(
        h.state().knowledge.claims.items.is_empty(),
        "the user's own lesson is effective, not a candidate"
    );
    pick(&mut h, "Rule state", "Effective");
    assert_eq!(h.state().knowledge.claims.items.len(), 1);
}

#[test]
fn a_claim_a_frontier_step_reached_stays_shown_after_relating_it() {
    let root = Root::new("knowledge-graph-refresh");
    let engine = start(&root);
    let backend = engine.backend();
    record_graph(&*backend);
    let mut h = open_knowledge(&engine, &root);
    h.get_by_label("Fact  ·  Storage uses SQLite").click();
    settle(&mut h);
    click(&mut h, Role::Button, "Open subject entity");
    click(&mut h, Role::Button, "Expand one step");
    let reached = "Open claim SQLite provides FTS5 search";
    assert!(h.query_by_role_and_label(Role::Link, reached).is_some());
    // Relating the entry the step reached re-reads the graph after the write.
    pick(&mut h, "Relation", "Depends on");
    pick(&mut h, "Related entry", "SQLite provides FTS5 search");
    click(&mut h, Role::Button, "Relate entries");
    assert!(has_text(&h, "Relate recorded."));
    assert!(
        h.query_by_role_and_label(Role::Link, reached).is_some(),
        "the expanded claim stays after the refresh"
    );
    assert_eq!(graph_title(&h).as_deref(), Some("Entity bundle: Storage"));
    assert!(h.query_by_label("Expanded 1 step").is_some());
    assert!(has_text(
        &h,
        "\"Storage uses SQLite\" depends on \"SQLite provides FTS5 search\""
    ));
}

#[test]
fn a_write_still_running_is_read_again_on_refresh_until_it_is_complete() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let root = Root::new("knowledge-write-status");
    let engine = start(&root);
    let spy = LoseAcks::over(engine.backend(), "no.such.command", 0);
    // While `running`, the runtime reports the write as a delete by source still dispatching.
    let running = Arc::new(AtomicBool::new(true));
    let reads = Arc::new(AtomicUsize::new(0));
    let (r, n) = (running.clone(), reads.clone());
    spy.rewrite_queries(move |reply| {
        if reply["result"]["kind"] == json!("knowledge.write_status") {
            n.fetch_add(1, Ordering::SeqCst);
            if r.load(Ordering::SeqCst) {
                reply["result"]["value"]["state"] = json!("Dispatching");
            }
        }
    });
    let mut h = harness(
        shell_with(Ok(spy.clone()), &root, Script::default(), None),
        1440.0,
        1000.0,
    );
    settle(&mut h);
    h.state_mut().navigate(Route::Knowledge);
    settle(&mut h);
    click(&mut h, Role::Button, "New entry");
    type_into(&mut h, "Entry text", "A write that keeps running");
    click(&mut h, Role::Button, "Save entry");
    assert!(has_text(&h, "Last change: Entry · still running"));
    // The periodic refresh may also read a running write, so compare counts across each step
    // rather than expecting exact totals.
    let first = reads.load(Ordering::SeqCst);
    assert!(
        first >= 1,
        "the write's status is read after it is recorded"
    );
    h.state_mut().refresh_visible();
    settle(&mut h);
    let second = reads.load(Ordering::SeqCst);
    assert!(second > first, "a refresh reads a running write again");
    running.store(false, Ordering::SeqCst);
    h.state_mut().refresh_visible();
    settle(&mut h);
    assert!(reads.load(Ordering::SeqCst) > second);
    assert!(has_text(&h, "Last change: Entry · complete"));
    let done = reads.load(Ordering::SeqCst);
    h.state_mut().refresh_visible();
    settle(&mut h);
    assert_eq!(
        reads.load(Ordering::SeqCst),
        done,
        "a complete write is not read again"
    );
}

#[test]
fn a_project_graph_renders_and_reads_its_summary() {
    let root = Root::new("knowledge-graph-project");
    let engine = start(&root);
    let backend = engine.backend();
    let made = ops::project_changed(backend.command(
        "projects.create",
        &ops::project_create(&*backend, "Engine", "", &[]),
    ))
    .unwrap();
    let project = made.id.0.as_str().to_owned();
    record_graph_in(
        &*backend,
        json!({"kind": "Project", "projectId": project.clone()}),
    );
    let mut h = open_knowledge(&engine, &root);
    h.state_mut().select_project(Some(project));
    settle(&mut h);
    assert_eq!(h.state().knowledge.claims.items.len(), 2);
    h.get_by_label("Fact  ·  Storage uses SQLite").click();
    settle(&mut h);
    click(&mut h, Role::Button, "Open claim bundle");
    click(&mut h, Role::Button, "Render summary");
    let summary = h
        .state()
        .knowledge
        .graph
        .view
        .as_ref()
        .and_then(|v| v.summary.clone());
    assert!(
        summary
            .as_deref()
            .is_some_and(|s| s.contains("**SQLite** (component)")),
        "{summary:?} {:?}",
        h.state().knowledge.graph.summary
    );
}
