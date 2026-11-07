//! Exact-row exception for immutable tables during one reviewed purge writer unit.
use crate::storage::sqlite::conn::db;
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::storage::StorageError;
use rusqlite::{Connection, functions::FunctionFlags};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicI64, Ordering};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Permit {
    pub table: String,
    pub key_digest: String,
    pub action: String,
}
struct Active {
    connection: i64,
    rows: BTreeSet<Permit>,
}
thread_local! {
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
}
static CONNECTIONS: AtomicI64 = AtomicI64::new(1);

pub(super) fn key_digest(json: &str) -> Result<String, StorageError> {
    let value: Vec<serde_json::Value> = serde_json::from_str(json)
        .map_err(|_| StorageError::InvalidRecord("purge primary-key tuple".into()))?;
    Ok(sha256_hex(
        serde_json::to_string(&value)
            .expect("JSON tuple")
            .as_bytes(),
    ))
}

pub(crate) fn register(conn: &Connection, writer: bool) -> Result<(), StorageError> {
    let connection = if writer {
        CONNECTIONS.fetch_add(1, Ordering::Relaxed)
    } else {
        0
    };
    db(conn.create_scalar_function(
        "project_purge_connection",
        0,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        move |_| Ok(connection),
    ))?;
    // Not deterministic: the scoped Rust guard may change between statements.
    db(conn.create_scalar_function(
        "project_purge_allowed",
        3,
        FunctionFlags::SQLITE_UTF8,
        move |ctx| {
            let table = ctx.get::<String>(0)?;
            let key = ctx.get::<String>(1)?;
            let action = ctx.get::<String>(2)?;
            let digest = match key_digest(&key) {
                Ok(d) => d,
                Err(_) => return Ok(false),
            };
            Ok(connection != 0
                && ACTIVE.with(|slot| {
                    slot.borrow().as_ref().is_some_and(|active| {
                        active.connection == connection
                            && active.rows.contains(&Permit {
                                table,
                                key_digest: digest,
                                action,
                            })
                    })
                }))
        },
    ))
}

struct Reset;
impl Drop for Reset {
    fn drop(&mut self) {
        ACTIVE.with(|slot| *slot.borrow_mut() = None);
    }
}

pub(super) fn with_guard<T>(
    conn: &Connection,
    rows: BTreeSet<Permit>,
    work: impl FnOnce() -> Result<T, StorageError>,
) -> Result<T, StorageError> {
    if conn.is_autocommit() {
        return Err(StorageError::InvalidRecord(
            "purge needs the active writer transaction".into(),
        ));
    }
    let connection: i64 =
        db(conn.query_row("SELECT project_purge_connection()", [], |r| r.get(0)))?;
    if connection <= 0 {
        return Err(StorageError::ScopeDenied);
    }
    ACTIVE.with(|slot| {
        let mut active = slot.borrow_mut();
        if active.is_some() {
            return Err(StorageError::Busy);
        }
        *active = Some(Active { connection, rows });
        Ok(())
    })?;
    let _reset = Reset;
    work()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn connection(writer: bool) -> Connection {
        let c = Connection::open_in_memory().unwrap();
        register(&c, writer).unwrap();
        c.execute_batch("CREATE TABLE guarded(id TEXT PRIMARY KEY,body TEXT); INSERT INTO guarded VALUES('a','private-a'),('b','private-b'); CREATE TRIGGER immutable_guard BEFORE DELETE ON guarded WHEN NOT project_purge_allowed('guarded',json_array(OLD.id),'Delete') BEGIN SELECT RAISE(ABORT,'immutable'); END;").unwrap();
        c
    }
    fn permits() -> BTreeSet<Permit> {
        [Permit {
            table: "guarded".into(),
            key_digest: key_digest("[\"a\"]").unwrap(),
            action: "Delete".into(),
        }]
        .into_iter()
        .collect()
    }
    #[test]
    fn exact_rows_and_connection_are_required_and_error_resets_the_guard() {
        let c = connection(true);
        let other = connection(true);
        assert!(c.execute("DELETE FROM guarded WHERE id='a'", []).is_err());
        assert!(with_guard(&c, permits(), || Ok(())).is_err());
        c.execute_batch("BEGIN IMMEDIATE").unwrap();
        let result: Result<(), StorageError> = with_guard(&c, permits(), || {
            assert!(
                other
                    .execute("DELETE FROM guarded WHERE id='a'", [])
                    .is_err()
            );
            assert!(c.execute("DELETE FROM guarded WHERE id='b'", []).is_err());
            assert_eq!(db(c.execute("DELETE FROM guarded WHERE id='a'", []))?, 1);
            Err(StorageError::ScopeDenied)
        });
        assert!(result.is_err());
        assert!(c.execute("DELETE FROM guarded WHERE id='b'", []).is_err());
        c.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            c.query_row("SELECT count(*) FROM guarded", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }
    #[test]
    fn unwind_resets_the_guard_and_reader_connections_never_enable_it() {
        let c = connection(true);
        c.execute_batch("BEGIN IMMEDIATE").unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<(), StorageError> =
                with_guard(&c, permits(), || panic!("controlled guard unwind"));
        }));
        assert!(result.is_err());
        assert!(c.execute("DELETE FROM guarded WHERE id='a'", []).is_err());
        c.execute_batch("ROLLBACK").unwrap();
        let reader = connection(false);
        reader.execute_batch("BEGIN").unwrap();
        assert!(matches!(
            with_guard(&reader, permits(), || Ok(())),
            Err(StorageError::ScopeDenied)
        ));
        reader.execute_batch("ROLLBACK").unwrap();
    }
    #[test]
    fn native_immutable_replay_guard_allows_only_the_reviewed_primary_key_and_keeps_updates_immutable()
     {
        let h = crate::storage::sqlite::tests::Harness::new();
        drop(h.store());
        let c = h.raw();
        register(&c, true).unwrap();
        c.execute("INSERT INTO projects(id,title,lifecycle,revision,scope_revision,created_at) VALUES('guard-project','Fixture','Deleted',1,1,0)",[]).unwrap();
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        for key in [&a, &b] {
            c.execute("INSERT INTO allocation_replays(project_id,key,intent_digest,result_json) VALUES('guard-project',?1,?2,'{}')",rusqlite::params![key,"c".repeat(64)]).unwrap();
        }
        assert!(
            c.execute("DELETE FROM allocation_replays WHERE key=?1", [&a])
                .is_err()
        );
        c.execute_batch("BEGIN IMMEDIATE").unwrap();
        let allowed = [Permit {
            table: "allocation_replays".into(),
            key_digest: key_digest(&serde_json::json!([a]).to_string()).unwrap(),
            action: "Delete".into(),
        }]
        .into_iter()
        .collect();
        let outcome: Result<(), StorageError> = with_guard(&c, allowed, || {
            assert!(
                c.execute(
                    "UPDATE allocation_replays SET result_json='{}' WHERE key=?1",
                    [&a]
                )
                .is_err()
            );
            assert!(
                c.execute("DELETE FROM allocation_replays WHERE key=?1", [&b])
                    .is_err()
            );
            assert_eq!(
                db(c.execute("DELETE FROM allocation_replays WHERE key=?1", [&a]))?,
                1
            );
            Err(StorageError::ScopeDenied)
        });
        assert!(outcome.is_err());
        c.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            c.query_row("SELECT count(*) FROM allocation_replays", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert!(
            c.execute("DELETE FROM allocation_replays WHERE key=?1", [&a])
                .is_err()
        );
    }
}
