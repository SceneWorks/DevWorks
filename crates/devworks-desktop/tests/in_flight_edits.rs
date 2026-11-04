//! Edits made while a request is in flight are never lost, special files are never read, and
//! a stuck request cannot keep the app from exiting — over the real runtime, headless.
mod common;

use common::*;
use devworks_core::gateway::{CommandOutcome, QueryOutcome};
use devworks_core::protocol::Id;
use devworks_core::protocol::provider::{TranscriptBody, UserContent};
use devworks_core::settings::catalog::descriptor;
use devworks_core::settings::view::Health;
use devworks_desktop::backend::{Backend, Client, Slot, Waker};
use devworks_desktop::chat::{ATTACH_LABEL, AttachState, COMPOSER_LABEL, SEND_LABEL};
use devworks_desktop::ops;
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::Queryable;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

fn composer(h: &egui_kittest::Harness<'_, devworks_desktop::shell::Shell>) -> String {
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .value()
        .unwrap_or_default()
}

#[test]
fn edits_made_while_a_message_is_being_saved_are_kept() {
    let root = Root::new("send-in-flight");
    let engine = start(&root);
    let real = engine.backend();
    let gate = Gate::new("sessions.append_message");
    let later = root.file("later.txt", b"attached while the message was saving");
    let script = Script::default();
    script.files.lock().unwrap().push(vec![later.clone()]);
    let mut h = harness(
        shell_with(Ok(gate.over(real.clone())), &root, script, None),
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
        .type_text("first message");
    h.step();
    gate.close();
    h.get_by_role_and_label(Role::Button, SEND_LABEL).click();
    h.run_steps(3);
    assert!(h.state().chat.sending.is_some(), "the append is in flight");
    // The composer stays usable: keep typing and attach a file meanwhile.
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
        .type_text(" and a follow-up");
    h.step();
    h.get_by_role_and_label(Role::Button, ATTACH_LABEL).click();
    // Let the attachment's save finish while the append is still held.
    for _ in 0..50 {
        h.step();
        if h.state().chat.draft().is_some_and(|d| {
            d.attachments
                .iter()
                .all(|a| matches!(a.state, AttachState::Stored(_)))
                && !d.attachments.is_empty()
        }) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    gate.open();
    settle(&mut h);
    // Only what was submitted was saved and cleared.
    let page = ops::transcript_page(real.query(
        "sessions.transcript",
        &ops::transcript(&*real, &session, None),
    ))
    .unwrap();
    match &page.entries[..] {
        [e] => match &e.body {
            TranscriptBody::UserMessage { content } => assert!(
                matches!(&content[..], [UserContent::Text { text }] if text == "first message"),
                "{content:?}"
            ),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
    assert_eq!(
        composer(&h),
        "and a follow-up",
        "later typing survives the send"
    );
    let draft = h.state().chat.draft().unwrap();
    assert_eq!(
        draft.attachments.len(),
        1,
        "the later attachment survives the send"
    );
    assert_eq!(draft.attachments[0].name, "later.txt");
}

#[test]
fn settings_edits_made_while_a_save_is_pending_stay_unsaved() {
    let root = Root::new("settings-in-flight");
    let engine = start(&root);
    let real = engine.backend();
    let gate = Gate::new("settings.save_revision");
    let first = root.dir("first");
    let second = root.dir("second");
    let script = Script::default();
    script
        .folders
        .lock()
        .unwrap()
        .extend([first.clone(), second.clone()]);
    let mut h = harness(
        shell_with(Ok(gate.over(real.clone())), &root, script, None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    click_nav(&mut h, "Settings");
    let key = "repositories.baseDirectory";
    let label = descriptor(key).unwrap().label;
    let choose = format!("Choose directory for {label}");
    click(&mut h, Role::Button, &choose);
    gate.close();
    h.get_by_role_and_label(Role::Button, "Save changes")
        .click();
    h.run_steps(3);
    assert!(h.state().settings.saving);
    // A newer edit to the same setting while the save is pending.
    h.get_by_role_and_label(Role::Button, &choose).click();
    h.run_steps(2);
    gate.open();
    settle(&mut h);
    let (revision, entries) =
        ops::settings_entries(real.query("settings.read_effective", &ops::settings_read(&*real)))
            .unwrap();
    let saved = entries.iter().find(|e| e.key.0 == key).unwrap();
    assert_eq!(revision, 2);
    assert_eq!(
        saved.value.value.0,
        json!(first.display().to_string()),
        "the submitted value"
    );
    let view = h.state().settings.view.as_ref().unwrap();
    assert_eq!(
        view.revision, 2,
        "the kept edit is a draft against the new revision"
    );
    assert_eq!(
        view.item(key).unwrap().draft,
        Some(json!(second.display().to_string())),
        "the later edit stays unsaved"
    );
    assert!(has_text(&h, "1 unsaved change"));
    assert!(view.conflict.is_none());
    // It saves normally afterwards.
    click(&mut h, Role::Button, "Save changes");
    assert!(has_text(&h, "Revision 3"));
}

#[cfg(unix)]
#[test]
fn a_dropped_fifo_is_refused_without_being_read() {
    let root = Root::new("fifo");
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    let fifo = root.dir("special").join("pipe");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    h.input_mut()
        .dropped_files
        .push(Arc::new(Dropped(fifo.clone())));
    h.step();
    h.step();
    let idle = h
        .state()
        .client()
        .unwrap()
        .wait_idle(Duration::from_secs(10));
    if !idle {
        // Failure-only release of a reader blocked on the FIFO, so the test can end.
        let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
        panic!("storing the FIFO blocked");
    }
    settle(&mut h);
    let a = &h.state().chat.draft().unwrap().attachments[0];
    assert!(
        matches!(&a.state, AttachState::Failed(e) if e.contains("regular files")),
        "{:?}",
        a.state
    );
}

/// A backend whose queries never return until released.
struct Stuck(Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>);
impl Backend for Stuck {
    fn query(&self, _: &str, _: &[u8]) -> QueryOutcome {
        let mut released = self.0.0.lock().unwrap();
        while !*released {
            released = self.0.1.wait(released).unwrap();
        }
        Err(devworks_core::gateway::GatewayFailure::NoUseCase(
            "released".into(),
        ))
    }
    fn command(&self, op: &str, _: &[u8]) -> CommandOutcome {
        Err(devworks_core::gateway::GatewayFailure::NoUseCase(op.into()))
    }
    fn mint_workspace_upload(&self, _: &Path) -> Result<Id, String> {
        Err("unused".into())
    }
    fn mint_project_upload(
        &self,
        _: &devworks_core::protocol::ProjectId,
        _: &Path,
    ) -> Result<Id, String> {
        Err("unused".into())
    }
    fn keep_awake_health(&self) -> Health {
        Health::NotChecked
    }
    fn hints(&self) -> Option<watch::Receiver<u64>> {
        None
    }
    fn set_waker(&self, _: Waker) {}
    fn token(&self, prefix: &str) -> String {
        format!("{prefix}-1")
    }
    fn scratch_dir(&self) -> PathBuf {
        std::env::temp_dir()
    }
    fn read_stream(&self, _: &Id, _: u64) -> Result<Vec<u8>, String> {
        Err("unused".into())
    }
}

#[test]
fn a_stuck_request_cannot_keep_the_client_from_closing() {
    let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let mut client = Client::new(Arc::new(Stuck(release.clone())));
    client.set_join_bound(Duration::from_millis(200));
    client.query(Slot::SessionsList, "stuck", "sessions.list", b"{}".to_vec());
    let started = Instant::now();
    drop(client);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "dropping waited {:?}",
        started.elapsed()
    );
    // Let the detached worker finish.
    *release.0.lock().unwrap() = true;
    release.1.notify_all();
}
