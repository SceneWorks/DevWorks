//! SQLite checkpoints for workspace backup. Use a read transaction to keep the exact WAL
//! snapshot stable while SQLite copies it to an independent file.
use super::{MIGRATIONS, SqliteStore, conn, migrations, repos};
use crate::fs::workspace_archive::archive_io as io;
use devworks_core::ports::packages::Inspection;
use devworks_core::ports::storage::{
    AccessMode, BlobWitness, Cancellation, FileDigest, MigrationWitness, PackageFileWitness,
    PackageWitness, RestoredEffect, StorageError, WorkspaceArchive, WorkspaceArchiveStorage,
    WorkspaceCheckpoint,
};
use devworks_core::protocol::{Id, StoreId};
use rusqlite::backup::{Backup, StepResult};
use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags};
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

fn invalid(reason: &str) -> StorageError {
    StorageError::IncompatibleSchema(reason.to_owned())
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn standalone(path: &Path) -> Result<(), StorageError> {
    if !std::fs::symlink_metadata(path).map_err(io)?.is_file() {
        return Err(invalid(
            "a workspace checkpoint must be one regular database file",
        ));
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        match std::fs::symlink_metadata(sidecar(path, suffix)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(error)),
            Ok(_) => return Err(invalid("a workspace checkpoint has a SQLite sidecar")),
        }
    }
    Ok(())
}

/// Use the same complete schema identity for restore input and project purge. SQLite's planner
/// statistics can be created at shutdown and do not describe the store's owning schema.
pub(super) fn schema_digest(conn: &Connection) -> Result<String, StorageError> {
    let mut statement = conn::db(conn.prepare(
        "SELECT type,name,tbl_name,sql FROM main.sqlite_schema
         WHERE NOT (type='table' AND name=tbl_name AND (
              (name='sqlite_stat1' AND COALESCE(sql,'')='CREATE TABLE sqlite_stat1(tbl,idx,stat)')
              OR (name='sqlite_stat4' AND COALESCE(sql,'')='CREATE TABLE sqlite_stat4(tbl,idx,neq,nlt,ndlt,sample)')
         ))
         ORDER BY type,name,tbl_name",
    ))?;
    let definitions: Vec<(String, String, String, Option<String>)> = conn::db(
        conn::db(statement.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        }))?
        .collect(),
    )?;
    let encoded =
        serde_json::to_vec(&definitions).map_err(|_| invalid("cannot encode the store schema"))?;
    Ok(devworks_core::fingerprint::sha256_hex(&encoded))
}

fn expected_digest(version: i64) -> Result<String, StorageError> {
    let count = MIGRATIONS
        .iter()
        .position(|entry| entry.version == version)
        .map(|index| index + 1)
        .ok_or_else(|| invalid("unsupported checkpoint schema"))?;
    let expected = conn::db(Connection::open_in_memory())?;
    conn::db(expected.pragma_update(None, "foreign_keys", true))?;
    migrations::prepare(
        &expected,
        &MIGRATIONS[..count],
        || "checkpoint-schema".into(),
        0,
        "checkpoint",
    )?;
    schema_digest(&expected)
}

fn identifier(value: String) -> Result<Id, StorageError> {
    Id::new(value).map_err(|_| invalid("invalid checkpoint identity"))
}

fn blobs(conn: &Connection, version: i64) -> Result<Vec<BlobWitness>, StorageError> {
    if version < 6 {
        let any: bool = conn::db(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM artifact_versions WHERE deleted_at IS NULL)",
            [],
            |r| r.get(0),
        ))?;
        if any {
            return Err(invalid(
                "checkpoint predates verifiable managed artifact generations",
            ));
        }
        return Ok(Vec::new());
    }
    let query = format!(
        "{} SELECT d.blob_digest,b.generation,b.byte_count,b.state
          FROM (SELECT DISTINCT blob_digest FROM live_artifact_references) d
          LEFT JOIN blobs b ON b.digest=d.blob_digest
          ORDER BY d.blob_digest",
        super::artifacts::live_reference_cte(version),
    );
    let mut statement = conn::db(conn.prepare(&query))?;
    let rows = conn::db(statement.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<i64>>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    }))?;
    rows.map(|row| {
        let (sha256, generation, bytes, state) = conn::db(row)?;
        let (Some(generation), Some(bytes), Some("Present")) =
            (generation, bytes, state.as_deref())
        else {
            return Err(invalid(
                "a live artifact does not have a verified file generation",
            ));
        };
        if !devworks_core::fingerprint::is_sha256_hex(&sha256) {
            return Err(invalid("invalid live artifact digest"));
        }
        Ok(BlobWitness {
            generation: identifier(generation)?,
            content: FileDigest {
                sha256,
                bytes: u64::try_from(bytes).map_err(|_| invalid("negative blob size"))?,
            },
        })
    })
    .collect()
}

fn packages(conn: &Connection, version: i64) -> Result<Vec<PackageWitness>, StorageError> {
    if version < 20 {
        return Ok(Vec::new());
    }
    let mut statement = conn::db(conn.prepare(
        "SELECT p.package_id,p.active_digest,p.predecessor_digest,p.pending_digest, v.manifest_digest,v.inspection_json
         FROM packages p LEFT JOIN package_versions v
           ON v.package_id=p.package_id
           AND v.manifest_digest IN (p.active_digest,p.predecessor_digest,p.pending_digest)
         WHERE p.installed=1 OR p.pending_digest IS NOT NULL
         ORDER BY p.package_id,v.manifest_digest",
    ))?;
    let mut rows = conn::db(statement.query([]))?;
    let mut actual = std::collections::BTreeSet::new();
    let mut expected = std::collections::BTreeSet::new();
    let mut entries = Vec::new();
    while let Some(row) = conn::db(rows.next())? {
        let id: String = conn::db(row.get(0))?;
        for column in [1, 2, 3] {
            if let Some(digest) = conn::db(row.get::<_, Option<String>>(column))? {
                expected.insert((id.clone(), digest));
            }
        }
        let Some(manifest_digest) = conn::db(row.get::<_, Option<String>>(4))? else {
            continue;
        };
        let raw: String = conn::db(row.get(5))?;
        let inspection: Inspection = serde_json::from_str(&raw)
            .map_err(|_| invalid("checkpoint package inspection is unreadable"))?;
        if inspection.package_id != id || inspection.manifest_digest != manifest_digest {
            return Err(invalid(
                "checkpoint package identity does not match its inspection",
            ));
        }
        let mut files = inspection
            .files
            .into_iter()
            .map(|file| PackageFileWitness {
                path: file.path,
                content: FileDigest {
                    sha256: file.sha256,
                    bytes: file.byte_length,
                },
                executable: file.executable,
            })
            .collect::<Vec<_>>();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        actual.insert((id.clone(), manifest_digest.clone()));
        entries.push(PackageWitness {
            package: identifier(id)?,
            manifest_digest,
            files,
        });
    }
    if actual != expected {
        return Err(invalid(
            "a checkpoint package pointer has no verified inspection",
        ));
    }
    Ok(entries)
}

fn catalog() -> Vec<MigrationWitness> {
    MIGRATIONS
        .iter()
        .map(|migration| MigrationWitness {
            version: migration.version,
            checksum: migration.checksum(),
            minimum_reader: migration.minimum_reader,
            minimum_writer: migration.minimum_writer,
        })
        .collect()
}

fn checkpoint(
    connection: &Connection,
    capture_time: i64,
) -> Result<WorkspaceCheckpoint, StorageError> {
    let (meta, _) = migrations::check(connection, MIGRATIONS)?
        .ok_or_else(|| invalid("the checkpoint has no workspace store metadata"))?;
    let shipped = catalog();
    let ledger = migrations::ledger(connection)?;
    let migrations = shipped
        .iter()
        .filter(|entry| entry.version <= meta.schema_version)
        .cloned()
        .collect::<Vec<_>>();
    if ledger.len() != migrations.len()
        || !ledger.iter().zip(&migrations).all(|(record, entry)| {
            record.version == entry.version && record.checksum == entry.checksum
        })
        || migrations.last().is_none_or(|last| {
            last.minimum_reader != meta.minimum_reader || last.minimum_writer != meta.minimum_writer
        })
    {
        return Err(invalid(
            "checkpoint ledger differs from the shipped migration range",
        ));
    }
    Ok(WorkspaceCheckpoint {
        store_id: StoreId(identifier(meta.store_id)?),
        restore_epoch: meta.restore_epoch,
        sequence: repos::committed_sequence(connection)?,
        captured_at_ms: capture_time,
        schema_version: meta.schema_version,
        minimum_reader: meta.minimum_reader,
        minimum_writer: meta.minimum_writer,
        migrations,
        schema_digest: schema_digest(connection)?,
        blobs: blobs(connection, meta.schema_version)?,
        packages: packages(connection, meta.schema_version)?,
    })
}

fn copy_database(
    source: &Connection,
    destination: &Path,
    cancellation: &Cancellation,
) -> Result<(), StorageError> {
    // The caller owns the containing staging directory. Only create a new file.
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(io)?;
    let result = (|| {
        let mut writer = conn::db(Connection::open_with_flags(
            destination,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ))?;
        {
            let backup = conn::db(Backup::new(source, &mut writer))?;
            loop {
                if cancellation.is_cancelled() {
                    return Err(StorageError::Cancelled);
                }
                match conn::db(backup.step(128))? {
                    StepResult::Done => break,
                    StepResult::More => {}
                    _ => return Err(StorageError::Busy),
                }
            }
        }
        let mode = conn::db(
            writer
                .pragma_update_and_check(None, "journal_mode", "DELETE", |r| r.get::<_, String>(0)),
        )?;
        if !mode.eq_ignore_ascii_case("delete") {
            return Err(invalid("SQLite did not seal the standalone checkpoint"));
        }
        writer.close().map_err(|(_, error)| conn::db_err(error))?;
        standalone(destination)?;
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(destination)
            .and_then(|file| file.sync_all())
            .map_err(io)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    result
}

/// Strictly sidecar-free probe for a closed prior store or sealed image. The ordinary
/// read-only open can create WAL shared-memory sidecars for a file last used in WAL mode.
fn immutable_probe(path: &Path) -> Result<Connection, StorageError> {
    standalone(path)?;
    let mut uri = url::Url::from_file_path(path)
        .map_err(|_| invalid("unsupported SQLite checkpoint path"))?;
    uri.query_pairs_mut().append_pair("immutable", "1");
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    conn::db(Connection::open_with_flags(uri.as_str(), flags))
}

fn inspect(source: &Path, captured_at_ms: i64) -> Result<WorkspaceCheckpoint, StorageError> {
    let probe = immutable_probe(source)?;
    conn::db(probe.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false))?;
    conn::db(probe.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true))?;
    conn::db(probe.pragma_update(None, "query_only", true))?;
    let version: i64 = conn::db(probe.pragma_query_value(None, "user_version", |r| r.get(0)))?;
    if schema_digest(&probe)? != expected_digest(version)? {
        return Err(invalid(
            "checkpoint schema differs from the shipped catalogue",
        ));
    }
    // Only the app's built-in vector module is made available after verifying the schema.
    conn::register_vec(&probe)?;
    migrations::verify_after(&probe)?;
    let result = checkpoint(&probe, captured_at_ms)?;
    if result.schema_version != version {
        return Err(invalid("checkpoint schema revisions differ"));
    }
    Ok(result)
}

mod recovery;
pub use recovery::arm_restore;
pub(super) use recovery::{recover_restore, require_no_armed_restore};

fn archived_effect_probe(
    image: &Path,
    manifest: &WorkspaceArchive,
    effect: &RestoredEffect,
) -> Result<Connection, StorageError> {
    if manifest.checkpoint.store_id != effect.origin
        || manifest.checkpoint.restore_epoch != effect.origin_epoch
        || crate::fs::workspace_archive::digest_private_file(
            image,
            manifest.database.bytes,
            &Cancellation::default(),
        )? != manifest.database
        || inspect(image, manifest.checkpoint.captured_at_ms)? != manifest.checkpoint
    {
        return Err(StorageError::ScopeDenied);
    }
    immutable_probe(image)
}

impl WorkspaceArchiveStorage for SqliteStore {
    fn capture_checkpoint(
        &self,
        expected_sequence: i64,
        destination: &Path,
        cancellation: &Cancellation,
    ) -> Result<Option<WorkspaceCheckpoint>, StorageError> {
        if self.info.access != AccessMode::ReadWrite {
            return Err(invalid(
                "a writable storage owner is required to capture a checkpoint",
            ));
        }
        let lease = self.lease(Some(cancellation))?;
        let source = lease.conn.as_ref().expect("checked out");
        conn::db(source.execute_batch("BEGIN"))?;
        let actual = repos::committed_sequence(source)?;
        if actual != expected_sequence {
            return Ok(None);
        }
        let metadata = checkpoint(source, self.clock.now_ms().get())?;
        copy_database(source, destination, cancellation)?;
        drop(lease);
        match self.inspect_checkpoint(destination, metadata.captured_at_ms) {
            Ok(sealed) if sealed == metadata => Ok(Some(metadata)),
            other => {
                let _ = std::fs::remove_file(destination);
                Err(other
                    .err()
                    .unwrap_or_else(|| invalid("sealed checkpoint changed during copy")))
            }
        }
    }

    fn inspect_checkpoint(
        &self,
        source: &Path,
        captured_at_ms: i64,
    ) -> Result<WorkspaceCheckpoint, StorageError> {
        inspect(source, captured_at_ms)
    }

    fn migrate_checkpoint(
        &self,
        source: &Path,
        manifest: &WorkspaceArchive,
        destination: &Path,
        app_build: &str,
        cancellation: &Cancellation,
    ) -> Result<WorkspaceCheckpoint, StorageError> {
        use crate::fs::workspace_archive::{copy_verified_member, digest_private_file};

        manifest.validate()?;
        standalone(source)?;
        if digest_private_file(source, manifest.database.bytes, cancellation)? != manifest.database
            || self.inspect_checkpoint(source, manifest.checkpoint.captured_at_ms)?
                != manifest.checkpoint
        {
            return Err(invalid("the reviewed migration source changed"));
        }
        let reader = std::fs::File::open(source).map_err(io)?;
        copy_verified_member(reader, destination, &manifest.database, cancellation)?;
        let result = (|| {
            // Writable connections are limited to our own verified copy of the source.
            let mut writer = conn::open(
                destination,
                conn::Role::Writer,
                std::time::Duration::from_secs(3),
            )?;
            let cancel = cancellation.clone();
            conn::db(writer.progress_handler(2_000, Some(move || cancel.is_cancelled())))?;
            let prepared = migrations::prepare(
                &writer,
                MIGRATIONS,
                || manifest.checkpoint.store_id.0.as_str().to_owned(),
                self.clock.now_ms().get(),
                app_build,
            );
            conn::db(writer.progress_handler(0, None::<fn() -> bool>))?;
            if cancellation.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            prepared?;
            let mode = conn::db(writer.pragma_update_and_check(
                None,
                "journal_mode",
                "DELETE",
                |r| r.get::<_, String>(0),
            ))?;
            if !mode.eq_ignore_ascii_case("delete") {
                return Err(invalid("SQLite did not seal the migrated checkpoint"));
            }
            writer.close().map_err(|(_, error)| conn::db_err(error))?;
            let migrated =
                self.inspect_checkpoint(destination, manifest.checkpoint.captured_at_ms)?;
            let original = &manifest.checkpoint;
            if migrated.store_id != original.store_id
                || migrated.restore_epoch != original.restore_epoch
                || migrated.sequence != original.sequence
                || migrated.blobs != original.blobs
                || migrated.packages != original.packages
            {
                return Err(invalid(
                    "the migrated checkpoint lost its source identity or file inventory",
                ));
            }
            Ok(migrated)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(destination);
        }
        result
    }

    fn active_checkpoint(&self) -> Result<WorkspaceCheckpoint, StorageError> {
        let lease = self.lease(None)?;
        let source = lease.conn.as_ref().expect("checked out");
        conn::db(source.execute_batch("BEGIN"))?;
        let active = checkpoint(source, self.clock.now_ms().get())?;
        if active.schema_digest != expected_digest(active.schema_version)? {
            return Err(invalid(
                "the active workspace schema differs from the shipped catalogue",
            ));
        }
        Ok(active)
    }

    fn archived_backup_intent(
        &self,
        image: &Path,
        manifest: &WorkspaceArchive,
        effect: &RestoredEffect,
    ) -> Result<StoredWorkspaceArchive, StorageError> {
        if effect.capability != "workspace.backup" {
            return Err(StorageError::ScopeDenied);
        }
        let probe = archived_effect_probe(image, manifest, effect)?;
        record(&probe,
            "SELECT r.record_json FROM workspace_archive_runs r JOIN operations o ON o.id=r.operation_id
             WHERE o.id=?1 AND o.capability=?2 AND o.args_digest=?3",
            params![effect.operation.0.as_str(), &effect.capability, &effect.fingerprint],
        )?.filter(|saved: &StoredWorkspaceArchive| {
            saved.store == effect.origin && saved.epoch == effect.origin_epoch
                && saved.operation.as_ref() == Some(&effect.operation)
        }).ok_or(StorageError::ScopeDenied)
    }

    fn archived_effect_operation(
        &self,
        image: &Path,
        manifest: &WorkspaceArchive,
        effect: &RestoredEffect,
    ) -> Result<devworks_core::ports::storage::OperationRecord, StorageError> {
        let probe = archived_effect_probe(image, manifest, effect)?;
        repos::operation(&probe, &effect.operation)?
            .filter(|origin| {
                origin.args_digest == effect.fingerprint && origin.capability == effect.capability
            })
            .ok_or(StorageError::ScopeDenied)
    }

    fn shipped_migrations(&self) -> Result<Vec<MigrationWitness>, StorageError> {
        Ok(catalog())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::tests::Harness;
    use devworks_core::ports::storage::contract::StoreHarness;

    #[test]
    fn checkpoint_is_sealed_at_the_requested_sequence_and_rejects_a_modified_schema() {
        let harness = Harness::new();
        let store = harness.store();
        let root = store.database_path().parent().unwrap();
        let archive = root.join("checkpoint-test.sqlite");
        let cancel = Cancellation::default();
        let sequence = store
            .with_writer_connection(repos::committed_sequence)
            .unwrap();
        assert!(
            store
                .capture_checkpoint(sequence + 1, &archive, &cancel)
                .unwrap()
                .is_none()
        );
        assert!(!archive.exists());
        let captured = store
            .capture_checkpoint(sequence, &archive, &cancel)
            .unwrap()
            .unwrap();
        assert_eq!(captured.sequence, sequence);
        assert_eq!(
            store.inspect_checkpoint(&archive, harness.now()).unwrap(),
            captured,
        );

        {
            let tampered = Connection::open(&archive).unwrap();
            tampered
                .execute_batch("CREATE VIEW unexpected_checkpoint_object AS SELECT 1;")
                .unwrap();
        }
        assert!(matches!(
            store.inspect_checkpoint(&archive, harness.now()),
            Err(StorageError::IncompatibleSchema(_)),
        ));
    }

    #[test]
    fn an_older_sealed_checkpoint_migrates_using_only_the_shipped_catalogue() {
        use crate::fs::workspace_archive::digest_private_file;
        use devworks_core::ports::storage::{ARCHIVE_FORMAT, ARCHIVE_REVISION};

        let harness = Harness::new();
        let store = harness.store();
        let root = store.database_path().parent().unwrap();
        let source = root.join("older-checkpoint.sqlite");
        let migrated = root.join("migrated-checkpoint.sqlite");
        let cancel = Cancellation::default();
        let through = MIGRATIONS.len() - 1;
        {
            let connection = Connection::open(&source).unwrap();
            connection
                .pragma_update(None, "foreign_keys", true)
                .unwrap();
            migrations::prepare(
                &connection,
                &MIGRATIONS[..through],
                || "fixture-store".into(),
                harness.now(),
                "fixture",
            )
            .unwrap();
        }
        let checkpoint = store.inspect_checkpoint(&source, harness.now()).unwrap();
        let manifest = WorkspaceArchive {
            format: ARCHIVE_FORMAT.into(),
            revision: ARCHIVE_REVISION,
            operation_id: Id::new("archived-operation").unwrap(),
            app_build: "fixture".into(),
            checkpoint,
            database: digest_private_file(&source, u64::MAX, &cancel).unwrap(),
        };
        let after = store
            .migrate_checkpoint(&source, &manifest, &migrated, "test-build", &cancel)
            .unwrap();
        assert_eq!(after.schema_version, MIGRATIONS.last().unwrap().version);
        assert_eq!(after.migrations, store.shipped_migrations().unwrap());
        assert_eq!(after.store_id, manifest.checkpoint.store_id);
        assert_eq!(
            digest_private_file(&source, u64::MAX, &cancel).unwrap(),
            manifest.database
        );
        assert!(store.database_path().is_file());
    }

    #[test]
    fn cancellation_removes_only_the_owned_checkpoint_file() {
        let harness = Harness::new();
        let store = harness.store();
        let root = store.database_path().parent().unwrap();
        let archive = root.join("cancelled-checkpoint.sqlite");
        let sequence = store
            .with_writer_connection(repos::committed_sequence)
            .unwrap();
        let cancel = Cancellation::default();
        cancel.cancel();
        assert!(matches!(
            store.capture_checkpoint(sequence, &archive, &cancel),
            Err(StorageError::Cancelled),
        ));
        assert!(!archive.exists());
        assert!(store.database_path().is_file());
    }
}

use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::{
    ArchivePhase, RestoredWorkspace, StoredBackupFolder, StoredWorkspaceArchive,
    WorkspaceArchiveRead, WorkspaceArchiveRepository,
};
use devworks_core::protocol::{OperationId, RevisionDimension};
use rusqlite::{OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};

fn record<T: DeserializeOwned>(
    conn: &Connection,
    sql: &str,
    arguments: impl rusqlite::Params,
) -> Result<Option<T>, StorageError> {
    let record: Option<String> =
        conn::db(conn.query_row(sql, arguments, |row| row.get(0)).optional())?;
    record
        .map(|raw| {
            serde_json::from_str(&raw).map_err(|_| invalid("malformed workspace archive state"))
        })
        .transpose()
}

fn encode(value: &impl Serialize) -> Result<String, StorageError> {
    serde_json::to_string(value)
        .map_err(|_| StorageError::InvalidRecord("workspace archive state".into()))
}

const RESTORED_EFFECT_COLUMNS: &str = "rowid,epoch,timeline,origin_store,origin_epoch,
    origin_stage,operation,capability,fingerprint,root";
fn restored_effect_of(row: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, RestoredEffect)> {
    let id = |index| -> rusqlite::Result<Id> {
        Id::new(row.get::<_, String>(index)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                error.into(),
            )
        })
    };
    let root = if row.get::<_, Option<String>>(9)?.is_some() {
        Some(devworks_core::protocol::RootId(id(9)?))
    } else {
        None
    };
    Ok((
        row.get(0)?,
        RestoredEffect {
            epoch: row.get(1)?,
            timeline: row.get(2)?,
            origin: StoreId(id(3)?),
            origin_epoch: row.get(4)?,
            origin_stage: id(5)?,
            operation: OperationId(id(6)?),
            capability: row.get(7)?,
            fingerprint: row.get(8)?,
            root,
        },
    ))
}

macro_rules! archive_reads {
    ($ty:ty) => {
        impl WorkspaceArchiveRead for $ty {
            fn backup_folder(&self, id: &Id) -> Result<Option<StoredBackupFolder>, StorageError> {
                record(
                    self.conn,
                    "SELECT record_json FROM workspace_backup_folders WHERE id=?1",
                    [id.as_str()],
                )
            }
            fn workspace_archive(
                &self,
                id: &Id,
            ) -> Result<Option<StoredWorkspaceArchive>, StorageError> {
                record(
                    self.conn,
                    "SELECT record_json FROM workspace_archive_runs WHERE id=?1",
                    [id.as_str()],
                )
            }
            fn workspace_archive_for_operation(
                &self,
                operation: &OperationId,
            ) -> Result<Option<StoredWorkspaceArchive>, StorageError> {
                record(
                    self.conn,
                    "SELECT record_json FROM workspace_archive_runs WHERE operation_id=?1",
                    [operation.0.as_str()],
                )
            }
            fn workspace_migration_plan(&self, artifact: &Id) -> Result<Option<StoredWorkspaceArchive>, StorageError> {
                record(self.conn,
                    "SELECT record_json FROM workspace_archive_runs WHERE phase='Prepared'
                     AND json_extract(record_json,'$.migrationManifest.artifactId')=?1",
                    [artifact.as_str()],
                )
            }

            fn latest_workspace_archive(
                &self,
                owner: &devworks_core::protocol::ActorId,
                epoch: i64,
            ) -> Result<Option<StoredWorkspaceArchive>, StorageError> {
                record(self.conn,
                    "SELECT record_json FROM workspace_archive_runs WHERE owner=?1
                     AND json_extract(record_json,'$.epoch')=?2
                     ORDER BY json_extract(record_json,'$.createdAtMs') DESC,rowid DESC LIMIT 1",
                    params![owner.0.as_str(), epoch],
                )
            }

            fn latest_workspace_restore(
                &self,
                epoch: i64,
            ) -> Result<Option<RestoredWorkspace>, StorageError> {
                type Raw = (String, String, String, String, String, i64, Option<String>);
                let row: Option<Raw> = conn::db(self.conn.query_row(
                    "SELECT proposal,operation,source_manifest,prior_manifest,json_extract(receipt,'$.actor'),
                      (SELECT count(*) FROM workspace_restore_effects WHERE epoch=?1 AND unresolved=1),
                      json_extract(receipt,'$.completion')
                      FROM workspace_restore_history WHERE epoch=?1",
                    [epoch],
                    |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?)),
                ).optional())?;
                let Some((proposal, operation, source, prior, owner, unresolved_effects, completion)) = row else { return Ok(None); };
                let completion = completion
                    .map(|state| serde_json::from_str(&state)
                        .map_err(|_| invalid("bad workspace restore completion")))
                    .transpose()?;
                Ok(Some(RestoredWorkspace {
                    epoch, proposal:identifier(proposal)?,operation:OperationId(identifier(operation)?),
                    owner: devworks_core::protocol::ActorId(identifier(owner)?),
                    source:serde_json::from_str(&source).map_err(|_| invalid("bad restore source"))?,
                    prior:serde_json::from_str(&prior).map_err(|_| invalid("bad restore prior image"))?,
                    unresolved_effects,completion,
                }))
            }

            fn archived_restore_receipt(
                &self,
                key: &devworks_core::ports::storage::ArchivedRestoreKey<'_>,
            ) -> Result<Option<devworks_core::protocol::Receipt<devworks_core::protocol::results::DomainDecision>>, StorageError> {
                record(self.conn,
                    "SELECT json_extract(result_json,'$.receipt') FROM workspace_archived_restore_receipts
                     WHERE proposal=?1 AND owner=?2 AND scope=?3 AND request=?4 AND fingerprint=?5
                     AND json_extract(result_json,'$.schemaRevision')=?6 AND json_extract(result_json,'$.contract')=?7
                     AND json_extract(result_json,'$.receipt.operationId')=operation",
                    params![key.proposal.as_str(),key.owner.0.as_str(),key.scope,key.request_key.0.as_str(),
                        key.fingerprint,key.revision.0,devworks_core::protocol::CONTRACT],
                )
            }

            fn unresolved_restore_effects(&self, epoch:i64, after:i64, limit:u16) -> Result<Vec<(i64,RestoredEffect)>,StorageError> {
                if !(1..=100).contains(&limit) { return Err(invalid("invalid effect read limit")); }
                let mut stmt = conn::db(self.conn.prepare(&format!(
                    "SELECT {RESTORED_EFFECT_COLUMNS} FROM workspace_restore_effects
                     WHERE epoch=?1 AND unresolved=1 AND rowid>?2 ORDER BY rowid LIMIT ?3"
                )))?;
                conn::db(conn::db(stmt.query_map(params![epoch,after,limit],restored_effect_of))?.collect())
            }

            fn restore_effect(&self, epoch:i64,id:i64) -> Result<Option<RestoredEffect>,StorageError> {
                conn::db(self.conn.query_row(
                    &format!("SELECT {RESTORED_EFFECT_COLUMNS} FROM workspace_restore_effects
                    WHERE epoch=?1 AND rowid=?2 AND unresolved=1"),
                    params![epoch,id], restored_effect_of,
                ).optional()).map(|value| value.map(|(_,effect)|effect))
            }

            fn restore_effect_held(
                &self,
                root: Option<&devworks_core::protocol::RootId>,
                capability: Option<&str>,
            ) -> Result<bool, StorageError> {
                conn::db(self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM workspace_restore_effects e
                     WHERE e.unresolved=1 AND e.epoch=(SELECT restore_epoch FROM store_meta WHERE singleton=1) AND ((?1 IS NOT NULL AND (e.root=?1 OR
                          e.root=(SELECT parent_feature_id FROM work_items WHERE id=?1)))
                       OR (?2 IS NOT NULL AND e.capability=?2 AND (e.root IS NULL OR
                          NOT EXISTS(SELECT 1 FROM work_items WHERE id=e.root)))))",
                    params![root.map(|r|r.0.as_str()), capability],
                    |row| row.get(0),
                ))
            }

            fn restore_awaiting_restart(
                &self,
            ) -> Result<Option<StoredWorkspaceArchive>, StorageError> {
                record(
                    self.conn,
                    "SELECT r.record_json FROM workspace_archive_runs r, store_meta m
                      WHERE r.phase='AwaitingRestart' AND m.singleton=1 AND
                      json_extract(r.record_json,'$.store')=m.store_id AND json_extract(r.record_json,'$.epoch')=m.restore_epoch",
                    [],
                )
            }
        }
    };
}

archive_reads!(Snapshot<'_>);
archive_reads!(Unit<'_>);

fn phase_name(phase: ArchivePhase) -> &'static str {
    match phase {
        ArchivePhase::Prepared => "Prepared",
        ArchivePhase::Queued => "Queued",
        ArchivePhase::AwaitingRestart => "AwaitingRestart",
        ArchivePhase::Published => "Published",
        ArchivePhase::Migrated => "Migrated",
        ArchivePhase::Restored => "Restored",
        ArchivePhase::Cancelled => "Cancelled",
        ArchivePhase::Failed => "Failed",
        ArchivePhase::Unknown => "Unknown",
    }
}

impl WorkspaceArchiveRepository for Unit<'_> {
    fn record_restored_completion(
        &mut self,
        epoch: i64,
        operation: &OperationId,
        completion: &devworks_core::protocol::results::OperationCompletion,
    ) -> Result<(), StorageError> {
        if conn::db(self.conn.execute(
            "UPDATE workspace_restore_history SET receipt=json_set(receipt,'$.completion',json(?3))
             WHERE epoch=?1 AND operation=?2 AND json_extract(receipt,'$.completion') IS NULL",
            params![epoch, operation.0.as_str(), encode(completion)?],
        ))? != 1
        {
            return Err(StorageError::ScopeDenied);
        }
        Ok(())
    }

    fn record_restored_observation(
        &mut self,
        effect: &RestoredEffect,
        evidence: &serde_json::Value,
    ) -> Result<(), StorageError> {
        if conn::db(self.conn.execute(
            "UPDATE workspace_restore_effects SET unresolved=0,evidence_json=?7
             WHERE epoch=?1 AND origin_store=?2 AND origin_epoch=?3 AND
               operation=?4 AND capability=?5 AND fingerprint=?6 AND unresolved=1",
            params![
                effect.epoch,
                effect.origin.0.as_str(),
                effect.origin_epoch,
                effect.operation.0.as_str(),
                &effect.capability,
                &effect.fingerprint,
                encode(evidence)?
            ],
        ))? == 0
        {
            return Err(StorageError::ScopeDenied);
        }
        Ok(())
    }

    fn register_backup_folder(&mut self, grant: &StoredBackupFolder) -> Result<(), StorageError> {
        if grant.redeemed_by.is_some() || grant.expires_at_ms <= self.now {
            return Err(StorageError::ScopeDenied);
        }
        conn::db(self.conn.execute(
            "INSERT INTO workspace_backup_folders(id,owner,expires_at_ms,record_json) VALUES(?1,?2,?3,?4)",
            params![grant.id.as_str(),grant.owner.0.as_str(),grant.expires_at_ms,encode(grant)?],
        ))?;
        Ok(())
    }

    fn redeem_backup_folder(
        &mut self,
        id: &Id,
        operation: &OperationId,
    ) -> Result<(), StorageError> {
        let mut grant = self.backup_folder(id)?.ok_or(StorageError::ScopeDenied)?;
        if grant.redeemed_by.is_some() || grant.expires_at_ms <= self.now {
            return Err(StorageError::ScopeDenied);
        }
        grant.redeemed_by = Some(operation.clone());
        if conn::db(self.conn.execute(
            "UPDATE workspace_backup_folders SET operation_id=?2,record_json=?3
             WHERE id=?1 AND operation_id IS NULL",
            params![id.as_str(), operation.0.as_str(), encode(&grant)?],
        ))? != 1
        {
            return Err(StorageError::ScopeDenied);
        }
        Ok(())
    }

    fn put_workspace_archive(
        &mut self,
        value: &StoredWorkspaceArchive,
        expected_revision: Option<i64>,
    ) -> Result<(), StorageError> {
        match (self.workspace_archive(&value.id)?, expected_revision) {
            (None, None) if value.revision == 1 => {}
            (Some(previous), Some(expected)) if previous.revision == expected => {
                if Some(value.revision) != expected.checked_add(1)
                    || previous.stage_id != value.stage_id
                    || previous.owner != value.owner
                    || previous.store != value.store
                    || previous.epoch != value.epoch
                    || previous.created_at_ms != value.created_at_ms
                    || previous.expires_at_ms != value.expires_at_ms
                    || (previous.operation.is_some() && previous.operation != value.operation)
                {
                    return Err(StorageError::InvalidRecord(
                        "workspace archive identity is immutable".into(),
                    ));
                }
            }
            (Some(previous), _) => {
                return Err(super::support::conflict(
                    RevisionDimension::Store,
                    "workspace-archive",
                    value.id.as_str(),
                    expected_revision.unwrap_or(1),
                    previous.revision,
                ));
            }
            (None, _) => {
                return Err(StorageError::NotFound {
                    kind: "workspace-archive",
                    id: value.id.as_str().to_owned(),
                });
            }
        }
        conn::db(self.conn.execute(
            "INSERT INTO workspace_archive_runs(id,owner,operation_id,revision,phase,record_json)
             VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(id) DO UPDATE SET operation_id=excluded.operation_id, revision=excluded.revision,
                                          phase=excluded.phase,record_json=excluded.record_json",
            params![value.id.as_str(),value.owner.0.as_str(),
                value.operation.as_ref().map(|id|id.0.as_str()), value.revision, phase_name(value.phase), encode(value)?],
        ))?;
        Ok(())
    }
}
