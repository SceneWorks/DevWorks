use crate::ports::storage::{
    ProposalRecord as SettingsProposal, StorageError, StoredApproval, StoredFeaturePlan,
    StoredProposal,
};
use crate::protocol::records::ApprovalViewRecord;
use crate::services::sessions::{content, counter, label, rev};
use serde_json::{Value, json};

pub(crate) fn hash(value: &Value) -> String {
    crate::fingerprint::canonical_digest_or_text(value)
}
/// The one binding shared by generic staging and the original owner's admission proof.
pub(crate) fn scope_digest(owner: &Value, authority: Option<&Value>) -> String {
    hash(&json!({"owner":owner,"authority":authority}))
}
pub(crate) fn wire(mut value: Value) -> Result<ApprovalViewRecord, StorageError> {
    if let Some(o) = value.as_object_mut() {
        o.retain(|_, v| !v.is_null());
    }
    serde_json::from_value(value)
        .map_err(|e| StorageError::InvalidRecord(format!("approval projection: {e}")))
}
pub(crate) fn settings(
    p: &SettingsProposal,
    current: i64,
    full: bool,
) -> Result<ApprovalViewRecord, StorageError> {
    let state = format!("{:?}", p.state);
    let scope = scope_wire(&p.scope);
    let binding = json!({"source":"Settings","scope":scope,"baseRevision":p.base_revision,"contentDigest":p.body["contentDigest"]});
    let mut v = json!({"id":p.id,"revision":rev(p.revision),"contentDigest":p.body["contentDigest"],"state":state,"operationKind":"settings.configure","targetId":p.scope.target_id(),"targetRevision":rev(p.base_revision),"summary":"Review settings changes","scope":scope,"stale":matches!(p.state,crate::ports::storage::ProposalState::Pending)&&current!=p.base_revision,"requestedBy":p.author,"createdAtMs":counter(p.created_at_ms),"scopeDigest":hash(&binding)});
    if full {
        v["reviewedSource"] = json!({"kind":"Settings","entries":p.body["entries"]});
        v["reviewedContent"] = binding;
    }
    wire(v)
}
pub(crate) fn plan(
    p: &StoredProposal,
    project: &crate::protocol::ProjectId,
    slot: Option<&StoredFeaturePlan>,
    full: bool,
) -> Result<ApprovalViewRecord, StorageError> {
    let binding = json!({"source":"FeaturePlan","featureId":p.feature_id,"planRevision":p.plan_revision,"contentDigest":p.plan_digest});
    let stale = p.state == "Pending"
        && slot.is_none_or(|s| {
            s.pending_revision != Some(p.plan_revision)
                || s.pending
                    .as_ref()
                    .and_then(|p| p.get("digest"))
                    .and_then(Value::as_str)
                    != Some(p.plan_digest.as_str())
        });
    let mut v = json!({"id":p.id,"revision":rev(p.revision),"contentDigest":p.plan_digest,"state":p.state,"operationKind":"features.plan_approval","targetId":p.feature_id,"targetRevision":rev(p.plan_revision),"summary":content(&p.summary),"scope":{"kind":"Project","projectId":project},"stale":stale,"requestedBy":p.requested_by,"createdAtMs":counter(p.requested_at_ms),"scopeDigest":hash(&binding)});
    if full {
        v["payload"] = json!({"kind":"FeaturePlan","proposalId":p.id});
        // Legacy rows have no captured snapshot: expose that fact, never substitute today's draft.
        v["reviewedContent"] =
            json!({"binding":binding,"snapshotAvailable":p.reviewed_snapshot.is_some()});
        if let Some(s) = &p.reviewed_snapshot {
            v["reviewedSource"] = json!({"kind":"FeaturePlan","plan":s["draft"]});
            if let Some(budget) = s.get("budget") {
                v["reviewedSource"]["budget"] = budget.clone();
            }
        }
    }
    wire(v)
}
pub(crate) fn generic(
    p: &StoredApproval,
    full: bool,
    resume: Option<&crate::ports::storage::StoredApprovalResume>,
) -> Result<ApprovalViewRecord, StorageError> {
    let mut v = json!({"id":p.id,"revision":rev(p.revision),"contentDigest":p.content_digest,"state":p.state,"operationKind":label(&p.operation_kind,"operation"),"targetId":p.target_id,"targetRevision":rev(p.target_revision),"summary":content(&p.summary),"scope":scope_wire(&p.scope),"requestedBy":p.author,"createdAtMs":counter(p.created_at_ms),"scopeDigest":p.scope_digest});
    if let Some(a) = &p.artifact {
        v["artifact"] = a.clone();
    }
    if let Some(r) = &p.rationale {
        v["rationale"] = json!(content(r));
    }
    if full {
        // People and agents read a redacted projection; the stored content stays exact for
        // digest validation and replay.
        v["payload"] = public(&p.payload["content"]);
        v["reviewedContent"] = public(&p.payload["reviewed"]);
        v["operationPreview"] = p
            .payload
            .get("operationPreview")
            .cloned()
            .unwrap_or(Value::Null);
    }
    if let Some(r) = resume {
        v["resumeStatus"] = json!(r.state);
        v["resumedOperationId"] = r
            .result
            .as_ref()
            .and_then(resumed_operation_id)
            .cloned()
            .unwrap_or(Value::Null);
    }
    wire(v)
}
/// A proposal's saved input or reviewed content as it is shown. Only typed secret fields are
/// redacted: the value of a field whose name marks it as a secret, the `value` of a
/// `{"name", "value"}` pair whose name does (an environment variable such as `API_TOKEN`), and
/// credential shapes inside such a `value`. Every other text, including commands and
/// definitions a person approves, is shown exactly: never cut or partly replaced. References
/// (`credentialRef`, ids) are kept.
fn public(content: &Value) -> Value {
    use crate::authority::redact::{REDACTED, Redactor, sensitive_name};
    fn secret(value: &Value) -> Value {
        match value {
            Value::Null | Value::Bool(_) => value.clone(),
            Value::Array(items) => Value::Array(items.iter().map(secret).collect()),
            Value::Object(map) => {
                Value::Object(map.iter().map(|(k, v)| (k.clone(), secret(v))).collect())
            }
            Value::String(s) if s.is_empty() => value.clone(),
            _ => Value::String(REDACTED.into()),
        }
    }
    fn walk(value: &Value, redactor: &Redactor) -> Value {
        match value {
            Value::Array(items) => Value::Array(items.iter().map(|v| walk(v, redactor)).collect()),
            Value::Object(map) => {
                let named_secret = map
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(sensitive_name);
                Value::Object(
                    map.iter()
                        .map(|(k, v)| {
                            let shown = if sensitive_name(k) || (named_secret && k == "value") {
                                secret(v)
                            } else if k == "value" && map.contains_key("name") {
                                // An environment-style value is data, not something that runs:
                                // credential shapes in it are redacted too.
                                match v {
                                    Value::String(s) => Value::String(redactor.redact_text(s)),
                                    other => walk(other, redactor),
                                }
                            } else {
                                walk(v, redactor)
                            };
                            (k.clone(), shown)
                        })
                        .collect(),
                )
            }
            other => other.clone(),
        }
    }
    walk(content, &Redactor::new())
}
pub(crate) fn scope_wire(scope: &crate::ports::storage::ConfigScope) -> Value {
    match scope {
        crate::ports::storage::ConfigScope::Workspace => json!({"kind":"Workspace"}),
        crate::ports::storage::ConfigScope::Project(p)
        | crate::ports::storage::ConfigScope::Repository { project: p, .. } => {
            json!({"kind":"Project","projectId":p})
        }
    }
}

fn resumed_operation_id(result: &Value) -> Option<&Value> {
    result
        .pointer("/receipt/operationId")
        .filter(|v| !v.is_null())
        .or_else(|| {
            result
                .pointer("/receipt/decision/operationId")
                .filter(|v| !v.is_null())
        })
        .or_else(|| result.get("operationId").filter(|v| !v.is_null()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn resumed_operation_projects_the_canonical_recorded_reply() {
        let reply:crate::protocol::results::DomainCommandReply=serde_json::from_value(serde_json::json!({"kind":"Recorded","protocol":"1.0-draft.1","replayed":false,"receipt":{"commandId":"command","committedSequence":"1","acceptedAtMs":"1","requestDigest":"a".repeat(64),"operationId":"operation","decision":{"kind":"Accepted","operationId":"operation"}}})).unwrap();
        let value = serde_json::to_value(reply).unwrap();
        assert_eq!(
            super::resumed_operation_id(&value),
            Some(&serde_json::json!("operation"))
        );
        assert!(
            super::resumed_operation_id(
                &serde_json::json!({"kind":"CommandStatusUnknown","requestKey":"request"})
            )
            .is_none()
        );
    }
    #[test]
    fn private_owner_digest_preserves_large_host_identifiers_without_rounding() {
        let a = super::hash(&serde_json::json!({"fileIdentity":u64::MAX}));
        let b = super::hash(&serde_json::json!({"fileIdentity":u64::MAX-1}));
        assert_ne!(a, b);
    }
}
