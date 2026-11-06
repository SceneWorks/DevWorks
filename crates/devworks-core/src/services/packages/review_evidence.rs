//! Large owner facts are retained once as immutable evidence, with exact JSON-pointer bindings.
use super::*;
use crate::ports::storage::ConfigScope;
use crate::services::approvals::{
    OwnerPreview,
    owner_preview::{artifact_binding, publish_review_evidence},
};
use std::collections::BTreeMap;

fn digest(value: &Value) -> Result<String, Failure> {
    Ok(crate::fingerprint::sha256_hex(
        crate::jcs::canonicalize(value)
            .unwrap_or_else(|_| value.to_string())
            .as_bytes(),
    ))
}
fn pointer(parent: &str, key: &str) -> String {
    format!("{}/{}", parent, key.replace('~', "~0").replace('/', "~1"))
}
fn facts(value: &Value, path: &str, out: &mut BTreeMap<String, String>) -> Result<(), Failure> {
    if !path.is_empty()
        && serde_json::to_vec(value)
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?
            .len()
            >= 4096
    {
        out.insert(digest(value)?, path.into());
    }
    match value {
        Value::Object(o) => {
            for (k, v) in o {
                facts(v, &pointer(path, k), out)?
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                facts(v, &pointer(path, &i.to_string()), out)?
            }
        }
        _ => {}
    }
    Ok(())
}
fn pack(
    value: &mut Value,
    path: &str,
    index: &BTreeMap<String, String>,
    replacements: &mut serde_json::Map<String, Value>,
) -> Result<(), Failure> {
    let hash = digest(value)?;
    if let Some(fact) = index.get(&hash) {
        replacements.insert(path.into(), json!({"pointer":fact,"sha256":hash}));
        *value = Value::Null;
        return Ok(());
    }
    match value {
        Value::Object(o) => {
            for (k, v) in o {
                pack(v, &pointer(path, k), index, replacements)?
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter_mut().enumerate() {
                pack(v, &pointer(path, &i.to_string()), index, replacements)?
            }
        }
        _ => {}
    }
    Ok(())
}
impl PackageService {
    pub(super) fn review_facts(
        &self,
        read: &dyn PackageRegistryRead,
        inspection: &Inspection,
        bundle: &ArtifactRef,
    ) -> Result<Value, Failure> {
        let current = read.package(&inspection.package_id)?;
        let base = current
            .as_ref()
            .and_then(|p| p.active_digest.as_ref())
            .map(|d| read.package_version(&inspection.package_id, d))
            .transpose()?
            .flatten()
            .as_ref()
            .and_then(policy::inspection);
        let summary = package_proposal_summary(inspection, base.as_ref(), bundle);
        Ok(
            json!({"inspection":inspection,"configuration":current.as_ref().map(|p|&p.config),"baseInspection":base,"proposalSummary":summary}),
        )
    }
    pub(super) fn publish_review_facts(
        &self,
        ctx: &InvocationContext,
        request: &crate::admission::DecodedRequest,
        env: &Preparation<'_>,
        value: &Value,
    ) -> Result<ArtifactRef, Failure> {
        publish_review_evidence(
            &self.artifacts,
            &*self.store,
            ctx,
            env,
            request,
            (
                "PackageApprovalEvidence",
                "Complete signed package review facts",
            ),
            value,
        )
    }
    pub(super) fn compact_review(
        &self,
        read: &dyn crate::ports::storage::ArtifactRead,
        mut preview: OwnerPreview,
        reference: &ArtifactRef,
        body: &Value,
    ) -> Result<OwnerPreview, Failure> {
        let mut index = BTreeMap::new();
        facts(body, "", &mut index)?;
        let mut combined = json!({"binding":preview.binding,"reviewed":preview.reviewed});
        let mut fragments = serde_json::Map::new();
        pack(&mut combined, "", &index, &mut fragments)?;
        let witness = artifact_binding(read, reference)?;
        let document = crate::jcs::canonicalize(body).unwrap_or_else(|_| body.to_string());
        if reference.media_type != "application/json"
            || reference.byte_length.get() != document.len() as i64
            || crate::fingerprint::sha256_hex(document.as_bytes()) != reference.sha256.0
        {
            return Err(invalid("package_review_evidence").into());
        }
        preview.binding = json!({"packageReview":{"artifact":witness,"fragments":fragments,"document":document},"owner":combined["binding"]});
        preview.reviewed = json!({"artifact":reference,"metadata":combined["reviewed"],"factsDigest":reference.sha256});
        Ok(preview)
    }
    pub(super) fn expand_review(
        &self,
        tx: &mut dyn WriteTransaction,
        _ctx: &InvocationContext,
        preview: &OwnerPreview,
    ) -> Result<OwnerPreview, Failure> {
        self.expand_review_mode(tx.artifacts(), preview, true)
    }
    pub(super) fn expand_review_mode(
        &self,
        read: &dyn crate::ports::storage::ArtifactRead,
        preview: &OwnerPreview,
        current: bool,
    ) -> Result<OwnerPreview, Failure> {
        let Some(envelope) = preview.binding.get("packageReview") else {
            return Ok(preview.clone());
        };
        let reference: ArtifactRef =
            serde_json::from_value(envelope["artifact"]["reference"].clone())
                .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        if current && artifact_binding(read, &reference)? != envelope["artifact"] {
            return Err(Refusal::new(
                DomainErrorCode::SourceChanged,
                "package_review_evidence",
                Some(reference.version_id),
            )
            .into());
        }
        let document = envelope["document"]
            .as_str()
            .ok_or_else(|| invalid("package_review_evidence"))?;
        if reference.media_type != "application/json"
            || reference.byte_length.get() != document.len() as i64
            || crate::fingerprint::sha256_hex(document.as_bytes()) != reference.sha256.0
        {
            return Err(invalid("package_review_evidence").into());
        }
        let body: Value = serde_json::from_str(document)
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        let mut combined =
            json!({"binding":preview.binding["owner"],"reviewed":preview.reviewed["metadata"]});
        for (path, fact) in envelope["fragments"]
            .as_object()
            .ok_or_else(|| invalid("package_review_evidence"))?
        {
            let value = body
                .pointer(
                    fact["pointer"]
                        .as_str()
                        .ok_or_else(|| invalid("package_review_evidence"))?,
                )
                .ok_or_else(|| invalid("package_review_evidence"))?;
            if digest(value)? != fact["sha256"] {
                return Err(invalid("package_review_evidence").into());
            }
            *combined
                .pointer_mut(path)
                .ok_or_else(|| invalid("package_review_evidence"))? = value.clone();
        }
        let mut result = preview.clone();
        result.binding = combined["binding"].clone();
        result.reviewed = combined["reviewed"].clone();
        Ok(result)
    }
    pub(super) fn expand_review_binding(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        binding: &Value,
    ) -> Result<Value, Failure> {
        if binding.get("packageReview").is_none() {
            return Ok(binding.clone());
        }
        let preview = OwnerPreview {
            scope: ConfigScope::Workspace,
            target_id: Id::new("evidence").expect("id"),
            target_revision: 1,
            summary: String::new(),
            reviewed: json!({"metadata":null}),
            binding: binding.clone(),
        };
        // Actual readmission only needs the owner binding. Ignore reviewed-only fragments.
        let mut preview = preview;
        if let Some(fragments) = preview.binding["packageReview"]["fragments"].as_object_mut() {
            fragments.retain(|path, _| path == "/binding" || path.starts_with("/binding/"));
        }
        Ok(self.expand_review(tx, ctx, &preview)?.binding)
    }
}
pub(super) fn package_proposal_summary(
    candidate: &Inspection,
    base: Option<&Inspection>,
    bundle: &ArtifactRef,
) -> Value {
    json!({"requestedAccess":candidate.access,"dependencies":candidate.dependencies,"runtimes":candidate.runtimes,"contributions":candidate.contribution_ids(),"changes":view::changes(base,candidate),"signed":candidate.signer_key.is_some(),"source":bundle,"verificationEvidence":[]})
}
