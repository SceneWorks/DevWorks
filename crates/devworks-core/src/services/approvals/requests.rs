//! The closed saved-command surface. A registry entry alone never qualifies an approval.
use crate::admission::{self, DecodedRequest, Selection};
use crate::registry::{self, Envelope, Mode};
use serde_json::{Value, json};

/// Jobs keep their proposer as the actor. Their owner also needs a reviewed, exact human
/// consent binding; an ordinary operation grant is insufficient.
pub const REVIEWED_JOB_OPERATIONS: [&str; 4] =
    ["jobs.create", "jobs.edit", "jobs.enable", "jobs.run_now"];

pub fn reviewed_job(operation: &str) -> bool {
    REVIEWED_JOB_OPERATIONS.contains(&operation)
}

/// Every candidate still needs its registered owner and current safety/target preview.
pub fn candidate(operation: &str) -> bool {
    reviewed_job(operation)
        || crate::authority::policy::APPROVAL_REQUIRED.contains(&operation)
        || crate::authority::policy::NATIVE_EXECUTION.contains(&operation)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestFailure {
    Unsupported,
    Invalid,
}

/// Decode through the same admission as dispatch. No actor, grant, decision or envelope
/// identity comes from the caller's selected argument body.
pub fn decode(
    operation: &str,
    schema_revision: u32,
    input: &Value,
    registered: bool,
) -> Result<DecodedRequest, RequestFailure> {
    registry::find(operation).ok_or(RequestFailure::Unsupported)?;
    if !registered || !candidate(operation) {
        return Err(RequestFailure::Unsupported);
    }
    if !input.is_object() || input.get("kind").is_some() {
        return Err(RequestFailure::Invalid);
    }
    let frame = frame(operation, schema_revision, input, "approval-preview")?;
    admission::decode(
        Selection {
            operation,
            mode: Mode::Command,
        },
        frame.to_string().as_bytes(),
    )
    .map_err(|_| RequestFailure::Invalid)
}

/// Construct only the operation's registered envelope; actual admission remains authoritative.
pub fn frame(
    operation: &str,
    schema_revision: u32,
    input: &Value,
    key: &str,
) -> Result<Value, RequestFailure> {
    let descriptor = registry::find(operation).ok_or(RequestFailure::Unsupported)?;
    let mut frame = match descriptor.envelope {
        Envelope::Domain => {
            let mut body = input.clone();
            body["kind"] = json!(operation);
            json!({"mode":"Command","body":body})
        }
        Envelope::Specialized { .. } => input.clone(),
    };
    if !frame.is_object() {
        return Err(RequestFailure::Invalid);
    }
    for field in [
        "protocol",
        "requestKey",
        "schemaRevision",
        "actor",
        "principal",
        "authoritySources",
        "human",
    ] {
        if input.get(field).is_some() {
            return Err(RequestFailure::Invalid);
        }
    }
    frame["protocol"] = json!("1.0-draft.1");
    frame["requestKey"] = json!(key);
    frame["schemaRevision"] = json!(schema_revision);
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_input_is_the_actual_registered_admission_and_never_caller_authority() {
        let args = json!({"jobId":"job-1", "expectedRevision":"1"});
        let decoded = decode("jobs.enable", 2, &args, true).unwrap();
        assert_eq!(decoded.descriptor.name, "jobs.enable");
        assert_eq!(decoded.normalized, args);
        let frame = json!({"mode":"Command", "protocol":"1.0-draft.1", "requestKey":"another-key",
            "schemaRevision":2, "body":{"kind":"jobs.enable","jobId":"job-1","expectedRevision":"1"}});
        let direct = admission::decode(
            Selection {
                operation: "jobs.enable",
                mode: Mode::Command,
            },
            frame.to_string().as_bytes(),
        )
        .unwrap();
        assert_eq!(decoded.fingerprint, direct.fingerprint);
        for bad in [
            json!({"jobId":"job-1","expectedRevision":"1","human":true}),
            json!({"jobId":"job-1","expectedRevision":"1","actor":"user"}),
            json!({"jobId":"job-1","expectedRevision":"1","kind":"jobs.enable"}),
        ] {
            assert!(matches!(
                decode("jobs.enable", 2, &bad, true),
                Err(RequestFailure::Invalid)
            ));
        }
        assert!(matches!(
            decode("jobs.enable", 2, &args, false),
            Err(RequestFailure::Unsupported)
        ));
        assert!(matches!(
            decode("process.execute", 1, &json!({}), false),
            Err(RequestFailure::Unsupported)
        ));
        for operation in [
            "runs.continue",
            "approvals.record_human_decision",
            "repositories.confirm_delete",
        ] {
            assert!(matches!(
                decode(operation, 1, &json!({}), true),
                Err(RequestFailure::Unsupported)
            ));
        }
    }
}
