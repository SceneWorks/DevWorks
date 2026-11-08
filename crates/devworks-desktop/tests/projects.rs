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
use devworks_core::protocol::records::{AccessMode, LifeState, ProjectRecord};
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

/// Waits (bounded) until the engine ran every pending archival stop (restore and delete wait for
/// it).
fn wait_archival_stop(engine: &devworks_desktop::host::Engine) {
    let deadline = std::time::Instant::now() + SETTLE_BOUND;
    loop {
        let pending = devworks_core::ports::storage::read_snapshot(
            &**engine.host().runtime().store(),
            &Default::default(),
            |s| s.projects().pending_archive_stops(10),
        )
        .unwrap();
        if pending.is_empty() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the archival stop never ran"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
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
    wait_archival_stop(&engine);
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
    wait_archival_stop(&engine);
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
    click(&mut h, Role::Button, "Save project settings");
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
    click(&mut h, Role::Button, "Save project settings");
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

#[test]
fn a_form_opened_before_a_concurrent_change_is_refused_rather_than_overwriting_it() {
    let root = Root::new("projects-concurrent");
    let engine = start(&root);
    register(&engine, "repo-a", "engine");
    register(&engine, "repo-b", "tools");
    let backend = engine.backend();
    let made = ops::project_changed(backend.command(
        "projects.create",
        &ops::project_create(&*backend, "Engine", "first", &["repo-a".into()]),
    ))
    .unwrap();
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click_nav(&mut h, "All projects");
    click_row(&mut h, "Engine  ·");
    // The edit form is opened on revision 1; someone else changes the description meanwhile and
    // the list refreshes.
    click(&mut h, Role::Button, "Edit");
    let changed = ops::project_changed(backend.command(
        "projects.edit",
        &ops::project_edit(&*backend, &made, None, Some("theirs"), None),
    ))
    .unwrap();
    click(&mut h, Role::Button, "Refresh");
    h.get_by_role_and_label(Role::TextInput, "Project name")
        .focus();
    h.step();
    h.key_press(egui::Key::End);
    h.get_by_role_and_label(Role::TextInput, "Project name")
        .type_text(" 2");
    h.step();
    click(&mut h, Role::Button, "Save");
    assert!(has_text(&h, "the project changed meanwhile"));
    let now = listed(&*backend, None).remove(0);
    assert_eq!(now.description.0, "theirs", "the concurrent change is kept");
    assert_eq!(now.name.0, "Engine");
    assert_eq!(now.revision, changed.revision);
    // The same for a membership change opened before a concurrent one.
    click_row(&mut h, "Engine  ·");
    click(&mut h, Role::Button, "Repositories…");
    let _ = ops::project_changed(backend.command(
        "projects.membership",
        &ops::project_membership(&*backend, &now, &["repo-a".into(), "repo-b".into()]),
    ))
    .unwrap();
    click(&mut h, Role::Button, "Refresh");
    click(&mut h, Role::CheckBox, "engine");
    click(&mut h, Role::Button, "Save repositories");
    assert!(has_text(&h, "the project changed meanwhile"));
    let members: Vec<String> = listed(&*backend, None)
        .remove(0)
        .repository_ids
        .0
        .iter()
        .map(|r| r.as_str().to_owned())
        .collect();
    assert_eq!(
        members,
        ["repo-a", "repo-b"],
        "the concurrent membership is kept"
    );
}

fn publish_text(engine: &devworks_desktop::host::Engine, readme: bool) {
    use devworks_core::ports::storage::{StoredIndexView, StoredRepositoryText, TextKind};
    run_write(
        &**engine.host().runtime().store(),
        WriteOptions::default(),
        move |tx| {
            let view = Id::new("view-1").unwrap();
            if tx.projects().index_view(&view)?.is_none() {
                tx.projects().publish_index_view(&StoredIndexView {
                    view_id: view.clone(),
                    repository_id: Id::new("repo-a").unwrap(),
                    source_revision: "rev-1".into(),
                    coverage: "Complete".into(),
                    published_at_ms: 1,
                })?;
            }
            tx.projects().put_repository_text(&StoredRepositoryText {
                view_id: view,
                repository_id: Id::new("repo-a").unwrap(),
                kind: if readme {
                    TextKind::Readme
                } else {
                    TextKind::Summary
                },
                artifact_id: Id::new(if readme {
                    "artifact-readme"
                } else {
                    "artifact-summary"
                })
                .unwrap(),
                version_id: Id::new("version-1").unwrap(),
                sha256: "b".repeat(64),
                byte_length: 12,
                media_type: "text/markdown".into(),
                coverage: "Complete".into(),
            })
        },
    )
    .unwrap();
}

#[test]
fn an_unavailable_repository_text_can_be_retried_and_refresh_reads_it_again() {
    let root = Root::new("projects-texts");
    let engine = start(&root);
    register(&engine, "repo-a", "engine");
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    click_nav(&mut h, "Repositories");
    click_row(&mut h, "engine  ·");
    assert!(has_text(&h, "Not indexed yet"));
    // Indexing publishes the README; the unavailable state offers a retry.
    publish_text(&engine, true);
    click(&mut h, Role::Button, "Retry README");
    let shown = |h: &egui_kittest::Harness<'_, devworks_desktop::shell::Shell>| {
        h.query_all_by_label_contains("View view-1 · 12 bytes")
            .count()
    };
    assert_eq!(
        shown(&h),
        1,
        "the README is read; the summary is still unavailable"
    );
    // Refresh reads the selected repository's texts again (the summary was published since).
    publish_text(&engine, false);
    click(&mut h, Role::Button, "Refresh");
    assert_eq!(shown(&h), 2);
}

/// A project's browser session that has not launched yet (Starting, no process), as the
/// browser service records one at admission.
fn seed_browser(engine: &devworks_desktop::host::Engine, session: &str, project: &Id) {
    use devworks_core::ports::storage::{BrowserPhase, NewOperation, StoredBrowserSession};
    use devworks_core::protocol::{ActorId, OperationId, ProjectId};
    let store = engine.host().runtime().store().clone();
    let boot = store.info().boot_id.unwrap();
    let (session, project) = (session.to_owned(), project.clone());
    run_write(&*store, WriteOptions::default(), move |tx| {
        let op = OperationId(Id::new(format!("op-{session}")).unwrap());
        tx.operations().record_intent(NewOperation {
            id: op.clone(),
            root_id: None,
            assignment_id: None,
            capability: "browser.create".into(),
            schema_version: 1,
            args_digest: "0".repeat(64),
            payload: serde_json::json!({}),
        })?;
        tx.browser().insert_browser_session(&StoredBrowserSession {
            tested_repository: None,
            source_binding: None,
            id: Id::new(session.clone()).unwrap(),
            revision: 1,
            phase: BrowserPhase::Starting,
            boot_id: boot,
            owner: ActorId(Id::new(devworks_desktop::host::USER_ACTOR).unwrap()),
            owner_kind: "Human".into(),
            agent_session_id: None,
            project_id: Some(ProjectId(project)),
            root_id: None,
            assignment_id: None,
            asset_id: "chrome".into(),
            process_id: None,
            create_operation_id: op,
            uncertain_operation_id: None,
            last_action_operation_id: None,
            close_operation_id: None,
            targets: vec![],
            observation_sequence: 0,
            detail: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        })
    })
    .unwrap();
}

fn browser_phase(
    engine: &devworks_desktop::host::Engine,
    session: &str,
) -> devworks_core::ports::storage::BrowserPhase {
    let id = Id::new(session).unwrap();
    devworks_core::ports::storage::read_snapshot(
        &**engine.host().runtime().store(),
        &Default::default(),
        move |s| s.browser().browser_session(&id),
    )
    .unwrap()
    .unwrap()
    .phase
}

#[test]
fn project_browser_sessions_are_listed_and_closed_from_the_browser_view() {
    let root = Root::new("projects-browser");
    let engine = start(&root);
    let backend = engine.backend();
    let create = |name: &str| {
        ops::project_changed(backend.command(
            "projects.create",
            &ops::project_create(&*backend, name, "", &[]),
        ))
        .unwrap()
        .id
        .0
    };
    let (engine_project, tools) = (create("Engine"), create("Tools"));
    seed_browser(&engine, "bs-engine", &engine_project);
    seed_browser(&engine, "bs-tools", &tools);
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    // Without a selection every project's sessions are listed, each with its project.
    h.state_mut().navigate(Route::Browser);
    settle(&mut h);
    assert!(has_text(&h, "bs-engine  ·  Engine  ·  starting"));
    assert!(has_text(&h, "bs-tools  ·  Tools  ·  starting"));
    // The selected project narrows the view to its own sessions.
    h.state_mut()
        .select_project(Some(engine_project.as_str().to_owned()));
    settle(&mut h);
    assert!(has_text(&h, "bs-engine  ·  Engine"));
    assert!(!has_text(&h, "bs-tools"));
    // A person interrupts the project's session from the view.
    click_row(&mut h, "bs-engine  ·  Engine");
    click(&mut h, Role::Button, "Close session");
    step_until(&mut h, "the project's session closed", |_| {
        browser_phase(&engine, "bs-engine") == devworks_core::ports::storage::BrowserPhase::Closed
    });
    assert_eq!(
        browser_phase(&engine, "bs-tools"),
        devworks_core::ports::storage::BrowserPhase::Starting
    );
}
/// A shell over `gate` with the Engine project (member repo-a) selected in All projects.
fn gated_engine(
    name: &str,
    gate: &Gate,
    script: Script,
) -> (
    Root,
    devworks_desktop::host::Engine,
    ProjectRecord,
    egui_kittest::Harness<'static, devworks_desktop::shell::Shell>,
) {
    let root = Root::new(name);
    let engine = start(&root);
    register(&engine, "repo-a", "engine");
    register(&engine, "repo-b", "tools");
    let real = engine.backend();
    let made = ops::project_changed(real.command(
        "projects.create",
        &ops::project_create(&*real, "Engine", "", &["repo-a".into()]),
    ))
    .unwrap();
    let mut h = harness(
        shell_with(Ok(gate.over(real.clone())), &root, script, None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    click_nav(&mut h, "All projects");
    click_row(&mut h, "Engine  ·");
    (root, engine, made, h)
}

#[test]
fn project_edits_made_while_a_save_runs_stay_open_at_the_saved_revision() {
    let gate = Gate::new("projects.edit");
    let (_root, engine, _made, mut h) =
        gated_engine("projects-edit-in-flight", &gate, Script::default());
    let backend = engine.backend();
    click(&mut h, Role::Button, "Edit");
    h.state_mut().projects.edit.as_mut().expect("editing").name = "Engine 2".into();
    settle(&mut h);
    gate.close();
    h.get_by_role_and_label(Role::Button, "Save").click();
    h.run_steps(3);
    assert!(
        !h.state().projects.edit_saving.is_empty(),
        "the save is in flight"
    );
    h.state_mut().projects.edit.as_mut().unwrap().name = "Engine 3".into();
    gate.open();
    settle(&mut h);
    let saved = listed(&*backend, None).remove(0);
    assert_eq!(saved.name.0, "Engine 2", "the save submitted its own form");
    let form = h
        .state()
        .projects
        .edit
        .clone()
        .expect("the later edit is kept");
    assert_eq!(form.name, "Engine 3");
    assert_eq!(form.snapshot.revision, saved.revision);
    // Saving it again applies at the saved revision and settles the editor.
    click(&mut h, Role::Button, "Save");
    assert_eq!(listed(&*backend, None).remove(0).name.0, "Engine 3");
    assert!(h.state().projects.edit.is_none());
}

#[test]
fn membership_choices_made_while_a_save_runs_stay_open_at_the_saved_revision() {
    let gate = Gate::new("projects.membership");
    let (_root, engine, _made, mut h) =
        gated_engine("projects-membership-in-flight", &gate, Script::default());
    let backend = engine.backend();
    click(&mut h, Role::Button, "Repositories…");
    let set = |v: &[&str]| {
        v.iter()
            .map(|s| s.to_string())
            .collect::<std::collections::BTreeSet<_>>()
    };
    h.state_mut()
        .projects
        .membership
        .as_mut()
        .expect("choosing")
        .repositories = set(&["repo-a", "repo-b"]);
    settle(&mut h);
    gate.close();
    h.get_by_role_and_label(Role::Button, "Save repositories")
        .click();
    h.run_steps(3);
    assert!(!h.state().projects.membership_saving.is_empty());
    h.state_mut()
        .projects
        .membership
        .as_mut()
        .unwrap()
        .repositories = set(&["repo-b"]);
    gate.open();
    settle(&mut h);
    let saved = listed(&*backend, None).remove(0);
    assert_eq!(
        saved
            .repository_ids
            .0
            .iter()
            .map(Id::as_str)
            .collect::<Vec<_>>(),
        ["repo-a", "repo-b"]
    );
    let form = h
        .state()
        .projects
        .membership
        .clone()
        .expect("the later choice is kept");
    assert_eq!(form.repositories, set(&["repo-b"]));
    assert_eq!(form.snapshot.revision, saved.revision);
}

#[test]
fn a_base_directory_typed_while_its_save_runs_stays_a_draft() {
    let gate = Gate::new("settings.save_revision");
    let script = Script::default();
    let (root, engine, made, mut h) =
        gated_engine("projects-base-in-flight", &gate, script.clone());
    let backend = engine.backend();
    script.folders.lock().unwrap().push(root.dir("clones"));
    let pid = made.id.0.as_str().to_owned();
    click(&mut h, Role::Button, "Choose…");
    gate.close();
    h.get_by_role_and_label(Role::Button, "Save project settings")
        .click();
    h.run_steps(3);
    assert!(
        h.state().projects.base[&pid].saving,
        "the save is in flight"
    );
    h.get_by_role_and_label(Role::TextInput, "Repository base directory")
        .focus();
    h.step();
    h.key_press(egui::Key::End);
    h.get_by_role_and_label(Role::TextInput, "Repository base directory")
        .type_text("-later");
    h.step();
    gate.open();
    settle(&mut h);
    let chosen = root.dir("clones").display().to_string();
    let entries = ops::settings_entries(backend.query(
        "settings.read_effective",
        &ops::project_settings_read(&*backend, &pid, &["repositories.baseDirectory"]),
    ))
    .unwrap()
    .1;
    assert_eq!(
        entries[0].value.value.0,
        serde_json::json!(chosen),
        "the save sent its own text"
    );
    let b = &h.state().projects.base[&pid];
    assert_eq!(
        b.buffer,
        format!("{chosen}-later"),
        "the later text is kept"
    );
    assert!(
        b.view
            .as_ref()
            .and_then(|v| v.item("repositories.baseDirectory"))
            .is_some_and(|i| i.is_dirty()),
        "and still unsaved"
    );
}

#[test]
fn an_unknown_project_edit_at_close_is_confirmed_at_the_next_launch_and_applied_once() {
    let root = Root::new("projects-edit-unknown");
    let engine = start(&root);
    let real = engine.backend();
    ops::project_changed(real.command(
        "projects.create",
        &ops::project_create(&*real, "Engine", "", &[]),
    ))
    .unwrap();
    let lossy = LoseAcks::over(real.clone(), "projects.edit", 4);
    let mut h = harness(
        shell_with(Ok(lossy.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    click_nav(&mut h, "All projects");
    click_row(&mut h, "Engine  ·");
    click(&mut h, Role::Button, "Edit");
    h.state_mut().projects.edit.as_mut().unwrap().name = "Engine 2".into();
    settle(&mut h);
    click(&mut h, Role::Button, "Save");
    assert!(has_text(&h, "not yet confirmed whether it was applied"));
    assert!(!h.state().projects.edit_saving.is_empty(), "still pending");
    h.state_mut().on_exit();
    drop(h);
    let again = LoseAcks::over(real.clone(), "projects.edit", 0);
    let mut h = harness(
        shell_with(Ok(again.clone()), &root, Script::default(), None),
        1440.0,
        900.0,
    );
    settle(&mut h);
    let now = listed(&*real, None).remove(0);
    assert_eq!(now.name.0, "Engine 2");
    assert_eq!(now.revision.as_str(), "2", "applied once");
    assert_eq!(
        again.request_keys().len(),
        1,
        "confirmed with its exact frame"
    );
    assert_eq!(h.state().unconfirmed_count(), 0);
}

/// The Engine project (member repo-a) selected in All projects, over the real runtime.
fn engine_shell(
    name: &str,
    script: Script,
) -> (
    Root,
    devworks_desktop::host::Engine,
    ProjectRecord,
    egui_kittest::Harness<'static, devworks_desktop::shell::Shell>,
) {
    gated_engine(name, &Gate::new("unused.operation"), script)
}

fn members_of(p: &ProjectRecord) -> Vec<&str> {
    p.repository_ids.0.iter().map(Id::as_str).collect()
}

fn repo_set(v: &[&str]) -> std::collections::BTreeSet<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// Sets `project`'s `safety.accessProfile` through the settings seam (as the Settings screen
/// does), outside the open editor.
fn set_access_profile(backend: &dyn Backend, project: &str, profile: &str) {
    let key = "safety.accessProfile";
    let (revision, _) = ops::settings_entries(backend.query(
        "settings.read_effective",
        &ops::project_settings_read(backend, project, &[key]),
    ))
    .unwrap();
    let d = devworks_core::settings::catalog::descriptor(key).unwrap();
    let body = serde_json::json!({"scope": {"kind": "Project", "projectId": project},
        "expectedRevision": revision.to_string(), "entries": [
        {"key": key, "value": {"schema": d.schema_ref(), "value": profile}}]});
    ops::settings_saved(backend.command(
        "settings.save_revision",
        &ops::command_frame(backend, "settings.save_revision", body),
    ))
    .expect("the access profile is saved");
}

#[test]
fn saving_a_project_edit_rebases_the_open_repositories_form() {
    let (_root, engine, _made, mut h) =
        engine_shell("projects-edit-rebases-members", Script::default());
    let backend = engine.backend();
    click(&mut h, Role::Button, "Edit");
    click(&mut h, Role::Button, "Repositories…");
    h.state_mut().projects.edit.as_mut().unwrap().name = "Engine 2".into();
    h.state_mut()
        .projects
        .membership
        .as_mut()
        .unwrap()
        .repositories = repo_set(&["repo-a", "repo-b"]);
    settle(&mut h);
    click(&mut h, Role::Button, "Save");
    let edited = listed(&*backend, None).remove(0);
    assert_eq!(edited.name.0, "Engine 2");
    let form = h
        .state()
        .projects
        .membership
        .clone()
        .expect("still choosing");
    assert_eq!(
        form.snapshot.revision, edited.revision,
        "rebased on the edit"
    );
    assert_eq!(
        form.repositories,
        repo_set(&["repo-a", "repo-b"]),
        "the choice is kept"
    );
    click(&mut h, Role::Button, "Save repositories");
    let now = listed(&*backend, None).remove(0);
    assert_eq!(
        members_of(&now),
        ["repo-a", "repo-b"],
        "saved without a conflict"
    );
    assert_eq!(now.name.0, "Engine 2");
    assert!(h.state().projects.membership.is_none());
    assert!(!has_text(&h, "the project changed meanwhile"));
}

#[test]
fn saving_repositories_rebases_the_open_project_editor() {
    let (_root, engine, _made, mut h) =
        engine_shell("projects-members-rebase-edit", Script::default());
    let backend = engine.backend();
    click(&mut h, Role::Button, "Edit");
    click(&mut h, Role::Button, "Repositories…");
    h.state_mut().projects.edit.as_mut().unwrap().description = "the engine".into();
    h.state_mut()
        .projects
        .membership
        .as_mut()
        .unwrap()
        .repositories = repo_set(&["repo-a", "repo-b"]);
    settle(&mut h);
    click(&mut h, Role::Button, "Save repositories");
    let changed = listed(&*backend, None).remove(0);
    assert_eq!(members_of(&changed), ["repo-a", "repo-b"]);
    let form = h.state().projects.edit.clone().expect("still editing");
    assert_eq!(
        form.snapshot.revision, changed.revision,
        "rebased on the membership"
    );
    assert_eq!(form.description, "the engine", "the draft is kept");
    click(&mut h, Role::Button, "Save");
    let now = listed(&*backend, None).remove(0);
    assert_eq!(now.description.0, "the engine", "saved without a conflict");
    assert_eq!(members_of(&now), ["repo-a", "repo-b"]);
    assert!(h.state().projects.edit.is_none());
    assert!(!has_text(&h, "the project changed meanwhile"));
}

#[test]
fn an_access_profile_changed_in_settings_is_kept_by_a_rebased_editor() {
    let (_root, engine, made, mut h) = engine_shell("projects-access-sibling", Script::default());
    let backend = engine.backend();
    let pid = made.id.0.as_str().to_owned();
    assert!(matches!(made.access_mode, AccessMode::Full));
    click(&mut h, Role::Button, "Edit");
    click(&mut h, Role::Button, "Repositories…");
    // The project is restricted in settings while the editor is open.
    set_access_profile(&*backend, &pid, "projectScope");
    h.state_mut()
        .projects
        .membership
        .as_mut()
        .unwrap()
        .repositories = repo_set(&["repo-a", "repo-b"]);
    settle(&mut h);
    click(&mut h, Role::Button, "Save repositories");
    let form = h.state().projects.edit.clone().expect("still editing");
    assert!(
        matches!(form.access, AccessMode::Project),
        "the untouched access takes the returned record's: {:?}",
        form.access
    );
    h.state_mut().projects.edit.as_mut().unwrap().name = "Engine 2".into();
    settle(&mut h);
    click(&mut h, Role::Button, "Save");
    let now = listed(&*backend, None).remove(0);
    assert_eq!(now.name.0, "Engine 2");
    assert!(
        matches!(now.access_mode, AccessMode::Project),
        "the restriction is not undone: {:?}",
        now.access_mode
    );
}

#[test]
fn an_access_profile_changed_in_settings_is_kept_after_the_editors_own_save() {
    let gate = Gate::new("projects.edit");
    let (_root, engine, made, mut h) =
        gated_engine("projects-access-own-save", &gate, Script::default());
    let backend = engine.backend();
    let pid = made.id.0.as_str().to_owned();
    click(&mut h, Role::Button, "Edit");
    set_access_profile(&*backend, &pid, "projectScope");
    h.state_mut().projects.edit.as_mut().unwrap().name = "Engine 2".into();
    settle(&mut h);
    gate.close();
    h.get_by_role_and_label(Role::Button, "Save").click();
    h.run_steps(3);
    assert!(
        !h.state().projects.edit_saving.is_empty(),
        "the save is in flight"
    );
    h.state_mut().projects.edit.as_mut().unwrap().description = "later".into();
    gate.open();
    settle(&mut h);
    let saved = listed(&*backend, None).remove(0);
    assert!(matches!(saved.access_mode, AccessMode::Project));
    let form = h
        .state()
        .projects
        .edit
        .clone()
        .expect("the later edit is kept");
    assert_eq!(form.description, "later");
    assert_eq!(form.name, "Engine 2");
    assert!(
        matches!(form.access, AccessMode::Project),
        "the untouched access takes the returned record's: {:?}",
        form.access
    );
    click(&mut h, Role::Button, "Save");
    let now = listed(&*backend, None).remove(0);
    assert_eq!(now.description.0, "later");
    assert!(
        matches!(now.access_mode, AccessMode::Project),
        "the restriction is not undone: {:?}",
        now.access_mode
    );
}

#[test]
fn an_archive_confirmation_waiting_during_a_save_archives_the_saved_project() {
    let (_root, engine, _made, mut h) = engine_shell("projects-confirm-rebase", Script::default());
    let backend = engine.backend();
    click(&mut h, Role::Button, "Edit");
    h.state_mut().projects.edit.as_mut().unwrap().name = "Engine 2".into();
    settle(&mut h);
    click(&mut h, Role::Button, "Archive…");
    assert!(h.state().projects.confirm.is_some());
    click(&mut h, Role::Button, "Save");
    assert_eq!(listed(&*backend, None).remove(0).name.0, "Engine 2");
    assert!(
        has_text(&h, "Archive Engine 2?"),
        "the confirmation shows the saved project"
    );
    click(&mut h, Role::Button, "Archive project");
    let now = listed(&*backend, None).remove(0);
    assert!(
        matches!(now.lifecycle, LifeState::Archived),
        "archived without a conflict: {:?}",
        now.lifecycle
    );
}

#[test]
fn a_base_directory_draft_saves_after_the_editor_changes_the_access_profile() {
    let script = Script::default();
    let (root, engine, made, mut h) = engine_shell("projects-base-after-access", script.clone());
    let backend = engine.backend();
    script.folders.lock().unwrap().push(root.dir("clones"));
    let pid = made.id.0.as_str().to_owned();
    click(&mut h, Role::Button, "Choose…");
    click(&mut h, Role::Button, "Edit");
    h.state_mut().projects.edit.as_mut().unwrap().access = AccessMode::Project;
    settle(&mut h);
    click(&mut h, Role::Button, "Save");
    assert!(matches!(
        listed(&*backend, None).remove(0).access_mode,
        AccessMode::Project
    ));
    click(&mut h, Role::Button, "Save project settings");
    let entries = ops::settings_entries(backend.query(
        "settings.read_effective",
        &ops::project_settings_read(&*backend, &pid, &["repositories.baseDirectory"]),
    ))
    .unwrap()
    .1;
    assert_eq!(
        entries[0].value.value.0,
        serde_json::json!(root.dir("clones").display().to_string()),
        "saved without a conflict"
    );
    assert!(h.state().projects.base[&pid].error.is_none());
}
