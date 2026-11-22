//! Manually scoped backup and restore through the real native gateway and SQLite owners.
mod common;
use common::*;
use devworks_core::ports::storage::{ArchiveAction, ArchivePhase, Cancellation, WorkspaceArchive};
use devworks_core::protocol::CommandReply;
use devworks_core::protocol::results::{DomainDecision, OperationCompletion, OperationResult};
use devworks_desktop::backend::Backend;
use devworks_desktop::frame::fixture;
use devworks_desktop::host::Engine;
use devworks_desktop::ops;
use eframe::egui::accesskit::Role;
use rusqlite::Connection;
use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};

fn storage_title(root: &Root) -> String {
    Connection::open(root.0.join("workspace.sqlite"))
        .unwrap()
        .query_row(
            "SELECT title FROM projects WHERE id='archive-project'",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

fn active_checkpoint(app: &Engine) -> i64 {
    app.host()
        .runtime()
        .workspace_archives()
        .unwrap()
        .shipped_schema()
        .unwrap()
}

fn await_quiet(app: &Engine, root: &Root) {
    let deadline = Instant::now() + SETTLE_BOUND;
    loop {
        let start = root.write_progress();
        std::thread::sleep(Duration::from_millis(300));
        if root.write_progress() == start {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "native app storage never settled"
        );
        assert!(active_checkpoint(app) > 0);
    }
}

fn make_backup(b: &dyn Backend, destination: &Path) -> Vec<u8> {
    let (grant, sequence) = b
        .workspace_backup_folder(destination, b.control_epoch())
        .unwrap();
    fixture::command(
        b,
        "workspace.backup",
        json!({
            "destinationGrantId": grant, "expectedStoreSequence": sequence.to_string()
        }),
    )
}

fn archive_command(
    b: &dyn Backend,
    operation: &str,
    input: &[u8],
) -> devworks_core::gateway::CommandOutcome {
    b.workspace_archive_command(operation, input, b.control_epoch())
}

fn accepted_backup(
    b: &dyn Backend,
    destination: &Path,
) -> (devworks_core::protocol::OperationId, Vec<u8>) {
    let mut remaining = 4;
    loop {
        let frame = make_backup(b, destination);
        let outcome = archive_command(b, "workspace.backup", &frame);
        let observed = format!("{outcome:?}");
        match ops::accepted_decision(outcome) {
            Ok(id) => return (id, frame),
            Err(ops::Failure::Refused(fault))
                if remaining > 0
                    && fault.code == devworks_core::protocol::DomainErrorCode::SourceChanged =>
            {
                remaining -= 1;
            }
            Err(error) => panic!("{error:?}: {observed}"),
        }
    }
}

fn read_manifest(bundle: &Path) -> WorkspaceArchive {
    serde_json::from_slice(&std::fs::read(bundle.join("manifest.json")).unwrap()).unwrap()
}

fn refresh_manifest(bundle: &Path, archive: &mut WorkspaceArchive) {
    let path = bundle.join("workspace.sqlite");
    {
        let db = Connection::open(&path).unwrap();
        let mut query = db.prepare(
            "SELECT type,name,tbl_name,sql FROM main.sqlite_schema
             WHERE NOT (type='table' AND name=tbl_name AND (
                  (name='sqlite_stat1' AND COALESCE(sql,'')='CREATE TABLE sqlite_stat1(tbl,idx,stat)')
                  OR (name='sqlite_stat4' AND COALESCE(sql,'')='CREATE TABLE sqlite_stat4(tbl,idx,neq,nlt,ndlt,sample)')
             )) ORDER BY type,name,tbl_name"
        ).unwrap();
        let rows = query
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        archive.checkpoint.schema_digest =
            devworks_core::fingerprint::sha256_hex(&serde_json::to_vec(&rows).unwrap());
    }
    let bytes = std::fs::read(path).unwrap();
    archive.database.bytes = bytes.len() as u64;
    archive.database.sha256 = devworks_core::fingerprint::sha256_hex(&bytes);
    std::fs::write(
        bundle.join("manifest.json"),
        serde_json::to_vec(archive).unwrap(),
    )
    .unwrap();
}

#[test]
fn native_backup_restore_keeps_snapshot_and_prior_profile_with_real_managed_bytes() {
    let _case = native_runtime_case();
    let root = Root::new("workspace-archive-roundtrip");
    let app = start(&root);
    let backend = app.backend();
    let b = backend.as_ref();
    let connection = Connection::open(root.0.join("workspace.sqlite")).unwrap();
    connection.busy_timeout(Duration::from_secs(5)).unwrap();
    connection
        .execute_batch(
            "INSERT INTO projects(id,title,lifecycle,revision,scope_revision,created_at)
         VALUES('archive-project','Checkpoint authorship','Active',1,1,1700000000000)",
        )
        .unwrap();
    let input = root.file("original.txt", b"These are the saved bytes.");
    let ticket = b.mint_workspace_upload(&input).unwrap();
    let bytes = b"These are the saved bytes.";
    let save = ops::save_workspace_artifact(
        b,
        ticket.as_str(),
        "Checkpoint file",
        "text/plain",
        &devworks_core::fingerprint::sha256_hex(bytes),
        bytes.len() as u64,
        &[],
    );
    let saved = ops::saved_artifact(b.command("artifacts.save", &save)).expect("save real file");
    await_quiet(&app, &root);
    let destination = root.dir("export");

    let unconfirmed = make_backup(b, &destination);
    assert!(matches!(
        b.command("workspace.backup", &unconfirmed),
        Ok(CommandReply::Recorded { receipt, .. }) if matches!(receipt.decision, DomainDecision::Refused{..})
    ));

    let (op, backup) = accepted_backup(b, &destination);
    let deadline = Instant::now() + SETTLE_BOUND;
    let record = loop {
        if let Some(record) = b.workspace_archive_status().unwrap().current {
            assert_ne!(record.phase, ArchivePhase::Failed, "{:?}", record.detail);
            if record.phase == ArchivePhase::Published {
                break record;
            }
        }
        assert!(Instant::now() < deadline, "backup must publish");
        std::thread::sleep(Duration::from_millis(35));
    };
    assert!(record.manifest.is_some());
    assert!(matches!(
        b.command("workspace.backup", &backup),
        Ok(CommandReply::Recorded { replayed: true, .. })
    ));
    let bundle = destination.join(format!("devworks-backup-{}", op.0.as_str()));
    assert!(bundle.join("manifest.json").is_file());
    let mut metadata = read_manifest(&bundle);
    {
        let external = Connection::open(bundle.join("workspace.sqlite")).unwrap();
        external
            .execute_batch("CREATE VIEW unexpected_archive_view AS SELECT 1;")
            .unwrap();
    }
    refresh_manifest(&bundle, &mut metadata);
    let rejected = b
        .workspace_restore_plan(&bundle, b.control_epoch(), &Cancellation::default())
        .unwrap_err();
    assert!(rejected.contains("schema"), "{rejected}");

    // Create an exact schema-68 bundle using the captured current file and the previously
    // shipped ledger entry. The user-facing migration can only restore the compiled 69th step.
    {
        let external = Connection::open(bundle.join("workspace.sqlite")).unwrap();
        external
            .execute_batch(
                "DROP VIEW unexpected_archive_view;
             DROP TABLE workspace_restore_effects;
             DROP TABLE workspace_restore_history;
             DROP TABLE workspace_archive_runs;
             DROP TABLE workspace_backup_folders;
             DELETE FROM schema_migrations WHERE version=69;
             PRAGMA user_version=68;",
            )
            .unwrap();
        let latest = metadata.checkpoint.migrations.pop().unwrap();
        assert_eq!(latest.version, 69);
        let previous = metadata.checkpoint.migrations.last().unwrap();
        metadata.checkpoint.schema_version = previous.version;
        metadata.checkpoint.minimum_reader = previous.minimum_reader;
        metadata.checkpoint.minimum_writer = previous.minimum_writer;
        external
            .execute(
                "UPDATE store_meta SET schema_version=?1,minimum_reader=?2,minimum_writer=?3",
                rusqlite::params![
                    previous.version,
                    previous.minimum_reader,
                    previous.minimum_writer
                ],
            )
            .unwrap();
    }
    refresh_manifest(&bundle, &mut metadata);
    connection
        .execute(
            "UPDATE projects SET title='Edit after backup' WHERE id='archive-project'",
            [],
        )
        .unwrap();
    drop(connection);
    let proposal = b
        .workspace_restore_plan(&bundle, b.control_epoch(), &Cancellation::default())
        .unwrap();
    let ArchiveAction::Restore { archive, .. } = &proposal.action else {
        panic!("not a restore")
    };
    assert!(
        archive
            .checkpoint
            .blobs
            .iter()
            .any(|entry| entry.content.bytes == 26)
    );
    let migration_manifest = proposal.migration_manifest.as_ref().unwrap();
    let migrate = fixture::command(
        b,
        "workspace.migrate",
        json!({
            "migrationManifest": migration_manifest,
            "expectedSchemaRevision": (archive.checkpoint.schema_version).to_string(),
            "proposalId": proposal.id,
        }),
    );
    let no_migration = fixture::command(
        b,
        "workspace.restore",
        json!({
            "backup":proposal.manifest,"expectedStoreId":proposal.store,
            "expectedRestoreEpoch":proposal.epoch.to_string(),"proposalId":proposal.id,
        }),
    );
    assert!(
        ops::accepted_decision(archive_command(b, "workspace.restore", &no_migration)).is_err()
    );
    let applied = ops::command_result(archive_command(b, "workspace.migrate", &migrate)).unwrap();
    assert!(matches!(applied, OperationResult::WorkspaceMigrate{value} if value.changed));
    assert!(matches!(
        b.command("workspace.migrate", &migrate),
        Ok(CommandReply::Recorded { replayed: true, .. })
    ));
    let decline = fixture::command(
        b,
        "workspace.restore",
        json!({
            "backup":proposal.manifest,"expectedStoreId":proposal.store,
            "expectedRestoreEpoch":(proposal.epoch + 1).to_string(),"proposalId":proposal.id,
        }),
    );
    assert!(ops::accepted_decision(archive_command(b, "workspace.restore", &decline)).is_err());
    let confirm = fixture::command(
        b,
        "workspace.restore",
        json!({
            "backup":proposal.manifest,"expectedStoreId":proposal.store,
            "expectedRestoreEpoch":proposal.epoch.to_string(),"proposalId":proposal.id,
        }),
    );
    ops::accepted_decision(archive_command(b, "workspace.restore", &confirm))
        .expect("restore accepted");
    assert_eq!(storage_title(&root), "Edit after backup");
    b.request_workspace_restore_shutdown(&proposal.id, b.control_epoch())
        .unwrap();
    drop(backend);
    assert!(app.shutdown().is_some());
    assert!(
        root.0.join("restore-pending.json").exists(),
        "successful close arms restore"
    );
    let reopened = start(&root);
    let status = reopened.backend().workspace_archive_status().unwrap();
    let restored = status
        .restored
        .expect("verify restored workspace in Settings");
    assert_eq!(restored.epoch, proposal.epoch + 1);
    let completion = restored
        .completion
        .as_ref()
        .expect("qualified native restore result");
    assert!(matches!(completion, OperationCompletion::Succeeded {
        result: OperationResult::WorkspaceRestore { value }, ..
    } if value.schema_version.get() == 69 && value.id == proposal.id));
    assert_eq!(storage_title(&root), "Checkpoint authorship");
    let prior = root
        .0
        .join("workspace-archives")
        .join(restored.prior.operation_id.as_str())
        .join("workspace.sqlite");
    let prior_title: String = Connection::open(prior)
        .unwrap()
        .query_row(
            "SELECT title FROM projects WHERE id='archive-project'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(prior_title, "Edit after backup");
    let native = reopened.host().runtime();
    let (bytes, _) = native
        .artifacts()
        .read_exact(
            native.store().as_ref(),
            reopened.host().user(),
            &saved.artifact,
            128,
        )
        .unwrap();
    assert_eq!(bytes, b"These are the saved bytes.");
    drop(reopened);
    let again = start(&root);
    assert_eq!(
        again
            .backend()
            .workspace_archive_status()
            .unwrap()
            .restored
            .unwrap()
            .epoch,
        restored.epoch
    );
}

#[test]
fn active_migration_has_an_exact_no_change_receipt_and_native_settings_entry() {
    let _case = native_runtime_case();
    let root = Root::new("workspace-schema-check");
    let app = start(&root);
    let backend = app.backend();
    let b = backend.as_ref();
    let plan = b.workspace_migration_plan(b.control_epoch()).unwrap();
    let schema = active_checkpoint(&app);
    let migration = |version: i64| {
        fixture::command(
            b,
            "workspace.migrate",
            json!({
                "migrationManifest": plan.migration_manifest,"expectedSchemaRevision":version.to_string(),
            }),
        )
    };
    let wrong = migration(schema - 1);
    assert!(ops::command_result(archive_command(b, "workspace.migrate", &wrong)).is_err());
    let exact = migration(schema);
    let outcome = ops::command_result(archive_command(b, "workspace.migrate", &exact)).unwrap();
    assert!(matches!(outcome, OperationResult::WorkspaceMigrate{value} if !value.changed));
    assert!(matches!(
        b.command("workspace.migrate", &exact),
        Ok(CommandReply::Recorded { replayed: true, .. })
    ));

    let mut h = harness(shell(&app, &root, Script::default()), 1320.0, 1050.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    click(&mut h, Role::Button, "Workspace backup and restore");
    assert!(has_text(&h, "Verify current migration ledger"));
    assert!(has_text(&h, "Choose backup to restore"));
}
