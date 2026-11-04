//! Repository indexing in the shell over the real runtime, headless (DW-23): the indexing
//! sources are read and saved in Settings, discovery registers a real Git checkout under the
//! configured base directory, the repository's index is built from the Repositories screen and
//! its index facts (freshness, coverage, languages) shown, and the Indexing screen lists the run.
//! A real Git repository in a temporary directory; no network, no inference.
mod common;

use common::*;
use devworks_core::ports::storage::{ConfigScope, StorageError, WriteOptions, run_write};
use devworks_core::protocol::records::IndexFreshnessState;
use devworks_core::settings::catalog::descriptor;
use devworks_desktop::route::Route;
use devworks_desktop::shell::indexing::StatusState;
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::{NodeT, Queryable};
use serde_json::json;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn sources_discovery_indexing_and_status_through_the_shared_commands() {
    let root = Root::new("indexing-ui");
    let base = std::fs::canonicalize(root.dir("sources")).unwrap();
    let repo = base.join("svc");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn run() { helper(); }\nfn helper() {}\n",
    )
    .unwrap();
    std::fs::write(repo.join("README.md"), "# svc\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    let engine = start(&root);
    // The workspace repository base directory (SET-07/08), as the Settings save writes it.
    let d = descriptor("repositories.baseDirectory").unwrap();
    let assignment = serde_json::from_value(json!({
        "key": "repositories.baseDirectory",
        "value": {"schema": d.schema_ref(), "value": base.display().to_string()},
    }))
    .unwrap();
    let user = engine.host().user().clone();
    run_write(
        &**engine.host().runtime().store(),
        WriteOptions::default(),
        move |tx| {
            devworks_core::settings::service::apply_human_assignments(
                tx,
                &user,
                &ConfigScope::Workspace,
                &[assignment],
                json!({}),
            )
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
        },
    )
    .unwrap();

    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 900.0);
    settle(&mut h);
    // Settings › General shows the indexing sources; a new exclusion is saved as a revision.
    click_nav(&mut h, "Settings");
    settle(&mut h);
    step_until(&mut h, "the sources form", |s| {
        s.projects.indexing.sources.is_some()
    });
    assert!(has_text(&h, "Repository indexing sources"));
    let revision = h
        .state()
        .projects
        .indexing
        .sources
        .as_ref()
        .unwrap()
        .revision;
    h.state_mut()
        .projects
        .indexing
        .sources
        .as_mut()
        .unwrap()
        .excludes
        .push_str("\n**/generated/**");
    click(&mut h, Role::Button, "Save indexing sources");
    step_until(&mut h, "the saved revision", |s| {
        s.projects
            .indexing
            .sources
            .as_ref()
            .is_some_and(|f| f.revision == revision + 1)
    });
    // Discovery registers the checkout.
    click(&mut h, Role::Button, "Discover repositories");
    step_until(&mut h, "discovery", |s| {
        s.projects
            .indexing
            .discovered
            .as_deref()
            .is_some_and(|d| d.starts_with("1 repository"))
    });
    // Repositories: select it and index it now.
    h.state_mut().navigate(Route::Repositories);
    settle(&mut h);
    h.query_all_by_label_contains("svc  ·")
        .find(|n| {
            n.accesskit_node()
                .label()
                .is_some_and(|l| l.starts_with("svc  ·"))
        })
        .expect("the discovered repository is listed")
        .click();
    settle(&mut h);
    click(&mut h, Role::Button, "Index now");
    let id = h.state().projects.repositories.items[0]
        .id
        .as_str()
        .to_owned();
    let current = |s: &devworks_desktop::shell::Shell| {
        matches!(
            s.projects.indexing.status.get(&id),
            Some(StatusState::Ready(st)) if st.freshness.as_ref().is_some_and(|f| matches!(f.state, IndexFreshnessState::Current))
        )
    };
    let deadline = std::time::Instant::now() + SETTLE_BOUND;
    while !current(h.state()) {
        assert!(
            std::time::Instant::now() < deadline,
            "the index never became current"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
        // Read the status again, as the Load control does.
        h.state_mut().projects.indexing.status.remove(&id);
        settle(&mut h);
        click(&mut h, Role::Button, "Load index status");
    }
    assert!(has_text(&h, "Current"));
    assert!(
        has_text(&h, "Rust · structural"),
        "language coverage with its grammar is shown"
    );
    // The Indexing screen lists the run.
    h.state_mut().navigate(Route::Indexing);
    settle(&mut h);
    step_until(&mut h, "the runs", |s| s.projects.indexing.runs.loaded);
    assert!(has_text(&h, "Succeeded"));
}
