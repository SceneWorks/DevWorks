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

struct ReviewQueries {
    inner: std::sync::Arc<dyn Backend>,
    frames: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
}
impl Backend for ReviewQueries {
    fn query(&self, op: &str, frame: &[u8]) -> devworks_core::gateway::QueryOutcome {
        if [
            "approvals.review_queue",
            "approvals.list",
            "notifications.list",
        ]
        .contains(&op)
        {
            self.frames
                .lock()
                .unwrap()
                .push((op.into(), serde_json::from_slice(frame).unwrap()));
        }
        self.inner.query(op, frame)
    }
    fn command(&self, op: &str, frame: &[u8]) -> devworks_core::gateway::CommandOutcome {
        self.inner.command(op, frame)
    }
    fn mint_workspace_upload(
        &self,
        path: &std::path::Path,
    ) -> Result<devworks_core::protocol::Id, String> {
        self.inner.mint_workspace_upload(path)
    }
    fn mint_project_upload(
        &self,
        project: &devworks_core::protocol::ProjectId,
        path: &std::path::Path,
    ) -> Result<devworks_core::protocol::Id, String> {
        self.inner.mint_project_upload(project, path)
    }
    fn keep_awake_health(&self) -> devworks_core::settings::view::Health {
        self.inner.keep_awake_health()
    }
    fn hints(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.inner.hints()
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
    fn read_stream(&self, id: &devworks_core::protocol::Id, max: u64) -> Result<Vec<u8>, String> {
        self.inner.read_stream(id, max)
    }
}
fn challenged_facts(
    b: &dyn Backend,
    scope: serde_json::Value,
    count: usize,
) -> std::collections::BTreeSet<String> {
    use devworks_desktop::knowledge_screen as knowledge;
    let claims=(0..count).map(|i|json!({"kind":"Fact","subjectId":"topic","predicate":format!("review-{i}"),"value":format!("A scoped fact requiring review {i}"),"confidence":0.8,"evidenceIds":[],"tags":[]})).collect::<Vec<_>>();
    let ids=match ops::command_result(b.command("knowledge.record",&knowledge::command(b,"knowledge.record",json!({"scope":scope,"batch":{"entities":[{"clientId":"topic","label":"Scoped review fixture","entityType":"topic","aliases":[]}],"evidence":[],"claims":claims}})))).unwrap() {
        devworks_core::protocol::results::OperationResult::KnowledgeRecord{value}=>value.claim_ids.0,
        other=>panic!("{other:?}")
    };
    for id in &ids {
        ops::command_result(b.command("knowledge.challenge",&knowledge::command(b,"knowledge.challenge",json!({"scope":scope,"claimId":id,"expectedRevision":"1","reason":"Review this project's current evidence","evidenceIds":[]})))).unwrap();
    }
    ids.into_iter().map(|id| id.as_str().to_owned()).collect()
}
fn choose_review_project(h: &mut Ui, name: &str) {
    click(h, Role::ComboBox, "Approval project filter");
    h.query_all_by_label(name)
        .find(|n| {
            matches!(
                n.accesskit_node().role(),
                Role::Button | Role::ListBoxOption
            ) && n.rect().left() > devworks_desktop::theme::NAV
        })
        .unwrap_or_else(|| {
            panic!(
                "Project option {name}; matching roles: {:?}",
                h.query_all_by_label(name)
                    .map(|node| node.accesskit_node().role())
                    .collect::<Vec<_>>()
            )
        })
        .click();
    settle(h);
}
#[test]
fn home_requires_a_project_for_knowledge_review_and_filters_before_paging() {
    let root = Root::new("approval-native-project-review");
    let engine = start(&root);
    let b = engine.backend();
    let project = |name: &str| {
        ops::project_changed(b.command("projects.create", &ops::project_create(&*b, name, "", &[])))
            .unwrap()
    };
    let alpha = project("Review alpha");
    let beta = project("Review beta");
    let workspace = challenged_facts(&*b, json!({"kind":"Workspace"}), 1);
    let beta_ids = challenged_facts(&*b, json!({"kind":"Project","projectId":beta.id}), 4);
    // More than one page of newer rows in another project must not crowd beta off its page.
    let alpha_ids = challenged_facts(&*b, json!({"kind":"Project","projectId":alpha.id}), 51);
    let traced = std::sync::Arc::new(ReviewQueries {
        inner: b,
        frames: Default::default(),
    });
    let mut h = harness(
        shell_with(Ok(traced.clone()), &root, Script::default(), None),
        1800.0,
        1400.0,
    );
    settle(&mut h);
    assert_eq!(h.state().route(), Route::Chat);
    h.state_mut().navigate(Route::Approvals);
    settle(&mut h);
    press(&mut h, "Knowledge review");
    assert!(has_text(&h, "Select a project to review Knowledge"));
    assert!(h.state().decisions.reviews.is_empty());
    assert!(!h.state().decisions.reviews_loaded);
    {
        let frames = traced.frames.lock().unwrap();
        assert!(
            !frames.iter().any(|(op, _)| op == "approvals.review_queue"),
            "Home never asks for a Workspace review queue"
        );
        for op in ["approvals.list", "notifications.list"] {
            assert!(
                frames.iter().any(|(name, frame)| name == op
                    && frame.pointer("/body/scope/kind") == Some(&json!("Workspace"))),
                "{op} remains global"
            );
        }
    }
    choose_review_project(&mut h, "Review beta");
    assert!(
        h.state().decisions.error.is_none(),
        "{:?}",
        h.state().decisions.error
    );
    let shown = |h: &Ui| {
        h.state()
            .decisions
            .reviews
            .iter()
            .map(|row| row.entry.id.as_str().to_owned())
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(shown(&h), beta_ids);
    assert!(shown(&h).is_disjoint(&workspace));
    choose_review_project(&mut h, "Review alpha");
    assert_eq!(shown(&h), alpha_ids);
    assert!(h.state().decisions.reviews_loaded);
    {
        let frames = traced.frames.lock().unwrap();
        let review = frames
            .iter()
            .filter(|(op, _)| op == "approvals.review_queue")
            .collect::<Vec<_>>();
        assert!(
            review
                .iter()
                .all(|(_, frame)| frame.pointer("/body/projectId").is_some()
                    && frame.pointer("/body/page/limit") == Some(&json!(50)))
        );
        assert!(
            review
                .iter()
                .any(
                    |(_, frame)| frame.pointer("/body/projectId") == Some(&json!(alpha.id))
                        && frame.pointer("/body/page/cursor").is_some()
                ),
            "the real project queue is read beyond its first page"
        );
    }
    choose_review_project(&mut h, "All projects");
    assert!(has_text(&h, "Select a project to review Knowledge"));
    assert!(h.state().decisions.reviews.is_empty());
    assert!(
        traced
            .frames
            .lock()
            .unwrap()
            .iter()
            .filter(|(op, _)| op == "approvals.review_queue")
            .all(|(_, frame)| frame.pointer("/body/projectId").is_some()),
        "clearing the project never issues a global fallback"
    );
}

fn signed_package_proposal(
    engine: &devworks_desktop::host::Engine,
    root: &Root,
) -> ApprovalViewRecord {
    use devworks_core::fingerprint::sha256_hex;
    use devworks_core::protocol::results::OperationResult;
    let b = engine.backend();
    let helper = b"inert signed fixture; deliberately not a host target";
    let help =
        b"# Native signed package review\nExact source bytes stay attached to the owner review.";
    let manifest=json!({"packageId":"native-signed-review","version":"1.0.0","publisherId":"native-test-publisher","protocolMin":{"major":1,"minor":0},"protocolMax":{"major":1,"minor":0},"minimumHostVersion":"0.1.0","dependencies":[],"runtimes":[],"schemaAssets":[],"requestedLimits":{"maxConcurrent":1,"queueCapacity":4,"maxMemoryBytes":"1048576","streamCreditBytes":4096},"targets":[{"targetTriple":"riscv64gc-unknown-none-elf","entrypoint":"bin/helper"}],"files":[{"path":"bin/helper","sha256":sha256_hex(helper),"byteLength":helper.len().to_string()},{"path":"help.md","sha256":sha256_hex(help),"byteLength":help.len().to_string()}],"stateSchema":{"schemaId":"native-signed-review/state","revision":"1","sha256":"a".repeat(64)},"configurationSchema":{"schemaId":"native-signed-review/config","revision":"1","sha256":"b".repeat(64)},"helpFile":"help.md","requestedPermissions":[],"contributions":[{"kind":"Tool","meta":{"id":"native-signed-review/inspect","inputSchema":{"schemaId":"native-signed-review/in","revision":"1","sha256":"c".repeat(64)},"outputSchema":{"schemaId":"native-signed-review/out","revision":"1","sha256":"d".repeat(64)},"helpTopic":"help","required":false,"cancellation":"Cooperative"},"requestedEffect":"ReadCandidate"}]}).to_string();
    let key = ed25519_dalek::SigningKey::from_bytes(&[17; 32]);
    let bundle = devworks_adapters::packages::write_bundle(
        manifest.as_bytes(),
        &[
            ("bin/helper", helper.as_slice()),
            ("help.md", help.as_slice()),
        ],
        Some(&key),
    );
    use devworks_core::ports::packages::PackageInspector;
    let inspection = devworks_adapters::packages::BundleInspector
        .inspect(&bundle)
        .unwrap();
    assert!(
        inspection.inspection.signer_key.is_some(),
        "the actual inspector verified the signature"
    );
    let path = root.file("native-signed-review.dwpkg", &bundle);
    let ticket = b.mint_workspace_upload(&path).unwrap();
    let source = ops::saved_artifact(b.command(
        "artifacts.save",
        &ops::save_workspace_artifact(
            &*b,
            ticket.as_str(),
            "Native signed source bundle",
            "application/vnd.devworks.package",
            &sha256_hex(&bundle),
            bundle.len() as u64,
            &[],
        ),
    ))
    .unwrap();
    let result = ops::command_result(b.command(
        "addons.propose",
        &ops::command_frame(
            &*b,
            "addons.propose",
            json!({"source":source.artifact,"manifestDigest":sha256_hex(manifest.as_bytes())}),
        ),
    ))
    .unwrap();
    assert!(matches!(result, OperationResult::AddonsPropose { .. }));
    let (rows, _) = dops::approvals(b.query(
        "approvals.list",
        &dops::query(
            &*b,
            "approvals.list",
            json!({"scope":{"kind":"Workspace"},"page":{"limit":100}}),
        ),
    ))
    .unwrap();
    let proposal = rows
        .iter()
        .find(|p| p.operation_kind.0 == "addons.install")
        .expect("owner-generated package review");
    dops::approval(b.query(
        "approvals.inspect",
        &dops::query(&*b, "approvals.inspect", json!({"proposalId":proposal.id})),
    ))
    .unwrap()
}
#[test]
fn native_small_signed_package_approves_owner_review_without_rendering_binary_source_as_text() {
    use devworks_core::protocol::records::{ApprovalState, ArtifactReadContent};
    let root = Root::new("approval-native-binary-package");
    let engine = start(&root);
    let b = engine.backend();
    let p = signed_package_proposal(&engine, &root);
    let artifact = p.artifact.as_ref().unwrap();
    assert_eq!(artifact.media_type, "application/vnd.devworks.package");
    assert!(
        matches!(
            ops::read_content(b.query("artifacts.read", &ops::artifact_read(&*b, artifact)))
                .unwrap(),
            ArtifactReadContent::Binary { .. }
        ),
        "this is the actual binary read path"
    );
    assert!(p.reviewed_content.as_ref().unwrap().0["inspection"].is_object());
    let mut h = open(&engine, &root, &p);
    assert!(
        h.state().decisions.text.is_none(),
        "a verified source attachment never creates a mandatory text reader"
    );
    assert!(has_text(&h, "Package source attachment"));
    assert!(
        h.query_by_role_and_label(Role::Button, "Open package source artifact")
            .is_some()
    );
    rationale(
        &mut h,
        "I reviewed the exact signed package, permissions and incompatible target; no helper should launch here.",
    );
    assert!(
        !h.get_by_role_and_label(Role::Button, "Approve exact proposal")
            .accesskit_node()
            .is_disabled()
    );
    press(&mut h, "Approve exact proposal");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    loop {
        let current = dops::approval(b.query(
            "approvals.inspect",
            &dops::query(&*b, "approvals.inspect", json!({"proposalId":p.id})),
        ))
        .unwrap();
        assert!(matches!(current.state, ApprovalState::Approved));
        if current
            .resume_status
            .as_ref()
            .is_some_and(|s| s.0 == "Admitted")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "exact saved installation readmits: {:?}",
            current.resume_status
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        engine
            .host()
            .runtime()
            .packages()
            .unwrap()
            .instance_status("native-signed-review")
            .is_empty(),
        "the deliberately unsupported target never launches a helper"
    );
}

#[path = "decisions/late_reply.rs"]
mod late_reply;
