//! Sample frames of the skill and role instruction requests.
use super::{Silent, from};
use crate::instructions_ops::{self as iop, Kind};
use devworks_core::protocol::provider::ArtifactRef;
use serde_json::json;

pub fn frames() -> Vec<(&'static str, Vec<u8>)> {
    let b = &Silent;
    let content: ArtifactRef = from(json!({"artifactId":"artifact-a","versionId":"version-a",
        "sha256":"a".repeat(64),"byteLength":"12","mediaType":"text/markdown"}));
    vec![
        (
            "instructions_discover_first",
            iop::query(b, iop::discover(None, None)),
        ),
        (
            "instructions_discover_cursor",
            iop::query(b, iop::discover(None, Some("cursor-1"))),
        ),
        (
            "instructions_role_read",
            iop::query(b, iop::read(Kind::Role, "role-worker", None)),
        ),
        (
            "instructions_skill_read_revision",
            iop::query(b, iop::read(Kind::Skill, "skill-1", Some(4))),
        ),
        (
            "instructions_skill_history",
            iop::query(b, iop::history(Kind::Skill, "skill-1", None)),
        ),
        (
            "instructions_role_history_cursor",
            iop::query(b, iop::history(Kind::Role, "role-worker", Some("cursor-2"))),
        ),
        (
            "instructions_resources_first",
            iop::query(b, iop::read_resources("skill-1", 3, None)),
        ),
        (
            "instructions_resources_cursor",
            iop::query(b, iop::read_resources("skill-1", 3, Some("cursor-3"))),
        ),
        (
            "instructions_skill_draft_new",
            iop::command(b, iop::draft(Kind::Skill, None, None, "lint", &content)),
        ),
        (
            "instructions_skill_draft_update",
            iop::command(
                b,
                iop::draft(Kind::Skill, None, Some(("skill-1", 7)), "lint", &content),
            ),
        ),
        (
            "instructions_role_draft_update",
            iop::command(
                b,
                iop::draft(
                    Kind::Role,
                    None,
                    Some(("role-worker", 2)),
                    "Implementation agent role",
                    &content,
                ),
            ),
        ),
        (
            "instructions_discover_project",
            iop::query(b, iop::discover(Some("project-a"), None)),
        ),
        (
            "instructions_skill_draft_project",
            iop::command(
                b,
                iop::draft(Kind::Skill, Some("project-a"), None, "lint", &content),
            ),
        ),
        (
            "instructions_skill_set_enabled",
            iop::command(b, iop::set_enabled("skill-1", 3, false)),
        ),
        (
            "instructions_skill_rollback",
            iop::command(b, iop::rollback(Kind::Skill, "skill-1", 5, 2)),
        ),
        (
            "instructions_role_rollback",
            iop::command(b, iop::rollback(Kind::Role, "role-worker", 4, 1)),
        ),
    ]
}
