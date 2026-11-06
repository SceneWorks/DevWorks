//! Native usage, price and feature-budget controls over the real runtime. Provider transports
//! remain scripted/no-network; these tests never start inference.
mod common;
use common::*;
use devworks_core::protocol::records::*;
use devworks_desktop::route::Route;
use devworks_desktop::{backend::Backend, ops, usage_ops as uops, work_ops as wops};
use eframe::egui::{self, accesskit::Role};
use egui_kittest::kittest::{NodeT, Queryable};
use serde_json::json;
type Ui = egui_kittest::Harness<'static, devworks_desktop::shell::Shell>;
fn press(h: &mut Ui, label: &str) {
    click(h, Role::Button, label);
}
fn replace(h: &mut Ui, label: &str, value: &str) {
    h.get_by_role_and_label(Role::TextInput, label)
        .scroll_to_me();
    settle(h);
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.step();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.get_by_role_and_label(Role::TextInput, label)
        .type_text(value);
    h.step();
    h.step();
}
fn project(b: &dyn Backend, name: &str) -> String {
    ops::project_changed(b.command("projects.create", &ops::project_create(b, name, "", &[])))
        .unwrap()
        .id
        .0
        .as_str()
        .into()
}
fn features(b: &dyn Backend, p: &str) -> Vec<WorkRecord> {
    wops::work_page(b.query(
        "features.list",
        &wops::work_list(b, "features.list", p, None),
    ))
    .unwrap()
    .0
}
fn feature(b: &dyn Backend, id: &str) -> WorkRecord {
    wops::work_record(b.query("features.get", &wops::work_get(b, "features.get", id))).unwrap()
}
fn select_feature(h: &mut Ui, prefix: &str) {
    h.query_all_by_label_contains(prefix)
        .find(|n| {
            n.accesskit_node()
                .label()
                .is_some_and(|label| label.starts_with(prefix))
        })
        .unwrap()
        .click();
    settle(h);
}
fn open_features(engine: &devworks_desktop::host::Engine, root: &Root, p: &str) -> Ui {
    let mut h = harness(shell(engine, root, Script::default()), 1800.0, 1500.0);
    settle(&mut h);
    h.state_mut().select_project(Some(p.into()));
    h.state_mut().navigate(Route::Features);
    settle(&mut h);
    h
}

#[test]
fn native_feature_zero_limit_latches_and_turning_it_off_does_not_continue_work() {
    let root = Root::new("usage-native-feature-cap");
    let engine = start(&root);
    let b = engine.backend();
    let p = project(&*b, "Native cap project");
    let mut h = open_features(&engine, &root, &p);
    press(&mut h, "New feature");
    replace(&mut h, "Title", "Zero is a real cap");
    click(&mut h, Role::CheckBox, "Limit feature spending");
    replace(&mut h, "Feature spending limit USD", "0");
    press(&mut h, "Create");
    let f = features(&*b, &p).remove(0);
    let budget = f.budget.as_ref().unwrap();
    assert_eq!(budget.limit_micro_usd.as_ref().unwrap().as_str(), "0");
    assert!(budget.continue_required);
    assert!(budget.warning);
    select_feature(&mut h, "Zero is a real cap  ·");
    assert!(has_text(&h, "75% spending warning reached"));
    assert!(has_text(&h, "Budget hold · explicit Continue required"));
    press(&mut h, "Edit");
    click(&mut h, Role::CheckBox, "Limit feature spending");
    press(&mut h, "Save");
    let current = feature(&*b, f.id.as_str());
    let budget = current.budget.unwrap();
    assert!(budget.limit_micro_usd.is_none());
    assert!(
        budget.continue_required,
        "Off never clears the separately latched Continue hold"
    );
    assert!(current.attempts.0.is_empty());
    assert!(has_text(&h, "explicit Continue required"));
    h.state_mut().navigate(Route::Issues);
    settle(&mut h);
    press(&mut h, "New issue");
    assert!(
        h.query_by_role_and_label(Role::CheckBox, "Limit feature spending")
            .is_none()
    );
    h.state_mut().navigate(Route::Jobs);
    settle(&mut h);
    press(&mut h, "New job");
    assert!(
        h.query_by_role_and_label(Role::CheckBox, "Limit feature spending")
            .is_none(),
        "jobs have no spending cap controls"
    );
}
#[test]
fn pending_plan_cap_reopens_and_is_approved_from_the_captured_backend_token() {
    let root = Root::new("usage-native-pending-cap");
    let engine = start(&root);
    let b = engine.backend();
    let p = project(&*b, "Proposed cap project");
    let mut h = open_features(&engine, &root, &p);
    press(&mut h, "New feature");
    replace(&mut h, "Title", "Captured cap proposal");
    press(&mut h, "Create");
    let f = features(&*b, &p).remove(0);
    select_feature(&mut h, "Captured cap proposal  ·");
    press(&mut h, "Edit plan");
    replace(&mut h, "Plan text", "Implement the bounded feature scope.");
    click(
        &mut h,
        Role::CheckBox,
        "Propose a spending cap change with this plan",
    );
    click(&mut h, Role::CheckBox, "Limit feature spending");
    replace(&mut h, "Feature spending limit USD", "12.50");
    press(&mut h, "Save plan draft");
    let current = feature(&*b, f.id.as_str());
    assert!(current.budget.as_ref().unwrap().limit_micro_usd.is_none());
    let pending = current.pending_plan.as_ref().unwrap();
    assert!(pending.plan_digest.is_some());
    assert!(
        matches!(&pending.budget,Some(FeatureBudgetInput::Limit{micro_usd})if micro_usd.as_str()=="12500000")
    );
    assert!(has_text(&h, "Proposed feature spending cap: $12.500000"));
    press(&mut h, "Edit plan");
    assert_eq!(
        h.state()
            .work
            .plan
            .as_ref()
            .unwrap()
            .budget
            .as_ref()
            .unwrap()
            .dollars,
        "12.500000"
    );
    press(&mut h, "Cancel");
    press(&mut h, "Request approval");
    let pending = feature(&*b, f.id.as_str()).pending_plan.unwrap();
    let proposal = pending
        .approval
        .as_ref()
        .unwrap()
        .proposal_id
        .as_str()
        .to_owned();
    h.state_mut().navigate(Route::Approvals);
    settle(&mut h);
    press(&mut h, &format!("Inspect proposal {proposal}"));
    assert!(has_text(&h, "Proposed feature spending cap: $12.500000"));
    replace(
        &mut h,
        "Decision rationale",
        "Approve this exact pending plan and its captured feature cap.",
    );
    press(&mut h, "Approve exact proposal");
    let applied = feature(&*b, f.id.as_str());
    assert!(applied.pending_plan.is_none());
    assert_eq!(
        applied.budget.unwrap().limit_micro_usd.unwrap().as_str(),
        "12500000"
    );
    assert!(applied.attempts.0.is_empty());
}
#[test]
fn local_free_connection_policy_is_explicit_and_unrelated_edits_preserve_it() {
    let root = Root::new("usage-native-local-policy");
    let mut config = config(&root.0);
    config.connection_tick = None;
    let engine = devworks_desktop::host::Engine::start(config).unwrap();
    let b = engine.backend();
    ops::configured(b.command("router.configure",&ops::router_configure(&*b,json!({"kind":"Connection","name":"Explicit local policy","profile":"CompatibleChat","endpoint":"http://127.0.0.1:11434/v1"})))).unwrap();
    let records = || {
        ops::connection_records(b.query("router.connections", &ops::router_connections(&*b, None)))
            .unwrap()
            .0
    };
    assert_eq!(
        records()[0].local_free,
        Some(false),
        "a localhost-looking endpoint never implies free billing"
    );
    let mut h = harness(shell(&engine, &root, Script::default()), 1800.0, 1500.0);
    settle(&mut h);
    h.state_mut().navigate(Route::Settings);
    settle(&mut h);
    press(&mut h, "Provider connections");
    press(&mut h, "Edit");
    // The new-connection form has the same policy choice; this targets the opened record editor.
    h.query_all_by_role_and_label(Role::CheckBox, "Local or free endpoint")
        .next()
        .unwrap()
        .click();
    settle(&mut h);
    press(&mut h, "Save connection");
    assert_eq!(records()[0].local_free, Some(true));
    press(&mut h, "Edit");
    replace(&mut h, "Explicit local policy name", "Renamed local policy");
    press(&mut h, "Save connection");
    assert_eq!(
        records()[0].local_free,
        Some(true),
        "unchanged checkbox omits localFree and preserves the recorded value"
    );
    assert_eq!(records()[0].name.0, "Renamed local policy");
    let row = records().remove(0);
    ops::configured(b.command("router.configure",&ops::router_configure(&*b,json!({"kind":"Connection","connectionId":row.id,"expectedRevision":row.revision,"name":row.name,"profile":"CompatibleChat","endpoint":row.endpoint})))).unwrap();
    assert_eq!(records()[0].local_free, Some(true));
}

fn price_catalog(b: &dyn Backend, model: &str) -> PriceCatalogView {
    uops::prices(b.query(
        "router.price_status",
        &uops::query(
            b,
            "router.price_status",
            json!({"modelIds":[model],"includeHistory":true,"page":{"limit":100}}),
        ),
    ))
    .unwrap()
}
#[test]
fn native_tariff_editor_preserves_all_dimensions_and_recovers_from_authoritative_cas_conflict() {
    let root = Root::new("usage-native-tariff-cas");
    let engine = start(&root);
    let b = engine.backend();
    let model = "native-price-model";
    ops::configured(b.command("router.configure",&ops::router_configure(&*b,json!({"kind":"Model","model":{"id":model,"displayName":"Native price model","endpointProfile":"CompatibleChat","upstreamModelId":"fixture/native-price","enabled":true,"tierMemberships":[],"efforts":[],"imageInput":false,"tools":false,"revision":"1"}})))).unwrap();
    let initial = price_catalog(&*b, model);
    assert_eq!(initial.models.0[0].revision.as_str(), "1");
    assert!(matches!(
        initial.models.0[0].price_kind,
        UsagePriceKind::Untracked
    ));
    let mut h = harness(shell(&engine, &root, Script::default()), 1800.0, 1800.0);
    settle(&mut h);
    h.state_mut().navigate(Route::Usage);
    settle(&mut h);
    press(&mut h, "Prices");
    press(&mut h, &format!("Edit tariff override for {model}"));
    for (label, value) in [
        ("Tariff upstream model", "fixture/native-price"),
        ("Service tier", "priority"),
        ("Context above (exclusive)", "200000"),
        ("Context up to (inclusive)", "1000000"),
        ("Cache TTL seconds", "3600"),
        ("Units per rate", "17"),
        ("Amount USD per rate", "0.000000000123"),
        ("Tariff region", "region-42"),
        ("Endpoint profile", "CompatibleChat"),
    ] {
        replace(&mut h, label, value);
    }
    for (label, option) in [
        ("Tariff modality", "Video"),
        ("Charge kind", "CacheWrite"),
        ("Charge unit", "Second"),
    ] {
        click(&mut h, Role::ComboBox, label);
        click(&mut h, Role::Button, option);
    }
    let expected = json!({"upstreamModelId":"fixture/native-price","serviceTier":"priority","contextLowerExclusive":"200000","contextUpperInclusive":"1000000","modality":"Video","chargeKind":"CacheWrite","cacheTtlSeconds":"3600","unit":"Second","units":"17","amountUsd":"0.000000000123","region":"region-42","endpointProfile":"CompatibleChat"});
    press(&mut h, "Save tariff override");
    let saved = price_catalog(&*b, model);
    assert_eq!(saved.models.0[0].revision.as_str(), "2");
    let tariff = &saved
        .items
        .0
        .iter()
        .find(|entry| {
            entry
                .model_id
                .as_ref()
                .is_some_and(|id| id.as_str() == model)
                && matches!(entry.origin, PriceOrigin::UserConfigured)
        })
        .unwrap()
        .tariff;
    assert_eq!(serde_json::to_value(tariff).unwrap(), expected);
    press(&mut h, &format!("Edit tariff override for {model}"));
    replace(&mut h, "Amount USD per rate", "3.25");
    let draft = h.state().usage.price_draft.clone().unwrap();
    let mut other = expected.clone();
    other["amountUsd"] = json!("9.1");
    uops::overridden(b.command("router.price_override",&uops::command(&*b,"router.price_override",json!({"modelId":model,"expectedRevision":saved.models.0[0].revision,"change":{"kind":"Set","tariffs":[other]}})))).unwrap();
    press(&mut h, "Reload price catalog");
    assert_eq!(h.state().usage.price_draft.as_ref().unwrap(), &draft);
    assert!(has_text(&h, "Your tariff draft is preserved"));
    assert!(
        h.get_by_role_and_label(Role::Button, "Save tariff override")
            .accesskit_node()
            .is_disabled()
    );
    press(&mut h, "Review and use current price revision");
    press(&mut h, "Save tariff override");
    let current = price_catalog(&*b, model);
    assert_eq!(current.models.0[0].revision.as_str(), "4");
    assert!(current.items.0.iter().any(|entry| matches!(
        entry.origin,
        PriceOrigin::UserConfigured
    ) && serde_json::to_value(&entry.tariff).unwrap()
        ["amountUsd"]
        == json!("3.25")));
    press(&mut h, &format!("Edit tariff override for {model}"));
    press(&mut h, "Clear tariff override");
    let cleared = price_catalog(&*b, model);
    assert_eq!(cleared.models.0[0].revision.as_str(), "5");
    assert!(
        !cleared
            .items
            .0
            .iter()
            .any(|entry| matches!(entry.origin, PriceOrigin::UserConfigured))
    );
}
