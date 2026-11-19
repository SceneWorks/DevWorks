//! Sample frames of the builders for specialized envelopes (transcript, messages, artifacts,
//! indexing commands, run controls).
use super::{Silent, from};
use crate::ops;
use devworks_core::protocol::provider::ArtifactRef;
use serde_json::json;

pub fn frames() -> Vec<(&'static str, Vec<u8>)> {
    let b = &Silent;
    let artifact: ArtifactRef = from(json!({"artifactId":"artifact-a","versionId":"version-a",
        "sha256":"a".repeat(64),"byteLength":"12","mediaType":"text/plain"}));
    let digest = "c".repeat(64);
    vec![
        ("transcript_first", ops::transcript(b, "session-a", None)),
        (
            "transcript_after",
            ops::transcript(b, "session-a", Some("cursor-1")),
        ),
        (
            "append_message_text",
            ops::append_message(b, "session-a", 3, "hello", &[]),
        ),
        (
            "append_message_attachment",
            ops::append_message(b, "session-a", 3, "  ", std::slice::from_ref(&artifact)),
        ),
        (
            "append_message_both",
            ops::append_message(b, "session-a", 3, "hello", std::slice::from_ref(&artifact)),
        ),
        (
            "save_workspace_artifact",
            ops::save_workspace_artifact(b, "ticket-a", "Title", "text/plain", &digest, 12, &[]),
        ),
        (
            "save_workspace_artifact_tags",
            ops::save_workspace_artifact(
                b,
                "ticket-a",
                "Title",
                "text/plain",
                &digest,
                12,
                &["conversation:session-a".to_string(), "x".to_string()],
            ),
        ),
        (
            "save_project_artifact",
            ops::save_project_artifact(
                b,
                "project-a",
                "ticket-a",
                "Title",
                "text/plain",
                &digest,
                12,
            ),
        ),
        ("artifact_read", ops::artifact_read(b, &artifact)),
        (
            "delete_unsent_attachment",
            ops::delete_unsent_attachment(b, &artifact),
        ),
        ("workspace_artifacts", ops::workspace_artifacts(b, None)),
        (
            "project_artifacts",
            ops::project_artifacts(b, "project-a", None),
        ),
        (
            "prompt_artifacts_workspace",
            ops::prompt_artifacts(b, None, None),
        ),
        (
            "prompt_artifacts_project_cursor",
            ops::prompt_artifacts(b, Some("project-a"), Some("cursor-1")),
        ),
        ("cancel_index_run", ops::cancel_index_run(b, "run-a")),
        (
            "refresh_overlay",
            ops::refresh_overlay(b, "repo-a", "worktree-a", 3),
        ),
    ]
}
