//! All projects and Repositories over the real runtime, headless (DW-11): a project is created,
//! edited, given repositories, archived, restored and deleted through the same commands an
//! agent submits, with the concrete confirmation shown before archive and delete; the
//! navigation selects a project and Repositories follows it; a project's repository base
//! directory override is read and saved at the project scope with its inheritance shown; an
//! archive whose outcome is unknown at close is confirmed at the next launch with its exact
//! frame, archiving once. No inference.
mod common;

use common::*;
use devworks_core::ports::storage::{StoredRepository, WriteOptions, run_write};
use devworks_core::protocol::Id;
use devworks_core::protocol::records::{LifeState, ProjectRecord};
use devworks_desktop::backend::Backend;
use devworks_desktop::ops;
use devworks_desktop::route::Route;
use eframe::egui::{self, accesskit::Role};
use egui_kittest::kittest::{NodeT, Queryable};

fn register(engine: &devworks_desktop::host::Engine, id: &str, name: &str) {
    let r = StoredRepository {
        id: Id::new(id).unwrap(),
        location: format!("/Users/me/src/{name}"),
        name: name.into(),
        remote_url: Some(format!("ssh://git@github.com/acme/{name}.git")),
        primary: true,
        lifecycle: "Active".into(),
        languages: vec!["rust".into()],
        revision: 1,
        registered_at_ms: 1,
    };
    run_write(
        &**engine.host().runtime().store(),
        WriteOptions::default(),
        move |tx| tx.projects().insert_repository(&r),
    )
    .unwrap();
}

fn listed(backend: &dyn Backend, lifecycle: Option<&str>) -> Vec<ProjectRecord> {
    ops::projects(backend.query("projects.list", &ops::projects_list(backend, lifecycle)))
        .unwrap()
        .0
}

fn type_into(
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    role: Role,
    label: &str,
    text: &str,
) {
    h.get_by_role_and_label(role, label).focus();
    h.step();
    h.get_by_role_and_label(role, label).type_text(text);
    h.step();
}

/// Clicks the first node whose label starts with `prefix` (a list row).
fn click_row(h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>, prefix: &str) {
    h.query_all_by_label_contains(prefix)
        .find(|n| {
            n.accesskit_node()
                .label()
                .is_some_and(|l| l.starts_with(prefix))
        })
        .unwrap_or_else(|| panic!("a row {prefix}"))
        .click();
    settle(h);
}

#[test]
fn a_project_is_created_changed_archived_and_deleted_through_the_shared_commands() {
    let root = Root::new("projects-lifecycle");
    let engine = start(&root);
    register(&engine, "repo-a", "engine");
    register(&engine, "repo-b", "tools");
    let backend = engine.backend();
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click_nav(&mut h, "All projects");
    assert!(has_text(&h, "No projects yet"));
    click(&mut h, Role::Button, "New project");
    type_into(&mut h, Role::TextInput, "Name", "Engine work");
    click(&mut h, Role::CheckBox, "engine");
    click(&mut h, Role::Button, "Create");
    let created = listed(&*backend, None);
    assert_eq!(created.len(), 1, "one project");
    let p = &created[0];
    assert_eq!(p.name.0, "Engine work");
    assert_eq!(
        p.repository_ids
            .0
            .iter()
            .map(Id::as_str)
            .collect::<Vec<_>>(),
        ["repo-a"]
    );
    assert!(has_text(&h, "Created Engine work."));
    // The created project is selected in the right inner sidebar.
    assert!(has_text(&h, "Revision 1"));
    // Edit its name.
    click(&mut h, Role::Button, "Edit");
    h.get_by_role_and_label(Role::TextInput, "Project name")
        .focus();
    h.step();
    h.key_press(egui::Key::End);
    h.get_by_role_and_label(Role::TextInput, "Project name")
        .type_text(" 2");
    h.step();
    click(&mut h, Role::Button, "Save");
    let edited = listed(&*backend, None).remove(0);
    assert_eq!(edited.name.0, "Engine work 2");
    assert_eq!(edited.revision.as_str(), "2");
    // Its repositories.
    click(&mut h, Role::Button, "Repositories…");
    click(&mut h, Role::CheckBox, "tools");
    click(&mut h, Role::Button, "Save repositories");
    let changed = listed(&*backend, None).remove(0);
    assert_eq!(
        changed
            .repository_ids
            .0
            .iter()
            .map(Id::as_str)
            .collect::<Vec<_>>(),
        ["repo-a", "repo-b"]
    );
    // Archive shows its concrete consequences first; cancelling changes nothing.
    click(&mut h, Role::Button, "Archive…");
    assert!(has_text(
        &h,
        "stops all of this project's running work immediately"
    ));
    click(&mut h, Role::Button, "Cancel");
    assert!(matches!(
        listed(&*backend, None)[0].lifecycle,
        LifeState::Active
    ));
    click(&mut h, Role::Button, "Archive…");
    click(&mut h, Role::Button, "Archive project");
    assert!(matches!(
        listed(&*backend, None)[0].lifecycle,
        LifeState::Archived
    ));
    assert!(has_text(&h, "is archived"));
    // The Archived tab lists it; restore and archive again, then delete.
    click(&mut h, Role::Button, "Archived");
    click_row(&mut h, "Engine work 2  ·");
    click(&mut h, Role::Button, "Restore");
    assert!(matches!(
        listed(&*backend, None)[0].lifecycle,
        LifeState::Active
    ));
    click(&mut h, Role::Button, "Active");
    click_row(&mut h, "Engine work 2  ·");
    click(&mut h, Role::Button, "Archive…");
    click(&mut h, Role::Button, "Archive project");
    click(&mut h, Role::Button, "Archived");
    click_row(&mut h, "Engine work 2  ·");
    click(&mut h, Role::Button, "Delete…");
    assert!(has_text(&h, "never deletes repository folders"));
    click(&mut h, Role::Button, "Delete project");
    assert!(listed(&*backend, None).is_empty());
    let deleted = listed(&*backend, Some("Deleted"));
    assert_eq!(deleted.len(), 1);
    assert!(deleted[0].repository_ids.0.is_empty());
    // Its repositories stay registered.
    let repos = ops::repositories(backend.query(
        "repositories.list",
        &ops::repositories_list(&*backend, None),
    ))
    .unwrap()
    .0;
    assert_eq!(repos.len(), 2);
    click(&mut h, Role::Button, "Deleted");
    assert!(has_text(&h, "Engine work 2"));
}

#[test]
fn the_selected_project_narrows_repositories_and_its_base_directory_inherits_or_overrides() {
    let root = Root::new("projects-select");
    let engine = start(&root);
    register(&engine, "repo-a", "engine");
    register(&engine, "repo-b", "tools");
    let backend = engine.backend();
    let made = ops::project_changed(backend.command(
        "projects.create",
        &ops::project_create(&*backend, "Engine", "", &["repo-a".into()]),
    ))
    .unwrap();
    let script = Script::default();
    script.folders.lock().unwrap().push(root.dir("clones"));
    let mut h = harness(shell(&engine, &root, script), 1440.0, 900.0);
    settle(&mut h);
    // Without a selection Repositories shows every registration, with remotes redacted.
    click_nav(&mut h, "Repositories");
    assert!(has_text(&h, "Every registered repository"));
    assert!(has_text(&h, "tools  ·"));
    assert!(has_text(&h, "https://github.com/acme/tools.git") && !has_text(&h, "ssh://"));
    // Selecting the project narrows it to its members.
    h.state_mut()
        .select_project(Some(made.id.0.as_str().to_owned()));
    settle(&mut h);
    assert!(has_text(&h, "Repositories of Engine"));
    assert!(has_text(&h, "engine  ·"));
    assert!(!has_text(&h, "tools  ·"));
    // A repository without a published view says so honestly.
    click_row(&mut h, "engine  ·");
    assert!(has_text(&h, "Not indexed yet"));
    // The project's repository base directory inherits until it is overridden.
    h.state_mut().navigate(Route::Portfolio);
    settle(&mut h);
    click_row(&mut h, "Engine  ·");
    assert!(has_text(&h, "Inherited from"));
    click(&mut h, Role::Button, "Choose…");
    click(&mut h, Role::Button, "Save directory");
    let entries = ops::settings_entries(backend.query(
        "settings.read_effective",
        &ops::project_settings_read(
            &*backend,
            made.id.0.as_str(),
            &["repositories.baseDirectory"],
        ),
    ))
    .unwrap()
    .1;
    let chosen = root.dir("clones").display().to_string();
    assert_eq!(entries[0].value.value.0, serde_json::json!(chosen));
    assert!(has_text(&h, "Overridden for this project"));
    // The workspace keeps its own value.
    let workspace = ops::settings_entries(
        backend.query("settings.read_effective", &ops::settings_read(&*backend)),
    )
    .unwrap()
    .1;
    let base = workspace
        .iter()
        .find(|e| e.key.0 == "repositories.baseDirectory")
        .unwrap();
    assert_ne!(base.value.value.0, serde_json::json!(chosen));
    // Back to inheriting.
    click(&mut h, Role::Button, "Use workspace default");
    click(&mut h, Role::Button, "Save directory");
    assert!(has_text(&h, "Inherited from"));
    // The selection survives a relaunch.
    h.state_mut().on_exit();
    drop(h);
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    assert_eq!(h.state().selected_project(), Some(made.id.0.as_str()));
}

#[test]
fn an_unknown_archive_at_close_is_confirmed_at_the_next_launch_and_archives_once() {
    let root = Root::new("projects-unknown");
    let engine = start(&root);
    let real = engine.backend();
    let made = ops::project_changed(real.command(
        "projects.create",
        &ops::project_create(&*real, "Engine", "", &[]),
    ))
    .unwrap();
    // More losses than automatic resubmissions: the outcome stays unknown.
    let lossy = LoseAcks::over(real.clone(), "projects.archive", 4);
    let mut h = harness(
        shell_with(Ok(lossy.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    click_nav(&mut h, "All projects");
    click_row(&mut h, "Engine  ·");
    click(&mut h, Role::Button, "Archive…");
    click(&mut h, Role::Button, "Archive project");
    assert!(has_text(&h, "not yet confirmed whether it was applied"));
    // Whether the refreshed record already shows it archived or not, no second lifecycle
    // command (a new request key) can start while the first is unknown.
    let controls: Vec<bool> = ["Archive…", "Restore", "Delete…"]
        .iter()
        .filter_map(|l| h.query_by_role_and_label(Role::Button, l))
        .map(|n| n.accesskit_node().is_disabled())
        .collect();
    assert!(
        !controls.is_empty() && controls.iter().all(|d| *d),
        "{controls:?}"
    );
    let again = LoseAcks::over(real.clone(), "projects.archive", 0);
    h.state_mut().on_exit();
    drop(h);
    let mut h = harness(
        shell_with(Ok(again.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    let archived = listed(&*real, None);
    assert!(matches!(archived[0].lifecycle, LifeState::Archived));
    assert_eq!(archived[0].revision.as_str(), "2", "archived once");
    assert_eq!(archived[0].id, made.id);
    let mut keys = lossy.request_keys();
    keys.extend(again.request_keys());
    assert!(keys.windows(2).all(|w| w[0] == w[1]), "{keys:?}");
    assert!(
        again.request_keys().len() == 1,
        "confirmed with one resubmission"
    );
}
