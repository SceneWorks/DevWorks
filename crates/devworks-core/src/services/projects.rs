//! `WorkService` projects and `RepositoryService` registrations (DW-011) behind `projects.list`,
//! `.get`, `.create`, `.edit`, `.membership`, `.archive`, `.restore`, `.delete` and
//! `repositories.list`, `.get`, `.remove_index`, `.readme`, `.summary`, `.search`, registered
//! through [`crate::services::UseCases::with_projects`] and
//! [`crate::services::UseCases::with_repositories`]. The native Projects and Repositories screens
//! and agent tools reach the same use cases through the gateway; nothing here has a second copy
//! in the shell.
//!
//! # Scope and authority
//!
//! Current authority (role, selected tools, read-only reviewers, project confinement) is the
//! gateway's authority port; archive, delete and index removal by any principal but the user
//! need an exact-request approval there (`authority::policy::APPROVAL_REQUIRED`), so an agent can
//! never archive, delete or drop an index on its own. On top of it every use case checks scope:
//! an invocation bound to a project sees only that project and its member repositories (and only
//! its own ID among a repository's projects); a workspace invocation sees every project and
//! registration. Anything else is `NotFound`, so existence is not disclosed. Only a workspace
//! invocation creates a project, and only the user changes a project's access profile (it is the
//! project's `safety.accessProfile` setting, written through the settings seam with its history).
//!
//! # Boundaries, membership and shared references
//!
//! A project's boundary is its repository membership. `projects.membership` replaces it
//! atomically under the expected revision: every added or removed repository must be visible to
//! the invocation, added ones registered and not deleted, and a repository named by the
//! project's work items cannot leave it (`Conflict`, `repository_in_use`). A repository is a
//! shared registration: leaving one project never touches its registration, its other projects
//! or its source. Each change advances the project revision and, for membership, its scope
//! revision.
//!
//! # Archive, restore and delete
//!
//! * **Archive** fences work in the same unit of work that records it: the project becomes
//!   `Archived`, so no root of it is eligible for dispatch, Start or Continue from that commit
//!   on, and every root is stopped at once ([`ControlService::stop_project`]: paused under a new
//!   stop epoch with a manual-resume hold, its activation asked to stop and its root-scoped
//!   deliveries cancelled, one journaled stop each). The engine then kills the project's owned
//!   process trees without cooperative grace and executes those stops; a stop it cannot verify
//!   stays an uncertain, held stop rather than a fabricated cancellation. The archival also
//!   starts a new archival epoch and marks its project-wide stop pending; the engine clears the
//!   mark only after that stop ran (run supervision retries it after a crash). A launch whose
//!   intent was recorded before the archival but whose tree registers after the stop's snapshot
//!   rechecks the epoch once registered and is killed at once.
//! * **Restore** returns an archived project to `Active` once its archival stop ran (until then
//!   it is `Conflict`, `project_stopping`, retry after the blocker clears: restoring earlier would
//!   let the archived epoch's trees run on). Its work stays stopped: each root keeps the hold the
//!   archival left until an explicit Continue.
//! * **Delete** tombstones an archived, quiescent project (no live activation, unfinished stop or
//!   unsettled owned process): `Deleted`, membership cleared, every other app record retained.
//!   It never touches source Git resources: no repository registration, checkout, branch,
//!   worktree or remote is removed, and shared artifacts and history remain. A tombstone stays
//!   readable through `projects.get` so retained references show the deletion honestly.
//!
//! `proposalId` names an approval proposal; proposals arrive with the approvals item, so one is
//! never found yet (`NotFound`, `proposal`) and the user's own command is the confirmation (the
//! native screen shows the concrete consequences before submitting it).
//!
//! # Repository indexes and texts
//!
//! `repositories.remove_index` removes only the app index views published for a repository and
//! their README/summary texts (`MutationSummary.changed` says whether there was one), with the
//! graph pointer, views and generations of the indexing item (retired, then reclaimed in bounded
//! batches); its active index runs are cancelled and its overlays become unavailable. The
//! registration, memberships, work history, run and overlay records and artifacts stay. `repositories.readme` and
//! `.summary` return the text of the selected published view as an artifact reference; without
//! one the honest answer is `ViewUnavailable` (views are published by the indexing item).
//!
//! # Retained CodeGraph behavior
//!
//! `RepositoriesController.List`/`Detail`/`Delete`/`Readme`, the `list_repositories`,
//! `search_repositories`, `get_service_summary` and `repository_report` tools and the
//! `codegraph://repositories/*` resources map to `repositories.list`, `.get`, `.remove_index`,
//! `.readme`, `.search` and `.summary`: a list filters by a case-insensitive search text and
//! pages; an unknown repository is `NotFound`; deletion removes the graph data but, unlike
//! CodeGraph, keeps the registration (DevWorks never treats index removal as source deletion),
//! and CodeGraph's admin-only delete is an exact-approval command for agents here.
//! `WorkController.Create`/`Find`/`Get`/`Update` and `create_work`/`find_work`/`get_work`/
//! `update_work` for the `project` kind map to `projects.create`, `.list`, `.get` and `.edit`
//! (`.membership`, `.archive`, `.restore`, `.delete` for the boundary and lifecycle changes).
//! `initialPlan` creates the plan's features (each with its pending plan), cards and links
//! through the work service in the project's own unit of work (all or nothing); a composition
//! without the work service answers `UnsupportedCapability` rather than ignoring it. A project's
//! work items name its repositories (context, expected touches, pending plan drafts), and such a
//! repository cannot leave the membership (`repository_in_use`).
use crate::authority::AccessProfile;
use crate::authority::policy::{ACCESS_PROFILE_KEY, resolve_policy};
use crate::context::InvocationContext;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, projects as pops, repositories as rops};
use crate::ports::storage::{
    ACTIVE, ARCHIVED, ConfigScope, ConfigurationRead, DELETED, NewEvent, ProcessFilter,
    ProjectFilter, ProjectPosition, ProjectRead, ReadSnapshot, RepositoryFilter, StorageError,
    StoredIndexView, StoredProject, StoredRepository, TextKind, WriteTransaction, is_terminal,
};
use crate::ports::{
    CommandHandler, Decided, HandlerFailure, IdKind, IdSource, QueryAnswer, QueryHandler,
};
use crate::protocol::data::{Coverage, CoverageState, DataScope, ViewSelection};
use crate::protocol::provider::{ArtifactRef, Digest};
use crate::protocol::records::PageRequest;
use crate::protocol::records::{
    AccessMode, LifeState, MutationSummary, ProjectRecord, RepositoryRecord, RepositoryText,
    SettingAssignment,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, Page, Url};
use crate::protocol::{
    CorrelationId, DomainErrorCode, DomainFault, EventId, Id, ProjectId, RetryAdvice,
    RevisionDimension,
};
use crate::services::cursor::{self, CursorBinding};
use crate::services::runs::{ControlService, control_of};
use crate::services::sessions::{
    Failure, Refusal, content, counter, decided, label, not_found, project_visible, query_fault,
    rev,
};
use crate::settings::catalog::descriptor;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;

pub mod purge;

/// The operations [`ProjectService`] is registered for.
pub const PROJECT_OPERATIONS: [&str; 8] = [
    "projects.list",
    "projects.get",
    "projects.create",
    "projects.edit",
    "projects.membership",
    "projects.archive",
    "projects.restore",
    "projects.delete",
];
/// The operations [`RepositoryService`] is registered for (`repositories.nodes` belongs to the
/// code graph item).
pub const REPOSITORY_OPERATIONS: [&str; 6] = [
    "repositories.list",
    "repositories.get",
    "repositories.remove_index",
    "repositories.readme",
    "repositories.summary",
    "repositories.search",
];
/// History aggregate of a project (`aggregate_id` = project ID).
pub const PROJECT_AGGREGATE: &str = "project";
/// History aggregate of a repository registration (`aggregate_id` = repository ID).
pub const REPOSITORY_AGGREGATE: &str = "repository";
/// What a project deletion never removes.
pub const RETAINED_ON_DELETE: [&str; 4] = [
    "repository registrations and their source checkouts, branches, worktrees and remotes",
    "work history and command receipts",
    "artifacts and their shares",
    "settings revisions",
];
use crate::ports::storage::MAX_PAGE;

fn refuse(code: DomainErrorCode, kind: &'static str, id: Option<&Id>) -> Failure {
    Refusal::new(code, kind, id.cloned()).into()
}
fn conflict(kind: &'static str, id: &Id) -> Failure {
    refuse(DomainErrorCode::Conflict, kind, Some(id))
}

// ---------------------------------------------------------------------------------------------
// Shared helpers

/// The project `id` if this invocation may see it (any lifecycle; otherwise `NotFound`).
pub fn visible_project(
    read: &dyn ProjectRead,
    ctx: &InvocationContext,
    id: &ProjectId,
) -> Result<StoredProject, Failure> {
    match read.project(id)? {
        Some(p) if project_visible(ctx, Some(&p.id)) => Ok(p),
        _ => Err(not_found("project", &id.0).into()),
    }
}

/// The registered, not deleted repository `id` if this invocation may see it: a workspace
/// invocation sees every registration, a project invocation only its project's members.
pub fn visible_repository(
    read: &dyn ProjectRead,
    ctx: &InvocationContext,
    id: &Id,
) -> Result<StoredRepository, Failure> {
    let found = match read.repository(id)? {
        Some(r) if r.lifecycle != DELETED => match ctx.project_id() {
            None => Some(r),
            Some(p) => read.members(p)?.contains(&r.id).then_some(r),
        },
        _ => None,
    };
    found.ok_or_else(|| not_found("repository", id).into())
}

/// The access mode a project's effective `safety.accessProfile` grants (an unsupported stored
/// profile fails closed for authority, so it is never reported as Full).
pub fn access_mode(
    read: &dyn ConfigurationRead,
    p: &ProjectId,
) -> Result<AccessMode, StorageError> {
    Ok(match resolve_policy(read, Some(p))?.profile {
        Some(AccessProfile::Full) => AccessMode::Full,
        _ => AccessMode::Project,
    })
}

fn life(s: &str) -> LifeState {
    match s {
        ARCHIVED => LifeState::Archived,
        DELETED => LifeState::Deleted,
        _ => LifeState::Active,
    }
}
fn life_name(l: &LifeState) -> &'static str {
    match l {
        LifeState::Active => ACTIVE,
        LifeState::Archived => ARCHIVED,
        LifeState::Deleted => DELETED,
    }
}

/// The wire record of a project with its members and access mode.
pub fn project_record(p: &StoredProject, members: Vec<Id>, access: AccessMode) -> ProjectRecord {
    ProjectRecord {
        id: p.id.clone(),
        revision: rev(p.revision),
        name: label(&p.name, "project"),
        description: content(&p.description),
        repository_ids: BoundedList(members.into_iter().take(128).collect()),
        lifecycle: life(&p.lifecycle),
        access_mode: access,
    }
}
fn read_record(
    projects: &dyn ProjectRead,
    config: &dyn ConfigurationRead,
    p: &StoredProject,
) -> Result<ProjectRecord, StorageError> {
    let members = projects.members(&p.id)?;
    Ok(project_record(p, members, access_mode(config, &p.id)?))
}
fn tx_record(
    tx: &mut dyn WriteTransaction,
    p: &StoredProject,
) -> Result<ProjectRecord, StorageError> {
    let members = tx.projects().members(&p.id)?;
    let access = access_mode(&*tx.configuration(), &p.id)?;
    Ok(project_record(p, members, access))
}

/// A registered remote, split into the parts display and the credential refusal look at.
#[derive(Debug, PartialEq, Eq)]
struct Remote<'a> {
    /// `https`, `http`, `ssh`, `git`, `file`, ... (lowercase); `None` for the scp-like form.
    scheme: Option<String>,
    /// User information before `@` (user name and optional `:password`).
    user: Option<&'a str>,
    /// Host with its port, or an IPv6 literal in brackets with its port.
    host: &'a str,
    /// The path, without query and fragment.
    path: &'a str,
    /// A query or fragment was present.
    extra: bool,
}

/// Parses `scheme://[user@]host[:port][/path][?query][#fragment]` (the authority ends at the
/// first `/`, `?` or `#`, so a pathless URL never keeps its query in the authority) and the
/// scp-like `[user@]host:path` (an IPv6 host in brackets). A one-letter "host" is a Windows
/// drive (`C:\src`), not a remote.
fn parse_remote(remote: &str) -> Option<Remote<'_>> {
    let r = remote.trim();
    let extra = r.contains(['?', '#']);
    let base = r.split(['?', '#']).next().unwrap_or("");
    if let Some((scheme, rest)) = base.split_once("://") {
        let end = rest.find('/').unwrap_or(rest.len());
        let (authority, path) = rest.split_at(end);
        let (user, host) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, authority),
        };
        return Some(Remote {
            scheme: Some(scheme.to_ascii_lowercase()),
            user,
            host,
            path,
            extra,
        });
    }
    // scp-like: the user part (possibly with a password) ends at the first '@' before any '/'.
    let at = base.find('@').filter(|i| !base[..*i].contains('/'));
    let (user, rest) = match at {
        Some(i) => (Some(&base[..i]), &base[i + 1..]),
        None => (None, base),
    };
    let (host, path) = if rest.starts_with('[') {
        let close = rest.find(']')?;
        let (h, p) = rest.split_at(close + 1);
        (h, p.strip_prefix(':')?)
    } else {
        let (h, p) = rest.split_once(':')?;
        (h, p)
    };
    if host.chars().count() < 2 || host.contains(['/', '\\']) {
        return None;
    }
    Some(Remote {
        scheme: None,
        user,
        host,
        path,
        extra,
    })
}

/// Whether a remote URL embeds credentials or may carry a secret: any user information of an
/// `http(s)` URL (a token is often the user name alone), a password of any other form, or any
/// query or fragment (`?access_token=...`). An SSH user name (`git@host:path`,
/// `ssh://git@host/path`) is not a secret. A registration never stores such a remote.
pub fn carries_credentials(remote: &str) -> bool {
    let Some(r) = parse_remote(remote) else {
        // Not a remote we can read (a local path): only an explicit query or fragment counts.
        return remote.contains(['?', '#']) && remote.contains("://");
    };
    let http = matches!(r.scheme.as_deref(), Some("http" | "https"));
    r.extra || r.user.is_some_and(|u| http || u.contains(':'))
}

/// A remote URL as it may be shown: `http(s)` with any user information, query and fragment
/// removed, and SSH remotes (`ssh://` or `user@host:path`) as the equivalent `https` address
/// (an SSH port is dropped; an IPv6 literal keeps its brackets). Anything else (`file://`,
/// `git://`, a local path) is not shown. Credentials embedded in a registered remote never leave
/// the store.
pub fn display_remote(remote: &str) -> Option<Url> {
    let r = parse_remote(remote)?;
    let (scheme, host) = match r.scheme.as_deref() {
        Some("https") => ("https://", r.host),
        Some("http") => ("http://", r.host),
        Some("ssh") | None => ("https://", without_port(r.host)),
        Some(_) => return None,
    };
    let path = if r.scheme.is_none() {
        format!("/{}", r.path.trim_start_matches('/'))
    } else {
        r.path.to_owned()
    };
    if host.is_empty() || host.contains(['@', '\\']) || path.contains('\\') {
        return None;
    }
    let s = format!("{scheme}{host}{path}");
    let ok = s.len() <= 4096 && !s.chars().any(|c| c.is_whitespace() || c == '\0');
    ok.then_some(Url(s))
}
/// A host without its port (`[::1]:22` keeps `[::1]`).
fn without_port(host: &str) -> &str {
    if host.starts_with('[') {
        return host.find(']').map_or(host, |i| &host[..=i]);
    }
    host.split(':').next().unwrap_or("")
}

fn coverage(state: &str) -> Coverage {
    Coverage {
        state: match state {
            "Complete" => CoverageState::Complete,
            "Partial" => CoverageState::Partial,
            _ => CoverageState::Unavailable,
        },
        gaps: Vec::new(),
        more_gaps: false,
    }
}

/// The wire record of a repository as `ctx` may see it: its projects are limited to the ones
/// the invocation may see, and its remote is shown without credentials.
pub fn repository_record(
    read: &dyn ProjectRead,
    ctx: &InvocationContext,
    r: &StoredRepository,
) -> Result<RepositoryRecord, StorageError> {
    let projects: Vec<ProjectId> = read
        .repository_projects(&r.id)?
        .into_iter()
        .filter(|p| project_visible(ctx, Some(p)))
        .take(128)
        .collect();
    let views = read.index_views(&r.id)?;
    Ok(RepositoryRecord {
        id: r.id.clone(),
        revision: rev(r.revision),
        name: label(&r.name, "repository"),
        remote_url: r.remote_url.as_deref().and_then(display_remote),
        project_ids: BoundedList(projects),
        primary: r.primary,
        lifecycle: life(&r.lifecycle),
        coverage: coverage(views.first().map_or("Unavailable", |v| v.coverage.as_str())),
        indexed_view_ids: BoundedList(views.iter().take(128).map(|v| v.view_id.clone()).collect()),
    })
}

fn event(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    project: Option<&ProjectId>,
    aggregate: (&str, &Id, i64),
    kind: &str,
    payload: Value,
) -> Result<(), StorageError> {
    let id = EventId(tx.new_id(IdKind::Event));
    let mut payload = payload;
    if let Some(map) = payload.as_object_mut() {
        map.insert(
            "principalKind".into(),
            json!(format!("{:?}", ctx.principal_kind())),
        );
    }
    tx.history().append(NewEvent {
        id,
        actor: ctx.principal().clone(),
        project_id: project.cloned(),
        root_id: None,
        operation_id: None,
        aggregate_kind: aggregate.0.into(),
        aggregate_id: aggregate.1.as_str().to_owned(),
        aggregate_revision: aggregate.2.max(1),
        kind: kind.into(),
        payload,
    })?;
    Ok(())
}

fn ids(v: &[Id]) -> Vec<&str> {
    v.iter().map(Id::as_str).collect()
}

/// A set of repository IDs, in ID order: duplicates are refused (a set identity, AD71).
fn id_set(v: &[Id]) -> Result<Vec<Id>, Failure> {
    let keys: BTreeSet<&str> = v.iter().map(Id::as_str).collect();
    if keys.len() != v.len() {
        return Err(refuse(
            DomainErrorCode::InvalidArgument,
            "repository_ids",
            None,
        ));
    }
    let mut out = v.to_vec();
    out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(out)
}
/// The members of `a` that are not in `b`.
fn minus(a: &[Id], b: &[Id]) -> Vec<Id> {
    let b: BTreeSet<&str> = b.iter().map(Id::as_str).collect();
    a.iter()
        .filter(|x| !b.contains(x.as_str()))
        .cloned()
        .collect()
}

fn revision_conflict(p: &StoredProject, expected: i64) -> Result<(), Failure> {
    if p.revision != expected {
        return Err(Failure::Storage(StorageError::conflict(
            RevisionDimension::Scope,
            "project",
            p.id.0.clone(),
            rev(expected),
            rev(p.revision),
        )));
    }
    Ok(())
}

/// The archival's immediate stop has not run yet: retry once it has.
fn stopping(p: &ProjectId) -> Failure {
    let mut r = Refusal::new(
        DomainErrorCode::Conflict,
        "project_stopping",
        Some(p.0.clone()),
    );
    r.retry = RetryAdvice::AfterBlockerClears;
    r.into()
}

/// Approval proposals arrive with the approvals item: none can be found yet.
fn no_proposal(proposal: Option<&Id>) -> Result<(), Failure> {
    match proposal {
        Some(p) => Err(not_found("proposal", p).into()),
        None => Ok(()),
    }
}

/// A non-blank label (surrounding whitespace is kept exactly as given).
fn require_name(name: &str) -> Result<(), Failure> {
    if name.trim().is_empty() {
        return Err(refuse(DomainErrorCode::InvalidArgument, "name", None));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Projects

/// The projects service: the one implementation of project boundaries and lifecycle.
pub struct ProjectService {
    ids: Arc<dyn IdSource>,
    runs: Arc<ControlService>,
    work: Option<Arc<crate::services::work::WorkService>>,
    purge: Option<Arc<purge::PurgeCoordinator>>,
}

impl ProjectService {
    /// A service whose archival stops roots through `runs` (the runtime's one control service).
    pub fn new(ids: Arc<dyn IdSource>, runs: Arc<ControlService>) -> Self {
        Self {
            ids,
            runs,
            work: None,
            purge: None,
        }
    }
    /// Creates a new project's initial plan (`initialPlan`) through the work service.
    pub fn with_work(mut self, work: Arc<crate::services::work::WorkService>) -> Self {
        self.work = Some(work);
        self
    }
    pub fn with_purge(mut self, purge: Arc<purge::PurgeCoordinator>) -> Self {
        self.purge = Some(purge);
        self
    }
    pub fn supports_purge(&self) -> bool {
        self.purge.is_some()
    }
    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }

    fn list(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &pops::ListInput,
    ) -> Result<(OperationResult, i64), Failure> {
        let mut scope_revision = 1;
        let only = match (&i.scope, ctx.project_id()) {
            (DataScope::Workspace, None) => None,
            (DataScope::Workspace, Some(_)) => {
                return Err(refuse(
                    DomainErrorCode::PermissionDenied,
                    "workspace_scope",
                    None,
                ));
            }
            (DataScope::Project { project_id }, _) => {
                let p = visible_project(read.projects(), ctx, project_id)?;
                scope_revision = p.scope_revision;
                Some(project_id.clone())
            }
        };
        let lifecycles: Vec<String> = match &i.lifecycle {
            None => vec![ACTIVE.into(), ARCHIVED.into()],
            Some(l) => vec![life_name(l).into()],
        };
        let filter = ProjectFilter { only, lifecycles };
        let scope_part = filter.only.as_ref().map_or("*", |p| p.0.as_str());
        let life_part = filter.lifecycles.join(",");
        let binding = CursorBinding::new(
            ctx,
            "projects.list",
            &[scope_part, &life_part],
            scope_revision,
        );
        let rows = cursor::page(
            &binding,
            &i.page,
            |after, n| {
                let after = after
                    .map(|pos| {
                        let (at, id) = pos.split_once(':').ok_or(StorageError::CursorExpired)?;
                        Ok::<_, StorageError>(ProjectPosition {
                            created_at_ms: at.parse().map_err(|_| StorageError::CursorExpired)?,
                            id: ProjectId(Id::new(id).map_err(|_| StorageError::CursorExpired)?),
                        })
                    })
                    .transpose()?;
                read.projects().projects(&filter, after.as_ref(), n)
            },
            |p| format!("{}:{}", p.created_at_ms, p.id.0.as_str()),
        )?;
        Ok((
            OperationResult::ProjectsList {
                value: rows
                    .try_into_page(|p| read_record(read.projects(), read.configuration(), &p))?,
            },
            scope_revision,
        ))
    }

    fn create(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &pops::CreateInput,
    ) -> Result<DomainDecision, Failure> {
        // A project invocation works inside its own boundary; a new project is outside it.
        if ctx.project_id().is_some() {
            return Err(refuse(
                DomainErrorCode::PermissionDenied,
                "workspace_scope",
                None,
            ));
        }
        if i.initial_plan.is_some() && self.work.is_none() {
            return Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "initial_plan",
                None,
            ));
        }
        require_name(&i.name.0)?;
        let members = id_set(&i.repository_ids.0)?;
        for r in &members {
            visible_repository(tx.projects(), ctx, r)?;
        }
        let now = tx.now_ms();
        let p = StoredProject {
            id: ProjectId(tx.new_id(IdKind::Project)),
            name: i.name.0.clone(),
            description: i.description.0.clone(),
            lifecycle: ACTIVE.into(),
            revision: 1,
            scope_revision: 1,
            created_at_ms: now,
            updated_at_ms: now,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 0,
            archive_stop_pending: false,
        };
        tx.projects().insert_project(&p)?;
        tx.projects().set_members(&p.id, &members)?;
        event(
            tx,
            ctx,
            Some(&p.id),
            (PROJECT_AGGREGATE, &p.id.0, p.revision),
            "ProjectCreated",
            json!({"name": p.name, "repositoryIds": ids(&members)}),
        )?;
        // The initial plan's features, cards and links are created in this same unit of work:
        // a refused plan creates no project either.
        if let (Some(plan), Some(work)) = (&i.initial_plan, &self.work) {
            // Its plan content predates the project: what the creator names is shared with the
            // new project, so the project's agents can read it (the plan checks it is visible).
            for f in &plan.features.0 {
                let named = std::iter::once(&f.plan.content).chain(
                    f.plan
                        .requirements
                        .0
                        .iter()
                        .filter_map(|r| r.drop_decision.as_ref()),
                );
                for a in named {
                    crate::services::artifacts::share_named_in(tx, ctx, a, &p.id)?;
                }
            }
            work.apply_atomic(tx, ctx, &p.id, plan, false)?;
        }
        Ok(DomainDecision::Applied {
            result: OperationResult::ProjectsCreate {
                value: tx_record(tx, &p)?,
            },
        })
    }

    /// The project for a boundary or metadata change: visible, at the expected revision, active.
    fn changeable(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        id: &ProjectId,
        expected: i64,
    ) -> Result<StoredProject, Failure> {
        let p = visible_project(tx.projects(), ctx, id)?;
        revision_conflict(&p, expected)?;
        match p.lifecycle.as_str() {
            ARCHIVED => Err(conflict("project_archived", &p.id.0)),
            DELETED => Err(conflict("project_deleted", &p.id.0)),
            _ => Ok(p),
        }
    }

    fn edit(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &pops::EditInput,
    ) -> Result<DomainDecision, Failure> {
        let before = self.changeable(tx, ctx, &i.project_id, i.expected_revision.get())?;
        let mut after = before.clone();
        let mut changed: Vec<&str> = Vec::new();
        if let Some(name) = &i.changes.name {
            require_name(&name.0)?;
            if name.0 != before.name {
                after.name = name.0.clone();
                changed.push("name");
            }
        }
        if let Some(d) = &i.changes.description
            && d.0 != before.description
        {
            after.description = d.0.clone();
            changed.push("description");
        }
        let current_access = access_mode(&*tx.configuration(), &before.id)?;
        let access = i
            .changes
            .access_mode
            .as_ref()
            .filter(|a| access_name(a) != access_name(&current_access));
        if let Some(a) = access {
            // The project's access profile is its `safety.accessProfile` setting: one writer
            // (the settings seam, human only) and one history.
            let entry = access_assignment(a)?;
            let wrote = crate::settings::service::apply_human_assignments(
                tx,
                ctx,
                &ConfigScope::Project(before.id.clone()),
                &[entry],
                json!({"kind": "ProjectEdit", "projectId": before.id.0.as_str()}),
            )
            .map_err(settings_failure)?;
            if wrote {
                changed.push("accessMode");
            }
        }
        if changed.is_empty() {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::ProjectsEdit {
                    value: tx_record(tx, &before)?,
                },
            });
        }
        after.revision = before.revision + 1;
        after.updated_at_ms = tx.now_ms();
        tx.projects().update_project(&after, before.revision)?;
        event(
            tx,
            ctx,
            Some(&after.id),
            (PROJECT_AGGREGATE, &after.id.0, after.revision),
            "ProjectEdited",
            json!({"changed": changed}),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::ProjectsEdit {
                value: tx_record(tx, &after)?,
            },
        })
    }

    fn membership(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &pops::MembershipInput,
    ) -> Result<DomainDecision, Failure> {
        let before = self.changeable(tx, ctx, &i.project_id, i.expected_revision.get())?;
        let wanted = id_set(&i.repository_ids.0)?;
        let current = tx.projects().members(&before.id)?;
        let added = minus(&wanted, &current);
        let removed = minus(&current, &wanted);
        if added.is_empty() && removed.is_empty() {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::ProjectsMembership {
                    value: tx_record(tx, &before)?,
                },
            });
        }
        // Every repository the change touches is authorized for this invocation.
        for r in added.iter().chain(&removed) {
            visible_repository(tx.projects(), ctx, r)?;
        }
        for r in &removed {
            if tx.projects().work_naming_repository(&before.id, r)? > 0 {
                return Err(conflict("repository_in_use", r));
            }
        }
        tx.projects().set_members(&before.id, &wanted)?;
        let mut after = before.clone();
        after.revision = before.revision + 1;
        after.scope_revision = before.scope_revision + 1;
        after.updated_at_ms = tx.now_ms();
        tx.projects().update_project(&after, before.revision)?;
        event(
            tx,
            ctx,
            Some(&after.id),
            (PROJECT_AGGREGATE, &after.id.0, after.revision),
            "ProjectMembershipChanged",
            json!({
                "added": ids(&added),
                "removed": ids(&removed),
                "scopeRevision": after.scope_revision,
            }),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::ProjectsMembership {
                value: tx_record(tx, &after)?,
            },
        })
    }

    fn archive(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &pops::ArchiveInput,
    ) -> Result<DomainDecision, Failure> {
        validate_reviewed_project_command(
            tx,
            ctx,
            "projects.archive",
            serde_json::to_value(i).expect("typed input"),
        )?;
        let before = visible_project(tx.projects(), ctx, &i.project_id)?;
        no_proposal(i.proposal_id.as_ref())?;
        revision_conflict(&before, i.expected_revision.get())?;
        match before.lifecycle.as_str() {
            DELETED => return Err(conflict("project_deleted", &before.id.0)),
            ARCHIVED => {
                return Ok(DomainDecision::NoChange {
                    result: OperationResult::ProjectsArchive {
                        value: tx_record(tx, &before)?,
                    },
                });
            }
            _ => {}
        }
        let now = tx.now_ms();
        let mut after = before.clone();
        after.lifecycle = ARCHIVED.into();
        after.revision = before.revision + 1;
        after.updated_at_ms = now;
        after.archived_at_ms = Some(now);
        // A new archival epoch whose immediate stop the engine still has to run: a launch
        // recorded under the previous epoch is killed when it registers, and restore and delete
        // wait until the project-wide stop has run.
        after.archive_epoch = before.archive_epoch + 1;
        after.archive_stop_pending = true;
        // The fence and the stops commit together: from this commit no root of the project is
        // eligible, and each is paused under a new stop epoch with its stop journaled.
        tx.projects().update_project(&after, before.revision)?;
        let stopped = self.runs.stop_project(tx, ctx.principal(), &after.id)?;
        // Its queued resource requests leave their queues: nothing is granted to it later.
        let cancelled =
            crate::services::resources::cancel_project_waiters(tx, ctx.principal(), &after.id)?;
        // Its schedules stop at once in the same unit of work: every enabled job is held and its
        // active run asked to stop (bound to that run; a run never dispatched ends here).
        let stopped_runs =
            crate::services::jobs::runner::project_archived(tx, ctx.principal(), &after.id)?;
        event(
            tx,
            ctx,
            Some(&after.id),
            (PROJECT_AGGREGATE, &after.id.0, after.revision),
            "ProjectArchived",
            json!({
                "stoppedRoots": stopped.iter().map(|(r, _)| r.0.as_str()).collect::<Vec<_>>(),
                "stopOperations": stopped.iter().map(|(_, o)| o.0.as_str()).collect::<Vec<_>>(),
                "cancelledWaitingLeases": cancelled,
                "stoppedJobRuns": stopped_runs.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
            }),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::ProjectsArchive {
                value: tx_record(tx, &after)?,
            },
        })
    }

    fn restore(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &pops::RestoreInput,
    ) -> Result<DomainDecision, Failure> {
        let before = visible_project(tx.projects(), ctx, &i.project_id)?;
        no_proposal(i.proposal_id.as_ref())?;
        revision_conflict(&before, i.expected_revision.get())?;
        match before.lifecycle.as_str() {
            DELETED => return Err(conflict("project_deleted", &before.id.0)),
            ACTIVE => {
                return Ok(DomainDecision::NoChange {
                    result: OperationResult::ProjectsRestore {
                        value: tx_record(tx, &before)?,
                    },
                });
            }
            _ => {}
        }
        // Restore waits for the archival's immediate stop: otherwise a restore committed
        // before the engine's stop read the lifecycle would let trees of the archived epoch run
        // on (and a stop run after restore could kill work started since).
        if before.archive_stop_pending {
            return Err(stopping(&before.id));
        }
        let mut after = before.clone();
        after.lifecycle = ACTIVE.into();
        after.revision = before.revision + 1;
        after.updated_at_ms = tx.now_ms();
        after.archived_at_ms = None;
        tx.projects().update_project(&after, before.revision)?;
        // Work stays stopped: every root keeps the hold its archival left until a Continue, and
        // every schedule stays held until a person enables it (ReactivationRequired).
        let held_jobs = crate::services::jobs::runner::project_restored(tx, &after.id)?;
        event(
            tx,
            ctx,
            Some(&after.id),
            (PROJECT_AGGREGATE, &after.id.0, after.revision),
            "ProjectRestored",
            json!({"workStopped": true, "heldJobs": held_jobs.iter().map(|j| j.as_str()).collect::<Vec<_>>()}),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::ProjectsRestore {
                value: tx_record(tx, &after)?,
            },
        })
    }

    /// What of the project may still be running: live activations, unfinished stops and
    /// unsettled owned processes (bounded to the first few, which a refusal names).
    fn activity(
        tx: &mut dyn WriteTransaction,
        project: &ProjectId,
    ) -> Result<Vec<Id>, StorageError> {
        let mut live: Vec<Id> = Vec::new();
        let mut after = None;
        loop {
            let page = tx.runs().project_roots(project, after.as_ref(), MAX_PAGE)?;
            for root in &page {
                if let Some(a) = tx.runs().live_activation(root)? {
                    live.push(a.id.clone());
                }
                let control = control_of(tx.runs(), root)?;
                if let Some(op) = &control.stop_operation_id
                    && tx
                        .operations()
                        .operation(op)?
                        .is_some_and(|o| !is_terminal(o.state))
                {
                    live.push(op.0.clone());
                }
            }
            if page.len() < usize::from(MAX_PAGE) {
                break;
            }
            after = page.last().cloned();
        }
        let mut cursor: Option<Id> = None;
        loop {
            let page =
                tx.resources()
                    .processes(&ProcessFilter::Unsettled, cursor.as_ref(), MAX_PAGE)?;
            live.extend(
                page.iter()
                    .filter(|p| p.project_id.as_ref() == Some(project))
                    .map(|p| p.id.clone()),
            );
            if page.len() < usize::from(MAX_PAGE) {
                break;
            }
            cursor = page.last().map(|p| p.id.clone());
        }
        Ok(live)
    }

    fn delete(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &pops::DeleteInput,
    ) -> Result<DomainDecision, Failure> {
        validate_reviewed_project_command(
            tx,
            ctx,
            "projects.delete",
            serde_json::to_value(i).expect("typed input"),
        )?;
        let before = visible_project(tx.projects(), ctx, &i.project_id)?;
        no_proposal(i.proposal_id.as_ref())?;
        revision_conflict(&before, i.expected_revision.get())?;
        match before.lifecycle.as_str() {
            DELETED => {
                return Ok(DomainDecision::NoChange {
                    result: OperationResult::ProjectsDelete {
                        value: tx_record(tx, &before)?,
                    },
                });
            }
            // Deletion follows archival: work is fenced and stopped first.
            ACTIVE => return Err(conflict("project_active", &before.id.0)),
            _ => {}
        }
        if before.archive_stop_pending {
            return Err(stopping(&before.id));
        }
        let live = Self::activity(tx, &before.id)?;
        if !live.is_empty() {
            let mut r = Refusal::new(
                DomainErrorCode::Conflict,
                "project_not_quiescent",
                Some(before.id.0.clone()),
            );
            r.retry = RetryAdvice::AfterBlockerClears;
            return Err(r.into());
        }
        let removed = tx.projects().members(&before.id)?;
        tx.projects().set_members(&before.id, &[])?;
        let now = tx.now_ms();
        let mut after = before.clone();
        after.lifecycle = DELETED.into();
        after.revision = before.revision + 1;
        after.scope_revision = before.scope_revision + 1;
        after.updated_at_ms = now;
        after.deleted_at_ms = Some(now);
        tx.projects().update_project(&after, before.revision)?;
        event(
            tx,
            ctx,
            Some(&after.id),
            (PROJECT_AGGREGATE, &after.id.0, after.revision),
            "ProjectDeleted",
            json!({
                "removedMemberships": ids(&removed),
                "retained": RETAINED_ON_DELETE,
                "sourceResourcesTouched": false,
            }),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::ProjectsDelete {
                value: tx_record(tx, &after)?,
            },
        })
    }
}

fn access_name(a: &AccessMode) -> &'static str {
    match a {
        AccessMode::Full => "full",
        AccessMode::Project => "projectScope",
    }
}
fn access_assignment(a: &AccessMode) -> Result<SettingAssignment, Failure> {
    let d = descriptor(ACCESS_PROFILE_KEY).expect("catalog key");
    serde_json::from_value(json!({
        "key": ACCESS_PROFILE_KEY,
        "value": {"schema": d.schema_ref(), "value": access_name(a)},
    }))
    .map_err(|_| refuse(DomainErrorCode::InvalidArgument, "access_mode", None))
}
fn settings_failure(f: crate::settings::service::Failure) -> Failure {
    match f {
        crate::settings::service::Failure::Refused(r) => {
            // A non-human principal never changes the access profile.
            let kind = if r.code == DomainErrorCode::PermissionDenied {
                "access_mode"
            } else {
                r.target_kind
            };
            Refusal {
                code: r.code,
                retry: r.retry,
                target_kind: kind,
                target_id: r.target_id,
            }
            .into()
        }
        crate::settings::service::Failure::Storage(e) => Failure::Storage(e),
    }
}

impl QueryHandler for ProjectService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let answered = match query.input() {
            OperationInput::Projects(pops::Input::List(i)) => self.list(snapshot, ctx, i),
            OperationInput::Projects(pops::Input::Get(i)) => {
                visible_project(snapshot.projects(), ctx, &i.project_id).and_then(|p| {
                    let value = read_record(snapshot.projects(), snapshot.configuration(), &p)?;
                    Ok((OperationResult::ProjectsGet { value }, p.scope_revision))
                })
            }
            OperationInput::Projects(pops::Input::PurgeManifest(i)) if self.supports_purge() => {
                purge::PurgeCoordinator::manifest_page(snapshot, ctx, i)
                    .map(|value| (OperationResult::ProjectsPurgeManifest { value }, 1))
            }
            OperationInput::Projects(pops::Input::PurgeStatus(i)) if self.supports_purge() => {
                purge::PurgeCoordinator::status(snapshot, ctx, i)
                    .map(|value| (OperationResult::ProjectsPurgeStatus { value }, 1))
            }
            _ => Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "operation",
                None,
            )),
        };
        let (result, scope) =
            answered.map_err(|f| query_fault(f, "project", self.correlation()))?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(scope)),
            result,
        })
    }
}

impl CommandHandler for ProjectService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let outcome = match command.input() {
            OperationInput::Projects(pops::Input::Create(i)) => self.create(tx, ctx, i),
            OperationInput::Projects(pops::Input::Edit(i)) => self.edit(tx, ctx, i),
            OperationInput::Projects(pops::Input::Membership(i)) => self.membership(tx, ctx, i),
            OperationInput::Projects(pops::Input::Archive(i)) => self.archive(tx, ctx, i),
            OperationInput::Projects(pops::Input::Restore(i)) => self.restore(tx, ctx, i),
            OperationInput::Projects(pops::Input::Delete(i)) => self.delete(tx, ctx, i),
            OperationInput::Projects(pops::Input::PreparePurge(i)) if self.supports_purge() => self
                .purge
                .as_ref()
                .expect("configured")
                .prepare(tx, ctx, i)
                .map(|value| DomainDecision::Applied {
                    result: OperationResult::ProjectsPreparePurge { value },
                }),
            OperationInput::Projects(pops::Input::Purge(i)) if self.supports_purge() => self
                .purge
                .as_ref()
                .expect("configured")
                .authorize(tx, ctx, i)
                .and_then(|permit| {
                    tx.projects()
                        .apply_project_purge(&permit)
                        .map_err(Failure::Storage)
                })
                .map(|value| DomainDecision::Applied {
                    result: OperationResult::ProjectsPurge { value },
                }),
            _ => Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "operation",
                None,
            )),
        };
        let mut decision = decided(tx, outcome)?;
        if let DomainDecision::Applied {
            result: OperationResult::ProjectsPurge { value },
        } = &decision.decision
        {
            decision.operation_id = value.operation_id.clone();
        }
        Ok(decision)
    }
}

// ---------------------------------------------------------------------------------------------
// Repositories

/// The repository registration service: listing, search, texts and index removal.
pub struct RepositoryService {
    ids: Arc<dyn IdSource>,
}

impl RepositoryService {
    pub fn new(ids: Arc<dyn IdSource>) -> Self {
        Self { ids }
    }
    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }

    #[allow(clippy::too_many_arguments)]
    fn page(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        operation: &str,
        scope: &DataScope,
        text: Option<&str>,
        languages: &[String],
        page: &PageRequest,
    ) -> Result<(Page<RepositoryRecord>, i64), Failure> {
        // A project-scoped listing is bound to the project's scope revision: a membership change
        // between pages expires its cursor, and its stamp carries that revision.
        let (project, scope_revision) = match (scope, ctx.project_id()) {
            (DataScope::Workspace, None) => (None, 1),
            (DataScope::Workspace, Some(_)) => {
                return Err(refuse(
                    DomainErrorCode::PermissionDenied,
                    "workspace_scope",
                    None,
                ));
            }
            (DataScope::Project { project_id }, _) => {
                let p = visible_project(read.projects(), ctx, project_id)?;
                (Some(project_id.clone()), p.scope_revision)
            }
        };
        let mut languages: Vec<String> = languages.iter().map(|l| l.to_lowercase()).collect();
        languages.sort();
        languages.dedup();
        let filter = RepositoryFilter {
            project,
            text: text.map(str::to_owned),
            languages,
            include_deleted: false,
        };
        let scope_part = filter.project.as_ref().map_or("*", |p| p.0.as_str());
        let text_part = filter.text.clone().unwrap_or_default();
        let language_part = filter.languages.join(",");
        let binding = CursorBinding::new(
            ctx,
            operation,
            &[scope_part, &text_part, &language_part],
            scope_revision,
        );
        let rows = cursor::page(
            &binding,
            page,
            |after, n| {
                let after = after
                    .map(|p| Id::new(p).map_err(|_| StorageError::CursorExpired))
                    .transpose()?;
                read.projects().repositories(&filter, after.as_ref(), n)
            },
            |r| r.id.as_str().to_owned(),
        )?;
        Ok((
            rows.try_into_page(|r| repository_record(read.projects(), ctx, &r))?,
            scope_revision,
        ))
    }

    fn text(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        repository: &Id,
        views: &ViewSelection,
        kind: TextKind,
    ) -> Result<RepositoryText, Failure> {
        let r = visible_repository(read.projects(), ctx, repository)?;
        let view = select_view(read.projects(), &r, views)?;
        let Some(t) = read.projects().repository_text(&view.view_id, kind)? else {
            return Err(refuse(
                DomainErrorCode::ViewUnavailable,
                match kind {
                    TextKind::Readme => "repository_readme",
                    TextKind::Summary => "repository_summary",
                },
                Some(&r.id),
            ));
        };
        let sha256 = Digest::try_from(t.sha256.clone())
            .map_err(|_| StorageError::InvalidRecord("stored text digest".into()))?;
        Ok(RepositoryText {
            repository_id: r.id.clone(),
            view_id: view.view_id.clone(),
            content: ArtifactRef {
                artifact_id: t.artifact_id.clone(),
                version_id: t.version_id.clone(),
                sha256,
                byte_length: counter(t.byte_length),
                media_type: t.media_type.clone(),
            },
            coverage: coverage(&t.coverage),
        })
    }

    fn remove_index(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &rops::RemoveIndexInput,
    ) -> Result<DomainDecision, Failure> {
        validate_reviewed_project_command(
            tx,
            ctx,
            "repositories.remove_index",
            serde_json::to_value(i).expect("typed input"),
        )?;
        let r = visible_repository(tx.projects(), ctx, &i.repository_id)?;
        let expected = i.expected_revision.get();
        if r.revision != expected {
            return Err(Failure::Storage(StorageError::conflict(
                RevisionDimension::Registry,
                "repository",
                r.id.clone(),
                rev(expected),
                rev(r.revision),
            )));
        }
        let removed = tx.projects().remove_index(&r.id)?;
        // The index's runs and overlays end with it (their records and history stay).
        let ended = crate::services::indexing::index_removed(tx, &r.id)?;
        if removed == 0 && ended == 0 {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::RepositoriesRemoveIndex {
                    value: MutationSummary {
                        id: r.id.clone(),
                        revision: rev(r.revision),
                        changed: false,
                    },
                },
            });
        }
        let revision = tx.projects().touch_repository(&r.id, r.revision)?;
        event(
            tx,
            ctx,
            None,
            (REPOSITORY_AGGREGATE, &r.id, revision),
            "RepositoryIndexRemoved",
            json!({
                "removedViews": removed,
                "registrationKept": true,
                "sourceTouched": false,
            }),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::RepositoriesRemoveIndex {
                value: MutationSummary {
                    id: r.id.clone(),
                    revision: rev(revision),
                    changed: true,
                },
            },
        })
    }
}

/// The published view a text request selects for `r`.
fn select_view(
    read: &dyn ProjectRead,
    r: &StoredRepository,
    views: &ViewSelection,
) -> Result<StoredIndexView, Failure> {
    let unavailable = || {
        refuse(
            DomainErrorCode::ViewUnavailable,
            "repository_view",
            Some(&r.id),
        )
    };
    match views {
        ViewSelection::Published { repository_ids } => {
            if !repository_ids.contains(&r.id) {
                return Err(refuse(DomainErrorCode::InvalidArgument, "views", None));
            }
            read.index_views(&r.id)?
                .into_iter()
                .next()
                .ok_or_else(unavailable)
        }
        ViewSelection::Exact { view_ids } => {
            for v in view_ids {
                if let Some(view) = read.index_view(v)?
                    && view.repository_id == r.id
                {
                    return Ok(view);
                }
            }
            Err(unavailable())
        }
        // Retrieval pins belong to the retrieval items; none exists to resolve here.
        ViewSelection::Pinned { .. } => Err(refuse(
            DomainErrorCode::UnsupportedCapability,
            "view_pin",
            None,
        )),
        ViewSelection::SchemaPublished { .. } => {
            Err(refuse(DomainErrorCode::InvalidArgument, "views", None))
        }
    }
}

impl QueryHandler for RepositoryService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        // A single-record read of a project invocation depends on its project's membership
        // (visibility): it is stamped with that project's scope revision.
        let own_scope = || -> Result<i64, Failure> {
            Ok(match ctx.project_id() {
                Some(p) => snapshot
                    .projects()
                    .project(p)?
                    .map_or(1, |x| x.scope_revision),
                None => 1,
            })
        };
        let answered = match query.input() {
            OperationInput::Repositories(rops::Input::List(i)) => self
                .page(
                    snapshot,
                    ctx,
                    "repositories.list",
                    &i.scope,
                    i.text.as_ref().map(|t| t.0.as_str()),
                    &[],
                    &i.page,
                )
                .map(|(value, scope)| (OperationResult::RepositoriesList { value }, scope)),
            OperationInput::Repositories(rops::Input::Search(i)) => {
                let languages: Vec<String> = i.language_ids.0.iter().map(|l| l.0.clone()).collect();
                self.page(
                    snapshot,
                    ctx,
                    "repositories.search",
                    &i.scope,
                    Some(i.query.0.as_str()),
                    &languages,
                    &i.page,
                )
                .map(|(value, scope)| (OperationResult::RepositoriesSearch { value }, scope))
            }
            OperationInput::Repositories(rops::Input::Get(i)) => {
                visible_repository(snapshot.projects(), ctx, &i.repository_id).and_then(|r| {
                    let value = repository_record(snapshot.projects(), ctx, &r)?;
                    Ok((OperationResult::RepositoriesGet { value }, own_scope()?))
                })
            }
            OperationInput::Repositories(rops::Input::Readme(i)) => self
                .text(snapshot, ctx, &i.repository_id, &i.views, TextKind::Readme)
                .and_then(|value| {
                    Ok((OperationResult::RepositoriesReadme { value }, own_scope()?))
                }),
            OperationInput::Repositories(rops::Input::Summary(i)) => self
                .text(snapshot, ctx, &i.repository_id, &i.views, TextKind::Summary)
                .and_then(|value| {
                    Ok((OperationResult::RepositoriesSummary { value }, own_scope()?))
                }),
            _ => Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "operation",
                None,
            )),
        };
        let (result, scope) =
            answered.map_err(|f| query_fault(f, "repository", self.correlation()))?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(scope)),
            result,
        })
    }
}

impl CommandHandler for RepositoryService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let outcome = match command.input() {
            OperationInput::Repositories(rops::Input::RemoveIndex(i)) => {
                self.remove_index(tx, ctx, i)
            }
            _ => Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "operation",
                None,
            )),
        };
        decided(tx, outcome)
    }
}

/// Exact owner preview for the existing project/index destructive commands. It shares the
/// target visibility and revision guards with their normal handlers; source files are untouched.
pub(crate) fn approval_preview(
    read: &dyn ProjectRead,
    ctx: &InvocationContext,
    request: &crate::admission::DecodedRequest,
) -> Result<crate::services::approvals::OwnerPreview, Failure> {
    use crate::services::approvals::OwnerPreview;
    match &request.input {
        OperationInput::Projects(pops::Input::Archive(i)) => project_approval(
            read,
            ctx,
            &i.project_id,
            i.expected_revision.get(),
            i.proposal_id.as_ref(),
            false,
        ),
        OperationInput::Projects(pops::Input::Delete(i)) => project_approval(
            read,
            ctx,
            &i.project_id,
            i.expected_revision.get(),
            i.proposal_id.as_ref(),
            true,
        ),
        OperationInput::Repositories(rops::Input::RemoveIndex(i)) => {
            let r = visible_repository(read, ctx, &i.repository_id)?;
            if r.revision != i.expected_revision.get() {
                return Err(StorageError::conflict(
                    RevisionDimension::Registry,
                    "repository",
                    r.id.clone(),
                    rev(i.expected_revision.get()),
                    rev(r.revision),
                )
                .into());
            }
            let views=read.index_views(&r.id)?.into_iter().map(|v|json!({"id":v.view_id,"sourceRevision":v.source_revision,"coverage":v.coverage,"publishedAtMs":v.published_at_ms})).collect::<Vec<_>>();
            let binding = json!({"repositoryId":r.id,"revision":r.revision,"location":r.location,"lifecycle":r.lifecycle,"projects":read.repository_projects(&r.id)?,"indexViews":views});
            Ok(OwnerPreview {
                scope: ctx
                    .project_id()
                    .cloned()
                    .map_or(ConfigScope::Workspace, ConfigScope::Project),
                target_id: r.id,
                target_revision: r.revision,
                summary: format!("Remove app index of {}", r.name),
                reviewed: json!({"effect":"Remove app index, overlays and cancel index runs; registered source, memberships and history stay","repository":binding}),
                binding,
            })
        }
        _ => Err(Refusal::new(
            DomainErrorCode::UnsupportedCapability,
            "approval_operation",
            None,
        )
        .into()),
    }
}
fn project_approval(
    read: &dyn ProjectRead,
    ctx: &InvocationContext,
    id: &ProjectId,
    expected: i64,
    proposal: Option<&Id>,
    delete: bool,
) -> Result<crate::services::approvals::OwnerPreview, Failure> {
    let p = visible_project(read, ctx, id)?;
    no_proposal(proposal)?;
    revision_conflict(&p, expected)?;
    if !delete && p.lifecycle == DELETED {
        return Err(conflict("project_deleted", &p.id.0));
    }
    if delete && p.lifecycle == ACTIVE {
        return Err(conflict("project_active", &p.id.0));
    }
    if delete && p.archive_stop_pending {
        return Err(stopping(&p.id));
    }
    let binding = json!({"projectId":p.id,"revision":p.revision,"scopeRevision":p.scope_revision,"lifecycle":p.lifecycle,"archiveEpoch":p.archive_epoch,"archiveStopPending":p.archive_stop_pending,"repositories":read.members(&p.id)?});
    Ok(crate::services::approvals::OwnerPreview {
        scope: ConfigScope::Project(p.id.clone()),
        target_id: p.id.0,
        target_revision: p.revision,
        summary: format!(
            "{} project {}",
            if delete {
                "Delete app records of"
            } else {
                "Archive and stop"
            },
            p.name
        ),
        reviewed: json!({"effect":if delete{"Tombstone project and detach repositories; source files and retained history stay"}else{"Archive project, pause all work and stop owned processes"},"project":binding}),
        binding,
    })
}
pub(crate) fn validate_approval_preview(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    request: &crate::admission::DecodedRequest,
    reviewed: &crate::services::approvals::OwnerPreview,
) -> Result<(), Failure> {
    let current = approval_preview(tx.projects(), ctx, request)?;
    if current != *reviewed {
        return Err(conflict("approval_scope_changed", &reviewed.target_id));
    }
    if let OperationInput::Projects(pops::Input::Delete(i)) = &request.input
        && !ProjectService::activity(tx, &i.project_id)?.is_empty()
    {
        return Err(conflict("project_not_quiescent", &i.project_id.0));
    }
    Ok(())
}
fn validate_reviewed_project_command(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    operation: &str,
    input: serde_json::Value,
) -> Result<(), Failure> {
    let Some(reviewed) = ctx.reviewed_operation() else {
        return Ok(());
    };
    let request = crate::services::approvals::requests::decode(
        operation,
        reviewed.schema_revision(),
        &input,
        true,
    )
    .map_err(|_| conflict("reviewed_operation", reviewed.approval_id()))?;
    if reviewed.operation() != operation
        || reviewed.request_digest() != request.fingerprint.as_str()
    {
        return Err(conflict("reviewed_operation", reviewed.approval_id()));
    }
    let current = approval_preview(tx.projects(), ctx, &request)?;
    if &current.binding != reviewed.owner_binding() {
        return Err(conflict("reviewed_scope_changed", &current.target_id));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::display_remote;

    #[test]
    fn remotes_are_shown_without_credentials() {
        let shown = |r: &str| display_remote(r).map(|u| u.0);
        // (remote, shown, refused at registration)
        let cases: [(&str, Option<&str>, bool); 20] = [
            (
                "https://github.com/acme/tools.git",
                Some("https://github.com/acme/tools.git"),
                false,
            ),
            (
                "https://user:ghp_secret@github.com/acme/tools.git?token=x#frag",
                Some("https://github.com/acme/tools.git"),
                true,
            ),
            (
                "https://ghp_token@github.com/acme/x.git",
                Some("https://github.com/acme/x.git"),
                true,
            ),
            (
                "https://git.example.com?access_token=secret",
                Some("https://git.example.com"),
                true,
            ),
            (
                "https://git.example.com#access_token=secret",
                Some("https://git.example.com"),
                true,
            ),
            (
                "https://git.example.com/x?ref=a@b",
                Some("https://git.example.com/x"),
                true,
            ),
            (
                "https://git.example.com:8443/acme/x",
                Some("https://git.example.com:8443/acme/x"),
                false,
            ),
            (
                "https://[2001:db8::1]:8443/acme/x",
                Some("https://[2001:db8::1]:8443/acme/x"),
                false,
            ),
            ("http://example.com/x", Some("http://example.com/x"), false),
            (
                "git@github.com:acme/tools.git",
                Some("https://github.com/acme/tools.git"),
                false,
            ),
            (
                "git:pw@github.com:acme/tools.git",
                Some("https://github.com/acme/tools.git"),
                true,
            ),
            (
                "git@[2001:db8::1]:acme/x.git",
                Some("https://[2001:db8::1]/acme/x.git"),
                false,
            ),
            (
                "github.com:acme/x.git?t=1",
                Some("https://github.com/acme/x.git"),
                true,
            ),
            (
                "ssh://git@example.com:2222/acme/tools",
                Some("https://example.com/acme/tools"),
                false,
            ),
            (
                "ssh://git:pw@example.com:2222/acme/tools",
                Some("https://example.com/acme/tools"),
                true,
            ),
            (
                "ssh://git@[2001:db8::1]:22/x",
                Some("https://[2001:db8::1]/x"),
                false,
            ),
            ("file:///Users/me/src", None, false),
            ("git://example.com/x", None, false),
            ("/Users/me/src", None, false),
            ("C:\\src\\tools", None, false),
        ];
        for (remote, want, refused) in cases {
            assert_eq!(shown(remote).as_deref(), want, "{remote}");
            assert_eq!(super::carries_credentials(remote), refused, "{remote}");
        }
        assert_eq!(shown("C:/src/tools"), None);
        assert!(super::carries_credentials("file:///x?token=1"));
    }
}
