//! One run's execution: Git access, batched reads, parsing, checkpoints and publication.
use super::*;

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
    /// Each group member's share of the repository-level group extraction of the layer being
    /// staged (see [`group_members`]).
    pub shares: BTreeMap<String, FileExtract>,
    pub semantic_packs: Vec<Arc<dyn crate::ports::indexing::SemanticEnricher>>,
    pub repository_semantics: Vec<semantic::RepositoryResult>,
}

impl Exec<'_> {
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst) || self.w.stopping()
    }
    pub fn git<'g>(&'g self, stop: &'g (dyn Fn() -> bool + Sync)) -> Git<'g> {
        git_for(&self.w.env, &self.ctx, stop)
    }

    /// Extracts each group of `groups` from its members' bytes and records every member's share
    /// in `self.shares`. A member is read from the worktree when `local` names it (and checked
    /// against its captured digest), else as the committed blob its digest names.
    pub(crate) fn group_shares(
        &mut self,
        dir: &Path,
        repository: &str,
        groups: &BTreeMap<&'static str, Vec<&PlanUnit>>,
        local: Option<&BTreeMap<String, String>>,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), Stop> {
        let env = self.w.env.clone();
        for (group, members) in groups {
            if self.stopped() {
                return Err(Stop::Cancelled);
            }
            let total: usize = members.iter().map(|u| u.size as usize).sum();
            let failed = |why: String| -> BTreeMap<String, FileExtract> {
                members
                    .iter()
                    .map(|u| {
                        (
                            u.key.clone(),
                            FileExtract {
                                incomplete: vec![format!("{group}: {why}")],
                                ..FileExtract::default()
                            },
                        )
                    })
                    .collect()
            };
            let shares = if total > MAX_GROUP_BYTES {
                failed(format!(
                    "the group's {} files exceed {MAX_GROUP_BYTES} bytes",
                    members.len()
                ))
            } else {
                let mut files: Vec<(String, Vec<u8>)> = Vec::with_capacity(members.len());
                let committed: Vec<&&PlanUnit> = members
                    .iter()
                    .filter(|u| !local.is_some_and(|l| l.contains_key(&u.key)))
                    .collect();
                let oids: Vec<String> = committed.iter().map(|u| u.digest.clone()).collect();
                let blobs = if oids.is_empty() {
                    Vec::new()
                } else {
                    self.git(stop).blobs(dir, &oids, total).map_err(Stop::git)?
                };
                let mut by_key: BTreeMap<&str, Vec<u8>> = committed
                    .iter()
                    .map(|u| u.key.as_str())
                    .zip(blobs)
                    .collect();
                for u in members {
                    let bytes = match local.and_then(|l| l.get(&u.key)) {
                        Some(captured) => {
                            let b =
                                env.source
                                    .read_file(dir, &u.key, u.size.max(1))
                                    .map_err(|_| {
                                        Stop::transient(
                                            "worktree_changed",
                                            format!("{} changed during capture", u.key),
                                        )
                                    })?;
                            if &format!("sha256:{}", sha256_hex(&b)) != captured {
                                return Err(Stop::transient(
                                    "worktree_changed",
                                    format!("{} changed during capture", u.key),
                                ));
                            }
                            b
                        }
                        None => by_key.remove(u.key.as_str()).unwrap_or_default(),
                    };
                    files.push((u.key.clone(), bytes));
                }
                match env.parser.extract_group(group, repository, &files, stop) {
                    Ok(mut map) => members
                        .iter()
                        .map(|u| (u.key.clone(), map.remove(&u.key).unwrap_or_default()))
                        .collect(),
                    Err(ExtractFault::Cancelled) => return Err(Stop::Cancelled),
                    Err(ExtractFault::TimedOut) => failed("the extraction budget passed".into()),
                    Err(ExtractFault::Failed(why)) => failed(why),
                }
            };
            self.shares.extend(shares);
        }
        Ok(())
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
    pub(super) fn log_finish(
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
                ..WriteOptions::host_state()
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
    pub(super) fn finish(&mut self, outcome: Result<(), Stop>) {
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
        let written = run_write(&*self.w.env.store, WriteOptions::settlement(), move |tx| {
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
                super::super::overlay::settle_overlay(
                    tx,
                    o,
                    &cur.id,
                    overlay_state,
                    Some(&reason),
                )?;
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

    pub(super) fn baseline(&mut self) -> Result<(), Stop> {
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
        let observed = git.head_observation(&dir).map_err(|error| match error {
            GitFault::Unborn => Stop::det("no_commit", "the repository has no commit to index"),
            other => Stop::git(other),
        })?;
        let head = observed.head;
        let head_ref = observed.branch;
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
                    .map(|root| super::super::discover::groups(root, &s.location))
            })
            .unwrap_or_default();
        if cfg.repository_excluded(&repo.name, &group) {
            return Err(Stop::det(
                "repository_excluded",
                "an exclusion now names this repository",
            ));
        }
        let registry_binding = read_snapshot(&*env.store, &ReadOptions::default(), |read| {
            let projects = read.projects().repository_projects(&repo_id)?;
            semantic::registry_binding(&env, read.packages(), &cfg, &projects)
        })
        .map_err(Stop::storage)?;
        self.semantic_packs = semantic::select(&env, &cfg, &repo_id, &registry_binding)?;
        let semantic_binding = semantic::binding(&self.semantic_packs);
        let pointer = previous.as_ref().map_or(0, |(_, r)| *r);
        let parent = previous
            .as_ref()
            .map(|(g, _)| g.clone())
            .filter(|g| g.state == GEN_SEALED);
        // Rename provenance between the published commit and this one.
        let mut renamed: BTreeMap<String, String> = BTreeMap::new();
        if let Some(p) = &parent
            && p.source_digest != head
            && crate::services::git::client::oid_ok(&p.source_digest)
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
                && p.manifest
                    .get("semanticBinding")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    == semantic_binding
                && p.manifest["semanticRegistryBinding"].as_str().unwrap_or("") == registry_binding
                && p.manifest.get("extractorVersion").and_then(Value::as_i64)
                    == Some(EXTRACTOR_VERSION)
        });
        // Identical immutable inputs reuse all rows, including any recorded coverage gap.
        // An explicit reindex bypasses reuse after a missing runtime or cache is repaired.
        let reuse_repository_semantics =
            reusable.is_some_and(|parent| parent.source_digest == head);
        let repository_semantics = self
            .semantic_packs
            .iter()
            .any(|pack| pack.descriptor().repository_source);
        let total = units.len() as i64;
        let plan_digest = digest(&[
            &head,
            &cfg.digest,
            &parent
                .as_ref()
                .map_or(String::new(), |p| p.id.as_str().to_owned()),
            &pointer.to_string(),
            &self.run.kind,
            &semantic_binding,
            &registry_binding,
        ]);
        // The staging generation: the checkpoint of an earlier attempt when its plan still holds.
        let existing = match &self.run.generation_id {
            Some(g) if !repository_semantics => {
                read_snapshot(&*env.store, &ReadOptions::default(), |s| {
                    s.indexing().generation(g)
                })
                .map_err(Stop::storage)?
                .filter(|g| {
                    g.state == GEN_STAGING
                        && g.manifest.get("planDigest").and_then(Value::as_str)
                            == Some(plan_digest.as_str())
                })
            }
            _ => None,
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
                        "semanticBinding": semantic_binding,
                        "semanticRegistryBinding": registry_binding,
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
        // Repository-level groups (Cargo, SwiftPM): a group with any changed member is extracted
        // again as a whole.
        let groups = group_members(&env, &cfg, &units);
        let regroup: BTreeSet<&str> = groups
            .iter()
            .filter(|(g, m)| parent_units.is_empty() || group_changed(&env, g, m, &parent_units))
            .flat_map(|(_, m)| m.iter().map(|u| u.key.as_str()))
            .collect();
        let mut inherit: Vec<String> = Vec::new();
        let mut extract: Vec<&PlanUnit> = Vec::new();
        for u in &units {
            if staged.contains(&u.key) {
                continue;
            }
            let same = parent_units
                .get(&u.key)
                .is_some_and(|(d, mode, lang, captured)| {
                    d == &u.digest
                        && mode == u.mode
                        && lang.as_deref() == u.language
                        && (env.artifacts.is_none() || u.oid.is_none() || *captured)
                });
            let rebuild_repository_semantics =
                !reuse_repository_semantics && semantic::repository_unit(&self.semantic_packs, u);
            if same
                && u.previous.is_none()
                && !regroup.contains(u.key.as_str())
                && !rebuild_repository_semantics
            {
                inherit.push(u.key.clone());
            } else {
                extract.push(u);
            }
        }
        let pending: BTreeSet<&str> = extract.iter().map(|u| u.key.as_str()).collect();
        let needed: BTreeMap<&'static str, Vec<&PlanUnit>> = groups
            .into_iter()
            .filter(|(_, m)| m.iter().any(|u| pending.contains(u.key.as_str())))
            .collect();
        self.group_shares(&dir, &repo.name, &needed, None, &stop)?;
        if extract
            .iter()
            .any(|unit| semantic::repository_unit(&self.semantic_packs, unit))
        {
            semantic::prepare_repository(
                self,
                semantic::SemanticBatch {
                    repository_name: &repo.name,
                    root: &dir,
                    commit: &head,
                    capture: None,
                    files: &[],
                },
                &units,
                None,
                &stop,
            )?;
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
            stored.manifest["packs"] = coverage["semanticPacks"].clone();
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
        let published = self.publish_baseline(&generation, &head, head_ref, pointer, texts);
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
        let packs = self.semantic_packs.clone();
        let commit = self.run.source_oid.clone().unwrap_or_default();
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
            let mut sources =
                super::super::source::Sources::new(env.clone(), self.run.repository_id.clone());
            for u in &batch {
                let c = u.oid.as_deref().and_then(|o| content.get(o).copied());
                let share = self.shares.get(&u.key).cloned();
                let mut row = build_unit(
                    &env,
                    &self.run.repository_id,
                    name,
                    generation,
                    u,
                    c,
                    share.as_ref(),
                    stop,
                    &mut self.warnings,
                )?;
                let capture = matches!(
                    u.mode,
                    MODE_STRUCTURAL | MODE_TEXT | MODE_TEXT_FALLBACK | MODE_NOT_SELECTED
                );
                sources.stage(&mut row.0, c.filter(|_| capture))?;
                rows.push(row);
            }
            if !packs.is_empty() {
                let files: Vec<_> = batch
                    .iter()
                    .filter_map(|unit| {
                        let language = unit.language?;
                        let bytes = content.get(unit.oid.as_deref()?)?;
                        matches!(unit.mode, MODE_STRUCTURAL | MODE_TEXT_FALLBACK).then_some(
                            crate::ports::indexing::EnrichmentSource {
                                unit: &unit.key,
                                language,
                                bytes,
                            },
                        )
                    })
                    .collect();
                semantic::enrich_batch(
                    self,
                    &packs,
                    semantic::SemanticBatch {
                        repository_name: name,
                        root: dir,
                        commit: &commit,
                        capture: None,
                        files: &files,
                    },
                    &mut rows,
                    stop,
                )?;
            }
            let n = rows.len() as i64;
            let staged = sources.records();
            let capture_env = env.clone();
            let repository = self.run.repository_id.clone();
            self.fenced(WritePriority::Bulk, move |tx, r| {
                for (unit, nodes, edges) in &rows {
                    super::super::source::Sources::commit(
                        &capture_env,
                        &repository,
                        &staged,
                        tx,
                        unit,
                    )?;
                    tx.indexing().put_unit(unit, nodes, edges)?;
                }
                r.completed_units += n;
                Ok(())
            })?;
            sources.committed();
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
    pub(super) fn publish_baseline(
        &mut self,
        generation: &Id,
        head: &str,
        head_ref: Option<String>,
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
                return Ok(Err(Stop::transient(
                    "pointer_moved",
                    format!("the published view moved (revision {current}) while this generation was built"),
                )));
            }
            let stored = tx
                .indexing()
                .generation(&g)?
                .filter(|x| x.state == GEN_SEALED)
                .ok_or(StorageError::NotFound { kind: "index_generation", id: g.as_str().to_owned() })?;
            let config = super::super::effective_config_tx(tx, &repo)?;
            let projects = tx.projects().repository_projects(&repo)?;
            if stored.manifest["configDigest"].as_str() != Some(config.digest.as_str())
                || stored.manifest["semanticRegistryBinding"].as_str().unwrap_or("")
                    != semantic::registry_binding(&env, tx.packages(), &config, &projects)?
            {
                return Ok(Err(Stop::transient(
                    "index_configuration_changed",
                    "the index configuration or semantic package changed during the build",
                )));
            }
            let (state, gaps) = super::super::status::manifest_coverage(&stored.manifest);
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
                    "headRef": head_ref, "sourceRevision": pointer + 1,
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
            let published_event = super::super::history_event(
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
            if env.settings.analysis_events {
                let delivery = tx.new_id(IdKind::QueueEntry);
                tx.queue().enqueue(NewQueueEntry {
                    id: delivery,
                    consumer: crate::services::graph::repository_analysis::INDEX_CONSUMER.into(),
                    event_id: published_event, root_id: None, operation_id: None,
                    partition: None, priority: 0, ready_at_ms: now,
                    payload: json!({"repositoryId": repo, "viewId": view_id}),
                })?;
            }
            Ok(Ok(()))
        });
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(stop)) => Err(stop),
            // A lease renewal inside the fenced write: the run is no longer ours.
            Err(Stop::Lost) => Err(Stop::Lost),
            Err(s) => Err(s),
        }
    }
}
