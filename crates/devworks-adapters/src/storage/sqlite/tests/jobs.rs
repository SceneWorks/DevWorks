//! Durable schedules and manual jobs end to end on the real SQLite store (DW-50): `jobs.*`
//! through the gateway with host authority and receipts, both schema revisions, the scheduler's
//! transactional steps (claims with missed and overlapping occurrences, DST gaps and folds,
//! native dispatch through the gateway, scheduled inference up to its dispatch port,
//! cancellation, interruption and resumption across restart), project archive and restore,
//! discovery checkpoints and action identities, per-run usage, and migration 17. The clock is the
//! harness's deterministic clock; nothing sleeps on a schedule and no inference runs.
use super::{Harness, T0};
use crate::storage::sqlite::{MIGRATION_0017_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::authority::HostAuthority;
use devworks_core::authority::service as auth;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::gateway::{AdmittedCommand, CommandOutcome, Gateway};
use devworks_core::ports::storage::*;
use devworks_core::ports::{CommandHandler, Decided, HandlerFailure, IdKind};
use devworks_core::protocol::records::ProjectRecord;
use devworks_core::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, OperationResult,
};
use devworks_core::protocol::{
    ActorId, CommandReply, Counter, DomainErrorCode, DomainFault, Id, OperationId, OperationState,
    ProjectId, Revision, RootId,
};
use devworks_core::registry::SchemaRevision;
use devworks_core::services::UseCases;
use devworks_core::services::jobs::discovery::{self, ActionClaim, ActionOutcome};
use devworks_core::services::jobs::runner::{
    InferenceJobs, InferenceOutcome, InferenceRefusal, InferenceStart, JobLifecycle,
    NativeJobDispatch, NoInferencePipeline, Step, StopVerdict,
};
use devworks_core::services::jobs::{JobConfig, JobService, NativeCatalog, argument_schema};
use devworks_core::services::projects::{ProjectService, RepositoryService};
use devworks_core::services::runs::ControlService;
use devworks_core::services::runs::recovery::LifecycleHooks;
use devworks_core::settings::catalog::AgentRole;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const DOCS: &str = "help.generate_capability_docs";
const MINUTE: i64 = 60_000;

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn actor(s: &str) -> ActorId {
    ActorId(id(s))
}
fn ms(s: &str) -> i64 {
    s.parse::<jiff::Timestamp>().unwrap().as_millisecond()
}

/// How the scripted native capability answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Docs {
    Applied,
    Refused,
    Accepted,
}
/// A scripted `help.generate_capability_docs` use case standing in for its owning item.
struct DocsHandler {
    mode: Mutex<Docs>,
    calls: AtomicUsize,
}
impl CommandHandler for DocsHandler {
    fn decide(
        &self,
        _: &InvocationContext,
        _: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mode = *self.mode.lock().unwrap();
        let op = OperationId(tx.new_id(IdKind::Operation));
        Ok(match mode {
            Docs::Applied => Decided {
                decision: DomainDecision::Applied {
                    result: OperationResult::HelpGenerateCapabilityDocs {
                        value: devworks_core::protocol::records::OperationRef {
                            operation_id: op,
                            state: OperationState::ObservedComplete,
                        },
                    },
                },
                operation_id: None,
            },
            Docs::Refused => Decided {
                decision: DomainDecision::Refused {
                    fault: devworks_core::services::sessions::Refusal::new(
                        DomainErrorCode::Conflict,
                        "help_revision",
                        None,
                    )
                    .fault(devworks_core::protocol::CorrelationId(
                        tx.new_id(IdKind::Correlation),
                    )),
                },
                operation_id: None,
            },
            Docs::Accepted => {
                tx.operations().record_intent(NewOperation {
                    id: op.clone(),
                    root_id: None,
                    assignment_id: None,
                    capability: DOCS.into(),
                    schema_version: 1,
                    args_digest: "d".repeat(64),
                    payload: json!({}),
                })?;
                Decided {
                    decision: DomainDecision::Accepted {
                        operation_id: op.clone(),
                    },
                    operation_id: Some(op),
                }
            }
        })
    }
}

/// A scripted inference pipeline (no model is called).
#[derive(Default)]
struct Pipeline {
    available: bool,
    started: Mutex<Vec<InferenceStart>>,
    cancelled: Mutex<Vec<Id>>,
}
impl InferenceJobs for Pipeline {
    fn availability(&self) -> Result<(), String> {
        if self.available {
            Ok(())
        } else {
            Err("offline".into())
        }
    }
    fn start(&self, run: &InferenceStart) -> Result<Option<Id>, InferenceRefusal> {
        self.started.lock().unwrap().push(run.clone());
        Ok(Some(id(&format!("session-{}", run.run_id.as_str()))))
    }
    fn cancel(&self, run: &Id) -> Result<bool, String> {
        self.cancelled.lock().unwrap().push(run.clone());
        Ok(true)
    }
}

struct Fx {
    h: Harness,
    store: Arc<SqliteStore>,
    jobs: Arc<JobService>,
    docs: Arc<DocsHandler>,
    cases: UseCases,
    n: AtomicU64,
}

impl Fx {
    fn new() -> Self {
        let fx = Self::open(Harness::new());
        fx.write(|tx| {
            for (a, k) in [
                ("user-a", PrincipalKind::Human),
                ("agent-a", PrincipalKind::Agent),
                ("agent-p", PrincipalKind::Agent),
            ] {
                tx.actors().register(&actor(a), k, a)?;
            }
            Ok(())
        });
        fx
    }
    fn open(h: Harness) -> Self {
        let store = Arc::new(h.store());
        let runs = Arc::new(ControlService::new(h.ids.clone()));
        let docs = Arc::new(DocsHandler {
            mode: Mutex::new(Docs::Applied),
            calls: AtomicUsize::new(0),
        });
        let jobs = Arc::new(JobService::new(
            NativeCatalog::of(&[DOCS]),
            JobConfig::default(),
        ));
        let mut cases = UseCases::new()
            .with_runs(runs.clone())
            .with_projects(Arc::new(ProjectService::new(h.ids.clone(), runs.clone())))
            .with_repositories(Arc::new(RepositoryService::new(h.ids.clone())))
            .with_jobs(jobs.clone());
        cases.register(DOCS, None, Some(docs.clone()));
        Self {
            h,
            store,
            jobs,
            docs,
            cases,
            n: AtomicU64::new(0),
        }
    }
    /// A process restart: the store is reopened (a new boot) and boot recovery runs the
    /// schedules' lifecycle hook, as the engine's run supervision does.
    fn restart(self) -> Self {
        let Fx {
            h, store, cases, ..
        } = self;
        drop((cases, store));
        let fx = Self::open(h);
        let hook = JobLifecycle(fx.jobs.clone());
        fx.write(move |tx| hook.recover(tx));
        fx
    }
    fn set_now(&self, at: i64) {
        self.h.clock.0.store(at, Ordering::SeqCst);
    }
    fn ctx(&self, who: &str, kind: PrincipalKind, p: Option<&ProjectId>) -> InvocationContext {
        let info = self.store.info();
        HostIngress::new(&*self.store)
            .context(
                actor(who),
                kind,
                id("invocation-1"),
                id("ui-main"),
                Counter::new(info.restore_epoch).unwrap(),
                Revision::new(1).unwrap(),
            )
            .with_lineage(p.cloned(), None, None)
    }
    fn user(&self) -> InvocationContext {
        self.ctx("user-a", PrincipalKind::Human, None)
    }
    fn agent(&self) -> InvocationContext {
        self.ctx("agent-a", PrincipalKind::Agent, None)
    }
    fn bind_agent(&self, who: &str, p: Option<&ProjectId>) {
        let by = self.user();
        let binding = NewPrincipalBinding {
            actor: actor(who),
            kind: PrincipalKind::Agent,
            role: Some(devworks_core::settings::catalog::AgentRole::Worker),
            project_id: p.cloned(),
            tool_families: None,
            bound_by: actor("user-a"),
        };
        self.write(move |tx| {
            auth::bind_principal(tx, &by, binding, None)
                .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
        });
    }
    fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    ) -> T {
        run_write(&*self.store, WriteOptions::default(), f).expect("write")
    }
    fn read<T>(&self, f: impl FnOnce(&dyn ReadSnapshot) -> Result<T, StorageError>) -> T {
        read_snapshot(&*self.store, &ReadOptions::default(), f).expect("read")
    }
    fn authority(&self) -> HostAuthority<'_> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.h.clock,
            protected_branches: &[],
        }
    }
    fn submit(&self, ctx: &InvocationContext, op: &str, raw: &[u8]) -> CommandOutcome {
        let authority = self.authority();
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        self.cases.command(&gateway, ctx, op, raw)
    }
    fn command_at(
        &self,
        ctx: &InvocationContext,
        op: &str,
        mut body: Value,
        revision: u32,
    ) -> DomainCommandReply {
        body["kind"] = json!(op);
        let key = format!("key-{}", self.n.fetch_add(1, Ordering::SeqCst));
        let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": key,
            "schemaRevision": revision, "body": body});
        self.submit(ctx, op, frame.to_string().as_bytes())
            .expect("addressable")
    }
    fn command(&self, ctx: &InvocationContext, op: &str, body: Value) -> DomainCommandReply {
        self.command_at(ctx, op, body, 2)
    }
    fn query_at(
        &self,
        ctx: &InvocationContext,
        op: &str,
        mut body: Value,
        revision: u32,
    ) -> DomainQueryReply {
        body["kind"] = json!(op);
        let frame = json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-1",
            "schemaRevision": revision, "body": body});
        let authority = self.authority();
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        self.cases
            .query(&gateway, ctx, op, frame.to_string().as_bytes())
            .expect("addressable")
    }

    // Projects (DW-11)

    fn project(&self, name: &str) -> ProjectRecord {
        match applied(self.command_at(
            &self.user(),
            "projects.create",
            json!({"name": name, "description": "work", "repositoryIds": []}),
            1,
        )) {
            OperationResult::ProjectsCreate { value } => value,
            other => panic!("{other:?}"),
        }
    }
    fn lifecycle(&self, op: &str, p: &ProjectId) -> ProjectRecord {
        let current = self
            .read({
                let p = p.clone();
                move |s| s.projects().project(&p)
            })
            .unwrap();
        let reply = self.command_at(
            &self.user(),
            op,
            json!({"projectId": p.0.as_str(), "expectedRevision": current.revision.to_string()}),
            1,
        );
        match applied(reply) {
            OperationResult::ProjectsArchive { value }
            | OperationResult::ProjectsRestore { value } => value,
            other => panic!("{other:?}"),
        }
    }
    fn archive_stop_ran(&self, p: &ProjectId) {
        let p = p.clone();
        self.write(move |tx| {
            let epoch = tx.projects().project(&p)?.expect("project").archive_epoch;
            tx.projects().finish_archive_stop(&p, epoch)
        });
    }

    // Jobs

    fn create(&self, ctx: &InvocationContext, body: Value) -> DomainCommandReply {
        self.command(ctx, "jobs.create", body)
    }
    fn job(&self, ctx: &InvocationContext, body: Value) -> Value {
        job_value(applied(self.create(ctx, body)))
    }
    fn inspect(&self, job: &str, extra: Value) -> Value {
        let mut body = json!({"jobId": job});
        if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        result_value(self.query_at(&self.user(), "jobs.inspect", body, 2))
    }
    fn stored(&self, job: &str) -> StoredJob {
        let j = id(job);
        self.read(move |s| s.jobs().job(&j)).unwrap()
    }
    fn run(&self, run: &str) -> StoredJobRun {
        let r = id(run);
        self.read(move |s| s.jobs().job_run(&r)).unwrap()
    }
    fn runs(&self, job: &str) -> Vec<StoredJobRun> {
        let j = id(job);
        self.read(move |s| s.jobs().job_runs(&j, None, 500))
    }
    fn events(&self, kind: &str) -> Vec<EventRecord> {
        let mut out = Vec::new();
        let mut after = 0;
        loop {
            let page = self.read(move |r| r.history().page(&EventFilter::All, after, 500));
            out.extend(page.events.iter().filter(|e| e.kind == kind).cloned());
            after = page.events.last().map_or(after, |e| e.sequence);
            if !page.has_more {
                return out;
            }
        }
    }
    fn claim(&self) -> Vec<StoredJobRun> {
        let svc = self.jobs.clone();
        self.write(move |tx| svc.claim_due(tx))
    }
    /// Executes one queued run the way the engine's scheduler does (start, dispatching, the
    /// effect through the gateway or the pipeline, settle), synchronously.
    fn execute(&self, run: &str, pipeline: &Arc<dyn InferenceJobs>) -> Option<StoredJobRun> {
        let (svc, r, p) = (self.jobs.clone(), id(run), pipeline.clone());
        let begun = self.write(move |tx| svc.begin(tx, &r, &*p))?;
        let (svc, b) = (self.jobs.clone(), begun.clone());
        if !self.write(move |tx| svc.dispatching(tx, &b)) {
            let (svc, b) = (self.jobs.clone(), begun.clone());
            self.write(move |tx| svc.undispatched(tx, &b));
            return Some(self.run(run));
        }
        match begun.step.clone() {
            Step::Native {
                ctx,
                operation,
                frame,
            } => {
                let outcome = Dispatch(self).command(&ctx, operation, &frame);
                let svc = self.jobs.clone();
                self.write(move |tx| svc.settle_native(tx, &begun, &outcome))
            }
            Step::Inference(start) => {
                let started = pipeline.start(&start);
                let svc = self.jobs.clone();
                self.write(move |tx| svc.settle_inference_start(tx, &begun, &started))
            }
        }
    }
}

/// The native dispatch port over this fixture's gateway (the engine uses the runtime's).
struct Dispatch<'a>(&'a Fx);
impl NativeJobDispatch for Dispatch<'_> {
    fn command(&self, ctx: &InvocationContext, operation: &str, raw: &[u8]) -> CommandOutcome {
        self.0.submit(ctx, operation, raw)
    }
}

fn recorded(reply: DomainCommandReply) -> (DomainDecision, bool) {
    match reply {
        CommandReply::Recorded {
            receipt, replayed, ..
        } => (receipt.decision, replayed),
        other => panic!("expected a recorded decision, got {other:?}"),
    }
}
fn applied(reply: DomainCommandReply) -> OperationResult {
    match recorded(reply).0 {
        DomainDecision::Applied { result } => result,
        other => panic!("expected Applied, got {other:?}"),
    }
}
fn refused(reply: DomainCommandReply) -> DomainFault {
    match recorded(reply).0 {
        DomainDecision::Refused { fault } => fault,
        other => panic!("expected Refused, got {other:?}"),
    }
}
fn kind(f: &DomainFault) -> (DomainErrorCode, String) {
    (
        f.code,
        f.target_kind
            .as_ref()
            .map(|k| k.0.clone())
            .unwrap_or_default(),
    )
}
fn value_of(r: &OperationResult) -> Value {
    serde_json::to_value(r).unwrap()["value"].clone()
}
fn job_value(r: OperationResult) -> Value {
    value_of(&r)
}
fn result_value(reply: DomainQueryReply) -> Value {
    match reply {
        DomainQueryReply::Result { result, .. } => value_of(&result),
        DomainQueryReply::Rejected { fault, .. } => panic!("rejected: {fault:?}"),
    }
}
fn query_fault(reply: DomainQueryReply) -> (DomainErrorCode, String) {
    match reply {
        DomainQueryReply::Rejected { fault, .. } => kind(&fault),
        other => panic!("expected a fault, got {other:?}"),
    }
}
fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_owned()
}

fn native() -> Value {
    let schema = argument_schema(DOCS, SchemaRevision(1)).unwrap();
    json!({"kind": "Native", "capabilityId": "help-generate_capability_docs", "arguments": {
        "schema": serde_json::to_value(schema).unwrap(),
        "value": {"registryRevision": "1", "expectedHelpRevision": "1"}}})
}
fn calendar(expr: &str, tz: &str) -> Value {
    json!({"kind": "Calendar", "expression": expr, "timezone": tz})
}
fn every(seconds: i64) -> Value {
    json!({"kind": "Interval", "seconds": seconds.to_string()})
}
fn body(title: &str, schedule: Value, definition: Value, enabled: bool) -> Value {
    json!({"scope": {"kind": "Workspace"}, "title": title, "schedule": schedule,
        "definition": definition, "enabled": enabled})
}
fn none() -> Arc<dyn InferenceJobs> {
    Arc::new(NoInferencePipeline)
}

// ---------------------------------------------------------------------------------------------
// Records, validation and the retained CodeGraph behavior

#[test]
fn jobs_are_created_listed_inspected_and_edited_with_both_revisions() {
    let fx = Fx::new();
    let user = fx.user();
    let job = fx.job(&user, body("Docs", every(3600), native(), true));
    assert_eq!(s(&job, "revision"), "1");
    assert_eq!(job["scope"], json!({"kind": "Workspace"}));
    assert_eq!(job["authorized"], json!(true));
    // The next run is one interval after creation, never now.
    assert_eq!(s(&job, "nextRunAtMs"), (T0 + 3_600_000).to_string());
    assert!(job.get("lastRunId").is_none() && job.get("activeRunId").is_none());
    // Revision 1 readers see exactly the revision-1 record.
    let r1 = result_value(fx.query_at(&user, "jobs.inspect", json!({"jobId": s(&job, "id")}), 1));
    assert!(r1["job"].get("nextRunAtMs").is_none() && r1["job"].get("scope").is_none());
    assert_eq!(r1["job"]["title"], json!("Docs"));
    // Titles are unique per scope, case-insensitively (CodeGraph "already exists").
    let dup = refused(fx.create(&user, body("docs", every(3600), native(), false)));
    assert_eq!(kind(&dup), (DomainErrorCode::Conflict, "job_title".into()));
    // Unknown zones, expressions without a future occurrence and too-short intervals are refused.
    for (schedule, target) in [
        (calendar("0 3 * * *", "Mars/Olympus"), "timezone"),
        (calendar("0 0 30 2 *", "UTC"), "schedule"),
        (calendar("61 * * * *", "UTC"), "schedule"),
        (every(30), "schedule"),
    ] {
        let f = refused(fx.create(&user, body("Bad", schedule, native(), true)));
        assert_eq!(kind(&f), (DomainErrorCode::InvalidArgument, target.into()));
    }
    // Arguments bound to another schema, or that the capability's reader refuses, are refused.
    let mut wrong = native();
    wrong["arguments"]["schema"]["sha256"] = json!("0".repeat(64));
    assert_eq!(
        kind(&refused(
            fx.create(&user, body("Wrong", every(3600), wrong, false))
        )),
        (DomainErrorCode::SchemaMismatch, "arguments".into())
    );
    let mut bad = native();
    bad["arguments"]["value"] = json!({"registryRevision": "1"});
    assert_eq!(
        kind(&refused(
            fx.create(&user, body("Bad args", every(3600), bad, false))
        )),
        (DomainErrorCode::InvalidArgument, "arguments".into())
    );
    let mut unqualified = native();
    unqualified["capabilityId"] = json!("maintenance-prune_sessions");
    assert_eq!(
        kind(&refused(fx.create(
            &user,
            body("Prune", every(3600), unqualified, false)
        ))),
        (DomainErrorCode::UnsupportedTarget, "capability".into())
    );
    // Edit recomputes the next occurrence from now and advances the revision.
    fx.set_now(T0 + 10 * MINUTE);
    let edited = job_value(applied(fx.command(
        &user,
        "jobs.edit",
        json!({"jobId": s(&job, "id"), "expectedRevision": "1", "title": "Docs nightly",
            "schedule": calendar("0 3 * * *", "America/New_York"), "definition": native()}),
    )));
    assert_eq!(s(&edited, "revision"), "2");
    let next: i64 = s(&edited, "nextRunAtMs").parse().unwrap();
    assert!(next > T0 + 10 * MINUTE);
    // A stale revision is a conflict, never a lost update.
    let stale = refused(fx.command(
        &user,
        "jobs.edit",
        json!({"jobId": s(&job, "id"), "expectedRevision": "1", "title": "X",
            "schedule": every(3600), "definition": native()}),
    ));
    assert_eq!(stale.code, DomainErrorCode::RevisionConflict);
}

#[test]
fn listing_pages_by_title_within_one_scope() {
    let fx = Fx::new();
    let user = fx.user();
    for t in ["Charlie", "alpha", "Bravo"] {
        fx.job(&user, body(t, every(3600), native(), false));
    }
    let p = fx.project("P");
    fx.job(
        &user,
        json!({"scope": {"kind": "Project", "projectId": p.id.0.as_str()}, "title": "Alpha",
            "schedule": every(3600), "definition": native(), "enabled": false}),
    );
    let page = |cursor: Option<&str>| {
        let mut b = json!({"scope": {"kind": "Workspace"}, "page": {"limit": 2}});
        if let Some(c) = cursor {
            b["page"]["cursor"] = json!(c);
        }
        result_value(fx.query_at(&user, "jobs.list", b, 2))
    };
    let first = page(None);
    let titles: Vec<&str> = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["alpha", "Bravo"]);
    assert_eq!(first["hasMore"], json!(true));
    let second = page(first["nextCursor"].as_str());
    assert_eq!(second["items"][0]["title"], json!("Charlie"));
    assert_eq!(second["hasMore"], json!(false));
    // A project's list holds only its own jobs; a project invocation cannot list app jobs.
    let proj = result_value(fx.query_at(
        &user,
        "jobs.list",
        json!({"scope": {"kind": "Project", "projectId": p.id.0.as_str()}, "page": {"limit": 500}}),
        2,
    ));
    assert_eq!(proj["items"].as_array().unwrap().len(), 1);
    let scoped = fx.ctx("user-a", PrincipalKind::Human, Some(&p.id));
    assert_eq!(
        query_fault(fx.query_at(
            &scoped,
            "jobs.list",
            json!({"scope": {"kind": "Workspace"}, "page": {"limit": 10}}),
            2
        )),
        (DomainErrorCode::PermissionDenied, "scope".into())
    );
    // A cursor of another query is expired, never silently continued.
    let other = fx.query_at(
        &user,
        "jobs.list",
        json!({"scope": {"kind": "Workspace"}, "enabled": true,
            "page": {"limit": 2, "cursor": first["nextCursor"]}}),
        2,
    );
    assert_eq!(query_fault(other).0, DomainErrorCode::CursorExpired);
}

#[test]
fn only_a_person_authorizes_a_schedule() {
    let fx = Fx::new();
    fx.bind_agent("agent-a", None);
    let (user, agent) = (fx.user(), fx.agent());
    // An agent's enabled schedule is refused; its draft is created unauthorized.
    let f = refused(fx.create(&agent, body("Agent job", every(3600), native(), true)));
    assert_eq!(
        kind(&f),
        (
            DomainErrorCode::ApprovalRequired,
            "job_authorization".into()
        )
    );
    let draft = fx.job(&agent, body("Agent job", every(3600), native(), false));
    assert_eq!(draft["authorized"], json!(false));
    let jid = s(&draft, "id");
    let enable = |ctx: &InvocationContext, rev: &str| {
        fx.command(
            ctx,
            "jobs.enable",
            json!({"jobId": jid, "expectedRevision": rev}),
        )
    };
    assert_eq!(
        refused(enable(&agent, "1")).code,
        DomainErrorCode::ApprovalRequired
    );
    // It cannot run what no person authorized either.
    let run = refused(fx.command(
        &agent,
        "jobs.run_now",
        json!({"jobId": jid, "expectedRevision": "1"}),
    ));
    assert_eq!(run.code, DomainErrorCode::ApprovalRequired);
    // The person enables it: authorized, with a next run.
    let enabled = job_value(applied(enable(&user, "1")));
    assert_eq!(
        (enabled["authorized"].clone(), enabled["enabled"].clone()),
        (json!(true), json!(true))
    );
    assert!(enabled.get("nextRunAtMs").is_some());
    // Now the agent may run it, but not edit it.
    applied(fx.command(
        &agent,
        "jobs.run_now",
        json!({"jobId": jid, "expectedRevision": "2"}),
    ));
    let edit = refused(fx.command(
        &agent,
        "jobs.edit",
        json!({"jobId": jid, "expectedRevision": "2", "title": "Changed", "schedule": every(3600), "definition": native()}),
    ));
    assert_eq!(edit.code, DomainErrorCode::ApprovalRequired);
    // Disabling is always allowed; a disabled job schedules nothing.
    let off = job_value(applied(fx.command(
        &agent,
        "jobs.disable",
        json!({"jobId": jid, "expectedRevision": "2"}),
    )));
    assert!(off.get("nextRunAtMs").is_none());
}

// ---------------------------------------------------------------------------------------------
// Claims: due, missed, overlap, DST

#[test]
fn a_due_occurrence_runs_once_overlap_is_skipped_and_missed_ones_are_never_caught_up() {
    let fx = Fx::new();
    let job = fx.job(&fx.user(), body("Docs", every(3600), native(), true));
    let jid = s(&job, "id");
    // Nothing is due before the first occurrence.
    fx.set_now(T0 + 3_599_000);
    assert!(fx.claim().is_empty());
    fx.set_now(T0 + 3_600_000 + 1_000);
    let queued = fx.claim();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].occurrence_key, format!("at-{}", T0 + 3_600_000));
    assert_eq!(fx.stored(&jid).next_at_ms, Some(T0 + 7_200_000));
    // Claiming again at the same instant records nothing more.
    assert!(fx.claim().is_empty());
    // The next occurrence arrives while the run is still active: Skipped, nothing queued.
    fx.set_now(T0 + 7_200_000 + 500);
    assert!(fx.claim().is_empty());
    let runs = fx.runs(&jid);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].status, "Skipped");
    assert_eq!(runs[0].skipped_for.as_ref(), Some(&queued[0].id));
    assert!(runs[0].operation_id.is_none());
    assert_eq!(fx.run(queued[0].id.as_str()).status, "Queued");
    // The app was closed for five hours: those occurrences are missed, counted once, and the
    // next is in the future — no catch-up run and no coalesced one.
    fx.execute(queued[0].id.as_str(), &none());
    fx.set_now(T0 + 7_200_000 + 5 * 3_600_000 + 30 * MINUTE);
    assert!(fx.claim().is_empty());
    assert_eq!(fx.runs(&jid).len(), 2);
    let missed = fx.events("JobOccurrencesMissed");
    assert_eq!(missed.last().unwrap().payload["count"], json!(5));
    let next = fx.stored(&jid).next_at_ms.unwrap();
    assert!(next > T0 + 7_200_000 + 5 * 3_600_000 + 30 * MINUTE);
    // Within the grace one occurrence runs and later past ones (none here) are missed.
    fx.set_now(next + 59_000);
    assert_eq!(fx.claim().len(), 1);
}

#[test]
fn calendar_occurrences_are_timezone_correct_across_dst_gap_and_fold() {
    let fx = Fx::new();
    let user = fx.user();
    // 2026-03-08: New York jumps from 02:00 EST to 03:00 EDT; 02:30 runs at the jump.
    fx.set_now(ms("2026-03-07T12:00:00Z"));
    let daily = fx.job(
        &user,
        body(
            "Daily",
            calendar("30 2 * * *", "America/New_York"),
            native(),
            true,
        ),
    );
    let jump = ms("2026-03-08T07:00:00Z");
    assert_eq!(s(&daily, "nextRunAtMs"), jump.to_string());
    fx.set_now(jump + 1_000);
    let run = fx.claim().pop().unwrap();
    assert_eq!(run.occurrence_key, format!("at-{jump}"));
    assert_eq!(run.occurrence_at_ms, jump);
    fx.execute(run.id.as_str(), &none());
    assert_eq!(
        fx.stored(&s(&daily, "id")).next_at_ms,
        Some(ms("2026-03-09T06:30:00Z"))
    );
    // 2026-11-01: 01:00-02:00 repeats; an interval expression runs at both instants, as two
    // distinct occurrences; a daily one only at the first.
    fx.set_now(ms("2026-11-01T05:10:00Z"));
    let half = fx.job(
        &user,
        body(
            "Half-hourly",
            calendar("*/30 * * * *", "America/New_York"),
            native(),
            true,
        ),
    );
    let folded = fx.job(
        &user,
        body(
            "Folded",
            calendar("30 1 * * *", "America/New_York"),
            native(),
            true,
        ),
    );
    let mut keys = Vec::new();
    for at in [
        "2026-11-01T05:30:00Z",
        "2026-11-01T06:00:00Z",
        "2026-11-01T06:30:00Z",
        "2026-11-01T07:00:00Z",
    ] {
        fx.set_now(ms(at) + 1_000);
        for r in fx.claim() {
            if r.job_id.as_str() == s(&half, "id") {
                keys.push(r.occurrence_key.clone());
            }
            fx.execute(r.id.as_str(), &none());
        }
    }
    assert_eq!(keys.len(), 4, "{keys:?}");
    let folded_runs = fx.runs(&s(&folded, "id"));
    assert_eq!(
        folded_runs.len(),
        1,
        "a daily job runs once on the fold day"
    );
    assert_eq!(folded_runs[0].occurrence_at_ms, ms("2026-11-01T05:30:00Z"));
}

// ---------------------------------------------------------------------------------------------
// Native runs through the gateway

#[test]
fn native_runs_dispatch_once_through_the_gateway_and_report_their_outcome() {
    let fx = Fx::new();
    let user = fx.user();
    let job = fx.job(
        &user,
        body("Docs", json!({"kind": "Manual"}), native(), false),
    );
    let jid = s(&job, "id");
    // Run now of a disabled job (CodeGraph allowed it): one Manual run with its operation.
    let run = match applied(fx.command(
        &user,
        "jobs.run_now",
        json!({"jobId": jid, "expectedRevision": "1"}),
    )) {
        OperationResult::JobsRunNow { value } => value,
        other => panic!("{other:?}"),
    };
    let run_v = serde_json::to_value(&run).unwrap();
    assert_eq!(
        (s(&run_v, "status"), s(&run_v, "trigger")),
        ("Queued".into(), "Manual".into())
    );
    // Run now again while it is queued: recorded Skipped (JOB-08), nothing queued.
    let again = value_of(&applied(fx.command(
        &user,
        "jobs.run_now",
        json!({"jobId": jid, "expectedRevision": "1"}),
    )));
    assert_eq!(s(&again, "status"), "Skipped");
    assert_eq!(s(&again, "skippedForRunId"), run.id.as_str());
    let done = fx.execute(run.id.as_str(), &none()).unwrap();
    assert_eq!(done.status, "Succeeded");
    assert_eq!(fx.docs.calls.load(Ordering::SeqCst), 1);
    let op = fx
        .read({
            let o = done.operation_id.clone().unwrap();
            move |s| s.operations().operation(&o)
        })
        .unwrap();
    assert_eq!(op.state, OperationState::ObservedComplete);
    let rid = run.id.clone();
    let inspected = fx.inspect(
        &jid,
        json!({"runId": rid.as_str(), "history": {"limit": 10}}),
    );
    assert_eq!(s(&inspected["run"], "status"), "Succeeded");
    assert_eq!(inspected["runs"].as_array().unwrap().len(), 2);
    assert_eq!(s(&inspected["job"], "lastRunStatus"), "Skipped");
    // A refused command fails the run with its reason; an accepted one keeps it running on that
    // operation until the operation ends.
    *fx.docs.mode.lock().unwrap() = Docs::Refused;
    let r2 = value_of(&applied(fx.command(
        &user,
        "jobs.run_now",
        json!({"jobId": jid, "expectedRevision": "1"}),
    )));
    let failed = fx.execute(&s(&r2, "id"), &none()).unwrap();
    assert_eq!(failed.status, "Failed");
    assert!(failed.reason.unwrap().contains("Conflict"));
    *fx.docs.mode.lock().unwrap() = Docs::Accepted;
    let r3 = value_of(&applied(fx.command(
        &user,
        "jobs.run_now",
        json!({"jobId": jid, "expectedRevision": "1"}),
    )));
    let running = fx.execute(&s(&r3, "id"), &none()).unwrap();
    assert_eq!(running.status, "Running");
    let inner = running.inner_operation_id.clone().unwrap();
    let svc = fx.jobs.clone();
    assert_eq!(fx.write(move |tx| svc.poll_inner(tx)), 0);
    fx.write(move |tx| {
        let rec = tx.operations().operation(&inner)?.unwrap();
        tx.operations()
            .transition(&inner, rec.revision, None, OperationState::Failed, None)
    });
    let svc = fx.jobs.clone();
    assert_eq!(fx.write(move |tx| svc.poll_inner(tx)), 1);
    assert_eq!(fx.run(&s(&r3, "id")).status, "Failed");
}

#[test]
fn cancel_is_bound_to_the_named_run_and_never_reports_an_unverified_stop() {
    let fx = Fx::new();
    let user = fx.user();
    let job = fx.job(
        &user,
        body("Docs", json!({"kind": "Manual"}), native(), false),
    );
    let jid = s(&job, "id");
    let first = value_of(&applied(fx.command(
        &user,
        "jobs.run_now",
        json!({"jobId": jid, "expectedRevision": "1"}),
    )));
    // A run never dispatched is cancelled at once (CancelledBeforeDispatch).
    let cancelled = value_of(&applied(fx.command(
        &user,
        "jobs.cancel",
        json!({"jobId": jid, "runId": s(&first, "id"), "expectedRevision": "1"}),
    )));
    assert_eq!(s(&cancelled, "status"), "Cancelled");
    assert_eq!(s(&cancelled, "state"), "CancelledBeforeDispatch");
    assert_eq!(fx.docs.calls.load(Ordering::SeqCst), 0);
    // The cancellation of the finished run answers unchanged and cannot touch the next run.
    let second = value_of(&applied(fx.command(
        &user,
        "jobs.run_now",
        json!({"jobId": jid, "expectedRevision": "1"}),
    )));
    let (decision, _) = recorded(fx.command(
        &user,
        "jobs.cancel",
        json!({"jobId": jid, "runId": s(&first, "id"), "expectedRevision": "2"}),
    ));
    assert!(matches!(decision, DomainDecision::NoChange { .. }));
    assert_eq!(fx.run(&s(&second, "id")).status, "Queued");
    assert!(!fx.run(&s(&second, "id")).cancel_requested);
    // A dispatched run (an accepted operation) is asked to stop: Accepted, not done.
    *fx.docs.mode.lock().unwrap() = Docs::Accepted;
    let running = fx.execute(&s(&second, "id"), &none()).unwrap();
    let (decision, _) = recorded(fx.command(
        &user,
        "jobs.cancel",
        json!({"jobId": jid, "runId": running.id.as_str(), "expectedRevision": running.revision.to_string()}),
    ));
    assert!(
        matches!(decision, DomainDecision::Accepted { .. }),
        "{decision:?}"
    );
    let asked = fx.run(running.id.as_str());
    assert!(asked.cancel_requested && asked.status == "Running");
    // While its operation still runs, the settle waits; an uncertain stop is never "cancelled".
    let svc = fx.jobs.clone();
    let (rid, rev) = (asked.id.clone(), asked.revision);
    assert!(
        fx.write(move |tx| svc.settle_cancel(tx, &rid, rev, &StopVerdict::Stopped))
            .is_none()
    );
    let inner = asked.inner_operation_id.clone().unwrap();
    fx.write(move |tx| {
        let rec = tx.operations().operation(&inner)?.unwrap();
        tx.operations()
            .transition(&inner, rec.revision, None, OperationState::Failed, None)
    });
    let svc = fx.jobs.clone();
    let (rid, rev) = (asked.id.clone(), asked.revision);
    let settled = fx
        .write(move |tx| {
            svc.settle_cancel(
                tx,
                &rid,
                rev,
                &StopVerdict::Uncertain("tree 7 alive".into()),
            )
        })
        .unwrap();
    assert_eq!(settled.status, "OutcomeUnknown");
    assert!(
        settled
            .reason
            .unwrap()
            .contains("could not be verified stopped")
    );
    // A stale settle (an earlier revision) changes nothing.
    let svc = fx.jobs.clone();
    let rid = asked.id.clone();
    assert!(
        fx.write(move |tx| svc.settle_cancel(tx, &rid, 1, &StopVerdict::Stopped))
            .is_none()
    );
}

#[test]
fn delete_tombstones_the_job_keeps_history_and_cancels_its_active_run() {
    let fx = Fx::new();
    let user = fx.user();
    let job = fx.job(&user, body("Docs", every(3600), native(), true));
    let jid = s(&job, "id");
    let run = value_of(&applied(fx.command(
        &user,
        "jobs.run_now",
        json!({"jobId": jid, "expectedRevision": "1"}),
    )));
    let deleted = job_value(applied(fx.command(
        &user,
        "jobs.delete",
        json!({"jobId": jid, "expectedRevision": "1"}),
    )));
    assert!(deleted.get("deletedAtMs").is_some() && deleted["enabled"] == json!(false));
    assert_eq!(fx.run(&s(&run, "id")).status, "Cancelled");
    // Gone from listing and inspection (CodeGraph 404), its runs and history kept.
    assert_eq!(
        query_fault(fx.query_at(&user, "jobs.inspect", json!({"jobId": jid}), 2)),
        (DomainErrorCode::NotFound, "job".into())
    );
    assert_eq!(fx.runs(&jid).len(), 1);
    assert_eq!(fx.events("JobDeleted").len(), 1);
    // The title is free again; the tombstone is never due.
    fx.job(&user, body("Docs", every(3600), native(), true));
    fx.set_now(T0 + 3_600_000 + 1);
    assert!(fx.claim().iter().all(|r| r.job_id.as_str() != jid));
}

// ---------------------------------------------------------------------------------------------
// Scheduled inference (no pipeline in this build; the run lifecycle up to the dispatch port)

fn seed_prompt(fx: &Fx, project: Option<&ProjectId>) -> Value {
    let p = project.cloned();
    fx.write(move |tx| {
        let digest = "a".repeat(64);
        tx.artifacts()
            .put_blob(&digest, 12, "gen-1", "blobs/aa/prompt")?;
        tx.artifacts().insert_artifact(&StoredArtifact {
            id: id("prompt-1"),
            origin_project_id: p.clone(),
            origin_work_id: None,
            title: "Triage new issues".into(),
            revision: 1,
            scope: match &p {
                Some(p) => ArtifactScope::Project(p.clone()),
                None => ArtifactScope::Workspace,
            },
            owner: if p.is_none() {
                Some(actor("user-a"))
            } else {
                None
            },
            retention: ArtifactRetention::Explicit,
            scope_revision: 1,
            reference_revision: 1,
            current_version_id: None,
            state: ArtifactState::Live,
            created_at_ms: T0,
            provenance: json!({}),
        })?;
        tx.artifacts().insert_version(&StoredVersion {
            id: id("prompt-v1"),
            artifact_id: id("prompt-1"),
            version_no: 1,
            blob_digest: digest.clone(),
            byte_count: 12,
            blob_generation: "gen-1".into(),
            media_type: "text/markdown".into(),
            created_at_ms: T0,
            deleted_at_ms: None,
            source: json!({}),
        })
    });
    json!({"kind": "Inference", "prompt": {"artifactId": "prompt-1", "versionId": "prompt-v1",
        "sha256": "a".repeat(64), "byteLength": "12", "mediaType": "text/markdown"},
        "route": {"kind": "Tier", "tier": "High"}, "effort": "Default"})
}

#[test]
fn scheduled_inference_fails_truthfully_without_a_pipeline_and_acts_as_a_bound_agent_with_one() {
    let fx = Fx::new();
    let user = fx.user();
    let p = fx.project("P");
    let def = seed_prompt(&fx, Some(&p.id));
    let scope = json!({"kind": "Project", "projectId": p.id.0.as_str()});
    let job = fx.job(
        &user,
        json!({"scope": scope, "title": "Issue scan", "schedule": every(900),
        "definition": def, "enabled": true}),
    );
    let jid = s(&job, "id");
    // This build has no inference pipeline: the run fails before dispatch with a visible
    // reason, its operation never dispatched, and nothing reports success.
    let r1 = value_of(&applied(fx.command(
        &user,
        "jobs.run_now",
        json!({"jobId": jid, "expectedRevision": "1"}),
    )));
    assert!(fx.execute(&s(&r1, "id"), &none()).is_none());
    let failed = fx.run(&s(&r1, "id"));
    assert_eq!(failed.status, "Failed");
    assert!(
        failed
            .reason
            .as_deref()
            .unwrap()
            .contains("not available in this build")
    );
    let op = failed.operation_id.clone().unwrap();
    let rec = fx.read(move |s| s.operations().operation(&op)).unwrap();
    assert_eq!(rec.state, OperationState::Failed);
    assert_eq!(
        rec.attempts.last().unwrap().observations.last().unwrap()["dispatched"],
        json!(false)
    );
    // With a pipeline the run hands over its agent: bound by the host as a Scheduled agent of the
    // job's project with every configured tool, acting through its own context.
    let pipeline: Arc<dyn InferenceJobs> = Arc::new(Pipeline {
        available: true,
        ..Pipeline::default()
    });
    let r2 = value_of(&applied(fx.command(
        &user,
        "jobs.run_now",
        json!({"jobId": jid, "expectedRevision": "1"}),
    )));
    let running = fx.execute(&s(&r2, "id"), &pipeline).unwrap();
    assert_eq!(running.status, "Running");
    assert_eq!(
        running.session_id.as_ref().map(Id::as_str),
        Some(format!("session-{}", s(&r2, "id")).as_str())
    );
    let agent = running.agent_actor.clone().unwrap();
    let binding = fx.read(move |s| s.authority().principal(&agent)).unwrap();
    assert_eq!(
        binding.role,
        Some(devworks_core::settings::catalog::AgentRole::Scheduled)
    );
    assert_eq!(binding.project_id.as_ref(), Some(&p.id));
    assert!(binding.tool_families.is_none() && binding.active);
    // The pipeline reports holds and the outcome, fenced by run identity and revision.
    let svc = fx.jobs.clone();
    let (rid, rev) = (running.id.clone(), running.revision);
    let holding = fx.write(move |tx| {
        svc.finish_inference(
            tx,
            &rid,
            rev,
            &InferenceOutcome::Holding("ApprovalWait".into()),
        )
    });
    assert_eq!(
        (holding.status.as_str(), holding.hold.as_deref()),
        ("Holding", Some("ApprovalWait"))
    );
    let svc = fx.jobs.clone();
    let (rid, stale) = (running.id.clone(), running.revision);
    let late = run_write(&*fx.store, WriteOptions::default(), move |tx| {
        svc.finish_inference(tx, &rid, stale, &InferenceOutcome::Succeeded)
    });
    assert!(
        matches!(late, Err(StorageError::LeaseLost { .. })),
        "{late:?}"
    );
    let svc = fx.jobs.clone();
    let (rid, rev) = (holding.id.clone(), holding.revision);
    let resumed =
        fx.write(move |tx| svc.finish_inference(tx, &rid, rev, &InferenceOutcome::Resumed));
    let svc = fx.jobs.clone();
    let (rid, rev) = (resumed.id.clone(), resumed.revision);
    assert_eq!(
        fx.write(move |tx| svc.finish_inference(tx, &rid, rev, &InferenceOutcome::Succeeded))
            .status,
        "Succeeded"
    );
}

/// Runs one inference run of `job` to `Succeeded` through `pipeline`; returns its agent.
fn run_inference_to_success(fx: &Fx, job: &str, pipeline: &Arc<dyn InferenceJobs>) -> ActorId {
    let rid = s(&value_of(&applied(run_now(fx, &fx.user(), job))), "id");
    let running = fx.execute(&rid, pipeline).expect("started");
    assert_eq!(running.status, "Running", "{:?}", running.reason);
    let svc = fx.jobs.clone();
    let (r, rev) = (running.id.clone(), running.revision);
    let done = fx.write(move |tx| svc.finish_inference(tx, &r, rev, &InferenceOutcome::Succeeded));
    assert_eq!(done.status, "Succeeded");
    running.agent_actor.expect("agent")
}
fn principal_of(fx: &Fx, agent: &ActorId) -> PrincipalBinding {
    let a = agent.clone();
    fx.read(move |s| s.authority().principal(&a))
        .expect("bound")
}

/// A person's revocation of a job's scheduled agent stands (JOB-07): the next run fails before
/// dispatch with a visible reason, and the binding stays revoked at its revision.
#[test]
fn a_revoked_scheduled_agent_is_never_rebound_and_its_run_fails_visibly() {
    let fx = Fx::new();
    let user = fx.user();
    let p = fx.project("P");
    let def = seed_prompt(&fx, Some(&p.id));
    let job = fx.job(
        &user,
        json!({"scope": {"kind": "Project", "projectId": p.id.0.as_str()},
        "title": "Issue scan", "schedule": every(900), "definition": def, "enabled": true}),
    );
    let jid = s(&job, "id");
    let pipeline = Arc::new(Pipeline {
        available: true,
        ..Pipeline::default()
    });
    let dynp: Arc<dyn InferenceJobs> = pipeline.clone();
    let agent = run_inference_to_success(&fx, &jid, &dynp);
    let bound = principal_of(&fx, &agent);
    let (by, a, rev) = (user.clone(), agent.clone(), bound.revision);
    let revoked = fx.write(move |tx| {
        auth::revoke_principal(tx, &by, &a, rev)
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    });
    assert!(!revoked.active);
    let rid = s(&value_of(&applied(run_now(&fx, &user, &jid))), "id");
    assert!(fx.execute(&rid, &dynp).is_none(), "nothing starts");
    let failed = fx.run(&rid);
    assert_eq!(failed.status, "Failed");
    assert!(
        failed.reason.as_deref().unwrap().contains("revoked"),
        "{:?}",
        failed.reason
    );
    let op = failed.operation_id.clone().unwrap();
    let rec = fx.read(move |s| s.operations().operation(&op)).unwrap();
    assert_eq!(rec.state, OperationState::Failed);
    assert_eq!(
        rec.attempts.last().unwrap().observations.last().unwrap()["dispatched"],
        json!(false)
    );
    assert_eq!(
        pipeline.started.lock().unwrap().len(),
        1,
        "no second dispatch"
    );
    let after = principal_of(&fx, &agent);
    assert!(!after.active);
    assert_eq!(after.revision, revoked.revision, "not re-bound");
}

/// A person's narrowing of a job's scheduled agent (role and tool families) stands across later
/// runs; retargeting the job moves only the binding's project.
#[test]
fn a_narrowed_scheduled_agent_keeps_its_restrictions_across_runs_and_retargeting() {
    let fx = Fx::new();
    let user = fx.user();
    let p = fx.project("P");
    let q = fx.project("Q");
    let jid = targeting(&fx, &p.id, "Scan P", every(3600));
    let dynp: Arc<dyn InferenceJobs> = Arc::new(Pipeline {
        available: true,
        ..Pipeline::default()
    });
    let agent = run_inference_to_success(&fx, &jid, &dynp);
    let first = principal_of(&fx, &agent);
    assert_eq!(first.role, Some(AgentRole::Scheduled));
    assert!(first.tool_families.is_none());
    let narrowed = vec!["issues".to_owned()];
    let (by, binding) = (
        user.clone(),
        NewPrincipalBinding {
            actor: agent.clone(),
            kind: PrincipalKind::Agent,
            role: Some(AgentRole::Worker),
            project_id: Some(p.id.clone()),
            tool_families: Some(narrowed.clone()),
            bound_by: actor("user-a"),
        },
    );
    let rev = first.revision;
    let narrowed_binding = fx.write(move |tx| {
        auth::bind_principal(tx, &by, binding, Some(rev))
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    });
    // The next run reuses the binding as the person left it.
    assert_eq!(run_inference_to_success(&fx, &jid, &dynp), agent);
    let kept = principal_of(&fx, &agent);
    assert_eq!(kept.revision, narrowed_binding.revision, "not re-bound");
    assert_eq!(kept.tool_families.as_ref(), Some(&narrowed));
    assert_eq!(kept.role, Some(AgentRole::Worker));
    // Retargeted to Q: the binding moves to Q and keeps the person's restrictions.
    let mut def = seed_prompt_once(&fx);
    def["projectId"] = json!(q.id.0.as_str());
    let rev = fx.stored(&jid).revision.to_string();
    applied(fx.command(
        &user,
        "jobs.edit",
        json!({"jobId": jid, "expectedRevision": rev, "title": "Scan Q",
            "schedule": every(3600), "definition": def}),
    ));
    assert_eq!(run_inference_to_success(&fx, &jid, &dynp), agent);
    let moved = principal_of(&fx, &agent);
    assert_eq!(moved.project_id.as_ref(), Some(&q.id));
    assert!(moved.active);
    assert_eq!(moved.tool_families.as_ref(), Some(&narrowed));
    assert_eq!(moved.role, Some(AgentRole::Worker));
}

#[test]
fn a_scheduled_agent_starts_work_only_under_ordinary_controls_and_budgets() {
    let fx = Fx::new();
    let user = fx.user();
    let p = fx.project("P");
    let def = seed_prompt(&fx, Some(&p.id));
    let job = fx.job(
        &user,
        json!({"scope": {"kind": "Project", "projectId": p.id.0.as_str()},
        "title": "Keeper", "schedule": every(900), "definition": def, "enabled": true}),
    );
    let pipeline = Arc::new(Pipeline {
        available: true,
        ..Pipeline::default()
    });
    let dynp: Arc<dyn InferenceJobs> = pipeline.clone();
    let run = value_of(&applied(fx.command(
        &user,
        "jobs.run_now",
        json!({"jobId": s(&job, "id"), "expectedRevision": "1"}),
    )));
    fx.execute(&s(&run, "id"), &dynp).unwrap();
    let start = pipeline.started.lock().unwrap().pop().unwrap();
    // A feature root of the project, paused by a person: the scheduled agent may select it, but
    // Start leaves the pause effective (ContinueRequired) — scheduling grants no bypass.
    let sql = format!(
        "INSERT INTO work_items VALUES('feature-1','{}','feature',NULL,NULL,NULL,'F',1,0,1,1,1,'{{}}');",
        p.id.0.as_str()
    );
    fx.store
        .with_writer_connection(move |c| super::super::conn::db(c.execute_batch(&sql)))
        .unwrap();
    let root = RootId(id("feature-1"));
    let control = |fx: &Fx| {
        let r = root.clone();
        fx.read(move |s| devworks_core::services::runs::control_of(s.runs(), &r))
    };
    let run_control = |fx: &Fx, ctx: &InvocationContext, op: &str| {
        let mut command = json!({"kind": op, "rootId": "feature-1",
            "expectedControlRevision": control(fx).revision.to_string()});
        if op == "runs.start" {
            command["expectedWorkRevision"] = json!("1");
        }
        let frame = json!({"protocol": "1.0-draft.1", "requestKey": format!("{}-{}", op.replace('.', "-"), fx.n.fetch_add(1, Ordering::SeqCst)),
            "command": command});
        fx.submit(ctx, op, frame.to_string().as_bytes())
            .expect("addressable")
    };
    let _ = recorded(run_control(&fx, &user, "runs.pause"));
    let started = refused(run_control(&fx, &start.ctx, "runs.start"));
    assert_eq!(started.code, DomainErrorCode::ContinueRequired);
    // Work it starts is linked to the run as provenance only.
    let (rid, r) = (run["id"].as_str().map(id).unwrap(), root.clone());
    fx.write(move |tx| discovery::link_root(tx, &rid, &r));
    assert_eq!(
        fx.inspect(&s(&job, "id"), json!({}))["run"]["linkedRootIds"],
        json!(["feature-1"])
    );
}

// ---------------------------------------------------------------------------------------------
// Discovery checkpoints, action identities and usage (JOB-06, JOB-09)

#[test]
fn repeated_scans_and_retries_never_repeat_an_action() {
    let fx = Fx::new();
    let user = fx.user();
    let p = fx.project("P");
    let def = seed_prompt(&fx, Some(&p.id));
    let job = fx.job(
        &user,
        json!({"scope": {"kind": "Project", "projectId": p.id.0.as_str()},
        "title": "Issue scan", "schedule": every(900), "definition": def, "enabled": true}),
    );
    let pipeline: Arc<dyn InferenceJobs> = Arc::new(Pipeline {
        available: true,
        ..Pipeline::default()
    });
    let run = value_of(&applied(fx.command(
        &user,
        "jobs.run_now",
        json!({"jobId": s(&job, "id"), "expectedRevision": "1"}),
    )));
    let run_id = id(&s(&run, "id"));
    fx.execute(run_id.as_str(), &pipeline).unwrap();
    let keys: Vec<String> = ["github:org/repo#41:triage", "github:org/repo#42:triage"]
        .map(String::from)
        .to_vec();
    let (r, k) = (run_id.clone(), keys.clone());
    let (claims, checkpoint) = fx.write(move |tx| {
        discovery::scan(
            tx,
            &r,
            "github:org/repo/issues",
            None,
            json!({"since": 42}),
            &k,
        )
    });
    assert!(claims.iter().all(|(_, c)| matches!(c, ActionClaim::New(_))));
    assert_eq!(checkpoint.revision, 1);
    // #41 is done; #42 was interrupted between its effect and its settle.
    let r = run_id.clone();
    fx.write(move |tx| {
        discovery::settle_action(
            tx,
            &r,
            "github:org/repo#41:triage",
            1,
            ActionOutcome::Done,
            json!({"comment": 9}),
        )
    });
    // A repeated scan over an overlapping cursor finds both identities: #41 is never repeated,
    // #42 must be reconciled first.
    let (r, k) = (run_id.clone(), keys.clone());
    let (again, _) = fx.write(move |tx| {
        discovery::scan(
            tx,
            &r,
            "github:org/repo/issues",
            Some(1),
            json!({"since": 42}),
            &k,
        )
    });
    assert!(matches!(again[0].1, ActionClaim::AlreadyDone(_)));
    assert!(matches!(again[1].1, ActionClaim::NeedsReconciliation(_)));
    // A stale checkpoint revision claims nothing and moves nothing.
    let (r, k) = (run_id.clone(), vec!["github:org/repo#43:triage".to_owned()]);
    let stale = run_write(&*fx.store, WriteOptions::default(), move |tx| {
        discovery::scan(
            tx,
            &r,
            "github:org/repo/issues",
            Some(1),
            json!({"since": 43}),
            &k,
        )
    });
    assert!(
        matches!(stale, Err(StorageError::RevisionConflict(_))),
        "{stale:?}"
    );
    let j = id(&s(&job, "id"));
    assert!(
        fx.read(move |s| s.jobs().action(&j, "github:org/repo#43:triage"))
            .is_none()
    );
    // Usage is reported per run with exact totals and no cap; a finished run takes no more.
    for (n, amount, kind) in [
        (1, Some("0.000001"), "Reported"),
        (2, None, "Untracked"),
        (3, Some("1.5"), "Estimated"),
    ] {
        let u = StoredJobUsage {
            run_id: run_id.clone(),
            seq: 0,
            request_id: id(&format!("req-{n}")),
            purpose: "Conversation".into(),
            model_id: id("opus"),
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            amount_usd: amount.map(str::to_owned),
            price_kind: kind.into(),
            price_origin: if amount.is_some() {
                "ProviderReported".into()
            } else {
                "Untracked".into()
            },
            price_revision: None,
            incomplete: false,
            recorded_at_ms: T0,
        };
        fx.write(move |tx| discovery::record_usage(tx, &u));
    }
    let usage = &fx.inspect(
        &s(&job, "id"),
        json!({"runId": run_id.as_str(), "usage": {"limit": 2}}),
    )["run"]["usage"];
    assert_eq!(usage["totalTrackedUsd"], json!("1.500001"));
    assert_eq!(usage["untrackedRequests"], json!("1"));
    assert_eq!(usage["items"].as_array().unwrap().len(), 2);
    let cursor = usage["nextCursor"].as_str().unwrap().to_owned();
    let rest = &fx.inspect(
        &s(&job, "id"),
        json!({"runId": run_id.as_str(), "usage": {"limit": 2, "cursor": cursor}}),
    )["run"]["usage"];
    assert_eq!(rest["items"][0]["requestId"], json!("req-3"));
    assert!(rest.get("nextCursor").is_none());
    let svc = fx.jobs.clone();
    let r = run_id.clone();
    let rev = fx.run(run_id.as_str()).revision;
    fx.write(move |tx| svc.finish_inference(tx, &r, rev, &InferenceOutcome::Succeeded));
    let r = run_id.clone();
    let late = run_write(&*fx.store, WriteOptions::default(), move |tx| {
        discovery::claim_action(tx, &r, "x")
    });
    assert!(matches!(late, Err(StorageError::LeaseLost { .. })));
}

// ---------------------------------------------------------------------------------------------
// Restart, interruption and resumption (JOB-03/04, CONTROL-10)

#[test]
fn reopening_resumes_interrupted_runs_with_their_identity_and_never_catches_up() {
    let fx = Fx::new();
    let user = fx.user();
    let job = fx.job(&user, body("Docs", every(3600), native(), true));
    let jid = s(&job, "id");
    // A run claimed but never dispatched, and one dispatched whose command committed.
    fx.set_now(T0 + 3_600_000 + 1_000);
    let queued = fx.claim().pop().unwrap();
    let manual_job = fx.job(
        &user,
        body("Manual docs", json!({"kind": "Manual"}), native(), true),
    );
    let manual = value_of(&applied(fx.command(
        &user,
        "jobs.run_now",
        json!({"jobId": s(&manual_job, "id"), "expectedRevision": "1"}),
    )));
    let (svc, r, p) = (fx.jobs.clone(), id(&s(&manual, "id")), none());
    let begun = fx.write(move |tx| svc.begin(tx, &r, &*p)).unwrap();
    let (svc, b) = (fx.jobs.clone(), begun.clone());
    assert!(fx.write(move |tx| svc.dispatching(tx, &b)));
    if let Step::Native {
        ctx,
        operation,
        frame,
    } = &begun.step
    {
        // The command commits but the app closes before its outcome is settled.
        assert!(matches!(
            Dispatch(&fx).command(ctx, operation, frame),
            Ok(CommandReply::Recorded { .. })
        ));
    }
    let hook = JobLifecycle(fx.jobs.clone());
    fx.write(move |tx| hook.quiesce(tx));
    assert_eq!(
        fx.stored(&jid).interrupted_run_id.as_ref(),
        Some(&queued.id)
    );
    // Closed for a day.
    fx.set_now(T0 + 27 * 3_600_000 + 10 * MINUTE);
    let fx = fx.restart();
    // The never-dispatched run resumes with the same identity (queued again, one resumption).
    let resumed = fx.run(queued.id.as_str());
    assert_eq!(
        (resumed.status.as_str(), resumed.resumptions),
        ("Queued", 1)
    );
    assert_eq!(fx.runs(&jid).len(), 1, "no new run for the interrupted one");
    // The dispatched one is settled from its command's receipt: it is not run again (this
    // boot's handler has not been called).
    let settled = fx.run(&s(&manual, "id"));
    assert_eq!(settled.status, "Succeeded");
    assert_eq!(fx.docs.calls.load(Ordering::SeqCst), 0);
    // Missed occurrences were counted, the next is in the future, and nothing caught up.
    assert!(fx.stored(&jid).next_at_ms.unwrap() > T0 + 27 * 3_600_000 + 10 * MINUTE);
    assert_eq!(
        fx.events("JobOccurrencesMissed").last().unwrap().payload["count"],
        json!(26)
    );
    assert_eq!(fx.events("JobRunResumed").len(), 1);
    // The resumed run executes once.
    assert_eq!(
        fx.execute(queued.id.as_str(), &none()).unwrap().status,
        "Succeeded"
    );
    assert_eq!(fx.docs.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_restored_backup_or_a_disabled_schedule_keeps_interrupted_runs_and_schedules_stopped() {
    let fx = Fx::new();
    let user = fx.user();
    let job = fx.job(&user, body("Docs", every(3600), native(), true));
    let jid = s(&job, "id");
    let run = value_of(&applied(fx.command(
        &user,
        "jobs.run_now",
        json!({"jobId": jid, "expectedRevision": "1"}),
    )));
    // A backup restore advances the store's restore epoch (DW-57).
    fx.store
        .with_writer_connection(|c| {
            super::super::conn::db(
                c.execute_batch("UPDATE store_meta SET restore_epoch=restore_epoch+1"),
            )
        })
        .unwrap();
    let fx = fx.restart();
    let held = fx.stored(&jid);
    assert_eq!(
        (held.enabled, held.hold.as_deref(), held.next_at_ms),
        (true, Some("ReactivationRequired"), None)
    );
    let r = fx.run(&s(&run, "id"));
    assert_eq!(r.status, "Cancelled");
    assert!(r.reason.unwrap().contains("restored"));
    fx.set_now(T0 + 10 * 3_600_000);
    assert!(fx.claim().is_empty());
    // A person reactivates it explicitly; then it schedules from now.
    let user = fx.user();
    let enabled = job_value(applied(fx.command(
        &user,
        "jobs.enable",
        json!({"jobId": jid, "expectedRevision": held.revision.to_string()}),
    )));
    assert!(enabled.get("hold").is_none());
    assert!(s(&enabled, "nextRunAtMs").parse::<i64>().unwrap() > T0 + 10 * 3_600_000);
}

// ---------------------------------------------------------------------------------------------
// Project archive and restore (DW-11)

#[test]
fn archive_stops_a_projects_schedules_at_once_and_restore_keeps_them_stopped() {
    let fx = Fx::new();
    let user = fx.user();
    let p = fx.project("P");
    let scope = json!({"kind": "Project", "projectId": p.id.0.as_str()});
    let job = fx.job(
        &user,
        json!({"scope": scope, "title": "Docs", "schedule": every(3600),
        "definition": native(), "enabled": true}),
    );
    let jid = s(&job, "id");
    let queued = value_of(&applied(fx.command(
        &user,
        "jobs.run_now",
        json!({"jobId": jid, "expectedRevision": "1"}),
    )));
    // A second job whose run was claimed and started before the archival (its dispatch has not
    // happened yet when the project closes).
    let other = fx.job(
        &user,
        json!({"scope": scope, "title": "Docs 2", "schedule": json!({"kind": "Manual"}),
        "definition": native(), "enabled": true}),
    );
    let started = value_of(&applied(fx.command(
        &user,
        "jobs.run_now",
        json!({"jobId": s(&other, "id"), "expectedRevision": "1"}),
    )));
    let (svc, r, pl) = (fx.jobs.clone(), id(&s(&started, "id")), none());
    let begun = fx.write(move |tx| svc.begin(tx, &r, &*pl)).unwrap();
    fx.lifecycle("projects.archive", &p.id);
    // In the archival's unit of work: the schedule is held and the queued run cancelled.
    assert_eq!(fx.stored(&jid).hold.as_deref(), Some("ProjectClosed"));
    assert_eq!(fx.run(&s(&queued, "id")).status, "Cancelled");
    // The run started under the earlier epoch is cancelled before dispatch, never dispatched.
    let (svc, b) = (fx.jobs.clone(), begun.clone());
    assert!(!fx.write(move |tx| svc.dispatching(tx, &b)));
    assert_eq!(fx.run(&s(&started, "id")).status, "Cancelled");
    assert_eq!(fx.docs.calls.load(Ordering::SeqCst), 0);
    // A closed project refuses Run now and enable.
    assert_eq!(
        kind(&refused(fx.command(
            &user,
            "jobs.run_now",
            json!({"jobId": jid, "expectedRevision": fx.stored(&jid).revision.to_string()})
        ))),
        (DomainErrorCode::Conflict, "project_closed".into())
    );
    // Restore: the schedules stay stopped until a person reactivates each.
    fx.archive_stop_ran(&p.id);
    fx.lifecycle("projects.restore", &p.id);
    let held = fx.stored(&jid);
    assert_eq!(held.hold.as_deref(), Some("ReactivationRequired"));
    fx.set_now(T0 + 5 * 3_600_000);
    assert!(fx.claim().is_empty());
    let enabled = job_value(applied(fx.command(
        &user,
        "jobs.enable",
        json!({"jobId": jid, "expectedRevision": held.revision.to_string()}),
    )));
    assert!(enabled.get("hold").is_none() && enabled.get("nextRunAtMs").is_some());
}

#[test]
fn an_archival_since_activation_holds_the_schedule_even_without_the_hook() {
    // The epoch fence holds on its own: a job activated under an earlier archive epoch is held at
    // its next claim even if nothing else told the scheduler (an archive and restore in between).
    let fx = Fx::new();
    let user = fx.user();
    let p = fx.project("P");
    let job = fx.job(
        &user,
        json!({"scope": {"kind": "Project", "projectId": p.id.0.as_str()},
        "title": "Docs", "schedule": every(3600), "definition": native(), "enabled": true}),
    );
    let jid = id(&s(&job, "id"));
    // A run claimed and started under the earlier epoch.
    let run = value_of(&applied(fx.command(
        &user,
        "jobs.run_now",
        json!({"jobId": jid.as_str(), "expectedRevision": "1"}),
    )));
    let (svc, r, pl) = (fx.jobs.clone(), id(&s(&run, "id")), none());
    let begun = fx.write(move |tx| svc.begin(tx, &r, &*pl)).unwrap();
    let pid = p.id.clone();
    fx.write(move |tx| {
        let mut proj = tx.projects().project(&pid)?.unwrap();
        let rev = proj.revision;
        proj.archive_epoch += 1;
        proj.revision += 1;
        tx.projects().update_project(&proj, rev)
    });
    // Its dispatch is refused under the new epoch: cancelled before dispatch, nothing ran.
    let (svc, b) = (fx.jobs.clone(), begun.clone());
    assert!(!fx.write(move |tx| svc.dispatching(tx, &b)));
    assert_eq!(fx.run(&s(&run, "id")).status, "Cancelled");
    assert_eq!(fx.docs.calls.load(Ordering::SeqCst), 0);
    fx.set_now(T0 + 3_600_000 + 1_000);
    assert!(fx.claim().is_empty());
    assert_eq!(
        fx.read(move |s| s.jobs().job(&jid))
            .unwrap()
            .hold
            .as_deref(),
        Some("ReactivationRequired")
    );
}

// ---------------------------------------------------------------------------------------------
// Migration 17

#[test]
fn migration_17_is_additive_and_enforces_one_active_run() {
    let m = MIGRATIONS
        .iter()
        .find(|m| m.version == 17)
        .expect("migration 17");
    assert_eq!(
        m.checksum(),
        MIGRATION_0017_SHA256,
        "an applied migration was edited"
    );
    assert_eq!((m.minimum_reader, m.minimum_writer), (8, 14));
    assert!(!m.rebuilds_tables);
    let fx = Fx::new();
    let job = fx.job(
        &fx.user(),
        body("Docs", json!({"kind": "Manual"}), native(), true),
    );
    let run = value_of(&applied(fx.command(
        &fx.user(),
        "jobs.run_now",
        json!({"jobId": s(&job, "id"), "expectedRevision": "1"}),
    )));
    // The store itself refuses a second active run of one job and a repeated occurrence.
    let mut second = fx.run(&s(&run, "id"));
    second.id = id("run-other");
    second.occurrence_key = "manual-other".into();
    let dup = run_write(&*fx.store, WriteOptions::default(), move |tx| {
        tx.jobs().insert_run(&second)
    });
    assert!(
        matches!(
            dup,
            Err(StorageError::Duplicate {
                kind: "active_run",
                ..
            })
        ),
        "{dup:?}"
    );
    let mut repeat = fx.run(&s(&run, "id"));
    repeat.id = id("run-repeat");
    repeat.status = "Skipped".into();
    repeat.operation_id = None;
    repeat.finished_at_ms = Some(T0);
    let dup = run_write(&*fx.store, WriteOptions::default(), move |tx| {
        tx.jobs().insert_run(&repeat)
    });
    assert!(
        matches!(
            dup,
            Err(StorageError::Duplicate {
                kind: "job_occurrence",
                ..
            })
        ),
        "{dup:?}"
    );
    // A finished run never becomes active again.
    let rid = id(&s(&run, "id"));
    let r2 = rid.clone();
    fx.write(move |tx| {
        let r = tx.jobs().job_run(&r2)?.unwrap();
        devworks_core::services::jobs::runner::request_cancel(tx, &actor("user-a"), &r, "x")
    });
    let mut back = fx.run(rid.as_str());
    back.status = "Queued".into();
    back.finished_at_ms = None;
    let rev = back.revision;
    let err = run_write(&*fx.store, WriteOptions::default(), move |tx| {
        tx.jobs().update_run(&back, rev)
    });
    assert!(
        matches!(err, Err(StorageError::InvalidRecord(_))),
        "{err:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Sleep and wake (CONTROL-11)

#[test]
fn a_wall_clock_jump_while_asleep_continues_the_run_and_misses_what_was_slept_through() {
    let fx = Fx::new();
    *fx.docs.mode.lock().unwrap() = Docs::Accepted;
    let job = fx.job(&fx.user(), body("Docs", every(3600), native(), true));
    let jid = s(&job, "id");
    fx.set_now(T0 + 3_600_000 + 1_000);
    let run = fx.claim().pop().unwrap();
    let running = fx.execute(run.id.as_str(), &none()).unwrap();
    assert_eq!(running.status, "Running");
    // The machine sleeps for five hours and wakes in the same process.
    fx.set_now(T0 + 6 * 3_600_000 + 10 * MINUTE);
    assert!(fx.claim().is_empty(), "nothing catches up");
    let after = fx.run(run.id.as_str());
    assert_eq!(
        (after.status.as_str(), after.revision, after.resumptions),
        ("Running", running.revision, 0)
    );
    assert!(fx.stored(&jid).interrupted_run_id.is_none());
    assert_eq!(
        fx.runs(&jid).len(),
        1,
        "no skipped or catch-up runs were recorded"
    );
    assert_eq!(
        fx.events("JobOccurrencesMissed").last().unwrap().payload["count"],
        json!(5)
    );
    assert!(fx.stored(&jid).next_at_ms.unwrap() > T0 + 6 * 3_600_000 + 10 * MINUTE);
}

#[test]
fn a_schedule_whose_zone_left_the_platform_stops_visibly_without_blocking_claims_or_boot() {
    let fx = Fx::new();
    let user = fx.user();
    let gone = fx.job(
        &user,
        body(
            "Gone",
            calendar("0 3 * * *", "Europe/Paris"),
            native(),
            true,
        ),
    );
    let fine = fx.job(&user, body("Fine", every(3600), native(), true));
    // The platform's time-zone database no longer knows the stored zone.
    fx.store
        .with_writer_connection(|c| {
            super::super::conn::db(c.execute_batch(
                "UPDATE schedules SET schedule_json='{\"expression\":\"0 3 * * *\",\"kind\":\"Calendar\",\"timezone\":\"Gone/Zone\"}', next_at=1 WHERE title='Gone'",
            ))
        })
        .unwrap();
    fx.set_now(T0 + 3_600_000 + 1_000);
    let queued = fx.claim();
    assert_eq!(queued.len(), 1, "the other job still runs");
    assert_eq!(queued[0].job_id.as_str(), s(&fine, "id"));
    assert_eq!(fx.stored(&s(&gone, "id")).next_at_ms, None);
    assert_eq!(fx.events("JobScheduleUnavailable").len(), 1);
    // Boot recovery is not blocked either.
    let fx = fx.restart();
    assert_eq!(fx.stored(&s(&gone, "id")).next_at_ms, None);
}

#[test]
fn listing_pages_through_titles_that_contain_newlines() {
    let fx = Fx::new();
    let user = fx.user();
    for t in ["a\nb", "a\nc", "a", "b\n"] {
        fx.job(&user, body(t, every(3600), native(), false));
    }
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let mut b = json!({"scope": {"kind": "Workspace"}, "page": {"limit": 1}});
        if let Some(c) = &cursor {
            b["page"]["cursor"] = json!(c);
        }
        let page = result_value(fx.query_at(&user, "jobs.list", b, 2));
        for j in page["items"].as_array().unwrap() {
            seen.push(j["title"].as_str().unwrap().to_owned());
        }
        cursor = page["nextCursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert!(cursor.is_none(), "the listing ends: {seen:?}");
    assert_eq!(seen, ["a", "a\nb", "a\nc", "b"]);
}

// ---------------------------------------------------------------------------------------------
// Codex round 2

fn disable(fx: &Fx, job: &str) {
    let rev = fx.stored(job).revision.to_string();
    applied(fx.command(
        &fx.user(),
        "jobs.disable",
        json!({"jobId": job, "expectedRevision": rev}),
    ));
}
fn run_now(fx: &Fx, ctx: &InvocationContext, job: &str) -> DomainCommandReply {
    let rev = fx.stored(job).revision.to_string();
    fx.command(
        ctx,
        "jobs.run_now",
        json!({"jobId": job, "expectedRevision": rev}),
    )
}

/// Another principal's Run now is fenced by the epochs a person activated the job under, also
/// for a disabled job (which no restore or archival holds): after a backup restore, or an
/// archival and restore of its project, an agent cannot run it, while a person still can (their
/// Run now authorizes it now). A run authorized under an earlier epoch never starts.
#[test]
fn an_agent_cannot_run_a_job_activated_before_a_restore_or_an_archival() {
    let fx = Fx::new();
    let user = fx.user();
    let p = fx.project("P");
    fx.bind_agent("agent-p", Some(&p.id));
    let pjob = s(
        &fx.job(
            &user,
            json!({"scope": {"kind": "Project", "projectId": p.id.0.as_str()}, "title": "Docs P",
            "schedule": every(3600), "definition": native(), "enabled": true}),
        ),
        "id",
    );
    disable(&fx, &pjob);
    fx.lifecycle("projects.archive", &p.id);
    fx.archive_stop_ran(&p.id);
    fx.lifecycle("projects.restore", &p.id);
    assert!(
        fx.stored(&pjob).hold.is_none(),
        "a disabled job is not held"
    );
    let agent_p = fx.ctx("agent-p", PrincipalKind::Agent, Some(&p.id));
    assert_eq!(
        kind(&refused(run_now(&fx, &agent_p, &pjob))),
        (
            DomainErrorCode::ApprovalRequired,
            "job_authorization".into()
        )
    );

    fx.bind_agent("agent-a", None);
    let job = s(
        &fx.job(&user, body("Docs", every(3600), native(), true)),
        "id",
    );
    disable(&fx, &job);
    // A backup restore advances the store's restore epoch (DW-57).
    fx.store
        .with_writer_connection(|c| {
            super::super::conn::db(
                c.execute_batch("UPDATE store_meta SET restore_epoch=restore_epoch+1"),
            )
        })
        .unwrap();
    let fx = fx.restart();
    assert!(fx.stored(&job).hold.is_none(), "a disabled job is not held");
    assert_eq!(
        kind(&refused(run_now(&fx, &fx.agent(), &job))),
        (
            DomainErrorCode::ApprovalRequired,
            "job_authorization".into()
        )
    );
    // A run authorized under the job's (earlier) activation never starts.
    let stale = fx.stored(&job);
    let stale_run = fx.write(move |tx| {
        let host = tx.store().host_actor.clone();
        devworks_core::services::jobs::runner::queue_run(
            tx,
            &host,
            &stale,
            devworks_core::services::jobs::runner::Occurrence::Manual { at_ms: T0 },
            false,
        )
    });
    assert!(fx.execute(stale_run.id.as_str(), &none()).is_none());
    let cancelled = fx.run(stale_run.id.as_str());
    assert_eq!(cancelled.status, "Cancelled");
    assert!(cancelled.reason.unwrap().contains("restored"));
    // A person's Run now authorizes it in the current epoch, and it runs.
    let mine = value_of(&applied(run_now(&fx, &fx.user(), &job)));
    assert_eq!(
        fx.execute(&s(&mine, "id"), &none()).unwrap().status,
        "Succeeded"
    );
    assert_eq!(fx.docs.calls.load(Ordering::SeqCst), 1);
}

/// A run holding at close (an approval wait, say) is reattached at reopen with its hold: its
/// session is handed to the pipeline again, still held and owned by this boot, so its discovery
/// writes are accepted and clearing the hold continues it.
#[test]
fn a_run_holding_at_close_is_reattached_with_its_hold_and_continues_once_cleared() {
    let fx = Fx::new();
    let user = fx.user();
    let def = seed_prompt(&fx, None);
    let jid = s(
        &fx.job(
            &user,
            json!({"scope": {"kind": "Workspace"}, "title": "Scan", "schedule": {"kind": "Manual"},
            "definition": def, "enabled": true}),
        ),
        "id",
    );
    let pipeline = Arc::new(Pipeline {
        available: true,
        ..Pipeline::default()
    });
    let dyn_pipeline: Arc<dyn InferenceJobs> = pipeline.clone();
    let rid = s(&value_of(&applied(run_now(&fx, &user, &jid))), "id");
    let running = fx.execute(&rid, &dyn_pipeline).unwrap();
    let svc = fx.jobs.clone();
    let (r, rev) = (running.id.clone(), running.revision);
    fx.write(move |tx| {
        svc.finish_inference(
            tx,
            &r,
            rev,
            &InferenceOutcome::Holding("ApprovalWait".into()),
        )
    });
    let hook = JobLifecycle(fx.jobs.clone());
    fx.write(move |tx| hook.quiesce(tx));
    let fx = fx.restart();
    let interrupted = fx.run(&rid);
    assert_eq!(
        (interrupted.status.as_str(), interrupted.hold.as_deref()),
        ("Holding", Some("ApprovalWait"))
    );
    // The scheduler picks it up and reattaches its session, still holding.
    let svc = fx.jobs.clone();
    let startable: Vec<Id> = fx
        .write(move |tx| svc.runnable(tx))
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(startable, vec![id(&rid)]);
    let reattached = fx.execute(&rid, &dyn_pipeline).unwrap();
    assert_eq!(
        (reattached.status.as_str(), reattached.hold.as_deref()),
        ("Holding", Some("ApprovalWait"))
    );
    let handed = pipeline.started.lock().unwrap().last().cloned().unwrap();
    assert!(handed.resume);
    assert_eq!(handed.hold.as_deref(), Some("ApprovalWait"));
    assert_eq!(handed.session_id, running.session_id);
    // Owned by this boot: its discovery writes are accepted.
    let r = id(&rid);
    fx.write(move |tx| discovery::owned_run(tx, &r).map(|_| ()));
    // Clearing the hold continues it.
    let svc = fx.jobs.clone();
    let (r, rev) = (reattached.id.clone(), reattached.revision);
    let resumed = fx.write(move |tx| svc.finish_inference(tx, &r, rev, &InferenceOutcome::Resumed));
    assert_eq!(
        (resumed.status.as_str(), resumed.hold.as_deref()),
        ("Running", None)
    );
}

/// A start whose executor is gone (its task ended after the start committed) is reclaimed:
/// never dispatched, it is queued again; dispatched with a committed command, it is settled from
/// the command's receipt without running it again; dispatched without a receipt, it is queued
/// again for the same identity and runs once.
#[test]
fn a_start_whose_executor_is_gone_is_reclaimed_from_what_is_known_of_its_effect() {
    let fx = Fx::new();
    let user = fx.user();
    let start = |job: &str| {
        let rid = s(&value_of(&applied(run_now(&fx, &user, job))), "id");
        let (svc, r, p) = (fx.jobs.clone(), id(&rid), none());
        let begun = fx.write(move |tx| svc.begin(tx, &r, &*p)).unwrap();
        (rid, begun)
    };
    let reclaim = |rid: &str| {
        let (svc, r) = (fx.jobs.clone(), id(rid));
        fx.write(move |tx| svc.reclaim(tx, &r))
    };
    let manual = |title: &str| {
        s(
            &fx.job(
                &user,
                body(title, json!({"kind": "Manual"}), native(), true),
            ),
            "id",
        )
    };
    // Never dispatched.
    let (a, _) = start(&manual("A"));
    let svc = fx.jobs.clone();
    let listed: Vec<Id> = fx
        .write(move |tx| svc.unsettled_starts(tx))
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(listed, vec![id(&a)]);
    assert_eq!(reclaim(&a).unwrap().status, "Queued");
    assert_eq!(fx.execute(&a, &none()).unwrap().status, "Succeeded");
    assert_eq!(fx.docs.calls.load(Ordering::SeqCst), 1);
    // Dispatched, and its command committed.
    let (b, begun) = start(&manual("B"));
    let (svc, bg) = (fx.jobs.clone(), begun.clone());
    assert!(fx.write(move |tx| svc.dispatching(tx, &bg)));
    if let Step::Native {
        ctx,
        operation,
        frame,
    } = &begun.step
    {
        assert!(matches!(
            Dispatch(&fx).command(ctx, operation, frame),
            Ok(CommandReply::Recorded { .. })
        ));
    }
    assert_eq!(fx.docs.calls.load(Ordering::SeqCst), 2);
    assert_eq!(reclaim(&b).unwrap().status, "Succeeded");
    assert_eq!(fx.docs.calls.load(Ordering::SeqCst), 2, "not run again");
    // Dispatched, but its command never committed. (A retry of the dispatching step, after its
    // acknowledgment was lost, finds the attempt already dispatching under its fence.)
    let (c, begun) = start(&manual("C"));
    for _ in 0..2 {
        let (svc, bg) = (fx.jobs.clone(), begun.clone());
        assert!(fx.write(move |tx| svc.dispatching(tx, &bg)));
    }
    let requeued = reclaim(&c).unwrap();
    assert_eq!(requeued.status, "Queued");
    assert_eq!(fx.execute(&c, &none()).unwrap().status, "Succeeded");
    assert_eq!(fx.docs.calls.load(Ordering::SeqCst), 3);
    // Nothing is reclaimed twice, and a settled run is not a start.
    assert!(reclaim(&c).is_none());
}

// ---------------------------------------------------------------------------------------------
// Codex round 3

/// An app-level inference task that targets project P (its definition's `projectId`).
fn targeting(fx: &Fx, p: &ProjectId, title: &str, schedule: Value) -> String {
    let mut def = seed_prompt_once(fx);
    def["projectId"] = json!(p.0.as_str());
    s(
        &fx.job(
            &fx.user(),
            json!({"scope": {"kind": "Workspace"}, "title": title, "schedule": schedule,
            "definition": def, "enabled": true}),
        ),
        "id",
    )
}
/// The workspace prompt, seeded on first use.
fn seed_prompt_once(fx: &Fx) -> Value {
    let exists = fx
        .read(|s| s.artifacts().artifact(&id("prompt-1")))
        .is_some();
    if exists {
        json!({"kind": "Inference", "prompt": {"artifactId": "prompt-1", "versionId": "prompt-v1",
            "sha256": "a".repeat(64), "byteLength": "12", "mediaType": "text/markdown"},
            "route": {"kind": "Tier", "tier": "High"}, "effort": "Default"})
    } else {
        seed_prompt(fx, None)
    }
}

/// The archive and restore hooks cover an app-level task that targets the project, not only the
/// jobs scoped to it: archiving holds it and stops its runs at once, and restore keeps it held
/// until a person enables it.
#[test]
fn archiving_a_project_stops_app_level_tasks_that_target_it() {
    let fx = Fx::new();
    let user = fx.user();
    let p = fx.project("P");
    let jid = targeting(&fx, &p.id, "Scan P", every(3600));
    // An app-level task with no target yet, retargeted to P after the restore below.
    let later = s(
        &fx.job(
            &user,
            body("Scan later", every(3600), seed_prompt_once(&fx), true),
        ),
        "id",
    );
    let queued = s(&value_of(&applied(run_now(&fx, &user, &jid))), "id");
    assert!(
        fx.run(&queued).project_id.is_none(),
        "listed in the workspace"
    );
    fx.lifecycle("projects.archive", &p.id);
    assert_eq!(fx.stored(&jid).hold.as_deref(), Some("ProjectClosed"));
    assert_eq!(fx.run(&queued).status, "Cancelled");
    assert_eq!(
        kind(&refused(run_now(&fx, &user, &jid))),
        (DomainErrorCode::Conflict, "project_closed".into())
    );
    fx.archive_stop_ran(&p.id);
    fx.lifecycle("projects.restore", &p.id);
    assert_eq!(
        fx.stored(&jid).hold.as_deref(),
        Some("ReactivationRequired")
    );
    // Retargeting a task to P authorizes it under P's current archive epoch.
    let mut def = seed_prompt_once(&fx);
    def["projectId"] = json!(p.id.0.as_str());
    let rev = fx.stored(&later).revision.to_string();
    applied(fx.command(
        &user,
        "jobs.edit",
        json!({"jobId": later, "expectedRevision": rev, "title": "Scan later",
            "schedule": every(3600), "definition": def}),
    ));
    fx.set_now(T0 + 5 * 3_600_000);
    assert!(
        fx.claim().iter().all(|r| r.job_id.as_str() != jid),
        "held until a person enables it"
    );
    // Enabled again, and created after the restore: both run under P's current epoch.
    let rev = fx.stored(&jid).revision.to_string();
    applied(fx.command(
        &user,
        "jobs.enable",
        json!({"jobId": jid, "expectedRevision": rev}),
    ));
    let fresh = targeting(&fx, &p.id, "Scan P again", every(3600));
    // A claim holds a job whose target was archived since its activation; these stay live.
    fx.set_now(T0 + 9 * 3_600_000);
    fx.claim();
    for j in [&jid, &later, &fresh] {
        let job = fx.stored(j);
        assert!(
            job.hold.is_none() && job.next_at_ms.is_some(),
            "{j}: {:?}",
            job.hold
        );
    }
}

/// The epoch fence follows the targeted project too, without the hooks: after an archival of P
/// since a task targeting P was activated (and its run authorized), the run never dispatches and
/// the schedule is held at its next claim.
#[test]
fn an_archival_of_the_targeted_project_fences_an_app_level_task() {
    let fx = Fx::new();
    let user = fx.user();
    let p = fx.project("P");
    let jid = targeting(&fx, &p.id, "Scan P", every(3600));
    let pipeline = Arc::new(Pipeline {
        available: true,
        ..Pipeline::default()
    });
    let rid = s(&value_of(&applied(run_now(&fx, &user, &jid))), "id");
    let pl: Arc<dyn InferenceJobs> = pipeline.clone();
    let (svc, r) = (fx.jobs.clone(), id(&rid));
    let begun = fx.write(move |tx| svc.begin(tx, &r, &*pl)).unwrap();
    // Its agent acts in P, not across the workspace.
    // Work it links is P's, never another project's.
    let q = fx.project("Q");
    let sql = format!(
        "INSERT INTO work_items VALUES('feature-q','{}','feature',NULL,NULL,NULL,'F',1,0,1,1,1,'{{}}');",
        q.id.0.as_str()
    );
    fx.store
        .with_writer_connection(move |c| super::super::conn::db(c.execute_batch(&sql)))
        .unwrap();
    let (r, root) = (id(&rid), RootId(id("feature-q")));
    let linked = run_write(&*fx.store, WriteOptions::default(), move |tx| {
        discovery::link_root(tx, &r, &root)
    });
    assert!(
        matches!(linked, Err(StorageError::ScopeDenied)),
        "{linked:?}"
    );
    let agent = begun.run.agent_actor.clone().unwrap();
    let binding = fx.read(move |s| s.authority().principal(&agent)).unwrap();
    assert_eq!(binding.project_id.as_ref(), Some(&p.id));
    let pid = p.id.clone();
    fx.write(move |tx| {
        let mut proj = tx.projects().project(&pid)?.unwrap();
        let rev = proj.revision;
        proj.archive_epoch += 1;
        proj.revision += 1;
        tx.projects().update_project(&proj, rev)
    });
    let (svc, b) = (fx.jobs.clone(), begun.clone());
    assert!(!fx.write(move |tx| svc.dispatching(tx, &b)));
    assert_eq!(fx.run(&rid).status, "Cancelled");
    assert!(
        pipeline.started.lock().unwrap().is_empty(),
        "nothing was handed over"
    );
    fx.set_now(T0 + 3_600_000 + 1_000);
    assert!(fx.claim().is_empty());
    assert_eq!(
        fx.stored(&jid).hold.as_deref(),
        Some("ReactivationRequired")
    );
}

/// A pipeline that takes a run without naming a session yet (`Ok(None)`) owns it: supervision
/// never reclaims it as lost, so it keeps its active-run slot and no later occurrence overlaps.
#[test]
fn a_handoff_accepted_without_a_session_is_not_reclaimed() {
    let fx = Fx::new();
    let user = fx.user();
    let def = seed_prompt(&fx, None);
    let jid = s(
        &fx.job(
            &user,
            json!({"scope": {"kind": "Workspace"}, "title": "Scan", "schedule": {"kind": "Manual"},
            "definition": def, "enabled": true}),
        ),
        "id",
    );
    let pipeline: Arc<dyn InferenceJobs> = Arc::new(Sessionless);
    let rid = s(&value_of(&applied(run_now(&fx, &user, &jid))), "id");
    let running = fx.execute(&rid, &pipeline).unwrap();
    assert_eq!(running.status, "Running");
    assert!(running.session_id.is_none());
    let svc = fx.jobs.clone();
    assert!(fx.write(move |tx| svc.unsettled_starts(tx)).is_empty());
    let (svc, r) = (fx.jobs.clone(), id(&rid));
    assert!(fx.write(move |tx| svc.reclaim(tx, &r)).is_none());
    assert_eq!(fx.run(&rid).status, "Running");
    // Its slot is still held: the next occurrence is skipped, never run alongside it.
    let next = value_of(&applied(run_now(&fx, &user, &jid)));
    assert_eq!(s(&next, "status"), "Skipped");
}
/// Takes every run without a session.
struct Sessionless;
impl InferenceJobs for Sessionless {
    fn availability(&self) -> Result<(), String> {
        Ok(())
    }
    fn start(&self, _: &InferenceStart) -> Result<Option<Id>, InferenceRefusal> {
        Ok(None)
    }
    fn cancel(&self, _: &Id) -> Result<bool, String> {
        Ok(true)
    }
}

// ---------------------------------------------------------------------------------------------
// Codex round 4

/// An archival stops the runs whose own snapshot acts on the project, not those of the jobs
/// whose current definition does: retargeting a task from A to B while its run is active leaves
/// that run acting on A, so archiving B holds the schedule but leaves the run, and archiving A
/// stops it.
#[test]
fn an_archival_stops_runs_by_their_own_target_not_their_jobs_current_one() {
    let fx = Fx::new();
    let user = fx.user();
    let a = fx.project("A");
    let b = fx.project("B");
    let jid = targeting(&fx, &a.id, "Scan", every(3600));
    let rid = s(&value_of(&applied(run_now(&fx, &user, &jid))), "id");
    assert_eq!(fx.run(&rid).status, "Queued");
    // Retargeted to B while its run (queued against A) is active.
    let mut def = seed_prompt_once(&fx);
    def["projectId"] = json!(b.id.0.as_str());
    let rev = fx.stored(&jid).revision.to_string();
    applied(fx.command(
        &user,
        "jobs.edit",
        json!({"jobId": jid, "expectedRevision": rev, "title": "Scan",
            "schedule": every(3600), "definition": def}),
    ));
    // Archiving B holds the schedule (it now targets B) but leaves the run, which acts on A.
    fx.lifecycle("projects.archive", &b.id);
    assert_eq!(fx.stored(&jid).hold.as_deref(), Some("ProjectClosed"));
    let run = fx.run(&rid);
    assert_eq!(run.status, "Queued");
    assert!(!run.cancel_requested);
    let archived = fx.events("ProjectArchived");
    assert_eq!(
        archived.last().unwrap().payload["stoppedJobRuns"],
        json!([])
    );
    // Archiving A stops it at once, in the archival's unit of work.
    fx.lifecycle("projects.archive", &a.id);
    assert_eq!(fx.run(&rid).status, "Cancelled");
    let archived = fx.events("ProjectArchived");
    assert_eq!(
        archived.last().unwrap().payload["stoppedJobRuns"],
        json!([rid])
    );
}
