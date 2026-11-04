//! Typed frames for the operations the shell binds, and the decoding of their replies into
//! results or a [`Failure`] the views can present honestly (UI-06). Frames are the same wire
//! envelopes an agent tool submits; nothing here decides anything a use case decides.
use crate::backend::Backend;
use devworks_core::gateway::{CommandOutcome, GatewayFailure, QueryOutcome};
use devworks_core::protocol::provider::{ArtifactRef, TranscriptPage};
use devworks_core::protocol::records::{
    AccessMode, ArtifactRecord, ConnectionRecord, DiagnosticEntry, EffortName, JobInspection,
    JobRecord, JobRun, MutationSummary, ProjectRecord, RepositoryRecord, RepositoryText,
    ResourceRecord, RouteAvailability, RouteChoice, RouteIntent, RouteModel, SessionRecord,
    SettingEntry, ValidationReport, WebDocument, WebResult,
};
use devworks_core::protocol::results::{DomainDecision, DomainQueryReply, OperationResult};
use devworks_core::protocol::{CommandReply, DomainFault};
use devworks_core::settings::view::SettingsView;
use serde_json::{Value, json};

pub const PROTOCOL: &str = "1.0-draft.1";
/// Workspace artifacts (attachments without a project) need artifact schema revision 3.
pub const WORKSPACE_ARTIFACT_REVISION: u64 = 3;
/// Most rows one page request asks for.
pub const PAGE_LIMIT: u16 = 200;
/// Session records carry their start and last-activity times from session schema revision 2.
pub const SESSION_REVISION: u64 = 2;

/// Why a request produced no usable result. Never rendered as success or as zero values.
#[derive(Clone, Debug)]
pub enum Failure {
    /// The use case refused (typed domain fault).
    Refused(DomainFault),
    /// The gateway rejected the frame before any use case ran.
    Rejected(String),
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
        CommandReply::CommandStatusUnknown { .. } => Err(Failure::Unknown),
    }
}

pub(crate) fn unexpected(op: &str, r: &OperationResult) -> Failure {
    Failure::Other(format!("{op} returned {}", r.operation()))
}

pub fn query_frame(backend: &dyn Backend, operation: &str, mut body: Value) -> Vec<u8> {
    body["kind"] = json!(operation);
    json!({"mode": "Query", "protocol": PROTOCOL, "queryId": backend.token("q"), "body": body})
        .to_string()
        .into_bytes()
}
/// A frame negotiating schema `revision` (the envelope member `schemaRevision`).
pub(crate) fn at_revision(frame: Vec<u8>, revision: u64) -> Vec<u8> {
    let mut v: Value = serde_json::from_slice(&frame).expect("frames are JSON");
    v["schemaRevision"] = json!(revision);
    v.to_string().into_bytes()
}
pub fn command_frame(backend: &dyn Backend, operation: &str, mut body: Value) -> Vec<u8> {
    body["kind"] = json!(operation);
    json!({"mode": "Command", "protocol": PROTOCOL, "requestKey": backend.token("rk"),
        "body": body})
    .to_string()
    .into_bytes()
}

// ---------------------------------------------------------------------------------------------
// Settings (SET-01/02/07/08)

pub fn settings_read(backend: &dyn Backend) -> Vec<u8> {
    query_frame(
        backend,
        "settings.read_effective",
        json!({"scope": {"kind": "Workspace"}, "keys": [], "page": {"limit": 500}}),
    )
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
    query_frame(
        backend,
        "settings.explain_source",
        json!({"scope": {"kind": "Workspace"}, "key": key}),
    )
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
    let body = serde_json::to_value(view.save_input()?).ok()?;
    Some(if validate {
        query_frame(backend, "settings.validate", body)
    } else {
        command_frame(backend, "settings.save_revision", body)
    })
}
/// `settings.validate` for explicit values (directory health checks without drafts).
pub fn settings_validate_values(
    backend: &dyn Backend,
    revision: i64,
    entries: &[(&str, Value)],
) -> Vec<u8> {
    let entries: Vec<Value> = entries
        .iter()
        .filter_map(|(k, v)| {
            let d = devworks_core::settings::catalog::descriptor(k)?;
            Some(json!({"key": k, "value": {"schema": d.schema_ref(), "value": v}}))
        })
        .collect();
    query_frame(
        backend,
        "settings.validate",
        json!({"scope": {"kind": "Workspace"}, "expectedRevision": revision.to_string(),
            "entries": entries}),
    )
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

pub fn sessions_list(backend: &dyn Backend) -> Vec<u8> {
    at_revision(
        query_frame(
            backend,
            "sessions.list",
            json!({"scope": {"kind": "Workspace"}, "page": {"limit": PAGE_LIMIT}}),
        ),
        SESSION_REVISION,
    )
}
pub fn sessions(outcome: QueryOutcome) -> Result<(Vec<SessionRecord>, bool), Failure> {
    match query_result(outcome)? {
        (OperationResult::SessionsList { value }, _) => Ok((value.items.0, value.has_more)),
        (other, _) => Err(unexpected("sessions.list", &other)),
    }
}
pub fn session_start(backend: &dyn Backend, title: &str) -> Vec<u8> {
    at_revision(
        command_frame(
            backend,
            "sessions.start",
            json!({"scope": {"kind": "Workspace"}, "title": title}),
        ),
        SESSION_REVISION,
    )
}
pub fn started(outcome: CommandOutcome) -> Result<SessionRecord, Failure> {
    match command_result(outcome)? {
        OperationResult::SessionsStart { value } => Ok(value),
        other => Err(unexpected("sessions.start", &other)),
    }
}
/// One transcript page after `after` (the projection's tail cursor).
pub fn transcript(backend: &dyn Backend, session: &str, after: Option<&str>) -> Vec<u8> {
    let mut frame = json!({"protocol": PROTOCOL, "queryId": backend.token("q"),
        "sessionId": session, "limit": 200});
    if let Some(a) = after {
        frame["after"] = json!(a);
    }
    frame.to_string().into_bytes()
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
    let mut content = Vec::new();
    if !text.trim().is_empty() {
        content.push(json!({"kind": "Text", "text": text}));
    }
    for a in attachments {
        content.push(json!({"kind": "Attachment", "artifact": a}));
    }
    json!({"protocol": PROTOCOL, "requestKey": backend.token("rk"), "sessionId": session,
        "expectedSessionRevision": revision.to_string(), "content": content,
        "schemaRevision": SESSION_REVISION})
    .to_string()
    .into_bytes()
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
    let mut command = json!({"kind": "Save", "workspace": true, "title": title,
        "mediaType": media_type, "source": {"kind": "Upload", "ticketId": ticket},
        "expectedDigest": digest, "expectedBytes": bytes.to_string()});
    if !tags.is_empty() {
        command["tags"] = json!(tags);
    }
    json!({"protocol": PROTOCOL, "requestKey": backend.token("rk"),
        "schemaRevision": WORKSPACE_ARTIFACT_REVISION, "command": command})
    .to_string()
    .into_bytes()
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
    json!({"protocol": PROTOCOL, "requestKey": backend.token("rk"),
        "command": {"kind": "Save", "projectId": project, "title": title,
            "mediaType": media_type, "source": {"kind": "Upload", "ticketId": ticket},
            "expectedDigest": digest, "expectedBytes": bytes.to_string()}})
    .to_string()
    .into_bytes()
}
pub fn saved_artifact(outcome: CommandOutcome) -> Result<ArtifactRecord, Failure> {
    match command_result(outcome)? {
        OperationResult::ArtifactsSave { value } => Ok(value),
        other => Err(unexpected("artifacts.save", &other)),
    }
}
/// `artifacts.read` of one version (binary content comes back as a stream handle).
pub fn artifact_read(backend: &dyn Backend, artifact: &ArtifactRef) -> Vec<u8> {
    json!({"protocol": PROTOCOL, "queryId": backend.token("q"),
        "query": {"kind": "Read", "artifactId": artifact.artifact_id,
            "versionId": artifact.version_id}, "limit": 1})
    .to_string()
    .into_bytes()
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
    json!({"protocol": PROTOCOL, "requestKey": backend.token("rk"), "deletion": {
        "kind": "DeleteVersion", "artifactId": artifact.artifact_id,
        "versionId": artifact.version_id, "expectedRevision": "1",
        "expectedReferenceRevision": "1"}})
    .to_string()
    .into_bytes()
}
pub fn deleted(outcome: CommandOutcome) -> Result<(), Failure> {
    match command_result(outcome)? {
        OperationResult::ArtifactsDelete { .. } => Ok(()),
        other => Err(unexpected("artifacts.delete", &other)),
    }
}
pub fn workspace_artifacts(backend: &dyn Backend) -> Vec<u8> {
    json!({"protocol": PROTOCOL, "queryId": backend.token("q"),
        "schemaRevision": WORKSPACE_ARTIFACT_REVISION,
        "query": {"kind": "List", "workspace": true}, "limit": PAGE_LIMIT})
    .to_string()
    .into_bytes()
}
/// `artifacts.list` of one project's artifacts.
pub fn project_artifacts(backend: &dyn Backend, project: &str) -> Vec<u8> {
    json!({"protocol": PROTOCOL, "queryId": backend.token("q"),
        "schemaRevision": WORKSPACE_ARTIFACT_REVISION,
        "query": {"kind": "List", "projectId": project}, "limit": PAGE_LIMIT})
    .to_string()
    .into_bytes()
}
pub fn artifacts(outcome: QueryOutcome) -> Result<(Vec<ArtifactRecord>, bool), Failure> {
    match query_result(outcome)? {
        (OperationResult::ArtifactsList { value }, _) => Ok((value.items.0, value.has_more)),
        (other, _) => Err(unexpected("artifacts.list", &other)),
    }
}
pub fn resources_list(backend: &dyn Backend) -> Vec<u8> {
    query_frame(
        backend,
        "resources.list",
        json!({"scope": {"kind": "Workspace"}, "page": {"limit": PAGE_LIMIT}}),
    )
}
pub fn resources(outcome: QueryOutcome) -> Result<(Vec<ResourceRecord>, bool), Failure> {
    match query_result(outcome)? {
        (OperationResult::ResourcesList { value }, _) => Ok((value.items.0, value.has_more)),
        (other, _) => Err(unexpected("resources.list", &other)),
    }
}

// ---------------------------------------------------------------------------------------------
// Router (UI-10, ROUTE-19, ROUTE-20)

/// `router.availability` reports a session's pin from schema revision 2.
pub const ROUTER_AVAILABILITY_REVISION: u64 = 2;

/// One page of the catalog, after `cursor` (the first page without one).
pub fn router_catalog(backend: &dyn Backend, cursor: Option<&str>) -> Vec<u8> {
    let mut page = json!({"limit": PAGE_LIMIT});
    if let Some(c) = cursor {
        page["cursor"] = json!(c);
    }
    query_frame(
        backend,
        "router.catalog",
        json!({"page": page, "includeDisabled": false}),
    )
}
/// A catalog page and the cursor of the next one (`None` on the last page).
pub fn route_models(outcome: QueryOutcome) -> Result<(Vec<RouteModel>, Option<String>), Failure> {
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
    at_revision(
        query_frame(backend, "sessions.get", json!({"sessionId": session})),
        SESSION_REVISION,
    )
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
    let mut body = json!({"intent": intent, "effort": effort, "requiredCapabilities": []});
    if let Some(s) = session {
        body["sessionId"] = json!(s);
    }
    at_revision(
        query_frame(backend, "router.availability", body),
        ROUTER_AVAILABILITY_REVISION,
    )
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
    command_frame(
        backend,
        "router.pin",
        json!({"sessionId": session, "expectedPinRevision": expected_pin_revision.to_string(),
            "intent": intent, "effort": effort}),
    )
}
pub fn router_route(
    backend: &dyn Backend,
    session: &str,
    context_epoch: i64,
    intent: &RouteIntent,
    effort: &EffortName,
) -> Vec<u8> {
    command_frame(
        backend,
        "router.route",
        json!({"sessionId": session, "expectedContextEpoch": context_epoch.to_string(),
            "intent": intent, "effort": effort, "requiredCapabilities": []}),
    )
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

/// The web tools' results carry trust and provenance from schema revision 2.
pub const WEB_REVISION: u64 = 2;
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
    at_revision(
        query_frame(
            backend,
            "web.fetch",
            json!({"scope": {"kind": "Workspace"}, "url": url, "maxBytes": WEB_FETCH_BYTES,
                "timeoutMs": WEB_FETCH_TIMEOUT_MS.to_string()}),
        ),
        WEB_REVISION,
    )
}
pub fn web_document(outcome: QueryOutcome) -> Result<WebDocument, Failure> {
    match query_result(outcome)? {
        (OperationResult::WebFetch { value }, _) => Ok(value),
        (other, _) => Err(unexpected("web.fetch", &other)),
    }
}
/// `web.search` of `query` with the configured `provider`, as the user.
pub fn web_search(backend: &dyn Backend, query: &str, provider: &str) -> Vec<u8> {
    at_revision(
        query_frame(
            backend,
            "web.search",
            json!({"scope": {"kind": "Workspace"}, "query": query, "providerId": provider,
                "page": {"limit": WEB_SEARCH_LIMIT}}),
        ),
        WEB_REVISION,
    )
}
pub fn web_results(outcome: QueryOutcome) -> Result<Vec<WebResult>, Failure> {
    match query_result(outcome)? {
        (OperationResult::WebSearch { value }, _) => Ok(value.items.0),
        (other, _) => Err(unexpected("web.search", &other)),
    }
}
/// The newest web activity records (`workspace.logs` of the `web` components).
pub fn web_activity(backend: &dyn Backend) -> Vec<u8> {
    at_revision(
        query_frame(
            backend,
            "workspace.logs",
            json!({"page": {"limit": WEB_ACTIVITY_LIMIT}, "component": "web",
                "afterSequence": "0", "order": "Descending"}),
        ),
        2,
    )
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
pub fn projects_list(backend: &dyn Backend, lifecycle: Option<&str>) -> Vec<u8> {
    let mut body = json!({"scope": {"kind": "Workspace"}, "page": {"limit": PAGE_LIMIT}});
    if let Some(l) = lifecycle {
        body["lifecycle"] = json!(l);
    }
    query_frame(backend, "projects.list", body)
}
pub fn projects(outcome: QueryOutcome) -> Result<(Vec<ProjectRecord>, bool), Failure> {
    match query_result(outcome)? {
        (OperationResult::ProjectsList { value }, _) => Ok((value.items.0, value.has_more)),
        (other, _) => Err(unexpected("projects.list", &other)),
    }
}
pub fn project_create(
    backend: &dyn Backend,
    name: &str,
    description: &str,
    repositories: &[String],
) -> Vec<u8> {
    command_frame(
        backend,
        "projects.create",
        json!({"name": name, "description": description, "repositoryIds": repositories}),
    )
}
/// `projects.edit` of the changed members only (an unchanged member is left out).
pub fn project_edit(
    backend: &dyn Backend,
    project: &ProjectRecord,
    name: Option<&str>,
    description: Option<&str>,
    access_mode: Option<&AccessMode>,
) -> Vec<u8> {
    let mut changes = json!({});
    if let Some(n) = name {
        changes["name"] = json!(n);
    }
    if let Some(d) = description {
        changes["description"] = json!(d);
    }
    if let Some(a) = access_mode {
        changes["accessMode"] = json!(a);
    }
    command_frame(
        backend,
        "projects.edit",
        json!({"projectId": project.id, "expectedRevision": project.revision,
            "changes": changes}),
    )
}
pub fn project_membership(
    backend: &dyn Backend,
    project: &ProjectRecord,
    repositories: &[String],
) -> Vec<u8> {
    command_frame(
        backend,
        "projects.membership",
        json!({"projectId": project.id, "expectedRevision": project.revision,
            "repositoryIds": repositories}),
    )
}
/// `projects.archive`, `.restore` or `.delete` of `project` at its revision.
pub fn project_lifecycle(
    backend: &dyn Backend,
    operation: &str,
    project: &ProjectRecord,
) -> Vec<u8> {
    command_frame(
        backend,
        operation,
        json!({"projectId": project.id, "expectedRevision": project.revision}),
    )
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
pub fn repositories_list(backend: &dyn Backend, project: Option<&str>) -> Vec<u8> {
    let scope = match project {
        Some(p) => json!({"kind": "Project", "projectId": p}),
        None => json!({"kind": "Workspace"}),
    };
    query_frame(
        backend,
        "repositories.list",
        json!({"scope": scope, "page": {"limit": PAGE_LIMIT}}),
    )
}
pub fn repositories(outcome: QueryOutcome) -> Result<(Vec<RepositoryRecord>, bool), Failure> {
    match query_result(outcome)? {
        (OperationResult::RepositoriesList { value }, _) => Ok((value.items.0, value.has_more)),
        (other, _) => Err(unexpected("repositories.list", &other)),
    }
}
/// `repositories.readme` or `.summary` of the repository's latest published view.
pub fn repository_text(backend: &dyn Backend, operation: &str, repository: &str) -> Vec<u8> {
    query_frame(
        backend,
        operation,
        json!({"repositoryId": repository,
            "views": {"kind": "Published", "repositoryIds": [repository]}}),
    )
}
pub fn repository_text_result(outcome: QueryOutcome) -> Result<RepositoryText, Failure> {
    match query_result(outcome)? {
        (OperationResult::RepositoriesReadme { value }, _)
        | (OperationResult::RepositoriesSummary { value }, _) => Ok(value),
        (other, _) => Err(unexpected("repositories.readme", &other)),
    }
}
pub fn remove_index(backend: &dyn Backend, repository: &RepositoryRecord) -> Vec<u8> {
    command_frame(
        backend,
        "repositories.remove_index",
        json!({"repositoryId": repository.id, "expectedRevision": repository.revision}),
    )
}
pub fn index_removed(outcome: CommandOutcome) -> Result<MutationSummary, Failure> {
    match command_result(outcome)? {
        OperationResult::RepositoriesRemoveIndex { value } => Ok(value),
        other => Err(unexpected("repositories.remove_index", &other)),
    }
}
/// `settings.read_effective` of `keys` at a project's scope (its overrides and inheritance).
pub fn project_settings_read(backend: &dyn Backend, project: &str, keys: &[&str]) -> Vec<u8> {
    query_frame(
        backend,
        "settings.read_effective",
        json!({"scope": {"kind": "Project", "projectId": project}, "keys": keys,
            "page": {"limit": 500}}),
    )
}
/// `settings.validate` of explicit values at a project's scope (directory health).
pub fn project_settings_validate(
    backend: &dyn Backend,
    project: &str,
    revision: i64,
    entries: &[(&str, Value)],
) -> Vec<u8> {
    let entries: Vec<Value> = entries
        .iter()
        .filter_map(|(k, v)| {
            let d = devworks_core::settings::catalog::descriptor(k)?;
            Some(json!({"key": k, "value": {"schema": d.schema_ref(), "value": v}}))
        })
        .collect();
    query_frame(
        backend,
        "settings.validate",
        json!({"scope": {"kind": "Project", "projectId": project},
            "expectedRevision": revision.to_string(), "entries": entries}),
    )
}

// ---------------------------------------------------------------------------------------------
// Scheduled jobs (JOB-01/02)

/// Jobs records carry their scope, next and last run, status, hold and authorization (and runs
/// their status, times and reason) from schema revision 2.
pub const JOBS_REVISION: u64 = 2;
/// Runs one history page of the Scheduled jobs screen shows.
pub const RUN_HISTORY_PAGE: u16 = 25;

fn job_scope(project: Option<&str>) -> Value {
    match project {
        Some(p) => json!({"kind": "Project", "projectId": p}),
        None => json!({"kind": "Workspace"}),
    }
}
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
    let mut page = json!({"limit": PAGE_LIMIT});
    if let Some(c) = cursor {
        page["cursor"] = json!(c);
    }
    at_revision(
        query_frame(
            backend,
            "jobs.list",
            json!({"scope": job_scope(project), "page": page}),
        ),
        JOBS_REVISION,
    )
}
/// A page of jobs with the cursor of the next one.
pub fn jobs_page(outcome: QueryOutcome) -> Result<(Vec<JobRecord>, Option<String>), Failure> {
    match query_result(outcome)? {
        (OperationResult::JobsList { value }, _) => {
            Ok((value.items.0, value.next_cursor.map(|c| c.0)))
        }
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
    let mut history = json!({"limit": RUN_HISTORY_PAGE});
    if let Some(c) = cursor {
        history["cursor"] = json!(c);
    }
    at_revision(
        query_frame(
            backend,
            "jobs.inspect",
            json!({"jobId": job, "history": history}),
        ),
        JOBS_REVISION,
    )
}
/// `jobs.inspect` of one named run of `job` (no history page).
pub fn jobs_inspect_run(backend: &dyn Backend, job: &str, run: &str) -> Vec<u8> {
    at_revision(
        query_frame(backend, "jobs.inspect", json!({"jobId": job, "runId": run})),
        JOBS_REVISION,
    )
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
    schedule: &Value,
    definition: &Value,
    enabled: bool,
) -> Vec<u8> {
    at_revision(
        command_frame(
            backend,
            "jobs.create",
            json!({"scope": job_scope(project), "title": title, "schedule": schedule,
                "definition": definition, "enabled": enabled}),
        ),
        JOBS_REVISION,
    )
}
/// `jobs.edit` at the record's revision (a concurrent change is a revision conflict).
pub fn job_edit(
    backend: &dyn Backend,
    job: &JobRecord,
    title: &str,
    schedule: &Value,
    definition: &Value,
) -> Vec<u8> {
    at_revision(
        command_frame(
            backend,
            "jobs.edit",
            json!({"jobId": job.id, "expectedRevision": job.revision, "title": title,
                "schedule": schedule, "definition": definition}),
        ),
        JOBS_REVISION,
    )
}
/// `jobs.enable`, `.disable`, `.delete` or `.run_now` of a job at its record's revision.
pub fn job_command(backend: &dyn Backend, operation: &str, job: &JobRecord) -> Vec<u8> {
    at_revision(
        command_frame(
            backend,
            operation,
            json!({"jobId": job.id, "expectedRevision": job.revision}),
        ),
        JOBS_REVISION,
    )
}
/// `jobs.cancel` of one run at the run's revision.
pub fn job_cancel(backend: &dyn Backend, run: &JobRun) -> Vec<u8> {
    at_revision(
        command_frame(
            backend,
            "jobs.cancel",
            json!({"jobId": run.job_id, "runId": run.id, "expectedRevision": run.revision}),
        ),
        JOBS_REVISION,
    )
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

/// `router.connections`, `router.catalog` and `router.configure` revision 2: connection
/// lifecycle, health, discovery and sign-in challenges; model context windows and bindings.
pub const ROUTER_CONNECTIONS_REVISION: u64 = 2;

pub fn router_connections(backend: &dyn Backend, cursor: Option<&str>) -> Vec<u8> {
    let mut page = json!({"limit": PAGE_LIMIT});
    if let Some(c) = cursor {
        page["cursor"] = json!(c);
    }
    at_revision(
        query_frame(backend, "router.connections", json!({"page": page})),
        ROUTER_CONNECTIONS_REVISION,
    )
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
/// The whole catalog, disabled models included, at revision 2 (context windows, bindings).
pub fn router_catalog_all(backend: &dyn Backend, cursor: Option<&str>) -> Vec<u8> {
    let mut page = json!({"limit": PAGE_LIMIT});
    if let Some(c) = cursor {
        page["cursor"] = json!(c);
    }
    at_revision(
        query_frame(
            backend,
            "router.catalog",
            json!({"page": page, "includeDisabled": true}),
        ),
        ROUTER_CONNECTIONS_REVISION,
    )
}
pub fn router_configure(backend: &dyn Backend, action: Value) -> Vec<u8> {
    at_revision(
        command_frame(backend, "router.configure", json!({"action": action})),
        ROUTER_CONNECTIONS_REVISION,
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
        CommandReply::CommandStatusUnknown { .. } => Err(Failure::Unknown),
    }
}
