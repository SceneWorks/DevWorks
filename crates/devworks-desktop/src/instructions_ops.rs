//! Frames and typed projections for the skill and role instruction owner (DW-34). Every read
//! and change goes through the registered `skills.*` / `roles.*` gateway operations; text is
//! read through `artifacts.read` and stored through `artifacts.save`. Nothing here decides
//! anything the instruction service decides.
use crate::{
    backend::{Backend, TaskOutput},
    ops::{self, Failure},
};
use devworks_core::{
    gateway::{CommandOutcome, QueryOutcome},
    protocol::{
        DomainErrorCode,
        provider::ArtifactRef,
        records::{ArtifactReadContent, InstructionRecord},
        results::OperationResult,
    },
    services::instructions::{HARNESS_ROOT_ID, MAX_TEXT_BYTES, SHARED_ROOT_ID, role_item_id},
    settings::catalog::AgentRole,
};
use serde_json::{Value, json};
use std::path::PathBuf;

/// Rows one discovery or history page asks for.
pub const PAGE: u16 = 100;
/// Media type of the instruction text this surface stores.
pub const MEDIA_TYPE: &str = "text/markdown";

/// Skill items (file-backed) or role items (one per agent role).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Skill,
    Role,
}
impl Kind {
    pub fn family(self) -> &'static str {
        match self {
            Self::Skill => "skills",
            Self::Role => "roles",
        }
    }
    pub fn read_op(self) -> &'static str {
        match self {
            Self::Skill => "skills.read",
            Self::Role => "roles.read",
        }
    }
    pub fn history_op(self) -> &'static str {
        match self {
            Self::Skill => "skills.history",
            Self::Role => "roles.history",
        }
    }
    pub fn draft_op(self) -> &'static str {
        match self {
            Self::Skill => "skills.draft",
            Self::Role => "roles.draft",
        }
    }
    pub fn rollback_op(self) -> &'static str {
        match self {
            Self::Skill => "skills.rollback",
            Self::Role => "roles.rollback",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Status labels

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Shared,
    Harness,
    Role,
    Unknown,
}
impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Self::Shared => "Shared",
            Self::Harness => "Harness",
            Self::Role => "Role",
            Self::Unknown => "Unknown origin",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Draft,
    Proposed,
    Approved,
    Rejected,
    Unknown,
}
impl State {
    pub fn label(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Proposed => "proposed",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Unknown => "unknown state",
        }
    }
}

/// The visible status a record's `applicability` labels carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub origin: Origin,
    pub state: State,
    pub active: bool,
    /// `Some(true)` effective, `Some(false)` overridden by a same-name harness skill.
    pub effective: Option<bool>,
    pub disabled: bool,
    pub missing: bool,
    /// Role applicability and any label this surface does not interpret.
    pub other: Vec<String>,
}
pub fn status(labels: &[String]) -> Status {
    let mut s = Status {
        origin: Origin::Unknown,
        state: State::Unknown,
        active: false,
        effective: None,
        disabled: false,
        missing: false,
        other: Vec::new(),
    };
    for label in labels {
        match label.as_str() {
            "origin:shared" => s.origin = Origin::Shared,
            "origin:harness" => s.origin = Origin::Harness,
            "origin:role" => s.origin = Origin::Role,
            "state:draft" => s.state = State::Draft,
            "state:proposed" => s.state = State::Proposed,
            "state:approved" => s.state = State::Approved,
            "state:rejected" => s.state = State::Rejected,
            "active" => s.active = true,
            "effective" => s.effective = Some(true),
            "overridden" => s.effective = Some(false),
            "disabled" => s.disabled = true,
            "missing" => s.missing = true,
            other => s.other.push(other.to_owned()),
        }
    }
    s
}
pub fn labels(record: &InstructionRecord) -> Vec<String> {
    record.applicability.0.iter().map(|l| l.0.clone()).collect()
}
pub fn record_status(record: &InstructionRecord) -> Status {
    status(&labels(record))
}

/// The item's latest revision: the highest revision in its history, in any state. This is
/// the `expectedRevision` a draft or rollback states.
pub fn latest_revision(history: &[InstructionRecord]) -> Option<i64> {
    history.iter().map(|r| r.revision.get()).max()
}
/// Whether a human may restore this revision directly (approved and not already active).
pub fn restorable(record: &InstructionRecord) -> bool {
    let s = record_status(record);
    record.approved && !s.active
}

/// Where a skill's files are, for display (`root` caption + relative directory).
pub fn location(record: &InstructionRecord) -> Option<String> {
    let path = record.source_path.as_ref()?;
    let root = match path.root_id.as_str() {
        SHARED_ROOT_ID => "~/.agents/skills".to_owned(),
        HARNESS_ROOT_ID => "<data folder>/skills".to_owned(),
        other => other.to_owned(),
    };
    Some(format!("{root}/{}", path.relative.0))
}

/// A new harness skill's name is its directory name: exactly one non-empty path segment.
pub fn valid_skill_name(name: &str) -> bool {
    let n = name.trim();
    !n.is_empty()
        && n == name
        && n != "."
        && n != ".."
        && !n.contains(['/', '\\', '\0'])
        && !(n.len() >= 2 && n.as_bytes()[1] == b':')
        && n.len() <= 255
}
/// The service refuses blank instruction text; so does the Save control.
pub fn savable_text(text: &str) -> bool {
    !text.trim().is_empty() && text.len() as u64 <= MAX_TEXT_BYTES
}

pub fn role_id(role: AgentRole) -> String {
    role_item_id(role, &devworks_core::ports::storage::ConfigScope::Workspace)
        .as_str()
        .to_owned()
}
pub fn role_title(role: AgentRole) -> &'static str {
    match role {
        AgentRole::WorkspaceCoordinator => "Workspace coordinator role",
        AgentRole::FeatureCoordinator => "Feature coordinator role",
        AgentRole::IssueCoordinator => "Issue coordinator role",
        AgentRole::Worker => "Implementation agent role",
        AgentRole::CodeReviewer => "Code review agent role",
        AgentRole::Dreaming => "Dreaming agent role",
        AgentRole::Scheduled => "Scheduled agent role",
    }
}

// ---------------------------------------------------------------------------------------------
// Frames

fn workspace() -> Value {
    json!({"kind": "Workspace"})
}
fn page(cursor: Option<&str>) -> Value {
    let mut p = json!({"limit": PAGE});
    if let Some(c) = cursor {
        p["cursor"] = json!(c);
    }
    p
}
pub fn discover_body(cursor: Option<&str>) -> Value {
    json!({"scope": workspace(), "rootIds": [], "page": page(cursor)})
}
pub fn read_body(id: &str, revision: Option<i64>) -> Value {
    let mut body = json!({"instructionId": id});
    if let Some(r) = revision {
        body["revision"] = json!(r.to_string());
    }
    body
}
pub fn history_body(id: &str, cursor: Option<&str>) -> Value {
    json!({"instructionId": id, "page": page(cursor)})
}
pub fn resources_body(id: &str, revision: i64, cursor: Option<&str>) -> Value {
    json!({"instructionId": id, "revision": revision.to_string(), "page": page(cursor)})
}
/// A human draft. `target` is `(instructionId, latest revision)`; `None` creates a new
/// harness skill named `name`.
pub fn draft_body(target: Option<(&str, i64)>, name: &str, content: &ArtifactRef) -> Value {
    let mut body = json!({"scope": workspace(), "name": name, "content": content});
    if let Some((id, expected)) = target {
        body["instructionId"] = json!(id);
        body["expectedRevision"] = json!(expected.to_string());
    }
    body
}
pub fn rollback_body(id: &str, expected: i64, target: i64) -> Value {
    json!({"instructionId": id, "expectedRevision": expected.to_string(),
        "targetRevision": target.to_string()})
}
pub fn query(b: &dyn Backend, operation: &str, body: Value) -> Vec<u8> {
    ops::query_frame(b, operation, body)
}
pub fn command(b: &dyn Backend, operation: &str, body: Value) -> Vec<u8> {
    ops::command_frame(b, operation, body)
}

// ---------------------------------------------------------------------------------------------
// Results

pub struct Listed {
    pub items: Vec<InstructionRecord>,
    pub next: Option<String>,
}
pub fn records_page(outcome: QueryOutcome) -> Result<Listed, Failure> {
    match ops::query_result(outcome)?.0 {
        OperationResult::SkillsDiscover { value }
        | OperationResult::SkillsHistory { value }
        | OperationResult::RolesHistory { value } => Ok(Listed {
            items: value.items.0,
            next: value.next_cursor.map(|c| c.0).filter(|_| value.has_more),
        }),
        other => Err(ops::unexpected("Instruction listing", &other)),
    }
}
pub fn record(outcome: QueryOutcome) -> Result<InstructionRecord, Failure> {
    match ops::query_result(outcome)?.0 {
        OperationResult::SkillsRead { value } | OperationResult::RolesRead { value } => Ok(value),
        other => Err(ops::unexpected("Instruction read", &other)),
    }
}
pub struct Resources {
    pub items: Vec<ArtifactRef>,
    pub next: Option<String>,
}
pub fn resources(outcome: QueryOutcome) -> Result<Resources, Failure> {
    match ops::query_result(outcome)?.0 {
        OperationResult::SkillsResources { value } => Ok(Resources {
            items: value.items.0,
            next: value.next_cursor.map(|c| c.0).filter(|_| value.has_more),
        }),
        other => Err(ops::unexpected("skills.resources", &other)),
    }
}
pub fn changed(outcome: CommandOutcome) -> Result<InstructionRecord, Failure> {
    match ops::command_result(outcome)? {
        OperationResult::SkillsDraft { value }
        | OperationResult::SkillsRollback { value }
        | OperationResult::RolesDraft { value }
        | OperationResult::RolesRollback { value } => Ok(value),
        other => Err(ops::unexpected("Instruction change", &other)),
    }
}
/// A stale `expectedRevision`: the item moved since it was loaded.
pub fn is_conflict(failure: &Failure) -> bool {
    matches!(failure, Failure::Refused(f) if f.code == DomainErrorCode::RevisionConflict)
}
pub fn is_not_found(failure: &Failure) -> bool {
    matches!(failure, Failure::Refused(f) if f.code == DomainErrorCode::NotFound)
}

// ---------------------------------------------------------------------------------------------
// Tasks (worker threads)

pub enum TaskResult {
    /// The complete text of one immutable instruction artifact.
    Text(String),
    /// The staged text and the exact `artifacts.save` frame that stores it.
    Prepared { frame: Vec<u8>, staged: PathBuf },
}

/// Reads the complete text of `artifact` through `artifacts.read` (paged by byte cursor, or a
/// verified stream for non-text bytes) and checks it against the reference's length and digest.
pub fn load_text(b: &dyn Backend, artifact: &ArtifactRef) -> Result<TaskOutput, String> {
    use sha2::{Digest, Sha256};
    let expected = artifact.byte_length.get() as u64;
    if expected > MAX_TEXT_BYTES {
        return Err("The instruction text is larger than the editable limit.".into());
    }
    let mut bytes: Vec<u8> = Vec::with_capacity(expected as usize);
    let mut cursor: Option<String> = None;
    loop {
        let mut frame: Value =
            serde_json::from_slice(&ops::artifact_read(b, artifact)).map_err(|e| e.to_string())?;
        if let Some(c) = &cursor {
            frame["cursor"] = json!(c);
        }
        let frame = frame.to_string().into_bytes();
        match ops::read_content(b.query("artifacts.read", &frame)).map_err(|e| e.describe())? {
            ArtifactReadContent::Text {
                content,
                truncated,
                next_byte,
                ..
            } => {
                bytes.extend_from_slice(content.0.as_bytes());
                if bytes.len() as u64 > expected {
                    return Err("The text exceeded its recorded length.".into());
                }
                if !truncated {
                    break;
                }
                let next = next_byte.get().to_string();
                if cursor.as_deref() == Some(next.as_str()) {
                    return Err("The text read did not advance.".into());
                }
                cursor = Some(next);
            }
            ArtifactReadContent::Binary { stream_id, .. } => {
                bytes = b.read_stream(&stream_id, expected)?;
                break;
            }
        }
    }
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if bytes.len() as u64 != expected || digest != artifact.sha256.as_str() {
        return Err("The text did not match its recorded length and digest.".into());
    }
    let text = String::from_utf8(bytes).map_err(|_| "The instruction text is not UTF-8.")?;
    Ok(TaskOutput::Instruction(TaskResult::Text(text)))
}

/// Stages `text` and builds the workspace `artifacts.save` frame that stores it. The frame is
/// submitted once and, if its outcome is unknown, resubmitted unchanged.
pub fn prepare_save(b: &dyn Backend, title: &str, text: &str) -> Result<TaskOutput, String> {
    use sha2::{Digest, Sha256};
    if !savable_text(text) {
        return Err("Instruction text must not be blank.".into());
    }
    let staged = b.stage_project_text(None, "md", text.as_bytes())?;
    let built = (|| {
        let ticket = b.mint_workspace_upload(&staged)?;
        let digest: String = Sha256::digest(text.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok::<_, String>(ops::save_workspace_artifact(
            b,
            ticket.as_str(),
            title,
            MEDIA_TYPE,
            &digest,
            text.len() as u64,
            &[],
        ))
    })();
    match built {
        Ok(frame) => Ok(TaskOutput::Instruction(TaskResult::Prepared {
            frame,
            staged,
        })),
        Err(e) => {
            crate::attachments::remove_owned(&staged, &b.scratch_dir());
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(revision: i64, approved: bool, labels: &[&str]) -> InstructionRecord {
        serde_json::from_value(json!({
            "id": "skill-1", "revision": revision.to_string(), "name": "lint",
            "scope": {"kind": "Workspace"},
            "content": {"artifactId": "art-1", "versionId": "ver-1",
                "sha256": "a".repeat(64), "byteLength": "3", "mediaType": "text/markdown"},
            "sourcePath": {"rootId": "skills-shared", "relative": "lint"},
            "approved": approved, "applicability": labels,
        }))
        .expect("record")
    }

    #[test]
    fn labels_parse_into_status() {
        let s = status(&[
            "origin:shared".into(),
            "state:approved".into(),
            "active".into(),
            "overridden".into(),
            "missing".into(),
            "worker".into(),
        ]);
        assert_eq!(s.origin, Origin::Shared);
        assert_eq!(s.state, State::Approved);
        assert!(s.active && s.missing && !s.disabled);
        assert_eq!(s.effective, Some(false));
        assert_eq!(s.other, vec!["worker".to_owned()]);
        let h = status(&[
            "origin:harness".into(),
            "effective".into(),
            "disabled".into(),
        ]);
        assert_eq!(h.origin, Origin::Harness);
        assert_eq!(h.effective, Some(true));
        assert!(h.disabled && !h.active);
        assert_eq!(h.state, State::Unknown);
        assert_eq!(status(&["origin:role".into()]).origin, Origin::Role);
    }

    #[test]
    fn latest_revision_is_the_highest_in_any_state() {
        let history = vec![
            rec(3, false, &["state:proposed"]),
            rec(5, false, &["state:rejected"]),
            rec(4, true, &["state:approved", "active"]),
        ];
        assert_eq!(latest_revision(&history), Some(5));
        assert_eq!(latest_revision(&[]), None);
        assert!(!restorable(&history[0]));
        assert!(!restorable(&history[2]), "already active");
        assert!(restorable(&rec(1, true, &["state:approved"])));
    }

    #[test]
    fn location_names_the_root_and_directory() {
        let r = rec(1, true, &[]);
        assert_eq!(location(&r).as_deref(), Some("~/.agents/skills/lint"));
    }

    #[test]
    fn bodies_use_camel_case_and_decimal_revisions() {
        let content: ArtifactRef = rec(1, true, &[]).content;
        let b = draft_body(Some(("skill-1", 7)), "lint", &content);
        assert_eq!(b["instructionId"], "skill-1");
        assert_eq!(b["expectedRevision"], "7");
        assert_eq!(b["scope"]["kind"], "Workspace");
        assert_eq!(b["content"]["artifactId"], "art-1");
        let new = draft_body(None, "fresh", &content);
        assert!(new.get("instructionId").is_none() && new.get("expectedRevision").is_none());
        let r = rollback_body("role-worker", 4, 1);
        assert_eq!(r["expectedRevision"], "4");
        assert_eq!(r["targetRevision"], "1");
        let d = discover_body(Some("c1"));
        assert_eq!(d["page"]["limit"], json!(PAGE));
        assert!(d["page"]["limit"].is_u64(), "page limit is a JSON integer");
        assert_eq!(d["page"]["cursor"], "c1");
        assert_eq!(d["rootIds"], json!([]));
        assert_eq!(read_body("x", Some(2))["revision"], "2");
        assert!(read_body("x", None).get("revision").is_none());
        assert_eq!(resources_body("x", 3, None)["revision"], "3");
        assert_eq!(history_body("x", None)["page"]["limit"], json!(PAGE));
    }

    #[test]
    fn role_ids_are_deterministic() {
        assert_eq!(role_id(AgentRole::Worker), "role-worker");
        assert_eq!(role_id(AgentRole::CodeReviewer), "role-codeReviewer");
    }

    #[test]
    fn names_and_text_are_checked_before_saving() {
        assert!(valid_skill_name("lint-rules"));
        for bad in ["", " x", "a/b", "a\\b", "..", ".", "C:x"] {
            assert!(!valid_skill_name(bad), "{bad:?}");
        }
        assert!(!savable_text("  \n\t"));
        assert!(savable_text("# Do it"));
    }
}
