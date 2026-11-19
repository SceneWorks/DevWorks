//! The requests the Add-ons screen waits for. The `addons.*` frames are in [`crate::addons`].
use crate::backend::screen_slot;
use std::path::PathBuf;

/// The Add-ons requests. The names are persisted (see [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum AddonsSlot {
    /// The Add-ons catalog.
    AddonsList,
    /// One package's logs.
    AddonsLogs(String),
    /// The Add-ons screen's one lifecycle command (or install-from-file task) at a time.
    AddonsCommand,
    /// Bundle artifact save before the exact install/update command; retained at close.
    AddonsSave {
        update: Option<String>,
        registry: i64,
        #[serde(default)]
        download: Option<PathBuf>,
    },
    /// The runtime probe and index refresh behind the Add-ons screen.
    AddonsDiscovery,
    PackageViews(String),
    PackageViewData {
        package: String,
        digest: String,
        view: String,
        request: u64,
    },
    PrepareViewAction,
    InvokeViewAction,
    ReadContent,
}
screen_slot!(AddonsSlot, AddOns);
