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
    assert!(has_text(&h, "full history"));
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
    // The plan text is uploading while the user keeps writing.
    gate.close();
    h.get_by_role_and_label(Role::Button, "Save plan draft")
        .click();
    h.run_steps(3);
    assert!(
        h.state().work.plan_saving.is_some(),
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
        h.state().work.edit_saving.is_some(),
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
