//! Native repository navigation. Positive cases use the real indexed Runtime once composed.
mod common;
use common::*;
use devworks_desktop::{chat::COMPOSER_LABEL, route::Route};
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::{NodeT, Queryable};

#[test]
fn native_unavailable_graph_keeps_workspace_draft_and_never_invents_an_index() {
    let root = Root::new("graph-native-unavailable");
    // Offline composition is read-only. Restore a real persisted draft rather than attempting
    // to type into its intentionally disabled composer.
    let mut memory = devworks_desktop::memory::Memory::default();
    memory
        .drafts
        .entry(devworks_desktop::chat::NEW_DRAFT.into())
        .or_default()
        .text = "Keep this unsent workspace draft".into();
    memory.save(&root.memory()).unwrap();
    let mut h = harness(
        shell_with(
            Err("The runtime is unavailable for this test.".into()),
            &root,
            Script::default(),
            None,
        ),
        1500.0,
        1100.0,
    );
    settle(&mut h);
    assert_eq!(
        h.state().route(),
        Route::Chat,
        "workspace chat remains the opening screen"
    );
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
            .accesskit_node()
            .value()
            .as_deref(),
        Some("Keep this unsent workspace draft")
    );
    h.state_mut()
        .open_repository_graph("local-repository", "Local repository");
    settle(&mut h);
    assert_eq!(h.state().route(), Route::Repositories);
    assert!(has_text(&h, "Local repository"));
    assert!(has_text(&h, "offline or closing"));
    assert!(h.state().graph.pin.is_none());
    assert!(h.state().graph.read.as_ref().unwrap().page.is_none());
    assert!(
        h.get_by_role_and_label(Role::Button, "Refresh selected graph view")
            .accesskit_node()
            .is_disabled()
    );
    assert!(
        h.query_by_role_and_label(Role::Button, "Mark untrusted")
            .is_none()
    );
    click(&mut h, Role::Button, "Back to repositories");
    assert!(!h.state().graph.opened);
    click_nav(&mut h, "Chat");
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
            .accesskit_node()
            .value()
            .as_deref(),
        Some("Keep this unsent workspace draft")
    );
}
