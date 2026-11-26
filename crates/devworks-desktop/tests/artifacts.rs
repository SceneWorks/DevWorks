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

    // Tags are edited under the listed revision.
    click(&mut h, Role::Button, "Edit tags");
    type_into(&mut h, "Artifact tags", "release, evidence");
    click(&mut h, Role::Button, "Save tags");
    let after = listed(&*b, &project);
    let tagged = after
        .iter()
        .find(|r| r.artifact.artifact_id == notes.artifact.artifact_id)
        .unwrap();
    assert_eq!(
        tagged.tags.as_ref().unwrap().0,
        ["evidence".to_owned(), "release".to_owned()]
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
