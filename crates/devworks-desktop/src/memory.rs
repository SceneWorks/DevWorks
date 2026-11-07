//! Pane restoration: the shell's navigation and view state, saved beside the store as
//! `ui-state.json` and restored at the next launch. It holds selected IDs, view parameters,
//! drafts and references to already-stored attachments — never business records, which are
//! always re-read through the runtime (a restored selection that no longer exists is dropped).
//! Requests whose outcome was still unknown at close keep their exact frames here, so the next
//! launch confirms them with the same request key instead of losing what may have committed.
use crate::backend::Slot;
use crate::route::Route;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub mod purge;

pub const FILE: &str = "ui-state.json";
/// Version of this file's shape; another version is ignored, never misread.
pub const VERSION: u32 = 1;
/// Largest view-state file read back (it holds selections and drafts, never records).
pub const MAX_BYTES: u64 = 16 << 20;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SidebarTab {
    #[default]
    Current,
    History,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredAttachment {
    pub name: String,
    pub media_type: String,
    /// The stored version's `ArtifactRef`, as its wire JSON.
    pub artifact: Value,
}

/// An attachment whose save outcome was not known at close: its exact `artifacts.save` frame.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UnconfirmedAttachment {
    pub name: String,
    pub media_type: String,
    pub path: Option<PathBuf>,
    /// The shell created `path` (a pasted image): removed once the artifact service holds it.
    #[serde(default)]
    pub owned: bool,
    pub frame: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DraftMemory {
    pub text: String,
    /// Attachments already stored as artifacts (uploads still running are not restored).
    pub attachments: Vec<StoredAttachment>,
    #[serde(default)]
    pub unconfirmed: Vec<UnconfirmedAttachment>,
    /// Attachments whose save was never admitted (waiting for their conversation, or being
    /// prepared): stored at the next launch.
    #[serde(default)]
    pub uploads: Vec<PendingUpload>,
    /// Attachments whose save failed: kept with their file, shown failed, and retried on request.
    #[serde(default)]
    pub failed: Vec<FailedAttachment>,
}

/// An attachment whose save never started: its file, stored at the next launch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingUpload {
    pub name: String,
    pub media_type: String,
    pub path: PathBuf,
    /// The shell created `path` (a pasted image).
    #[serde(default)]
    pub owned: bool,
}

/// An attachment whose save failed (refused, or never possible): restored as failed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FailedAttachment {
    pub name: String,
    pub media_type: String,
    pub path: Option<PathBuf>,
    /// The shell created `path` (a pasted image).
    #[serde(default)]
    pub owned: bool,
    pub error: String,
}

/// A command whose outcome was not known at close.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingCommand {
    pub slot: Slot,
    pub label: String,
    pub operation: String,
    pub frame: Vec<u8>,
    #[serde(default)]
    pub dispatch: crate::backend::CommandDispatch,
}

/// The message an unconfirmed `sessions.append_message` submitted: its text and the stored
/// attachments (artifact IDs) it carried, settled from the draft once it is confirmed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubmittedMemory {
    pub text: String,
    pub artifacts: Vec<String>,
}

/// Removed attachments whose save outcome was not known at close: confirmed, then deleted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemovedMemory {
    /// A file the shell created for it (a pasted image), removed once resolved.
    pub path: Option<PathBuf>,
    pub frame: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PendingMemory {
    #[serde(default)]
    pub conventions: Value,
    #[serde(default)]
    pub databases: Value,
    pub commands: Vec<PendingCommand>,
    /// Why the unconfirmed `sessions.start` was started.
    pub starting: Option<crate::chat::StartReason>,
    pub submitted: Option<SubmittedMemory>,
    /// Stored attachments removed while that message was unconfirmed (`ArtifactRef` wire JSON):
    /// deleted if it turns out not to have committed.
    pub removed_while_sending: Vec<Value>,
    pub removed: Vec<RemovedMemory>,
    /// A Knowledge import whose save outcome is confirmed at the next launch.
    #[serde(default)]
    pub knowledge_import: Option<UnconfirmedImport>,
    /// Plan saves still running, one snapshot per feature, finished at the next launch.
    #[serde(default)]
    pub plans: Vec<Value>,
}
impl PendingMemory {
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
            && self.conventions.is_null()
            && self.databases.is_null()
            && self.removed.is_empty()
            && self.knowledge_import.is_none()
            && self.plans.is_empty()
    }
}

/// An import file's unconfirmed `artifacts.save`: its name and exact frame.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UnconfirmedImport {
    pub name: String,
    pub frame: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RouteMemory {
    pub tab: usize,
    pub filter: String,
    pub selection: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SettingsMemory {
    /// Selected category label (`None`: all categories).
    pub category: Option<String>,
    pub query: String,
    /// Unsaved drafts by setting key; re-applied (and re-validated) once settings load.
    pub drafts: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    #[serde(default)]
    pub purge_reviews: BTreeMap<String, Value>,
    #[serde(default)]
    pub purge_live: Option<crate::project_purge::Live>,
    #[serde(default)]
    pub purge_fences: BTreeMap<String, String>,
    pub version: u32,
    pub route: Route,
    pub intelligence_open: bool,
    pub coordinator_open: bool,
    pub selected_session: Option<String>,
    /// The project the navigation selected (dropped if it no longer exists).
    #[serde(default)]
    pub selected_project: Option<String>,
    pub sidebar_tab: SidebarTab,
    pub history_query: String,
    pub drafts: BTreeMap<String, DraftMemory>,
    pub routes: BTreeMap<Route, RouteMemory>,
    pub settings: SettingsMemory,
    /// Vertical scroll offsets of lists by their stable salt.
    #[serde(default)]
    pub scroll: BTreeMap<String, f32>,
    #[serde(default)]
    pub pending: PendingMemory,
    /// Route and effort selections of conversations that have no pin yet (the router holds a
    /// pinned conversation's selection), by conversation key.
    #[serde(default)]
    pub routing: BTreeMap<String, crate::routing::Selection>,
    /// Conversations whose first message waits to be routed (see `routing::Routing`).
    #[serde(default)]
    pub routing_first: std::collections::BTreeSet<String>,
}
impl Default for Memory {
    fn default() -> Self {
        Self {
            version: VERSION,
            purge_reviews: BTreeMap::new(),
            purge_fences: BTreeMap::new(),
            purge_live: None,
            route: Route::Chat,
            intelligence_open: true,
            coordinator_open: false,
            selected_session: None,
            selected_project: None,
            sidebar_tab: SidebarTab::Current,
            history_query: String::new(),
            drafts: BTreeMap::new(),
            routes: BTreeMap::new(),
            settings: SettingsMemory::default(),
            scroll: BTreeMap::new(),
            pending: PendingMemory::default(),
            routing: BTreeMap::new(),
            routing_first: std::collections::BTreeSet::new(),
        }
    }
}

impl Memory {
    /// Reads `path`; a missing, unreadable or other-version file restores the defaults.
    pub fn load(path: &Path) -> Self {
        crate::files::read_regular(path, MAX_BYTES)
            .ok()
            .and_then(|b| serde_json::from_slice::<Memory>(&b).ok())
            .filter(|m| m.version == VERSION)
            .unwrap_or_default()
    }
    /// Writes atomically (temporary file, then rename).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_round_trips_and_ignores_other_versions() {
        let dir = std::env::temp_dir().join(format!("devworks-memory-{}", std::process::id()));
        let path = dir.join(FILE);
        let mut m = Memory {
            route: Route::Settings,
            coordinator_open: true,
            ..Memory::default()
        };
        m.drafts.insert(
            "s-1".into(),
            DraftMemory {
                text: "draft".into(),
                attachments: Vec::new(),
                unconfirmed: Vec::new(),
                uploads: Vec::new(),
                failed: Vec::new(),
            },
        );
        m.save(&path).unwrap();
        assert_eq!(Memory::load(&path), m);
        std::fs::write(&path, br#"{"version": 99}"#).unwrap();
        assert_eq!(Memory::load(&path), Memory::default());
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(Memory::load(&path), Memory::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
