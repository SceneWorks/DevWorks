//! Revision-bound code review and evidence used by Git delivery.
mod candidate;
mod commands;
mod effect;
mod final_review;
mod findings;
mod local;
mod local_fix;
pub use local::{CodeReviewReport, LocalReviewInput};
pub mod policy;
mod queries;
mod readiness;
mod remote;

use crate::context::InvocationContext;
use crate::ports::storage::{ApplicationStore, ReadSnapshot, StorageError, StoredCodeReview};
use crate::ports::{Clock, IdSource};
use crate::protocol::data::DeliveryBarrier;
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{LocalReviewTarget, ReviewRecord};
use crate::protocol::{ActorId, DomainErrorCode, Id};
use crate::services::kernel::{Failure, Refusal, not_found};
use crate::services::{artifacts::ArtifactService, git::GitService};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::sync::Arc;

pub const REVISION: u32 = 15;
pub const EFFECT_CONSUMER: &str = "code-review";

pub struct ReviewService {
    ids: Arc<dyn IdSource>,
    store: Arc<dyn ApplicationStore>,
    git: Arc<GitService>,
    artifacts: Arc<ArtifactService>,
    clock: Arc<dyn Clock>,
    github: Option<Arc<crate::services::github::GitHubService>>,
}

impl ReviewService {
    pub fn new(
        ids: Arc<dyn IdSource>,
        store: Arc<dyn ApplicationStore>,
        git: Arc<GitService>,
        artifacts: Arc<ArtifactService>,
        clock: Arc<dyn Clock>,
        github: Option<Arc<crate::services::github::GitHubService>>,
    ) -> Self {
        Self {
            ids,
            store,
            git,
            artifacts,
            clock,
            github,
        }
    }
}

/// A review's attribution remains on the original request if a later candidate is qualified.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewStateRecord {
    value: ReviewRecord,
    target: ReviewTarget,
    requested_by: ActorId,
    revision_of_policy: i64,
    source_artifacts: Vec<ReviewDiffArtifact>,
    local_binding: Option<local::LocalBinding>,
    operation: crate::protocol::OperationId,
    restore_epoch: i64,
    failure: Option<String>,
    #[serde(default)]
    final_rounds: Vec<final_review::FinalRound>,
    #[serde(default)]
    verified_candidates: Vec<crate::protocol::records::ReviewCandidateRepair>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewDiffArtifact {
    pub repository_id: Id,
    pub reference: ArtifactRef,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum ReviewTarget {
    Local { target: LocalReviewTarget },
    PullRequests { barrier: DeliveryBarrier },
}

fn decode<T: DeserializeOwned>(value: &serde_json::Value) -> Result<T, Failure> {
    serde_json::from_value(value.clone()).map_err(|_| {
        StorageError::InvalidRecord("stored code review state is invalid".into()).into()
    })
}

fn not_implemented() -> Failure {
    Refusal::new(DomainErrorCode::UnsupportedCapability, "review", None).into()
}

fn visible_review(
    snapshot: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    id: &Id,
) -> Result<(StoredCodeReview, ReviewStateRecord), Failure> {
    let missing = || not_found("review", id);
    let row = snapshot.git().code_review(id)?.ok_or_else(missing)?;
    let root = crate::services::work::visible_work(snapshot.work(), ctx, &row.root_id.0)
        .map_err(|_| missing())?;
    if root.project_id != row.project_id {
        return Err(missing().into());
    }
    let review: ReviewStateRecord = decode(&row.body)?;
    Ok((row, review))
}
