use devworks_core::ports::storage::{StorageError, Unavailable};

/// Stands in for every repository a test double does not model. Each required method of each
/// storage repository trait answers [`unmodelled`] (see `unmodelled_generated.rs`).
pub struct Unmodelled;

/// The answer of a method nothing models: storage unavailable, so a test that reaches it fails
/// loudly instead of seeing invented data.
pub fn unmodelled<T>() -> Result<T, StorageError> {
    Err(StorageError::unavailable(
        Unavailable::Io,
        "not modelled by the in-memory substitute",
    ))
}
