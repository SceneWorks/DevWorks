//! Database configuration drafts are independently owned Human authoring. Project scope
//! is the invocation's view, not ownership of the connection name/endpoint/credentials.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    draft: Option<crate::databases_screen::Draft>,
    #[serde(rename = "batchActive")]
    active: bool,
    #[serde(rename = "batchScanning")]
    scanning: bool,
    remaining: Vec<(String, i64)>,
    #[serde(rename = "batchScope")]
    scope: Option<String>,
    submitted: usize,
    failures: usize,
    tab: usize,
}
pub(super) fn partition(
    memory: &mut Memory,
    scope: &ProjectPurgeClientScope,
) -> Result<bool, MemoryPurgeFault> {
    if memory.pending.databases.is_null() {
        return Ok(false);
    }
    let mut pending: Pending = super::scoped::checked(&memory.pending.databases)?;
    let p = scope.project_id.0.as_str();
    let active = pending.scope.as_deref() == Some(p) && (pending.active || pending.scanning);
    if pending.scope.as_deref() == Some(p) {
        pending.active = false;
        pending.scanning = false;
        pending.remaining.clear();
        pending.scope = None;
        pending.submitted = 0;
        pending.failures = 0;
    }
    if let Some(draft) = &mut pending.draft {
        if draft.scope.as_deref() == Some(p) {
            draft.scope = None;
        }
        if let Some(access) = draft.config.get_mut("projectAccess") {
            let entries = access
                .as_array_mut()
                .ok_or(MemoryPurgeFault::UnknownShape)?;
            for value in entries.iter() {
                let _: devworks_core::protocol::records::SchemaProjectAccess =
                    super::scoped::checked(value)?;
            }
            entries.retain(|a| a["projectId"].as_str() != Some(p));
        }
    }
    memory.pending.databases =
        serde_json::to_value(pending).map_err(|_| MemoryPurgeFault::UnknownShape)?;
    Ok(active)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn database_projection_keeps_independent_unsaved_configuration_and_other_access() {
        let mut memory = Memory::default();
        let mut screen = crate::databases_screen::DatabasesScreen {
            draft: Some(crate::databases_screen::Draft::new(Some("p".into()))),
            ..Default::default()
        };
        let draft = screen.draft.as_mut().unwrap();
        draft.config["name"] = json!("independent unsaved connection");
        draft.config["endpoint"] = json!("independent.sqlite");
        draft.config["projectAccess"].as_array_mut().unwrap().push(json!({"projectId":"other","metadataEnabled":true,"dataReadEnabled":false,"dataWriteEnabled":false}));
        memory.pending.databases = screen.pending_memory();
        assert!(!partition(&mut memory, &super::super::tests::scope()).unwrap());
        let kept = &memory.pending.databases["draft"];
        assert_eq!(kept["config"]["name"], "independent unsaved connection");
        assert_eq!(kept["config"]["endpoint"], "independent.sqlite");
        assert_eq!(kept["scope"], Value::Null);
        assert_eq!(kept["config"]["projectAccess"].as_array().unwrap().len(), 1);
        assert_eq!(kept["config"]["projectAccess"][0]["projectId"], "other");
    }
}
