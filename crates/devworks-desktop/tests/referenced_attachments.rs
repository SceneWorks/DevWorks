//! Attachments a message references survive removal during its send, a committed transcript
//! reference protects its artifact from stale deletes, previews never block on special files,
//! and "Discard all" resets every editor — over the real runtime, headless.
mod common;

use common::*;
use devworks_core::protocol::DomainErrorCode;
use devworks_core::protocol::provider::{TranscriptBody, UserContent};
use devworks_core::settings::catalog::descriptor;
use devworks_desktop::backend::Backend;
use devworks_desktop::chat::{ATTACH_LABEL, AttachState, SEND_LABEL};
use devworks_desktop::ops;
use devworks_desktop::shell::Shell;
use eframe::egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

fn live_workspace_artifacts(backend: &dyn Backend) -> Vec<String> {
    ops::artifacts(backend.query("artifacts.list", &ops::workspace_artifacts(backend)))
        .unwrap()
        .0
        .into_iter()
        .map(|a| a.name.0)
        .collect()
}

/// Starts a conversation and stores the scripted file as its attachment; returns the session.
fn conversation_with_attachment(h: &mut Harness<'_, Shell>) -> String {
    settle(h);
    click(h, Role::Button, "New Chat");
    click(h, Role::Button, ATTACH_LABEL);
    let a = &h.state().chat.draft().unwrap().attachments[0];
    assert!(matches!(a.state, AttachState::Stored(_)), "{:?}", a.state);
    h.state().chat.selected.clone().unwrap()
}

fn script_with(root: &Root, name: &str) -> Script {
    let script = Script::default();
    script
        .files
        .lock()
        .unwrap()
        .push(vec![root.file(name, name.as_bytes())]);
    script
}

#[test]
fn deleting_an_artifact_referenced_by_an_appended_message_is_refused() {
    let root = Root::new("referenced-delete");
    let engine = start(&root);
    let real = engine.backend();
    let mut h = harness(
        shell(&engine, &root, script_with(&root, "evidence.txt")),
        1440.0,
        900.0,
    );
    conversation_with_attachment(&mut h);
    let AttachState::Stored(artifact) =
        h.state().chat.draft().unwrap().attachments[0].state.clone()
    else {
        unreachable!()
    };
    click(&mut h, Role::Button, SEND_LABEL);
    // The decision an unsent attachment's delete uses (reference revision 1) is now stale.
    match ops::deleted(real.command(
        "artifacts.delete",
        &ops::delete_unsent_attachment(&*real, &artifact),
    )) {
        Err(ops::Failure::Refused(f)) => {
            assert_eq!(f.code, DomainErrorCode::RevisionConflict);
            assert_eq!(f.current_revision.map(|r| r.get()), Some(2));
        }
        other => panic!("{other:?}"),
    }
    let listed = ops::artifacts(real.query("artifacts.list", &ops::workspace_artifacts(&*real)))
        .unwrap()
        .0;
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].reference_count.get(),
        1,
        "the transcript reference is counted"
    );
}

#[test]
fn removing_a_submitted_attachment_during_the_send_keeps_what_the_message_references() {
    let root = Root::new("remove-during-send");
    let engine = start(&root);
    let real = engine.backend();
    let gate = Gate::new("sessions.append_message");
    let mut h = harness(
        shell_with(
            Ok(gate.over(real.clone())),
            &root,
            script_with(&root, "plan.txt"),
            None,
        ),
        1440.0,
        900.0,
    );
    let session = conversation_with_attachment(&mut h);
    gate.close();
    h.get_by_role_and_label(Role::Button, SEND_LABEL).click();
    h.run_steps(3);
    assert!(h.state().chat.sending.is_some());
    h.get_by_role_and_label(Role::Button, "Remove attachment plan.txt")
        .click();
    h.run_steps(2);
    gate.open();
    settle(&mut h);
    // The append committed: its message references the artifact, so it is kept.
    let page = ops::transcript_page(real.query(
        "sessions.transcript",
        &ops::transcript(&*real, &session, None),
    ))
    .unwrap();
    assert!(
        matches!(&page.entries[0].body, TranscriptBody::UserMessage { content }
        if content.iter().any(|c| matches!(c, UserContent::Attachment { .. })))
    );
    assert_eq!(live_workspace_artifacts(&*real), ["plan.txt"]);
    assert!(h.state().chat.draft().unwrap().attachments.is_empty());
    // The shell never tried to delete it (the service would have refused a stale delete).
    assert!(
        !h.state().status().is_some_and(|s| s.contains("deleted")),
        "{:?}",
        h.state().status()
    );
}

#[test]
fn a_submitted_attachment_removed_during_a_send_that_fails_is_deleted() {
    let root = Root::new("remove-failed-send");
    let engine = start(&root);
    let real = engine.backend();
    let gate = Gate::new("sessions.append_message");
    let mut h = harness(
        shell_with(
            Ok(gate.over(real.clone())),
            &root,
            script_with(&root, "draft.txt"),
            None,
        ),
        1440.0,
        900.0,
    );
    let session = conversation_with_attachment(&mut h);
    gate.close();
    h.get_by_role_and_label(Role::Button, SEND_LABEL).click();
    h.run_steps(3);
    h.get_by_role_and_label(Role::Button, "Remove attachment draft.txt")
        .click();
    h.run_steps(2);
    // Another surface ends the session first, so the held append is refused.
    let revision = h.state().chat.session(&session).unwrap().revision.get();
    ops::command_result(real.command(
        "sessions.end",
        &ops::command_frame(
            &*real,
            "sessions.end",
            json!({"sessionId": session, "expectedRevision": revision.to_string()}),
        ),
    ))
    .unwrap();
    gate.open();
    settle(&mut h);
    assert!(h.state().chat.notice.is_some(), "the send failed visibly");
    assert!(
        live_workspace_artifacts(&*real).is_empty(),
        "an attachment no committed message references is deleted"
    );
}

#[cfg(unix)]
#[test]
fn a_fifo_with_an_image_name_never_blocks_a_preview_worker() {
    use std::sync::Arc;
    use std::time::Duration;
    let root = Root::new("fifo-preview");
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    let dir = root.dir("special");
    // More FIFOs than request workers: blocked previews would exhaust the pool.
    let fifos: Vec<std::path::PathBuf> =
        (0..4).map(|i| dir.join(format!("pipe-{i}.png"))).collect();
    for f in &fifos {
        assert!(
            std::process::Command::new("mkfifo")
                .arg(f)
                .status()
                .unwrap()
                .success()
        );
        h.input_mut()
            .dropped_files
            .push(Arc::new(Dropped(f.clone())));
    }
    h.step();
    h.step();
    if !h
        .state()
        .client()
        .unwrap()
        .wait_idle(Duration::from_secs(10))
    {
        // Failure-only release of readers blocked on the FIFOs, so the test can end.
        for f in &fifos {
            let _ = std::fs::OpenOptions::new().write(true).open(f);
        }
        panic!("a preview blocked on a FIFO");
    }
    settle(&mut h);
    let draft = h.state().chat.draft().unwrap();
    assert_eq!(draft.attachments.len(), 4);
    for a in &draft.attachments {
        assert!(a.preview.is_none());
        assert!(
            a.preview_note
                .as_deref()
                .is_some_and(|n| n.contains("not a regular file")),
            "{:?}",
            a.preview_note
        );
        assert!(matches!(&a.state, AttachState::Failed(e) if e.contains("regular files")));
    }
}

#[test]
fn discard_all_resets_model_list_editors() {
    let root = Root::new("discard-all");
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    let label = descriptor("routing.tierMax.members").unwrap().label;
    let before = h
        .get_by_role_and_label(Role::TextInput, label)
        .value()
        .unwrap();
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, label)
        .type_text(", extra-model");
    settle(&mut h);
    assert!(has_text(&h, "1 unsaved change"));
    click(&mut h, Role::Button, "Discard all");
    assert!(!has_text(&h, "unsaved change"));
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, label)
            .value()
            .unwrap(),
        before,
        "the editor shows the configured list again"
    );
}
