//! The requests the Scheduled jobs screen waits for. The jobs frames are in [`crate::ops`].
use crate::backend::screen_slot;

/// The jobs requests. The names are persisted (see [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum JobsSlot {
    /// The jobs of the selected scope, and a further page of them.
    JobsList,
    /// A further page of the shown jobs list (that list's generation).
    JobsMore(u64),
    /// A further page re-read by a refresh of the shown jobs list (the refresh's generation).
    JobsRefresh(u64),
    /// The jobs of one project, for its run status on All projects.
    ProjectJobs(String),
    /// One job's inspection (its first run-history page) and an older history page (job ID).
    JobInspect(String),
    JobHistory(String),
    /// The job's active run, inspected by its ID (job ID): the latest occurrence may be a
    /// skipped one while the active run continues.
    JobActiveRun(String),
    /// A scoped prompt page of one form generation (None is workspace).
    JobPrompts(u64, Option<String>),
    JobCreate,
    /// A command of one job or its run (one at a time per job).
    JobCommand(String),
}
screen_slot!(JobsSlot, Jobs);
