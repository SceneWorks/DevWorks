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
    feature_plan_source(
        b,
        root,
        title,
        json!([{"id":"E1","statement":"Preserve the exact frozen plan","status":"Confirmed","coveredByTaskIds":[]}]),
    )
}
fn feature_plan_source(
    b: &dyn Backend,
    root: &Root,
    title: &str,
    requirements: serde_json::Value,
) -> WorkRecord {
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
    wops::work_changed(b.command("features.create",&wops::work_command(b,"features.create",json!({"projectId":p.id,"creation":{"kind":"Single","title":title,"description":"A concrete native approval fixture.","repositoryIds":[],"statusId":status.id,"priority":10,"plan":{"content":artifact.artifact,"goals":["Ship the inspected plan"],"requirements":requirements,"acceptance":[],"acceptanceTests":[],"repositoryIds":[]}}})))).unwrap()
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
    assert!(get(&*b, &feature).approval.is_none());
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
    click(
        &mut h,
        Role::DisclosureTriangle,
        "Exact proposal artifact text",
    );
    assert!(has_text(&h, "Ship the inspected plan"));
    rationale(
        &mut h,
        "I reviewed this exact pending plan and its target revision.",
    );
    press(&mut h, "Approve exact proposal");
    let current = get(&*b, &feature);
    assert!(current.pending_plan.is_none());
    assert!(current.approval.is_some());
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
    let current = get(&*b, &feature);
    let mut revised = serde_json::to_value(&current.pending_plan.as_ref().unwrap().draft).unwrap();
    revised["goals"] = json!(["A changed frozen plan target"]);
    wops::work_changed(b.command(
        "features.edit_plan",
        &wops::work_command(
            &*b,
            "features.edit_plan",
            json!({"featureId":feature.id,"expectedRevision":current.revision,"plan":revised}),
        ),
    ))
    .unwrap();
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
    assert!(get(&*b, &feature).approval.is_none());
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
    assert!(get(&*real, &feature).approval.is_some());
}

fn seed_notification(
    engine: &devworks_desktop::host::Engine,
    id: &str,
    title: &str,
    work: &WorkRecord,
    at: i64,
) {
    use devworks_core::ports::storage::{StoredNotification, WriteOptions, run_write};
    use devworks_core::protocol::{ActorId, Id, RootId};
    let n = StoredNotification {
        id: Id::new(id).unwrap(),
        revision: 1,
        user_id: ActorId(Id::new(devworks_desktop::host::USER_ACTOR).unwrap()),
        project_id: Some(work.project_id.clone()),
        root_id: Some(RootId(work.id.clone())),
        decision_id: None,
        category: "Completion".into(),
        priority: 1,
        title: title.into(),
        body: "Verified completion fixture published to the durable notification port.".into(),
        source_id: format!("verified-{id}"),
        links: json!([{"kind":"work","targetId":work.id,"projectId":work.project_id}]),
        requires_input: false,
        read: false,
        acknowledged: false,
        created_at_ms: at,
        updated_at_ms: at,
    };
    run_write(
        &**engine.host().runtime().store(),
        WriteOptions::default(),
        move |tx| tx.decisions().put_notification(&n, None),
    )
    .unwrap();
}
fn notification(b: &dyn Backend, id: &str) -> devworks_core::protocol::records::NotificationRecord {
    dops::notification(b.query(
        "notifications.read",
        &dops::query(b, "notifications.read", json!({"notificationId":id})),
    ))
    .unwrap()
}
#[test]
fn durable_completion_cards_open_actual_work_and_separate_read_from_individual_acknowledgment() {
    let root = Root::new("notification-native-durable");
    let engine = start(&root);
    let b = engine.backend();
    let feature = feature_plan(&*b, &root, "Completion link target");
    seed_notification(
        &engine,
        "notice-a",
        "Earlier verified completion",
        &feature,
        100,
    );
    seed_notification(
        &engine,
        "notice-b",
        "Newest verified completion",
        &feature,
        200,
    );
    let mut h = harness(shell(&engine, &root, Script::default()), 1800.0, 1400.0);
    settle(&mut h);
    assert_eq!(
        h.state()
            .completions
            .cards()
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        ["notice-b", "notice-a"]
    );
    click(&mut h, Role::Link, "View completed work");
    assert_eq!(h.state().route(), Route::Features);
    assert_eq!(
        h.state().route_memory(Route::Features).selection.as_deref(),
        Some(feature.id.as_str()),
        "notification identity is never mistaken for work identity"
    );
    assert!(!notification(&*b, "notice-b").acknowledged);
    press(&mut h, "Notifications");
    press(&mut h, "Read Newest verified completion");
    assert!(!notification(&*b, "notice-b").acknowledged);
    assert_eq!(
        notification(&*b, "notice-b").read,
        Some(false),
        "notifications.read is a pure query"
    );
    // The record detail is separate from state-changing actions. Read state never dismisses.
    h.query_all_by_role_and_label(Role::Button, "Mark read")
        .next()
        .unwrap()
        .click_accesskit();
    settle(&mut h);
    assert!(!notification(&*b, "notice-b").acknowledged);
    assert_eq!(h.state().completions.cards().len(), 2);
    press(&mut h, "Dismiss Newest verified completion");
    assert!(notification(&*b, "notice-b").acknowledged);
    assert!(!notification(&*b, "notice-a").acknowledged);
    assert_eq!(h.state().completions.cards().len(), 1);
    h.state_mut().navigate(Route::Settings);
    settle(&mut h);
    assert!(has_text(&h, "Earlier verified completion"));
    h.state_mut().on_exit();
    drop(h);
    drop(b);
    drop(engine);
    let again = start(&root);
    let mut h = harness(shell(&again, &root, Script::default()), 1800.0, 1400.0);
    settle(&mut h);
    assert_eq!(
        h.state()
            .completions
            .cards()
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        ["notice-a"],
        "only the explicit individually acknowledged card stays dismissed after runtime restart"
    );
}
#[test]
fn unknown_dismissal_retains_its_original_request_until_restart_confirms_the_receipt() {
    let root = Root::new("notification-native-unknown");
    let engine = start(&root);
    let real = engine.backend();
    let feature = feature_plan(&*real, &root, "Unknown dismissal target");
    seed_notification(&engine, "notice-loss", "Durable dismissal", &feature, 300);
    let lossy = LoseAcks::over(real.clone(), "notifications.acknowledge", 100);
    let mut h = harness(
        shell_with(Ok(lossy.clone()), &root, Script::default(), None),
        1800.0,
        1400.0,
    );
    settle(&mut h);
    press(&mut h, "Dismiss Durable dismissal");
    assert!(notification(&*real, "notice-loss").acknowledged);
    assert!(
        h.state()
            .client()
            .unwrap()
            .unresolved()
            .iter()
            .any(|u| u.operation() == "notifications.acknowledge")
    );
    let frame = lossy.frames.lock().unwrap()[0].clone();
    h.state_mut().on_exit();
    drop(h);
    let honest = LoseAcks::over(real.clone(), "notifications.acknowledge", 0);
    let mut h = harness(
        shell_with(Ok(honest.clone()), &root, Script::default(), None),
        1800.0,
        1400.0,
    );
    settle(&mut h);
    assert!(h.state().client().unwrap().unresolved().is_empty());
    assert!(h.state().completions.cards().is_empty());
    assert!(honest.frames.lock().unwrap().iter().all(|f| f == &frame));
}

#[test]
fn rejected_native_plan_never_promotes_or_starts_work() {
    let root = Root::new("approval-native-reject");
    let engine = start(&root);
    let b = engine.backend();
    let feature = feature_plan(&*b, &root, "Rejected plan");
    let p = request(&*b, &feature);
    let mut h = open(&engine, &root, &p);
    rationale(&mut h, "The acceptance scope needs another revision.");
    press(&mut h, "Reject proposal");
    assert!(matches!(
        h.state().decisions.selected.as_ref().unwrap().state,
        devworks_core::protocol::records::ApprovalState::Rejected
    ));
    let current = get(&*b, &feature);
    assert!(current.approval.is_none());
    assert!(current.attempts.0.is_empty());
}
#[test]
fn native_inspection_keeps_full_typed_plan_source_beyond_the_json_metadata_limit() {
    let root = Root::new("approval-native-large");
    let engine = start(&root);
    let b = engine.backend();
    let statement = "Exact frozen requirement content. ".repeat(180);
    let requirements=(1..=128).map(|i|json!({"id":format!("E{i}"),"statement":statement,"status":"Confirmed","coveredByTaskIds":[]})).collect::<Vec<_>>();
    let feature = feature_plan_source(&*b, &root, "Large complete plan", json!(requirements));
    let p = request(&*b, &feature);
    assert!(
        serde_json::to_vec(p.reviewed_source.as_ref().unwrap())
            .unwrap()
            .len()
            > 524_288
    );
    let h = open(&engine, &root, &p);
    let source = h
        .state()
        .decisions
        .selected
        .as_ref()
        .unwrap()
        .reviewed_source
        .as_ref()
        .unwrap();
    match source {
        devworks_core::protocol::records::ApprovalReviewedContent::FeaturePlan { plan } => {
            assert_eq!(plan.requirements.0.len(), 128);
            assert_eq!(plan.requirements.0.last().unwrap().statement.0, statement);
        }
        other => panic!("{other:?}"),
    };
    assert!(has_text(&h, "Exact proposed plan"));
}

#[test]
fn finding_specific_native_risk_acceptance_keeps_prerequisites_blocking_and_ack_is_independent() {
    let root = Root::new("approval-native-risk");
    let engine = start(&root);
    let b = engine.backend();
    let feature = feature_plan(&*b, &root, "Finding risk scope");
    let p = request(&*b, &feature);
    dops::decided(b.human_decision(&dops::command(&*b,"approvals.record_human_decision",json!({"proposalId":p.id,"expectedRevision":p.revision,"contentDigest":p.content_digest,"decision":"Approve","rationale":"Approve exact plan for readiness assessment."})))).unwrap();
    let feature = get(&*b, &feature);
    let approval = feature.approval.as_ref().unwrap();
    let finding = |id: &str, prerequisite: bool| json!({"id":id,"disposition":"NeedsUserDecision","blocking":true,"prerequisite":prerequisite,"state":"Open","requirementIds":["E1"],"scopeKey":"E1","affectedWorkIds":[],"summary":if prerequisite{"Required dependency unavailable"}else{"Product choice remains uncertain"},"missing":"A scoped decision","consequence":"Verification remains blocked","attempts":["Read repository conventions"],"insufficiency":"Requires an explicit product decision","closureCondition":"Resolve this exact finding","evidence":[],"question":"Which supported behavior is intended?","recommendation":"Keep the current supported behavior","closureEvidence":[]});
    let assessed=wops::work_changed(b.command("features.edit",&wops::work_command(&*b,"features.edit",json!({"workId":feature.id,"expectedRevision":feature.revision,"changes":{"readiness":{"kind":"Assess","planRevision":approval.plan_revision,"planDigest":approval.plan_digest,"summary":"Two independently scoped readiness findings","findings":[finding("F1",false),finding("F2",true)]}}})))).unwrap();
    let assessment = assessed.readiness.as_ref().unwrap();
    let proposal_body = |finding: &str| {
        let payload = json!({"kind":"FindingRisk","featureId":feature.id,"findingId":finding,"assessmentRevision":assessment.revision});
        let digest = devworks_core::fingerprint::sha256_hex(
            devworks_core::jcs::canonicalize(&payload)
                .unwrap()
                .as_bytes(),
        );
        json!({"scope":{"kind":"Project","projectId":feature.project_id},"operationKind":"features.accept_finding_risk","target":{"id":finding,"revision":assessment.revision},"contentDigest":digest,"summary":"Accept this specific product risk with rationale","payload":payload})
    };
    let proposed = dops::decided(b.command(
        "approvals.propose",
        &dops::command(&*b, "approvals.propose", proposal_body("F1")),
    ))
    .unwrap();
    let notifications = dops::notifications(b.query(
        "notifications.list",
        &dops::query(
            &*b,
            "notifications.list",
            json!({"scope":{"kind":"Workspace"},"page":{"limit":100},"unacknowledgedOnly":true}),
        ),
    ))
    .unwrap()
    .0;
    let n = notifications
        .iter()
        .find(|n| {
            n.decision_id
                .as_ref()
                .is_some_and(|id| id.as_str() == proposed.id.as_str())
        })
        .unwrap();
    dops::notified(b.command(
        "notifications.acknowledge",
        &dops::command(
            &*b,
            "notifications.acknowledge",
            json!({"notificationId":n.id,"expectedRevision":n.revision}),
        ),
    ))
    .unwrap();
    assert!(
        matches!(
            get(&*b, &feature).readiness.as_ref().unwrap().findings.0[0].state,
            devworks_core::protocol::records::FindingState::Open
        ),
        "acknowledgment never accepts finding risk"
    );
    assert!(
        ops::command_result(b.command(
            "approvals.propose",
            &dops::command(&*b, "approvals.propose", proposal_body("F2"))
        ))
        .is_err(),
        "a prerequisite is never presented as waivable risk"
    );
    let mut h = open(&engine, &root, &proposed);
    assert!(has_text(&h, "Risk acceptance does not waive prerequisites"));
    rationale(
        &mut h,
        "Accept only F1's product uncertainty within this reviewed scope.",
    );
    press(&mut h, "Accept this finding's risk");
    let current = get(&*b, &feature);
    let readiness = current.readiness.unwrap();
    assert!(!readiness.ready);
    assert!(matches!(
        readiness
            .findings
            .0
            .iter()
            .find(|f| f.id.as_str() == "F1")
            .unwrap()
            .state,
        devworks_core::protocol::records::FindingState::RiskAccepted
    ));
    assert!(matches!(
        readiness
            .findings
            .0
            .iter()
            .find(|f| f.id.as_str() == "F2")
            .unwrap()
            .state,
        devworks_core::protocol::records::FindingState::Open
    ));
    assert!(current.attempts.0.is_empty());
}

#[test]
fn native_notification_preferences_use_their_own_revision_and_keep_in_app_access() {
    let root = Root::new("notification-native-settings");
    let engine = start(&root);
    let b = engine.backend();
    let mut h = harness(shell(&engine, &root, Script::default()), 1800.0, 1400.0);
    settle(&mut h);
    h.state_mut().navigate(Route::Settings);
    settle(&mut h);
    press(&mut h, "Notification preferences");
    click(&mut h, Role::DisclosureTriangle, "Notification preferences");
    click(&mut h, Role::CheckBox, "Desktop notifications");
    h.get_by_role_and_label(Role::TextInput, "Notification kinds")
        .focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, "Notification kinds")
        .type_text("Completion, ");
    h.step();
    h.get_by_role_and_label(Role::TextInput, "Notification kinds")
        .type_text("Approval");
    h.step();
    h.step();
    press(&mut h, "Save notification preferences");
    match ops::query_result(b.query(
        "notifications.settings",
        &ops::query_frame(&*b, "notifications.settings", json!({})),
    ))
    .unwrap()
    .0
    {
        devworks_core::protocol::results::OperationResult::NotificationsSettings { value } => {
            assert_eq!(value.revision.as_str(), "2");
            assert!(!value.config.desktop_enabled);
            assert_eq!(
                value
                    .config
                    .kinds
                    .0
                    .iter()
                    .map(|k| k.0.as_str())
                    .collect::<Vec<_>>(),
                ["Completion", "Approval"]
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(has_text(&h, "Preferences revision 2"));
    assert!(has_text(&h, "In-app history remains available"));
    assert!(has_text(&h, "No notifications in this view."));
}
