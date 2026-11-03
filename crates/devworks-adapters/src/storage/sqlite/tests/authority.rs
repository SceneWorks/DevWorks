//! Host authority end to end on the real SQLite store: role/tool/profile restrictions, scoped
//! grants, revocation races at the commit and publication points, destructive refusals in
//! history and the dedicated primary repository deletion. Deterministic; no inference.
use super::{Accept, Harness, Seq, T0, start};
use crate::fs::NativeFilesystem;
use crate::paths::FsPathProbe;
use crate::storage::sqlite::SqliteStore;
use devworks_core::admission::{self, Selection};
use devworks_core::authority::deletion::{self, DeletionRefusal, RepositoryDependencies};
use devworks_core::authority::destructive::{
    self, BlockReason, DestructiveExecutor, DestructiveKind, EffectPlan, IntendedScope,
    PreflightVerdict, ProtectedRoots,
};
use devworks_core::authority::service::{self as auth, AdminError, GrantBounds};
use devworks_core::authority::{
    AccessProfile, DenialReason, Denied, GrantBinding, HostAuthority, HostRoles,
};
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::gateway::{AdmittedQuery, Gateway};
use devworks_core::ports::storage::*;
use devworks_core::ports::{
    AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization, QueryAnswer, QueryHandler,
};
use devworks_core::protocol::results::DomainQueryReply;
use devworks_core::protocol::{
    ActorId, CommandReply, Counter, DomainErrorCode, DomainFault, Id, ProjectId, Rejection,
    Revision,
};
use devworks_core::registry::Mode;
use devworks_core::services::UseCases;
use devworks_core::settings::catalog::{AgentRole, descriptor};
use devworks_core::settings::service::{self as svc, PrincipalRoles, SettingsService};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn actor(s: &str) -> ActorId {
    ActorId(id(s))
}

struct Fx {
    h: Harness,
    store: Arc<SqliteStore>,
    settings: Arc<SettingsService>,
    /// The production operation registry: settings reach the gateway through it.
    cases: UseCases,
    ids: Seq,
    n: AtomicU64,
}

const ACTORS: [(&str, PrincipalKind); 9] = [
    ("user-a", PrincipalKind::Human),
    ("agent-a", PrincipalKind::Agent),
    ("agent-p", PrincipalKind::Agent),
    ("agent-x", PrincipalKind::Agent),
    ("reviewer-a", PrincipalKind::Agent),
    ("pkg-a", PrincipalKind::Package),
    ("mcp-a", PrincipalKind::McpClient),
    ("sched-a", PrincipalKind::Schedule),
    ("agent-r", PrincipalKind::Agent),
];

impl Fx {
    fn new() -> Self {
        Self::with_id_offset(0)
    }
    /// A fixture whose store draws identifiers from `offset` on, so two fixtures are two
    /// distinct stores.
    fn with_id_offset(offset: u64) -> Self {
        let h = Harness::new();
        h.ids.0.store(offset, Ordering::SeqCst);
        let store = Arc::new(h.store());
        store
            .with_writer_connection(|c| {
                super::super::conn::db(c.execute_batch(
                    "INSERT INTO projects VALUES('project-a','A','Active',1,1,1);
                     INSERT INTO projects VALUES('project-b','B','Active',1,1,1);
                     INSERT INTO work_items VALUES('root-a','project-a','feature',NULL,NULL,NULL,'Fixture',1,0,1,1,1,'{}');",
                ))
            })
            .unwrap();
        let settings = Arc::new(SettingsService {
            roles: Arc::new(HostRoles {
                store: store.clone(),
            }),
            paths: Arc::new(FsPathProbe),
            ids: h.ids.clone(),
        });
        let fx = Self {
            h,
            store,
            cases: UseCases::new().with_settings(settings.clone()),
            settings,
            ids: Seq(AtomicU64::new(0)),
            n: AtomicU64::new(0),
        };
        fx.write(|tx| {
            for (a, k) in ACTORS {
                tx.actors().register(&actor(a), k, a)?;
            }
            Ok(())
        });
        let user = fx.ctx("user-a", PrincipalKind::Human);
        for (a, kind, role, project) in [
            (
                "agent-a",
                PrincipalKind::Agent,
                Some(AgentRole::Worker),
                None,
            ),
            (
                "agent-p",
                PrincipalKind::Agent,
                Some(AgentRole::Worker),
                Some("project-a"),
            ),
            (
                "reviewer-a",
                PrincipalKind::Agent,
                Some(AgentRole::CodeReviewer),
                None,
            ),
            ("pkg-a", PrincipalKind::Package, None, None),
            ("mcp-a", PrincipalKind::McpClient, None, None),
            ("sched-a", PrincipalKind::Schedule, None, None),
            (
                "agent-r",
                PrincipalKind::Agent,
                Some(AgentRole::Worker),
                None,
            ),
        ] {
            fx.bind(&user, a, kind, role, project).unwrap();
        }
        fx
    }
    fn authority(&self) -> HostAuthority<'_> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.h.clock,
            protected_branches: &[],
        }
    }
    fn gateway<'a>(&'a self, authority: &'a dyn AuthorityPort) -> Gateway<'a> {
        Gateway {
            authority,
            store: &*self.store,
            ids: &self.ids,
        }
    }
    fn ctx(&self, who: &str, kind: PrincipalKind) -> InvocationContext {
        let info = self.store.info();
        devworks_core::context::HostIngress::new(&*self.store).context(
            actor(who),
            kind,
            id("invocation-1"),
            id("ui-main"),
            Counter::new(info.restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
    }
    fn agent(&self, who: &str) -> InvocationContext {
        self.ctx(who, PrincipalKind::Agent)
    }
    fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    ) -> T {
        run_write(&*self.store, WriteOptions::default(), f).expect("write")
    }
    fn bind(
        &self,
        by: &InvocationContext,
        who: &str,
        kind: PrincipalKind,
        role: Option<AgentRole>,
        project: Option<&str>,
    ) -> Result<PrincipalBinding, AdminError> {
        let (by, binding) = (
            by.clone(),
            NewPrincipalBinding {
                actor: actor(who),
                kind,
                role,
                project_id: project.map(|p| ProjectId(id(p))),
                tool_families: None,
                bound_by: actor("agent-a"),
            },
        );
        self.write(move |tx| Ok(auth::bind_principal(tx, &by, binding, None)))
    }
    fn mint(
        &self,
        by: &InvocationContext,
        holder: &str,
        kind: GrantKind,
        binding: GrantBinding,
        uses: Option<i64>,
    ) -> Result<AuthorityGrant, AdminError> {
        let (by, holder) = (by.clone(), actor(holder));
        self.write(move |tx| {
            Ok(auth::mint_grant(
                tx,
                &by,
                holder,
                kind,
                binding,
                GrantBounds {
                    expires_at_ms: T0 + 60_000,
                    uses: uses.unwrap_or(10),
                },
            ))
        })
    }
    /// Digest of the current effective access policy of `project`.
    fn policy_digest(&self, project: &str) -> String {
        let project = ProjectId(id(project));
        read_snapshot(&*self.store, &ReadOptions::default(), |s| {
            devworks_core::authority::policy::resolve_policy(s.configuration(), Some(&project))
        })
        .unwrap()
        .digest()
    }
    fn key(&self) -> String {
        format!("key-{}", self.n.fetch_add(1, Ordering::SeqCst))
    }
    fn principal(&self, who: &str) -> PrincipalBinding {
        let who = actor(who);
        read_snapshot(&*self.store, &ReadOptions::default(), |s| {
            s.authority().principal(&who)
        })
        .unwrap()
        .unwrap()
    }
    fn grant(&self, gid: &Id) -> AuthorityGrant {
        let gid = gid.clone();
        read_snapshot(&*self.store, &ReadOptions::default(), |s| {
            s.authority().authority_grant(&gid)
        })
        .unwrap()
        .unwrap()
    }
    fn events(&self) -> Vec<EventRecord> {
        read_snapshot(&*self.store, &ReadOptions::default(), |s| {
            s.history()
                .page(&EventFilter::All, 0, 500)
                .map(|p| p.events)
        })
        .unwrap()
    }
    fn count(&self, kind: &str) -> usize {
        self.events().iter().filter(|e| e.kind == kind).count()
    }
    fn save_frame(&self, scope: Value, expected: i64, key: &str, value: Value) -> String {
        let schema = serde_json::to_value(descriptor(key).unwrap().schema_ref()).unwrap();
        json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": self.key(), "body": {
            "kind": "settings.save_revision", "scope": scope, "expectedRevision": expected.to_string(),
            "entries": [{"key": key, "value": {"schema": schema, "value": value}}]}})
        .to_string()
    }
    /// The user saves one setting through the real gateway and authority.
    fn user_saves(&self, scope: Value, expected: i64, key: &str, value: Value) {
        let authority = self.authority();
        let reply = self
            .cases
            .command(
                &self.gateway(&authority),
                &self.ctx("user-a", PrincipalKind::Human),
                "settings.save_revision",
                self.save_frame(scope, expected, key, value).as_bytes(),
            )
            .unwrap();
        assert!(matches!(reply, CommandReply::Recorded { .. }), "{reply:?}");
    }
}

fn decide(
    fx: &Fx,
    ctx: &InvocationContext,
    op: &str,
    mode: Mode,
    frame: &str,
) -> Result<Authorization, Denied> {
    let d = admission::decode(
        Selection {
            operation: op,
            mode,
        },
        frame.as_bytes(),
    )
    .expect("frame");
    fx.authority().decide(
        ctx,
        &AuthorityRequest {
            descriptor: d.descriptor,
            binding: d.binding,
            effect: d.effect,
            action: d.action,
            input: &d.input,
            fingerprint: &d.fingerprint,
        },
    )
}
fn digest(op: &str, mode: Mode, frame: &str) -> String {
    admission::decode(
        Selection {
            operation: op,
            mode,
        },
        frame.as_bytes(),
    )
    .unwrap()
    .fingerprint
    .as_str()
    .to_owned()
}
fn reason(r: Result<Authorization, Denied>) -> Option<DenialReason> {
    r.err().map(|d| d.reason)
}

const EXECUTE: &str = r#"{"mode":"Command","protocol":"1.0-draft.1","requestKey":"request-a","body":{"kind":"process.execute","spec":{"executableRef":"id-a","args":[],"workingDirectory":{"rootId":"id-a","relative":"src/main.rs"},"environment":[],"profile":"VerifiedRead","timeoutMs":"0","outputLimitBytes":"0"}}}"#;
const FETCH: &str = r#"{"mode":"Query","protocol":"1.0-draft.1","queryId":"request-a","body":{"kind":"web.fetch","scope":{"kind":"Workspace"},"url":"https://fixture.invalid/path","maxBytes":4096,"timeoutMs":"1"}}"#;
const MCP_SERVERS: &str = r#"{"mode":"Query","protocol":"1.0-draft.1","queryId":"request-a","body":{"kind":"mcp.servers","scope":{"kind":"Workspace"},"page":{"limit":20}}}"#;
const READ_SETTINGS: &str = r#"{"mode":"Query","protocol":"1.0-draft.1","queryId":"request-a","body":{"kind":"settings.read_effective","scope":{"kind":"Workspace"},"keys":[],"page":{"limit":20}}}"#;
const SYSTEM_INSTALL: &str = r#"{"mode":"Command","protocol":"1.0-draft.1","requestKey":"request-a","body":{"kind":"process.request_system_install","packageSpec":{"artifactId":"artifact-a","versionId":"version-a","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","byteLength":"8","mediaType":"text/plain"},"contentDigest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","reason":"Fixture content"}}"#;
fn push(target: &str, head: char) -> String {
    let hex: String = std::iter::repeat_n(head, 40).collect();
    format!(
        r#"{{"protocol":"1.0-draft.1","requestKey":"git-command","command":{{"kind":"Push","worktreeId":"work-a","remoteId":"remote-a","targetRef":"{target}","expectedHead":{{"format":"Sha1","hex":"{hex}"}},"expectedRemote":{{"format":"Sha1","hex":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}},"mode":"ForceWithLease"}}}}"#
    )
}

#[test]
fn full_is_the_default_and_role_restrictions_hold_under_it() {
    let fx = Fx::new();
    let perms = fx
        .authority()
        .effective_permissions(&fx.agent("agent-a"))
        .unwrap();
    assert_eq!(perms.profile, Some(AccessProfile::Full));
    let source = perms.profile_source.unwrap();
    assert_eq!((source.scope, source.revision), (ConfigScope::Workspace, 1));
    assert!(perms.mandatory.iter().any(|m| m.contains("read-only")));

    // A worker under Full may execute; the reviewer never writes and reads only verified reads.
    assert!(
        decide(
            &fx,
            &fx.agent("agent-a"),
            "process.execute",
            Mode::Command,
            EXECUTE
        )
        .is_ok()
    );
    let reviewer = fx.agent("reviewer-a");
    assert_eq!(
        reason(decide(
            &fx,
            &reviewer,
            "process.execute",
            Mode::Command,
            EXECUTE
        )),
        Some(DenialReason::ReadOnlyRole)
    );
    assert_eq!(
        reason(decide(&fx, &reviewer, "web.fetch", Mode::Query, FETCH)),
        Some(DenialReason::UnverifiedRead)
    );
    assert_eq!(
        reason(decide(
            &fx,
            &reviewer,
            "mcp.servers",
            Mode::Query,
            MCP_SERVERS
        )),
        Some(DenialReason::UnverifiedRead)
    );
    assert!(
        decide(
            &fx,
            &reviewer,
            "settings.read_effective",
            Mode::Query,
            READ_SETTINGS
        )
        .is_ok()
    );

    // Through the gateway a reviewer's settings write is denied before the store: no receipt.
    let authority = fx.authority();
    let reply = fx
        .cases
        .command(
            &fx.gateway(&authority),
            &reviewer,
            "settings.save_revision",
            fx.save_frame(
                json!({"kind": "Workspace"}),
                1,
                "power.keepAwake",
                json!(true),
            )
            .as_bytes(),
        )
        .unwrap();
    assert!(matches!(
        reply,
        CommandReply::Rejected {
            code: Rejection::PermissionDenied,
            ..
        }
    ));
    assert_eq!(fx.count("CommandDecided"), 0);

    // User policy: turning shell tools off denies agents the process family.
    fx.user_saves(json!({"kind": "Workspace"}), 1, "tools.shell", json!(false));
    assert_eq!(
        reason(decide(
            &fx,
            &fx.agent("agent-a"),
            "process.execute",
            Mode::Command,
            EXECUTE
        )),
        Some(DenialReason::ShellToolsDisabled)
    );
    // The user is not subject to agent tool policy.
    assert!(
        decide(
            &fx,
            &fx.ctx("user-a", PrincipalKind::Human),
            "process.execute",
            Mode::Command,
            EXECUTE
        )
        .is_ok()
    );
}

#[test]
fn force_push_of_main_is_refused_for_everyone_and_feature_force_push_needs_exact_approval() {
    let fx = Fx::new();
    let user = fx.ctx("user-a", PrincipalKind::Human);
    let agent = fx.agent("agent-a");
    // `refs/heads/main` is already refused by admission; `master` is refused by authority, even
    // for the user.
    assert!(
        admission::decode(
            Selection {
                operation: "git.push",
                mode: Mode::Command
            },
            push("refs/heads/main", 'b').as_bytes()
        )
        .is_err()
    );
    for ctx in [&user, &agent] {
        assert_eq!(
            reason(decide(
                &fx,
                ctx,
                "git.push",
                Mode::Command,
                &push("refs/heads/master", 'b')
            )),
            Some(DenialReason::ProtectedBranch)
        );
    }
    let feature = push("refs/heads/codex/task-a", 'b');
    assert_eq!(
        reason(decide(&fx, &agent, "git.push", Mode::Command, &feature)),
        Some(DenialReason::ApprovalRequired)
    );
    // An approval that does not bind the exact request does not count.
    let loose = fx
        .mint(
            &user,
            "agent-a",
            GrantKind::Operation,
            GrantBinding {
                operation: "git.push".into(),
                action: None,
                request_digest: None,
                project_id: None,
                root_id: None,
                policy_digest: None,
            },
            Some(1),
        )
        .unwrap();
    assert_eq!(
        reason(decide(
            &fx,
            &agent.clone().with_authority_source(loose.id.clone()),
            "git.push",
            Mode::Command,
            &feature
        )),
        Some(DenialReason::ApprovalRequired)
    );
    let exact = fx
        .mint(
            &user,
            "agent-a",
            GrantKind::Operation,
            GrantBinding {
                operation: "git.push".into(),
                action: None,
                request_digest: Some(digest("git.push", Mode::Command, &feature)),
                project_id: None,
                root_id: None,
                policy_digest: None,
            },
            Some(1),
        )
        .unwrap();
    let approved = agent.clone().with_authority_source(exact.id.clone());
    let authorization = decide(&fx, &approved, "git.push", Mode::Command, &feature).unwrap();
    assert_eq!(authorization.authority_grants()[0].grant_id, exact.id);
    // Changed arguments cannot reuse the narrowly approved request.
    assert_eq!(
        reason(decide(
            &fx,
            &approved,
            "git.push",
            Mode::Command,
            &push("refs/heads/codex/task-a", 'c')
        )),
        Some(DenialReason::ApprovalRequired)
    );
    // The approval never reaches main/master.
    assert_eq!(
        reason(decide(
            &fx,
            &approved,
            "git.push",
            Mode::Command,
            &push("refs/heads/master", 'b')
        )),
        Some(DenialReason::ProtectedBranch)
    );
    // System-wide installation needs explicit approval under Full access too.
    assert_eq!(
        reason(decide(
            &fx,
            &agent,
            "process.request_system_install",
            Mode::Command,
            SYSTEM_INSTALL
        )),
        Some(DenialReason::ApprovalRequired)
    );
    // Protected refs compare case-insensitively, and a known remote default branch is
    // protected the same way.
    let trunk = vec!["trunk".to_owned()];
    let with_default = HostAuthority {
        store: &*fx.store,
        clock: &*fx.h.clock,
        protected_branches: &trunk,
    };
    let decide_with = |frame: &str| {
        let d = admission::decode(
            Selection {
                operation: "git.push",
                mode: Mode::Command,
            },
            frame.as_bytes(),
        )
        .unwrap();
        with_default
            .decide(
                &user,
                &AuthorityRequest {
                    descriptor: d.descriptor,
                    binding: d.binding,
                    effect: d.effect,
                    action: d.action,
                    input: &d.input,
                    fingerprint: &d.fingerprint,
                },
            )
            .err()
            .map(|d| d.reason)
    };
    for target in [
        "refs/heads/Master",
        "refs/heads/MAIN",
        "refs/heads/trunk",
        "refs/heads/Trunk",
    ] {
        assert_eq!(
            decide_with(&push(target, 'b')),
            Some(DenialReason::ProtectedBranch),
            "{target}"
        );
    }
    assert_eq!(decide_with(&push("refs/heads/codex/task-a", 'b')), None);
}

#[test]
fn project_scope_denies_native_execution_without_a_reviewed_native_grant() {
    let fx = Fx::new();
    let user = fx.ctx("user-a", PrincipalKind::Human);
    fx.user_saves(
        json!({"kind": "Project", "projectId": "project-a"}),
        1,
        "safety.accessProfile",
        json!("projectScope"),
    );
    let lineage =
        |c: InvocationContext| c.with_lineage(Some(ProjectId(id("project-a"))), None, None);
    let agent = lineage(fx.agent("agent-p"));
    let perms = fx.authority().effective_permissions(&agent).unwrap();
    assert_eq!(perms.profile, Some(AccessProfile::ProjectScope));
    let source = perms.profile_source.unwrap();
    assert_eq!(
        (source.scope, source.revision),
        (ConfigScope::Project(ProjectId(id("project-a"))), 2)
    );
    assert!(perms.guarantee.contains("refused"));
    assert_eq!(
        reason(decide(
            &fx,
            &agent,
            "process.execute",
            Mode::Command,
            EXECUTE
        )),
        Some(DenialReason::EnforcementUnavailable)
    );
    // Other projects keep the inherited Full default.
    assert!(
        decide(
            &fx,
            &fx.agent("agent-a"),
            "process.execute",
            Mode::Command,
            EXECUTE
        )
        .is_ok()
    );
    // A native grant must bind one exact request.
    let loose = fx.mint(
        &user,
        "agent-p",
        GrantKind::NativeExecution,
        GrantBinding {
            operation: "process.execute".into(),
            action: None,
            request_digest: None,
            project_id: None,
            root_id: None,
            policy_digest: None,
        },
        Some(1),
    );
    assert!(matches!(loose, Err(AdminError::Invalid(_))));
    let native = |policy_digest: Option<String>, project: Option<&str>| GrantBinding {
        operation: "process.execute".into(),
        action: None,
        request_digest: Some(digest("process.execute", Mode::Command, EXECUTE)),
        project_id: project.map(|p| ProjectId(id(p))),
        root_id: None,
        policy_digest,
    };
    // It must also bind its project and the policy it was reviewed under.
    let current = fx.policy_digest("project-a");
    for (binding, why) in [
        (native(None, Some("project-a")), "no policy digest"),
        (native(Some(current.clone()), None), "no project"),
        (
            native(Some("0".repeat(64)), Some("project-a")),
            "stale policy digest",
        ),
    ] {
        assert!(
            matches!(
                fx.mint(
                    &user,
                    "agent-p",
                    GrantKind::NativeExecution,
                    binding,
                    Some(1)
                ),
                Err(AdminError::Invalid(_))
            ),
            "{why}"
        );
    }
    let grant = fx
        .mint(
            &user,
            "agent-p",
            GrantKind::NativeExecution,
            native(Some(current), Some("project-a")),
            Some(2),
        )
        .unwrap();
    let granted = agent.clone().with_authority_source(grant.id.clone());
    assert!(decide(&fx, &granted, "process.execute", Mode::Command, EXECUTE).is_ok());
    // A policy change after review invalidates the native grant.
    fx.user_saves(
        json!({"kind": "Workspace"}),
        1,
        "tools.webSearch",
        json!(false),
    );
    assert_eq!(
        reason(decide(
            &fx,
            &granted,
            "process.execute",
            Mode::Command,
            EXECUTE
        )),
        Some(DenialReason::EnforcementUnavailable)
    );
    // The agent bound to project-a cannot act in another project's scope.
    let elsewhere = fx
        .agent("agent-p")
        .with_lineage(Some(ProjectId(id("project-b"))), None, None);
    assert_eq!(
        reason(decide(
            &fx,
            &elsewhere,
            "settings.read_effective",
            Mode::Query,
            READ_SETTINGS
        )),
        Some(DenialReason::OutOfProjectScope)
    );
}

#[test]
fn unsupported_profiles_and_principals_are_denied() {
    let fx = Fx::new();
    let user = fx.ctx("user-a", PrincipalKind::Human);
    // Unbound and unregistered principals.
    assert_eq!(
        reason(decide(
            &fx,
            &fx.agent("agent-x"),
            "settings.read_effective",
            Mode::Query,
            READ_SETTINGS
        )),
        Some(DenialReason::UnboundPrincipal)
    );
    assert_eq!(
        reason(decide(
            &fx,
            &fx.agent("nobody"),
            "settings.read_effective",
            Mode::Query,
            READ_SETTINGS
        )),
        Some(DenialReason::UnregisteredPrincipal)
    );
    // A principal claiming a different kind than the host registered.
    assert_eq!(
        reason(decide(
            &fx,
            &fx.ctx("pkg-a", PrincipalKind::Human),
            "process.execute",
            Mode::Command,
            EXECUTE
        )),
        Some(DenialReason::UnregisteredPrincipal)
    );
    // Unsupported bindings are refused at creation.
    assert!(matches!(
        fx.bind(&user, "agent-x", PrincipalKind::Agent, None, None),
        Err(AdminError::Invalid(_))
    ));
    assert!(matches!(
        fx.bind(
            &user,
            "agent-x",
            PrincipalKind::Package,
            Some(AgentRole::Worker),
            None
        ),
        Err(AdminError::Invalid(_))
    ));
    // Packages, MCP clients and schedules need a host grant for commands; a package can never
    // write configuration.
    for who in [
        ("pkg-a", PrincipalKind::Package),
        ("mcp-a", PrincipalKind::McpClient),
        ("sched-a", PrincipalKind::Schedule),
    ] {
        let ctx = fx.ctx(who.0, who.1);
        assert_eq!(
            reason(decide(&fx, &ctx, "process.execute", Mode::Command, EXECUTE)),
            Some(DenialReason::GrantRequired)
        );
        assert!(
            decide(
                &fx,
                &ctx,
                "settings.read_effective",
                Mode::Query,
                READ_SETTINGS
            )
            .is_ok()
        );
    }
    let save = fx.save_frame(
        json!({"kind": "Workspace"}),
        1,
        "power.keepAwake",
        json!(true),
    );
    assert_eq!(
        reason(decide(
            &fx,
            &fx.ctx("pkg-a", PrincipalKind::Package),
            "settings.save_revision",
            Mode::Command,
            &save
        )),
        Some(DenialReason::HumanOnly)
    );
    // An unsupported stored profile fails closed for agent commands.
    let schema =
        serde_json::to_value(descriptor("safety.accessProfile").unwrap().schema_ref()).unwrap();
    fx.write(move |tx| {
        tx.configuration().append_revision(
            &ConfigScope::Workspace,
            "settings",
            1,
            1,
            json!({"values": {"safety.accessProfile": {"schema": schema, "value": "sandboxed"}}}),
        )
    });
    assert_eq!(
        reason(decide(
            &fx,
            &fx.agent("agent-a"),
            "process.execute",
            Mode::Command,
            EXECUTE
        )),
        Some(DenialReason::UnsupportedProfile)
    );
    assert_eq!(
        fx.authority()
            .effective_permissions(&fx.agent("agent-a"))
            .unwrap()
            .profile,
        None
    );
}

#[test]
fn claimed_authority_in_text_json_or_package_metadata_is_ignored() {
    let fx = Fx::new();
    let user = fx.ctx("user-a", PrincipalKind::Human);
    // A body naming another actor is not a context: strict decoding refuses unknown members.
    let claimed = READ_SETTINGS.replace(r#""keys":[]"#, r#""keys":[],"actorId":"user-a""#);
    assert!(
        admission::decode(
            Selection {
                operation: "settings.read_effective",
                mode: Mode::Query
            },
            claimed.as_bytes()
        )
        .is_err()
    );
    // Only the user mints grants; agents and the host cannot, and agents cannot bind authority.
    let binding = GrantBinding {
        operation: "runs.start".into(),
        action: None,
        request_digest: None,
        project_id: None,
        root_id: None,
        policy_digest: None,
    };
    for ctx in [
        fx.agent("agent-a"),
        fx.ctx("host", PrincipalKind::Host),
        fx.ctx("pkg-a", PrincipalKind::Package),
    ] {
        assert!(matches!(
            fx.mint(
                &ctx,
                "pkg-a",
                GrantKind::Operation,
                binding.clone(),
                Some(1)
            ),
            Err(AdminError::Denied(DenialReason::HumanOnly))
        ));
    }
    assert!(matches!(
        fx.bind(
            &fx.agent("agent-a"),
            "agent-x",
            PrincipalKind::Agent,
            Some(AgentRole::Worker),
            None
        ),
        Err(AdminError::Denied(_))
    ));
    // `bound_by` in the request is ignored: the binding is attributed to the caller.
    assert_eq!(fx.principal("agent-a").bound_by, actor("user-a"));
    // A grant held by another principal is not usable by copying its ID.
    let grant = fx
        .mint(
            &user,
            "sched-a",
            GrantKind::Operation,
            binding.clone(),
            Some(1),
        )
        .unwrap();
    let copied = fx
        .ctx("pkg-a", PrincipalKind::Package)
        .with_authority_source(grant.id.clone());
    assert_eq!(
        reason(decide(
            &fx,
            &copied,
            "runs.start",
            Mode::Command,
            &start("k")
        )),
        Some(DenialReason::GrantRequired)
    );
    // A stored grant whose binding claims extra powers does not parse and grants nothing.
    let forged = id("grant-forged");
    let f = forged.clone();
    fx.write(move |tx| {
        tx.authority().issue_grant(NewAuthorityGrant {
            id: f,
            actor: actor("pkg-a"),
            kind: GrantKind::Operation,
            issued_by: actor("user-a"),
            binding: json!({"operation": "runs.start", "fullAccess": true}),
            expires_at_ms: None,
            uses: None,
        })
    });
    let forged_ctx = fx
        .ctx("pkg-a", PrincipalKind::Package)
        .with_authority_source(forged);
    assert_eq!(
        reason(decide(
            &fx,
            &forged_ctx,
            "runs.start",
            Mode::Command,
            &start("k")
        )),
        Some(DenialReason::GrantRequired)
    );
    // The holder exercising its own grant is admitted and stays attributed to itself.
    let own = fx
        .ctx("sched-a", PrincipalKind::Schedule)
        .with_authority_source(grant.id.clone());
    assert!(decide(&fx, &own, "runs.start", Mode::Command, &start("k")).is_ok());
    let issued = fx
        .events()
        .into_iter()
        .find(|e| e.kind == "AuthorityGrantIssued")
        .unwrap();
    assert_eq!(issued.actor, actor("user-a"));
}

fn accept() -> Arc<Accept> {
    Arc::new(Accept {
        calls: AtomicUsize::new(0),
        fail: false,
    })
}

#[test]
fn grants_are_scoped_counted_expire_and_revoke_through_the_gateway() {
    let fx = Fx::new();
    let user = fx.ctx("user-a", PrincipalKind::Human);
    let grant = fx
        .mint(
            &user,
            "pkg-a",
            GrantKind::Operation,
            GrantBinding {
                operation: "runs.start".into(),
                action: None,
                request_digest: None,
                project_id: Some(ProjectId(id("project-a"))),
                root_id: None,
                policy_digest: None,
            },
            Some(2),
        )
        .unwrap();
    let pkg = fx
        .ctx("pkg-a", PrincipalKind::Package)
        .with_lineage(Some(ProjectId(id("project-a"))), None, None)
        .with_authority_source(grant.id.clone());
    let authority = fx.authority();
    let g = fx.gateway(&authority);
    let handler = accept();
    let run = |ctx: &InvocationContext, key: &str| {
        g.command(ctx, "runs.start", start(key).as_bytes(), handler.clone())
            .unwrap()
    };
    assert!(matches!(
        run(&pkg, "k-1"),
        CommandReply::Recorded {
            replayed: false,
            ..
        }
    ));
    assert_eq!(fx.grant(&grant.id).uses_remaining, Some(1));
    // A replay spends nothing.
    assert!(matches!(
        run(&pkg, "k-1"),
        CommandReply::Recorded { replayed: true, .. }
    ));
    assert_eq!(fx.grant(&grant.id).uses_remaining, Some(1));
    // Out of the grant's project scope.
    let other = fx
        .ctx("pkg-a", PrincipalKind::Package)
        .with_lineage(Some(ProjectId(id("project-b"))), None, None)
        .with_authority_source(grant.id.clone());
    assert!(matches!(
        run(&other, "k-2"),
        CommandReply::Rejected {
            code: Rejection::PermissionDenied,
            ..
        }
    ));
    assert!(matches!(
        run(&pkg, "k-3"),
        CommandReply::Recorded {
            replayed: false,
            ..
        }
    ));
    let spent = fx.grant(&grant.id);
    assert_eq!(
        (spent.uses_remaining, spent.state),
        (Some(0), GrantState::Consumed)
    );
    // A spent grant still resolves its own receipts (a lost acknowledgment), never a new one.
    assert!(matches!(
        run(&pkg, "k-3"),
        CommandReply::Recorded { replayed: true, .. }
    ));
    assert!(matches!(
        run(&pkg, "k-4"),
        CommandReply::Rejected {
            code: Rejection::PermissionDenied,
            ..
        }
    ));
    assert_eq!(handler.calls.load(Ordering::SeqCst), 2);

    // Expiry.
    let timed = fx
        .mint(
            &user,
            "pkg-a",
            GrantKind::Operation,
            GrantBinding {
                operation: "runs.start".into(),
                action: None,
                request_digest: None,
                project_id: None,
                root_id: None,
                policy_digest: None,
            },
            None,
        )
        .unwrap();
    let timed_ctx = fx
        .ctx("pkg-a", PrincipalKind::Package)
        .with_authority_source(timed.id.clone());
    assert!(matches!(
        run(&timed_ctx, "k-5"),
        CommandReply::Recorded { .. }
    ));
    fx.h.clock.0.store(T0 + 60_001, Ordering::SeqCst);
    assert!(matches!(
        run(&timed_ctx, "k-6"),
        CommandReply::Rejected {
            code: Rejection::PermissionDenied,
            ..
        }
    ));
    fx.h.clock.0.store(T0, Ordering::SeqCst);
    assert!(matches!(
        run(&timed_ctx, "k-7"),
        CommandReply::Recorded { .. }
    ));

    // Revocation is attributed and immediate; only an active grant can be revoked.
    let current = fx.grant(&timed.id).revision;
    let (u, tid) = (user.clone(), timed.id.clone());
    let revoked = fx
        .write(move |tx| Ok(auth::revoke_grant(tx, &u, &tid, current)))
        .unwrap();
    assert_eq!(
        (revoked.state, revoked.revoked_by),
        (GrantState::Revoked, Some(actor("user-a")))
    );
    assert!(matches!(
        run(&timed_ctx, "k-8"),
        CommandReply::Rejected {
            code: Rejection::PermissionDenied,
            ..
        }
    ));
    // A revoked caller is denied rather than handed its old receipt.
    assert!(matches!(
        run(&timed_ctx, "k-7"),
        CommandReply::Rejected {
            code: Rejection::PermissionDenied,
            ..
        }
    ));
    assert_eq!(fx.count("AuthorityGrantRevoked"), 1);
}

/// Runs a host write between authority resolution and the command's unit of work.
struct Interleave<'a> {
    inner: HostAuthority<'a>,
    between: Box<dyn Fn() + Send + Sync + 'a>,
}
impl AuthorityPort for Interleave<'_> {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        let resolved = self.inner.authorize(ctx, request);
        (self.between)();
        resolved
    }
}

#[test]
fn a_grant_revoked_between_authorize_and_commit_is_never_consumed() {
    let fx = Fx::new();
    let user = fx.ctx("user-a", PrincipalKind::Human);
    let grant = fx
        .mint(
            &user,
            "pkg-a",
            GrantKind::Operation,
            GrantBinding {
                operation: "runs.start".into(),
                action: None,
                request_digest: None,
                project_id: None,
                root_id: None,
                policy_digest: None,
            },
            Some(5),
        )
        .unwrap();
    let store = fx.store.clone();
    let (u, gid) = (user.clone(), grant.id.clone());
    let racing = Interleave {
        inner: fx.authority(),
        between: Box::new(move || {
            let (u, gid) = (u.clone(), gid.clone());
            run_write(&*store, WriteOptions::default(), move |tx| {
                let rev = tx.authority().authority_grant(&gid)?.unwrap().revision;
                auth::revoke_grant(tx, &u, &gid, rev).map_err(|_| StorageError::ScopeDenied)
            })
            .unwrap();
        }),
    };
    let handler = accept();
    let ctx = fx
        .ctx("pkg-a", PrincipalKind::Package)
        .with_authority_source(grant.id.clone());
    let reply = fx
        .gateway(&racing)
        .command(
            &ctx,
            "runs.start",
            start("race-1").as_bytes(),
            handler.clone(),
        )
        .unwrap();
    assert!(
        matches!(
            reply,
            CommandReply::Rejected {
                code: Rejection::PermissionDenied,
                ..
            }
        ),
        "{reply:?}"
    );
    let after = fx.grant(&grant.id);
    // Only the revocation changed it: no use was spent, and the decision left nothing behind.
    assert_eq!(
        (after.state, after.uses_remaining, after.revision),
        (GrantState::Revoked, Some(5), 2)
    );
    assert_eq!(fx.count("CommandDecided"), 0);
    let ops = read_snapshot(&*fx.store, &ReadOptions::default(), |s| {
        s.operations().unresolved(100)
    })
    .unwrap();
    assert!(ops.is_empty(), "the use case's writes were rolled back");
}

#[test]
fn a_principal_or_policy_change_between_authorize_and_commit_fences_the_command() {
    let fx = Fx::new();
    let user = fx.ctx("user-a", PrincipalKind::Human);
    // Revoked principal.
    let store = fx.store.clone();
    let u = user.clone();
    let revoke = Interleave {
        inner: fx.authority(),
        between: Box::new(move || {
            let u = u.clone();
            run_write(&*store, WriteOptions::default(), move |tx| {
                let rev = tx
                    .authority()
                    .principal(&actor("agent-r"))?
                    .unwrap()
                    .revision;
                auth::revoke_principal(tx, &u, &actor("agent-r"), rev)
                    .map_err(|_| StorageError::ScopeDenied)
            })
            .unwrap();
        }),
    };
    let handler = accept();
    let reply = fx
        .gateway(&revoke)
        .command(
            &fx.agent("agent-r"),
            "runs.start",
            start("p-1").as_bytes(),
            handler.clone(),
        )
        .unwrap();
    assert!(matches!(
        reply,
        CommandReply::Rejected {
            code: Rejection::PermissionDenied,
            ..
        }
    ));
    assert_eq!(
        handler.calls.load(Ordering::SeqCst),
        0,
        "fenced before the use case ran"
    );
    assert!(!fx.principal("agent-r").active);
    assert_eq!(fx.count("PrincipalAuthorityRevoked"), 1);

    // Narrowed policy (shell tools turned off) committed after authorization.
    let store = fx.store.clone();
    let schema = serde_json::to_value(descriptor("tools.shell").unwrap().schema_ref()).unwrap();
    let narrow = Interleave {
        inner: fx.authority(),
        between: Box::new(move || {
            let schema = schema.clone();
            run_write(&*store, WriteOptions::default(), move |tx| {
                let current = tx
                    .configuration()
                    .current(&ConfigScope::Workspace, "settings")?;
                let expected = current.map_or(1, |c| c.revision);
                tx.configuration().append_revision(
                    &ConfigScope::Workspace,
                    "settings",
                    expected,
                    1,
                    json!({"values": {"tools.shell": {"schema": schema, "value": false}}}),
                )
            })
            .unwrap();
        }),
    };
    let reply = fx
        .gateway(&narrow)
        .command(
            &fx.agent("agent-a"),
            "runs.start",
            start("p-2").as_bytes(),
            handler.clone(),
        )
        .unwrap();
    assert!(matches!(
        reply,
        CommandReply::Rejected {
            code: Rejection::PermissionDenied,
            ..
        }
    ));
    assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
    // An unrelated committed change does not fence: the same agent, fresh authority, proceeds.
    let authority = fx.authority();
    let reply = fx
        .gateway(&authority)
        .command(
            &fx.agent("agent-a"),
            "runs.start",
            start("p-3").as_bytes(),
            handler.clone(),
        )
        .unwrap();
    assert!(matches!(reply, CommandReply::Recorded { .. }));
}

#[test]
fn concurrent_commands_never_spend_a_grant_beyond_its_uses() {
    let fx = Fx::new();
    let user = fx.ctx("user-a", PrincipalKind::Human);
    let grant = fx
        .mint(
            &user,
            "pkg-a",
            GrantKind::Operation,
            GrantBinding {
                operation: "runs.start".into(),
                action: None,
                request_digest: None,
                project_id: None,
                root_id: None,
                policy_digest: None,
            },
            Some(3),
        )
        .unwrap();
    let recorded = Mutex::new(0);
    std::thread::scope(|s| {
        for i in 0..12 {
            let (fx, recorded, gid) = (&fx, &recorded, grant.id.clone());
            s.spawn(move || {
                let authority = fx.authority();
                let ctx = fx
                    .ctx("pkg-a", PrincipalKind::Package)
                    .with_authority_source(gid);
                let reply = fx
                    .gateway(&authority)
                    .command(
                        &ctx,
                        "runs.start",
                        start(&format!("c-{i}")).as_bytes(),
                        accept(),
                    )
                    .unwrap();
                if matches!(reply, CommandReply::Recorded { .. }) {
                    *recorded.lock().unwrap() += 1;
                }
            });
        }
    });
    // Racers that resolved the same grant revision lose the compare-and-set and are denied;
    // every recorded command spent exactly one use.
    let first = *recorded.lock().unwrap();
    let remaining = fx.grant(&grant.id).uses_remaining.unwrap();
    assert!((1..=3).contains(&first));
    assert_eq!(first as i64, 3 - remaining);
    assert_eq!(fx.count("CommandDecided"), first);
    // Fresh authority admits the remaining uses, and not one more.
    let authority = fx.authority();
    let ctx = fx
        .ctx("pkg-a", PrincipalKind::Package)
        .with_authority_source(grant.id.clone());
    let mut later = 0;
    for i in 0..5 {
        let reply = fx
            .gateway(&authority)
            .command(
                &ctx,
                "runs.start",
                start(&format!("s-{i}")).as_bytes(),
                accept(),
            )
            .unwrap();
        later += usize::from(matches!(reply, CommandReply::Recorded { .. }));
    }
    assert_eq!(first + later, 3);
    assert_eq!(fx.grant(&grant.id).state, GrantState::Consumed);
    assert_eq!(fx.count("CommandDecided"), 3);
}

/// Answers through the real settings service, revoking the caller while the answer is prepared.
struct RevokeDuringRead<'a> {
    fx: &'a Fx,
    revoke: bool,
}
impl QueryHandler for RevokeDuringRead<'_> {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let answer = self.fx.settings.query(ctx, query, snapshot);
        if self.revoke {
            let user = self.fx.ctx("user-a", PrincipalKind::Human);
            self.fx.write(move |tx| {
                let rev = tx
                    .authority()
                    .principal(&actor("agent-a"))?
                    .unwrap()
                    .revision;
                auth::revoke_principal(tx, &user, &actor("agent-a"), rev)
                    .map_err(|_| StorageError::ScopeDenied)
            });
        }
        answer
    }
}

#[test]
fn a_read_revoked_before_publication_is_not_published() {
    let fx = Fx::new();
    let authority = fx.authority();
    let g = fx.gateway(&authority);
    let ctx = fx.agent("agent-a");
    let published = g
        .query(
            &ctx,
            "settings.read_effective",
            READ_SETTINGS.as_bytes(),
            &RevokeDuringRead {
                fx: &fx,
                revoke: false,
            },
        )
        .unwrap();
    assert!(matches!(published, DomainQueryReply::Result { .. }));
    let fenced = g
        .query(
            &ctx,
            "settings.read_effective",
            READ_SETTINGS.as_bytes(),
            &RevokeDuringRead {
                fx: &fx,
                revoke: true,
            },
        )
        .unwrap();
    match fenced {
        DomainQueryReply::Rejected { fault, .. } => {
            assert!(matches!(fault.code, DomainErrorCode::PermissionDenied))
        }
        other => panic!("published after revocation: {other:?}"),
    }
}

/// A uniquely named disposable directory, removed on drop.
pub(crate) struct Scratch(pub std::path::PathBuf);
impl Scratch {
    pub(crate) fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "devworks-authority-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    pub(crate) fn path(&self, rel: &str) -> String {
        self.0.join(rel).to_string_lossy().into_owned()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn register_repository(fx: &Fx, rid: &str, location: &str) {
    let (rid, location) = (rid.to_owned(), location.to_owned());
    fx.store
        .with_writer_connection(move |c| {
            super::super::conn::db(c.execute(
                "INSERT INTO repositories(id,canonical_location,revision) VALUES(?1,?2,1)",
                rusqlite::params![rid, location],
            ))
            .map(|_| ())
        })
        .unwrap();
}
/// The protected roots as the host reads them: from the store, bound to its identity.
fn roots(fx: &Fx) -> ProtectedRoots {
    ProtectedRoots::read(&*fx.store, Vec::new(), None).unwrap()
}

#[test]
fn a_blocked_destructive_step_is_recorded_with_rationale_and_attribution() {
    let fx = Fx::new();
    let scratch = Scratch::new("history");
    let repo = scratch.path("repo");
    std::fs::create_dir_all(&repo).unwrap();
    register_repository(&fx, "repo-a", &repo);
    let fs = NativeFilesystem::new();
    let decision = destructive::preflight(
        &fs,
        &roots(&fx),
        &EffectPlan {
            kind: DestructiveKind::RemoveTree,
            targets: vec!["../repo".into()],
            working_directory: Some(scratch.path("repo")),
            scope: IntendedScope::Ordinary,
        },
    );
    assert_eq!(
        decision.verdict,
        PreflightVerdict::Blocked(BlockReason::PrimaryRepository {
            repository_id: id("repo-a")
        })
    );
    // The agent acts under a linked user approval: still attributed to the agent.
    let ctx = fx
        .agent("agent-a")
        .with_lineage(
            Some(ProjectId(id("project-a"))),
            Some(devworks_core::protocol::RootId(id("root-a"))),
            None,
        )
        .with_authority_source(id("approval-1"));
    fx.write(move |tx| destructive::record_refusal(tx, &ctx, "process.execute", &decision));
    let event = fx
        .events()
        .into_iter()
        .find(|e| e.kind == "DestructiveOperationBlocked")
        .unwrap();
    assert_eq!(event.actor, actor("agent-a"));
    assert_eq!(event.project_id, Some(ProjectId(id("project-a"))));
    assert_eq!(event.payload["reason"], "PrimaryRepository");
    assert_eq!(event.payload["repositoryId"], "repo-a");
    assert_eq!(event.payload["authoritySources"], json!(["approval-1"]));
    assert!(
        event.payload["rationale"]
            .as_str()
            .unwrap()
            .contains("dedicated")
    );
    assert!(std::path::Path::new(&repo).exists());
}

// Primary deletion needs stable directory identities, which this adapter has only on Unix.
#[cfg(unix)]
#[test]
fn primary_repository_deletion_is_only_the_users_dedicated_confirmation() {
    let fx = Fx::new();
    let scratch = Scratch::new("deletion");
    let repo = scratch.path("repo");
    std::fs::create_dir_all(format!("{repo}/.git/objects")).unwrap();
    std::fs::write(format!("{repo}/src.txt"), b"source").unwrap();
    register_repository(&fx, "repo-a", &repo);
    let fs = NativeFilesystem::new();
    let user = fx.ctx("user-a", PrincipalKind::Human);
    let agent = fx.agent("agent-a");
    let deps = || RepositoryDependencies {
        project_references: vec![ProjectId(id("project-a"))],
        ..Default::default()
    };
    // Anyone may prepare the concrete preview; it confers nothing.
    let preview = deletion::prepare(&fs, &roots(&fx), &id("repo-a"), deps()).unwrap();
    assert!(preview.blockers.is_empty());
    assert_eq!(preview.retained, deletion::RETAINED);

    // An agent cannot confirm, even holding an approval for the operation it would use.
    let (a, p) = (agent.clone(), preview.clone());
    assert!(matches!(
        deletion::confirm(&*fx.store, &a, &p, T0 + 60_000),
        Err(DeletionRefusal::HumanOnly)
    ));
    let approval = fx
        .mint(
            &user,
            "agent-a",
            GrantKind::Operation,
            GrantBinding {
                operation: "artifacts.delete".into(),
                action: None,
                request_digest: None,
                project_id: None,
                root_id: None,
                policy_digest: None,
            },
            Some(1),
        )
        .unwrap();
    let (a, p, gid) = (
        agent.clone().with_authority_source(approval.id.clone()),
        preview.clone(),
        approval.id.clone(),
    );
    assert!(matches!(
        deletion::authorize(&*fx.store, &a, &gid, &p),
        Err(DeletionRefusal::HumanOnly)
    ));
    // The user's operation approval is not the dedicated confirmation either.
    let (u, p, gid) = (user.clone(), preview.clone(), approval.id.clone());
    assert!(matches!(
        deletion::authorize(&*fx.store, &u, &gid, &p),
        Err(DeletionRefusal::ConfirmationUnavailable)
    ));
    // Ordinary preflight refuses the repository root for everyone.
    let ordinary = destructive::preflight(
        &fs,
        &roots(&fx),
        &EffectPlan {
            kind: DestructiveKind::RemoveTree,
            targets: vec![repo.clone()],
            working_directory: None,
            scope: IntendedScope::Ordinary,
        },
    );
    assert!(matches!(
        ordinary.verdict,
        PreflightVerdict::Blocked(BlockReason::PrimaryRepository { .. })
    ));

    // The user confirms; a changed target afterwards blocks instead of enlarging it.
    let (u, p) = (user.clone(), preview.clone());
    let confirmation = deletion::confirm(&*fx.store, &u, &p, T0 + 60_000).unwrap();
    let moved = deletion::prepare(
        &fs,
        &ProtectedRoots::new(
            vec![RegisteredRepository {
                id: id("repo-a"),
                location: repo.clone(),
                revision: 2,
            }],
            Vec::new(),
            None,
        ),
        &id("repo-a"),
        deps(),
    )
    .unwrap();
    let (u, m, gid) = (user.clone(), moved, confirmation.id.clone());
    assert!(matches!(
        deletion::authorize(&*fx.store, &u, &gid, &m),
        Err(DeletionRefusal::Changed)
    ));
    // Another live project reference blocks until the user resolves it separately.
    let shared = deletion::prepare(
        &fs,
        &roots(&fx),
        &id("repo-a"),
        RepositoryDependencies {
            project_references: vec![ProjectId(id("project-a")), ProjectId(id("project-b"))],
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!shared.blockers.is_empty());

    // Fresh preview, matching confirmation: the permit passes the guard for exactly this root.
    let current = deletion::prepare(&fs, &roots(&fx), &id("repo-a"), deps()).unwrap();
    let (u, c, gid) = (user.clone(), current.clone(), confirmation.id.clone());
    let permit = deletion::authorize(&*fx.store, &u, &gid, &c).unwrap();
    assert_eq!(fx.grant(&confirmation.id).state, GrantState::Consumed);
    let registry = roots(&fx);
    let plan = EffectPlan {
        kind: DestructiveKind::RemoveTree,
        targets: vec![repo.clone()],
        working_directory: None,
        scope: IntendedScope::PrimaryRepositoryDeletion(permit),
    };
    let decision = destructive::preflight(&fs, &registry, &plan);
    assert_eq!(decision.verdict, PreflightVerdict::Allowed);
    destructive::revalidate(&fs, &decision, &registry.revision).unwrap();
    let target = &decision.allowed_targets().unwrap()[0];
    let outcome = fs
        .execute(
            target,
            DestructiveKind::RemoveTree,
            decision.protected_identities(),
        )
        .unwrap();
    assert!(outcome.complete, "{outcome:?}");
    assert!(!std::path::Path::new(&repo).exists());
    let IntendedScope::PrimaryRepositoryDeletion(permit) = plan.scope else {
        unreachable!()
    };
    let u = user.clone();
    deletion::record_outcome(&*fx.store, &u, &permit, &outcome).unwrap();
    assert_eq!(fx.count("RepositoryDeletionCompleted"), 1);
    // The confirmation was single use.
    let (u, c, gid) = (user.clone(), current, confirmation.id.clone());
    assert!(matches!(
        deletion::authorize(&*fx.store, &u, &gid, &c),
        Err(DeletionRefusal::ConfirmationUnavailable)
    ));
    assert_eq!(fx.count("RepositoryDeletionConfirmed"), 1);
    assert_eq!(fx.count("RepositoryDeletionAuthorized"), 1);
}

#[test]
fn host_roles_resolve_only_active_agent_bindings_and_settings_fail_closed_otherwise() {
    let fx = Fx::new();
    let roles = HostRoles {
        store: fx.store.clone(),
    };
    assert_eq!(roles.role(&fx.agent("agent-a")), Some(AgentRole::Worker));
    assert_eq!(
        roles.role(&fx.agent("reviewer-a")),
        Some(AgentRole::CodeReviewer)
    );
    assert_eq!(roles.role(&fx.agent("agent-x")), None, "unbound");
    assert_eq!(roles.role(&fx.ctx("user-a", PrincipalKind::Human)), None);
    assert_eq!(roles.role(&fx.ctx("pkg-a", PrincipalKind::Package)), None);
    let user = fx.ctx("user-a", PrincipalKind::Human);
    let rev = fx.principal("agent-r").revision;
    fx.write(move |tx| Ok(auth::revoke_principal(tx, &user, &actor("agent-r"), rev)))
        .unwrap();
    assert_eq!(roles.role(&fx.agent("agent-r")), None, "revoked");
    // The settings proposal seam refuses an unresolved role and accepts a bound worker.
    let schema = serde_json::to_value(descriptor("power.keepAwake").unwrap().schema_ref()).unwrap();
    let entries: Vec<devworks_core::protocol::records::SettingAssignment> = vec![
        serde_json::from_value(
            json!({"key": "power.keepAwake", "value": {"schema": schema, "value": true}}),
        )
        .unwrap(),
    ];
    let stage = |who: &str| {
        let (ctx, entries, roles) = (
            fx.agent(who),
            entries.clone(),
            HostRoles {
                store: fx.store.clone(),
            },
        );
        fx.write(move |tx| {
            Ok(svc::stage_proposal(
                tx,
                &ctx,
                &roles,
                &ConfigScope::Workspace,
                1,
                &entries,
            ))
        })
    };
    match stage("agent-r") {
        Err(svc::Failure::Refused(r)) => assert_eq!(r.target_kind, "role_unresolved"),
        other => panic!("{other:?}"),
    }
    match stage("reviewer-a") {
        Err(svc::Failure::Refused(r)) => assert_eq!(r.target_kind, "read_only_role"),
        other => panic!("{other:?}"),
    }
    assert!(stage("agent-a").is_ok());
}

// --- the handler view of the authority and grant ledgers ---------------------------------------

use devworks_core::gateway::AdmittedCommand;
use devworks_core::ports::{CommandHandler, Decided, GrantUse, HandlerFailure, IdKind};
use devworks_core::protocol::results::DomainDecision;
use devworks_core::protocol::{OperationId, RootId};

/// Records an operation intent and accepts, so the use case's other writes commit with it.
fn accept_in(tx: &mut dyn WriteTransaction) -> Result<Decided, HandlerFailure> {
    let op = OperationId(tx.new_id(IdKind::Operation));
    tx.operations().record_intent(NewOperation {
        id: op.clone(),
        root_id: Some(RootId(id("root-a"))),
        assignment_id: None,
        capability: "runs.start".into(),
        schema_version: 1,
        args_digest: "d".into(),
        payload: json!({}),
    })?;
    Ok(Decided {
        decision: DomainDecision::Accepted {
            operation_id: op.clone(),
        },
        operation_id: Some(op),
    })
}

fn refused<T: std::fmt::Debug>(r: Result<T, StorageError>) -> bool {
    matches!(r, Err(StorageError::InvalidRecord(m)) if m.contains("authority seams"))
}

/// A use case that tries every raw authority and grant write, and the reads it may use.
#[derive(Default)]
struct RawWriter {
    outcomes: Mutex<Vec<(&'static str, bool)>>,
}
impl CommandHandler for RawWriter {
    fn decide(
        &self,
        _: &InvocationContext,
        _: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let mut out = self.outcomes.lock().unwrap();
        let grant = tx.authority().authority_grant(&id("grant-pkg"))?.unwrap();
        out.push((
            "read principal",
            tx.authority().principal(&actor("agent-a"))?.is_some(),
        ));
        out.push(("read grant", grant.state == GrantState::Active));
        out.push((
            "read resume grant",
            tx.grants().grant(&id("resume-1"))?.is_some(),
        ));
        out.push((
            "bind",
            refused(tx.authority().bind_principal(
                NewPrincipalBinding {
                    actor: actor("agent-x"),
                    kind: PrincipalKind::Agent,
                    role: Some(AgentRole::Worker),
                    project_id: None,
                    tool_families: None,
                    bound_by: actor("user-a"),
                },
                None,
            )),
        ));
        out.push((
            "revoke principal",
            refused(tx.authority().revoke_principal(&actor("agent-a"), 1)),
        ));
        out.push((
            "issue",
            refused(tx.authority().issue_grant(NewAuthorityGrant {
                id: id("grant-self"),
                actor: actor("pkg-a"),
                kind: GrantKind::Operation,
                issued_by: actor("user-a"),
                binding: json!({"operation": "runs.start"}),
                expires_at_ms: None,
                uses: None,
            })),
        ));
        out.push((
            "revoke grant",
            refused(
                tx.authority()
                    .revoke_grant(&grant.id, grant.revision, &actor("user-a")),
            ),
        ));
        let spend = GrantUse {
            grant_id: grant.id.clone(),
            expected_revision: grant.revision,
        };
        out.push((
            "consume",
            refused(tx.authority().consume_grant(&spend, &actor("pkg-a"))),
        ));
        out.push((
            "issue resume",
            refused(tx.grants().issue(NewGrant {
                id: id("resume-2"),
                actor: actor("pkg-a"),
                root_id: RootId(id("root-a")),
                authority_source: "handler".into(),
                binding: json!({}),
                expires_at_ms: None,
                uses: None,
            })),
        ));
        let resume = GrantUse {
            grant_id: id("resume-1"),
            expected_revision: 1,
        };
        out.push((
            "consume resume",
            refused(tx.grants().consume(&resume, &actor("user-a"))),
        ));
        drop(out);
        accept_in(tx)
    }
}

#[test]
fn a_use_case_cannot_write_authority_or_grant_records_directly() {
    let fx = Fx::new();
    let user = fx.ctx("user-a", PrincipalKind::Human);
    let grant = fx
        .mint(
            &user,
            "pkg-a",
            GrantKind::Operation,
            GrantBinding {
                operation: "runs.start".into(),
                action: None,
                request_digest: None,
                project_id: None,
                root_id: None,
                policy_digest: None,
            },
            Some(2),
        )
        .unwrap();
    // Re-key the minted grant under a fixed ID the handler can address.
    let gid = grant.id.clone();
    fx.store
        .with_writer_connection(move |c| {
            super::super::conn::db(c.execute(
                "UPDATE authority_grants SET id='grant-pkg' WHERE id=?1",
                [gid.as_str()],
            ))
            .map(|_| ())
        })
        .unwrap();
    fx.write(|tx| {
        tx.grants().issue(NewGrant {
            id: id("resume-1"),
            actor: actor("user-a"),
            root_id: RootId(id("root-a")),
            authority_source: "explicit-user-instruction".into(),
            binding: json!({"rootId": "root-a"}),
            expires_at_ms: None,
            uses: Some(1),
        })
    });
    let before = (fx.principal("agent-a"), fx.events().len());
    let handler = Arc::new(RawWriter::default());
    let authority = fx.authority();
    let ctx = fx
        .ctx("pkg-a", PrincipalKind::Package)
        .with_authority_source(id("grant-pkg"));
    let reply = fx
        .gateway(&authority)
        .command(
            &ctx,
            "runs.start",
            start("raw-1").as_bytes(),
            handler.clone(),
        )
        .unwrap();
    assert!(matches!(reply, CommandReply::Recorded { .. }), "{reply:?}");
    let outcomes = handler.outcomes.lock().unwrap().clone();
    assert_eq!(outcomes.len(), 10);
    for (what, ok) in outcomes {
        assert!(
            ok,
            "{what}: reads are served and every raw write is refused"
        );
    }
    // Nothing the handler attempted was written; the gateway still spent exactly one use.
    let after = fx.grant(&id("grant-pkg"));
    assert_eq!(
        (after.state, after.uses_remaining, after.revision),
        (GrantState::Active, Some(1), 2)
    );
    assert_eq!(fx.principal("agent-a"), before.0);
    let read = |gid: &str| {
        let gid = id(gid);
        read_snapshot(&*fx.store, &ReadOptions::default(), |s| {
            Ok((
                s.authority().authority_grant(&gid)?,
                s.grants().grant(&gid)?,
                s.authority().principal(&actor("agent-x"))?,
            ))
        })
        .unwrap()
    };
    let (self_issued, resume_two, bound) = read("grant-self");
    assert!(self_issued.is_none() && bound.is_none());
    assert!(read("resume-2").1.is_none() && resume_two.is_none());
    let resume = read("resume-1").1.unwrap();
    assert_eq!((resume.revision, resume.uses_remaining), (1, Some(1)));
    // Only the gateway's own history was added.
    let added: Vec<String> = fx.events()[before.1..]
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert_eq!(added, ["CommandDecided"]);
}

/// A use case that creates and revokes authority through the seams, optionally acting under a
/// context it was handed (a forgery) and optionally through a lying transaction wrapper.
struct Seams {
    forged: Option<InvocationContext>,
    lying: bool,
    outcomes: Mutex<Vec<(&'static str, bool)>>,
}
impl Seams {
    fn new(forged: Option<InvocationContext>, lying: bool) -> Arc<Self> {
        Arc::new(Self {
            forged,
            lying,
            outcomes: Mutex::new(Vec::new()),
        })
    }
    fn run(&self, ctx: &InvocationContext, tx: &mut dyn WriteTransaction) {
        let mut out = self.outcomes.lock().unwrap();
        let binding = GrantBinding {
            operation: "runs.start".into(),
            action: None,
            request_digest: None,
            project_id: None,
            root_id: None,
            policy_digest: None,
        };
        let bounds = GrantBounds {
            expires_at_ms: T0 + 60_000,
            uses: 1,
        };
        let minted = auth::mint_grant(
            tx,
            ctx,
            actor("pkg-a"),
            GrantKind::Operation,
            binding,
            bounds,
        );
        out.push(("mint", minted.is_ok()));
        if let Ok(g) = minted {
            out.push((
                "revoke grant",
                auth::revoke_grant(tx, ctx, &g.id, g.revision).is_ok(),
            ));
        }
        let bound = auth::bind_principal(
            tx,
            ctx,
            NewPrincipalBinding {
                actor: actor("agent-x"),
                kind: PrincipalKind::Agent,
                role: Some(AgentRole::Worker),
                project_id: None,
                tool_families: None,
                bound_by: actor("user-a"),
            },
            None,
        );
        out.push(("bind", bound.is_ok()));
        out.push((
            "revoke principal",
            auth::revoke_principal(tx, ctx, &actor("agent-a"), 1).is_ok(),
        ));
    }
}
impl CommandHandler for Seams {
    fn decide(
        &self,
        ctx: &InvocationContext,
        _: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let ctx = self.forged.as_ref().unwrap_or(ctx);
        if self.lying {
            self.run(ctx, &mut Lying { inner: &mut *tx });
        } else {
            self.run(ctx, tx);
        }
        accept_in(tx)
    }
}

/// A foreign transaction wrapper that forwards everything and claims the user's principal for
/// every authority write.
struct Lying<'a> {
    inner: &'a mut dyn WriteTransaction,
}
impl WriteTransaction for Lying<'_> {
    fn now_ms(&self) -> i64 {
        self.inner.now_ms()
    }
    fn new_id(&mut self, kind: IdKind) -> Id {
        self.inner.new_id(kind)
    }
    fn store(&self) -> &StoreInfo {
        self.inner.store()
    }
    fn committed_sequence(&self) -> Result<i64, StorageError> {
        self.inner.committed_sequence()
    }
    fn savepoint(&mut self) -> Result<Savepoint, StorageError> {
        self.inner.savepoint()
    }
    fn rollback_to(&mut self, savepoint: Savepoint) -> Result<(), StorageError> {
        self.inner.rollback_to(savepoint)
    }
    fn release(&mut self, savepoint: Savepoint) -> Result<(), StorageError> {
        self.inner.release(savepoint)
    }
    fn actors(&mut self) -> &mut dyn ActorRegistry {
        self.inner.actors()
    }
    fn receipts(&mut self) -> &mut dyn CommandLedger {
        self.inner.receipts()
    }
    fn history(&mut self) -> &mut dyn HistoryLog {
        self.inner.history()
    }
    fn operations(&mut self) -> &mut dyn OperationJournal {
        self.inner.operations()
    }
    fn queue(&mut self) -> &mut dyn DurableQueue {
        self.inner.queue()
    }
    fn grants(&mut self) -> &mut dyn GrantLedger {
        self.inner.grants()
    }
    fn packages(&mut self) -> &mut dyn PackageHandlerLedger {
        self.inner.packages()
    }
    fn configuration(&mut self) -> &mut dyn ConfigurationRepository {
        self.inner.configuration()
    }
    fn authority(&mut self) -> &mut dyn AuthorityLedger {
        self.inner.authority()
    }
    fn authority_writes(
        &mut self,
        capability: &devworks_core::authority::AuthorityCapability,
        _: &ActorId,
    ) -> Result<&mut dyn AuthorityLedger, StorageError> {
        self.inner.authority_writes(capability, &actor("user-a"))
    }
    fn grant_writes(
        &mut self,
        capability: &devworks_core::authority::AuthorityCapability,
        _: &ActorId,
    ) -> Result<&mut dyn GrantLedger, StorageError> {
        self.inner.grant_writes(capability, &actor("user-a"))
    }
}

fn seams_run(
    fx: &Fx,
    ingress: &InvocationContext,
    handler: Arc<Seams>,
    key: &str,
) -> Vec<(&'static str, bool)> {
    let authority = fx.authority();
    let reply = fx
        .gateway(&authority)
        .command(
            ingress,
            "runs.start",
            start(key).as_bytes(),
            handler.clone(),
        )
        .unwrap();
    assert!(matches!(reply, CommandReply::Recorded { .. }), "{reply:?}");
    handler.outcomes.lock().unwrap().clone()
}

#[test]
fn authority_seams_write_only_for_the_user_or_host_ingress_principal() {
    let fx = Fx::new();
    let user = fx.ctx("user-a", PrincipalKind::Human);
    let before = fx.events().len();
    // An agent-invoked command: the handler is refused under the real context, under a forged
    // user context and through a lying wrapper, and nothing is written.
    for (name, handler) in [
        ("plain", Seams::new(None, false)),
        ("forged", Seams::new(Some(user.clone()), false)),
        ("lying", Seams::new(Some(user.clone()), true)),
    ] {
        let outcomes = seams_run(&fx, &fx.agent("agent-r"), handler, &format!("agent-{name}"));
        assert!(!outcomes.is_empty());
        assert!(outcomes.iter().all(|(_, ok)| !ok), "{name}: {outcomes:?}");
    }
    let added: Vec<String> = fx.events()[before..]
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert!(added.iter().all(|k| k == "CommandDecided"), "{added:?}");
    assert!(fx.principal("agent-a").active);
    // A user-ingress command may also not write as another principal it names.
    let other = fx.ctx("host", PrincipalKind::Host);
    let outcomes = seams_run(&fx, &user, Seams::new(Some(other), false), "user-as-host");
    assert!(outcomes.iter().all(|(_, ok)| !ok), "{outcomes:?}");
    // The user's own command: the seams write, attributed to the user, and commit with it.
    let outcomes = seams_run(&fx, &user, Seams::new(None, false), "user-own");
    assert_eq!(outcomes.len(), 4);
    assert!(outcomes.iter().all(|(_, ok)| *ok), "{outcomes:?}");
    let x = fx.principal("agent-x");
    assert_eq!((x.active, x.bound_by.clone()), (true, actor("user-a")));
    assert!(!fx.principal("agent-a").active);
    for kind in [
        "AuthorityGrantIssued",
        "AuthorityGrantRevoked",
        "PrincipalAuthorityBound",
        "PrincipalAuthorityRevoked",
    ] {
        assert!(fx.count(kind) >= 1, "{kind}");
    }
}

// --- typed denials, bounded grants, residual risk and redaction ------------------------------

#[test]
fn a_policy_denial_carries_its_typed_reason_and_a_denied_command_is_recorded() {
    let fx = Fx::new();
    let authority = fx.authority();
    let reviewer = fx
        .agent("reviewer-a")
        .with_lineage(Some(ProjectId(id("project-a"))), None, None)
        .with_authority_source(id("approval-9"));
    let reply = fx
        .cases
        .command(
            &fx.gateway(&authority),
            &reviewer,
            "settings.save_revision",
            fx.save_frame(
                json!({"kind": "Workspace"}),
                1,
                "power.keepAwake",
                json!(true),
            )
            .as_bytes(),
        )
        .unwrap();
    let CommandReply::Rejected {
        code: Rejection::PermissionDenied,
        correlation_id,
        ..
    } = reply
    else {
        panic!("{reply:?}")
    };
    let denied: Vec<EventRecord> = fx
        .events()
        .into_iter()
        .filter(|e| e.kind == "AuthorityDenied")
        .collect();
    assert_eq!(denied.len(), 1);
    let e = &denied[0];
    assert_eq!(e.aggregate_id, correlation_id.0.as_str());
    assert_eq!(e.actor, actor("reviewer-a"));
    assert_eq!(e.project_id, Some(ProjectId(id("project-a"))));
    assert_eq!(e.payload["reason"], "ReadOnlyRole");
    assert_eq!(e.payload["operation"], "settings.save_revision");
    assert_eq!(e.payload["authoritySources"], json!(["approval-9"]));
    assert!(
        e.payload["rationale"]
            .as_str()
            .unwrap()
            .contains("read-only")
    );
    // No receipt and no work: only the denial record.
    assert_eq!(fx.count("CommandDecided"), 0);
    // A refused query carries the typed reason in its fault.
    match fx
        .gateway(&authority)
        .query(&reviewer, "web.fetch", FETCH.as_bytes(), &*fx.settings)
        .unwrap()
    {
        DomainQueryReply::Rejected { fault, .. } => {
            assert!(matches!(fault.code, DomainErrorCode::PermissionDenied));
            assert_eq!(fault.target_kind.unwrap().0, "authority:UnverifiedRead");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn grants_need_explicit_bounded_expiry_and_uses() {
    let fx = Fx::new();
    let user = fx.ctx("user-a", PrincipalKind::Human);
    let binding = || GrantBinding {
        operation: "runs.start".into(),
        action: None,
        request_digest: None,
        project_id: None,
        root_id: None,
        policy_digest: None,
    };
    let mint = |expires_at_ms: i64, uses: i64| {
        let (u, b) = (user.clone(), binding());
        fx.write(move |tx| {
            Ok(auth::mint_grant(
                tx,
                &u,
                actor("pkg-a"),
                GrantKind::Operation,
                b,
                GrantBounds {
                    expires_at_ms,
                    uses,
                },
            ))
        })
    };
    for (expires, uses, why) in [
        (T0 + 60_000, 0, "no uses"),
        (T0 + 60_000, auth::MAX_GRANT_USES + 1, "too many uses"),
        (T0, 1, "already expired"),
        (T0 + auth::MAX_GRANT_LIFETIME_MS + 1, 1, "lifetime too long"),
    ] {
        assert!(
            matches!(mint(expires, uses), Err(AdminError::Invalid(_))),
            "{why}"
        );
    }
    let ok = mint(T0 + auth::MAX_GRANT_LIFETIME_MS, auth::MAX_GRANT_USES).unwrap();
    assert_eq!(
        (ok.expires_at_ms, ok.uses_remaining),
        (
            Some(T0 + auth::MAX_GRANT_LIFETIME_MS),
            Some(auth::MAX_GRANT_USES)
        )
    );
}

#[test]
fn an_unsupported_stored_profile_also_denies_reads_for_non_human_principals() {
    let fx = Fx::new();
    let schema =
        serde_json::to_value(descriptor("safety.accessProfile").unwrap().schema_ref()).unwrap();
    fx.write(move |tx| {
        tx.configuration().append_revision(
            &ConfigScope::Workspace,
            "settings",
            1,
            1,
            json!({"values": {"safety.accessProfile": {"schema": schema, "value": "sandboxed"}}}),
        )
    });
    for ctx in [fx.agent("agent-a"), fx.ctx("pkg-a", PrincipalKind::Package)] {
        assert_eq!(
            reason(decide(
                &fx,
                &ctx,
                "settings.read_effective",
                Mode::Query,
                READ_SETTINGS
            )),
            Some(DenialReason::UnsupportedProfile)
        );
    }
    // The user can still read and repair the setting.
    assert!(
        decide(
            &fx,
            &fx.ctx("user-a", PrincipalKind::Human),
            "settings.read_effective",
            Mode::Query,
            READ_SETTINGS
        )
        .is_ok()
    );
}

#[test]
fn unrecognized_commands_record_residual_risk_under_full_access() {
    use devworks_core::authority::shell::{assess_command, record_assessment};
    let fx = Fx::new();
    let token = "ghp_0123456789abcdefABCDEF0123";
    let opaque: Vec<String> = ["python3", "-c", token]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let forced: Vec<String> = ["git", "push", "-f", "origin", "main"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let ctx = fx.agent("agent-a");
    for (argv, profile) in [
        (opaque.clone(), AccessProfile::Full),
        (opaque.clone(), AccessProfile::ProjectScope),
        (forced.clone(), AccessProfile::Full),
    ] {
        let (ctx, argv) = (ctx.clone(), argv);
        fx.write(move |tx| {
            let assessment = assess_command(&argv, None);
            record_assessment(tx, &ctx, "process.execute", &argv, &assessment, profile)
        });
    }
    let residual: Vec<EventRecord> = fx
        .events()
        .into_iter()
        .filter(|e| e.aggregate_kind == "shell_preflight")
        .collect();
    let kinds: Vec<&str> = residual.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        ["NativeExecutionResidualRisk", "DestructiveOperationBlocked"]
    );
    assert_eq!(residual[0].payload["profile"], "full");
    assert_eq!(residual[0].payload["program"], "python3");
    assert_eq!(residual[1].payload["reason"], "ProtectedBranch");
    assert!(
        residual
            .iter()
            .all(|e| !e.payload.to_string().contains(token))
    );
}

/// Writes a history note carrying credentials, then accepts.
struct Leaky;
impl CommandHandler for Leaky {
    fn decide(
        &self,
        ctx: &InvocationContext,
        _: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let id = devworks_core::protocol::EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id,
            actor: ctx.principal().clone(),
            project_id: None,
            root_id: None,
            operation_id: None,
            aggregate_kind: "note".into(),
            aggregate_id: "note-1".into(),
            aggregate_revision: 1,
            kind: "Note".into(),
            payload: json!({
                "text": "Authorization: Bearer sekret-bearer-value",
                "clone": "https://bob:hunter22secret@example.invalid/repo.git",
                "cli": "deploy --password hunter33secret --token tok_99887766",
                "aws_secret_access_key": "AKIAIOSFODNN7EXAMPLEKEY",
            }),
        })?;
        accept_in(tx)
    }
}

#[test]
fn credentials_never_reach_history_receipts_or_returned_errors() {
    let fx = Fx::new();
    let token = "ghp_0123456789abcdefABCDEF0123";
    let user = fx.ctx("user-a", PrincipalKind::Human);
    let authority = fx.authority();
    // A credential in a setting value is refused and never stored or echoed.
    let reply = fx
        .cases
        .command(
            &fx.gateway(&authority),
            &user,
            "settings.save_revision",
            fx.save_frame(
                json!({"kind": "Workspace"}),
                1,
                "roles.worker.instructions",
                json!(format!("use {token} to push")),
            )
            .as_bytes(),
        )
        .unwrap();
    assert!(!format!("{reply:?}").contains(token));
    // Free text a use case writes to history is redacted before it is persisted.
    let reply = fx
        .gateway(&authority)
        .command(
            &user,
            "runs.start",
            start("leak-1").as_bytes(),
            Arc::new(Leaky),
        )
        .unwrap();
    assert!(matches!(reply, CommandReply::Recorded { .. }));
    // A credential in a malformed frame never comes back in the error.
    let frame = format!(r#"{{"protocol":"{token}","command":{{"kind":"runs.start"}}}}"#);
    let failure = fx
        .gateway(&authority)
        .command(&user, "runs.start", frame.as_bytes(), accept())
        .unwrap_err();
    assert!(
        !format!("{failure:?} {failure}").contains(token),
        "{failure}"
    );
    let raw = fx.h.raw();
    let dump = |sql: &str| -> String {
        raw.query_row(sql, [], |r| r.get::<_, Option<String>>(0))
            .unwrap()
            .unwrap_or_default()
    };
    let history = dump("SELECT group_concat(payload_json, ' ') FROM events");
    let receipts = dump("SELECT group_concat(result_json, ' ') FROM command_receipts");
    for secret in [
        token,
        "sekret-bearer-value",
        "hunter22secret",
        "hunter33secret",
        "tok_99887766",
        "AKIAIOSFODNN7EXAMPLEKEY",
    ] {
        assert!(
            !history.contains(secret),
            "history leaks {secret}: {history}"
        );
        assert!(!receipts.contains(secret), "receipts leak {secret}");
    }
    assert!(history.contains("https://bob:[REDACTED]@example.invalid/repo.git"));
}

#[cfg(unix)]
#[test]
fn deletion_confirmations_are_short_lived_and_history_revisions_are_real() {
    let fx = Fx::new();
    let scratch = Scratch::new("deletion-revisions");
    let repo = scratch.path("repo");
    std::fs::create_dir_all(&repo).unwrap();
    register_repository(&fx, "repo-a", &repo);
    let fs = NativeFilesystem::new();
    let user = fx.ctx("user-a", PrincipalKind::Human);
    let preview = deletion::prepare(
        &fs,
        &roots(&fx),
        &id("repo-a"),
        RepositoryDependencies::default(),
    )
    .unwrap();
    assert!(matches!(
        deletion::confirm(
            &*fx.store,
            &user,
            &preview,
            T0 + deletion::MAX_CONFIRMATION_LIFETIME_MS + 1
        ),
        Err(DeletionRefusal::ConfirmationUnavailable)
    ));
    let confirmation = deletion::confirm(&*fx.store, &user, &preview, T0 + 60_000).unwrap();
    let again = deletion::confirm(&*fx.store, &user, &preview, T0 + 60_000).unwrap();
    let permit = deletion::authorize(&*fx.store, &user, &again.id, &preview).unwrap();
    let outcome = devworks_core::authority::destructive::RemovalOutcome {
        removed_entries: 0,
        remaining: vec![repo.clone()],
        complete: false,
    };
    deletion::record_outcome(&*fx.store, &user, &permit, &outcome).unwrap();
    let revisions: Vec<(String, i64)> = fx
        .events()
        .into_iter()
        .filter(|e| e.aggregate_kind == deletion::DELETION_AGGREGATE)
        .map(|e| (e.kind, e.aggregate_revision))
        .collect();
    assert_eq!(
        revisions,
        [
            ("RepositoryDeletionConfirmed".to_owned(), 1),
            ("RepositoryDeletionConfirmed".to_owned(), 2),
            ("RepositoryDeletionAuthorized".to_owned(), 3),
            ("RepositoryDeletionPartial".to_owned(), 4),
        ]
    );
    // The first confirmation is unused and expires on its own.
    assert_eq!(fx.grant(&confirmation.id).state, GrantState::Active);
    // An agent can never reach the dedicated path, even with the user's confirmation ID.
    assert!(matches!(
        deletion::authorize(&*fx.store, &fx.agent("agent-a"), &confirmation.id, &preview),
        Err(DeletionRefusal::HumanOnly)
    ));
}

// --- permits bound to their store; redaction scoped to secrets --------------------------------

#[cfg(unix)]
#[test]
fn a_permit_minted_on_another_store_is_refused_against_the_real_roots() {
    let fx = Fx::new();
    let other = Fx::with_id_offset(10_000);
    assert_ne!(fx.store.info().store_id, other.store.info().store_id);
    let scratch = Scratch::new("foreign-permit");
    let repo = scratch.path("repo");
    std::fs::create_dir_all(&repo).unwrap();
    register_repository(&fx, "repo-a", &repo);
    register_repository(&other, "repo-a", &repo);
    let fs = NativeFilesystem::new();
    // The forger confirms and authorizes on a store it controls.
    let forger = other.ctx("user-a", PrincipalKind::Human);
    let foreign_roots = roots(&other);
    let preview = deletion::prepare(
        &fs,
        &foreign_roots,
        &id("repo-a"),
        RepositoryDependencies::default(),
    )
    .unwrap();
    let grant = deletion::confirm(&*other.store, &forger, &preview, T0 + 60_000).unwrap();
    let permit = deletion::authorize(&*other.store, &forger, &grant.id, &preview).unwrap();
    assert_eq!(permit.store_id(), &other.store.info().store_id);
    // Against the real store's roots it matches nothing.
    let plan = EffectPlan {
        kind: DestructiveKind::RemoveTree,
        targets: vec![repo.clone()],
        working_directory: None,
        scope: IntendedScope::PrimaryRepositoryDeletion(permit),
    };
    assert_eq!(
        destructive::preflight(&fs, &roots(&fx), &plan).verdict,
        PreflightVerdict::Blocked(BlockReason::PermitMismatch)
    );
    // Nor against roots that were not read from any store.
    let unbound = ProtectedRoots::new(foreign_roots.repositories.clone(), Vec::new(), None);
    assert_eq!(
        destructive::preflight(&fs, &unbound, &plan).verdict,
        PreflightVerdict::Blocked(BlockReason::PermitMismatch)
    );
    // A registry change after confirmation also refuses it on its own store.
    register_repository(&other, "repo-z", &scratch.path("other"));
    assert_eq!(
        destructive::preflight(&fs, &roots(&other), &plan).verdict,
        PreflightVerdict::Blocked(BlockReason::PermitMismatch)
    );
    assert!(std::path::Path::new(&repo).exists());
}

/// Writes history carrying ordinary paths, branch names and credential metadata, plus a token.
struct Records;
impl CommandHandler for Records {
    fn decide(
        &self,
        ctx: &InvocationContext,
        _: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let id = devworks_core::protocol::EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id,
            actor: ctx.principal().clone(),
            project_id: None,
            root_id: None,
            operation_id: None,
            aggregate_kind: "note".into(),
            aggregate_id: "note-2".into(),
            aggregate_revision: 1,
            kind: "Note".into(),
            payload: json!({
                "path": "/Users/Michael/Projects2025/DevWorksData",
                "windowsPath": "C:/Users/Michael/Source/Repos/DevWorks2",
                "branch": "feature/AddOAuth2LoginFlowForUsersV2",
                "title": "Rename QueryBuilder2 to QueryPlannerImplementation",
                "sha256": "deadbeefcafebabedeadbeefcafebabedeadbeefcafebabedeadbeefcafebabe",
                "credentialRevision": "3",
                "credentialRef": "dwsecret-0123456789abcdef0123456789abcdef",
                "credentialGeneration": 4,
                "tokenEstimate": 5,
                "sessionToken": "plain-session-value",
                "leak": "uses ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            }),
        })?;
        accept_in(tx)
    }
}

/// Accepts with an operation whose identifier is shaped like a token.
struct TokenShapedId;
impl CommandHandler for TokenShapedId {
    fn decide(
        &self,
        _: &InvocationContext,
        _: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let op = OperationId(id("ghp_abcdefghijklmnopqrstuvwxyz0123456789"));
        tx.operations().record_intent(NewOperation {
            id: op.clone(),
            root_id: Some(RootId(id("root-a"))),
            assignment_id: None,
            capability: "runs.start".into(),
            schema_version: 1,
            args_digest: "d".into(),
            payload: json!({}),
        })?;
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: op.clone(),
            },
            operation_id: Some(op),
        })
    }
}

#[test]
fn redaction_keeps_ordinary_values_and_refuses_what_it_cannot_redact() {
    let fx = Fx::new();
    let user = fx.ctx("user-a", PrincipalKind::Human);
    let authority = fx.authority();
    // A path setting survives in the receipt and its replay, byte for byte.
    let path = "/Users/Michael/Projects2025/DevWorksData";
    let frame = fx.save_frame(
        json!({"kind": "Workspace"}),
        1,
        "repositories.baseDirectory",
        json!(path),
    );
    let first = fx
        .cases
        .command(
            &fx.gateway(&authority),
            &user,
            "settings.save_revision",
            frame.as_bytes(),
        )
        .unwrap();
    let replay = fx
        .cases
        .command(
            &fx.gateway(&authority),
            &user,
            "settings.save_revision",
            frame.as_bytes(),
        )
        .unwrap();
    assert!(format!("{first:?}").contains(path), "{first:?}");
    assert!(matches!(
        replay,
        CommandReply::Recorded { replayed: true, .. }
    ));
    assert!(format!("{replay:?}").contains(path));
    // History keeps paths, branch names, digests and credential metadata; secrets go.
    let reply = fx
        .gateway(&authority)
        .command(
            &user,
            "runs.start",
            start("records-1").as_bytes(),
            Arc::new(Records),
        )
        .unwrap();
    assert!(matches!(reply, CommandReply::Recorded { .. }));
    let note = fx
        .events()
        .into_iter()
        .find(|e| e.aggregate_id == "note-2")
        .unwrap()
        .payload;
    for key in [
        "path",
        "windowsPath",
        "branch",
        "title",
        "sha256",
        "credentialRevision",
        "credentialRef",
        "credentialGeneration",
        "tokenEstimate",
    ] {
        assert_ne!(
            note[key],
            json!(devworks_core::authority::redact::REDACTED),
            "{key}"
        );
    }
    assert_eq!(note["path"], path);
    assert_eq!(note["branch"], "feature/AddOAuth2LoginFlowForUsersV2");
    assert_eq!(
        note["credentialRef"],
        "dwsecret-0123456789abcdef0123456789abcdef"
    );
    assert_eq!(
        note["sessionToken"],
        devworks_core::authority::redact::REDACTED
    );
    assert!(!note["leak"].as_str().unwrap().contains("ghp_"));
    let receipts =
        fx.h.raw()
            .query_row(
                "SELECT group_concat(result_json, ' ') FROM command_receipts",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap();
    assert!(receipts.contains(path));
    // A decision whose secret sits in a field that cannot hold a redaction is refused, with
    // every write undone, never stored unredacted.
    let before = fx.events().len();
    let failure = fx
        .gateway(&authority)
        .command(
            &user,
            "runs.start",
            start("token-id").as_bytes(),
            Arc::new(TokenShapedId),
        )
        .unwrap_err();
    assert!(matches!(
        failure,
        devworks_core::gateway::GatewayFailure::HandlerContract(_)
    ));
    assert_eq!(fx.events().len(), before);
    let ops = read_snapshot(&*fx.store, &ReadOptions::default(), |s| {
        s.operations().unresolved(100)
    })
    .unwrap();
    assert!(ops.iter().all(|o| !o.id.0.as_str().starts_with("ghp_")));
}

#[test]
fn a_blocked_command_records_every_refused_step_with_its_rationale() {
    use devworks_core::authority::shell::{CommandVerdict, preflight_command, record_verdict};
    let fx = Fx::new();
    let scratch = Scratch::new("verdict");
    let repo = scratch.path("repo");
    std::fs::create_dir_all(format!("{repo}/.git")).unwrap();
    register_repository(&fx, "repo-a", &repo);
    let argv: Vec<String> = vec![
        "sh".into(),
        "-c".into(),
        format!("rm -rf {repo}; git push -f origin main; cp a b"),
    ];
    let verdict = preflight_command(&NativeFilesystem::new(), &roots(&fx), &argv, None, &[]);
    assert!(
        matches!(verdict, CommandVerdict::Blocked { .. }),
        "{verdict:?}"
    );
    let ctx = fx.agent("agent-a").with_authority_source(id("approval-3"));
    let (c, a, v) = (ctx.clone(), argv.clone(), verdict.clone());
    fx.write(move |tx| record_verdict(tx, &c, "process.execute", &a, &v));
    let event = fx
        .events()
        .into_iter()
        .find(|e| e.kind == "DestructiveOperationBlocked")
        .unwrap();
    assert_eq!(event.actor, actor("agent-a"));
    let reasons: Vec<&str> = event.payload["blocked"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["reason"].as_str().unwrap())
        .collect();
    assert_eq!(reasons, ["ProtectedBranch", "PrimaryRepository"]);
    assert!(
        event.payload["blocked"]
            .as_array()
            .unwrap()
            .iter()
            .all(|b| !b["rationale"].as_str().unwrap().is_empty())
    );
    assert_eq!(
        event.payload["held"],
        json!(["UnmodeledConsequentialCommand"])
    );
    assert_eq!(event.payload["authoritySources"], json!(["approval-3"]));
}
