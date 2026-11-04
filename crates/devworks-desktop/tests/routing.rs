//! The composer's route and effort selectors over the real runtime (UI-10, ROUTE-19, ROUTE-20):
//! tiers before named models, a separate effort selector, one selection shared by Home and the
//! sidebar, the selection of an unpinned chat restored after a restart, a pinned conversation
//! changed through `router.pin` (and an unconfirmed change confirmed with its identical frame at
//! the next launch), and truthful availability when no provider is connected. Connections are
//! scripted; no provider is contacted and no inference runs.
mod common;

use common::*;
use devworks_core::ports::router::{ConnectionDirectory, ConnectionState, ConnectionStatus};
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
    fn connections(&self) -> Vec<ConnectionStatus> {
        [
            ("ClaudeSubscriptionMessages", "conn-claude"),
            ("CodexSubscriptionResponses", "conn-codex"),
        ]
        .into_iter()
        .map(|(profile, id)| ConnectionStatus {
            id: Id::new(id).unwrap(),
            profile: profile.into(),
            state: ConnectionState::Connected,
            discovered: None,
        })
        .collect()
    }
}

fn start_connected(root: &Root) -> Engine {
    let mut c = config(&root.0);
    c.connections = Arc::new(Connected);
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
    let engine = start(&root);
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
    // Nothing is connected in this build: the selection's availability says so.
    assert!(has_text(&h, "no eligible model: no provider connection"));
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
    assert!(has_text(&h, "Opus · High · no eligible model"));
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
        has_text(&h, "Pinned: Opus · Provider default"),
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
    assert!(has_text(&h, "Pinned: Sonnet · Medium"));
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
    assert!(has_text(&h, "Pinned: Haiku · Medium"));
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
    assert!(has_text(&h, "Pinned: Haiku"));
}
