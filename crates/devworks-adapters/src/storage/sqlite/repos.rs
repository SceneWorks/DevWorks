//! Repository implementations. All SQL lives here and in the migrations; callers see only the
//! core port traits. Read operations are free functions over `&Connection` shared by snapshots
//! and the write unit; write operations exist only on [`Unit`], which borrows the writer's open
//! transaction.
//!
//! This file is the composition point: shared value helpers, the read macro, [`Snapshot`] and
//! [`Unit`]. Each core repository lives in its own file under `repos/`.
mod actors;
mod grants;
mod history;
mod operations;
mod packages;
mod queue;
mod receipts;

use super::conn::db;
use super::support::{conflict, conflict_strict, corrupt, id_of, invalid, page_limit};
pub(crate) use actors::actor_kind;
use actors::actor_profile;
use devworks_core::context::{PrincipalKind, ReceiptIdentity};
use devworks_core::fingerprint::Fingerprint;
use devworks_core::ports::storage::*;
use devworks_core::ports::{GrantUse, IdKind, IdSource, StoredReceipt};
use devworks_core::protocol::results::DomainDecision;
use devworks_core::protocol::{
    ActorId, AssignmentId, AttemptId, CONTRACT, CommandId, EventId, ExecutionId, Id, OperationId,
    OperationState, ProjectId, Receipt, RevisionDimension, RootId, StoreId,
};
use devworks_core::registry::SchemaRevision;
use grants::grant;
use history::{AuthorityHistory, event, page};
pub(crate) use history::{EVENT_COLUMNS, event_of, event_row};
pub(super) use operations::operation;
use operations::{operations_for_capability, state_name, unresolved};
use packages::{package_state, processing};
use queue::{
    awaiting_eligibility, checkpoint, deliveries, delivery, entry, next_due, progress, status,
};
use receipts::{lookup_receipt, receipt_reply};
use rusqlite::{CachedStatement, Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// Value helpers

fn opt_id(s: Option<String>) -> Result<Option<Id>, StorageError> {
    s.map(id_of).transpose()
}
fn json_of(s: &str) -> Result<Value, StorageError> {
    serde_json::from_str(s).map_err(|e| corrupt(format!("stored JSON: {e}")))
}
fn text(v: &Value) -> String {
    v.to_string()
}
fn bounded(field: &str, s: &str, max: usize) -> Result<(), StorageError> {
    if s.is_empty() || s.len() > max || s.contains('\0') {
        return Err(invalid(format!("{field} must be 1..={max} bytes")));
    }
    Ok(())
}
fn object(field: &str, v: &Value) -> Result<(), StorageError> {
    if !v.is_object() {
        return Err(invalid(format!("{field} must be a JSON object")));
    }
    Ok(())
}
fn stmt<'c>(conn: &'c Connection, sql: &str) -> Result<CachedStatement<'c>, StorageError> {
    db(conn.prepare_cached(sql))
}
fn exists(conn: &Connection, sql: &str, id: &str) -> Result<bool, StorageError> {
    db(stmt(conn, sql)?.exists([id]))
}
fn require(
    conn: &Connection,
    table: &'static str,
    kind: &'static str,
    id: Option<&str>,
) -> Result<(), StorageError> {
    let Some(id) = id else { return Ok(()) };
    let sql = format!("SELECT 1 FROM {table} WHERE id=?1");
    if exists(conn, &sql, id)? {
        Ok(())
    } else {
        Err(StorageError::NotFound {
            kind,
            id: id.to_owned(),
        })
    }
}

pub(crate) fn principal_name(k: PrincipalKind) -> &'static str {
    match k {
        PrincipalKind::Human => "human",
        PrincipalKind::Agent => "agent",
        PrincipalKind::Package => "package",
        PrincipalKind::McpClient => "mcp_client",
        PrincipalKind::Schedule => "schedule",
        PrincipalKind::Host => "host",
    }
}
pub(crate) fn parse_principal(s: &str) -> Result<PrincipalKind, StorageError> {
    Ok(match s {
        "human" => PrincipalKind::Human,
        "agent" => PrincipalKind::Agent,
        "package" => PrincipalKind::Package,
        "mcp_client" => PrincipalKind::McpClient,
        "schedule" => PrincipalKind::Schedule,
        "host" => PrincipalKind::Host,
        other => return Err(corrupt(format!("unknown actor kind {other}"))),
    })
}

// ---------------------------------------------------------------------------------------------
// Reads shared by snapshots and the write unit

pub(crate) fn committed_sequence(conn: &Connection) -> Result<i64, StorageError> {
    db(stmt(conn, "SELECT coalesce(max(seq),0) FROM events")?.query_row([], |r| r.get(0)))
}

pub(crate) fn stamp(
    conn: &Connection,
    now_ms: i64,
    boot_id: Option<Id>,
) -> Result<SnapshotStamp, StorageError> {
    let (store_id, restore_epoch): (String, i64) = db(stmt(
        conn,
        "SELECT store_id,restore_epoch FROM store_meta WHERE singleton=1",
    )?
    .query_row([], |r| Ok((r.get(0)?, r.get(1)?))))?;
    Ok(SnapshotStamp {
        store_id: StoreId(id_of(store_id)?),
        restore_epoch,
        committed_sequence: committed_sequence(conn)?,
        now_ms,
        boot_id,
    })
}

macro_rules! read_impls {
    ($t:ty) => {
        impl ActorRead for $t {
            fn actor_kind(&self, actor: &ActorId) -> Result<Option<PrincipalKind>, StorageError> {
                actor_kind(self.conn, actor)
            }
            fn actor(&self, actor: &ActorId) -> Result<Option<StoredActor>, StorageError> {
                actor_profile(self.conn, actor)
            }
        }
        impl CommandLedgerRead for $t {
            fn lookup(
                &self,
                identity: &ReceiptIdentity,
            ) -> Result<Option<LedgerEntry>, StorageError> {
                lookup_receipt(self.conn, identity)
            }
            fn reply(
                &self,
                identity: &ReceiptIdentity,
            ) -> Result<Option<ReceiptReply>, StorageError> {
                receipt_reply(self.conn, identity)
            }
        }
        impl HistoryRead for $t {
            fn event(&self, id: &EventId) -> Result<Option<EventRecord>, StorageError> {
                event(self.conn, id)
            }
            fn page(
                &self,
                filter: &EventFilter,
                after: i64,
                limit: u16,
            ) -> Result<EventPage, StorageError> {
                page(self.conn, filter, after, limit)
            }
        }
        impl OperationRead for $t {
            fn operation(&self, id: &OperationId) -> Result<Option<OperationRecord>, StorageError> {
                operation(self.conn, id)
            }
            fn unresolved(&self, limit: u16) -> Result<Vec<OperationRecord>, StorageError> {
                unresolved(self.conn, limit)
            }
            fn operations_for_capability(
                &self,
                capability: &str,
                states: &[OperationState],
                after: Option<&OperationId>,
                limit: u16,
            ) -> Result<Vec<OperationRecord>, StorageError> {
                operations_for_capability(self.conn, capability, states, after, limit)
            }
        }
        impl QueueRead for $t {
            fn entry(&self, id: &Id) -> Result<Option<QueueEntry>, StorageError> {
                entry(self.conn, id)
            }
            fn delivery(
                &self,
                consumer: &str,
                event: &EventId,
            ) -> Result<Option<QueueEntry>, StorageError> {
                delivery(self.conn, consumer, event)
            }
            fn partition_head(
                &self,
                consumer: &str,
                partition: &str,
            ) -> Result<Option<QueueEntry>, StorageError> {
                queue::partition_head(self.conn, consumer, partition)
            }
            fn status(&self, consumer: &str) -> Result<QueueStatus, StorageError> {
                status(self.conn, consumer)
            }
            fn deliveries(
                &self,
                filter: &DeliveryFilter,
                after: Option<&DeliveryKey>,
                limit: u16,
            ) -> Result<DeliveryPage, StorageError> {
                deliveries(self.conn, filter, after, limit)
            }
            fn progress(&self, consumer: &str) -> Result<ConsumerProgress, StorageError> {
                progress(self.conn, consumer)
            }
            fn checkpoint(
                &self,
                consumer: &str,
                partition: Option<&str>,
            ) -> Result<i64, StorageError> {
                checkpoint(self.conn, consumer, partition)
            }
            fn next_due_ms(&self, now_ms: i64) -> Result<Option<i64>, StorageError> {
                next_due(self.conn, now_ms)
            }
            fn awaiting_eligibility(
                &self,
                consumer: &str,
                after: Option<&Id>,
                limit: u16,
            ) -> Result<Vec<QueueEntry>, StorageError> {
                awaiting_eligibility(self.conn, consumer, after, limit)
            }
        }
        impl PackageHandlerRead for $t {
            fn package_state(
                &self,
                package_id: &str,
            ) -> Result<Option<PackageState>, StorageError> {
                package_state(self.conn, package_id)
            }
            fn processing(
                &self,
                package_id: &str,
                subscription_id: &Id,
                event_id: &EventId,
            ) -> Result<Option<ProcessingRecord>, StorageError> {
                processing(self.conn, package_id, subscription_id, event_id)
            }
        }
        impl GrantRead for $t {
            fn grant(&self, id: &Id) -> Result<Option<GrantRecord>, StorageError> {
                grant(self.conn, id)
            }
        }
    };
}

/// One read snapshot on a query-only connection.
pub(crate) struct Snapshot<'c> {
    pub conn: &'c Connection,
    pub stamp: SnapshotStamp,
    pub host_actor: &'c ActorId,
}
read_impls!(Snapshot<'_>);
impl ReadSnapshot for Snapshot<'_> {
    fn stamp(&self) -> &SnapshotStamp {
        &self.stamp
    }
    fn actors(&self) -> &dyn ActorRead {
        self
    }
    fn receipts(&self) -> &dyn CommandLedgerRead {
        self
    }
    fn history(&self) -> &dyn HistoryRead {
        self
    }
    fn operations(&self) -> &dyn OperationRead {
        self
    }
    fn queue(&self) -> &dyn QueueRead {
        self
    }
    fn grants(&self) -> &dyn GrantRead {
        self
    }
    fn packages(&self) -> &dyn PackageHandlerRead {
        self
    }
    fn configuration(&self) -> &dyn ConfigurationRead {
        self
    }
    fn authority(&self) -> &dyn AuthorityRead {
        self
    }
    fn artifacts(&self) -> &dyn ArtifactRead {
        self
    }
    fn sessions(&self) -> &dyn SessionRead {
        self
    }
    fn resources(&self) -> &dyn ResourceRead {
        self
    }
    fn runs(&self) -> &dyn RunRead {
        self
    }
    fn diagnostics(&self) -> &dyn DiagnosticRead {
        self
    }
    fn git(&self) -> &dyn GitRead {
        self
    }
    fn browser(&self) -> &dyn BrowserRead {
        self
    }
    fn usage(&self) -> &dyn UsageRead {
        self
    }
    fn decisions(&self) -> &dyn DecisionRead {
        self
    }
    fn evals(&self) -> &dyn EvalRead {
        self
    }
    fn watchers(&self) -> &dyn WatcherRead {
        self
    }

    fn retention(&self) -> Result<&dyn RetentionRead, StorageError> {
        Ok(self)
    }
    fn updates(&self) -> Result<&dyn devworks_core::ports::update::UpdateRead, StorageError> {
        Ok(self)
    }
    fn workspace_archives(&self) -> Result<&dyn WorkspaceArchiveRead, StorageError> {
        Ok(self)
    }
    fn work_imports(&self) -> Result<&dyn WorkImportRead, StorageError> {
        Ok(self)
    }
    fn conventions(&self) -> &dyn ConventionRead {
        self
    }
    fn schemas(&self) -> &dyn SchemaRead {
        self
    }
    fn documents(&self) -> &dyn DocumentRead {
        self
    }
    fn dreaming(&self) -> Result<&dyn DreamingRead, StorageError> {
        Ok(self)
    }
    fn help(&self) -> &dyn HelpRead {
        self
    }
    fn instructions(&self) -> &dyn InstructionRead {
        self
    }
    fn github(&self) -> &dyn GithubRead {
        self
    }
    fn router(&self) -> &dyn RouterRead {
        self
    }
    fn projects(&self) -> &dyn ProjectRead {
        self
    }
    fn indexing(&self) -> &dyn IndexRead {
        self
    }
    fn graph(&self) -> &dyn GraphRead {
        self
    }
    fn knowledge(&self) -> &dyn KnowledgeRead {
        self
    }
    fn jobs(&self) -> &dyn JobRead {
        self
    }
    fn work(&self) -> &dyn WorkRead {
        self
    }
}

// ---------------------------------------------------------------------------------------------
// The write unit

/// The writer's open `BEGIN IMMEDIATE` transaction with transaction-scoped repositories.
pub(crate) struct Unit<'c> {
    pub conn: &'c Connection,
    pub now: i64,
    pub ids: &'c dyn IdSource,
    pub info: &'c StoreInfo,
    /// Open savepoint depth.
    pub savepoints: u32,
    /// Serial of this unit of work; a savepoint issued by another unit is refused.
    pub serial: u64,
    /// The capability-gated history view, built on first use.
    pub authority_log: Option<AuthorityHistory<'c>>,
}
read_impls!(Unit<'_>);

impl Unit<'_> {
    fn open_savepoint(&self, sp: &Savepoint) -> Result<u32, StorageError> {
        if sp.transaction() != self.serial || sp.depth() == 0 || sp.depth() > self.savepoints {
            return Err(invalid("savepoint is not open in this transaction"));
        }
        Ok(sp.depth())
    }
    fn boot(&self) -> Result<&Id, StorageError> {
        self.info
            .boot_id
            .as_ref()
            .ok_or_else(|| StorageError::IncompatibleSchema("store is read-only".into()))
    }
}

impl WriteTransaction for Unit<'_> {
    fn now_ms(&self) -> i64 {
        self.now
    }
    fn new_id(&mut self, kind: IdKind) -> Id {
        self.ids.next_id(kind)
    }
    fn store(&self) -> &StoreInfo {
        self.info
    }
    fn committed_sequence(&self) -> Result<i64, StorageError> {
        committed_sequence(self.conn)
    }
    fn savepoint(&mut self) -> Result<Savepoint, StorageError> {
        let depth = self.savepoints + 1;
        db(self.conn.execute_batch(&format!("SAVEPOINT dw_sp{depth}")))?;
        self.savepoints = depth;
        Ok(Savepoint::issue(self.serial, depth))
    }
    fn rollback_to(&mut self, sp: Savepoint) -> Result<(), StorageError> {
        let depth = self.open_savepoint(&sp)?;
        // SQLite semantics: unwinding an outer savepoint also discards every inner one.
        db(self
            .conn
            .execute_batch(&format!("ROLLBACK TO dw_sp{depth}; RELEASE dw_sp{depth}")))?;
        self.savepoints = depth - 1;
        Ok(())
    }
    fn release(&mut self, sp: Savepoint) -> Result<(), StorageError> {
        let depth = self.open_savepoint(&sp)?;
        db(self.conn.execute_batch(&format!("RELEASE dw_sp{depth}")))?;
        self.savepoints = depth - 1;
        Ok(())
    }
    fn actors(&mut self) -> &mut dyn ActorRegistry {
        self
    }
    fn receipts(&mut self) -> &mut dyn CommandLedger {
        self
    }
    fn history(&mut self) -> &mut dyn HistoryLog {
        self
    }
    fn operations(&mut self) -> &mut dyn OperationJournal {
        self
    }
    fn queue(&mut self) -> &mut dyn DurableQueue {
        self
    }
    fn grants(&mut self) -> &mut dyn GrantRead {
        self
    }
    fn grant_writes(
        &mut self,
        _: &devworks_core::authority::AuthorityCapability,
        _: &ActorId,
    ) -> Result<&mut dyn GrantLedger, StorageError> {
        Ok(self)
    }
    fn packages(&mut self) -> &mut dyn PackageHandlerLedger {
        self
    }
    fn configuration(&mut self) -> &mut dyn ConfigurationRepository {
        self
    }
    fn authority(&mut self) -> &mut dyn AuthorityRead {
        self
    }
    fn authority_writes(
        &mut self,
        _: &devworks_core::authority::AuthorityCapability,
        _: &ActorId,
    ) -> Result<&mut dyn AuthorityLedger, StorageError> {
        Ok(self)
    }
    fn authority_history(
        &mut self,
        _: &devworks_core::authority::AuthorityCapability,
        _: &ActorId,
    ) -> Result<&mut dyn HistoryLog, StorageError> {
        let (conn, now) = (self.conn, self.now);
        Ok(self.authority_log.insert(AuthorityHistory { conn, now }))
    }
    fn artifacts(&mut self) -> &mut dyn ArtifactRepository {
        self
    }
    fn sessions(&mut self) -> &mut dyn SessionRepository {
        self
    }
    fn resources(&mut self) -> &mut dyn ResourceRepository {
        self
    }
    fn runs(&mut self) -> &mut dyn RunRepository {
        self
    }
    fn diagnostics(&mut self) -> &mut dyn DiagnosticRepository {
        self
    }
    fn git(&mut self) -> &mut dyn GitRepository {
        self
    }
    fn browser(&mut self) -> &mut dyn BrowserRepository {
        self
    }
    fn usage(&mut self) -> &mut dyn UsageRepository {
        self
    }
    fn decisions(&mut self) -> &mut dyn DecisionRepository {
        self
    }
    fn evals(&mut self) -> &mut dyn EvalRepository {
        self
    }
    fn watchers(&mut self) -> &mut dyn WatcherRepository {
        self
    }

    fn retention(&mut self) -> Result<&mut dyn RetentionRepository, StorageError> {
        Ok(self)
    }
    fn updates(
        &mut self,
    ) -> Result<&mut dyn devworks_core::ports::update::UpdateRepository, StorageError> {
        Ok(self)
    }
    fn workspace_archives(&mut self) -> Result<&mut dyn WorkspaceArchiveRepository, StorageError> {
        Ok(self)
    }
    fn work_imports(&mut self) -> Result<&mut dyn WorkImportRepository, StorageError> {
        Ok(self)
    }
    fn conventions(&mut self) -> &mut dyn ConventionRepository {
        self
    }
    fn schemas(&mut self) -> &mut dyn SchemaRepository {
        self
    }
    fn documents(&mut self) -> &mut dyn DocumentRepository {
        self
    }
    fn dreaming(&mut self) -> Result<&mut dyn DreamingRepository, StorageError> {
        Ok(self)
    }
    fn help(&mut self) -> &mut dyn HelpRepository {
        self
    }
    fn instructions(&mut self) -> &mut dyn InstructionRepository {
        self
    }
    fn github(&mut self) -> &mut dyn GithubRepository {
        self
    }
    fn router(&mut self) -> &mut dyn RouterRepository {
        self
    }
    fn projects(&mut self) -> &mut dyn ProjectRepository {
        self
    }
    fn indexing(&mut self) -> &mut dyn IndexRepository {
        self
    }
    fn graph(&mut self) -> &mut dyn GraphRepository {
        self
    }
    fn knowledge(&mut self) -> &mut dyn KnowledgeRepository {
        self
    }
    fn jobs(&mut self) -> &mut dyn JobRepository {
        self
    }
    fn work(&mut self) -> &mut dyn WorkRepository {
        self
    }
}
