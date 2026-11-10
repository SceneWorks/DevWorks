//! Settings → Roles and skills (DW-34) over the real runtime, driven headlessly: role drafts
//! and restores, and a new harness skill, all through the registered gateway operations.
mod common;

use common::*;
use devworks_desktop::instructions_ops as iop;
use devworks_desktop::shell::Shell;
use eframe::egui::{Key, Modifiers, accesskit::Role};
use egui_kittest::kittest::Queryable;

fn idle(s: &Shell) -> bool {
    s.settings.instructions.pending.is_none()
}
fn history_latest(s: &Shell) -> Option<i64> {
    s.settings.instructions.detail.latest()
}
fn wait(h: &mut egui_kittest::Harness<'_, Shell>, what: &str, done: impl Fn(&Shell) -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !done(h.state()) {
        let p = &h.state().settings.instructions;
        assert!(
            std::time::Instant::now() < deadline,
            "{what} never happened; selected={:?} history={:?} text={:?} resources={:?} \
             pending={:?} message={:?}",
            p.selected
                .as_ref()
                .map(|s| (&s.id, s.record.revision.get())),
            p.detail.history.as_ref().map(|h| h.as_ref().map(Vec::len)),
            p.detail.text.as_ref().map(|t| t.as_ref().map(String::len)),
            p.detail
                .resources
                .as_ref()
                .map(|r| r.as_ref().map(Vec::len)),
            p.pending,
            p.message,
        );
        h.step();
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}
fn replace_text(h: &mut egui_kittest::Harness<'_, Shell>, label: &str, text: &str) {
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.step();
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.key_press(Key::Backspace);
    h.get_by_role_and_label(Role::TextInput, label)
        .type_text(text);
    h.step();
}

#[test]
fn roles_are_drafted_and_reset_and_harness_skills_are_created() {
    let root = Root::new("instructions");
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 1400.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    click(&mut h, Role::Button, "Role instructions");
    assert!(has_text(&h, "Roles and skills"));
    wait(&mut h, "every role read", |s| {
        s.settings.instructions.roles.len() == 7 && s.settings.instructions.skills.is_some()
    });
    for (id, r) in &h.state().settings.instructions.roles {
        assert!(r.is_ok(), "{id}: {r:?}");
    }

    // Edit and save the implementation agent's instructions (roles.draft).
    click(&mut h, Role::Button, "Implementation agent role");
    wait(&mut h, "role history and text", |s| {
        history_latest(s).is_some() && matches!(s.settings.instructions.detail.text, Some(Ok(_)))
    });
    let before = history_latest(h.state()).unwrap();
    click(&mut h, Role::Button, "Edit");
    replace_text(
        &mut h,
        "Edit Implementation agent role text",
        "Prefer small diffs.",
    );
    click(&mut h, Role::Button, "Save");
    wait(&mut h, "role draft saved", |s| {
        idle(s) && history_latest(s).is_some_and(|l| l > before)
    });
    settle(&mut h);
    let msg = h.state().settings.instructions.message.clone();
    assert!(
        matches!(&msg, Some((devworks_desktop::widgets::Tone::Good, t)) if t.contains("next safe step")),
        "{msg:?}"
    );
    step_until(
        &mut h,
        "saved text shown",
        |s| matches!(&s.settings.instructions.detail.text, Some(Ok(t)) if t == "Prefer small diffs."),
    );
    let saved = history_latest(h.state()).unwrap();
    {
        let s = h.state();
        let history = match &s.settings.instructions.detail.history {
            Some(Ok(h)) => h.clone(),
            other => panic!("{other:?}"),
        };
        let top = history.iter().find(|r| r.revision.get() == saved).unwrap();
        assert!(top.approved && iop::record_status(top).active);
    }

    // Reset to the default revision (roles.rollback of revision 1).
    {
        assert!(
            h.query_by_role_and_label(Role::Button, "Reset to default")
                .is_some()
        );
        click(&mut h, Role::Button, "Reset to default");
        wait(&mut h, "role reset", |s| {
            let p = &s.settings.instructions;
            idle(s)
                && matches!(&p.message, Some((_, t)) if t.starts_with("Restored revision 1"))
                && matches!(p.roles.get("role-worker"), Some(Ok(r)) if r.revision.get() == 1)
                && matches!(&p.detail.history, Some(Ok(h)) if h.iter().any(|r| {
                    r.revision.get() == 1 && iop::record_status(r).active
                }))
        });
    }

    // A new harness skill (skills.draft without an instruction ID).
    click(&mut h, Role::Button, "New harness skill");
    replace_text(&mut h, "New skill name", "release-notes");
    replace_text(
        &mut h,
        "New skill text",
        "# Release notes\nSummarize merged changes.",
    );
    click(&mut h, Role::Button, "Create skill");
    wait(&mut h, "skill created and listed", |s| {
        idle(s)
            && matches!(&s.settings.instructions.skills, Some(Ok(list)) if list.iter().any(|r| {
                r.name.0 == "release-notes"
                    && iop::record_status(r).origin == iop::Origin::Harness
            }))
    });
    settle(&mut h);
    assert!(
        root.0.join("skills").join("release-notes").is_dir(),
        "the harness skill directory is written under the data root"
    );
}
