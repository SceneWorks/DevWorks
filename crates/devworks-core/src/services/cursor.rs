//! Versioned opaque cursors for session and history pages (shared-operation-contracts.md
//! "Queries, cursors and event delivery").
//!
//! A cursor is `v1.<hex position>.<tag>`, where the tag is a digest of the binding and the
//! position. The binding names the operation and its query/filter, the invoking principal and
//! project (visibility), the store and restore epoch, and the scope revision the page was read
//! under. A cursor presented under any other binding — another query, principal, project, store,
//! restore epoch or a changed scope revision — or one that was altered is `CursorExpired`, so a
//! caller re-snapshots instead of silently continuing a different list.
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::ports::storage::StorageError;

const VERSION: &str = "v1";

/// What a cursor is bound to.
#[derive(Clone, Debug)]
pub struct CursorBinding(String);
impl CursorBinding {
    /// Binds `operation` with its query/filter `parts` for `ctx` at `scope_revision`.
    pub fn new(
        ctx: &InvocationContext,
        operation: &str,
        parts: &[&str],
        scope_revision: i64,
    ) -> Self {
        let mut basis = format!(
            "{operation}|{}|{}|{}|{}|{scope_revision}",
            ctx.principal().0.as_str(),
            ctx.project_id().map_or("", |p| p.0.as_str()),
            ctx.store_id().0.as_str(),
            ctx.restore_epoch().as_str(),
        );
        for p in parts {
            basis.push('|');
            basis.push_str(p);
        }
        Self(sha256_hex(basis.as_bytes()))
    }
    fn tag(&self, position: &str) -> String {
        sha256_hex(format!("{}|{position}", self.0).as_bytes())[..24].to_owned()
    }
    /// The cursor resuming after `position`.
    pub fn encode(&self, position: &str) -> String {
        let hex: String = position.bytes().map(|b| format!("{b:02x}")).collect();
        format!("{VERSION}.{hex}.{}", self.tag(position))
    }
    /// The position of a cursor issued under this binding.
    pub fn decode(&self, cursor: &str) -> Result<String, StorageError> {
        let mut parts = cursor.split('.');
        let (Some(VERSION), Some(hex), Some(tag), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(StorageError::CursorExpired);
        };
        if !hex.is_ascii() || hex.len() % 2 != 0 {
            return Err(StorageError::CursorExpired);
        }
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
            .collect::<Result<Vec<u8>, _>>()
            .map_err(|_| StorageError::CursorExpired)?;
        let position = String::from_utf8(bytes).map_err(|_| StorageError::CursorExpired)?;
        if self.tag(&position) != tag {
            return Err(StorageError::CursorExpired);
        }
        Ok(position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_ascii_hex_is_expired_not_a_panic() {
        let binding = CursorBinding(String::new());
        for cursor in ["v1.aéa.x", "v1.éa.x", "v1.é.x", "v1.aaé.x"] {
            assert!(matches!(
                binding.decode(cursor),
                Err(StorageError::CursorExpired)
            ));
        }
    }
}
