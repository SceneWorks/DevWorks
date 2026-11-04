//! `IndexWorker`: claims recorded runs and executes them (baseline index or rebuild here,
//! overlays in [`super::overlay`]), with fenced checkpoints, bounded batches, cooperative
//! cancellation and atomic publication.
//!
//! # Claims and fences
//!
//! A claim moves a run to `Running` with this worker's owner identity, a lease and the next
//! attempt number in one unit of work. Every later write of the run (each checkpoint batch, the
//! seal and the publication) re-reads it inside its own unit of work and proceeds only while it
//! is still `Running` under the same owner and attempt with no cancellation requested; the same
//! write renews the lease. A worker that lost its lease (another boot reclaimed the run) or whose
//! run was cancelled therefore never writes again, and a cancellation never reaches a later run:
//! it is bound to the run's identity, and the in-memory signal also to its attempt.
//!
//! # Checkpoints and resume
//!
//! The staging generation is recorded on the run when it is created. A resumed attempt reuses it
//! (when its plan still holds) and stages only the units not yet staged; a retried batch with
//! different content under the same unit is a conflict. An abandoned staging generation is
//! retired and reclaimed in bounded batches.
//!
//! # Bounds
//!
//! Blob reads are batched by count and bytes ([`super::IndexSettings::batch_units`],
//! `batch_bytes`); a file larger than the configured bound is never read (it is recorded as an
//! oversize unit); every Git call has a deadline and output bound and runs through the process
//! owner, so cancellation kills its tree; parsing polls the stop signal and has a per-file
//! budget.
use super::config::{EXTRACTOR_VERSION, EffectiveConfig, dependency_directory};
use super::git::{Change, Git, GitFault, TreeEntry};
use super::paths::{edge_id, fold, node_id, parent, symbol_id};
use super::{IndexEnv, RUN_AGGREGATE, digest, effective_config, history};
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::indexing::{EdgeTarget, ExtractFault, FileExtract, LanguageMode};
use crate::ports::storage::*;
use crate::protocol::Id;
use crate::protocol::records::{DiagnosticErrorClass, DiagnosticOutcome, Severity};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Unit modes recorded in unit payloads.
pub const MODE_STRUCTURAL: &str = "Structural";
pub const MODE_TEXT_FALLBACK: &str = "TextFallback";
pub const MODE_TEXT: &str = "Text";
pub const MODE_NOT_SELECTED: &str = "NotSelected";
pub const MODE_OVERSIZE: &str = "Oversize";
pub const MODE_LINK: &str = "Link";
pub const MODE_SUBMODULE: &str = "Submodule";
pub const MODE_DIRECTORY: &str = "Directory";
pub const MODE_UNREADABLE: &str = "Unreadable";
/// The tree mode a capture gives a worktree file it could not read safely.
pub const UNREADABLE_MODE: &str = "000000";
/// Most warnings a run record keeps (the log keeps every one).
const MAX_WARNINGS: usize = 48;

/// Why execution stopped before success.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    Cancelled,
    /// The claim is no longer this worker's (lease lost, reclaimed): write nothing more.
    Lost,
    Failed {
        code: String,
        message: String,
        deterministic: bool,
    },
}
impl Stop {
    pub(crate) fn det(code: &str, message: impl Into<String>) -> Self {
        Self::Failed {
            code: code.into(),
            message: message.into(),
            deterministic: true,
        }
    }
    pub(crate) fn transient(code: &str, message: impl Into<String>) -> Self {
        Self::Failed {
            code: code.into(),
            message: message.into(),
            deterministic: false,
        }
    }
    pub(crate) fn storage(e: StorageError) -> Self {
        match e {
            StorageError::Busy
            | StorageError::StorageUnavailable { .. }
            | StorageError::CommitUnknown
            | StorageError::Cancelled
            | StorageError::RevisionConflict(_) => Self::transient("storage", e.to_string()),
            StorageError::LeaseLost { .. } => Self::Lost,
            other => Self::det("storage_invariant", other.to_string()),
        }
    }
    pub(crate) fn git(e: GitFault) -> Self {
        if e.cancelled() {
            return Self::Cancelled;
        }
        if e.transient() {
            return Self::transient("git", e.to_string());
        }
        Self::det("git", e.to_string())
    }
}

/// One planned unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlanUnit {
    pub key: String,
    pub digest: String,
    pub mode: &'static str,
    pub language: Option<&'static str>,
    pub size: u64,
    /// The blob to read (structural units only).
    pub oid: Option<String>,
    pub previous: Option<String>,
    /// Directory units: child keys and whether each is a directory.
    pub children: Vec<(String, bool)>,
}

/// The directory unit key of `dir` (`/` is the repository root): never a valid file path.
pub fn dir_key(dir: &str) -> String {
    format!("/{dir}")
}

/// Plans the units of `entries` under `cfg`: excluded paths are left out, and every included
/// path, its directories and the root get a unit.
pub(crate) fn plan(
    env: &IndexEnv,
    cfg: &EffectiveConfig,
    entries: &[TreeEntry],
    renamed_from: &BTreeMap<String, String>,
    warnings: &mut Vec<RunWarning>,
    case_insensitive: bool,
) -> (Vec<PlanUnit>, BTreeMap<String, i64>) {
    let present: BTreeSet<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    let mut excluded: BTreeMap<String, i64> = BTreeMap::new();
    let mut units = Vec::new();
    let mut dirs: BTreeMap<String, BTreeSet<(String, bool)>> = BTreeMap::new();
    let mut folded: BTreeMap<String, String> = BTreeMap::new();
    for e in entries {
        if cfg.path_excluded(&e.path) || dependency_directory(&e.path, &|p| present.contains(p)) {
            *excluded.entry("excluded".into()).or_default() += 1;
            continue;
        }
        if case_insensitive && let Some(other) = folded.insert(fold(&e.path), e.path.clone()) {
            push_warning(
                warnings,
                "case_collision",
                format!(
                    "{} and {other} name the same file on this file system",
                    e.path
                ),
            );
        }
        let language = env.parser.language_of(&e.path);
        let (mode, lang) = if e.submodule() {
            (MODE_SUBMODULE, None)
        } else if e.mode == UNREADABLE_MODE {
            (MODE_UNREADABLE, None)
        } else if e.link() {
            (MODE_LINK, None)
        } else {
            match language {
                Some(l) if !cfg.language_selected(l.id) => (MODE_NOT_SELECTED, Some(l.id)),
                Some(l) if l.mode == LanguageMode::TextFallback => (MODE_TEXT_FALLBACK, Some(l.id)),
                Some(l) if e.size as i64 > cfg.max_bytes_for(&e.path) => {
                    (MODE_OVERSIZE, Some(l.id))
                }
                Some(l) => (MODE_STRUCTURAL, Some(l.id)),
                None => (MODE_TEXT, None),
            }
        };
        units.push(PlanUnit {
            key: e.path.clone(),
            digest: e.oid.clone(),
            mode,
            language: lang,
            size: e.size,
            oid: (mode == MODE_STRUCTURAL).then(|| e.oid.clone()),
            previous: renamed_from.get(&e.path).cloned(),
            children: Vec::new(),
        });
        // Every ancestor directory lists its child.
        let mut child = (e.path.clone(), false);
        let mut dir = parent(&e.path).to_owned();
        loop {
            let new = dirs.entry(dir.clone()).or_default().insert(child.clone());
            if dir.is_empty() || !new {
                break;
            }
            child = (dir.clone(), true);
            dir = parent(&dir).to_owned();
        }
    }
    dirs.entry(String::new()).or_default();
    for (dir, children) in dirs {
        let children: Vec<(String, bool)> = children.into_iter().collect();
        let basis: Vec<String> = children.iter().map(|(c, d)| format!("{c}:{d}")).collect();
        units.push(PlanUnit {
            key: dir_key(&dir),
            digest: sha256_hex(basis.join("\u{0}").as_bytes()),
            mode: MODE_DIRECTORY,
            language: None,
            size: 0,
            oid: None,
            previous: None,
            children,
        });
    }
    units.sort_by(|a, b| a.key.cmp(&b.key));
    (units, excluded)
}

pub(crate) fn push_warning(w: &mut Vec<RunWarning>, code: &str, message: String) {
    if w.len() < MAX_WARNINGS {
        w.push(RunWarning {
            code: code.into(),
            message: message.chars().take(1024).collect(),
        });
    }
}

/// The unit payload of a planned unit.
pub(crate) fn unit_payload(u: &PlanUnit, parse_errors: u32, extra: Option<&str>) -> Value {
    let mut p = json!({
        "mode": u.mode,
        "size": u.size,
        "extractorVersion": EXTRACTOR_VERSION,
    });
    if let Some(l) = u.language {
        p["language"] = json!(l);
    }
    if parse_errors > 0 {
        p["parseErrors"] = json!(parse_errors);
    }
    if let Some(prev) = &u.previous {
        p["previousPath"] = json!(prev);
    }
    if let Some(x) = extra {
        p["note"] = json!(x);
    }
    p
}

/// The stable ID of a file's node.
pub fn file_node_id(repository: &str, key: &str) -> String {
    node_id(repository, key, "File", key, 0)
}
/// The stable ID of a directory's node (`""`: the repository node).
pub fn dir_node_id(repository: &str, dir: &str) -> String {
    let key = dir_key(dir);
    node_id(
        repository,
        &key,
        if dir.is_empty() {
            "Repository"
        } else {
            "Folder"
        },
        dir,
        0,
    )
}

/// Converts one file's extraction into stored rows (stable IDs, provenance).
pub(crate) fn rows_of(
    repository: &str,
    key: &str,
    extract: &FileExtract,
    language: Option<&str>,
    grammar: Option<&str>,
    extractor: &str,
) -> (Vec<StoredNode>, Vec<StoredEdge>) {
    let mut ids: Vec<String> = Vec::with_capacity(extract.nodes.len());
    let mut seen: HashMap<(String, String), usize> = HashMap::new();
    let mut nodes = Vec::with_capacity(extract.nodes.len());
    for (i, n) in extract.nodes.iter().enumerate() {
        let ordinal = {
            let c = seen
                .entry((n.kind.clone(), n.qualified_name.clone()))
                .or_default();
            let o = *c;
            *c += 1;
            o
        };
        let id = if i == 0 {
            file_node_id(repository, key)
        } else {
            node_id(repository, key, &n.kind, &n.qualified_name, ordinal)
        };
        ids.push(id);
    }
    for (i, n) in extract.nodes.iter().enumerate() {
        let span = json!({
            "startLine": n.span.start_line, "startColumn": n.span.start_column,
            "endLine": n.span.end_line, "endColumn": n.span.end_column,
            "startByte": n.span.start_byte, "endByte": n.span.end_byte,
        });
        let mut payload = json!({
            "qualifiedName": n.qualified_name,
            "span": span,
            "properties": Value::Object(n.properties.clone()),
            "confidence": "high",
            "provenance": {"extractor": extractor, "grammar": grammar, "extractorVersion": EXTRACTOR_VERSION},
        });
        if let Some(l) = language {
            payload["language"] = json!(l);
        }
        if let Some(p) = n.parent.and_then(|p| ids.get(p)) {
            payload["parentId"] = json!(p);
        }
        nodes.push(StoredNode {
            unit_key: key.into(),
            node_id: ids[i].clone(),
            kind: n.kind.clone(),
            name: n.name.chars().take(512).collect(),
            payload,
        });
    }
    let mut edges = Vec::with_capacity(extract.edges.len());
    let mut edge_seen: HashMap<(String, String, String), usize> = HashMap::new();
    for e in &extract.edges {
        let Some(source) = ids.get(e.source) else {
            continue;
        };
        let (target, target_key, mut payload) = match &e.target {
            EdgeTarget::Local(j) => match ids.get(*j) {
                Some(t) => (
                    t.clone(),
                    t.clone(),
                    json!({"resolution": e.resolution.as_str(), "targetUnit": key}),
                ),
                None => continue,
            },
            EdgeTarget::Symbol(name) => {
                let name: String = name.chars().take(1024).collect();
                (
                    symbol_id(repository, &name),
                    name.clone(),
                    json!({"resolution": e.resolution.as_str(), "targetName": name}),
                )
            }
        };
        payload["confidence"] = json!((f64::from(e.confidence) * 100.0).round() / 100.0);
        payload["provenance"] = json!({"extractor": extractor});
        if !e.properties.is_empty() {
            payload["properties"] = Value::Object(e.properties.clone());
        }
        let ordinal = {
            let c = edge_seen
                .entry((source.clone(), e.kind.clone(), target_key.clone()))
                .or_default();
            let o = *c;
            *c += 1;
            o
        };
        edges.push(StoredEdge {
            unit_key: key.into(),
            edge_id: edge_id(source, &e.kind, &target_key, ordinal),
            source_node_id: source.clone(),
            target_node_id: target,
            kind: e.kind.clone(),
            payload,
        });
    }
    (nodes, edges)
}

/// The rows of a unit that is not parsed: its file node only.
pub(crate) fn file_only(repository: &str, u: &PlanUnit) -> Vec<StoredNode> {
    let name = u.key.rsplit('/').next().unwrap_or(&u.key).to_owned();
    vec![StoredNode {
        unit_key: u.key.clone(),
        node_id: file_node_id(repository, &u.key),
        kind: "File".into(),
        name,
        payload: json!({
            "qualifiedName": u.key, "language": u.language, "mode": u.mode, "size": u.size,
            "confidence": "high",
            "provenance": {"extractor": "inventory", "extractorVersion": EXTRACTOR_VERSION},
        }),
    }]
}

/// The rows of a directory unit: its node and containment of its children.
pub(crate) fn dir_rows(
    repository: &str,
    name: &str,
    u: &PlanUnit,
) -> (Vec<StoredNode>, Vec<StoredEdge>) {
    let dir = &u.key[1..];
    let id = dir_node_id(repository, dir);
    let label = if dir.is_empty() {
        name.to_owned()
    } else {
        dir.rsplit('/').next().unwrap_or(dir).to_owned()
    };
    let node = StoredNode {
        unit_key: u.key.clone(),
        node_id: id.clone(),
        kind: if dir.is_empty() {
            "Repository"
        } else {
            "Folder"
        }
        .into(),
        name: label,
        payload: json!({
            "qualifiedName": dir, "confidence": "high",
            "provenance": {"extractor": "inventory", "extractorVersion": EXTRACTOR_VERSION},
        }),
    };
    let mut edges = Vec::new();
    for (child, is_dir) in &u.children {
        let (kind, target, unit) = if *is_dir {
            (
                "CONTAINS_FOLDER",
                dir_node_id(repository, child),
                dir_key(child),
            )
        } else {
            (
                "CONTAINS_FILE",
                file_node_id(repository, child),
                child.clone(),
            )
        };
        edges.push(StoredEdge {
            unit_key: u.key.clone(),
            edge_id: edge_id(&id, kind, &target, 0),
            source_node_id: id.clone(),
            target_node_id: target,
            kind: kind.into(),
            payload: json!({"resolution": "Resolved", "confidence": 1.0, "targetUnit": unit,
                "provenance": {"extractor": "inventory"}}),
        });
    }
    (vec![node], edges)
}

/// Builds the stored rows of one planned unit (`content`: the blob of a structural unit).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_unit(
    env: &IndexEnv,
    repository: &Id,
    name: &str,
    generation: &Id,
    u: &PlanUnit,
    content: Option<&[u8]>,
    stop: &(dyn Fn() -> bool + Sync),
    warnings: &mut Vec<RunWarning>,
) -> Result<(StoredUnit, Vec<StoredNode>, Vec<StoredEdge>), Stop> {
    let repo = repository.as_str();
    let (nodes, edges, parse_errors, note) = match u.mode {
        MODE_DIRECTORY => {
            let (n, e) = dir_rows(repo, name, u);
            (n, e, 0, None)
        }
        MODE_STRUCTURAL => {
            let language = u.language.unwrap_or("text");
            let info = env.parser.languages().iter().find(|l| l.id == language);
            let bytes = content.unwrap_or_default();
            match env
                .parser
                .extract(language, &u.key, bytes, stop, env.settings.file_budget_ms)
            {
                Ok(x) => {
                    let errors = x.parse_errors;
                    if errors > 0 {
                        push_warning(
                            warnings,
                            "parse_errors",
                            format!("{}: {errors} syntax errors recovered", u.key),
                        );
                    }
                    let (n, e) = rows_of(
                        repo,
                        &u.key,
                        &x,
                        Some(language),
                        info.and_then(|i| i.grammar),
                        "tree-sitter",
                    );
                    (n, e, errors, None)
                }
                Err(ExtractFault::Cancelled) => return Err(Stop::Cancelled),
                Err(ExtractFault::TimedOut) => {
                    push_warning(
                        warnings,
                        "parse_timeout",
                        format!("{}: the parse budget passed", u.key),
                    );
                    (
                        file_only(repo, u),
                        Vec::new(),
                        1,
                        Some("parse budget exceeded; recorded without definitions"),
                    )
                }
                Err(ExtractFault::Failed(why)) => {
                    push_warning(warnings, "parse_failed", format!("{}: {why}", u.key));
                    (
                        file_only(repo, u),
                        Vec::new(),
                        1,
                        Some("not parsed; recorded without definitions"),
                    )
                }
            }
        }
        _ => (file_only(repo, u), Vec::new(), 0, None),
    };
    let unit = StoredUnit {
        generation_id: generation.clone(),
        unit_key: u.key.clone(),
        content_digest: u.digest.clone(),
        action: UNIT_REPLACE.into(),
        payload: unit_payload(u, parse_errors, note),
    };
    Ok((unit, nodes, edges))
}

/// Retires the generations of a run that will not publish them: its staging generation, and a
/// sealed one no view names (a publication that did not commit). Overlay runs also name their
/// branch and worktree layers in the overlay record (see `overlay::settle_overlay`).
pub(crate) fn retire_staging(
    tx: &mut dyn WriteTransaction,
    run: &StoredIndexRun,
) -> Result<(), StorageError> {
    let mut layers: Vec<Id> = run.generation_id.iter().cloned().collect();
    if let Some(o) = &run.overlay_id
        && let Some(o) = tx.indexing().overlay(o)?
        && o.run_id.as_ref() == Some(&run.id)
    {
        layers.extend(o.branch_generation_id.iter().cloned());
        layers.extend(o.worktree_generation_id.iter().cloned());
    }
    for g in layers {
        retire_unused(tx, &g)?;
    }
    Ok(())
}

/// Retires `g` when it is staging, or sealed and named by no view.
pub(crate) fn retire_unused(tx: &mut dyn WriteTransaction, g: &Id) -> Result<(), StorageError> {
    if let Some(mut stored) = tx.indexing().generation(g)? {
        let unused = match stored.state.as_str() {
            GEN_STAGING => true,
            GEN_SEALED => !tx.indexing().generation_in_view(g)?,
            _ => false,
        };
        if unused {
            stored.state = GEN_RETIRED.into();
            tx.indexing().update_generation(&stored)?;
        }
    }
    Ok(())
}

/// Retires one unpublished view: removes it (and its registry row and texts) and retires every
/// layer no other view, overlay or active run still uses.
pub(crate) fn retire_view(
    tx: &mut dyn WriteTransaction,
    v: &StoredView,
) -> Result<(), StorageError> {
    tx.indexing().remove_view(&v.view_id)?;
    let overlays = tx.indexing().overlays(&v.repository_id)?;
    let published = match tx.indexing().published(&v.repository_id)? {
        Some(p) => tx.indexing().view(&p.view_id)?,
        None => None,
    };
    for g in [
        Some(&v.baseline_id),
        v.branch_id.as_ref(),
        v.worktree_id.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        let in_use = published.as_ref().is_some_and(|p| &p.baseline_id == g)
            || overlays.iter().any(|o| {
                !matches!(o.state.as_str(), OVERLAY_EXPIRED | OVERLAY_UNAVAILABLE)
                    && o.view_id.as_ref() != Some(&v.view_id)
                    && [
                        o.base_generation_id.as_ref(),
                        o.branch_generation_id.as_ref(),
                        o.worktree_generation_id.as_ref(),
                    ]
                    .contains(&Some(g))
            });
        if in_use {
            continue;
        }
        if let Some(mut stored) = tx.indexing().generation(g)?
            && stored.state == GEN_SEALED
        {
            stored.state = GEN_RETIRED.into();
            tx.indexing().update_generation(&stored)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The worker

struct LiveRun {
    attempt: i64,
    stop: Arc<AtomicBool>,
}

/// What one housekeeping pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Housekeeping {
    pub expired_overlays: usize,
    pub reclaimed_rows: u64,
    pub stale_overlays: usize,
    pub refreshes_queued: usize,
}

pub struct IndexWorker {
    pub(crate) env: Arc<IndexEnv>,
    owner: String,
    live: Mutex<HashMap<String, LiveRun>>,
    pub(crate) stopping: AtomicBool,
    /// Where overlay observation continues at the next housekeeping pass.
    observe_cursor: Mutex<Option<Id>>,
}

impl IndexWorker {
    /// A worker claiming as `owner` (unique per boot).
    pub fn new(env: Arc<IndexEnv>, owner: String) -> Self {
        Self {
            env,
            owner,
            live: Mutex::new(HashMap::new()),
            stopping: AtomicBool::new(false),
            observe_cursor: Mutex::new(None),
        }
    }
    pub fn env(&self) -> &Arc<IndexEnv> {
        &self.env
    }
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// Boot recovery: a run left `Running` by another owner (an earlier boot) is made claimable
    /// now instead of after its lease, so it resumes from its checkpoint. Returns how many.
    pub fn recover(&self) -> Result<usize, StorageError> {
        let owner = self.owner.clone();
        run_write(&*self.env.store, WriteOptions::default(), move |tx| {
            let mut n = 0;
            for mut r in tx.indexing().running_runs(MAX_PAGE)? {
                if r.owner.as_deref() == Some(owner.as_str()) {
                    continue;
                }
                let expected = r.revision;
                r.revision += 1;
                r.lease_until_ms = Some(0);
                r.stage = "interrupted".into();
                tx.indexing().update_run(&r, expected)?;
                n += 1;
            }
            Ok(n)
        })
    }

    /// Signals the run's in-memory stop (after a committed `indexing.cancel`); the durable
    /// request is what the fence checks.
    pub fn cancel_local(&self, run: &Id) {
        if let Some(l) = self
            .live
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(run.as_str())
        {
            l.stop.store(true, Ordering::SeqCst);
        }
    }
    /// Stops every run this worker executes (shutdown) and claims nothing new. The runs keep
    /// their claims and checkpoints: a stopped worker is not a cancelled run, and the next boot
    /// resumes them.
    pub fn stop_all(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }
    /// Runs being executed right now.
    pub fn executing(&self) -> usize {
        self.live.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
    pub fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Claims the oldest claimable run whose repository has no other run in progress. A run of
    /// a closed project is cancelled, a run past its attempts fails, and a cancelled one settles.
    pub fn claim(&self) -> Result<Option<StoredIndexRun>, StorageError> {
        if self.stopping() {
            return Ok(None);
        }
        let owner = self.owner.clone();
        let lease = self.env.settings.lease_ms;
        let executing: Vec<String> = self
            .live
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect();
        let claimed = run_write(&*self.env.store, WriteOptions::default(), move |tx| {
            let now = tx.now_ms();
            let busy: BTreeSet<String> = tx
                .indexing()
                .running_runs(MAX_PAGE)?
                .into_iter()
                .filter(|r| r.lease_until_ms.is_some_and(|l| l >= now))
                .map(|r| r.repository_id.as_str().to_owned())
                .collect();
            for mut r in tx.indexing().claimable_runs(now, 64)? {
                if executing.contains(&r.id.as_str().to_owned())
                    || busy.contains(r.repository_id.as_str())
                {
                    continue;
                }
                let expected = r.revision;
                r.revision += 1;
                let closed = match &r.project_id {
                    Some(p) => tx
                        .projects()
                        .project(p)?
                        .is_none_or(|p| crate::services::runs::closed_to_work(&p.lifecycle)),
                    None => false,
                };
                let terminal = |r: &mut StoredIndexRun, state: &str, stage: &str| {
                    r.state = state.into();
                    r.stage = stage.into();
                    r.owner = None;
                    r.lease_until_ms = None;
                    r.finished_at_ms = Some(now);
                };
                if r.cancel_requested_at_ms.is_some() || closed {
                    if closed {
                        r.error_code = Some("project_archived".into());
                        r.error_message = Some("the project was archived or deleted".into());
                    }
                    terminal(&mut r, RUN_CANCELLED, "cancelled");
                    retire_staging(tx, &r)?;
                    if let Some(o) = r.overlay_id.clone() {
                        super::overlay::settle_overlay(
                            tx,
                            &o,
                            &r.id,
                            OVERLAY_FAILED,
                            Some("the overlay run was cancelled"),
                        )?;
                    }
                    tx.indexing().update_run(&r, expected)?;
                    continue;
                }
                if r.attempt >= r.max_attempts {
                    r.error_code = Some(
                        r.error_code
                            .clone()
                            .unwrap_or_else(|| "attempts_exhausted".into()),
                    );
                    r.error_message = Some(format!(
                        "no attempt succeeded ({} of {}); last error: {}",
                        r.attempt,
                        r.max_attempts,
                        r.error_message
                            .clone()
                            .unwrap_or_else(|| "lease expired".into())
                    ));
                    terminal(&mut r, RUN_FAILED, "failed");
                    retire_staging(tx, &r)?;
                    if let Some(o) = r.overlay_id.clone() {
                        super::overlay::settle_overlay(
                            tx,
                            &o,
                            &r.id,
                            OVERLAY_FAILED,
                            r.error_message.as_deref(),
                        )?;
                    }
                    tx.indexing().update_run(&r, expected)?;
                    continue;
                }
                r.state = RUN_RUNNING.into();
                r.stage = "claimed".into();
                r.attempt += 1;
                r.owner = Some(owner.clone());
                r.lease_until_ms = Some(now + lease);
                r.started_at_ms = r.started_at_ms.or(Some(now));
                tx.indexing().update_run(&r, expected)?;
                if let Some(o) = r.overlay_id.clone() {
                    super::overlay::mark_indexing(tx, &o, &r.id)?;
                }
                return Ok(Some(r));
            }
            Ok(None)
        })?;
        if let Some(r) = &claimed {
            self.live.lock().unwrap_or_else(|p| p.into_inner()).insert(
                r.id.as_str().to_owned(),
                LiveRun {
                    attempt: r.attempt,
                    stop: Arc::new(AtomicBool::new(false)),
                },
            );
        }
        Ok(claimed)
    }

    /// Executes a claimed run to its outcome (or until its claim is lost).
    pub fn execute(&self, run: StoredIndexRun) {
        let stop = {
            let live = self.live.lock().unwrap_or_else(|p| p.into_inner());
            match live.get(run.id.as_str()) {
                Some(l) if l.attempt == run.attempt => l.stop.clone(),
                _ => Arc::new(AtomicBool::new(false)),
            }
        };
        let mut exec = Exec {
            w: self,
            run: run.clone(),
            stop,
            ctx: self.env.host_context(run.project_id.as_ref()),
            warnings: run.warnings.clone(),
        };
        exec.log(
            "claimed",
            Severity::Info,
            &format!("attempt {} of {}", run.attempt, run.max_attempts),
            None,
        );
        let outcome = if run.kind == RUN_OVERLAY {
            super::overlay::execute(&mut exec)
        } else {
            exec.baseline()
        };
        exec.finish(outcome);
        self.live
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(run.id.as_str());
    }

    /// Expires overlays past their retention, reclaims retired generations in bounded batches and
    /// marks overlays stale (queueing their refresh) when their worktree or baseline moved.
    pub fn housekeeping(
        &self,
        observe: bool,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<Housekeeping, StorageError> {
        let mut report = Housekeeping::default();
        let due = read_snapshot(&*self.env.store, &ReadOptions::default(), |s| {
            s.indexing().overlays_due(self.env.now(), 64)
        })?;
        for o in due {
            let expired = run_write(
                &*self.env.store,
                WriteOptions::default(),
                move |tx| match tx.indexing().overlay(&o.id)? {
                    Some(cur) if cur.revision == o.revision => {
                        super::overlay::expire_overlay(tx, &cur)?;
                        Ok(true)
                    }
                    _ => Ok(false),
                },
            )?;
            report.expired_overlays += usize::from(expired);
        }
        if observe {
            let (stale, queued) =
                super::overlay::observe_all(&self.env, &self.observe_cursor, stop)?;
            report.stale_overlays = stale;
            report.refreshes_queued = queued;
        }
        let retired = read_snapshot(&*self.env.store, &ReadOptions::default(), |s| {
            s.indexing().generations_in_state(GEN_RETIRED, 16)
        })?;
        for g in retired {
            if stop() {
                break;
            }
            let id = g.id.clone();
            report.reclaimed_rows += run_write(
                &*self.env.store,
                WriteOptions {
                    priority: WritePriority::Bulk,
                    ..WriteOptions::default()
                },
                move |tx| tx.indexing().reclaim_generation(&id, 5_000),
            )?;
        }
        Ok(report)
    }
}

/// Git calls of `ctx` with the environment's program, deadline and `stop`.
pub(crate) fn git_for<'g>(
    env: &'g IndexEnv,
    ctx: &'g InvocationContext,
    stop: &'g (dyn Fn() -> bool + Sync),
) -> Git<'g> {
    Git {
        runner: &*env.helpers,
        ctx,
        program: &env.settings.git_program,
        deadline: env.settings.git_deadline,
        stop,
    }
}

/// One run being executed.
pub(crate) struct Exec<'a> {
    pub w: &'a IndexWorker,
    pub run: StoredIndexRun,
    pub stop: Arc<AtomicBool>,
    pub ctx: InvocationContext,
    pub warnings: Vec<RunWarning>,
}

impl Exec<'_> {
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst) || self.w.stopping()
    }
    pub fn git<'g>(&'g self, stop: &'g (dyn Fn() -> bool + Sync)) -> Git<'g> {
        git_for(&self.w.env, &self.ctx, stop)
    }

    pub fn log(
        &self,
        stage: &str,
        severity: Severity,
        message: &str,
        progress: Option<(i64, i64)>,
    ) {
        if let Some(d) = &self.w.env.diagnostics {
            let mut log = d
                .run(
                    crate::services::diagnostics::channels::REPOSITORY_INDEXING,
                    "indexing.index",
                )
                .operation_id(crate::protocol::OperationId(self.run.id.clone()))
                .target("repository", self.run.repository_id.clone())
                .attempt(self.run.attempt);
            if let Some(oid) = &self.run.source_oid {
                log = log.source_revision(oid);
            }
            if let Some(p) = &self.run.project_id {
                log = log.project(p.clone());
            }
            if let Some(at) = self.run.started_at_ms {
                log = log.started_at(at);
            }
            let _ = log.stage(stage, severity, message, progress);
        }
    }
    fn log_finish(
        &self,
        outcome: DiagnosticOutcome,
        class: Option<DiagnosticErrorClass>,
        message: &str,
    ) {
        if let Some(d) = &self.w.env.diagnostics {
            let mut log = d
                .run(
                    crate::services::diagnostics::channels::REPOSITORY_INDEXING,
                    "indexing.index",
                )
                .operation_id(crate::protocol::OperationId(self.run.id.clone()))
                .target("repository", self.run.repository_id.clone())
                .attempt(self.run.attempt);
            if let Some(oid) = &self.run.source_oid {
                log = log.source_revision(oid);
            }
            if let Some(p) = &self.run.project_id {
                log = log.project(p.clone());
            }
            if let Some(at) = self.run.started_at_ms {
                log = log.started_at(at);
            }
            let progress = self
                .run
                .total_units
                .map(|t| (self.run.completed_units.min(t), t));
            let _ = log.finish(
                &self.run.stage,
                outcome,
                class,
                message,
                None,
                progress,
                Vec::new(),
            );
        }
    }

    /// One fenced write of the run: `f` runs only while the run is still this claim's, sees and
    /// may change the current run, and commits with the renewed lease and the next revision.
    pub fn fenced<T, F>(&mut self, priority: WritePriority, f: F) -> Result<T, Stop>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn WriteTransaction, &mut StoredIndexRun) -> Result<T, StorageError>
            + Send
            + 'static,
    {
        if self.stopped() {
            return Err(Stop::Cancelled);
        }
        let (id, owner, attempt) = (self.run.id.clone(), self.w.owner.clone(), self.run.attempt);
        let lease = self.w.env.settings.lease_ms;
        let warnings = self.warnings.clone();
        let r = run_write(
            &*self.w.env.store,
            WriteOptions {
                priority,
                ..WriteOptions::default()
            },
            move |tx| {
                let Some(cur) = tx.indexing().run(&id)? else {
                    return Ok(Err(Stop::Lost));
                };
                if cur.state != RUN_RUNNING
                    || cur.owner.as_deref() != Some(owner.as_str())
                    || cur.attempt != attempt
                {
                    return Ok(Err(Stop::Lost));
                }
                if cur.cancel_requested_at_ms.is_some() {
                    return Ok(Err(Stop::Cancelled));
                }
                let mut next = cur.clone();
                next.warnings = warnings;
                let value = f(tx, &mut next)?;
                next.revision = cur.revision + 1;
                // The lease is renewed while the run stays claimed (a finishing write releases it).
                if next.owner.is_some() {
                    next.lease_until_ms = Some(tx.now_ms() + lease);
                }
                tx.indexing().update_run(&next, cur.revision)?;
                Ok(Ok((value, next)))
            },
        );
        match r {
            Ok(Ok((value, next))) => {
                self.run = next;
                Ok(value)
            }
            Ok(Err(stop)) => Err(stop),
            Err(e) => Err(Stop::storage(e)),
        }
    }
    pub fn stage(&mut self, stage: &'static str) -> Result<(), Stop> {
        self.fenced(WritePriority::Authoritative, move |_, r| {
            r.stage = stage.into();
            Ok(())
        })?;
        self.log(stage, Severity::Info, stage, None);
        Ok(())
    }

    /// Records the run's outcome (or nothing, when its claim was lost).
    fn finish(&mut self, outcome: Result<(), Stop>) {
        let stop = match outcome {
            Ok(()) => {
                self.log_finish(DiagnosticOutcome::Succeeded, None, "published");
                return;
            }
            Err(Stop::Lost) => return,
            Err(s) => s,
        };
        let (id, owner, attempt) = (self.run.id.clone(), self.w.owner.clone(), self.run.attempt);
        let base = self.w.env.settings.retry_base_ms;
        let warnings = self.warnings.clone();
        let stop2 = stop.clone();
        // A stop of the worker itself (shutdown) is not a cancellation of the run: unless one was
        // requested, the run keeps its claim and checkpoint and resumes at the next boot.
        let shutting_down = self.w.stopping() && !self.stop.load(Ordering::SeqCst);
        let written = run_write(&*self.w.env.store, WriteOptions::default(), move |tx| {
            let Some(cur) = tx.indexing().run(&id)? else {
                return Ok(None);
            };
            if cur.state != RUN_RUNNING
                || cur.owner.as_deref() != Some(owner.as_str())
                || cur.attempt != attempt
            {
                return Ok(None);
            }
            if shutting_down
                && matches!(stop2, Stop::Cancelled)
                && cur.cancel_requested_at_ms.is_none()
            {
                return Ok(None);
            }
            let now = tx.now_ms();
            let mut next = cur.clone();
            next.revision = cur.revision + 1;
            next.owner = None;
            next.lease_until_ms = None;
            next.warnings = warnings;
            let cancelled =
                matches!(stop2, Stop::Cancelled) || cur.cancel_requested_at_ms.is_some();
            let (state, overlay_state, reason) = if cancelled {
                next.state = RUN_CANCELLED.into();
                next.stage = "cancelled".into();
                next.finished_at_ms = Some(now);
                (
                    RUN_CANCELLED,
                    OVERLAY_FAILED,
                    "the overlay run was cancelled".to_owned(),
                )
            } else if let Stop::Failed {
                code,
                message,
                deterministic,
            } = &stop2
            {
                next.error_code = Some(code.clone());
                next.error_message = Some(message.chars().take(4096).collect());
                next.deterministic = *deterministic;
                if !*deterministic && cur.attempt < cur.max_attempts {
                    // Transient: back off and resume from the checkpoint.
                    next.state = RUN_QUEUED.into();
                    next.stage = "retry-pending".into();
                    let exp = (cur.attempt - 1).clamp(0, 16) as u32;
                    next.next_attempt_at_ms = now + base.saturating_mul(1i64 << exp);
                    (RUN_QUEUED, OVERLAY_QUEUED, message.clone())
                } else {
                    next.state = RUN_FAILED.into();
                    next.stage = "failed".into();
                    next.finished_at_ms = Some(now);
                    (RUN_FAILED, OVERLAY_FAILED, message.clone())
                }
            } else {
                return Ok(None);
            };
            if state != RUN_QUEUED {
                retire_staging(tx, &cur)?;
            }
            if let Some(o) = &cur.overlay_id {
                super::overlay::settle_overlay(tx, o, &cur.id, overlay_state, Some(&reason))?;
            }
            tx.indexing().update_run(&next, cur.revision)?;
            let actor = tx.store().host_actor.clone();
            history(
                tx,
                &actor,
                cur.project_id.as_ref(),
                (RUN_AGGREGATE, &cur.id, next.revision),
                match state {
                    RUN_CANCELLED => "IndexRunCancelled",
                    RUN_QUEUED => "IndexRunRetryScheduled",
                    _ => "IndexRunFailed",
                },
                json!({
                    "attempt": cur.attempt, "errorCode": next.error_code, "deterministic": next.deterministic,
                    "principalKind": "Host",
                }),
            )?;
            Ok(Some(next))
        });
        match written {
            Ok(Some(next)) => self.run = next,
            _ if shutting_down => {
                self.log(
                    "interrupted",
                    Severity::Info,
                    "stopped by shutdown; resumes from its checkpoint",
                    None,
                );
                return;
            }
            _ => {}
        }
        let (outcome, class, message) = match &stop {
            Stop::Cancelled => (
                DiagnosticOutcome::Cancelled,
                Some(DiagnosticErrorClass::Cancelled),
                "cancelled".to_owned(),
            ),
            Stop::Failed {
                message,
                deterministic,
                code,
            } => (
                if self.run.state == RUN_QUEUED {
                    DiagnosticOutcome::Retryable
                } else {
                    DiagnosticOutcome::Failed
                },
                Some(if *deterministic {
                    if code == "git" {
                        DiagnosticErrorClass::Dependency
                    } else {
                        DiagnosticErrorClass::InvalidInput
                    }
                } else {
                    DiagnosticErrorClass::Transient
                }),
                message.clone(),
            ),
            Stop::Lost => return,
        };
        self.log_finish(outcome, class, &message);
    }

    // -----------------------------------------------------------------------------------------
    // Baseline

    fn baseline(&mut self) -> Result<(), Stop> {
        let env = self.w.env.clone();
        let repo_id = self.run.repository_id.clone();
        let (repo, source) = read_snapshot(&*env.store, &ReadOptions::default(), |s| {
            Ok((
                s.projects().repository(&repo_id)?,
                s.indexing().repository_source(&repo_id)?,
            ))
        })
        .map_err(Stop::storage)?;
        let Some(repo) = repo.filter(|r| r.lifecycle == ACTIVE) else {
            return Err(Stop::det(
                "repository_unavailable",
                "the repository is not an active registration",
            ));
        };
        let location = source
            .as_ref()
            .map_or(repo.location.clone(), |s| s.location.clone());
        let dir = PathBuf::from(&location);
        match env.source.git_marker(&dir) {
            Ok(Some(crate::ports::indexing::GitMarker::Directory)) => {}
            Ok(_) => {
                return Err(Stop::det(
                    "source_missing",
                    format!("{location} is not a Git checkout"),
                ));
            }
            Err(e) => return Err(Stop::det("source_missing", format!("{location}: {e}"))),
        }
        self.stage("resolving")?;
        let ctx = self.ctx.clone();
        let stop_flag = self.stop.clone();
        let stopping = &self.w.stopping;
        let stop = move || stop_flag.load(Ordering::SeqCst) || stopping.load(Ordering::SeqCst);
        let git = git_for(&env, &ctx, &stop);
        let head = git
            .head(&dir)
            .map_err(Stop::git)?
            .ok_or_else(|| Stop::det("no_commit", "the repository has no commit to index"))?;
        let format = git.object_format(&dir).map_err(Stop::git)?;
        let h = head.clone();
        self.fenced(WritePriority::Authoritative, move |_, r| {
            r.source_oid = Some(h);
            r.stage = "inventory".into();
            Ok(())
        })?;
        self.log(
            "inventory",
            Severity::Info,
            &format!("indexing commit {head}"),
            None,
        );

        let git = git_for(&env, &ctx, &stop);
        let (entries, refused) = git.tree(&dir, &head).map_err(Stop::git)?;
        for p in refused.iter().take(16) {
            push_warning(
                &mut self.warnings,
                "path_refused",
                format!("{p:?} is not a safe relative UTF-8 path; skipped"),
            );
        }
        let (cfg, previous) = read_snapshot(&*env.store, &ReadOptions::default(), |s| {
            let cfg = effective_config(s.indexing(), s.projects(), &repo_id)?;
            let previous = match s.indexing().published(&repo_id)? {
                Some(p) => match s.indexing().view(&p.view_id)? {
                    Some(v) => s
                        .indexing()
                        .generation(&v.baseline_id)?
                        .map(|g| (g, p.revision)),
                    None => None,
                },
                None => None,
            };
            Ok((cfg, previous))
        })
        .map_err(Stop::storage)?;
        if cfg.digest != self.run.config_digest {
            push_warning(
                &mut self.warnings,
                "configuration_changed",
                "the source configuration changed after the run was requested; the current one applies".into(),
            );
        }
        let group = source
            .as_ref()
            .and_then(|s| {
                s.section_root
                    .as_ref()
                    .map(|root| super::discover::groups(root, &s.location))
            })
            .unwrap_or_default();
        if cfg.repository_excluded(&repo.name, &group) {
            return Err(Stop::det(
                "repository_excluded",
                "an exclusion now names this repository",
            ));
        }
        let pointer = previous.as_ref().map_or(0, |(_, r)| *r);
        let parent = previous
            .as_ref()
            .map(|(g, _)| g.clone())
            .filter(|g| g.state == GEN_SEALED);
        // Rename provenance between the published commit and this one.
        let mut renamed: BTreeMap<String, String> = BTreeMap::new();
        if let Some(p) = &parent
            && p.source_digest != head
            && super::git::oid_ok(&p.source_digest)
        {
            match git.diff(&dir, &p.source_digest, &head) {
                Ok((changes, _)) => {
                    for c in changes {
                        if let Change::Renamed { from, to } = c {
                            renamed.insert(to, from);
                        }
                    }
                }
                // The previous commit may be gone (history rewritten): no rename provenance.
                Err(e) if !e.cancelled() => {}
                Err(e) => return Err(Stop::git(e)),
            }
        }
        let case_insensitive = env.source.case_insensitive(&dir);
        let (units, excluded) = plan(
            &env,
            &cfg,
            &entries,
            &renamed,
            &mut self.warnings,
            case_insensitive,
        );
        let readme = entries
            .iter()
            .filter(|e| !e.path.contains('/') && !e.link() && !e.submodule())
            .filter(|e| {
                let l = e.path.to_lowercase();
                l == "readme" || l.starts_with("readme.")
            })
            .min_by_key(|e| (e.path.to_lowercase() != "readme.md", e.path.clone()))
            .cloned();
        // Reuse only an equal plan under the same configuration and extractor.
        let reusable = parent.as_ref().filter(|p| {
            self.run.kind == RUN_INDEX
                && p.manifest.get("configDigest").and_then(Value::as_str)
                    == Some(cfg.digest.as_str())
                && p.manifest.get("extractorVersion").and_then(Value::as_i64)
                    == Some(EXTRACTOR_VERSION)
        });
        let total = units.len() as i64;
        let plan_digest = digest(&[
            &head,
            &cfg.digest,
            &parent
                .as_ref()
                .map_or(String::new(), |p| p.id.as_str().to_owned()),
            &pointer.to_string(),
            &self.run.kind,
        ]);
        // The staging generation: the checkpoint of an earlier attempt when its plan still holds.
        let existing = match &self.run.generation_id {
            Some(g) => read_snapshot(&*env.store, &ReadOptions::default(), |s| {
                s.indexing().generation(g)
            })
            .map_err(Stop::storage)?
            .filter(|g| {
                g.state == GEN_STAGING
                    && g.manifest.get("planDigest").and_then(Value::as_str)
                        == Some(plan_digest.as_str())
            }),
            None => None,
        };
        let generation = match existing {
            Some(g) => g.id,
            None => {
                let stored = StoredGeneration {
                    id: env.ids.next_id(IdKind::Generation),
                    repository_id: repo_id.clone(),
                    layer: LAYER_BASELINE.into(),
                    state: GEN_STAGING.into(),
                    source_digest: head.clone(),
                    watermark: 0,
                    manifest: json!({
                        "runId": self.run.id.as_str(),
                        "parentGenerationId": parent.as_ref().map(|p| p.id.as_str()),
                        "parentPointerRevision": pointer,
                        "commit": head, "objectFormat": format,
                        "configDigest": cfg.digest, "configRevision": self.run.config_revision,
                        "extractorVersion": EXTRACTOR_VERSION, "planDigest": plan_digest,
                    }),
                };
                let id = stored.id.clone();
                self.fenced(WritePriority::Authoritative, move |tx, r| {
                    retire_staging(tx, r)?;
                    tx.indexing().insert_generation(&stored)?;
                    r.generation_id = Some(stored.id.clone());
                    r.completed_units = 0;
                    Ok(())
                })?;
                id
            }
        };
        self.fenced(WritePriority::Authoritative, move |_, r| {
            r.total_units = Some(total);
            r.stage = "extracting".into();
            Ok(())
        })?;
        self.log(
            "extracting",
            Severity::Info,
            &format!(
                "{} units planned ({} paths excluded)",
                total,
                excluded.values().sum::<i64>()
            ),
            Some((self.run.completed_units, total)),
        );

        // Already staged units (an earlier attempt's checkpoint) are skipped.
        let staged = staged_keys(&env, &generation).map_err(Stop::storage)?;
        let parent_units = match reusable {
            Some(p) => unit_index(&env, &p.id).map_err(Stop::storage)?,
            None => BTreeMap::new(),
        };
        let mut inherit: Vec<String> = Vec::new();
        let mut extract: Vec<&PlanUnit> = Vec::new();
        for u in &units {
            if staged.contains(&u.key) {
                continue;
            }
            let same = parent_units.get(&u.key).is_some_and(|(d, mode, lang)| {
                d == &u.digest && mode == u.mode && lang.as_deref() == u.language
            });
            if same && u.previous.is_none() {
                inherit.push(u.key.clone());
            } else {
                extract.push(u);
            }
        }
        let mut done = staged.len() as i64;
        if let Some(p) = reusable {
            for chunk in inherit.chunks(500) {
                let (from, to, keys) = (p.id.clone(), generation.clone(), chunk.to_vec());
                let n = self.fenced(WritePriority::Bulk, move |tx, r| {
                    let n = tx.indexing().inherit_units(&from, &to, &keys)? as i64;
                    r.completed_units += n;
                    Ok(n)
                })?;
                done += n;
            }
        }
        self.stage_units(
            &dir,
            &repo.name,
            &generation,
            &extract,
            &mut done,
            total,
            &stop,
        )?;

        // Linking: unresolved references by name across the generation.
        self.stage("linking")?;
        link_generation(self, &generation, &[], &stop)?;

        // Seal: every planned unit is staged; coverage from the staged units.
        self.stage("sealing")?;
        let (coverage, count) = coverage_of(&env, &generation, &excluded).map_err(Stop::storage)?;
        if count != total {
            return Err(Stop::det(
                "plan_mismatch",
                format!("{count} units are staged but {total} were planned"),
            ));
        }
        let now = env.now();
        let g = generation.clone();
        self.fenced(WritePriority::Authoritative, move |tx, r| {
            let mut stored = tx
                .indexing()
                .generation(&g)?
                .ok_or(StorageError::NotFound {
                    kind: "index_generation",
                    id: g.as_str().to_owned(),
                })?;
            stored.state = GEN_SEALED.into();
            stored.watermark = count;
            stored.manifest["coverage"] = coverage;
            stored.manifest["indexedAtMs"] = json!(now);
            stored.manifest["units"] = json!(count);
            tx.indexing().update_generation(&stored)?;
            r.completed_units = count;
            r.stage = "publishing".into();
            Ok(())
        })?;

        // Texts (README and structural summary) are staged outside the publishing write.
        let readme_bytes = match &readme {
            Some(e) if (e.size as usize) <= env.settings.readme_max_bytes => {
                let git = git_for(&env, &ctx, &stop);
                git.blobs(&dir, std::slice::from_ref(&e.oid), e.size as usize)
                    .ok()
                    .and_then(|mut b| b.pop())
            }
            _ => None,
        };
        let summary = read_snapshot(&*env.store, &ReadOptions::default(), |s| {
            s.indexing().generation(&generation)
        })
        .map_err(Stop::storage)?
        .map(|g| summary_text(&repo.name, &head, &g.manifest))
        .unwrap_or_default();
        let texts = stage_texts(&env, &repo_id, readme_bytes, summary);
        let staged_files: Vec<_> = texts.iter().map(|t| t.2.clone()).collect();
        let published = self.publish_baseline(&generation, &head, pointer, texts);
        if let Some(a) = &env.artifacts {
            for f in &staged_files {
                a.settle_host_bytes(&*env.store, f, published.is_ok());
            }
        }
        published
    }

    /// Stages extracted units in batches bounded by count and blob bytes.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn stage_units(
        &mut self,
        dir: &Path,
        name: &str,
        generation: &Id,
        units: &[&PlanUnit],
        done: &mut i64,
        total: i64,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), Stop> {
        let env = self.w.env.clone();
        let mut i = 0;
        while i < units.len() {
            if self.stopped() {
                return Err(Stop::Cancelled);
            }
            let mut batch = Vec::new();
            let mut bytes = 0usize;
            while i < units.len() && batch.len() < env.settings.batch_units {
                let u = units[i];
                let size = if u.oid.is_some() { u.size as usize } else { 0 };
                if !batch.is_empty() && bytes + size > env.settings.batch_bytes {
                    break;
                }
                bytes += size;
                batch.push(u);
                i += 1;
            }
            let oids: Vec<String> = batch.iter().filter_map(|u| u.oid.clone()).collect();
            let blobs = if oids.is_empty() {
                Vec::new()
            } else {
                self.git(stop).blobs(dir, &oids, bytes).map_err(Stop::git)?
            };
            let mut content: HashMap<&str, &[u8]> = HashMap::new();
            for (o, b) in oids.iter().zip(blobs.iter()) {
                content.insert(o.as_str(), b.as_slice());
            }
            let mut rows = Vec::with_capacity(batch.len());
            for u in &batch {
                let c = u.oid.as_deref().and_then(|o| content.get(o).copied());
                rows.push(build_unit(
                    &env,
                    &self.run.repository_id,
                    name,
                    generation,
                    u,
                    c,
                    stop,
                    &mut self.warnings,
                )?);
            }
            let n = rows.len() as i64;
            self.fenced(WritePriority::Bulk, move |tx, r| {
                for (unit, nodes, edges) in &rows {
                    tx.indexing().put_unit(unit, nodes, edges)?;
                }
                r.completed_units += n;
                Ok(())
            })?;
            *done += n;
            self.log(
                "extracting",
                Severity::Info,
                "batch staged",
                Some((*done, total)),
            );
        }
        Ok(())
    }

    /// The publishing unit of work (see the module documentation of `super`).
    fn publish_baseline(
        &mut self,
        generation: &Id,
        head: &str,
        pointer: i64,
        texts: Vec<(TextKind, String, crate::services::artifacts::StagedFile)>,
    ) -> Result<(), Stop> {
        let env = self.w.env.clone();
        let view_id = env.ids.next_id(IdKind::View);
        let (g, head, repo) = (
            generation.clone(),
            head.to_owned(),
            self.run.repository_id.clone(),
        );
        let artifacts = env.artifacts.clone();
        let retain = env.settings.retained_baselines;
        let result = self.fenced(WritePriority::Authoritative, move |tx, r| {
            let current = tx.indexing().published(&repo)?.map_or(0, |p| p.revision);
            if current != pointer {
                // Another publication (or an index removal) moved the pointer since this plan:
                // never overwrite it. The generation is abandoned and the run re-plans.
                return Ok(Err(current));
            }
            let stored = tx
                .indexing()
                .generation(&g)?
                .filter(|x| x.state == GEN_SEALED)
                .ok_or(StorageError::NotFound { kind: "index_generation", id: g.as_str().to_owned() })?;
            let (state, gaps) = super::status::manifest_coverage(&stored.manifest);
            let coverage_name = match state {
                crate::protocol::data::CoverageState::Complete => "Complete",
                crate::protocol::data::CoverageState::Partial => "Partial",
                crate::protocol::data::CoverageState::Unavailable => "Unavailable",
            };
            let view = StoredView {
                view_id: view_id.clone(),
                repository_id: repo.clone(),
                view_digest: digest(&[repo.as_str(), g.as_str(), &head, "baseline"]),
                baseline_id: g.clone(),
                branch_id: None,
                worktree_id: None,
                coverage: json!({"state": coverage_name, "gaps": gaps, "commit": head,
                    "configDigest": stored.manifest.get("configDigest")}),
            };
            tx.indexing().insert_view(&view)?;
            let revision = tx.indexing().publish(&repo, &view_id, pointer)?;
            let now = tx.now_ms();
            tx.projects().publish_index_view(&StoredIndexView {
                view_id: view_id.clone(),
                repository_id: repo.clone(),
                source_revision: head.clone(),
                coverage: coverage_name.into(),
                published_at_ms: now,
            })?;
            if let Some(a) = &artifacts {
                for (kind, media, staged) in &texts {
                    let title = format!("{} {}", repo.as_str(), kind.as_str());
                    let version = a.commit_host_bytes(
                        tx,
                        staged,
                        &title,
                        media,
                        json!({"repositoryId": repo.as_str(), "viewId": view_id.as_str(), "text": kind.as_str(), "commit": head}),
                    )?;
                    tx.projects().put_repository_text(&StoredRepositoryText {
                        view_id: view_id.clone(),
                        repository_id: repo.clone(),
                        kind: *kind,
                        artifact_id: version.artifact_id.clone(),
                        version_id: version.id.clone(),
                        sha256: version.blob_digest.clone(),
                        byte_length: version.byte_count,
                        media_type: version.media_type.clone(),
                        coverage: if *kind == TextKind::Summary { coverage_name.into() } else { "Complete".into() },
                    })?;
                }
            }
            // Overlays composed over the old baseline no longer answer for the current one.
            for mut o in tx.indexing().overlays(&repo)? {
                if o.state == OVERLAY_READY && o.base_generation_id.as_ref() != Some(&g) {
                    let expected = o.revision;
                    o.revision += 1;
                    o.state = OVERLAY_STALE.into();
                    o.reason = Some("the repository baseline moved".into());
                    o.updated_at_ms = now;
                    tx.indexing().put_overlay(&o, Some(expected))?;
                }
            }
            // Earlier baselines beyond retention retire once nothing uses them.
            let registry = tx.projects().index_views(&repo)?;
            for old in registry.iter().filter(|v| v.view_id != view_id).skip(retain) {
                if tx.indexing().live_pins(&old.view_id, now)? > 0 {
                    continue;
                }
                if let Some(v) = tx.indexing().view(&old.view_id)? {
                    retire_view(tx, &v)?;
                } else {
                    tx.indexing().remove_view(&old.view_id)?;
                }
            }
            r.state = RUN_SUCCEEDED.into();
            r.stage = "published".into();
            r.output_view_id = Some(view_id.clone());
            r.finished_at_ms = Some(now);
            r.owner = None;
            r.lease_until_ms = None;
            let host = tx.store().host_actor.clone();
            history(
                tx,
                &host,
                r.project_id.as_ref(),
                (RUN_AGGREGATE, &r.id, r.revision + 1),
                "IndexPublished",
                json!({
                    "repositoryId": repo.as_str(), "viewId": view_id.as_str(), "generationId": g.as_str(),
                    "commit": head, "pointerRevision": revision, "coverage": coverage_name,
                    "principalKind": "Host",
                }),
            )?;
            Ok(Ok(()))
        });
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(moved)) => Err(Stop::transient(
                "pointer_moved",
                format!(
                    "the published view moved (revision {moved}) while this generation was built"
                ),
            )),
            // A lease renewal inside the fenced write: the run is no longer ours.
            Err(Stop::Lost) => Err(Stop::Lost),
            Err(s) => Err(s),
        }
    }
}

/// Unit keys already staged in a generation.
pub(crate) fn staged_keys(
    env: &IndexEnv,
    generation: &Id,
) -> Result<BTreeSet<String>, StorageError> {
    let mut out = BTreeSet::new();
    let mut after: Option<String> = None;
    loop {
        let page = read_snapshot(&*env.store, &ReadOptions::default(), |s| {
            s.indexing().units(generation, after.as_deref(), MAX_PAGE)
        })?;
        for u in &page {
            out.insert(u.unit_key.clone());
        }
        if page.len() < MAX_PAGE as usize {
            break;
        }
        after = page.last().map(|u| u.unit_key.clone());
    }
    Ok(out)
}

/// A unit's content digest, mode and language.
pub(crate) type UnitFacts = (String, String, Option<String>);

/// The digest, mode and language of every unit of a sealed generation.
pub(crate) fn unit_index(
    env: &IndexEnv,
    generation: &Id,
) -> Result<BTreeMap<String, UnitFacts>, StorageError> {
    let mut out = BTreeMap::new();
    let mut after: Option<String> = None;
    loop {
        let page = read_snapshot(&*env.store, &ReadOptions::default(), |s| {
            s.indexing().units(generation, after.as_deref(), MAX_PAGE)
        })?;
        for u in &page {
            out.insert(
                u.unit_key.clone(),
                (
                    u.content_digest.clone(),
                    u.payload
                        .get("mode")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    u.payload
                        .get("language")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                ),
            );
        }
        if page.len() < MAX_PAGE as usize {
            break;
        }
        after = page.last().map(|u| u.unit_key.clone());
    }
    Ok(out)
}

/// The edge kinds linked by name, with the node kinds that may be their target.
fn target_kinds(edge: &str) -> Option<&'static [&'static str]> {
    Some(match edge {
        "CALLS" => &["Function", "Method", "Constructor"],
        "INHERITS" | "IMPLEMENTS" | "USES_TYPE" => &[
            "Class",
            "Interface",
            "Struct",
            "Enum",
            "Record",
            "Trait",
            "Module",
        ],
        "IMPORTS" => &["File", "Module", "Namespace"],
        _ => return None,
    })
}

/// Candidate file paths an import string names, relative to the importing file.
fn import_candidates(from: &str, module: &str) -> Vec<String> {
    let dir = parent(from);
    let mut out = Vec::new();
    let join = |base: &str, rel: &str| -> Option<String> {
        let mut parts: Vec<&str> = if base.is_empty() {
            Vec::new()
        } else {
            base.split('/').collect()
        };
        for seg in rel.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    parts.pop()?;
                }
                s => parts.push(s),
            }
        }
        Some(parts.join("/"))
    };
    if module.starts_with("./") || module.starts_with("../") {
        if let Some(p) = join(dir, module) {
            for ext in [
                "",
                ".ts",
                ".tsx",
                ".js",
                ".jsx",
                ".mjs",
                ".cjs",
                "/index.ts",
                "/index.tsx",
                "/index.js",
            ] {
                out.push(format!("{p}{ext}"));
            }
        }
    } else if module.contains('.') && !module.contains('/') && !module.contains(' ') {
        // Python and Java style dotted modules.
        let p = module.replace('.', "/");
        out.push(format!("{p}.py"));
        out.push(format!("{p}/__init__.py"));
        out.push(format!("{p}.java"));
        if let Some(p) = join(dir, &p) {
            out.push(format!("{p}.py"));
        }
    } else if module.ends_with(".h") || module.ends_with(".hpp") || module.ends_with(".hh") {
        if let Some(p) = join(dir, module) {
            out.push(p);
        }
        out.push(module.to_owned());
    } else {
        if let Some(p) = join(dir, module) {
            out.push(format!("{p}.py"));
        }
        out.push(format!("{module}.py"));
    }
    out
}

/// Links unresolved references of `generation` by name. Candidates come from `generation` and
/// then from `lower` layers (an overlay's branch and baseline), skipping units a higher layer
/// replaced or deleted. A unique candidate makes the edge `Inferred`; none or several leave it
/// `Unresolved` with the candidate count.
pub(crate) fn link_generation(
    exec: &mut Exec<'_>,
    generation: &Id,
    lower: &[Id],
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<(), Stop> {
    let env = exec.w.env.clone();
    let mut after: Option<String> = None;
    loop {
        if stop() {
            return Err(Stop::Cancelled);
        }
        let (g, low) = (generation.clone(), lower.to_vec());
        let repo = exec.run.repository_id.clone();
        let a = after.clone();
        let (updates, last, full) = read_snapshot(&*env.store, &ReadOptions::default(), move |s| {
            let units = s.indexing().units(&g, a.as_deref(), 200)?;
            let full = units.len() == 200;
            let last = units.last().map(|u| u.unit_key.clone());
            let mut updates = Vec::new();
            let mut cache: HashMap<String, Vec<(String, String, String)>> = HashMap::new();
            for u in &units {
                for e in s.indexing().edges(&g, &u.unit_key)? {
                    let resolution = e
                        .payload
                        .get("resolution")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if resolution == "Resolved" {
                        continue;
                    }
                    let Some(name) = e.payload.get("targetName").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(kinds) = target_kinds(&e.kind) else {
                        continue;
                    };
                    let candidates: Vec<(String, String, String)> = if e.kind == "IMPORTS" {
                        let mut found = Vec::new();
                        for path in import_candidates(&u.unit_key, name) {
                            let basename = path.rsplit('/').next().unwrap_or(&path).to_owned();
                            for n in visible_named(s, &g, &low, &basename)? {
                                if n.1 == "File" && n.0 == path {
                                    found.push(n.clone());
                                }
                            }
                            if !found.is_empty() {
                                break;
                            }
                        }
                        found
                    } else {
                        let simple = name
                            .rsplit(['.', ':'])
                            .find(|s| !s.is_empty())
                            .unwrap_or(name)
                            .to_owned();
                        let all = match cache.get(&simple) {
                            Some(c) => c.clone(),
                            None => {
                                let c = visible_named(s, &g, &low, &simple)?;
                                cache.insert(simple.clone(), c.clone());
                                c
                            }
                        };
                        all.into_iter()
                            .filter(|c| kinds.contains(&c.1.as_str()))
                            .collect()
                    };
                    let mut payload = e.payload.clone();
                    let (target, unit) = if candidates.len() == 1 {
                        payload["resolution"] = json!("Inferred");
                        payload["candidates"] = json!(1);
                        (candidates[0].2.clone(), Some(candidates[0].0.clone()))
                    } else {
                        // None or several: the name stays a symbol, never a guessed target.
                        payload["resolution"] = json!("Unresolved");
                        payload["candidates"] = json!(candidates.len());
                        (symbol_id(repo.as_str(), name), None)
                    };
                    match unit {
                        Some(u) => {
                            payload["targetUnit"] = json!(u);
                        }
                        None => {
                            if let Some(m) = payload.as_object_mut() {
                                m.remove("targetUnit");
                            }
                        }
                    }
                    if payload != e.payload || target != e.target_node_id {
                        updates.push(EdgeTargetUpdate {
                            unit_key: e.unit_key.clone(),
                            edge_id: e.edge_id.clone(),
                            target_node_id: target,
                            payload,
                        });
                    }
                }
            }
            Ok((updates, last, full))
        })
        .map_err(Stop::storage)?;
        for chunk in updates.chunks(2000) {
            let (g, chunk) = (generation.clone(), chunk.to_vec());
            exec.fenced(WritePriority::Bulk, move |tx, _| {
                tx.indexing().set_edge_targets(&g, &chunk)
            })?;
        }
        if !full {
            return Ok(());
        }
        after = last;
    }
}

/// Nodes named `name` visible from `generation` (then `lower` layers, skipping units a higher
/// layer mentions): (unit key, kind, node ID).
fn visible_named(
    s: &dyn ReadSnapshot,
    generation: &Id,
    lower: &[Id],
    name: &str,
) -> Result<Vec<(String, String, String)>, StorageError> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    let mut layers = vec![generation.clone()];
    layers.extend(lower.iter().cloned());
    for (i, g) in layers.iter().enumerate() {
        for n in s.indexing().nodes_named(g, name, 64)? {
            // A unit mentioned by a higher layer is masked here.
            let mut masked = false;
            for higher in &layers[..i] {
                if s.indexing().unit(higher, &n.unit_key)?.is_some() {
                    masked = true;
                    break;
                }
            }
            if !masked {
                out.push((n.unit_key.clone(), n.kind.clone(), n.node_id.clone()));
            }
        }
    }
    Ok(out)
}

/// Coverage of a staged generation (languages, modes, gaps) and its unit count.
pub(crate) fn coverage_of(
    env: &IndexEnv,
    generation: &Id,
    excluded: &BTreeMap<String, i64>,
) -> Result<(Value, i64), StorageError> {
    let mut languages: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    let mut gaps: BTreeSet<String> = BTreeSet::new();
    let mut count = 0i64;
    let mut after: Option<String> = None;
    let bump = |m: &mut Map<String, Value>, k: &str| {
        let v = m.get(k).and_then(Value::as_i64).unwrap_or(0) + 1;
        m.insert(k.into(), json!(v));
    };
    loop {
        let page = read_snapshot(&*env.store, &ReadOptions::default(), |s| {
            s.indexing().units(generation, after.as_deref(), MAX_PAGE)
        })?;
        for u in &page {
            count += 1;
            let mode = u.payload.get("mode").and_then(Value::as_str).unwrap_or("");
            let lang = u.payload.get("language").and_then(Value::as_str);
            let errors = u
                .payload
                .get("parseErrors")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if let Some(l) = lang {
                let m = languages.entry(l.to_owned()).or_default();
                bump(m, "files");
                match mode {
                    MODE_STRUCTURAL => bump(m, "structural"),
                    MODE_TEXT_FALLBACK | MODE_NOT_SELECTED => bump(m, "text"),
                    _ => bump(m, "skipped"),
                }
                if errors > 0 {
                    bump(m, "parseErrors");
                }
            }
            match mode {
                MODE_TEXT_FALLBACK => {
                    gaps.insert(format!("text-fallback-{}", lang.unwrap_or("unknown")));
                }
                MODE_OVERSIZE => {
                    gaps.insert("oversize-files".into());
                }
                MODE_UNREADABLE => {
                    gaps.insert("unreadable-files".into());
                }
                _ => {}
            }
            if errors > 0 {
                gaps.insert("parse-errors".into());
            }
        }
        if page.len() < MAX_PAGE as usize {
            break;
        }
        after = page.last().map(|u| u.unit_key.clone());
    }
    let state = if gaps.is_empty() {
        "Complete"
    } else {
        "Partial"
    };
    Ok((
        json!({
            "state": state,
            "gaps": gaps.into_iter().collect::<Vec<_>>(),
            "languages": Value::Object(languages.into_iter().map(|(k, v)| (k, Value::Object(v))).collect()),
            "excluded": excluded.get("excluded").copied().unwrap_or(0),
        }),
        count,
    ))
}

/// The deterministic structural summary of a generation (no inference).
pub(crate) fn summary_text(name: &str, commit: &str, manifest: &Value) -> String {
    let mut out = format!(
        "# {name}\n\nStructural summary of commit `{commit}` (Tree-sitter baseline; no compiler semantics, no model inference).\n\n"
    );
    let units = manifest.get("units").and_then(Value::as_i64).unwrap_or(0);
    out.push_str(&format!("- Units indexed: {units}\n"));
    if let Some(c) = manifest.get("coverage") {
        out.push_str(&format!(
            "- Coverage: {}\n",
            c.get("state")
                .and_then(Value::as_str)
                .unwrap_or("Unavailable")
        ));
        if let Some(g) = c
            .get("gaps")
            .and_then(Value::as_array)
            .filter(|g| !g.is_empty())
        {
            let gaps: Vec<&str> = g.iter().filter_map(Value::as_str).collect();
            out.push_str(&format!("- Gaps: {}\n", gaps.join(", ")));
        }
        if let Some(l) = c.get("languages").and_then(Value::as_object) {
            out.push_str("\n| Language | Files | Structural | Text | Parse errors |\n| --- | --- | --- | --- | --- |\n");
            for (lang, m) in l {
                let n = |k: &str| m.get(k).and_then(Value::as_i64).unwrap_or(0);
                out.push_str(&format!(
                    "| {lang} | {} | {} | {} | {} |\n",
                    n("files"),
                    n("structural"),
                    n("text"),
                    n("parseErrors")
                ));
            }
        }
    }
    out
}

/// Stages the README and summary texts (none without an artifact service).
fn stage_texts(
    env: &IndexEnv,
    repository: &Id,
    readme: Option<Vec<u8>>,
    summary: String,
) -> Vec<(TextKind, String, crate::services::artifacts::StagedFile)> {
    let Some(a) = &env.artifacts else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut add = |kind: TextKind, media: &str, bytes: &[u8]| {
        let source = json!({"kind": "RepositoryText", "repositoryId": repository.as_str(), "text": kind.as_str()});
        if let Ok(Ok(staged)) = a.stage_host_bytes(&*env.store, bytes, source) {
            out.push((kind, media.to_owned(), staged));
        }
    };
    if let Some(r) = readme {
        add(TextKind::Readme, "text/markdown", &r);
    }
    if !summary.is_empty() {
        add(TextKind::Summary, "text/markdown", summary.as_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_resolve_to_candidate_paths_without_escaping() {
        let c = import_candidates("web/src/app.ts", "./util");
        assert!(c.contains(&"web/src/util.ts".to_owned()));
        assert!(c.contains(&"web/src/util/index.ts".to_owned()));
        let up = import_candidates("a/b.ts", "../../../x");
        assert!(
            up.is_empty(),
            "an import above the root names nothing: {up:?}"
        );
        let py = import_candidates("pkg/mod.py", "pkg.helpers");
        assert!(py.contains(&"pkg/helpers.py".to_owned()));
        assert_eq!(dir_key(""), "/");
        assert_eq!(dir_key("src"), "/src");
    }

    #[test]
    fn summaries_state_their_basis() {
        let m = json!({"units": 3, "coverage": {"state": "Partial", "gaps": ["parse-errors"], "languages": {"rust": {"files": 2, "structural": 2}}}});
        let s = summary_text("demo", "abc", &m);
        assert!(s.contains("no model inference"));
        assert!(s.contains("| rust | 2 | 2 | 0 | 0 |"));
        assert!(s.contains("Gaps: parse-errors"));
    }
}
