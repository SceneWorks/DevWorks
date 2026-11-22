//! The requests the Settings screen waits for: settings, containers, web, MCP, provider
//! connections, embedding spaces and credential intake. The frames are the shared `settings.*`,
//! `mcp.*`, `web.*` and `router.*` builders in [`crate::ops`].
use crate::backend::screen_slot;

/// The Settings requests. The names are persisted (see [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SettingsSlot {
    SettingsRead,
    RetentionPreview,
    RetentionApply(String),
    RetentionInspect,
    RetentionCancel,
    WorkspaceStatus,
    WorkspaceFolder,
    WorkspaceReview,
    WorkspaceMigrationPlan,
    WorkspaceCancel,
    WorkspaceCommand,
    WorkspaceClose,
    ContainerCapabilities,
    SettingsValidate,
    SettingsHealth,
    SettingsSave,
    /// Transient native credential intake; no raw value is serialized with this slot.
    SettingsSecret(String),
    SettingsExplain(String),
    /// A page fetch from Settings > Agent tools.
    WebFetch,
    /// A search from Settings > Agent tools.
    WebSearch,
    /// The recent web activity record.
    WebActivity,
    McpServers(String, Option<String>),
    McpOperation(String),
    McpCapabilities(String, String, Option<String>),
    /// A resource or prompt read from Settings (scope key, server ID).
    McpContent(String, String),
    McpCommand,
    /// Deletes MCP secrets a configuration change left unreferenced.
    McpRelease,
    /// A page of `router.connections` for Settings → Provider connections (load generation).
    Connections(u64),
    /// A page of the full catalog (disabled models included) for the connections panel.
    ConnectionCatalog(u64),
    DecisionModels(u64),
    SystemOneDecide,
    SystemOneCompletion,
    /// One bounded persistent embedding-space status page.
    EmbeddingSpaces(u64),
    /// One `router.configure` submission from the connections panel (its sequence number).
    Configure(u64),
}
screen_slot!(SettingsSlot, Settings);
