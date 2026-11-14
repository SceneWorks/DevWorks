//! The requests of All projects and Repositories. Their frames are the shared `projects.*`,
//! `repositories.*`, `settings.*` and `git.*` builders in [`crate::ops`].
use crate::backend::screen_slot;

/// The All projects and Repositories requests the screen waits for. The names are persisted
/// (see [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ProjectsSlot {
    DeletedProjects,
    ProjectCreate,
    /// An edit, membership or lifecycle command of one project (one at a time per project).
    ProjectCommand(String),
    /// A project's repository base directory setting (read, health, save).
    ProjectSettings(String),
    ProjectSettingsHealth(String),
    ProjectSettingsSave(String),
    /// A repository's README (`true`) or summary text.
    RepositoryText(String, bool),
    /// The artifact content of a repository's README (`true`) or summary.
    RepositoryContent(String, bool),
    RemoveIndex(String),
    /// A repository's source control state (repository ID).
    GitInspect(String),
    /// A patch between two commits of a repository, and its artifact text.
    GitDiff(String),
    GitDiffContent(String),
}
screen_slot!(ProjectsSlot, Projects);
