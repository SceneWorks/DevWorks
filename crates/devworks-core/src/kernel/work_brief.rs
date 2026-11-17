//! The stored brief of a work item (`work_items.brief_json`), which the authority reads to bind
//! an agent to its item without depending on the work service.
use crate::ports::storage::{StorageError, StoredWork};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{
    AcceptanceCriterion, HandoffRecord, TouchRef, WorkComment, WorkItemType, WorkLink,
};
use crate::protocol::support::Label;
use crate::protocol::{ActorId, AssignmentId, Id};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A recorded entry with who added it and when (the wire entry carries neither).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Noted<T> {
    pub item: T,
    pub actor: ActorId,
    pub at_ms: i64,
}

/// What a work item holds beyond its indexed columns (`work_items.brief_json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WorkBrief {
    /// Authored task classification, independent of feature membership. Legacy rows stay None.
    pub task_type: Option<WorkItemType>,
    pub labels: Vec<Label>,
    pub description: String,
    pub goals: Vec<String>,
    /// A card's acceptance criteria. For a feature, the recorded results (state and evidence)
    /// of its active plan's acceptance criteria, by ID at their approved text.
    pub acceptance: Vec<AcceptanceCriterion>,
    /// A feature's recorded results of its active plan's acceptance tests.
    pub acceptance_tests: Vec<AcceptanceCriterion>,
    pub inherits: Vec<Id>,
    pub previous_inheritance: Option<crate::protocol::records::WorkInheritanceOrigin>,
    pub touches: Vec<TouchRef>,
    pub links: Vec<WorkLink>,
    pub comments: Vec<Noted<WorkComment>>,
    pub handoffs: Vec<Noted<HandoffRecord>>,
    pub acceptance_report: Option<ArtifactRef>,
    pub assignment_id: Option<AssignmentId>,
    pub assigned_agent_id: Option<Id>,
    pub completed_by: Option<ActorId>,
    pub completed_at_ms: Option<i64>,
}
impl WorkBrief {
    pub fn of(w: &StoredWork) -> Result<Self, StorageError> {
        serde_json::from_value(w.brief.clone())
            .map_err(|e| StorageError::InvalidRecord(format!("stored work brief: {e}")))
    }
    pub fn value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }
}
