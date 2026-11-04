//! Wire records of runs and `indexing.status` (CODE-07: freshness, language coverage, semantic
//! pack state and overlays). Stale data never reads as current: an index whose commit, source
//! or configuration moved on carries freshness gaps and is never `Complete`.
use super::{IndexEnv, effective_config};
use crate::context::InvocationContext;
use crate::ports::indexing::LanguageMode;
use crate::ports::storage::*;
use crate::protocol::data::{Coverage, CoverageState};
use crate::protocol::records::{
    Diagnostic, IndexFreshness, IndexFreshnessState, IndexRunRecord, LanguageCoverage,
    LanguageCoverageMode, OverlayState, OverlayStatus, RepositoryIndexStatus, SemanticPackState,
    SemanticPackStatus, Severity,
};
use crate::protocol::support::{BoundedList, BoundedText};
use crate::protocol::{Id, OperationState};
use crate::services::projects::visible_repository;
use crate::services::sessions::{Failure, counter, label, rev};
use serde_json::Value;

/// The `OperationState` a run reports.
pub fn run_state(run: &StoredIndexRun) -> OperationState {
    match run.state.as_str() {
        RUN_QUEUED if run.attempt > 0 => OperationState::RetryPending,
        RUN_QUEUED => OperationState::IntentRecorded,
        RUN_RUNNING => OperationState::Dispatching,
        RUN_SUCCEEDED => OperationState::ObservedComplete,
        RUN_FAILED => OperationState::Failed,
        _ => OperationState::CancelledBeforeDispatch,
    }
}

fn diagnostic(run: &Id, n: usize, severity: Severity, code: &str, message: &str) -> Diagnostic {
    let base: String = run.as_str().chars().take(120).collect();
    Diagnostic {
        id: Id::new(format!("{base}-d{n}")).unwrap_or_else(|_| run.clone()),
        severity,
        component: label("indexing.repository", "indexing"),
        code: label(code, "note"),
        message: BoundedText(message.chars().take(4096).collect()),
        correlation_id: None,
        evidence: None,
    }
}

/// The wire record of one run: its kind, commit, attempt, failure and warnings are its
/// diagnostics.
pub fn run_record(run: &StoredIndexRun) -> IndexRunRecord {
    let mut d = Vec::new();
    let mut push = |severity: Severity, code: &str, message: String| {
        if d.len() < 128 {
            let n = d.len();
            d.push(diagnostic(&run.id, n, severity, code, &message));
        }
    };
    push(
        Severity::Info,
        "run_kind",
        match &run.overlay_id {
            Some(o) => format!("{} of overlay {}", run.kind, o.as_str()),
            None => run.kind.clone(),
        },
    );
    if let Some(oid) = &run.source_oid {
        push(Severity::Info, "source_commit", oid.clone());
    }
    push(
        Severity::Info,
        "attempt",
        format!("attempt {} of {}", run.attempt, run.max_attempts),
    );
    if run.batch_id != run.id {
        push(Severity::Info, "batch", run.batch_id.as_str().to_owned());
    }
    if run.cancel_requested_at_ms.is_some() {
        push(
            Severity::Info,
            "cancel_requested",
            "cancellation was requested".into(),
        );
    }
    if let Some(code) = &run.error_code {
        let severity = if run.state == RUN_FAILED {
            Severity::Error
        } else {
            Severity::Warning
        };
        let what = if run.deterministic {
            "deterministic; not retried"
        } else {
            "transient"
        };
        push(
            severity,
            code,
            format!("{} ({what})", run.error_message.as_deref().unwrap_or(code)),
        );
    }
    for w in run.warnings.iter().take(64) {
        push(Severity::Warning, &w.code, w.message.clone());
    }
    IndexRunRecord {
        id: run.id.clone(),
        revision: rev(run.revision),
        repository_ids: BoundedList(vec![run.repository_id.clone()]),
        stage: label(&run.stage, "queued"),
        state: run_state(run),
        completed_units: counter(run.completed_units),
        total_units: run.total_units.map(counter),
        output_views: BoundedList(run.output_view_id.iter().cloned().collect()),
        diagnostics: BoundedList(d),
    }
}

/// The reply of a request that recorded or reused several runs: one record naming every
/// repository, identified by the request's batch (cancelling it cancels the batch).
pub fn batch_record(runs: &[StoredIndexRun]) -> IndexRunRecord {
    if runs.len() == 1 {
        return run_record(&runs[0]);
    }
    let opener = runs
        .iter()
        .find(|r| r.batch_id == r.id && runs.iter().filter(|x| x.batch_id == r.id).count() > 1)
        .or_else(|| runs.iter().find(|r| r.batch_id == r.id))
        .unwrap_or(&runs[0]);
    let states: Vec<&str> = runs.iter().map(|r| r.state.as_str()).collect();
    let state = if states.contains(&RUN_RUNNING) {
        OperationState::Dispatching
    } else if states.contains(&RUN_QUEUED) {
        OperationState::IntentRecorded
    } else if states.contains(&RUN_FAILED) {
        OperationState::Failed
    } else if states.iter().all(|s| *s == RUN_SUCCEEDED) {
        OperationState::ObservedComplete
    } else {
        OperationState::CancelledBeforeDispatch
    };
    let total = runs.iter().map(|r| r.total_units).sum::<Option<i64>>();
    let mut d = Vec::new();
    for (n, r) in runs.iter().enumerate().take(128) {
        d.push(diagnostic(
            &opener.id,
            n,
            Severity::Info,
            "batch_run",
            &format!(
                "{}: run {} {}{}",
                r.repository_id.as_str(),
                r.id.as_str(),
                r.state,
                if r.batch_id != opener.id {
                    " (reused)"
                } else {
                    ""
                }
            ),
        ));
    }
    IndexRunRecord {
        id: opener.id.clone(),
        revision: rev(runs.iter().map(|r| r.revision).max().unwrap_or(1)),
        repository_ids: BoundedList(
            runs.iter()
                .take(128)
                .map(|r| r.repository_id.clone())
                .collect(),
        ),
        stage: label("batch", "batch"),
        state,
        completed_units: counter(runs.iter().map(|r| r.completed_units).sum()),
        total_units: total.map(counter),
        output_views: BoundedList(
            runs.iter()
                .filter_map(|r| r.output_view_id.clone())
                .take(128)
                .collect(),
        ),
        diagnostics: BoundedList(d),
    }
}

/// The coverage a generation's manifest records.
pub fn manifest_coverage(manifest: &Value) -> (CoverageState, Vec<String>) {
    let c = manifest.get("coverage");
    let state = match c.and_then(|c| c.get("state")).and_then(Value::as_str) {
        Some("Complete") => CoverageState::Complete,
        Some("Partial") => CoverageState::Partial,
        _ => CoverageState::Unavailable,
    };
    let gaps = c
        .and_then(|c| c.get("gaps"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|g| g.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    (state, gaps)
}

/// The overlay state an observer must see now: a ready overlay past its retention reads
/// `Expired`, one over a baseline that is no longer published reads `Stale`.
pub fn effective_overlay_state(
    o: &StoredOverlay,
    published_generation: Option<&Id>,
    now: i64,
) -> &'static str {
    let live = !matches!(o.state.as_str(), OVERLAY_EXPIRED | OVERLAY_UNAVAILABLE);
    if live && now >= o.expires_at_ms {
        return OVERLAY_EXPIRED;
    }
    if o.state == OVERLAY_READY && o.base_generation_id.as_ref() != published_generation {
        return OVERLAY_STALE;
    }
    OVERLAY_STATES
        .iter()
        .find(|s| **s == o.state)
        .copied()
        .unwrap_or(OVERLAY_UNAVAILABLE)
}

fn overlay_state(s: &str) -> OverlayState {
    match s {
        OVERLAY_QUEUED => OverlayState::Queued,
        OVERLAY_INDEXING => OverlayState::Indexing,
        OVERLAY_READY => OverlayState::Ready,
        OVERLAY_FAILED => OverlayState::Failed,
        OVERLAY_STALE => OverlayState::Stale,
        OVERLAY_EXPIRED => OverlayState::Expired,
        _ => OverlayState::Unavailable,
    }
}

fn gap_id(g: &str) -> Option<Id> {
    let s: String = g
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(128)
        .collect();
    Id::new(s).ok()
}

/// `indexing.status` of one repository (schema revision 2 adds freshness, languages, packs and
/// overlays).
pub fn status(
    env: &IndexEnv,
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    repository: &Id,
    revision: u32,
) -> Result<RepositoryIndexStatus, Failure> {
    let r = visible_repository(read.projects(), ctx, repository)?;
    if let Some(own) = ctx.project_id()
        && !read.projects().members(own)?.contains(&r.id)
    {
        return Err(crate::services::sessions::not_found("repository", repository).into());
    }
    let idx = read.indexing();
    let now = env.now();
    let published = idx.published(&r.id)?;
    let view = published
        .as_ref()
        .map(|p| idx.view(&p.view_id))
        .transpose()?
        .flatten();
    let generation = view
        .as_ref()
        .map(|v| idx.generation(&v.baseline_id))
        .transpose()?
        .flatten();
    let source = idx.repository_source(&r.id)?;
    let effective = effective_config(idx, read.projects(), &r.id)?;
    let last_run = idx.latest_run(&r.id, None)?;
    let active = idx.active_run(&r.id, None)?;

    // Freshness: the indexed commit against the observed HEAD and the current configuration.
    let indexed_commit = generation.as_ref().map(|g| g.source_digest.clone());
    let config_current = generation
        .as_ref()
        .and_then(|g| g.manifest.get("configDigest").and_then(Value::as_str))
        .is_none_or(|d| d == effective.digest);
    let mut reasons: Vec<String> = Vec::new();
    if let Some(s) = &source {
        if s.state != SOURCE_PRESENT {
            reasons.push(format!("source-{}", s.state.to_lowercase()));
        } else if indexed_commit.is_some() && s.head_oid != indexed_commit {
            reasons.push("head-moved".into());
        }
    }
    if generation.is_some() && !config_current {
        reasons.push("configuration-changed".into());
    }
    let freshness_state = if active.is_some() {
        IndexFreshnessState::Indexing
    } else if generation.is_none() {
        if last_run.as_ref().is_some_and(|l| l.state == RUN_FAILED) {
            IndexFreshnessState::Failed
        } else {
            IndexFreshnessState::NotIndexed
        }
    } else if !reasons.is_empty() {
        IndexFreshnessState::Stale
    } else {
        IndexFreshnessState::Current
    };

    let (mut state, mut gaps) = generation
        .as_ref()
        .map(|g| manifest_coverage(&g.manifest))
        .unwrap_or((CoverageState::Unavailable, Vec::new()));
    if generation.is_none() {
        gaps.push("not-indexed".into());
    }
    for reason in &reasons {
        gaps.push(format!("stale-{reason}"));
    }
    if !reasons.is_empty() && matches!(state, CoverageState::Complete) {
        state = CoverageState::Partial;
    }
    let all_gaps: Vec<Id> = gaps.iter().filter_map(|g| gap_id(g)).collect();
    let more = all_gaps.len() > 128;
    let coverage = Coverage {
        state,
        gaps: all_gaps.into_iter().take(128).collect(),
        more_gaps: more,
    };

    let overlays = idx.overlays(&r.id)?;
    let mut current_view_ids: Vec<Id> = published.iter().map(|p| p.view_id.clone()).collect();
    let published_gen = view.as_ref().map(|v| v.baseline_id.clone());
    for o in &overlays {
        if effective_overlay_state(o, published_gen.as_ref(), now) == OVERLAY_READY
            && let Some(v) = &o.view_id
        {
            current_view_ids.push(v.clone());
        }
    }
    let mut status = RepositoryIndexStatus {
        repository_id: r.id.clone(),
        current_view_ids: BoundedList(current_view_ids.into_iter().take(128).collect()),
        coverage,
        last_run: last_run.as_ref().map(run_record),
        freshness: None,
        languages: None,
        semantic_packs: None,
        overlays: None,
    };
    if revision < 2 {
        return Ok(status);
    }
    status.freshness = Some(IndexFreshness {
        state: freshness_state,
        indexed_commit: indexed_commit.map(|c| label(&c, "commit")),
        head_commit: source
            .as_ref()
            .and_then(|s| s.head_oid.as_ref())
            .map(|c| label(c, "commit")),
        head_ref: source
            .as_ref()
            .and_then(|s| s.head_ref.as_ref())
            .map(|c| label(c, "ref")),
        indexed_at_ms: generation
            .as_ref()
            .and_then(|g| g.manifest.get("indexedAtMs").and_then(Value::as_i64))
            .map(counter),
        observed_at_ms: source.as_ref().map(|s| counter(s.observed_at_ms)),
        configuration_current: config_current,
        reasons: BoundedList(reasons.iter().map(|r| label(r, "stale")).collect()),
    });
    let counts = generation
        .as_ref()
        .and_then(|g| {
            g.manifest
                .get("coverage")
                .and_then(|c| c.get("languages"))
                .cloned()
        })
        .unwrap_or(Value::Null);
    let mut languages = Vec::new();
    for l in env.parser.languages() {
        let c = counts.get(l.id);
        let n = |k: &str| {
            c.and_then(|c| c.get(k))
                .and_then(Value::as_i64)
                .unwrap_or(0)
        };
        if c.is_none() && generation.is_some() {
            continue;
        }
        let mode = if !effective.language_selected(l.id) {
            LanguageCoverageMode::NotSelected
        } else {
            match l.mode {
                LanguageMode::Structural => LanguageCoverageMode::Structural,
                LanguageMode::TextFallback => LanguageCoverageMode::TextFallback,
            }
        };
        languages.push(LanguageCoverage {
            language_id: label(l.id, "language"),
            name: label(l.name, "language"),
            mode,
            grammar: l.grammar.map(|g| label(g, "grammar")),
            files: counter(n("files")),
            structural_files: counter(n("structural")),
            text_files: counter(n("text")),
            parse_error_files: counter(n("parseErrors")),
            skipped_files: counter(n("skipped")),
            note: l.note.map(|n| BoundedText(n.to_owned())),
        });
    }
    status.languages = Some(BoundedList(languages.into_iter().take(128).collect()));
    let installed = env.enrichers();
    let applied: Vec<String> = generation
        .as_ref()
        .and_then(|g| g.manifest.get("packs").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|p| p.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let mut packs = Vec::new();
    let mut ids: Vec<String> = effective.packages.clone();
    for e in &installed {
        let d = e.descriptor();
        if !ids.contains(&d.id) {
            ids.push(d.id);
        }
    }
    for id in ids {
        let found = installed
            .iter()
            .map(|e| e.descriptor())
            .find(|d| d.id == id);
        let enabled = effective.packages.contains(&id);
        let (state, note, languages) = match (&found, enabled) {
            (None, _) => (
                SemanticPackState::NotInstalled,
                Some(
                    "the semantic pack is not installed; structural (Tree-sitter) coverage applies"
                        .to_owned(),
                ),
                Vec::new(),
            ),
            (Some(d), false) => (SemanticPackState::Disabled, None, d.languages.clone()),
            (Some(d), true) if !d.missing.is_empty() => (
                SemanticPackState::Unavailable,
                Some(format!("missing prerequisites: {}", d.missing.join(", "))),
                d.languages.clone(),
            ),
            (Some(d), true) if applied.contains(&id) => {
                (SemanticPackState::Applied, None, d.languages.clone())
            }
            (Some(d), true) => (SemanticPackState::Enabled, None, d.languages.clone()),
        };
        let Ok(pack_id) = Id::new(id.clone()) else {
            continue;
        };
        packs.push(SemanticPackStatus {
            pack_id,
            state,
            languages: BoundedList(
                languages
                    .iter()
                    .take(128)
                    .map(|l| label(l, "language"))
                    .collect(),
            ),
            note: note.map(BoundedText),
        });
    }
    status.semantic_packs = Some(BoundedList(packs.into_iter().take(128).collect()));
    status.overlays = Some(BoundedList(
        overlays
            .iter()
            .take(128)
            .map(|o| OverlayStatus {
                overlay_id: o.id.clone(),
                revision: rev(o.revision),
                worktree_id: o.worktree_id.clone(),
                state: overlay_state(effective_overlay_state(o, published_gen.as_ref(), now)),
                view_id: o.view_id.clone(),
                base_commit: o.base_oid.as_ref().map(|c| label(c, "commit")),
                head_commit: o.head_oid.as_ref().map(|c| label(c, "commit")),
                head_ref: o.head_ref.as_ref().map(|c| label(c, "ref")),
                capture_digest: o.capture_digest.as_ref().map(|c| label(c, "capture")),
                run_id: o.run_id.clone(),
                reason: o
                    .reason
                    .as_ref()
                    .map(|r| BoundedText(r.chars().take(4096).collect())),
                updated_at_ms: counter(o.updated_at_ms),
                expires_at_ms: counter(o.expires_at_ms),
            })
            .collect(),
    ));
    Ok(status)
}
