//! Features, the board, Issues and Overview over the real runtime, headless (DW-12): a feature
//! and an issue are created from their screens with the same commands an agent submits; the
//! feature's detail shows plan approval and readiness apart; the board shows a feature's cards
//! in their workflow columns with accessible move and priority controls beside drag and drop; a
//! priority control reorders through `tasks.prioritize`; the issue detail shows Blocked by and
//! Blocks of one stored link; Overview's summary counts the project's work and its Activity
//! lists the full change feed with filters; the archive confirmation counts the project's work.
//! No inference.
mod common;

use common::*;
use devworks_core::protocol::records::WorkRecord;
use devworks_core::services::work::workflow::default_status_id;
use devworks_desktop::backend::Backend;
use devworks_desktop::ops;
use devworks_desktop::route::Route;
use devworks_desktop::work_ops as wops;
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::{NodeT, Queryable};
use serde_json::json;

fn type_into(
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    role: Role,
    label: &str,
    text: &str,
) {
    h.get_by_role_and_label(role, label).focus();
    h.step();
    h.get_by_role_and_label(role, label).type_text(text);
    h.step();
}
fn list(backend: &dyn Backend, op: &str, project: &str) -> Vec<WorkRecord> {
    wops::work_page(backend.query(op, &wops::work_list(backend, op, project, None)))
        .unwrap()
        .0
}
fn command(backend: &dyn Backend, op: &str, body: serde_json::Value) -> WorkRecord {
    wops::work_changed(backend.command(op, &wops::work_command(backend, op, body))).unwrap()
}
fn click_row(h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>, prefix: &str) {
    h.query_all_by_label_contains(prefix)
        .find(|n| {
            n.accesskit_node()
                .label()
                .is_some_and(|l| l.starts_with(prefix))
        })
        .unwrap_or_else(|| panic!("a row {prefix}"))
        .click();
    settle(h);
}

#[test]
fn overview_messages_use_the_retained_gateway_and_show_delivered_then_read_state() {
    let root = Root::new("work-message-board");
    let engine = start(&root);
    let backend = engine.backend();
    let project = ops::project_changed(backend.command(
        "projects.create",
        &ops::project_create(&*backend, "Messages", "", &[]),
    ))
    .unwrap();
    let pid = project.id.0.as_str().to_owned();
    let posted = wops::message_posted(backend.command(
        "agents.post",
        &wops::message_post(
            &*backend,
            &pid,
            json!({"body":"Please review the build","messageKind":"Question","toHuman":true}),
        ),
    ))
    .unwrap();
    assert!(posted.created);
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1000.0);
    settle(&mut h);
    h.state_mut().select_project(Some(pid.clone()));
    h.state_mut().navigate(Route::Overview);
    settle(&mut h);
    click(&mut h, Role::Button, "Messages");
    assert!(has_text(&h, "Please review the build"));
    assert_eq!(h.state().work.messages.inbox.len(), 1);
    assert_eq!(h.state().work.messages.history.len(), 1);
    assert!(matches!(
        h.state().work.messages.inbox[0].delivery,
        Some(devworks_core::protocol::records::RetainedDeliveryState::Delivered)
    ));
    click(&mut h, Role::Button, "Answer question");
    assert!(h.state().work.messages.inbox.is_empty());
    assert_eq!(h.state().work.messages.history.len(), 1);
    assert!(matches!(
        h.state().work.messages.history[0].state,
        Some(devworks_core::protocol::records::RetainedMessageState::Answered)
    ));
    type_into(&mut h, Role::TextInput, "Message body", "I will check it");
    click(&mut h, Role::CheckBox, "To you");
    click(&mut h, Role::Button, "Post message");
    assert!(
        h.state()
            .work
            .messages
            .history
            .iter()
            .any(|m| m.body.0 == "I will check it")
    );
    assert!(
        h.state()
            .work
            .messages
            .inbox
            .iter()
            .any(|m| m.body.0 == "I will check it")
    );
    click(&mut h, Role::Button, "Mark read");
    assert!(h.state().work.messages.inbox.is_empty());
    wops::message_posted(backend.command(
        "agents.post",
        &wops::message_post(
            &*backend,
            &pid,
            json!({"body":"External update","toHuman":true}),
        ),
    ))
    .unwrap();
    h.state_mut().work.messages.event_poll_at =
        Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
    settle(&mut h);
    assert!(
        h.state()
            .work
            .messages
            .inbox
            .iter()
            .any(|m| m.body.0 == "External update")
    );
}

#[test]
fn message_event_refresh_keeps_earlier_unread_inbox_entries() {
    let root = Root::new("work-message-refresh");
    let engine = start(&root);
    let backend = engine.backend();
    let project = ops::project_changed(backend.command(
        "projects.create",
        &ops::project_create(&*backend, "Inbox refresh", "", &[]),
    ))
    .unwrap();
    let pid = project.id.0.as_str().to_owned();
    let post = |body: &str| {
        wops::message_posted(backend.command(
            "agents.post",
            &wops::message_post(&*backend, &pid, json!({"body":body,"toHuman":true})),
        ))
        .unwrap()
    };
    post("First unread");
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1000.0);
    settle(&mut h);
    h.state_mut().select_project(Some(pid.clone()));
    h.state_mut().navigate(Route::Overview);
    settle(&mut h);
    click(&mut h, Role::Button, "Messages");
    assert_eq!(h.state().work.messages.inbox.len(), 1);

    post("Second unread");
    h.state_mut().work.messages.event_poll_at =
        Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
    settle(&mut h);
    let inbox = &h.state().work.messages.inbox;
    assert_eq!(inbox.len(), 2);
    assert!(inbox.iter().any(|message| message.body.0 == "First unread"));
    assert!(
        inbox
            .iter()
            .any(|message| message.body.0 == "Second unread")
    );

    for n in 0..99 {
        post(&format!("More {n}"));
    }
    h.state_mut().work.messages.event_poll_at =
        Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
    settle(&mut h);
    assert_eq!(h.state().work.messages.inbox.len(), 100);
    h.state_mut().work.messages.event_poll_at =
        Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
    click(&mut h, Role::Button, "Load more inbox messages");
    assert_eq!(
        h.state().work.messages.inbox.len(),
        101,
        "has_more={} cursor={:?} error={:?}",
        h.state().work.messages.inbox_has_more,
        h.state().work.messages.inbox_cursor,
        h.state().work.messages.error
    );
    assert!(
        h.state()
            .work
            .messages
            .inbox
            .iter()
            .any(|message| message.body.0 == "More 98")
    );
}

#[test]
fn features_board_issues_and_overview_share_the_agent_operations() {
    let root = Root::new("work-screens");
    let engine = start(&root);
    let backend = engine.backend();
    let project = ops::project_changed(backend.command(
        "projects.create",
        &ops::project_create(&*backend, "Shop", "", &[]),
    ))
    .unwrap();
    let pid = project.id.0.as_str().to_owned();
    let backlog = default_status_id(&project.id, "backlog")
        .as_str()
        .to_owned();
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1000.0);
    settle(&mut h);
    // Without a project the work screens say so honestly.
    h.state_mut().navigate(Route::Features);
    settle(&mut h);
    assert!(has_text(&h, "No project selected"));
    h.state_mut().select_project(Some(pid.clone()));
    settle(&mut h);
    assert!(has_text(&h, "No features yet"));
    // A feature is created from its screen.
    click(&mut h, Role::Button, "New feature");
    type_into(&mut h, Role::TextInput, "Title", "Checkout");
    click(&mut h, Role::Button, "Create");
    let features = list(&*backend, "features.list", &pid);
    assert_eq!(features.len(), 1);
    let feature = features[0].clone();
    assert!(has_text(&h, "Created feature Checkout."));
    // Its detail shows plan approval and readiness as separate facts.
    click_row(&mut h, "Checkout  ·");
    assert!(has_text(&h, "No plan"));
    assert!(has_text(&h, "No readiness assessment"));
    // Two cards on its board (an agent could have created them: the same command).
    let a = command(
        &*backend,
        "tasks.create",
        json!({"projectId": pid, "title": "Card A", "description": "", "repositoryIds": [],
            "statusId": backlog, "priority": 1, "parentId": feature.id}),
    );
    let b = command(
        &*backend,
        "tasks.create",
        json!({"projectId": pid, "title": "Card B", "description": "", "repositoryIds": [],
            "statusId": backlog, "priority": 1, "parentId": feature.id}),
    );
    h.state_mut().work.board = Some(feature.id.as_str().to_owned());
    click(&mut h, Role::Button, "Board");
    click(&mut h, Role::Button, "Refresh");
    assert!(has_text(&h, "Backlog (2)"));
    // Accessible alternatives to drag and drop exist for every card.
    assert!(
        h.query_by_role_and_label(Role::ComboBox, "Move Card A")
            .is_some()
    );
    // "Move Card B up" reorders through tasks.prioritize.
    click(&mut h, Role::Button, "Move Card B up");
    let cards = list(&*backend, "tasks.list", &pid);
    assert_eq!(
        cards.iter().map(|c| c.title.0.as_str()).collect::<Vec<_>>(),
        ["Card B", "Card A"]
    );
    // The accessible Move control changes the workflow status through tasks.move; the card
    // stays in its column until the move is confirmed, then shows in its new column.
    click(&mut h, Role::ComboBox, "Move Card A");
    let option = h
        .query_all_by_label("In progress")
        .find(|n| n.accesskit_node().role() != Role::ComboBox)
        .expect("the In progress option");
    option.click();
    settle(&mut h);
    let moved = list(&*backend, "tasks.list", &pid);
    let in_progress = default_status_id(&project.id, "in-progress");
    assert_eq!(
        moved
            .iter()
            .find(|c| c.title.0 == "Card A")
            .unwrap()
            .status_id,
        in_progress
    );
    assert!(has_text(&h, "In progress (1)"));
    let cards = moved;
    // A link written by an agent shows as Blocked by on one card and Blocks on the other.
    let a = cards.iter().find(|c| c.id == a.id).unwrap().clone();
    command(
        &*backend,
        "tasks.dependency_links",
        json!({"taskId": a.id, "expectedRevision": a.revision,
            "replacement": {"edges": [{"predecessor": b.id, "successor": a.id}]}}),
    );
    // An issue from the Issues screen, without a feature.
    h.state_mut().navigate(Route::Issues);
    settle(&mut h);
    click(&mut h, Role::Button, "New issue");
    type_into(&mut h, Role::TextInput, "Title", "Crash on start");
    click(&mut h, Role::Button, "Create");
    let issues = list(&*backend, "issues.list", &pid);
    assert_eq!(issues.len(), 1);
    assert!(issues[0].parent_id.is_none());
    click_row(&mut h, "Crash on start  ·");
    assert!(has_text(&h, "Blocked by"));
    assert!(has_text(&h, "Nothing blocks it."));
    // The board card shows its blocker.
    h.state_mut().navigate(Route::Features);
    settle(&mut h);
    h.state_mut().work.card = Some(a.id.as_str().to_owned());
    settle(&mut h);
    assert!(has_text(&h, "Blocked by 1"));
    // Overview: the summary strip counts the project's work; Activity has the full feed.
    h.state_mut().navigate(Route::Overview);
    settle(&mut h);
    click(&mut h, Role::Button, "Refresh");
    assert!(h.query_by_label("1 Features").is_some());
    assert!(h.query_by_label("1 Issues").is_some());
    click(&mut h, Role::Button, "Activity");
    assert!(has_text(&h, "older history on demand"));
    assert!(has_text(&h, "You"));
    // The archive confirmation counts the work it would stop.
    h.state_mut().navigate(Route::Portfolio);
    settle(&mut h);
    click_row(&mut h, "Shop  ·");
    click(&mut h, Role::Button, "Archive…");
    settle(&mut h);
    assert!(has_text(
        &h,
        "features and issues have no agent working now"
    ));
    click(&mut h, Role::Button, "Cancel");
}

#[test]
fn the_plan_editor_keeps_what_it_does_not_show() {
    use devworks_desktop::shell::work::{Entry, open_plan_form, plan_body};
    let art = json!({"artifactId": "art-1", "versionId": "ver-1", "sha256": "a".repeat(64),
        "byteLength": "4", "mediaType": "text/plain"});
    let draft = json!({
        "content": art,
        "acceptance": [{"id": "A1", "text": "works\non every browser: Safari and Edge",
            "evidenceIds": ["ev-1"], "state": "Met"}],
        "repositoryIds": [], "goals": ["ship\nthis quarter"],
        "requirements": [
            {"id": "E1", "statement": "pay", "status": "Proposed", "coveredByTaskIds": ["task-1"]},
            {"id": "E2", "statement": "refunds", "status": "Dropped", "dropDecision": art,
             "coveredByTaskIds": []}],
        "acceptanceTests": [{"id": "T1", "text": "checkout e2e\n  1. add\n  2. pay",
            "evidenceIds": ["ev-2"], "state": "Met"}],
    });
    let record: WorkRecord = serde_json::from_value(json!({
        "id": "feature-1", "revision": "3", "projectId": "project-1", "kind": "Feature",
        "title": "Checkout", "description": "", "statusId": "s", "priority": 0,
        "repositoryIds": [], "dependencyIds": [], "goals": [], "requirements": [],
        "acceptance": [], "inheritsRequirementIds": [], "touches": [], "links": [],
        "comments": [], "handoffs": [], "attempts": [], "mergedAwaitingAcceptance": false,
        "detailCoverage": {"state": "Complete", "gaps": [], "moreGaps": false},
        "commentCount": "0", "handoffCount": "0", "attemptCount": "0", "dependentIds": [],
        "pendingPlan": {"revision": "2", "draft": draft},
    }))
    .unwrap();
    let mut form = open_plan_form(&record);
    // One entry per criterion, test and live requirement, multi-line text intact.
    assert_eq!(
        form.acceptance_tests,
        vec![Entry {
            id: "T1".into(),
            text: "checkout e2e\n  1. add\n  2. pay".into()
        }]
    );
    assert_eq!(
        form.requirements,
        vec![Entry {
            id: "E1".into(),
            text: "pay".into()
        }]
    );
    form.requirements[0].text = "pay by card\nor wallet".into();
    let content = serde_json::from_value(art.clone()).unwrap();
    let body = plan_body(&form, &content);
    // Saving keeps the acceptance tests, each requirement's status and coverage, the dropped
    // requirement with its decision, and the unchanged criterion's state and evidence.
    assert_eq!(body["acceptanceTests"], draft["acceptanceTests"]);
    assert_eq!(body["acceptance"], draft["acceptance"]);
    assert_eq!(
        body["requirements"][0]["statement"],
        "pay by card\nor wallet"
    );
    assert_eq!(body["goals"], draft["goals"]);
    assert_eq!(body["requirements"][0]["status"], "Proposed");
    assert_eq!(
        body["requirements"][0]["coveredByTaskIds"],
        json!(["task-1"])
    );
    assert_eq!(body["requirements"][1], draft["requirements"][1]);
    let root = Root::new("requirement-id-reserved");
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 720.0);
    settle(&mut h);
    let backend = engine.backend();
    let project = ops::project_changed(backend.command(
        "projects.create",
        &ops::project_create(&*backend, "Shop", "", &[]),
    ))
    .unwrap();
    let feature =
        features_from_screen(&mut h, &*backend, project.id.0.as_str(), &["Checkout"]).remove(0);
    click_row(&mut h, "Checkout  ·");
    click(&mut h, Role::Button, "Edit plan");
    form.snapshot = feature;
    h.state_mut().work.plan = Some(form);
    // This preservation scenario invokes each real button's accessibility Click action.
    // Render the injected draft before querying its controls, then observe each UI edit.
    settle(&mut h);
    for (label, acceptance_count) in [
        ("Add requirement", 1),
        ("Add acceptance criterion", 2),
        ("Remove acceptance criterion 2", 1),
    ] {
        h.get_by_role_and_label(Role::Button, label)
            .click_accesskit();
        settle(&mut h);
        let form = h.state().work.plan.as_ref().expect("the editor stays open");
        assert_eq!(
            form.requirements.last().map(|e| e.id.as_str()),
            Some("E3"),
            "after {label}"
        );
        assert_eq!(form.acceptance.len(), acceptance_count, "after {label}");
    }
    let form = h.state().work.plan.as_ref().unwrap();
    assert_eq!(form.requirements.last().unwrap().id, "E3");
    let mut form = form.clone();
    form.requirements.last_mut().unwrap().text = "new requirement".into();
    let saved = plan_body(&form, &content);
    let added = saved["requirements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "E3")
        .unwrap();
    assert_eq!(added["status"], "Confirmed");
    assert_eq!(
        saved["requirements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == "E2")
            .unwrap(),
        &draft["requirements"][1]
    );
}

#[test]
fn a_plan_save_submits_the_form_it_started_from_into_the_project() {
    use devworks_desktop::shell::work::Entry;
    let root = Root::new("plan-save");
    let engine = start(&root);
    let real = engine.backend();
    let project = ops::project_changed(real.command(
        "projects.create",
        &ops::project_create(&*real, "Shop", "", &[]),
    ))
    .unwrap();
    let pid = project.id.0.as_str().to_owned();
    let gate = Gate::new("artifacts.save");
    let mut h = harness(
        shell_with(Ok(gate.over(real.clone())), &root, Script::default(), None),
        1600.0,
        1000.0,
    );
    settle(&mut h);
    h.state_mut().navigate(Route::Features);
    h.state_mut().select_project(Some(pid.clone()));
    settle(&mut h);
    click(&mut h, Role::Button, "New feature");
    type_into(&mut h, Role::TextInput, "Title", "Checkout");
    click(&mut h, Role::Button, "Create");
    let feature = list(&*real, "features.list", &pid).remove(0);
    click_row(&mut h, "Checkout  ·");
    click(&mut h, Role::Button, "Edit plan");
    {
        let form = h
            .state_mut()
            .work
            .plan
            .as_mut()
            .expect("the editor is open");
        form.text = "Ship checkout".into();
        form.requirements = vec![Entry {
            id: "E1".into(),
            text: "pay".into(),
        }];
    }
    settle(&mut h);
    // The feature accounting summary can put this control below the viewport. Scroll before
    // closing the upload gate: settling a held request afterwards would wait on the test gate.
    h.get_by_role_and_label(Role::Button, "Save plan draft")
        .scroll_to_me();
    settle(&mut h);
    assert!(
        h.get_by_role_and_label(Role::Button, "Save plan draft")
            .rect()
            .bottom()
            <= 1000.0,
        "the save control is in the native viewport"
    );
    // The plan text is uploading while the user keeps writing.
    gate.close();
    h.get_by_role_and_label(Role::Button, "Save plan draft")
        .click();
    h.run_steps(3);
    assert!(
        !h.state().work.plan_saving.is_empty(),
        "the save is in flight: {:?}",
        h.state().work.message
    );
    h.state_mut().work.plan.as_mut().unwrap().requirements[0].text = "pay by card".into();
    gate.open();
    settle(&mut h);
    // What was saved is the form the save started from, its text an artifact of the project
    // (the feature's agents read it there; a workspace-only artifact is refused).
    let saved = wops::work_record(real.query(
        "features.get",
        &wops::work_get(&*real, "features.get", feature.id.as_str()),
    ))
    .unwrap();
    let pending = saved.pending_plan.clone().expect("the draft is saved");
    assert_eq!(pending.draft.requirements.0[0].statement.0, "pay");
    // The later edit stays open as a draft on the saved revision.
    let form = h.state().work.plan.clone().expect("the later edit is kept");
    assert_eq!(form.requirements[0].text, "pay by card");
    assert_eq!(form.snapshot.revision, saved.revision);
    // Saving it again needs no new upload (its text is the saved one) and settles the editor.
    click(&mut h, Role::Button, "Save plan draft");
    settle(&mut h);
    let saved = wops::work_record(real.query(
        "features.get",
        &wops::work_get(&*real, "features.get", feature.id.as_str()),
    ))
    .unwrap();
    let again = saved.pending_plan.expect("the draft is saved");
    assert_eq!(again.draft.requirements.0[0].statement.0, "pay by card");
    assert_eq!(
        again.draft.content.version_id,
        pending.draft.content.version_id
    );
    assert!(h.state().work.plan.is_none());
}

#[test]
fn a_blocker_change_starts_from_the_complete_list() {
    use devworks_core::services::work::deps::MAX_PREREQUISITES;
    use devworks_desktop::shell::work::{BlockerEdit, blocker_change};
    let root = Root::new("blocker-change");
    let engine = start(&root);
    let backend = engine.backend();
    let project = ops::project_changed(backend.command(
        "projects.create",
        &ops::project_create(&*backend, "Shop", "", &[]),
    ))
    .unwrap();
    let backlog = default_status_id(&project.id, "backlog")
        .as_str()
        .to_owned();
    let mut w = command(
        &*backend,
        "issues.create",
        json!({"projectId": project.id.0.as_str(), "title": "Bug", "description": "",
            "repositoryIds": [], "statusId": backlog, "priority": 0}),
    );
    w.dependency_ids.0 = vec![
        devworks_core::protocol::Id::new("a").unwrap(),
        devworks_core::protocol::Id::new("b").unwrap(),
    ];
    let edges = |v: serde_json::Value| -> Vec<String> {
        v["replacement"]["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["predecessor"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        edges(blocker_change(&w, BlockerEdit::Add("c")).unwrap()),
        ["a", "b", "c"]
    );
    assert_eq!(
        edges(blocker_change(&w, BlockerEdit::Remove("a")).unwrap()),
        ["b"]
    );
    assert!(blocker_change(&w, BlockerEdit::Add("a")).is_none());
    assert!(blocker_change(&w, BlockerEdit::Remove("z")).is_none());
    w.dependency_ids.0 = (0..MAX_PREREQUISITES)
        .map(|n| devworks_core::protocol::Id::new(format!("p{n}")).unwrap())
        .collect();
    assert!(blocker_change(&w, BlockerEdit::Add("one-more")).is_none());
}

#[test]
fn an_item_edit_made_while_a_save_runs_stays_open() {
    let root = Root::new("edit-in-flight");
    let engine = start(&root);
    let real = engine.backend();
    let project = ops::project_changed(real.command(
        "projects.create",
        &ops::project_create(&*real, "Shop", "", &[]),
    ))
    .unwrap();
    let pid = project.id.0.as_str().to_owned();
    let backlog = default_status_id(&project.id, "backlog")
        .as_str()
        .to_owned();
    let issue = command(
        &*real,
        "issues.create",
        json!({"projectId": pid, "title": "Crash", "description": "", "repositoryIds": [],
            "statusId": backlog, "priority": 0}),
    );
    let gate = Gate::new("tasks.edit");
    let mut h = harness(
        shell_with(Ok(gate.over(real.clone())), &root, Script::default(), None),
        1600.0,
        1000.0,
    );
    settle(&mut h);
    h.state_mut().select_project(Some(pid.clone()));
    h.state_mut().navigate(Route::Issues);
    settle(&mut h);
    click_row(&mut h, "Crash  ·");
    click(&mut h, Role::Button, "Edit");
    h.state_mut().work.edit.as_mut().expect("editing").title = "Crash on start".into();
    settle(&mut h);
    gate.close();
    h.get_by_role_and_label(Role::Button, "Save").click();
    h.run_steps(3);
    assert!(
        !h.state().work.edit_saving.is_empty(),
        "the save is in flight"
    );
    h.state_mut().work.edit.as_mut().unwrap().title = "Crash on start-up".into();
    gate.open();
    settle(&mut h);
    let saved = wops::work_record(real.query(
        "issues.get",
        &wops::work_get(&*real, "issues.get", issue.id.as_str()),
    ))
    .unwrap();
    assert_eq!(saved.title.0, "Crash on start");
    let form = h.state().work.edit.clone().expect("the later edit is kept");
    assert_eq!(form.title, "Crash on start-up");
    assert_eq!(form.snapshot.revision, saved.revision);
}

#[test]
fn a_creation_locks_its_form_while_it_runs() {
    let root = Root::new("create-in-flight");
    let engine = start(&root);
    let real = engine.backend();
    let project = ops::project_changed(real.command(
        "projects.create",
        &ops::project_create(&*real, "Shop", "", &[]),
    ))
    .unwrap();
    let pid = project.id.0.as_str().to_owned();
    let gate = Gate::new("issues.create");
    let mut h = harness(
        shell_with(Ok(gate.over(real.clone())), &root, Script::default(), None),
        1600.0,
        1000.0,
    );
    settle(&mut h);
    h.state_mut().select_project(Some(pid.clone()));
    h.state_mut().navigate(Route::Issues);
    settle(&mut h);
    click(&mut h, Role::Button, "New issue");
    type_into(&mut h, Role::TextInput, "Title", "Crash");
    gate.close();
    h.get_by_role_and_label(Role::Button, "Create").click();
    h.run_steps(3);
    // Nothing can be typed into a form its running creation will consume.
    assert!(
        h.get_by_role_and_label(Role::TextInput, "Title")
            .accesskit_node()
            .is_disabled()
    );
    gate.open();
    settle(&mut h);
    assert_eq!(list(&*real, "issues.list", &pid).len(), 1);
}

#[test]
fn a_creation_reply_keeps_another_projects_draft() {
    creation_reply_preserves_draft(true);
}

#[test]
fn a_creation_reply_keeps_later_edits_in_the_same_project() {
    creation_reply_preserves_draft(false);
}

fn creation_reply_preserves_draft(switch_project: bool) {
    let root = Root::new("create-reply-scope");
    let engine = start(&root);
    let real = engine.backend();
    let make_project = |title| {
        ops::project_changed(real.command(
            "projects.create",
            &ops::project_create(&*real, title, "", &[]),
        ))
        .unwrap()
    };
    let a = make_project("Shop A");
    let b = make_project("Shop B");
    let gate = Gate::new("issues.create");
    let mut h = harness(
        shell_with(Ok(gate.over(real.clone())), &root, Script::default(), None),
        1600.0,
        1000.0,
    );
    settle(&mut h);
    h.state_mut()
        .select_project(Some(a.id.0.as_str().to_owned()));
    h.state_mut().navigate(Route::Issues);
    settle(&mut h);
    click(&mut h, Role::Button, "New issue");
    type_into(&mut h, Role::TextInput, "Title", "Submitted issue");
    gate.close();
    h.get_by_role_and_label(Role::Button, "Create").click();
    h.run_steps(3);
    if switch_project {
        h.state_mut()
            .select_project(Some(b.id.0.as_str().to_owned()));
        h.state_mut().work.create = Some(devworks_desktop::shell::work::CreateForm {
            kind: "issue".into(),
            title: "Unsaved issue".into(),
            ..Default::default()
        });
        h.run_steps(3);
    } else {
        // Preserve any later draft edits even if they arrived outside the disabled widget.
        h.state_mut().work.create.as_mut().unwrap().title = "Unsaved issue".into();
    }
    gate.open();
    settle(&mut h);
    assert_eq!(
        list(&*real, "issues.list", a.id.0.as_str())[0].title.0,
        "Submitted issue"
    );
    assert!(list(&*real, "issues.list", b.id.0.as_str()).is_empty());
    assert_eq!(
        h.state()
            .work
            .create
            .as_ref()
            .expect("the unsaved draft stays open")
            .title,
        "Unsaved issue"
    );
    assert!(h.state().work.create_saving.is_empty());
}

/// A project with features `titles`, created from the Features screen of `h`.
fn features_from_screen(
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    backend: &dyn Backend,
    pid: &str,
    titles: &[&str],
) -> Vec<WorkRecord> {
    h.state_mut().navigate(Route::Features);
    h.state_mut().select_project(Some(pid.to_owned()));
    settle(h);
    for t in titles {
        click(h, Role::Button, "New feature");
        type_into(h, Role::TextInput, "Title", t);
        click(h, Role::Button, "Create");
    }
    list(backend, "features.list", pid)
}
/// Opens `title`'s plan editor, writes `requirement` and saves, without waiting for replies.
fn save_plan_now(
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    title: &str,
    requirement: &str,
) {
    let prefix = format!("{title}  ·");
    h.query_all_by_label_contains(&prefix)
        .find(|n| {
            n.accesskit_node()
                .label()
                .is_some_and(|l| l.starts_with(&prefix))
        })
        .expect("the feature row")
        .click();
    h.run_steps(3);
    h.get_by_role_and_label(Role::Button, "Edit plan").click();
    h.run_steps(3);
    {
        let form = h
            .state_mut()
            .work
            .plan
            .as_mut()
            .expect("the editor is open");
        form.text = format!("Plan of {title}");
        form.requirements = vec![devworks_desktop::shell::work::Entry {
            id: "E1".into(),
            text: requirement.into(),
        }];
    }
    h.run_steps(3);
    // Keep uploads in flight while activating the actual Save control, even offscreen.
    h.get_by_role_and_label(Role::Button, "Save plan draft")
        .click_accesskit();
    h.run_steps(3);
}
fn pending_statement(backend: &dyn Backend, feature: &WorkRecord) -> Option<String> {
    wops::work_record(backend.query(
        "features.get",
        &wops::work_get(backend, "features.get", feature.id.as_str()),
    ))
    .unwrap()
    .pending_plan
    .map(|p| p.draft.requirements.0[0].statement.0.clone())
}
fn shop(backend: &dyn Backend) -> String {
    ops::project_changed(backend.command(
        "projects.create",
        &ops::project_create(backend, "Shop", "", &[]),
    ))
    .unwrap()
    .id
    .0
    .as_str()
    .to_owned()
}

#[test]
fn two_plan_saves_in_flight_each_submit_their_own_draft() {
    let root = Root::new("two-plan-saves");
    let engine = start(&root);
    let real = engine.backend();
    let pid = shop(&*real);
    let gate = Gate::new("artifacts.save");
    let mut h = harness(
        shell_with(Ok(gate.over(real.clone())), &root, Script::default(), None),
        1600.0,
        1000.0,
    );
    settle(&mut h);
    let features = features_from_screen(&mut h, &*real, &pid, &["Checkout", "Refunds"]);
    // Both plan texts are uploading at once.
    gate.close();
    save_plan_now(&mut h, "Checkout", "pay");
    save_plan_now(&mut h, "Refunds", "refund");
    assert_eq!(h.state().work.plan_saving.len(), 2);
    gate.open();
    settle(&mut h);
    let by = |t: &str| features.iter().find(|f| f.title.0 == t).unwrap();
    assert_eq!(
        pending_statement(&*real, by("Checkout")).as_deref(),
        Some("pay")
    );
    assert_eq!(
        pending_statement(&*real, by("Refunds")).as_deref(),
        Some("refund")
    );
    assert!(h.state().work.plan_saving.is_empty());
}

#[test]
fn plan_saves_pending_at_close_are_finished_at_the_next_launch() {
    let root = Root::new("plan-saves-relaunch");
    let engine = start(&root);
    let real = engine.backend();
    let pid = shop(&*real);
    // Checkout's save committed but its acknowledgment was lost, so its outcome is unknown at
    // close; Refunds is still storing its text.
    let lossy = LoseAcks::over(real.clone(), "features.edit_plan", 100);
    let gate = Gate::new("artifacts.save");
    let mut h = harness(
        shell_with(Ok(gate.over(lossy.clone())), &root, Script::default(), None),
        1600.0,
        1000.0,
    );
    settle(&mut h);
    let features = features_from_screen(&mut h, &*real, &pid, &["Checkout", "Refunds"]);
    save_plan_now(&mut h, "Checkout", "pay");
    settle(&mut h);
    gate.close();
    save_plan_now(&mut h, "Refunds", "refund");
    assert_eq!(h.state().work.plan_saving.len(), 2);
    h.state_mut().on_exit();
    drop(h);
    gate.open();
    let by = |t: &str| features.iter().find(|f| f.title.0 == t).unwrap();
    assert_eq!(
        pending_statement(&*real, by("Checkout")).as_deref(),
        Some("pay"),
        "it committed; only its outcome is unknown"
    );
    let mut h = harness(
        shell_with(Ok(real.clone()), &root, Script::default(), None),
        1600.0,
        1000.0,
    );
    settle(&mut h);
    assert_eq!(
        pending_statement(&*real, by("Checkout")).as_deref(),
        Some("pay")
    );
    assert_eq!(
        pending_statement(&*real, by("Refunds")).as_deref(),
        Some("refund")
    );
    assert!(h.state().work.plan_saving.is_empty());
    assert_eq!(h.state().unconfirmed_count(), 0);
    // Checkout was confirmed with its exact frame, not saved again (which its own commit would
    // refuse as a conflict, reopening the draft).
    assert!(
        h.state().work.plan.is_none(),
        "{:?}",
        h.state().work.message
    );
}

struct CountHistory {
    inner: std::sync::Arc<dyn Backend>,
    requests: std::sync::Mutex<Vec<serde_json::Value>>,
}
impl Backend for CountHistory {
    fn query(&self, operation: &str, frame: &[u8]) -> devworks_core::gateway::QueryOutcome {
        if operation == "history.events" {
            self.requests
                .lock()
                .unwrap()
                .push(serde_json::from_slice(frame).unwrap());
        }
        self.inner.query(operation, frame)
    }
    fn command(&self, operation: &str, frame: &[u8]) -> devworks_core::gateway::CommandOutcome {
        self.inner.command(operation, frame)
    }
    fn refresh_packages(&self) -> Result<(), String> {
        self.inner.refresh_packages()
    }
    fn fetch_index_bundle(
        &self,
        package: &str,
        digest: &str,
    ) -> Result<std::path::PathBuf, String> {
        self.inner.fetch_index_bundle(package, digest)
    }
    fn stage_api_key(
        &self,
        key: devworks_core::ports::secrets::Secret,
    ) -> Result<devworks_core::protocol::Id, String> {
        self.inner.stage_api_key(key)
    }
    fn secret_store(&self) -> devworks_desktop::backend::SecretStoreStatus {
        self.inner.secret_store()
    }
    fn mint_workspace_upload(
        &self,
        file: &std::path::Path,
    ) -> Result<devworks_core::protocol::Id, String> {
        self.inner.mint_workspace_upload(file)
    }
    fn mint_project_upload(
        &self,
        project: &devworks_core::protocol::ProjectId,
        file: &std::path::Path,
    ) -> Result<devworks_core::protocol::Id, String> {
        self.inner.mint_project_upload(project, file)
    }
    fn keep_awake_health(&self) -> devworks_core::settings::view::Health {
        self.inner.keep_awake_health()
    }
    fn hints(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        None
    }
    fn set_waker(&self, waker: devworks_desktop::backend::Waker) {
        self.inner.set_waker(waker)
    }
    fn token(&self, prefix: &str) -> String {
        self.inner.token(prefix)
    }
    fn scratch_dir(&self) -> std::path::PathBuf {
        self.inner.scratch_dir()
    }
    fn read_stream(
        &self,
        stream: &devworks_core::protocol::Id,
        max: u64,
    ) -> Result<Vec<u8>, String> {
        self.inner.read_stream(stream, max)
    }
}

#[test]
fn activity_reads_to_the_newest_change_and_refresh_adds_newer_ones() {
    use devworks_desktop::work_ops::ACTIVITY_PAGE;
    let root = Root::new("activity-newest");
    let engine = start(&root);
    let real = engine.backend();
    let pid = shop(&*real);
    let backlog = default_status_id(
        &devworks_core::protocol::ProjectId(devworks_core::protocol::Id::new(pid.clone()).unwrap()),
        "backlog",
    )
    .as_str()
    .to_owned();
    let issue = |n: usize| {
        command(
            &*real,
            "issues.create",
            json!({"projectId": pid, "title": format!("Issue {n}"), "description": "",
                "repositoryIds": [], "statusId": backlog, "priority": 0}),
        )
    };
    let n = usize::from(ACTIVITY_PAGE) + 20;
    for i in 0..n {
        issue(i);
    }
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1000.0);
    settle(&mut h);
    h.state_mut().select_project(Some(pid.clone()));
    h.state_mut().navigate(Route::Overview);
    settle(&mut h);
    click(&mut h, Role::Button, "Activity");
    let read = |h: &egui_kittest::Harness<'_, devworks_desktop::shell::Shell>| {
        h.state()
            .work
            .activity
            .items
            .iter()
            .map(|e| e.sequence.as_str().parse::<u64>().unwrap())
            .collect::<Vec<_>>()
    };
    let first = read(&h);
    // One newest page first; older pages require an explicit native action.
    assert_eq!(first.len(), usize::from(ACTIVITY_PAGE));
    assert!(
        first.windows(2).all(|w| w[0] > w[1]),
        "newest first, once each"
    );
    assert!(h.query_by_label("Load older changes").is_some());
    // The newest change is shown first, above the one before it.
    let shown: Vec<(f64, String)> = h
        .query_all_by_label_contains(" · ")
        .filter_map(|n| {
            let v = n.accesskit_node().value()?.to_string();
            let y = n.accesskit_node().bounding_box()?.y0;
            v.starts_with('#').then_some((y, v))
        })
        .collect();
    let top = shown
        .iter()
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, v)| v.clone())
        .expect("activity rows");
    assert!(
        top.starts_with(&format!("#{} ·", first.first().unwrap())),
        "the top row is the newest change: {top}"
    );
    let row = |seq: u64| {
        let prefix = format!("#{seq} ·");
        h.query_all_by_label_contains(&prefix)
            .find(|n| {
                n.accesskit_node()
                    .value()
                    .is_some_and(|l| l.starts_with(&prefix))
            })
            .unwrap_or_else(|| panic!("a row {prefix}"))
            .accesskit_node()
            .bounding_box()
            .expect("laid out")
            .y0
    };
    let last = *first.first().unwrap();
    let before = first[1];
    assert!(row(last) < row(before), "newest first");
    click(&mut h, Role::Button, "Load older changes");
    let complete = read(&h);
    assert!(complete.len() > first.len());
    assert!(h.query_by_label("Load older changes").is_none());
    // Refresh replaces only the number of pages explicitly loaded.
    issue(n);
    click(&mut h, Role::Button, "Refresh");
    let after = read(&h);
    assert_eq!(after.len(), complete.len() + 1);
    assert!(after.windows(2).all(|w| w[0] > w[1]));
}

#[test]
fn an_adjacent_move_is_one_bounded_placement_whatever_the_bucket_size() {
    use devworks_desktop::shell::work::adjacent_move;
    let names: Vec<String> = (0..300).map(|n| format!("w{n}")).collect();
    let bucket: Vec<&str> = names.iter().map(String::as_str).collect();
    let up = adjacent_move(&bucket, 250, true).unwrap();
    assert_eq!(
        up,
        json!({"workId": "w250", "anchorId": "w249", "side": "Before"})
    );
    let down = adjacent_move(&bucket, 250, false).unwrap();
    assert_eq!(
        down,
        json!({"workId": "w250", "anchorId": "w251", "side": "After"})
    );
    assert!(adjacent_move(&bucket, 0, true).is_none());
    assert!(adjacent_move(&bucket, 299, false).is_none());
}

#[test]
fn show_its_history_opens_overviews_activity_for_that_item() {
    let root = Root::new("show-history");
    let engine = start(&root);
    let backend = engine.backend();
    let project = ops::project_changed(backend.command(
        "projects.create",
        &ops::project_create(&*backend, "Shop", "", &[]),
    ))
    .unwrap();
    let pid = project.id.0.as_str().to_owned();
    let backlog = default_status_id(&project.id, "backlog")
        .as_str()
        .to_owned();
    let issue = command(
        &*backend,
        "issues.create",
        json!({"projectId": pid, "title": "Crash", "description": "", "repositoryIds": [],
            "statusId": backlog, "priority": 0}),
    );
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1000.0);
    settle(&mut h);
    h.state_mut().select_project(Some(pid.clone()));
    h.state_mut().navigate(Route::Issues);
    settle(&mut h);
    click_row(&mut h, "Crash  ·");
    click(&mut h, Role::Button, "Show its history");
    assert_eq!(h.state().route(), Route::Overview);
    assert_eq!(
        h.state().work.activity.item.as_deref(),
        Some(issue.id.as_str())
    );
    assert_eq!(
        h.state().memory().routes[&Route::Overview].tab,
        1,
        "the Activity tab"
    );
    assert!(has_text(&h, "older history on demand"), "Activity is shown");
}

#[test]
fn a_plan_stays_savable_after_a_covering_card_leaves_its_feature() {
    let root = Root::new("plan-after-unlink");
    let engine = start(&root);
    let real = engine.backend();
    let pid = shop(&*real);
    let mut h = harness(
        shell_with(Ok(real.clone()), &root, Script::default(), None),
        1600.0,
        1000.0,
    );
    settle(&mut h);
    let feature = features_from_screen(&mut h, &*real, &pid, &["Checkout"]).remove(0);
    save_plan_now(&mut h, "Checkout", "pay");
    settle(&mut h);
    // An agent plans a card covering E1, then the card leaves the feature.
    let status = default_status_id(
        &devworks_core::protocol::ProjectId(devworks_core::protocol::Id::new(pid.clone()).unwrap()),
        "backlog",
    );
    let card = command(
        &*real,
        "tasks.create",
        json!({"projectId": pid, "title": "Pay button", "description": "",
            "repositoryIds": [], "statusId": status.as_str(), "priority": 0,
            "parentId": feature.id.as_str()}),
    );
    let get = |id: &str| {
        wops::work_record(real.query("features.get", &wops::work_get(&*real, "features.get", id)))
            .unwrap()
    };
    let f = get(feature.id.as_str());
    let mut draft = serde_json::to_value(f.pending_plan.clone().unwrap().draft).unwrap();
    draft["requirements"][0]["coveredByTaskIds"] = json!([card.id.as_str()]);
    command(
        &*real,
        "features.edit_plan",
        json!({"featureId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
            "plan": draft}),
    );
    command(
        &*real,
        "tasks.set_feature",
        json!({"taskId": card.id.as_str(), "expectedRevision": card.revision.as_str(),
            "clearFeature": true}),
    );
    // The editor round-trips the stored coverage it does not show; the save still succeeds and
    // the departed card leaves the requirement's coverage.
    h.state_mut().work.plan = None;
    settle(&mut h);
    save_plan_now(&mut h, "Checkout", "pay by card");
    settle(&mut h);
    assert!(
        h.state().work.plan.is_none(),
        "the save settled: {:?}",
        h.state().work.message
    );
    let saved = get(feature.id.as_str()).pending_plan.unwrap().draft;
    assert_eq!(saved.requirements.0[0].statement.0, "pay by card");
    assert!(saved.requirements.0[0].covered_by_task_ids.0.is_empty());
}

fn approve_plan(
    engine: &devworks_desktop::host::Engine,
    backend: &dyn Backend,
    feature: &WorkRecord,
) -> WorkRecord {
    use devworks_core::ports::storage::{StorageError, WriteOptions, run_write};
    use devworks_core::protocol::results::OperationResult;
    let pending = feature.pending_plan.as_ref().unwrap();
    let proposal = match ops::command_result(backend.command(
        "features.request_plan_approval",
        &wops::work_command(
            backend,
            "features.request_plan_approval",
            json!({"featureId":feature.id,
            "expectedRevision":feature.revision, "planRevision":pending.revision,
            "planDigest":devworks_core::services::work::plan_digest(&pending.draft)}),
        ),
    ))
    .unwrap()
    {
        OperationResult::FeaturesRequestPlanApproval { value } => value,
        other => panic!("{other:?}"),
    };
    let ctx = engine.host().user().clone();
    run_write(
        &**engine.host().runtime().store(),
        WriteOptions::default(),
        move |tx| {
            devworks_core::services::work::plans::decide_plan_approval(
                tx,
                &ctx,
                &proposal.id,
                1,
                true,
            )
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
        },
    )
    .unwrap()
}

fn initial_approved_plan(
    engine: &devworks_desktop::host::Engine,
    backend: &dyn Backend,
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
) -> WorkRecord {
    let project = ops::project_changed(backend.command(
        "projects.create",
        &ops::project_create(backend, "Shop", "", &[]),
    ))
    .unwrap();
    let feature = features_from_screen(h, backend, project.id.0.as_str(), &["Checkout"]).remove(0);
    click_row(h, "Checkout  ·");
    click(h, Role::Button, "Edit plan");
    {
        let form = h.state_mut().work.plan.as_mut().unwrap();
        form.text = "Ship checkout".into();
        form.requirements
            .push(devworks_desktop::shell::work::Entry {
                id: "E1".into(),
                text: "Pay by card".into(),
            });
    }
    click(h, Role::Button, "Save plan draft");
    let saved = wops::work_record(backend.query(
        "features.get",
        &wops::work_get(backend, "features.get", feature.id.as_str()),
    ))
    .unwrap();
    let approved = approve_plan(engine, backend, &saved);
    h.state_mut()
        .work
        .detail
        .insert(approved.id.as_str().to_owned(), approved.clone());
    click(h, Role::Button, "Edit plan");
    assert!(
        h.get_by_role_and_label(Role::TextInput, "Requirement 1 ID")
            .accesskit_node()
            .is_disabled()
    );
    approved
}

fn decision_text(
    backend: &dyn Backend,
    reference: &devworks_core::protocol::provider::ArtifactRef,
) -> String {
    match ops::read_content(
        backend.query("artifacts.read", &ops::artifact_read(backend, reference)),
    )
    .unwrap()
    {
        devworks_core::protocol::records::ArtifactReadContent::Text { content, .. } => content.0,
        other => panic!("{other:?}"),
    }
}

#[test]
fn refreshing_a_cached_feature_shows_independent_readiness_changes() {
    let root = Root::new("readiness-after-unlink");
    let engine = start(&root);
    let backend = engine.backend();
    let gate = Gate::new("features.get");
    let mut h = harness(
        shell_with(
            Ok(gate.over(backend.clone())),
            &root,
            Script::default(),
            None,
        ),
        1600.0,
        1300.0,
    );
    struct Release(Gate);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.open();
        }
    }
    let _release = Release(gate.clone());
    settle(&mut h);
    let f = initial_approved_plan(&engine, &*backend, &mut h);
    h.state_mut().work.plan = None;
    let card = command(
        &*backend,
        "tasks.create",
        json!({"projectId": f.project_id,
        "parentId": f.id, "title":"Pay button", "description":"", "repositoryIds":[],
        "statusId":default_status_id(&f.project_id,"backlog"),"priority":0}),
    );
    let mut requirements = serde_json::to_value(&f.requirements).unwrap();
    requirements[0]["coveredByTaskIds"] = json!([card.id]);
    let f = command(
        &*backend,
        "features.edit_plan",
        json!({"featureId":f.id,
        "expectedRevision":f.revision,"plan":{"content":f.plan,"goals":f.goals,
        "repositoryIds":f.repository_ids,"requirements":requirements,
        "acceptance":f.acceptance,"acceptanceTests":f.acceptance_tests.map(|x| x.0).unwrap_or_default()}}),
    );
    let f = approve_plan(&engine, &*backend, &f);
    let approval = f.approval.as_ref().unwrap();
    let f = command(
        &*backend,
        "features.edit",
        json!({"workId":f.id,"expectedRevision":f.revision,
        "changes":{"readiness":{"kind":"Assess","planRevision":approval.plan_revision,
        "planDigest":approval.plan_digest,"summary":"Ready to implement","findings":[]}}}),
    );
    assert!(f.readiness.as_ref().unwrap().ready);
    h.state_mut()
        .work
        .detail
        .insert(f.id.as_str().into(), f.clone());
    click(&mut h, Role::Button, "Refresh");
    assert!(
        h.state()
            .work
            .freshest(f.id.as_str())
            .unwrap()
            .readiness
            .unwrap()
            .ready
    );
    command(
        &*backend,
        "tasks.set_feature",
        json!({"taskId":card.id,
        "expectedRevision":card.revision,"clearFeature":true}),
    );
    gate.close();
    h.get_by_role_and_label(Role::Button, "Refresh").click();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !h
        .state()
        .work
        .features
        .items
        .iter()
        .any(|w| w.id == f.id && w.readiness.as_ref().is_some_and(|r| !r.ready))
    {
        assert!(
            std::time::Instant::now() < until,
            "the refreshed list arrives while full detail is held"
        );
        h.step();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        h.state()
            .work
            .detail
            .get(f.id.as_str())
            .unwrap()
            .readiness
            .as_ref()
            .unwrap()
            .ready,
        "the full-detail reply is still held"
    );
    let shown = h.state().work.freshest(f.id.as_str()).unwrap();
    assert_eq!(shown.revision, f.revision, "coverage has its own revision");
    let r = shown.readiness.unwrap();
    assert!(
        !r.ready,
        "the refreshed detail must show the independent blocker"
    );
    let finding = r.findings.0.iter().find(|x| x.blocking).unwrap();
    assert!(
        has_text(&h, &finding.summary.0),
        "the detail renders the new finding"
    );
    gate.open();
    settle(&mut h);
}

#[test]
fn refreshing_a_cached_prerequisite_shows_new_dependents_at_the_same_work_revision() {
    let root = Root::new("blocks-after-refresh");
    let engine = start(&root);
    let backend = engine.backend();
    let pid = shop(&*backend);
    let project =
        devworks_core::protocol::ProjectId(devworks_core::protocol::Id::new(pid.clone()).unwrap());
    let issue = |title| {
        command(
            &*backend,
            "issues.create",
            json!({"projectId":pid,"title":title,
        "description":"","repositoryIds":[],"priority":0,"statusId":default_status_id(&project,"backlog")}),
        )
    };
    let prerequisite = issue("Prerequisite");
    let dependent = issue("Dependent");
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1000.0);
    settle(&mut h);
    h.state_mut().select_project(Some(pid));
    h.state_mut().navigate(Route::Issues);
    settle(&mut h);
    click_row(&mut h, "Prerequisite  ·");
    assert!(h.state().work.detail.contains_key(prerequisite.id.as_str()));
    assert!(has_text(&h, "It blocks nothing."));
    command(
        &*backend,
        "tasks.dependency_links",
        json!({"taskId":dependent.id,
        "expectedRevision":dependent.revision,"replacement":{"edges":[{
            "predecessor":prerequisite.id,"successor":dependent.id}]}}),
    );
    click(&mut h, Role::Button, "Refresh");
    let shown = h.state().work.freshest(prerequisite.id.as_str()).unwrap();
    assert_eq!(
        shown.revision, prerequisite.revision,
        "the reverse view is derived"
    );
    assert_eq!(
        shown.dependent_ids.0,
        vec![dependent.id],
        "Refresh must update Blocks"
    );
    assert!(!has_text(&h, "It blocks nothing."));
}

#[test]
fn an_approved_requirement_is_dropped_with_a_project_decision_and_can_be_restored() {
    restored_requirement_case(false);
}

#[test]
fn a_delayed_native_detail_reply_preserves_the_newer_plan_and_restore_gesture() {
    restored_requirement_case(true);
}

fn restored_requirement_case(delayed_detail: bool) {
    use devworks_core::ports::storage::{ArtifactScope, read_snapshot};
    use devworks_core::protocol::records::RequirementStatus;
    let root = Root::new("plan-drop-decision");
    let engine = start(&root);
    let backend = engine.backend();
    let gate = Gate::holding_replies("features.get").missing_hints();
    let shell = if delayed_detail {
        shell_with(
            Ok(gate.over(backend.clone())),
            &root,
            Script::default(),
            None,
        )
    } else {
        shell(&engine, &root, Script::default())
    };
    let mut h = harness(shell, 1600.0, 1100.0);
    settle(&mut h);
    let feature = initial_approved_plan(&engine, &*backend, &mut h);
    h.state_mut().work.plan.as_mut().unwrap().text = "Revised checkout plan".into();
    click(&mut h, Role::Button, "Drop requirement E1");
    assert!(
        h.get_by_role_and_label(Role::Button, "Save plan draft")
            .accesskit_node()
            .is_disabled()
    );
    type_into(
        &mut h,
        Role::TextInput,
        "Decision to drop E1",
        "Payments moved to another feature",
    );
    click(&mut h, Role::Button, "Save plan draft");
    assert!(h.state().work.plan.is_none());
    let saved = wops::work_record(backend.query(
        "features.get",
        &wops::work_get(&*backend, "features.get", feature.id.as_str()),
    ))
    .unwrap();
    assert!(
        matches!(saved.requirements.0[0].status, RequirementStatus::Confirmed),
        "the active plan is untouched"
    );
    assert!(
        decision_text(
            &*backend,
            &saved.pending_plan.as_ref().unwrap().draft.content
        )
        .contains("Revised checkout plan")
    );
    let dropped = &saved.pending_plan.as_ref().unwrap().draft.requirements.0[0];
    assert!(matches!(dropped.status, RequirementStatus::Dropped));
    let reference = dropped.drop_decision.as_ref().unwrap();
    assert!(decision_text(&*backend, reference).contains("Payments moved to another feature"));
    let aid = reference.artifact_id.clone();
    let artifact = read_snapshot(
        &**engine.host().runtime().store(),
        &Default::default(),
        move |s| s.artifacts().artifact(&aid),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        artifact.scope,
        ArtifactScope::Project(feature.project_id.clone())
    );
    if delayed_detail {
        gate.close();
        h.get_by_role_and_label(Role::Button, "Refresh").click();
        let deadline = std::time::Instant::now() + SETTLE_BOUND;
        while !gate.has_waiter() {
            h.step();
            if std::time::Instant::now() >= deadline {
                gate.open();
                panic!("the real pre-approval detail reply did not reach the gate");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    let approved_drop = approve_plan(&engine, &*backend, &saved);
    let approved_revision = approved_drop.revision.clone();
    h.state_mut()
        .work
        .detail
        .insert(feature.id.as_str().to_owned(), approved_drop);
    if delayed_detail {
        h.step();
        h.get_by_role_and_label(Role::Button, "Edit plan").click();
        h.step();
        h.step();
        h.get_by_label("Dropped requirements").click();
        h.step();
        h.step();
        gate.open();
    } else {
        click(&mut h, Role::Button, "Edit plan");
        h.get_by_label("Dropped requirements").click();
    }
    settle(&mut h);
    if delayed_detail {
        assert_eq!(
            h.state()
                .work
                .freshest(feature.id.as_str())
                .unwrap()
                .revision,
            approved_revision,
            "an older native read must not replace a known approval"
        );
    }
    h.get_by_role_and_label(Role::Button, "Restore requirement E1")
        .scroll_to_me();
    settle(&mut h);
    let restore_rect = h
        .get_by_role_and_label(Role::Button, "Restore requirement E1")
        .rect();
    h.get_by_role_and_label(Role::Button, "Restore requirement E1")
        .click();
    settle(&mut h);
    assert!(
        h.state()
            .work
            .plan
            .as_ref()
            .unwrap()
            .requirements
            .iter()
            .any(|r| r.id == "E1"),
        "Restore gesture was not applied; rect={restore_rect:?}, current={:?}, detail_revision={:?}, message={:?}",
        h.query_by_role_and_label(Role::Button, "Restore requirement E1")
            .map(|n| n.rect()),
        h.state()
            .work
            .freshest(feature.id.as_str())
            .map(|f| f.revision),
        h.state().work.message,
    );
    click(&mut h, Role::ComboBox, "Status of requirement E1");
    h.get_by_label("Confirmed").click();
    settle(&mut h);
    click(&mut h, Role::Button, "Save plan draft");
    let restored = wops::work_record(backend.query(
        "features.get",
        &wops::work_get(&*backend, "features.get", feature.id.as_str()),
    ))
    .unwrap();
    assert!(matches!(
        restored.requirements.0[0].status,
        RequirementStatus::Dropped
    ));
    let pending = &restored.pending_plan.unwrap().draft.requirements.0[0];
    assert!(matches!(pending.status, RequirementStatus::Confirmed));
    assert!(pending.drop_decision.is_none());
}

#[test]
fn later_drop_decision_edits_are_preserved_across_the_save() {
    drop_save_race(false);
}
#[test]
fn keeping_a_requirement_during_its_drop_save_preserves_the_later_draft() {
    drop_save_race(true);
}

fn drop_save_race(keep: bool) {
    use devworks_core::protocol::records::RequirementStatus;
    struct OpenOnDrop(Gate);
    impl Drop for OpenOnDrop {
        fn drop(&mut self) {
            self.0.open();
        }
    }
    let root = Root::new("plan-drop-race");
    let engine = start(&root);
    let real = engine.backend();
    let gate = Gate::new("artifacts.save");
    let mut h = harness(
        shell_with(Ok(gate.over(real.clone())), &root, Script::default(), None),
        1600.0,
        1100.0,
    );
    settle(&mut h);
    let feature = initial_approved_plan(&engine, &*real, &mut h);
    click(&mut h, Role::Button, "Drop requirement E1");
    type_into(
        &mut h,
        Role::TextInput,
        "Decision to drop E1",
        "First submitted decision",
    );
    let _release = OpenOnDrop(gate.clone());
    gate.close();
    h.get_by_role_and_label(Role::Button, "Save plan draft")
        .click();
    h.run_steps(3);
    {
        let form = h.state_mut().work.plan.as_mut().unwrap();
        form.goals.push("A later goal".into());
        if keep {
            form.drops.remove("E1");
        } else {
            form.drops.get_mut("E1").unwrap().reason = "Later decision".into();
        }
    }
    gate.open();
    settle(&mut h);
    let saved = wops::work_record(real.query(
        "features.get",
        &wops::work_get(&*real, "features.get", feature.id.as_str()),
    ))
    .unwrap();
    let first = &saved.pending_plan.as_ref().unwrap().draft;
    assert!(first.goals.0.is_empty());
    assert!(
        decision_text(
            &*real,
            first.requirements.0[0].drop_decision.as_ref().unwrap()
        )
        .contains("First submitted decision")
    );
    assert!(h.state().work.plan.is_some(), "the later draft stays open");
    click(&mut h, Role::Button, "Save plan draft");
    assert!(h.state().work.plan.is_none());
    let saved = wops::work_record(real.query(
        "features.get",
        &wops::work_get(&*real, "features.get", feature.id.as_str()),
    ))
    .unwrap();
    let pending = saved.pending_plan.unwrap().draft;
    assert_eq!(pending.goals.0[0].0, "A later goal");
    let requirement = &pending.requirements.0[0];
    if keep {
        assert!(matches!(requirement.status, RequirementStatus::Confirmed));
        assert!(requirement.drop_decision.is_none());
    } else {
        assert!(matches!(requirement.status, RequirementStatus::Dropped));
        assert!(
            decision_text(&*real, requirement.drop_decision.as_ref().unwrap())
                .contains("Later decision")
        );
    }
}

#[test]
fn a_drop_decision_upload_failure_keeps_the_draft_and_changes_no_plan() {
    let root = Root::new("plan-drop-failure");
    let engine = start(&root);
    let backend = engine.backend();
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1100.0);
    settle(&mut h);
    let feature = initial_approved_plan(&engine, &*backend, &mut h);
    click(&mut h, Role::Button, "Drop requirement E1");
    type_into(
        &mut h,
        Role::TextInput,
        "Decision to drop E1",
        "Keep this decision after an IO failure",
    );
    let scratch = backend.scratch_dir();
    let backup = scratch.with_file_name("saved-plan-scratch");
    std::fs::rename(&scratch, &backup).unwrap();
    std::fs::write(&scratch, b"cannot create a directory here").unwrap();
    click(&mut h, Role::Button, "Save plan draft");
    std::fs::remove_file(&scratch).unwrap();
    std::fs::rename(&backup, &scratch).unwrap();
    assert!(h.state().work.plan_saving.is_empty());
    let form = h
        .state()
        .work
        .plan
        .as_ref()
        .expect("the failed draft stays open");
    assert!(form.drops["E1"].reason.contains("IO failure"));
    let saved = wops::work_record(backend.query(
        "features.get",
        &wops::work_get(&*backend, "features.get", feature.id.as_str()),
    ))
    .unwrap();
    assert!(
        saved.pending_plan.is_none(),
        "upload failure never reaches edit_plan"
    );
    click(&mut h, Role::Button, "Save plan draft");
    assert!(
        h.state().work.plan.is_none(),
        "retry settles the original draft"
    );
}

#[test]
fn a_drop_decision_upload_pending_at_close_finishes_after_relaunch() {
    use devworks_core::protocol::records::RequirementStatus;
    let root = Root::new("plan-drop-relaunch");
    let engine = start(&root);
    let real = engine.backend();
    let gate = Gate::new("artifacts.save");
    let mut h = harness(
        shell_with(Ok(gate.over(real.clone())), &root, Script::default(), None),
        1600.0,
        1100.0,
    );
    settle(&mut h);
    let feature = initial_approved_plan(&engine, &*real, &mut h);
    click(&mut h, Role::Button, "Drop requirement E1");
    type_into(
        &mut h,
        Role::TextInput,
        "Decision to drop E1",
        "Retain this decision across close",
    );
    gate.close();
    h.get_by_role_and_label(Role::Button, "Save plan draft")
        .click();
    h.run_steps(3);
    h.state_mut().on_exit();
    drop(h);
    gate.open();
    let mut h = harness(
        shell_with(Ok(real.clone()), &root, Script::default(), None),
        1600.0,
        1100.0,
    );
    settle(&mut h);
    let saved = wops::work_record(real.query(
        "features.get",
        &wops::work_get(&*real, "features.get", feature.id.as_str()),
    ))
    .unwrap();
    let requirement = &saved.pending_plan.unwrap().draft.requirements.0[0];
    assert!(matches!(requirement.status, RequirementStatus::Dropped));
    assert!(
        decision_text(&*real, requirement.drop_decision.as_ref().unwrap())
            .contains("Retain this decision across close")
    );
    assert!(h.state().work.plan_saving.is_empty());
    assert!(h.state().work.plan.is_none());
}

#[test]
fn next_task_selects_within_the_approved_assessed_feature_without_starting_it() {
    let root = Root::new("work-native-next-feature-task");
    let engine = start(&root);
    let backend = engine.backend();
    let spy = LoseAcks::over(backend.clone(), "next_work.claim_feature_task", 0);
    let mut h = harness(
        shell_with(Ok(spy.clone()), &root, Script::default(), None),
        1800.0,
        1500.0,
    );
    settle(&mut h);
    let feature = initial_approved_plan(&engine, &*backend, &mut h);
    h.state_mut().work.plan = None;
    let card = command(
        &*backend,
        "tasks.create",
        json!({"projectId":feature.project_id,"parentId":feature.id,
        "title":"Selected feature task","description":"Implement the approved pay-by-card requirement.","repositoryIds":[],
        "statusId":default_status_id(&feature.project_id,"ready"),"priority":10}),
    );
    let mut requirements = serde_json::to_value(&feature.requirements).unwrap();
    requirements[0]["coveredByTaskIds"] = json!([card.id]);
    let feature = command(
        &*backend,
        "features.edit_plan",
        json!({"featureId":feature.id,"expectedRevision":feature.revision,
        "plan":{"content":feature.plan,"goals":feature.goals,"repositoryIds":feature.repository_ids,"requirements":requirements,
        "acceptance":feature.acceptance,"acceptanceTests":feature.acceptance_tests.map(|a|a.0).unwrap_or_default()}}),
    );
    let feature = approve_plan(&engine, &*backend, &feature);
    let approval = feature.approval.as_ref().unwrap();
    let feature = command(
        &*backend,
        "features.edit",
        json!({"workId":feature.id,"expectedRevision":feature.revision,
        "changes":{"statusId":default_status_id(&feature.project_id,"ready"),"readiness":{"kind":"Assess","planRevision":approval.plan_revision,"planDigest":approval.plan_digest,
        "summary":"Ready to implement the approved feature","findings":[]}}}),
    );
    h.state_mut()
        .work
        .detail
        .insert(feature.id.as_str().into(), feature.clone());
    click(&mut h, Role::Button, "Refresh");
    h.get_by_role_and_label(Role::Button, "Next task")
        .click_accesskit();
    settle(&mut h);
    let claim = h
        .state()
        .work
        .allocation_claims
        .values()
        .find(|claim| claim.work_id == card.id)
        .unwrap();
    assert_eq!(claim.root_id.as_ref().unwrap().0, feature.id);
    let frames = spy.frames.lock().unwrap();
    let frame: serde_json::Value = serde_json::from_slice(&frames[0]).unwrap();
    assert_eq!(frame["body"]["rootId"], json!(feature.id));
    assert_eq!(frame["schemaRevision"], 3);
    let card = wops::work_record(backend.query(
        "tasks.get",
        &wops::work_get(&*backend, "tasks.get", card.id.as_str()),
    ))
    .unwrap();
    assert!(
        card.attempts.0.is_empty(),
        "selection does not dispatch the task"
    );
    assert!(has_text(&h, "Open feature controls"));
    click(&mut h, Role::Button, "Board");
    h.get_by_role_and_label(Role::Button, "Open feature controls")
        .click_accesskit();
    settle(&mut h);
    assert_eq!(
        h.state().route_memory(Route::Features).tab,
        0,
        "the explicit feature link opens its detail, not a previouslyselectedboard"
    );
}

/// Seed recorded decisions through the real write owner, without executing 10,000 commands or
/// their unrelated publication jobs. Rendering and every query still use the native gateway.
fn seed_history(
    engine: &devworks_desktop::host::Engine,
    project: &str,
    card: &str,
    n: usize,
) -> i64 {
    use devworks_core::ports::storage::{NewEvent, write_job};
    use devworks_core::protocol::{EventId, Id, ProjectId, RootId};
    let project = ProjectId(Id::new(project).unwrap());
    let root = RootId(Id::new(card).unwrap());
    let actor = engine.host().user().principal().clone();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    engine.host().runtime().store().submit_write(Default::default(),write_job(move |tx| {
        let mut last=0;
        for i in 0..n {
            last=tx.history().append(NewEvent {
                id:EventId(Id::new(format!("history-fixture-event-{i}")).unwrap()),actor:actor.clone(),
                project_id:Some(project.clone()),root_id:Some(root.clone()),operation_id:None,
                aggregate_kind:"command".into(),aggregate_id:format!("history-fixture-command-{i}"),
                aggregate_revision:1,kind:"CommandDecided".into(),
                payload:json!({"subject":{"kind":"root","dimension":"Work","id":root.0.as_str(),"revision":1}}),
            })?;
        }
        Ok(last)
    }, move |result| {tx.send(result).unwrap();})).unwrap();
    rx.recv_timeout(std::time::Duration::from_secs(30))
        .unwrap()
        .unwrap()
}

#[test]
fn ten_thousand_events_show_the_newest_page_after_one_native_request() {
    use devworks_desktop::work_ops::ACTIVITY_PAGE;
    let _case = native_runtime_case();
    let root = Root::new("history-10000");
    let engine = start(&root);
    let real = engine.backend();
    let pid = shop(&*real);
    let status = default_status_id(
        &devworks_core::protocol::ProjectId(devworks_core::protocol::Id::new(&pid).unwrap()),
        "backlog",
    );
    let card = command(
        &*real,
        "issues.create",
        json!({"projectId":pid,"title":"History card","description":"","repositoryIds":[],"statusId":status,"priority":0}),
    );
    let newest = seed_history(&engine, &pid, card.id.as_str(), 10000);
    let counted = std::sync::Arc::new(CountHistory {
        inner: real,
        requests: Default::default(),
    });
    let mut h = harness(
        shell_with(Ok(counted.clone()), &root, Script::default(), None),
        1600.0,
        1000.0,
    );
    settle(&mut h);
    h.state_mut().select_project(Some(pid.clone()));
    h.state_mut().navigate(Route::Overview);
    settle(&mut h);
    click(&mut h, Role::Button, "Activity");
    assert_eq!(
        counted.requests.lock().unwrap().len(),
        1,
        "no automatic traversal of old history"
    );
    let feed = &h.state().work.activity;
    assert_eq!(feed.items.len(), usize::from(ACTIVITY_PAGE));
    assert_eq!(feed.items[0].sequence.as_str(), newest.to_string());
    assert!(feed.more);
    click(&mut h, Role::Button, "Load older changes");
    assert_eq!(counted.requests.lock().unwrap().len(), 2);
    assert_eq!(
        h.state().work.activity.items.len(),
        2 * usize::from(ACTIVITY_PAGE)
    );
    // A card gets its own bounded root feed, even after project pages have been loaded.
    h.state_mut().navigate(Route::Issues);
    settle(&mut h);
    click_row(&mut h, "History card  ·");
    assert_eq!(counted.requests.lock().unwrap().len(), 3);
    let feed = &h.state().work.histories[card.id.as_str()];
    assert_eq!(feed.items.len(), usize::from(ACTIVITY_PAGE));
    assert_eq!(feed.items[0].sequence.as_str(), newest.to_string());
    click(&mut h, Role::Button, "Load older card changes");
    assert_eq!(counted.requests.lock().unwrap().len(), 4);
    assert_eq!(
        h.state().work.histories[card.id.as_str()].items.len(),
        2 * usize::from(ACTIVITY_PAGE)
    );
    click(&mut h, Role::Button, "Show its history");
    assert_eq!(
        counted.requests.lock().unwrap().len(),
        4,
        "opening the already loaded card feed does not fetch it again"
    );
    click(&mut h, Role::Button, "Load older changes");
    assert_eq!(counted.requests.lock().unwrap().len(), 5);
    let feed = &h.state().work.histories[card.id.as_str()];
    assert_eq!(feed.items.len(), 3 * usize::from(ACTIVITY_PAGE));
    assert!(
        feed.items
            .windows(2)
            .all(|w| w[0].sequence.as_str().parse::<i64>().unwrap()
                > w[1].sequence.as_str().parse::<i64>().unwrap())
    );
    // A genuinely refused continuation keeps visible rows and offers an explicit fresh reload.
    h.state_mut()
        .work
        .histories
        .get_mut(card.id.as_str())
        .unwrap()
        .resume = Some("expired".into());
    click(&mut h, Role::Button, "Load older changes");
    assert!(h.state().work.histories[card.id.as_str()].error.is_some());
    assert_eq!(
        h.state().work.histories[card.id.as_str()].items.len(),
        3 * usize::from(ACTIVITY_PAGE)
    );
    click(&mut h, Role::Button, "Reload history");
    let feed = &h.state().work.histories[card.id.as_str()];
    assert!(feed.error.is_none());
    assert_eq!(feed.items.len(), 3 * usize::from(ACTIVITY_PAGE));
    assert_eq!(feed.items[0].sequence.as_str(), newest.to_string());
    let requests = counted.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .all(|v| v.pointer("/query/newestFirst") == Some(&json!(true)))
    );
}
