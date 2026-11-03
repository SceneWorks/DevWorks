//! Lost acknowledgments, stale replies and bounded layouts, over the real runtime, headless:
//! a command whose outcome is unknown is confirmed by resubmitting its identical frame (never
//! a new request key), a reply older than a newer mutation never overwrites it, and every
//! layout keeps its essential controls on screen at the minimum logical window and largest
//! text scale.
mod common;

use common::*;
use devworks_core::protocol::provider::TranscriptBody;
use devworks_desktop::backend::{Backend, Slot};
use devworks_desktop::chat::{self, ATTACH_LABEL, AttachState, COMPOSER_LABEL, SEND_LABEL};
use devworks_desktop::ops;
use devworks_desktop::route::Route;
use devworks_desktop::shell::Lifecycle;
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
// Attachment and draft lifecycle

fn workspace_artifacts(backend: &dyn Backend) -> Vec<String> {
    ops::artifacts(backend.query("artifacts.list", &ops::workspace_artifacts(backend)))
        .unwrap()
        .0
        .into_iter()
        .map(|a| a.name.0)
        .collect()
}

fn scratch_files(backend: &dyn Backend) -> usize {
    std::fs::read_dir(backend.scratch_dir())
        .map(|d| d.count())
        .unwrap_or(0)
}

#[test]
fn removing_an_unconfirmed_attachment_confirms_it_then_deletes_it_and_its_paste() {
    let root = Root::new("remove-unconfirmed");
    let engine = start(&root);
    let real = engine.backend();
    let lossy = LoseAcks::over(real.clone(), "artifacts.save", 4);
    let mut h = harness(
        shell_over(lossy.clone(), &root, Script::default()),
        1440.0,
        900.0,
    );
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .focus();
    h.step();
    let pasted = egui::ColorImage::new([3, 3], vec![egui::Color32::GREEN; 9]);
    h.event(egui::Event::PasteImage(Arc::new(pasted)));
    settle(&mut h);
    let a = &h.state().chat.draft().unwrap().attachments[0];
    assert!(
        matches!(a.state, AttachState::Unconfirmed(_)),
        "{:?}",
        a.state
    );
    let name = a.name.clone();
    assert_eq!(
        workspace_artifacts(&*real).len(),
        1,
        "the lost acknowledgment's save committed"
    );
    assert_eq!(
        scratch_files(&*real),
        1,
        "the paste is kept until the outcome is known"
    );
    click(&mut h, Role::Button, &format!("Remove attachment {name}"));
    assert!(h.state().chat.draft().unwrap().attachments.is_empty());
    assert!(
        has_text(&h, "1 outcome not confirmed"),
        "the removed save is still tracked"
    );
    click(&mut h, Role::Button, "Confirm outcomes");
    assert!(
        workspace_artifacts(&*real).is_empty(),
        "confirmed, then deleted"
    );
    assert_eq!(
        scratch_files(&*real),
        0,
        "its materialized paste is removed"
    );
    assert!(!has_text(&h, "not confirmed"));
    assert!(all_same(&lossy.request_keys()));
}

#[test]
fn a_session_list_reply_never_rehomes_the_draft_of_a_pending_start() {
    let root = Root::new("pending-start-draft");
    let engine = start(&root);
    let real = engine.backend();
    let older = ops::started(real.command("sessions.start", &ops::session_start(&*real, "Older")))
        .unwrap()
        .id
        .as_str()
        .to_owned();
    let list = Gate::new("sessions.list");
    let begin = Gate::new("sessions.start");
    list.close();
    begin.close();
    let script = Script::default();
    script
        .files
        .lock()
        .unwrap()
        .push(vec![root.file("early.txt", b"early")]);
    let backend = list.over(begin.over(real.clone()));
    let mut h = harness(shell_over(backend, &root, script), 1440.0, 900.0);
    h.step();
    h.step();
    // Attached before any conversation is listed: a start begins for the new draft.
    h.get_by_role_and_label(Role::Button, ATTACH_LABEL).click();
    step_until(&mut h, "a start is pending", |s| s.chat.starting.is_some());
    // The list arrives first.
    list.open();
    step_until(&mut h, "the list arrived", |s| s.chat.sessions_loaded);
    h.step();
    assert!(
        h.state().chat.selected.is_none(),
        "nothing chosen while the start is pending"
    );
    begin.open();
    settle(&mut h);
    let selected = h.state().chat.selected.clone().unwrap();
    assert_ne!(selected, older, "the started conversation is selected");
    let draft = &h.state().chat.drafts[&selected];
    assert_eq!(draft.attachments.len(), 1);
    assert!(matches!(draft.attachments[0].state, AttachState::Stored(_)));
    assert!(
        h.state()
            .chat
            .drafts
            .get(&older)
            .is_none_or(|d| d.attachments.is_empty()),
        "the older conversation never received it"
    );
    let tag = ops::conversation_tag(&selected).unwrap();
    let listed = ops::artifacts(real.query("artifacts.list", &ops::workspace_artifacts(&*real)))
        .unwrap()
        .0;
    assert!(listed[0].tags.as_ref().is_some_and(|t| t.0.contains(&tag)));
}

#[test]
fn a_new_draft_joining_a_conversation_keeps_both_texts() {
    let root = Root::new("adopt-texts");
    let engine = start(&root);
    let real = engine.backend();
    {
        let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
        settle(&mut h);
        click(&mut h, Role::Button, "New Chat");
        h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
            .focus();
        h.step();
        h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
            .type_text("earlier words");
        settle(&mut h);
        h.state_mut().on_exit();
    }
    let list = Gate::new("sessions.list");
    list.close();
    let mut h = harness(
        shell_over(list.over(real.clone()), &root, Script::default()),
        1440.0,
        900.0,
    );
    h.step();
    h.step();
    // Typed into the new draft before the restored conversation is listed and selected.
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .type_text("new words");
    h.step();
    list.open();
    settle(&mut h);
    let text = &h.state().chat.draft().unwrap().text;
    assert!(
        text.contains("earlier words") && text.contains("new words"),
        "{text:?}"
    );
}

#[test]
fn confirming_after_close_keeps_the_unresolved_frames() {
    let root = Root::new("confirm-closed");
    let engine = start(&root);
    let lossy = LoseAcks::over(engine.backend(), "sessions.start", 4);
    let mut h = harness(shell_over(lossy, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    assert_eq!(h.state().unconfirmed_count(), 1);
    h.state_mut().request_close();
    h.state_mut().confirm_outcomes();
    assert_eq!(
        h.state().unconfirmed_count(),
        1,
        "nothing ran, so nothing was dropped"
    );
    assert_eq!(h.state().memory().pending.commands.len(), 1);
}

/// Closes the first shell (persisting its state) and opens a second over `backend`.
fn relaunch(
    h: egui_kittest::Harness<'static, devworks_desktop::shell::Shell>,
    backend: Arc<dyn Backend>,
    root: &Root,
    script: Script,
) -> egui_kittest::Harness<'static, devworks_desktop::shell::Shell> {
    let mut h = h;
    h.state_mut().on_exit();
    drop(h);
    let mut h = harness(shell_over(backend, root, script), 1440.0, 900.0);
    settle(&mut h);
    h
}

#[test]
fn an_unknown_attachment_save_survives_close_and_is_confirmed_at_the_next_launch() {
    let root = Root::new("restart-attach");
    let engine = start(&root);
    let real = engine.backend();
    let lossy = LoseAcks::over(real.clone(), "artifacts.save", 4);
    let script = Script::default();
    script
        .files
        .lock()
        .unwrap()
        .push(vec![root.file("later.txt", b"later")]);
    let mut h = harness(shell_over(lossy.clone(), &root, script), 1440.0, 900.0);
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    click(&mut h, Role::Button, ATTACH_LABEL);
    let session = h.state().chat.selected.clone().unwrap();
    let again = LoseAcks::over(real.clone(), "artifacts.save", 0);
    let h = relaunch(h, again.clone(), &root, Script::default());
    let draft = &h.state().chat.drafts[&session];
    assert_eq!(draft.attachments.len(), 1);
    assert!(
        matches!(draft.attachments[0].state, AttachState::Stored(_)),
        "{:?}",
        draft.attachments[0].state
    );
    assert_eq!(workspace_artifacts(&*real), ["later.txt"], "stored once");
    let mut keys = lossy.request_keys();
    keys.extend(again.request_keys());
    assert!(
        keys.len() > 4 && all_same(&keys),
        "the same request key: {keys:?}"
    );
}

#[test]
fn a_removed_unconfirmed_attachment_survives_close_and_is_deleted_at_the_next_launch() {
    let root = Root::new("restart-removed");
    let engine = start(&root);
    let real = engine.backend();
    let lossy = LoseAcks::over(real.clone(), "artifacts.save", 4);
    let script = Script::default();
    script
        .files
        .lock()
        .unwrap()
        .push(vec![root.file("dropped.txt", b"dropped")]);
    let mut h = harness(shell_over(lossy, &root, script), 1440.0, 900.0);
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    click(&mut h, Role::Button, ATTACH_LABEL);
    click(&mut h, Role::Button, "Remove attachment dropped.txt");
    assert_eq!(workspace_artifacts(&*real), ["dropped.txt"]);
    let h = relaunch(h, real.clone(), &root, Script::default());
    assert!(
        workspace_artifacts(&*real).is_empty(),
        "confirmed, then deleted"
    );
    assert_eq!(h.state().unconfirmed_count(), 0);
}

#[test]
fn an_unknown_message_survives_close_and_is_saved_once_at_the_next_launch() {
    let root = Root::new("restart-append");
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
        .type_text("said once");
    settle(&mut h);
    click(&mut h, Role::Button, SEND_LABEL);
    assert!(has_text(
        &h,
        "Not yet confirmed whether your message was saved"
    ));
    let again = LoseAcks::over(real.clone(), "sessions.append_message", 0);
    let h = relaunch(h, again.clone(), &root, Script::default());
    let page = ops::transcript_page(real.query(
        "sessions.transcript",
        &ops::transcript(&*real, &session, None),
    ))
    .unwrap();
    assert_eq!(page.entries.len(), 1, "saved once");
    assert_eq!(h.state().chat.selected.as_deref(), Some(session.as_str()));
    assert!(
        h.state()
            .chat
            .drafts
            .get(&session)
            .is_none_or(|d| d.text.is_empty()),
        "the confirmed message left the draft"
    );
    assert!(h.state().chat.sending.is_none());
    let mut keys = lossy.request_keys();
    keys.extend(again.request_keys());
    assert!(all_same(&keys), "{keys:?}");
}

#[test]
fn an_unknown_conversation_start_survives_close_and_is_resolved_at_the_next_launch() {
    let root = Root::new("restart-start");
    let engine = start(&root);
    let real = engine.backend();
    let lossy = LoseAcks::over(real.clone(), "sessions.start", 4);
    let mut h = harness(
        shell_over(lossy.clone(), &root, Script::default()),
        1440.0,
        900.0,
    );
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    assert!(h.state().chat.selected.is_none());
    let again = LoseAcks::over(real.clone(), "sessions.start", 0);
    let h = relaunch(h, again.clone(), &root, Script::default());
    let sessions = listed_sessions(&*real);
    assert_eq!(sessions.len(), 1, "never started twice");
    assert_eq!(
        h.state().chat.selected.as_deref(),
        Some(sessions[0].as_str())
    );
    assert!(h.state().chat.starting.is_none());
    let mut keys = lossy.request_keys();
    keys.extend(again.request_keys());
    assert!(all_same(&keys), "{keys:?}");
}

#[test]
fn an_unknown_settings_save_survives_close_and_is_confirmed_at_the_next_launch() {
    let root = Root::new("restart-settings");
    let engine = start(&root);
    let real = engine.backend();
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
    let again = LoseAcks::over(real.clone(), "settings.save_revision", 0);
    let h = relaunch(h, again.clone(), &root, Script::default());
    assert!(
        has_text(&h, "Revision 2"),
        "saved once, not refused as a conflict"
    );
    assert!(!has_text(&h, "unsaved change"));
    assert!(!h.state().settings.saving);
    let mut keys = lossy.request_keys();
    keys.extend(again.request_keys());
    assert!(all_same(&keys), "{keys:?}");
}

#[test]
fn a_replayed_settings_save_answered_before_the_first_read_still_finishes_saving() {
    let root = Root::new("restart-settings-order");
    let engine = start(&root);
    let real = engine.backend();
    let lossy = LoseAcks::over(real.clone(), "settings.save_revision", 4);
    let script = Script::default();
    script.folders.lock().unwrap().push(root.dir("repos"));
    let mut h = harness(shell_over(lossy, &root, script), 1440.0, 900.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    click(
        &mut h,
        Role::Button,
        "Choose directory for Repository base directory",
    );
    click(&mut h, Role::Button, "Save changes");
    h.state_mut().on_exit();
    drop(h);
    // The launch's settings read is held, so the replayed save answers first and its
    // follow-up read supersedes the launch's: that read builds the view.
    let read = Gate::new("settings.read_effective");
    read.close();
    let mut h = harness(
        shell_over(read.over(real.clone()), &root, Script::default()),
        1440.0,
        900.0,
    );
    let deadline = std::time::Instant::now() + SETTLE_BOUND;
    while h.state().client().unwrap().is_pending(&Slot::SettingsSave) {
        assert!(std::time::Instant::now() < deadline, "the save is answered");
        h.step();
    }
    h.step();
    read.open();
    settle(&mut h);
    assert!(has_text(&h, "Revision 2"));
    assert!(!has_text(&h, "unsaved change"));
    assert!(!h.state().settings.saving, "the save finished");
    assert!(!has_text(&h, "Not yet confirmed"));
}

#[test]
fn an_attachment_waiting_for_a_lost_start_survives_close_and_joins_its_own_conversation() {
    let root = Root::new("restart-waiting");
    let engine = start(&root);
    let real = engine.backend();
    let lossy = LoseAcks::losing_requests(real.clone(), "sessions.start", 4);
    let script = Script::default();
    script
        .files
        .lock()
        .unwrap()
        .push(vec![root.file("waiting.txt", b"waiting")]);
    let mut h = harness(shell_over(lossy, &root, script), 1440.0, 900.0);
    settle(&mut h);
    // No conversation yet: attaching starts one, whose request is lost (nothing committed).
    click(&mut h, Role::Button, ATTACH_LABEL);
    assert!(matches!(
        h.state().chat.draft().unwrap().attachments[0].state,
        AttachState::WaitingForConversation
    ));
    h.state_mut().on_exit();
    drop(h);
    // Another surface starts a conversation meanwhile; the next launch lists it first.
    let older = ops::started(real.command("sessions.start", &ops::session_start(&*real, "Older")))
        .unwrap()
        .id
        .as_str()
        .to_owned();
    let begin = Gate::new("sessions.start");
    begin.close();
    let mut h = harness(
        shell_over(begin.over(real.clone()), &root, Script::default()),
        1440.0,
        900.0,
    );
    step_until(&mut h, "the list arrived", |s| s.chat.sessions_loaded);
    h.step();
    begin.open();
    settle(&mut h);
    let selected = h.state().chat.selected.clone().unwrap();
    assert_ne!(selected, older, "the restored start's own conversation");
    let draft = &h.state().chat.drafts[&selected];
    assert_eq!(draft.attachments.len(), 1);
    assert!(
        matches!(draft.attachments[0].state, AttachState::Stored(_)),
        "{:?}",
        draft.attachments[0].state
    );
    assert!(
        h.state()
            .chat
            .drafts
            .get(&older)
            .is_none_or(|d| d.attachments.is_empty())
    );
    assert_eq!(
        listed_sessions(&*real).len(),
        2,
        "one restored start, one other"
    );
    assert_eq!(workspace_artifacts(&*real), ["waiting.txt"]);
}

#[test]
fn an_attachment_removed_during_an_unconfirmed_send_is_deleted_when_the_send_did_not_commit() {
    let root = Root::new("restart-removed-sending");
    let engine = start(&root);
    let real = engine.backend();
    let lossy = LoseAcks::losing_requests(real.clone(), "sessions.append_message", 4);
    let script = Script::default();
    script
        .files
        .lock()
        .unwrap()
        .push(vec![root.file("sent.txt", b"sent")]);
    let mut h = harness(shell_over(lossy, &root, script), 1440.0, 900.0);
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    let session = h.state().chat.selected.clone().unwrap();
    click(&mut h, Role::Button, ATTACH_LABEL);
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .type_text("with a file");
    settle(&mut h);
    click(&mut h, Role::Button, SEND_LABEL);
    assert!(has_text(
        &h,
        "Not yet confirmed whether your message was saved"
    ));
    click(&mut h, Role::Button, "Remove attachment sent.txt");
    assert_eq!(
        workspace_artifacts(&*real),
        ["sent.txt"],
        "kept while the send is unknown"
    );
    h.state_mut().on_exit();
    drop(h);
    // The conversation moved on elsewhere, so the restored send is refused (never committed).
    let (sessions, _) =
        ops::sessions(real.query("sessions.list", &ops::sessions_list(&*real))).unwrap();
    let revision = sessions[0].revision.get();
    ops::appended(real.command(
        "sessions.append_message",
        &ops::append_message(&*real, &session, revision, "elsewhere", &[]),
    ))
    .unwrap();
    let mut h = harness(
        shell_over(real.clone(), &root, Script::default()),
        1440.0,
        900.0,
    );
    settle(&mut h);
    assert!(
        workspace_artifacts(&*real).is_empty(),
        "the removed attachment of the uncommitted send is deleted"
    );
    assert!(
        h.state().chat.drafts[&session].text.contains("with a file"),
        "the draft is kept"
    );
    assert!(h.state().chat.sending.is_none());
}

#[test]
fn restored_uploads_without_a_pending_start_are_stored_at_the_next_launch() {
    // What a close leaves when its bounded quiesce ended before a start or save replied: the
    // uploads are kept, and nothing else is pending for them.
    use devworks_desktop::memory::{DraftMemory, Memory, PendingUpload};
    let root = Root::new("restart-uploads");
    let engine = start(&root);
    let real = engine.backend();
    let older = ops::started(real.command("sessions.start", &ops::session_start(&*real, "Older")))
        .unwrap()
        .id
        .as_str()
        .to_owned();
    let upload = |name: &str| PendingUpload {
        name: name.into(),
        media_type: "text/plain".into(),
        path: root.file(name, name.as_bytes()),
        owned: false,
    };
    let mut memory = Memory::default();
    for (key, name) in [
        (older.as_str(), "for-older.txt"),
        (chat::NEW_DRAFT, "for-new.txt"),
    ] {
        memory.drafts.insert(
            key.into(),
            DraftMemory {
                uploads: vec![upload(name)],
                ..DraftMemory::default()
            },
        );
    }
    memory.save(&root.memory()).unwrap();
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    let chat = &h.state().chat;
    let stored = |key: &str| {
        chat.drafts[key]
            .attachments
            .iter()
            .all(|a| matches!(a.state, AttachState::Stored(_)))
    };
    assert!(stored(&older), "stored with its conversation");
    let started = chat.selected.clone().unwrap();
    assert_ne!(started, older, "the new draft got its own conversation");
    assert_eq!(chat.drafts[&started].attachments.len(), 1);
    assert!(stored(&started));
    let mut names = workspace_artifacts(&*real);
    names.sort();
    assert_eq!(names, ["for-new.txt", "for-older.txt"]);
}

#[cfg(unix)]
#[test]
fn only_files_the_shell_created_are_ever_removed() {
    let root = Root::new("owned-files");
    let engine = start(&root);
    let real = engine.backend();
    let scratch = real.scratch_dir();
    std::fs::create_dir_all(&scratch).unwrap();
    // 1. The exact `..` path: lexically under the scratch directory, really beside it.
    let beside = scratch.parent().unwrap().join("notes.txt");
    std::fs::write(&beside, b"notes").unwrap();
    let dotted = scratch.join("../notes.txt");
    // 2. A symbolic link in the scratch directory to a user's file.
    let target = root.file("target.txt", b"target");
    let link = scratch.join("link.txt");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    // 3. A user's file inside the scratch directory that the shell did not create.
    let mine = scratch.join("mine.txt");
    std::fs::write(&mine, b"mine").unwrap();
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    for path in [&dotted, &link, &mine] {
        h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
            .focus();
        h.step();
        h.event(egui::Event::Paste(path.display().to_string()));
        settle(&mut h);
    }
    let draft = h.state().chat.draft().unwrap();
    assert_eq!(draft.attachments.len(), 3);
    let states: Vec<_> = draft
        .attachments
        .iter()
        .map(|a| (a.name.as_str(), &a.state, a.owned))
        .collect();
    assert!(
        draft.attachments.iter().all(|a| !a.owned),
        "never the shell's own files: {states:?}"
    );
    // The two regular files are stored; the link is refused (storage fails), and still kept.
    assert!(matches!(states[0].1, AttachState::Stored(_)), "{states:?}");
    assert!(matches!(states[1].1, AttachState::Failed(_)), "{states:?}");
    assert!(matches!(states[2].1, AttachState::Stored(_)), "{states:?}");
    assert!(
        std::fs::symlink_metadata(&link).is_ok(),
        "a failed upload removed nothing"
    );
    // Uploaded, then removed from the draft: the user's files are untouched either way.
    for name in ["notes.txt", "link.txt", "mine.txt"] {
        click(&mut h, Role::Button, &format!("Remove attachment {name}"));
    }
    assert!(
        workspace_artifacts(&*real).is_empty(),
        "their artifacts were deleted"
    );
    assert_eq!(std::fs::read(&beside).unwrap(), b"notes");
    assert!(std::fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()));
    assert_eq!(std::fs::read(&target).unwrap(), b"target");
    assert_eq!(std::fs::read(&mine).unwrap(), b"mine");
}

#[test]
fn a_save_held_past_the_quiesce_bound_is_confirmed_at_the_next_launch() {
    let root = Root::new("held-save");
    let engine = start(&root);
    let real = engine.backend();
    let first = LoseAcks::over(real.clone(), "artifacts.save", 0);
    // The save commits; its reply is held until after the window has closed.
    let gate = Gate::holding_replies("artifacts.save");
    let script = Script::default();
    script
        .files
        .lock()
        .unwrap()
        .push(vec![root.file("held.txt", b"held")]);
    let mut h = harness(
        shell_over(gate.over(first.clone()), &root, script),
        1440.0,
        900.0,
    );
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    let session = h.state().chat.selected.clone().unwrap();
    gate.close();
    h.get_by_role_and_label(Role::Button, ATTACH_LABEL).click();
    step_until(&mut h, "the save was admitted", |s| {
        s.chat.draft().is_some_and(|d| {
            d.attachments
                .iter()
                .any(|a| matches!(a.state, AttachState::Saving(_)))
        })
    });
    h.state_mut().request_close();
    // Quiescing gives up at its bound with the save still in flight.
    step_until(&mut h, "the window closed", |s| {
        matches!(s.lifecycle(), Lifecycle::Closed)
    });
    gate.open();
    drop(h);
    let again = LoseAcks::over(real.clone(), "artifacts.save", 0);
    let mut h = harness(
        shell_over(again.clone(), &root, Script::default()),
        1440.0,
        900.0,
    );
    settle(&mut h);
    let draft = &h.state().chat.drafts[&session];
    assert_eq!(draft.attachments.len(), 1, "kept in its draft");
    assert!(
        matches!(draft.attachments[0].state, AttachState::Stored(_)),
        "{:?}",
        draft.attachments[0].state
    );
    assert_eq!(workspace_artifacts(&*real), ["held.txt"], "stored once");
    let mut keys = first.request_keys();
    keys.extend(again.request_keys());
    assert!(
        keys.len() >= 2 && all_same(&keys),
        "the same request: {keys:?}"
    );
}

#[test]
fn a_save_prepared_after_close_is_kept_as_an_upload_and_stored_afresh() {
    let root = Root::new("prepared-after-close");
    let engine = start(&root);
    let real = engine.backend();
    // Preparing reads the largest-file setting: held, the preparation is still running at close.
    let prepare = Gate::new("settings.read_effective");
    let script = Script::default();
    script
        .files
        .lock()
        .unwrap()
        .push(vec![root.file("late.txt", b"late")]);
    let mut h = harness(
        shell_over(prepare.over(real.clone()), &root, script),
        1440.0,
        900.0,
    );
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    let session = h.state().chat.selected.clone().unwrap();
    prepare.close();
    h.get_by_role_and_label(Role::Button, ATTACH_LABEL).click();
    step_until(&mut h, "the save is being prepared", |s| {
        s.chat.draft().is_some_and(|d| {
            d.attachments
                .iter()
                .any(|a| matches!(a.state, AttachState::Storing))
        })
    });
    h.state_mut().request_close();
    h.step();
    // The preparation answers after the client closed: its frame is never admitted.
    prepare.open();
    step_until(&mut h, "the window closed", |s| {
        matches!(s.lifecycle(), Lifecycle::Closed)
    });
    let memory = h.state().memory();
    let draft = &memory.drafts[&session];
    assert!(
        draft.unconfirmed.is_empty(),
        "never admitted: no frame to confirm"
    );
    assert_eq!(draft.uploads.len(), 1, "prepared afresh at the next launch");
    drop(h);
    let saves = LoseAcks::over(real.clone(), "artifacts.save", 0);
    let mut h = harness(
        shell_over(saves.clone(), &root, Script::default()),
        1440.0,
        900.0,
    );
    settle(&mut h);
    let draft = &h.state().chat.drafts[&session];
    assert!(
        matches!(draft.attachments[0].state, AttachState::Stored(_)),
        "{:?}",
        draft.attachments[0].state
    );
    assert_eq!(workspace_artifacts(&*real), ["late.txt"]);
    assert_eq!(saves.request_keys().len(), 1, "one fresh save");
}

#[test]
fn a_refused_paste_survives_close_as_failed_and_is_stored_on_retry() {
    let root = Root::new("refused-paste-restart");
    let engine = start(&root);
    let real = engine.backend();
    let quota = |revision: &str, bytes: u64| {
        let d = devworks_core::settings::catalog::descriptor(
            devworks_core::settings::catalog::ARTIFACT_QUOTA_KEY,
        )
        .unwrap();
        let body = serde_json::json!({"scope": {"kind": "Workspace"},
            "expectedRevision": revision, "entries": [
            {"key": d.key, "value": {"schema": d.schema_ref(), "value": bytes}}]});
        ops::settings_saved(real.command(
            "settings.save_revision",
            &ops::command_frame(&*real, "settings.save_revision", body),
        ))
        .expect("the quota is saved");
    };
    quota("1", 1);
    let mut h = harness(
        shell_over(real.clone(), &root, Script::default()),
        1440.0,
        900.0,
    );
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    let session = h.state().chat.selected.clone().unwrap();
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .focus();
    h.step();
    let pasted = egui::ColorImage::new([3, 3], vec![egui::Color32::GREEN; 9]);
    h.event(egui::Event::PasteImage(Arc::new(pasted)));
    settle(&mut h);
    let h = relaunch(h, real.clone(), &root, Script::default());
    let a = &h.state().chat.drafts[&session].attachments;
    assert_eq!(a.len(), 1, "the failed paste is still in its draft");
    assert!(
        matches!(a[0].state, AttachState::Failed(_)),
        "{:?}",
        a[0].state
    );
    let (name, path) = (a[0].name.clone(), a[0].path.clone().unwrap());
    assert!(path.is_file(), "with its file");
    let mut h = h;
    quota("2", 0);
    click(&mut h, Role::Button, &format!("Retry upload of {name}"));
    let a = &h.state().chat.drafts[&session].attachments;
    assert!(
        matches!(a[0].state, AttachState::Stored(_)),
        "{:?}",
        a[0].state
    );
    assert!(!path.exists(), "the shell's own file goes once stored");
    assert_eq!(workspace_artifacts(&*real), [name]);
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
