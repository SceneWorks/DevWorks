//! Connection setup, effective-PRAGMA verification, runtime identity and driver error mapping.
use devworks_core::ports::storage::{RuntimeIdentity, StorageError, Unavailable};
use rusqlite::{Connection, OpenFlags, ffi};
use std::os::raw::{c_char, c_int};
use std::path::Path;
use std::time::Duration;

/// Minimum linked SQLite: the release line with the WAL-reset fix (AD 21).
pub const MIN_SQLITE_VERSION: i32 = 3_051_003;
/// Bundled sqlite-vec identity this build verifies at startup.
pub const EXPECTED_VEC_VERSION: &str = "v0.1.7";

pub(crate) fn db_err(e: rusqlite::Error) -> StorageError {
    match &e {
        rusqlite::Error::SqliteFailure(f, msg) => {
            let diagnostic = msg.clone().unwrap_or_else(|| f.to_string());
            match f.code {
                ffi::ErrorCode::DiskFull => {
                    StorageError::unavailable(Unavailable::DiskFull, diagnostic)
                }
                ffi::ErrorCode::DatabaseBusy => StorageError::Busy,
                ffi::ErrorCode::DatabaseLocked => {
                    StorageError::unavailable(Unavailable::Locked, diagnostic)
                }
                ffi::ErrorCode::DatabaseCorrupt | ffi::ErrorCode::NotADatabase => {
                    StorageError::unavailable(Unavailable::Corrupt, diagnostic)
                }
                ffi::ErrorCode::ReadOnly => {
                    StorageError::unavailable(Unavailable::ReadOnly, diagnostic)
                }
                ffi::ErrorCode::ConstraintViolation => StorageError::InvalidRecord(diagnostic),
                _ => StorageError::unavailable(Unavailable::Io, diagnostic),
            }
        }
        rusqlite::Error::FromSqlConversionFailure(..)
        | rusqlite::Error::InvalidColumnType(..)
        | rusqlite::Error::IntegralValueOutOfRange(..) => {
            StorageError::unavailable(Unavailable::Corrupt, e.to_string())
        }
        _ => StorageError::unavailable(Unavailable::Io, e.to_string()),
    }
}
pub(crate) fn db<T>(r: rusqlite::Result<T>) -> Result<T, StorageError> {
    r.map_err(db_err)
}

/// Registers the bundled sqlite-vec functions on one connection. Per-connection registration
/// avoids the process-global `sqlite3_auto_extension` table (no global mutable state); user
/// extensions are never loaded into the internal store.
fn register_vec(conn: &Connection) -> Result<(), StorageError> {
    type Init = unsafe extern "C" fn(
        *mut ffi::sqlite3,
        *mut *mut c_char,
        *const ffi::sqlite3_api_routines,
    ) -> c_int;
    // SAFETY: sqlite-vec 0.1.7 is compiled with SQLITE_CORE, so its entry point has the standard
    // extension signature `int(sqlite3*, char**, const sqlite3_api_routines*)` and ignores the
    // routines pointer; the crate merely declares it without parameters. `conn.handle()` is a
    // valid open handle for the duration of the call, and the error-message out-pointer may be
    // null.
    let rc = unsafe {
        let init =
            std::mem::transmute::<*const (), Init>(sqlite_vec::sqlite3_vec_init as *const ());
        init(conn.handle(), std::ptr::null_mut(), std::ptr::null())
    };
    if rc != ffi::SQLITE_OK {
        return Err(StorageError::unavailable(
            Unavailable::UnsupportedRuntime,
            format!("sqlite-vec initialization failed ({rc})"),
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Writer,
    Reader,
}

/// A read-only connection used to inspect a file before any write-capable connection (and its
/// journal-mode/fsync PRAGMAs) is opened on it.
pub(crate) fn open_probe(path: &Path) -> Result<Connection, StorageError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    db(Connection::open_with_flags(path, flags))
}

pub(crate) fn open(path: &Path, role: Role, busy: Duration) -> Result<Connection, StorageError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = db(Connection::open_with_flags(path, flags))?;
    register_vec(&conn)?;
    super::conventions::register_functions(&conn)?;
    super::graph::register_name_match(&conn)?;
    super::project_purge::guard::register(&conn, role == Role::Writer)?;
    db(conn.busy_timeout(busy))?;
    db(conn.pragma_update(None, "foreign_keys", true))?;
    match role {
        Role::Writer => {
            let mode: String =
                db(conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0)))?;
            if !mode.eq_ignore_ascii_case("wal") {
                return Err(StorageError::unavailable(
                    Unavailable::UnsupportedRuntime,
                    format!("journal_mode is {mode}, not WAL"),
                ));
            }
            db(conn.pragma_update(None, "synchronous", "FULL"))?;
            if cfg!(target_os = "macos") {
                db(conn.pragma_update(None, "fullfsync", true))?;
                db(conn.pragma_update(None, "checkpoint_fullfsync", true))?;
            }
        }
        Role::Reader => db(conn.pragma_update(None, "query_only", true))?,
    }
    verify_effective(&conn, role)?;
    Ok(conn)
}

fn pragma_i64(conn: &Connection, name: &str) -> Result<i64, StorageError> {
    db(conn.pragma_query_value(None, name, |r| r.get(0)))
}

/// Effective settings, read back rather than assumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveSettings {
    pub journal_mode: String,
    pub foreign_keys: bool,
    /// SQLite numeric level: 2 = FULL.
    pub synchronous: i64,
    pub query_only: bool,
    pub fullfsync: bool,
}
pub(crate) fn effective(conn: &Connection) -> Result<EffectiveSettings, StorageError> {
    Ok(EffectiveSettings {
        journal_mode: db(conn.pragma_query_value(None, "journal_mode", |r| r.get(0)))?,
        foreign_keys: pragma_i64(conn, "foreign_keys")? == 1,
        synchronous: pragma_i64(conn, "synchronous")?,
        query_only: pragma_i64(conn, "query_only")? == 1,
        fullfsync: pragma_i64(conn, "fullfsync")? == 1,
    })
}
fn verify_effective(conn: &Connection, role: Role) -> Result<(), StorageError> {
    let s = effective(conn)?;
    let mut problems = Vec::new();
    if !s.journal_mode.eq_ignore_ascii_case("wal") {
        problems.push(format!("journal_mode={}", s.journal_mode));
    }
    if !s.foreign_keys {
        problems.push("foreign_keys off".to_owned());
    }
    match role {
        Role::Writer => {
            if s.synchronous != 2 {
                problems.push(format!("synchronous={}", s.synchronous));
            }
            if cfg!(target_os = "macos") && !s.fullfsync {
                problems.push("fullfsync off".to_owned());
            }
        }
        Role::Reader => {
            if !s.query_only {
                problems.push("reader not query_only".to_owned());
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        // Never continue with weaker durability than requested.
        Err(StorageError::unavailable(
            Unavailable::UnsupportedRuntime,
            problems.join(", "),
        ))
    }
}

/// Verifies the linked engine version, required compile options and the sqlite-vec identity.
pub(crate) fn verify_runtime(conn: &Connection) -> Result<RuntimeIdentity, StorageError> {
    let unsupported = |m: String| StorageError::unavailable(Unavailable::UnsupportedRuntime, m);
    let linked = rusqlite::version_number();
    let engine_version: String = db(conn.query_row("SELECT sqlite_version()", [], |r| r.get(0)))?;
    if linked < MIN_SQLITE_VERSION {
        return Err(unsupported(format!(
            "SQLite {engine_version} is older than the required 3.51.3"
        )));
    }
    let mut stmt = db(conn.prepare("PRAGMA compile_options"))?;
    let compile_options: Vec<String> = db(db(stmt.query_map([], |r| r.get(0)))?.collect())?;
    if !compile_options.iter().any(|o| o == "ENABLE_FTS5") {
        return Err(unsupported("SQLite lacks ENABLE_FTS5".into()));
    }
    if compile_options.iter().any(|o| o == "THREADSAFE=0") {
        return Err(unsupported("SQLite built without thread safety".into()));
    }
    let vec_version: String = db(conn.query_row("SELECT vec_version()", [], |r| r.get(0)))?;
    if vec_version != EXPECTED_VEC_VERSION {
        return Err(unsupported(format!(
            "sqlite-vec {vec_version}, expected {EXPECTED_VEC_VERSION}"
        )));
    }
    Ok(RuntimeIdentity {
        engine: "sqlite".into(),
        engine_version,
        compile_options,
        extensions: vec![("sqlite-vec".into(), vec_version)],
    })
}
