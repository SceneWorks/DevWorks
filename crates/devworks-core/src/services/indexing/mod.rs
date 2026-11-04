//! `IndexCoordinator` (DW-023): repository discovery, structural (Tree-sitter) baseline indexes,
//! worktree overlays and immutable generation publication behind `indexing.discover`, `.index`,
//! `.reindex`, `.refresh_overlay`, `.retire_view`, `.cancel`, `.status`, `.runs` and
//! `.configure_sources`, registered through [`crate::services::UseCases::with_indexing`]. The
//! native Settings and Repository screens and agent tools reach the same use cases through the
//! gateway; the [`worker::IndexWorker`] executes the runs they record.
//!
//! # Sections, discovery and source identity
//!
//! A section is the workspace or one project (see [`config`]). [`discover::Discovery`] scans
//! each section's repository base directory (`repositories.baseDirectory`, SET-07/08) for Git
//! checkouts (bounded depth and entry count, never following links, never descending into a
//! found repository), registers new ones through the projects port (`insert_repository`) and
//! records each registration's exact source identity: location, `origin` remote, `HEAD`
//! commit and branch, object format, and its worktrees. `indexing.discover` returns the
//! registrations of the requested sections; the runtime runs a scan before answering it.
//!
//! # Runs
//!
//! `indexing.index` (incremental) and `.reindex` (manual rebuild) record one run per
//! repository in the command's unit of work: an active run of the same repository is reused,
//! never duplicated (a queued `Index` becomes a `Reindex` when a rebuild is asked for). The run
//! is fenced by the invocation's project: an archived or deleted project accepts no new runs,
//! and a queued run of a project archived later is cancelled at claim. `expectedSourceRevision`
//! is the revision of the request's section configuration, so a run is never recorded under a
//! configuration its caller did not see.
//!
//! The worker claims a run (owner, lease, attempt), resolves the commit, inventories its tree,
//! stages units in bounded batches with checkpoints (a resumed attempt continues from the units
//! already staged), links unresolved references by name, seals the generation and publishes it.
//! A transient failure is retried with backoff up to the run's attempt limit; a deterministic
//! one fails at once and is never retried. `indexing.cancel` cancels a queued run immediately
//! and asks a running one to stop; the worker kills its helper processes and records the
//! cancellation. Every stage, warning and outcome is written to the `indexing.repository`
//! diagnostics channel with the run's operation, target, commit and attempt.
//!
//! # Publication
//!
//! A generation is complete before it is visible: sealing checks every planned unit is staged,
//! then one unit of work inserts the immutable view, moves the repository's published pointer
//! by compare-and-set from the revision the run planned against (a slower or stale builder
//! never overwrites a newer pointer), registers the view and its README and structural summary
//! texts, marks overlays of the old baseline stale and finishes the run. Readers only ever see
//! one published generation. Earlier generations are retired once no view, overlay or pin needs
//! them and reclaimed in bounded batches.
//!
//! # Overlays
//!
//! `indexing.refresh_overlay` builds the view of one worktree over the repository's published
//! baseline: a branch layer for the commits between the baseline and the worktree `HEAD`, and a
//! worktree layer for its staged, unstaged and indexable untracked changes (see [`overlay`]).
//! An overlay is identified by repository, baseline generation and commit, `HEAD`, worktree and
//! capture digest; a request with the same snapshot reuses it, anything else rebuilds it. From
//! schema revision 2 a refresh may name a ref (a branch, tag or full commit) instead of a
//! worktree: the ref is resolved to its current commit, the overlay has only the branch layer
//! of that commit and is identified by repository and commit, and a live one is reused. States
//! are `Queued`, `Indexing`, `Ready`, `Failed`, `Stale`, `Expired` and `Unavailable`.
//! `indexing.retire_view` retires an overlay (its derived view and generations go; its record and
//! history stay). Overlays expire after their retention (72 hours by default, separate from
//! artifact retention) and are rebuilt on request.
//!
//! # Retained CodeGraph behavior
//!
//! `IndexerController.ProcessRepositories`/`ReIndexAll`/`Discover`/`ListRuns`/`GetRun`/`CancelRun`,
//! `SettingsController` processing, discovery, section and exclusion endpoints, the overlay
//! endpoints and the `request_overlay`/`get_overlay_status` tools map to these operations. Kept:
//! the `queued`/`running`/`completed`/`failed`/`canceled` run lifecycle (as `OperationState`),
//! cooperative cancellation (queued runs cancel at once), one active run per target with
//! request dedupe, bounded attempts (3) with deterministic failures not retried, the default
//! skip patterns and 512 KiB size bound (2 MiB for Swift), case-insensitive glob exclusions,
//! repository and group exclusions, overlay reuse of a live snapshot and its 72-hour TTL,
//! direct base..head diffs with rename detection, renames recorded as delete plus add with the
//! previous path, and the retained call confidences (0.7, 0.65, 0.45). Changed: baselines read
//! the exact commit (CodeGraph read the working tree under the HEAD label), deletions and
//! renames are reconciled (CodeGraph left stale nodes), progress is reported, overlays include
//! uncommitted worktree changes, and inferred or unresolved relationships are marked instead of
//! dropped.
pub mod config;
pub mod discover;
pub mod git;
pub mod helper;
pub(crate) mod linking;
pub mod overlay;
pub mod paths;
pub mod status;
pub mod views;
pub mod worker;

use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, indexing as iops};
use crate::ports::indexing::{SemanticEnricher, SourceTree, StructuralParser};
use crate::ports::storage::*;
use crate::ports::{
    Clock, CommandHandler, Decided, HandlerFailure, IdKind, IdSource, Preparation, Prepared,
    QueryAnswer, QueryHandler,
};
use crate::protocol::data::{DataScope, IndexCommand, ViewSelection};
use crate::protocol::records::{
    IndexRunKind, IndexRunRecord, IndexRunState, IndexSourceConfig, MutationSummary,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Page};
use crate::protocol::{
    ActorId, CorrelationId, Counter, DomainErrorCode, EventId, Id, ProjectId, Revision,
    RevisionDimension,
};
use crate::services::artifacts::ArtifactService;
use crate::services::cursor::CursorBinding;
use crate::services::diagnostics::DiagnosticService;
use crate::services::projects::{repository_record, visible_project, visible_repository};
use crate::services::runs::closed_to_work;
use crate::services::sessions::{Failure, Refusal, decided, not_found, query_fault, rev};
use config::{EffectiveConfig, default_config, parse_exclusion};
use helper::HelperRunner;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// The operations [`IndexCoordinator`] is registered for.
pub const OPERATIONS: [&str; 9] = [
    "indexing.discover",
    "indexing.index",
    "indexing.reindex",
    "indexing.refresh_overlay",
    "indexing.retire_view",
    "indexing.cancel",
    "indexing.status",
    "indexing.runs",
    "indexing.configure_sources",
];
/// History aggregate of index runs (`aggregate_id` = run ID).
pub const RUN_AGGREGATE: &str = "index_run";
/// History aggregate of a section's index configuration.
pub const CONFIG_AGGREGATE: &str = "index_source_config";
/// History aggregate of overlays.
pub const OVERLAY_AGGREGATE: &str = "index_overlay";
/// Most repositories one index request names (the retained `ProcessRepositories` bound).
pub const MAX_REQUEST_REPOSITORIES: usize = 128;

/// Engineering defaults of indexing work (CODE-08 bounds).
#[derive(Clone, Debug)]
pub struct IndexSettings {
    /// The Git program (resolved on `PATH`).
    pub git_program: String,
    /// Deadline of one Git call.
    pub git_deadline: Duration,
    /// Lease a run claim holds between checkpoints. Only another boot reclaims an expired
    /// lease (boot recovery expires an earlier boot's claims at once), so it is generous: a
    /// single long Git read never loses its run.
    pub lease_ms: i64,
    /// Claims a run gets before a transient failure becomes terminal (the retained 3).
    pub max_attempts: i64,
    /// First retry delay; doubles per attempt.
    pub retry_base_ms: i64,
    /// Units per staging batch.
    pub batch_units: usize,
    /// Blob bytes per staging batch (bounds memory).
    pub batch_bytes: usize,
    /// Parse budget of one file.
    pub file_budget_ms: u64,
    /// Overlay retention from its last refresh (the retained 72 hours).
    pub overlay_ttl_ms: i64,
    /// Ended ref-overlay records one housekeeping pass prunes at most (one short write each).
    pub overlay_prune_batch: u16,
    /// Baseline views kept besides the published one (for exact pins and overlays).
    pub retained_baselines: usize,
    /// Discovery depth below a base directory and the most directories one scan visits.
    pub discovery_depth: usize,
    pub discovery_max_dirs: usize,
    /// Largest README published as a repository text.
    pub readme_max_bytes: usize,
}
impl Default for IndexSettings {
    fn default() -> Self {
        Self {
            git_program: "git".into(),
            git_deadline: Duration::from_secs(300),
            lease_ms: 600_000,
            max_attempts: 3,
            retry_base_ms: 5_000,
            batch_units: 64,
            batch_bytes: 16 * 1024 * 1024,
            file_budget_ms: 10_000,
            overlay_ttl_ms: 72 * 3_600_000,
            overlay_prune_batch: 16,
            retained_baselines: 1,
            discovery_depth: 6,
            discovery_max_dirs: 20_000,
            readme_max_bytes: 1024 * 1024,
        }
    }
}

/// What every indexing component shares.
pub struct IndexEnv {
    pub store: Arc<dyn ApplicationStore>,
    pub parser: Arc<dyn StructuralParser>,
    pub source: Arc<dyn SourceTree>,
    pub helpers: Arc<dyn HelperRunner>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdSource>,
    /// Publishes README and summary texts (none: texts are not published).
    pub artifacts: Option<Arc<ArtifactService>>,
    /// The `indexing.repository` log channel (none: runs are not logged).
    pub diagnostics: Option<Arc<DiagnosticService>>,
    /// Installed semantic language packs (items 24–27 register here).
    pub enrichers: RwLock<Vec<Arc<dyn SemanticEnricher>>>,
    pub settings: IndexSettings,
}
impl IndexEnv {
    pub fn now(&self) -> i64 {
        self.clock.now_ms().get()
    }
    /// Registers a semantic pack (its contributions join the next generation it applies to).
    pub fn register_enricher(&self, enricher: Arc<dyn SemanticEnricher>) {
        self.enrichers
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .push(enricher);
    }
    pub fn enrichers(&self) -> Vec<Arc<dyn SemanticEnricher>> {
        self.enrichers
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    /// The host invocation helper processes of `project`'s work are launched under (the process
    /// owner fences it by the project's archival).
    pub fn host_context(&self, project: Option<&ProjectId>) -> InvocationContext {
        let info = self.store.info();
        InvocationContext::new(
            info.host_actor.clone(),
            PrincipalKind::Host,
            self.ids.next_id(IdKind::Correlation),
            Id::new("indexing").expect("valid"),
            info.store_id.clone(),
            Counter::new(info.restore_epoch.max(0)).expect("non-negative"),
            Revision::new(1).expect("positive"),
        )
        .with_lineage(project.cloned(), None, None)
    }
}

// ---------------------------------------------------------------------------------------------
// Shared helpers

fn refuse(code: DomainErrorCode, kind: &'static str, id: Option<&Id>) -> Failure {
    Refusal::new(code, kind, id.cloned()).into()
}

/// The section key of a scope.
pub fn section_key(scope: Option<&ProjectId>) -> String {
    scope.map_or_else(
        || WORKSPACE_SCOPE_KEY.to_owned(),
        |p| p.0.as_str().to_owned(),
    )
}

/// A section's stored configuration or its defaults.
pub fn section_config(read: &dyn IndexRead, key: &str) -> Result<StoredSourceConfig, StorageError> {
    Ok(read
        .source_config(key)?
        .unwrap_or_else(|| default_config(key)))
}

/// The effective configuration of a repository (workspace plus every project it belongs to).
pub fn effective_config(
    indexing: &dyn IndexRead,
    projects: &dyn ProjectRead,
    repository: &Id,
) -> Result<EffectiveConfig, StorageError> {
    let workspace = section_config(indexing, WORKSPACE_SCOPE_KEY)?;
    let mut sections = Vec::new();
    for p in projects.repository_projects(repository)? {
        sections.push(section_config(indexing, p.0.as_str())?);
    }
    Ok(EffectiveConfig::combine(&workspace, &sections))
}

/// [`effective_config`] inside a unit of work.
pub fn effective_config_tx(
    tx: &mut dyn WriteTransaction,
    repository: &Id,
) -> Result<EffectiveConfig, StorageError> {
    let projects = tx.projects().repository_projects(repository)?;
    let workspace = section_config(tx.indexing(), WORKSPACE_SCOPE_KEY)?;
    let mut sections = Vec::new();
    for p in projects {
        sections.push(section_config(tx.indexing(), p.0.as_str())?);
    }
    Ok(EffectiveConfig::combine(&workspace, &sections))
}

/// The invocation's request scope: `None` for the workspace, the project otherwise. A project
/// invocation cannot address the workspace; a closed project accepts no index work.
fn request_scope(
    projects: &dyn ProjectRead,
    ctx: &InvocationContext,
    scope: &DataScope,
    work: bool,
) -> Result<Option<StoredProject>, Failure> {
    let project = match (scope, ctx.project_id()) {
        (DataScope::Workspace, None) => None,
        (DataScope::Workspace, Some(_)) => {
            return Err(refuse(
                DomainErrorCode::PermissionDenied,
                "workspace_scope",
                None,
            ));
        }
        (DataScope::Project { project_id }, _) => Some(visible_project(projects, ctx, project_id)?),
    };
    if work {
        let closed = |p: &StoredProject| closed_to_work(&p.lifecycle);
        if let Some(p) = &project
            && closed(p)
        {
            return Err(refuse(
                DomainErrorCode::Conflict,
                "project_archived",
                Some(&p.id.0),
            ));
        }
        if let Some(own) = ctx.project_id()
            && projects.project(own)?.is_none_or(|p| closed(&p))
        {
            return Err(refuse(
                DomainErrorCode::Conflict,
                "project_archived",
                Some(&own.0),
            ));
        }
    }
    Ok(project)
}

fn history(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    project: Option<&ProjectId>,
    aggregate: (&str, &Id, i64),
    kind: &str,
    payload: Value,
) -> Result<(), StorageError> {
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id,
        actor: actor.clone(),
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

/// A new queued run.
#[allow(clippy::too_many_arguments)]
pub(crate) fn new_run(
    tx: &mut dyn WriteTransaction,
    kind: &str,
    repository: &Id,
    project: Option<&ProjectId>,
    overlay: Option<&Id>,
    config: &EffectiveConfig,
    config_revision: i64,
    actor: &ActorId,
    max_attempts: i64,
) -> StoredIndexRun {
    let now = tx.now_ms();
    let id = tx.new_id(IdKind::Operation);
    StoredIndexRun {
        batch_id: id.clone(),
        id,
        kind: kind.into(),
        repository_id: repository.clone(),
        project_id: project.cloned(),
        overlay_id: overlay.cloned(),
        state: RUN_QUEUED.into(),
        stage: "queued".into(),
        revision: 1,
        attempt: 0,
        max_attempts,
        owner: None,
        lease_until_ms: None,
        cancel_requested_at_ms: None,
        config_revision: config_revision.max(1),
        config_digest: config.digest.clone(),
        source_oid: None,
        generation_id: None,
        completed_units: 0,
        total_units: None,
        output_view_id: None,
        error_code: None,
        error_message: None,
        deterministic: false,
        warnings: Vec::new(),
        requested_by: actor.clone(),
        created_at_ms: now,
        started_at_ms: None,
        finished_at_ms: None,
        next_attempt_at_ms: now,
    }
}

// ---------------------------------------------------------------------------------------------
// The use cases

/// The indexing use cases (one implementation for the UI and agent tools).
pub struct IndexCoordinator {
    env: Arc<IndexEnv>,
}

impl IndexCoordinator {
    pub fn new(env: Arc<IndexEnv>) -> Self {
        Self { env }
    }
    pub fn env(&self) -> &Arc<IndexEnv> {
        &self.env
    }
    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.env.ids.next_id(IdKind::Correlation))
    }

    // -----------------------------------------------------------------------------------------
    // Queries

    fn discover(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &iops::DiscoverInput,
    ) -> Result<(OperationResult, i64), Failure> {
        let project = request_scope(read.projects(), ctx, &i.scope, false)?;
        let scope_revision = project.as_ref().map_or(1, |p| p.scope_revision);
        // Sections: the requested roots (the workspace or project IDs), every visible section
        // when none is named.
        let mut sections: BTreeSet<String> = BTreeSet::new();
        for r in &i.roots.0 {
            let key = r.as_str();
            if key != WORKSPACE_SCOPE_KEY {
                visible_project(read.projects(), ctx, &ProjectId(r.clone()))?;
            } else if ctx.project_id().is_some() {
                return Err(refuse(
                    DomainErrorCode::PermissionDenied,
                    "workspace_scope",
                    None,
                ));
            }
            sections.insert(key.to_owned());
        }
        let filter = RepositoryFilter {
            project: project.as_ref().map(|p| p.id.clone()),
            ..RepositoryFilter::default()
        };
        let roots: Vec<&str> = sections.iter().map(String::as_str).collect();
        let roots_part = roots.join(",");
        let binding = CursorBinding::new(
            ctx,
            "indexing.discover",
            &[
                filter.project.as_ref().map_or("*", |p| p.0.as_str()),
                &roots_part,
            ],
            scope_revision,
        );
        let mut after = i
            .page
            .cursor
            .as_ref()
            .map(|c| {
                binding
                    .decode(&c.0)
                    .and_then(|p| Id::new(p).map_err(|_| StorageError::CursorExpired))
            })
            .transpose()?;
        let limit = i.page.limit.get() as usize;
        let mut items = Vec::new();
        let mut has_more = false;
        // Pages of registrations, filtered by section, until the page fills (bounded scan).
        'pages: loop {
            let rows = read
                .projects()
                .repositories(&filter, after.as_ref(), MAX_PAGE)?;
            let full = rows.len() == MAX_PAGE as usize;
            for r in rows {
                after = Some(r.id.clone());
                let section = read.indexing().repository_source(&r.id)?.map(|s| s.section);
                let wanted = match &section {
                    None => false,
                    Some(s) => sections.is_empty() || sections.contains(s),
                };
                if !wanted {
                    continue;
                }
                if items.len() == limit {
                    has_more = true;
                    break 'pages;
                }
                items.push(repository_record(read.projects(), ctx, &r)?);
            }
            if !full {
                break;
            }
        }
        let next = has_more.then(|| {
            let last = &items[limit - 1];
            BoundedText(binding.encode(last.id.as_str()))
        });
        Ok((
            OperationResult::IndexingDiscover {
                value: Page {
                    items: BoundedList(items),
                    next_cursor: next,
                    has_more,
                },
            },
            scope_revision,
        ))
    }

    fn runs(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &iops::RunsInput,
    ) -> Result<(OperationResult, i64), Failure> {
        let project = request_scope(read.projects(), ctx, &i.scope, false)?;
        let scope_revision = project.as_ref().map_or(1, |p| p.scope_revision);
        let mut filter = RunFilter::default();
        if let Some(r) = &i.repository_id {
            let repo = visible_repository(read.projects(), ctx, r)?;
            if let Some(p) = &project
                && !read.projects().members(&p.id)?.contains(&repo.id)
            {
                return Err(not_found("repository", r).into());
            }
            filter.repository = Some(repo.id);
        } else if let Some(p) = &project {
            filter.repositories = Some(read.projects().members(&p.id)?);
        }
        // Schema revision 2: only runs in the listed states and of the listed kinds (sets).
        let set = |names: Vec<&'static str>| {
            let mut names: Vec<String> = names.into_iter().map(str::to_owned).collect();
            names.sort();
            names.dedup();
            names
        };
        filter.states = i.states.as_ref().map(|s| {
            set(s
                .0
                .iter()
                .map(|x| match x {
                    IndexRunState::Queued => RUN_QUEUED,
                    IndexRunState::Running => RUN_RUNNING,
                    IndexRunState::Succeeded => RUN_SUCCEEDED,
                    IndexRunState::Failed => RUN_FAILED,
                    IndexRunState::Cancelled => RUN_CANCELLED,
                })
                .collect())
        });
        filter.kinds = i.kinds.as_ref().map(|k| {
            set(k
                .0
                .iter()
                .map(|x| match x {
                    IndexRunKind::Index => RUN_INDEX,
                    IndexRunKind::Reindex => RUN_REINDEX,
                    IndexRunKind::Overlay => RUN_OVERLAY,
                })
                .collect())
        });
        let states_part = filter.states.as_ref().map_or("*".into(), |s| s.join(","));
        let kinds_part = filter.kinds.as_ref().map_or("*".into(), |k| k.join(","));
        let binding = CursorBinding::new(
            ctx,
            "indexing.runs",
            &[
                project.as_ref().map_or("*", |p| p.id.0.as_str()),
                filter.repository.as_ref().map_or("*", Id::as_str),
                &states_part,
                &kinds_part,
            ],
            scope_revision,
        );
        let after = match &i.page.cursor {
            None => None,
            Some(c) => {
                let pos = binding.decode(&c.0)?;
                let (at, id) = pos.split_once(':').ok_or(StorageError::CursorExpired)?;
                Some(RunPosition {
                    created_at_ms: at.parse().map_err(|_| StorageError::CursorExpired)?,
                    id: Id::new(id).map_err(|_| StorageError::CursorExpired)?,
                })
            }
        };
        let limit = i.page.limit.get() as usize;
        let rows = read
            .indexing()
            .runs(&filter, after.as_ref(), limit as u16)?;
        let has_more = rows.len() == limit && {
            let last = &rows[limit - 1];
            !read
                .indexing()
                .runs(
                    &filter,
                    Some(&RunPosition {
                        created_at_ms: last.created_at_ms,
                        id: last.id.clone(),
                    }),
                    1,
                )?
                .is_empty()
        };
        let next = has_more.then(|| {
            let last = &rows[limit - 1];
            BoundedText(binding.encode(&format!("{}:{}", last.created_at_ms, last.id.as_str())))
        });
        let items = rows.iter().map(status::run_record).collect();
        Ok((
            OperationResult::IndexingRuns {
                value: Page {
                    items: BoundedList(items),
                    next_cursor: next,
                    has_more,
                },
            },
            scope_revision,
        ))
    }

    // -----------------------------------------------------------------------------------------
    // Commands

    #[allow(clippy::too_many_arguments)]
    fn index(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        scope: &DataScope,
        repository_ids: &[Id],
        expected: i64,
        views: &ViewSelection,
        rebuild: bool,
    ) -> Result<DomainDecision, Failure> {
        let project = request_scope(tx.projects(), ctx, scope, true)?;
        if repository_ids.len() > MAX_REQUEST_REPOSITORIES {
            return Err(refuse(
                DomainErrorCode::InvalidArgument,
                "repository_ids",
                None,
            ));
        }
        let mut ids: Vec<Id> = repository_ids.to_vec();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        if ids.windows(2).any(|w| w[0] == w[1]) {
            return Err(refuse(
                DomainErrorCode::InvalidArgument,
                "repository_ids",
                None,
            ));
        }
        // The baseline of exactly the named repositories is what an index run builds.
        match views {
            ViewSelection::Published { repository_ids: v } => {
                let mut v = v.clone();
                v.sort_by(|a, b| a.as_str().cmp(b.as_str()));
                if v != ids {
                    return Err(refuse(DomainErrorCode::InvalidArgument, "views", None));
                }
            }
            _ => return Err(refuse(DomainErrorCode::InvalidArgument, "views", None)),
        }
        let section = section_key(project.as_ref().map(|p| &p.id));
        let current = section_config(tx.indexing(), &section)?;
        if current.revision != expected {
            return Err(Failure::Storage(StorageError::conflict(
                RevisionDimension::Configuration,
                "index_source_config",
                Id::new(section.clone()).expect("valid section key"),
                rev(expected),
                rev(current.revision),
            )));
        }
        let kind = if rebuild { RUN_REINDEX } else { RUN_INDEX };
        let mut runs = Vec::new();
        let mut changed = false;
        let mut batch: Option<Id> = None;
        for id in &ids {
            let r = visible_repository(tx.projects(), ctx, id)?;
            if let Some(p) = &project
                && !tx.projects().members(&p.id)?.contains(&r.id)
            {
                return Err(not_found("repository", id).into());
            }
            if r.lifecycle != ACTIVE {
                return Err(refuse(
                    DomainErrorCode::Conflict,
                    "repository_archived",
                    Some(id),
                ));
            }
            let effective = effective_config_tx(tx, &r.id)?;
            if effective.repository_excluded(&r.name, &discover::group_path(tx, &r)?) {
                return Err(refuse(
                    DomainErrorCode::Conflict,
                    "repository_excluded",
                    Some(id),
                ));
            }
            match tx.indexing().active_run(&r.id, None)? {
                Some(mut existing) => {
                    if rebuild && existing.kind == RUN_INDEX && existing.state == RUN_QUEUED {
                        let expected = existing.revision;
                        existing.kind = RUN_REINDEX.into();
                        existing.revision += 1;
                        tx.indexing().update_run(&existing, expected)?;
                        changed = true;
                    }
                    runs.push(existing);
                }
                None => {
                    let mut run = new_run(
                        tx,
                        kind,
                        &r.id,
                        project.as_ref().map(|p| &p.id),
                        None,
                        &effective,
                        current.revision,
                        ctx.principal(),
                        self.env.settings.max_attempts,
                    );
                    // Every run this request records joins the batch of its first one.
                    if let Some(b) = &batch {
                        run.batch_id = b.clone();
                    } else {
                        batch = Some(run.id.clone());
                    }
                    tx.indexing().insert_run(&run)?;
                    history(
                        tx,
                        ctx.principal(),
                        project.as_ref().map(|p| &p.id),
                        (RUN_AGGREGATE, &run.id, run.revision),
                        "IndexRunQueued",
                        json!({
                            "kind": kind, "repositoryId": r.id.as_str(),
                            "configRevision": current.revision, "configDigest": effective.digest,
                            "principalKind": format!("{:?}", ctx.principal_kind()),
                        }),
                    )?;
                    changed = true;
                    runs.push(run);
                }
            }
        }
        let record = status::batch_record(&runs);
        let result = if rebuild {
            OperationResult::IndexingReindex { value: record }
        } else {
            OperationResult::IndexingIndex { value: record }
        };
        Ok(if changed {
            DomainDecision::Applied { result }
        } else {
            DomainDecision::NoChange { result }
        })
    }

    fn configure(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &iops::ConfigureSourcesInput,
    ) -> Result<DomainDecision, Failure> {
        let project = request_scope(tx.projects(), ctx, &i.scope, true)?;
        let key = section_key(project.as_ref().map(|p| &p.id));
        let current = section_config(tx.indexing(), &key)?;
        let wanted = self.check_config(&key, &i.config)?;
        let expected = i.expected_revision.get();
        let summary_id = Id::new(key.clone()).expect("valid section key");
        if current.revision != expected {
            return Err(Failure::Storage(StorageError::conflict(
                RevisionDimension::Configuration,
                "index_source_config",
                summary_id,
                rev(expected),
                rev(current.revision),
            )));
        }
        let same = current.structural_languages == wanted.structural_languages
            && current.semantic_packages == wanted.semantic_packages
            && current.exclude_patterns == wanted.exclude_patterns
            && current.max_file_bytes == wanted.max_file_bytes;
        if same {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::IndexingConfigureSources {
                    value: MutationSummary {
                        id: summary_id,
                        revision: rev(current.revision),
                        changed: false,
                    },
                },
            });
        }
        let next = StoredSourceConfig {
            revision: current.revision + 1,
            updated_at_ms: tx.now_ms(),
            ..wanted
        };
        tx.indexing().put_source_config(&next, current.revision)?;
        // Ready overlays built under the previous configuration are stale from this write on
        // (a project section covers its members; the workspace section every repository).
        let affected: Vec<Id> = match &project {
            Some(p) => tx.projects().members(&p.id)?,
            None => {
                let mut all = Vec::new();
                let mut after = None;
                loop {
                    let page = tx.projects().repositories(
                        &RepositoryFilter::default(),
                        after.as_ref(),
                        MAX_PAGE,
                    )?;
                    all.extend(page.iter().map(|r| r.id.clone()));
                    if page.len() < MAX_PAGE as usize {
                        break;
                    }
                    after = page.last().map(|r| r.id.clone());
                }
                all
            }
        };
        overlay::invalidate_for_configuration(tx, ctx, &affected)?;
        history(
            tx,
            ctx.principal(),
            project.as_ref().map(|p| &p.id),
            (CONFIG_AGGREGATE, &summary_id, next.revision),
            "IndexSourcesConfigured",
            json!({
                "section": key,
                "structuralLanguages": next.structural_languages,
                "semanticPackages": next.semantic_packages,
                "excludePatterns": next.exclude_patterns,
                "maxFileBytes": next.max_file_bytes,
                "principalKind": format!("{:?}", ctx.principal_kind()),
            }),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::IndexingConfigureSources {
                value: MutationSummary {
                    id: summary_id,
                    revision: rev(next.revision),
                    changed: true,
                },
            },
        })
    }

    /// Validates a wire configuration into the stored form (sorted, deduplicated sets).
    fn check_config(
        &self,
        key: &str,
        c: &IndexSourceConfig,
    ) -> Result<StoredSourceConfig, Failure> {
        let mut languages: Vec<String> = c
            .structural_languages
            .0
            .iter()
            .map(|l| l.0.to_lowercase())
            .collect();
        languages.sort();
        languages.dedup();
        for l in &languages {
            if !self
                .env
                .parser
                .languages()
                .iter()
                .any(|x| x.id == l.as_str())
            {
                return Err(refuse(
                    DomainErrorCode::InvalidArgument,
                    "structural_languages",
                    None,
                ));
            }
        }
        let mut packages: Vec<String> = c
            .semantic_package_ids
            .0
            .iter()
            .map(|p| p.as_str().to_owned())
            .collect();
        packages.sort();
        packages.dedup();
        let mut excludes: Vec<String> = Vec::new();
        for e in &c.exclude_patterns.0 {
            if parse_exclusion(&e.0).is_err() {
                return Err(refuse(
                    DomainErrorCode::InvalidArgument,
                    "exclude_patterns",
                    None,
                ));
            }
            if !excludes.contains(&e.0) {
                excludes.push(e.0.clone());
            }
        }
        let max = c.max_file_bytes.get();
        if !(1..=config::MAX_FILE_BYTES_LIMIT).contains(&max) {
            return Err(refuse(
                DomainErrorCode::InvalidArgument,
                "max_file_bytes",
                None,
            ));
        }
        let mut stored = default_config(key);
        stored.structural_languages = languages;
        stored.semantic_packages = packages;
        stored.exclude_patterns = excludes;
        stored.max_file_bytes = max;
        Ok(stored)
    }

    fn cancel(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        operation: &Id,
    ) -> Result<DomainDecision, Failure> {
        let Some(named) = tx.indexing().run(operation)? else {
            return Err(not_found("index_run", operation).into());
        };
        // The named run and, when it opened a request batch, the other runs of that batch.
        let mut targets = vec![named.clone()];
        if named.batch_id == named.id {
            for r in tx.indexing().batch_runs(&named.id)? {
                if r.id != named.id {
                    targets.push(r);
                }
            }
        }
        // Visibility follows each run's repository (and project, for a project invocation): an
        // invisible run is neither changed nor reported.
        let mut visible_targets = Vec::with_capacity(targets.len());
        for run in targets {
            let visible = visible_repository(tx.projects(), ctx, &run.repository_id).is_ok()
                && match ctx.project_id() {
                    None => true,
                    Some(own) => tx.projects().members(own)?.contains(&run.repository_id),
                };
            if visible {
                visible_targets.push(run);
            } else if run.id == named.id {
                return Err(not_found("index_run", operation).into());
            }
        }
        let targets = visible_targets;
        let mut changed = Vec::new();
        for run in &targets {
            if run_terminal(&run.state) || run.cancel_requested_at_ms.is_some() {
                continue;
            }
            let now = tx.now_ms();
            let mut next = run.clone();
            next.revision += 1;
            next.cancel_requested_at_ms = Some(now);
            if run.state == RUN_QUEUED {
                // Nothing was dispatched for it: cancelled at once.
                next.state = RUN_CANCELLED.into();
                next.stage = "cancelled".into();
                next.finished_at_ms = Some(now);
                worker::retire_staging(tx, run)?;
                if let Some(o) = &run.overlay_id {
                    overlay::settle_overlay(
                        tx,
                        o,
                        &run.id,
                        OVERLAY_FAILED,
                        Some("the overlay run was cancelled"),
                    )?;
                }
            } else {
                // The owner observes it at its next check, stops its helpers and records it.
                next.stage = "cancelling".into();
            }
            tx.indexing().update_run(&next, run.revision)?;
            history(
                tx,
                ctx.principal(),
                run.project_id.as_ref(),
                (RUN_AGGREGATE, &run.id, next.revision),
                "IndexRunCancelRequested",
                json!({
                    "attempt": run.attempt, "wasQueued": run.state == RUN_QUEUED,
                    "batchId": run.batch_id.as_str(),
                    "principalKind": format!("{:?}", ctx.principal_kind()),
                }),
            )?;
            changed.push(run.id.clone());
        }
        let mut current = Vec::new();
        for r in &targets {
            current.push(tx.indexing().run(&r.id)?.unwrap_or_else(|| r.clone()));
        }
        let value = if current.len() == 1 {
            status::run_record(&current[0])
        } else {
            status::batch_record(&current)
        };
        let result = OperationResult::IndexingCancel { value };
        Ok(if changed.is_empty() {
            DomainDecision::NoChange { result }
        } else {
            DomainDecision::Applied { result }
        })
    }

    fn retire(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &iops::RetireViewInput,
    ) -> Result<DomainDecision, Failure> {
        let target = &i.view_id;
        // An overlay is named by its view or its own ID.
        let found = match tx.indexing().overlay(target)? {
            Some(o) => Some(o),
            None => match tx.indexing().view(target)? {
                Some(v) => tx
                    .indexing()
                    .overlays(&v.repository_id)?
                    .into_iter()
                    .find(|o| o.view_id.as_ref() == Some(target)),
                None => None,
            },
        };
        let Some(o) = found else {
            // A baseline view: the published one is removed through `repositories.remove_index`;
            // an older one is retired here.
            let Some(v) = tx.indexing().view(target)? else {
                return Err(not_found("index_view", target).into());
            };
            visible_repository(tx.projects(), ctx, &v.repository_id)?;
            if tx
                .indexing()
                .published(&v.repository_id)?
                .is_some_and(|p| &p.view_id == target)
            {
                return Err(refuse(
                    DomainErrorCode::Conflict,
                    "published_view",
                    Some(target),
                ));
            }
            if i.expected_revision.get() != 1 {
                return Err(Failure::Storage(StorageError::conflict(
                    RevisionDimension::Source,
                    "index_view",
                    target.clone(),
                    i.expected_revision.clone(),
                    rev(1),
                )));
            }
            worker::retire_view(tx, &v)?;
            return Ok(DomainDecision::Applied {
                result: OperationResult::IndexingRetireView {
                    value: MutationSummary {
                        id: target.clone(),
                        revision: rev(1),
                        changed: true,
                    },
                },
            });
        };
        visible_repository(tx.projects(), ctx, &o.repository_id)?;
        if let Some(own) = ctx.project_id()
            && !tx.projects().members(own)?.contains(&o.repository_id)
        {
            return Err(not_found("index_view", target).into());
        }
        if o.revision != i.expected_revision.get() {
            return Err(Failure::Storage(StorageError::conflict(
                RevisionDimension::Source,
                "index_overlay",
                o.id.clone(),
                i.expected_revision.clone(),
                rev(o.revision),
            )));
        }
        if o.state == OVERLAY_UNAVAILABLE {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::IndexingRetireView {
                    value: MutationSummary {
                        id: o.id.clone(),
                        revision: rev(o.revision),
                        changed: false,
                    },
                },
            });
        }
        let revision = overlay::retire_overlay(tx, &o, "retired")?;
        history(
            tx,
            ctx.principal(),
            ctx.project_id(),
            (OVERLAY_AGGREGATE, &o.id, revision),
            "IndexOverlayRetired",
            json!({
                "worktreeId": o.worktree_id.as_ref().map(Id::as_str), "viewId": o.view_id.as_ref().map(Id::as_str),
                "historyKept": true,
                "principalKind": format!("{:?}", ctx.principal_kind()),
            }),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::IndexingRetireView {
                value: MutationSummary {
                    id: o.id.clone(),
                    revision: rev(revision),
                    changed: true,
                },
            },
        })
    }
}

impl QueryHandler for IndexCoordinator {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let own_scope = || -> Result<i64, Failure> {
            Ok(match ctx.project_id() {
                Some(p) => snapshot
                    .projects()
                    .project(p)?
                    .map_or(1, |x| x.scope_revision),
                None => 1,
            })
        };
        let revision = query.schema_revision().0;
        let answered = match query.input() {
            OperationInput::Indexing(iops::Input::Discover(i)) => self.discover(snapshot, ctx, i),
            OperationInput::Indexing(iops::Input::Runs(i)) => self.runs(snapshot, ctx, i),
            OperationInput::Indexing(iops::Input::Status(i)) => {
                match (&i.repository_id, &i.overlay_id) {
                    (_, Some(o)) => status::overlay_status(&self.env, snapshot, ctx, o, revision),
                    (Some(r), None) => status::status(&self.env, snapshot, ctx, r, revision, None),
                    (None, None) => Err(refuse(DomainErrorCode::InvalidArgument, "status", None)),
                }
                .and_then(|value| Ok((OperationResult::IndexingStatus { value }, own_scope()?)))
            }
            _ => Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "operation",
                None,
            )),
        };
        let (result, scope) =
            answered.map_err(|f| query_fault(f, "indexing", self.correlation()))?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(scope)),
            result,
        })
    }
}

use crate::protocol::DomainFault;

impl CommandHandler for IndexCoordinator {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        // The overlay snapshot (or the commit a ref names) a refresh is decided against is
        // observed before the unit of work (Git and file reads never run inside a transaction).
        if let OperationInput::Indexing(iops::Input::RefreshOverlay(r)) = command.input()
            && let IndexCommand::RefreshOverlay {
                repository_id,
                worktree_id,
                git_ref,
                ..
            } = &r.command
        {
            let stop = || env.stopped(self.env.now());
            return Ok(match (worktree_id, git_ref) {
                (Some(w), _) => Some(Prepared::new(overlay::observe(
                    &self.env,
                    ctx,
                    repository_id,
                    w,
                    &stop,
                ))),
                (None, Some(name)) => Some(Prepared::new(overlay::resolve(
                    &self.env,
                    repository_id,
                    name,
                    &stop,
                ))),
                (None, None) => None,
            });
        }
        Ok(None)
    }

    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let outcome = match command.input() {
            OperationInput::Indexing(iops::Input::Index(i)) => self.index(
                tx,
                ctx,
                &i.scope,
                &i.repository_ids.0,
                i.expected_source_revision.get(),
                &i.views,
                false,
            ),
            OperationInput::Indexing(iops::Input::Reindex(i)) => self.index(
                tx,
                ctx,
                &i.scope,
                &i.repository_ids.0,
                i.expected_source_revision.get(),
                &i.views,
                true,
            ),
            OperationInput::Indexing(iops::Input::ConfigureSources(i)) => {
                self.configure(tx, ctx, i)
            }
            OperationInput::Indexing(iops::Input::RetireView(i)) => self.retire(tx, ctx, i),
            OperationInput::Indexing(iops::Input::Cancel(r)) => match &r.command {
                IndexCommand::Cancel { operation_id } => self.cancel(tx, ctx, &operation_id.0),
                _ => Err(refuse(DomainErrorCode::InvalidArgument, "command", None)),
            },
            OperationInput::Indexing(iops::Input::RefreshOverlay(r)) => match &r.command {
                IndexCommand::RefreshOverlay {
                    repository_id,
                    worktree_id,
                    git_ref,
                    expected_source_revision,
                } => {
                    let requested = match (worktree_id, git_ref) {
                        (Some(id), _) => Some(overlay::Requested::Worktree {
                            id,
                            observed: command.prepared::<overlay::Observed>(),
                        }),
                        (None, Some(_)) => Some(overlay::Requested::Ref {
                            resolved: command.prepared::<overlay::Resolved>(),
                        }),
                        (None, None) => None,
                    };
                    match requested {
                        Some(requested) => overlay::refresh(
                            &self.env,
                            tx,
                            ctx,
                            repository_id,
                            requested,
                            expected_source_revision.get(),
                        ),
                        None => Err(refuse(DomainErrorCode::InvalidArgument, "command", None)),
                    }
                }
                _ => Err(refuse(DomainErrorCode::InvalidArgument, "command", None)),
            },
            _ => Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "operation",
                None,
            )),
        };
        decided(tx, outcome)
    }
}

/// `repositories.remove_index` of a repository (inside its unit of work): every active run of
/// it is cancelled (a running one stops at its next check) and every overlay becomes
/// unavailable. Returns how many runs and overlays it ended.
pub fn index_removed(tx: &mut dyn WriteTransaction, repository: &Id) -> Result<u64, StorageError> {
    let mut ended = 0;
    for o in tx.indexing().overlays(repository)? {
        if !matches!(o.state.as_str(), OVERLAY_UNAVAILABLE) {
            overlay::retire_overlay(tx, &o, "the repository index was removed")?;
            ended += 1;
        }
    }
    if let Some(mut run) = tx.indexing().active_run(repository, None)? {
        let now = tx.now_ms();
        let expected = run.revision;
        run.revision += 1;
        run.cancel_requested_at_ms = Some(now);
        run.error_code = Some("index_removed".into());
        run.error_message = Some("the repository index was removed".into());
        if run.state == RUN_QUEUED {
            run.state = RUN_CANCELLED.into();
            run.stage = "cancelled".into();
            run.finished_at_ms = Some(now);
        } else {
            run.stage = "cancelling".into();
        }
        tx.indexing().update_run(&run, expected)?;
        ended += 1;
    }
    Ok(ended)
}

/// The run records of an index request reply: one per repository (see [`status::batch_record`]).
pub fn record_of(run: &StoredIndexRun) -> IndexRunRecord {
    status::run_record(run)
}

/// Digest helper shared by the components.
pub(crate) fn digest(parts: &[&str]) -> String {
    sha256_hex(parts.join("\u{0}").as_bytes())
}
