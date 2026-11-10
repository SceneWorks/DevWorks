//! Skills and role instructions end to end (DW-034) through the booted runtime, the real gateway,
//! host authority, the approvals path and the SQLite store, with real skill directories in a
//! temporary root. Shared skills are read in place and never written; agent content takes effect
//! only through the exact human approval. No inference.
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
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::instructions::SkillRoots;
use devworks_core::services::{
    UseCases, diagnostics::DiagnosticService, resources::ResourceService, runs::ControlService,
    sessions::SessionService,
};
use devworks_core::settings::catalog::AgentRole;
use devworks_engine::runtime::InstructionComposition;
use devworks_engine::{Composition, RunComposition, Runtime, RuntimeConfig, boot};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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
    artifacts: Arc<ArtifactService>,
    shared: PathBuf,
    harness: PathBuf,
}

fn write_skill(root: &Path, name: &str, files: &[(&str, &str)]) {
    for (path, text) in files {
        let path = root.join(name).join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}
fn tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                out.push((
                    e.path()
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    std::fs::read(e.path()).unwrap(),
                ));
            }
        }
    }
    out.sort();
    out
}

impl Fx {
    fn new() -> Self {
        let h = Harness::new();
        let store = h.open();
        let shared = h.root.with_extension("shared-skills");
        let harness = h.root.with_extension("harness-skills");
        let _ = std::fs::remove_dir_all(&shared);
        let _ = std::fs::remove_dir_all(&harness);
        std::fs::create_dir_all(&shared).unwrap();
        let artifacts = Arc::new(ArtifactService::new(
            Arc::new(NativeArtifactFiles::new(
                h.root.clone(),
                h.root.with_extension("cache"),
            )),
            Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
            Arc::new(NativeFilesystem::new()),
            h.clock(),
            h.ids.clone(),
        ));
        Self {
            h,
            store,
            artifacts,
            shared,
            harness,
        }
    }
    fn composition(&self) -> Composition {
        let h = &self.h;
        let mut config = RuntimeConfig::new(id("skills-dispatcher"));
        config.approval_resume_interval = Duration::from_secs(3600);
        config.instructions = Some(InstructionComposition {
            source: Arc::new(NativeSourceTree::new()),
            files: Arc::new(NativeSkillFiles::new(&self.harness)),
            roots: SkillRoots {
                shared: Some(self.shared.clone()),
                harness: self.harness.clone(),
            },
        });
        Composition {
            store: self.store.clone(),
            clock: h.clock(),
            ids: h.ids.clone(),
            authority: Arc::new(NativeAuthority {
                store: self.store.clone(),
                clock: h.clock(),
            }),
            artifacts: self.artifacts.clone(),
            resources: Arc::new(ResourceService::new(h.ids.clone(), h.clock())),
            process_host: Arc::new(NativeProcessHost),
            runs: RunComposition::new(Arc::new(ControlService::new(h.ids.clone()))),
            power: Arc::new(NativePower),
            diagnostics: Arc::new(DiagnosticService::new(
                self.store.clone(),
                h.ids.clone(),
                h.clock(),
                None,
            )),
            browser: None,
            git: None,
            packages: None,
            use_cases: UseCases::new().with_sessions(Arc::new(SessionService::new(
                h.ids.clone(),
                self.store.clone(),
            ))),
            consumers: vec![],
            indexing: None,
            config,
        }
    }
    /// A human, a writable agent and a read-only reviewer, bound by the human.
    fn principals(&self) -> (InvocationContext, InvocationContext, InvocationContext) {
        let store = &*self.store;
        let user = principal(store, "user", PrincipalKind::Human);
        let agent = principal(store, "agent", PrincipalKind::Agent);
        let reviewer = principal(store, "reviewer", PrincipalKind::Agent);
        for (who, role) in [
            (&agent, AgentRole::WorkspaceCoordinator),
            (&reviewer, AgentRole::CodeReviewer),
        ] {
            let u = user.clone();
            let actor = who.principal().clone();
            write(store, move |tx| {
                bind_principal(
                    tx,
                    &u,
                    NewPrincipalBinding {
                        actor,
                        kind: PrincipalKind::Agent,
                        role: Some(role),
                        project_id: None,
                        tool_families: None,
                        bound_by: u.principal().clone(),
                    },
                    None,
                )
                .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
            })
            .unwrap();
        }
        let recipient = user.principal().clone();
        write(store, move |tx| {
            tx.decisions().bind_notification_user(&recipient)
        })
        .unwrap();
        (user, agent, reviewer)
    }
    /// An upload the actor owns, as a client would attach draft text.
    fn upload(&self, owner: &InvocationContext, text: &str) -> Value {
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
                    scope: ArtifactScope::Workspace,
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
        mut body: Value,
    ) -> DomainCommandReply {
        body["kind"] = json!(op);
        let frame = serde_json::to_vec(
            &json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":1,
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
        let frame = serde_json::to_vec(
            &json!({"mode":"Query","protocol":"1.0-draft.1","queryId":"q","schemaRevision":1,"body":body}),
        )
        .unwrap();
        match app.query(ctx, op, &frame).unwrap() {
            DomainQueryReply::Result { result, .. } => {
                Ok(serde_json::to_value(result).unwrap()["value"].clone())
            }
            DomainQueryReply::Rejected { fault, .. } => Err(serde_json::to_value(fault).unwrap()),
        }
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
/// The refusal code of a recorded refusal or a rejected command.
fn refused(reply: DomainCommandReply) -> String {
    match reply {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Refused { fault } => serde_json::to_value(fault).unwrap()["code"]
                .as_str()
                .unwrap()
                .to_owned(),
            other => panic!("expected a refusal, got {other:?}"),
        },
        // Refused before the use case (host authority): the rejection carries the code.
        other => serde_json::to_value(other).unwrap()["code"]
            .as_str()
            .unwrap()
            .to_owned(),
    }
}
fn labels(record: &Value) -> Vec<String> {
    record["applicability"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_owned())
        .collect()
}
fn find<'a>(page: &'a Value, name: &str, origin: &str) -> &'a Value {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name && labels(r).contains(&format!("origin:{origin}")))
        .unwrap_or_else(|| panic!("{name} ({origin}) in {page}"))
}
fn discover(fx: &Fx, app: &Runtime, ctx: &InvocationContext) -> Value {
    fx.query(
        app,
        ctx,
        "skills.discover",
        json!({"scope":{"kind":"Workspace"},"rootIds":[],"page":{"limit":100}}),
    )
    .unwrap()
}
fn setting(store: &dyn ApplicationStore, role: AgentRole) -> String {
    read(store, |s| {
        Ok(
            devworks_core::settings::service::role_instructions(s.configuration(), role, None)
                .map(|e| e.text)
                .unwrap(),
        )
    })
}

#[test]
fn shared_skills_are_discovered_in_place_and_harness_skills_take_precedence() {
    let fx = Fx::new();
    write_skill(
        &fx.shared,
        "alpha",
        &[
            (
                "SKILL.md",
                "---\nname: alpha\ndescription: Alpha skill\n---\nDo alpha.",
            ),
            ("scripts/run.sh", "#!/bin/sh\necho alpha\n"),
            ("docs/ref.md", "reference"),
        ],
    );
    write_skill(&fx.shared, "beta", &[("SKILL.md", "shared beta")]);
    write_skill(&fx.harness, "beta", &[("SKILL.md", "harness beta")]);
    write_skill(
        &fx.shared,
        "notaskill",
        &[("README.md", "no SKILL.md here")],
    );
    let shared_before = tree(&fx.shared);
    runtime().block_on(async {
        let (user, agent, _) = fx.principals();
        let app = boot(fx.composition()).await.unwrap();
        assert!(app.instruction_boot_sync().unwrap().as_ref().unwrap().failures.is_empty());
        let page = discover(&fx, &app, &agent);
        assert_eq!(page["items"].as_array().unwrap().len(), 3, "{page}");
        let alpha = find(&page, "alpha", "shared");
        assert!(labels(alpha).contains(&"effective".into()));
        assert!(labels(alpha).contains(&"active".into()));
        assert_eq!(alpha["sourcePath"], json!({"rootId":"skills-shared","relative":"alpha"}));
        assert_eq!(alpha["approved"], true);
        assert!(labels(find(&page, "beta", "harness")).contains(&"effective".into()));
        assert!(labels(find(&page, "beta", "shared")).contains(&"overridden".into()));
        let only_harness = fx
            .query(&app, &agent, "skills.discover", json!({"scope":{"kind":"Workspace"},"rootIds":["skills-harness"],"page":{"limit":100}}))
            .unwrap();
        assert_eq!(only_harness["items"].as_array().unwrap().len(), 1);
        // Resources resolve from the owning skill directory, with their exact digests.
        let resources = fx
            .query(&app, &agent, "skills.resources", json!({"instructionId":alpha["id"],"revision":alpha["revision"],"page":{"limit":10}}))
            .unwrap();
        let digests: Vec<_> = resources["items"].as_array().unwrap().iter().map(|r| r["sha256"].as_str().unwrap().to_owned()).collect();
        assert_eq!(digests, [
            devworks_core::fingerprint::sha256_hex(b"reference"),
            devworks_core::fingerprint::sha256_hex(b"#!/bin/sh\necho alpha\n"),
        ]);
        let first = app.effective_skills().unwrap();
        assert_eq!(first.skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["alpha", "beta"]);
        assert_eq!(first.skills[1].root_id, "skills-harness");
        // An external edit of a user-owned shared file is observed as a new active revision.
        std::fs::write(fx.shared.join("alpha/SKILL.md"), "Do alpha better.").unwrap();
        let page = discover(&fx, &app, &user);
        let alpha2 = find(&page, "alpha", "shared");
        assert_eq!(alpha2["revision"], "2");
        let history = fx
            .query(&app, &user, "skills.history", json!({"instructionId":alpha2["id"],"page":{"limit":1}}))
            .unwrap();
        assert_eq!(history["items"][0]["revision"], "2");
        assert_eq!(history["hasMore"], true);
        let older = fx
            .query(&app, &user, "skills.history", json!({"instructionId":alpha2["id"],"page":{"limit":5,"cursor":history["nextCursor"]}}))
            .unwrap();
        assert_eq!(older["items"][0]["revision"], "1");
        let second = app.effective_skills().unwrap();
        assert_ne!(first.digest, second.digest);
        assert_eq!(second.transitions(&first), ["alpha"]);
        // Idempotent: unchanged sources record nothing new.
        assert_eq!(app.instructions().unwrap().sync().unwrap().observed, 0);
        // A human restores the earlier captured revision; the shared file is not touched.
        let restored = applied(fx.command(&app, &user, "skills.rollback", json!({"instructionId":alpha2["id"],"expectedRevision":"2","targetRevision":"1"})));
        assert_eq!(restored["revision"], "1");
        assert!(labels(&restored).contains(&"active".into()));
        let mut shared_now = shared_before.clone();
        shared_now.iter_mut().find(|(p, _)| p == "alpha/SKILL.md").unwrap().1 = b"Do alpha better.".to_vec();
        assert_eq!(tree(&fx.shared), shared_now);
        // Unknown roots are refused, not ignored.
        assert_eq!(
            fx.query(&app, &user, "skills.discover", json!({"scope":{"kind":"Workspace"},"rootIds":["elsewhere"],"page":{"limit":5}}))
                .unwrap_err()["code"],
            "InvalidArgument"
        );
        app.shutdown().await;
    });
}

#[test]
fn agent_skill_changes_take_effect_only_through_exact_human_approval() {
    let fx = Fx::new();
    write_skill(
        &fx.shared,
        "alpha",
        &[("SKILL.md", "Do alpha."), ("scripts/run.sh", "#!/bin/sh\n")],
    );
    let shared_before = tree(&fx.shared);
    runtime().block_on(async {
        let (user, agent, reviewer) = fx.principals();
        let app = boot(fx.composition()).await.unwrap();
        let shared = find(&discover(&fx, &app, &agent), "alpha", "shared").clone();
        let content = fx.upload(&agent, "Do alpha, improved.");
        // A read-only reviewer cannot author skills.
        assert_eq!(
            refused(fx.command(&app, &reviewer, "skills.draft", json!({"scope":{"kind":"Workspace"},"instructionId":shared["id"],"expectedRevision":"1","name":"alpha","content":fx.upload(&reviewer, "x")}))),
            "PermissionDenied"
        );
        // An agent improvement of a shared skill drafts a harness override; nothing is effective.
        let draft = applied(fx.command(&app, &agent, "skills.draft", json!({"scope":{"kind":"Workspace"},"instructionId":shared["id"],"expectedRevision":"1","name":"alpha","content":content})));
        assert_eq!(draft["approved"], false);
        assert!(labels(&draft).contains(&"origin:harness".into()));
        assert!(labels(&draft).contains(&"state:draft".into()));
        assert_eq!(tree(&fx.shared), shared_before, "shared originals are untouched");
        assert!(!fx.harness.join("alpha").exists());
        assert_eq!(app.effective_skills().unwrap().skills[0].root_id, "skills-shared");
        // Direct activation by the agent is refused: it needs the exact human approval.
        let direct = fx.command(&app, &agent, "skills.rollback", json!({"instructionId":draft["id"],"expectedRevision":"1","targetRevision":"1"}));
        assert!(!matches!(&direct, CommandReply::Recorded{receipt,..} if matches!(receipt.decision, DomainDecision::Applied{..})), "{direct:?}");
        let digest = read(&*fx.store, |s| s.instructions().instruction_revision(&id(draft["id"].as_str().unwrap()), 1))
            .unwrap()
            .content_digest;
        let proposal = applied(fx.command(&app, &agent, "skills.propose", json!({"instructionId":draft["id"],"expectedRevision":"1","contentDigest":digest})));
        assert_eq!(proposal["state"], "Pending");
        assert_eq!(proposal["operationKind"], "skills.rollback");
        assert_eq!(app.effective_skills().unwrap().skills[0].root_id, "skills-shared", "pending proposals change nothing");
        let decision = |p: &Value, verdict: &str| {
            serde_json::to_vec(&json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,
                "requestKey":fx.h.ids.next_id(IdKind::Link),"body":{"kind":"approvals.record_human_decision",
                "proposalId":p["id"],"expectedRevision":p["revision"],"contentDigest":p["contentDigest"],
                "decision":verdict,"rationale":"Reviewed the exact skill text"}})).unwrap()
        };
        applied(app.record_human_decision(&user, &decision(&proposal, "Approve")).unwrap());
        app.approval_resumes().tick().await.unwrap();
        let effective = app.effective_skills().unwrap();
        assert_eq!(effective.skills.len(), 1);
        assert_eq!(effective.skills[0].root_id, "skills-harness");
        let page = discover(&fx, &app, &user);
        assert!(labels(find(&page, "alpha", "harness")).contains(&"effective".into()));
        assert!(labels(find(&page, "alpha", "shared")).contains(&"overridden".into()));
        // The approved override is materialized with the shared skill's resources carried over.
        assert_eq!(
            tree(&fx.harness.join("alpha")),
            [("SKILL.md".to_owned(), b"Do alpha, improved.".to_vec()), ("scripts/run.sh".to_owned(), b"#!/bin/sh\n".to_vec())]
        );
        assert_eq!(tree(&fx.shared), shared_before);
        let events = read(&*fx.store, |s| s.history().page(&EventFilter::Aggregate{kind:"instruction".into(),id:draft["id"].as_str().unwrap().into()},0,500));
        let kinds: Vec<_> = events.events.iter().map(|e| (e.kind.as_str(), e.actor.0.as_str().to_owned())).collect();
        assert!(kinds.contains(&("InstructionDrafted", "agent".into())), "{kinds:?}");
        assert!(kinds.contains(&("InstructionProposed", "agent".into())));
        assert!(kinds.contains(&("InstructionProposalApproved", "user".into())));
        assert!(kinds.contains(&("InstructionActivated", "agent".into())), "the replay keeps its original actor");
        // A rejected proposal leaves the active revision unchanged.
        let item = read(&*fx.store, |s| s.instructions().instruction_item(&id(draft["id"].as_str().unwrap()))).unwrap();
        let second = applied(fx.command(&app, &agent, "skills.draft", json!({"scope":{"kind":"Workspace"},"instructionId":draft["id"],"expectedRevision":item.latest_revision.to_string(),"name":"alpha","content":fx.upload(&agent, "Worse alpha.")})));
        let digest = read(&*fx.store, |s| s.instructions().instruction_revision(&id(draft["id"].as_str().unwrap()), 2)).unwrap().content_digest;
        let rejected = applied(fx.command(&app, &agent, "skills.propose", json!({"instructionId":second["id"],"expectedRevision":"2","contentDigest":digest})));
        applied(app.record_human_decision(&user, &decision(&rejected, "Reject")).unwrap());
        app.approval_resumes().tick().await.unwrap();
        let revision = read(&*fx.store, |s| s.instructions().instruction_revision(&id(draft["id"].as_str().unwrap()), 2)).unwrap();
        assert_eq!(revision.state, InstructionRevisionState::Rejected);
        assert_eq!(app.effective_skills().unwrap().skills[0].revision, 1);
        assert_eq!(std::fs::read(fx.harness.join("alpha/SKILL.md")).unwrap(), b"Do alpha, improved.");
        // A decided proposal cannot be proposed again, and nobody activates the rejected content.
        let again = fx.command(&app, &agent, "skills.propose", json!({"instructionId":draft["id"],"expectedRevision":"2","contentDigest":digest}));
        assert_eq!(refused(again), "Conflict");
        let latest = read(&*fx.store, |s| s.instructions().instruction_item(&id(draft["id"].as_str().unwrap()))).unwrap().latest_revision;
        assert_eq!(
            refused(fx.command(&app, &user, "skills.rollback", json!({"instructionId":draft["id"],"expectedRevision":latest.to_string(),"targetRevision":"2"}))),
            "ApprovalRequired"
        );
        app.shutdown().await;
    });
}

#[test]
fn role_saves_adopt_at_once_and_agent_role_edits_need_approval() {
    let fx = Fx::new();
    runtime().block_on(async {
        let (user, agent, reviewer) = fx.principals();
        let app = boot(fx.composition()).await.unwrap();
        let default = setting(&*fx.store, AgentRole::Worker);
        let role = fx.query(&app, &agent, "roles.read", json!({"instructionId":"role-worker"})).unwrap();
        assert_eq!(role["revision"], "1");
        assert_eq!(role["approved"], true);
        assert!(labels(&role).contains(&"worker".into()));
        assert_eq!(role["content"]["sha256"], devworks_core::fingerprint::sha256_hex(default.as_bytes()));
        // Retained prompt-editor rules: blank text is refused; an unknown role is not found.
        assert_eq!(
            refused(fx.command(&app, &user, "roles.draft", json!({"scope":{"kind":"Workspace"},"instructionId":"role-worker","expectedRevision":"1","name":"worker","content":fx.upload(&user, "  \n")}))),
            "InvalidArgument"
        );
        assert_eq!(
            refused(fx.command(&app, &user, "roles.draft", json!({"scope":{"kind":"Workspace"},"name":"nobody","content":fx.upload(&user, "x")}))),
            "NotFound"
        );
        // A human save is effective at once, through the setting running agents adopt.
        let saved = applied(fx.command(&app, &user, "roles.draft", json!({"scope":{"kind":"Workspace"},"instructionId":"role-worker","expectedRevision":"1","name":"worker","content":fx.upload(&user, "Implement carefully.")})));
        assert_eq!(saved["revision"], "2");
        assert_eq!(setting(&*fx.store, AgentRole::Worker), "Implement carefully.");
        // Reset to the default restores revision 1 as a new effective setting.
        applied(fx.command(&app, &user, "roles.rollback", json!({"instructionId":"role-worker","expectedRevision":"2","targetRevision":"1"})));
        assert_eq!(setting(&*fx.store, AgentRole::Worker), default);
        // Stale writers are refused instead of overwriting a later edit.
        assert_eq!(
            refused(fx.command(&app, &user, "roles.rollback", json!({"instructionId":"role-worker","expectedRevision":"1","targetRevision":"2"}))),
            "RevisionConflict"
        );
        assert_eq!(
            refused(fx.command(&app, &reviewer, "roles.draft", json!({"scope":{"kind":"Workspace"},"instructionId":"role-worker","expectedRevision":"2","name":"worker","content":fx.upload(&reviewer, "x")}))),
            "PermissionDenied"
        );
        // An agent edit stays a draft until the exact human approval.
        let draft = applied(fx.command(&app, &agent, "roles.draft", json!({"scope":{"kind":"Workspace"},"instructionId":"role-worker","expectedRevision":"2","name":"worker","content":fx.upload(&agent, "Agent-suggested worker rules.")})));
        assert_eq!(draft["approved"], false);
        assert_eq!(setting(&*fx.store, AgentRole::Worker), default);
        let digest = read(&*fx.store, |s| s.instructions().instruction_revision(&id("role-worker"), 3)).unwrap().content_digest;
        let proposal = applied(fx.command(&app, &agent, "roles.propose", json!({"instructionId":"role-worker","expectedRevision":"3","contentDigest":digest})));
        let decision = serde_json::to_vec(&json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,
            "requestKey":fx.h.ids.next_id(IdKind::Link),"body":{"kind":"approvals.record_human_decision",
            "proposalId":proposal["id"],"expectedRevision":proposal["revision"],"contentDigest":proposal["contentDigest"],
            "decision":"Approve","rationale":"Adopt these worker rules"}})).unwrap();
        applied(app.record_human_decision(&user, &decision).unwrap());
        app.approval_resumes().tick().await.unwrap();
        assert_eq!(setting(&*fx.store, AgentRole::Worker), "Agent-suggested worker rules.");
        let history = fx.query(&app, &user, "roles.history", json!({"instructionId":"role-worker","page":{"limit":10}})).unwrap();
        let states: Vec<_> = history["items"].as_array().unwrap().iter().map(|r| (r["revision"].as_str().unwrap().to_owned(), r["approved"].as_bool().unwrap())).collect();
        assert_eq!(states, [("3".into(), true), ("2".into(), true), ("1".into(), true)]);
        // A setting saved elsewhere is observed as a new approved revision.
        let (ctx, store) = (user.clone(), fx.store.clone());
        write(&*store, move |tx| {
            devworks_core::settings::service::mirror_role_instructions(tx, &ctx, &ConfigScope::Workspace, AgentRole::Worker, "Edited in Settings.", json!({"kind":"Saved"}))
                .map(|_| ())
                .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
        })
        .unwrap();
        let current = fx.query(&app, &user, "roles.read", json!({"instructionId":"role-worker"})).unwrap();
        assert_eq!(current["revision"], "4");
        app.shutdown().await;
    });
}
