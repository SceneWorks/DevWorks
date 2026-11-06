//! The dedicated manager over the native gateway. No inference, network or successful stub.
mod common;
#[path = "conventions/runtime.rs"]
mod runtime;
use common::*;
use devworks_desktop::{conventions_screen::Scope, route::Route};
use eframe::egui::{self, accesskit::Role};
use egui_kittest::kittest::{NodeT, Queryable};
fn replace(
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    label: &str,
    text: &str,
) {
    h.get_by_role_and_label(Role::TextInput, label)
        .scroll_to_me();
    settle(h);
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.step();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    if text.is_empty() {
        h.key_press(egui::Key::Backspace);
    } else {
        h.get_by_role_and_label(Role::TextInput, label)
            .type_text(text);
    }
    settle(h);
}
#[test]
fn native_conventions_empty_listing_keeps_draft_and_separates_knowledge_navigation() {
    let root = Root::new("conventions-unavailable");
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1500.0, 1050.0);
    h.ctx
        .set_missing_glyph_policy(egui::MissingGlyphPolicy::Tofu);
    settle(&mut h);
    nav(&h, "Knowledge").scroll_to_me();
    settle(&mut h);
    click_nav(&mut h, "Knowledge");
    click(&mut h, Role::Button, "Conventions");
    assert_eq!(h.state().route(), Route::Knowledge);
    let state = h
        .state()
        .conventions
        .state()
        .expect("selected convention scope");
    assert!(
        state.available,
        "the actual registered convention owner answered"
    );
    assert!(state.list_error.is_none());
    assert!(state.items.is_empty() && state.hits.is_empty());
    click(&mut h, Role::Button, "New convention");
    replace(&mut h, "Convention title", "Literal title, 日本語");
    replace(&mut h, "Convention slug", "literal-slug");
    replace(
        &mut h,
        "Convention Markdown",
        "# Original\r\ntext preserved",
    );
    let draft = h
        .state()
        .conventions
        .state()
        .unwrap()
        .new_draft
        .as_ref()
        .unwrap()
        .clone();
    assert!(
        !h.get_by_role_and_label(Role::Button, "Create convention")
            .accesskit_node()
            .is_disabled()
    );
    click(&mut h, Role::Button, "Refresh conventions");
    assert_eq!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .new_draft
            .as_ref()
            .unwrap()
            .markdown,
        draft.markdown
    );
    nav(&h, "Chat").scroll_to_me();
    settle(&mut h);
    click_nav(&mut h, "Chat");
    nav(&h, "Knowledge").scroll_to_me();
    settle(&mut h);
    click_nav(&mut h, "Knowledge");
    assert_eq!(h.state().route_memory(Route::Knowledge).tab, 2);
    assert_eq!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .new_draft
            .as_ref()
            .unwrap()
            .title,
        draft.title
    );
    click(&mut h, Role::Button, "Entries");
    assert_eq!(h.state().route_memory(Route::Knowledge).tab, 0);
    click(&mut h, Role::Button, "Conventions");
    assert_eq!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .new_draft
            .as_ref()
            .unwrap()
            .markdown,
        draft.markdown
    );
}
#[test]
fn native_offline_conventions_has_accessible_forms_without_changing_scope_or_chat_draft() {
    let root = Root::new("conventions-offline");
    let mut s = shell_with(
        Err("Store is offline".into()),
        &root,
        Script::default(),
        None,
    );
    s.conventions.scope = Scope::Workspace;
    s.conventions.state_mut().text = "kept search".into();
    let mut h = harness(s, 760.0, 650.0);
    settle(&mut h);
    nav(&h, "Knowledge").scroll_to_me();
    settle(&mut h);
    click_nav(&mut h, "Knowledge");
    click(&mut h, Role::Button, "Conventions");
    click(&mut h, Role::Button, "New convention");
    replace(&mut h, "Convention title", "Offline title");
    assert!(
        h.get_by_role_and_label(Role::Button, "Create convention")
            .accesskit_node()
            .is_disabled()
    );
    nav(&h, "Chat").scroll_to_me();
    settle(&mut h);
    click_nav(&mut h, "Chat");
    nav(&h, "Knowledge").scroll_to_me();
    settle(&mut h);
    click_nav(&mut h, "Knowledge");
    assert_eq!(h.state().conventions.scope, Scope::Workspace);
    assert_eq!(h.state().conventions.state().unwrap().text, "kept search");
    assert_eq!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .new_draft
            .as_ref()
            .unwrap()
            .title,
        "Offline title"
    );
}

#[test]
fn native_large_source_editor_keeps_unshown_unicode_and_nul_bytes() {
    use devworks_desktop::conventions_screen::{Draft, Editor};
    let root = Root::new("conventions-large-source");
    let mut shell = shell_with(Err("Offline".into()), &root, Script::default(), None);
    shell.navigate(Route::Knowledge);
    let mut h = harness(shell, 1500.0, 1050.0);
    settle(&mut h);
    click(&mut h, Role::Button, "Conventions");
    click(&mut h, Role::Button, "New convention");
    h.ctx
        .set_missing_glyph_policy(egui::MissingGlyphPolicy::Tofu);
    let mut text = "α\0line\r\n".repeat(2_000_000);
    let mut boundary =
        devworks_core::services::conventions::model::MAX_MARKDOWN_BYTES as usize - 20;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
    text.push_str("\nend λ\0");
    let start = text.len() - "end λ\0".len();
    let expected_prefix = text[..start].to_owned();
    let state = h.state_mut().conventions.state_mut();
    state.new_draft = Some(Draft {
        title: "Large source".into(),
        slug: "large-source".into(),
        markdown: std::sync::Arc::new(text),
        markdown_offset: start,
        ..Draft::default()
    });
    state.editor = Some(Editor::New);
    state.detail_tab = 1;
    settle(&mut h);
    replace(&mut h, "Convention Markdown", "changed Ω\0");
    let draft = h
        .state()
        .conventions
        .state()
        .unwrap()
        .new_draft
        .as_ref()
        .unwrap();
    assert!(draft.markdown.starts_with(&expected_prefix));
    assert_eq!(&draft.markdown[start..], "changed Ω\0");
    assert!(draft.body_complete && draft.markdown_changed);
    assert!(
        h.get_by_role_and_label(Role::Button, "Create convention")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn native_convention_context_help_returns_to_the_same_scope_and_unsaved_source() {
    use devworks_core::protocol::records::HelpReadView;
    let root = Root::new("conventions-context-help");
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1500.0, 1050.0);
    settle(&mut h);
    click_nav(&mut h, "Knowledge");
    click(&mut h, Role::Button, "Conventions");
    click(&mut h, Role::Button, "New convention");
    replace(&mut h, "Convention title", "Kept through Help");
    replace(&mut h, "Convention Markdown", "# Kept draft");
    let scope = h.state().conventions.scope.clone();
    let draft = h
        .state()
        .conventions
        .state()
        .unwrap()
        .new_draft
        .as_ref()
        .unwrap()
        .clone();
    click(&mut h, Role::Button, "Help for current screen");
    step_until(
        &mut h,
        "actual convention context topic",
        |s| matches!(&s.help.read,Some(HelpReadView::Available{topic,..}) if topic.id.as_str()=="conventions"),
    );
    assert!(
        matches!(&h.state().help.read,Some(HelpReadView::Available{passage,..}) if passage.text.0.contains("Conventions and reusable guidance"))
    );
    click(&mut h, Role::Button, "Back to previous screen");
    assert_eq!(h.state().route(), Route::Knowledge);
    assert_eq!(h.state().route_memory(Route::Knowledge).tab, 2);
    assert_eq!(h.state().conventions.scope, scope);
    let kept = h
        .state()
        .conventions
        .state()
        .unwrap()
        .new_draft
        .as_ref()
        .unwrap();
    assert_eq!(kept.markdown, draft.markdown);
    assert_eq!(kept.title, draft.title);
}

#[test]
fn native_embedding_setup_is_navigation_only_and_retains_convention_scope_and_source() {
    let root = Root::new("conventions-embedding-navigation");
    let engine = start(&root);
    let backend = LoseAcks::over(engine.backend(), "router.configure", 0);
    let mut h = harness(
        shell_with(Ok(backend.clone()), &root, Script::default(), None),
        1500.0,
        1050.0,
    );
    settle(&mut h);
    click_nav(&mut h, "Knowledge");
    click(&mut h, Role::Button, "Conventions");
    click(&mut h, Role::Button, "New convention");
    replace(&mut h, "Convention title", "Retained setup draft");
    replace(&mut h, "Convention Markdown", "# Retained setup source");
    let scope = h.state().conventions.scope.clone();
    let source = h
        .state()
        .conventions
        .state()
        .unwrap()
        .new_draft
        .as_ref()
        .unwrap()
        .markdown
        .clone();
    click(&mut h, Role::Button, "Embedding setup");
    assert_eq!(h.state().route(), Route::Settings);
    assert!(h.state().settings.show_connections);
    assert!(h.state().settings.connections.embedding_spaces.open);
    assert!(
        h.query_by_role_and_label(Role::TextInput, "Embedding space name")
            .is_some()
    );
    assert!(
        backend.frames.lock().unwrap().is_empty(),
        "opening setup submits no router configuration or qualification"
    );
    click_nav(&mut h, "Knowledge");
    assert_eq!(h.state().conventions.scope, scope);
    assert_eq!(
        h.state()
            .conventions
            .state()
            .unwrap()
            .new_draft
            .as_ref()
            .unwrap()
            .markdown,
        source
    );
}
