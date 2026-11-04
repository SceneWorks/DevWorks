//! Worktree overlays (CODE-09/10/11): the view of one worktree composed over its repository's
//! published baseline, without changing the baseline.
//!
//! # Snapshot identity
//!
//! An overlay snapshot is identified by repository, baseline generation and commit, the
//! worktree's `HEAD` commit, the worktree, and a capture digest over its staged, unstaged and
//! indexable untracked changes (every changed path with its action and content digest; ignored
//! and excluded files never count). [`observe`] computes it without writing; the refresh command
//! decides reuse against it, and the build re-captures it and checks every file it stages still
//! has the captured content, so a concurrent edit makes the build retry rather than publish a
//! mix.
//!
//! A ref overlay (schema revision 2) has no worktree: it is identified by repository and the
//! commit its ref resolved to (resolved by [`resolve`] before the unit of work), is built from
//! the repository's own checkout and has only the branch layer (empty at the baseline commit,
//! so its view is never a bare baseline view). A live one of the same commit is reused; the
//! commit never moves, so only its baseline, configuration or retention can make it stale.
//!
//! # Layers
//!
//! The branch layer holds the units that differ between the baseline commit and `HEAD`
//! (`git diff -M base head`: a direct diff, as the retained overlay did), read from the commit;
//! the worktree layer holds the units the worktree changes against `HEAD`, read from the
//! worktree (no-follow, bounded). Deletions are `Delete` units and renames are a delete of the
//! old path plus a replacement recording the previous path, so renamed-away and deleted data is
//! hidden. Directory units of affected directories are rebuilt from the composed tree. A layer
//! links its unresolved references against itself first and then the lower layers, skipping
//! units a higher layer replaced.
//!
//! # Lifecycle
//!
//! `Queued` → `Indexing` → `Ready`, or `Failed` (with its reason; a transient failure returns it
//! to `Queued` for the retry). A ready overlay becomes `Stale` when its baseline is republished or
//! its worktree changes (observed by housekeeping, which queues the rebuild), `Expired` once its
//! retention passes (its view and layers go; the record stays), and `Unavailable` when retired
//! or when its index is removed. Only the run an overlay names can change its state. An ended
//! ref overlay's record is pruned by record retention ([`prune_ref_overlays`]).
use super::config::{EffectiveConfig, MANIFEST_DIRECTORIES};
use super::git::{Change, EMPTY_TREE_SHA1, EMPTY_TREE_SHA256, TreeEntry};
use super::worker::{
    Exec, PlanUnit, Stop, build_unit, coverage_of, dir_key, link_generation, plan, push_warning,
    retire_unused, staged_keys,
};
use super::{
    IndexEnv, OVERLAY_AGGREGATE, RUN_AGGREGATE, digest, effective_config, effective_config_tx,
    history, new_run, record_of, section_config, section_key,
};
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::indexing::SourceFault;
use crate::ports::storage::*;
use crate::protocol::records::Severity;
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::{DomainErrorCode, Id, RevisionDimension};
use crate::services::projects::visible_repository;
use crate::services::sessions::{Failure, Refusal, rev};
use crate::settings::catalog::{OVERLAY_RECORD_RETENTION_DAYS_KEY, OVERLAY_RECORDS_KEPT_KEY};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

/// Most changed paths one worktree capture reads (each bounded by the size limit).
pub const MAX_LOCAL_CHANGES: usize = 20_000;
/// Overlays observed per housekeeping pass (the next pass continues after the last one).
pub const OBSERVED_PER_PASS: usize = 64;

/// The observed snapshot of one worktree (or why it could not be observed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observed(pub Result<Snapshot, String>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// The checkout the build reads: the worktree, or the repository's for a ref overlay.
    pub worktree_path: String,
    pub base_generation: Id,
    pub base_oid: String,
    pub head_oid: Option<String>,
    pub head_ref: Option<String>,
    /// The worktree capture (`None` for a ref overlay, which has no worktree).
    pub capture_digest: Option<String>,
    pub local: Vec<LocalChange>,
    pub refused: Vec<String>,
}

/// One captured worktree change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalChange {
    pub path: String,
    /// `None`: deleted.
    pub content: Option<Captured>,
    pub previous: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Captured {
    /// `sha256:<hex>` of the bytes, or an `oversize:`/`link:` marker for content not read.
    pub digest: String,
    pub size: u64,
    pub link: bool,
}

fn refuse(code: DomainErrorCode, kind: &'static str, id: Option<&Id>) -> Failure {
    Refusal::new(code, kind, id.cloned()).into()
}

/// Reads one worktree file for capture.
fn capture_file(env: &IndexEnv, root: &Path, path: &str, max: i64) -> Option<Captured> {
    match env.source.read_file(root, path, max.max(0) as u64) {
        Ok(bytes) => Some(Captured {
            digest: format!("sha256:{}", sha256_hex(&bytes)),
            size: bytes.len() as u64,
            link: false,
        }),
        Err(SourceFault::TooLarge { size }) => Some(Captured {
            digest: format!("oversize:{size}"),
            size,
            link: false,
        }),
        // A link is recorded as a link, never followed; anything else that cannot be read
        // safely (a hard link, a device, an unsupported identity) is recorded unreadable.
        Err(SourceFault::Refused(why)) => Some(Captured {
            digest: if why.contains("LinkRefused") {
                "link:".into()
            } else {
                "unreadable:".into()
            },
            size: 0,
            link: true,
        }),
        // Vanished since Git listed it: a deletion.
        Err(_) => None,
    }
}

/// Observes the worktree's snapshot (Git and bounded file reads; nothing is written).
pub fn observe(
    env: &IndexEnv,
    _ctx: &InvocationContext,
    repository: &Id,
    worktree: &Id,
    stop: &(dyn Fn() -> bool + Sync),
) -> Observed {
    Observed(observe_inner(env, repository, worktree, stop))
}
fn observe_inner(
    env: &IndexEnv,
    repository: &Id,
    worktree: &Id,
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<Snapshot, String> {
    let (wt, base, cfg) = read_snapshot(&*env.store, &ReadOptions::default(), |s| {
        let wt = s
            .indexing()
            .worktrees(repository)?
            .into_iter()
            .find(|w| &w.worktree_id == worktree);
        let base = match s.indexing().published(repository)? {
            Some(p) => match s.indexing().view(&p.view_id)? {
                Some(v) => s.indexing().generation(&v.baseline_id)?,
                None => None,
            },
            None => None,
        };
        let cfg = effective_config(s.indexing(), s.projects(), repository)?;
        Ok((wt, base, cfg))
    })
    .map_err(|e| e.to_string())?;
    let wt = wt.ok_or("the worktree is not a recorded worktree of this repository")?;
    let base = base.ok_or("the repository has no published baseline")?;
    let ctx = env.host_context(None);
    let git = super::git::Git {
        runner: &*env.helpers,
        ctx: &ctx,
        program: &env.settings.git_program,
        deadline: env.settings.git_deadline,
        stop,
    };
    let root = PathBuf::from(&wt.path);
    // The recorded path must still be a checkout (its own `.git` directory or a linked
    // worktree's `.git` file), never an arbitrary directory.
    match env.source.git_marker(&root) {
        Ok(Some(_)) => {}
        Ok(None) => return Err("the worktree is no longer a checkout".into()),
        Err(e) => return Err(format!("the worktree cannot be read: {e}")),
    }
    let head = git.head(&root).map_err(|e| e.to_string())?;
    let head_ref = git.branch(&root).map_err(|e| e.to_string())?;
    let against = head.clone().unwrap_or_else(|| {
        if base.source_digest.len() == 64 {
            EMPTY_TREE_SHA256.into()
        } else {
            EMPTY_TREE_SHA1.into()
        }
    });
    let (changes, refused) = git
        .local_changes(&root, &against)
        .map_err(|e| e.to_string())?;
    if changes.len() > MAX_LOCAL_CHANGES {
        return Err(format!(
            "the worktree has {} changed paths (more than {MAX_LOCAL_CHANGES}); exclude generated files or commit them",
            changes.len()
        ));
    }
    let mut local: BTreeMap<String, LocalChange> = BTreeMap::new();
    for c in changes {
        if stop() {
            return Err("cancelled".into());
        }
        let (path, previous) = match c {
            Change::Deleted(p) => {
                if !cfg.path_excluded(&p) {
                    local.insert(
                        p.clone(),
                        LocalChange {
                            path: p,
                            content: None,
                            previous: None,
                        },
                    );
                }
                continue;
            }
            Change::Added(p) | Change::Modified(p) => (p, None),
            Change::Renamed { from, to } => {
                if !cfg.path_excluded(&from) {
                    local.insert(
                        from.clone(),
                        LocalChange {
                            path: from.clone(),
                            content: None,
                            previous: None,
                        },
                    );
                }
                (to, Some(from))
            }
        };
        if cfg.path_excluded(&path) {
            continue;
        }
        let content = capture_file(env, &root, &path, cfg.max_bytes_for(&path));
        local.insert(
            path.clone(),
            LocalChange {
                path,
                content,
                previous,
            },
        );
    }
    let local: Vec<LocalChange> = local.into_values().collect();
    let mut basis = format!("{}\u{0}", head.as_deref().unwrap_or("unborn"));
    for l in &local {
        basis.push_str(&format!(
            "{}\u{0}{}\u{0}{}\u{1}",
            l.path,
            l.content.as_ref().map_or("deleted", |c| c.digest.as_str()),
            l.previous.as_deref().unwrap_or("")
        ));
    }
    Ok(Snapshot {
        worktree_path: wt.path.clone(),
        base_generation: base.id.clone(),
        base_oid: base.source_digest.clone(),
        head_oid: head,
        head_ref,
        capture_digest: Some(sha256_hex(basis.as_bytes())),
        local,
        refused,
    })
}

/// The commit a ref names now, observed before the unit of work (or why it names none).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// The commit and the full ref it resolved through (`None` for a commit ID).
    Commit {
        oid: String,
        full_ref: Option<String>,
    },
    /// The ref names no commit of the repository.
    Unknown,
    /// Git could not answer (the checkout is missing, a deadline, a cancellation).
    Failed(String),
}

/// Where a repository's own checkout is (its observed source location, else its registration).
fn checkout(read: &dyn ReadSnapshot, repository: &Id) -> Result<Option<String>, StorageError> {
    Ok(match read.indexing().repository_source(repository)? {
        Some(src) => Some(src.location),
        None => read.projects().repository(repository)?.map(|r| r.location),
    })
}

/// Resolves a ref overlay's ref in the repository's checkout (Git reads only; nothing is
/// written).
pub fn resolve(
    env: &IndexEnv,
    repository: &Id,
    name: &str,
    stop: &(dyn Fn() -> bool + Sync),
) -> Resolved {
    let location = match read_snapshot(&*env.store, &ReadOptions::default(), |s| {
        checkout(s, repository)
    }) {
        Ok(Some(l)) => l,
        Ok(None) => return Resolved::Failed("the repository is not registered".into()),
        Err(e) => return Resolved::Failed(e.to_string()),
    };
    let ctx = env.host_context(None);
    match super::worker::git_for(env, &ctx, stop).resolve(Path::new(&location), name) {
        Ok(Some((oid, full_ref))) => Resolved::Commit { oid, full_ref },
        Ok(None) => Resolved::Unknown,
        Err(e) => Resolved::Failed(e.to_string()),
    }
}

/// The snapshot a ref overlay is built from: its commit over the published baseline, read from
/// the repository's checkout (no worktree capture).
fn ref_snapshot(env: &IndexEnv, o: &StoredOverlay) -> Result<Snapshot, String> {
    let (base, location) = read_snapshot(&*env.store, &ReadOptions::default(), |s| {
        let base = match s.indexing().published(&o.repository_id)? {
            Some(p) => match s.indexing().view(&p.view_id)? {
                Some(v) => s.indexing().generation(&v.baseline_id)?,
                None => None,
            },
            None => None,
        };
        Ok((base, checkout(s, &o.repository_id)?))
    })
    .map_err(|e| e.to_string())?;
    let base = base.ok_or("the repository has no published baseline")?;
    Ok(Snapshot {
        worktree_path: location.ok_or("the repository is not registered")?,
        base_generation: base.id.clone(),
        base_oid: base.source_digest.clone(),
        head_oid: o.head_oid.clone(),
        head_ref: o.head_ref.clone(),
        capture_digest: None,
        local: Vec::new(),
        refused: Vec::new(),
    })
}

/// The overlay layers (branch, worktree) a view composes over its baseline.
fn view_layers(v: &StoredView) -> Vec<Id> {
    v.branch_id
        .iter()
        .chain(v.worktree_id.iter())
        .cloned()
        .collect()
}

/// Marks every ready overlay of `repositories` that was built under a configuration other than
/// its repository's effective one (after a `configure_sources` write) stale. Returns how many.
pub(crate) fn invalidate_for_configuration(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    repositories: &[Id],
) -> Result<usize, StorageError> {
    let mut n = 0;
    for repo in repositories {
        let overlays = tx.indexing().overlays(repo)?;
        if !overlays.iter().any(|o| o.state == OVERLAY_READY) {
            continue;
        }
        let effective = effective_config_tx(tx, repo)?;
        for o in overlays {
            if !super::status::overlay_config_moved(tx.indexing(), &o, &effective.digest)? {
                continue;
            }
            let now = tx.now_ms();
            let mut next = o.clone();
            next.revision += 1;
            next.state = OVERLAY_STALE.into();
            next.reason = Some("the source configuration changed".into());
            next.updated_at_ms = now;
            tx.indexing().put_overlay(&next, Some(o.revision))?;
            history(
                tx,
                ctx.principal(),
                ctx.project_id(),
                (OVERLAY_AGGREGATE, &o.id, next.revision),
                "IndexOverlayStale",
                json!({"reason": next.reason, "principalKind": format!("{:?}", ctx.principal_kind())}),
            )?;
            n += 1;
        }
    }
    Ok(n)
}

/// Whether a stored overlay holds exactly this snapshot.
fn same_snapshot(o: &StoredOverlay, s: &Snapshot) -> bool {
    o.base_generation_id.as_ref() == Some(&s.base_generation)
        && o.head_oid == s.head_oid
        && o.capture_digest == s.capture_digest
}

/// What a refresh asks an overlay of: a worktree (with its snapshot observed before the unit
/// of work), or from schema revision 2 a ref (with its resolution).
pub(crate) enum Requested<'a> {
    Worktree {
        id: &'a Id,
        observed: Option<&'a Observed>,
    },
    Ref {
        resolved: Option<&'a Resolved>,
    },
}

/// `indexing.refresh_overlay`: reuse a live overlay of the same snapshot (a worktree's
/// capture, or a ref's commit), otherwise queue its build.
pub(crate) fn refresh(
    env: &IndexEnv,
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    repository: &Id,
    requested: Requested<'_>,
    expected: i64,
) -> Result<DomainDecision, Failure> {
    let r = visible_repository(tx.projects(), ctx, repository)?;
    if let Some(own) = ctx.project_id() {
        let p = tx.projects().project(own)?;
        if p.as_ref()
            .is_none_or(|p| crate::services::runs::closed_to_work(&p.lifecycle))
        {
            return Err(refuse(
                DomainErrorCode::Conflict,
                "project_archived",
                Some(&own.0),
            ));
        }
        if !tx.projects().members(own)?.contains(&r.id) {
            return Err(refuse(
                DomainErrorCode::NotFound,
                "repository",
                Some(repository),
            ));
        }
    }
    if r.lifecycle != ACTIVE {
        return Err(refuse(
            DomainErrorCode::Conflict,
            "repository_archived",
            Some(repository),
        ));
    }
    let section = section_key(ctx.project_id());
    let current = section_config(tx.indexing(), &section)?;
    if current.revision != expected {
        return Err(Failure::Storage(StorageError::conflict(
            RevisionDimension::Configuration,
            "index_source_config",
            Id::new(section).expect("valid section key"),
            rev(expected),
            rev(current.revision),
        )));
    }
    // The worktree (and its path), or the commit and full ref a ref names now.
    let (wt, commit) = match requested {
        Requested::Worktree { id, observed } => {
            let Some(wt) = tx
                .indexing()
                .worktrees(&r.id)?
                .into_iter()
                .find(|w| &w.worktree_id == id)
            else {
                return Err(refuse(DomainErrorCode::NotFound, "worktree", Some(id)));
            };
            (Some((wt, observed)), None)
        }
        Requested::Ref { resolved } => match resolved {
            Some(Resolved::Commit { oid, full_ref }) => (None, Some((oid, full_ref))),
            Some(Resolved::Unknown) => {
                return Err(refuse(DomainErrorCode::InvalidArgument, "ref", None));
            }
            Some(Resolved::Failed(_)) | None => {
                return Err(refuse(DomainErrorCode::ExternalFailure, "ref", None));
            }
        },
    };
    let Some(published) = tx.indexing().published(&r.id)? else {
        return Err(refuse(
            DomainErrorCode::ViewUnavailable,
            "repository_view",
            Some(&r.id),
        ));
    };
    let base = tx
        .indexing()
        .view(&published.view_id)?
        .map(|v| v.baseline_id);
    let snapshot = wt
        .as_ref()
        .and_then(|(_, observed)| observed.and_then(|o| o.0.as_ref().ok()));
    let now = tx.now_ms();
    let existing = match (&wt, &commit) {
        (Some((w, _)), _) => tx.indexing().overlay_for(&r.id, &w.worktree_id)?,
        (None, Some((oid, _))) => tx.indexing().overlay_at(&r.id, oid)?,
        (None, None) => None,
    };
    let effective = effective_config_tx(tx, &r.id)?;
    if let Some(o) = &existing {
        if let Some(active) = tx.indexing().active_run(&r.id, Some(&o.id))? {
            // A build is queued or running: it captures the worktree when it runs.
            return Ok(DomainDecision::NoChange {
                result: OperationResult::IndexingRefreshOverlay {
                    value: record_of(&active),
                },
            });
        }
        // The reuse identity is the snapshot (a ref overlay's is its commit, which `existing`
        // already matched) and the configuration it was built under.
        let moved = super::status::overlay_config_moved(tx.indexing(), o, &effective.digest)?;
        let state = super::status::effective_overlay_state(o, base.as_ref(), now, moved);
        if state == OVERLAY_READY
            && (commit.is_some() || snapshot.is_some_and(|s| same_snapshot(o, s)))
            && let Some(run) = o
                .run_id
                .as_ref()
                .map(|id| tx.indexing().run(id))
                .transpose()?
                .flatten()
        {
            // The same snapshot is live: reused, never rebuilt.
            return Ok(DomainDecision::NoChange {
                result: OperationResult::IndexingRefreshOverlay {
                    value: record_of(&run),
                },
            });
        }
    }
    let overlay_id = existing
        .as_ref()
        .map_or_else(|| tx.new_id(IdKind::Overlay), |o| o.id.clone());
    let run = new_run(
        tx,
        RUN_OVERLAY,
        &r.id,
        ctx.project_id(),
        Some(&overlay_id),
        &effective,
        current.revision,
        ctx.principal(),
        env.settings.max_attempts,
    );
    tx.indexing().insert_run(&run)?;
    let ttl = env.settings.overlay_ttl_ms;
    let worktree_id = wt.as_ref().map(|(w, _)| w.worktree_id.clone());
    let worktree_path = wt.as_ref().map(|(w, _)| w.path.clone());
    let next = match existing {
        None => StoredOverlay {
            id: overlay_id.clone(),
            repository_id: r.id.clone(),
            worktree_id: worktree_id.clone(),
            worktree_path,
            state: OVERLAY_QUEUED.into(),
            revision: 1,
            base_generation_id: None,
            base_oid: None,
            // A ref overlay's commit (its identity) and the ref it was requested by.
            head_oid: commit.map(|(oid, _)| oid.clone()),
            head_ref: commit.and_then(|(_, r)| r.clone()),
            capture_digest: None,
            branch_generation_id: None,
            worktree_generation_id: None,
            view_id: None,
            run_id: Some(run.id.clone()),
            reason: None,
            created_at_ms: now,
            updated_at_ms: now,
            expires_at_ms: now + ttl,
        },
        Some(o) => StoredOverlay {
            state: OVERLAY_QUEUED.into(),
            revision: o.revision + 1,
            worktree_path,
            head_ref: match commit {
                Some((_, r)) => r.clone(),
                None => o.head_ref.clone(),
            },
            run_id: Some(run.id.clone()),
            reason: None,
            updated_at_ms: now,
            expires_at_ms: now + ttl,
            ..o
        },
    };
    let expected_overlay = (next.revision > 1).then_some(next.revision - 1);
    tx.indexing().put_overlay(&next, expected_overlay)?;
    history(
        tx,
        ctx.principal(),
        ctx.project_id(),
        (OVERLAY_AGGREGATE, &overlay_id, next.revision),
        "IndexOverlayRequested",
        json!({
            "repositoryId": r.id.as_str(), "worktreeId": worktree_id.as_ref().map(Id::as_str),
            "runId": run.id.as_str(),
            "commit": commit.map(|(oid, _)| oid.as_str()), "ref": commit.and_then(|(_, r)| r.as_deref()),
            "observedCapture": snapshot.and_then(|s| s.capture_digest.clone()),
            "observedHead": snapshot.and_then(|s| s.head_oid.clone()),
            "principalKind": format!("{:?}", ctx.principal_kind()),
        }),
    )?;
    history(
        tx,
        ctx.principal(),
        ctx.project_id(),
        (RUN_AGGREGATE, &run.id, run.revision),
        "IndexRunQueued",
        json!({"kind": RUN_OVERLAY, "repositoryId": r.id.as_str(), "overlayId": overlay_id.as_str(),
            "principalKind": format!("{:?}", ctx.principal_kind())}),
    )?;
    Ok(DomainDecision::Applied {
        result: OperationResult::IndexingRefreshOverlay {
            value: record_of(&run),
        },
    })
}

/// Sets an overlay's state on behalf of the run it names (a stale run changes nothing).
pub(crate) fn settle_overlay(
    tx: &mut dyn WriteTransaction,
    overlay: &Id,
    run: &Id,
    state: &str,
    reason: Option<&str>,
) -> Result<(), StorageError> {
    let Some(mut o) = tx.indexing().overlay(overlay)? else {
        return Ok(());
    };
    if o.run_id.as_ref() != Some(run)
        || matches!(o.state.as_str(), OVERLAY_EXPIRED | OVERLAY_UNAVAILABLE)
    {
        return Ok(());
    }
    let expected = o.revision;
    o.revision += 1;
    o.state = state.into();
    o.reason = reason.map(|r| r.chars().take(4096).collect());
    o.updated_at_ms = tx.now_ms();
    tx.indexing().put_overlay(&o, Some(expected))
}

/// The claimed run's overlay is now indexing.
pub(crate) fn mark_indexing(
    tx: &mut dyn WriteTransaction,
    overlay: &Id,
    run: &Id,
) -> Result<(), StorageError> {
    settle_overlay(tx, overlay, run, OVERLAY_INDEXING, None)
}

/// Ends an overlay's derived data: its active run is cancelled, its view removed and its layers
/// retired; the record (and history) stays with `state`. Returns the new revision.
fn end_overlay(
    tx: &mut dyn WriteTransaction,
    o: &StoredOverlay,
    state: &str,
    reason: &str,
) -> Result<i64, StorageError> {
    let now = tx.now_ms();
    if let Some(mut run) = tx.indexing().active_run(&o.repository_id, Some(&o.id))? {
        let expected = run.revision;
        run.revision += 1;
        run.cancel_requested_at_ms = Some(now);
        if run.state == RUN_QUEUED {
            run.state = RUN_CANCELLED.into();
            run.stage = "cancelled".into();
            run.finished_at_ms = Some(now);
        } else {
            run.stage = "cancelling".into();
        }
        tx.indexing().update_run(&run, expected)?;
    }
    // The view's own layers, which a build in flight may no longer name on the record.
    let mut layers: Vec<Id> = Vec::new();
    if let Some(v) = &o.view_id {
        let named = tx
            .indexing()
            .published(&o.repository_id)?
            .is_some_and(|p| &p.view_id == v);
        if !named && let Some(view) = tx.indexing().view(v)? {
            layers.extend(view_layers(&view));
            tx.indexing().remove_view(v)?;
        }
    }
    layers.extend(o.branch_generation_id.iter().cloned());
    layers.extend(o.worktree_generation_id.iter().cloned());
    for g in &layers {
        retire_unused(tx, g)?;
    }
    let mut next = o.clone();
    next.revision += 1;
    next.state = state.into();
    next.reason = Some(reason.into());
    next.view_id = None;
    next.updated_at_ms = now;
    tx.indexing().put_overlay(&next, Some(o.revision))?;
    Ok(next.revision)
}

/// `indexing.retire_view` of an overlay.
pub(crate) fn retire_overlay(
    tx: &mut dyn WriteTransaction,
    o: &StoredOverlay,
    reason: &str,
) -> Result<i64, StorageError> {
    end_overlay(tx, o, OVERLAY_UNAVAILABLE, reason)
}

/// Retention expiry of an overlay (housekeeping).
pub(crate) fn expire_overlay(
    tx: &mut dyn WriteTransaction,
    o: &StoredOverlay,
) -> Result<(), StorageError> {
    let revision = end_overlay(tx, o, OVERLAY_EXPIRED, "the overlay retention expired")?;
    let host = tx.store().host_actor.clone();
    history(
        tx,
        &host,
        None,
        (OVERLAY_AGGREGATE, &o.id, revision),
        "IndexOverlayExpired",
        json!({"worktreeId": o.worktree_id.as_ref().map(Id::as_str), "principalKind": "Host"}),
    )
}

/// Record retention of ended ref overlays (CODE-11; separate from artifact retention): at most
/// [`super::IndexSettings::overlay_prune_batch`] records that ended longer ago than
/// `indexing.overlayRecordRetentionDays` and are beyond the `indexing.overlayRecordsKept` most
/// recently ended ref overlays of their repository are removed, each in its own short write
/// that re-checks it: still ended at the same revision, naming no view, with no active run and
/// no layer a view names. Its layers are retired (the generation reclamation pass removes their
/// graph data); its runs, history and logs stay, and the removal is recorded in history. Returns
/// how many were pruned.
pub(crate) fn prune_ref_overlays(
    env: &IndexEnv,
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<usize, StorageError> {
    let now = env.now();
    let batch = env.settings.overlay_prune_batch;
    if batch == 0 {
        return Ok(0);
    }
    let candidates = read_snapshot(&*env.store, &ReadOptions::default(), |s| {
        let days = crate::services::diagnostics::setting(
            s.configuration(),
            OVERLAY_RECORD_RETENTION_DAYS_KEY,
        )?;
        let keep =
            crate::services::diagnostics::setting(s.configuration(), OVERLAY_RECORDS_KEPT_KEY)?;
        let ended_before = now.saturating_sub(days.saturating_mul(86_400_000));
        // A few more than the batch, so records a view still names do not stall the pass.
        s.indexing().prunable_ref_overlays(
            ended_before,
            u32::try_from(keep.max(0)).unwrap_or(u32::MAX),
            batch.saturating_mul(4),
        )
    })?;
    let mut pruned = 0;
    for o in candidates {
        if pruned >= usize::from(batch) || stop() {
            break;
        }
        let removed = run_write(&*env.store, WriteOptions::default(), move |tx| {
            let Some(cur) = tx.indexing().overlay(&o.id)? else {
                return Ok(false);
            };
            let layers: Vec<Id> = [&cur.branch_generation_id, &cur.worktree_generation_id]
                .into_iter()
                .flatten()
                .cloned()
                .collect();
            let mut in_view = false;
            for g in &layers {
                in_view |= tx.indexing().generation_in_view(g)?;
            }
            // The same revision keeps what the selection checked (ended, no worktree, no view);
            // a view may since name one of its layers, and a run may have been queued.
            if cur.revision != o.revision
                || in_view
                || tx
                    .indexing()
                    .active_run(&cur.repository_id, Some(&cur.id))?
                    .is_some()
            {
                return Ok(false);
            }
            for g in &layers {
                retire_unused(tx, g)?;
            }
            tx.indexing().remove_overlay(&cur.id, cur.revision)?;
            let host = tx.store().host_actor.clone();
            history(
                tx,
                &host,
                None,
                (OVERLAY_AGGREGATE, &cur.id, cur.revision + 1),
                "IndexOverlayPruned",
                json!({
                    "repositoryId": cur.repository_id.as_str(),
                    "headCommit": cur.head_oid,
                    "state": cur.state,
                    "principalKind": "Host",
                }),
            )?;
            Ok(true)
        })?;
        pruned += usize::from(removed);
    }
    Ok(pruned)
}

/// Housekeeping observation: ready overlays whose worktree or baseline moved are marked stale
/// and rebuilt; stale ones without a build get one. Returns (marked stale, builds queued).
pub(crate) fn observe_all(
    env: &IndexEnv,
    cursor: &std::sync::Mutex<Option<Id>>,
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<(usize, usize), StorageError> {
    // A bounded slice per pass, continuing after the overlay the previous pass ended at (and
    // wrapping around), so every live overlay is observed in turn.
    let start = cursor.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let mut all = read_snapshot(&*env.store, &ReadOptions::default(), |s| {
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let page = s.projects().repositories(
                &RepositoryFilter::default(),
                after.as_ref(),
                MAX_PAGE,
            )?;
            for r in &page {
                for o in s.indexing().overlays(&r.id)? {
                    if matches!(o.state.as_str(), OVERLAY_READY | OVERLAY_STALE) {
                        out.push(o);
                    }
                }
            }
            if page.len() < MAX_PAGE as usize {
                return Ok(out);
            }
            after = page.last().map(|r| r.id.clone());
        }
    })?;
    all.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    let split = start
        .as_ref()
        .map_or(0, |c| all.partition_point(|o| o.id.as_str() <= c.as_str()));
    let split = split.min(all.len());
    all.rotate_left(split);
    all.truncate(OBSERVED_PER_PASS);
    let next = all.last().map(|o| o.id.clone());
    let overlays = all;
    *cursor.lock().unwrap_or_else(|p| p.into_inner()) = next;
    let (mut stale, mut queued) = (0, 0);
    let ctx = env.host_context(None);
    for o in overlays {
        if stop() {
            break;
        }
        // A ref overlay's commit never moves; only its baseline or configuration can.
        let moved = match &o.worktree_id {
            Some(w) => match observe(env, &ctx, &o.repository_id, w, stop).0 {
                Ok(s) => !same_snapshot(&o, &s),
                // Unobservable (the worktree went away): leave it for retirement or expiry.
                Err(_) => false,
            },
            None => false,
        };
        let config_moved = read_snapshot(&*env.store, &ReadOptions::default(), |s| {
            let c = super::effective_config(s.indexing(), s.projects(), &o.repository_id)?;
            super::status::overlay_config_moved(s.indexing(), &o, &c.digest)
        })?;
        if o.state == OVERLAY_READY && !moved && !config_moved {
            continue;
        }
        let ttl = env.settings.overlay_ttl_ms;
        let max = env.settings.max_attempts;
        let did = run_write(&*env.store, WriteOptions::default(), move |tx| {
            let Some(cur) = tx.indexing().overlay(&o.id)? else {
                return Ok((false, false));
            };
            if cur.revision != o.revision
                || tx
                    .indexing()
                    .active_run(&cur.repository_id, Some(&cur.id))?
                    .is_some()
            {
                return Ok((false, false));
            }
            let section = section_config(tx.indexing(), WORKSPACE_SCOPE_KEY)?;
            let effective = effective_config_tx(tx, &cur.repository_id)?;
            let host = tx.store().host_actor.clone();
            let run = new_run(
                tx,
                RUN_OVERLAY,
                &cur.repository_id,
                None,
                Some(&cur.id),
                &effective,
                section.revision,
                &host,
                max,
            );
            tx.indexing().insert_run(&run)?;
            let now = tx.now_ms();
            let mut next = cur.clone();
            next.revision += 1;
            next.state = OVERLAY_STALE.into();
            next.reason = Some(
                if moved {
                    "the worktree changed"
                } else if config_moved {
                    "the source configuration changed"
                } else {
                    "the repository baseline moved"
                }
                .into(),
            );
            next.run_id = Some(run.id.clone());
            next.updated_at_ms = now;
            next.expires_at_ms = next.expires_at_ms.max(now + ttl.min(60_000));
            tx.indexing().put_overlay(&next, Some(cur.revision))?;
            history(
                tx,
                &host,
                None,
                (OVERLAY_AGGREGATE, &cur.id, next.revision),
                "IndexOverlayStale",
                json!({"reason": next.reason, "runId": run.id.as_str(), "principalKind": "Host"}),
            )?;
            Ok((cur.state == OVERLAY_READY, true))
        })?;
        stale += usize::from(did.0);
        queued += usize::from(did.1);
    }
    Ok((stale, queued))
}

// ---------------------------------------------------------------------------------------------
// The build

/// Builds and publishes the overlay of a claimed overlay run.
pub(crate) fn execute(exec: &mut Exec<'_>) -> Result<(), Stop> {
    let env = exec.w.env.clone();
    let Some(overlay_id) = exec.run.overlay_id.clone() else {
        return Err(Stop::det(
            "overlay_missing",
            "an overlay run names no overlay",
        ));
    };
    let repo_id = exec.run.repository_id.clone();
    let (overlay, repo) = read_snapshot(&*env.store, &ReadOptions::default(), |s| {
        Ok((
            s.indexing().overlay(&overlay_id)?,
            s.projects().repository(&repo_id)?,
        ))
    })
    .map_err(Stop::storage)?;
    let Some(overlay) = overlay else {
        return Err(Stop::det("overlay_missing", "the overlay record is gone"));
    };
    let Some(repo) = repo.filter(|r| r.lifecycle == ACTIVE) else {
        return Err(Stop::det(
            "repository_unavailable",
            "the repository is not an active registration",
        ));
    };
    exec.stage("capturing")?;
    let stop_flag = exec.stop.clone();
    let stopping = &exec.w.stopping;
    let stop = move || stop_flag.load(Ordering::SeqCst) || stopping.load(Ordering::SeqCst);
    let observed = match &overlay.worktree_id {
        Some(w) => observe_inner(&env, &repo_id, w, &stop),
        None => ref_snapshot(&env, &overlay),
    };
    let snap = observed.map_err(|e| {
        if stop() {
            Stop::Cancelled
        } else if e.contains("no published baseline") {
            Stop::det("not_indexed", e)
        } else if e.contains("not a recorded worktree") {
            Stop::det("worktree_missing", e)
        } else {
            Stop::transient("capture", e)
        }
    })?;
    for p in snap.refused.iter().take(16) {
        push_warning(
            &mut exec.warnings,
            "path_refused",
            format!("{p:?} is not a safe relative UTF-8 path; skipped"),
        );
    }
    let (cfg, base_view) = read_snapshot(&*env.store, &ReadOptions::default(), |s| {
        Ok((
            effective_config(s.indexing(), s.projects(), &repo_id)?,
            s.indexing().published(&repo_id)?,
        ))
    })
    .map_err(Stop::storage)?;
    let base_pointer = base_view.map_or(0, |p| p.revision);
    let root = PathBuf::from(&snap.worktree_path);
    let head = snap.head_oid.clone();
    if let Some(h) = &head {
        let h = h.clone();
        exec.fenced(WritePriority::Authoritative, move |_, r| {
            r.source_oid = Some(h);
            Ok(())
        })?;
    }

    // The branch layer: base..HEAD. A ref overlay always has one (empty at the baseline commit),
    // so its view is never a bare baseline view.
    let ref_overlay = overlay.worktree_id.is_none();
    let mut layers: Vec<Id> = vec![snap.base_generation.clone()];
    let mut head_entries: Vec<TreeEntry> = Vec::new();
    let mut branch_generation: Option<Id> = None;
    let git = exec.git(&stop);
    if let Some(h) = &head {
        let (entries, _) = git.tree(&root, h).map_err(Stop::git)?;
        head_entries = entries;
    }
    let diff: Vec<Change> = match &head {
        Some(h) if *h != snap.base_oid => match git.diff(&root, &snap.base_oid, h) {
            Ok((c, _)) => c,
            Err(e) if e.cancelled() => return Err(Stop::Cancelled),
            Err(e) => {
                return Err(Stop::det(
                    "base_unavailable",
                    format!("the baseline commit cannot be compared: {e}"),
                ));
            }
        },
        _ => Vec::new(),
    };
    // Unchanged files the commit plans differently from the baseline (another configuration or
    // extractor, a dependency manifest added or removed) are reconciled in the branch layer,
    // even at the baseline commit, so the overlay never shows the baseline's plan for them.
    let reconciled = plan_changes(
        &env,
        &cfg,
        &layers,
        &head_entries,
        &diff,
        env.source.case_insensitive(&root),
    )
    .map_err(Stop::storage)?;
    if let Some(h) = head
        .as_ref()
        .filter(|h| **h != snap.base_oid || ref_overlay || !reconciled.is_empty())
    {
        exec.stage("branch")?;
        let mut changes = diff;
        changes.extend(reconciled.iter().cloned());
        let (units, groups) = layer_units(
            &env,
            &cfg,
            &head_entries,
            &changes,
            &mut exec.warnings,
            env.source.case_insensitive(&root),
        );
        let stored = new_layer(exec, &repo_id, LAYER_BRANCH, h, &snap, &layers)?;
        exec.shares.clear();
        let groups: BTreeMap<&'static str, Vec<&PlanUnit>> = groups
            .iter()
            .map(|(g, m)| (*g, m.iter().collect()))
            .collect();
        exec.group_shares(&root, &repo.name, &groups, None, &stop)?;
        stage_layer(exec, &root, &repo.name, &stored, &units, None, &stop)?;
        link_generation(exec, &stored, &layers, &stop)?;
        seal_layer(exec, &stored)?;
        layers.insert(0, stored.clone());
        branch_generation = Some(stored);
    }

    // The worktree layer: staged, unstaged and untracked changes against HEAD (a ref overlay
    // has none).
    let wgen = if ref_overlay {
        None
    } else {
        exec.stage("worktree")?;
        let mut local_changes: Vec<Change> = snap
            .local
            .iter()
            .flat_map(|l| match (&l.content, &l.previous) {
                (None, _) => vec![Change::Deleted(l.path.clone())],
                (Some(_), Some(prev)) => vec![Change::Renamed {
                    from: prev.clone(),
                    to: l.path.clone(),
                }],
                (Some(_), None) => vec![Change::Modified(l.path.clone())],
            })
            .collect();
        // The composed tree: HEAD's entries with the worktree's adds and deletes applied.
        let mut composed: BTreeMap<String, TreeEntry> = head_entries
            .iter()
            .map(|e| (e.path.clone(), e.clone()))
            .collect();
        for l in &snap.local {
            match &l.content {
                None => {
                    composed.remove(&l.path);
                }
                Some(c) => {
                    composed.insert(
                        l.path.clone(),
                        TreeEntry {
                            mode: match (c.link, c.digest.as_str()) {
                                (true, "unreadable:") => super::worker::UNREADABLE_MODE.into(),
                                (true, _) => "120000".into(),
                                _ => "100644".into(),
                            },
                            oid: c.digest.clone(),
                            size: c.size,
                            path: l.path.clone(),
                        },
                    );
                }
            }
        }
        let composed: Vec<TreeEntry> = composed.into_values().collect();
        // Unchanged files the worktree's changes plan differently (a dependency manifest added
        // or removed locally) are reconciled in this layer.
        let reconciled = plan_changes(
            &env,
            &cfg,
            &layers,
            &composed,
            &local_changes,
            env.source.case_insensitive(&root),
        )
        .map_err(Stop::storage)?;
        local_changes.extend(reconciled);
        let (units, groups) = layer_units(
            &env,
            &cfg,
            &composed,
            &local_changes,
            &mut exec.warnings,
            env.source.case_insensitive(&root),
        );
        let capture = snap.capture_digest.clone().unwrap_or_default();
        let wgen = new_layer(exec, &repo_id, LAYER_WORKTREE, &capture, &snap, &layers)?;
        let expected: BTreeMap<String, String> = snap
            .local
            .iter()
            .filter_map(|l| {
                l.content
                    .as_ref()
                    .map(|c| (l.path.clone(), c.digest.clone()))
            })
            .collect();
        exec.shares.clear();
        let groups: BTreeMap<&'static str, Vec<&PlanUnit>> = groups
            .iter()
            .map(|(g, m)| (*g, m.iter().collect()))
            .collect();
        exec.group_shares(&root, &repo.name, &groups, Some(&expected), &stop)?;
        stage_layer(
            exec,
            &root,
            &repo.name,
            &wgen,
            &units,
            Some(&expected),
            &stop,
        )?;
        link_generation(exec, &wgen, &layers, &stop)?;
        seal_layer(exec, &wgen)?;
        Some(wgen)
    };

    // Publication: the view of exactly these layers, while the baseline is still the one it
    // composes over.
    exec.stage("publishing")?;
    let view_id = env.ids.next_id(IdKind::View);
    let (o_id, base_gen, repo2) = (
        overlay_id.clone(),
        snap.base_generation.clone(),
        repo_id.clone(),
    );
    let snap2 = snap.clone();
    let ttl = env.settings.overlay_ttl_ms;
    let published = exec.fenced(WritePriority::Authoritative, move |tx, r| {
        let current = tx.indexing().published(&repo2)?;
        if current.as_ref().map_or(0, |p| p.revision) != base_pointer {
            return Ok(Err(
                "the repository baseline moved while the overlay was built",
            ));
        }
        let Some(mut o) = tx.indexing().overlay(&o_id)? else {
            return Ok(Err("the overlay record is gone"));
        };
        if o.run_id.as_ref() != Some(&r.id)
            || matches!(o.state.as_str(), OVERLAY_EXPIRED | OVERLAY_UNAVAILABLE)
        {
            return Ok(Err("the overlay was retired or rebuilt by another run"));
        }
        let view = StoredView {
            view_id: view_id.clone(),
            repository_id: repo2.clone(),
            view_digest: digest(&[
                repo2.as_str(),
                base_gen.as_str(),
                branch_generation.as_ref().map_or("", Id::as_str),
                wgen.as_ref().map_or("", Id::as_str),
                o.worktree_id.as_ref().map_or("", Id::as_str),
                snap2.capture_digest.as_deref().unwrap_or(""),
            ]),
            baseline_id: base_gen.clone(),
            branch_id: branch_generation.clone(),
            worktree_id: wgen.clone(),
            coverage: json!({
                "base": snap2.base_oid, "head": snap2.head_oid, "headRef": snap2.head_ref,
                "worktreeId": o.worktree_id.as_ref().map(Id::as_str), "captureDigest": snap2.capture_digest,
            }),
        };
        tx.indexing().insert_view(&view)?;
        // The previous view of this overlay and its layers go. Its layers are read from the
        // view itself: this build's `new_layer` already replaced the IDs on the record.
        let previous = o.clone();
        let mut old_layers: Vec<Id> = Vec::new();
        if let Some(v) = &previous.view_id
            && let Some(old) = tx.indexing().view(v)?
        {
            old_layers.extend(view_layers(&old));
            tx.indexing().remove_view(v)?;
        }
        let now = tx.now_ms();
        let expected = o.revision;
        o.revision += 1;
        o.state = OVERLAY_READY.into();
        o.base_generation_id = Some(base_gen.clone());
        o.base_oid = Some(snap2.base_oid.clone());
        o.head_oid = snap2.head_oid.clone();
        o.head_ref = snap2.head_ref.clone();
        o.capture_digest = snap2.capture_digest.clone();
        o.branch_generation_id = branch_generation.clone();
        o.worktree_generation_id = wgen.clone();
        o.view_id = Some(view_id.clone());
        o.reason = None;
        o.updated_at_ms = now;
        o.expires_at_ms = now + ttl;
        tx.indexing().put_overlay(&o, Some(expected))?;
        for g in old_layers.iter().chain(
            [
                previous.branch_generation_id.as_ref(),
                previous.worktree_generation_id.as_ref(),
            ]
            .into_iter()
            .flatten(),
        ) {
            if Some(g) != o.branch_generation_id.as_ref()
                && Some(g) != o.worktree_generation_id.as_ref()
            {
                retire_unused(tx, g)?;
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
            (OVERLAY_AGGREGATE, &o.id, o.revision),
            "IndexOverlayReady",
            json!({
                "viewId": view_id.as_str(), "base": snap2.base_oid, "head": snap2.head_oid,
                "captureDigest": snap2.capture_digest, "worktreeId": o.worktree_id.as_ref().map(Id::as_str),
                "principalKind": "Host",
            }),
        )?;
        Ok(Ok(()))
    })?;
    match published {
        Ok(()) => {
            exec.log("published", Severity::Info, "overlay ready", None);
            Ok(())
        }
        Err(why) if why.contains("baseline moved") => Err(Stop::transient("base_moved", why)),
        Err(why) => Err(Stop::det("overlay_superseded", why)),
    }
}

/// The files whose plan changed although their content did not, relative to the layers below
/// (`lower`, highest first, the baseline last): every planned file as modified when a lower
/// layer was built by another extractor; otherwise, when the baseline was built under another
/// configuration or `changes` add, remove or rename a dependency-directory manifest
/// (`Podfile`, `Cargo.toml`, ...: [`MANIFEST_DIRECTORIES`]), each file `tree` plans with
/// another mode or language than the lower layers show (or that they lack) as modified, and
/// each lower file the plan no longer has (excluded now) as deleted. Nothing else changes the
/// plan of an unchanged file (`.gitignore` governs only untracked capture, and repository-level
/// groups are regrouped by their own changed members), so otherwise the comparison is skipped.
fn plan_changes(
    env: &IndexEnv,
    cfg: &EffectiveConfig,
    lower: &[Id],
    tree: &[TreeEntry],
    changes: &[Change],
    case_insensitive: bool,
) -> Result<Vec<Change>, StorageError> {
    let manifests = read_snapshot(&*env.store, &ReadOptions::default(), |s| {
        let mut out = Vec::new();
        for g in lower {
            out.push(
                s.indexing()
                    .generation(g)?
                    .map(|g| g.manifest)
                    .unwrap_or_default(),
            );
        }
        Ok(out)
    })?;
    let same_extractor = manifests.iter().all(|m| {
        m.get("extractorVersion")
            .and_then(serde_json::Value::as_i64)
            == Some(super::config::EXTRACTOR_VERSION)
    });
    let same_config = manifests
        .last()
        .and_then(|m| m.get("configDigest"))
        .and_then(serde_json::Value::as_str)
        == Some(cfg.digest.as_str());
    let manifest_name = |p: &str| {
        let name = p.rsplit('/').next().unwrap_or(p);
        MANIFEST_DIRECTORIES
            .iter()
            .any(|(_, ms)| ms.contains(&name))
    };
    let manifest_moved = changes.iter().any(|c| match c {
        Change::Added(p) | Change::Modified(p) | Change::Deleted(p) => manifest_name(p),
        Change::Renamed { from, to } => manifest_name(from) || manifest_name(to),
    });
    if same_extractor && same_config && !manifest_moved {
        return Ok(Vec::new());
    }
    let lower_units = composed_inventory(env, lower)?;
    let mut ignored = Vec::new();
    let (planned, _) = plan(
        env,
        cfg,
        tree,
        &BTreeMap::new(),
        &mut ignored,
        case_insensitive,
    );
    let mut out = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for u in planned.iter().filter(|u| !u.key.starts_with('/')) {
        seen.insert(u.key.as_str());
        let same = same_extractor
            && lower_units.get(&u.key).is_some_and(|(mode, language)| {
                mode == u.mode && language.as_deref() == u.language
            });
        if !same {
            out.push(Change::Modified(u.key.clone()));
        }
    }
    for key in lower_units.keys() {
        if !seen.contains(key.as_str()) {
            out.push(Change::Deleted(key.clone()));
        }
    }
    Ok(out)
}

/// The file units `layers` (highest first) compose, with their mode and language: a unit a
/// higher layer replaces or deletes is the higher layer's (a deletion hides it).
fn composed_inventory(
    env: &IndexEnv,
    layers: &[Id],
) -> Result<BTreeMap<String, (String, Option<String>)>, StorageError> {
    let mut seen: BTreeMap<String, Option<(String, Option<String>)>> = BTreeMap::new();
    for g in layers {
        let mut after: Option<String> = None;
        loop {
            let page = read_snapshot(&*env.store, &ReadOptions::default(), |s| {
                s.indexing().units(g, after.as_deref(), MAX_PAGE)
            })?;
            for u in &page {
                if u.unit_key.starts_with('/') || seen.contains_key(&u.unit_key) {
                    continue;
                }
                let facts = (u.action == UNIT_REPLACE).then(|| {
                    (
                        u.payload["mode"].as_str().unwrap_or("").to_owned(),
                        u.payload["language"].as_str().map(str::to_owned),
                    )
                });
                seen.insert(u.unit_key.clone(), facts);
            }
            if page.len() < usize::from(MAX_PAGE) {
                break;
            }
            after = page.last().map(|u| u.unit_key.clone());
        }
    }
    Ok(seen
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| (k, v)))
        .collect())
}

/// The units of one layer: changed files (replaced or deleted) and the directories they affect,
/// planned over the layer's full tree.
fn layer_units(
    env: &IndexEnv,
    cfg: &EffectiveConfig,
    tree: &[TreeEntry],
    changes: &[Change],
    warnings: &mut Vec<RunWarning>,
    case_insensitive: bool,
) -> (Vec<LayerUnit>, BTreeMap<&'static str, Vec<PlanUnit>>) {
    let mut renamed: BTreeMap<String, String> = BTreeMap::new();
    let mut changed: BTreeSet<String> = BTreeSet::new();
    for c in changes {
        match c {
            Change::Added(p) | Change::Modified(p) | Change::Deleted(p) => {
                changed.insert(p.clone());
            }
            Change::Renamed { from, to } => {
                changed.insert(from.clone());
                changed.insert(to.clone());
                renamed.insert(to.clone(), from.clone());
            }
        }
    }
    let (planned, _) = plan(env, cfg, tree, &renamed, warnings, case_insensitive);
    let by_key: BTreeMap<String, PlanUnit> =
        planned.into_iter().map(|u| (u.key.clone(), u)).collect();
    // Affected directories: every ancestor of a changed path.
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    for p in &changed {
        let mut d = super::paths::parent(p).to_owned();
        loop {
            dirs.insert(dir_key(&d));
            if d.is_empty() {
                break;
            }
            d = super::paths::parent(&d).to_owned();
        }
    }
    // A repository-level group with a changed member (added, changed or deleted) is staged
    // again as a whole in this layer: each member's output depends on the others.
    let mut groups: BTreeMap<&'static str, Vec<PlanUnit>> = BTreeMap::new();
    for (g, members) in super::worker::group_members(env, cfg, by_key.values()) {
        if changed.iter().any(|k| env.parser.group_of(k) == Some(g)) {
            groups.insert(g, members.into_iter().cloned().collect());
        }
    }
    let regrouped: BTreeSet<String> = groups
        .values()
        .flatten()
        .map(|u| u.key.clone())
        .filter(|k| !changed.contains(k))
        .collect();
    let mut out = Vec::new();
    for key in changed.iter().chain(dirs.iter()).chain(regrouped.iter()) {
        match by_key.get(key) {
            Some(u) => out.push(LayerUnit::Replace(u.clone())),
            // Deleted, renamed away or now excluded: masks the lower layers.
            None => out.push(LayerUnit::Delete(key.clone())),
        }
    }
    (out, groups)
}

#[derive(Clone, Debug)]
enum LayerUnit {
    Replace(PlanUnit),
    Delete(String),
}

fn new_layer(
    exec: &mut Exec<'_>,
    repository: &Id,
    layer: &str,
    source: &str,
    snap: &Snapshot,
    lower: &[Id],
) -> Result<Id, Stop> {
    let env = exec.w.env.clone();
    let stored = StoredGeneration {
        id: env.ids.next_id(IdKind::Generation),
        repository_id: repository.clone(),
        layer: layer.into(),
        state: GEN_STAGING.into(),
        source_digest: source.into(),
        watermark: 0,
        manifest: json!({
            "runId": exec.run.id.as_str(),
            "parentGenerationId": lower.first().map(Id::as_str),
            "base": snap.base_oid, "head": snap.head_oid, "captureDigest": snap.capture_digest,
            "extractorVersion": super::config::EXTRACTOR_VERSION,
        }),
    };
    let id = stored.id.clone();
    exec.fenced(WritePriority::Authoritative, move |tx, r| {
        tx.indexing().insert_generation(&stored)?;
        if let Some(o) = &r.overlay_id
            && let Some(mut ov) = tx.indexing().overlay(o)?
            && ov.run_id.as_ref() == Some(&r.id)
        {
            // Recorded on the overlay so an abandoned build's layers are retired with its run.
            let expected = ov.revision;
            ov.revision += 1;
            if stored.layer == LAYER_BRANCH {
                ov.branch_generation_id = Some(stored.id.clone());
            } else {
                ov.worktree_generation_id = Some(stored.id.clone());
            }
            tx.indexing().put_overlay(&ov, Some(expected))?;
        }
        Ok(())
    })?;
    Ok(id)
}

/// Stages one layer's units: replacements read from the commit (`expected` `None`) or from the
/// worktree (each checked against its captured digest), and deletions.
fn stage_layer(
    exec: &mut Exec<'_>,
    root: &Path,
    name: &str,
    generation: &Id,
    units: &[LayerUnit],
    expected: Option<&BTreeMap<String, String>>,
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<(), Stop> {
    let env = exec.w.env.clone();
    let staged = staged_keys(&env, generation).map_err(Stop::storage)?;
    let total = units.len() as i64;
    let mut done = staged.len() as i64;
    let mut deletes = Vec::new();
    let mut from_commit: Vec<&PlanUnit> = Vec::new();
    let mut from_worktree: Vec<&PlanUnit> = Vec::new();
    for u in units {
        match u {
            LayerUnit::Delete(k) if !staged.contains(k) => deletes.push(StoredUnit {
                generation_id: generation.clone(),
                unit_key: k.clone(),
                content_digest: "deleted".into(),
                action: UNIT_DELETE.into(),
                payload: json!({"mode": "Deleted"}),
            }),
            LayerUnit::Replace(p) if !staged.contains(&p.key) => {
                // Captured local changes come from the worktree; everything else in the layer
                // (directories, unchanged members of a changed group) from the commit.
                if expected.is_some_and(|e| e.contains_key(&p.key))
                    && p.mode != super::worker::MODE_DIRECTORY
                {
                    from_worktree.push(p);
                } else {
                    from_commit.push(p);
                }
            }
            _ => {}
        }
    }
    for chunk in deletes.chunks(500) {
        let chunk = chunk.to_vec();
        let n = chunk.len() as i64;
        exec.fenced(WritePriority::Bulk, move |tx, r| {
            for u in &chunk {
                tx.indexing().put_unit(u, &[], &[])?;
            }
            r.completed_units += n;
            Ok(())
        })?;
        done += n;
    }
    exec.stage_units(root, name, generation, &from_commit, &mut done, total, stop)?;
    // Worktree content, in bounded batches, verified against the capture.
    let mut i = 0;
    while i < from_worktree.len() {
        if exec.stopped() {
            return Err(Stop::Cancelled);
        }
        let mut rows = Vec::new();
        let mut bytes = 0usize;
        while i < from_worktree.len()
            && rows.len() < env.settings.batch_units
            && bytes < env.settings.batch_bytes
        {
            let u = from_worktree[i];
            i += 1;
            let captured = expected.and_then(|e| e.get(&u.key));
            let content = if u.mode == super::worker::MODE_STRUCTURAL {
                match env.source.read_file(root, &u.key, u.size.max(1)) {
                    Ok(b) => {
                        let d = format!("sha256:{}", sha256_hex(&b));
                        if captured != Some(&d) {
                            return Err(Stop::transient(
                                "worktree_changed",
                                format!("{} changed during capture", u.key),
                            ));
                        }
                        bytes += b.len();
                        Some(b)
                    }
                    Err(_) => {
                        return Err(Stop::transient(
                            "worktree_changed",
                            format!("{} changed during capture", u.key),
                        ));
                    }
                }
            } else {
                None
            };
            let share = exec.shares.get(&u.key).cloned();
            rows.push(build_unit(
                &env,
                &exec.run.repository_id,
                name,
                generation,
                u,
                content.as_deref(),
                share.as_ref(),
                stop,
                &mut exec.warnings,
            )?);
        }
        let n = rows.len() as i64;
        exec.fenced(WritePriority::Bulk, move |tx, r| {
            for (unit, nodes, edges) in &rows {
                tx.indexing().put_unit(unit, nodes, edges)?;
            }
            r.completed_units += n;
            Ok(())
        })?;
        done += n;
        exec.log(
            "worktree",
            Severity::Info,
            "batch staged",
            Some((done, total)),
        );
    }
    let _ = done;
    Ok(())
}

fn seal_layer(exec: &mut Exec<'_>, generation: &Id) -> Result<(), Stop> {
    let env = exec.w.env.clone();
    let (coverage, count) =
        coverage_of(&env, generation, &BTreeMap::new()).map_err(Stop::storage)?;
    let g = generation.clone();
    let now = env.now();
    exec.fenced(WritePriority::Authoritative, move |tx, _| {
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
        stored.manifest["units"] = json!(count);
        stored.manifest["indexedAtMs"] = json!(now);
        tx.indexing().update_generation(&stored)
    })
}
