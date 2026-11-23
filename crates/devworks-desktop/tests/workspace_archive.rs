//! Manually scoped backup and restore through the real native gateway and SQLite owners.
mod common;
use common::*;
use devworks_core::ports::secrets::{Secret, SecretError, SecretKey, SecretStore, SessionSecrets};
use devworks_core::ports::storage::{ArchiveAction, ArchivePhase, Cancellation, WorkspaceArchive};
use devworks_core::protocol::records::{DocumentResult, McpClientAccess};
use devworks_core::protocol::results::{DomainDecision, OperationCompletion, OperationResult};
use devworks_core::protocol::{CommandReply, Id, ProjectId, Revision};
use devworks_core::services::mcp_inbound::{Binding, CredentialError, Credentials};
use devworks_desktop::backend::Backend;
use devworks_desktop::frame::fixture;
use devworks_desktop::host::Engine;
use devworks_desktop::ops;
use eframe::egui::accesskit::Role;
use rusqlite::Connection;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
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

struct McpVault(Arc<SessionSecrets>);
impl SecretStore for McpVault {
    fn name(&self) -> &'static str {
        self.0.name()
    }
    fn put(&self, k: &SecretKey, v: &Secret) -> Result<(), SecretError> {
        self.0.put(k, v)
    }
    fn get(&self, k: &SecretKey) -> Result<Option<Secret>, SecretError> {
        self.0.get(k)
    }
    fn delete(&self, k: &SecretKey) -> Result<bool, SecretError> {
        self.0.delete(k)
    }
    fn list(&self, namespace: &str) -> Result<Vec<Id>, SecretError> {
        self.0.list(namespace)
    }
}

fn apply_mcp(b: &dyn Backend, name: &str, body: serde_json::Value) -> OperationResult {
    ops::command_result(b.command(name, &fixture::command(b, name, body))).unwrap()
}

fn configure_mcp(b: &dyn Backend, revision: &str) {
    apply_mcp(
        b,
        "mcp_server.configure",
        json!({"expectedRevision":revision,"enabled":true,"bindings":["Stdio"]}),
    );
}

fn allow_document_commands(b: &dyn Backend, registration: &McpClientAccess) -> McpClientAccess {
    let OperationResult::McpServerSetClientAccess { value } = apply_mcp(
        b,
        "mcp_server.set_client_access",
        json!({"action":{
            "kind":"Replace","clientId":registration.client_id,"expectedRevision":registration.revision,
            "name":registration.name,"enabled":true,"projectIds":["archive-project"],
            "repositoryIds":[],"worktreeIds":[],"readOperations":[],
            "commandOperations":["documents.import","documents.disable_retrieval","documents.remove_index","documents.index","documents.retry"]
        }}),
    ) else {
        panic!("MCP grant reply")
    };
    value
}

fn credentials(app: &Engine, vault: &Arc<SessionSecrets>) -> Credentials {
    Credentials::new(
        app.host().runtime().store().clone(),
        Box::new(McpVault(vault.clone())),
        Arc::new(devworks_adapters::OsEntropy),
    )
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

    configure_mcp(b, "1");
    let OperationResult::McpServerSetClientAccess { value: registered } = apply_mcp(
        b,
        "mcp_server.set_client_access",
        json!({"action":{"kind":"Register","name":"Restore probe"}}),
    ) else {
        panic!("register reply")
    };
    let pairing = allow_document_commands(b, &registered);
    let vault = Arc::new(SessionSecrets::new());
    let project = ProjectId(Id::new("archive-project").unwrap());
    let ticket = b.mint_project_upload(&project, &input).unwrap();
    let saved_for_project = ops::saved_artifact(b.command(
        "artifacts.save",
        &ops::save_project_artifact(
            b,
            project.0.as_str(),
            ticket.as_str(),
            "Import source",
            "text/plain",
            &devworks_core::fingerprint::sha256_hex(bytes),
            bytes.len() as u64,
        ),
    ))
    .unwrap();
    {
        let secrets = credentials(&app, &vault);
        let token = secrets
            .provision(app.host().user(), &pairing.client_id, &pairing.revision)
            .unwrap();
        let proof = secrets.authenticate(Binding::LocalIpc, &token).unwrap();
        let context = proof.invocation(
            Id::new("native-doc-restore").unwrap(),
            project.clone(),
            Revision::new(1).unwrap(),
        );
        let frame = fixture::command(
            b,
            "documents.import",
            json!({
                "scope":{"kind":"Project","projectId":project},"source":saved_for_project.artifact,
                "title":"Preserved document",
            }),
        );
        let import = || {
            app.host()
                .runtime()
                .command(&context, "documents.import", &frame)
        };
        let result = ops::command_result(import()).unwrap();
        let OperationResult::DocumentsImport {
            value: DocumentResult::Legacy(initial),
        } = result
        else {
            panic!("typed legacy document import reply")
        };
        assert!(matches!(
            import(),
            Ok(CommandReply::Recorded { replayed: true, .. })
        ));

        let change = |operation: &str, kind, revision| {
            let frame = json!({
                "protocol":"1.0-draft.1", "requestKey":b.token("document-access"),
                "command":{"kind":kind,"artifactId":initial.id,"expectedRevision":revision}
            });
            app.host()
                .runtime()
                .command(&context, operation, frame.to_string().as_bytes())
        };
        let OperationResult::DocumentsDisableRetrieval {
            value: DocumentResult::Legacy(disabled),
        } = ops::command_result(change(
            "documents.disable_retrieval",
            "DisableRetrieval",
            &initial.revision,
        ))
        .unwrap()
        else {
            panic!("typed disable reply")
        };
        assert!(!disabled.retrieval_enabled);
        assert_ne!(disabled.revision, initial.revision);
        assert!(matches!(
            ops::command_result(change(
                "documents.remove_index",
                "RemoveIndex",
                &disabled.revision
            )),
            Ok(OperationResult::DocumentsRemoveIndex { .. })
        ));
        // A stale frame must fail before the service can start extraction or inference.
        for operation in ["documents.index", "documents.retry"] {
            let frame = fixture::command(
                b,
                operation,
                json!({
                    "documentId":initial.id, "versionId":initial.current_version.version_id,
                    "expectedRevision":initial.revision
                }),
            );
            assert!(matches!(
                app.host().runtime().command(&context, operation, &frame),
                Ok(CommandReply::Recorded { receipt, .. })
                    if matches!(&receipt.decision, DomainDecision::Refused { fault }
                        if fault.code == devworks_core::protocol::DomainErrorCode::Conflict)
            ));
        }
    }
    let original_token = credentials(&app, &vault)
        .provision(app.host().user(), &pairing.client_id, &pairing.revision)
        .unwrap();
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
    let published = destination.join(format!("devworks-backup-{}", op.0.as_str()));
    assert!(published.join("manifest.json").is_file());
    let bundle = root.dir("transferred-bundle");
    for child in std::fs::read_dir(&published).unwrap() {
        let child = child.unwrap();
        std::fs::copy(child.path(), bundle.join(child.file_name())).unwrap();
    }
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

    // Preserve the captured authored data in an exact schema-75 bundle so restore must
    // obtain separate approval to run the compiled Dreaming migration.
    {
        let external = Connection::open(bundle.join("workspace.sqlite")).unwrap();
        external
            .execute_batch(
                "DROP VIEW unexpected_archive_view;
             DROP TRIGGER dreaming_source_observed;
             DROP TRIGGER dreaming_source_replaced;
             DROP TRIGGER dreaming_repository_published;
             DROP INDEX dreaming_knowledge_events;
             DROP INDEX dreaming_review_events;
             DROP INDEX dreaming_claim_scope;
             DROP INDEX dreaming_proposal_target;
             DROP TABLE dreaming_reversions;
             DROP TABLE dreaming_actions;
             DROP TABLE dreaming_runs;
             DROP TABLE dreaming_source_changes;
             DROP TABLE dreaming_instruction_revisions;
             DROP TABLE dreaming_scope_state;
             DELETE FROM schema_migrations WHERE version=76;
             PRAGMA user_version=75;",
            )
            .unwrap();
        let latest = metadata.checkpoint.migrations.pop().unwrap();
        assert_eq!(latest.version, 76);
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
    // Lost settlement after publication leaves a real, exact export on disk.
    connection
        .execute(
            "UPDATE operations SET state='OutcomeUnknown' WHERE id=?1",
            [op.0.as_str()],
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
    let backend = reopened.backend();
    let status = backend.workspace_archive_status().unwrap();
    assert!(matches!(
        backend.command("workspace.restore", &confirm),
        Ok(CommandReply::Recorded { replayed: true, .. })
    ));
    let (export, effect) = status
        .unresolved
        .iter()
        .find(|(_, effect)| effect.operation == op)
        .expect("the prior destination export remains unresolved");
    assert_eq!(effect.timeline, "destination");
    let wrong = root.dir("wrong-export");
    assert!(
        backend
            .observe_archived_workspace_export(*export, &wrong, backend.control_epoch())
            .is_err()
    );
    assert!(
        backend
            .workspace_archive_status()
            .unwrap()
            .unresolved
            .iter()
            .any(|(_, effect)| effect.operation == op)
    );
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
    } if value.schema_version.get() == 76 && value.id == proposal.id));
    assert_eq!(storage_title(&root), "Checkpoint authorship");
    {
        let secrets = credentials(&reopened, &vault);
        assert!(matches!(
            secrets.authenticate(Binding::LocalIpc, &original_token),
            Err(CredentialError::Denied)
        ));
        configure_mcp(backend.as_ref(), "2");
        assert!(matches!(
            secrets.provision(
                reopened.host().user(),
                &pairing.client_id,
                &pairing.revision
            ),
            Err(CredentialError::Denied)
        ));
        let renewed = allow_document_commands(backend.as_ref(), &pairing);
        assert_ne!(renewed.revision, pairing.revision);
        let token = secrets
            .provision(
                reopened.host().user(),
                &renewed.client_id,
                &renewed.revision,
            )
            .unwrap();
        assert!(secrets.authenticate(Binding::LocalIpc, &token).is_ok());
        assert!(matches!(
            secrets.authenticate(Binding::LocalIpc, &original_token),
            Err(CredentialError::Denied)
        ));
    }
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
    drop(backend);
    drop(reopened);
    let again = start(&root);
    let backend = again.backend();
    let status = backend.workspace_archive_status().unwrap();
    assert!(
        status
            .unresolved
            .iter()
            .any(|(_, effect)| effect.operation == op)
    );
    let next = backend
        .workspace_restore_plan(
            &published,
            backend.control_epoch(),
            &Cancellation::default(),
        )
        .unwrap();
    let second = fixture::command(
        backend.as_ref(),
        "workspace.restore",
        json!({
            "backup":next.manifest,"expectedStoreId":next.store,
            "expectedRestoreEpoch":next.epoch.to_string(),"proposalId":next.id,
        }),
    );
    ops::accepted_decision(archive_command(
        backend.as_ref(),
        "workspace.restore",
        &second,
    ))
    .unwrap();
    backend
        .request_workspace_restore_shutdown(&next.id, backend.control_epoch())
        .unwrap();
    drop(backend);
    assert!(again.shutdown().is_some());

    let second_app = start(&root);
    let backend = second_app.backend();
    let status = backend.workspace_archive_status().unwrap();
    assert_eq!(status.restored.unwrap().epoch, restored.epoch + 1);
    assert!(matches!(
        backend.command("workspace.restore", &confirm),
        Ok(CommandReply::Recorded { replayed: true, .. })
    ));
    let (export, effect) = status
        .unresolved
        .iter()
        .find(|(_, effect)| effect.operation == op)
        .expect("a second restore from the older bundle retained the original unresolved effect");
    assert_eq!(effect.origin_epoch, proposal.epoch);
    assert!(
        backend
            .observe_archived_workspace_export(*export, &destination, backend.control_epoch())
            .unwrap()
    );
    assert!(
        !backend
            .workspace_archive_status()
            .unwrap()
            .unresolved
            .iter()
            .any(|(_, effect)| effect.operation == op)
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

#[test]
fn cross_profile_restore_installs_graph_capture_without_an_artifact_version() {
    use devworks_adapters::fs::artifacts::NativeArtifactFiles;
    use devworks_core::ports::files::ArtifactFiles;

    let _case = native_runtime_case();
    let source = Root::new("workspace-archive-graph-source");
    let app = start(&source);
    let bytes = b"graph-owned source bytes";
    let digest = devworks_core::fingerprint::sha256_hex(bytes);
    let generation = "graph-fixture-generation";
    let files = NativeArtifactFiles::new(&source.0, source.0.join("cache"));
    let stored = files
        .publish(
            "graph-fixture-staging",
            &mut bytes.as_slice(),
            &digest,
            bytes.len() as u64,
            None,
            generation,
        )
        .unwrap();
    let db = Connection::open(source.0.join("workspace.sqlite")).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db.execute(
        "INSERT INTO repositories(id,canonical_location,revision)
        VALUES('archive-graph-repo','fixture://isolated/archive-graph',1)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO blobs(digest,relative_path,byte_count,state,generation)
         VALUES(?1,?2,?3,'Present',?4)",
        rusqlite::params![digest, stored.relative_path, bytes.len() as i64, generation],
    )
    .unwrap();
    db.execute(
        "INSERT INTO graph_source_captures(id,repository_id,unit_key,source_digest,blob_digest,blob_generation,byte_count,created_at)
         VALUES('archive-only-capture','archive-graph-repo','file.rs',?1,?1,?2,?3,1)",
        rusqlite::params![digest,generation,bytes.len() as i64],
    ).unwrap();
    drop(db);
    await_quiet(&app, &source);
    let export = source.dir("export");
    let backend = app.backend();
    let (op, _) = accepted_backup(backend.as_ref(), &export);
    let bundle = export.join(format!("devworks-backup-{}", op.0.as_str()));
    let end = Instant::now() + SETTLE_BOUND;
    while !bundle.join("manifest.json").exists() {
        assert!(Instant::now() < end, "graph snapshot did not publish");
        std::thread::sleep(Duration::from_millis(35));
    }
    assert!(
        read_manifest(&bundle)
            .checkpoint
            .blobs
            .iter()
            .any(|blob| blob.content.sha256 == digest && blob.generation.as_str() == generation)
    );
    drop(backend);
    drop(app);

    let target = Root::new("workspace-archive-graph-target");
    let app = start(&target);
    let backend = app.backend();
    let restore = backend
        .workspace_restore_plan(&bundle, backend.control_epoch(), &Cancellation::default())
        .unwrap();
    let confirm = fixture::command(
        backend.as_ref(),
        "workspace.restore",
        json!({
            "backup":restore.manifest,"expectedStoreId":restore.store,
            "expectedRestoreEpoch":restore.epoch.to_string(),"proposalId":restore.id,
        }),
    );
    ops::accepted_decision(archive_command(
        backend.as_ref(),
        "workspace.restore",
        &confirm,
    ))
    .unwrap();
    backend
        .request_workspace_restore_shutdown(&restore.id, backend.control_epoch())
        .unwrap();
    drop(backend);
    assert!(app.shutdown().is_some());

    let reopened = start(&target);
    let store = reopened.host().runtime().store();
    let capture =
        devworks_core::ports::storage::read_snapshot(store.as_ref(), &Default::default(), |db| {
            db.graph().graph_source_capture(
                &devworks_core::protocol::Id::new("archive-only-capture").unwrap(),
            )
        })
        .unwrap()
        .expect("restored graph capture");
    assert_eq!(capture.blob_digest, digest);
    let files = NativeArtifactFiles::new(&target.0, target.0.join("cache"));
    let mut reader = files
        .open_controlled(&digest, generation, bytes.len() as u64, &mut || false)
        .unwrap();
    let mut actual = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut actual).unwrap();
    assert_eq!(actual, bytes);
}
