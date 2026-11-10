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

    // Reset saves the role's catalog default text as a new approved revision (it does not
    // assume revision 1 holds the default).
    {
        assert!(
            h.query_by_role_and_label(Role::Button, "Reset to default")
                .is_some()
        );
        let default = iop::role_default_text("worker").expect("worker has a default");
        click(&mut h, Role::Button, "Reset to default");
        let reset = saved + 1;
        wait(&mut h, "role reset", |s| {
            let p = &s.settings.instructions;
            idle(s)
                && matches!(p.roles.get("role-worker"), Some(Ok(r)) if r.revision.get() == reset)
                && matches!(&p.detail.history, Some(Ok(h)) if h.iter().any(|r| {
                    r.revision.get() == reset && iop::record_status(r).active
                }))
                && matches!(&p.detail.text, Some(Ok(t)) if *t == default)
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

#[test]
fn a_skill_lists_its_resource_files_by_path() {
    let root = Root::new("instruction-resources");
    let dir = root.0.join("skills").join("lint");
    std::fs::create_dir_all(dir.join("references")).unwrap();
    std::fs::write(dir.join("SKILL.md"), "Run the linter.").unwrap();
    std::fs::write(dir.join("references").join("setup.md"), "Install it.").unwrap();
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 1400.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    click(&mut h, Role::Button, "Role instructions");
    wait(
        &mut h,
        "skills listed",
        |s| matches!(&s.settings.instructions.skills, Some(Ok(list)) if list.iter().any(|r| r.name.0 == "lint")),
    );
    click(&mut h, Role::Button, "lint");
    // skills.resources at schema revision 3 carries each resource's skill-relative path.
    wait(&mut h, "resources listed", |s| {
        matches!(&s.settings.instructions.detail.resources, Some(Ok(list))
            if list.len() == 1 && list[0].path.0 == "references/setup.md" && !list[0].executable)
    });
    settle(&mut h);
    assert!(has_text(&h, "references/setup.md · text/markdown"));
}
