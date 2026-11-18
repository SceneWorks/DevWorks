//! Native Help frames. Topic selection, versions, passage boundaries and permission facts are
//! provided by the shared Help service; this module only selects operations and wire revision.
use crate::{
    backend::{Backend, Slot},
    frame::{self, Negotiate},
    ops,
};
use devworks_core::operations::{OperationInput as In, help};
use devworks_core::protocol::records::PageRequest;

/// The Help requests the screen waits for. The names are persisted (see [`Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum HelpSlot {
    /// A topic page (list generation, appended to the shown page).
    HelpTopics(u64, bool),
    /// The periodic catalog check (list generation).
    HelpProbe(u64),
    /// A passage read (read generation).
    HelpRead(u64),
    HelpGenerate,
}
impl From<HelpSlot> for Slot {
    fn from(slot: HelpSlot) -> Self {
        Slot::Help(slot)
    }
}
impl TryFrom<Slot> for HelpSlot {
    type Error = Slot;
    fn try_from(slot: Slot) -> Result<Self, Slot> {
        match slot {
            Slot::Help(slot) => Ok(slot),
            other => Err(other),
        }
    }
}

pub const PASSAGE_CHARS: u32 = 16_000;
pub const TOPICS_PER_PAGE: u16 = 50;
fn page(cursor: Option<&str>) -> Option<PageRequest> {
    frame::page(TOPICS_PER_PAGE.into(), cursor)
}
fn query(b: &dyn Backend, input: impl FnOnce() -> Option<help::Input>) -> Vec<u8> {
    frame::with(b, Negotiate::Latest, || input().map(In::Help))
}
pub fn search(b: &dyn Backend, text: &str, cursor: Option<&str>) -> Vec<u8> {
    query(b, || {
        Some(help::Input::Search(help::SearchInput {
            query: frame::text(if text.trim().is_empty() { "*" } else { text })?,
            page: page(cursor)?,
        }))
    })
}
pub fn context(b: &dyn Backend, screen: &str, cursor: Option<&str>) -> Vec<u8> {
    query(b, || {
        Some(help::Input::TopicForScreen(help::TopicForScreenInput {
            screen_id: frame::id(screen)?,
            capability_revision: None,
            current: Some(true),
            page: Some(page(cursor)?),
        }))
    })
}
pub fn read(b: &dyn Backend, topic: &str, version: &str, cursor: Option<&str>) -> Vec<u8> {
    query(b, || {
        Some(help::Input::Read(help::ReadInput {
            topic_id: frame::id(topic)?,
            version: frame::text(version)?,
            cursor: frame::lift(cursor.map(frame::text))?,
            max_chars: Some(frame::number(PASSAGE_CHARS.into())?),
        }))
    })
}
pub fn guides(b: &dyn Backend, package: &str, version: &str, cursor: Option<&str>) -> Vec<u8> {
    query(b, || {
        Some(help::Input::PackageGuides(help::PackageGuidesInput {
            package_id: frame::id(package)?,
            version: frame::text(version)?,
            page: page(cursor)?,
        }))
    })
}
pub fn generate(b: &dyn Backend, registry_revision: i64, help_revision: i64) -> Vec<u8> {
    frame::with(b, Negotiate::Initial, || {
        Some(In::Help(help::Input::GenerateCapabilityDocs(
            help::GenerateCapabilityDocsInput {
                registry_revision: frame::number(registry_revision)?,
                expected_help_revision: frame::number(help_revision)?,
            },
        )))
    })
}

use crate::ops::Failure;
use devworks_core::gateway::{CommandOutcome, QueryOutcome};
use devworks_core::protocol::records::{
    HelpReadResult, HelpReadView, HelpTopicPage, HelpTopicPageResult, OperationRef,
};
use devworks_core::protocol::results::{DomainDecision, OperationResult};
use devworks_core::protocol::{CommandReply, OperationId};

pub fn topics(outcome: QueryOutcome) -> Result<HelpTopicPage, Failure> {
    match ops::query_result(outcome)?.0 {
        OperationResult::HelpSearch { value }
        | OperationResult::HelpTopicForScreen { value }
        | OperationResult::HelpPackageGuides { value } => match value {
            HelpTopicPageResult::Current(page) => Ok(page),
            _ => Err(Failure::Other("The Help service did not return a current version-matched catalog. Refresh Help or check app compatibility.".into())),
        },
        other => Err(ops::unexpected("Help topics", &other)),
    }
}
pub fn passage(outcome: QueryOutcome) -> Result<HelpReadView, Failure> {
    match ops::query_result(outcome)?.0 {
        OperationResult::HelpRead { value: HelpReadResult::Current(view) } => Ok(view),
        OperationResult::HelpRead { .. } => Err(Failure::Other("The Help service did not return a bounded passage for this version. Refresh Help or check app compatibility.".into())),
        other => Err(ops::unexpected("Help passage", &other)),
    }
}
pub enum Generated {
    Accepted(OperationId),
    Recorded(OperationRef),
}
pub fn generated(outcome: CommandOutcome) -> Result<Generated, Failure> {
    match outcome {
        Ok(CommandReply::Recorded { receipt, .. })
            if matches!(receipt.decision, DomainDecision::Accepted { .. }) =>
        {
            let DomainDecision::Accepted { operation_id } = receipt.decision else {
                unreachable!()
            };
            Ok(Generated::Accepted(operation_id))
        }
        other => match ops::command_result(other)? {
            OperationResult::HelpGenerateCapabilityDocs { value } => Ok(Generated::Recorded(value)),
            result => Err(ops::unexpected("Help generation", &result)),
        },
    }
}
