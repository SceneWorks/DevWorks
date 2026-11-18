//! Test support
mod unmodelled_generated;
pub struct Unmodelled;
use devworks_core::ports::storage::{StorageError, Unavailable};
pub fn unmodelled<T>() -> Result<T, StorageError> {
    Err(StorageError::unavailable(
        Unavailable::Io,
        "not modelled by the in-memory substitute",
    ))
}
