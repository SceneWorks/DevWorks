//! Lost acknowledgments, stale replies and bounded layouts, over the real runtime, headless:
//! a command whose outcome is unknown is confirmed by resubmitting its identical frame (never
//! a new request key), a reply older than a newer mutation never overwrites it, and every
//! layout keeps its essential controls on screen at the minimum logical window and largest
//! text scale.
mod common;

use common::*;
use devworks_core::protocol::provider::TranscriptBody;
use devworks_desktop::backend::Backend;
use devworks_desktop::chat::{ATTACH_LABEL, AttachState, COMPOSER_LABEL, SEND_LABEL};
use devworks_desktop::ops;
use devworks_desktop::route::Route;
use devworks_desktop::theme;
use eframe::egui::{self, accesskit::Role};
use egui_kittest::kittest::{NodeT, Queryable};
use std::sync::Arc;

fn shell_over(
    backend: Arc<dyn Backend>,
    root: &Root,
    script: Script,
) -> devworks_desktop::shell::Shell {
    shell_with(Ok(backend), root, script, None)
}

fn all_same(keys: &[String]) -> bool {
    keys.windows(2).all(|w| w[0] == w[1])
}

// ---------------------------------------------------------------------------------------------
// Unknown outcomes

#[test]
fn an_unknown_conversation_start_is_confirmed_by_replaying_the_same_request() {
    let root = Root::new("unknown-start");
    let engine = start(&root);
    let real = engine.backend();
    let lossy = LoseAcks::over(real.clone(), "sessions.start", 1);
    let mut h = harness(
        shell_over(lossy.clone(), &root, Script::default()),
        1440.0,
        900.0,
    );
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    let sessions = listed_sessions(&*real);
    assert_eq!(
        sessions.len(),
        1,
        "one conversation, however many admissions"
    );
    assert_eq!(
        h.state().chat.selected.as_deref(),
        Some(sessions[0].as_str())
    );
    let keys = lossy.request_keys();
    assert_eq!(
        keys.len(),
        2,
        "the lost acknowledgment was resubmitted once"
    );
    assert!(
        all_same(&keys),
        "the resubmission reused the request key: {keys:?}"
    );
}

#[test]
fn an_unconfirmed_conversation_start_waits_for_confirm_outcomes_and_never_starts_twice() {
    let root = Root::new("unconfirmed-start");
    let engine = start(&root);
    let real = engine.backend();
    // More losses than automatic resubmissions: the shell must say it is not confirmed.
    let lossy = LoseAcks::over(real.clone(), "sessions.start", 4);
    let mut h = harness(
        shell_over(lossy.clone(), &root, Script::default()),
        1440.0,
        900.0,
    );
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    assert!(h.state().chat.selected.is_none());
    assert!(has_text(
        &h,
        "Not yet confirmed whether the conversation started"
    ));
    assert!(has_text(&h, "1 outcome not confirmed"));
    // A second New Chat does not start another conversation while the first is unknown.
    click(&mut h, Role::Button, "New Chat");
    click(&mut h, Role::Button, "Confirm outcomes");
    let sessions = listed_sessions(&*real);
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        h.state().chat.selected.as_deref(),
        Some(sessions[0].as_str())
    );
    assert!(!has_text(&h, "not confirmed"));
    assert!(all_same(&lossy.request_keys()));
}

#[test]
fn an_unknown_message_is_saved_exactly_once_and_its_draft_cleared() {
    let root = Root::new("unknown-append");
    let engine = start(&root);
    let real = engine.backend();
    let lossy = LoseAcks::over(real.clone(), "sessions.append_message", 4);
    let mut h = harness(
        shell_over(lossy.clone(), &root, Script::default()),
        1440.0,
        900.0,
    );
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    let session = h.state().chat.selected.clone().unwrap();
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .type_text("only once");
    settle(&mut h);
    click(&mut h, Role::Button, SEND_LABEL);
    assert!(has_text(
        &h,
        "Not yet confirmed whether your message was saved"
    ));
    assert!(
        h.get_by_role_and_label(Role::Button, SEND_LABEL)
            .accesskit_node()
            .is_disabled(),
        "no second send while the first is unknown"
    );
    click(&mut h, Role::Button, "Confirm outcomes");
    let page = ops::transcript_page(real.query(
        "sessions.transcript",
        &ops::transcript(&*real, &session, None),
    ))
    .unwrap();
    assert_eq!(page.entries.len(), 1, "saved once");
    assert!(matches!(
        page.entries[0].body,
        TranscriptBody::UserMessage { .. }
    ));
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
            .value()
            .as_deref(),
        Some(""),
        "the confirmed message leaves the draft"
    );
    assert!(all_same(&lossy.request_keys()));
}

#[test]
fn an_unconfirmed_attachment_save_keeps_its_frame_and_is_confirmed_once() {
    let root = Root::new("unknown-attach");
    let engine = start(&root);
    let real = engine.backend();
    let lossy = LoseAcks::over(real.clone(), "artifacts.save", 4);
    let script = Script::default();
    script
        .files
        .lock()
        .unwrap()
        .push(vec![root.file("kept.txt", b"kept")]);
    let mut h = harness(shell_over(lossy.clone(), &root, script), 1440.0, 900.0);
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    click(&mut h, Role::Button, ATTACH_LABEL);
    let a = &h.state().chat.draft().unwrap().attachments[0];
    assert!(
        matches!(a.state, AttachState::Unconfirmed(_)),
        "{:?}",
        a.state
    );
    assert!(
        h.get_by_role_and_label(Role::Button, SEND_LABEL)
            .accesskit_node()
            .is_disabled()
    );
    click(&mut h, Role::Button, "Confirm upload of kept.txt");
    let a = &h.state().chat.draft().unwrap().attachments[0];
    assert!(matches!(a.state, AttachState::Stored(_)), "{:?}", a.state);
    let listed = ops::artifacts(real.query("artifacts.list", &ops::workspace_artifacts(&*real)))
        .unwrap()
        .0;
    assert_eq!(listed.len(), 1, "stored once");
    assert!(all_same(&lossy.request_keys()));
}

#[test]
fn an_unknown_settings_save_is_confirmed_without_a_conflict() {
    let root = Root::new("unknown-settings");
    let engine = start(&root);
    let real = engine.backend();
    let lossy = LoseAcks::over(real.clone(), "settings.save_revision", 1);
    let script = Script::default();
    script.folders.lock().unwrap().push(root.dir("repos"));
    let mut h = harness(shell_over(lossy.clone(), &root, script), 1440.0, 900.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    click(
        &mut h,
        Role::Button,
        "Choose directory for Repository base directory",
    );
    click(&mut h, Role::Button, "Save changes");
    assert!(
        has_text(&h, "Settings saved."),
        "replayed, not refused as a conflict"
    );
    assert!(has_text(&h, "Revision 2"));
    assert!(!has_text(&h, "unsaved change"));
    assert!(all_same(&lossy.request_keys()));
}

#[test]
fn an_unconfirmed_settings_save_keeps_saving_until_confirm_outcomes_replays_it() {
    let root = Root::new("unconfirmed-settings");
    let engine = start(&root);
    let real = engine.backend();
    // More losses than automatic resubmissions.
    let lossy = LoseAcks::over(real.clone(), "settings.save_revision", 4);
    let script = Script::default();
    script.folders.lock().unwrap().push(root.dir("repos"));
    let mut h = harness(shell_over(lossy.clone(), &root, script), 1440.0, 900.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    click(
        &mut h,
        Role::Button,
        "Choose directory for Repository base directory",
    );
    click(&mut h, Role::Button, "Save changes");
    assert!(has_text(
        &h,
        "Not yet confirmed whether the settings were saved"
    ));
    assert!(
        h.get_by_role_and_label(Role::Button, "Save changes")
            .accesskit_node()
            .is_disabled(),
        "no second save (a new request key) while the first is unknown"
    );
    assert!(has_text(&h, "1 outcome not confirmed"));
    click(&mut h, Role::Button, "Confirm outcomes");
    assert!(has_text(&h, "Settings saved."));
    assert!(has_text(&h, "Revision 2"));
    assert!(!has_text(&h, "unsaved change"));
    assert!(all_same(&lossy.request_keys()));
}

// ---------------------------------------------------------------------------------------------
// Stale replies

#[test]
fn a_session_list_read_before_a_new_conversation_never_removes_it() {
    let root = Root::new("stale-sessions");
    let engine = start(&root);
    let real = engine.backend();
    let gate = Gate::holding_replies("sessions.list").missing_hints();
    gate.close();
    let mut h = harness(
        shell_over(gate.over(real.clone()), &root, Script::default()),
        1440.0,
        900.0,
    );
    // The startup listing has read its snapshot (no conversations) and its reply is held.
    h.step();
    h.step();
    h.get_by_role_and_label(Role::Button, "New Chat").click();
    step_until(&mut h, "the conversation started", |s| {
        s.chat.selected.is_some()
    });
    let started = h.state().chat.selected.clone().unwrap();
    gate.open();
    settle(&mut h);
    assert_eq!(h.state().chat.selected.as_deref(), Some(started.as_str()));
    assert!(h.state().chat.session(&started).is_some(), "still listed");
    assert_eq!(listed_sessions(&*real), [started]);
}

#[test]
fn an_artifact_list_read_before_an_attachment_was_stored_is_requeried() {
    let root = Root::new("stale-artifacts");
    let engine = start(&root);
    let real = engine.backend();
    // Missed hints: nothing but the shell's own mutation count can repair the stale listing.
    let gate = Gate::holding_replies("artifacts.list").missing_hints();
    let script = Script::default();
    script
        .files
        .lock()
        .unwrap()
        .push(vec![root.file("late.txt", b"late")]);
    let mut h = harness(
        shell_over(gate.over(real.clone()), &root, script),
        1440.0,
        900.0,
    );
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    gate.close();
    nav(&h, "Artifacts").click();
    h.step();
    h.step();
    // While that listing's reply is held, attach a file from the coordinator sidebar.
    h.get_by_role_and_label(Role::Button, "Workspace coordinator")
        .click();
    h.step();
    h.get_by_role_and_label(Role::Button, ATTACH_LABEL).click();
    step_until(&mut h, "the attachment was stored", |s| {
        s.chat.draft().is_some_and(|d| {
            d.attachments
                .iter()
                .any(|a| matches!(a.state, AttachState::Stored(_)))
        })
    });
    gate.open();
    settle(&mut h);
    assert!(
        has_text(&h, "1 of 1 shown"),
        "the stale empty listing was re-queried"
    );
}

#[test]
fn a_validation_report_for_older_drafts_is_not_applied_to_newer_ones() {
    let root = Root::new("stale-validate");
    let engine = start(&root);
    let real = engine.backend();
    let gate = Gate::holding_replies("settings.validate");
    let script = Script::default();
    script.folders.lock().unwrap().push(root.dir("valid"));
    let mut h = harness(
        shell_over(gate.over(real.clone()), &root, script),
        1440.0,
        900.0,
    );
    settle(&mut h);
    click_nav(&mut h, "Settings");
    let label = "Repository base directory";
    click(
        &mut h,
        Role::Button,
        &format!("Choose directory for {label}"),
    );
    gate.close();
    h.get_by_role_and_label(Role::Button, "Validate").click();
    h.step();
    h.step();
    // A newer, invalid edit while the earlier drafts are being validated.
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.step();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.get_by_role_and_label(Role::TextInput, label)
        .type_text("relative/path");
    h.step();
    h.step();
    gate.open();
    settle(&mut h);
    assert!(
        has_text(&h, "Use an absolute path or one starting with ~/"),
        "the newer draft keeps its own finding"
    );
    assert!(has_text(&h, "Values changed while validating"));
    assert!(
        h.get_by_role_and_label(Role::Button, "Save changes")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn a_source_explanation_read_before_a_save_is_explained_again_at_the_new_revision() {
    let root = Root::new("stale-explain");
    let engine = start(&root);
    let real = engine.backend();
    let gate = Gate::holding_replies("settings.explain_source").missing_hints();
    let script = Script::default();
    script.folders.lock().unwrap().push(root.dir("first"));
    script.folders.lock().unwrap().push(root.dir("second"));
    let mut h = harness(
        shell_over(gate.over(real.clone()), &root, script),
        1440.0,
        900.0,
    );
    settle(&mut h);
    click_nav(&mut h, "Settings");
    let label = "Repository base directory";
    click(
        &mut h,
        Role::Button,
        &format!("Choose directory for {label}"),
    );
    click(&mut h, Role::Button, "Save changes");
    assert!(has_text(&h, "Revision 2"));
    // The explanation reads revision 2; its reply is held while revision 3 is saved.
    gate.close();
    h.get_by_role_and_label(Role::Button, &format!("Explain source of {label}"))
        .click();
    h.step();
    h.step();
    h.get_by_role_and_label(Role::Button, &format!("Choose directory for {label}"))
        .click();
    step_until(&mut h, "the second directory is a draft", |s| {
        s.settings.view.as_ref().is_some_and(|v| v.is_dirty())
    });
    h.step();
    h.get_by_role_and_label(Role::Button, "Save changes")
        .click();
    step_until(&mut h, "the save was read back at revision 3", |s| {
        s.settings.view.as_ref().is_some_and(|v| v.revision == 3) && !s.settings.saving
    });
    gate.open();
    settle(&mut h);
    assert!(
        has_text(&h, "Workspace (revision 3): "),
        "explained at the view's revision"
    );
    assert!(
        !has_text(&h, "Workspace (revision 2): "),
        "the stale chain is never shown"
    );
}

// ---------------------------------------------------------------------------------------------
// Bounded layouts

/// Harness size for the minimum logical window at `scale`.
fn minimum(scale: f32) -> (f32, f32) {
    (theme::MIN_WINDOW[0] * scale, theme::MIN_WINDOW[1] * scale)
}

/// Inside the minimum logical window and above its status bar (where the panels' content
/// ends; anything lower is drawn under the status bar or off screen).
fn in_content(r: egui::Rect) -> bool {
    let content = egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(
            theme::MIN_WINDOW[0],
            theme::MIN_WINDOW[1] - theme::STATUS_BAR,
        ),
    )
    .expand(0.5);
    content.contains_rect(r)
}

const SEND_CAPTION: &str = "Cmd / Ctrl + Enter to send";

#[test]
fn the_composer_stays_bounded_with_a_long_draft_and_many_attachments() {
    let root = Root::new("composer-bounds");
    let engine = start(&root);
    let files: Vec<_> = (0..10)
        .map(|i| root.file(&format!("file-{i}.txt"), b"x"))
        .collect();
    for scale in [1.0, theme::MAX_TEXT_SCALE] {
        let script = Script::default();
        script.files.lock().unwrap().push(files.clone());
        let (w, hgt) = minimum(scale);
        let mut h = harness(shell(&engine, &root, script), w, hgt);
        h.ctx.set_zoom_factor(scale);
        settle(&mut h);
        click(&mut h, Role::Button, "New Chat");
        // An empty composer: the transcript takes the rest, so the composer sits at the bottom.
        let empty = h.get_by_label(SEND_CAPTION).rect();
        assert!(
            in_content(empty),
            "@{scale}: the metadata line at {empty:?}"
        );
        click(&mut h, Role::Button, ATTACH_LABEL);
        h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
            .focus();
        h.step();
        h.event(egui::Event::Paste("a long line of the draft\n".repeat(80)));
        settle(&mut h);
        for label in [SEND_LABEL, ATTACH_LABEL] {
            let r = h.get_by_role_and_label(Role::Button, label).rect();
            assert!(in_content(r), "@{scale}: {label} at {r:?}");
        }
        let send = h.get_by_role_and_label(Role::Button, SEND_LABEL).rect();
        let caption = h.get_by_label(SEND_CAPTION).rect();
        assert!(
            in_content(caption),
            "@{scale}: the metadata line at {caption:?}"
        );
        assert!(caption.top() >= send.bottom() - 1.0);
        // Sized from the composer's measured height, the transcript leaves no gap and no
        // overlap: the bounded composer still ends where the empty one did.
        assert!(
            (caption.bottom() - empty.bottom()).abs() <= 1.0,
            "@{scale}: the composer ends at {} (empty: {})",
            caption.bottom(),
            empty.bottom()
        );
        // At most two thirds of the conversation (never less than its fixed controls need).
        let (composer, conversation) = (
            h.state().chat.composer_heights[0],
            h.state().chat.conversation_heights[0],
        );
        assert!(
            composer <= (conversation * 2.0 / 3.0).max(110.0) + 0.5,
            "@{scale}: composer height {composer} of {conversation}"
        );
    }
}

#[test]
fn every_route_keeps_its_essential_controls_on_the_minimum_window_at_the_largest_text() {
    let root = Root::new("route-bounds");
    let engine = start(&root);
    for scale in [1.0, 1.5, theme::MAX_TEXT_SCALE] {
        let (w, hgt) = minimum(scale);
        let mut h = harness(shell(&engine, &root, Script::default()), w, hgt);
        // The builder's first frame set the minimum for scale 1; a new scale updates it.
        h.step();
        h.ctx.set_zoom_factor(scale);
        h.step();
        if scale != 1.0 {
            let commands = &h.output().viewport_output[&egui::ViewportId::ROOT].commands;
            assert!(
                commands.contains(&egui::ViewportCommand::MinInnerSize(egui::vec2(w, hgt))),
                "@{scale}: the native minimum keeps the logical minimum: {commands:?}"
            );
        }
        settle(&mut h);
        for r in Route::all() {
            h.state_mut().navigate(r);
            settle(&mut h);
            let mut essential: Vec<(Role, String)> = vec![
                (Role::Button, "Workspace coordinator".into()),
                (Role::Button, "Notifications".into()),
            ];
            if let Some((label, _)) = r.screen().link {
                essential.push((Role::Button, label.into()));
            }
            if r == Route::Chat {
                essential.extend([
                    (Role::Button, "New Chat".into()),
                    (Role::Button, SEND_LABEL.into()),
                    (Role::Button, ATTACH_LABEL.into()),
                    (Role::TextInput, COMPOSER_LABEL.into()),
                ]);
            }
            for (role, label) in &essential {
                let node = h
                    .query_all_by_role_and_label(*role, label)
                    .find(|n| n.rect().left() > theme::NAV - 1.0)
                    .unwrap_or_else(|| panic!("@{scale} {r:?}: {label}"));
                let rect = node.rect();
                assert!(in_content(rect), "@{scale} {r:?}: {label} at {rect:?}");
            }
            let title = match r {
                Route::Settings => "All settings",
                _ => r.screen().title,
            };
            let heading = h
                .query_all_by_label(title)
                .map(|n| n.rect())
                .find(|rect| rect.left() > theme::NAV)
                .unwrap_or_else(|| panic!("@{scale} {r:?}: heading"));
            assert!(
                in_content(heading),
                "@{scale} {r:?}: heading at {heading:?}"
            );
            // The navigation stays reachable: its last item is on screen.
            let settings = nav(&h, "Settings").rect();
            assert!(
                in_content(settings),
                "@{scale} {r:?}: Settings nav at {settings:?}"
            );
            if r != Route::Chat {
                // The coordinator sidebar beside this screen keeps its composer controls.
                click(&mut h, Role::Button, "Workspace coordinator");
                for label in [SEND_LABEL, ATTACH_LABEL] {
                    let rect = h.get_by_role_and_label(Role::Button, label).rect();
                    assert!(
                        in_content(rect),
                        "@{scale} {r:?} sidebar: {label} at {rect:?}"
                    );
                }
                let close = h
                    .get_by_role_and_label(Role::Button, "Close the workspace coordinator sidebar")
                    .rect();
                assert!(
                    in_content(close),
                    "@{scale} {r:?} sidebar: close at {close:?}"
                );
                let heading = h
                    .query_all_by_label(title)
                    .map(|n| n.rect())
                    .find(|rect| rect.left() > theme::NAV)
                    .unwrap_or_else(|| panic!("@{scale} {r:?}: heading beside the sidebar"));
                assert!(
                    in_content(heading),
                    "@{scale} {r:?}: heading beside the sidebar at {heading:?}"
                );
                click(
                    &mut h,
                    Role::Button,
                    "Close the workspace coordinator sidebar",
                );
            }
        }
        // The navigation footer sits below the last item; both overlays fit the window.
        let settings = nav(&h, "Settings").rect();
        let updates = h
            .get_by_role_and_label(Role::Button, "Updates: not checked")
            .rect();
        assert!(
            in_content(updates) && updates.top() >= settings.bottom() - 0.5,
            "@{scale}: the updates footer at {updates:?} (Settings nav at {settings:?})"
        );
        click(&mut h, Role::Button, "Updates: not checked");
        click(&mut h, Role::Button, "Notifications");
        let window = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(theme::MIN_WINDOW[0], theme::MIN_WINDOW[1]),
        )
        .expand(0.5);
        for text in [
            "Update discovery, approval and installation",
            "Notification history, read state and filters",
        ] {
            let r = h.get_by_label_contains(text).rect();
            assert!(window.contains_rect(r), "@{scale}: overlay text at {r:?}");
        }
    }
}

#[test]
fn a_text_scale_beyond_the_largest_supported_is_clamped() {
    let root = Root::new("scale-clamp");
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1100.0);
    h.ctx.set_zoom_factor(4.0);
    settle(&mut h);
    assert_eq!(h.ctx.zoom_factor(), theme::MAX_TEXT_SCALE);
}
