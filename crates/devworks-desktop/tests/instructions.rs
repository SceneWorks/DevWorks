//! Settings → Roles and skills (DW-34) over the real runtime, driven headlessly: role drafts
//! and restores, and a new harness skill, all through the registered gateway operations.
mod common;

use common::*;
use devworks_core::ports::storage::{StoredProject, WriteOptions, run_write};
use devworks_core::protocol::{Id, ProjectId};
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
            p.detail.history.result().map(|h| h.map(|h| h.len())),
            p.detail.text.as_ref().map(|t| t.as_ref().map(String::len)),
            p.detail.resources.result().map(|r| r.map(|r| r.len())),
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
        s.settings.instructions.roles.len() == 7
            && s.settings.instructions.skills.result().is_some()
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
        let history = match s.settings.instructions.detail.history.result() {
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
                && matches!(p.detail.history.result(), Some(Ok(h)) if h.iter().any(|r| {
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
            && matches!(s.settings.instructions.skills.result(), Some(Ok(list)) if list.iter().any(|r| {
                r.name.0 == "release-notes"
                    && iop::record_status(r).origin == iop::Origin::Harness
            }))
    });
    settle(&mut h);
    assert!(
        root.0.join("skills").join("release-notes").is_dir(),
        "the harness skill directory is written under the data root"
    );

    // Disable and re-enable it (skills.set_enabled): it leaves agents' effective skills
    // while its history stays, and returns when enabled again.
    let effective = || -> Vec<String> {
        engine
            .host()
            .runtime()
            .effective_skills()
            .unwrap()
            .skills
            .iter()
            .map(|s| s.name.clone())
            .collect()
    };
    click(&mut h, Role::Button, "release-notes");
    wait(&mut h, "skill history", |s| history_latest(s).is_some());
    assert!(effective().contains(&"release-notes".to_owned()));
    click(&mut h, Role::Button, "Disable skill");
    wait(&mut h, "skill disabled", |s| {
        idle(s)
            && matches!(&s.settings.instructions.selected, Some(sel)
                if iop::record_status(&sel.record).disabled)
    });
    settle(&mut h);
    assert!(!effective().contains(&"release-notes".to_owned()));
    assert!(
        h.query_by_role_and_label(Role::Button, "Enable skill")
            .is_some()
    );
    assert!(
        matches!(h.state().settings.instructions.detail.history.result(), Some(Ok(hist)) if !hist.is_empty())
    );
    click(&mut h, Role::Button, "Enable skill");
    wait(&mut h, "skill enabled", |s| {
        idle(s)
            && matches!(&s.settings.instructions.selected, Some(sel)
                if !iop::record_status(&sel.record).disabled)
    });
    settle(&mut h);
    assert!(effective().contains(&"release-notes".to_owned()));
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
        |s| matches!(s.settings.instructions.skills.result(), Some(Ok(list)) if list.iter().any(|r| r.name.0 == "lint")),
    );
    click(&mut h, Role::Button, "lint");
    // skills.resources at schema revision 3 carries each resource's skill-relative path.
    wait(&mut h, "resources listed", |s| {
        matches!(s.settings.instructions.detail.resources.result(), Some(Ok(list))
            if list.len() == 1 && list[0].path.0 == "references/setup.md" && !list[0].executable)
    });
    settle(&mut h);
    assert!(has_text(&h, "references/setup.md · text/markdown"));
}

#[test]
fn a_person_creates_and_edits_a_skill_for_one_project() {
    let _case = native_runtime_case();
    let root = Root::new("instruction-project-skill");
    let engine = start(&root);
    let project = ProjectId(Id::new("skill-project").unwrap());
    let for_write = project.clone();
    run_write(
        &**engine.host().runtime().store(),
        WriteOptions::fixture(),
        move |tx| {
            tx.projects().insert_project(&StoredProject {
                id: for_write,
                name: "Skill project".into(),
                description: String::new(),
                lifecycle: "Active".into(),
                revision: 1,
                scope_revision: 1,
                created_at_ms: 1,
                updated_at_ms: 1,
                archived_at_ms: None,
                deleted_at_ms: None,
                archive_epoch: 0,
                archive_stop_pending: false,
            })
        },
    )
    .unwrap();
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 1400.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    click(&mut h, Role::Button, "Role instructions");
    wait(&mut h, "skills listed and the project known", |s| {
        s.settings.instructions.skills.result().is_some()
            && !s.settings.instructions.projects.is_empty()
    });
    // The scope choice offers the project; choosing it saves the skill into its own store.
    click(&mut h, Role::Button, "New harness skill");
    assert!(h.query_by_label("Skill scope").is_some());
    h.state_mut().settings.instructions.new_skill_project = Some("skill-project".into());
    settle(&mut h);
    replace_text(&mut h, "New skill name", "deploy");
    replace_text(&mut h, "New skill text", "Deploy this project.");
    click(&mut h, Role::Button, "Create skill");
    let project_skill = |s: &Shell| -> bool {
        matches!(s.settings.instructions.skills.result(), Some(Ok(list)) if list.iter().any(|r| {
            r.name.0 == "deploy" && iop::record_project(r).as_deref() == Some("skill-project")
        }))
    };
    wait(&mut h, "project skill created and listed", |s| {
        idle(s) && project_skill(s)
    });
    settle(&mut h);
    assert_eq!(
        std::fs::read(root.0.join("project-skills/skill-project/deploy/SKILL.md")).unwrap(),
        b"Deploy this project."
    );
    assert!(!root.0.join("skills/deploy").exists());
    let names = |project: Option<&ProjectId>| -> Vec<String> {
        engine
            .host()
            .runtime()
            .effective_skills_in(project)
            .unwrap()
            .skills
            .iter()
            .map(|s| s.name.clone())
            .collect()
    };
    assert_eq!(names(Some(&project)), ["deploy"]);
    assert!(names(None).is_empty(), "the workspace is not offered it");
    assert!(has_text(&h, "Project: Skill project"));
    // Editing it again keeps it in the project's store.
    click(&mut h, Role::Button, "deploy");
    wait(&mut h, "skill history and text", |s| {
        history_latest(s).is_some() && matches!(s.settings.instructions.detail.text, Some(Ok(_)))
    });
    let before = history_latest(h.state()).unwrap();
    click(&mut h, Role::Button, "Edit");
    replace_text(&mut h, "Edit deploy text", "Deploy this project twice.");
    click(&mut h, Role::Button, "Save");
    wait(&mut h, "project skill saved again", |s| {
        idle(s) && history_latest(s).is_some_and(|l| l > before)
    });
    settle(&mut h);
    assert_eq!(
        std::fs::read(root.0.join("project-skills/skill-project/deploy/SKILL.md")).unwrap(),
        b"Deploy this project twice."
    );
    assert!(!root.0.join("skills/deploy").exists());
}

#[test]
fn projects_beyond_the_first_page_are_scoped_and_discovered() {
    let _case = native_runtime_case();
    let root = Root::new("instruction-many-projects");
    let engine = start(&root);
    let backend = engine.backend();
    let page = usize::from(devworks_desktop::ops::PAGE_LIMIT);
    for n in 0..page + 3 {
        let made = devworks_desktop::ops::project_changed(backend.command(
            "projects.create",
            &devworks_desktop::ops::project_create(&*backend, &format!("Project {n:03}"), "", &[]),
        ));
        assert!(made.is_ok(), "{n}");
    }
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 1400.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    click(&mut h, Role::Button, "Role instructions");
    wait(&mut h, "every project gathered", |s| {
        s.settings.instructions.skills.result().is_some()
            && s.settings.instructions.projects.len() == page + 3
    });
    // The shell's own list holds one page at most; the panel's choices hold them all.
    assert!(h.state().projects.list.items.len() <= page);
    let shown: Vec<String> = h
        .state()
        .projects
        .list
        .items
        .iter()
        .map(|p| p.id.0.as_str().to_owned())
        .collect();
    let beyond = h
        .state()
        .settings
        .instructions
        .projects
        .iter()
        .map(|(id, _)| id.clone())
        .find(|id| !shown.contains(id))
        .expect("a project beyond the shell's first page is selectable");
    // Its own skill is discovered by a refresh.
    let dir = root.0.join("project-skills").join(&beyond).join("deploy");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), b"Deploy this project.").unwrap();
    click(&mut h, Role::Button, "Refresh roles and skills");
    wait(&mut h, "the later project's skill discovered", |s| {
        matches!(s.settings.instructions.skills.result(), Some(Ok(list)) if list.iter().any(|r| {
            r.name.0 == "deploy" && iop::record_project(r).as_deref() == Some(beyond.as_str())
        }))
    });
}
