//! The Artifacts screen's Project tab against the real runtime: a person finds a project's
//! artifacts, previews text and images, exports one to a chosen file, edits tags and deletes
//! one, every action going through the gateway.
mod common;
use common::*;
use devworks_core::protocol::ProjectId;
use devworks_core::protocol::records::ArtifactRecord;
use devworks_desktop::backend::Backend;
use devworks_desktop::ops;
use devworks_desktop::route::Route;
use eframe::egui;
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::{NodeT, Queryable};

fn save(
    b: &dyn Backend,
    root: &Root,
    project: &ProjectId,
    name: &str,
    media: &str,
    bytes: &[u8],
) -> ArtifactRecord {
    let path = root.file(name, bytes);
    let ticket = b.mint_project_upload(project, &path).unwrap();
    ops::saved_artifact(b.command(
        "artifacts.save",
        &ops::save_project_artifact(
            b,
            project.0.as_str(),
            ticket.as_str(),
            name,
            media,
            &devworks_core::fingerprint::sha256_hex(bytes),
            bytes.len() as u64,
        ),
    ))
    .unwrap()
}

fn type_into(
    h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>,
    label: &str,
    text: &str,
) {
    h.get_by_role_and_label(Role::TextInput, label)
        .scroll_to_me();
    settle(h);
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, label)
        .type_text(text);
    settle(h);
}

/// Selects the listed row whose label starts with `name`.
fn select(h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>, name: &str) {
    let label = h
        .query_all_by_label_contains(name)
        .filter_map(|n| n.accesskit_node().label())
        .find(|l| l.starts_with(&format!("{name}  ·")))
        .unwrap_or_else(|| panic!("a row for {name}"));
    let role = h.get_by_label(&label).accesskit_node().role();
    click(h, role, &label);
}

fn listed(b: &dyn Backend, project: &ProjectId) -> Vec<ArtifactRecord> {
    ops::artifacts(b.query(
        "artifacts.list",
        &ops::project_artifacts(b, project.0.as_str(), None),
    ))
    .unwrap()
    .items
}

#[test]
fn project_artifacts_are_searched_previewed_exported_tagged_and_deleted() {
    let root = Root::new("project-artifacts");
    let engine = start(&root);
    let b = engine.backend();
    let project = ops::project_changed(b.command(
        "projects.create",
        &ops::project_create(&*b, "Artifacts project", "", &[]),
    ))
    .unwrap()
    .id;
    let text = b"# Release notes\n\nEvery check passes.\n";
    let notes = save(&*b, &root, &project, "notes.md", "text/markdown", text);
    save(&*b, &root, &project, "diagram.png", "image/png", &png());
    let target = root
        .file("placeholder", b"")
        .with_file_name("exported-notes.md");
    let script = Script::default();
    script.saves.lock().unwrap().push(target.clone());
    let mut h = harness(shell(&engine, &root, script), 1400.0, 1100.0);
    settle(&mut h);
    h.state_mut()
        .select_project(Some(project.0.as_str().into()));
    h.state_mut().navigate(Route::Artifacts);
    settle(&mut h);
    click(&mut h, Role::Button, "Project");
    assert!(!has_text(&h, "is not available"), "the placeholder is gone");
    assert!(has_text(&h, "notes.md") && has_text(&h, "diagram.png"));

    // Search narrows the list; the image preview decodes the stored image.
    type_into(&mut h, "Filter artifacts", "png");
    assert!(has_text(&h, "diagram.png") && !has_text(&h, "notes.md"));
    select(&mut h, "diagram.png");
    click(&mut h, Role::Button, "Preview");
    let shown: Vec<String> = h
        .query_all_by_label_contains("image")
        .chain(h.query_all_by_label_contains("preview"))
        .filter_map(|n| n.accesskit_node().label())
        .collect();
    assert!(
        has_text(&h, "PNG image, 2 × 2"),
        "{:?} {shown:?}",
        h.state().status()
    );
    assert!(
        h.query_all_by_role(Role::Image).next().is_some(),
        "the stored image is decoded and drawn"
    );

    // The text preview shows the file's text.
    h.get_by_role_and_label(Role::TextInput, "Filter artifacts")
        .focus();
    h.step();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.key_press(egui::Key::Backspace);
    settle(&mut h);
    type_into(&mut h, "Filter artifacts", "notes");
    select(&mut h, "notes.md");
    click(&mut h, Role::Button, "Preview");
    assert!(
        has_text(&h, "Every check passes."),
        "{:?}",
        h.state().status()
    );

    // Export writes the verified bytes to the chosen file.
    click(&mut h, Role::Button, "Export…");
    assert_eq!(std::fs::read(&target).unwrap(), text);
    assert!(h.state().status().is_some_and(|s| s.contains("Exported")));

    // Tags are edited one per field, kept exactly as written (a comma is part of a tag).
    click(&mut h, Role::Button, "Edit tags");
    click(&mut h, Role::Button, "Add tag");
    type_into(&mut h, "Tag 1", "release, candidate");
    click(&mut h, Role::Button, "Add tag");
    type_into(&mut h, "Tag 2", "evidence");
    click(&mut h, Role::Button, "Save tags");
    let tags = |b: &dyn Backend| {
        listed(b, &project)
            .into_iter()
            .find(|r| r.artifact.artifact_id == notes.artifact.artifact_id)
            .unwrap()
            .tags
            .unwrap()
            .0
    };
    assert_eq!(
        tags(&*b),
        ["evidence".to_owned(), "release, candidate".to_owned()]
    );
    // Saving the opened tags unchanged keeps them.
    click(&mut h, Role::Button, "Edit tags");
    click(&mut h, Role::Button, "Save tags");
    assert_eq!(
        tags(&*b),
        ["evidence".to_owned(), "release, candidate".to_owned()]
    );

    // Delete asks once more, then removes the whole artifact.
    click(&mut h, Role::Button, "Delete");
    assert!(
        listed(&*b, &project).len() == 2,
        "nothing is deleted before confirmation"
    );
    click(&mut h, Role::Button, "Confirm delete");
    let left = listed(&*b, &project);
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].name.0, "diagram.png");
    assert!(h.state().status().is_some_and(|s| s.contains("deleted")));
}

fn retag(b: &dyn Backend, record: &ArtifactRecord, tags: &[&str]) {
    let revision = record.revision.as_ref().unwrap().as_str().to_owned();
    ops::artifact_tagged(b.command(
        "artifacts.tag",
        &ops::artifact_tag(
            b,
            record.artifact.artifact_id.as_str(),
            &revision,
            tags.iter().map(|t| (*t).to_owned()).collect(),
        ),
    ))
    .unwrap();
}

/// Edits and deletions are bound to what the person opened: a change another client makes in
/// between is refused, never overwritten or swept in. The Project tab follows the selected
/// project at once, and an export into DevWorks' own data folder is refused.
#[test]
fn artifact_edits_are_bound_to_what_was_opened_and_follow_the_selected_project() {
    let root = Root::new("project-artifacts-bound");
    let engine = start(&root);
    let b = engine.backend();
    let make = |name: &str| {
        ops::project_changed(b.command("projects.create", &ops::project_create(&*b, name, "", &[])))
            .unwrap()
            .id
    };
    let (first, second) = (make("First project"), make("Second project"));
    let report = save(
        &*b,
        &root,
        &first,
        "report.md",
        "text/markdown",
        b"# Report\n",
    );
    save(
        &*b,
        &root,
        &second,
        "other.md",
        "text/markdown",
        b"# Other\n",
    );
    let script = Script::default();
    script
        .saves
        .lock()
        .unwrap()
        .push(root.0.join("exported.md"));
    let mut h = harness(shell(&engine, &root, script), 1400.0, 1100.0);
    settle(&mut h);
    h.state_mut().select_project(Some(first.0.as_str().into()));
    h.state_mut().navigate(Route::Artifacts);
    settle(&mut h);
    click(&mut h, Role::Button, "Project");
    select(&mut h, "report.md");

    // A tag edit opened before another client's change is refused on save.
    click(&mut h, Role::Button, "Edit tags");
    click(&mut h, Role::Button, "Add tag");
    type_into(&mut h, "Tag 1", "mine");
    retag(&*b, &listed(&*b, &first)[0], &["theirs"]);
    click(&mut h, Role::Button, "Refresh");
    click(&mut h, Role::Button, "Save tags");
    assert_eq!(
        listed(&*b, &first)[0].tags.as_ref().unwrap().0,
        ["theirs".to_owned()],
        "the concurrent tags are kept"
    );
    assert!(
        h.state().status().is_some_and(|s| s.contains("not saved")),
        "{:?}",
        h.state().status()
    );
    click(&mut h, Role::Button, "Cancel");

    // A deletion confirmed after another change is refused; the artifact stays.
    click(&mut h, Role::Button, "Delete");
    retag(&*b, &listed(&*b, &first)[0], &["theirs", "again"]);
    click(&mut h, Role::Button, "Refresh");
    click(&mut h, Role::Button, "Confirm delete");
    assert_eq!(
        listed(&*b, &first).len(),
        1,
        "nothing is swept into the earlier decision"
    );
    assert!(
        h.state()
            .status()
            .is_some_and(|s| s.contains("not deleted")),
        "{:?}",
        h.state().status()
    );

    // An export into the application data folder is refused by the shared export writer.
    select(&mut h, "report.md");
    click(&mut h, Role::Button, "Export…");
    assert!(!root.0.join("exported.md").exists());
    assert!(
        h.state()
            .status()
            .is_some_and(|s| s.contains("data folder")),
        "{:?}",
        h.state().status()
    );

    // Selecting another project replaces the Project tab's list without a refresh.
    h.state_mut().select_project(Some(second.0.as_str().into()));
    settle(&mut h);
    assert!(has_text(&h, "other.md") && !has_text(&h, "report.md"));
    let _ = report;
}

/// A slow tag save closes only the draft it submitted: a draft opened for another artifact while
/// it was in flight is kept.
#[test]
fn a_finished_tag_save_keeps_a_draft_opened_since() {
    let root = Root::new("project-artifacts-slow-tag");
    let engine = start(&root);
    let real = engine.backend();
    let project = ops::project_changed(real.command(
        "projects.create",
        &ops::project_create(&*real, "Tags project", "", &[]),
    ))
    .unwrap()
    .id;
    save(
        &*real,
        &root,
        &project,
        "first.md",
        "text/markdown",
        b"# First\n",
    );
    save(
        &*real,
        &root,
        &project,
        "second.md",
        "text/markdown",
        b"# Second\n",
    );
    let gate = Gate::new("artifacts.tag");
    let mut h = harness(
        shell_with(Ok(gate.over(real.clone())), &root, Script::default(), None),
        1400.0,
        1100.0,
    );
    settle(&mut h);
    h.state_mut()
        .select_project(Some(project.0.as_str().into()));
    h.state_mut().navigate(Route::Artifacts);
    settle(&mut h);
    click(&mut h, Role::Button, "Project");
    select(&mut h, "first.md");
    click(&mut h, Role::Button, "Edit tags");
    click(&mut h, Role::Button, "Add tag");
    type_into(&mut h, "Tag 1", "first-tag");
    gate.close();
    h.get_by_role_and_label(Role::Button, "Save tags").click();
    step_until(&mut h, "the save is in flight", |_| gate.has_waiter());
    // While the save is held, frames run without waiting for requests to finish.
    let poke = |h: &mut egui_kittest::Harness<'_, devworks_desktop::shell::Shell>, label: &str| {
        let node = h
            .query_all_by_label_contains(label)
            .find(|n| {
                n.accesskit_node()
                    .label()
                    .is_some_and(|l| l.starts_with(label))
            })
            .unwrap_or_else(|| panic!("{label}"));
        node.scroll_to_me();
        for _ in 0..3 {
            h.step();
        }
        let node = h
            .query_all_by_label_contains(label)
            .find(|n| {
                n.accesskit_node()
                    .label()
                    .is_some_and(|l| l.starts_with(label))
            })
            .unwrap();
        node.click();
        for _ in 0..3 {
            h.step();
        }
    };
    poke(&mut h, "second.md  ·");
    poke(&mut h, "Edit tags");
    poke(&mut h, "Add tag");
    h.get_by_role_and_label(Role::TextInput, "Tag 1").focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, "Tag 1")
        .type_text("second-draft");
    h.step();
    gate.open();
    settle(&mut h);
    assert!(h.state().status().is_some_and(|s| s.contains("Tags saved")));
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, "Tag 1")
            .value()
            .as_deref(),
        Some("second-draft"),
        "the second artifact's unsaved draft is kept"
    );
}

/// The detail scrolls, so the full 32-tag set can be edited and saved at the minimum window size.
#[test]
fn thirty_two_tags_can_be_saved_at_the_minimum_window_size() {
    let root = Root::new("project-artifacts-many-tags");
    let engine = start(&root);
    let b = engine.backend();
    let project = ops::project_changed(b.command(
        "projects.create",
        &ops::project_create(&*b, "Many tags", "", &[]),
    ))
    .unwrap()
    .id;
    save(
        &*b,
        &root,
        &project,
        "tagged.md",
        "text/markdown",
        b"# Tagged\n",
    );
    let names: Vec<String> = (0..32).map(|i| format!("tag-{i:02}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    retag(&*b, &listed(&*b, &project)[0], &refs);
    let mut h = harness(shell(&engine, &root, Script::default()), 760.0, 520.0);
    settle(&mut h);
    h.state_mut()
        .select_project(Some(project.0.as_str().into()));
    h.state_mut().navigate(Route::Artifacts);
    settle(&mut h);
    click(&mut h, Role::Button, "Project");
    select(&mut h, "tagged.md");
    click(&mut h, Role::Button, "Edit tags");
    let fields = |h: &egui_kittest::Harness<'_, devworks_desktop::shell::Shell>| {
        h.query_all_by_role(Role::TextInput)
            .filter(|n| {
                n.accesskit_node()
                    .label()
                    .is_some_and(|l| l.starts_with("Tag "))
            })
            .count()
    };
    let before = fields(&h);
    click(&mut h, Role::Button, "Remove tag 32");
    assert_eq!((before, fields(&h)), (32, 31));
    assert!(
        h.query_by_role_and_label(Role::TextInput, "Tag 32")
            .is_none(),
        "the last tag field is removed"
    );
    click(&mut h, Role::Button, "Save tags");
    assert!(
        h.state().status().is_some_and(|s| s.contains("Tags saved")),
        "{:?}",
        h.state().status()
    );
    assert_eq!(listed(&*b, &project)[0].tags.as_ref().unwrap().0.len(), 31);
    // Delete, its confirmation and Keep artifact are all reachable at this size.
    click(&mut h, Role::Button, "Delete");
    click(&mut h, Role::Button, "Keep artifact");
    assert_eq!(
        listed(&*b, &project).len(),
        1,
        "keeping cancels the deletion"
    );
    click(&mut h, Role::Button, "Delete");
    click(&mut h, Role::Button, "Confirm delete");
    assert!(listed(&*b, &project).is_empty(), "{:?}", h.state().status());
}

/// Editing the same draft while its save is in flight keeps the edits, and the next save goes
/// through from the revision the first save produced.
#[test]
fn a_draft_edited_during_its_own_save_saves_again() {
    let root = Root::new("project-artifacts-same-draft");
    let engine = start(&root);
    let real = engine.backend();
    let project = ops::project_changed(real.command(
        "projects.create",
        &ops::project_create(&*real, "Same draft", "", &[]),
    ))
    .unwrap()
    .id;
    save(
        &*real,
        &root,
        &project,
        "notes.md",
        "text/markdown",
        b"# Notes\n",
    );
    let gate = Gate::new("artifacts.tag");
    let mut h = harness(
        shell_with(Ok(gate.over(real.clone())), &root, Script::default(), None),
        1400.0,
        1100.0,
    );
    settle(&mut h);
    h.state_mut()
        .select_project(Some(project.0.as_str().into()));
    h.state_mut().navigate(Route::Artifacts);
    settle(&mut h);
    click(&mut h, Role::Button, "Project");
    select(&mut h, "notes.md");
    click(&mut h, Role::Button, "Edit tags");
    click(&mut h, Role::Button, "Add tag");
    type_into(&mut h, "Tag 1", "first");
    gate.close();
    h.get_by_role_and_label(Role::Button, "Save tags").click();
    step_until(&mut h, "the save is in flight", |_| gate.has_waiter());
    h.get_by_role_and_label(Role::Button, "Add tag").click();
    for _ in 0..3 {
        h.step();
    }
    h.get_by_role_and_label(Role::TextInput, "Tag 2").focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, "Tag 2")
        .type_text("second");
    h.step();
    gate.open();
    settle(&mut h);
    assert_eq!(
        listed(&*real, &project)[0].tags.as_ref().unwrap().0,
        ["first".to_owned()]
    );
    assert!(
        h.query_by_role_and_label(Role::TextInput, "Tag 2")
            .is_some(),
        "the edit is kept"
    );
    click(&mut h, Role::Button, "Save tags");
    assert!(
        h.state().status().is_some_and(|s| s.contains("Tags saved")),
        "{:?}",
        h.state().status()
    );
    assert_eq!(
        listed(&*real, &project)[0].tags.as_ref().unwrap().0,
        ["first".to_owned(), "second".to_owned()]
    );
}
