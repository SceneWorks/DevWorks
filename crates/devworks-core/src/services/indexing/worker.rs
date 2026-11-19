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
mod calls;
mod exec;
mod links;
pub(crate) mod semantic;
mod units;

use calls::*;
pub(crate) use exec::*;
pub use links::*;
pub use units::*;

use super::config::{EXTRACTOR_VERSION, EffectiveConfig, dependency_directory};
use super::linking::{CallTarget, SwiftImport, SwiftModule, SwiftScopes, call_target_allowed};
use super::paths::{edge_id, fold, node_id, parent, symbol_id};
use super::{IndexEnv, RUN_AGGREGATE, digest, effective_config, history};
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::locks::LockExt;
use crate::ports::IdKind;
use crate::ports::indexing::{
    EdgeTarget, ExtractFault, ExtractedNode, FileExtract, LanguageMode, Span,
};
use crate::ports::storage::*;
use crate::protocol::Id;
use crate::protocol::records::{DiagnosticErrorClass, DiagnosticOutcome, Severity};
use crate::services::git::client::{Change, Git, GitFault, TreeEntry};
use serde_json::{Map, Value, json};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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
    /// Ended ref-overlay records removed by record retention.
    pub pruned_overlays: usize,
    pub reclaimed_rows: u64,
    pub stale_overlays: usize,
    pub refreshes_queued: usize,
    /// Expired graph read lease headers and all their child pins removed this pass.
    pub expired_read_pins: usize,
    /// Superseded overlay views retired after their last browsing lease ended.
    pub retired_read_views: usize,
    /// Overlay views published without stored links that this pass linked.
    pub linked_views: usize,
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
        run_write(&*self.env.store, WriteOptions::recovery(), move |tx| {
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
        if let Some(l) = self.live.locked().get(run.as_str()) {
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
        self.live.locked().len()
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
        let executing: Vec<String> = self.live.locked().keys().cloned().collect();
        let claimed = run_write(&*self.env.store, WriteOptions::host_state(), move |tx| {
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
            self.live.locked().insert(
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
            let live = self.live.locked();
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
            shares: BTreeMap::new(),
            semantic_packs: Vec::new(),
            repository_semantics: Vec::new(),
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
        self.live.locked().remove(run.id.as_str());
    }

    /// Expires overlays past their retention, reclaims retired generations in bounded batches and
    /// marks overlays stale (queueing their refresh) when their worktree or baseline moved.
    pub fn housekeeping(
        &self,
        observe: bool,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<Housekeeping, StorageError> {
        let mut report = Housekeeping::default();
        let expired = read_snapshot(&*self.env.store, &ReadOptions::default(), |s| {
            s.graph().graph_expired_pins(self.env.now(), None, 64)
        })?;
        for pin in expired.items {
            if stop() {
                return Ok(report);
            }
            let removed = run_write(&*self.env.store, WriteOptions::housekeeping(), move |tx| {
                let Some(current) = tx.graph().graph_pin(&pin.id)? else {
                    return Ok(false);
                };
                if current.revision != pin.revision || current.expires_at_ms > tx.now_ms() {
                    return Ok(false);
                }
                tx.graph().retire_graph_pin(
                    &current.id,
                    &current.owner_namespace,
                    current.revision,
                )?;
                Ok(true)
            })?;
            report.expired_read_pins += usize::from(removed);
        }
        let detached = read_snapshot(&*self.env.store, &ReadOptions::default(), |s| {
            s.graph().graph_retirable_views(self.env.now(), None, 64)
        })?;
        for view in detached.items {
            if stop() {
                return Ok(report);
            }
            let removed = run_write(&*self.env.store, WriteOptions::housekeeping(), move |tx| {
                let Some(current) = tx.indexing().view(&view.view_id)? else {
                    return Ok(false);
                };
                let now = tx.now_ms();
                if current != view
                    || tx.indexing().live_pins(&view.view_id, now)? > 0
                    || tx
                        .indexing()
                        .published(&view.repository_id)?
                        .is_some_and(|p| p.view_id == view.view_id)
                {
                    return Ok(false);
                }
                let overlay = if let Some(id) = view
                    .coverage
                    .get("overlayId")
                    .and_then(|v| serde_json::from_value::<Id>(v.clone()).ok())
                {
                    tx.indexing().overlay(&id)?
                } else if let Some(id) = view
                    .coverage
                    .get("worktreeId")
                    .and_then(|v| serde_json::from_value::<Id>(v.clone()).ok())
                {
                    tx.indexing().overlay_for(&view.repository_id, &id)?
                } else if let Some(head) = view.coverage.get("head").and_then(Value::as_str) {
                    tx.indexing().overlay_at(&view.repository_id, head)?
                } else {
                    return Ok(false);
                };
                if overlay.is_some_and(|o| o.view_id.as_ref() == Some(&view.view_id)) {
                    return Ok(false);
                }
                retire_view(tx, &view)?;
                Ok(true)
            })?;
            report.retired_read_views += usize::from(removed);
        }
        // An overlay view published before its links were stored (by an earlier build) gets
        // them here, a bounded batch per pass.
        let unlinked = read_snapshot(&*self.env.store, &ReadOptions::default(), |s| {
            s.graph().graph_unlinked_views(None, 16)
        })?;
        for view in unlinked.items {
            if stop() {
                return Ok(report);
            }
            let layers: Vec<Id> = [
                view.worktree_id.clone(),
                view.branch_id.clone(),
                Some(view.baseline_id.clone()),
            ]
            .into_iter()
            .flatten()
            .collect();
            let repository = view.repository_id.clone();
            let links = read_snapshot(&*self.env.store, &ReadOptions::default(), |s| {
                view_links(s.indexing(), &repository, &layers)
            })?;
            let linked = run_write(&*self.env.store, WriteOptions::housekeeping(), move |tx| {
                // The view may have been retired meanwhile.
                if tx.indexing().view(&view.view_id)?.as_ref() != Some(&view) {
                    return Ok(false);
                }
                tx.indexing().insert_view_links(&view.view_id, &links)?;
                Ok(true)
            })?;
            report.linked_views += usize::from(linked);
        }
        let due = read_snapshot(&*self.env.store, &ReadOptions::default(), |s| {
            s.indexing().overlays_due(self.env.now(), 64)
        })?;
        for o in due {
            let expired =
                run_write(
                    &*self.env.store,
                    WriteOptions::housekeeping(),
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
        report.pruned_overlays = super::overlay::prune_ref_overlays(&self.env, stop)?;
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
                    ..WriteOptions::housekeeping()
                },
                move |tx| tx.indexing().reclaim_generation(&id, 5_000),
            )?;
        }
        Ok(report)
    }
}

/// Coverage of a staged generation (languages, modes, gaps) and its unit count.
pub(crate) fn coverage_of(
    env: &IndexEnv,
    generation: &Id,
    excluded: &BTreeMap<String, i64>,
) -> Result<(Value, i64), StorageError> {
    let mut languages: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    let mut gaps: BTreeSet<String> = BTreeSet::new();
    let mut semantic = BTreeMap::new();
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
            for pack in u.payload["semanticPacks"].as_array().into_iter().flatten() {
                if let Some(id) = pack["id"].as_str() {
                    semantic.insert(id.to_owned(), pack.clone());
                }
            }
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
            // A pass that could not complete (`backend-endpoints: ...`) names its own gap.
            for reason in u
                .payload
                .get("incomplete")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                let pass = reason.split(':').next().unwrap_or(reason).trim();
                gaps.insert(format!("incomplete-{pass}"));
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
            "semanticPacks": semantic.into_values().collect::<Vec<_>>(),
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
        assert_eq!(
            import_candidates("pkg/consumer.py", "foo"),
            vec!["foo/__init__.py", "foo.py"]
        );
        assert_eq!(
            import_candidates("pkg/consumer.py", ".foo"),
            vec!["pkg/foo/__init__.py", "pkg/foo.py"]
        );
        assert_eq!(
            import_candidates("pkg/sub/consumer.py", "..foo"),
            vec!["pkg/foo/__init__.py", "pkg/foo.py"]
        );
        assert!(import_candidates("pkg/consumer.py", "..foo").is_empty());
        assert!(import_candidates("consumer.py", ".foo").is_empty());
        // A dotted include is a header, never a module path.
        for (from, include, want) in [
            ("src/main.c", "foo.h", "src/foo.h"),
            ("src/a.cpp", "widget.hpp", "src/widget.hpp"),
            ("lib/x.cc", "util.hh", "lib/util.hh"),
        ] {
            let c = import_candidates(from, include);
            assert_eq!(
                c.first().map(String::as_str),
                Some(want),
                "{include}: {c:?}"
            );
            assert!(!c.iter().any(|p| p.ends_with(".py")), "{include}: {c:?}");
        }
        assert_eq!(dir_key(""), "/");
        assert_eq!(dir_key("src"), "/src");
    }

    fn call_node(unit: &str, kind: &str, name: &str, qn: &str, owner: Option<&str>) -> CallNode {
        CallNode {
            unit: unit.into(),
            kind: kind.into(),
            id: format!("{unit}:{qn}"),
            name: name.into(),
            qualified_name: qn.into(),
            receiver_owner: owner.map(str::to_owned),
            argument_labels: None,
            abi: None,
        }
    }

    #[test]
    fn calls_bind_only_as_the_retained_rules_allow() {
        // An unproven receiver never binds by name; a bare or resolved one may.
        assert!(call_binding_refused(
            &json!({"receiver_kind": "Unresolved"})
        ));
        assert!(call_binding_refused(&json!({"receiver_kind": "Unknown"})));
        assert!(!call_binding_refused(&json!({"receiver_kind": "Bare"})));
        assert!(!call_binding_refused(&json!({"receiver_kind": "Resolved"})));
        assert!(!call_binding_refused(&json!({})));
        // A call may construct a type.
        let kinds = target_kinds("CALLS").unwrap();
        for k in [
            "Function",
            "Method",
            "Constructor",
            "Class",
            "Struct",
            "Record",
            "Enum",
        ] {
            assert!(kinds.contains(&k), "{k}");
        }
        assert!(!kinds.contains(&"Interface") && !kinds.contains(&"Property"));

        // Members of the receiver type: same unit and qualified-name prefix, or (a unique owner
        // name) the member's receiver_owner.
        let owner = call_node("src/s.rs", "Class", "S", "#type:S", None);
        let member = call_node(
            "src/s.rs",
            "Method",
            "f",
            "#type:S#method:f(&self)",
            Some("S"),
        );
        let nested = call_node("src/s.rs", "Method", "f", "#type:S#type:T#method:f()", None);
        let other_unit = call_node("src/x.rs", "Method", "f", "#type:S#method:f(&self)", None);
        let impl_elsewhere = call_node("src/i.rs", "Method", "f", "#method:f(&self)", Some("S"));
        let prefix_only = call_node("src/s.rs", "Method", "f", "#type:Sx#method:f()", None);
        assert!(owned_by(&member, &owner, false));
        assert!(!owned_by(&nested, &owner, false), "a nested type's member");
        assert!(!owned_by(&other_unit, &owner, true));
        assert!(!owned_by(&prefix_only, &owner, false));
        assert!(!owned_by(&impl_elsewhere, &owner, false));
        assert!(owned_by(&impl_elsewhere, &owner, true));

        // Swift: the type at a dotted path by exact qualified names, and extension members.
        assert_eq!(
            swift_type_qualified_names("Outer.Inner"),
            vec![
                "#type:Outer#type:Inner".to_owned(),
                "#extension:Outer#type:Inner".into()
            ]
        );
        assert_eq!(
            swift_type_qualified_names("Box"),
            vec!["#type:Box".to_owned()]
        );
        assert!(swift_type_qualified_names("").is_empty());
        let ext = call_node(
            "Sources/E.swift",
            "Method",
            "y",
            "#extension:Remote.Thing#method:y()",
            Some("Thing"),
        );
        assert!(owned_by_swift_extension(&ext, "Remote.Thing"));
        assert!(
            !owned_by_swift_extension(&ext, "Thing"),
            "never by last segment"
        );
        let not_swift = CallNode {
            unit: "src/e.rs".into(),
            ..ext.clone()
        };
        assert!(!owned_by_swift_extension(&not_swift, "Remote.Thing"));

        // Argument labels select overloads: defaults may be omitted, variadics take any number of
        // unlabeled arguments, a trailing closure binds any label.
        assert!(labels_accept("by:_=:", "by:"));
        assert!(labels_accept("by:_=:", "by:_:"));
        assert!(!labels_accept("by:_=:", "_:"));
        assert!(labels_accept("radius:", "radius:"));
        assert!(!labels_accept("", "radius:"));
        assert!(labels_accept("", ""));
        assert!(labels_accept("_...:to:", "_:_:_:to:"));
        assert!(labels_accept("_...:to:", "to:"));
        assert!(labels_accept("x:completion:", "x:*:"));
        assert!(!labels_accept("x:", "x:*:"));
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
