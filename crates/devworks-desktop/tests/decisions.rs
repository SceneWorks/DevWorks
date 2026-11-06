//! Native Human decisions through the real gateway; no provider or inference execution.
mod common;
use common::*;
use devworks_core::protocol::records::{ApprovalViewRecord, WorkRecord};
use devworks_desktop::backend::{Backend, CommandDispatch};
use devworks_desktop::route::Route;
use devworks_desktop::{decision_ops as dops, ops, work_ops as wops};
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::{NodeT, Queryable};
use serde_json::json;

type Ui = egui_kittest::Harness<'static, devworks_desktop::shell::Shell>;
fn press(h: &mut Ui, label: &str) {
    click(h, Role::Button, label);
}
fn rationale(h: &mut Ui, text: &str) {
    h.get_by_role_and_label(Role::TextInput, "Decision rationale")
        .focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, "Decision rationale")
        .type_text(text);
    h.step();
    h.step();
}
fn feature_plan(b: &dyn Backend, root: &Root, title: &str) -> WorkRecord {
    let p =
        ops::project_changed(b.command("projects.create", &ops::project_create(b, title, "", &[])))
            .unwrap();
    let statuses = wops::workflow_page(b.query(
        "workflows.list",
        &wops::workflow_list(b, p.id.0.as_str(), None),
    ))
    .unwrap()
    .0;
    let status = statuses.iter().find(|s| s.category.0 == "Backlog").unwrap();
    let text = "# Exact frozen plan\nShip the inspected plan, preserving approval scope.";
    let path = root.file("review-plan.md", text.as_bytes());
    let ticket = b.mint_project_upload(&p.id, &path).unwrap();
    let artifact = ops::saved_artifact(b.command(
        "artifacts.save",
        &ops::save_project_artifact(
            b,
            p.id.0.as_str(),
            ticket.as_str(),
            "Exact proposal text",
            "text/markdown",
            &devworks_core::fingerprint::sha256_hex(text.as_bytes()),
            text.len() as u64,
        ),
    ))
    .unwrap();
    wops::work_changed(b.command("features.create",&wops::work_command(b,"features.create",json!({"projectId":p.id,"creation":{"kind":"Single","title":title,"description":"A concrete native approval fixture.","repositoryIds":[],"statusId":status.id,"priority":10,"plan":{"content":artifact.artifact,"goals":["Ship the inspected plan"],"requirements":[{"id":"E1","statement":"Preserve the exact frozen plan","status":"Confirmed","coveredByTaskIds":[]}],"acceptance":[],"acceptanceTests":[],"repositoryIds":[]}}})))).unwrap()
}
fn request(b: &dyn Backend, feature: &WorkRecord) -> ApprovalViewRecord {
    let pending = feature.pending_plan.as_ref().unwrap();
    let p=wops::approval_requested(b.command("features.request_plan_approval",&wops::work_command(b,"features.request_plan_approval",json!({"featureId":feature.id,"expectedRevision":feature.revision,"planRevision":pending.revision,"planDigest":devworks_core::services::work::plan_digest(&pending.draft)})))).unwrap();
    dops::approval(b.query(
        "approvals.inspect",
        &dops::query(b, "approvals.inspect", json!({"proposalId":p.id})),
    ))
    .unwrap()
}
fn get(b: &dyn Backend, w: &WorkRecord) -> WorkRecord {
    wops::work_record(b.query(
        "features.get",
        &wops::work_get(b, "features.get", w.id.as_str()),
    ))
    .unwrap()
}
fn open(engine: &devworks_desktop::host::Engine, root: &Root, p: &ApprovalViewRecord) -> Ui {
    let mut h = harness(shell(engine, root, Script::default()), 1800.0, 1400.0);
    settle(&mut h);
    h.state_mut().navigate(Route::Approvals);
    settle(&mut h);
    press(&mut h, &format!("Inspect proposal {}", p.id.as_str()));
    h
}
#[test]
fn native_approval_inspects_frozen_content_and_promotes_only_through_trusted_human_ingress() {
    let root = Root::new("approval-native-exact");
    let engine = start(&root);
    let b = engine.backend();
    let feature = feature_plan(&*b, &root, "Exact native plan");
    let p = request(&*b, &feature);
    let generic = dops::command(
        &*b,
        "approvals.record_human_decision",
        json!({"proposalId":p.id,"expectedRevision":p.revision,"contentDigest":p.content_digest,"decision":"Approve","rationale":"Generic command cannot mint a Human witness"}),
    );
    assert!(ops::command_result(b.command("approvals.record_human_decision", &generic)).is_err());
    assert!(get(&*b, &feature).approved_plan.is_none());
    let mut h = open(&engine, &root, &p);
    assert!(has_text(&h, "Exact proposed plan"));
    assert!(has_text(
        &h,
        &format!(
            "Target {} @ {}",
            feature.id.as_str(),
            p.target_revision.as_str()
        )
    ));
    press(&mut h, "Exact proposal artifact text");
    assert!(has_text(&h, "Ship the inspected plan"));
    rationale(
        &mut h,
        "I reviewed this exact pending plan and its target revision.",
    );
    press(&mut h, "Approve exact proposal");
    let current = get(&*b, &feature);
    assert!(current.pending_plan.is_none());
    assert!(current.approved_plan.is_some());
    assert!(has_text(&h, "Recorded rationale:"));
    assert!(
        current.attempts.0.is_empty(),
        "approval alone never starts inference or execution"
    );
}
#[test]
fn a_refreshed_inbox_keeps_the_inspected_draft_and_reports_target_staleness() {
    let root = Root::new("approval-native-stale");
    let engine = start(&root);
    let b = engine.backend();
    let feature = feature_plan(&*b, &root, "Stale native plan");
    let p = request(&*b, &feature);
    let mut h = open(&engine, &root, &p);
    rationale(&mut h, "A draft bound to the original target.");
    wops::work_changed(b.command("features.edit",&wops::work_command(&*b,"features.edit",json!({"workId":feature.id,"expectedRevision":feature.revision,"changes":{"title":"A changed target"}})))).unwrap();
    press(&mut h, "Refresh approvals");
    assert!(has_text(&h, "This proposal is stale"));
    assert_eq!(
        h.state()
            .decisions
            .selected
            .as_ref()
            .unwrap()
            .target_revision
            .as_str(),
        p.target_revision.as_str()
    );
    assert_eq!(
        h.state().decisions.rationale,
        "A draft bound to the original target."
    );
    assert!(
        h.get_by_role_and_label(Role::Button, "Approve exact proposal")
            .accesskit_node()
            .is_disabled()
    );
    assert!(get(&*b, &feature).approved_plan.is_none());
}
#[test]
fn unknown_native_decisions_retain_the_exact_trusted_dispatch_across_restart() {
    let root = Root::new("approval-native-restart");
    let engine = start(&root);
    let real = engine.backend();
    let feature = feature_plan(&*real, &root, "Durable native decision");
    let p = request(&*real, &feature);
    let lossy = LoseAcks::over(real.clone(), "approvals.record_human_decision", 100);
    let mut h = harness(
        shell_with(Ok(lossy.clone()), &root, Script::default(), None),
        1800.0,
        1400.0,
    );
    settle(&mut h);
    h.state_mut().navigate(Route::Approvals);
    settle(&mut h);
    press(&mut h, &format!("Inspect proposal {}", p.id.as_str()));
    rationale(&mut h, "Review once; confirm the same recorded decision.");
    press(&mut h, "Approve exact proposal");
    assert_eq!(
        h.state().client().unwrap().unresolved()[0].dispatch,
        CommandDispatch::HumanDecision
    );
    let frame = lossy.frames.lock().unwrap()[0].clone();
    h.state_mut().on_exit();
    drop(h);
    let honest = LoseAcks::over(real.clone(), "approvals.record_human_decision", 0);
    let mut h = harness(
        shell_with(Ok(honest.clone()), &root, Script::default(), None),
        1800.0,
        1400.0,
    );
    settle(&mut h);
    assert!(h.state().client().unwrap().unresolved().is_empty());
    assert!(honest.frames.lock().unwrap().iter().all(|f| f == &frame));
    assert!(get(&*real, &feature).approved_plan.is_some());
}
