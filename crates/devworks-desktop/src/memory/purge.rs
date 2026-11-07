//! Checked session-memory partition. This projection is not a purge permit or a complete
//! client cleanup: other scoped editors and original command outcomes are coordinated too.
use super::{DraftMemory, Memory, PendingCommand, VERSION};
use crate::backend::Slot;
use devworks_core::ports::storage::ProjectPurgeClientScope;
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::support::Check;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryPurgeFault {
    UnknownShape,
    UnsupportedVersion,
    Unavailable,
}

/// Unknown fields must not silently disappear in a destructive rewrite. Normal application
/// restoration remains tolerant; purge uses this checked decoding boundary instead.
pub fn decode(bytes: &[u8]) -> Result<Memory, MemoryPurgeFault> {
    let original: Value =
        serde_json::from_slice(bytes).map_err(|_| MemoryPurgeFault::UnknownShape)?;
    let memory: Memory =
        serde_json::from_value(original.clone()).map_err(|_| MemoryPurgeFault::UnknownShape)?;
    if memory.version != VERSION {
        return Err(MemoryPurgeFault::UnsupportedVersion);
    }
    let encoded = serde_json::to_value(&memory).map_err(|_| MemoryPurgeFault::UnknownShape)?;
    if !closed_keys(&original, &encoded) {
        return Err(MemoryPurgeFault::UnknownShape);
    }
    Ok(memory)
}
pub fn load(path: &Path) -> Result<Memory, MemoryPurgeFault> {
    match crate::files::read_regular(path, super::MAX_BYTES) {
        Ok(bytes) => decode(&bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Memory::default()),
        Err(_) => Err(MemoryPurgeFault::Unavailable),
    }
}
fn closed_keys(original: &Value, encoded: &Value) -> bool {
    match (original, encoded) {
        (Value::Object(a), Value::Object(b)) => a
            .iter()
            .all(|(k, v)| b.get(k).is_some_and(|e| closed_keys(v, e))),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| closed_keys(a, b))
        }
        _ => true,
    }
}

pub struct SessionProjection {
    pub before_digest: String,
    pub after: Memory,
    pub removed_draft_count: usize,
    /// Exact original frames; the coordinator must establish their outcomes before discard.
    pub unsettled_commands: Vec<PendingCommand>,
    pub unsettled_attachments: Vec<Vec<u8>>,
    pub owned_paths: BTreeSet<PathBuf>,
    /// Every surviving path is a keeper, including a picked input marked owned=false.
    pub surviving_paths: BTreeSet<PathBuf>,
    pub removed_references: Vec<ArtifactRef>,
    pub surviving_references: Vec<ArtifactRef>,
}
fn references(draft: &DraftMemory) -> Result<Vec<ArtifactRef>, MemoryPurgeFault> {
    draft
        .attachments
        .iter()
        .map(|a| {
            let reference: ArtifactRef = serde_json::from_value(a.artifact.clone())
                .map_err(|_| MemoryPurgeFault::UnknownShape)?;
            reference
                .check()
                .map_err(|_| MemoryPurgeFault::UnknownShape)?;
            Ok(reference)
        })
        .collect()
}
fn paths(draft: &DraftMemory, owned_only: bool) -> BTreeSet<PathBuf> {
    draft
        .uploads
        .iter()
        .filter(|a| !owned_only || a.owned)
        .map(|a| a.path.clone())
        .chain(
            draft
                .failed
                .iter()
                .filter(|a| !owned_only || a.owned)
                .filter_map(|a| a.path.clone()),
        )
        .chain(
            draft
                .unconfirmed
                .iter()
                .filter(|a| !owned_only || a.owned)
                .filter_map(|a| a.path.clone()),
        )
        .collect()
}
fn session_slot(slot: &Slot) -> Option<&str> {
    match slot {
        Slot::Append(id)
        | Slot::Attach(id, _)
        | Slot::Preview(id, _)
        | Slot::RouteSession(id)
        | Slot::RouteStatus(id)
        | Slot::RoutePin(id)
        | Slot::RouteFirst(id)
        | Slot::Transcript(id)
        | Slot::TextPreview(id, _) => Some(id),
        _ => None,
    }
}

pub fn project_sessions(
    memory: &Memory,
    scope: &ProjectPurgeClientScope,
) -> Result<SessionProjection, MemoryPurgeFault> {
    let sessions = scope
        .session_ids
        .iter()
        .map(|s| s.as_str())
        .collect::<BTreeSet<_>>();
    if sessions.contains("new") {
        return Err(MemoryPurgeFault::UnknownShape);
    }
    let mut after = memory.clone();
    let mut owned_paths = BTreeSet::new();
    let mut surviving_paths = BTreeSet::new();
    let mut removed_references = Vec::new();
    let mut surviving_references = Vec::new();
    let mut unsettled_attachments = Vec::new();
    let mut removed_draft_count = 0;
    for (key, draft) in &memory.drafts {
        if sessions.contains(key.as_str()) {
            owned_paths.extend(paths(draft, true));
            removed_references.extend(references(draft)?);
            unsettled_attachments.extend(draft.unconfirmed.iter().map(|a| a.frame.clone()));
            after.drafts.remove(key);
            removed_draft_count += 1;
        } else {
            surviving_paths.extend(paths(draft, false));
            surviving_references.extend(references(draft)?);
        }
    }
    let unsettled_commands = memory
        .pending
        .commands
        .iter()
        .filter(|c| session_slot(&c.slot).is_some_and(|id| sessions.contains(id)))
        .cloned()
        .collect::<Vec<_>>();
    let sending = unsettled_commands
        .iter()
        .any(|c| matches!(c.slot, Slot::Append(_)));
    after
        .pending
        .commands
        .retain(|c| !session_slot(&c.slot).is_some_and(|id| sessions.contains(id)));
    if sending {
        after.pending.submitted = None;
        after.pending.removed_while_sending.clear();
    }
    if after
        .selected_session
        .as_deref()
        .is_some_and(|id| sessions.contains(id))
    {
        after.selected_session = None;
    }
    if after.selected_project.as_deref() == Some(scope.project_id.0.as_str()) {
        after.selected_project = None;
    }
    after
        .routing
        .retain(|key, _| !sessions.contains(key.as_str()));
    after
        .routing_first
        .retain(|key| !sessions.contains(key.as_str()));
    for session in sessions {
        for surface in ["home", "sidebar"] {
            after
                .scroll
                .remove(&format!("transcript-{surface}-{session}"));
        }
    }
    let before_digest = devworks_core::fingerprint::sha256_hex(
        &serde_json::to_vec(memory).map_err(|_| MemoryPurgeFault::UnknownShape)?,
    );
    Ok(SessionProjection {
        before_digest,
        after,
        removed_draft_count,
        unsettled_commands,
        unsettled_attachments,
        owned_paths,
        surviving_paths,
        removed_references,
        surviving_references,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{PendingUpload, SubmittedMemory};
    use devworks_core::protocol::{Id, ProjectId};
    fn scope() -> ProjectPurgeClientScope {
        ProjectPurgeClientScope {
            project_id: ProjectId(Id::new("p").unwrap()),
            session_ids: vec![Id::new("p-session").unwrap()],
            conversation_ids: vec![],
            work_ids: vec![],
            operation_ids: vec![],
            deleted_artifact_ids: vec![],
        }
    }
    #[test]
    fn checked_purge_memory_refuses_unknown_fields_and_versions() {
        let mut value = serde_json::to_value(Memory::default()).unwrap();
        value["futurePrivateState"] = serde_json::json!({"secret":"must not silently disappear"});
        assert!(matches!(
            decode(&serde_json::to_vec(&value).unwrap()),
            Err(MemoryPurgeFault::UnknownShape)
        ));
        value.as_object_mut().unwrap().remove("futurePrivateState");
        value["version"] = 2.into();
        assert!(matches!(
            decode(&serde_json::to_vec(&value).unwrap()),
            Err(MemoryPurgeFault::UnsupportedVersion)
        ));
    }
    #[test]
    fn session_partition_keeps_home_other_project_and_surviving_picked_paths() {
        let mut memory = Memory::default();
        for key in ["new", "p-session", "other-session"] {
            memory.drafts.insert(
                key.into(),
                DraftMemory {
                    text: format!("{key} private p-session text"),
                    ..Default::default()
                },
            );
        }
        let shared = PathBuf::from("staging-ui/shared.png");
        for (key, owned) in [("p-session", true), ("other-session", false)] {
            memory
                .drafts
                .get_mut(key)
                .unwrap()
                .uploads
                .push(PendingUpload {
                    name: "private filename".into(),
                    media_type: "image/png".into(),
                    path: shared.clone(),
                    owned,
                });
        }
        memory.selected_session = Some("p-session".into());
        memory.selected_project = Some("p".into());
        memory
            .scroll
            .insert("transcript-home-p-session".into(), 42.0);
        let projected = project_sessions(&memory, &scope()).unwrap();
        assert_eq!(projected.removed_draft_count, 1);
        assert!(!projected.after.drafts.contains_key("p-session"));
        for key in ["new", "other-session"] {
            assert_eq!(projected.after.drafts[key], memory.drafts[key]);
        }
        assert!(
            projected.owned_paths.contains(&shared) && projected.surviving_paths.contains(&shared)
        );
        assert!(
            projected.after.selected_session.is_none()
                && projected.after.selected_project.is_none()
        );
        assert!(
            !projected
                .after
                .scroll
                .contains_key("transcript-home-p-session")
        );
        assert_eq!(
            memory.drafts.len(),
            3,
            "assessment does not mutate live memory"
        );
    }
    #[test]
    fn session_partition_retains_original_pending_frames_as_settlement_barriers() {
        let mut memory = Memory::default();
        let frame =
            br#"{"requestKey":"original-key","body":{"text":"private submitted text"}}"#.to_vec();
        memory.pending.commands.push(PendingCommand {
            slot: Slot::Append("p-session".into()),
            label: "Append".into(),
            operation: "sessions.append_message".into(),
            frame: frame.clone(),
            dispatch: Default::default(),
        });
        memory.pending.submitted = Some(SubmittedMemory {
            text: "private submitted text".into(),
            artifacts: vec![],
        });
        let projected = project_sessions(&memory, &scope()).unwrap();
        assert_eq!(projected.unsettled_commands[0].frame, frame);
        assert!(
            projected.after.pending.commands.is_empty()
                && projected.after.pending.submitted.is_none()
        );
        assert_eq!(
            memory.pending.commands.len(),
            1,
            "a coordinator must settle before applying the projection"
        );
    }
}
