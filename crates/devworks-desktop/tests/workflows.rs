//! Project workflow and atomic manual allocation through the real native gateway. All provider
//! transports are disabled; selecting work never runs inference or starts execution.
mod common;
use common::*;
use devworks_core::protocol::records::{StatusRecord, WorkRecord};
use devworks_desktop::backend::Backend;
use devworks_desktop::ops;
use devworks_desktop::route::Route;
use devworks_desktop::work_ops as wops;
use eframe::egui::{self, accesskit::Role};
use egui_kittest::kittest::{NodeT, Queryable};
use serde_json::{Value, json};
type Ui = egui_kittest::Harness<'static, devworks_desktop::shell::Shell>;
fn press(h: &mut Ui, label: &str) {
    h.get_by_role_and_label(Role::Button, label)
        .click_accesskit();
    settle(h);
}
fn input(h: &mut Ui, label: &str, text: &str) {
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.step();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.step();
    h.get_by_role_and_label(Role::TextInput, label)
        .type_text(text);
    h.step();
}
fn choose(h: &mut Ui, label: &str, option: &str) {
    h.get_by_role_and_label(Role::ComboBox, label)
        .click_accesskit();
    h.step();
    h.step();
    h.query_all_by_label(option)
        .find(|n| {
            matches!(
                n.accesskit_node().role(),
                Role::Button | Role::ListBoxOption
            )
        })
        .unwrap_or_else(|| panic!("{option} in {label}"))
        .click_accesskit();
    settle(h);
}
fn project(b: &dyn Backend, name: &str) -> String {
    ops::project_changed(b.command("projects.create", &ops::project_create(b, name, "", &[])))
        .unwrap()
        .id
        .0
        .as_str()
        .into()
}
fn statuses(b: &dyn Backend, p: &str) -> Vec<StatusRecord> {
    wops::workflow_page(b.query("workflows.list", &wops::workflow_list(b, p, None)))
        .unwrap()
        .0
}
fn create_issue(b: &dyn Backend, p: &str, title: &str, status: &str) -> WorkRecord {
    wops::work_changed(b.command("issues.create",&wops::work_command(b,"issues.create",json!({
        "projectId":p,"title":title,"description":"A concrete actionable native test issue.","repositoryIds":[],"priority":10,"statusId":status
    })))).unwrap()
}
fn open(engine: &devworks_desktop::host::Engine, root: &Root, p: &str) -> Ui {
    let mut h = harness(shell(engine, root, Script::default()), 1800.0, 1300.0);
    settle(&mut h);
    h.state_mut().select_project(Some(p.into()));
    h.state_mut().navigate(Route::Issues);
    settle(&mut h);
    h
}
fn close_editor(h: &mut Ui) {
    let p = h.state().work.of.clone().unwrap();
    h.state_mut().work.workflows.get_mut(&p).unwrap().open = false;
    settle(h);
}

#[test]
fn custom_columns_rename_order_and_occupied_removal_use_backend_fences() {
    let root = Root::new("workflow-native-custom");
    let engine = start(&root);
    let b = engine.backend();
    let p = project(&*b, "Workflow project");
    let mut h = open(&engine, &root, &p);
    press(&mut h, "Project workflow");
    press(&mut h, "New status");
    input(&mut h, "Status name", "Incoming");
    choose(&mut h, "Status meaning", "Backlog");
    press(&mut h, "Create status");
    let incoming = statuses(&*b, &p)
        .into_iter()
        .find(|s| s.label.0 == "Incoming")
        .unwrap();
    press(&mut h, "Rename Incoming");
    input(&mut h, "Status name", "Intake");
    press(&mut h, "Save status name");
    let before_order = statuses(&*b, &p);
    let position = before_order
        .iter()
        .position(|s| s.id == incoming.id)
        .unwrap();
    let (direction, next) = if position > 0 {
        ("earlier", position - 1)
    } else {
        ("later", 1)
    };
    let mut expected: Vec<_> = before_order.iter().map(|s| s.id.clone()).collect();
    expected.swap(position, next);
    press(&mut h, &format!("Move Intake column {direction}"));
    let ordered = statuses(&*b, &p);
    assert_eq!(
        ordered.iter().map(|s| s.id.clone()).collect::<Vec<_>>(),
        expected
    );
    let intake = ordered.iter().find(|s| s.id == incoming.id).unwrap();
    assert_eq!(
        intake.category.0, "Backlog",
        "renaming never changes meaning"
    );
    let issue = create_issue(&*b, &p, "Preserved work", intake.id.as_str());
    // Refresh the backend-owned occupancy snapshot before opening removal.
    press(&mut h, "Refresh");
    press(&mut h, "Remove Intake column");
    assert!(has_text(&h, "1 work records will move together"));
    assert!(
        h.get_by_role_and_label(Role::Button, "Reassign work and remove status")
            .accesskit_node()
            .is_disabled(),
        "destination must be chosen before removal"
    );
    choose(&mut h, "Reassign work to", "Backlog");
    press(&mut h, "Reassign work and remove status");
    assert!(statuses(&*b, &p).iter().all(|s| s.id != incoming.id));
    let changed = wops::work_record(b.query(
        "tasks.get",
        &wops::work_get(&*b, "tasks.get", issue.id.as_str()),
    ))
    .unwrap();
    assert_eq!(changed.id, issue.id);
    assert_ne!(changed.status_id, incoming.id);
    close_editor(&mut h);
    assert!(has_text(&h, "Preserved work"));
}

#[test]
fn a_stale_status_editor_keeps_its_name_until_explicit_current_review() {
    let root = Root::new("workflow-native-stale");
    let engine = start(&root);
    let b = engine.backend();
    let p = project(&*b, "Stale workflow");
    let mut h = open(&engine, &root, &p);
    press(&mut h, "Project workflow");
    press(&mut h, "Rename Ready");
    input(&mut h, "Status name", "Authored ready name");
    let ready = statuses(&*b, &p)
        .into_iter()
        .find(|s| s.category.0 == "Ready")
        .unwrap();
    wops::workflow_changed(b.command("workflows.rename_status",&wops::workflow_command(&*b,"workflows.rename_status",json!({
        "projectId":p,"statusId":ready.id,"expectedRevision":ready.revision,"label":"Changed elsewhere"
    })))).unwrap();
    settle(&mut h);
    press(&mut h, "Save status name");
    assert!(has_text(&h, "Your draft is kept"));
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, "Status name")
            .accesskit_node()
            .value()
            .unwrap(),
        "Authored ready name"
    );
    press(&mut h, "Review current workflow");
    press(&mut h, "Save status name");
    assert!(
        statuses(&*b, &p)
            .iter()
            .any(|s| s.label.0 == "Authored ready name")
    );
}

#[test]
fn empty_queue_is_truthful_and_a_manual_claim_never_starts_execution() {
    let root = Root::new("allocation-native-empty");
    let engine = start(&root);
    let b = engine.backend();
    let p = project(&*b, "Empty queue");
    let mut h = open(&engine, &root, &p);
    input(&mut h, "Claim lease (ms)", "0");
    assert!(
        h.get_by_role_and_label(Role::Button, "Next work")
            .accesskit_node()
            .is_disabled()
    );
    input(&mut h, "Claim lease (ms)", "300000");
    press(&mut h, "Next work");
    assert!(has_text(&h, "No eligible work"));
    assert!(h.state().work.allocation_claims.is_empty());
    let ready = statuses(&*b, &p)
        .into_iter()
        .find(|s| s.category.0 == "Ready")
        .unwrap();
    let issue = create_issue(&*b, &p, "Pick this issue", ready.id.as_str());
    press(&mut h, "Refresh");
    press(&mut h, "Next work");
    let claim = h
        .state()
        .work
        .allocation_claims
        .values()
        .find(|c| c.work_id == issue.id)
        .unwrap()
        .clone();
    assert!(has_text(&h, "Selected: Pick this issue"));
    assert!(claim.work_kind.is_some() && claim.work_revision.is_some());
    let record = wops::work_record(b.query(
        "tasks.get",
        &wops::work_get(&*b, "tasks.get", issue.id.as_str()),
    ))
    .unwrap();
    assert!(
        record.attempts.0.is_empty(),
        "selection did not start a run"
    );
    press(&mut h, &format!("Inspect claim {}", claim.id.as_str()));
    press(&mut h, &format!("Release claim {}", claim.id.as_str()));
    assert!(matches!(
        h.state().work.allocation_claims[claim.id.as_str()].state,
        devworks_core::protocol::records::LeaseState::Released
    ));
}

#[test]
fn allocation_unknown_outcomes_replay_the_same_claim_and_survive_relaunch() {
    let root = Root::new("allocation-native-replay");
    let engine = start(&root);
    let real = engine.backend();
    let p = project(&*real, "Recover claim");
    let ready = statuses(&*real, &p)
        .into_iter()
        .find(|s| s.category.0 == "Ready")
        .unwrap();
    let issue = create_issue(&*real, &p, "Durably selected issue", ready.id.as_str());
    let lossy = LoseAcks::over(real.clone(), "next_work.claim_project_queue", 100);
    let mut h = harness(
        shell_with(Ok(lossy.clone()), &root, Script::default(), None),
        1800.0,
        1300.0,
    );
    settle(&mut h);
    h.state_mut().select_project(Some(p.clone()));
    h.state_mut().navigate(Route::Issues);
    settle(&mut h);
    press(&mut h, "Next work");
    assert!(
        h.state()
            .client()
            .unwrap()
            .unresolved()
            .iter()
            .any(|u| u.operation() == "next_work.claim_project_queue")
    );
    let frame = lossy.frames.lock().unwrap()[0].clone();
    h.state_mut().on_exit();
    drop(h);
    let honest = LoseAcks::over(real.clone(), "next_work.claim_project_queue", 0);
    let mut h = harness(
        shell_with(Ok(honest.clone()), &root, Script::default(), None),
        1800.0,
        1300.0,
    );
    settle(&mut h);
    h.state_mut().select_project(Some(p));
    h.state_mut().navigate(Route::Issues);
    settle(&mut h);
    assert!(
        honest.frames.lock().unwrap().iter().all(|f| f == &frame),
        "exact saved frame, including filters and queue fence"
    );
    let claims: Vec<_> = h
        .state()
        .work
        .allocation_claims
        .values()
        .filter(|c| c.work_id == issue.id)
        .collect();
    assert_eq!(claims.len(), 1);
    assert!(has_text(&h, "Durably selected issue"));
}

#[test]
fn native_type_and_label_editing_preserves_unset_and_uses_selection_filters() {
    let root = Root::new("work-native-metadata");
    let engine = start(&root);
    let real = engine.backend();
    let p = project(&*real, "Typed work");
    let spy = LoseAcks::over(real.clone(), "next_work.claim_project_queue", 0);
    let mut h = harness(
        shell_with(Ok(spy.clone()), &root, Script::default(), None),
        1800.0,
        1500.0,
    );
    settle(&mut h);
    h.state_mut().select_project(Some(p.clone()));
    h.state_mut().navigate(Route::Issues);
    settle(&mut h);
    press(&mut h, "New issue");
    input(&mut h, "Title", "Typed issue");
    input(
        &mut h,
        "Description",
        "Implement the concrete issue and verify its acceptance.",
    );
    choose(&mut h, "Work type", "Bug");
    press(&mut h, "Add label");
    input(&mut h, "Label 1", "triage");
    press(&mut h, "Create");
    let created = wops::work_page(real.query(
        "issues.list",
        &wops::work_list(&*real, "issues.list", &p, None),
    ))
    .unwrap()
    .0
    .remove(0);
    assert!(matches!(
        created.task_type,
        Some(devworks_core::protocol::records::WorkItemType::Bug)
    ));
    assert_eq!(created.labels.as_ref().unwrap().0[0].0, "triage");
    h.query_all_by_label_contains("Typed issue  ·")
        .next()
        .unwrap()
        .click_accesskit();
    settle(&mut h);
    choose(&mut h, "Status", "Ready");
    h.query_by_label("Selection options")
        .unwrap()
        .click_accesskit();
    settle(&mut h);
    choose(&mut h, "Selection work type", "Bug");
    input(&mut h, "Selection label (exact label, optional)", "TRIAGE");
    input(
        &mut h,
        "Selection retry key (optional)",
        "native-filter-case",
    );
    press(&mut h, "Next work");
    let claim = h
        .state()
        .work
        .allocation_claims
        .values()
        .find(|c| c.work_id == created.id)
        .unwrap()
        .clone();
    let frame: Value = serde_json::from_slice(&spy.frames.lock().unwrap()[0]).unwrap();
    assert_eq!(frame["schemaRevision"], 3);
    assert_eq!(frame["body"]["taskType"], "Bug");
    assert_eq!(frame["body"]["label"], "TRIAGE");
    assert_eq!(frame["body"]["idempotencyKey"], "native-filter-case");
    press(&mut h, &format!("Release claim {}", claim.id.as_str()));
    press(&mut h, "Edit");
    press(&mut h, "Add label");
    input(&mut h, "Label 2", "TRIAGE");
    press(&mut h, "Save");
    assert!(
        h.state().work.edit.is_some(),
        "the backend refused duplicate labels and retained the editor"
    );
    press(&mut h, "Remove label 2");
    press(&mut h, "Remove label 1");
    choose(&mut h, "Work type", "Not specified");
    press(&mut h, "Save");
    let current = wops::work_record(real.query(
        "tasks.get",
        &wops::work_get(&*real, "tasks.get", created.id.as_str()),
    ))
    .unwrap();
    assert!(current.task_type.is_none());
    assert!(current.labels.unwrap().0.is_empty());
}
