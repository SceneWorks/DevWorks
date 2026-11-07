//! Native client custody: one lock covers live review, atomic persistence and cleanup.
use crate::memory::{Memory, purge};
use devworks_core::{
    fingerprint::sha256_hex,
    ports::{
        project_purge::{ProjectPurgeClient, PurgeClientSnapshot, PurgeFileFault},
        storage::ProjectPurgeClientScope,
    },
    protocol::provider::ArtifactRef,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, PartialEq)]
struct Fence {
    pub scope: ProjectPurgeClientScope,
    pub digest: String,
}
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Live {
    pub projects: BTreeMap<String, Value>,
    pub references: BTreeMap<String, Vec<ArtifactRef>>,
    pub busy: bool,
    #[serde(default)]
    pub paths: BTreeMap<String, Vec<PathBuf>>,
}
impl PartialEq for Live {
    fn eq(&self, other: &Self) -> bool {
        serde_json::to_value(self).ok() == serde_json::to_value(other).ok()
    }
}
struct State {
    memory: Option<Memory>,
    live: Live,
    online: bool,
    frozen: BTreeSet<String>,
    cleaned: Vec<ProjectPurgeClientScope>,
    awaiting_ui: BTreeSet<String>,
    fences: BTreeMap<String, Fence>,
}
pub struct NativePurgeClient {
    path: PathBuf,
    sources: Arc<devworks_adapters::fs::sources::HostArtifactSources>,
    state: Mutex<State>,
}
fn interrupted(path: &std::path::Path) -> bool {
    [
        path.with_extension("purge.tmp"),
        path.with_extension("json.tmp"),
    ]
    .iter()
    .any(|p| std::fs::symlink_metadata(p).is_ok())
}
fn save_checkpoint(memory: &Memory, path: &std::path::Path) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(memory)?;
    if bytes.len() as u64 > crate::memory::MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            "client checkpoint exceeds its read bound",
        ));
    }
    let tmp = path.with_extension("purge.tmp");
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}
fn fault(_: purge::MemoryPurgeFault) -> PurgeFileFault {
    PurgeFileFault::Unattributed
}
fn pending(p: &purge::ScopedProjection, scope: &ProjectPurgeClientScope) -> bool {
    !p.sessions.unsettled_commands.is_empty()
        || !p.sessions.unsettled_attachments.is_empty()
        || !p.convention_frames.is_empty()
        || p.convention_operations
            .iter()
            .any(|id| !scope.operation_ids.iter().any(|known| known.as_str() == id))
        || !p.active_convention_pipelines.is_empty()
        || !p.active_plan_saves.is_empty()
        || p.active_database_batch
}
fn witness(
    state: &State,
    scope: &ProjectPurgeClientScope,
) -> Result<(purge::ScopedProjection, PurgeClientSnapshot), PurgeFileFault> {
    let memory = state.memory.as_ref().ok_or(PurgeFileFault::Unattributed)?;
    let (partition, digest) = purge::scoped_witness(memory, scope).map_err(fault)?;
    let project = scope.project_id.0.as_str();
    let mut surviving = partition.sessions.surviving_references.clone();
    surviving.extend(
        state
            .live
            .references
            .iter()
            .filter(|(p, _)| p.as_str() != project)
            .flat_map(|(_, r)| r.clone()),
    );
    let digest = sha256_hex(&serde_json::to_vec(&serde_json::json!({"memory":digest,"live":state.live.projects.get(project),"references":surviving,"paths":state.live.paths.iter().filter(|(p,_)|p.as_str()!=project).collect::<BTreeMap<_,_>>()})).map_err(|_| PurgeFileFault::Unattributed)?);
    let snapshot = PurgeClientSnapshot {
        digest,
        surviving_artifacts: surviving,
        pending: pending(&partition, scope) || state.live.busy,
        removing_drafts: partition.sessions.removed_draft_count as u64
            + u64::from(state.live.projects.contains_key(project)),
        confirmation_ready: state.online && state.frozen.contains(project),
        surviving_paths: partition
            .sessions
            .surviving_paths
            .iter()
            .cloned()
            .chain(
                state
                    .live
                    .paths
                    .iter()
                    .filter(|(p, _)| p.as_str() != project)
                    .flat_map(|(_, v)| v.clone()),
            )
            .collect(),
    };
    Ok((partition, snapshot))
}
impl NativePurgeClient {
    pub fn new(
        path: PathBuf,
        sources: Arc<devworks_adapters::fs::sources::HostArtifactSources>,
    ) -> Self {
        let memory = purge::load(&path)
            .ok()
            .filter(|m| m.purge_fences.is_empty());
        let live = memory
            .as_ref()
            .and_then(|m| m.purge_live.clone())
            .unwrap_or_default();
        Self {
            path,
            sources,
            state: Mutex::new(State {
                memory,
                live,
                online: false,
                frozen: BTreeSet::new(),
                cleaned: vec![],
                awaiting_ui: BTreeSet::new(),
                fences: BTreeMap::new(),
            }),
        }
    }
    /// Restore fences only from the exact committed native manifest, never from a forged
    /// UI-state scope. This checks identities against the immutable reviewed row hashes.
    pub fn with_store(self, store: &dyn devworks_core::ports::storage::ApplicationStore) -> Self {
        let restored = purge::load(&self.path).ok().and_then(|memory| {
            let fences =
                devworks_core::ports::storage::read_snapshot(store, &Default::default(), |read| {
                    let mut fences = BTreeMap::new();
                    for (project, digest) in &memory.purge_fences {
                        let project_id = devworks_core::protocol::ProjectId(
                            devworks_core::protocol::Id::new(project).map_err(|_| {
                                devworks_core::ports::storage::StorageError::ScopeDenied
                            })?,
                        );
                        let Some(scope) =
                            read.projects().purge_client_fence(&project_id, digest)?
                        else {
                            return Ok(None);
                        };
                        fences.insert(
                            project.clone(),
                            Fence {
                                scope,
                                digest: digest.clone(),
                            },
                        );
                    }
                    Ok(Some(fences))
                })
                .ok()
                .flatten()?;
            Some((memory, fences))
        });
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((memory, fences)) = restored {
                state.live = memory.purge_live.clone().unwrap_or_default();
                state.memory = Some(memory);
                state.fences = fences;
            } else {
                state.memory = None;
                state.fences.clear();
            }
        }
        self
    }
    /// Called on the native frame loop. Persistent fences win over an older autosave snapshot.
    pub fn publish(
        &self,
        mut memory: Memory,
        mut live: Live,
        save: bool,
    ) -> Result<Vec<ProjectPurgeClientScope>, String> {
        serde_json::to_value(&live)
            .map_err(|_| "Native client paths cannot be safely checkpointed")?;
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let current = state
            .memory
            .as_ref()
            .ok_or("Purge custody could not validate the saved client state")?;
        memory.purge_fences = current.purge_fences.clone();
        for fence in state.fences.values() {
            let fences = memory.purge_fences.clone();
            memory = purge::project_scoped_editors(&memory, &fence.scope)
                .map_err(|_| "Client cleanup is held by unknown saved state")?
                .sessions
                .after;
            memory.purge_fences = fences;
            let project = fence.scope.project_id.0.as_str();
            live.projects.remove(project);
            live.references.remove(project);
            live.paths.remove(project);
        }
        memory.purge_live =
            (!state.frozen.is_empty() || current.purge_live.is_some()).then(|| live.clone());
        if save {
            save_checkpoint(&memory, &self.path).map_err(|e| e.to_string())?;
        }
        state.memory = Some(memory);
        state.live = live;
        state.online = true;
        Ok(std::mem::take(&mut state.cleaned))
    }
    pub fn acknowledge(&self, scope: &ProjectPurgeClientScope) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .awaiting_ui
            .remove(scope.project_id.0.as_str());
    }
    pub fn freeze(&self, project: &str, frozen: bool) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !frozen {
            state.frozen.remove(project);
            return Ok(());
        }
        let mut memory = state
            .memory
            .clone()
            .ok_or("Saved client state is unavailable")?;
        memory.purge_live = Some(state.live.clone());
        save_checkpoint(&memory, &self.path).map_err(|e| e.to_string())?;
        state.memory = Some(memory);
        state.frozen.insert(project.into());
        Ok(())
    }
}
impl ProjectPurgeClient for NativePurgeClient {
    fn inspect(
        &self,
        scope: &ProjectPurgeClientScope,
    ) -> Result<PurgeClientSnapshot, PurgeFileFault> {
        if interrupted(&self.path) {
            return Err(PurgeFileFault::Unattributed);
        }
        witness(&self.state.lock().unwrap_or_else(|p| p.into_inner()), scope).map(|(_, s)| s)
    }
    fn remove_reviewed(
        &self,
        scope: &ProjectPurgeClientScope,
        digest: &str,
    ) -> Result<(), PurgeFileFault> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if interrupted(&self.path) {
            return Err(PurgeFileFault::Unattributed);
        }
        let key = scope.project_id.0.as_str();
        if state
            .fences
            .get(key)
            .is_some_and(|f| f.scope == *scope && f.digest == digest)
        {
            self.sources.revoke_project(&scope.project_id);
            return if state.online && state.awaiting_ui.contains(key) {
                Err(PurgeFileFault::InUse)
            } else {
                Ok(())
            };
        }
        let (partition, snapshot) = witness(&state, scope)?;
        // A restart loses volatile editors, but not the reviewed persisted draft witness.
        if snapshot.digest != digest || snapshot.pending {
            return Err(PurgeFileFault::Changed);
        }
        let mut after = partition.sessions.after;
        let mut live = state.live.clone();
        live.projects.remove(key);
        live.references.remove(key);
        live.paths.remove(key);
        after.purge_live = Some(live);
        // Persist only the compact commitment. Exact identities already live in the
        // immutable native effect and are restored through the validating store port.
        after.purge_fences.insert(key.into(), digest.into());
        save_checkpoint(&after, &self.path).map_err(|_| PurgeFileFault::Io)?;
        state.fences.insert(
            key.into(),
            Fence {
                scope: scope.clone(),
                digest: digest.into(),
            },
        );
        state.memory = Some(after);
        state.live.projects.remove(key);
        state.live.references.remove(key);
        state.live.paths.remove(key);
        state.cleaned.push(scope.clone());
        self.sources.revoke_project(&scope.project_id);
        if state.online {
            state.awaiting_ui.insert(key.into());
            Err(PurgeFileFault::InUse)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        memory::DraftMemory,
        platform::{HostIds, SystemClock},
    };
    use devworks_core::protocol::{Id, ProjectId};
    fn scope() -> ProjectPurgeClientScope {
        ProjectPurgeClientScope {
            project_id: ProjectId(Id::new("p").unwrap()),
            session_ids: vec![Id::new("session-p").unwrap()],
            conversation_ids: vec![],
            work_ids: vec![],
            operation_ids: vec![],
            deleted_artifact_ids: vec![],
            record_ids: BTreeMap::new(),
        }
    }
    fn client(path: PathBuf) -> NativePurgeClient {
        NativePurgeClient::new(
            path,
            Arc::new(devworks_adapters::fs::sources::HostArtifactSources::new(
                Arc::new(SystemClock),
                Arc::new(HostIds::new()),
            )),
        )
    }
    #[test]
    fn purge_client_checkpoint_recovers_original_live_review_and_fences_stale_autosaves() {
        let root =
            std::env::temp_dir().join(format!("dw83-client-{}", HostIds::new().token("fixture")));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("ui-state.json");
        let mut memory = Memory::default();
        memory.drafts.insert(
            "session-p".into(),
            DraftMemory {
                text: "PRIVATE-CLIENT-SENTINEL".into(),
                ..Default::default()
            },
        );
        memory.drafts.insert(
            "new".into(),
            DraftMemory {
                text: "independent Home draft".into(),
                ..Default::default()
            },
        );
        let mut live = Live::default();
        live.projects.insert(
            "p".into(),
            serde_json::json!({"editor":"PRIVATE-LIVE-SENTINEL"}),
        );
        live.projects.insert(
            "other".into(),
            serde_json::json!({"editor":"independent editor"}),
        );
        let first = client(path.clone());
        first.publish(memory.clone(), live.clone(), true).unwrap();
        assert!(!first.inspect(&scope()).unwrap().confirmation_ready);
        first.freeze("p", true).unwrap();
        let reviewed = first.inspect(&scope()).unwrap();
        assert!(reviewed.confirmation_ready);
        drop(first);
        let restarted = client(path.clone());
        assert_eq!(restarted.inspect(&scope()).unwrap().digest, reviewed.digest);
        restarted
            .remove_reviewed(&scope(), &reviewed.digest)
            .unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("PRIVATE-"));
        assert!(saved.contains("independent Home draft"));
        assert!(saved.contains("independent editor"));
        restarted.publish(memory, live, true).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("PRIVATE-"));
        restarted
            .remove_reviewed(&scope(), &reviewed.digest)
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn large_native_record_inventory_keeps_the_client_checkpoint_compact() {
        let root =
            std::env::temp_dir().join(format!("dw83-large-{}", HostIds::new().token("fixture")));
        let path = root.join("ui-state.json");
        let client = client(path.clone());
        let mut scope = scope();
        scope.record_ids.insert(
            "messages".into(),
            (0..200_000)
                .map(|i| Id::new(format!("record-{i:08}-{}", "x".repeat(80))).unwrap())
                .collect(),
        );
        assert!(serde_json::to_vec(&scope).unwrap().len() as u64 > crate::memory::MAX_BYTES);
        client
            .publish(Memory::default(), Live::default(), true)
            .unwrap();
        let reviewed = client.inspect(&scope).unwrap();
        assert_eq!(
            client.remove_reviewed(&scope, &reviewed.digest),
            Err(PurgeFileFault::InUse)
        );
        assert!(std::fs::metadata(&path).unwrap().len() < 16_384);
        let saved = purge::load(&path).unwrap();
        assert_eq!(saved.purge_fences.get("p"), Some(&reviewed.digest));
        client.acknowledge(&scope);
        client.remove_reviewed(&scope, &reviewed.digest).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn failed_checkpoint_cannot_create_confirmation_readiness_or_overwrite_an_existing_file() {
        let root = std::env::temp_dir().join(format!(
            "dw83-checkpoint-{}",
            HostIds::new().token("fixture")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("ui-state.json");
        let client = client(path.clone());
        client
            .publish(Memory::default(), Live::default(), false)
            .unwrap();
        let temporary = path.with_extension("purge.tmp");
        std::fs::write(&temporary, b"independent existing bytes").unwrap();
        assert!(client.freeze("p", true).is_err());
        assert_eq!(
            std::fs::read(&temporary).unwrap(),
            b"independent existing bytes"
        );
        std::fs::remove_file(temporary).unwrap();
        assert!(!client.inspect(&scope()).unwrap().confirmation_ready);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn purge_client_changed_owned_draft_or_local_producer_refuses_original_cleanup() {
        let root =
            std::env::temp_dir().join(format!("dw83-client-{}", HostIds::new().token("fixture")));
        let path = root.join("ui-state.json");
        let client = client(path);
        let mut memory = Memory::default();
        memory.drafts.insert(
            "session-p".into(),
            DraftMemory {
                text: "first".into(),
                ..Default::default()
            },
        );
        client
            .publish(memory.clone(), Live::default(), true)
            .unwrap();
        let reviewed = client.inspect(&scope()).unwrap();
        memory.drafts.get_mut("session-p").unwrap().text = "changed".into();
        client.publish(memory, Live::default(), true).unwrap();
        assert_eq!(
            client.remove_reviewed(&scope(), &reviewed.digest),
            Err(PurgeFileFault::Changed)
        );
        client
            .publish(
                Memory::default(),
                Live {
                    busy: true,
                    ..Default::default()
                },
                false,
            )
            .unwrap();
        assert!(client.inspect(&scope()).unwrap().pending);
        std::fs::remove_dir_all(root).unwrap();
    }
}
