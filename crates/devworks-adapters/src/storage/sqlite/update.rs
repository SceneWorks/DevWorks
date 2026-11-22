//! Store-owned update records and a read-only check of the stopped profile.
use super::{DATABASE_FILE, LOCK_FILE, MIGRATIONS, Migration, conn::db, migrations, repos};
use devworks_core::ports::storage::{AccessMode, MigrationWitness, StorageError, Unavailable};
use devworks_core::ports::update::{
    InstallApproval, UpdateProfile, UpdateRead, UpdateRepository, UpdateState,
};
use rusqlite::{Connection, OptionalExtension};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

const MAX_RECORD: usize = 12 * 1024 * 1024;

fn read(conn: &Connection) -> Result<UpdateState, StorageError> {
    let raw: Option<String> = db(conn
        .query_row(
            "SELECT record_json FROM app_update_state WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional())?;
    let Some(raw) = raw else {
        return Ok(UpdateState::default());
    };
    if raw.len() > MAX_RECORD {
        return Err(invalid(
            "The application update record exceeds its storage bound.",
        ));
    }
    serde_json::from_str(&raw).map_err(|_| invalid("The application update record is unreadable."))
}

fn profile(conn: &Connection) -> Result<UpdateProfile, StorageError> {
    let meta = migrations::read_meta(conn)?
        .ok_or_else(|| invalid("The application update profile has no schema record."))?;
    Ok(UpdateProfile {
        schema: meta.schema_version,
        minimum_reader: meta.minimum_reader,
        minimum_writer: meta.minimum_writer,
        migrations: migrations::ledger(conn)?
            .into_iter()
            .map(|m| (m.version, m.checksum))
            .collect(),
    })
}

impl UpdateRead for repos::Snapshot<'_> {
    fn update_profile(&self) -> Result<UpdateProfile, StorageError> {
        profile(self.conn)
    }
    fn update_state(&self) -> Result<UpdateState, StorageError> {
        read(self.conn)
    }
}

impl UpdateRead for repos::Unit<'_> {
    fn update_profile(&self) -> Result<UpdateProfile, StorageError> {
        profile(self.conn)
    }
    fn update_state(&self) -> Result<UpdateState, StorageError> {
        read(self.conn)
    }
}

impl UpdateRepository for repos::Unit<'_> {
    fn save_update_state(&mut self, state: &UpdateState) -> Result<(), StorageError> {
        let raw = serde_json::to_string(state)
            .map_err(|_| invalid("The application update record cannot be serialized."))?;
        if raw.len() > MAX_RECORD {
            return Err(invalid(
                "The application update record exceeds its storage bound.",
            ));
        }
        db(self.conn.execute(
            "INSERT INTO app_update_state(singleton,record_json) VALUES (1,?1)
             ON CONFLICT(singleton) DO UPDATE SET record_json=excluded.record_json",
            [&raw],
        ))?;
        Ok(())
    }
}

fn invalid(message: &str) -> StorageError {
    StorageError::IncompatibleSchema(message.into())
}

pub fn shipped_witness() -> Vec<MigrationWitness> {
    witness(MIGRATIONS)
}

fn witness(migrations: &[Migration]) -> Vec<MigrationWitness> {
    migrations
        .iter()
        .map(|migration| MigrationWitness {
            version: migration.version,
            checksum: migration.checksum(),
            minimum_reader: migration.minimum_reader,
            minimum_writer: migration.minimum_writer,
        })
        .collect()
}

/// The installer keeps the same OS lock the SQLite owner uses until it completes the exchange.
pub fn stopped_profile(root: &Path, approval: &InstallApproval) -> Result<File, StorageError> {
    let lock = try_profile_lock(root)?;
    check_approved(root, approval, MIGRATIONS, false)?;
    Ok(lock)
}

pub(super) fn try_profile_lock(root: &Path) -> Result<File, StorageError> {
    let io = |e: std::io::Error| StorageError::unavailable(Unavailable::Io, e.to_string());
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(LOCK_FILE))
        .map_err(io)?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(TryLockError::WouldBlock) => Err(StorageError::unavailable(
            Unavailable::DataRootInUse,
            "another DevWorks process owns this data root",
        )),
        Err(TryLockError::Error(e)) => Err(io(e)),
    }
}

/// Called with the profile OS lock, before its recovery or first writable connection.
pub(super) fn check_approved(
    root: &Path,
    approval: &InstallApproval,
    migrations: &[Migration],
    is_candidate: bool,
) -> Result<(), StorageError> {
    super::workspace_archive::require_no_armed_restore(root)?;
    let conn = super::conn::open_probe(&root.join(DATABASE_FILE))?;
    let (meta, access) = migrations::check(&conn, migrations)?
        .ok_or_else(|| invalid("The approved profile has no application store."))?;
    let clean_boot = db(conn
        .query_row(
            "SELECT id,shutdown_state,stopped_at FROM app_boots
         ORDER BY started_at DESC,rowid DESC LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .optional())?;
    let compiled = witness(migrations);
    let applied = migrations::ledger(&conn)?;
    if !matches!(access, AccessMode::ReadWrite)
        || meta.store_id != approval.store.0.as_str()
        || meta.restore_epoch != approval.epoch
        || meta.schema_version != approval.profile_schema
        || clean_boot.as_ref().is_none_or(|(id, state, stopped)| {
            id != approval.approved_boot.as_str() || state != "Clean" || stopped.is_none()
        })
        || !approved_dispatch(&conn, approval)?
        || !approved_prefix(&compiled, approval)
        || applied.len() != approval.profile_migrations.len()
        || !applied
            .iter()
            .zip(&approval.profile_migrations)
            .all(|(a, b)| a.version == b.version && a.checksum == b.checksum)
        || (is_candidate && compiled != approval.stage.manifest.migrations)
    {
        return Err(invalid(
            "The approved profile, shutdown or migration plan changed.",
        ));
    }
    Ok(())
}

fn approved_dispatch(conn: &Connection, approval: &InstallApproval) -> Result<bool, StorageError> {
    let state = read(conn)?;
    let recorded = state.approval.as_ref() == Some(approval)
        && state.update.as_ref().is_some_and(|update| {
            update.stage == approval.stage
                && update.record.id == approval.update
                && update.proposal_id == approval.proposal
                && update.record.state == devworks_core::protocol::OperationState::Dispatching
        });
    let operation = db(conn
        .query_row(
            "SELECT capability,args_digest,state FROM operations WHERE id=?1",
            [approval.operation.0.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional())?;
    Ok(recorded
        && operation.is_some_and(|(capability, fingerprint, status)| {
            capability == devworks_core::services::update::APPROVE
                && fingerprint == approval.request_sha256
                && status == "Dispatching"
        }))
}

fn approved_prefix(compiled: &[MigrationWitness], approval: &InstallApproval) -> bool {
    !approval.stage.manifest.destructive_migration
        && approval.profile_schema == approval.profile_migrations.len() as i64
        && (compiled.starts_with(&approval.profile_migrations)
            || approval.profile_migrations.starts_with(compiled))
        && approval
            .stage
            .manifest
            .migrations
            .starts_with(&approval.profile_migrations)
}

/// A person can explicitly open the existing profile with a different compatible build.
pub fn verify_recovery_store(
    root: &Path,
    approval: &InstallApproval,
) -> Result<File, StorageError> {
    let lock = try_profile_lock(root)?;
    super::workspace_archive::require_no_armed_restore(root)?;
    let conn = super::conn::open_probe(&root.join(DATABASE_FILE))?;
    let (meta, access) = migrations::check(&conn, MIGRATIONS)?
        .ok_or_else(|| invalid("The recovery profile has no application store."))?;
    if !matches!(access, AccessMode::ReadWrite)
        || meta.store_id != approval.store.0.as_str()
        || meta.restore_epoch != approval.epoch
        || read(&conn)?.approval.as_ref() != Some(approval)
    {
        return Err(invalid(
            "This build cannot safely open the original approved workspace.",
        ));
    }
    Ok(lock)
}

/// Rollback restores only an authenticated predecessor. Calls are serialized before
/// candidate UI admission. The stopped-store lock must cover this check and the exchange.
pub fn compatible_predecessor(
    root: &Path,
    approval: &InstallApproval,
) -> Result<File, StorageError> {
    let lock = try_profile_lock(root)?;
    super::workspace_archive::require_no_armed_restore(root)?;
    let conn = super::conn::open_probe(&root.join(DATABASE_FILE))?;
    let (meta, access) = migrations::check(&conn, MIGRATIONS)?
        .ok_or_else(|| invalid("The updated profile has no application store."))?;
    let applied = migrations::ledger(&conn)?;
    let plan = &approval.stage.manifest.migrations;
    let schema = usize::try_from(meta.schema_version).unwrap_or(usize::MAX);
    let witness = schema.checked_sub(1).and_then(|index| plan.get(index));
    if !matches!(access, AccessMode::ReadWrite)
        || meta.store_id != approval.store.0.as_str()
        || meta.restore_epoch != approval.epoch
        || approval.old_schema_capability < meta.minimum_reader
        || approval.old_schema_capability < meta.minimum_writer
        || meta.schema_version < approval.profile_schema
        || witness.is_none_or(|m| {
            m.minimum_reader != meta.minimum_reader || m.minimum_writer != meta.minimum_writer
        })
        || applied.len() != schema
        || !applied
            .iter()
            .zip(plan)
            .all(|(a, b)| a.version == b.version && a.checksum == b.checksum)
        || read(&conn)?.approval.as_ref() != Some(approval)
    {
        return Err(invalid(
            "The previous app cannot safely write this profile. Its data was kept.",
        ));
    }
    Ok(lock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use devworks_core::ports::storage::{
        ApplicationStore, AttemptBinding, NewOperation, WriteOptions, run_write,
    };
    use devworks_core::ports::update::{
        DownloadedUpdate, InstallOwner, ReleaseFile, ReleaseManifest, StagedRelease,
    };
    use devworks_core::protocol::provider::Digest;
    use devworks_core::protocol::records::{UpdateChannel, UpdateRecord};
    use devworks_core::protocol::{
        Id, OperationId, OperationState, Revision, support::BoundedText,
    };

    #[test]
    fn shipped_migration_checksums_match_exact_sql() {
        for entry in MIGRATIONS {
            assert_eq!(
                entry.checksum(),
                entry.expected_sha256,
                "schema {}",
                entry.version
            );
        }
    }

    fn approved_profile(
        name: &str,
    ) -> (
        devworks_test_support::TempRoot,
        super::super::SqliteConfig,
        InstallApproval,
    ) {
        let root = devworks_test_support::TempRoot::new(name);
        super::super::seed::seed_migrated_store(root.path(), "update-store", 2000, "old-build");
        let ids = std::sync::Arc::new(devworks_test_support::TestIds::new());
        let clock = std::sync::Arc::new(devworks_test_support::TestClock::new(3000));
        let id = |s: &str| Id::new(s).unwrap();
        let config = super::super::SqliteConfig::new(
            root.path(),
            clock,
            ids,
            devworks_core::protocol::ActorId(id("host")),
        );
        let store = super::super::SqliteStore::open(config.clone()).unwrap();
        let migrations = shipped_witness();
        let info = store.info();
        let hash = "a".repeat(64);
        let digest = devworks_core::ports::storage::FileDigest {
            bytes: 50,
            sha256: hash.clone(),
        };
        let target = "aarch64-apple-darwin";
        let stage = StagedRelease {
            manifest: ReleaseManifest {
                format: 1,
                product: "SceneWorks/DevWorks".into(),
                release_id: 99,
                tag: "v1.9.0".into(),
                build: "a".repeat(40),
                target: target.into(),
                owner: InstallOwner::MacApplication,
                archive: digest.clone(),
                archive_asset: devworks_core::ports::update::asset_name(target, "bundle"),
                help_digest: hash.clone(),
                migrations: migrations.clone(),
                files: vec![ReleaseFile {
                    path: InstallOwner::MacApplication.entrypoint().into(),
                    executable: true,
                    content: digest,
                }],
                destructive_migration: false,
            },
            manifest_digest: hash.clone(),
        };
        let approval = InstallApproval {
            operation: OperationId(id("install-operation")),
            proposal: id("install-proposal"),
            update: id("install-update"),
            revision: 1,
            store: info.store_id.clone(),
            epoch: info.restore_epoch,
            actor: info.host_actor.clone(),
            approved_boot: info.boot_id.unwrap(),
            request_sha256: hash.clone(),
            expires_at_ms: 10_000,
            old_build: "b".repeat(40),
            old_binary_digest: hash.clone(),
            old_version: "1.8.0".into(),
            old_schema_capability: 70,
            profile_schema: migrations.last().unwrap().version,
            profile_migrations: migrations,
            stage,
        };
        let saved = approval.clone();
        run_write(&store, WriteOptions::host_state(), move |tx| {
            let op = tx.operations().record_intent(NewOperation {
                id: saved.operation.clone(),
                root_id: None,
                assignment_id: None,
                capability: devworks_core::services::update::APPROVE.into(),
                schema_version: 1,
                args_digest: saved.request_sha256.clone(),
                payload: serde_json::json!({"approval":saved}),
            })?;
            let attempt =
                tx.operations()
                    .begin_attempt(&op.id, op.revision, AttemptBinding::default())?;
            tx.operations().transition(
                &op.id,
                op.revision + 1,
                Some(&attempt.fence()),
                OperationState::Dispatching,
                None,
            )?;
            tx.updates()?.save_update_state(&UpdateState {
                profile: Some((saved.store.clone(), saved.epoch)),
                update: Some(DownloadedUpdate {
                    record: UpdateRecord {
                        id: saved.update.clone(),
                        revision: Revision::new(2).unwrap(),
                        version: BoundedText("1.9.0".into()),
                        channel: UpdateChannel::Stable,
                        digest: Digest::try_from(hash.clone()).unwrap(),
                        signature_verified: true,
                        state: OperationState::Dispatching,
                        migration_required: false,
                    },
                    stage: saved.stage.clone(),
                    proposal_id: saved.proposal.clone(),
                }),
                approval: Some(saved),
                ..UpdateState::default()
            })
        })
        .unwrap();
        drop(store);
        (root, config, approval)
    }

    #[test]
    fn candidate_checks_the_exact_approved_store_and_migrations_before_open() {
        let (root, mut config, approval) = approved_profile("update-prewrite");
        let lock = stopped_profile(root.path(), &approval).unwrap();
        config.app_update = Some(approval.clone());
        assert!(
            super::super::SqliteStore::open(config.clone()).is_err(),
            "a locked profile cannot enter the candidate"
        );
        drop(lock);
        let mut forged = approval.clone();
        forged
            .stage
            .manifest
            .migrations
            .last_mut()
            .unwrap()
            .checksum = "b".repeat(64);
        config.app_update = Some(forged);
        assert!(super::super::SqliteStore::open(config.clone()).is_err());
        config.app_update = Some(approval.clone());
        std::fs::write(
            root.join("restore-pending.json"),
            b"existing restore marker",
        )
        .unwrap();
        assert!(super::super::SqliteStore::open(config.clone()).is_err());
        assert_eq!(
            std::fs::read(root.join("restore-pending.json")).unwrap(),
            b"existing restore marker"
        );
        std::fs::remove_file(root.join("restore-pending.json")).unwrap();
        let opened = super::super::SqliteStore::open(config).unwrap();
        assert_eq!(opened.info().store_id, approval.store);
        assert_ne!(opened.info().boot_id, Some(approval.approved_boot.clone()));
        drop(opened);
        assert!(compatible_predecessor(root.path(), &approval).is_ok());
    }

    #[test]
    fn a_newer_verifier_cannot_substitute_its_own_writer_capability() {
        let (root, _, mut approval) = approved_profile("predecessor-writer-capability");
        // This test binary supports schema 70, while recovery must evaluate the actual
        // predecessor recorded in the receipt. SQLite remains untouched.
        approval.old_schema_capability = 69;
        let db = Connection::open(root.join(DATABASE_FILE)).unwrap();
        db.execute(
            "UPDATE app_update_state SET record_json=json_set(record_json,'$.approval',json(?1))",
            [serde_json::to_string(&approval).unwrap()],
        )
        .unwrap();
        drop(db);
        assert!(compatible_predecessor(root.path(), &approval).is_err());
        assert!(verify_recovery_store(root.path(), &approval).is_ok());
    }

    #[test]
    fn incompatible_committed_writer_floor_cannot_restore_the_previous_app_or_data() {
        let (root, _, approval) = approved_profile("update-rollback");
        assert!(compatible_predecessor(root.path(), &approval).is_ok());
        let db_path = root.join(DATABASE_FILE);
        let connection = Connection::open(&db_path).unwrap();
        let schema = approval.profile_schema + 1;
        connection
            .execute(
                "UPDATE store_meta SET schema_version=?1,minimum_writer=?1",
                [schema],
            )
            .unwrap();
        connection.execute("INSERT INTO schema_migrations(version,checksum,applied_at,app_build) VALUES (?1,?2,5000,'new-build')",
            rusqlite::params![schema, "f".repeat(64)]).unwrap();
        let before = devworks_core::fingerprint::sha256_hex(&std::fs::read(&db_path).unwrap());
        assert!(compatible_predecessor(root.path(), &approval).is_err());
        let actual: i64 = connection
            .query_row("SELECT minimum_writer FROM store_meta", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(actual, schema);
        assert_eq!(
            devworks_core::fingerprint::sha256_hex(&std::fs::read(db_path).unwrap()),
            before
        );
    }
}
