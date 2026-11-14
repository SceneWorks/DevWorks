//! Watchers in the desktop app (DW-101, WATCH-09) over the real runtime, headless: the Watchers
//! screen lists every project's watchers with owner attribution, source, state, elapsed time,
//! last poll and output tail; a feature's detail lists only its agents' watchers and the chat
//! sidebar's Current tab only the selected conversation's; a watcher is cancelled from a view;
//! and a watcher's event appears in the conversation that owns it. Watchers are seeded as rows
//! the watcher service then advances and delivers. No inference.
mod common;

use common::*;
use devworks_core::ports::storage::{
    ReadOptions, StoredSession, StoredWatcher, WatcherPhase, WriteOptions, read_snapshot, run_write,
};
use devworks_core::protocol::{ActorId, Id, ProjectId, RootId};
use devworks_core::services::work::workflow::default_status_id;

use devworks_desktop::ops;
use devworks_desktop::route::Route;
use devworks_desktop::shell::Shell;
use devworks_desktop::work_ops as wops;
use eframe::egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use serde_json::json;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// An open timer watcher of `session`, due `due_in_ms` from now.
fn timer(
    id: &str,
    session: &str,
    actor: &ActorId,
    project: Option<&ProjectId>,
    due_in_ms: i64,
) -> StoredWatcher {
    let now = now_ms();
    StoredWatcher {
        id: Id::new(id).unwrap(),
        revision: 1,
        phase: WatcherPhase::Active,
        source_kind: "Timer".into(),
        project_id: project.cloned(),
        session_id: Id::new(session).unwrap(),
        actor: actor.clone(),
        process_operation_id: None,
        executable_ref: None,
        tail_bytes: 64,
        outcome: None,
        duration_ms: None,
        output_tail: Some(format!("{id} compiling\n{id} tests passed\n").into_bytes()),
        output_truncated: false,
        stop_requested: false,
        created_at_ms: now - 65_000,
        updated_at_ms: now,
        settled_at_ms: None,
        probe: None,
        due_at_ms: Some(now + due_in_ms),
        expires_at_ms: None,
        matching: None,
        file_path: None,
    }
}

fn seed(
    engine: &devworks_desktop::host::Engine,
    f: impl FnOnce(
        &mut dyn devworks_core::ports::storage::WriteTransaction,
    ) -> Result<(), devworks_core::ports::storage::StorageError>
    + Send
    + 'static,
) {
    run_write(
        &**engine.host().runtime().store(),
        WriteOptions::default(),
        f,
    )
    .expect("seeded rows");
}

fn watcher_phase(engine: &devworks_desktop::host::Engine, id: &str) -> WatcherPhase {
    let id = Id::new(id).unwrap();
    read_snapshot(
        &**engine.host().runtime().store(),
        &ReadOptions::default(),
        move |read| read.watchers().watcher(&id),
    )
    .unwrap()
    .expect("the watcher")
    .phase
}

/// Every shown text.
fn texts(h: &Harness<'_, Shell>) -> Vec<String> {
    h.query_all(egui_kittest::kittest::by())
        .filter_map(|n| {
            let node = n.accesskit_node();
            node.label().or_else(|| node.value())
        })
        .collect()
}

/// Runs frames (the lists read again every few seconds) until `text` is shown.
fn shown(h: &mut Harness<'_, Shell>, text: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !has_text(h, text) {
        assert!(
            std::time::Instant::now() < deadline,
            "{text:?} was not shown"
        );
        std::thread::sleep(Duration::from_millis(50));
        h.step();
        settle(h);
    }
}

#[test]
fn watchers_are_listed_attributed_cancelled_and_reported_in_their_conversation() {
    let _case = native_runtime_case();
    let root = Root::new("watchers");
    let engine = start(&root);
    let backend = engine.backend();
    let project = ops::project_changed(backend.command(
        "projects.create",
        &ops::project_create(&*backend, "Shop", "", &[]),
    ))
    .unwrap();
    let pid = project.id.0.as_str().to_owned();
    let backlog = default_status_id(&project.id, "backlog");
    let feature = wops::work_changed(backend.command(
        "features.create",
        &wops::work_command(
            &*backend,
            "features.create",
            json!({"projectId": pid, "creation": {"kind": "Single", "title": "Checkout",
                "description": "", "repositoryIds": [], "statusId": backlog, "priority": 1}}),
        ),
    ))
    .unwrap();
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1000.0);
    settle(&mut h);
    click(&mut h, Role::Button, "New Chat");
    // The Home conversation owns one watcher; an agent working on the feature owns another.
    let home = h
        .state()
        .chat
        .selected_session()
        .expect("the Home conversation")
        .id
        .as_str()
        .to_owned();
    // Sessions name stored actors: the agent is the Home conversation's.
    let agent = {
        let id = Id::new(home.clone()).unwrap();
        read_snapshot(
            &**engine.host().runtime().store(),
            &ReadOptions::default(),
            move |read| read.sessions().session(&id),
        )
        .unwrap()
        .expect("the stored Home session")
        .agent_id
    };
    let agent_session = StoredSession {
        id: Id::new("feature-agent-session").unwrap(),
        conversation_id: Id::new("feature-agent-conversation").unwrap(),
        agent_id: agent.clone(),
        project_id: Some(project.id.clone()),
        root_id: Some(RootId(feature.id.clone())),
        title: "Builder".into(),
        context_epoch: 0,
        revision: 1,
        created_by: agent.clone(),
        created_at_ms: now_ms(),
        closed_at_ms: None,
        closed_by: None,
        next_sequence: 1,
        checkpoint_sequence: 0,
    };
    let in_project = timer(
        "watch-feature",
        "feature-agent-session",
        &agent,
        Some(&project.id),
        3_600_000,
    );
    let in_home = timer("watch-home", &home, &agent, None, 3_600_000);
    seed(&engine, move |tx| {
        tx.sessions().insert_session(&agent_session)?;
        tx.watchers().insert_watcher(&in_project)?;
        tx.watchers().insert_watcher(&in_home)
    });

    // The Watchers screen lists both, newest first, with who owns them and what they show.
    click_nav(&mut h, "Watchers");
    assert_eq!(h.state().route(), Route::Watchers);
    shown(&mut h, "watch-feature tests passed");
    assert!(has_text(&h, "watch-home tests passed"));
    let shown_labels = texts(&h);
    let owner = shown_labels
        .iter()
        .find(|l| l.contains("Feature “Checkout”"))
        .unwrap_or_else(|| panic!("the feature watcher owner: {shown_labels:?}"));
    assert!(owner.contains("Project Shop"), "{owner}");
    assert!(
        owner.contains(&format!("Agent {}", agent.0.as_str())),
        "{owner}"
    );
    assert!(
        shown_labels
            .iter()
            .any(|l| l.starts_with("No project") && l.contains("Agent")),
        "{shown_labels:?}"
    );
    assert!(
        shown_labels
            .iter()
            .filter(|l| l.as_str() == "Timer")
            .count()
            >= 2,
        "{shown_labels:?}"
    );
    assert!(
        shown_labels
            .iter()
            .filter(|l| l.as_str() == "Watching")
            .count()
            >= 2
    );
    assert!(
        shown_labels.iter().any(|l| l.starts_with("Elapsed 1 m")),
        "{shown_labels:?}"
    );
    // The watcher service polls them; the next read shows when.
    shown(&mut h, "last poll");

    // A feature's detail lists only its agents' watchers.
    h.state_mut().select_project(Some(pid.clone()));
    h.state_mut().navigate(Route::Features);
    settle(&mut h);
    h.query_all_by_label_contains("Checkout  ·")
        .next()
        .expect("the feature row")
        .click();
    settle(&mut h);
    shown(&mut h, "watch-feature tests passed");
    assert!(!has_text(&h, "watch-home tests passed"));

    // Cancel from there: the watcher stops observing and the row says so.
    click(&mut h, Role::Button, "Cancel watcher");
    shown(&mut h, "Cancelled");
    assert_eq!(
        watcher_phase(&engine, "watch-feature"),
        WatcherPhase::Cancelled
    );
    assert_eq!(watcher_phase(&engine, "watch-home"), WatcherPhase::Active);

    // The Home conversation's Current tab lists only its own watchers.
    h.state_mut().navigate(Route::Chat);
    settle(&mut h);
    click(&mut h, Role::Button, "Current");
    shown(&mut h, "watch-home tests passed");
    assert!(!has_text(&h, "watch-feature tests passed"));

    // A watcher that fires delivers its event into the conversation that owns it.
    let soon = timer("watch-soon", &home, &agent, None, 500);
    seed(&engine, move |tx| tx.watchers().insert_watcher(&soon));
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !texts(&h).iter().any(|l| l.contains("watch-soon")) {
        assert!(std::time::Instant::now() < deadline, "no watcher notice");
        std::thread::sleep(Duration::from_millis(100));
        h.step();
        settle(&mut h);
    }
    h.state_mut().on_exit();
}
