//! The desktop request frames, byte for byte. The golden file was recorded from the frames the
//! hand-written `json!` builders produced; the typed builders must keep producing the same bytes.
//! Re-record only for a deliberate wire change: `UPDATE_FRAMES=1 cargo test -p devworks-desktop
//! --lib frame_golden`.
use crate::backend::{Backend, Waker};
use crate::ops;
use devworks_core::gateway::{CommandOutcome, GatewayFailure, QueryOutcome};
use devworks_core::protocol::Id;
use devworks_core::protocol::records::{
    AccessMode, EffortName, JobDefinition, JobRecord, JobRun, ProjectRecord, RepositoryRecord,
    RouteIntent, Schedule, WatcherRecord,
};
use devworks_core::settings::view::Health;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tokio::sync::watch;

mod addons;
mod conventions;
mod databases;
mod decisions;
mod graph;
mod help;
mod instructions;
mod jobs;
mod knowledge;
mod mcp;
mod misc;
mod projects;
mod router;
mod specialized;
mod usage;
mod work;

/// A backend that answers nothing and mints the token prefix itself, so frames are reproducible.
pub(super) struct Silent;
impl Backend for Silent {
    fn query(&self, op: &str, _: &[u8]) -> QueryOutcome {
        Err(GatewayFailure::NoUseCase(op.into()))
    }
    fn command(&self, op: &str, _: &[u8]) -> CommandOutcome {
        Err(GatewayFailure::NoUseCase(op.into()))
    }
    fn mint_workspace_upload(&self, _: &Path) -> Result<Id, String> {
        Err("unavailable".into())
    }
    fn mint_project_upload(
        &self,
        _: &devworks_core::protocol::ProjectId,
        _: &Path,
    ) -> Result<Id, String> {
        Err("unavailable".into())
    }
    fn keep_awake_health(&self) -> Health {
        Health::Available
    }
    fn hints(&self) -> Option<watch::Receiver<u64>> {
        None
    }
    fn set_waker(&self, _: Waker) {}
    fn stage_project_text(
        &self,
        _: Option<&devworks_core::protocol::ProjectId>,
        extension: &str,
        _: &[u8],
    ) -> Result<PathBuf, String> {
        Ok(PathBuf::from(format!("/staged/file.{extension}")))
    }
    fn token(&self, prefix: &str) -> String {
        prefix.into()
    }
    fn scratch_dir(&self) -> PathBuf {
        PathBuf::new()
    }
    fn read_stream(&self, _: &Id, _: u64) -> Result<Vec<u8>, String> {
        Err("unavailable".into())
    }
}

pub(super) fn from<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
    serde_json::from_value(value).expect("sample")
}

/// Every converted builder with sample arguments, keyed by a stable name.
fn frames() -> BTreeMap<String, String> {
    let b = &Silent;
    let project: ProjectRecord = from(json!({"id":"project-a","revision":"3","name":"Project A",
        "description":"About","repositoryIds":["repo-a"],"lifecycle":"Active","accessMode":"Full"}));
    let repository: RepositoryRecord = from(json!({"id":"repo-a","revision":"2","name":"Repo A",
        "projectIds":["project-a"],"primary":true,"lifecycle":"Active",
        "coverage":{"state":"Complete","gaps":[],"moreGaps":false},"indexedViewIds":[]}));
    let intent: RouteIntent = from(json!({"kind":"Model","modelId":"model-a"}));
    let tier: RouteIntent = from(json!({"kind":"Tier","tier":"High"}));
    let effort: EffortName = from(json!("Default"));
    let job: JobRecord = from(json!({"id":"job-a","revision":"4","title":"Nightly",
        "schedule":{"kind":"Manual"},"enabled":true,
        "definition":{"kind":"Inference","prompt":{"artifactId":"artifact-a","versionId":"version-a",
            "sha256":"a".repeat(64),"byteLength":"12","mediaType":"text/plain"},
            "route":{"kind":"Model","modelId":"model-a"},"effort":"Default"}}));
    let run: JobRun = from(
        json!({"id":"run-a","jobId":"job-a","revision":"6","occurrenceKey":"k",
        "state":"Preflighted","usage":{"items":[],"totalTrackedUsd":"0","untrackedRequests":"0",
            "incompleteRequests":"0"}}),
    );
    let watcher: WatcherRecord = from(json!({"id":"watcher-a","revision":"2","state":"Active",
        "sourceKind":"Process","sessionId":"session-a","createdAtMs":"1","elapsedMs":"2",
        "outputTruncated":false,"stopRequested":false}));
    let schedule: Schedule = from(json!({"kind":"Interval","seconds":"60"}));
    let definition: JobDefinition = from(
        json!({"kind":"Native","capabilityId":"capability-a","arguments":{"schema":
        {"schemaId":"core/jobs-arguments","revision":"1","sha256":"b".repeat(64)},"value":{}}}),
    );
    let project_id: devworks_core::protocol::ProjectId = from(json!("project-a"));
    let server: Id = from(json!("server-a"));
    let access: AccessMode = from(json!("Project"));
    let oid1 = "1".repeat(40);
    let oid2 = "2".repeat(64);
    let mut out: Vec<(&str, Vec<u8>)> = vec![
        ("mcp_servers_first", ops::mcp_servers(b, None, None)),
        (
            "mcp_servers_project_cursor",
            ops::mcp_servers(b, Some(&project_id), Some("cursor-1")),
        ),
        (
            "mcp_capabilities",
            ops::mcp_capabilities(b, Some(&project_id), &server, 4, Some("c")),
        ),
        (
            "mcp_capabilities_workspace",
            ops::mcp_capabilities(b, None, &server, 4, None),
        ),
        ("settings_read", ops::settings_read(b)),
        ("container_capabilities", ops::container_capabilities(b)),
        (
            "settings_explain",
            ops::settings_explain(b, "power.keepAwake"),
        ),
        ("sessions_list", ops::sessions_list(b, None)),
        ("session_start", ops::session_start(b, "A title")),
        ("resources_list", ops::resources_list(b, None)),
        (
            "browser_sessions_workspace",
            ops::browser_sessions_list(b, None, None),
        ),
        (
            "browser_sessions_project",
            ops::browser_sessions_list(b, Some("project-a"), None),
        ),
        ("browser_close", ops::browser_close(b, "session-a", "5")),
        ("router_catalog_first", ops::router_catalog(b, None)),
        ("router_catalog_cursor", ops::router_catalog(b, Some("cur"))),
        ("session_get", ops::session_get(b, "session-a")),
        (
            "router_availability",
            ops::router_availability(b, &intent, &effort, None),
        ),
        (
            "router_availability_session",
            ops::router_availability(b, &tier, &effort, Some("session-a")),
        ),
        (
            "router_pin",
            ops::router_pin(b, "session-a", 7, &intent, &effort),
        ),
        (
            "router_route",
            ops::router_route(b, "session-a", 9, &tier, &effort),
        ),
        ("web_fetch", ops::web_fetch(b, "https://example.com/a?b=c")),
        ("web_search", ops::web_search(b, "query text", "provider-a")),
        ("web_activity", ops::web_activity(b)),
        ("projects_list", ops::projects_list(b, None, None)),
        (
            "projects_list_archived",
            ops::projects_list(b, Some("Archived"), None),
        ),
        (
            "project_create",
            ops::project_create(b, "Name", "Desc", &["repo-a".into(), "repo-b".into()]),
        ),
        (
            "project_edit_all",
            ops::project_edit(b, &project, Some("New"), Some("Text"), Some(&access)),
        ),
        (
            "project_edit_none",
            ops::project_edit(b, &project, None, None, None),
        ),
        (
            "project_membership",
            ops::project_membership(b, &project, &["repo-a".into()]),
        ),
        (
            "project_archive",
            ops::project_lifecycle(b, ops::LifecycleChange::Archive, &project),
        ),
        (
            "project_restore",
            ops::project_lifecycle(b, ops::LifecycleChange::Restore, &project),
        ),
        (
            "repositories_list_workspace",
            ops::repositories_list(b, None, None),
        ),
        (
            "repositories_list_project",
            ops::repositories_list(b, Some("project-a"), None),
        ),
        ("repository_readme", ops::repository_text(b, "repo-a", true)),
        (
            "repository_summary",
            ops::repository_text(b, "repo-a", false),
        ),
        ("remove_index", ops::remove_index(b, &repository)),
        (
            "project_settings_read",
            ops::project_settings_read(b, "project-a", &["power.keepAwake"]),
        ),
        ("index_status", ops::index_status(b, "repo-a")),
        ("index_runs_workspace", ops::index_runs(b, None, None, None)),
        (
            "index_runs_project_repo",
            ops::index_runs(b, Some("project-a"), Some("repo-a"), None),
        ),
        (
            "index_repository",
            ops::index_repository(b, "repo-a", false, 0),
        ),
        (
            "reindex_repository",
            ops::index_repository(b, "repo-a", true, 4),
        ),
        ("retire_view", ops::retire_view(b, "view-a", "6")),
        (
            "configure_sources",
            ops::configure_sources(
                b,
                Some("project-a"),
                2,
                &["rust".into()],
                &["pkg".into()],
                &["target/**".into()],
                1_000_000,
            ),
        ),
        (
            "configure_sources_workspace",
            ops::configure_sources(b, None, 0, &[], &[], &[], 0),
        ),
        ("discover", ops::discover(b)),
        ("jobs_list_app", ops::jobs_list(b, None)),
        (
            "jobs_list_project_cursor",
            ops::jobs_list_after(b, Some("project-a"), Some("c")),
        ),
        ("jobs_inspect", ops::jobs_inspect(b, "job-a", None)),
        (
            "jobs_inspect_cursor",
            ops::jobs_inspect(b, "job-a", Some("c")),
        ),
        (
            "jobs_inspect_run",
            ops::jobs_inspect_run(b, "job-a", "run-a"),
        ),
        ("router_connections", ops::router_connections(b, Some("c"))),
        (
            "router_embedding_spaces",
            ops::router_embedding_spaces(b, None),
        ),
        ("router_catalog_all", ops::router_catalog_all(b, Some("c"))),
        ("git_inspect", ops::git_inspect(b, "repo-a")),
        (
            "git_inspect_worktree",
            ops::git_inspect_worktree(b, "repo-a", Some("worktree-a")),
        ),
        ("git_diff", ops::git_diff(b, "repo-a", &oid1, &oid2)),
        (
            "job_create",
            ops::job_create(
                b,
                Some("project-a"),
                "Nightly",
                &schedule,
                &definition,
                true,
            ),
        ),
        (
            "job_create_app",
            ops::job_create(b, None, "Nightly", &schedule, &definition, false),
        ),
        (
            "job_edit",
            ops::job_edit(b, &job, "Renamed", &schedule, &definition),
        ),
        (
            "job_enable",
            ops::job_command(b, ops::JobCommand::Enable, &job),
        ),
        (
            "job_disable",
            ops::job_command(b, ops::JobCommand::Disable, &job),
        ),
        (
            "job_delete",
            ops::job_command(b, ops::JobCommand::Delete, &job),
        ),
        (
            "job_run_now",
            ops::job_command(b, ops::JobCommand::RunNow, &job),
        ),
        ("job_cancel", ops::job_cancel(b, &run)),
        ("watch_cancel", crate::watch_ops::cancel(b, &watcher, true)),
        (
            "watch_cancel_keep",
            crate::watch_ops::cancel(b, &watcher, false),
        ),
        (
            "message_history",
            crate::work_ops::message_history(b, "project-a", None),
        ),
        (
            "message_history_before",
            crate::work_ops::message_history(b, "project-a", Some(42)),
        ),
        (
            "message_inbox",
            crate::work_ops::message_inbox(b, "project-a", None),
        ),
        (
            "message_inbox_cursor",
            crate::work_ops::message_inbox(b, "project-a", Some("c")),
        ),
        (
            "message_ack",
            crate::work_ops::message_ack(b, "project-a", "message-a", None),
        ),
        (
            "message_ack_resolution",
            crate::work_ops::message_ack(b, "project-a", "message-a", Some("Answered")),
        ),
        (
            "message_events",
            crate::work_ops::message_events(b, "project-a", None),
        ),
        (
            "message_events_after",
            crate::work_ops::message_events(b, "project-a", Some(7)),
        ),
        (
            "allocation_queue",
            crate::work_ops::allocation_queue(b, "project-a", None),
        ),
        (
            "allocation_queue_feature",
            crate::work_ops::allocation_queue(b, "project-a", Some("feature-a")),
        ),
        (
            "allocation_inspect",
            crate::work_ops::allocation_inspect(b, "claim-a"),
        ),
        (
            "watch_list_all",
            crate::watch_ops::list(b, &crate::watch_ops::WatchScope::All, None, 50),
        ),
        (
            "watch_list_session",
            crate::watch_ops::list(
                b,
                &crate::watch_ops::WatchScope::Session("session-a".into()),
                Some("c"),
                9999,
            ),
        ),
        (
            "watch_list_feature",
            crate::watch_ops::list(
                b,
                &crate::watch_ops::WatchScope::Feature("feature-a".into()),
                None,
                0,
            ),
        ),
    ];
    out.extend(addons::frames());
    out.extend(conventions::frames());
    out.extend(databases::frames());
    out.extend(decisions::frames());
    out.extend(graph::frames());
    out.extend(help::frames());
    out.extend(instructions::frames());
    out.extend(jobs::frames());
    out.extend(knowledge::frames());
    out.extend(mcp::frames());
    out.extend(misc::frames());
    out.extend(projects::frames());
    out.extend(router::frames());
    out.extend(specialized::frames());
    out.extend(usage::frames());
    out.extend(work::frames());
    out.sort_by_key(|(name, _)| *name);
    out.into_iter()
        .map(|(name, frame)| {
            (
                name.to_string(),
                String::from_utf8(frame).expect("frames are UTF-8"),
            )
        })
        .collect()
}

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/desktop_frames.json")
}

#[test]
fn typed_frames_match_the_recorded_wire_bytes() {
    let now = frames();
    if std::env::var_os("UPDATE_FRAMES").is_some() {
        // Adds the frames the file does not hold yet and never rewrites a recorded one, so a
        // frame is recorded from the builder it came from before that builder changes.
        let mut kept: BTreeMap<String, String> = std::fs::read(golden_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        for (name, frame) in &now {
            kept.entry(name.clone()).or_insert_with(|| frame.clone());
        }
        std::fs::write(
            golden_path(),
            serde_json::to_string_pretty(&kept).unwrap() + "\n",
        )
        .unwrap();
        return;
    }
    let recorded: BTreeMap<String, String> =
        serde_json::from_slice(&std::fs::read(golden_path()).expect("golden frames")).unwrap();
    assert_eq!(
        recorded.keys().collect::<Vec<_>>(),
        now.keys().collect::<Vec<_>>(),
        "the sampled builders changed"
    );
    let different: Vec<_> = now
        .iter()
        .filter(|(name, frame)| &recorded[*name] != *frame)
        .map(|(name, frame)| format!("{name}\n  recorded {}\n  built    {frame}", recorded[name]))
        .collect();
    assert!(
        different.is_empty(),
        "frames differ:\n{}",
        different.join("\n")
    );
}

/// The desktop selects the current revisions for each of these operation bindings.
#[test]
fn the_registry_lists_the_wire_revisions_the_desktop_expects() {
    use crate::frame::latest_revision;
    for (operation, revision) in [
        ("mcp.servers", 8),
        ("mcp.capabilities", 8),
        ("mcp.resources", 8),
        ("mcp.prompts", 8),
        ("sessions.list", 2),
        ("sessions.start", 2),
        ("sessions.get", 2),
        ("browser.sessions", 3),
        ("router.catalog", 4),
        ("router.availability", 3),
        ("router.pin", 3),
        ("router.route", 3),
        ("router.connections", 3),
        ("router.embedding_spaces", 3),
        ("web.fetch", 2),
        ("web.search", 2),
        ("workspace.logs", 2),
        ("indexing.status", 2),
        ("jobs.list", 3),
        ("jobs.inspect", 3),
        ("agents.history", 7),
        ("agents.receive", 7),
        ("agents.ack", 7),
        ("agents.events", 7),
        ("next_work.inspect_queue", 3),
        ("next_work.inspect_claim", 3),
        ("watch.list", 8),
        ("github.account", 3),
        ("github.poll_status", 3),
        ("issues.link", 3),
        ("issues.publish", 3),
    ] {
        assert_eq!(latest_revision(operation), Some(revision), "{operation}");
    }
}

/// `settings.validate` and the MCP content reads against the `json!` frames they replaced.
#[test]
fn settings_and_mcp_content_frames_match_the_earlier_json_builders() {
    let b = &Silent;
    let key = "power.keepAwake";
    let schema = devworks_core::settings::catalog::descriptor(key)
        .unwrap()
        .schema_ref();
    let validate = |scope: serde_json::Value, revision: i64, value: serde_json::Value| {
        json!({"mode":"Query","protocol":ops::PROTOCOL,"queryId":"q","body":{"kind":"settings.validate",
            "scope":scope,"expectedRevision":revision.to_string(),
            "entries":[{"key":key,"value":{"schema":schema,"value":value}}]}})
        .to_string()
        .into_bytes()
    };
    assert_eq!(
        ops::settings_validate_values(b, 3, &[(key, json!(true)), ("no.such.key", json!(1))]),
        validate(json!({"kind":"Workspace"}), 3, json!(true))
    );
    assert_eq!(
        ops::project_settings_validate(b, "project-a", 4, &[(key, json!(false))]),
        validate(
            json!({"kind":"Project","projectId":"project-a"}),
            4,
            json!(false)
        )
    );
    // No known key leaves no entries, which is not a frame the gateway admits.
    assert!(ops::settings_validate_values(b, 3, &[("no.such.key", json!(1))]).is_empty());

    let server: Id = from(json!("server-a"));
    let project: devworks_core::protocol::ProjectId = from(json!("project-a"));
    let read = |body: serde_json::Value| {
        json!({"mode":"Query","protocol":ops::PROTOCOL,"queryId":"q","schemaRevision":8,"body":body})
            .to_string()
            .into_bytes()
    };
    let resource = ops::McpContentRequest::Resource {
        uri: "file:///a.txt".into(),
    };
    assert_eq!(
        ops::mcp_content_frame(b, Some(&project), &server, 5, &resource),
        read(
            json!({"kind":"mcp.resources","scope":{"kind":"Project","projectId":"project-a"},
            "serverId":"server-a","catalogRevision":"5","page":{"limit":1},"uri":"file:///a.txt"})
        )
    );
    let prompt = ops::McpContentRequest::Prompt {
        name: "summarize".into(),
        arguments: Some(from(
            json!({"schema":{"schemaId":"mcp/server-a.summarize","revision":"1",
            "sha256":"a".repeat(64)},"value":{"topic":"x"}}),
        )),
    };
    assert_eq!(
        ops::mcp_content_frame(b, None, &server, 5, &prompt),
        read(
            json!({"kind":"mcp.prompts","scope":{"kind":"Workspace"},"serverId":"server-a",
            "catalogRevision":"5","page":{"limit":1},"name":"summarize",
            "arguments":{"schema":{"schemaId":"mcp/server-a.summarize","revision":"1",
                "sha256":"a".repeat(64)},"value":{"topic":"x"}}})
        )
    );
}
