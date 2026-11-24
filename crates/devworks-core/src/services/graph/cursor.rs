//! Compact positions bound independently from the reusable retention lease.
use super::read::Position;
use crate::ports::storage::StorageError;
use crate::protocol::Id;
use crate::services::cursor::CursorBinding;
use crate::services::kernel::Failure;

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
    pub fn pin(cursor: &str) -> Result<Option<Id>, Failure> {
        crate::kernel::graph_request::cursor_pin(cursor)
    }
}
