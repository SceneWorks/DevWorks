//! Versioned eval suites end to end (DW-72) through the booted runtime, the real gateway, host
//! authority, the approvals path and the SQLite store, with real suite folders in temporary
//! shared and harness roots. A suite is kept apart from what it measures, an agent's suite
//! change takes effect only through its own approved proposal, and only a person accepts an
//! exception. No inference.
mod common;
use common::*;
use devworks_adapters::fs::NativeFilesystem;
use devworks_adapters::fs::artifacts::NativeArtifactFiles;
use devworks_adapters::fs::skills::NativeSkillFiles;
use devworks_adapters::fs::sources::HostArtifactSources;
use devworks_adapters::index::NativeSourceTree;
use devworks_adapters::power::NativePower;
use devworks_adapters::process::NativeProcessHost;
use devworks_core::authority::HostAuthority;
use devworks_core::authority::service::bind_principal;
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::ports::storage::*;
use devworks_core::ports::{
    AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization, Clock, IdKind, IdSource,
};
use devworks_core::protocol::CommandReply;
use devworks_core::protocol::results::{DomainCommandReply, DomainDecision, DomainQueryReply};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::instructions::{SkillRoots, SuiteFolders};
use devworks_core::settings::catalog::AgentRole;
use devworks_engine::runtime::InstructionComposition;
use devworks_engine::{ArtifactComposition, DiagnosticComposition, KnowledgeComposition};
use devworks_engine::{Composition, RunComposition, Runtime, RuntimeConfig, boot};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The schema revision that introduces the suite operations.
const R: u32 = 23;

struct NativeAuthority {
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn Clock>,
}
impl AuthorityPort for NativeAuthority {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.clock,
            protected_branches: &[],
        }
        .authorize(ctx, request)
    }
}

struct Fx {
    h: Harness,
    store: Arc<dyn ApplicationStore>,
    artifact_ports: ArtifactComposition,
    artifacts: Arc<ArtifactService>,
    shared_skills: PathBuf,
    harness_skills: PathBuf,
    shared_evals: PathBuf,
    harness_evals: PathBuf,
}

fn write_folder(root: &Path, name: &str, files: &[(&str, &str)]) {
    for (path, text) in files {
        let path = root.join(name).join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}
fn tree(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                out.push(
                    e.path()
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    out.sort();
    out
}

/// Valid suite documentation naming the given checks.
fn documentation(checks: &[&str]) -> String {
    let mut graders = String::new();
    for c in checks {
        graders.push_str(&format!(
            "### {c}\nChecks: the recorded call succeeded.\nCannot check: whether the call was needed.\n"
        ));
    }
    format!(
        "---\ndescription: Measures skill reads\n---\n# Reads\n\n## What it measures\nWhether sessions read the skills they use.\n\n## Graders\n{graders}\n## How to run\nGrade a recorded session with evals.grade.\n\n## Reading results\nCounts with denominators; only passed and failed enter rates.\n"
    )
}
fn check(id: &str) -> Value {
    json!({"id": id, "ruleRef": "SKILL-03", "measurement": "Proxy",
        "predicate": {"kind": "ToolSucceeded", "operation": "skills.read"}})
}
fn definition(checks: &[&str]) -> String {
    serde_json::to_string_pretty(&json!({
        "measures": [{"kind": "Skill", "name": "alpha"}],
        "checks": checks.iter().map(|c| check(c)).collect::<Vec<_>>(),
    }))
    .unwrap()
}
fn instance(sequence: &str) -> Value {
    json!({"sessionId": "session-a", "sequence": sequence, "entryId": format!("entry-{sequence}"), "kind": "tool_call"})
}

impl Fx {
    fn new() -> Self {
        let h = Harness::new();
        let store = h.open();
        let shared_skills = h.root.with_extension("shared-skills");
        let harness_skills = h.root.with_extension("harness-skills");
        let shared_evals = h.root.with_extension("shared-evals");
        let harness_evals = h.root.with_extension("harness-evals");
        for dir in [
            &shared_skills,
            &harness_skills,
            &shared_evals,
            &harness_evals,
        ] {
            let _ = std::fs::remove_dir_all(dir);
        }
        std::fs::create_dir_all(&shared_skills).unwrap();
        std::fs::create_dir_all(&shared_evals).unwrap();
        let artifact_ports = ArtifactComposition::new(
            Arc::new(NativeArtifactFiles::new(
                h.root.clone(),
                h.root.with_extension("cache"),
            )),
            Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
            Arc::new(NativeFilesystem::new()),
        );
        let artifacts = h.artifact_service(&store, &artifact_ports);
        Self {
            h,
            store,
            artifact_ports,
            artifacts,
            shared_skills,
            harness_skills,
            shared_evals,
            harness_evals,
        }
    }
    fn composition(&self) -> Composition {
        let h = &self.h;
        let mut config = RuntimeConfig::new(id("evals-dispatcher"));
        config.approval_resume_interval = Duration::from_secs(3600);
        config.instructions = Some(InstructionComposition {
            source: Arc::new(NativeSourceTree::new()),
            files: Arc::new(NativeSkillFiles::new(&self.harness_skills)),
            roots: SkillRoots {
                shared: Some(self.shared_skills.clone()),
                harness: self.harness_skills.clone(),
            },
            suites: Some(SuiteFolders {
                files: Arc::new(NativeSkillFiles::new(&self.harness_evals)),
                roots: SkillRoots {
                    shared: Some(self.shared_evals.clone()),
                    harness: self.harness_evals.clone(),
                },
            }),
        });
        Composition {
            store: self.store.clone(),
            clock: h.clock(),
            ids: h.ids.clone(),
            authority: Arc::new(NativeAuthority {
                store: self.store.clone(),
                clock: h.clock(),
            }),
            artifacts: self.artifact_ports.clone(),
            process_host: Arc::new(NativeProcessHost),
            runs: RunComposition::default(),
            knowledge: KnowledgeComposition::default(),
            application: devworks_engine::ApplicationComposition {
                sessions: Some(Default::default()),
                ..Default::default()
            },
            web: None,
            power: Arc::new(NativePower),
            diagnostics: DiagnosticComposition::default(),
            browser: None,
            git: None,
            packages: None,
            use_cases: UseCases::new(),
            test_overrides: Default::default(),
            consumers: vec![],
            indexing: None,
            config,
        }
    }
    /// A person and a writable agent, bound by the person.
    fn principals(&self) -> (InvocationContext, InvocationContext) {
        let store = &*self.store;
        let user = principal(store, "user", PrincipalKind::Human);
        let agent = principal(store, "agent", PrincipalKind::Agent);
        let (u, actor) = (user.clone(), agent.principal().clone());
        write(store, move |tx| {
            bind_principal(
                tx,
                &u,
                NewPrincipalBinding {
                    actor,
                    kind: PrincipalKind::Agent,
                    role: Some(AgentRole::WorkspaceCoordinator),
                    project_id: None,
                    tool_families: None,
                    bound_by: u.principal().clone(),
                },
                None,
            )
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
        })
        .unwrap();
        let recipient = user.principal().clone();
        write(store, move |tx| {
            tx.decisions().bind_notification_user(&recipient)
        })
        .unwrap();
        (user, agent)
    }
    /// An upload the actor owns, as a client would attach draft text, in the actor's project
    /// when it works in one.
    fn upload(&self, owner: &InvocationContext, text: &str) -> Value {
        let scope = owner.project_id().map_or(ArtifactScope::Workspace, |p| {
            ArtifactScope::Project(p.clone())
        });
        let staged = self
            .artifacts
            .stage_host_bytes(&*self.store, text.as_bytes(), json!({"kind": "Test"}))
            .unwrap()
            .unwrap();
        let (artifacts, actor, s) = (
            self.artifacts.clone(),
            owner.principal().clone(),
            staged.clone(),
        );
        let version = write(&*self.store, move |tx| {
            artifacts.commit_host_bytes_in_scope(
                tx,
                &s,
                devworks_core::services::artifacts::host::HostArtifactPublication {
                    title: "draft",
                    media_type: "text/markdown",
                    provenance: json!({}),
                    scope,
                    owner: actor,
                },
            )
        })
        .unwrap();
        self.artifacts
            .settle_host_bytes(&*self.store, &staged, true);
        serde_json::to_value(devworks_core::services::artifacts::version_ref(&version)).unwrap()
    }
    fn command(
        &self,
        app: &Runtime,
        ctx: &InvocationContext,
        op: &str,
        revision: u32,
        mut body: Value,
    ) -> DomainCommandReply {
        body["kind"] = json!(op);
        let frame = serde_json::to_vec(
            &json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":revision,
            "requestKey":self.h.ids.next_id(IdKind::Link),"body":body}),
        )
        .unwrap();
        app.command(ctx, op, &frame).unwrap()
    }
    fn query(
        &self,
        app: &Runtime,
        ctx: &InvocationContext,
        op: &str,
        mut body: Value,
    ) -> Result<Value, Value> {
        body["kind"] = json!(op);
        // Skill reads keep their first revision; the suite reads exist from revision 23.
        let revision = if op.starts_with("evals.") { R } else { 1 };
        let frame = serde_json::to_vec(
            &json!({"mode":"Query","protocol":"1.0-draft.1","queryId":"q","schemaRevision":revision,"body":body}),
        )
        .unwrap();
        match app.query(ctx, op, &frame).unwrap() {
            DomainQueryReply::Result { result, .. } => {
                Ok(serde_json::to_value(result).unwrap()["value"].clone())
            }
            DomainQueryReply::Rejected { fault, .. } => Err(serde_json::to_value(fault).unwrap()),
        }
    }
    /// A complete suite draft body.
    fn draft_body(
        &self,
        author: &InvocationContext,
        name: &str,
        checks: &[&str],
        resources: &[(&str, &str)],
    ) -> Value {
        json!({
            "name": name,
            "documentation": self.upload(author, &documentation(checks)),
            "measures": [{"kind": "Skill", "name": "alpha"}],
            "checks": checks.iter().map(|c| check(c)).collect::<Vec<_>>(),
            "resources": resources.iter().map(|(p, t)| json!({"path": p, "artifact": self.upload(author, t)})).collect::<Vec<_>>(),
        })
    }
    fn suites(&self, app: &Runtime, ctx: &InvocationContext) -> Vec<Value> {
        self.query(app, ctx, "evals.suites", json!({"page": {"limit": 100}}))
            .unwrap()["items"]
            .as_array()
            .unwrap()
            .clone()
    }
    fn history(&self, app: &Runtime, ctx: &InvocationContext, suite: &Value) -> Vec<Value> {
        self.query(
            app,
            ctx,
            "evals.suite_history",
            json!({"suiteId": suite, "page": {"limit": 50}}),
        )
        .unwrap()["items"]
            .as_array()
            .unwrap()
            .clone()
    }
    fn decide(&self, proposal: &Value, verdict: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,
            "requestKey":self.h.ids.next_id(IdKind::Link),"body":{"kind":"approvals.record_human_decision",
            "proposalId":proposal["id"],"expectedRevision":proposal["revision"],"contentDigest":proposal["contentDigest"],
            "decision":verdict,"rationale":"Reviewed the exact suite"}}))
        .unwrap()
    }
}

fn applied(reply: DomainCommandReply) -> Value {
    match reply {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied { result } | DomainDecision::NoChange { result } => {
                serde_json::to_value(result).unwrap()["value"].clone()
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}
/// The refusal code of a recorded refusal or a rejected command, with its target kind.
fn refused(reply: DomainCommandReply) -> (String, String) {
    let fault = match reply {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Refused { fault } => serde_json::to_value(fault).unwrap(),
            other => panic!("expected a refusal, got {other:?}"),
        },
        other => serde_json::to_value(other).unwrap(),
    };
    (
        fault["code"].as_str().unwrap_or_default().to_owned(),
        fault["targetKind"].as_str().unwrap_or_default().to_owned(),
    )
}
fn named<'a>(list: &'a [Value], name: &str, origin: &str) -> &'a Value {
    list.iter()
        .find(|r| r["name"] == name && r["origin"] == origin)
        .unwrap_or_else(|| panic!("{name} ({origin}) in {list:?}"))
}

#[test]
fn shared_and_harness_suites_are_versioned_with_origin_location_and_active_revision() {
    let fx = Fx::new();
    write_folder(
        &fx.shared_evals,
        "reads",
        &[
            ("SUITE.md", &documentation(&["read-ok"])),
            ("suite.json", &definition(&["read-ok"])),
            ("cases/session-a.json", "{}"),
            ("graders/read-ok.md", "Checks the call."),
            ("rubrics/judge.md", "Rubric."),
            ("fixtures/store.json", "{}"),
        ],
    );
    // A folder whose documentation lacks a section is not a suite revision.
    write_folder(
        &fx.shared_evals,
        "broken",
        &[
            (
                "SUITE.md",
                &documentation(&["x"]).replace("## How to run", "## Usage"),
            ),
            ("suite.json", &definition(&["x"])),
        ],
    );
    let shared_before = tree(&fx.shared_evals);
    runtime().block_on(async {
        let (user, _) = fx.principals();
        let app = boot(fx.composition()).await.unwrap();
        // The shared suite is found in place, waiting for a person's confirmation.
        let listed = fx.suites(&app, &user);
        assert!(!listed.iter().any(|r| r["name"] == "broken"), "{listed:?}");
        let shared = named(&listed, "reads", "Shared").clone();
        assert_eq!(
            shared["location"],
            json!({"rootId": "evals-shared", "relative": "reads"})
        );
        assert_eq!(shared["state"], "Draft");
        assert!(shared["activeRevision"].is_null());
        assert_eq!(shared["latestRevision"], "1");
        assert_eq!(shared["description"], "Measures skill reads");
        assert_eq!(
            shared["measures"],
            json!([{"kind": "Skill", "name": "alpha"}])
        );
        assert_eq!(shared["checks"][0]["id"], "read-ok");
        let parts: Vec<(String, String)> = shared["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["path"].as_str().unwrap().to_owned(),
                    r["part"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(
            parts,
            [
                ("cases/session-a.json".to_owned(), "Case".to_owned()),
                ("fixtures/store.json".to_owned(), "Fixture".to_owned()),
                ("graders/read-ok.md".to_owned(), "Grader".to_owned()),
                ("rubrics/judge.md".to_owned(), "Rubric".to_owned()),
            ]
        );
        // The person confirms it: revision 1 becomes active.
        let active = applied(fx.command(
            &app,
            &user,
            "evals.activate_suite",
            R,
            json!({"suiteId": shared["id"], "expectedRevision": "1", "targetRevision": "1"}),
        ));
        assert_eq!(active["activeRevision"], "1");
        assert_eq!(active["approved"], true);
        assert_eq!(
            tree(&fx.shared_evals),
            shared_before,
            "shared suites are never written"
        );

        // A person creates a harness suite: it is approved, active and written to its folder.
        let created = applied(fx.command(
            &app,
            &user,
            "evals.draft_suite",
            R,
            fx.draft_body(
                &user,
                "writes",
                &["write-ok"],
                &[("cases/one.json", "{\"case\":1}")],
            ),
        ));
        assert_eq!(created["origin"], "Harness");
        assert_eq!(
            created["location"],
            json!({"rootId": "evals-harness", "relative": "writes"})
        );
        assert_eq!(created["activeRevision"], "1");
        assert_eq!(
            tree(&fx.harness_evals.join("writes")),
            ["SUITE.md", "cases/one.json", "suite.json"]
        );
        // A second save is revision 2; history keeps both, newest first; a restore of revision 1
        // makes it active again without rewriting history.
        let second = applied(
            fx.command(
                &app,
                &user,
                "evals.draft_suite",
                R,
                json!({"suiteId": created["id"], "expectedRevision": "1"})
                    .as_object()
                    .unwrap()
                    .clone()
                    .into_iter()
                    .chain(
                        fx.draft_body(&user, "writes", &["write-ok", "write-safe"], &[])
                            .as_object()
                            .unwrap()
                            .clone(),
                    )
                    .collect::<serde_json::Map<_, _>>()
                    .into(),
            ),
        );
        assert_eq!(second["revision"], "2");
        assert_eq!(second["checks"].as_array().unwrap().len(), 2);
        assert_eq!(
            tree(&fx.harness_evals.join("writes")),
            ["SUITE.md", "suite.json"]
        );
        let history: Vec<Value> = fx
            .history(&app, &user, &created["id"])
            .iter()
            .map(|r| r["revision"].clone())
            .collect();
        assert_eq!(history, [json!("2"), json!("1")]);
        let restored = applied(fx.command(
            &app,
            &user,
            "evals.activate_suite",
            R,
            json!({"suiteId": created["id"], "expectedRevision": "2", "targetRevision": "1"}),
        ));
        assert_eq!(restored["activeRevision"], "1");
        assert_eq!(restored["latestRevision"], "2");
        let read = fx
            .query(
                &app,
                &user,
                "evals.suite",
                json!({"suiteId": created["id"]}),
            )
            .unwrap();
        assert_eq!(read["revision"], "1");
        assert_eq!(
            tree(&fx.harness_evals.join("writes")),
            ["SUITE.md", "cases/one.json", "suite.json"]
        );
        // Suites are kept apart from the subjects they measure: no skill was written or listed.
        assert!(tree(&fx.harness_skills).is_empty());
        let skills = fx
            .query(
                &app,
                &user,
                "skills.discover",
                json!({"scope": {"kind": "Workspace"}, "rootIds": [], "page": {"limit": 100}}),
            )
            .unwrap();
        assert_eq!(skills["items"], json!([]), "suites are not skills");
        app.shutdown().await;
    });
}

#[test]
fn an_agent_suite_change_takes_effect_only_through_its_own_approved_proposal() {
    let fx = Fx::new();
    // A skill folder that bundles its own measurement.
    write_folder(
        &fx.shared_skills,
        "alpha",
        &[
            ("SKILL.md", "Do alpha."),
            ("evals/SUITE.md", &documentation(&["read-ok"])),
            ("evals/suite.json", &definition(&["read-ok"])),
        ],
    );
    runtime().block_on(async {
        let (user, agent) = fx.principals();
        let app = boot(fx.composition()).await.unwrap();
        // An agent drafts a suite: recorded, not active, not written.
        let draft = applied(fx.command(&app, &agent, "evals.draft_suite", R, fx.draft_body(&agent, "reads", &["read-ok"], &[("rubrics/judge.md", "Rubric.")])));
        assert_eq!(draft["state"], "Draft");
        assert_eq!(draft["approved"], false);
        assert!(draft["activeRevision"].is_null());
        assert!(!fx.harness_evals.join("reads").exists());
        // Activating it directly needs the exact human approval.
        let direct = fx.command(&app, &agent, "evals.activate_suite", R, json!({"suiteId": draft["id"], "expectedRevision": "1", "targetRevision": "1"}));
        assert!(!matches!(&direct, CommandReply::Recorded { receipt, .. } if matches!(receipt.decision, DomainDecision::Applied { .. })), "{direct:?}");
        assert!(fx.query(&app, &user, "evals.suite", json!({"suiteId": draft["id"]})).unwrap()["activeRevision"].is_null());
        // Its own proposal, approved by a person, activates exactly that revision.
        let proposal = applied(fx.command(&app, &agent, "evals.propose_suite", R, json!({"suiteId": draft["id"], "expectedRevision": "1", "contentDigest": draft["contentDigest"]})));
        assert_eq!(proposal["state"], "Pending");
        assert_eq!(proposal["operationKind"], "evals.activate_suite");
        applied(app.record_human_decision(&user, &fx.decide(&proposal, "Approve")).unwrap());
        app.approval_resumes().tick().await.unwrap();
        let active = fx.query(&app, &user, "evals.suite", json!({"suiteId": draft["id"]})).unwrap();
        assert_eq!(active["activeRevision"], "1", "{active}");
        assert_eq!(active["approvalOutcome"], "Approved");
        assert_eq!(tree(&fx.harness_evals.join("reads")), ["SUITE.md", "rubrics/judge.md", "suite.json"]);
        let events = read(&*fx.store, |s| s.history().page(&EventFilter::Aggregate { kind: "instruction".into(), id: draft["id"].as_str().unwrap().into() }, 0, 500));
        let kinds: Vec<_> = events.events.iter().map(|e| (e.kind.as_str(), e.actor.0.as_str().to_owned())).collect();
        assert!(kinds.contains(&("InstructionDrafted", "agent".into())), "{kinds:?}");
        assert!(kinds.contains(&("InstructionProposed", "agent".into())));
        assert!(kinds.contains(&("InstructionProposalApproved", "user".into())));
        assert!(kinds.contains(&("InstructionActivated", "agent".into())));

        // A suite never carries the subject it measures.
        let mut with_skill = fx.draft_body(&agent, "bundled", &["read-ok"], &[("alpha/SKILL.md", "Do alpha differently.")]);
        with_skill["name"] = json!("bundled");
        assert_eq!(
            refused(fx.command(&app, &agent, "evals.draft_suite", R, with_skill)),
            ("InvalidArgument".to_owned(), "eval_suite_carries_subject".to_owned())
        );
        // A skill proposal that carries a suite of its own is refused: one proposal never
        // changes both a subject and its measurement.
        let skills = fx.query(&app, &user, "skills.discover", json!({"scope": {"kind": "Workspace"}, "rootIds": [], "page": {"limit": 100}})).unwrap();
        let alpha = skills["items"][0].clone();
        let improved = applied(fx.command(&app, &agent, "skills.draft", 1, json!({"scope": {"kind": "Workspace"}, "instructionId": alpha["id"], "expectedRevision": "1", "name": "alpha", "content": fx.upload(&agent, "Do alpha better.")})));
        let digest = improved["applicability"].as_array().unwrap().iter().find_map(|l| l.as_str().unwrap().strip_prefix("digest:").map(str::to_owned)).unwrap();
        assert_eq!(
            refused(fx.command(&app, &agent, "skills.propose", 1, json!({"instructionId": improved["id"], "expectedRevision": "1", "contentDigest": digest}))),
            ("InvalidArgument".to_owned(), "skill_carries_eval_suite".to_owned())
        );
        // A draft whose documentation omits what a grader cannot check is refused.
        let mut undocumented = fx.draft_body(&agent, "terse", &["read-ok"], &[]);
        undocumented["documentation"] = fx.upload(&agent, &documentation(&["read-ok"]).replace("Cannot check: whether the call was needed.\n", ""));
        assert_eq!(
            refused(fx.command(&app, &agent, "evals.draft_suite", R, undocumented)).0,
            "InvalidArgument"
        );
        app.shutdown().await;
    });
}

#[test]
fn only_a_person_accepts_an_exception_and_it_records_check_instance_reason_and_approver() {
    let fx = Fx::new();
    write_folder(
        &fx.shared_evals,
        "shared-reads",
        &[
            ("SUITE.md", &documentation(&["read-ok"])),
            ("suite.json", &definition(&["read-ok"])),
        ],
    );
    let shared_before = tree(&fx.shared_evals);
    runtime().block_on(async {
        let (user, agent) = fx.principals();
        let app = boot(fx.composition()).await.unwrap();
        let suite = applied(fx.command(&app, &user, "evals.draft_suite", R, fx.draft_body(&user, "reads", &["read-ok"], &[])));
        let accept = |who: &InvocationContext, expected: &str, check: &str, seq: &str| {
            fx.command(&app, who, "evals.accept_exception", R, json!({"suiteId": suite["id"], "expectedRevision": expected, "checkId": check, "matchedInstance": instance(seq), "reason": "The read was cached by design."}))
        };
        // An agent cannot accept an exception.
        // Host authority refuses it before the owner (the operation is human-only).
        assert_eq!(refused(accept(&agent, "1", "read-ok", "7")).0, "PermissionDenied");
        assert!(fx.query(&app, &user, "evals.suite", json!({"suiteId": suite["id"]})).unwrap()["exceptions"].as_array().unwrap().is_empty());
        // A person accepts one: a new approved, active revision records it with its approver.
        let accepted = applied(accept(&user, "1", "read-ok", "7"));
        assert_eq!(accepted["revision"], "2");
        assert_eq!(accepted["activeRevision"], "2");
        assert_eq!(accepted["approved"], true);
        let exception = &accepted["exceptions"][0];
        assert_eq!(exception["checkId"], "read-ok");
        assert_eq!(exception["matchedInstance"], instance("7"));
        assert_eq!(exception["reason"], "The read was cached by design.");
        assert_eq!(exception["approver"], "user");
        assert_eq!(tree(&fx.harness_evals.join("reads")), ["SUITE.md", "exceptions.json", "suite.json"]);
        let file: Value = serde_json::from_slice(&std::fs::read(fx.harness_evals.join("reads/exceptions.json")).unwrap()).unwrap();
        assert_eq!(file[0]["approver"], "user");
        // An unknown check and a second exception for the same instance are refused.
        assert_eq!(refused(accept(&user, "2", "missing", "8")).0, "NotFound");
        assert_eq!(refused(accept(&user, "2", "read-ok", "7")).0, "Conflict");
        // An agent's later draft keeps the accepted exception unchanged.
        let edit = applied(fx.command(&app, &agent, "evals.draft_suite", R, json!({"suiteId": suite["id"], "expectedRevision": "2"}).as_object().unwrap().clone().into_iter().chain(fx.draft_body(&agent, "reads", &["read-ok"], &[]).as_object().unwrap().clone()).collect::<serde_json::Map<_, _>>().into()));
        assert_eq!(edit["exceptions"], accepted["exceptions"]);
        // An exception written into the folder by hand is observed, but never confirmed.
        std::fs::write(fx.harness_evals.join("reads/exceptions.json"), "[]\n").unwrap();
        let history = fx.history(&app, &user, &suite["id"]);
        let observed = &history[0];
        assert_eq!(observed["revision"], "4", "{history:?}");
        assert!(observed["exceptions"].as_array().unwrap().is_empty());
        assert_eq!(
            refused(fx.command(&app, &user, "evals.activate_suite", R, json!({"suiteId": suite["id"], "expectedRevision": "4", "targetRevision": "4"}))),
            ("InvalidArgument".to_owned(), "eval_exceptions_changed".to_owned())
        );
        // An exception to a shared suite goes to a same-name harness suite; the shared folder
        // stays as it was.
        let shared = named(&fx.suites(&app, &user), "shared-reads", "Shared").clone();
        applied(fx.command(&app, &user, "evals.activate_suite", R, json!({"suiteId": shared["id"], "expectedRevision": "1", "targetRevision": "1"})));
        let over = applied(fx.command(&app, &user, "evals.accept_exception", R, json!({"suiteId": shared["id"], "expectedRevision": "1", "checkId": "read-ok", "matchedInstance": instance("3"), "reason": "Known gap."})));
        assert_eq!(over["origin"], "Harness");
        assert_eq!(over["name"], "shared-reads");
        assert_eq!(over["exceptions"][0]["approver"], "user");
        assert_eq!(tree(&fx.shared_evals), shared_before);
        assert!(fx.harness_evals.join("shared-reads/exceptions.json").exists());
        app.shutdown().await;
    });
}

/// An agent bound to `project` (created here), working in it.
fn project_agent(
    fx: &Fx,
    user: &InvocationContext,
    name: &str,
    project: &str,
) -> InvocationContext {
    use devworks_core::protocol::ProjectId;
    let agent = principal(&*fx.store, name, PrincipalKind::Agent);
    let (u, actor, p) = (
        user.clone(),
        agent.principal().clone(),
        ProjectId(id(project)),
    );
    let (pid, title) = (p.clone(), project.to_owned());
    write(&*fx.store, move |tx| {
        tx.projects().insert_project(&StoredProject {
            id: pid.clone(),
            name: title.clone(),
            description: String::new(),
            lifecycle: "Active".into(),
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 0,
            archive_stop_pending: false,
        })?;
        bind_principal(
            tx,
            &u,
            NewPrincipalBinding {
                actor,
                kind: PrincipalKind::Agent,
                role: Some(AgentRole::Worker),
                project_id: Some(pid),
                tool_families: None,
                bound_by: u.principal().clone(),
            },
            None,
        )
        .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    })
    .unwrap();
    agent.with_lineage(Some(p), None, None)
}

#[test]
fn a_project_agents_suite_draft_is_confined_to_its_project_until_a_person_approves_it() {
    let fx = Fx::new();
    runtime().block_on(async {
        let (user, _) = fx.principals();
        let app = boot(fx.composition()).await.unwrap();
        let a = project_agent(&fx, &user, "agent-a", "project-a");
        let b = project_agent(&fx, &user, "agent-b", "project-b");
        let scope_of = |artifact: &Value| {
            let artifact = id(artifact["artifactId"].as_str().unwrap());
            read(&*fx.store, |s| s.artifacts().artifact(&artifact)).unwrap().scope
        };
        let draft = applied(fx.command(&app, &a, "evals.draft_suite", R, fx.draft_body(&a, "reads", &["read-ok"], &[("cases/one.json", "{\"secret\":1}")])));
        let in_a = ArtifactScope::Project(devworks_core::protocol::ProjectId(id("project-a")));
        assert_eq!(scope_of(&draft["documentation"]), in_a);
        assert_eq!(scope_of(&draft["resources"][0]["artifact"]), in_a);
        // Another project neither lists nor reads it.
        assert!(!fx.suites(&app, &b).iter().any(|r| r["id"] == draft["id"]));
        let hidden = fx.query(&app, &b, "evals.suite", json!({"suiteId": draft["id"]})).unwrap_err();
        assert_eq!(hidden["code"], "NotFound");
        // Approved by a person, every file of the revision is released to the workspace.
        let proposal = applied(fx.command(&app, &a, "evals.propose_suite", R, json!({"suiteId": draft["id"], "expectedRevision": "1", "contentDigest": draft["contentDigest"]})));
        applied(app.record_human_decision(&user, &fx.decide(&proposal, "Approve")).unwrap());
        app.approval_resumes().tick().await.unwrap();
        let seen = fx.query(&app, &b, "evals.suite", json!({"suiteId": draft["id"]})).unwrap();
        assert_eq!(seen["activeRevision"], "1", "{seen}");
        assert_eq!(seen["contentDigest"], draft["contentDigest"], "the same approved files");
        assert_eq!(scope_of(&seen["documentation"]), ArtifactScope::Workspace);
        assert_eq!(scope_of(&seen["resources"][0]["artifact"]), ArtifactScope::Workspace);
        assert_eq!(std::fs::read(fx.harness_evals.join("reads/cases/one.json")).unwrap(), b"{\"secret\":1}");
        // A later confined draft by project A is not counted in what project B sees.
        let edit = applied(fx.command(&app, &a, "evals.draft_suite", R, json!({"suiteId": draft["id"], "expectedRevision": "1"}).as_object().unwrap().clone().into_iter().chain(fx.draft_body(&a, "reads", &["read-ok"], &[]).as_object().unwrap().clone()).collect::<serde_json::Map<_, _>>().into()));
        assert_eq!(edit["latestRevision"], "2");
        let from_b = fx.query(&app, &b, "evals.suite", json!({"suiteId": draft["id"]})).unwrap();
        assert_eq!(from_b["latestRevision"], "1");
        assert_eq!(fx.history(&app, &b, &draft["id"]).len(), 1);
        app.shutdown().await;
    });
}

#[test]
fn exceptions_never_reach_an_active_suite_except_through_a_persons_acceptance() {
    let fx = Fx::new();
    // Someone wrote an exception into a shared suite folder by hand.
    let forged = serde_json::to_string(
        &json!([{"checkId": "read-ok", "matchedInstance": instance("1"),
        "reason": "trust me", "approver": "user", "acceptedAtMs": "1"}]),
    )
    .unwrap();
    write_folder(
        &fx.shared_evals,
        "forged",
        &[
            ("SUITE.md", &documentation(&["read-ok"])),
            ("suite.json", &definition(&["read-ok"])),
            ("exceptions.json", &forged),
        ],
    );
    runtime().block_on(async {
        let (user, agent) = fx.principals();
        let app = boot(fx.composition()).await.unwrap();
        // An agent's draft over the unconfirmed folder keeps no exception, so approving its
        // proposal activates none.
        let shared = named(&fx.suites(&app, &user), "forged", "Shared").clone();
        assert_eq!(shared["exceptions"].as_array().unwrap().len(), 1, "the folder is shown as it is");
        let draft = applied(fx.command(&app, &agent, "evals.draft_suite", R, json!({"suiteId": shared["id"], "expectedRevision": "1"}).as_object().unwrap().clone().into_iter().chain(fx.draft_body(&agent, "forged", &["read-ok"], &[]).as_object().unwrap().clone()).collect::<serde_json::Map<_, _>>().into()));
        assert!(draft["exceptions"].as_array().unwrap().is_empty(), "{draft}");
        let proposal = applied(fx.command(&app, &agent, "evals.propose_suite", R, json!({"suiteId": draft["id"], "expectedRevision": "1", "contentDigest": draft["contentDigest"]})));
        applied(app.record_human_decision(&user, &fx.decide(&proposal, "Approve")).unwrap());
        app.approval_resumes().tick().await.unwrap();
        let active = fx.query(&app, &user, "evals.suite", json!({"suiteId": draft["id"]})).unwrap();
        assert_eq!(active["activeRevision"], "1");
        assert!(active["exceptions"].as_array().unwrap().is_empty());

        // An agent's draft overtaken by an exception a person accepted is never activated, so
        // the person's exception is not dropped.
        let suite = applied(fx.command(&app, &user, "evals.draft_suite", R, fx.draft_body(&user, "reads", &["read-ok"], &[])));
        let stale = applied(fx.command(&app, &agent, "evals.draft_suite", R, json!({"suiteId": suite["id"], "expectedRevision": "1"}).as_object().unwrap().clone().into_iter().chain(fx.draft_body(&agent, "reads", &["read-ok"], &[]).as_object().unwrap().clone()).collect::<serde_json::Map<_, _>>().into()));
        let accepted = applied(fx.command(&app, &user, "evals.accept_exception", R, json!({"suiteId": suite["id"], "expectedRevision": "2", "checkId": "read-ok", "matchedInstance": instance("5"), "reason": "Cached by design."})));
        assert_eq!(accepted["revision"], "3");
        let proposal = applied(fx.command(&app, &agent, "evals.propose_suite", R, json!({"suiteId": suite["id"], "expectedRevision": "2", "contentDigest": stale["contentDigest"]})));
        applied(app.record_human_decision(&user, &fx.decide(&proposal, "Approve")).unwrap());
        app.approval_resumes().tick().await.unwrap();
        let now = fx.query(&app, &user, "evals.suite", json!({"suiteId": suite["id"]})).unwrap();
        assert_eq!(now["activeRevision"], "3", "{now}");
        assert_eq!(now["exceptions"][0]["reason"], "Cached by design.");
        app.shutdown().await;
    });
}
