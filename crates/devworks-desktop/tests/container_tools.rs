//! The test harness composes no host container CLI: opening Settings (which probes container
//! runtimes) never runs `docker`, `podman` or `container` found on the host `PATH`.
//!
//! Unix only: the fake CLIs are shell scripts. The test owns its process (one test in this
//! binary), so changing `PATH` cannot race another test.
#![cfg(unix)]
mod common;
use common::*;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn fake_clis(dir: &Path, marker: &Path) {
    for name in ["docker", "podman", "container"] {
        let script = dir.join(name);
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho {name} >> '{}'\nexit 1\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn open_settings(root: &Root, search_path: bool) {
    let mut cfg = config(&root.0);
    if !search_path {
        // Production behaviour: resolve from the process PATH.
        cfg.container_search_path = None;
    }
    let engine = devworks_desktop::host::Engine::start(cfg).expect("the runtime boots");
    let mut h = harness(shell(&engine, root, Script::default()), 1440.0, 1200.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    settle(&mut h);
    h.state().wait_idle();
    drop(h);
    drop(engine);
}

#[test]
fn opening_settings_runs_no_host_container_cli() {
    let _case = native_runtime_case();
    let bin = Root::new("container-tools-bin");
    let dir = bin.dir("bin");
    let marker = bin.0.with_extension("ran");
    fake_clis(&dir, &marker);
    let old = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![dir.clone()];
    paths.extend(std::env::split_paths(&old));
    // SAFETY: this binary holds a single test, so no other thread reads the environment.
    unsafe { std::env::set_var("PATH", std::env::join_paths(paths).unwrap()) };

    // The harness default composes an empty container search path: nothing runs.
    let quiet = Root::new("container-tools-quiet");
    open_settings(&quiet, true);
    assert!(
        !marker.exists(),
        "a host container CLI ran: {:?}",
        std::fs::read_to_string(&marker)
    );

    // Control: composed from PATH (production), the same fakes do run, so the check above is
    // not vacuous.
    let control = Root::new("container-tools-control");
    open_settings(&control, false);
    let ran = std::fs::read_to_string(&marker).unwrap_or_default();
    unsafe { std::env::set_var("PATH", old) };
    let _ = std::fs::remove_file(&marker);
    assert!(
        ran.contains("docker"),
        "the control never ran a fake CLI: {ran:?}"
    );
}
