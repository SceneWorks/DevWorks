use crate::ports::storage::{
    ProposalRecord as SettingsProposal, StorageError, StoredApproval, StoredFeaturePlan,
    StoredProposal,
};
use crate::protocol::records::ApprovalViewRecord;
use crate::services::sessions::{content, counter, label, rev};
use serde_json::{Value, json};

pub(crate) fn hash(value: &Value) -> String {
    crate::fingerprint::sha256_hex(
        crate::jcs::canonicalize(value)
            .unwrap_or_else(|_| value.to_string())
            .as_bytes(),
    )
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
        v["payload"] = p.payload["content"].clone();
        v["reviewedContent"] = p.payload["reviewed"].clone();
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
            .and_then(|v| v.get("operationId"))
            .cloned()
            .unwrap_or(Value::Null);
    }
    wire(v)
}
pub(crate) fn scope_wire(scope: &crate::ports::storage::ConfigScope) -> Value {
    match scope {
        crate::ports::storage::ConfigScope::Workspace => json!({"kind":"Workspace"}),
        crate::ports::storage::ConfigScope::Project(p) => json!({"kind":"Project","projectId":p}),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn private_owner_digest_preserves_large_host_identifiers_without_rounding() {
        let a = super::hash(&serde_json::json!({"fileIdentity":u64::MAX}));
        let b = super::hash(&serde_json::json!({"fileIdentity":u64::MAX-1}));
        assert_ne!(a, b);
    }
}
