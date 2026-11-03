//! Pane restoration: the shell's navigation and view state, saved beside the store as
//! `ui-state.json` and restored at the next launch. It holds selected IDs, view parameters,
//! drafts and references to already-stored attachments — never business records, which are
//! always re-read through the runtime (a restored selection that no longer exists is dropped).
use crate::route::Route;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DraftMemory {
    pub text: String,
    /// Attachments already stored as artifacts (pending uploads are not restored).
    pub attachments: Vec<StoredAttachment>,
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
    pub version: u32,
    pub route: Route,
    pub intelligence_open: bool,
    pub coordinator_open: bool,
    pub selected_session: Option<String>,
    pub sidebar_tab: SidebarTab,
    pub history_query: String,
    pub drafts: BTreeMap<String, DraftMemory>,
    pub routes: BTreeMap<Route, RouteMemory>,
    pub settings: SettingsMemory,
    /// Vertical scroll offsets of lists by their stable salt.
    #[serde(default)]
    pub scroll: BTreeMap<String, f32>,
}
impl Default for Memory {
    fn default() -> Self {
        Self {
            version: VERSION,
            route: Route::Chat,
            intelligence_open: true,
            coordinator_open: false,
            selected_session: None,
            sidebar_tab: SidebarTab::Current,
            history_query: String::new(),
            drafts: BTreeMap::new(),
            routes: BTreeMap::new(),
            settings: SettingsMemory::default(),
            scroll: BTreeMap::new(),
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
