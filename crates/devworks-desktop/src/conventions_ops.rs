//! Frames and typed projections for the shared convention owner. No native second store.
use crate::{
    backend::Backend,
    ops::{self, Failure},
};
use devworks_core::{
    gateway::{CommandOutcome, QueryOutcome},
    protocol::{
        CommandReply,
        records::*,
        results::{DomainDecision, OperationResult},
    },
};
use serde_json::Value;
pub const PAGE: u16 = 50;
pub const PASSAGE_CHARS: u32 = 16384;
pub fn query(b: &dyn Backend, operation: &str, body: Value) -> Vec<u8> {
    ops::at_revision(ops::query_frame(b, operation, body), 3)
}
pub fn command(b: &dyn Backend, operation: &str, body: Value) -> Vec<u8> {
    ops::at_revision(ops::command_frame(b, operation, body), 3)
}
fn earlier() -> Failure {
    Failure::Other("This saved response uses an earlier convention format. Refresh to inspect the current convention without changing the saved request.".into())
}
pub fn page(outcome: QueryOutcome) -> Result<ConventionPage, Failure> {
    match ops::query_result(outcome)?.0 {
        OperationResult::ConventionsList {
            value: ConventionPageResult::Current(page),
        }
        | OperationResult::ConventionsVersion {
            value: ConventionPageResult::Current(page),
        } => Ok(page),
        OperationResult::ConventionsList { .. } | OperationResult::ConventionsVersion { .. } => {
            Err(earlier())
        }
        other => Err(ops::unexpected("conventions.list", &other)),
    }
}
pub fn search(outcome: QueryOutcome) -> Result<ConventionSearchPage, Failure> {
    match ops::query_result(outcome)?.0 {
        OperationResult::ConventionsSearch {
            value: ConventionSearchPageResult::Current(page),
        } => Ok(page),
        OperationResult::ConventionsSearch { .. } => Err(earlier()),
        other => Err(ops::unexpected("conventions.search", &other)),
    }
}
pub fn document(outcome: QueryOutcome) -> Result<ConventionReadView, Failure> {
    match ops::query_result(outcome)?.0 {
        OperationResult::ConventionsRead {
            value: ConventionReadResult::Current(view),
        } => Ok(view),
        OperationResult::ConventionsRead { .. } => Err(earlier()),
        other => Err(ops::unexpected("conventions.read", &other)),
    }
}
pub fn resources(outcome: QueryOutcome) -> Result<ConventionResourcePage, Failure> {
    match ops::query_result(outcome)?.0 {
        OperationResult::ConventionsResources {
            value: ConventionResourcePageResult::Current(page),
        } => Ok(page),
        OperationResult::ConventionsResources { .. } => Err(earlier()),
        other => Err(ops::unexpected("conventions.resources", &other)),
    }
}
pub enum Change {
    Record(ConventionRecord),
    Mutation(ConventionMutation),
    Accepted(String),
    Earlier,
}
pub fn changed(outcome: CommandOutcome) -> Result<Change, Failure> {
    if let Ok(CommandReply::Recorded { receipt, .. }) = &outcome
        && let DomainDecision::Accepted { operation_id } = &receipt.decision
    {
        return Ok(Change::Accepted(operation_id.0.as_str().into()));
    }
    match ops::command_result(outcome)? {
        OperationResult::ConventionsEdit { value }
        | OperationResult::ConventionsApplicability { value }
        | OperationResult::ConventionsMove { value }
        | OperationResult::ConventionsImport { value } => Ok(match value {
            ConventionRecordResult::Current(record) => Change::Record(record),
            ConventionRecordResult::Legacy(_) => Change::Earlier,
        }),
        OperationResult::ConventionsRetire { value }
        | OperationResult::ConventionsDelete { value }
        | OperationResult::ConventionsDeleteResource { value }
        | OperationResult::ConventionsIndex { value } => Ok(match value {
            ConventionMutationResult::Current(mutation) => Change::Mutation(mutation),
            ConventionMutationResult::Legacy(_) => Change::Earlier,
        }),
        other => Err(ops::unexpected("Convention change", &other)),
    }
}
