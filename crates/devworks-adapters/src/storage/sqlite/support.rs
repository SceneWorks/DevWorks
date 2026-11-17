//! Helpers shared by every SQLite repository: error constructors, stored-value decoding, the
//! page-limit rule and the one atomicity mechanism. Repository files must not declare their own
//! copies of these.
//!
//! # Error mapping
//!
//! - [`invalid`] is for a request the caller got wrong (`StorageError::InvalidRecord`).
//! - [`corrupt`] is for a stored row that cannot be decoded (`Unavailable::Corrupt`). A bad
//!   stored identifier, JSON document or non-positive revision is never `InvalidRecord`, which
//!   would blame the caller for damage in the store.
//! - [`conflict`] builds a revision conflict. A stored revision that is not positive is a
//!   corrupt-store error, not a panic and not a silently clamped number.
//!
//! # Atomicity rule
//!
//! The writer thread runs every write job in one `BEGIN IMMEDIATE` transaction. If the job
//! returns an error or panics, the whole transaction rolls back (see `writer.rs`). The job is
//! therefore the unit of atomicity, and a repository method does not need to protect its own
//! effects against its own failure.
//!
//! The exception is a caller that catches a repository error and keeps writing in the same job.
//! A method that writes several rows and can fail after its first write must then undo itself.
//! Such a method wraps its body in [`atomic`], which is the only savepoint mechanism the
//! repositories use. A method that validates before it writes, or writes a single statement,
//! does not wrap. `atomic` nests, so a wrapped method may call another wrapped method.
use super::conn::db;
use devworks_core::ports::storage::{MAX_PAGE, StorageError, Unavailable};
use devworks_core::protocol::{Id, Revision, RevisionDimension};
use rusqlite::Connection;

/// A request the caller got wrong.
pub(super) fn invalid(message: impl Into<String>) -> StorageError {
    StorageError::InvalidRecord(message.into())
}

/// A stored row that cannot be decoded.
pub(super) fn corrupt(message: impl Into<String>) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, message)
}

/// Decodes a stored identifier; an invalid one is a corrupt store.
pub(super) fn id_of(value: String) -> Result<Id, StorageError> {
    Id::new(value).map_err(|e| corrupt(format!("stored identifier: {e}")))
}

/// Decodes a stored revision; zero or negative is a corrupt store.
pub(super) fn revision(value: i64) -> Result<Revision, StorageError> {
    Revision::new(value).map_err(|_| corrupt("stored revision is not positive"))
}

/// Checks a page limit (1..=`MAX_PAGE`) and returns it as a SQL `LIMIT` value.
pub(super) fn page_limit(limit: u16) -> Result<i64, StorageError> {
    if limit == 0 || limit > MAX_PAGE {
        return Err(invalid(format!("page limit must be 1..={MAX_PAGE}")));
    }
    Ok(i64::from(limit))
}

/// A revision conflict. `expected` comes from the caller and is shown at least as 1;
/// `current` is read from the store and must be positive. A target name that is not a valid
/// identifier is reported under its `kind`.
pub(super) fn conflict(
    dimension: RevisionDimension,
    kind: &'static str,
    id: &str,
    expected: i64,
    current: i64,
) -> StorageError {
    let current = match revision(current) {
        Ok(current) => current,
        Err(error) => return error,
    };
    let target = Id::new(id)
        .or_else(|_| Id::new(kind))
        .unwrap_or_else(|_| Id::new("unknown").expect("literal identifier"));
    let expected = Revision::new(expected.max(1)).expect("positive");
    StorageError::conflict(dimension, kind, target, expected, current)
}

/// Runs `f` inside a savepoint: its writes stay if it returns `Ok` and are undone if it returns
/// `Err`. See the module documentation for when a repository method uses this.
pub(super) fn atomic<T>(
    conn: &Connection,
    f: impl FnOnce() -> Result<T, StorageError>,
) -> Result<T, StorageError> {
    db(conn.execute_batch("SAVEPOINT dw_atomic"))?;
    match f() {
        Ok(value) => {
            db(conn.execute_batch("RELEASE dw_atomic"))?;
            Ok(value)
        }
        Err(error) => {
            db(conn.execute_batch("ROLLBACK TO dw_atomic; RELEASE dw_atomic"))?;
            Err(error)
        }
    }
}
