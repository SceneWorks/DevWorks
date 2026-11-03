//! Chat History date grouping from durable session times (UI-03), attachment previews restored
//! from stored artifacts after a restart, and attachments removed while their save is in flight
//! (UI-09) — over the real runtime, headless.
mod common;

use common::*;
use devworks_core::protocol::records::SessionRecord;
use devworks_desktop::backend::Backend;
use devworks_desktop::chat::{
    self, ATTACH_LABEL, AttachState, Chat, ChatAction, DateGroup, date_group,
};
use devworks_desktop::memory::SidebarTab;
use devworks_desktop::ops;
use eframe::egui::{self, accesskit::Role};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

fn zoned(s: &str) -> jiff::Zoned {
    s.parse().unwrap()
}
fn ms(z: &jiff::Zoned) -> i64 {
    z.timestamp().as_millisecond()
}

#[test]
fn date_groups_follow_the_local_calendar_day() {
    // A fixed offset: no time-zone database is needed on CI.
    let now = zoned("2026-11-03T09:00:00-05:00[-05:00]");
    let at = |s: &str| Some(ms(&zoned(s)));
    assert_eq!(
        date_group(&now, at("2026-11-03T00:05:00-05:00[-05:00]")),
        DateGroup::Today
    );
    // 23:30 the previous evening locally is yesterday although it is "today" in UTC.
    assert_eq!(
        date_group(&now, at("2026-11-02T23:30:00-05:00[-05:00]")),
        DateGroup::Yesterday
    );
    assert_eq!(
        date_group(&now, at("2026-10-28T12:00:00-05:00[-05:00]")),
        DateGroup::Previous7Days
    );
    assert_eq!(
        date_group(&now, at("2026-10-10T12:00:00-05:00[-05:00]")),
        DateGroup::Previous30Days
    );
    assert_eq!(
        date_group(&now, at("2026-08-15T12:00:00-05:00[-05:00]")),
        DateGroup::Month {
            year: 2026,
            month: 8
        }
    );
    assert_eq!(
        DateGroup::Month {
            year: 2025,
            month: 12
        }
        .label(),
        "December 2025"
    );
    assert_eq!(
        date_group(&now, at("2026-11-04T08:00:00-05:00[-05:00]")),
        DateGroup::Today
    );
    assert_eq!(date_group(&now, None), DateGroup::Undated);
}

fn record(id: &str, title: &str, last: Option<i64>) -> SessionRecord {
    let mut v = json!({"id": id, "revision": "1", "title": title, "contextEpoch": "0",
        "closed": false});
    if let Some(t) = last {
        v["createdAtMs"] = json!(t.to_string());
        v["lastActivityMs"] = json!(t.to_string());
    }
    serde_json::from_value(v).unwrap()
}

#[test]
fn chat_history_groups_conversations_by_last_activity_newest_first() {
    let now = jiff::Zoned::now();
    let days = |n: i64| ms(&now.checked_sub(jiff::Span::new().days(n)).unwrap());
    let mut chat = Chat::new();
    chat.tab = SidebarTab::History;
    chat.sessions_loaded = true;
    chat.sessions = vec![
        record("s-old", "Old plan", Some(days(120))),
        record("s-today", "Morning triage", Some(ms(&now))),
        record("s-yesterday", "Release notes", Some(days(1))),
        record("s-week", "Flaky test", Some(days(4))),
    ];
    let mut h = Harness::builder()
        .with_size(egui::vec2(360.0, 700.0))
        .build_ui_state(
            |ui, chat: &mut Chat| {
                let mut actions: Vec<ChatAction> = Vec::new();
                chat::sidebar(ui, chat, Ok(()), &mut actions);
            },
            chat,
        );
    h.run();
    let top = |label: &str| h.get_by_label(label).rect().top();
    let order = [
        top("Today"),
        top("Morning triage"),
        top("Yesterday"),
        top("Release notes"),
        top("Previous 7 days"),
        top("Flaky test"),
    ];
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
    let old = jiff::Timestamp::from_millisecond(days(120))
        .unwrap()
        .to_zoned(now.time_zone().clone());
    let month = DateGroup::Month {
        year: old.year(),
        month: old.month(),
    }
    .label();
    assert!(top(&month) > top("Flaky test") && top("Old plan") > top(&month));
}

#[test]
fn the_runtime_reports_session_times_and_new_conversations_appear_under_today() {
    let root = Root::new("history-today");
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    let s = h.state().chat.selected_session().cloned().unwrap();
    assert!(
        s.created_at_ms.is_some() && s.last_activity_ms.is_some(),
        "{s:?}"
    );
    click(&mut h, Role::Button, "Chat History");
    let today = h.get_by_label("Today").rect();
    assert!(
        h.get_by_role_and_label(Role::Button, "Conversation 1")
            .rect()
            .top()
            > today.top()
    );
}

// ---------------------------------------------------------------------------------------------
// Previews restored from stored artifacts

#[test]
fn a_restored_image_attachment_gets_its_preview_back_from_the_stored_artifact() {
    let root = Root::new("restore-preview");
    let engine = start(&root);
    let picture = root.file("chart.png", &png());
    let script = Script::default();
    script.files.lock().unwrap().push(vec![picture.clone()]);
    {
        let mut h = harness(shell(&engine, &root, script.clone()), 1440.0, 900.0);
        settle(&mut h);
        click(&mut h, Role::Button, ATTACH_LABEL);
        let a = &h.state().chat.draft().unwrap().attachments[0];
        assert!(matches!(a.state, AttachState::Stored(_)) && a.preview.is_some());
        h.state_mut().on_exit();
    }
    // The picked file is gone: the preview can only come from the artifact store.
    std::fs::remove_file(&picture).unwrap();
    let mut h = harness(shell(&engine, &root, script), 1440.0, 900.0);
    settle(&mut h);
    let draft = h.state().chat.draft().expect("the draft is restored");
    let a = &draft.attachments[0];
    assert_eq!(a.name, "chart.png");
    assert!(matches!(a.state, AttachState::Stored(_)));
    assert!(a.path.is_none());
    assert!(a.preview.is_some(), "{:?}", a.preview_note);
    assert_eq!(a.preview.as_ref().unwrap().size(), [2, 2]);
    assert!(h.query_by_label("Preview of chart.png").is_some());
}

// ---------------------------------------------------------------------------------------------
// Removal while a save is in flight

fn workspace_artifacts(backend: &dyn Backend) -> Vec<String> {
    ops::artifacts(backend.query("artifacts.list", &ops::workspace_artifacts(backend)))
        .unwrap()
        .0
        .into_iter()
        .map(|a| a.name.0)
        .collect()
}

#[test]
fn an_attachment_removed_while_it_is_being_stored_does_not_survive() {
    let root = Root::new("remove-in-flight");
    let engine = start(&root);
    let real = engine.backend();
    let gate = Gate::new("artifacts.save");
    let gated = gate.over(real.clone());
    let slow = root.file("slow.txt", b"removed before its save finished");
    let kept = root.file("kept.txt", b"kept");
    let script = Script::default();
    script.files.lock().unwrap().push(vec![kept.clone()]);
    let mut h = harness(
        shell_with(Ok(gated), &root, script.clone(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    // A stored attachment removed before sending is deleted too.
    click(&mut h, Role::Button, ATTACH_LABEL);
    assert_eq!(workspace_artifacts(&*real), ["kept.txt"]);
    click(&mut h, Role::Button, "Remove attachment kept.txt");
    assert!(
        workspace_artifacts(&*real).is_empty(),
        "removed after it was stored"
    );

    // Now the save is held in flight while the user removes the attachment.
    gate.close();
    script.files.lock().unwrap().push(vec![slow.clone()]);
    h.get_by_role_and_label(Role::Button, ATTACH_LABEL).click();
    h.run_steps(3);
    let a = &h.state().chat.draft().unwrap().attachments[0];
    assert!(matches!(a.state, AttachState::Storing), "{:?}", a.state);
    h.get_by_role_and_label(Role::Button, "Remove attachment slow.txt")
        .click();
    h.run_steps(2);
    assert!(h.state().chat.draft().unwrap().attachments.is_empty());
    gate.open();
    settle(&mut h);
    assert!(
        workspace_artifacts(&*real).is_empty(),
        "the save that completed after removal was deleted"
    );
    assert!(has_text(&h, "The removed attachment was deleted."));
}
