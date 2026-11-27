//! Scheduled inference jobs through the native pipeline (DW-172): a job run is an agent turn of
//! the job's own principal, routed through the router to a scripted provider transport, its tool
//! calls submitted through the gateway under that principal, its transcript, usage and outcome
//! recorded. No model inference: every provider response is scripted.
use super::*;
use devworks_core::protocol::provider::TranscriptBody;

struct JobEntropy;
impl devworks_core::ports::http::Entropy for JobEntropy {
    fn fill(&self, bytes: &mut [u8]) {
        bytes.fill(13);
    }
}

/// The real host authority over the one store: tool calls are admitted as the desktop admits
/// them.
struct JobAuthority {
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn devworks_core::ports::Clock>,
}
impl devworks_core::ports::AuthorityPort for JobAuthority {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &devworks_core::ports::AuthorityRequest<'_>,
    ) -> Result<devworks_core::ports::Authorization, devworks_core::ports::AuthorityDenial> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.clock,
            protected_branches: &[],
        }
        .authorize(ctx, request)
    }
}

/// How the scripted transport answers its `n`th request.
enum Answer {
    Text(&'static str),
    Tool {
        name: &'static str,
        arguments: &'static str,
    },
    /// Holds the stream open until the request's cancellation (or 20 s).
    Block,
    /// Waits to connect until the request's cancellation (or 20 s): the request is never sent.
    BlockUnsent,
}

struct JobWire {
    requests: Mutex<Vec<Value>>,
    script: Vec<Answer>,
}
impl JobWire {
    fn new(script: Vec<Answer>) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            script,
        })
    }
    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

struct BlockedStream {
    head: ResponseHead,
}

fn cancelled(phase: devworks_core::ports::provider_http::SendPhase) -> TransportError {
    TransportError {
        phase,
        kind: devworks_core::ports::provider_http::TransportFailure::Cancelled,
    }
}

fn wait_cancelled(cancel: &Cancellation) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !cancel.is_cancelled() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}
impl HttpStream for BlockedStream {
    fn closer(&self) -> Arc<dyn StreamCloser> {
        Arc::new(AgentClose)
    }
    fn head(&self) -> &ResponseHead {
        &self.head
    }
    fn read(&mut self, _: &mut [u8], cancel: &Cancellation) -> Result<usize, TransportError> {
        wait_cancelled(cancel);
        Err(cancelled(
            devworks_core::ports::provider_http::SendPhase::AfterSend,
        ))
    }
    fn close(&mut self) -> Result<(), TransportError> {
        Ok(())
    }
}

impl StreamingHttp for JobWire {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        cancel: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        let payload: Value = serde_json::from_slice(request.body.as_ref().unwrap()).unwrap();
        let index = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(payload);
            requests.len() - 1
        };
        let head = ResponseHead {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
        };
        let (delta, finish) = match self.script.get(index).unwrap_or(&Answer::Text("extra")) {
            Answer::Text(text) => (json!({"content": text}), "stop"),
            Answer::Tool { name, arguments } => (
                json!({"tool_calls":[{"index":0,"id":"job-call-1","type":"function",
                    "function":{"name":name,"arguments":arguments}}]}),
                "tool_calls",
            ),
            Answer::Block => return Ok(Box::new(BlockedStream { head })),
            Answer::BlockUnsent => {
                wait_cancelled(cancel);
                return Err(cancelled(
                    devworks_core::ports::provider_http::SendPhase::NotSent,
                ));
            }
        };
        let frame = json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
        Ok(Box::new(AgentStream {
            head,
            body: Some(format!("data: {frame}\n\ndata: [DONE]\n\n").into_bytes()),
        }))
    }
}

/// A runtime over the real host authority with provider connections, the router and `wire` as
/// the provider transport (a test seeds a local route, or none).
fn job_ports(f: &Fx, wire: Arc<dyn StreamingHttp>) -> Composition {
    let mut ports = composition_with(
        f,
        None,
        Arc::new(FakePower::default()),
        Arc::new(JobAuthority {
            store: f.store.clone(),
            clock: f.h.clock(),
        }),
        Arc::new(FakeHost::default()),
    );
    ports.application = devworks_engine::ApplicationComposition {
        connections: Some(devworks_engine::ConnectionPorts {
            http: Arc::new(devworks_core::ports::http::NoNetwork),
            secrets: Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
            entropy: Arc::new(JobEntropy),
            endpoints: None,
        }),
        router: Some(Default::default()),
        ..Default::default()
    };
    ports.config.providers =
        Some(devworks_engine::providers::driver::ProviderComposition::new(wire));
    ports
}

const PROMPT: &str = "File an issue for the failing nightly check, then report done.";

/// Saves `PROMPT` as a live project artifact owned by the user (its blob written where the
/// artifact store keeps it) and returns its reference; gives the project a Ready status.
fn seed_job_prompt(f: &Fx) -> Value {
    let digest = devworks_core::fingerprint::sha256_hex(PROMPT.as_bytes());
    let relative = format!("artifacts/sha256/{}/{digest}.fixture", &digest[..2]);
    let path = f.h.root.join(&relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, PROMPT).unwrap();
    let blob = digest.clone();
    f.tx(move |_, tx| {
        let project = ProjectId(id("project-a"));
        tx.work()
            .insert_status(&devworks_core::ports::storage::StoredStatus {
                id: id("job-ready"),
                project_id: project.clone(),
                name: "Ready".into(),
                semantic: "Ready".into(),
                sort_order: 1,
                revision: 1,
            })?;
        tx.artifacts()
            .put_blob(&blob, PROMPT.len() as i64, "fixture", &relative)?;
        tx.artifacts().insert_artifact(&StoredArtifact {
            id: id("job-prompt"),
            origin_project_id: Some(project.clone()),
            origin_work_id: None,
            title: "Job prompt".into(),
            revision: 1,
            scope: ArtifactScope::Project(project),
            owner: Some(ActorId(id("user-a"))),
            retention: ArtifactRetention::Explicit,
            scope_revision: 1,
            reference_revision: 1,
            current_version_id: None,
            state: ArtifactState::Live,
            created_at_ms: T0,
            provenance: json!({}),
        })?;
        tx.artifacts().insert_version(&StoredVersion {
            id: id("job-prompt-v1"),
            artifact_id: id("job-prompt"),
            version_no: 1,
            blob_digest: blob,
            byte_count: PROMPT.len() as i64,
            blob_generation: "fixture".into(),
            media_type: "text/markdown".into(),
            created_at_ms: T0,
            deleted_at_ms: None,
            source: json!({}),
        })
    });
    json!({"artifactId":"job-prompt","versionId":"job-prompt-v1","sha256":digest,
        "byteLength":PROMPT.len().to_string(),"mediaType":"text/markdown"})
}

fn job_command(
    f: &Fx,
    app: &devworks_engine::Runtime,
    op: &str,
    key: &str,
    revision: u32,
    mut body: Value,
) -> Value {
    body["kind"] = json!(op);
    let frame = json!({"mode":"Command","protocol":PROTOCOL,"requestKey":key,
        "schemaRevision":revision,"body":body});
    applied(
        &app.command(&f.user, op, frame.to_string().as_bytes())
            .unwrap(),
    )
}

/// Creates a manual project job over the saved prompt and runs it now; returns the run.
fn run_job(f: &Fx, app: &devworks_engine::Runtime, prompt: Value, route: Value) -> Id {
    let definition = json!({"kind":"Inference","prompt":prompt,"route":route,"effort":"Default"});
    let job = job_command(
        f,
        app,
        "jobs.create",
        "job-create",
        3,
        json!({"scope":{"kind":"Project","projectId":"project-a"},"title":"Job scan",
            "schedule":{"kind":"Manual"},"definition":definition,"enabled":true}),
    );
    let run = job_command(
        f,
        app,
        "jobs.run_now",
        "job-run-now",
        2,
        json!({"jobId":job["id"],"expectedRevision":"1"}),
    );
    id(run["id"].as_str().unwrap())
}

fn job_run(f: &Fx, run: &Id) -> StoredJobRun {
    let run = run.clone();
    read(&*f.store, move |r| r.jobs().job_run(&run)).unwrap()
}

/// Ticks the scheduler until `run` satisfies `done` (a command already woke its loop).
async fn wait_run(
    f: &Fx,
    app: &devworks_engine::Runtime,
    run: &Id,
    done: impl Fn(&StoredJobRun) -> bool,
) -> StoredJobRun {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let current = job_run(f, run);
        if done(&current) {
            return current;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the scheduled run stalled: {current:?}"
        );
        let _ = app.scheduler().tick().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn terminal(run: &StoredJobRun) -> bool {
    !matches!(run.status.as_str(), "Queued" | "Running" | "Holding")
}

#[test]
fn a_scheduled_inference_run_completes_as_its_agent_with_a_tool_call_through_the_gateway() {
    let _native_case = native_runtime_case();
    let f = Fx::new();
    seed_local_route(&f);
    let prompt = seed_job_prompt(&f);
    let wire = JobWire::new(vec![
        Answer::Tool {
            name: "issues_create_command",
            arguments: r#"{"projectId":"project-a","title":"Nightly check fails","description":"Found by the scheduled scan.","repositoryIds":[],"statusId":"job-ready","priority":1}"#,
        },
        Answer::Text("Scheduled work done"),
    ]);
    runtime().block_on(async {
        let app = boot(job_ports(&f, wire.clone())).await.unwrap();
        let run = run_job(
            &f,
            &app,
            prompt,
            json!({"kind":"Model","modelId":"watch-model"}),
        );
        let finished = within(
            "a scheduled inference run",
            wait_run(&f, &app, &run, terminal),
        )
        .await;
        let requests = wire.requests();
        assert_eq!(finished.status, "Succeeded", "{finished:?}; {requests:?}");
        let agent = finished.agent_actor.clone().expect("the run's agent");
        let session = finished
            .session_id
            .clone()
            .expect("the run's linked session");
        // The saved prompt is the first user content the provider saw.
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert!(requests[0].to_string().contains(PROMPT), "{}", requests[0]);
        let offered: Vec<String> = requests[0]["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| t["function"]["name"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        // JOB-07: the coordinator's configured tools, including next work and starting runs.
        for tool in [
            "issues_create_command",
            "next_work_claim_project_queue_command",
            "runs_start_command",
        ] {
            assert!(
                offered.iter().any(|name| name == tool),
                "{tool}: {offered:?}"
            );
        }
        // The transcript: the prompt notice, the tool turn, its result and the final answer.
        let (stored, entries) = {
            let session = session.clone();
            read(&*f.store, move |r| {
                Ok((
                    r.sessions().session(&session)?.unwrap(),
                    r.sessions().entries(&session, 0, 50)?,
                ))
            })
        };
        assert_eq!(stored.agent_id, agent);
        assert_eq!(stored.project_id, Some(ProjectId(id("project-a"))));
        assert!(stored.root_id.is_none());
        let kinds: Vec<&str> = entries.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds.first(), Some(&"HostNotice"), "{kinds:?}");
        assert!(kinds.contains(&"ToolCallRecorded"), "{kinds:?}");
        assert!(kinds.contains(&"ToolResponseRecorded"), "{kinds:?}");
        assert!(
            entries
                .iter()
                .any(|e| e.kind == "AssistantAttempt"
                    && e.body["visibleText"] == "Scheduled work done"),
            "{entries:?}"
        );
        // Usage: every inference attempt is the agent's, in the run's session, for the run.
        let mut attempts: Vec<Id> = entries
            .iter()
            .filter_map(|e| {
                match serde_json::from_value::<TranscriptBody>(e.body.clone()).ok()? {
                    TranscriptBody::AssistantAttempt { attempt_id, .. } => Some(attempt_id.0),
                    TranscriptBody::ToolCallRecorded { tool_call } => {
                        Some(tool_call.call.attempt_id.0)
                    }
                    _ => None,
                }
            })
            .collect();
        attempts.dedup();
        assert_eq!(attempts.len(), 2, "{entries:?}");
        for attempt in attempts {
            let usage = read(&*f.store, move |r| r.usage().inference_attempt(&attempt))
                .expect("a recorded inference attempt");
            assert_eq!(usage.actor_id, agent);
            assert_eq!(usage.session_id.as_ref(), Some(&session));
            assert_eq!(usage.job_run_id.as_ref(), Some(&run));
            assert_eq!(usage.project_id, Some(ProjectId(id("project-a"))));
        }
        // The tool call was admitted through the gateway as the agent and its result recorded.
        let response = entries
            .iter()
            .find(|e| e.kind == "ToolResponseRecorded")
            .unwrap();
        assert_eq!(response.actor, agent, "{response:?}");
        assert_eq!(
            response.body["outcome"]["kind"], "Completed",
            "{response:?}"
        );
        let history = read(&*f.store, |r| {
            Ok(r.history().page(&EventFilter::All, 0, 500)?.events)
        });
        // The gateway decided the command as the agent, and its effect is the agent's.
        assert!(
            history.iter().any(|e| e.actor == agent
                && e.kind == "CommandDecided"
                && e.payload["operation"] == "issues.create"
                && e.payload["decision"] == "Applied"),
            "the agent's command decision is in history: {history:?}"
        );
        let created = history
            .iter()
            .find(|e| e.actor == agent && e.kind == "WorkCreated")
            .unwrap_or_else(|| panic!("the agent's issue is in history: {history:?}"));
        assert_eq!(created.payload["title"], "Nightly check fails");
        assert_eq!(created.payload["principalKind"], "Agent");
        assert_eq!(created.project_id, Some(ProjectId(id("project-a"))));
        let work = id(created.payload["workId"].as_str().unwrap());
        assert!(read(&*f.store, move |r| r.work().work(&work)).is_some());
        app.shutdown().await;
    });
}

#[test]
fn cancelling_a_scheduled_inference_run_stops_its_agent_turn() {
    let _native_case = native_runtime_case();
    let f = Fx::new();
    seed_local_route(&f);
    let prompt = seed_job_prompt(&f);
    let wire = JobWire::new(vec![Answer::Block]);
    runtime().block_on(async {
        let app = boot(job_ports(&f, wire.clone())).await.unwrap();
        let run = run_job(
            &f,
            &app,
            prompt,
            json!({"kind":"Model","modelId":"watch-model"}),
        );
        within("the provider request is sent", async {
            while wire.requests().is_empty() {
                let _ = app.scheduler().tick().await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        let current = job_run(&f, &run);
        let frame = json!({"mode":"Command","protocol":PROTOCOL,"requestKey":"job-cancel",
            "schemaRevision":2,"body":{"kind":"jobs.cancel","jobId":current.job_id,"runId":run,
            "expectedRevision":current.revision.to_string()}});
        // A dispatched run's cancellation is accepted; the scheduler stops its agent.
        assert!(matches!(
            decision(
                &app.command(&f.user, "jobs.cancel", frame.to_string().as_bytes())
                    .unwrap()
            ),
            DomainDecision::Accepted { .. }
        ));
        let finished = within(
            "a cancelled scheduled run",
            wait_run(&f, &app, &run, terminal),
        )
        .await;
        assert_eq!(finished.status, "Cancelled", "{finished:?}");
        assert_eq!(wire.requests().len(), 1);
        // The agent's turn stopped: its one provider attempt settled, nothing was left open.
        let session = finished.session_id.clone().expect("the run's session");
        let settled = read(&*f.store, move |r| {
            Ok(r.history()
                .page(&EventFilter::All, 0, 500)?
                .events
                .into_iter()
                .filter(|e| e.kind == "ProviderResultRecorded")
                .map(|e| e.payload["outcome"].clone())
                .collect::<Vec<_>>())
        });
        assert_eq!(settled.len(), 1, "{session:?}: {settled:?}");
        let report = app.shutdown().await;
        assert!(report.unclosed_inference_attempts.is_empty(), "{report:?}");
    });
}

#[test]
fn a_scheduled_inference_run_without_a_route_fails_with_the_routers_reason() {
    let _native_case = native_runtime_case();
    let f = Fx::new();
    let prompt = seed_job_prompt(&f);
    let wire = JobWire::new(vec![Answer::Text("never")]);
    // No connection or model is configured: the saved tier has nothing to route to.
    runtime().block_on(async {
        let app = boot(job_ports(&f, wire.clone())).await.unwrap();
        let run = run_job(&f, &app, prompt, json!({"kind":"Tier","tier":"High"}));
        let finished = within(
            "an unroutable scheduled run",
            wait_run(&f, &app, &run, terminal),
        )
        .await;
        assert_eq!(finished.status, "Failed", "{finished:?}");
        let reason = finished.reason.clone().unwrap();
        assert!(reason.contains("agent route"), "{reason}");
        assert!(reason.contains("Nothing was sent to a model"), "{reason}");
        assert!(
            !reason.contains(devworks_core::services::jobs::runner::NO_PIPELINE),
            "{reason}"
        );
        assert!(wire.requests().is_empty());
        app.shutdown().await;
    });
}

#[test]
fn a_scheduled_inference_run_interrupted_by_shutdown_resumes_its_session_after_restart() {
    let _native_case = native_runtime_case();
    let f = Fx::new();
    seed_local_route(&f);
    let prompt = seed_job_prompt(&f);
    let rt = runtime();
    let first = JobWire::new(vec![Answer::BlockUnsent]);
    let (run, session) = rt.block_on(async {
        let app = boot(job_ports(&f, first.clone())).await.unwrap();
        let run = run_job(
            &f,
            &app,
            prompt,
            json!({"kind":"Model","modelId":"watch-model"}),
        );
        within("the first provider request is opening", async {
            while first.requests().is_empty() {
                let _ = app.scheduler().tick().await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        // Shutdown pauses the agent's turn; the run is recorded interrupted, not ended.
        app.shutdown().await;
        let paused = job_run(&f, &run);
        assert!(!terminal(&paused), "{paused:?}");
        (run, paused.session_id.expect("the run's session"))
    });
    drop(rt);
    let (h, old_store) = f.into_parts();
    drop(old_store);
    h.wait_released();
    let store = h.open();
    let f = Fx::over(h, store);
    let second = JobWire::new(vec![Answer::Text("Resumed and done")]);
    runtime().block_on(async {
        let app = boot(job_ports(&f, second.clone())).await.unwrap();
        let finished = within(
            "the resumed scheduled run",
            wait_run(&f, &app, &run, terminal),
        )
        .await;
        assert_eq!(finished.status, "Succeeded", "{finished:?}");
        assert_eq!(finished.resumptions, 1, "{finished:?}");
        assert_eq!(finished.session_id.as_ref(), Some(&session));
        let entries = {
            let session = session.clone();
            read(&*f.store, move |r| r.sessions().entries(&session, 0, 50))
        };
        // The one prompt notice, the interruption reconciled, then the resumed answer.
        assert_eq!(
            entries.iter().filter(|e| e.kind == "HostNotice").count(),
            1,
            "{entries:?}"
        );
        assert!(
            entries.iter().any(
                |e| e.kind == "AssistantAttempt" && e.body["visibleText"] == "Resumed and done"
            ),
            "{entries:?}"
        );
        assert_eq!(second.requests().len(), 1);
        assert!(second.requests()[0].to_string().contains(PROMPT));
        app.shutdown().await;
    });
}
