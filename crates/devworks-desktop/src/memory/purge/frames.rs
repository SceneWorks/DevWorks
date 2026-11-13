//! Exact typed request affinity for local pending work and stale query fencing.
//! Prose, labels, navigation and filesystem strings never establish ownership.
use super::*;
use devworks_core::{
    admission::{Selection, decode},
    operations::{OperationInput, projects},
    registry::Mode,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Affinity {
    Owned,
    Independent,
    Control,
}
fn named(value: &Value, member: &str, ids: &[devworks_core::protocol::Id]) -> bool {
    let has = |v: &Value| {
        v.as_str()
            .is_some_and(|s| ids.iter().any(|id| id.as_str() == s))
    };
    value
        .get(member)
        .is_some_and(|v| has(v) || v.as_array().is_some_and(|a| a.iter().any(has)))
}
pub fn affinity(
    operation: &str,
    mode: Mode,
    frame: &[u8],
    scope: &ProjectPurgeClientScope,
) -> Result<Affinity, MemoryPurgeFault> {
    let request =
        decode(Selection { operation, mode }, frame).map_err(|_| MemoryPurgeFault::UnknownShape)?;
    if matches!(
        &request.input,
        OperationInput::Projects(
            projects::Input::PreparePurge(_)
                | projects::Input::Purge(_)
                | projects::Input::PurgeManifest(_)
                | projects::Input::PurgeStatus(_)
        )
    ) {
        return Ok(Affinity::Control);
    }
    let body = &request.normalized;
    let containers = [
        Some(body),
        body.get("command"),
        body.get("query"),
        body.get("target"),
        body.get("subject"),
        body.get("conversation"),
        body.get("owner"),
    ];
    for value in containers.into_iter().flatten() {
        if devworks_core::admission::project_targets(value).contains(&scope.project_id)
            || devworks_core::admission::work_targets(value)
                .iter()
                .any(|id| scope.work_ids.contains(id))
            || named(value, "operationId", &scope.operation_ids)
        {
            return Ok(Affinity::Owned);
        }
        let family = request.descriptor.family;
        if matches!(family, "sessions" | "history" | "ui" | "router")
            && (named(value, "sessionId", &scope.session_ids)
                || named(value, "conversationId", &scope.conversation_ids))
        {
            return Ok(Affinity::Owned);
        }
        let fields: &[(&str, &str)] = match family {
            "approvals" => &[
                ("approvalId", "decision_approvals"),
                ("proposalId", "decision_approvals"),
            ],
            "notifications" => &[
                ("notificationId", "notifications"),
                ("notificationIds", "notifications"),
            ],
            "jobs" => &[
                ("scheduleId", "schedules"),
                ("jobId", "schedules"),
                ("runId", "job_runs"),
            ],
            "browser" => &[("sessionId", "browser_sessions")],
            "knowledge" => &[
                ("claimId", "knowledge_claims"),
                ("entityId", "knowledge_entities"),
                ("challengeId", "knowledge_challenges"),
                ("evidenceId", "knowledge_evidence"),
                ("relationId", "knowledge_relations"),
            ],
            "conventions" => &[
                ("conventionId", "convention_heads"),
                ("resourceId", "convention_resource_heads"),
            ],
            "resources" => &[("resourceId", "resources"), ("leaseId", "resource_leases")],
            "artifacts" | "documents" => &[
                ("artifactId", "artifacts"),
                ("versionId", "artifact_versions"),
            ],
            "process" => &[("processId", "owned_processes")],
            "indexing" => &[("overlayId", "index_overlays"), ("runId", "index_runs")],
            "next_work" => &[("claimId", "allocation_claims")],
            "settings" => &[("proposalId", "configuration_proposals")],
            _ => &[],
        };
        if fields.iter().any(|(field, table)| {
            scope
                .record_ids
                .get(*table)
                .is_some_and(|ids| named(value, field, ids))
        }) {
            return Ok(Affinity::Owned);
        }
    }
    Ok(Affinity::Independent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn exact_frame_scope_ignores_text_and_preserves_another_projects_original_request() {
        let scope = super::super::tests::scope();
        let frame = |project: &str, description: &str| {
            serde_json::to_vec(&json!({"mode":"Command","protocol":"1.0-draft.1","requestKey":"original","body":{"kind":"projects.edit","projectId":project,"expectedRevision":"1","changes":{"description":description}}})).unwrap()
        };
        assert_eq!(
            affinity(
                "projects.edit",
                Mode::Command,
                &frame("p", "independent wording"),
                &scope
            )
            .unwrap(),
            Affinity::Owned
        );
        assert_eq!(
            affinity(
                "projects.edit",
                Mode::Command,
                &frame("other", "p and p-session private text"),
                &scope
            )
            .unwrap(),
            Affinity::Independent
        );
        let bad = json!({"mode":"Command","protocol":"1.0-draft.1","requestKey":"original","body":{"kind":"projects.edit","projectId":"p","expectedRevision":"1","unknownPrivateField":"data"}});
        assert!(
            affinity(
                "projects.edit",
                Mode::Command,
                &serde_json::to_vec(&bad).unwrap(),
                &scope
            )
            .is_err()
        );
    }
}
