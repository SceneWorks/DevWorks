//! Frames and typed projections for the shared convention owner. No native second store.
use crate::{
    backend::{Backend, screen_slot},
    ops::{self, Failure},
};

/// The convention requests the screen waits for, each under its scope key. The names are
/// persisted (see [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ConventionsSlot {
    /// A Knowledge listing page (its request generation).
    ConventionsList(String, u64, bool, bool),
    ConventionRead(String, u64),
    ConventionEditor(String, String, u64),
    ConventionVersions(String, u64, bool),
    ConventionResources(String, u64, bool),
    ConventionParents(String, String, u64, bool),
    ConventionCommand(String, String),
    ConventionOperation(String, String),
    ConventionPrepare(String, String),
    ConventionArtifact(String, String),
    ConventionTransfer(String, String),
}
screen_slot!(ConventionsSlot, Conventions);
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
    ops::at_revision(
        ops::query_frame(b, operation, body),
        if matches!(operation, "conventions.list" | "conventions.search") {
            4
        } else {
            3
        },
    )
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
            value: ConventionReadResult::Current(ConventionReadOutcome::Document { view }),
        } => Ok(view),
        OperationResult::ConventionsRead { .. } => Err(earlier()),
        other => Err(ops::unexpected("conventions.read", &other)),
    }
}
pub fn resources(outcome: QueryOutcome) -> Result<ConventionResourcesView, Failure> {
    match ops::query_result(outcome)?.0 {
        OperationResult::ConventionsResources {
            value: ConventionResourcePageResult::Current(page),
        } => Ok(page),
        OperationResult::ConventionsResources { .. } => Err(earlier()),
        other => Err(ops::unexpected("conventions.resources", &other)),
    }
}
pub fn operation(outcome: QueryOutcome) -> Result<ConventionOperationStatus, Failure> {
    match ops::query_result(outcome)?.0 {
        OperationResult::ConventionsRead {
            value: ConventionReadResult::Current(ConventionReadOutcome::Operation { status }),
        } => Ok(status),
        other => Err(ops::unexpected("Convention operation", &other)),
    }
}
pub struct ReadResource {
    pub resource: ConventionResourceRecord,
    pub passage: ConventionBinaryPassage,
}
pub fn binary(result: OperationResult) -> Result<ReadResource, Failure> {
    match result {
        OperationResult::ConventionsRead {
            value:
                ConventionReadResult::Current(ConventionReadOutcome::Resource {
                    resource, passage, ..
                }),
        } => Ok(ReadResource { resource, passage }),
        other => Err(ops::unexpected("Convention attachment", &other)),
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

/// The editor owns one complete source buffer. Cursor/page bounds are never a document bound.
pub enum TaskResult {
    Pipeline(crate::conventions_transfer::Pipeline),
    Prepared(Vec<u8>),
    Transferred(String),
    Editor {
        id: String,
        identity: u64,
        markdown: std::sync::Arc<String>,
        raw: std::sync::Arc<String>,
    },
}
pub fn load_editor(
    b: &dyn Backend,
    scope: &crate::conventions_screen::Scope,
    record: &ConventionRecord,
    identity: u64,
) -> Result<TaskResult, String> {
    let markdown = load_complete_source(b, scope, record, false)?;
    let raw = if record.raw_content.is_some() {
        load_complete_source(b, scope, record, true)?
    } else {
        String::new()
    };
    Ok(TaskResult::Editor {
        id: record.id.as_str().into(),
        identity,
        markdown: std::sync::Arc::new(markdown),
        raw: std::sync::Arc::new(raw),
    })
}
fn load_complete_source(
    b: &dyn Backend,
    scope: &crate::conventions_screen::Scope,
    record: &ConventionRecord,
    raw: bool,
) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let expected = if raw {
        record.raw_content.as_ref()
    } else {
        record.content.as_ref()
    }
    .ok_or("The exact source artifact is unavailable; a partial draft cannot be saved.")?;
    if expected.byte_length.get() as u64
        > devworks_core::services::conventions::model::MAX_MARKDOWN_BYTES
    {
        return Err(
            "The source is larger than the convention owner's editable document limit.".into(),
        );
    }
    let mut text = String::with_capacity(expected.byte_length.get() as usize);
    let mut hash = Sha256::new();
    let mut cursor = None;
    let mut seen = std::collections::HashSet::new();
    loop {
        let mut body = serde_json::json!({"scope":scope.wire(),"conventionId":record.id,"revision":record.revision,"format":if raw{"Raw"}else{"Markdown"},"maxChars":"65536"});
        if let Some(next) = cursor.as_ref() {
            body["cursor"] = serde_json::json!(next);
        }
        let view = document(b.query("conventions.read", &query(b, "conventions.read", body)))
            .map_err(|e| e.describe())?;
        let actual = if raw {
            view.convention.raw_content.as_ref()
        } else {
            view.convention.content.as_ref()
        };
        let matches = actual.is_some_and(|a| {
            a.artifact_id == expected.artifact_id
                && a.version_id == expected.version_id
                && a.sha256.as_str() == expected.sha256.as_str()
                && a.byte_length.get() == expected.byte_length.get()
        });
        if view.convention.id != record.id
            || view.convention.revision.get() != record.revision.get()
            || !matches
        {
            return Err(
                "The source identity changed while loading. Your opening revision is retained."
                    .into(),
            );
        }
        if !matches!(view.availability, ConventionAvailability::Available) {
            return Err(view
                .reason
                .map_or_else(|| "The exact source is unavailable.".into(), |r| r.0));
        }
        let passage = view
            .passage
            .ok_or("The source owner returned no document passage.")?;
        if text.len().saturating_add(passage.text.0.len()) > expected.byte_length.get() as usize {
            return Err("The source exceeded its immutable byte length.".into());
        }
        hash.update(passage.text.0.as_bytes());
        text.push_str(&passage.text.0);
        if !passage.has_more {
            if passage.next_cursor.is_some() {
                return Err("The source paging result was inconsistent.".into());
            }
            break;
        }
        let next = passage
            .next_cursor
            .ok_or("More source was reported without a continuation.")?
            .0;
        if !seen.insert(next.clone()) {
            return Err("The source continuation did not advance.".into());
        }
        cursor = Some(next);
    }
    let digest: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if text.len() != expected.byte_length.get() as usize || digest != expected.sha256.as_str() {
        return Err(
            "The complete source did not match its immutable length and digest. Nothing was saved."
                .into(),
        );
    }
    Ok(text)
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Submission {
    pub scope: crate::conventions_screen::Scope,
    pub operation: String,
    pub target: Option<String>,
    pub draft: Option<(u64, u64)>,
    pub reader_generation: u64,
    #[serde(default)]
    pub resource: Option<(bool, String, u64)>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchRecovery {
    pub frame: Vec<u8>,
    pub append: bool,
    pub generation: u64,
}
/// Admission failures never started preparation. An ambiguous preparation/read failure keeps
/// its original query identity; a new invocation may confirm it without creating another.
pub fn search_uncertain(outcome: &QueryOutcome) -> bool {
    use devworks_core::{
        gateway::GatewayFailure,
        protocol::{DomainErrorCode, results::DomainQueryReply},
    };
    match outcome {
        Ok(DomainQueryReply::Rejected { fault, .. }) => matches!(
            fault.code,
            DomainErrorCode::OutcomeUnknown
                | DomainErrorCode::StorageUnavailable
                | DomainErrorCode::Cancelled
        ),
        Err(GatewayFailure::HandlerContract(_)) => true,
        _ => false,
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchContext {
    pub request: SearchRecovery,
    pub preparation: Option<(String, OperationRef)>,
}
pub fn preparation_unsettled(operation: &OperationRef) -> bool {
    !matches!(
        operation.state,
        devworks_core::protocol::OperationState::ObservedComplete
            | devworks_core::protocol::OperationState::Failed
            | devworks_core::protocol::OperationState::CancelledBeforeDispatch
    )
}
