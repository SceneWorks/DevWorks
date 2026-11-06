//! Native usage, price and feature-budget controls over the real runtime. Provider transports
//! remain scripted/no-network; these tests never start inference.
mod common;
use common::*;
use devworks_core::protocol::{data::CoverageState, records::*};
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
    if value.is_empty() {
        h.key_press(egui::Key::Backspace);
    } else {
        h.get_by_role_and_label(Role::TextInput, label)
            .type_text(value);
    }
    h.step();
    h.step();
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, label)
            .accesskit_node()
            .value()
            .as_deref(),
        Some(value),
        "native input {label} retains the entered value across frame updates"
    );
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
fn withdrawing_a_reopened_pending_cap_preserves_active_budget_and_off_remains_explicit() {
    let root = Root::new("usage-native-withdraw-cap");
    let engine = start(&root);
    let b = engine.backend();
    let p = project(&*b, "Withdraw cap project");
    let mut h = open_features(&engine, &root, &p);
    press(&mut h, "New feature");
    replace(&mut h, "Title", "Withdraw captured cap");
    click(&mut h, Role::CheckBox, "Limit feature spending");
    replace(&mut h, "Feature spending limit USD", "5");
    press(&mut h, "Create");
    let f = features(&*b, &p).remove(0);
    let active_budget = f.budget.as_ref().unwrap().clone();
    select_feature(&mut h, "Withdraw captured cap  ·");
    press(&mut h, "Edit plan");
    replace(&mut h, "Plan text", "The exact retained plan scope.");
    click(
        &mut h,
        Role::CheckBox,
        "Propose a spending cap change with this plan",
    );
    replace(&mut h, "Feature spending limit USD", "12.50");
    press(&mut h, "Save plan draft");
    let saved = feature(&*b, f.id.as_str());
    let pending = saved.pending_plan.as_ref().unwrap();
    let old_digest = pending.plan_digest.clone();
    assert!(
        matches!(&pending.budget, Some(FeatureBudgetInput::Limit { micro_usd }) if micro_usd.as_str() == "12500000")
    );
    // An old client omitting both fields retains the already saved proposal.
    let legacy = wops::work_changed(b.command(
        "features.edit_plan",
        &wops::work_command(
            &*b,
            "features.edit_plan",
            json!({
                "featureId":f.id,"expectedRevision":saved.revision,"plan":pending.draft
            }),
        ),
    ))
    .unwrap();
    assert_eq!(
        legacy.pending_plan.as_ref().unwrap().plan_digest,
        old_digest
    );
    assert!(legacy.pending_plan.as_ref().unwrap().budget.is_some());
    // Reopen a new native editor from durable backend state before unchecking.
    drop(h);
    let mut h = open_features(&engine, &root, &p);
    select_feature(&mut h, "Withdraw captured cap  ·");
    press(&mut h, "Edit plan");
    assert!(h.state().work.plan.as_ref().unwrap().budget.is_some());
    click(
        &mut h,
        Role::CheckBox,
        "Propose a spending cap change with this plan",
    );
    press(&mut h, "Save plan draft");
    let withdrawn = feature(&*b, f.id.as_str());
    let pending = withdrawn.pending_plan.as_ref().unwrap();
    assert!(pending.budget.is_none());
    assert_ne!(pending.plan_digest, old_digest);
    assert_eq!(
        serde_json::to_value(&withdrawn.budget).unwrap(),
        serde_json::to_value(Some(&active_budget)).unwrap()
    );
    approve_pending_cap(&mut h, &*b, f.id.as_str());
    let approved = feature(&*b, f.id.as_str());
    assert!(approved.pending_plan.is_none());
    assert_eq!(
        serde_json::to_value(&approved.budget).unwrap(),
        serde_json::to_value(Some(&active_budget)).unwrap()
    );
    // A new explicit Off proposal has different meaning and changes the active policy only
    // after approval of that exact pending plan.
    h.state_mut().navigate(Route::Features);
    settle(&mut h);
    select_feature(&mut h, "Withdraw captured cap  ·");
    press(&mut h, "Edit plan");
    click(
        &mut h,
        Role::CheckBox,
        "Propose a spending cap change with this plan",
    );
    click(&mut h, Role::CheckBox, "Limit feature spending");
    press(&mut h, "Save plan draft");
    let off = feature(&*b, f.id.as_str());
    assert!(matches!(
        off.pending_plan.as_ref().unwrap().budget,
        Some(FeatureBudgetInput::Off {})
    ));
    assert_eq!(
        off.budget
            .as_ref()
            .unwrap()
            .limit_micro_usd
            .as_ref()
            .unwrap()
            .as_str(),
        "5000000"
    );
    approve_pending_cap(&mut h, &*b, f.id.as_str());
    assert!(
        feature(&*b, f.id.as_str())
            .budget
            .unwrap()
            .limit_micro_usd
            .is_none()
    );
}
fn approve_pending_cap(h: &mut Ui, b: &dyn Backend, id: &str) {
    press(h, "Request approval");
    let pending = feature(b, id).pending_plan.unwrap();
    let proposal = pending.approval.unwrap().proposal_id;
    h.state_mut().navigate(Route::Approvals);
    settle(h);
    press(h, &format!("Inspect proposal {}", proposal.as_str()));
    replace(
        h,
        "Decision rationale",
        "Approve this exact saved plan and budget intent.",
    );
    press(h, "Approve exact proposal");
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
    h.query_all_by_role_and_label(
        Role::CheckBox,
        "Local or free endpoint · Explicit local policy",
    )
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
    let models = if model.is_empty() {
        Vec::<&str>::new()
    } else {
        vec![model]
    };
    uops::prices(b.query(
        "router.price_status",
        &uops::query(
            b,
            "router.price_status",
            json!({"modelIds":models,"includeHistory":true,"page":{"limit":100}}),
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
    assert!(
        h.state().usage.price_draft.is_some(),
        "editor open: busy={}, loading={}, update={}, pages={}, more={}, error={:?}",
        h.state().usage.price_busy,
        h.state().usage.prices_loading,
        h.state().usage.price_update_available,
        h.state().usage.price_pages,
        h.state().usage.prices.as_ref().is_some_and(|p| p.has_more),
        h.state().usage.price_error
    );
    press(&mut h, "Clear tariff override");
    let cleared = price_catalog(&*b, model);
    assert_eq!(cleared.models.0[0].revision.as_str(), "5");
    assert_eq!(
        cleared.models.0[0]
            .override_revision
            .as_ref()
            .map(|r| r.as_str()),
        Some("5"),
        "the clear tombstone retains its authoritative revision"
    );
    assert!(
        !cleared.items.0.iter().any(|entry| entry
            .override_revision
            .as_ref()
            .is_some_and(|r| r.as_str() == "5")),
        "Clear never invents a tariff"
    );
    assert!(matches!(
        cleared.models.0[0].price_kind,
        UsagePriceKind::Untracked
    ));
    let history = cleared
        .override_history
        .as_ref()
        .expect("configured history is returned when requested");
    assert_eq!(history.0.len(), 4);
    assert!(history.0.iter().any(|item|matches!(item, PriceOverrideOutcome::Cleared{revision,updated_by:Some(_),updated_at_ms:Some(_),..} if revision.as_str()=="5")));
    click(&mut h, Role::CheckBox, "Include historical price sources");
    press(&mut h, "Apply price catalog filters");
    click(&mut h, Role::DisclosureTriangle, "Configured price history");
    assert!(has_text(&h, "revision 5 · Override cleared"));
    assert!(has_text(&h, "This change removed the configured override"));
    press(&mut h, &format!("Edit tariff override for {model}"));
    let blank = h.state().usage.price_draft.as_ref().unwrap();
    assert_eq!(blank.tariffs.len(), 1);
    assert!(
        blank.tariffs[0].amount.is_empty(),
        "archived inactive rates never prefill a new override"
    );
}

#[derive(Default)]
struct PublicPriceHttp {
    state: std::sync::Mutex<(bool, u64, usize)>,
    changed: std::sync::Condvar,
}
impl PublicPriceHttp {
    fn release(&self) {
        self.state.lock().unwrap().0 = true;
        self.changed.notify_all();
    }
    fn next(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 = false;
        state.1 += 1;
    }
}
impl devworks_core::ports::http::HttpTransport for PublicPriceHttp {
    fn send(
        &self,
        request: &devworks_core::ports::http::HttpRequest,
    ) -> Result<devworks_core::ports::http::HttpResponse, devworks_core::ports::http::HttpError>
    {
        use devworks_core::ports::http::*;
        assert_eq!(
            request.url,
            "https://developers.openai.com/api/docs/pricing.md"
        );
        assert!(request.header_value("Authorization").is_none());
        assert!(request.body.is_none());
        let mut state = self.state.lock().unwrap();
        state.2 += 1;
        let rate = state.1 + 1;
        while !state.0 {
            let (next, timeout) = self
                .changed
                .wait_timeout(state, std::time::Duration::from_secs(10))
                .unwrap();
            state = next;
            if timeout.timed_out() {
                return Err(HttpError::Timeout);
            }
        }
        if rate == 3 {
            return Ok(HttpResponse {
                status: 503,
                body: Vec::new(),
            });
        }
        // The actual official adapter parses this retained Markdown table, as in its source tests.
        Ok(HttpResponse { status: 200, body: format!("# Pricing\nPrices per 1M tokens.\n| Model | Input | Output |\n| --- | --- | --- |\n| gpt-test | ${rate} | $2 |\n").into_bytes() })
    }
}
fn start_native_price_refresh(h: &mut Ui) {
    // A provider publication precedes the operation's terminal journal write. A quiet GUI
    // can still display that legitimate earlier status, so confirm it before a new gesture.
    h.state_mut().usage.last_refresh = None;
    step_until(
        h,
        "previous price refresh reaches its recorded terminal state",
        |shell| shell.usage.refresh.is_none(),
    );
    settle(h);
    h.get_by_role_and_label(Role::Button, "Refresh official price sources")
        .scroll_to_me();
    settle(h);
    assert!(
        !h.get_by_role_and_label(Role::Button, "Refresh official price sources")
            .accesskit_node()
            .is_disabled(),
        "the refresh control is enabled before the single user gesture"
    );
    h.get_by_role_and_label(Role::Button, "Refresh official price sources")
        .click();
    // A live operation deliberately schedules periodic repaints. Step the command receipt,
    // without asking the harness to reach an idle repaint state while HTTP is held open.
    for _ in 0..4 {
        h.step();
        assert!(h.state().wait_idle());
    }
    h.step();
    assert!(
        h.state().usage.refresh.is_some(),
        "the single gesture has an accepted durable receipt: {:?}",
        h.state().usage.price_error
    );
}
fn wait_for_price_publication(b: &dyn Backend, snapshots: usize) -> PriceCatalogView {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    loop {
        let catalog = price_catalog(b, "");
        if catalog.snapshots.0.len() == snapshots {
            return catalog;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "actual price publication completes"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
#[test]
fn native_manual_public_price_refresh_is_async_and_keeps_verified_history_without_credentials() {
    let root = Root::new("usage-native-official-refresh");
    let http = std::sync::Arc::new(PublicPriceHttp::default());
    let mut c = config(&root.0);
    c.http = Some(http.clone());
    let engine = devworks_desktop::host::Engine::start(c).unwrap();
    let b = engine.backend();
    let mut h = harness(shell(&engine, &root, Script::default()), 1800.0, 1800.0);
    settle(&mut h);
    h.state_mut().navigate(Route::Usage);
    settle(&mut h);
    press(&mut h, "Prices");
    click(&mut h, Role::DisclosureTriangle, "Price catalog filters");
    replace(&mut h, "Price source IDs", "openai");
    press(&mut h, "Apply price catalog filters");
    assert!(
        h.state()
            .usage
            .prices
            .as_ref()
            .unwrap()
            .snapshots
            .0
            .is_empty()
    );
    start_native_price_refresh(&mut h);
    let first = h
        .state()
        .usage
        .refresh
        .as_ref()
        .unwrap()
        .operation_id
        .clone();
    assert!(has_text(&h, "awaiting the recorded operation outcome"));
    assert!(
        h.state()
            .usage
            .prices
            .as_ref()
            .unwrap()
            .snapshots
            .0
            .is_empty(),
        "accepted never invents a verified snapshot"
    );
    // GUI requests and frames completed while the real adapter's public HTTP request was blocked.
    http.release();
    let first_catalog = wait_for_price_publication(&*b, 1);
    assert_eq!(first_catalog.snapshots.0[0].provider_id.as_str(), "openai");
    assert_eq!(
        first_catalog.snapshots.0[0].source_url.0,
        "https://developers.openai.com/api/docs/pricing"
    );
    h.state_mut().usage.last_refresh = None;
    settle(&mut h);
    assert!(h.state().usage.refresh.is_none());
    assert!(has_text(&h, "ObservedComplete"));
    let snapshot = first_catalog.snapshots.0[0].id.clone();
    http.next();
    start_native_price_refresh(&mut h);
    let second = h
        .state()
        .usage
        .refresh
        .as_ref()
        .unwrap()
        .operation_id
        .clone();
    assert_ne!(first, second);
    let delayed_second_status = h.state().usage.refresh.clone().unwrap();
    assert!(
        h.state()
            .usage
            .prices
            .as_ref()
            .unwrap()
            .snapshots
            .0
            .iter()
            .any(|s| s.id.as_str() == snapshot.as_str()),
        "prior verified snapshot stays visible while refreshing"
    );
    http.release();
    let second_catalog = wait_for_price_publication(&*b, 2);
    let last_verified = second_catalog
        .providers
        .0
        .iter()
        .find(|p| p.provider_id.as_str() == "openai")
        .unwrap()
        .snapshot_id
        .clone();
    h.state_mut().usage.last_refresh = None;
    settle(&mut h);
    click(&mut h, Role::CheckBox, "Include historical price sources");
    press(&mut h, "Apply price catalog filters");
    assert_eq!(
        h.state().usage.prices.as_ref().unwrap().snapshots.0.len(),
        2,
        "history={} query={:?} loading={} refresh={:?} error={:?}",
        h.state().usage.history,
        h.state().usage.price_query,
        h.state().usage.prices_loading,
        h.state().usage.refresh,
        h.state().usage.price_error
    );
    click(&mut h, Role::DisclosureTriangle, "Price source history");
    assert!(has_text(&h, "official-markdown-v1"));
    replace(&mut h, "Price snapshot IDs", snapshot.as_str());
    press(&mut h, "Apply price catalog filters");
    assert_eq!(
        h.state().usage.prices.as_ref().unwrap().snapshots.0.len(),
        1
    );
    assert_eq!(
        h.state().usage.prices.as_ref().unwrap().snapshots.0[0]
            .id
            .as_str(),
        snapshot.as_str()
    );
    // Reapply the real pre-publication GUI observation to deterministically exercise a
    // delayed status reply: publication is committed before the operation journal settles.
    // The next user action must wait for the actual recorded terminal state, not click a
    // control that remains disabled because this earlier observation is still displayed.
    h.state_mut().usage.refresh = Some(delayed_second_status);
    h.state_mut().usage.last_refresh = Some(std::time::Instant::now());
    http.next();
    start_native_price_refresh(&mut h);
    let third = h
        .state()
        .usage
        .refresh
        .as_ref()
        .unwrap()
        .operation_id
        .clone();
    assert_ne!(
        second, third,
        "the third user action admitted a fresh durable refresh"
    );
    http.release();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    loop {
        let catalog = price_catalog(&*b, "");
        if let Some(provider) = catalog
            .providers
            .0
            .iter()
            .find(|p| p.provider_id.as_str() == "openai" && p.error.is_some())
        {
            assert_eq!(
                provider.snapshot_id.as_ref().map(|s| s.as_str()),
                last_verified.as_ref().map(|s| s.as_str())
            );
            assert_eq!(
                catalog.snapshots.0.len(),
                2,
                "failed source refresh publishes no fabricated snapshot"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "failed source is recorded"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    h.state_mut().usage.last_refresh = None;
    settle(&mut h);
    assert!(h.state().usage.refresh.is_none());
    assert!(has_text(&h, "HTTP 503"));
    assert!(has_text(&h, "Failed"));
    assert_eq!(http.state.lock().unwrap().2, 3);
    let connections =
        ops::connection_records(b.query("router.connections", &ops::router_connections(&*b, None)))
            .unwrap()
            .0;
    assert!(
        connections.is_empty(),
        "public price refresh never enables or authenticates a connection"
    );
}

#[test]
fn native_price_paging_keeps_loaded_rows_and_draft_until_explicit_catalog_reload() {
    let root = Root::new("usage-native-price-paging");
    let engine = start(&root);
    let b = engine.backend();
    let model = "paged-price-model";
    ops::configured(b.command("router.configure",&ops::router_configure(&*b,json!({"kind":"Model","model":{"id":model,"displayName":"Paged price model","endpointProfile":"CompatibleChat","upstreamModelId":"fixture/paged-price","enabled":true,"tierMemberships":[],"efforts":[],"imageInput":false,"tools":false,"revision":"1"}})))).unwrap();
    let tariffs=(0..105).map(|i|json!({"upstreamModelId":"fixture/paged-price","serviceTier":format!("service-{i:03}"),"modality":"Text","chargeKind":"Input","unit":"Token","units":"1000000","amountUsd":"1"})).collect::<Vec<_>>();
    uops::overridden(b.command("router.price_override",&uops::command(&*b,"router.price_override",json!({"modelId":model,"expectedRevision":"1","change":{"kind":"Set","tariffs":tariffs}})))).unwrap();
    let mut h = harness(shell(&engine, &root, Script::default()), 1800.0, 1800.0);
    settle(&mut h);
    h.state_mut().navigate(Route::Usage);
    settle(&mut h);
    press(&mut h, "Prices");
    assert!(h.state().usage.prices.as_ref().unwrap().has_more);
    assert!(
        h.get_by_role_and_label(Role::Button, &format!("Edit tariff override for {model}"))
            .accesskit_node()
            .is_disabled(),
        "a partial rate set cannot replace the full configured override"
    );
    press(&mut h, "Load more price rows");
    assert_eq!(h.state().usage.prices.as_ref().unwrap().items.0.len(), 105);
    assert_eq!(h.state().usage.price_pages, 2);
    press(&mut h, &format!("Edit tariff override for {model}"));
    let draft = h.state().usage.price_draft.clone().unwrap();
    assert_eq!(draft.tariffs.len(), 105);
    let replacement = json!({"upstreamModelId":"fixture/paged-price","serviceTier":"changed-source","modality":"Text","chargeKind":"Input","unit":"Token","units":"1000000","amountUsd":"2"});
    uops::overridden(b.command("router.price_override",&uops::command(&*b,"router.price_override",json!({"modelId":model,"expectedRevision":"2","change":{"kind":"Set","tariffs":[replacement]}})))).unwrap();
    settle(&mut h);
    assert!(h.state().usage.price_update_available);
    assert_eq!(
        h.state().usage.prices.as_ref().unwrap().items.0.len(),
        105,
        "background updates never silently discard a loaded page set"
    );
    assert_eq!(h.state().usage.price_draft.as_ref().unwrap(), &draft);
    assert!(has_text(
        &h,
        "loaded pages and your tariff draft are preserved"
    ));
    assert!(
        h.get_by_role_and_label(Role::Button, "Save tariff override")
            .accesskit_node()
            .is_disabled()
    );
    press(&mut h, "Reload price catalog");
    assert_eq!(h.state().usage.prices.as_ref().unwrap().items.0.len(), 1);
    assert_eq!(h.state().usage.price_draft.as_ref().unwrap(), &draft);
    assert!(!h.state().usage.price_update_available);
    assert!(has_text(&h, "price revision changed"));
}

fn configured_connection(b: &dyn Backend) -> ConnectionRecord {
    ops::connection_records(b.query("router.connections", &ops::router_connections(b, None)))
        .unwrap()
        .0
        .remove(0)
}
#[test]
fn native_connection_billing_edit_preserves_refused_and_stale_draft_until_explicit_review() {
    let root = Root::new("connection-billing-stale");
    let mut c = config(&root.0);
    c.connection_tick = None;
    let engine = devworks_desktop::host::Engine::start(c).unwrap();
    let b = engine.backend();
    ops::configured(b.command("router.configure",&ops::router_configure(&*b,json!({"kind":"Connection","name":"Captured billing","profile":"CompatibleChat","endpoint":"http://127.0.0.1:11434/v1"})))).unwrap();
    let mut h = harness(shell(&engine, &root, Script::default()), 1800.0, 1600.0);
    settle(&mut h);
    h.state_mut().navigate(Route::Settings);
    settle(&mut h);
    press(&mut h, "Provider connections");
    press(&mut h, "Edit");
    replace(&mut h, "Captured billing endpoint", "not a URL");
    click(
        &mut h,
        Role::CheckBox,
        "Local or free endpoint · Captured billing",
    );
    press(&mut h, "Save connection");
    assert_eq!(configured_connection(&*b).revision.as_str(), "1");
    assert!(
        h.query_by_role_and_label(Role::Button, "Close editor")
            .is_some(),
        "refusal keeps the editor open"
    );
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, "Captured billing endpoint")
            .accesskit_node()
            .value()
            .as_deref(),
        Some("not a URL")
    );
    assert_eq!(
        h.get_by_role_and_label(Role::CheckBox, "Local or free endpoint · Captured billing")
            .accesskit_node()
            .toggled(),
        Some(egui::accesskit::Toggled::True)
    );
    replace(
        &mut h,
        "Captured billing endpoint",
        "http://127.0.0.1:11434/v1",
    );
    replace(&mut h, "Captured billing name", "My billing choice");
    let r = configured_connection(&*b);
    ops::configured(b.command("router.configure",&ops::router_configure(&*b,json!({"kind":"Connection","connectionId":r.id,"expectedRevision":r.revision,"name":"External current","profile":"CompatibleChat","endpoint":"http://127.0.0.1:11435/v1","localFree":false})))).unwrap();
    press(&mut h, "Reload");
    assert!(has_text(&h, "Editing captured connection revision 1"));
    assert!(has_text(&h, "Stored revision 2"));
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, "External current name")
            .accesskit_node()
            .value()
            .as_deref(),
        Some("My billing choice")
    );
    assert!(
        h.get_by_role_and_label(Role::Button, "Save connection")
            .accesskit_node()
            .is_disabled()
    );
    press(&mut h, "Review and use current connection revision");
    press(&mut h, "Save connection");
    let saved = configured_connection(&*b);
    assert_eq!(saved.revision.as_str(), "3");
    assert_eq!(saved.name.0, "My billing choice");
    assert_eq!(saved.local_free, Some(true));
    assert!(
        h.query_by_role_and_label(Role::Button, "Close editor")
            .is_none(),
        "confirmed success closes the unchanged submitted draft"
    );
}
#[test]
fn native_unknown_connection_billing_edit_survives_restart_with_exact_frame_and_command_identity() {
    let root = Root::new("connection-billing-unknown");
    let mut c = config(&root.0);
    c.connection_tick = None;
    let engine = devworks_desktop::host::Engine::start(c).unwrap();
    let real = engine.backend();
    ops::configured(real.command("router.configure",&ops::router_configure(&*real,json!({"kind":"Connection","name":"Uncertain billing","profile":"CompatibleChat","endpoint":"http://127.0.0.1:11434/v1"})))).unwrap();
    let lossy = LoseAcks::over(real.clone(), "router.configure", 4);
    let mut h = harness(
        shell_with(Ok(lossy.clone()), &root, Script::default(), None),
        1800.0,
        1600.0,
    );
    settle(&mut h);
    h.state_mut().navigate(Route::Settings);
    settle(&mut h);
    press(&mut h, "Provider connections");
    press(&mut h, "Edit");
    click(
        &mut h,
        Role::CheckBox,
        "Local or free endpoint · Uncertain billing",
    );
    press(&mut h, "Save connection");
    assert_eq!(configured_connection(&*real).local_free, Some(true));
    assert!(has_text(&h, "Awaiting confirmation for connection edit"));
    assert!(
        h.get_by_role_and_label(Role::Button, "Save connection")
            .accesskit_node()
            .is_disabled()
    );
    let frames = lossy.frames.lock().unwrap().clone();
    assert!(!frames.is_empty());
    assert!(frames.windows(2).all(|w| w[0] == w[1]));
    let id = serde_json::from_slice::<serde_json::Value>(&frames[0]).unwrap()["requestKey"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(has_text(&h, &id));
    h.state_mut().on_exit();
    drop(h);
    let confirming = LoseAcks::over(real.clone(), "router.configure", 0);
    let mut h = harness(
        shell_with(Ok(confirming.clone()), &root, Script::default(), None),
        1800.0,
        1600.0,
    );
    settle(&mut h);
    h.state_mut().navigate(Route::Settings);
    settle(&mut h);
    press(&mut h, "Provider connections");
    assert_eq!(
        configured_connection(&*real).revision.as_str(),
        "2",
        "recovery confirms one committed billing change"
    );
    assert!(!has_text(&h, "Awaiting confirmation for connection edit"));
    assert!(
        h.query_by_role_and_label(Role::Button, "Close editor")
            .is_none()
    );
    assert_eq!(
        confirming.frames.lock().unwrap().as_slice(),
        &[frames[0].clone()],
        "recovery retains original schema, command, request key and captured revision"
    );
}

fn recorded_report_origins(engine: &devworks_desktop::host::Engine, project: &str, other: &str) {
    use devworks_core::ports::storage::{StoredUsageOrigin, WriteOptions, run_write};
    use devworks_core::protocol::{Id, ProjectId};
    let b = engine.backend();
    let session=ops::started(b.command("sessions.start", &ops::command_frame(&*b,"sessions.start",json!({"scope":{"kind":"Project","projectId":project},"title":"Recorded report context"})))).unwrap().id;
    let project = ProjectId(Id::new(project).unwrap());
    let other = ProjectId(Id::new(other).unwrap());
    run_write(&**engine.host().runtime().store(),WriteOptions::default(),move |tx| {
        // Immutable historical source fixtures, deliberately without canonical attempt identity.
        // This exercises the actual SQLite/gateway projection, not a frontend report substitute.
        for (source,category,input,output) in [("assistant","Assistant",10,5),("review","CodeReview",7,3),("analysis","RepositoryAnalysis",2,1)] {
            tx.usage().insert_usage_origin(&StoredUsageOrigin {source_kind:"SessionUsage".into(),source_id:source.into(),attempt_id:None,request_id:Some(Id::new(format!("recorded-{source}")).unwrap()),project_id:Some(project.clone()),feature_id:None,work_id:None,session_id:Some(session.clone()),job_run_id:None,payload:json!({"actorId":"recorded-user","providerId":"openrouter","reportCategory":category,"item":{"purpose":"Conversation","modelId":"recorded-user","inputTokens":input.to_string(),"outputTokens":output.to_string()}}),recorded_at_ms:1000,priced_micro_usd:None,classification:"Untracked".into(),monetary_complete:false,incomplete:true,unknown_tail:false})?;
        }
        for (kind,source,payload) in [("RouterRequest","activity",json!({"actorId":"recorded-user","providerId":"openrouter","reportCategory":"Assistant","modelId":"recorded-user","purpose":"Conversation","state":"completed"})),("ToolInvocation","mcp",json!({"actorId":"recorded-user","transportOrigin":"Mcp","capabilityId":"fixture.tool","outcome":"success","durationMs":"3"}))] {
            tx.usage().insert_usage_origin(&StoredUsageOrigin{source_kind:kind.into(),source_id:source.into(),attempt_id:None,request_id:Some(Id::new(format!("recorded-{source}")).unwrap()),project_id:Some(project.clone()),feature_id:None,work_id:None,session_id:Some(session.clone()),job_run_id:None,payload,recorded_at_ms:1000,priced_micro_usd:None,classification:"Untracked".into(),monetary_complete:false,incomplete:true,unknown_tail:false})?;
        }
        tx.usage().insert_usage_origin(&StoredUsageOrigin{source_kind:"SessionUsage".into(),source_id:"other-project".into(),attempt_id:None,request_id:Some(Id::new("other-request").unwrap()),project_id:Some(other.clone()),feature_id:None,work_id:None,session_id:None,job_run_id:None,payload:json!({"actorId":"other-user","providerId":"other-provider","reportCategory":"Assistant","item":{"purpose":"Conversation","modelId":"other-model","inputTokens":"999","outputTokens":"999"}}),recorded_at_ms:1000,priced_micro_usd:None,classification:"Untracked".into(),monetary_complete:false,incomplete:true,unknown_tail:false})?;
        Ok(())
    }).unwrap();
}
fn report_total(h: &Ui, key: &str) -> Option<String> {
    match h.state().usage.data.as_ref().unwrap() {
        UsageQueryResult::Report { report } => report.items.0.iter().find_map(|r| match r {
            UsageReportRow::Total { key: k, value, .. } if k.0 == key => {
                Some(value.as_str().to_owned())
            }
            _ => None,
        }),
        other => panic!("expected report: {other:?}"),
    }
}
#[test]
fn native_usage_six_reports_share_authoritative_filters_utc_buckets_and_partial_legacy_truth() {
    let root = Root::new("usage-six-native-reports");
    let engine = start(&root);
    let b = engine.backend();
    let p = project(&*b, "Report alpha");
    let other = project(&*b, "Report beta");
    recorded_report_origins(&engine, &p, &other);
    let mut h = harness(shell(&engine, &root, Script::default()), 1800.0, 1800.0);
    settle(&mut h);
    h.state_mut().navigate(Route::Usage);
    settle(&mut h);
    click(&mut h, Role::DisclosureTriangle, "Usage filters");
    click(&mut h, Role::ComboBox, "Usage project");
    h.query_all_by_label("Report alpha")
        .find(|n| {
            matches!(
                n.accesskit_node().role(),
                Role::Button | Role::ListBoxOption
            ) && n.rect().left() > devworks_desktop::theme::NAV
        })
        .unwrap()
        .click();
    settle(&mut h);
    replace(&mut h, "Start UTC", "1970-01-01T00:00:00Z");
    replace(&mut h, "End UTC (exclusive)", "1970-01-02T00:00:00Z");
    click(&mut h, Role::ComboBox, "Usage interval");
    click(&mut h, Role::Button, "Week (Monday)");
    replace(&mut h, "Usage provider", "openrouter");
    replace(&mut h, "Usage tool", "ignored-for-llm");
    press(&mut h, "Apply usage filters");
    assert_eq!(report_total(&h, "totalTokens").as_deref(), Some("15"));
    assert!(
        h.query_by_role_and_label(Role::DisclosureTriangle, "User · recorded-user over time")
            .is_some()
    );
    assert!(
        h.query_by_role_and_label(Role::DisclosureTriangle, "Model · recorded-user over time")
            .is_some(),
        "equal labels from different dimensions remain distinct series"
    );
    assert!(has_text(&h, "Partial coverage"));
    click(&mut h, Role::DisclosureTriangle, "Total Tokens over time");
    assert!(
        has_text(&h, "1969-12-29"),
        "negative epoch Monday bucket renders exactly"
    );
    if let UsageQueryResult::Report { report } = h.state().usage.data.as_ref().unwrap() {
        assert!(report.context.applied_filters.tool.is_none());
        assert_eq!(
            report
                .context
                .applied_filters
                .project_id
                .as_ref()
                .unwrap()
                .0
                .as_str(),
            p
        );
    }
    click(&mut h, Role::ComboBox, "Report");
    click(&mut h, Role::Button, "Assistant activity");
    assert_eq!(report_total(&h, "completedRuns").as_deref(), Some("1"));
    click(&mut h, Role::ComboBox, "Report");
    click(&mut h, Role::Button, "Code review usage");
    assert_eq!(report_total(&h, "totalTokens").as_deref(), Some("10"));
    click(&mut h, Role::ComboBox, "Report");
    click(&mut h, Role::Button, "Repository analysis usage");
    assert_eq!(report_total(&h, "totalTokens").as_deref(), Some("3"));
    replace(&mut h, "Usage tool", "fixture.tool");
    click(&mut h, Role::ComboBox, "Report");
    click(&mut h, Role::Button, "MCP usage");
    assert_eq!(report_total(&h, "callCount").as_deref(), Some("1"));
    assert_eq!(report_total(&h, "averageDurationMs").as_deref(), Some("3"));
    if let UsageQueryResult::Report { report } = h.state().usage.data.as_ref().unwrap() {
        assert!(report.context.applied_filters.provider.is_none());
    }
    click(&mut h, Role::ComboBox, "Report");
    click(&mut h, Role::Button, "Filter discovery");
    let UsageQueryResult::Filters { filters } = h.state().usage.data.as_ref().unwrap() else {
        panic!("filter discovery uses real typed response")
    };
    assert!(
        filters
            .items
            .0
            .iter()
            .any(|v| v.dimension.0 == "tool" && v.value.0 == "fixture.tool")
    );
    assert!(
        !filters
            .items
            .0
            .iter()
            .any(|v| v.value.0 == "other-provider")
    );
    replace(&mut h, "Usage tool", "");
    press(&mut h, "Attempts");
    let UsageQueryResult::Attempts { page } = h.state().usage.data.as_ref().unwrap() else {
        panic!("attempt page")
    };
    assert_eq!(page.items.0.len(), 3);
    assert_eq!(page.totals.attempts.get(), 0);
    assert_eq!(page.totals.legacy_usage_facts.get(), 3);
    assert_eq!(page.totals.tracked_micro_usd.get(), 0);
    assert!(matches!(page.coverage.state, CoverageState::Partial));
    assert!(page.items.0.iter().all(|r| r.attempt_id.is_none()
        && !r.cost.monetary_complete
        && r.cost.amount_micro_usd.is_none()));
    assert!(has_text(&h, "Historical usage facts"));
    assert!(has_text(&h, "Partial coverage"));
    replace(&mut h, "Start UTC", "1960-01-01T00:00:00Z");
    replace(&mut h, "End UTC (exclusive)", "1961-01-01T00:00:00Z");
    press(&mut h, "Apply usage filters");
    let UsageQueryResult::Attempts { page } = h.state().usage.data.as_ref().unwrap() else {
        panic!("historical range remains an actual attempt query")
    };
    assert_eq!(page.context.range.start_ms.0, "-315619200000");
    assert_eq!(page.context.range.end_ms.0, "-283996800000");
    assert!(page.items.0.is_empty());
    assert!(has_text(&h, "1960-01-01"));
    assert!(has_text(&h, "1961-01-01"));
}

#[test]
fn native_unknown_tariff_override_restores_exact_typed_draft_and_replays_one_original_change() {
    let root = Root::new("price-native-unknown-restart");
    let engine = start(&root);
    let real = engine.backend();
    let model = "unknown-tariff-model";
    ops::configured(real.command("router.configure",&ops::router_configure(&*real,json!({"kind":"Model","model":{"id":model,"displayName":"Unknown tariff model","endpointProfile":"CompatibleChat","upstreamModelId":"fixture/unknown-tariff","enabled":true,"tierMemberships":[],"efforts":[],"imageInput":false,"tools":false,"revision":"1"}})))).unwrap();
    let lossy = LoseAcks::over(real.clone(), "router.price_override", 4);
    let mut h = harness(
        shell_with(Ok(lossy.clone()), &root, Script::default(), None),
        1800.0,
        1800.0,
    );
    settle(&mut h);
    h.state_mut().navigate(Route::Usage);
    settle(&mut h);
    press(&mut h, "Prices");
    press(&mut h, &format!("Edit tariff override for {model}"));
    replace(&mut h, "Amount USD per rate", "2.25");
    let draft = h.state().usage.price_draft.clone().unwrap();
    press(&mut h, "Save tariff override");
    assert!(h.state().usage.price_busy);
    assert_eq!(h.state().usage.price_draft.as_ref(), Some(&draft));
    assert!(
        h.get_by_role_and_label(Role::Button, "Save tariff override")
            .accesskit_node()
            .is_disabled()
    );
    assert_eq!(
        price_catalog(&*real, model).models.0[0].revision.as_str(),
        "2"
    );
    let original = lossy.frames.lock().unwrap()[0].clone();
    h.state_mut().on_exit();
    drop(h);
    let still_unknown = LoseAcks::over(real.clone(), "router.price_override", 4);
    let mut h = harness(
        shell_with(Ok(still_unknown.clone()), &root, Script::default(), None),
        1800.0,
        1800.0,
    );
    settle(&mut h);
    h.state_mut().navigate(Route::Usage);
    settle(&mut h);
    press(&mut h, "Prices");
    assert!(h.state().usage.price_busy);
    assert_eq!(h.state().usage.price_draft.as_ref(), Some(&draft));
    assert!(
        h.get_by_role_and_label(Role::Button, "Save tariff override")
            .accesskit_node()
            .is_disabled()
    );
    press(&mut h, "Confirm outcomes");
    assert!(!h.state().usage.price_busy);
    assert!(h.state().usage.price_draft.is_none());
    assert_eq!(
        price_catalog(&*real, model).models.0[0].revision.as_str(),
        "2"
    );
    assert!(lossy.frames.lock().unwrap().iter().all(|f| f == &original));
    assert!(
        still_unknown
            .frames
            .lock()
            .unwrap()
            .iter()
            .all(|f| f == &original)
    );
}

#[test]
fn native_usage_paging_keeps_applied_filters_and_preserves_unsubmitted_filter_draft() {
    use devworks_core::ports::storage::{StoredUsageOrigin, WriteOptions, run_write};
    use devworks_core::protocol::{Id, ProjectId};
    let root = Root::new("usage-native-paging");
    let engine = start(&root);
    let b = engine.backend();
    let p = project(&*b, "Usage paging");
    let session = ops::started(b.command(
        "sessions.start",
        &ops::command_frame(
            &*b,
            "sessions.start",
            json!({"scope":{"kind":"Project","projectId":p},"title":"Recorded paging context"}),
        ),
    ))
    .unwrap()
    .id;
    let project_id = ProjectId(Id::new(p.clone()).unwrap());
    run_write(&**engine.host().runtime().store(),WriteOptions::default(),move|tx|{
        for i in 0..120 {
            tx.usage().insert_usage_origin(&StoredUsageOrigin{source_kind:"SessionUsage".into(),source_id:format!("page-{i:03}"),attempt_id:None,request_id:Some(Id::new(format!("recorded-page-{i:03}")).unwrap()),project_id:Some(project_id.clone()),feature_id:None,work_id:None,session_id:Some(session.clone()),job_run_id:None,payload:json!({"actorId":format!("recorded-user-{i:03}"),"providerId":"openrouter","reportCategory":"Assistant","item":{"purpose":"Conversation","modelId":format!("recorded-model-{i:03}"),"inputTokens":"1","outputTokens":"1"}}),recorded_at_ms:1000,priced_micro_usd:None,classification:"Untracked".into(),monetary_complete:false,incomplete:true,unknown_tail:false})?;
        }Ok(())
    }).unwrap();
    let mut h = harness(shell(&engine, &root, Script::default()), 1800.0, 1800.0);
    settle(&mut h);
    h.state_mut().navigate(Route::Usage);
    settle(&mut h);
    click(&mut h, Role::DisclosureTriangle, "Usage filters");
    replace(&mut h, "Start UTC", "1970-01-01T00:00:00Z");
    replace(&mut h, "End UTC (exclusive)", "1970-01-02T00:00:00Z");
    h.state_mut().usage.draft.project = Some(p);
    press(&mut h, "Apply usage filters");
    let UsageQueryResult::Report { report } = h.state().usage.data.as_ref().unwrap() else {
        panic!("actualreport")
    };
    let context = serde_json::to_value(&report.context).unwrap();
    let initial = report.items.0.len();
    assert!(report.has_more);
    replace(&mut h, "Usage user", "unsubmitted-user");
    press(&mut h, "Load more usage rows");
    let UsageQueryResult::Report { report } = h.state().usage.data.as_ref().unwrap() else {
        panic!("actualreport")
    };
    assert!(
        report.items.0.len() > initial,
        "initial={initial} rows={} loading={} generation={} error={:?} hasmore={} applied={:?} context={:?}",
        report.items.0.len(),
        h.state().usage.loading,
        h.state().usage.generation,
        h.state().usage.error,
        report.has_more,
        h.state().usage.applied,
        report.context
    );
    assert_eq!(serde_json::to_value(&report.context).unwrap(), context);
    assert!(report.context.applied_filters.user.is_none());
    assert_eq!(h.state().usage.draft.user, "unsubmitted-user");
    replace(&mut h, "Usage user", "");
    assert!(h.state().usage.draft.user.is_empty());
    press(&mut h, "Attempts");
    let UsageQueryResult::Attempts { page } = h.state().usage.data.as_ref().unwrap() else {
        panic!("actualattemptpage")
    };
    assert!(page.has_more);
    assert_eq!(page.items.0.len(), 100);
    let context = serde_json::to_value(&page.context).unwrap();
    press(&mut h, "Load more usage rows");
    let UsageQueryResult::Attempts { page } = h.state().usage.data.as_ref().unwrap() else {
        panic!("actualattemptpage")
    };
    assert!(!page.has_more);
    assert_eq!(page.items.0.len(), 120);
    assert_eq!(page.totals.legacy_usage_facts.get(), 120);
    assert_eq!(serde_json::to_value(&page.context).unwrap(), context);
}

#[test]
fn native_connection_state_change_preserves_an_unsaved_billing_editor() {
    let root = Root::new("connection-billing-independent-state");
    let mut c = config(&root.0);
    c.connection_tick = None;
    let engine = devworks_desktop::host::Engine::start(c).unwrap();
    let b = engine.backend();
    ops::configured(b.command("router.configure",&ops::router_configure(&*b,json!({"kind":"Connection","name":"Independent billing","profile":"CompatibleChat","endpoint":"http://127.0.0.1:11434/v1"})))).unwrap();
    let mut h = harness(shell(&engine, &root, Script::default()), 1800.0, 1600.0);
    settle(&mut h);
    h.state_mut().navigate(Route::Settings);
    settle(&mut h);
    press(&mut h, "Provider connections");
    press(&mut h, "Edit");
    replace(&mut h, "Independent billing name", "Retained unsaved name");
    click(
        &mut h,
        Role::CheckBox,
        "Local or free endpoint · Independent billing",
    );
    press(&mut h, "Turn off");
    let stored = configured_connection(&*b);
    assert_eq!(stored.name.0, "Independent billing");
    assert_eq!(stored.local_free, Some(false));
    assert_eq!(stored.enabled, Some(false));
    assert!(
        h.query_by_role_and_label(Role::Button, "Close editor")
            .is_some(),
        "an independent connection command never settles an unsubmitted editor"
    );
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, "Independent billing name")
            .accesskit_node()
            .value()
            .as_deref(),
        Some("Retained unsaved name")
    );
    assert!(is_on(&h.get_by_role_and_label(
        Role::CheckBox,
        "Local or free endpoint · Independent billing"
    )));
    assert!(
        h.get_by_role_and_label(Role::Button, "Save connection")
            .accesskit_node()
            .is_disabled()
    );
    press(&mut h, "Review and use current connection revision");
    press(&mut h, "Save connection");
    let stored = configured_connection(&*b);
    assert_eq!(stored.name.0, "Retained unsaved name");
    assert_eq!(stored.local_free, Some(true));
    assert_eq!(stored.enabled, Some(false));
}

#[test]
fn settling_applies_requests_started_in_the_last_successful_repaint_frame() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let root = Root::new("usage-settle-final-frame");
    let engine = start(&root);
    let prices = Gate::new("router.price_status").missing_hints();
    let catalog = Gate::new("router.catalog").missing_hints();
    let backend = prices.over(catalog.over(engine.backend()));
    let armed = Arc::new(AtomicUsize::new(0));
    let (frame, price_gate, catalog_gate) = (armed.clone(), prices.clone(), catalog.clone());
    let mut h = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1800.0, 1800.0))
        .with_max_steps(12)
        .build_ui_state(
            move |ui, shell: &mut devworks_desktop::shell::Shell| {
                shell.ui(ui);
                if frame.load(Ordering::SeqCst) > 0 {
                    match frame.fetch_add(1, Ordering::SeqCst) {
                        // settle's two preliminary frames are quiet. A real query is submitted
                        // after rendering the final try_run frame, before its next worker wake.
                        3 => shell.navigate(Route::Usage),
                        4 => {
                            price_gate.open();
                            catalog_gate.open();
                        }
                        _ => {}
                    }
                }
            },
            shell_with(Ok(backend), &root, Script::default(), None),
        );
    settle(&mut h);
    h.state_mut().navigate(Route::Usage);
    settle(&mut h);
    press(&mut h, "Prices");
    while !h.state().client().unwrap().activities().is_empty() {
        assert!(h.state().wait_idle());
        h.step();
    }
    struct Release(Gate, Gate);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.open();
            self.1.open();
        }
    }
    let _release = Release(prices.clone(), catalog.clone());
    prices.close();
    catalog.close();
    armed.store(1, Ordering::SeqCst);
    settle(&mut h);
    assert!(
        h.state().client().unwrap().activities().is_empty(),
        "a successful repaint left a real backend reply pending"
    );
    assert!(!h.state().usage.prices_loading);
}

#[test]
fn native_blank_tariff_editor_prices_default_requests_and_preserves_exact_custom_tiers() {
    use devworks_core::ports::{
        storage::{ReadOptions, read_snapshot},
        usage::{CostClassification, PriceQuery, PriceResolver},
    };
    use devworks_core::services::prices::CatalogPriceResolver;
    let root = Root::new("usage-native-default-service-tier");
    let mut config = config(&root.0);
    config.connection_tick = None;
    let engine = devworks_desktop::host::Engine::start(config).unwrap();
    let b = engine.backend();
    ops::configured(b.command("router.configure", &ops::router_configure(&*b,json!({"kind":"Connection","name":"Default tier quote connection","profile":"CompatibleChat","endpoint":"http://127.0.0.1:9/v1"})))).unwrap();
    let connections =
        ops::connection_records(b.query("router.connections", &ops::router_connections(&*b, None)))
            .unwrap()
            .0;
    let connection = connections
        .iter()
        .find(|record| record.name.0 == "Default tier quote connection")
        .unwrap();
    let model = "native-default-service-tier";
    ops::configured(b.command("router.configure", &ops::router_configure(&*b,json!({"kind":"Model","model":{"id":model,"displayName":"Default tier quote model","endpointProfile":"CompatibleChat","connectionId":connection.id,"upstreamModelId":"fixture/default-tier","enabled":true,"tierMemberships":[],"efforts":[],"imageInput":false,"tools":false,"revision":"1"}})))).unwrap();
    assert!(matches!(
        price_catalog(&*b, model).models.0[0].price_kind,
        UsagePriceKind::Untracked
    ));
    let mut h = harness(shell(&engine, &root, Script::default()), 1800.0, 1400.0);
    settle(&mut h);
    h.state_mut().navigate(Route::Usage);
    settle(&mut h);
    press(&mut h, "Prices");
    press(&mut h, &format!("Edit tariff override for {model}"));
    // Leave the new editor's service tier untouched, as a person pricing ordinary requests would.
    replace(&mut h, "Amount USD per rate", "2.5");
    press(&mut h, "Save tariff override");
    let store = engine.host().runtime().store();
    let mut query = PriceQuery {
        catalog_model_id: devworks_core::protocol::Id::new(model).unwrap(),
        provider_model_id: Some("fixture/default-tier".into()),
        connection_id: connection.id.clone(),
        endpoint_profile: "CompatibleChat".into(),
        access_mode: "Api".into(),
        service_tier: None,
        input_tokens: Some(100),
        cache_write_ttl_ms: None,
        processing_region: None,
        required_modalities: vec!["Text".into()],
    };
    let quote = read_snapshot(&**store, &ReadOptions::default(), |read| {
        CatalogPriceResolver.quote(read, &query)
    })
    .unwrap();
    assert_eq!(
        quote.classification,
        CostClassification::ConfiguredEstimate,
        "an untouched blank tariff editor must price a request without an explicit service tier: {quote:?}"
    );
    assert_eq!(quote.untracked_reason, None);
    assert_eq!(quote.tariffs[0].service_tier, "Standard");
    assert_eq!(
        quote
            .rates
            .as_ref()
            .unwrap()
            .input_usd_per_million
            .as_deref(),
        Some("2.5")
    );
    press(&mut h, &format!("Edit tariff override for {model}"));
    replace(&mut h, "Service tier", "CustomPriority");
    press(&mut h, "Save tariff override");
    let quote_for = |query: &PriceQuery| {
        read_snapshot(&**store, &ReadOptions::default(), |read| {
            CatalogPriceResolver.quote(read, query)
        })
        .unwrap()
    };
    let ordinary = quote_for(&query);
    assert_eq!(
        ordinary.untracked_reason.as_deref(),
        Some("service_tier_unpriced")
    );
    query.service_tier = Some("CustomPriority".into());
    let exact = quote_for(&query);
    assert_eq!(exact.classification, CostClassification::ConfiguredEstimate);
    assert_eq!(exact.tariffs[0].service_tier, "CustomPriority");
    query.service_tier = Some("custompriority".into());
    assert_eq!(
        quote_for(&query).untracked_reason.as_deref(),
        Some("service_tier_unpriced"),
        "custom labels remain case-sensitive"
    );
}

#[path = "usage/official.rs"]
mod official;
#[path = "usage/refresh.rs"]
mod refresh;
