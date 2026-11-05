//! Native package UI recovery: the bundle artifact and installation are separate durable
//! commands. Invalid signed-bundle bytes make the final installation refuse deterministically;
//! the engine package suite covers successful signed installations. No network or inference.
mod common;
use common::*;
use devworks_desktop::backend::Backend;
use devworks_desktop::memory::Memory;
use devworks_desktop::ops;
use devworks_desktop::route::Route;
use devworks_desktop::shell::Lifecycle;
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::Queryable;
use std::sync::Arc;

fn open_install(
    root: &Root,
    backend: Arc<dyn Backend>,
) -> egui_kittest::Harness<'static, devworks_desktop::shell::Shell> {
    let script = Script::default();
    script
        .files
        .lock()
        .unwrap()
        .push(vec![root.file("fixture.dwpkg", b"invalid signed bundle")]);
    let mut h = harness(shell_with(Ok(backend), root, script, None), 1440.0, 1000.0);
    settle(&mut h);
    h.state_mut().navigate(Route::AddOns);
    settle(&mut h);
    h
}

fn confirm_after_relaunch(
    root: &Root,
    real: Arc<dyn Backend>,
    operation: &'static str,
    frame: &[u8],
) {
    let again = LoseAcks::over(real.clone(), operation, 0);
    let mut h = harness(
        shell_with(Ok(again.clone()), root, Script::default(), None),
        1440.0,
        1000.0,
    );
    settle(&mut h);
    assert_eq!(h.state().unconfirmed_count(), 0);
    assert!(!h.state().addons.busy);
    assert_eq!(again.frames.lock().unwrap().as_slice(), &[frame.to_vec()]);
    assert_eq!(
        ops::artifacts(real.query("artifacts.list", &ops::workspace_artifacts(&*real)))
            .unwrap()
            .0
            .len(),
        1,
        "upload is stored once even when installation is refused"
    );
    assert!(
        h.state().addons.items.is_empty(),
        "invalid bundle is never installed"
    );
}

#[test]
fn unknown_bundle_save_and_install_commands_keep_their_exact_frames() {
    for operation in ["artifacts.save", "addons.install"] {
        let root = Root::new("package-unknown");
        let engine = start(&root);
        let real = engine.backend();
        let lossy = LoseAcks::over(real.clone(), operation, 4);
        let mut h = open_install(&root, lossy.clone());
        click(&mut h, Role::Button, "Install from file…");
        assert_eq!(h.state().unconfirmed_count(), 1);
        assert!(h.state().addons.busy);
        let frames = lossy.frames.lock().unwrap().clone();
        assert_eq!(frames.len(), 4);
        assert!(frames.iter().all(|f| f == &frames[0]));
        h.state_mut().on_exit();
        let saved = Memory::load(&root.memory());
        assert_eq!(saved.pending.commands.len(), 1);
        assert_eq!(saved.pending.commands[0].operation, operation);
        assert_eq!(saved.pending.commands[0].frame, frames[0]);
        drop(h);
        confirm_after_relaunch(&root, real, operation, &frames[0]);
    }
}

#[test]
fn either_package_command_held_past_close_is_confirmed_after_relaunch() {
    for operation in ["artifacts.save", "addons.install"] {
        let root = Root::new("package-close");
        let engine = start(&root);
        let real = engine.backend();
        let seen = LoseAcks::over(real.clone(), operation, 0);
        let gate = Gate::holding_replies(operation);
        let mut h = open_install(&root, gate.over(seen.clone()));
        gate.close();
        h.get_by_role_and_label(Role::Button, "Install from file…")
            .click();
        step_until(&mut h, "package command admitted", |_| {
            !seen.frames.lock().unwrap().is_empty()
        });
        h.state_mut().request_close();
        step_until(&mut h, "closed with package command pending", |s| {
            matches!(s.lifecycle(), Lifecycle::Closed)
        });
        let saved = Memory::load(&root.memory());
        gate.open();
        let frame = seen.frames.lock().unwrap()[0].clone();
        assert_eq!(saved.pending.commands.len(), 1);
        assert_eq!(saved.pending.commands[0].operation, operation);
        assert_eq!(saved.pending.commands[0].frame, frame);
        drop(h);
        confirm_after_relaunch(&root, real, operation, &frame);
    }
}

#[test]
fn unused_download_preparation_cleans_its_owned_scratch_file() {
    let root = Root::new("package-preparation-drop");
    let engine = start(&root);
    let backend = engine.backend();
    let dir = backend.scratch_dir().join("packages");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("unused.dwpkg");
    std::fs::write(&path, b"not yet submitted").unwrap();
    let prepared =
        devworks_desktop::addons::prepare_downloaded_install(&*backend, path.clone()).unwrap();
    assert!(path.is_file());
    assert!(
        ops::artifacts(backend.query("artifacts.list", &ops::workspace_artifacts(&*backend)))
            .unwrap()
            .0
            .is_empty()
    );
    drop(prepared);
    assert!(
        !path.exists(),
        "a dropped reply cannot leave an unused download"
    );
}
