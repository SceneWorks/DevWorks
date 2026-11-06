//! The composer's route and effort selectors over the real runtime (UI-10, ROUTE-19, ROUTE-20):
//! tiers before named models, a separate effort selector, one selection shared by Home and the
//! sidebar, the selection of an unpinned chat restored after a restart, a pinned conversation
//! changed through `router.pin` (and an unconfirmed change confirmed with its identical frame at
//! the next launch), and truthful availability when no provider is connected. Connections are
//! scripted; no provider is contacted and no inference runs.
mod common;

use common::*;
use devworks_core::ports::router::{ConnectionDirectory, ConnectionState, ConnectionStatus};
use devworks_core::ports::storage::{RouterRead, StorageError};
use devworks_core::protocol::Id;
use devworks_core::protocol::records::RouteIntent;
use devworks_desktop::backend::Backend;
use devworks_desktop::chat::{COMPOSER_LABEL, SEND_LABEL};
use devworks_desktop::host::Engine;
use devworks_desktop::ops;
use devworks_desktop::routing::{EFFORT_LABEL, ROUTE_LABEL};
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::{NodeT, Queryable};
use serde_json::json;
use std::sync::Arc;

/// Both subscriptions connected (no discovery yet).
struct Connected;
impl ConnectionDirectory for Connected {
    fn connections(&self, _: &dyn RouterRead) -> Result<Vec<ConnectionStatus>, StorageError> {
        Ok([
            ("ClaudeSubscriptionMessages", "conn-claude"),
            ("CodexSubscriptionResponses", "conn-codex"),
        ]
        .into_iter()
        .map(|(profile, id)| ConnectionStatus {
            id: Id::new(id).unwrap(),
            profile: profile.into(),
            state: ConnectionState::Connected,
            discovered: Some(if profile == "ClaudeSubscriptionMessages" {
                [
                    "claude-fable-5-1",
                    "claude-opus-5-5",
                    "claude-sonnet-5-5",
                    "claude-haiku-5-5",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect()
            } else {
                ["gpt-6-astra", "gpt-6.1-sol", "gpt-5.6-terra", "gpt-6-luna"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect()
            }),
            allowed: Vec::new(),
            excluded: Vec::new(),
        })
        .collect())
    }
}

fn start_connected(root: &Root) -> Engine {
    let mut c = config(&root.0);
    c.connections = Some(Arc::new(Connected));
    Engine::start(c).expect("the runtime boots")
}

fn combo_text(
    h: &egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    label: &str,
) -> String {
    let n = h.get_by_role_and_label(Role::ComboBox, label);
    n.accesskit_node().value().unwrap_or_default()
}

/// Opens the selector `combo` and picks `option`.
fn choose(
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    combo: &str,
    option: &str,
) {
    h.get_by_role_and_label(Role::ComboBox, combo).click();
    h.step();
    h.step();
    // The option is activated as an assistive technology would (a pointer click in the
    // headless popup does not hit-test).
    h.query_all_by_label(option)
        .find(|n| n.accesskit_node().role() != Role::ComboBox)
        .unwrap_or_else(|| panic!("option {option} in {combo}"))
        .click_accesskit();
    settle(h);
}

fn options(
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    combo: &str,
) -> Vec<String> {
    h.get_by_role_and_label(Role::ComboBox, combo).click();
    h.step();
    h.step();
    let got: Vec<String> = h
        .query_all_by_role(Role::Button)
        .chain(h.query_all_by_role(Role::ListBoxOption))
        .filter_map(|n| n.accesskit_node().label())
        .collect();
    // Close the popup again.
    h.key_press(eframe::egui::Key::Escape);
    settle(h);
    got
}

fn pin_of(backend: &dyn Backend, session: &str) -> Option<(String, RouteIntent, String)> {
    let a = ops::availability(backend.query(
        "router.availability",
        &ops::router_availability(
            backend,
            &RouteIntent::Tier {
                tier: devworks_core::protocol::records::TierName::High,
            },
            &devworks_core::protocol::records::EffortName::Default,
            Some(session),
        ),
    ))
    .unwrap();
    a.pin.map(|p| {
        (
            p.model_id.as_str().to_owned(),
            p.intent,
            serde_json::to_value(&p.requested_effort)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned(),
        )
    })
}

#[test]
fn the_composer_offers_tiers_then_named_models_and_a_separate_effort() {
    let root = Root::new("routing-selectors");
    let engine = start_connected(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    assert_eq!(
        combo_text(&h, ROUTE_LABEL),
        "Tier High",
        "the default selection"
    );
    let routes = options(&mut h, ROUTE_LABEL);
    let wanted = [
        "Tier Max",
        "Tier High",
        "Tier Mid",
        "Tier Low",
        "Open Router Auto",
        "Fable",
        "Opus",
        "Sonnet",
        "Haiku",
        "Astra",
        "Sol",
        "Terra",
        "Luna",
    ];
    let positions: Vec<usize> = wanted
        .iter()
        .map(|w| {
            routes
                .iter()
                .position(|r| r == w)
                .unwrap_or_else(|| panic!("{w} missing from {routes:?}"))
        })
        .collect();
    assert!(positions.windows(2).all(|p| p[0] < p[1]), "{routes:?}");
    for (_, label) in devworks_desktop::routing::LATEST_FAMILIES {
        assert!(
            routes.iter().any(|r| r == label),
            "{label} missing from {routes:?}"
        );
    }
    assert!(has_text(&h, "available:"));
    // A named model is chosen directly; effort is a separate choice from what it declares.
    choose(&mut h, ROUTE_LABEL, "Opus");
    assert_eq!(combo_text(&h, ROUTE_LABEL), "Opus");
    let efforts = options(&mut h, EFFORT_LABEL);
    for e in ["Provider default", "Low", "Medium", "High", "Max"] {
        assert!(efforts.iter().any(|x| x == e), "{e} in {efforts:?}");
    }
    assert!(
        !efforts.iter().any(|x| x == "Minimal"),
        "Opus declares no Minimal"
    );
    choose(&mut h, EFFORT_LABEL, "High");
    assert_eq!(combo_text(&h, EFFORT_LABEL), "Effort: High");
    assert_eq!(
        combo_text(&h, ROUTE_LABEL),
        "Opus",
        "effort never changes the route"
    );
    assert!(has_text(&h, "Opus · High · available:"));
    // The sidebar shows the same conversation's selection.
    click_nav(&mut h, "Settings");
    click(&mut h, Role::Button, "Workspace coordinator");
    assert_eq!(combo_text(&h, ROUTE_LABEL), "Opus");
    assert_eq!(combo_text(&h, EFFORT_LABEL), "Effort: High");
    // An unpinned chat's selection is restored after a restart.
    h.state_mut().on_exit();
    drop(h);
    drop(engine);
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click_nav(&mut h, "Chat");
    assert_eq!(combo_text(&h, ROUTE_LABEL), "Opus");
    assert_eq!(combo_text(&h, EFFORT_LABEL), "Effort: High");
    // Starting the conversation carries the selection over to it; its first message routes it,
    // and with nothing connected the router refuses (no pin) while the message is still saved.
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .type_text("first");
    settle(&mut h);
    click(&mut h, Role::Button, SEND_LABEL);
    let session = h.state().chat.selected.clone().expect("started");
    assert_eq!(combo_text(&h, ROUTE_LABEL), "Opus");
    assert_eq!(combo_text(&h, EFFORT_LABEL), "Effort: High");
    assert!(
        pin_of(&*engine.backend(), &session).is_none(),
        "nothing eligible: no pin"
    );
    assert!(has_text(
        &h,
        "Opus · High · no eligible model: no provider connection"
    ));
}

#[test]
fn a_pinned_conversation_shows_its_pin_and_changes_through_router_pin() {
    let root = Root::new("routing-pin");
    let engine = start_connected(&root);
    let backend = engine.backend();
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    let session = h.state().chat.selected.clone().unwrap();
    assert!(has_text(
        &h,
        "Tier High · Provider default · available: Opus, Sol"
    ));
    // The first message routes the conversation: its selection becomes its durable pin.
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .type_text("hello");
    settle(&mut h);
    click(&mut h, Role::Button, SEND_LABEL);
    assert_eq!(pin_of(&*backend, &session).unwrap().0, "opus");
    click_nav(&mut h, "Artifacts");
    click_nav(&mut h, "Chat");
    step_until(&mut h, "the pin is shown", |s| {
        s.chat
            .routing
            .status
            .get(&session)
            .and_then(|st| st.availability.as_ref())
            .is_some_and(|a| a.pin.is_some())
    });
    settle(&mut h);
    assert!(
        has_text(&h, "Pinned: claude-opus-5-5 · Provider default"),
        "{:?}",
        labels(&h, Role::Label)
    );
    // A change of a pinned conversation is the explicit router.pin command.
    choose(&mut h, ROUTE_LABEL, "Sonnet");
    let (model, intent, effort) = pin_of(&*backend, &session).unwrap();
    assert_eq!(model, "sonnet");
    assert!(
        matches!(intent, RouteIntent::Model { .. }),
        "named, never a tier"
    );
    assert_eq!(effort, "Default");
    choose(&mut h, EFFORT_LABEL, "Medium");
    assert_eq!(pin_of(&*backend, &session).unwrap().2, "Medium");
    assert!(has_text(&h, "Pinned: claude-sonnet-5-5 · Medium"));
    // Back to a tier: router selection with pinning.
    choose(&mut h, ROUTE_LABEL, "Tier Low");
    let (model, intent, effort) = pin_of(&*backend, &session).unwrap();
    assert_eq!(
        model, "haiku",
        "the Low cursor's first turn among eligible members"
    );
    assert!(matches!(intent, RouteIntent::Tier { .. }));
    assert_eq!(
        effort, "Medium",
        "effort persists independently of the route"
    );
    // The pin survives a restart and is what the composer shows.
    h.state_mut().on_exit();
    drop(h);
    drop(backend);
    drop(engine);
    let engine = start_connected(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    assert_eq!(h.state().chat.selected.as_deref(), Some(session.as_str()));
    assert_eq!(combo_text(&h, ROUTE_LABEL), "Tier Low");
    assert!(has_text(&h, "Pinned: claude-haiku-5-5 · Medium"));
}

#[test]
fn an_unconfirmed_pin_change_is_confirmed_with_its_frame_at_the_next_launch() {
    let root = Root::new("routing-pin-unknown");
    let engine = start_connected(&root);
    let real = engine.backend();
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    let session = h.state().chat.selected.clone().unwrap();
    let route = ops::command_frame(
        &*real,
        "router.route",
        json!({"sessionId": session, "expectedContextEpoch": "0",
            "intent": {"kind": "Tier", "tier": "High"}, "effort": "Default",
            "requiredCapabilities": []}),
    );
    ops::command_result(real.command("router.route", &route)).unwrap();
    h.state_mut().on_exit();
    drop(h);
    // The change commits but its acknowledgments are lost (including the client's own retries).
    let lossy = LoseAcks::over(real.clone(), "router.pin", 16);
    let mut h = harness(
        shell_with(Ok(lossy.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    choose(&mut h, ROUTE_LABEL, "Haiku");
    assert!(has_text(
        &h,
        "Not yet confirmed whether the model change was saved"
    ));
    let lost = lossy.request_keys();
    assert!(lost.windows(2).all(|w| w[0] == w[1]), "{lost:?}");
    h.state_mut().on_exit();
    drop(h);
    // The next launch confirms it with the identical request, which replays its receipt.
    let honest = LoseAcks::over(real.clone(), "router.pin", 0);
    let mut h = harness(
        shell_with(Ok(honest.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    let keys = honest.request_keys();
    assert!(
        !keys.is_empty() && keys.iter().all(|k| *k == lost[0]),
        "{keys:?} vs {lost:?}"
    );
    assert_eq!(pin_of(&*real, &session).unwrap().0, "haiku");
    assert!(has_text(&h, "Pinned: claude-haiku-5-5"));
}

/// Starts a conversation and saves one message while every availability query is held, so the
/// message is saved before the router has said whether the conversation is pinned.
fn send_before_availability(
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    gate: &Gate,
) -> String {
    gate.close();
    h.get_by_role_and_label(Role::Button, "New Chat").click();
    step_until(h, "the conversation starts", |s| s.chat.selected.is_some());
    let session = h.state().chat.selected.clone().unwrap();
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .type_text("before availability");
    h.step();
    h.get_by_role_and_label(Role::Button, SEND_LABEL).click();
    step_until(h, "the message is saved", |s| {
        s.chat.sending.is_none()
            && s.chat
                .drafts
                .get(&session)
                .is_some_and(|d| d.text.is_empty())
    });
    assert!(
        !h.state().chat.routing.status.contains_key(&session),
        "its availability is still unknown"
    );
    session
}

#[test]
fn a_first_message_saved_before_availability_is_known_is_routed_once_it_is() {
    let root = Root::new("routing-first-late");
    let engine = start_connected(&root);
    let real = engine.backend();
    let gate = Gate::new("router.availability");
    let routes = LoseAcks::over(gate.over(real.clone()), "router.route", 0);
    let mut h = harness(
        shell_with(Ok(routes.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    let session = send_before_availability(&mut h, &gate);
    assert!(
        routes.request_keys().is_empty(),
        "not routed over an unknown pin"
    );
    gate.open();
    settle(&mut h);
    assert_eq!(
        routes.request_keys().len(),
        1,
        "routed once the router answered"
    );
    assert_eq!(pin_of(&*real, &session).unwrap().0, "opus");
    assert!(h.state().chat.routing.first_route.is_empty());
    // A later message of the pinned conversation is not routed again by the composer.
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .type_text("second");
    settle(&mut h);
    click(&mut h, Role::Button, SEND_LABEL);
    assert_eq!(routes.request_keys().len(), 1);
}

#[test]
fn a_first_route_still_waiting_at_close_is_routed_once_at_the_next_launch() {
    let root = Root::new("routing-first-restart");
    let engine = start_connected(&root);
    let real = engine.backend();
    let gate = Gate::new("router.availability");
    let mut h = harness(
        shell_with(Ok(gate.over(real.clone())), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    let session = send_before_availability(&mut h, &gate);
    h.state_mut().on_exit();
    gate.open();
    drop(h);
    assert!(
        pin_of(&*real, &session).is_none(),
        "nothing was routed before the close"
    );
    let routes = LoseAcks::over(real.clone(), "router.route", 0);
    let mut h = harness(
        shell_with(Ok(routes.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    assert_eq!(
        routes.request_keys().len(),
        1,
        "routed exactly once after the relaunch"
    );
    assert_eq!(pin_of(&*real, &session).unwrap().0, "opus");
    assert!(has_text(&h, "Pinned: claude-opus-5-5"));
}

#[test]
fn a_routing_settings_save_refreshes_the_selectors_catalog() {
    use devworks_desktop::memory::Memory;
    let root = Root::new("routing-settings");
    // An unsaved draft of Tier Low's membership, applied when Settings loads.
    let mut memory = Memory::default();
    memory
        .settings
        .drafts
        .insert("routing.tierLow.members".into(), json!(["astra"]));
    memory.save(&root.memory()).unwrap();
    let engine = start_connected(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    choose(&mut h, ROUTE_LABEL, "Tier Low");
    let before = options(&mut h, EFFORT_LABEL);
    assert!(
        !before.iter().any(|e| e == "Minimal"),
        "Haiku and Luna declare no Minimal"
    );
    click_nav(&mut h, "Settings");
    click(&mut h, Role::Button, "Save changes");
    click_nav(&mut h, "Chat");
    // The selectors now offer what the router allocates from: Astra, which declares Minimal.
    let after = options(&mut h, EFFORT_LABEL);
    assert!(after.iter().any(|e| e == "Minimal"), "{after:?}");
}

#[test]
fn returning_to_a_conversation_rereads_a_pin_changed_meanwhile() {
    let root = Root::new("routing-reselect");
    let engine = start_connected(&root);
    let backend = engine.backend();
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    let first = h.state().chat.selected.clone().unwrap();
    let route = ops::command_frame(
        &*backend,
        "router.route",
        json!({"sessionId": first, "expectedContextEpoch": "0",
            "intent": {"kind": "Tier", "tier": "High"}, "effort": "Default",
            "requiredCapabilities": []}),
    );
    ops::command_result(backend.command("router.route", &route)).unwrap();
    click_nav(&mut h, "Artifacts");
    click_nav(&mut h, "Chat");
    step_until(&mut h, "the pin is known", |s| {
        s.chat
            .routing
            .status
            .get(&first)
            .and_then(|st| st.availability.as_ref())
            .is_some_and(|a| a.pin.is_some())
    });
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    // Meanwhile another surface (an agent, the sidebar of another window) repins the first.
    let pin = ops::router_pin(
        &*backend,
        &first,
        1,
        &RouteIntent::Model {
            model_id: Id::new("haiku").unwrap(),
        },
        &devworks_core::protocol::records::EffortName::Default,
    );
    ops::pinned(backend.command("router.pin", &pin)).unwrap();
    click(&mut h, Role::Button, "Chat History");
    click(&mut h, Role::Button, "Conversation 1");
    assert_eq!(h.state().chat.selected.as_deref(), Some(first.as_str()));
    assert!(
        has_text(&h, "Pinned: claude-haiku-5-5"),
        "not the stale Opus pin"
    );
    assert_eq!(combo_text(&h, ROUTE_LABEL), "Haiku");
}

fn route_selector_enabled(h: &egui_kittest::Harness<'_, devworks_desktop::shell::Shell>) -> bool {
    !h.get_by_role_and_label(Role::ComboBox, ROUTE_LABEL)
        .accesskit_node()
        .is_disabled()
}

/// Sends one message in a started conversation whose availability (no pin yet) is known.
fn send_first(h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>) -> String {
    click(h, Role::Button, "New Chat");
    let session = h.state().chat.selected.clone().unwrap();
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .type_text("first");
    h.step();
    h.get_by_role_and_label(Role::Button, SEND_LABEL).click();
    session
}

#[test]
fn the_selection_cannot_change_while_the_first_route_carrying_it_is_saved() {
    let root = Root::new("routing-first-lock");
    let engine = start_connected(&root);
    let real = engine.backend();
    let gate = Gate::new("router.route");
    let mut h = harness(
        shell_with(Ok(gate.over(real.clone())), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    gate.close();
    let session = send_first(&mut h);
    step_until(&mut h, "the first route is in flight", |s| {
        s.chat.sending.is_none() && s.chat.routing.first_in_flight.contains(&session)
    });
    h.run_steps(2);
    assert!(
        !route_selector_enabled(&h),
        "locked while the route carrying Tier High runs"
    );
    gate.open();
    settle(&mut h);
    assert!(route_selector_enabled(&h));
    assert_eq!(pin_of(&*real, &session).unwrap().0, "opus");
    // Changed after the confirmed pin: the explicit router.pin, never dropped.
    choose(&mut h, ROUTE_LABEL, "Sonnet");
    assert_eq!(pin_of(&*real, &session).unwrap().0, "sonnet");
}

#[test]
fn an_unconfirmed_first_route_keeps_the_selection_locked_after_a_relaunch() {
    let root = Root::new("routing-first-lock-restart");
    let engine = start_connected(&root);
    let real = engine.backend();
    // The first route never reaches the runtime; its outcome stays unknown.
    let lossy = LoseAcks::losing_requests(real.clone(), "router.route", 16);
    let mut h = harness(
        shell_with(Ok(lossy.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    let session = send_first(&mut h);
    settle(&mut h);
    assert!(
        !route_selector_enabled(&h),
        "locked while its outcome is unknown"
    );
    h.state_mut().on_exit();
    drop(h);
    // At the next launch the identical request is resubmitted (held here): still locked.
    let lost = lossy.request_keys();
    assert!(
        !lost.is_empty() && lost.iter().all(|k| *k == lost[0]),
        "{lost:?}"
    );
    let gate = Gate::new("router.route");
    gate.close();
    let resubmitted = LoseAcks::over(gate.over(real.clone()), "router.route", 0);
    let mut h = harness(
        shell_with(Ok(resubmitted.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    for _ in 0..20 {
        h.step();
    }
    step_until(&mut h, "the conversation is shown", |s| {
        s.chat.selected.as_deref() == Some(session.as_str())
            && s.chat
                .routing
                .status
                .get(&session)
                .is_some_and(|st| st.availability.is_some())
    });
    h.run_steps(2);
    assert!(
        !route_selector_enabled(&h),
        "still locked after the relaunch"
    );
    gate.open();
    settle(&mut h);
    assert_eq!(pin_of(&*real, &session).unwrap().0, "opus");
    assert!(route_selector_enabled(&h));
    let keys = resubmitted.request_keys();
    assert!(
        !keys.is_empty() && keys.iter().all(|k| *k == lost[0]),
        "the identical request"
    );
}

#[test]
fn the_selectors_gather_every_catalog_page() {
    let root = Root::new("routing-catalog-pages");
    let engine = start(&root);
    // More custom models than one page holds (catalog edits arrive with DW-18; seeded here).
    let db = rusqlite::Connection::open(root.0.join("workspace.sqlite")).unwrap();
    db.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    for i in 0..250 {
        db.execute(
            "INSERT INTO router_models VALUES(?1,?2,'CompatibleChat',?1,1,'[]',0,1,NULL,?3,1,0,NULL)",
            rusqlite::params![format!("custom-{i:03}"), format!("Custom {i:03}"), 1000 + i],
        )
        .unwrap();
    }
    drop(db);
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    let catalog = &h.state().chat.routing.catalog;
    assert_eq!(
        catalog.len(),
        267,
        "every page, including the eight logical aliases"
    );
    assert_eq!(catalog.last().unwrap().id.as_str(), "custom-249");
    assert!(h.state().chat.routing.catalog_load.is_none());
    let routes = options(&mut h, ROUTE_LABEL);
    assert!(
        routes
            .iter()
            .any(|r| r == "Custom 249 (availability unknown)"),
        "the last page reaches the selector"
    );
}

#[test]
fn a_waiting_first_route_of_a_conversation_beyond_the_listed_page_is_still_routed() {
    let root = Root::new("routing-first-unlisted");
    let engine = start_connected(&root);
    let real = engine.backend();
    let gate = Gate::new("router.availability");
    let mut h = harness(
        shell_with(Ok(gate.over(real.clone())), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    let session = send_before_availability(&mut h, &gate);
    h.state_mut().on_exit();
    gate.open();
    drop(h);
    // More conversations start than one listed page holds; the waiting one is not on it.
    for i in 0..ops::PAGE_LIMIT {
        ops::started(real.command(
            "sessions.start",
            &ops::session_start(&*real, &format!("Later {i}")),
        ))
        .unwrap();
    }
    assert!(!listed_sessions(&*real).contains(&session));
    let mut h = harness(
        shell_with(Ok(real.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    assert_eq!(
        pin_of(&*real, &session).unwrap().0,
        "opus",
        "its record is read, then routed"
    );
    assert!(h.state().chat.routing.first_route.is_empty());
}

#[test]
fn a_first_route_refused_while_the_window_closes_is_routed_at_the_next_launch() {
    use devworks_desktop::shell::Lifecycle;
    let root = Root::new("routing-first-closing");
    let engine = start_connected(&root);
    let real = engine.backend();
    // The message's save commits; its reply is held until the window is closing.
    let gate = Gate::holding_replies("sessions.append_message");
    let routes = LoseAcks::over(gate.over(real.clone()), "router.route", 0);
    let mut h = harness(
        shell_with(Ok(routes.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    let session = h.state().chat.selected.clone().unwrap();
    assert!(
        h.state()
            .chat
            .routing
            .status
            .get(&session)
            .and_then(|s| s.availability.as_ref())
            .is_some_and(|a| a.pin.is_none()),
        "known to have no pin"
    );
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .type_text("sent while closing");
    h.step();
    gate.close();
    h.get_by_role_and_label(Role::Button, SEND_LABEL).click();
    step_until(&mut h, "the message is being saved", |s| {
        s.chat.sending.is_some()
    });
    h.state_mut().request_close();
    h.step();
    // The save's success is applied while quiescing; the route it asks for is not admitted.
    gate.open();
    step_until(&mut h, "the window closed", |s| {
        matches!(s.lifecycle(), Lifecycle::Closed)
    });
    assert!(
        h.state().chat.sending.is_none(),
        "the save was applied before closing"
    );
    assert!(
        routes.request_keys().is_empty(),
        "nothing is admitted while closing"
    );
    assert!(
        h.state().chat.routing.first_route.contains(&session),
        "it keeps waiting"
    );
    drop(h);
    let again = LoseAcks::over(real.clone(), "router.route", 0);
    let mut h = harness(
        shell_with(Ok(again.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    assert_eq!(
        again.request_keys().len(),
        1,
        "routed once at the next launch"
    );
    assert_eq!(pin_of(&*real, &session).unwrap().0, "opus");
    assert!(h.state().chat.routing.first_route.is_empty());
}

/// A backend whose calls of held operations wait until the test releases them, so the order in
/// which their replies reach the shell is chosen by the test, not by thread timing. Released
/// calls are counted when they returned; the shell then receives them in that order.
struct Held {
    inner: Arc<dyn Backend>,
    state: std::sync::Mutex<HeldState>,
    changed: std::sync::Condvar,
    /// Forward the runtime's change hints (off: only released replies reach the shell).
    hints: bool,
}
#[derive(Default)]
struct HeldState {
    held: std::collections::BTreeSet<String>,
    /// Per operation: calls arrived, calls released, calls returned.
    counts: std::collections::BTreeMap<String, (u64, u64, u64)>,
}
impl Held {
    fn over(inner: Arc<dyn Backend>, held: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            inner,
            state: std::sync::Mutex::new(HeldState {
                held: held.iter().map(|s| (*s).to_owned()).collect(),
                counts: Default::default(),
            }),
            changed: std::sync::Condvar::new(),
            hints: false,
        })
    }
    /// Holds nothing until [`Self::hold`], and forwards the runtime's change hints.
    fn with_hints(inner: Arc<dyn Backend>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            state: std::sync::Mutex::new(HeldState::default()),
            changed: std::sync::Condvar::new(),
            hints: true,
        })
    }
    fn hold(&self, operation: &str) {
        self.state.lock().unwrap().held.insert(operation.to_owned());
    }
    fn gated<T>(&self, operation: &str, call: impl FnOnce() -> T) -> T {
        let mut st = self.state.lock().unwrap();
        if !st.held.contains(operation) {
            drop(st);
            return call();
        }
        let c = st.counts.entry(operation.to_owned()).or_default();
        let ticket = c.0;
        c.0 += 1;
        self.changed.notify_all();
        while st.held.contains(operation) && st.counts[operation].1 <= ticket {
            st = self.changed.wait(st).unwrap();
        }
        drop(st);
        let out = call();
        let mut st = self.state.lock().unwrap();
        st.counts.entry(operation.to_owned()).or_default().2 += 1;
        self.changed.notify_all();
        out
    }
    fn arrived(&self, operation: &str) -> u64 {
        self.state
            .lock()
            .unwrap()
            .counts
            .get(operation)
            .map_or(0, |c| c.0)
    }
    /// Releases every call of `operation` that has arrived and waits until each returned, then
    /// gives the client's worker a moment to hand its reply over (failure-only bound).
    fn release(&self, operation: &str) {
        let mut st = self.state.lock().unwrap();
        let c = st.counts.entry(operation.to_owned()).or_default();
        c.1 = c.0;
        let target = c.1;
        self.changed.notify_all();
        while st.counts[operation].2 < target {
            st = self.changed.wait(st).unwrap();
        }
        drop(st);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    /// Stops holding every operation.
    fn open_all(&self) {
        self.state.lock().unwrap().held.clear();
        self.changed.notify_all();
    }
}
impl Backend for Held {
    fn query(&self, operation: &str, frame: &[u8]) -> devworks_core::gateway::QueryOutcome {
        self.gated(operation, || self.inner.query(operation, frame))
    }
    fn command(&self, operation: &str, frame: &[u8]) -> devworks_core::gateway::CommandOutcome {
        self.gated(operation, || self.inner.command(operation, frame))
    }
    fn mint_workspace_upload(&self, file: &std::path::Path) -> Result<Id, String> {
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
        if self.hints { self.inner.hints() } else { None }
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
    fn read_stream(&self, stream: &Id, max: u64) -> Result<Vec<u8>, String> {
        self.inner.read_stream(stream, max)
    }
}

/// The reply orders after a relaunch with a first route waiting: which of the availability and
/// sessions-list replies comes first, and in which order the session-record and route replies
/// reach the shell in one frame.
#[derive(Clone, Copy, Debug)]
enum Order {
    /// Availability first (the record is not listed yet: it is read), then one frame applying
    /// the record's reply before the route's (the CI race).
    StatusThenRecordBeforeRoute,
    /// Availability first, then one frame applying the route's reply before the record's.
    StatusThenRouteBeforeRecord,
    /// The sessions list first, then availability.
    ListThenStatus,
}

fn relaunch_in_order(order: Order) {
    let root = Root::new(&format!("routing-order-{order:?}"));
    let engine = start_connected(&root);
    let real = engine.backend();
    let gate = Gate::new("router.availability");
    let mut h = harness(
        shell_with(Ok(gate.over(real.clone())), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    let session = send_before_availability(&mut h, &gate);
    h.state_mut().on_exit();
    gate.open();
    drop(h);

    let held = Held::over(
        real.clone(),
        &[
            "router.availability",
            "sessions.list",
            "sessions.get",
            "router.route",
        ],
    );
    let routes = LoseAcks::over(held.clone(), "router.route", 0);
    let mut h = harness(
        shell_with(Ok(routes.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    let wait = |h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
                what: &str,
                op: &str,
                n: u64| {
        step_until(h, what, |_| held.arrived(op) >= n);
    };
    wait(
        &mut h,
        "the waiting route's availability is asked",
        "router.availability",
        1,
    );
    wait(&mut h, "the conversations are listed", "sessions.list", 1);
    match order {
        Order::StatusThenRecordBeforeRoute | Order::StatusThenRouteBeforeRecord => {
            held.release("router.availability");
            h.step();
            // Not listed yet: its record is read before routing.
            wait(&mut h, "the record is read", "sessions.get", 1);
            assert!(routes.request_keys().is_empty());
            held.release("sessions.list");
            h.step();
            wait(&mut h, "the route is submitted", "router.route", 1);
            let (first, second) = match order {
                Order::StatusThenRecordBeforeRoute => ("sessions.get", "router.route"),
                _ => ("router.route", "sessions.get"),
            };
            held.release(first);
            held.release(second);
            // Both replies are applied in this one frame, in that order.
            h.step();
        }
        Order::ListThenStatus => {
            held.release("sessions.list");
            h.step();
            held.release("router.availability");
            h.step();
            wait(&mut h, "the route is submitted", "router.route", 1);
            held.release("router.route");
            h.step();
        }
    }
    held.open_all();
    settle(&mut h);
    assert_eq!(
        routes.request_keys().len(),
        1,
        "{order:?}: routed exactly once after the relaunch"
    );
    assert_eq!(pin_of(&*real, &session).unwrap().0, "opus");
    assert!(h.state().chat.routing.first_route.is_empty());
    assert!(h.state().chat.routing.first_in_flight.is_empty());
}

#[test]
fn a_waiting_first_route_is_sent_once_in_every_reply_order_after_a_relaunch() {
    for order in [
        Order::StatusThenRecordBeforeRoute,
        Order::StatusThenRouteBeforeRecord,
        Order::ListThenStatus,
    ] {
        relaunch_in_order(order);
    }
}

#[test]
fn refreshes_never_stack_availability_queries_on_a_slow_router() {
    let root = Root::new("routing-slow-availability");
    let engine = start_connected(&root);
    let real = engine.backend();
    let held = Held::with_hints(real.clone());
    let mut h = harness(
        shell_with(Ok(held.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    // The router stops answering availability.
    held.hold("router.availability");
    h.get_by_role_and_label(Role::Button, "New Chat").click();
    step_until(&mut h, "the conversation starts", |s| {
        s.chat.selected.is_some()
    });
    let session = h.state().chat.selected.clone().unwrap();
    step_until(&mut h, "its availability is asked", |_| {
        held.arrived("router.availability") >= 1
    });
    let asked = held.arrived("router.availability");
    // Runtime changes elsewhere wake the shell's refreshes again and again.
    for i in 0..6 {
        ops::started(real.command(
            "sessions.start",
            &ops::session_start(&*real, &format!("Elsewhere {i}")),
        ))
        .unwrap();
        for _ in 0..5 {
            h.step();
        }
    }
    assert_eq!(
        held.arrived("router.availability"),
        asked,
        "the same question is not asked again while it is unanswered"
    );
    // So a worker is still free for the message's save.
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .type_text("while the router is slow");
    h.step();
    h.get_by_role_and_label(Role::Button, SEND_LABEL).click();
    step_until(&mut h, "the message is saved", |s| {
        s.chat.sending.is_none()
            && s.chat
                .drafts
                .get(&session)
                .is_some_and(|d| d.text.is_empty())
    });
    held.open_all();
    settle(&mut h);
    assert_eq!(pin_of(&*real, &session).unwrap().0, "opus");
}

#[test]
fn unavailable_latest_routes_are_offered_shared_and_keep_the_selected_effort() {
    let root = Root::new("routing-latest-unavailable");
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    let choices = options(&mut h, ROUTE_LABEL);
    for (_, label) in devworks_desktop::routing::LATEST_FAMILIES {
        assert!(choices.iter().any(|x| x == label), "{label} in {choices:?}");
    }
    assert!(
        !choices.iter().any(|x| x == "Opus"),
        "unavailable concrete versions are not new choices"
    );
    choose(&mut h, ROUTE_LABEL, "Astra Latest");
    assert_eq!(combo_text(&h, ROUTE_LABEL), "Astra Latest");
    let efforts = options(&mut h, EFFORT_LABEL);
    assert!(efforts.iter().any(|e| e == "Provider default"));
    assert!(
        !efforts.iter().any(|e| e == "High"),
        "no declared current target, no invented effort"
    );
    click_nav(&mut h, "Settings");
    click(&mut h, Role::Button, "Workspace coordinator");
    assert_eq!(combo_text(&h, ROUTE_LABEL), "Astra Latest");
    let key = h.state().chat.draft_key();
    h.state_mut()
        .chat
        .routing
        .selections
        .get_mut(&key)
        .unwrap()
        .effort = devworks_core::protocol::records::EffortName::High;
    settle(&mut h);
    assert_eq!(
        combo_text(&h, EFFORT_LABEL),
        "Effort: High",
        "saved constraint is retained"
    );
}
