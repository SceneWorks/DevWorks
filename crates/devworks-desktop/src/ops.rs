//! Typed frames for the operations the shell binds, and the decoding of their replies into
//! results or a [`Failure`] the views can present honestly (UI-06). Frames are the same wire
//! envelopes an agent tool submits; nothing here decides anything a use case decides.
use crate::backend::Backend;
use devworks_core::gateway::{CommandOutcome, GatewayFailure, QueryOutcome};
use devworks_core::protocol::provider::{ArtifactRef, TranscriptPage};
use devworks_core::protocol::records::{
    AccessMode, ArtifactRecord, DiagnosticEntry, EffortName, MutationSummary, ProjectRecord,
    RepositoryRecord, RepositoryText, ResourceRecord, RouteAvailability, RouteChoice, RouteIntent,
    RouteModel, SessionRecord, SettingEntry, ValidationReport, WebDocument, WebResult,
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

fn unexpected(op: &str, r: &OperationResult) -> Failure {
    Failure::Other(format!("{op} returned {}", r.operation()))
}

pub fn query_frame(backend: &dyn Backend, operation: &str, mut body: Value) -> Vec<u8> {
    body["kind"] = json!(operation);
    json!({"mode": "Query", "protocol": PROTOCOL, "queryId": backend.token("q"), "body": body})
        .to_string()
        .into_bytes()
}
/// A frame negotiating schema `revision` (the envelope member `schemaRevision`).
fn at_revision(frame: Vec<u8>, revision: u64) -> Vec<u8> {
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
// Repository indexing (DW-23)

/// `indexing.status` of one repository at schema revision 2 (freshness, languages, packs,
/// overlays).
pub fn index_status(backend: &dyn Backend, repository: &str) -> Vec<u8> {
    at_revision(
        query_frame(
            backend,
            "indexing.status",
            json!({"repositoryId": repository}),
        ),
        2,
    )
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
) -> Vec<u8> {
    let scope = match project {
        Some(p) => json!({"kind": "Project", "projectId": p}),
        None => json!({"kind": "Workspace"}),
    };
    let mut body = json!({"scope": scope, "page": {"limit": 50}});
    if let Some(r) = repository {
        body["repositoryId"] = json!(r);
    }
    query_frame(backend, "indexing.runs", body)
}
pub fn index_runs_result(
    outcome: QueryOutcome,
) -> Result<(Vec<devworks_core::protocol::records::IndexRunRecord>, bool), Failure> {
    match query_result(outcome)? {
        (OperationResult::IndexingRuns { value }, _) => Ok((value.items.0, value.has_more)),
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
    command_frame(
        backend,
        if rebuild {
            "indexing.reindex"
        } else {
            "indexing.index"
        },
        json!({"scope": {"kind": "Workspace"}, "repositoryIds": [repository],
            "expectedSourceRevision": section_revision.max(1).to_string(),
            "views": {"kind": "Published", "repositoryIds": [repository]}}),
    )
}
/// `indexing.cancel` of a run (or of every run of the request batch it opened).
pub fn cancel_index_run(backend: &dyn Backend, run: &str) -> Vec<u8> {
    json!({"protocol": PROTOCOL, "requestKey": backend.token("rk"),
        "command": {"kind": "Cancel", "operationId": run}})
    .to_string()
    .into_bytes()
}
/// `indexing.refresh_overlay` of one worktree.
pub fn refresh_overlay(
    backend: &dyn Backend,
    repository: &str,
    worktree: &str,
    section_revision: i64,
) -> Vec<u8> {
    json!({"protocol": PROTOCOL, "requestKey": backend.token("rk"),
        "command": {"kind": "RefreshOverlay", "repositoryId": repository, "worktreeId": worktree,
            "expectedSourceRevision": section_revision.max(1).to_string()}})
    .to_string()
    .into_bytes()
}
/// `indexing.retire_view` of an overlay at its revision.
pub fn retire_view(backend: &dyn Backend, view: &str, revision: &str) -> Vec<u8> {
    command_frame(
        backend,
        "indexing.retire_view",
        json!({"viewId": view, "expectedRevision": revision}),
    )
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
    let scope = match project {
        Some(p) => json!({"kind": "Project", "projectId": p}),
        None => json!({"kind": "Workspace"}),
    };
    command_frame(
        backend,
        "indexing.configure_sources",
        json!({"scope": scope, "expectedRevision": expected_revision.max(1).to_string(), "config": {
            "structuralLanguages": languages, "semanticPackageIds": packages,
            "excludePatterns": excludes, "maxFileBytes": max_file_bytes.max(1).to_string()}}),
    )
}
/// `indexing.discover` of every visible section (the runtime scans before answering).
pub fn discover(backend: &dyn Backend) -> Vec<u8> {
    query_frame(
        backend,
        "indexing.discover",
        json!({"scope": {"kind": "Workspace"}, "roots": [], "page": {"limit": PAGE_LIMIT}}),
    )
}
pub fn discover_result(outcome: QueryOutcome) -> Result<(Vec<RepositoryRecord>, bool), Failure> {
    match query_result(outcome)? {
        (OperationResult::IndexingDiscover { value }, _) => Ok((value.items.0, value.has_more)),
        (other, _) => Err(unexpected("indexing.discover", &other)),
    }
}
