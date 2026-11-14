//! Exact typed request affinity for local pending work and stale query fencing.
//! Prose, labels, navigation and filesystem strings never establish ownership.
use super::*;
use devworks_core::ports::storage::ProjectRecordKind as K;
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
        let fields: &[(&str, K)] = match family {
            "approvals" => &[
                ("approvalId", K::DecisionApprovals),
                ("proposalId", K::DecisionApprovals),
            ],
            "notifications" => &[
                ("notificationId", K::Notifications),
                ("notificationIds", K::Notifications),
            ],
            "jobs" => &[
                ("scheduleId", K::Schedules),
                ("jobId", K::Schedules),
                ("runId", K::JobRuns),
            ],
            "browser" => &[("sessionId", K::BrowserSessions)],
            "knowledge" => &[
                ("claimId", K::KnowledgeClaims),
                ("entityId", K::KnowledgeEntities),
                ("challengeId", K::KnowledgeChallenges),
                ("evidenceId", K::KnowledgeEvidence),
                ("relationId", K::KnowledgeRelations),
            ],
            "conventions" => &[
                ("conventionId", K::ConventionHeads),
                ("resourceId", K::ConventionResourceHeads),
            ],
            "resources" => &[("resourceId", K::Resources), ("leaseId", K::ResourceLeases)],
            "artifacts" | "documents" => &[
                ("artifactId", K::Artifacts),
                ("versionId", K::ArtifactVersions),
            ],
            "process" => &[("processId", K::OwnedProcesses)],
            "indexing" => &[("overlayId", K::IndexOverlays), ("runId", K::IndexRuns)],
            "next_work" => &[("claimId", K::AllocationClaims)],
            "settings" => &[("proposalId", K::ConfigurationProposals)],
            _ => &[],
        };
        if fields.iter().any(|(field, table)| {
            scope
                .record_ids
                .get(table)
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
