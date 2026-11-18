//! Frames and typed projections for the shared convention owner. No native second store.
use crate::{
    backend::{Backend, screen_slot},
    conventions_screen::{Scope, Target},
    frame::{self, Negotiate},
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
    operations::{OperationInput as In, conventions},
    protocol::{
        CommandReply, Id, OperationId, Revision,
        records::*,
        results::{DomainDecision, OperationResult},
        support::{BoundedList, Label},
    },
};
pub const PAGE: u16 = 50;
pub const PASSAGE_CHARS: u32 = 16384;
/// A convention request at the operation's newest schema revision, except the version and
/// resource pages, which stay at revision 3 (see [`negotiation`]). A request that did not
/// convert (`None`) is sent as no bytes, which the gateway rejects like any malformed request.
pub fn query(b: &dyn Backend, input: impl Into<Option<In>>) -> Vec<u8> {
    let input = input.into();
    let negotiate = input.as_ref().map_or(Negotiate::Latest, negotiation);
    frame::build(b, negotiate, input)
}
pub fn command(b: &dyn Backend, input: impl Into<Option<In>>) -> Vec<u8> {
    frame::build(b, Negotiate::Latest, input.into())
}
/// The desktop read these two pages at revision 3 while the registry lists a revision 4 for
/// them. This pin keeps the requests as they were; it is not a decision to stay on revision 3.
fn negotiation(input: &In) -> Negotiate {
    match input.operation() {
        "conventions.version" | "conventions.resources" => Negotiate::pinned(
            3,
            "the desktop read this page at revision 3 before revision 4",
        ),
        _ => Negotiate::Latest,
    }
}

fn page_request(cursor: Option<&str>) -> Option<PageRequest> {
    frame::page(u32::from(PAGE), cursor)
}
fn wrap(input: conventions::Input) -> Option<In> {
    Some(In::Conventions(input))
}
fn labels(values: &[String]) -> Option<BoundedList<Label, 0, 128>> {
    frame::texts(values)
}
fn lifecycle(index: usize) -> Vec<ConventionLifecycle> {
    match index {
        0 => vec![ConventionLifecycle::Active],
        1 => vec![ConventionLifecycle::Retired],
        2 => vec![ConventionLifecycle::Active, ConventionLifecycle::Retired],
        _ => vec![ConventionLifecycle::Deleted],
    }
}
/// `conventions.list` of the first page, by the screen's lifecycle (0 active, 1 retired, 2 both,
/// otherwise deleted) and order (0 by title, otherwise by organization) choices.
pub fn list(
    scope: &Scope,
    applicability: &[String],
    lifecycle_choice: usize,
    order: usize,
) -> Option<In> {
    wrap(conventions::Input::List(conventions::ListInput {
        scope: scope.data()?,
        page: page_request(None)?,
        applicability: labels(applicability)?,
        lifecycles: Some(BoundedList(lifecycle(lifecycle_choice))),
        order: Some(if order == 0 {
            ConventionOrder::Title
        } else {
            ConventionOrder::Organization
        }),
    }))
}
/// `conventions.search` of the first page; `top_k` is left out when empty.
pub fn search_input(
    scope: &Scope,
    applicability: &[String],
    text: &str,
    top_k: &str,
) -> Option<In> {
    wrap(conventions::Input::Search(conventions::SearchInput {
        scope: scope.data()?,
        page: page_request(None)?,
        applicability: labels(applicability)?,
        text: frame::text(text)?,
        top_k: frame::lift((!top_k.is_empty()).then(|| frame::text(top_k)))?,
    }))
}
/// The active and retired conventions of `scope`, one page (the choices of a move).
pub fn parents(scope: &Scope, cursor: Option<&str>) -> Option<In> {
    wrap(conventions::Input::List(conventions::ListInput {
        scope: scope.data()?,
        page: page_request(cursor)?,
        applicability: BoundedList(Vec::new()),
        lifecycles: Some(BoundedList(lifecycle(2))),
        order: None,
    }))
}
/// `input` continued at the cursor `cursor` (the page after the one that returned it).
pub fn at_cursor(input: &In, cursor: &str) -> Option<In> {
    let mut input = input.clone();
    let cursor = frame::text(cursor)?;
    match &mut input {
        In::Conventions(conventions::Input::List(i)) => i.page.cursor = Some(cursor),
        In::Conventions(conventions::Input::Search(i)) => i.page.cursor = Some(cursor),
        In::Conventions(conventions::Input::Version(i)) => i.page.cursor = Some(cursor),
        In::Conventions(conventions::Input::Resources(i)) => i.page.cursor = Some(cursor),
        In::Conventions(conventions::Input::Read(i)) => i.cursor = Some(cursor),
        _ => return None,
    }
    Some(input)
}
/// A document resource request at the passage after `cursor` (the first passage without one).
pub fn at_document_cursor(input: &In, cursor: Option<&str>) -> Option<In> {
    let mut input = input.clone();
    if let In::Conventions(conventions::Input::Resources(conventions::ResourcesInput {
        selection: Some(ConventionResourcesSelection::Document { cursor: at, .. }),
        ..
    })) = &mut input
    {
        *at = frame::lift(cursor.map(frame::text))?;
        return Some(input);
    }
    None
}
/// The request with its continuation cursor left out, as data to compare two requests.
pub fn first_page(input: &In) -> Option<serde_json::Value> {
    let mut body = input.body().ok()?;
    if let Some(page) = body
        .get_mut("page")
        .and_then(serde_json::Value::as_object_mut)
    {
        page.remove("cursor");
    }
    Some(body)
}
/// The text a search request looks for.
pub fn search_text(input: &In) -> Option<&str> {
    match input {
        In::Conventions(conventions::Input::Search(i)) => Some(i.text.0.as_str()),
        _ => None,
    }
}
/// `conventions.read` of the passage the reader pane shows: `target` by ID, slug or indexed
/// path, in its stored format, at one revision or the current one.
pub fn read(scope: &Scope, target: &Target) -> Option<In> {
    let by_id = target.path.is_none() && !target.by_slug;
    let by_slug = target.path.is_none() && target.by_slug;
    wrap(conventions::Input::Read(conventions::ReadInput {
        convention_id: if by_id {
            Some(frame::id(&target.id)?)
        } else {
            None
        },
        revision: frame::lift(target.revision.map(frame::number))?,
        slug: frame::lift(by_slug.then(|| frame::text(&target.id)))?,
        scope: Some(scope.data()?),
        format: Some(frame::text(
            ["Markdown", "Raw", "Manifest", "SectionPath"][target.format],
        )?),
        cursor: None,
        max_chars: Some(frame::number(i64::from(PASSAGE_CHARS))?),
        section_path: target.section.clone(),
        operation_id: None,
        resource_id: None,
        max_bytes: None,
        path: frame::lift(target.path.as_deref().map(frame::texts))?,
    }))
}
/// `conventions.read` of one passage of the complete source of a convention (`format` names
/// `Markdown`, `Raw` or `Manifest`) or, with `resource`, of one attachment's bytes.
pub fn read_source(
    scope: &Scope,
    convention: &Id,
    revision: &Revision,
    resource: Option<&Id>,
    format: &str,
    cursor: Option<&str>,
) -> Option<In> {
    wrap(conventions::Input::Read(conventions::ReadInput {
        convention_id: Some(convention.clone()),
        revision: Some(revision.clone()),
        slug: None,
        scope: Some(scope.data()?),
        format: Some(frame::text(format)?),
        cursor: frame::lift(cursor.map(frame::text))?,
        max_chars: if resource.is_none() {
            Some(frame::number(65536)?)
        } else {
            None
        },
        section_path: None,
        operation_id: None,
        resource_id: resource.cloned(),
        max_bytes: if resource.is_some() {
            Some(frame::number(49152)?)
        } else {
            None
        },
        path: None,
    }))
}
/// `conventions.read` of the status of one operation.
pub fn operation_status(scope: &Scope, operation: &str) -> Option<In> {
    wrap(conventions::Input::Read(conventions::ReadInput {
        convention_id: None,
        revision: None,
        slug: None,
        scope: Some(scope.data()?),
        format: None,
        cursor: None,
        max_chars: None,
        section_path: None,
        operation_id: Some(OperationId(frame::id(operation)?)),
        resource_id: None,
        max_bytes: None,
        path: None,
    }))
}
/// `conventions.version` of one convention, one page.
pub fn versions(scope: &Scope, convention: &Id, cursor: Option<&str>) -> Option<In> {
    wrap(conventions::Input::Version(conventions::VersionInput {
        convention_id: convention.clone(),
        page: page_request(cursor)?,
        scope: Some(scope.data()?),
    }))
}
/// `conventions.resources` of the first page. `mode` is 1 for the catalog (narrowed by
/// `applicability`), 2 for the document under `root_slug`, otherwise the attachments of
/// `owner`.
pub fn resources_input(
    scope: &Scope,
    mode: usize,
    applicability: &[String],
    owner: Option<&ConventionRecord>,
    root_slug: &str,
) -> Option<In> {
    let selection = match mode {
        1 => ConventionResourcesSelection::Catalog {},
        2 => ConventionResourcesSelection::Document {
            root_slug: frame::text(root_slug)?,
            cursor: None,
            max_chars: Some(frame::number(i64::from(PASSAGE_CHARS))?),
        },
        _ => {
            let owner = owner?;
            ConventionResourcesSelection::Attachments {
                convention_id: owner.id.clone(),
                convention_revision: Some(owner.revision.clone()),
                lifecycles: Some(BoundedList(vec![
                    ConventionLifecycle::Active,
                    ConventionLifecycle::Retired,
                    ConventionLifecycle::Deleted,
                ])),
            }
        }
    };
    wrap(conventions::Input::Resources(conventions::ResourcesInput {
        scope: scope.data()?,
        page: page_request(None)?,
        applicability: labels(if mode == 1 { applicability } else { &[] })?,
        selection: Some(selection),
    }))
}

// ---------------------------------------------------------------------------------------------
// Commands

/// `conventions.applicability` setting the applicability and whether `base` is retired.
pub fn applicability(
    scope: &Scope,
    base: &ConventionRecord,
    applicability: &[String],
    retired: bool,
) -> Option<In> {
    applicability_input(scope, base, labels(applicability)?, retired)
}
fn applicability_input(
    scope: &Scope,
    base: &ConventionRecord,
    applicability: BoundedList<Label, 0, 128>,
    retired: bool,
) -> Option<In> {
    wrap(conventions::Input::Applicability(
        conventions::ApplicabilityInput {
            convention_id: base.id.clone(),
            expected_revision: base.revision.clone(),
            applicability,
            lifecycle: Some(if retired {
                ConventionLifecycle::Retired
            } else {
                ConventionLifecycle::Active
            }),
            scope: Some(scope.data()?),
        },
    ))
}
/// The command that retires `record` or, when it is retired, makes it active again with its
/// own applicability.
pub fn retire(scope: &Scope, record: &ConventionRecord) -> (&'static str, Option<In>) {
    if matches!(record.lifecycle, ConventionLifecycle::Retired) {
        (
            "conventions.applicability",
            applicability_input(scope, record, record.applicability.clone(), false),
        )
    } else {
        (
            "conventions.retire",
            (|| {
                wrap(conventions::Input::Retire(conventions::RetireInput {
                    convention_id: Some(record.id.clone()),
                    expected_revision: record.revision.clone(),
                    resource_id: None,
                    scope: Some(scope.data()?),
                    operation_id: None,
                }))
            })(),
        )
    }
}
/// `conventions.retire` asking a running operation to cancel, at the operation's revision.
pub fn cancel_operation(scope: &Scope, operation: &str, revision: i64) -> Option<In> {
    wrap(conventions::Input::Retire(conventions::RetireInput {
        convention_id: None,
        expected_revision: frame::number(revision)?,
        resource_id: None,
        scope: Some(scope.data()?),
        operation_id: Some(OperationId(frame::id(operation)?)),
    }))
}
/// `conventions.delete` of a convention.
pub fn delete(scope: &Scope, record: &ConventionRecord) -> Option<In> {
    wrap(conventions::Input::Delete(conventions::DeleteInput {
        convention_id: record.id.clone(),
        expected_revision: record.revision.clone(),
        resource_id: None,
        scope: Some(scope.data()?),
    }))
}
/// `conventions.delete_resource` of an attachment, at its convention's revision.
pub fn delete_resource(scope: &Scope, resource: &ConventionResourceRecord) -> Option<In> {
    wrap(conventions::Input::DeleteResource(
        conventions::DeleteResourceInput {
            convention_id: resource.convention_id.clone(),
            expected_revision: resource.convention_revision.clone(),
            resource_id: Some(resource.id.clone()),
            scope: Some(scope.data()?),
        },
    ))
}
/// `conventions.move` of `base` into `destination` under `parent` (the root without one).
pub fn move_to(
    scope: &Scope,
    base: &ConventionRecord,
    destination: &Scope,
    parent: Option<&str>,
    order: i64,
) -> Option<In> {
    wrap(conventions::Input::Move(conventions::MoveInput {
        convention_id: base.id.clone(),
        expected_revision: base.revision.clone(),
        destination: destination.data()?,
        parent: Some(match parent {
            Some(id) => ConventionParentChange::Child {
                parent_id: frame::id(id)?,
            },
            None => ConventionParentChange::Root {},
        }),
        sort_order: Some(frame::number(order)?),
        scope: Some(scope.data()?),
    }))
}
/// `conventions.index` of the whole scope at its source revision.
pub fn index_scope(scope: &Scope, revision: i64) -> Option<In> {
    wrap(conventions::Input::Index(conventions::IndexInput {
        convention_id: None,
        expected_revision: frame::number(revision)?,
        resource_id: None,
        all_scope: Some(scope.data()?),
        scope: None,
    }))
}
/// `conventions.index` of one convention.
pub fn index(scope: &Scope, record: &ConventionRecord) -> Option<In> {
    wrap(conventions::Input::Index(conventions::IndexInput {
        convention_id: Some(record.id.clone()),
        expected_revision: record.revision.clone(),
        resource_id: None,
        all_scope: None,
        scope: Some(scope.data()?),
    }))
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
        > devworks_core::protocol::helpers::conventions::MAX_MARKDOWN_BYTES
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
        let input = read_source(
            scope,
            &record.id,
            &record.revision,
            None,
            if raw { "Raw" } else { "Markdown" },
            cursor.as_deref(),
        );
        let view =
            document(b.query("conventions.read", &query(b, input))).map_err(|e| e.describe())?;
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
