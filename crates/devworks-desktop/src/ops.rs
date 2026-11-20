//! Typed frames for the operations the shell binds, and the decoding of their replies into
//! results or a [`Failure`] the views can present honestly (UI-06). Frames are the same wire
//! envelopes an agent tool submits; nothing here decides anything a use case decides.
use crate::backend::Backend;
use crate::frame::{self, Negotiate};
use crate::paged::Page;
use devworks_core::gateway::{CommandOutcome, GatewayFailure, QueryOutcome};
use devworks_core::operations::{
    OperationInput as In, artifacts, browser, containers, git, indexing, jobs, mcp, projects,
    repositories, resources, router, sessions, settings, web, workspace,
};
use devworks_core::protocol::OperationId;
use devworks_core::protocol::ProjectId;
use devworks_core::protocol::data::{
    ArtifactCommand, ArtifactCommandRequest, ArtifactDeletion, ArtifactDeletionRequest,
    ArtifactQuery, ArtifactQueryRequest, ArtifactSource, ArtifactTags, IndexCommand,
    IndexCommandRequest,
};
use devworks_core::protocol::data::{DataScope, GitOid, ObjectFormat, ViewSelection};
use devworks_core::protocol::provider::{
    ArtifactRef, SessionCommandRequest, SessionQueryRequest, TranscriptPage, UserContent,
};
use devworks_core::protocol::records::{
    AccessMode, ArtifactRecord, BrowserSession, ConnectionRecord, ContainerCapability,
    DiagnosticEntry, EffortName, JobDefinition, JobInspection, JobRecord, JobRun, MutationSummary,
    ProjectRecord, RepositoryRecord, RepositoryText, ResourceRecord, RouteAvailability,
    RouteCatalogModel, RouteChoice, RouteIntent, Schedule, SessionRecord, SettingEntry,
    ValidationReport, WebDocument, WebResult,
};
use devworks_core::protocol::records::{
    BrowserSessionAction, IndexSourceConfig, ProjectPatch, ProjectPurgeManifest,
    RouterConfigAction, SchemaValue, SettingAssignment, SortDirection,
};
use devworks_core::protocol::results::{DomainDecision, DomainQueryReply, OperationResult};
use devworks_core::protocol::support::{BoundedList, BoundedText, DynamicJson};
use devworks_core::protocol::{CommandReply, DomainFault};
use devworks_core::settings::view::SettingsView;
use serde_json::Value;

pub const PROTOCOL: &str = "1.0-draft.1";
/// Workspace artifacts (attachments without a project) need artifact schema revision 3. The
/// registry lists a newer one for `artifacts.save`; the desktop keeps sending the revision its
/// workspace saves were written against.
const WORKSPACE_ARTIFACT_NEGOTIATION: Negotiate = Negotiate::pinned(
    3,
    "workspace artifact saves are written against schema revision 3",
);
/// Most rows one page request asks for.
pub const PAGE_LIMIT: u16 = 200;

/// A list reply as a page: its rows and the cursor of the next page (none on the last page). A
/// reply that says more rows exist without a cursor to read them is refused, not shown as a
/// complete list.
fn page_of<T>(
    operation: &str,
    items: Vec<T>,
    has_more: bool,
    next_cursor: Option<devworks_core::protocol::support::Cursor>,
) -> Result<Page<T>, Failure> {
    let next = next_cursor.map(|c| c.0).filter(|_| has_more);
    if has_more && next.is_none() {
        return Err(Failure::Other(format!(
            "{operation} reported more pages without a cursor"
        )));
    }
    Ok(Page::new(items, next))
}

/// Why a request produced no usable result. Never rendered as success or as zero values.
#[derive(Clone, Debug)]
pub enum Failure {
    /// The use case refused (typed domain fault).
    Refused(DomainFault),
    /// The gateway rejected the frame before any use case ran.
    Rejected(String),
    Purged(devworks_core::protocol::records::PurgedReceiptOutcome),
    /// The command's outcome is unknown (lost acknowledgment); its receipt resolves it.
    Unknown,
    /// No use case is registered for the operation (owned by a later item).
    NoUseCase(String),
    /// Anything else: a contract violation, a local I/O error, an unexpected result.
    Other(String),
}
impl Failure {
    pub fn describe(&self) -> String {
        match self {
            Self::Refused(f) => {
                let target = f
                    .target_kind
                    .as_ref()
                    .map(|k| format!(" ({})", k.0))
                    .unwrap_or_default();
                format!(
                    "Refused: {:?}{target}. Reference {}.",
                    f.code,
                    f.correlation_id.0.as_str()
                )
            }
            Self::Rejected(r) => format!("The request was rejected: {r}."),
            Self::Purged(outcome) => format!(
                "The original request {} was recorded as {:?}. Its saved result was removed by project purge.",
                outcome.command_id.0.as_str(),
                outcome.original_decision
            ),
            Self::Unknown => {
                "The outcome is not known yet; it is resolved from the durable receipt.".into()
            }
            Self::NoUseCase(op) => format!("{op} is not available in this build."),
            Self::Other(e) => e.clone(),
        }
    }
}

fn gateway(e: GatewayFailure) -> Failure {
    match e {
        GatewayFailure::NoUseCase(op) => Failure::NoUseCase(op),
        other => Failure::Other(other.to_string()),
    }
}

/// A query result with the stamp's scope revision.
pub fn query_result(outcome: QueryOutcome) -> Result<(OperationResult, i64), Failure> {
    match outcome.map_err(gateway)? {
        DomainQueryReply::Result { result, stamp, .. } => Ok((result, stamp.scope_revision.get())),
        DomainQueryReply::Rejected { fault, .. } => Err(Failure::Refused(fault)),
    }
}

/// A command's applied (or unchanged) result.
pub fn command_result(outcome: CommandOutcome) -> Result<OperationResult, Failure> {
    match outcome.map_err(gateway)? {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied { result } | DomainDecision::NoChange { result } => Ok(result),
            DomainDecision::Accepted { operation_id } => Err(Failure::Other(format!(
                "accepted as operation {}",
                operation_id.0.as_str()
            ))),
            DomainDecision::Refused { fault } => Err(Failure::Refused(fault)),
        },
        CommandReply::Rejected { code, .. } => Err(Failure::Rejected(format!("{code:?}"))),
        CommandReply::ReceiptPurged { outcome, .. } => Err(Failure::Purged(outcome)),
        CommandReply::CommandStatusUnknown { .. } => Err(Failure::Unknown),
    }
}

pub(crate) fn unexpected(op: &str, r: &OperationResult) -> Failure {
    Failure::Other(format!("{op} returned {}", r.operation()))
}

pub fn mcp_servers(
    backend: &dyn Backend,
    scope: Option<&devworks_core::protocol::ProjectId>,
    cursor: Option<&str>,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Mcp(mcp::Input::Servers(mcp::ServersInput {
            scope: mcp_data_scope(scope),
            page: frame::page(100, cursor)?,
        })))
    })
}

pub fn mcp_server_page(
    outcome: QueryOutcome,
) -> Result<
    (
        Vec<devworks_core::protocol::records::McpServer>,
        Option<String>,
    ),
    Failure,
> {
    match query_result(outcome)? {
        (OperationResult::McpServers { value }, _) => {
            Ok((value.items.0, value.next_cursor.map(|cursor| cursor.0)))
        }
        (other, _) => Err(unexpected("mcp.servers", &other)),
    }
}

pub fn mcp_capabilities(
    backend: &dyn Backend,
    scope: Option<&devworks_core::protocol::ProjectId>,
    server: &devworks_core::protocol::Id,
    revision: i64,
    cursor: Option<&str>,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Mcp(mcp::Input::Capabilities(mcp::CapabilitiesInput {
            scope: Some(mcp_data_scope(scope)),
            server_id: server.clone(),
            catalog_revision: frame::number(revision)?,
            page: frame::page(100, cursor)?,
        })))
    })
}

/// The saved scope an outgoing MCP input names: the workspace, or one project.
pub fn mcp_data_scope(project: Option<&devworks_core::protocol::ProjectId>) -> DataScope {
    project.map_or(DataScope::Workspace, |id| DataScope::Project {
        project_id: id.clone(),
    })
}
fn router_catalog_input(cursor: Option<&str>) -> Option<In> {
    Some(In::Router(router::Input::Catalog(router::CatalogInput {
        page: frame::page(PAGE_LIMIT.into(), cursor)?,
        include_disabled: true,
    })))
}

pub fn mcp_capability_page(
    outcome: QueryOutcome,
) -> Result<
    (
        Vec<devworks_core::protocol::records::McpCapability>,
        Option<String>,
    ),
    Failure,
> {
    match query_result(outcome)? {
        (OperationResult::McpCapabilities { value }, _) => {
            Ok((value.items.0, value.next_cursor.map(|cursor| cursor.0)))
        }
        (other, _) => Err(unexpected("mcp.capabilities", &other)),
    }
}

/// The largest MCP resource or prompt result Settings shows inline.
pub const MCP_CONTENT_VIEW_BYTES: u64 = 1024 * 1024;

/// One resource or prompt a person asked to open from Settings.
#[derive(Clone, Debug)]
pub enum McpContentRequest {
    Resource {
        uri: String,
    },
    /// `arguments` is the prompt's schema and value when the person filled any in.
    Prompt {
        name: String,
        arguments: Option<SchemaValue>,
    },
}

/// The `mcp.resources` or `mcp.prompts` request for one resource or prompt.
pub(crate) fn mcp_content_frame(
    backend: &dyn Backend,
    scope: Option<&devworks_core::protocol::ProjectId>,
    server: &devworks_core::protocol::Id,
    catalog_revision: i64,
    request: &McpContentRequest,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        let scope = Some(mcp_data_scope(scope));
        let server_id = server.clone();
        let catalog_revision = frame::number(catalog_revision)?;
        let page = frame::page(1, None)?;
        Some(In::Mcp(match request {
            McpContentRequest::Resource { uri } => mcp::Input::Resources(mcp::ResourcesInput {
                scope,
                server_id,
                catalog_revision,
                uri: Some(frame::text(uri)?),
                page,
            }),
            McpContentRequest::Prompt { name, arguments } => {
                mcp::Input::Prompts(mcp::PromptsInput {
                    scope,
                    server_id,
                    catalog_revision,
                    name: Some(frame::text(name)?),
                    arguments: arguments.clone(),
                    page,
                })
            }
        }))
    })
}

/// Reads one MCP resource or prompt through `mcp.resources` or `mcp.prompts` and returns its
/// published content as text. Content larger than the viewer bound is named, not loaded.
pub fn mcp_content(
    backend: &dyn Backend,
    scope: Option<&devworks_core::protocol::ProjectId>,
    server: &devworks_core::protocol::Id,
    catalog_revision: i64,
    request: &McpContentRequest,
) -> Result<String, String> {
    let operation = match request {
        McpContentRequest::Resource { .. } => "mcp.resources",
        McpContentRequest::Prompt { .. } => "mcp.prompts",
    };
    let frame = mcp_content_frame(backend, scope, server, catalog_revision, request);
    let artifact =
        match query_result(backend.query(operation, &frame)).map_err(|e| e.describe())? {
            (OperationResult::McpResources { value }, _) => {
                value.items.0.into_iter().next().map(|item| item.content)
            }
            (OperationResult::McpPrompts { value }, _) => {
                value.items.0.into_iter().next().map(|item| item.messages)
            }
            (other, _) => return Err(unexpected(operation, &other).describe()),
        }
        .ok_or("The server returned nothing for this request.")?;
    if artifact.byte_length.get() as u64 > MCP_CONTENT_VIEW_BYTES {
        return Ok(format!(
            "The content is {} bytes, more than this viewer shows. It is saved as artifact {}.",
            artifact.byte_length.get(),
            artifact.artifact_id.as_str()
        ));
    }
    let bytes = read_artifact_bytes(backend, &artifact)?;
    Ok(match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_default(),
        Err(_) => String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// An `mcp.*` command at the newest schema revision.
pub fn mcp_command(backend: &dyn Backend, input: In) -> Vec<u8> {
    frame::build(backend, Negotiate::Latest, Some(input))
}

/// The message, any accepted operation, and the configured or reselected server.
pub fn mcp_command_status(
    outcome: CommandOutcome,
) -> Result<
    (
        String,
        Option<devworks_core::protocol::OperationId>,
        Option<devworks_core::protocol::records::McpServer>,
    ),
    Failure,
> {
    let server = |result| match result {
        OperationResult::McpConfigure { value }
        | OperationResult::McpSelectTools { value }
        | OperationResult::McpClassifyTool { value }
        | OperationResult::McpDisable { value } => Some(value),
        // A removed server no longer exists; there is nothing to select or refill.
        _ => None,
    };
    match outcome.map_err(gateway)? {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied { result } => {
                Ok(("MCP configuration applied".into(), None, server(result)))
            }
            DomainDecision::NoChange { result } => {
                Ok(("MCP configuration unchanged".into(), None, server(result)))
            }
            DomainDecision::Accepted { operation_id } => Ok((
                format!("MCP operation {} accepted", operation_id.0.as_str()),
                Some(operation_id),
                None,
            )),
            DomainDecision::Refused { fault } => Err(Failure::Refused(fault)),
        },
        CommandReply::Rejected { code, .. } => Err(Failure::Rejected(format!("{code:?}"))),
        CommandReply::ReceiptPurged { outcome, .. } => Err(Failure::Purged(outcome)),
        CommandReply::CommandStatusUnknown { .. } => Err(Failure::Unknown),
    }
}

// ---------------------------------------------------------------------------------------------
// Settings (SET-01/02/07/08)

pub fn settings_read(backend: &dyn Backend) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Settings(settings::Input::ReadEffective(
            settings::ReadEffectiveInput {
                scope: DataScope::Workspace,
                repository_id: None,
                keys: BoundedList(Vec::new()),
                page: frame::page(500, None)?,
            },
        )))
    })
}
pub fn container_capabilities(backend: &dyn Backend) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Containers(containers::Input::Capabilities(
            containers::CapabilitiesInput { engine_id: None },
        )))
    })
}
pub fn container_capability_entries(
    outcome: QueryOutcome,
) -> Result<Vec<ContainerCapability>, Failure> {
    match query_result(outcome)? {
        (OperationResult::ContainersCapabilities { value }, _) => Ok(value.items.0),
        (other, _) => Err(unexpected("containers.capabilities", &other)),
    }
}
pub fn settings_entries(outcome: QueryOutcome) -> Result<(i64, Vec<SettingEntry>), Failure> {
    match query_result(outcome)? {
        (OperationResult::SettingsReadEffective { value }, revision) => {
            Ok((revision, value.items.0))
        }
        (other, _) => Err(unexpected("settings.read_effective", &other)),
    }
}
pub fn settings_explain(backend: &dyn Backend, key: &str) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Settings(settings::Input::ExplainSource(
            settings::ExplainSourceInput {
                scope: DataScope::Workspace,
                repository_id: None,
                key: frame::text(key)?,
            },
        )))
    })
}
pub fn settings_chain(outcome: QueryOutcome) -> Result<Vec<SettingEntry>, Failure> {
    match query_result(outcome)? {
        (OperationResult::SettingsExplainSource { value }, _) => Ok(value.items.0),
        (other, _) => Err(unexpected("settings.explain_source", &other)),
    }
}
/// The view's drafts as `settings.validate` (when `validate`) or `settings.save_revision`.
pub fn settings_submit(
    backend: &dyn Backend,
    view: &SettingsView,
    validate: bool,
) -> Option<Vec<u8>> {
    let input = view.save_input()?;
    // A repository scope exists from schema revision 8; earlier revisions refuse the member.
    let negotiate = if input.repository_id.is_some() {
        Negotiate::Latest
    } else {
        Negotiate::Initial
    };
    Some(frame::build(
        backend,
        negotiate,
        Some(In::Settings(if validate {
            settings::Input::Validate(settings::ValidateInput {
                scope: input.scope,
                repository_id: input.repository_id,
                expected_revision: input.expected_revision,
                entries: input.entries,
            })
        } else {
            settings::Input::SaveRevision(input)
        })),
    ))
}
/// `settings.validate` for explicit values (directory health checks without drafts).
pub fn settings_validate_values(
    backend: &dyn Backend,
    revision: i64,
    entries: &[(&str, Value)],
) -> Vec<u8> {
    settings_validate(backend, DataScope::Workspace, revision, entries)
}
/// `settings.validate` of explicit values in `scope`; a key the catalog does not know is left out.
fn settings_validate(
    backend: &dyn Backend,
    scope: DataScope,
    revision: i64,
    entries: &[(&str, Value)],
) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        let entries: Vec<SettingAssignment> = entries
            .iter()
            .filter_map(|(key, value)| {
                let descriptor = devworks_core::settings::catalog::descriptor(key)?;
                Some(SettingAssignment {
                    key: BoundedText((*key).to_owned()),
                    value: SchemaValue {
                        schema: descriptor.schema_ref(),
                        value: DynamicJson(value.clone()),
                    },
                })
            })
            .collect();
        Some(In::Settings(settings::Input::Validate(
            settings::ValidateInput {
                scope,
                repository_id: None,
                expected_revision: frame::number(revision)?,
                entries: frame::bounded(entries)?,
            },
        )))
    })
}
pub fn settings_report(outcome: QueryOutcome) -> Result<ValidationReport, Failure> {
    match query_result(outcome)? {
        (OperationResult::SettingsValidate { value }, _) => Ok(value),
        (other, _) => Err(unexpected("settings.validate", &other)),
    }
}
pub fn settings_saved(outcome: CommandOutcome) -> Result<(), Failure> {
    match command_result(outcome)? {
        OperationResult::SettingsSaveRevision { .. } => Ok(()),
        other => Err(unexpected("settings.save_revision", &other)),
    }
}

// ---------------------------------------------------------------------------------------------
// Sessions (UI-01..04, UI-11..13)

pub fn sessions_list(backend: &dyn Backend, cursor: Option<&str>) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Sessions(sessions::Input::List(sessions::ListInput {
            scope: DataScope::Workspace,
            page: frame::page(PAGE_LIMIT.into(), cursor)?,
            closed: None,
        })))
    })
}
pub fn sessions(outcome: QueryOutcome) -> Result<Page<SessionRecord>, Failure> {
    match query_result(outcome)? {
        (OperationResult::SessionsList { value }, _) => page_of(
            "sessions.list",
            value.items.0,
            value.has_more,
            value.next_cursor,
        ),
        (other, _) => Err(unexpected("sessions.list", &other)),
    }
}
pub fn session_start(backend: &dyn Backend, title: &str) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Sessions(sessions::Input::Start(sessions::StartInput {
            scope: DataScope::Workspace,
            root_id: None,
            title: frame::text(title)?,
        })))
    })
}
pub fn started(outcome: CommandOutcome) -> Result<SessionRecord, Failure> {
    match command_result(outcome)? {
        OperationResult::SessionsStart { value } => Ok(value),
        other => Err(unexpected("sessions.start", &other)),
    }
}
/// One transcript page after `after` (the projection's tail cursor).
pub fn transcript(backend: &dyn Backend, session: &str, after: Option<&str>) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Sessions(sessions::Input::Transcript(
            SessionQueryRequest {
                protocol: frame::protocol(),
                query_id: frame::query_id(backend)?,
                session_id: frame::id(session)?,
                after: after.map(str::to_owned),
                limit: 200,
            },
        )))
    })
}
pub fn transcript_page(outcome: QueryOutcome) -> Result<TranscriptPage, Failure> {
    match query_result(outcome)? {
        (OperationResult::SessionsTranscript { value }, _) => Ok(value),
        (other, _) => Err(unexpected("sessions.transcript", &other)),
    }
}
/// `sessions.append_message`: one user message with its text and stored attachments.
pub fn append_message(
    backend: &dyn Backend,
    session: &str,
    revision: i64,
    text: &str,
    attachments: &[ArtifactRef],
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        let mut content = Vec::new();
        if !text.trim().is_empty() {
            content.push(UserContent::Text {
                text: text.to_owned(),
            });
        }
        content.extend(attachments.iter().map(|artifact| UserContent::Attachment {
            artifact: artifact.clone(),
        }));
        Some(In::Sessions(sessions::Input::AppendMessage(
            SessionCommandRequest {
                protocol: frame::protocol(),
                request_key: frame::request_key(backend)?,
                session_id: frame::id(session)?,
                expected_session_revision: frame::text(&revision.to_string())?,
                content,
            },
        )))
    })
}
pub fn appended(outcome: CommandOutcome) -> Result<SessionRecord, Failure> {
    match command_result(outcome)? {
        OperationResult::SessionsAppendMessage { value } => Ok(value),
        other => Err(unexpected("sessions.append_message", &other)),
    }
}

// ---------------------------------------------------------------------------------------------
// Artifacts (UI-09) and resources

/// The tag that keeps a workspace attachment's conversation provenance (UI-09) until it is
/// associated with work.
pub fn conversation_tag(session: &str) -> Option<String> {
    let tag = format!("conversation:{session}");
    (tag.chars().count() <= devworks_core::protocol::data::MAX_ARTIFACT_TAG_CHARS).then_some(tag)
}

/// `artifacts.save` of an upload ticket into the workspace scope.
pub fn save_workspace_artifact(
    backend: &dyn Backend,
    ticket: &str,
    title: &str,
    media_type: &str,
    digest: &str,
    bytes: u64,
    tags: &[String],
) -> Vec<u8> {
    frame::with(backend, WORKSPACE_ARTIFACT_NEGOTIATION, || {
        artifact_save(
            backend,
            ArtifactCommand::Save {
                project_id: None,
                workspace: Some(true),
                title: title.to_owned(),
                media_type: media_type.to_owned(),
                source: ArtifactSource::Upload {
                    ticket_id: frame::id(ticket)?,
                },
                expected_digest: frame::text(digest)?,
                expected_bytes: frame::text(&bytes.to_string())?,
                tags: if tags.is_empty() {
                    None
                } else {
                    Some(ArtifactTags::new(tags.to_vec()).ok()?)
                },
            },
        )
    })
}
/// The `artifacts.save` input of one save command.
fn artifact_save(backend: &dyn Backend, command: ArtifactCommand) -> Option<In> {
    Some(In::Artifacts(artifacts::Input::Save(
        ArtifactCommandRequest {
            protocol: frame::protocol(),
            request_key: frame::request_key(backend)?,
            command,
        },
    )))
}
/// An `artifacts.save` frame storing an upload into `project` (ticket from
/// `mint_project_upload`).
pub fn save_project_artifact(
    backend: &dyn Backend,
    project: &str,
    ticket: &str,
    title: &str,
    media_type: &str,
    digest: &str,
    bytes: u64,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        artifact_save(
            backend,
            ArtifactCommand::Save {
                project_id: Some(ProjectId(frame::id(project)?)),
                workspace: None,
                title: title.to_owned(),
                media_type: media_type.to_owned(),
                source: ArtifactSource::Upload {
                    ticket_id: frame::id(ticket)?,
                },
                expected_digest: frame::text(digest)?,
                expected_bytes: frame::text(&bytes.to_string())?,
                tags: None,
            },
        )
    })
}
pub fn saved_artifact(outcome: CommandOutcome) -> Result<ArtifactRecord, Failure> {
    match command_result(outcome)? {
        OperationResult::ArtifactsSave { value } => Ok(value),
        other => Err(unexpected("artifacts.save", &other)),
    }
}
/// `artifacts.read` of one version (binary content comes back as a stream handle).
pub fn artifact_read(backend: &dyn Backend, artifact: &ArtifactRef) -> Vec<u8> {
    artifact_read_after(backend, artifact, None)
}
/// [`artifact_read`] continuing at the byte cursor of the previous page.
pub fn artifact_read_after(
    backend: &dyn Backend,
    artifact: &ArtifactRef,
    cursor: Option<&str>,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Artifacts(artifacts::Input::Read(
            ArtifactQueryRequest {
                protocol: frame::protocol(),
                query_id: frame::query_id(backend)?,
                query: ArtifactQuery::Read {
                    artifact_id: artifact.artifact_id.clone(),
                    version_id: artifact.version_id.clone(),
                },
                limit: 1,
                cursor: cursor.map(str::to_owned),
            },
        )))
    })
}
/// The complete bytes of `artifact` through `artifacts.read` (paged by byte cursor, or a
/// verified stream for non-text bytes), checked against the reference's length and digest.
pub fn read_artifact_bytes(b: &dyn Backend, artifact: &ArtifactRef) -> Result<Vec<u8>, String> {
    use sha2::{Digest, Sha256};
    let expected = artifact.byte_length.get() as u64;
    let mut bytes: Vec<u8> = Vec::with_capacity(expected as usize);
    let mut cursor: Option<String> = None;
    loop {
        let frame = artifact_read_after(b, artifact, cursor.as_deref());
        match read_content(b.query("artifacts.read", &frame)).map_err(|e| e.describe())? {
            devworks_core::protocol::records::ArtifactReadContent::Text {
                content,
                truncated,
                next_byte,
                ..
            } => {
                bytes.extend_from_slice(content.0.as_bytes());
                if bytes.len() as u64 > expected {
                    return Err("The text exceeded its recorded length.".into());
                }
                if !truncated {
                    break;
                }
                let next = next_byte.get().to_string();
                if cursor.as_deref() == Some(next.as_str()) {
                    return Err("The text read did not advance.".into());
                }
                cursor = Some(next);
            }
            devworks_core::protocol::records::ArtifactReadContent::Binary { stream_id, .. } => {
                bytes = b.read_stream(&stream_id, expected)?;
                break;
            }
        }
    }
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if bytes.len() as u64 != expected || digest != artifact.sha256.as_str() {
        return Err("The text did not match its recorded length and digest.".into());
    }
    Ok(bytes)
}
pub fn read_content(
    outcome: QueryOutcome,
) -> Result<devworks_core::protocol::records::ArtifactReadContent, Failure> {
    match query_result(outcome)? {
        (OperationResult::ArtifactsRead { value }, _) => Ok(value),
        (other, _) => Err(unexpected("artifacts.read", &other)),
    }
}
/// `artifacts.delete` of the one version of an attachment saved for a draft and removed from it
/// before it was ever sent (revision and reference revision 1: nothing referenced it).
pub fn delete_unsent_attachment(backend: &dyn Backend, artifact: &ArtifactRef) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Artifacts(artifacts::Input::Delete(
            ArtifactDeletionRequest {
                protocol: frame::protocol(),
                request_key: frame::request_key(backend)?,
                deletion: ArtifactDeletion::DeleteVersion {
                    artifact_id: artifact.artifact_id.clone(),
                    version_id: artifact.version_id.clone(),
                    expected_revision: frame::text("1")?,
                    expected_reference_revision: frame::text("1")?,
                },
            },
        )))
    })
}
pub fn deleted(outcome: CommandOutcome) -> Result<(), Failure> {
    match command_result(outcome)? {
        OperationResult::ArtifactsDelete { .. } => Ok(()),
        other => Err(unexpected("artifacts.delete", &other)),
    }
}
pub fn workspace_artifacts(backend: &dyn Backend, cursor: Option<&str>) -> Vec<u8> {
    artifact_list(backend, None, true, cursor)
}
/// One page of `artifacts.list` of the workspace (`workspace`) or of `project`.
fn artifact_list(
    backend: &dyn Backend,
    project: Option<&str>,
    workspace: bool,
    cursor: Option<&str>,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Artifacts(artifacts::Input::List(
            ArtifactQueryRequest {
                protocol: frame::protocol(),
                query_id: frame::query_id(backend)?,
                query: ArtifactQuery::List {
                    project_id: frame::lift(project.map(|p| frame::id(p).map(ProjectId)))?,
                    workspace: workspace.then_some(true),
                    tags: None,
                },
                limit: PAGE_LIMIT,
                cursor: cursor.map(str::to_owned),
            },
        )))
    })
}
/// `artifacts.list` of one project's artifacts.
pub fn project_artifacts(backend: &dyn Backend, project: &str, cursor: Option<&str>) -> Vec<u8> {
    artifact_list(backend, Some(project), false, cursor)
}
/// A bounded prompt-picker page, independent of the artifacts screen's current page.
pub fn prompt_artifacts(
    backend: &dyn Backend,
    project: Option<&str>,
    cursor: Option<&str>,
) -> Vec<u8> {
    artifact_list(backend, project, project.is_none(), cursor)
}
pub fn prompt_artifact_page(
    outcome: QueryOutcome,
) -> Result<(Vec<ArtifactRecord>, Option<String>), Failure> {
    match query_result(outcome)? {
        (OperationResult::ArtifactsList { value }, _) => Ok((
            value.items.0,
            value.next_cursor.map(|c| c.0).filter(|_| value.has_more),
        )),
        (other, _) => Err(unexpected("artifacts.list", &other)),
    }
}
pub fn artifacts(outcome: QueryOutcome) -> Result<Page<ArtifactRecord>, Failure> {
    match query_result(outcome)? {
        (OperationResult::ArtifactsList { value }, _) => page_of(
            "artifacts.list",
            value.items.0,
            value.has_more,
            value.next_cursor,
        ),
        (other, _) => Err(unexpected("artifacts.list", &other)),
    }
}
pub fn resources_list(backend: &dyn Backend, cursor: Option<&str>) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Resources(resources::Input::List(
            resources::ListInput {
                scope: DataScope::Workspace,
                page: frame::page(PAGE_LIMIT.into(), cursor)?,
            },
        )))
    })
}
/// The owned browser sessions (`browser.sessions`) of `project`, or of the workspace.
pub fn browser_sessions_list(
    backend: &dyn Backend,
    project: Option<&str>,
    cursor: Option<&str>,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Browser(browser::Input::SessionsQuery(
            browser::SessionsQueryInput {
                scope: frame::scope(project)?,
                page: frame::page(PAGE_LIMIT.into(), cursor)?,
            },
        )))
    })
}
pub fn browser_sessions(outcome: QueryOutcome) -> Result<Page<BrowserSession>, Failure> {
    match query_result(outcome)? {
        (OperationResult::BrowserSessions { value }, _) => page_of(
            "browser.sessions",
            value.items.0,
            value.has_more,
            value.next_cursor,
        ),
        (other, _) => Err(unexpected("browser.sessions", &other)),
    }
}
/// Closes (interrupts) a browser session at its current revision.
pub fn browser_close(backend: &dyn Backend, session: &str, revision: &str) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Browser(browser::Input::SessionsCommand(
            browser::SessionsCommandInput {
                action: BrowserSessionAction::Close {
                    session_id: frame::id(session)?,
                    expected_revision: frame::text(revision)?,
                },
            },
        )))
    })
}
/// `true` when the close was accepted (the browser is being stopped through the process
/// owner), `false` when the session had already stopped.
pub fn browser_closed(outcome: CommandOutcome) -> Result<bool, Failure> {
    match outcome.map_err(gateway)? {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { .. } => Ok(true),
            DomainDecision::Applied { .. } | DomainDecision::NoChange { .. } => Ok(false),
            DomainDecision::Refused { fault } => Err(Failure::Refused(fault)),
        },
        CommandReply::Rejected { code, .. } => Err(Failure::Rejected(format!("{code:?}"))),
        CommandReply::ReceiptPurged { outcome, .. } => Err(Failure::Purged(outcome)),
        CommandReply::CommandStatusUnknown { .. } => Err(Failure::Unknown),
    }
}
pub fn resources(outcome: QueryOutcome) -> Result<Page<ResourceRecord>, Failure> {
    match query_result(outcome)? {
        (OperationResult::ResourcesList { value }, _) => page_of(
            "resources.list",
            value.items.0,
            value.has_more,
            value.next_cursor,
        ),
        (other, _) => Err(unexpected("resources.list", &other)),
    }
}

// ---------------------------------------------------------------------------------------------
// Router (UI-10, ROUTE-19, ROUTE-20)

/// One page of the catalog, after `cursor` (the first page without one).
pub fn router_catalog(backend: &dyn Backend, cursor: Option<&str>) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Router(router::Input::Catalog(router::CatalogInput {
            page: frame::page(PAGE_LIMIT.into(), cursor)?,
            include_disabled: true,
        })))
    })
}
/// A catalog page and the cursor of the next one (`None` on the last page).
pub fn route_models(
    outcome: QueryOutcome,
) -> Result<(Vec<RouteCatalogModel>, Option<String>), Failure> {
    match query_result(outcome)? {
        (OperationResult::RouterCatalog { value }, _) => {
            let next = value.next_cursor.map(|c| c.0).filter(|_| value.has_more);
            if value.has_more && next.is_none() {
                return Err(Failure::Other(
                    "router.catalog reported more pages without a cursor".into(),
                ));
            }
            Ok((value.items.0, next))
        }
        (other, _) => Err(unexpected("router.catalog", &other)),
    }
}
pub fn session_get(backend: &dyn Backend, session: &str) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Sessions(sessions::Input::Get(sessions::GetInput {
            session_id: frame::id(session)?,
        })))
    })
}
pub fn session_record(outcome: QueryOutcome) -> Result<SessionRecord, Failure> {
    match query_result(outcome)? {
        (OperationResult::SessionsGet { value }, _) => Ok(value),
        (other, _) => Err(unexpected("sessions.get", &other)),
    }
}
pub fn router_availability(
    backend: &dyn Backend,
    intent: &RouteIntent,
    effort: &EffortName,
    session: Option<&str>,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Router(router::Input::Availability(
            router::AvailabilityInput {
                intent: intent.clone(),
                effort: effort.clone(),
                required_capabilities: BoundedList(Vec::new()),
                session_id: frame::lift(session.map(frame::id))?,
            },
        )))
    })
}
pub fn availability(outcome: QueryOutcome) -> Result<RouteAvailability, Failure> {
    match query_result(outcome)? {
        (OperationResult::RouterAvailability { value }, _) => Ok(value),
        (other, _) => Err(unexpected("router.availability", &other)),
    }
}
pub fn router_pin(
    backend: &dyn Backend,
    session: &str,
    expected_pin_revision: i64,
    intent: &RouteIntent,
    effort: &EffortName,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Router(router::Input::Pin(router::PinInput {
            session_id: frame::id(session)?,
            expected_pin_revision: frame::number(expected_pin_revision)?,
            intent: intent.clone(),
            effort: effort.clone(),
        })))
    })
}
pub fn router_route(
    backend: &dyn Backend,
    session: &str,
    context_epoch: i64,
    intent: &RouteIntent,
    effort: &EffortName,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Router(router::Input::Route(router::RouteInput {
            session_id: frame::id(session)?,
            expected_context_epoch: frame::number(context_epoch)?,
            intent: intent.clone(),
            effort: effort.clone(),
            required_capabilities: BoundedList(Vec::new()),
        })))
    })
}
pub fn routed(outcome: CommandOutcome) -> Result<RouteChoice, Failure> {
    match command_result(outcome)? {
        OperationResult::RouterRoute { value } => Ok(value),
        other => Err(unexpected("router.route", &other)),
    }
}
pub fn pinned(outcome: CommandOutcome) -> Result<RouteChoice, Failure> {
    match command_result(outcome)? {
        OperationResult::RouterPin { value } => Ok(value),
        other => Err(unexpected("router.pin", &other)),
    }
}

// ---------------------------------------------------------------------------------------------
// Web search and fetch (DW-51)

/// The most a page fetched from the UI keeps (the contract's bound).
pub const WEB_FETCH_BYTES: u32 = 1_048_576;
/// The time the UI asks for; the `web.timeoutMs` setting caps it.
pub const WEB_FETCH_TIMEOUT_MS: u64 = 30_000;
/// Results one search page asks for.
pub const WEB_SEARCH_LIMIT: u16 = 10;
/// Recent web activity records shown.
pub const WEB_ACTIVITY_LIMIT: u16 = 20;

/// `web.fetch` of `url` into the workspace, as the user.
pub fn web_fetch(backend: &dyn Backend, url: &str) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Web(web::Input::Fetch(web::FetchInput {
            scope: DataScope::Workspace,
            url: frame::text(url)?,
            max_bytes: frame::integer(WEB_FETCH_BYTES.into())?,
            timeout_ms: frame::number(WEB_FETCH_TIMEOUT_MS as i64)?,
        })))
    })
}
pub fn web_document(outcome: QueryOutcome) -> Result<WebDocument, Failure> {
    match query_result(outcome)? {
        (OperationResult::WebFetch { value }, _) => Ok(value),
        (other, _) => Err(unexpected("web.fetch", &other)),
    }
}
/// `web.search` of `query` with the configured `provider`, as the user.
pub fn web_search(backend: &dyn Backend, query: &str, provider: &str) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Web(web::Input::Search(web::SearchInput {
            scope: DataScope::Workspace,
            query: frame::text(query)?,
            provider_id: frame::id(provider)?,
            page: frame::page(WEB_SEARCH_LIMIT.into(), None)?,
        })))
    })
}
pub fn web_results(outcome: QueryOutcome) -> Result<Vec<WebResult>, Failure> {
    match query_result(outcome)? {
        (OperationResult::WebSearch { value }, _) => Ok(value.items.0),
        (other, _) => Err(unexpected("web.search", &other)),
    }
}
/// The newest web activity records (`workspace.logs` of the `web` components).
pub fn web_activity(backend: &dyn Backend) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Workspace(workspace::Input::Logs(
            workspace::LogsInput {
                page: frame::page(WEB_ACTIVITY_LIMIT.into(), None)?,
                component: Some(frame::text("web")?),
                after_sequence: frame::number(0)?,
                severity: None,
                outcome: None,
                operation: None,
                operation_id: None,
                correlation_id: None,
                target_id: None,
                stage: None,
                project_id: None,
                search: None,
                since_ms: None,
                until_ms: None,
                order: Some(SortDirection::Descending),
            },
        )))
    })
}
pub fn web_entries(outcome: QueryOutcome) -> Result<Vec<DiagnosticEntry>, Failure> {
    match query_result(outcome)? {
        (OperationResult::WorkspaceLogs { value }, _) => Ok(value.items.0),
        (other, _) => Err(unexpected("workspace.logs", &other)),
    }
}

// ---------------------------------------------------------------------------------------------
// Projects and repository registrations (DW-11)

/// The lifecycle a projects listing shows (`None`: active and archived).
pub fn projects_list(
    backend: &dyn Backend,
    lifecycle: Option<&str>,
    cursor: Option<&str>,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Projects(projects::Input::List(projects::ListInput {
            scope: DataScope::Workspace,
            page: frame::page(PAGE_LIMIT.into(), cursor)?,
            lifecycle: frame::lift(lifecycle.map(frame::text))?,
        })))
    })
}
pub fn projects(outcome: QueryOutcome) -> Result<Page<ProjectRecord>, Failure> {
    match query_result(outcome)? {
        (OperationResult::ProjectsList { value }, _) => page_of(
            "projects.list",
            value.items.0,
            value.has_more,
            value.next_cursor,
        ),
        (other, _) => Err(unexpected("projects.list", &other)),
    }
}
pub fn project_create(
    backend: &dyn Backend,
    name: &str,
    description: &str,
    repositories: &[String],
) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Projects(projects::Input::Create(
            projects::CreateInput {
                name: frame::text(name)?,
                description: frame::text(description)?,
                repository_ids: frame::ids(repositories)?,
                initial_plan: None,
            },
        )))
    })
}
/// `projects.edit` of the changed members only (an unchanged member is left out).
pub fn project_edit(
    backend: &dyn Backend,
    project: &ProjectRecord,
    name: Option<&str>,
    description: Option<&str>,
    access_mode: Option<&AccessMode>,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Projects(projects::Input::Edit(projects::EditInput {
            project_id: project.id.clone(),
            expected_revision: project.revision.clone(),
            changes: ProjectPatch {
                name: frame::lift(name.map(frame::text))?,
                description: frame::lift(description.map(frame::text))?,
                access_mode: access_mode.cloned(),
            },
        })))
    })
}
pub fn project_membership(
    backend: &dyn Backend,
    project: &ProjectRecord,
    repositories: &[String],
) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Projects(projects::Input::Membership(
            projects::MembershipInput {
                project_id: project.id.clone(),
                expected_revision: project.revision.clone(),
                repository_ids: frame::ids(repositories)?,
            },
        )))
    })
}
/// A project lifecycle change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleChange {
    Archive,
    Restore,
    Delete,
}
impl LifecycleChange {
    /// The operation the change runs.
    pub const fn operation(self) -> &'static str {
        match self {
            Self::Archive => "projects.archive",
            Self::Restore => "projects.restore",
            Self::Delete => "projects.delete",
        }
    }
}
/// `projects.archive`, `.restore` or `.delete` of `project` at its revision.
pub fn project_lifecycle(
    backend: &dyn Backend,
    change: LifecycleChange,
    project: &ProjectRecord,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        let (project_id, expected_revision) = (project.id.clone(), project.revision.clone());
        Some(In::Projects(match change {
            LifecycleChange::Archive => projects::Input::Archive(projects::ArchiveInput {
                project_id,
                expected_revision,
                proposal_id: None,
            }),
            LifecycleChange::Restore => projects::Input::Restore(projects::RestoreInput {
                project_id,
                expected_revision,
                proposal_id: None,
            }),
            LifecycleChange::Delete => projects::Input::Delete(projects::DeleteInput {
                project_id,
                expected_revision,
                proposal_id: None,
            }),
        }))
    })
}
/// `projects.prepare_purge` of a project at its revision.
pub fn purge_prepare(b: &dyn Backend, project: &ProjectRecord) -> Vec<u8> {
    frame::with(b, Negotiate::Latest, || {
        Some(In::Projects(projects::Input::PreparePurge(
            projects::PreparePurgeInput {
                project_id: project.id.clone(),
                expected_revision: project.revision.clone(),
            },
        )))
    })
}
/// `projects.purge_status` of a prepared manifest.
pub fn purge_status(b: &dyn Backend, m: &ProjectPurgeManifest) -> Vec<u8> {
    frame::with(b, Negotiate::Latest, || {
        Some(In::Projects(projects::Input::PurgeStatus(
            projects::PurgeStatusInput {
                project_id: m.project_id.clone(),
                manifest_id: m.id.clone(),
            },
        )))
    })
}
/// `projects.purge_manifest`: one page of a manifest's entries.
pub fn purge_manifest_page(
    b: &dyn Backend,
    m: &ProjectPurgeManifest,
    cursor: Option<&str>,
) -> Vec<u8> {
    frame::with(b, Negotiate::Latest, || {
        Some(In::Projects(projects::Input::PurgeManifest(
            projects::PurgeManifestInput {
                project_id: m.project_id.clone(),
                manifest_id: m.id.clone(),
                expected_revision: m.revision.clone(),
                page: frame::page(200, cursor)?,
            },
        )))
    })
}
/// `projects.purge` of the reviewed manifest.
pub fn purge_confirm(b: &dyn Backend, m: &ProjectPurgeManifest) -> Vec<u8> {
    frame::with(b, Negotiate::Latest, || {
        Some(In::Projects(projects::Input::Purge(projects::PurgeInput {
            project_id: m.project_id.clone(),
            manifest_id: m.id.clone(),
            expected_revision: m.revision.clone(),
            expected_digest: m.digest.clone(),
            expected_project_revision: m.project_revision.clone(),
            confirm: true,
        })))
    })
}
/// The project a projects command applied (or left unchanged).
pub fn project_changed(outcome: CommandOutcome) -> Result<ProjectRecord, Failure> {
    match command_result(outcome)? {
        OperationResult::ProjectsCreate { value }
        | OperationResult::ProjectsEdit { value }
        | OperationResult::ProjectsMembership { value }
        | OperationResult::ProjectsArchive { value }
        | OperationResult::ProjectsRestore { value }
        | OperationResult::ProjectsDelete { value } => Ok(value),
        other => Err(unexpected("projects", &other)),
    }
}
/// Registered repositories: a project's members, or every registration (`None`).
pub fn repositories_list(
    backend: &dyn Backend,
    project: Option<&str>,
    cursor: Option<&str>,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Repositories(repositories::Input::List(
            repositories::ListInput {
                scope: frame::scope(project)?,
                page: frame::page(PAGE_LIMIT.into(), cursor)?,
                text: None,
            },
        )))
    })
}
pub fn repositories(outcome: QueryOutcome) -> Result<Page<RepositoryRecord>, Failure> {
    match query_result(outcome)? {
        (OperationResult::RepositoriesList { value }, _) => page_of(
            "repositories.list",
            value.items.0,
            value.has_more,
            value.next_cursor,
        ),
        (other, _) => Err(unexpected("repositories.list", &other)),
    }
}
/// `repositories.readme` (or `.summary`) of the repository's latest published view.
pub fn repository_text(backend: &dyn Backend, repository: &str, readme: bool) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        let repository_id = frame::id(repository)?;
        let views = ViewSelection::Published {
            repository_ids: vec![repository_id.clone()],
        };
        Some(In::Repositories(if readme {
            repositories::Input::Readme(repositories::ReadmeInput {
                repository_id,
                views,
            })
        } else {
            repositories::Input::Summary(repositories::SummaryInput {
                repository_id,
                views,
            })
        }))
    })
}
pub fn repository_text_result(outcome: QueryOutcome) -> Result<RepositoryText, Failure> {
    match query_result(outcome)? {
        (OperationResult::RepositoriesReadme { value }, _)
        | (OperationResult::RepositoriesSummary { value }, _) => Ok(value),
        (other, _) => Err(unexpected("repositories.readme", &other)),
    }
}
pub fn remove_index(backend: &dyn Backend, repository: &RepositoryRecord) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Repositories(repositories::Input::RemoveIndex(
            repositories::RemoveIndexInput {
                repository_id: repository.id.clone(),
                expected_revision: repository.revision.clone(),
            },
        )))
    })
}
pub fn index_removed(outcome: CommandOutcome) -> Result<MutationSummary, Failure> {
    match command_result(outcome)? {
        OperationResult::RepositoriesRemoveIndex { value } => Ok(value),
        other => Err(unexpected("repositories.remove_index", &other)),
    }
}
/// `settings.read_effective` of `keys` at a project's scope (its overrides and inheritance).
pub fn project_settings_read(backend: &dyn Backend, project: &str, keys: &[&str]) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Settings(settings::Input::ReadEffective(
            settings::ReadEffectiveInput {
                scope: frame::scope(Some(project))?,
                repository_id: None,
                keys: BoundedList(
                    keys.iter()
                        .map(|key| frame::text(key))
                        .collect::<Option<Vec<_>>>()?,
                ),
                page: frame::page(500, None)?,
            },
        )))
    })
}
/// `settings.read_effective` of `keys` at one repository of a project: the repository's own
/// values over the project's, over the workspace's, each entry naming the layer it came from.
/// A repository scope exists from schema revision 8.
pub fn repository_settings_read(
    backend: &dyn Backend,
    project: &str,
    repository: &str,
    keys: &[&str],
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Settings(settings::Input::ReadEffective(
            settings::ReadEffectiveInput {
                scope: frame::scope(Some(project))?,
                repository_id: Some(frame::id(repository)?),
                keys: BoundedList(
                    keys.iter()
                        .map(|key| frame::text(key))
                        .collect::<Option<Vec<_>>>()?,
                ),
                page: frame::page(500, None)?,
            },
        )))
    })
}
/// `settings.validate` of explicit values at a project's scope (directory health).
pub fn project_settings_validate(
    backend: &dyn Backend,
    project: &str,
    revision: i64,
    entries: &[(&str, Value)],
) -> Vec<u8> {
    match frame::scope(Some(project)) {
        Some(scope) => settings_validate(backend, scope, revision, entries),
        None => Vec::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// Repository indexing (DW-23)

/// `indexing.status` of one repository at schema revision 2 (freshness, languages, packs,
/// overlays).
pub fn index_status(backend: &dyn Backend, repository: &str) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Indexing(indexing::Input::Status(
            indexing::StatusInput {
                repository_id: Some(frame::id(repository)?),
                overlay_id: None,
            },
        )))
    })
}
pub fn index_status_result(
    outcome: QueryOutcome,
) -> Result<devworks_core::protocol::records::RepositoryIndexStatus, Failure> {
    match query_result(outcome)? {
        (OperationResult::IndexingStatus { value }, _) => Ok(value),
        (other, _) => Err(unexpected("indexing.status", &other)),
    }
}
/// The newest index runs of the workspace or a project (one repository, when named).
pub fn index_runs(
    backend: &dyn Backend,
    project: Option<&str>,
    repository: Option<&str>,
    cursor: Option<&str>,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Indexing(indexing::Input::Runs(indexing::RunsInput {
            scope: frame::scope(project)?,
            repository_id: frame::lift(repository.map(frame::id))?,
            states: None,
            kinds: None,
            page: frame::page(50, cursor)?,
        })))
    })
}
pub fn index_runs_result(
    outcome: QueryOutcome,
) -> Result<Page<devworks_core::protocol::records::IndexRunRecord>, Failure> {
    match query_result(outcome)? {
        (OperationResult::IndexingRuns { value }, _) => page_of(
            "indexing.runs",
            value.items.0,
            value.has_more,
            value.next_cursor,
        ),
        (other, _) => Err(unexpected("indexing.runs", &other)),
    }
}
/// `indexing.index` (or `.reindex`, a manual rebuild) of one repository's baseline under the
/// section configuration revision the caller saw.
pub fn index_repository(
    backend: &dyn Backend,
    repository: &str,
    rebuild: bool,
    section_revision: i64,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        let repository_id = frame::id(repository)?;
        let repository_ids = BoundedList(vec![repository_id.clone()]);
        let expected_source_revision = frame::number(section_revision.max(1))?;
        let views = ViewSelection::Published {
            repository_ids: vec![repository_id],
        };
        Some(In::Indexing(if rebuild {
            indexing::Input::Reindex(indexing::ReindexInput {
                scope: DataScope::Workspace,
                repository_ids,
                expected_source_revision,
                views,
            })
        } else {
            indexing::Input::Index(indexing::IndexInput {
                scope: DataScope::Workspace,
                repository_ids,
                expected_source_revision,
                views,
            })
        }))
    })
}
/// `indexing.cancel` of a run (or of every run of the request batch it opened).
pub fn cancel_index_run(backend: &dyn Backend, run: &str) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        index_command(
            backend,
            IndexCommand::Cancel {
                operation_id: OperationId(frame::id(run)?),
            },
        )
    })
}
/// The `indexing.cancel` or `indexing.refresh_overlay` input of one index command.
fn index_command(backend: &dyn Backend, command: IndexCommand) -> Option<In> {
    let request = IndexCommandRequest {
        protocol: frame::protocol(),
        request_key: frame::request_key(backend)?,
        command,
    };
    Some(In::Indexing(match request.command {
        IndexCommand::Cancel { .. } => indexing::Input::Cancel(request),
        _ => indexing::Input::RefreshOverlay(request),
    }))
}
/// `indexing.refresh_overlay` of one worktree.
pub fn refresh_overlay(
    backend: &dyn Backend,
    repository: &str,
    worktree: &str,
    section_revision: i64,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        index_command(
            backend,
            IndexCommand::RefreshOverlay {
                repository_id: frame::id(repository)?,
                worktree_id: Some(frame::id(worktree)?),
                git_ref: None,
                expected_source_revision: frame::number(section_revision.max(1))?,
            },
        )
    })
}
/// `indexing.retire_view` of an overlay at its revision.
pub fn retire_view(backend: &dyn Backend, view: &str, revision: &str) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Indexing(indexing::Input::RetireView(
            indexing::RetireViewInput {
                view_id: frame::id(view)?,
                expected_revision: frame::text(revision)?,
            },
        )))
    })
}
pub fn index_run_result(
    outcome: CommandOutcome,
) -> Result<devworks_core::protocol::records::IndexRunRecord, Failure> {
    match command_result(outcome)? {
        OperationResult::IndexingIndex { value }
        | OperationResult::IndexingReindex { value }
        | OperationResult::IndexingCancel { value }
        | OperationResult::IndexingRefreshOverlay { value } => Ok(value),
        other => Err(unexpected("indexing", &other)),
    }
}
pub fn index_mutation_result(outcome: CommandOutcome) -> Result<MutationSummary, Failure> {
    match command_result(outcome)? {
        OperationResult::IndexingConfigureSources { value }
        | OperationResult::IndexingRetireView { value } => Ok(value),
        other => Err(unexpected("indexing", &other)),
    }
}
/// `indexing.configure_sources` of the workspace (or a project) section.
#[allow(clippy::too_many_arguments)]
pub fn configure_sources(
    backend: &dyn Backend,
    project: Option<&str>,
    expected_revision: i64,
    languages: &[String],
    packages: &[String],
    excludes: &[String],
    max_file_bytes: i64,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Indexing(indexing::Input::ConfigureSources(
            indexing::ConfigureSourcesInput {
                scope: frame::scope(project)?,
                expected_revision: frame::number(expected_revision.max(1))?,
                config: IndexSourceConfig {
                    structural_languages: frame::texts(languages)?,
                    semantic_package_ids: frame::ids(packages)?,
                    exclude_patterns: frame::texts(excludes)?,
                    max_file_bytes: frame::number(max_file_bytes.max(1))?,
                },
            },
        )))
    })
}
/// `indexing.discover` of every visible section (the runtime scans before answering).
pub fn discover(backend: &dyn Backend) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Indexing(indexing::Input::Discover(
            indexing::DiscoverInput {
                scope: DataScope::Workspace,
                roots: BoundedList(Vec::new()),
                page: frame::page(PAGE_LIMIT.into(), None)?,
            },
        )))
    })
}
pub fn discover_result(outcome: QueryOutcome) -> Result<(Vec<RepositoryRecord>, bool), Failure> {
    match query_result(outcome)? {
        (OperationResult::IndexingDiscover { value }, _) => Ok((value.items.0, value.has_more)),
        (other, _) => Err(unexpected("indexing.discover", &other)),
    }
}

// ---------------------------------------------------------------------------------------------
// Scheduled jobs (JOB-01/02)

/// Runs one history page of the Scheduled jobs screen shows.
pub const RUN_HISTORY_PAGE: u16 = 25;

/// The jobs of one scope (app-level for `None`), at the jobs revision.
pub fn jobs_list(backend: &dyn Backend, project: Option<&str>) -> Vec<u8> {
    jobs_list_after(backend, project, None)
}
/// The page of a scope's jobs after `cursor` (the first page without one).
pub fn jobs_list_after(
    backend: &dyn Backend,
    project: Option<&str>,
    cursor: Option<&str>,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Jobs(jobs::Input::List(jobs::ListInput {
            scope: frame::scope(project)?,
            page: frame::page(PAGE_LIMIT.into(), cursor)?,
            enabled: None,
        })))
    })
}
/// A page of jobs with the cursor of the next one.
pub fn jobs_page(outcome: QueryOutcome) -> Result<Page<JobRecord>, Failure> {
    match query_result(outcome)? {
        (OperationResult::JobsList { value }, _) => page_of(
            "jobs.list",
            value.items.0,
            value.has_more,
            value.next_cursor,
        ),
        (other, _) => Err(unexpected("jobs.list", &other)),
    }
}
pub fn jobs(outcome: QueryOutcome) -> Result<(Vec<JobRecord>, bool), Failure> {
    match query_result(outcome)? {
        (OperationResult::JobsList { value }, _) => Ok((value.items.0, value.has_more)),
        (other, _) => Err(unexpected("jobs.list", &other)),
    }
}
/// A job with its latest run and a page of its run history (after `cursor`).
pub fn jobs_inspect(backend: &dyn Backend, job: &str, cursor: Option<&str>) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Jobs(jobs::Input::Inspect(jobs::InspectInput {
            job_id: frame::id(job)?,
            run_id: None,
            history: Some(frame::page(RUN_HISTORY_PAGE.into(), cursor)?),
            usage: None,
        })))
    })
}
/// `jobs.inspect` of one named run of `job` (no history page).
pub fn jobs_inspect_run(backend: &dyn Backend, job: &str, run: &str) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Jobs(jobs::Input::Inspect(jobs::InspectInput {
            job_id: frame::id(job)?,
            run_id: Some(frame::id(run)?),
            history: None,
            usage: None,
        })))
    })
}
pub fn job_inspection(outcome: QueryOutcome) -> Result<JobInspection, Failure> {
    match query_result(outcome)? {
        (OperationResult::JobsInspect { value }, _) => Ok(value),
        (other, _) => Err(unexpected("jobs.inspect", &other)),
    }
}
/// `jobs.create` of a new job.
pub fn job_create(
    backend: &dyn Backend,
    project: Option<&str>,
    title: &str,
    schedule: &Schedule,
    definition: &JobDefinition,
    enabled: bool,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Jobs(jobs::Input::Create(jobs::CreateInput {
            scope: frame::scope(project)?,
            title: frame::text(title)?,
            schedule: schedule.clone(),
            definition: definition.clone(),
            enabled,
        })))
    })
}
/// `jobs.edit` at the record's revision (a concurrent change is a revision conflict).
pub fn job_edit(
    backend: &dyn Backend,
    job: &JobRecord,
    title: &str,
    schedule: &Schedule,
    definition: &JobDefinition,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Jobs(jobs::Input::Edit(jobs::EditInput {
            job_id: job.id.clone(),
            expected_revision: job.revision.clone(),
            title: frame::text(title)?,
            schedule: schedule.clone(),
            definition: definition.clone(),
        })))
    })
}
/// A command on one job that takes only the job and its revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobCommand {
    Enable,
    Disable,
    Delete,
    RunNow,
}
/// `jobs.enable`, `.disable`, `.delete` or `.run_now` of a job at its record's revision.
pub fn job_command(backend: &dyn Backend, command: JobCommand, job: &JobRecord) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        let (job_id, expected_revision) = (job.id.clone(), job.revision.clone());
        Some(In::Jobs(match command {
            JobCommand::Enable => jobs::Input::Enable(jobs::EnableInput {
                job_id,
                expected_revision,
            }),
            JobCommand::Disable => jobs::Input::Disable(jobs::DisableInput {
                job_id,
                expected_revision,
            }),
            JobCommand::Delete => jobs::Input::Delete(jobs::DeleteInput {
                job_id,
                expected_revision,
            }),
            JobCommand::RunNow => jobs::Input::RunNow(jobs::RunNowInput {
                job_id,
                expected_revision,
            }),
        }))
    })
}
/// `jobs.cancel` of one run at the run's revision.
pub fn job_cancel(backend: &dyn Backend, run: &JobRun) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Jobs(jobs::Input::Cancel(jobs::CancelInput {
            job_id: run.job_id.clone(),
            run_id: run.id.clone(),
            expected_revision: run.revision.clone(),
        })))
    })
}
/// What a jobs command did, or that a cancellation was accepted (it settles asynchronously).
pub enum JobChange {
    Job(JobRecord),
    Run(JobRun),
    /// A dispatched run was asked to stop; its outcome is recorded when known.
    CancelAccepted,
}
pub fn job_changed(outcome: CommandOutcome) -> Result<JobChange, Failure> {
    if let Ok(CommandReply::Recorded { receipt, .. }) = &outcome
        && let DomainDecision::Accepted { .. } = receipt.decision
    {
        return Ok(JobChange::CancelAccepted);
    }
    match command_result(outcome)? {
        OperationResult::JobsCreate { value }
        | OperationResult::JobsEdit { value }
        | OperationResult::JobsEnable { value }
        | OperationResult::JobsDisable { value }
        | OperationResult::JobsDelete { value } => Ok(JobChange::Job(value)),
        OperationResult::JobsRunNow { value } | OperationResult::JobsCancel { value } => {
            Ok(JobChange::Run(value))
        }
        other => Err(unexpected("jobs", &other)),
    }
}

// ---------------------------------------------------------------------------------------------
// Provider connections (DW-18)

pub fn router_connections(backend: &dyn Backend, cursor: Option<&str>) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Router(router::Input::Connections(
            router::ConnectionsInput {
                page: frame::page(PAGE_LIMIT.into(), cursor)?,
            },
        )))
    })
}
/// A page of connections and the cursor of the next one.
pub fn connection_records(
    outcome: QueryOutcome,
) -> Result<(Vec<ConnectionRecord>, Option<String>), Failure> {
    match query_result(outcome)? {
        (OperationResult::RouterConnections { value }, _) => {
            let next = value.next_cursor.map(|c| c.0).filter(|_| value.has_more);
            if value.has_more && next.is_none() {
                return Err(Failure::Other(
                    "router.connections reported more pages without a cursor".into(),
                ));
            }
            Ok((value.items.0, next))
        }
        (other, _) => Err(unexpected("router.connections", &other)),
    }
}
/// The whole query catalog, including availability and logical family targets (revision 3).
pub fn router_embedding_spaces(backend: &dyn Backend, cursor: Option<&str>) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Router(router::Input::EmbeddingSpaces(
            router::EmbeddingSpacesInput {
                page: frame::page(100, cursor)?,
            },
        )))
    })
}
pub fn embedding_space_page(
    outcome: QueryOutcome,
) -> Result<devworks_core::protocol::records::EmbeddingSpacePage, Failure> {
    match query_result(outcome)? {
        (OperationResult::RouterEmbeddingSpaces { value }, _) => {
            if value.has_more && value.next_cursor.is_none() {
                return Err(Failure::Other(
                    "router.embedding_spaces reported more pages without a cursor".into(),
                ));
            }
            Ok(value)
        }
        (other, _) => Err(unexpected("router.embedding_spaces", &other)),
    }
}
pub fn router_catalog_all(backend: &dyn Backend, cursor: Option<&str>) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || router_catalog_input(cursor))
}
pub fn router_decision_models(backend: &dyn Backend, cursor: Option<&str>) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Router(router::Input::DecisionModels(
            router::DecisionModelsInput {
                page: frame::page(100, cursor)?,
                include_disabled: true,
            },
        )))
    })
}

pub fn router_decide(backend: &dyn Backend, input: router::DecideInput) -> Vec<u8> {
    frame::build(
        backend,
        Negotiate::Latest,
        Some(In::Router(router::Input::Decide(input))),
    )
}

pub fn accepted_decision(
    outcome: CommandOutcome,
) -> Result<devworks_core::protocol::OperationId, Failure> {
    match outcome.map_err(gateway)? {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => Ok(operation_id),
            DomainDecision::Refused { fault } => Err(Failure::Refused(fault)),
            _ => Err(Failure::Other("Unexpected decision receipt".into())),
        },
        _ => Err(Failure::Other("Decision receipt was not confirmed".into())),
    }
}

pub fn decision_model_records(
    outcome: QueryOutcome,
) -> Result<crate::paged::Page<devworks_core::protocol::records::DecisionCatalogModel>, Failure> {
    match query_result(outcome)? {
        (OperationResult::RouterDecisionModels { value }, _) => Ok(crate::paged::Page::new(
            value.items.0,
            value.next_cursor.map(|cursor| cursor.0),
        )),
        (other, _) => Err(unexpected("router.decision_models", &other)),
    }
}
/// A `router.configure` command at the newest schema revision.
pub fn router_configure(backend: &dyn Backend, action: RouterConfigAction) -> Vec<u8> {
    frame::build(
        backend,
        Negotiate::Latest,
        Some(In::Router(router::Input::Configure(
            router::ConfigureInput { action },
        ))),
    )
}
/// Whether `router.configure` changed something (`Applied`) or found it as asked (`NoChange`).
pub fn configured(outcome: CommandOutcome) -> Result<bool, Failure> {
    match outcome.map_err(gateway)? {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied {
                result: OperationResult::RouterConfigure { value },
            }
            | DomainDecision::NoChange {
                result: OperationResult::RouterConfigure { value },
            } => Ok(value.changed),
            DomainDecision::Refused { fault } => Err(Failure::Refused(fault)),
            other => Err(Failure::Other(format!(
                "router.configure returned {}",
                match other {
                    DomainDecision::Accepted { .. } => "an accepted operation",
                    _ => "an unexpected result",
                }
            ))),
        },
        CommandReply::Rejected { code, .. } => Err(Failure::Rejected(format!("{code:?}"))),
        CommandReply::ReceiptPurged { outcome, .. } => Err(Failure::Purged(outcome)),
        CommandReply::CommandStatusUnknown { .. } => Err(Failure::Unknown),
    }
}

pub fn git_inspect(backend: &dyn Backend, repository: &str) -> Vec<u8> {
    git_inspect_worktree(backend, repository, None)
}
pub fn git_inspect_worktree(
    backend: &dyn Backend,
    repository: &str,
    worktree: Option<&str>,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        Some(In::Git(git::Input::Inspect(git::InspectInput {
            repository_id: frame::id(repository)?,
            worktree_id: frame::lift(worktree.map(frame::id))?,
        })))
    })
}
pub fn git_inspected(
    outcome: QueryOutcome,
) -> Result<devworks_core::protocol::records::GitInspection, Failure> {
    match query_result(outcome)? {
        (OperationResult::GitInspect { value }, _) => Ok(value),
        (other, _) => Err(unexpected("git.inspect", &other)),
    }
}
pub fn git_diff(backend: &dyn Backend, repository: &str, base: &str, head: &str) -> Vec<u8> {
    frame::with(backend, Negotiate::Initial, || {
        let oid = |hex: &str| GitOid {
            format: if hex.len() == 64 {
                ObjectFormat::Sha256
            } else {
                ObjectFormat::Sha1
            },
            hex: hex.into(),
        };
        Some(In::Git(git::Input::Diff(git::DiffInput {
            repository_id: frame::id(repository)?,
            worktree_id: None,
            base: oid(base),
            head: oid(head),
            paths: BoundedList(Vec::new()),
            page: frame::page(1, None)?,
        })))
    })
}
pub fn git_patch(outcome: QueryOutcome) -> Result<Option<ArtifactRef>, Failure> {
    match query_result(outcome)? {
        (OperationResult::GitDiff { value }, _) => Ok(value.items.0.into_iter().next()),
        (other, _) => Err(unexpected("git.diff", &other)),
    }
}
