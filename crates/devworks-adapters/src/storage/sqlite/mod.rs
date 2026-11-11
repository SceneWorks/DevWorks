//! The SQLite implementation of `devworks_core::ports::storage` (STORE-01/08/09/11).
//!
//! # Boundary and runtime rationale
//!
//! * **One embedded file, no daemon (ARCH-01, STORE-01).** `workspace.sqlite` in the data root
//!   holds authoritative records, history, the durable queue/outbox and later the graph/FTS5/
//!   sqlite-vec tables, so related mutations stay atomic without multi-file coordination.
//! * **One write owner per data root.** An OS advisory lock on `workspace.lock` admits one store
//!   per data root; a second open fails with `DataRootInUse`. Inside the process one dedicated
//!   writer thread owns the only write connection (module `writer`); reads use a bounded pool of
//!   query-only connections on the caller's (non-UI) thread.
//! * **Verified durability.** WAL, `foreign_keys=ON` on every connection, `synchronous=FULL` and
//!   (macOS) `fullfsync` on the writer are read back after being set; startup refuses SQLite
//!   older than 3.51.3, missing FTS5/thread safety, or an unexpected sqlite-vec identity. Only the
//!   writer commits, so only it checkpoints; readers are closed before the writer on shutdown.
//! * **Schema gate before writes (STORE-06).** Module `migrations` checks store identity, minimum
//!   reader/writer versions and the checksum ledger before any write access; a newer
//!   incompatible store opens read-only for diagnostics, never migrated or rewritten.
//! * **No automatic backup (STORE-05/07/13).** Nothing here copies the store; manual backup and
//!   restore extend [`StorageMaintenance`] in their own item.
//! * **SQL stays here.** Repositories (module `repos`) are the only place statements exist. Nothing in
//!   the public API exposes rusqlite types, rows, statements or a generic execute-SQL.
//! * **Async facade.** [`write_async`] wraps the same admission with a Tokio `oneshot`; dropping
//!   the future stops waiting but never rolls back an admitted job.
mod agent_messages;
mod allocations;
mod artifacts;
mod authority;
mod browser;
mod configuration;
mod conn;
mod connections;
mod conventions;
mod decisions;
mod diagnostics;
mod embedding;
mod evals;
mod git;
mod github;
mod graph;
mod handoff;
mod help;
mod indexing;
mod instructions;
mod jobs;
mod knowledge;
mod migrations;
mod notification_delivery;
mod packages;
mod project_purge;
mod projects;
mod repos;
mod resources;
mod router;
mod runs;
mod schemas;
#[cfg(any(test, feature = "test-support"))]
mod seed;
mod sessions;
mod usage;
mod work;
mod writer;

#[cfg(test)]
mod tests;

pub use conn::{EXPECTED_VEC_VERSION, EffectiveSettings, MIN_SQLITE_VERSION};
pub use migrations::{
    MIGRATION_0001_SHA256, MIGRATION_0002_SHA256, MIGRATION_0003_SHA256, MIGRATION_0004_SHA256,
    MIGRATION_0005_SHA256, MIGRATION_0006_SHA256, MIGRATION_0007_SHA256, MIGRATION_0008_SHA256,
    MIGRATION_0009_SHA256, MIGRATION_0010_SHA256, MIGRATION_0011_SHA256, MIGRATION_0012_SHA256,
    MIGRATION_0013_SHA256, MIGRATION_0014_SHA256, MIGRATION_0015_SHA256, MIGRATION_0016_SHA256,
    MIGRATION_0017_SHA256, MIGRATION_0018_SHA256, MIGRATION_0019_SHA256, MIGRATION_0020_SHA256,
    MIGRATION_0021_SHA256, MIGRATION_0022_SHA256, MIGRATION_0023_SHA256, MIGRATION_0024_SHA256,
    MIGRATION_0025_SHA256, MIGRATION_0026_SHA256, MIGRATION_0027_SHA256, MIGRATION_0028_SHA256,
    MIGRATION_0029_SHA256, MIGRATION_0030_SHA256, MIGRATION_0031_SHA256, MIGRATION_0032_SHA256,
    MIGRATION_0033_SHA256, MIGRATION_0034_SHA256, MIGRATION_0035_SHA256, MIGRATIONS, Migration,
};
#[cfg(any(test, feature = "test-support"))]
pub use seed::seed_migrated_store;

use conn::{Role, db};
use devworks_core::context::PrincipalKind;
use devworks_core::ports::storage::{
    AccessMode, ApplicationStore, Cancellation, CompatibilityReport, IntegrityReport, ReadOptions,
    ReadSnapshot, RuntimeIdentity, StorageError, StorageMaintenance, StoreInfo, Unavailable,
    WriteOptions, WriteTransaction, write_job,
};
use devworks_core::ports::{Clock, IdKind, IdSource};
use devworks_core::protocol::{ActorId, Id, StoreId};
use rusqlite::{Connection, OptionalExtension, params};
use std::fs::{File, OpenOptions, TryLockError};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use writer::{WriterConfig, WriterHandle};

pub const DATABASE_FILE: &str = "workspace.sqlite";
pub const LOCK_FILE: &str = "workspace.lock";

/// Composition-root configuration.
pub struct SqliteConfig {
    pub data_root: PathBuf,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdSource>,
    /// Host principal that attributes store-owned bookkeeping.
    pub host_actor: ActorId,
    /// Application build identity recorded with applied migrations.
    pub app_build: String,
    /// Size of the query-only read connection pool.
    pub read_connections: usize,
    /// Bound of each writer mailbox (per priority class).
    pub mailbox_capacity: usize,
    /// After this many consecutive authoritative jobs with bulk work waiting, one bulk job runs.
    pub bulk_share: u32,
    /// Longest wait for a free read connection before `Busy`.
    pub read_wait: Duration,
    /// Bounded SQLite busy handling per connection.
    pub busy_timeout: Duration,
}
impl SqliteConfig {
    pub fn new(
        data_root: impl Into<PathBuf>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
        host_actor: ActorId,
    ) -> Self {
        Self {
            data_root: data_root.into(),
            clock,
            ids,
            host_actor,
            app_build: concat!("devworks-adapters/", env!("CARGO_PKG_VERSION")).to_owned(),
            read_connections: 4,
            mailbox_capacity: 64,
            bulk_share: 4,
            read_wait: Duration::from_secs(5),
            busy_timeout: Duration::from_secs(5),
        }
    }
}

/// Longest delay before a cancelled read stops waiting for a free connection.
const CANCEL_POLL: Duration = Duration::from_millis(10);

struct ReadPool {
    idle: Mutex<Vec<Connection>>,
    available: Condvar,
    wait: Duration,
}
impl ReadPool {
    fn checkout(
        &self,
        cancel: Option<&Cancellation>,
        operation_deadline: Option<Instant>,
    ) -> Result<Connection, StorageError> {
        let deadline = Instant::now() + self.wait;
        let mut idle = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if cancel.is_some_and(Cancellation::is_cancelled)
                || operation_deadline.is_some_and(|until| Instant::now() >= until)
            {
                return Err(StorageError::Cancelled);
            }
            if let Some(c) = idle.pop() {
                return Ok(c);
            }
            let until = operation_deadline.map_or(deadline, |until| until.min(deadline));
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(
                    if operation_deadline.is_some_and(|until| Instant::now() >= until) {
                        StorageError::Cancelled
                    } else {
                        StorageError::Busy
                    },
                );
            }
            // Wake at least every CANCEL_POLL so a cancelled caller stops waiting promptly.
            idle = self
                .available
                .wait_timeout(idle, left.min(CANCEL_POLL))
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }
    fn checkin(&self, conn: Connection) {
        self.idle
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(conn);
        self.available.notify_one();
    }
}

/// Ends the read transaction and returns the connection to the pool, even on panic.
struct ReadLease<'p> {
    pool: &'p ReadPool,
    conn: Option<Connection>,
}
impl Drop for ReadLease<'_> {
    fn drop(&mut self) {
        if let Some(mut conn) = self.conn.take() {
            // Remove callbacks before cleanup on success, error, cancellation AND panic. A
            // connection whose hook or rollback cannot be cleared never re-enters the pool.
            if conn.progress_handler(0, None::<fn() -> bool>).is_err() {
                return;
            }
            if !conn.is_autocommit() && conn.execute_batch("ROLLBACK").is_err() {
                return;
            }
            self.pool.checkin(conn);
        }
    }
}

/// The opened store. Dropping it closes readers, stops the writer (recording a clean boot
/// shutdown) and releases the data-root lock, in that order.
pub struct SqliteStore {
    writer: Option<WriterHandle>,
    readers: ReadPool,
    info: StoreInfo,
    runtime: RuntimeIdentity,
    clock: Arc<dyn Clock>,
    build_schema_version: i64,
    path: PathBuf,
    previous_shutdown: Option<String>,
    _lock: File,
}

impl SqliteStore {
    /// Opens (creating if absent) the store in `config.data_root`.
    pub fn open(config: SqliteConfig) -> Result<Self, StorageError> {
        Self::open_with(config, MIGRATIONS)
    }

    pub(crate) fn open_with(
        config: SqliteConfig,
        migrations: &[Migration],
    ) -> Result<Self, StorageError> {
        let io = |e: std::io::Error| StorageError::unavailable(Unavailable::Io, e.to_string());
        std::fs::create_dir_all(&config.data_root).map_err(io)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(config.data_root.join(LOCK_FILE))
            .map_err(io)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(StorageError::unavailable(
                    Unavailable::DataRootInUse,
                    "another DevWorks process owns this data root",
                ));
            }
            Err(TryLockError::Error(e)) => return Err(io(e)),
        }
        let path = config.data_root.join(DATABASE_FILE);
        // Gate on a read-only connection first: a foreign file or a store this build may not read
        // is refused before any writer PRAGMA (journal mode, fsync policy) can touch it.
        let gated = if path.exists() {
            migrations::check(&conn::open_probe(&path)?, migrations)?
        } else {
            None
        };
        let read_only = matches!(gated, Some((_, AccessMode::ReadOnly { .. })));
        let now = config.clock.now_ms().get();
        let mut previous_shutdown = None;
        let (writer_conn, runtime, meta, access, boot_id) = if read_only {
            let Some((meta, access)) = gated else {
                unreachable!("read-only implies a gated store")
            };
            let probe = conn::open(&path, Role::Reader, config.busy_timeout)?;
            let runtime = conn::verify_runtime(&probe)?;
            (None, runtime, meta, access, None)
        } else {
            let writer_conn = conn::open(&path, Role::Writer, config.busy_timeout)?;
            let runtime = conn::verify_runtime(&writer_conn)?;
            let ids = config.ids.clone();
            let (meta, access) = migrations::prepare(
                &writer_conn,
                migrations,
                || ids.next_id(IdKind::Store).as_str().to_owned(),
                now,
                &config.app_build,
            )?;
            let boot = config.ids.next_id(IdKind::Boot);
            previous_shutdown = record_boot(&writer_conn, &config, &boot, now)?;
            (Some(writer_conn), runtime, meta, access, Some(boot))
        };
        // Pure expected metadata belongs to store boot, before a runtime can schedule its
        // monitor. Replaying the catalogue after shutdown could otherwise strand Tokio's
        // blocking-pool destructor. This never validates or substitutes the actual schema.
        diagnostics::prepare_expected_schema(meta.schema_version);
        let store_id = StoreId(
            Id::new(meta.store_id.clone())
                .map_err(|_| StorageError::IncompatibleSchema("invalid store identity".into()))?,
        );
        let info = StoreInfo {
            store_id,
            restore_epoch: meta.restore_epoch,
            boot_id,
            host_actor: config.host_actor.clone(),
            access,
        };
        let mut idle = Vec::with_capacity(config.read_connections.max(1));
        for _ in 0..config.read_connections.max(1) {
            idle.push(conn::open(&path, Role::Reader, config.busy_timeout)?);
        }
        let writer = match writer_conn {
            Some(writer_conn) => Some(WriterHandle::start(
                writer_conn,
                WriterConfig {
                    capacity: config.mailbox_capacity,
                    bulk_share: config.bulk_share,
                    clock: config.clock.clone(),
                    ids: config.ids.clone(),
                    info: info.clone(),
                },
            )?),
            None => None,
        };
        Ok(Self {
            writer,
            readers: ReadPool {
                idle: Mutex::new(idle),
                available: Condvar::new(),
                wait: config.read_wait,
            },
            info,
            runtime,
            clock: config.clock,
            build_schema_version: migrations.last().map_or(0, |m| m.version),
            path,
            previous_shutdown,
            _lock: lock,
        })
    }

    /// Shutdown state of the most recent earlier boot (`Clean` or `Interrupted`), if any.
    pub fn previous_shutdown(&self) -> Option<&str> {
        self.previous_shutdown.as_deref()
    }

    /// Effective settings of one read connection (diagnostics).
    pub fn reader_settings(&self) -> Result<EffectiveSettings, StorageError> {
        let lease = self.lease(None)?;
        conn::effective(lease.conn.as_ref().expect("leased"))
    }

    /// Effective settings of the write connection, read on the write owner.
    pub fn writer_settings(&self) -> Result<EffectiveSettings, StorageError> {
        self.with_writer_connection(conn::effective)
    }

    fn lease(&self, cancel: Option<&Cancellation>) -> Result<ReadLease<'_>, StorageError> {
        self.lease_bounded(cancel, None)
    }
    fn lease_bounded(
        &self,
        cancel: Option<&Cancellation>,
        deadline: Option<Instant>,
    ) -> Result<ReadLease<'_>, StorageError> {
        Ok(ReadLease {
            pool: &self.readers,
            conn: Some(self.readers.checkout(cancel, deadline)?),
        })
    }

    /// Runs `f` with the raw write connection on the write owner, outside any transaction.
    /// Adapter-internal (diagnostics and tests); never exposed through a port.
    pub(crate) fn with_writer_connection<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, StorageError> + Send + 'static,
    ) -> Result<T, StorageError> {
        let writer = self
            .writer
            .as_ref()
            .ok_or_else(|| StorageError::IncompatibleSchema("store is read-only".into()))?;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        writer.submit_raw(Box::new(move |conn: &Connection| {
            let _ = tx.send(f(conn));
        }))?;
        rx.recv().unwrap_or(Err(StorageError::CommitUnknown))
    }

    #[cfg(test)]
    pub(crate) fn lose_next_ack(&self) {
        if let Some(w) = &self.writer {
            w.faults
                .lose_next_ack
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    pub fn database_path(&self) -> &Path {
        &self.path
    }
}

/// Registers the host actor, marks earlier unfinished boots as interrupted and records this boot,
/// in one transaction. Returns the previous boot's shutdown state.
fn record_boot(
    conn: &Connection,
    config: &SqliteConfig,
    boot: &Id,
    now: i64,
) -> Result<Option<String>, StorageError> {
    db(conn.execute_batch("BEGIN IMMEDIATE"))?;
    let result = (|| {
        let previous: Option<String> = db(conn
            .query_row(
                "SELECT CASE WHEN stopped_at IS NULL THEN 'Interrupted' ELSE shutdown_state END FROM app_boots ORDER BY started_at DESC, rowid DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional())?;
        db(conn.execute(
            "UPDATE app_boots SET shutdown_state='Interrupted' WHERE stopped_at IS NULL",
            [],
        ))?;
        db(conn.execute(
            "INSERT INTO actors(id,kind,display_name) SELECT ?1,?2,?3 WHERE NOT EXISTS(SELECT 1 FROM actors WHERE id=?1)",
            params![
                config.host_actor.0.as_str(),
                repos::principal_name(PrincipalKind::Host),
                "DevWorks host"
            ],
        ))?;
        db(conn.execute(
            "INSERT INTO app_boots(id,started_at,stopped_at,shutdown_state) VALUES(?1,?2,NULL,'Open')",
            params![boot.as_str(), now],
        ))?;
        Ok(previous)
    })();
    match result {
        Ok(previous) => {
            db(conn.execute_batch("COMMIT"))?;
            Ok(previous)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

impl Drop for SqliteStore {
    fn drop(&mut self) {
        // Readers first so the last connection to close is the writer (the only checkpointer).
        self.readers
            .idle
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        if let Some(mut w) = self.writer.take() {
            w.stop();
        }
    }
}

impl ApplicationStore for SqliteStore {
    fn info(&self) -> StoreInfo {
        self.info.clone()
    }

    fn submit_write(
        &self,
        options: WriteOptions,
        job: Box<dyn devworks_core::ports::storage::WriteJob>,
    ) -> Result<(), StorageError> {
        match &self.writer {
            Some(w) => w.submit(options, job),
            None => Err(StorageError::IncompatibleSchema(match &self.info.access {
                AccessMode::ReadOnly { reason } => reason.clone(),
                AccessMode::ReadWrite => "write owner unavailable".into(),
            })),
        }
    }

    fn read(
        &self,
        options: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        let now = self.clock.now_ms().get();
        let monotonic = options.deadline_ms.and_then(|until| {
            Instant::now().checked_add(Duration::from_millis(
                until.saturating_sub(now).max(0) as u64
            ))
        });
        let expired = || {
            options
                .cancellation
                .as_ref()
                .is_some_and(Cancellation::is_cancelled)
                || monotonic.is_some_and(|until| Instant::now() >= until)
                || options
                    .deadline_ms
                    .is_some_and(|until| self.clock.now_ms().get() >= until)
        };
        if expired() {
            return Err(StorageError::Cancelled);
        }
        let mut lease = self.lease_bounded(options.cancellation.as_ref(), monotonic)?;
        if expired() {
            return Err(StorageError::Cancelled);
        }
        let interrupted = Arc::new(AtomicBool::new(false));
        let flag = interrupted.clone();
        let cancel = options.cancellation.clone();
        let clock = self.clock.clone();
        let deadline = options.deadline_ms;
        db(lease.conn.as_mut().expect("leased").progress_handler(
            1000,
            Some(move || {
                let stop = cancel.as_ref().is_some_and(Cancellation::is_cancelled)
                    || monotonic.is_some_and(|until| Instant::now() >= until)
                    || deadline.is_some_and(|until| clock.now_ms().get() >= until);
                if stop {
                    flag.store(true, Ordering::SeqCst);
                }
                stop
            }),
        ))?;
        let conn = lease.conn.as_ref().expect("leased");
        let result = (|| {
            db(conn.execute_batch("BEGIN DEFERRED"))?;
            // The first read establishes the snapshot; every statement shares this snapshot
            // and the same VM progress bound. The lease unregisters before rollback/check-in.
            let stamp = repos::stamp(conn)?;
            let snapshot = repos::Snapshot {
                conn,
                stamp,
                host_actor: &self.info.host_actor,
            };
            work(&snapshot)?;
            if expired() {
                return Err(StorageError::Cancelled);
            }
            db(conn.execute_batch("COMMIT"))
        })();
        if interrupted.load(Ordering::SeqCst) {
            Err(StorageError::Cancelled)
        } else {
            result
        }
    }
}

impl StorageMaintenance for SqliteStore {
    fn compatibility(&self) -> Result<CompatibilityReport, StorageError> {
        let lease = self.lease(None)?;
        let conn = lease.conn.as_ref().expect("leased");
        let meta = migrations::read_meta(conn)?
            .ok_or_else(|| StorageError::IncompatibleSchema("store_meta missing".into()))?;
        Ok(CompatibilityReport {
            build_schema_version: self.build_schema_version,
            store_schema_version: meta.schema_version,
            minimum_reader: meta.minimum_reader,
            minimum_writer: meta.minimum_writer,
            access: self.info.access.clone(),
            migrations: migrations::ledger(conn)?,
        })
    }

    fn runtime_identity(&self) -> RuntimeIdentity {
        self.runtime.clone()
    }

    fn integrity_check(&self) -> Result<IntegrityReport, StorageError> {
        let lease = self.lease(None)?;
        let conn = lease.conn.as_ref().expect("leased");
        let mut problems = Vec::new();
        let mut stmt = db(conn.prepare("PRAGMA integrity_check"))?;
        let rows: Vec<String> = db(db(stmt.query_map([], |r| r.get(0)))?.collect())?;
        problems.extend(rows.into_iter().filter(|r| r != "ok"));
        let mut stmt =
            db(conn.prepare("SELECT \"table\",rowid,parent FROM pragma_foreign_key_check"))?;
        let fks: Vec<String> = db(db(stmt.query_map([], |r| {
            Ok(format!(
                "foreign key violation in {} row {:?} -> {}",
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, String>(2)?
            ))
        }))?
        .collect())?;
        problems.extend(fks);
        Ok(IntegrityReport { problems })
    }
}

/// Async facade over the same write admission: the future resolves after the outcome is known.
/// Dropping it stops waiting; it never rolls back an admitted job (use a [`Cancellation`]).
pub fn write_async<T, F>(
    store: &dyn ApplicationStore,
    options: WriteOptions,
    work: F,
) -> impl Future<Output = Result<T, StorageError>> + Send + 'static
where
    T: Send + 'static,
    F: FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let admitted = store.submit_write(
        options,
        write_job(work, move |r| {
            let _ = tx.send(r);
        }),
    );
    async move {
        admitted?;
        rx.await.unwrap_or(Err(StorageError::CommitUnknown))
    }
}
