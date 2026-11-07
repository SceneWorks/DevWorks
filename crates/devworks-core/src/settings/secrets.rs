//! Settings credential custody at the configuration writer boundary.
use crate::ports::storage::{StorageError, WriteTransaction};
use crate::protocol::Id;
use serde_json::Value;
use std::collections::BTreeSet;

/// Every configuration introduction (save, proposal, reconcile, approval and rollback)
/// shares the same retirement fence. Reference registration commits with the document.
pub(crate) fn retain(tx: &mut dyn WriteTransaction, body: &Value) -> Result<(), StorageError> {
    fn collect(value: &Value, handles: &mut BTreeSet<String>) -> Result<(), StorageError> {
        match value {
            Value::Object(fields) => {
                if let Some(Value::String(reference)) = fields.get("secretRef") {
                    if !super::validate::is_secret_ref(reference) {
                        return Err(StorageError::ScopeDenied);
                    }
                    handles.insert(reference.clone());
                }
                for child in fields.values() {
                    collect(child, handles)?;
                }
            }
            Value::Array(values) => {
                for child in values {
                    collect(child, handles)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut handles = BTreeSet::new();
    collect(body, &mut handles)?;
    for handle in handles {
        let handle = Id::new(handle).map_err(|_| StorageError::ScopeDenied)?;
        tx.configuration().retain_settings_secret(&handle)?;
    }
    Ok(())
}
