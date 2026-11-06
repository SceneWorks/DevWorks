//! Compact positions bound independently from the reusable retention lease.
use super::read::Position;
use crate::ports::storage::StorageError;
use crate::protocol::Id;
use crate::services::cursor::CursorBinding;
use crate::services::sessions::Failure;

pub(super) struct Cursors {
    binding: CursorBinding,
    pin: Option<Id>,
}

impl Cursors {
    pub fn new(binding: CursorBinding, pin: Option<Id>) -> Self {
        Self { binding, pin }
    }
    pub fn encode(&self, position: &Position) -> Result<String, Failure> {
        let position = serde_json::to_string(position).map_err(|_| StorageError::CursorExpired)?;
        let body = self.binding.encode(&position);
        let cursor = match &self.pin {
            Some(pin) => format!("g1.{}.{body}", pin.as_str()),
            None => format!("m1.{body}"),
        };
        if cursor.len() > 4096 {
            return Err(StorageError::CursorExpired.into());
        }
        Ok(cursor)
    }
    pub fn decode(&self, cursor: &str) -> Result<Position, Failure> {
        let body = match &self.pin {
            Some(pin) => cursor.strip_prefix(&format!("g1.{}.", pin.as_str())),
            None => cursor.strip_prefix("m1."),
        }
        .ok_or(StorageError::CursorExpired)?;
        serde_json::from_str(&self.binding.decode(body)?)
            .map_err(|_| StorageError::CursorExpired.into())
    }
    /// The untrusted header only locates a lease. Ownership/current authority and the full
    /// binding are checked before any result can be published; it never selects another view.
    pub fn pin(cursor: &str) -> Result<Option<Id>, Failure> {
        if cursor.starts_with("m1.") {
            return Ok(None);
        }
        let mut parts = cursor.splitn(3, '.');
        if parts.next() != Some("g1") {
            return Err(StorageError::CursorExpired.into());
        }
        let pin = parts.next().ok_or(StorageError::CursorExpired)?;
        if parts.next().is_none() {
            return Err(StorageError::CursorExpired.into());
        }
        Ok(Some(
            Id::new(pin.to_owned()).map_err(|_| StorageError::CursorExpired)?,
        ))
    }
}
