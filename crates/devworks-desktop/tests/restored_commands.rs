//! A command in flight at close is confirmed with its original request key at the next launch,
//! whichever screen sent it.
mod common;
use common::*;
use devworks_desktop::route::Route;
use eframe::egui::{Key, Modifiers, accesskit::Role};
use egui_kittest::kittest::Queryable;

fn replace_text(
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    label: &str,
    text: &str,
) {
    h.get_by_role_and_label(Role::TextInput, label)
        .scroll_to_me();
    settle(h);
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.step();
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.get_by_role_and_label(Role::TextInput, label)
        .type_text(text);
    settle(h);
}

#[test]
fn unconfirmed_schemas_configure_and_skills_draft_are_confirmed_at_the_next_launch() {
    let _native_case = native_runtime_case();
    let root = Root::new("restored-commands");
    let path = root.file("source.sqlite", b"");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY)")
        .unwrap();
    let engine = start(&root);
    let real = engine.backend();
    // More losses than automatic resubmissions: each outcome stays unknown at close.
    let configure_lossy = LoseAcks::over(real.clone(), "schemas.configure", 4);
    let lossy = LoseAcks::over(configure_lossy.clone(), "skills.draft", 4);
    let dialogs = Script::default();
    dialogs.files.lock().unwrap().push(vec![path.clone()]);
    let mut h = harness(
        shell_with(Ok(lossy.clone()), &root, dialogs, None),
        1600.0,
        1400.0,
    );
    settle(&mut h);

    h.state_mut().navigate(Route::Databases);
    settle(&mut h);
    click(&mut h, Role::Button, "New database source");
    replace_text(&mut h, "Database source name", "Restored source");
    click(&mut h, Role::Button, "Choose database file");
    click(&mut h, Role::Button, "Save database source");
    step_until(&mut h, "configure sent", |_| {
        configure_lossy.frames.lock().unwrap().len() >= 4
    });

    click_nav(&mut h, "Settings");
    click(&mut h, Role::Button, "Role instructions");
    click(&mut h, Role::Button, "New harness skill");
    replace_text(&mut h, "New skill name", "restored-skill");
    replace_text(&mut h, "New skill text", "# Restored\nBody.");
    // No settle: the unconfirmed command keeps resubmitting, so the shell never goes quiet.
    h.get_by_role_and_label(Role::Button, "Create skill")
        .click();
    step_until(&mut h, "draft sent", |_| {
        lossy.frames.lock().unwrap().len() >= 4
    });
    step_until(&mut h, "both unconfirmed", |s| {
        let ops: Vec<_> = s
            .client()
            .unwrap()
            .unconfirmed()
            .iter()
            .map(|u| u.operation())
            .collect();
        ops.contains(&"schemas.configure") && ops.contains(&"skills.draft")
    });
    let configure_key = configure_lossy.request_keys()[0].clone();
    let draft_key = lossy.request_keys()[0].clone();
    h.state_mut().on_exit();
    drop(h);

    let configure_again = LoseAcks::over(real.clone(), "schemas.configure", 0);
    let again = LoseAcks::over(configure_again.clone(), "skills.draft", 0);
    let mut h = harness(
        shell_with(Ok(again.clone()), &root, Script::default(), None),
        1600.0,
        1400.0,
    );
    settle(&mut h);
    step_until(&mut h, "both confirmed", |s| {
        s.client().unwrap().unresolved().is_empty()
    });
    assert_eq!(configure_again.request_keys(), [configure_key]);
    assert_eq!(again.request_keys(), [draft_key]);
}
