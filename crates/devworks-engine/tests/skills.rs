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
use devworks_core::protocol::results::{DomainCommandReply, DomainDecision, DomainQueryReply};
use devworks_core::protocol::{CommandReply, ProjectId};
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
                        .replace('\\', "/"),
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
        self.upload_in(owner, text, ArtifactScope::Workspace)
    }
    fn upload_in(&self, owner: &InvocationContext, text: &str, scope: ArtifactScope) -> Value {
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
        let scope = scope.clone();
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
/// The revision digest a record exposes (what `*.propose` binds to).
fn digest_of(record: &Value) -> String {
    labels(record)
        .into_iter()
        .find_map(|l| l.strip_prefix("digest:").map(str::to_owned))
        .expect("records expose their revision digest")
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
    // A resource name the harness directory could never be written with is not captured.
    // (Unix only: NTFS reads `notes:today.md` as an alternate data stream of `notes`.)
    #[cfg(unix)]
    write_skill(
        &fx.shared,
        "colon",
        &[("SKILL.md", "colon"), ("notes:today.md", "x")],
    );
    let shared_before = tree(&fx.shared);
    runtime().block_on(async {
        let (user, agent, reviewer) = fx.principals();
        let app = boot(fx.composition()).await.unwrap();
        let boot_sync = app.instruction_boot_sync().unwrap().as_ref().unwrap();
        assert!(boot_sync.failures.is_empty());
        #[cfg(unix)]
        assert!(
            boot_sync.skipped.iter().any(|(n, reason)| n == "colon" && reason.contains("unsupported file name")),
            "{boot_sync:?}"
        );
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
        // Agents adopting a skill can read its text and resources, even a workspace code
        // reviewer that is not the workspace coordinator.
        let read_artifact = |ctx: &InvocationContext, r: &Value| {
            let frame = serde_json::to_vec(&json!({"protocol":"1.0-draft.1","queryId":"read",
                "query":{"kind":"Read","artifactId":r["artifactId"],"versionId":r["versionId"]},"limit":1})).unwrap();
            serde_json::to_value(app.query(ctx, "artifacts.read", &frame).unwrap()).unwrap()
        };
        let text = read_artifact(&reviewer, &alpha["content"]);
        assert!(text.to_string().contains("Do alpha."), "{text}");
        let resource = read_artifact(&reviewer, &resources["items"][0]);
        assert!(resource.to_string().contains("reference"), "{resource}");
        // Ordinary host artifacts do not become readable through this rule.
        let staged = fx.artifacts.stage_host_bytes(&*fx.store, b"private host text", json!({"kind":"Test"})).unwrap().unwrap();
        let (artifacts, s1) = (fx.artifacts.clone(), staged.clone());
        let host_only = write(&*fx.store, move |tx| artifacts.commit_host_bytes(tx, &s1, "Host only", "text/plain", json!({"note":"x"}))).unwrap();
        fx.artifacts.settle_host_bytes(&*fx.store, &staged, true);
        let hidden = read_artifact(&reviewer, &serde_json::to_value(devworks_core::services::artifacts::version_ref(&host_only)).unwrap());
        assert!(!hidden.to_string().contains("private host text"), "{hidden}");
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
        // A session records the revisions it adopts, and only real transitions.
        let session = applied(fx.command(&app, &user, "sessions.start", json!({"scope":{"kind":"Workspace"},"title":"Skills"})));
        let session_id = id(session["id"].as_str().unwrap());
        let in_session = user.clone().with_session(session_id.clone());
        let (adopted, changed) = app.adopt_skills(&in_session, &session_id, Some(&first)).unwrap();
        assert!(changed);
        let (_, again) = app.adopt_skills(&in_session, &session_id, Some(&adopted)).unwrap();
        assert!(!again, "an unchanged set is not a new adoption");
        let events = read(&*fx.store, |s| s.history().page(&EventFilter::Aggregate{kind:"session".into(),id:session_id.as_str().into()},0,500));
        let adoptions: Vec<_> = events.events.iter().filter(|e| e.kind == "SkillRevisionsAdopted").collect();
        assert_eq!(adoptions.len(), 1);
        assert_eq!(adoptions[0].payload["transitions"], json!(["alpha"]));
        assert_eq!(adoptions[0].payload["skills"][0]["revision"], 2);
        assert!(app.adopt_skills(&user, &session_id, None).is_err(), "another session cannot record");
        // A skill that temporarily cannot be captured (here: a link appears in it) is reported,
        // not treated as deleted: its last approved revision stays in effect.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(fx.shared.join("alpha/SKILL.md"), fx.shared.join("alpha/link.md")).unwrap();
            let report = app.instructions().unwrap().sync().unwrap();
            assert!(report.skipped.iter().any(|(name, _)| name == "alpha"), "{report:?}");
            assert!(app.effective_skills().unwrap().skills.iter().any(|s| s.name == "alpha"));
            std::fs::remove_file(fx.shared.join("alpha/link.md")).unwrap();
        }
        // Idempotent: unchanged sources record nothing new.
        assert_eq!(app.instructions().unwrap().sync().unwrap().observed, 0);
        // A human restores the earlier captured revision. The shared file is not touched, so the
        // restored revision becomes a harness override whose files match it exactly.
        let restored = applied(fx.command(&app, &user, "skills.rollback", json!({"instructionId":alpha2["id"],"expectedRevision":"2","targetRevision":"1"})));
        assert!(labels(&restored).contains(&"origin:harness".into()) && labels(&restored).contains(&"active".into()));
        assert_eq!(
            std::fs::read(fx.harness.join("alpha/SKILL.md")).unwrap(),
            b"---\nname: alpha\ndescription: Alpha skill\n---\nDo alpha."
        );
        assert_eq!(std::fs::read(fx.harness.join("alpha/docs/ref.md")).unwrap(), b"reference");
        // A second shared restore cannot replace that override: the shared item's revision does
        // not guard it, so it is refused and the override's files stay as they are.
        assert_eq!(
            refused(fx.command(&app, &user, "skills.rollback", json!({"instructionId":alpha2["id"],"expectedRevision":"2","targetRevision":"1"}))),
            "Conflict"
        );
        assert_eq!(std::fs::read(fx.harness.join("alpha/docs/ref.md")).unwrap(), b"reference");
        let restored_set = app.effective_skills().unwrap();
        let alpha_now = restored_set.skills.iter().find(|s| s.name == "alpha").unwrap();
        assert_eq!(alpha_now.root_id, "skills-harness");
        assert_eq!(alpha_now.content_digest, first.skills[0].content_digest, "the restored files are in effect");
        let mut shared_now = shared_before.clone();
        shared_now.iter_mut().find(|(p, _)| p == "alpha/SKILL.md").unwrap().1 = b"Do alpha better.".to_vec();
        assert_eq!(tree(&fx.shared), shared_now);
        // An on-disk edit of a harness skill after it was read makes a stale save a conflict, and
        // the edit is captured rather than overwritten.
        let beta = find(&discover(&fx, &app, &user), "beta", "harness").clone();
        std::fs::write(fx.harness.join("beta/notes.md"), "added on disk").unwrap();
        assert_eq!(
            refused(fx.command(&app, &user, "skills.draft", json!({"scope":{"kind":"Workspace"},"instructionId":beta["id"],"expectedRevision":beta["revision"],"name":"beta","content":fx.upload(&user, "harness beta v2")}))),
            "RevisionConflict"
        );
        assert_eq!(std::fs::read(fx.harness.join("beta/notes.md")).unwrap(), b"added on disk");
        assert_eq!(find(&discover(&fx, &app, &user), "beta", "harness")["revision"], "2");
        // While the target's harness directory cannot be captured, no save may replace it.
        #[cfg(unix)]
        {
            let beta = find(&discover(&fx, &app, &user), "beta", "harness").clone();
            std::fs::write(fx.harness.join("beta/notes.md"), "edited again").unwrap();
            std::os::unix::fs::symlink("notes.md", fx.harness.join("beta/link.md")).unwrap();
            assert_eq!(
                refused(fx.command(&app, &user, "skills.draft", json!({"scope":{"kind":"Workspace"},"instructionId":beta["id"],"expectedRevision":beta["revision"],"name":"beta","content":fx.upload(&user, "harness beta v3")}))),
                "SourceChanged"
            );
            assert_eq!(std::fs::read(fx.harness.join("beta/notes.md")).unwrap(), b"edited again");
            std::fs::remove_file(fx.harness.join("beta/link.md")).unwrap();
        }
        // A new harness skill whose name differs only in case from an existing one would share
        // its directory on case-insensitive filesystems: refused, never an unchecked overwrite.
        assert_eq!(
            refused(fx.command(&app, &user, "skills.draft", json!({"scope":{"kind":"Workspace"},"name":"BETA","content":fx.upload(&user, "other beta")}))),
            "Conflict"
        );
        // A harness directory that is not a discovered skill (no SKILL.md) holds uncaptured
        // files: a new skill of that name is refused rather than replacing them.
        std::fs::create_dir_all(fx.harness.join("gamma")).unwrap();
        std::fs::write(fx.harness.join("gamma/notes.md"), "keep me").unwrap();
        assert_eq!(
            refused(fx.command(&app, &user, "skills.draft", json!({"scope":{"kind":"Workspace"},"name":"gamma","content":fx.upload(&user, "new gamma")}))),
            "Conflict"
        );
        assert_eq!(std::fs::read(fx.harness.join("gamma/notes.md")).unwrap(), b"keep me");
        // A restore whose file `docs` would replace a live `docs/` folder holding an uncaptured
        // `.env` is refused before activation, so the reported active revision is the one on disk.
        write_skill(&fx.harness, "delta", &[("SKILL.md", "---\nname: delta\ndescription: Delta\n---\nDo delta."), ("docs", "flat")]);
        let delta = find(&discover(&fx, &app, &user), "delta", "harness").clone();
        std::fs::remove_file(fx.harness.join("delta/docs")).unwrap();
        write_skill(&fx.harness, "delta", &[("docs/ref.md", "nested"), ("docs/.env", "secret")]);
        let delta2 = find(&discover(&fx, &app, &user), "delta", "harness").clone();
        assert_ne!(delta2["revision"], delta["revision"]);
        assert_eq!(
            refused(fx.command(&app, &user, "skills.rollback", json!({"instructionId":delta2["id"],"expectedRevision":delta2["revision"],"targetRevision":delta["revision"]}))),
            "Conflict"
        );
        assert_eq!(find(&discover(&fx, &app, &user), "delta", "harness")["revision"], delta2["revision"]);
        assert_eq!(std::fs::read(fx.harness.join("delta/docs/.env")).unwrap(), b"secret");
        // A deleted directory is written again by a restore of its active revision, even though
        // the recorded digests already match it.
        std::fs::remove_dir_all(fx.harness.join("delta")).unwrap();
        let gone = find(&discover(&fx, &app, &user), "delta", "harness").clone();
        assert!(labels(&gone).contains(&"missing".into()), "{gone}");
        fx.command(&app, &user, "skills.rollback", json!({"instructionId":gone["id"],"expectedRevision":gone["revision"],"targetRevision":delta2["revision"]}));
        assert_eq!(std::fs::read(fx.harness.join("delta/docs/ref.md")).unwrap(), b"nested");
        assert!(!labels(find(&discover(&fx, &app, &user), "delta", "harness")).contains(&"missing".into()));
        // A directory that is there but no longer a capturable skill (its SKILL.md removed, a
        // resource edited) is not missing: restoring over it is refused and the edit stays.
        std::fs::remove_file(fx.harness.join("delta/SKILL.md")).unwrap();
        std::fs::write(fx.harness.join("delta/docs/ref.md"), "edited on disk").unwrap();
        let occupied = find(&discover(&fx, &app, &user), "delta", "harness").clone();
        assert!(labels(&occupied).contains(&"missing".into()), "{occupied}");
        assert_eq!(
            refused(fx.command(&app, &user, "skills.rollback", json!({"instructionId":occupied["id"],"expectedRevision":occupied["revision"],"targetRevision":delta2["revision"]}))),
            "Conflict"
        );
        assert_eq!(std::fs::read(fx.harness.join("delta/docs/ref.md")).unwrap(), b"edited on disk");
        std::fs::remove_dir_all(fx.harness.join("delta")).unwrap();
        // Captured skill versions stay out of artifact listings; other host artifacts remain.
        let staged = fx.artifacts.stage_host_bytes(&*fx.store, b"repository summary", json!({"kind":"Test"})).unwrap().unwrap();
        let (artifacts, s2) = (fx.artifacts.clone(), staged.clone());
        write(&*fx.store, move |tx| artifacts.commit_host_bytes(tx, &s2, "Repository summary", "text/markdown", json!({"repositoryId":"repo"}))).unwrap();
        fx.artifacts.settle_host_bytes(&*fx.store, &staged, true);
        let list = serde_json::to_vec(&json!({"protocol":"1.0-draft.1","queryId":"list","schemaRevision":4,
            "query":{"kind":"List","workspace":true},"limit":100})).unwrap();
        let listed = serde_json::to_value(app.query(&user, "artifacts.list", &list).unwrap()).unwrap();
        let names: Vec<String> = listed.pointer("/result/value/items").and_then(Value::as_array).unwrap_or_else(|| panic!("{listed}"))
            .iter().map(|a| a["name"].as_str().unwrap().to_owned()).collect();
        assert!(names.contains(&"Repository summary".to_owned()), "{names:?}");
        assert!(!names.iter().any(|n| n.starts_with("Skill ") || n.starts_with("Role ")), "{names:?}");
        // A save whose text with the carried resources would exceed what discovery captures is
        // refused; a save that fits is applied.
        let mib = 1024 * 1024;
        std::fs::create_dir_all(fx.harness.join("epsilon/assets")).unwrap();
        std::fs::write(fx.harness.join("epsilon/SKILL.md"), "Do epsilon.").unwrap();
        for i in 0..8 {
            let size = if i == 7 { 4 * mib - 100 * 1024 } else { 4 * mib };
            std::fs::write(fx.harness.join(format!("epsilon/assets/blob-{i}.bin")), vec![b'x'; size]).unwrap();
        }
        let epsilon = find(&discover(&fx, &app, &user), "epsilon", "harness").clone();
        let big = "y".repeat(150 * 1024);
        assert_eq!(
            refused(fx.command(&app, &user, "skills.draft", json!({"scope":{"kind":"Workspace"},"instructionId":epsilon["id"],"expectedRevision":epsilon["revision"],"name":"epsilon","content":fx.upload(&user, &big)}))),
            "InvalidArgument"
        );
        applied(fx.command(&app, &user, "skills.draft", json!({"scope":{"kind":"Workspace"},"instructionId":epsilon["id"],"expectedRevision":epsilon["revision"],"name":"epsilon","content":fx.upload(&user, "Do epsilon well.")})));
        std::fs::remove_dir_all(fx.harness.join("epsilon")).unwrap();
        // A creation that would take the harness root past what discovery reads is refused, so
        // the existing harness skills stay editable.
        // An agent draft adds no directory, so the root's room is checked again when it is
        // activated: here another entry filled the root in between.
        let present = std::fs::read_dir(&fx.harness).unwrap().count();
        for i in present..511 {
            std::fs::create_dir(fx.harness.join(format!("filler-{i}"))).unwrap();
        }
        let late = applied(fx.command(&app, &agent, "skills.draft", json!({"scope":{"kind":"Workspace"},"name":"late","content":fx.upload(&agent, "Do it late.")})));
        assert!(!fx.harness.join("late").exists());
        std::fs::create_dir(fx.harness.join("filler-last")).unwrap();
        let proposal = applied(fx.command(&app, &agent, "skills.propose", json!({"instructionId":late["id"],"expectedRevision":"1","contentDigest":digest_of(&late)})));
        let decision = serde_json::to_vec(&json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,
            "requestKey":fx.h.ids.next_id(IdKind::Link),"body":{"kind":"approvals.record_human_decision",
            "proposalId":proposal["id"],"expectedRevision":proposal["revision"],"contentDigest":proposal["contentDigest"],
            "decision":"Approve","rationale":"Reviewed"}})).unwrap();
        applied(app.record_human_decision(&user, &decision).unwrap());
        app.approval_resumes().tick().await.unwrap();
        // The approved activation is refused for lack of room: nothing is written or effective.
        assert!(!fx.harness.join("late").exists());
        assert!(!app.effective_skills().unwrap().skills.iter().any(|s| s.name == "late"));
        assert_eq!(
            refused(fx.command(&app, &user, "skills.draft", json!({"scope":{"kind":"Workspace"},"name":"overflow","content":fx.upload(&user, "one too many")}))),
            "Conflict"
        );
        assert!(!fx.harness.join("overflow").exists());
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
        &[
            ("SKILL.md", "Do alpha."),
            ("scripts/run.sh", "#!/bin/sh\n"),
            ("bin/tool", "ELF"),
        ],
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            fx.shared.join("alpha/bin/tool"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::set_permissions(
            fx.shared.join("alpha/scripts/run.sh"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
    }
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
        // The unapproved override is discoverable (to keep editing it with its own revision)
        // but never effective.
        let listed = find(&discover(&fx, &app, &user), "alpha", "harness").clone();
        assert_eq!(listed["id"], draft["id"]);
        assert!(labels(&listed).contains(&"inactive".into()) && labels(&listed).contains(&"state:draft".into()));
        assert!(labels(find(&discover(&fx, &app, &user), "alpha", "shared")).contains(&"effective".into()));
        assert!(!fx.harness.join("alpha").exists());
        assert_eq!(app.effective_skills().unwrap().skills[0].root_id, "skills-shared");
        // Direct activation by the agent is refused: it needs the exact human approval.
        let direct = fx.command(&app, &agent, "skills.rollback", json!({"instructionId":draft["id"],"expectedRevision":"1","targetRevision":"1"}));
        assert!(!matches!(&direct, CommandReply::Recorded{receipt,..} if matches!(receipt.decision, DomainDecision::Applied{..})), "{direct:?}");
        // The revision digest a proposal binds to comes from the public draft record.
        let digest = digest_of(&draft);
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
            [("SKILL.md".to_owned(), b"Do alpha, improved.".to_vec()), ("bin/tool".to_owned(), b"ELF".to_vec()), ("scripts/run.sh".to_owned(), b"#!/bin/sh\n".to_vec())]
        );
        // Execute permissions follow the source, not script detection.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &str| std::fs::metadata(fx.harness.join(p)).unwrap().permissions().mode() & 0o111;
            assert_ne!(mode("alpha/bin/tool"), 0, "a native helper stays executable");
            assert_eq!(mode("alpha/scripts/run.sh"), 0, "a non-executable script stays non-executable");
        }
        assert_eq!(tree(&fx.shared), shared_before);
        let events = read(&*fx.store, |s| s.history().page(&EventFilter::Aggregate{kind:"instruction".into(),id:draft["id"].as_str().unwrap().into()},0,500));
        let kinds: Vec<_> = events.events.iter().map(|e| (e.kind.as_str(), e.actor.0.as_str().to_owned())).collect();
        assert!(kinds.contains(&("InstructionDrafted", "agent".into())), "{kinds:?}");
        assert!(kinds.contains(&("InstructionProposed", "agent".into())));
        assert!(kinds.contains(&("InstructionProposalApproved", "user".into())));
        assert!(kinds.contains(&("InstructionActivated", "agent".into())), "the replay keeps its original actor");
        // Once an override exists, edits go through its identity and revision, never the shared
        // item's, so a concurrent override edit cannot be overwritten silently.
        assert_eq!(
            refused(fx.command(&app, &agent, "skills.draft", json!({"scope":{"kind":"Workspace"},"instructionId":shared["id"],"expectedRevision":"1","name":"alpha","content":fx.upload(&agent, "Another.")}))),
            "Conflict"
        );
        // A rejected proposal leaves the active revision unchanged.
        let item = read(&*fx.store, |s| s.instructions().instruction_item(&id(draft["id"].as_str().unwrap()))).unwrap();
        let second = applied(fx.command(&app, &agent, "skills.draft", json!({"scope":{"kind":"Workspace"},"instructionId":draft["id"],"expectedRevision":item.latest_revision.to_string(),"name":"alpha","content":fx.upload(&agent, "Worse alpha.")})));
        let digest = digest_of(&second);
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

/// A writable agent bound to (and invoking within) its own new project.
fn project_agent(
    fx: &Fx,
    user: &InvocationContext,
    name: &str,
    project: &str,
) -> InvocationContext {
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
fn a_project_agents_skill_draft_is_confined_to_its_project_until_a_person_approves_it() {
    let fx = Fx::new();
    runtime().block_on(async {
        let (user, _, _) = fx.principals();
        let app = boot(fx.composition()).await.unwrap();
        let a = project_agent(&fx, &user, "agent-a", "project-a");
        let b = project_agent(&fx, &user, "agent-b", "project-b");
        let in_a = ArtifactScope::Project(ProjectId(id("project-a")));
        let names = |page: &Value| -> Vec<String> {
            page["items"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap().to_owned()).collect()
        };
        let code = |r: Result<Value, Value>| r.expect_err("refused")["code"].as_str().unwrap().to_owned();
        let artifact_scope = |content: &Value| {
            let artifact = id(content["artifactId"].as_str().unwrap());
            read(&*fx.store, |s| s.artifacts().artifact(&artifact)).unwrap().scope
        };
        // A project-confined agent drafts a new workspace skill: it is recorded, confined to its
        // project, and its text lives in a project-scoped artifact.
        let draft = applied(fx.command(&app, &a, "skills.draft", json!({"scope":{"kind":"Workspace"},"name":"deploy","content":fx.upload_in(&a, "Project A deploy steps.", in_a.clone())})));
        assert!(labels(&draft).contains(&"state:draft".into()));
        assert_eq!(artifact_scope(&draft["content"]), in_a);
        let item_id = draft["id"].clone();
        // Project B cannot find, read, list the history of, or list the resources of it.
        assert!(!names(&discover(&fx, &app, &b)).contains(&"deploy".to_owned()));
        assert_eq!(code(fx.query(&app, &b, "skills.read", json!({"instructionId":item_id}))), "NotFound");
        assert_eq!(code(fx.query(&app, &b, "skills.history", json!({"instructionId":item_id,"page":{"limit":10}}))), "NotFound");
        assert_eq!(code(fx.query(&app, &b, "skills.resources", json!({"instructionId":item_id,"revision":"1","page":{"limit":10}}))), "NotFound");
        // Taking the same name is a conflict that does not name the hidden item.
        let taken = fx.command(&app, &b, "skills.draft", json!({"scope":{"kind":"Workspace"},"name":"deploy","content":fx.upload_in(&b, "B text", ArtifactScope::Project(ProjectId(id("project-b"))))}));
        let taken = serde_json::to_value(&taken).unwrap();
        assert_eq!(taken.pointer("/receipt/decision/fault/code").and_then(Value::as_str), Some("Conflict"), "{taken}");
        assert!(taken.pointer("/receipt/decision/fault/targetId").is_none_or(Value::is_null), "{taken}");
        // Its own project and the person see it.
        assert!(names(&discover(&fx, &app, &a)).contains(&"deploy".to_owned()));
        assert!(names(&discover(&fx, &app, &user)).contains(&"deploy".to_owned()));
        // The proposal's review is scoped to project A, like the draft.
        let proposal = applied(fx.command(&app, &a, "skills.propose", json!({"instructionId":item_id,"expectedRevision":"1","contentDigest":digest_of(&draft)})));
        let approval = read(&*fx.store, |s| s.decisions().approval(&id(proposal["id"].as_str().unwrap()))).unwrap();
        assert_eq!(approval.scope, ConfigScope::Project(ProjectId(id("project-a"))));
        assert_eq!(code(fx.query(&app, &b, "skills.read", json!({"instructionId":item_id}))), "NotFound");
        // Once the person approves, the skill is a normal workspace skill everyone can read.
        let decision = serde_json::to_vec(&json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,
            "requestKey":fx.h.ids.next_id(IdKind::Link),"body":{"kind":"approvals.record_human_decision",
            "proposalId":proposal["id"],"expectedRevision":proposal["revision"],"contentDigest":proposal["contentDigest"],
            "decision":"Approve","rationale":"Reviewed"}})).unwrap();
        applied(app.record_human_decision(&user, &decision).unwrap());
        app.approval_resumes().tick().await.unwrap();
        let seen = fx.query(&app, &b, "skills.read", json!({"instructionId":item_id})).unwrap();
        assert!(labels(&seen).contains(&"active".into()) && labels(&seen).contains(&"effective".into()), "{seen}");
        assert_eq!(artifact_scope(&seen["content"]), ArtifactScope::Workspace);
        assert_eq!(seen["content"]["sha256"], draft["content"]["sha256"], "the same approved bytes");
        assert_eq!(std::fs::read(fx.harness.join("deploy/SKILL.md")).unwrap(), b"Project A deploy steps.");
        let stored = read(&*fx.store, |s| s.instructions().instruction_item(&id(item_id.as_str().unwrap()))).unwrap();
        assert_eq!(stored.confined_project, None);
        // A later edit by project A of the now-shared skill is again confined: B still reads the
        // active revision and sees only that one in history.
        let edit = applied(fx.command(&app, &a, "skills.draft", json!({"scope":{"kind":"Workspace"},"instructionId":item_id,"expectedRevision":"1","name":"deploy","content":fx.upload_in(&a, "A secret change.", in_a.clone())})));
        assert_eq!(edit["revision"], "2");
        assert_eq!(fx.query(&app, &b, "skills.read", json!({"instructionId":item_id})).unwrap()["revision"], "1");
        assert_eq!(code(fx.query(&app, &b, "skills.read", json!({"instructionId":item_id,"revision":"2"}))), "NotFound");
        assert_eq!(code(fx.query(&app, &b, "skills.resources", json!({"instructionId":item_id,"revision":"2","page":{"limit":10}}))), "NotFound");
        let history = |who: &InvocationContext| -> Vec<String> {
            fx.query(&app, who, "skills.history", json!({"instructionId":item_id,"page":{"limit":10}})).unwrap()["items"]
                .as_array().unwrap().iter().map(|r| r["revision"].as_str().unwrap().to_owned()).collect()
        };
        assert_eq!(history(&b), ["1"]);
        assert_eq!(history(&a), ["2", "1"]);
        assert_eq!(history(&user), ["2", "1"]);
        // B cannot propose A's draft either.
        assert_eq!(
            refused(fx.command(&app, &b, "skills.propose", json!({"instructionId":item_id,"expectedRevision":"2","contentDigest":digest_of(&edit)}))),
            "NotFound"
        );
        // A's hidden draft never blocks B: B saves against the revision its own history shows.
        let b_edit = applied(fx.command(&app, &b, "skills.draft", json!({"scope":{"kind":"Workspace"},"instructionId":item_id,"expectedRevision":"1","name":"deploy","content":fx.upload_in(&b, "B change.", ArtifactScope::Project(ProjectId(id("project-b"))))})));
        assert_eq!(b_edit["revision"], "3");
        assert_eq!(history(&b), ["3", "1"]);
        assert_eq!(history(&a), ["2", "1"]);
        // A stale token is still refused.
        assert_eq!(
            refused(fx.command(&app, &b, "skills.draft", json!({"scope":{"kind":"Workspace"},"instructionId":item_id,"expectedRevision":"1","name":"deploy","content":fx.upload_in(&b, "B again.", ArtifactScope::Project(ProjectId(id("project-b"))))}))),
            "RevisionConflict"
        );
        // Events about a confined draft belong to its project.
        let events = read(&*fx.store, |s| s.history().page(&EventFilter::Aggregate{kind:"instruction".into(),id:item_id.as_str().unwrap().into()},0,500));
        let drafted: Vec<_> = events.events.iter().filter(|e| e.kind == "InstructionDrafted").map(|e| e.project_id.clone()).collect();
        assert_eq!(drafted, [Some(ProjectId(id("project-a"))), Some(ProjectId(id("project-a"))), Some(ProjectId(id("project-b"))), ]);
        app.shutdown().await;
    });
}

#[test]
fn a_project_agents_workspace_role_draft_is_confined_until_approved() {
    let fx = Fx::new();
    runtime().block_on(async {
        let (user, _, _) = fx.principals();
        let app = boot(fx.composition()).await.unwrap();
        let a = project_agent(&fx, &user, "agent-a", "project-a");
        let b = project_agent(&fx, &user, "agent-b", "project-b");
        let in_a = ArtifactScope::Project(ProjectId(id("project-a")));
        let default = setting(&*fx.store, AgentRole::Worker);
        let draft = applied(fx.command(&app, &a, "roles.draft", json!({"scope":{"kind":"Workspace"},"instructionId":"role-worker","expectedRevision":"1","name":"worker","content":fx.upload_in(&a, "Project A worker rules.", in_a.clone())})));
        assert_eq!(draft["revision"], "2");
        let revisions = |who: &InvocationContext| -> Vec<String> {
            fx.query(&app, who, "roles.history", json!({"instructionId":"role-worker","page":{"limit":10}})).unwrap()["items"]
                .as_array().unwrap().iter().map(|r| r["revision"].as_str().unwrap().to_owned()).collect()
        };
        // Project B keeps reading the active default and never sees A's draft.
        assert_eq!(fx.query(&app, &b, "roles.read", json!({"instructionId":"role-worker"})).unwrap()["revision"], "1");
        let hidden = fx.query(&app, &b, "roles.read", json!({"instructionId":"role-worker","revision":"2"})).expect_err("hidden");
        assert_eq!(hidden["code"], "NotFound");
        assert_eq!(revisions(&b), ["1"]);
        assert_eq!(revisions(&a), ["2", "1"]);
        let proposal = applied(fx.command(&app, &a, "roles.propose", json!({"instructionId":"role-worker","expectedRevision":"2","contentDigest":digest_of(&draft)})));
        let decision = serde_json::to_vec(&json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,
            "requestKey":fx.h.ids.next_id(IdKind::Link),"body":{"kind":"approvals.record_human_decision",
            "proposalId":proposal["id"],"expectedRevision":proposal["revision"],"contentDigest":proposal["contentDigest"],
            "decision":"Approve","rationale":"Adopt"}})).unwrap();
        assert_eq!(setting(&*fx.store, AgentRole::Worker), default);
        applied(app.record_human_decision(&user, &decision).unwrap());
        app.approval_resumes().tick().await.unwrap();
        // Approved: the workspace setting changes and project B reads the released revision.
        assert_eq!(setting(&*fx.store, AgentRole::Worker), "Project A worker rules.");
        let seen = fx.query(&app, &b, "roles.read", json!({"instructionId":"role-worker"})).unwrap();
        assert_eq!(seen["revision"], "2");
        let artifact = id(seen["content"]["artifactId"].as_str().unwrap());
        assert_eq!(read(&*fx.store, |s| s.artifacts().artifact(&artifact)).unwrap().scope, ArtifactScope::Workspace);
        assert_eq!(revisions(&b), ["2", "1"]);
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
        // Role text must satisfy the role setting's rules for every author (here: an embedded NUL).
        assert_eq!(
            refused(fx.command(&app, &agent, "roles.draft", json!({"scope":{"kind":"Workspace"},"instructionId":"role-worker","expectedRevision":"2","name":"worker","content":fx.upload(&agent, "bad\u{0}text")}))),
            "InvalidArgument"
        );
        // An agent edit stays a draft until the exact human approval.
        let draft = applied(fx.command(&app, &agent, "roles.draft", json!({"scope":{"kind":"Workspace"},"instructionId":"role-worker","expectedRevision":"2","name":"worker","content":fx.upload(&agent, "Agent-suggested worker rules.")})));
        assert_eq!(draft["approved"], false);
        assert_eq!(setting(&*fx.store, AgentRole::Worker), default);
        let digest = digest_of(&draft);
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
        // A project override saved only through settings is discovered and readable, and a
        // first observation that is not the catalog default is not labelled as the default.
        let project = devworks_core::protocol::ProjectId(id("project-a"));
        let (ctx, store, p) = (user.clone(), fx.store.clone(), project.clone());
        write(&*store, move |tx| {
            tx.projects().insert_project(&StoredProject {
                id: p.clone(),
                name: "A".into(),
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
            devworks_core::settings::service::mirror_role_instructions(tx, &ctx, &ConfigScope::Project(p), AgentRole::Worker, "Project worker rules.", json!({"kind":"Saved"}))
                .map(|_| ())
                .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
        })
        .unwrap();
        let project_id = devworks_core::services::instructions::role_item_id(AgentRole::Worker, &ConfigScope::Project(project.clone()));
        let scoped = user.clone().with_lineage(Some(project), None, None);
        let override_role = fx.query(&app, &scoped, "roles.read", json!({"instructionId":project_id.as_str()})).unwrap();
        assert_eq!(override_role["content"]["sha256"], devworks_core::fingerprint::sha256_hex(b"Project worker rules."));
        let first = read(&*fx.store, |s| s.instructions().instruction_revision(&project_id, 1)).unwrap();
        assert_eq!(first.origin["kind"], "SettingsObserved");
        app.shutdown().await;
    });
}

#[test]
fn provider_contexts_bind_skills_gate_dispatch_and_adopt_changes_at_an_epoch() {
    let fx = Fx::new();
    write_skill(&fx.shared, "alpha", &[("SKILL.md", "Do alpha.")]);
    runtime().block_on(async {
        let (user, _, _) = fx.principals();
        let app = boot(fx.composition()).await.unwrap();
        let base = devworks_core::providers::context::ProviderContext {
            version: 1,
            instructions: vec![devworks_core::providers::context::TextBlock {
                text: "role".into(),
                cache: None,
            }],
            instruction_stamps: vec![],
            skill_stamp: None,
            messages: vec![devworks_core::providers::context::Message {
                role: devworks_core::providers::context::MessageRole::User,
                content: vec![devworks_core::providers::context::ContentBlock::Text {
                    text: "work".into(),
                    cache: None,
                }],
            }],
            parameters: Default::default(),
        };
        // Binding puts the effective skill catalog after the role prefix, stamped exactly.
        let bound = app
            .bind_provider_instructions(&user, base.clone(), &[])
            .unwrap();
        let stamp = bound.skill_stamp.clone().expect("skills bound");
        assert_eq!(stamp.skills[0].name, "alpha");
        assert_eq!(bound.instructions[0].text, "role");
        assert!(
            bound.instructions[1]
                .text
                .contains("- alpha (skills-shared, revision 1")
        );
        let mut forged = bound.clone();
        forged.instructions[1]
            .text
            .push_str("- beta (skills-shared, revision 9)\n");
        assert!(
            forged.validate().is_err(),
            "the catalog block must match its stamp"
        );
        // The dispatch gate accepts the current set and refuses a context without it.
        app.verify_instruction_context(&user, &bound, None).unwrap();
        assert!(
            app.verify_instruction_context(&user, &base, None).is_err(),
            "skills in effect must be bound"
        );
        // A session adopts a changed skill set at an explicit epoch, recorded in its history.
        // Epochs are published by the host for the session's turn.
        let session_id = id("skill-session");
        let host = principal(&*fx.store, "skill-host", PrincipalKind::Host)
            .with_session(session_id.clone());
        let (actor, sid) = (host.principal().clone(), session_id.clone());
        write(&*fx.store, move |tx| {
            let now = tx.now_ms();
            tx.sessions().insert_session(&StoredSession {
                id: sid,
                conversation_id: id("skill-conversation"),
                agent_id: actor.clone(),
                project_id: None,
                root_id: None,
                title: "Skills".into(),
                context_epoch: 0,
                revision: 1,
                created_by: actor,
                created_at_ms: now,
                closed_at_ms: None,
                closed_by: None,
                next_sequence: 1,
                checkpoint_sequence: 0,
            })
        })
        .unwrap();
        let staged = app
            .artifacts()
            .stage_host_bytes(
                &*fx.store,
                &serde_json::to_vec(&bound).unwrap(),
                json!({"fixture":"context"}),
            )
            .unwrap()
            .unwrap();
        let (artifacts, file) = (app.artifacts().clone(), staged.clone());
        let version = write(&*fx.store, move |tx| {
            artifacts.commit_host_bytes(
                tx,
                &file,
                "Context",
                "application/vnd.devworks.provider-context+json",
                json!({"fixture":true}),
            )
        })
        .unwrap();
        app.artifacts().settle_host_bytes(&*fx.store, &staged, true);
        let previous = devworks_core::services::artifacts::version_ref(&version);
        let binding = devworks_core::protocol::provider::NativeBinding {
            connection_id: id("provider"),
            account_revision: devworks_core::protocol::Revision::new(1).unwrap(),
            endpoint_profile: devworks_core::protocol::provider::EndpointProfile::CompatibleChat,
            model_id: id("model"),
            context_epoch: devworks_core::protocol::Counter::new(0).unwrap(),
        };
        let same = app
            .adopt_instruction_epoch(&host, &session_id, &previous, &binding, &[])
            .unwrap();
        assert_eq!(
            same.version_id, previous.version_id,
            "an unchanged skill set keeps the context"
        );
        std::fs::write(fx.shared.join("alpha/SKILL.md"), "Do alpha, revised.").unwrap();
        let adopted = app
            .adopt_instruction_epoch(&host, &session_id, &previous, &binding, &[])
            .unwrap();
        assert_ne!(adopted.version_id, previous.version_id);
        assert!(
            app.verify_instruction_context(&user, &bound, None).is_err(),
            "a context bound to the old skill set is not dispatched"
        );
        let epoch = read(&*fx.store, |s| s.sessions().epoch(&session_id, 1)).unwrap();
        assert_eq!(epoch.reason, "InstructionAdoption");
        let bytes = app
            .artifacts()
            .read_exact(&*fx.store, &host, &adopted, 1024 * 1024)
            .unwrap()
            .0;
        let fresh: devworks_core::providers::context::ProviderContext =
            serde_json::from_slice(&bytes).unwrap();
        assert_eq!(fresh.skill_stamp.as_ref().unwrap().skills[0].revision, 2);
        assert_eq!(fresh.instructions[0].text, "role");
        assert_eq!(
            fresh.instructions.len(),
            2,
            "the old catalog block was replaced, not appended"
        );
        let events = read(&*fx.store, |s| {
            s.history().page(
                &EventFilter::Aggregate {
                    kind: "session".into(),
                    id: session_id.as_str().into(),
                },
                0,
                500,
            )
        });
        let adoption = events
            .events
            .iter()
            .find(|e| e.kind == "SkillRevisionsAdopted")
            .expect("recorded");
        assert_eq!(adoption.payload["transitions"], json!(["alpha"]));
        // A refused transition (here: from a stale epoch) records no adoption.
        std::fs::write(fx.shared.join("alpha/SKILL.md"), "Do alpha, third.").unwrap();
        assert!(
            app.adopt_instruction_epoch(&host, &session_id, &previous, &binding, &[])
                .is_err()
        );
        let events = read(&*fx.store, |s| {
            s.history().page(
                &EventFilter::Aggregate {
                    kind: "session".into(),
                    id: session_id.as_str().into(),
                },
                0,
                500,
            )
        });
        assert_eq!(
            events
                .events
                .iter()
                .filter(|e| e.kind == "SkillRevisionsAdopted")
                .count(),
            1
        );
        app.shutdown().await;
    });
}
