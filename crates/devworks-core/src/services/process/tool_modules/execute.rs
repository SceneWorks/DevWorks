//! Executing a recorded operation: install planning, launch, supervision and observation.
use super::*;

impl ProcessToolService {
    pub(super) fn install_plan(
        &self,
        ctx: &InvocationContext,
        input: &ops::InstallProjectDependenciesInput,
    ) -> Result<InstallPlan, BindingRefusal> {
        if !matches!(input.profile, ToolProfile::Full) {
            return Err(BindingRefusal::UnsupportedProfile);
        }
        let root = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            let Some(repository) = snapshot.projects().repository(&input.repository_id)? else {
                return Ok(Err(BindingRefusal::UnknownRepository));
            };
            if repository.lifecycle == "Deleted" {
                return Ok(Err(BindingRefusal::UnknownRepository));
            }
            if repository.revision != input.expected_revision.get() {
                return Ok(Err(BindingRefusal::StaleRepository));
            }
            if !visible_to(snapshot, ctx, &repository.id)? {
                return Ok(Err(BindingRefusal::UnknownRepository));
            }
            let viewer = Viewer::resolve(snapshot.authority(), ctx)?;
            let Some(artifact) = snapshot.artifacts().artifact(&input.manifest.artifact_id)? else {
                return Ok(Err(BindingRefusal::UnknownManifest));
            };
            if artifact.state != ArtifactState::Live
                || !visible(snapshot.artifacts(), &viewer, &artifact)?
            {
                return Ok(Err(BindingRefusal::UnknownManifest));
            }
            let version = snapshot.artifacts().version(&input.manifest.version_id)?;
            let exact = version.is_some_and(|version| {
                version.live()
                    && version.artifact_id == artifact.id
                    && version.blob_digest == input.manifest.sha256.0
                    && version.byte_count == input.manifest.byte_length.get()
                    && version.media_type == input.manifest.media_type
            });
            if !exact {
                return Ok(Err(BindingRefusal::ChangedManifest));
            }
            self.repository_root(snapshot, ctx, &repository)
        })
        .map_err(|_| BindingRefusal::ChangedRoot)??;
        let root = exact_directory(&root).ok_or(BindingRefusal::ChangedRoot)?;
        const MANIFESTS: &[&str] = &[
            "Cargo.toml",
            "Cargo.lock",
            "package-lock.json",
            "package.json",
            "pnpm-lock.yaml",
            "yarn.lock",
            "pyproject.toml",
            "uv.lock",
            "requirements.txt",
            "go.mod",
            "Gemfile.lock",
            "composer.lock",
        ];
        let mut selected = None;
        for name in MANIFESTS {
            let path = root.join(name);
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.file_type().is_file()
                || metadata.len() > 8 * 1024 * 1024
                || metadata.len() != input.manifest.byte_length.get() as u64
            {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if sha256_hex(&bytes) == input.manifest.sha256.0
                && selected.replace(((*name).to_owned(), bytes)).is_some()
            {
                return Err(BindingRefusal::ChangedManifest);
            }
        }
        let (name, bytes) = selected.ok_or(BindingRefusal::ChangedManifest)?;
        let allow_scripts = input.allow_scripts == Some(true);
        if !allow_scripts && install_runs_repository_code(&name, &root) {
            return Err(BindingRefusal::ScriptsRequired);
        }
        // Every install keeps its home and caches in a host directory, one per checkout, so no
        // repository file configures the manager as user configuration. A script-free uv
        // install also runs the host's interpreter and keeps its environment there, so no
        // interpreter in the repository runs.
        let directory = self
            .bindings
            .install_environments
            .as_ref()
            .ok_or(BindingRefusal::NoInstallHome)?;
        let checkout = sha256_hex(root.to_string_lossy().as_bytes());
        let home = directory.join("home").join(&checkout);
        let host_python = match name.as_str() {
            "pyproject.toml" | "uv.lock" if !allow_scripts => {
                let (interpreter, _) = self.bindings.resolve_launch("python3")?;
                Some(HostPython {
                    interpreter,
                    environment: directory.join("uv").join(&checkout),
                })
            }
            _ => None,
        };
        let command = install_command(
            &name,
            &root,
            &home,
            &bytes,
            allow_scripts,
            host_python.as_ref(),
        )?;
        let (executable, arg0) = self.bindings.resolve_launch(command.program)?;
        let mut env = install_home_environment(&home);
        env.push(("PATH".into(), std::env::var("PATH").unwrap_or_default()));
        if command.program == "cargo" {
            let rustup_home = std::env::var_os("RUSTUP_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup"))
                });
            if let Some(home) = rustup_home.filter(|path| path.is_dir()) {
                env.push(("RUSTUP_HOME".into(), home.to_string_lossy().into_owned()));
            }
        }
        env.extend(command.env);
        if !allow_scripts {
            env.extend(script_free_environment());
        }
        Ok(InstallPlan {
            home,
            spec: NativeProcessSpec {
                program: executable,
                arg0: Some(arg0),
                args: command.args,
                cwd: Some(root),
                env,
                clear_env: true,
                capture_output: true,
                merge_output: false,
                no_child_processes: false,
                control_pipe: None,
                pipe_stdin: false,
                stdin: None,
            },
        })
    }

    /// Runs the committed effect once. An unknown observation never launches a second tree.
    pub fn execute(&self, operation: &OperationRecord) -> EffectOutcome {
        self.execute_reporting(operation, None)
    }

    /// Like [`Self::execute`], except that a watcher's process does not hold the caller (a
    /// dispatcher worker) for its lifetime: once it is launched this answers `launched`, and a
    /// monitor thread records the observed outcome on the operation as `watchOutcome`.
    pub fn execute_detached(self: &Arc<Self>, operation: &OperationRecord) -> EffectOutcome {
        let watched = operation.capability == "process.execute"
            && serde_json::from_value::<ExecuteJob>(operation.payload.clone())
                .is_ok_and(|job| job.watch.is_some());
        if !watched {
            // Named explicitly: `self` is an `Arc`, and `self.execute` would resolve to the
            // `JournaledEffect` impl for `Arc<ProcessToolService>`, which calls this method.
            return ProcessToolService::execute(self, operation);
        }
        let (report, launched) = std::sync::mpsc::channel();
        let report = LaunchReport::new(report);
        let service = self.clone();
        let monitored = operation.clone();
        self.monitoring
            .locked()
            .insert(operation.id.0.as_str().to_owned());
        std::thread::spawn(move || {
            let outcome = service.execute_reporting(&monitored, Some(&report));
            if let Some(outcome) = report.finish(outcome) {
                service.record_watch_outcome(&monitored, &outcome);
            }
            service.monitoring.locked().remove(monitored.id.0.as_str());
        });
        match launched.recv() {
            Ok(None) => {
                drop(launched);
                EffectOutcome::Completed(json!({"launched": true}))
            }
            Ok(Some(outcome)) => outcome,
            Err(_) => EffectOutcome::Unknown("watched process launch was not reported".into()),
        }
    }

    /// Whether a monitor thread of this boot still owns the watched process `operation`.
    pub fn is_monitoring(&self, operation: &OperationId) -> bool {
        self.monitoring.locked().contains(operation.0.as_str())
    }

    pub(super) fn record_watch_outcome(
        &self,
        operation: &OperationRecord,
        outcome: &EffectOutcome,
    ) {
        let evidence = match outcome {
            EffectOutcome::Completed(evidence) => evidence.clone(),
            EffectOutcome::Failed(reason) => json!({"failed": reason}),
            EffectOutcome::NotApplied(reason) => json!({"notApplied": reason}),
            EffectOutcome::Unknown(reason) => json!({"unknown": reason}),
        };
        let id = operation.id.clone();
        let _ = crate::ports::storage::run_write(
            &*self.store,
            crate::ports::storage::WriteOptions::observed(),
            move |tx| {
                let Some(current) = tx.operations().operation(&id)? else {
                    return Ok(());
                };
                let Some(attempt) = current.latest_attempt().map(|a| a.attempt_no) else {
                    return Ok(());
                };
                tx.operations()
                    .observe(&id, attempt, json!({"watchOutcome": evidence}))
            },
        );
    }

    pub(super) fn execute_reporting(
        &self,
        operation: &OperationRecord,
        launched: Option<&LaunchReport>,
    ) -> EffectOutcome {
        if operation.capability == "process.cancel" {
            return self.execute_cancel(operation);
        }
        if operation.capability == "process.install_project_dependencies" {
            return self.execute_install(operation);
        }
        if operation.capability != "process.execute"
            && operation.capability != "process.install_project_dependencies"
        {
            return EffectOutcome::Failed("unsupported process effect".into());
        }
        let Ok(job) = serde_json::from_value::<ExecuteJob>(operation.payload.clone()) else {
            return EffectOutcome::Failed("invalid process intent".into());
        };
        // A probe's run starts only while its probe still observes: one queued before the probe
        // ended (it timed out, or was stopped between runs) never launches. A one-shot process
        // watcher's process is the owner's request and launches even when only its observation
        // was cancelled (an explicit stop cancels the undispatched intent itself).
        if job.watch.is_some() {
            let watcher = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
                snapshot.watchers().watcher_for_process(&operation.id)
            });
            match watcher {
                // ... and nothing a watcher launched starts once the watcher was ended with a stop
                // (its owner ended, or a stop was requested before the launch).
                Ok(Some(w))
                    if (w.source_kind == "Probe" && (!w.phase.is_open() || w.stop_requested))
                        || (!w.phase.is_open() && w.stop_requested) =>
                {
                    return EffectOutcome::Failed(
                        "the watcher ended before this run started".into(),
                    );
                }
                Err(error) => {
                    return EffectOutcome::Unknown(format!("watcher state unavailable: {error}"));
                }
                _ => {}
            }
        }
        let Some(ctx) = job.context(&*self.store, &operation.id) else {
            return EffectOutcome::Failed("invalid process principal kind".into());
        };
        let admitted_by = if job.watch.is_some() {
            "watch.create"
        } else {
            "process.execute"
        };
        let Some(descriptor) = crate::registry::find(admitted_by) else {
            return EffectOutcome::Failed("process descriptor unavailable".into());
        };
        let Some(binding) = descriptor.binding(Mode::Command) else {
            return EffectOutcome::Failed("process command binding unavailable".into());
        };
        let Some(fingerprint) = Fingerprint::from_hex(&job.fingerprint) else {
            return EffectOutcome::Failed("invalid process intent fingerprint".into());
        };
        let input = match &job.watch {
            Some(origin) => OperationInput::Watch(crate::operations::watch::Input::Create(
                crate::operations::watch::CreateInput {
                    process: Some(job.spec.clone()),
                    probe: None,
                    timer: None,
                    file: None,
                    github: None,
                    matching: None,
                    max_lifetime_ms: None,
                    tail_bytes: origin
                        .requested_tail_bytes
                        .and_then(crate::protocol::support::ByteLimit::new),
                },
            )),
            None => OperationInput::Process(ops::Input::Execute(ops::ExecuteInput {
                spec: job.spec.clone(),
            })),
        };
        if self
            .authority
            .authorize(
                &ctx,
                &AuthorityRequest {
                    descriptor,
                    binding,
                    effect: binding.effect,
                    action: None,
                    input: &input,
                    fingerprint: &fingerprint,
                },
            )
            .is_err()
        {
            return EffectOutcome::Failed("current process authority denied dispatch".into());
        }
        let mut spec = match self.bound_spec(&job.spec, &ctx) {
            Ok(spec) => spec,
            Err(reason) => {
                return EffectOutcome::Failed(format!("process binding changed: {reason:?}"));
            }
        };
        let secret_values = match self.bindings.resolve_credentials(
            &job.spec,
            job.project.as_ref(),
            &*self.secrets,
            self.store.info().store_id.0.as_str(),
            &mut spec,
        ) {
            Ok(values) => values,
            Err(_) => return EffectOutcome::Failed("process credential unavailable".into()),
        };
        let redactions = job
            .spec
            .environment
            .0
            .iter()
            .filter_map(|entry| entry.value.as_ref().map(|value| value.0.clone()))
            .chain(secret_values)
            .collect();
        let check_root = &job.spec.working_directory.root_id;
        let directory = spec.cwd.clone();
        let git = self.git.as_ref().filter(|_| job.watch.is_none());
        let subject = || {
            git.zip(directory.as_deref())
                .and_then(|(git, cwd)| git.check_subject(&ctx, check_root, cwd))
        };
        let before = subject();
        let mut outcome = self.run_owned(
            operation,
            &ctx,
            spec,
            &job.spec.working_directory.root_id,
            job.spec.timeout_ms.get(),
            job.spec.output_limit_bytes.get() as usize,
            job.tail_bytes as usize,
            redactions,
            None,
            launched,
            job.watch.as_ref().and_then(|w| w.pattern.as_deref()),
        );
        if let (Some(before), EffectOutcome::Completed(observed)) = (before, &mut outcome)
            && subject().as_ref() == Some(&before)
        {
            observed["verifiedWorktree"] = before;
        }
        outcome
    }

    pub(super) fn execute_install(&self, operation: &OperationRecord) -> EffectOutcome {
        let Ok(job) = serde_json::from_value::<InstallJob>(operation.payload.clone()) else {
            return EffectOutcome::Failed("invalid project install intent".into());
        };
        let Some(ctx) = job.context(&*self.store, &operation.id) else {
            return EffectOutcome::Failed("invalid process principal kind".into());
        };
        let Some(descriptor) = crate::registry::find("process.install_project_dependencies") else {
            return EffectOutcome::Failed("project install descriptor unavailable".into());
        };
        let Some(binding) = descriptor.binding(Mode::Command) else {
            return EffectOutcome::Failed("project install binding unavailable".into());
        };
        let Some(fingerprint) = Fingerprint::from_hex(&job.fingerprint) else {
            return EffectOutcome::Failed("invalid project install fingerprint".into());
        };
        let input = job.input();
        let typed = OperationInput::Process(ops::Input::InstallProjectDependencies(input.clone()));
        if self
            .authority
            .authorize(
                &ctx,
                &AuthorityRequest {
                    descriptor,
                    binding,
                    effect: binding.effect,
                    action: None,
                    input: &typed,
                    fingerprint: &fingerprint,
                },
            )
            .is_err()
        {
            return EffectOutcome::Failed(
                "current project install authority denied dispatch".into(),
            );
        }
        let plan = match self.install_plan(&ctx, &input) {
            Ok(plan) => plan,
            Err(reason) => {
                return EffectOutcome::Failed(format!(
                    "project install source changed: {reason:?}"
                ));
            }
        };
        self.run_owned(
            operation,
            &ctx,
            plan.spec,
            &input.repository_id,
            3_600_000,
            1_048_576,
            0,
            Vec::new(),
            Some(&plan.home),
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_owned(
        &self,
        operation: &OperationRecord,
        ctx: &InvocationContext,
        spec: NativeProcessSpec,
        root: &Id,
        timeout_ms: i64,
        output_limit: usize,
        tail_limit: usize,
        redactions: Vec<String>,
        install_home: Option<&Path>,
        launched: Option<&LaunchReport>,
        pattern: Option<&str>,
    ) -> EffectOutcome {
        if let Some(home) = install_home {
            let Some(root) = spec.cwd.as_ref() else {
                return EffectOutcome::Failed("project install has no working root".into());
            };
            // The home and its temporary folder are the host's; only the pip install target
            // stays in the checkout.
            for directory in [home.join("tmp"), root.join(".devworks-deps")] {
                if std::fs::create_dir_all(directory).is_err() {
                    return EffectOutcome::Failed("project install workspace unavailable".into());
                }
            }
        }
        let Ok(branches) = self.protected_branches(root) else {
            return EffectOutcome::Failed("protected branches unavailable".into());
        };
        let policy = LaunchPolicy {
            fs: &*self.fs,
            roots: &self.protected_roots,
            protected_branches: &branches,
        };
        let process = match self.processes.launch_reserved(
            ctx,
            LaunchRequest {
                spec,
                operation_id: Some(operation.id.clone()),
                lease: None,
                approved_hold: false,
            },
            &policy,
            operation.id.0.clone(),
        ) {
            Ok(process) => {
                if let Some(report) = launched {
                    report.launched();
                }
                process
            }
            Err(LaunchRefusal::Storage(error)) => {
                return EffectOutcome::Unknown(format!(
                    "process launch record unavailable: {error}"
                ));
            }
            Err(reason) => {
                return EffectOutcome::Failed(format!("process launch refused: {reason:?}"));
            }
        };
        let started = Instant::now();
        let output = Arc::new(Mutex::new(BoundedOutput::new(output_limit)));
        self.live_output
            .locked()
            .insert(process.id.as_str().to_owned(), output.clone());
        let redactions = Arc::new(redactions);
        let tail = (tail_limit > 0).then(|| Arc::new(Mutex::new(TailOutput::new(tail_limit))));
        if let Some(tail) = &tail {
            self.live_tails.locked().insert(
                process.id.as_str().to_owned(),
                (tail.clone(), redactions.clone()),
            );
        }
        let _output_guard = LiveOutputGuard {
            service: self,
            id: process.id.clone(),
        };
        let Some((stdout, stderr)) = self.processes.take_output(&process) else {
            let _ = self
                .processes
                .cancel(&process, Duration::ZERO, Duration::from_secs(5));
            return EffectOutcome::Unknown("owned process output could not be captured".into());
        };
        let matches = pattern
            .and_then(|p| Some([MatchCounter::new(p)?, MatchCounter::new(p)?]))
            .map(|counters| Arc::new(Mutex::new(counters)));
        if let Some(matches) = &matches {
            self.live_matches
                .locked()
                .insert(process.id.as_str().to_owned(), matches.clone());
        }
        let streams = [0, 1].map(|stream| {
            let (output, tail, matches) = (output.clone(), tail.clone(), matches.clone());
            let finish = matches.clone();
            CaptureStream {
                on_chunk: Box::new(move |chunk| {
                    output.locked().append(chunk);
                    if let Some(tail) = &tail {
                        tail.locked().append(chunk);
                    }
                    if let Some(matches) = &matches {
                        matches.locked()[stream].feed(chunk);
                    }
                }),
                on_end: finish.map(|matches| {
                    Box::new(move || {
                        matches.locked()[stream].finish();
                    }) as Box<dyn FnOnce() + Send>
                }),
            }
        });
        let captured = self.processes.run_captured(
            &process,
            (stdout, stderr),
            streams,
            CapturePolicy {
                stop_at: Instant::now() + Duration::from_millis(timeout_ms as u64),
                term_grace: Duration::from_secs(1),
                stop_wait: CaptureWait::For(Duration::from_secs(5)),
                drain_wait: CaptureWait::For(Duration::from_secs(5)),
                drain_stop_wait: Duration::from_secs(5),
            },
        );
        let exit = match captured {
            Ok(exit) => exit,
            Err(CaptureFault::StopNotRecorded(error)) => {
                return EffectOutcome::Unknown(format!(
                    "process timeout stop was not recorded: {error}"
                ));
            }
            Err(CaptureFault::StopUnverified(reason)) => return EffectOutcome::Unknown(reason),
            Err(CaptureFault::DrainTimedOut) => {
                return EffectOutcome::Unknown("owned process output did not drain".into());
            }
        };
        let (exit_code, signal, timed_out) = (exit.exit_code, exit.signal, exit.timed_out);
        let duration_ms = exit.exited_at.duration_since(started).as_millis();
        let stored = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            snapshot.resources().process(&process.id)
        });
        let Ok(Some(stored)) = stored else {
            return EffectOutcome::Unknown("owned process record unavailable".into());
        };
        let output = output.locked();
        let bytes = redacted_output(output.retained(), &redactions, output.truncated());
        let truncated = output.truncated()
            || redactions
                .iter()
                .any(|value| !value.is_empty() && value.len() < 4);
        let record = ProcessRecord {
            id: stored.id.clone(),
            revision: rev(stored.revision),
            state: OperationState::ObservedComplete,
            owner_id: stored.actor.0.clone(),
            exit_code,
            output: None,
            truncated,
        };
        drop(output);
        let mut evidence = json!({
            "result": if install_home.is_some() {
                OperationResult::ProcessInstallProjectDependencies { value: record }
            } else {
                OperationResult::ProcessExecute { value: record }
            },
            "outputBase64": base64::engine::general_purpose::STANDARD.encode(bytes),
            "truncated": truncated,
        });
        evidence["durationMs"] = json!(u64::try_from(duration_ms).unwrap_or(u64::MAX));
        if timed_out {
            evidence["timedOut"] = json!(true);
        }
        if let Some(signal) = signal {
            evidence["signal"] = json!(signal);
        }
        // Every match in the whole output, counted as it streamed (both streams drained above).
        if let Some(matches) = &matches {
            let counters = matches.locked();
            evidence["matches"] = json!(counters[0].count() + counters[1].count());
        }
        if let Some(tail) = &tail {
            let tail = tail.locked();
            let bytes = redacted_tail(&tail.retained(), &redactions, tail.truncated(), false);
            evidence["tailBase64"] = json!(base64::engine::general_purpose::STANDARD.encode(bytes));
            evidence["tailTruncated"] = json!(tail.truncated());
        }
        EffectOutcome::Completed(evidence)
    }

    /// An earlier boot's effect is observed from its owned-process record; it is never
    /// redispatched. A verified terminal process can settle its outcome without inventing
    /// output bytes; `process.output` reports the missing body as OutcomeUnknown.
    pub fn observe(&self, operation: &OperationRecord) -> EffectOutcome {
        if operation.capability == "process.cancel" {
            return self.observe_cancel(operation);
        }
        if operation.capability != "process.execute" {
            return EffectOutcome::Failed("unsupported process effect".into());
        }
        let found = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            snapshot.resources().processes(
                &crate::ports::storage::ProcessFilter::Operation(operation.id.clone()),
                None,
                2,
            )
        });
        match found {
            Ok(ref records) if records.len() == 1 && records[0].phase == ProcessPhase::Reaped => {
                let process = &records[0];
                EffectOutcome::Completed(json!({
                    "result": if operation.capability == "process.install_project_dependencies" {
                        OperationResult::ProcessInstallProjectDependencies { value: ProcessRecord {
                            id: process.id.clone(), revision: rev(process.revision),
                            state: OperationState::ObservedComplete, owner_id: process.actor.0.clone(),
                            exit_code: process.exit_code.and_then(|code| i32::try_from(code).ok()),
                            output: None, truncated: true,
                        } }
                    } else { OperationResult::ProcessExecute { value: ProcessRecord {
                        id: process.id.clone(), revision: rev(process.revision),
                        state: OperationState::ObservedComplete, owner_id: process.actor.0.clone(),
                        exit_code: process.exit_code.and_then(|code| i32::try_from(code).ok()),
                        output: None, truncated: true,
                    } } },
                    "outputLost": true,
                    "truncated": true,
                }))
            }
            Ok(ref records) if records.len() == 1 && records[0].phase == ProcessPhase::Failed => {
                EffectOutcome::Failed("owned process failed before a verified result".into())
            }
            Ok(_) => EffectOutcome::Unknown("the owned process still needs reconciliation".into()),
            Err(error) => {
                EffectOutcome::Unknown(format!("process observation unavailable: {error}"))
            }
        }
    }
}
