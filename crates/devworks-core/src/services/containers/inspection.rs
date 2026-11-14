//! Parse engine observations and fence lifecycle actions to an inspected immutable object ID.
use super::backend::{Backend, ownership_matches};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservedPhase {
    Running,
    Stopped,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inspected {
    pub native_id: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub phase: ObservedPhase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectionError {
    InvalidResponse,
    OwnershipMismatch,
    NoImmutableTarget,
}

pub fn parse(backend: Backend, bytes: &[u8]) -> Result<Inspected, InspectionError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| InspectionError::InvalidResponse)?;
    let row = value
        .as_array()
        .and_then(|rows| (rows.len() == 1).then(|| &rows[0]))
        .ok_or(InspectionError::InvalidResponse)?;
    let label_object = match backend {
        Backend::Docker | Backend::Podman => row.pointer("/Config/Labels"),
        Backend::Apple => row.pointer("/configuration/labels"),
    }
    .and_then(serde_json::Value::as_object)
    .ok_or(InspectionError::InvalidResponse)?;
    let labels = label_object
        .iter()
        .map(|(key, value)| {
            value
                .as_str()
                .map(|value| (key.clone(), value.to_owned()))
                .ok_or(InspectionError::InvalidResponse)
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let native_id = match backend {
        Backend::Docker | Backend::Podman => row.get("Id").and_then(serde_json::Value::as_str),
        Backend::Apple => None,
    }
    .filter(|id| crate::fingerprint::is_lower_hex(id, 64))
    .map(str::to_owned);
    let phase = match backend {
        Backend::Docker | Backend::Podman => match row
            .pointer("/State/Running")
            .and_then(serde_json::Value::as_bool)
        {
            Some(true) => ObservedPhase::Running,
            Some(false) => ObservedPhase::Stopped,
            None => ObservedPhase::Unknown,
        },
        Backend::Apple => match row.get("status").and_then(serde_json::Value::as_str) {
            Some("running") => ObservedPhase::Running,
            Some("stopped" | "exited") => ObservedPhase::Stopped,
            _ => ObservedPhase::Unknown,
        },
    };
    Ok(Inspected {
        native_id,
        labels,
        phase,
    })
}

/// Never pass a public name to a destructive lifecycle verb after inspection. Docker and Podman
/// expose immutable IDs; Apple's current CLI does not expose a per-incarnation target that can
/// survive a remove/recreate race, so its stop remains unsupported here.
pub fn verified_lifecycle_target(
    backend: Backend,
    inspected: &Inspected,
    create_operation: &str,
    project: &str,
    owner_kind: &str,
) -> Result<String, InspectionError> {
    if !ownership_matches(&inspected.labels, create_operation, project, owner_kind) {
        return Err(InspectionError::OwnershipMismatch);
    }
    if backend == Backend::Apple {
        return Err(InspectionError::NoImmutableTarget);
    }
    inspected
        .native_id
        .clone()
        .ok_or(InspectionError::NoImmutableTarget)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_labels_and_immutable_id_are_required_before_stop() {
        let id = "a".repeat(64);
        let body = serde_json::json!([{
            "Id": id,
            "Config": {"Labels": {
                "devworks.create_operation":"op-a",
                "devworks.project":"project-a",
                "devworks.owner_kind":"Agent"
            }},
            "State":{"Running":true}
        }]);
        let inspected = parse(Backend::Docker, body.to_string().as_bytes()).unwrap();
        assert_eq!(inspected.phase, ObservedPhase::Running);
        assert_eq!(
            verified_lifecycle_target(Backend::Docker, &inspected, "op-a", "project-a", "Agent"),
            Ok("a".repeat(64))
        );
        assert_eq!(
            verified_lifecycle_target(Backend::Docker, &inspected, "op-b", "project-a", "Agent"),
            Err(InspectionError::OwnershipMismatch)
        );
        assert_eq!(
            verified_lifecycle_target(Backend::Apple, &inspected, "op-a", "project-a", "Agent"),
            Err(InspectionError::NoImmutableTarget)
        );
    }
}
