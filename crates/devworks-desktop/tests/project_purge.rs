//! DW-83 native composition acceptance. No OS keychain, inference or external remote.
mod common;
use common::*;
use devworks_core::{
    ports::storage::{StoredRepository, WriteOptions, read_snapshot, run_write},
    protocol::results::OperationResult,
    protocol::{
        Id,
        records::{ProjectPurgeManifest, PurgePhase},
    },
};
use devworks_desktop::{
    backend::Backend,
    memory::{DraftMemory, Memory},
    ops,
    project_purge::Live,
};
use serde_json::{Value, json};
use std::{process::Command, time::Instant};
fn frame(b: &dyn Backend, operation: &str, body: Value, query: bool) -> Vec<u8> {
    let raw = if query {
        ops::query_frame(b, operation, body)
    } else {
        ops::command_frame(b, operation, body)
    };
    let mut v: Value = serde_json::from_slice(&raw).unwrap();
    v["schemaRevision"] = json!(6);
    serde_json::to_vec(&v).unwrap()
}
fn manifest(result: OperationResult) -> ProjectPurgeManifest {
    match result {
        OperationResult::ProjectsPreparePurge { value }
        | OperationResult::ProjectsPurge { value }
        | OperationResult::ProjectsPurgeStatus { value } => value,
        r => panic!("unexpected {}", r.operation()),
    }
}
fn assert_no_private_records(root: &Root) {
    let db = rusqlite::Connection::open_with_flags(
        root.0.join("workspace.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let tables=db.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' AND sql NOT LIKE 'CREATE VIRTUAL TABLE%' ORDER BY name").unwrap().query_map([],|r|r.get::<_,String>(0)).unwrap().collect::<Result<Vec<_>,_>>().unwrap();
    for table in tables {
        let quoted = table.replace('"', "\"\"");
        let mut stmt = db.prepare(&format!("SELECT * FROM \"{quoted}\"")).unwrap();
        let columns = stmt.column_count();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            for i in 0..columns {
                if let rusqlite::types::ValueRef::Text(text) = row.get_ref(i).unwrap() {
                    assert!(
                        !text.windows(8).any(|w| w == b"PRIVATE-"),
                        "private sentinel retained in {table} column {i}"
                    );
                }
            }
        }
    }
}
fn wait_quiescent(
    engine: &devworks_desktop::host::Engine,
    project: &devworks_core::protocol::ProjectId,
) {
    let deadline = Instant::now() + SETTLE_BOUND;
    loop {
        let inventory = read_snapshot(
            &**engine.host().runtime().store(),
            &Default::default(),
            |r| r.projects().purge_inventory(project),
        )
        .unwrap()
        .unwrap();
        if inventory.unsettled_operations.is_empty() && inventory.unsettled_deliveries.is_empty() {
            return;
        }
        assert!(Instant::now() < deadline, "owned deliveries did not settle");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
#[test]
fn native_review_purge_preserves_shared_git_and_other_drafts_and_recovers_after_restart() {
    let _case = native_runtime_case();
    let root = Root::new("purge-acceptance");
    let git = root.dir("shared-repository");
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(&git)
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(
        git.join("source.txt"),
        b"repository input survives byte exact",
    )
    .unwrap();
    let no_hooks = root.dir("no-hooks");
    let git_run = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&git)
            .arg("-c")
            .arg(format!("core.hooksPath={}", no_hooks.display()))
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    git_run(&["add", "source.txt"]);
    git_run(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "-c",
        "commit.gpgSign=false",
        "commit",
        "--quiet",
        "-m",
        "shared source",
    ]);
    git_run(&["branch", "kept-branch"]);
    let linked = git.parent().unwrap().join("linked-worktree");
    git_run(&[
        "worktree",
        "add",
        "--quiet",
        "--detach",
        linked.to_str().unwrap(),
        "HEAD",
    ]);
    let refs_before = git_run(&["show-ref"]);
    let worktrees_before = git_run(&["worktree", "list", "--porcelain"]);
    let before = Command::new("git")
        .arg("-C")
        .arg(&git)
        .args(["status", "--porcelain=v1", "--branch"])
        .output()
        .unwrap()
        .stdout;
    let engine = start(&root);
    let b = engine.backend();
    let repository = StoredRepository {
        id: Id::new("shared-repository").unwrap(),
        location: git.to_string_lossy().into(),
        name: "shared repo".into(),
        remote_url: None,
        primary: true,
        lifecycle: "Active".into(),
        languages: vec!["rust".into()],
        revision: 1,
        registered_at_ms: 1,
    };
    let inserted = repository.clone();
    run_write(
        &**engine.host().runtime().store(),
        WriteOptions::default(),
        move |tx| tx.projects().insert_repository(&inserted),
    )
    .unwrap();
    let p = ops::project_changed(b.command(
        "projects.create",
        &ops::project_create(
            &*b,
            "PRIVATE-PROJECT-SENTINEL",
            "PRIVATE-DESCRIPTION-SENTINEL",
            &["shared-repository".into()],
        ),
    ))
    .unwrap();
    let other = ops::project_changed(b.command(
        "projects.create",
        &ops::project_create(
            &*b,
            "Independent project",
            "surviving project text",
            &["shared-repository".into()],
        ),
    ))
    .unwrap();
    let save_owned = |bytes: &[u8], title: &str| {
        let path = b.stage_project_text(Some(&p.id), "txt", bytes).unwrap();
        let ticket = b.mint_project_upload(&p.id, &path).unwrap();
        let digest = devworks_core::fingerprint::sha256_hex(bytes);
        let record = ops::saved_artifact(b.command(
            "artifacts.save",
            &ops::save_project_artifact(
                &*b,
                p.id.0.as_str(),
                ticket.as_str(),
                title,
                "text/plain",
                &digest,
                bytes.len() as u64,
            ),
        ))
        .unwrap();
        (path, record.artifact)
    };
    let (private_stage, private_artifact) =
        save_owned(b"PRIVATE-UNSHARED-CONTENT", "PRIVATE-UNSHARED-TITLE");
    let (shared_stage, shared_artifact) = save_owned(
        b"independently retained shared content",
        "PRIVATE-SHARED-HEADER",
    );
    let archived = ops::project_changed(b.command(
        "projects.archive",
        &ops::project_lifecycle(&*b, "projects.archive", &p),
    ))
    .unwrap();
    let deadline = Instant::now() + SETTLE_BOUND;
    loop {
        let pending = read_snapshot(
            &**engine.host().runtime().store(),
            &Default::default(),
            |s| s.projects().pending_archive_stops(100),
        )
        .unwrap();
        if pending.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let deleted = ops::project_changed(b.command(
        "projects.delete",
        &ops::project_lifecycle(&*b, "projects.delete", &archived),
    ))
    .unwrap();
    let mut memory = Memory::default();
    memory.drafts.insert(
        "new".into(),
        DraftMemory {
            text: "independent Home draft".into(),
            ..Default::default()
        },
    );
    let mut live = Live::default();
    live.projects.insert(
        p.id.0.as_str().into(),
        json!({"editor":"PRIVATE-LIVE-SENTINEL"}),
    );
    live.projects.insert(
        other.id.0.as_str().into(),
        json!({"editor":"independent editor"}),
    );
    assert!(
        b.purge_client_publish(memory.clone(), live.clone(), true)
            .unwrap()
            .is_some()
    );
    live.references
        .insert(other.id.0.as_str().into(), vec![shared_artifact.clone()]);
    live.paths
        .insert(other.id.0.as_str().into(), vec![shared_stage.clone()]);
    b.purge_client_publish(memory.clone(), live.clone(), true)
        .unwrap();
    wait_quiescent(&engine, &p.id);
    let prepared = manifest(
        ops::command_result(b.command(
            "projects.prepare_purge",
            &frame(
                &*b,
                "projects.prepare_purge",
                json!({"projectId":p.id,"expectedRevision":deleted.revision}),
                false,
            ),
        ))
        .unwrap(),
    );
    let page=ops::query_result(b.query("projects.purge_manifest",&frame(&*b,"projects.purge_manifest",json!({"projectId":p.id,"manifestId":prepared.id,"expectedRevision":prepared.revision,"page":{"limit":200}}),true))).unwrap().0;
    assert!(matches!(
        page,
        OperationResult::ProjectsPurgeManifest { .. }
    ));
    let confirm = frame(
        &*b,
        "projects.purge",
        json!({"projectId":p.id,"manifestId":prepared.id,"expectedRevision":prepared.revision,"expectedDigest":prepared.digest,"expectedProjectRevision":deleted.revision,"confirm":true}),
        false,
    );
    assert!(
        ops::command_result(b.command("projects.purge", &confirm)).is_err(),
        "generic Human ingress cannot confirm"
    );
    b.purge_client_freeze(p.id.0.as_str(), true).unwrap();
    // The refusal adds durable business evidence; a stale review cannot authorize it.
    let stale = frame(
        &*b,
        "projects.purge",
        json!({"projectId":p.id,"manifestId":prepared.id,"expectedRevision":prepared.revision,"expectedDigest":prepared.digest,"expectedProjectRevision":deleted.revision,"confirm":true}),
        false,
    );
    assert!(ops::command_result(b.confirm_project_purge(&stale)).is_err());
    wait_quiescent(&engine, &p.id);
    let prepared = manifest(
        ops::command_result(b.command(
            "projects.prepare_purge",
            &frame(
                &*b,
                "projects.prepare_purge",
                json!({"projectId":p.id,"expectedRevision":deleted.revision}),
                false,
            ),
        ))
        .unwrap(),
    );

    let confirm = frame(
        &*b,
        "projects.purge",
        json!({"projectId":p.id,"manifestId":prepared.id,"expectedRevision":prepared.revision,"expectedDigest":prepared.digest,"expectedProjectRevision":deleted.revision,"confirm":true}),
        false,
    );
    let original = read_snapshot(
        &**engine.host().runtime().store(),
        &Default::default(),
        |r| r.projects().purge_manifest(&p.id, &prepared.id),
    )
    .unwrap()
    .unwrap();
    let fresh = read_snapshot(
        &**engine.host().runtime().store(),
        &Default::default(),
        |r| {
            r.projects()
                .purge_inventory_with_client_keepers(&p.id, std::slice::from_ref(&shared_artifact))
        },
    )
    .unwrap()
    .unwrap();
    let changed = fresh
        .planning
        .rows
        .iter()
        .filter(|r| {
            r.reason != devworks_core::ports::storage::ProjectPurgeRowReason::PurgeControlAudit
                && !original.rows.contains(r)
        })
        .map(|r| (&r.table, &r.action, &r.reason, &r.key_digest))
        .collect::<Vec<_>>();
    assert_eq!(
        original.policy_digest, fresh.planning.digest,
        "changed rows: {changed:?}"
    );
    let committed = manifest(ops::command_result(b.confirm_project_purge(&confirm)).unwrap());
    assert!(matches!(committed.phase, PurgePhase::RemovingBytes));
    let deadline = Instant::now() + SETTLE_BOUND;
    loop {
        for scope in b
            .purge_client_publish(memory.clone(), live.clone(), true)
            .unwrap()
            .unwrap()
        {
            b.purge_client_acknowledge(&scope);
        }
        let status = manifest(
            ops::query_result(b.query(
                "projects.purge_status",
                &frame(
                    &*b,
                    "projects.purge_status",
                    json!({"projectId":p.id,"manifestId":prepared.id}),
                    true,
                ),
            ))
            .unwrap()
            .0,
        );
        if matches!(status.phase, PurgePhase::Complete) {
            break;
        }
        assert!(Instant::now() < deadline, "purge held: {status:?}");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    b.purge_client_publish(memory, live, true).unwrap();
    let saved = std::fs::read_to_string(root.memory()).unwrap();
    assert!(!saved.contains("PRIVATE-"));
    assert!(saved.contains("independent Home draft"));
    assert!(
        !private_stage.exists(),
        "unshared native staging bytes are removed"
    );
    assert!(
        shared_stage.exists(),
        "an independently picked native path survives"
    );
    assert!(
        ops::read_content(b.query(
            "artifacts.read",
            &ops::artifact_read(&*b, &private_artifact)
        ))
        .is_err()
    );
    match ops::read_content(b.query("artifacts.read", &ops::artifact_read(&*b, &shared_artifact)))
        .unwrap()
    {
        devworks_core::protocol::records::ArtifactReadContent::Text { content, .. } => {
            assert_eq!(content.0, "independently retained shared content")
        }
        other => panic!("{other:?}"),
    }
    let blob = read_snapshot(
        &**engine.host().runtime().store(),
        &Default::default(),
        |r| r.artifacts().blob(private_artifact.sha256.as_str()),
    )
    .unwrap()
    .unwrap();
    assert!(
        !root.0.join(&blob.relative_path).exists(),
        "unshared native blob generation is unlinked"
    );
    let actual = read_snapshot(
        &**engine.host().runtime().store(),
        &Default::default(),
        |s| s.projects().repository(&repository.id),
    )
    .unwrap()
    .unwrap();
    assert_eq!(actual, repository);
    let projects = ops::projects(b.query("projects.list", &ops::projects_list(&*b, None)))
        .unwrap()
        .0;
    assert!(
        projects
            .iter()
            .any(|r| r.id == other.id && r.name.0 == "Independent project")
    );
    assert_no_private_records(&root);
    drop(b);
    drop(engine);
    let engine = start(&root);
    let b = engine.backend();
    let status = manifest(
        ops::query_result(b.query(
            "projects.purge_status",
            &frame(
                &*b,
                "projects.purge_status",
                json!({"projectId":p.id,"manifestId":prepared.id}),
                true,
            ),
        ))
        .unwrap()
        .0,
    );
    assert!(matches!(status.phase, PurgePhase::Complete));
    assert_eq!(git_run(&["show-ref"]), refs_before);
    assert_eq!(
        git_run(&["worktree", "list", "--porcelain"]),
        worktrees_before
    );
    assert_eq!(
        std::fs::read(linked.join("source.txt")).unwrap(),
        b"repository input survives byte exact"
    );
    assert_eq!(
        std::fs::read(git.join("source.txt")).unwrap(),
        b"repository input survives byte exact"
    );
    assert_eq!(
        Command::new("git")
            .arg("-C")
            .arg(&git)
            .args(["status", "--porcelain=v1", "--branch"])
            .output()
            .unwrap()
            .stdout,
        before
    );
}

#[test]
fn native_confirmation_ui_removes_owned_session_drafts_and_paste_keeps_home_and_requires_review() {
    use devworks_desktop::route::Route;
    use eframe::egui::accesskit::Role;
    use egui_kittest::kittest::{NodeT, Queryable};
    let _case = native_runtime_case();
    let root = Root::new("purge-ui");
    let engine = start(&root);
    let b = engine.backend();
    let p = ops::project_changed(b.command(
        "projects.create",
        &ops::project_create(&*b, "PRIVATE-UI-PROJECT", "PRIVATE-UI-DESCRIPTION", &[]),
    ))
    .unwrap();
    let mut start_frame: Value =
        serde_json::from_slice(&ops::session_start(&*b, "PRIVATE-UI-SESSION")).unwrap();
    start_frame["body"]["scope"] = json!({"kind":"Project","projectId":p.id});
    let session = match ops::command_result(
        b.command("sessions.start", &serde_json::to_vec(&start_frame).unwrap()),
    )
    .unwrap()
    {
        OperationResult::SessionsStart { value } => value,
        r => panic!("{}", r.operation()),
    };
    let paste = b.stage_pasted_image(Some(&session.id), &png()).unwrap();
    let archived = ops::project_changed(b.command(
        "projects.archive",
        &ops::project_lifecycle(&*b, "projects.archive", &p),
    ))
    .unwrap();
    let deadline = Instant::now() + SETTLE_BOUND;
    loop {
        if read_snapshot(
            &**engine.host().runtime().store(),
            &Default::default(),
            |r| r.projects().pending_archive_stops(100),
        )
        .unwrap()
        .is_empty()
        {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    ops::project_changed(b.command(
        "projects.delete",
        &ops::project_lifecycle(&*b, "projects.delete", &archived),
    ))
    .unwrap();
    let mut memory = Memory::default();
    memory.drafts.insert(
        session.id.as_str().into(),
        DraftMemory {
            text: "PRIVATE-UI-DRAFT".into(),

            ..Default::default()
        },
    );
    memory.drafts.insert(
        "new".into(),
        DraftMemory {
            text: "independent Home text".into(),
            ..Default::default()
        },
    );
    memory.save(&root.memory()).unwrap();
    // The host was composed before this deliberate disk fixture: publish the live starting state.
    b.purge_client_publish(memory, Live::default(), true)
        .unwrap();
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 1000.0);
    settle(&mut h);
    h.state_mut().navigate(Route::Portfolio);
    settle(&mut h);
    click(&mut h, Role::Button, "Deleted");
    h.query_all_by_label_contains("PRIVATE-UI-PROJECT")
        .find(|n| {
            n.accesskit_node()
                .label()
                .is_some_and(|l| l.starts_with("PRIVATE-UI-PROJECT  ·"))
        })
        .unwrap()
        .click();
    settle(&mut h);
    wait_quiescent(&engine, &p.id);
    click(&mut h, Role::Button, "Review permanent purge");
    step_until(&mut h, "reviewed manifest", |s| {
        s.purge.loaded || s.purge.error.is_some()
    });
    assert!(
        h.state().purge.error.is_none(),
        "{:?}",
        h.state().purge.error
    );
    assert!(!h.state().purge.acknowledged);
    assert!(paste.exists());
    click(
        &mut h,
        Role::CheckBox,
        "I reviewed every entry and want to permanently purge this project",
    );
    click(&mut h, Role::Button, "Permanently purge");
    step_until(&mut h, "native client cleanup", |s| {
        s.purge.cleanup_acknowledged || s.purge.error.is_some()
    });
    assert!(
        h.state().purge.error.is_none(),
        "{:?}",
        h.state().purge.error
    );
    let deadline = Instant::now() + SETTLE_BOUND;
    loop {
        let m = h.state().purge.manifest.clone().unwrap();
        let status = manifest(
            ops::query_result(b.query(
                "projects.purge_status",
                &frame(
                    &*b,
                    "projects.purge_status",
                    json!({"projectId":m.project_id,"manifestId":m.id}),
                    true,
                ),
            ))
            .unwrap()
            .0,
        );
        if matches!(status.phase, PurgePhase::Complete) {
            break;
        }
        assert!(Instant::now() < deadline, "{status:?}");
        h.step();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(!paste.exists());
    assert!(!h.state().chat.drafts.contains_key(session.id.as_str()));
    assert_eq!(h.state().chat.drafts["new"].text, "independent Home text");
    let saved = std::fs::read_to_string(root.memory()).unwrap();
    assert!(!saved.contains("PRIVATE-UI-"));
    assert!(saved.contains("independent Home text"));
    click(&mut h, Role::Button, "Close review");
    assert!(!h.state().purge.frozen);
    assert_no_private_records(&root);
    h.state_mut().on_exit();
    drop(h);
    drop(b);
    drop(engine);
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1440.0, 1000.0);
    settle(&mut h);
    h.state_mut().navigate(Route::Portfolio);
    settle(&mut h);
    click(&mut h, Role::Button, "Deleted");
    h.query_all_by_label_contains("Deleted project")
        .find(|n| {
            n.accesskit_node()
                .label()
                .is_some_and(|l| l.starts_with("Deleted project  ·"))
        })
        .unwrap()
        .click();
    settle(&mut h);
    click(&mut h, Role::Button, "Review permanent purge");
    step_until(&mut h, "restored purge status", |s| {
        s.purge
            .manifest
            .as_ref()
            .is_some_and(|m| matches!(m.phase, PurgePhase::Complete))
            || s.purge.error.is_some()
    });
    assert!(
        h.state().purge.error.is_none(),
        "{:?}",
        h.state().purge.error
    );
    assert!(
        !h.state().purge.acknowledged,
        "restoration never creates new Human consent"
    );
}
