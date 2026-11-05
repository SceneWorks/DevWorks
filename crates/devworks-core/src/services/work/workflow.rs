//! The project workflow the work service reads: stable status identities with an explicit
//! semantic meaning (WORK-13/14), kept separate from execution and pause state (MODEL-03).
//!
//! # How the workflow item plugs in
//!
//! The work service never infers behavior from a status name: it reads the project's
//! `workflow_statuses` rows ([`crate::ports::storage::WorkRead::statuses`]) and acts on their
//! `semantic` only — `Done` records a completion (manual, or verified by acceptance evidence),
//! `Closed` is a card a re-plan removed (never a completion), and the others are ordinary
//! planning columns. A project that has no statuses yet gets the [`DEFAULT_STATUSES`] the first
//! time work is written in it, with deterministic IDs ([`default_status_id`]) so a client can
//! address them before any listing exists.
//!
//! The workflow item (custom statuses) replaces this default set by writing the same rows
//! (create, rename, order, reassign-and-remove) and advancing the project's
//! [`crate::ports::storage::WORKFLOW_QUEUE`] revision in the same unit of work; `tasks.move` checks that revision, so a
//! card moved against a changed workflow is refused rather than placed in a removed column. No
//! code here changes when custom statuses arrive: the seeding below only runs for a project
//! without any status row.
use crate::fingerprint::sha256_hex;
use crate::ports::storage::{StorageError, StoredStatus, WriteTransaction};
use crate::protocol::{Id, ProjectId};

/// The status semantics (`workflow_statuses.semantic`).
pub const BACKLOG: &str = "Backlog";
pub const READY: &str = "Ready";
pub const IN_PROGRESS: &str = "InProgress";
pub const IN_REVIEW: &str = "InReview";
pub const DONE: &str = "Done";
pub const CLOSED: &str = "Closed";

/// The default workflow: `(key, name, semantic)` in column order.
pub const DEFAULT_STATUSES: [(&str, &str, &str); 6] = [
    ("backlog", "Backlog", BACKLOG),
    ("ready", "Ready", READY),
    ("in-progress", "In progress", IN_PROGRESS),
    ("in-review", "In review", IN_REVIEW),
    ("done", "Done", DONE),
    ("closed", "Closed", CLOSED),
];
/// The status new work gets when its creator names none (`backlog`).
pub const DEFAULT_KEY: &str = "backlog";

/// The stable ID of a default status of `project`: `<projectId>-<key>`, or a digest-derived ID
/// when that would exceed the 128-character identifier bound.
pub fn default_status_id(project: &ProjectId, key: &str) -> Id {
    let plain = format!("{}-{key}", project.0.as_str());
    if plain.len() <= 128 {
        return Id::new(plain).expect("project IDs and keys are identifier characters");
    }
    let digest = sha256_hex(project.0.as_str().as_bytes());
    Id::new(format!("wf-{}-{key}", &digest[..40])).expect("derived identifier")
}

/// The project's statuses, seeding the default workflow when it has none.
pub fn ensure_workflow(
    tx: &mut dyn WriteTransaction,
    project: &ProjectId,
) -> Result<Vec<StoredStatus>, StorageError> {
    let current = tx.work().statuses(project)?;
    if !current.is_empty() {
        return Ok(current);
    }
    for (i, (key, name, semantic)) in DEFAULT_STATUSES.iter().enumerate() {
        tx.work().insert_status(&StoredStatus {
            id: default_status_id(project, key),
            project_id: project.clone(),
            name: (*name).to_owned(),
            semantic: (*semantic).to_owned(),
            sort_order: i as i64 + 1,
            revision: 1,
        })?;
    }
    tx.work().statuses(project)
}

/// The semantic of `status` among `statuses` (`None`: not a status of the project).
pub fn semantic_of<'a>(statuses: &'a [StoredStatus], status: &Id) -> Option<&'a str> {
    statuses
        .iter()
        .find(|s| &s.id == status)
        .map(|s| s.semantic.as_str())
}
/// Whether a status semantic ends a card's planning (completed or closed).
pub fn finished(semantic: &str) -> bool {
    semantic == DONE || semantic == CLOSED
}

/// The current Backlog column for new work, retaining custom identity after default removal.
pub fn default_status(
    tx: &mut dyn WriteTransaction,
    project: &ProjectId,
) -> Result<StoredStatus, StorageError> {
    ensure_workflow(tx, project)?
        .into_iter()
        .find(|s| s.semantic == BACKLOG)
        .ok_or_else(|| StorageError::InvalidRecord("workflow has no Backlog status".into()))
}
