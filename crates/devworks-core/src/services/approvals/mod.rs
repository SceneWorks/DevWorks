//! Exact human decisions over source-owned proposals and closed, currently qualified commands.
//! Settings and feature plans retain their own business rules. Approval never clears pauses,
//! closes readiness findings generically, or supplies authority for an unqualified capability.
mod model;
pub mod owner_preview;
pub mod requests;

use crate::admission::DecodedRequest;
use crate::context::InvocationContext;
use crate::ports::Preparation;
use crate::ports::storage::{ConfigScope, WriteTransaction};
use crate::protocol::Id;
use crate::services::sessions::Failure;
use serde_json::Value;

/// Actual owner-derived contents and the source-generation fence reviewed by the person.
#[derive(Clone, Debug, PartialEq)]
pub struct OwnerPreview {
    pub scope: ConfigScope,
    pub target_id: Id,
    pub target_revision: i64,
    pub summary: String,
    pub reviewed: Value,
    pub binding: Value,
}

/// Only an actual business owner attests its current target, safety and effective contents.
/// Preparation must not publish/quiesce/install or execute the proposed effect. Validation
/// reuses owner rules inside the deciding transaction; actual dispatch runs them again.
pub trait SavedOperationOwners: Send + Sync {
    fn supports(&self, operation: &str) -> bool;
    fn decide_source(
        &self,
        _tx: &mut dyn WriteTransaction,
        _human: &InvocationContext,
        _request: &DecodedRequest,
        _preview: &OwnerPreview,
        _approve: bool,
    ) -> Result<(), Failure> {
        Ok(())
    }

    fn prepare(
        &self,
        ctx: &InvocationContext,
        request: &DecodedRequest,
        env: &Preparation<'_>,
    ) -> Result<OwnerPreview, Failure>;
    fn validate(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        request: &DecodedRequest,
        preview: &OwnerPreview,
    ) -> Result<(), Failure>;
}
mod replay;
mod review_queue;
mod service;
mod writes;
pub use replay::{ApprovedCommand, ResumePreparation};
pub use service::{ApprovalCatalog, ApprovalService, OPERATIONS};

pub use writes::stage_saved;
/// Canonical digest for a source-typed risk proposal; saved operations use requests::decode().fingerprint.
pub fn content_digest(content: &crate::protocol::records::ApprovalContent) -> String {
    model::hash(&serde_json::to_value(content).expect("typed approval content"))
}
