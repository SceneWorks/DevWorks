//! Repository discovery, structural indexing, runs and overlays (DW-023) over the real SQLite
//! store, the native process owner (Git runs as owned trees), the Tree-sitter parser and real
//! Git repositories created in temporary directories. No network, no inference.
//!
//! Tests spawn processes, so they run serialized (see `processes.rs`).
#![allow(clippy::result_large_err)]
use devworks_adapters::fs::NativeFilesystem;
use devworks_adapters::fs::artifacts::NativeArtifactFiles;
use devworks_adapters::fs::sources::HostArtifactSources;
use devworks_adapters::index::NativeSourceTree;
use devworks_adapters::index::treesitter::TreeSitterParser;
use devworks_adapters::process::NativeProcessHost;
use devworks_adapters::storage::sqlite::{SqliteConfig, SqliteStore};
use devworks_core::authority::HostAuthority;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::indexing::{
    ExtractFault, FileExtract, LanguageInfo, SourceTree, StructuralParser,
};
use devworks_core::ports::storage::*;
use devworks_core::ports::{Clock, IdKind, IdSource};
use devworks_core::protocol::records::{
    IndexFreshnessState, IndexRunRecord, LanguageCoverageMode, OverlayState, RepositoryIndexStatus,
    RepositoryRecord,
};
use devworks_core::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, OperationResult,
};
use devworks_core::protocol::support::Page;
use devworks_core::protocol::{
    ActorId, CommandReply, Counter, DomainErrorCode, DomainFault, Id, OperationState, Revision,
};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::indexing::discover::{self, worktree_id};
use devworks_core::services::indexing::helper::SupervisedHelpers;
use devworks_core::services::indexing::paths::location_identity;
use devworks_core::services::indexing::worker::IndexWorker;
use devworks_core::services::indexing::{IndexCoordinator, IndexEnv, IndexSettings, views};
use devworks_core::services::process::ProcessSupervisor;
use devworks_core::services::projects::{ProjectService, RepositoryService};
use devworks_core::services::resources::ResourceService;
use devworks_core::services::runs::ControlService;
use devworks_core::settings::catalog::{OVERLAY_RECORDS_KEPT_KEY, descriptor};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
#[cfg(unix)]
use std::time::{Duration, Instant};

const T0: i64 = 1_800_000_000_000;

fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}
struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn now_ms(&self) -> Counter {
        Counter::new(self.0.load(Ordering::SeqCst)).unwrap()
    }
}
struct TestIds(AtomicU64);
impl IdSource for TestIds {
    fn next_id(&self, kind: IdKind) -> Id {
        let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        Id::new(format!("{kind:?}-{n}").to_lowercase()).unwrap()
    }
}
fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}

/// A removed-on-drop temporary directory.
struct Temp(PathBuf);
impl Temp {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "devworks-index-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // In the identity form discovery records (macOS /private/var; on Windows without the
        // verbatim prefix `canonicalize` adds, which Git also refuses in `worktree add`).
        Self(NativeSourceTree::new().canonical_dir(&dir).unwrap())
    }
    fn join(&self, p: &str) -> PathBuf {
        self.0.join(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------------------------
// Git fixtures

fn git(dir: &Path, args: &[&str]) -> String {
    let home = dir.join(".git-fixture-home");
    std::fs::create_dir_all(&home).unwrap();
    let mut command = Command::new("git");
    command.env_clear();
    for name in [
        "PATH",
        "SYSTEMROOT",
        "WINDIR",
        "TMPDIR",
        "TMP",
        "TEMP",
        "LANG",
        "LC_ALL",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let out = command
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "push.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "core.autocrlf=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", &home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", null)
        .env("GIT_CONFIG_GLOBAL", null)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}
fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}
/// A repository with one commit of `files`.
fn repo(dir: &Path, files: &[(&str, &str)]) -> String {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    for (p, t) in files {
        write(dir, p, t);
    }
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "initial"]);
    git(dir, &["rev-parse", "HEAD"])
}
fn commit(dir: &Path, message: &str) -> String {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
    git(dir, &["rev-parse", "HEAD"])
}

// ---------------------------------------------------------------------------------------------
// The fixture

/// A hook run at the n-th extraction.
type Hook = (usize, Box<dyn Fn() + Send + Sync>);

/// A parser that counts extractions and can run a hook after the n-th one.
struct Counting {
    inner: TreeSitterParser,
    count: AtomicUsize,
    hook: Mutex<Option<Hook>>,
}
impl StructuralParser for Counting {
    fn languages(&self) -> &[LanguageInfo] {
        self.inner.languages()
    }
    fn language_of(&self, path: &str) -> Option<&LanguageInfo> {
        self.inner.language_of(path)
    }
    fn extract(
        &self,
        language: &str,
        path: &str,
        repository: &str,
        source: &[u8],
        stop: &(dyn Fn() -> bool + Sync),
        budget_ms: u64,
    ) -> Result<FileExtract, ExtractFault> {
        let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some((at, f)) = &*self.hook.lock().unwrap()
            && *at == n
        {
            f();
        }
        self.inner
            .extract(language, path, repository, source, stop, budget_ms)
    }
    fn group_of(&self, path: &str) -> Option<&'static str> {
        self.inner.group_of(path)
    }
    fn extract_group(
        &self,
        group: &str,
        repository: &str,
        files: &[(String, Vec<u8>)],
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<std::collections::BTreeMap<String, FileExtract>, ExtractFault> {
        self.inner.extract_group(group, repository, files, stop)
    }
}

struct Fx {
    root: Temp,
    clock: Arc<TestClock>,
    ids: Arc<TestIds>,
    store: Arc<SqliteStore>,
    env: Arc<IndexEnv>,
    parser: Arc<Counting>,
    worker: Arc<IndexWorker>,
    cases: UseCases,
    n: AtomicU64,
}

impl Fx {
    fn new(tweak: impl FnOnce(&mut IndexSettings)) -> Self {
        let root = Temp::new("store");
        let clock = Arc::new(TestClock(AtomicI64::new(T0)));
        let ids = Arc::new(TestIds(AtomicU64::new(0)));
        let mut settings = IndexSettings {
            retry_base_ms: 1_000,
            ..IndexSettings::default()
        };
        tweak(&mut settings);
        Self::open(root, clock, ids, settings, "owner-a")
    }
    fn open(
        root: Temp,
        clock: Arc<TestClock>,
        ids: Arc<TestIds>,
        settings: IndexSettings,
        owner: &str,
    ) -> Self {
        let store = Arc::new(
            SqliteStore::open(SqliteConfig::new(
                root.join("data"),
                clock.clone(),
                ids.clone(),
                ActorId(id("host")),
            ))
            .expect("open store"),
        );
        let dyn_store: Arc<dyn ApplicationStore> = store.clone();
        let resources = Arc::new(ResourceService::new(ids.clone(), clock.clone()));
        let supervisor = Arc::new(ProcessSupervisor::new(
            dyn_store.clone(),
            Arc::new(NativeProcessHost),
            resources,
        ));
        let artifacts = Arc::new(ArtifactService::new(
            Arc::new(NativeArtifactFiles::new(
                root.join("data"),
                root.join("data/cache"),
            )),
            Arc::new(HostArtifactSources::new(clock.clone(), ids.clone())),
            Arc::new(NativeFilesystem::new()),
            clock.clone(),
            ids.clone(),
        ));
        let parser = Arc::new(Counting {
            inner: TreeSitterParser::new(),
            count: AtomicUsize::new(0),
            hook: Mutex::new(None),
        });
        let env = Arc::new(IndexEnv {
            store: dyn_store.clone(),
            parser: parser.clone(),
            source: Arc::new(NativeSourceTree::new()),
            helpers: Arc::new(SupervisedHelpers {
                supervisor,
                store: dyn_store.clone(),
                fs: Arc::new(NativeFilesystem::new()),
            }),
            clock: clock.clone(),
            ids: ids.clone(),
            artifacts: Some(artifacts.clone()),
            diagnostics: None,
            enrichers: Default::default(),
            settings,
        });
        let worker = Arc::new(IndexWorker::new(env.clone(), owner.into()));
        let control = Arc::new(ControlService::new(ids.clone()));
        let cases = UseCases::new()
            .with_indexing(Arc::new(IndexCoordinator::new(env.clone())))
            .with_projects(Arc::new(ProjectService::new(ids.clone(), control.clone())))
            .with_repositories(Arc::new(RepositoryService::new(ids.clone())))
            .with_artifacts(artifacts);
        let fx = Self {
            root,
            clock,
            ids,
            store,
            env,
            parser,
            worker,
            cases,
            n: AtomicU64::new(0),
        };
        fx.write(|tx| {
            tx.actors()
                .register(&ActorId(id("user")), PrincipalKind::Human, "User")
        });
        fx
    }
    /// A new boot over the same data root (every handle of the old one dropped).
    fn restart(self, owner: &str) -> Self {
        let settings = self.env.settings.clone();
        self.reopen(settings, owner)
    }
    /// Drops every handle of this boot (the data root is released), then opens a new one.
    fn reopen(self, settings: IndexSettings, owner: &str) -> Self {
        let Fx {
            root,
            clock,
            ids,
            store,
            env,
            parser,
            worker,
            cases,
            n,
        } = self;
        drop((cases, worker, parser, env, store));
        let fx = Self::open(root, clock, ids, settings, owner);
        // Request keys stay unique across boots (a reused key would replay its receipt).
        fx.n.store(n.load(Ordering::SeqCst), Ordering::SeqCst);
        fx
    }
    fn user(&self) -> InvocationContext {
        let info = self.store.info();
        HostIngress::new(&*self.store).context(
            ActorId(id("user")),
            PrincipalKind::Human,
            id("invocation-1"),
            id("ui-main"),
            Counter::new(info.restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
    }
    fn now(&self, ms: i64) {
        self.clock.0.store(ms, Ordering::SeqCst);
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
    fn gateway_auth(&self) -> HostAuthority<'_> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.clock,
            protected_branches: &[],
        }
    }
    fn command_frame(&self, ctx: &InvocationContext, op: &str, frame: Value) -> DomainCommandReply {
        let authority = self.gateway_auth();
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.ids,
        };
        self.cases
            .command(&gateway, ctx, op, frame.to_string().as_bytes())
            .expect("addressable")
    }
    fn command(&self, op: &str, mut body: Value) -> DomainCommandReply {
        body["kind"] = json!(op);
        let key = format!("key-{}", self.n.fetch_add(1, Ordering::SeqCst));
        self.command_frame(
            &self.user(),
            op,
            json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": key, "body": body}),
        )
    }
    fn index_command(&self, op: &str, command: Value) -> DomainCommandReply {
        let key = format!("key-{}", self.n.fetch_add(1, Ordering::SeqCst));
        self.command_frame(
            &self.user(),
            op,
            json!({"protocol": "1.0-draft.1", "requestKey": key, "command": command}),
        )
    }
    fn query(&self, op: &str, mut body: Value, revision: Option<u32>) -> DomainQueryReply {
        body["kind"] = json!(op);
        let mut frame =
            json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-1", "body": body});
        if let Some(r) = revision {
            frame["schemaRevision"] = json!(r);
        }
        let authority = self.gateway_auth();
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.ids,
        };
        self.cases
            .query(&gateway, &self.user(), op, frame.to_string().as_bytes())
            .expect("addressable")
    }
    /// Sets the workspace repository base directory (SET-07/08).
    fn base_directory(&self, dir: &Path) {
        let d = descriptor(discover::BASE_DIRECTORY_KEY).unwrap();
        let assignment = serde_json::from_value(json!({
            "key": discover::BASE_DIRECTORY_KEY,
            "value": {"schema": d.schema_ref(), "value": dir.display().to_string()},
        }))
        .unwrap();
        let ctx = self.user();
        self.write(move |tx| {
            devworks_core::settings::service::apply_human_assignments(
                tx,
                &ctx,
                &ConfigScope::Workspace,
                &[assignment],
                json!({"test": true}),
            )
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
        });
    }
    fn scan(&self) -> discover::DiscoveryReport {
        discover::scan(&self.env, &self.user(), None, &|| false).expect("scan")
    }
    /// The stored identity of a directory (the product's form for every lookup by location).
    fn identity(&self, dir: &Path) -> String {
        location_identity(&*self.env.source, dir)
    }
    fn repository_at(&self, dir: &Path) -> Id {
        let loc = self.identity(dir);
        self.read(|s| s.indexing().repository_at(&loc))
            .expect("registered")
    }
    fn index(&self, repo: &Id, rebuild: bool, expected: &str) -> DomainCommandReply {
        self.command(
            if rebuild { "indexing.reindex" } else { "indexing.index" },
            json!({"scope": {"kind": "Workspace"}, "repositoryIds": [repo], "expectedSourceRevision": expected,
                "views": {"kind": "Published", "repositoryIds": [repo]}}),
        )
    }
    /// Claims and executes every claimable run (bounded).
    fn drain(&self) -> usize {
        let mut n = 0;
        while let Some(run) = self.worker.claim().expect("claim") {
            self.worker.execute(run);
            n += 1;
            assert!(n < 50, "runs keep coming");
        }
        n
    }
    fn status(&self, repo: &Id) -> RepositoryIndexStatus {
        match result(self.query("indexing.status", json!({"repositoryId": repo}), Some(2))) {
            OperationResult::IndexingStatus { value } => value,
            other => panic!("{other:?}"),
        }
    }
    fn run(&self, run: &Id) -> StoredIndexRun {
        let r = run.clone();
        self.read(move |s| s.indexing().run(&r)).expect("run")
    }
    fn published(&self, repo: &Id) -> views::ViewLayers {
        let r = repo.clone();
        let ctx = self.user();
        read_snapshot(&*self.store, &ReadOptions::default(), |s| {
            views::select(
                s,
                &ctx,
                &devworks_core::protocol::data::ViewSelection::Published {
                    repository_ids: vec![r],
                },
            )
            .map_err(|f| StorageError::InvalidRecord(format!("{f:?}")))
        })
        .expect("published view")
        .remove(0)
    }
    fn nodes(&self, view: &views::ViewLayers, key: &str) -> Vec<StoredNode> {
        let (v, k) = (view.clone(), key.to_owned());
        self.read(move |s| views::unit_nodes(s.indexing(), &v, &k))
    }
    fn unit(&self, view: &views::ViewLayers, key: &str) -> Option<(Id, StoredUnit)> {
        let (v, k) = (view.clone(), key.to_owned());
        self.read(move |s| views::resolve_unit(s.indexing(), &v, &k))
    }
}

fn result(r: DomainQueryReply) -> OperationResult {
    match r {
        DomainQueryReply::Result { result, .. } => result,
        DomainQueryReply::Rejected { fault, .. } => panic!("rejected: {fault:?}"),
    }
}
fn fault(r: DomainQueryReply) -> DomainFault {
    match r {
        DomainQueryReply::Rejected { fault, .. } => fault,
        other => panic!("expected a fault: {other:?}"),
    }
}
fn decision(r: DomainCommandReply) -> DomainDecision {
    match r {
        CommandReply::Recorded { receipt, .. } => receipt.decision,
        other => panic!("not recorded: {other:?}"),
    }
}
fn run_of(d: DomainDecision) -> IndexRunRecord {
    match d {
        DomainDecision::Applied { result } | DomainDecision::NoChange { result } => match result {
            OperationResult::IndexingIndex { value }
            | OperationResult::IndexingReindex { value }
            | OperationResult::IndexingCancel { value }
            | OperationResult::IndexingRefreshOverlay { value } => value,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}
fn refused(d: DomainDecision) -> DomainFault {
    match d {
        DomainDecision::Refused { fault } => fault,
        other => panic!("expected a refusal: {other:?}"),
    }
}
fn has_node(nodes: &[StoredNode], kind: &str, qn: &str) -> bool {
    nodes.iter().any(|n| {
        n.kind == kind && n.payload.get("qualifiedName").and_then(Value::as_str) == Some(qn)
    })
}

// ---------------------------------------------------------------------------------------------
// Discovery

#[test]
fn discovery_registers_checkouts_with_exact_identity_without_following_links() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("src");
    let a = src.join("team/app");
    let head = repo(&a, &[("src/lib.rs", "pub fn a() {}\n")]);
    git(
        &a,
        &["remote", "add", "origin", "git@github.com:acme/app.git"],
    );
    // A nested repository inside a found one is not a separate registration.
    repo(&a.join("vendor/inner"), &[("x.rs", "fn x() {}\n")]);
    // A linked worktree (a `.git` file) is recorded through its primary, never on its own.
    git(
        &a,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "topic",
            src.join("app-topic").to_str().unwrap(),
        ],
    );
    // A link to a checkout is never followed.
    #[cfg(unix)]
    std::os::unix::fs::symlink(&a, src.join("link-to-app")).unwrap();
    let excluded = src.join("legacy-tool");
    repo(&excluded, &[("a.py", "x = 1\n")]);
    let credentials = src.join("secret");
    repo(&credentials, &[("a.py", "x = 1\n")]);
    git(
        &credentials,
        &[
            "remote",
            "add",
            "origin",
            "https://user:ghp_token@github.com/acme/s.git",
        ],
    );
    fx.base_directory(&src.0);
    let ex = fx.command(
        "indexing.configure_sources",
        json!({"scope": {"kind": "Workspace"}, "expectedRevision": "1", "config": {
            "structuralLanguages": [], "semanticPackageIds": [], "excludePatterns": ["repository:legacy-*", "**/node_modules/**"],
            "maxFileBytes": "524288"}}),
    );
    assert!(matches!(decision(ex), DomainDecision::Applied { .. }));

    let report = fx.scan();
    assert_eq!(report.found.len(), 2, "{report:?}");
    assert_eq!(report.registered.len(), 2);
    assert_eq!(report.excluded, vec![excluded.display().to_string()]);
    let app = fx.repository_at(&a);
    let r = app.clone();
    let (source, worktrees, registration) = fx.read(move |s| {
        Ok((
            s.indexing().repository_source(&r)?,
            s.indexing().worktrees(&r)?,
            s.projects().repository(&r)?,
        ))
    });
    let source = source.unwrap();
    assert_eq!(
        source.location,
        fx.identity(&a),
        "the exact canonical location"
    );
    assert_eq!(source.head_oid.as_deref(), Some(head.as_str()));
    assert_eq!(source.head_ref.as_deref(), Some("refs/heads/main"));
    assert_eq!(
        source.remote_url.as_deref(),
        Some("git@github.com:acme/app.git")
    );
    assert_eq!(source.object_format, "Sha1");
    assert_eq!(source.state, SOURCE_PRESENT);
    assert_eq!(registration.unwrap().name, "app");
    assert_eq!(worktrees.len(), 2);
    assert!(worktrees[0].main && !worktrees[1].main);
    assert_eq!(worktrees[1].head_ref.as_deref(), Some("refs/heads/topic"));
    // Credentials are never recorded.
    let secret = fx.repository_at(&credentials);
    let s2 = secret.clone();
    let secret_source = fx
        .read(move |s| s.indexing().repository_source(&s2))
        .unwrap();
    assert!(secret_source.remote_url.is_none());

    // The discover query answers with the registrations of the section, paged.
    let page = match result(fx.query(
        "indexing.discover",
        json!({"scope": {"kind": "Workspace"}, "roots": ["workspace"], "page": {"limit": 1}}),
        None,
    )) {
        OperationResult::IndexingDiscover { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(page.items.0.len(), 1);
    assert!(page.has_more);
    let cursor = page.next_cursor_text();
    let rest: Page<RepositoryRecord> = match result(fx.query(
        "indexing.discover",
        json!({"scope": {"kind": "Workspace"}, "roots": ["workspace"], "page": {"limit": 5, "cursor": cursor}}),
        None,
    )) {
        OperationResult::IndexingDiscover { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(rest.items.0.len(), 1);
    assert!(!rest.has_more);
    // A cursor of another root set is expired, never reinterpreted.
    let expired = fault(fx.query(
        "indexing.discover",
        json!({"scope": {"kind": "Workspace"}, "roots": [], "page": {"limit": 5, "cursor": page.next_cursor_text()}}),
        None,
    ));
    assert_eq!(expired.code, DomainErrorCode::CursorExpired);

    // A new commit advances the source revision; a vanished checkout is recorded missing.
    write(&a, "src/b.rs", "pub fn b() {}\n");
    let head2 = commit(&a, "b");
    let report = fx.scan();
    assert!(report.changed.contains(&app));
    let r = app.clone();
    let after = fx
        .read(move |s| s.indexing().repository_source(&r))
        .unwrap();
    assert_eq!(after.head_oid.as_deref(), Some(head2.as_str()));
    assert_eq!(after.revision, source.revision + 1);
    std::fs::remove_dir_all(&credentials).unwrap();
    let report = fx.scan();
    assert!(report.missing.contains(&secret), "{report:?}");
}

trait NextCursor {
    fn next_cursor_text(&self) -> String;
}
impl NextCursor for Page<RepositoryRecord> {
    fn next_cursor_text(&self) -> String {
        self.next_cursor
            .as_ref()
            .map(|c| c.0.clone())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------------------------
// Baseline indexing

fn sample_repo(dir: &Path) -> String {
    repo(
        dir,
        &[
            ("README.md", "# Sample\n\nA test repository.\n"),
            (
                "src/lib.rs",
                "mod util;\npub fn main() {\n    helper();\n    util::shared();\n}\nfn helper() {}\n",
            ),
            ("src/util.rs", "pub fn shared() {}\n"),
            (
                "tools/run.py",
                "import os\nfrom tools import helpers\n\ndef go():\n    shared_helper()\n",
            ),
            ("tools/helpers.py", "def shared_helper():\n    pass\n"),
            ("app/Main.kt", "fun main() {}\n"),
            ("web/node_modules/dep/index.js", "module.exports = 1;\n"),
            ("big/data.rs", &"// filler\n".repeat(100_000)),
            ("docs/guide.md", "Guide\n"),
        ],
    )
}

#[test]
fn baseline_indexes_the_exact_commit_with_spans_relationships_coverage_and_texts() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("src");
    let dir = src.join("sample");
    let head = sample_repo(&dir);
    // Uncommitted edits never enter the baseline.
    write(
        &dir,
        "src/util.rs",
        "pub fn shared() {}\npub fn uncommitted() {}\n",
    );
    fx.base_directory(&src.0);
    fx.scan();
    let repo_id = fx.repository_at(&dir);
    let queued = run_of(decision(fx.index(&repo_id, false, "1")));
    assert!(matches!(queued.state, OperationState::IntentRecorded));
    assert_eq!(queued.stage.0, "queued");
    // A second request while one is queued reuses it.
    let again = decision(fx.index(&repo_id, false, "1"));
    assert!(matches!(again, DomainDecision::NoChange { .. }));
    assert_eq!(fx.drain(), 1);
    let run = fx.run(&queued.id);
    assert_eq!(run.state, RUN_SUCCEEDED, "{run:?}");
    assert_eq!(run.source_oid.as_deref(), Some(head.as_str()));
    assert_eq!(Some(run.completed_units), run.total_units);

    let status = fx.status(&repo_id);
    let fresh = status.freshness.clone().unwrap();
    assert!(matches!(fresh.state, IndexFreshnessState::Current));
    assert_eq!(
        fresh.indexed_commit.as_ref().map(|c| c.0.as_str()),
        Some(head.as_str())
    );
    let gaps: Vec<&str> = status.coverage.gaps.iter().map(Id::as_str).collect();
    assert!(gaps.contains(&"text-fallback-kotlin"), "{gaps:?}");
    assert!(gaps.contains(&"oversize-files"), "{gaps:?}");
    let languages = status.languages.clone().unwrap().0;
    let rust = languages
        .iter()
        .find(|l| l.language_id.0 == "rust")
        .unwrap();
    assert!(matches!(rust.mode, LanguageCoverageMode::Structural));
    assert_eq!(
        (
            rust.files.get(),
            rust.structural_files.get(),
            rust.skipped_files.get()
        ),
        (3, 2, 1)
    );
    assert!(
        rust.grammar
            .as_ref()
            .is_some_and(|g| g.0 == "tree-sitter-rust 0.24.2")
    );
    let kotlin = languages
        .iter()
        .find(|l| l.language_id.0 == "kotlin")
        .unwrap();
    assert!(matches!(kotlin.mode, LanguageCoverageMode::TextFallback));
    assert!(status.semantic_packs.as_ref().unwrap().0.is_empty());
    // Revision 1 of status carries none of the revision 2 members.
    let r1 = match result(fx.query("indexing.status", json!({"repositoryId": repo_id}), None)) {
        OperationResult::IndexingStatus { value } => value,
        other => panic!("{other:?}"),
    };
    assert!(r1.freshness.is_none() && r1.languages.is_none());

    let view = fx.published(&repo_id);
    assert_eq!(view.layers.len(), 1);
    let lib = fx.nodes(&view, "src/lib.rs");
    let main = lib.iter().find(|n| n.name == "main").unwrap();
    assert_eq!(main.kind, "Function");
    assert_eq!(main.payload["span"]["startLine"], json!(2));
    assert_eq!(main.payload["span"]["endLine"], json!(5));
    assert_eq!(
        main.payload["provenance"]["grammar"],
        json!("tree-sitter-rust 0.24.2")
    );
    let (_, util_unit) = fx.unit(&view, "src/util.rs").unwrap();
    let util_nodes = fx.nodes(&view, "src/util.rs");
    assert!(
        !util_nodes.iter().any(|n| n.name == "uncommitted"),
        "the baseline is the commit"
    );
    assert_eq!(util_unit.payload["mode"], json!("Structural"));
    let v = view.clone();
    let edges = fx.read(move |s| views::unit_edges(s.indexing(), &v, "src/lib.rs"));
    let call = |name: &str| {
        edges
            .iter()
            .find(|(e, _)| {
                e.kind == "CALLS"
                    && e.payload
                        .get("targetName")
                        .and_then(Value::as_str)
                        .is_some_and(|t| t.ends_with(name))
                    || e.kind == "CALLS"
                        && lib
                            .iter()
                            .any(|n| n.node_id == e.target_node_id && n.name == name)
            })
            .cloned()
            .unwrap_or_else(|| panic!("no call of {name}: {edges:?}"))
    };
    let (local, resolves) = call("helper");
    assert_eq!(local.payload["resolution"], json!("Inferred"));
    assert!(resolves);
    // A cross-file call is linked by name (a unique candidate), inferred, and resolves.
    let (cross, resolves) = call("shared");
    assert_eq!(cross.payload["resolution"], json!("Inferred"), "{cross:?}");
    assert_eq!(cross.payload["targetUnit"], json!("src/util.rs"));
    assert!(resolves);
    // Its receiver (`util::`) names no type of the index: bound by its name alone, which the
    // edge says (CODE-02), at the unresolved-receiver confidence.
    assert_eq!(cross.payload["resolutionBasis"], json!("name"), "{cross:?}");
    assert_eq!(cross.payload["confidence"], json!(0.45));
    assert_eq!(cross.payload["extractedConfidence"], json!(0.65));
    // A bare cross-file call is an ordinary unique-name binding.
    let v = view.clone();
    let run_py = fx.read(move |s| views::unit_edges(s.indexing(), &v, "tools/run.py"));
    let bare = run_py
        .iter()
        .find(|(e, _)| e.kind == "CALLS" && e.payload["targetName"] == json!("shared_helper"))
        .unwrap_or_else(|| panic!("no call of shared_helper: {run_py:?}"));
    assert_eq!(bare.0.payload["resolution"], json!("Inferred"), "{bare:?}");
    assert!(bare.0.payload.get("resolutionBasis").is_none(), "{bare:?}");
    // Python: an absolute import of a repository module resolves; `os` stays unresolved.
    let v = view.clone();
    let py = fx.read(move |s| views::unit_edges(s.indexing(), &v, "tools/run.py"));
    let os = py
        .iter()
        .find(|(e, _)| e.payload.get("targetName") == Some(&json!("os")))
        .unwrap();
    assert_eq!(os.0.payload["resolution"], json!("Unresolved"));
    assert!(!os.1);
    let helpers = py
        .iter()
        .find(|(e, _)| e.payload.get("targetName") == Some(&json!("tools")))
        .unwrap();
    let _ = helpers;
    // Excluded paths are not part of the index; oversize and text-only files are honest units.
    assert!(fx.unit(&view, "web/node_modules/dep/index.js").is_none());
    let (_, big) = fx.unit(&view, "big/data.rs").unwrap();
    assert_eq!(big.payload["mode"], json!("Oversize"));
    let (_, kt) = fx.unit(&view, "app/Main.kt").unwrap();
    assert_eq!(kt.payload["mode"], json!("TextFallback"));
    assert!(
        fx.nodes(&view, "app/Main.kt")
            .iter()
            .all(|n| n.kind == "File"),
        "no definition is claimed"
    );
    // Directory structure: the repository contains its folders and files.
    let root = fx.nodes(&view, "/");
    assert_eq!(root[0].kind, "Repository");
    assert!(has_node(&fx.nodes(&view, "/src"), "Folder", "src"));

    // README and the structural summary are published texts of the view.
    for (op, expected) in [
        (
            "repositories.readme",
            Some("# Sample\n\nA test repository.\n"),
        ),
        ("repositories.summary", None),
    ] {
        let text = match result(fx.query(op, json!({"repositoryId": repo_id, "views": {"kind": "Published", "repositoryIds": [repo_id]}}), None)) {
            OperationResult::RepositoriesReadme { value } | OperationResult::RepositoriesSummary { value } => value,
            other => panic!("{other:?}"),
        };
        assert_eq!(text.view_id, view.view.view_id);
        if let Some(e) = expected {
            assert_eq!(
                String::from(text.content.sha256.clone()),
                devworks_core::fingerprint::sha256_hex(e.as_bytes())
            );
        }
    }
}

#[test]
fn incremental_runs_reconcile_edits_deletions_and_renames_and_stale_never_reads_current() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("src");
    let dir = src.join("inc");
    repo(
        &dir,
        &[
            ("a.rs", "fn a() {}\n"),
            ("b.py", "def b():\n    pass\n"),
            ("c.rs", "pub fn c() {}\n"),
            ("d.rs", "fn d() { c(); }\n"),
        ],
    );
    fx.base_directory(&src.0);
    fx.scan();
    let repo_id = fx.repository_at(&dir);
    run_of(decision(fx.index(&repo_id, false, "1")));
    fx.drain();
    let first = fx.published(&repo_id);
    let extracted_first = fx.parser.count.load(Ordering::SeqCst);

    write(&dir, "a.rs", "fn a() {}\nfn a2() {}\n");
    std::fs::remove_file(dir.join("b.py")).unwrap();
    git(&dir, &["mv", "c.rs", "e.rs"]);
    let head2 = commit(&dir, "edit, delete, rename");
    fx.scan();
    let stale = fx.status(&repo_id);
    let f = stale.freshness.unwrap();
    assert!(matches!(f.state, IndexFreshnessState::Stale));
    assert_eq!(
        f.reasons.0.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        vec!["head-moved"]
    );
    assert!(
        stale
            .coverage
            .gaps
            .iter()
            .any(|g| g.as_str() == "stale-head-moved")
    );
    assert!(!matches!(
        stale.coverage.state,
        devworks_core::protocol::data::CoverageState::Complete
    ));

    run_of(decision(fx.index(&repo_id, false, "1")));
    fx.drain();
    let second = fx.published(&repo_id);
    assert_ne!(first.view.view_id, second.view.view_id);
    // Only the changed and renamed files were parsed again; d.rs was inherited.
    let extracted = fx.parser.count.load(Ordering::SeqCst) - extracted_first;
    assert_eq!(extracted, 2, "a.rs and e.rs");
    assert!(fx.unit(&second, "b.py").is_none(), "deleted");
    assert!(fx.unit(&second, "c.rs").is_none(), "renamed away");
    let (_, e) = fx.unit(&second, "e.rs").unwrap();
    assert_eq!(e.payload["previousPath"], json!("c.rs"));
    assert!(fx.nodes(&second, "a.rs").iter().any(|n| n.name == "a2"));
    // d.rs calls c(): after the rename it links to e.rs's definition.
    let v = second.clone();
    let d = fx.read(move |s| views::unit_edges(s.indexing(), &v, "d.rs"));
    let call = d.iter().find(|(e, _)| e.kind == "CALLS").unwrap();
    assert_eq!(call.0.payload["targetUnit"], json!("e.rs"));
    assert!(call.1);
    assert!(matches!(
        fx.status(&repo_id).freshness.unwrap().state,
        IndexFreshnessState::Current
    ));
    // The previous baseline stays readable by its exact view (retained), the one before it goes.
    assert!(
        fx.unit(&first, "b.py").is_some(),
        "an exact older view keeps its own facts"
    );
    write(&dir, "f.rs", "fn f() {}\n");
    commit(&dir, "third");
    run_of(decision(fx.index(&repo_id, true, "1")));
    let before = fx.parser.count.load(Ordering::SeqCst);
    fx.drain();
    assert!(
        fx.parser.count.load(Ordering::SeqCst) - before >= 4,
        "a rebuild parses everything"
    );
    let gone = first.view.view_id.clone();
    assert!(
        fx.read(move |s| s.indexing().view(&gone)).is_none(),
        "beyond retention"
    );
    let reclaimed = fx.worker.housekeeping(false, &|| false).unwrap();
    assert!(reclaimed.reclaimed_rows > 0);
    let _ = head2;
}

// ---------------------------------------------------------------------------------------------
// Runs: cancellation, failures, retries, resume

#[test]
fn cancellation_is_bound_to_the_run_and_kills_its_helper_tree() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("src");
    let dir = src.join("c");
    repo(&dir, &[("a.rs", "fn a() {}\n")]);
    fx.base_directory(&src.0);
    fx.scan();
    let repo_id = fx.repository_at(&dir);
    // A queued run cancels at once and is never claimed.
    let q = run_of(decision(fx.index(&repo_id, false, "1")));
    let c = run_of(decision(fx.index_command(
        "indexing.cancel",
        json!({"kind": "Cancel", "operationId": q.id}),
    )));
    assert!(matches!(c.state, OperationState::CancelledBeforeDispatch));
    assert!(fx.worker.claim().unwrap().is_none());
    // Cancelling it again changes nothing.
    assert!(matches!(
        decision(fx.index_command(
            "indexing.cancel",
            json!({"kind": "Cancel", "operationId": q.id})
        )),
        DomainDecision::NoChange { .. }
    ));
    // A running run whose Git hangs (a POSIX shell fake; Windows process-tree supervision is
    // covered by the process tests).
    #[cfg(unix)]
    hanging_git_is_killed_on_cancel(fx, &repo_id);
}

#[cfg(unix)]
fn hanging_git_is_killed_on_cancel(fx: Fx, repo_id: &Id) {
    let bin = Temp::new("bin");
    let fake = bin.join("slow-git");
    std::fs::write(&fake, "#!/bin/sh\nsleep 30 &\nwait\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let repo_id = repo_id.clone();
    // The cancellation kills the tree within the bound.
    let fx = {
        let mut s = fx.env.settings.clone();
        s.git_program = fake.display().to_string();
        fx.reopen(s, "owner-b")
    };
    let r = run_of(decision(fx.index(&repo_id, false, "1")));
    let claimed = fx.worker.claim().unwrap().unwrap();
    assert_eq!(claimed.id, r.id);
    let w = fx.worker.clone();
    let started = Instant::now();
    let t = std::thread::spawn(move || w.execute(claimed));
    std::thread::sleep(Duration::from_millis(300));
    run_of(decision(fx.index_command(
        "indexing.cancel",
        json!({"kind": "Cancel", "operationId": r.id}),
    )));
    fx.worker.cancel_local(&r.id);
    t.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the tree was killed, not waited for"
    );
    let done = fx.run(&r.id);
    assert_eq!(done.state, RUN_CANCELLED);
    assert!(done.output_view_id.is_none(), "nothing was published");
    let unsettled = fx.read(|s| s.resources().processes(&ProcessFilter::Unsettled, None, 50));
    assert!(unsettled.is_empty(), "no orphan helper: {unsettled:?}");
    // A later run is untouched by the earlier cancellation.
    let later = run_of(decision(fx.index(&repo_id, false, "1")));
    assert_ne!(later.id, r.id);
    assert_eq!(fx.run(&later.id).cancel_requested_at_ms, None);
}

#[test]
fn a_cancel_recorded_without_a_local_signal_stops_the_run_at_its_next_checkpoint() {
    let _s = serial();
    let fx = Fx::new(|s| s.batch_units = 2);
    let src = Temp::new("src");
    let dir = src.join("durable-cancel");
    let files: Vec<(String, String)> = (0..10)
        .map(|i| (format!("m{i}.rs"), format!("fn f{i}() {{}}\n")))
        .collect();
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    repo(&dir, &refs);
    fx.base_directory(&src.0);
    fx.scan();
    let repo_id = fx.repository_at(&dir);
    let r = run_of(decision(fx.index(&repo_id, false, "1")));
    // The cancellation is recorded durably (as another process would) and the worker is never
    // signalled: its next fenced write observes it.
    let (store, run_id) = (fx.store.clone(), r.id.clone());
    *fx.parser.hook.lock().unwrap() = Some((
        3,
        Box::new(move || {
            let run_id = run_id.clone();
            run_write(&*store, WriteOptions::default(), move |tx| {
                let cur = tx.indexing().run(&run_id)?.expect("run");
                let mut next = cur.clone();
                next.revision += 1;
                next.cancel_requested_at_ms = Some(tx.now_ms());
                next.stage = "cancelling".into();
                tx.indexing().update_run(&next, cur.revision)
            })
            .expect("record the cancellation");
        }),
    ));
    let claimed = fx.worker.claim().unwrap().unwrap();
    fx.worker.execute(claimed);
    *fx.parser.hook.lock().unwrap() = None;
    let done = fx.run(&r.id);
    assert_eq!(done.state, RUN_CANCELLED, "{done:?}");
    assert!(done.output_view_id.is_none(), "nothing was published");
    assert!(
        fx.parser.count.load(Ordering::SeqCst) < 10,
        "stopped at the next checkpoint, not run to the end"
    );
    let pointer = fx.read({
        let repo = repo_id.clone();
        move |s| s.indexing().published(&repo)
    });
    assert!(pointer.is_none(), "{pointer:?}");
}

#[test]
fn deterministic_failures_fail_once_and_transient_ones_back_off_then_stop() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("src");
    let empty = src.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    git(&empty, &["init", "-q"]);
    fx.base_directory(&src.0);
    fx.scan();
    let repo_id = fx.repository_at(&empty);
    let r = run_of(decision(fx.index(&repo_id, false, "1")));
    fx.drain();
    let done = fx.run(&r.id);
    assert_eq!(
        (done.state.as_str(), done.attempt, done.deterministic),
        (RUN_FAILED, 1, true)
    );
    assert_eq!(done.error_code.as_deref(), Some("no_commit"));
    let status = fx.status(&repo_id);
    assert!(matches!(
        status.freshness.unwrap().state,
        IndexFreshnessState::Failed
    ));
    assert!(matches!(
        status.last_run.unwrap().state,
        OperationState::Failed
    ));
    // Git that never answers in time (a POSIX shell fake; Windows process-tree supervision is
    // covered by the process tests).
    #[cfg(unix)]
    transient_git_backs_off_then_stops(fx, &repo_id, &empty);
}

#[cfg(unix)]
fn transient_git_backs_off_then_stops(fx: Fx, repo_id: &Id, empty: &Path) {
    let bin = Temp::new("bin");
    let fake = bin.join("slow-git");
    std::fs::write(&fake, "#!/bin/sh\nsleep 5\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let repo_id = repo_id.clone();
    // Transient, retried with backoff, then failed.
    let mut s = fx.env.settings.clone();
    s.git_program = fake.display().to_string();
    s.git_deadline = Duration::from_millis(200);
    let fx = fx.reopen(s, "owner-b");
    write(empty, "a.rs", "fn a() {}\n");
    commit(empty, "now with a commit");
    let r = run_of(decision(fx.index(&repo_id, false, "1")));
    assert_eq!(fx.drain(), 1);
    let first = fx.run(&r.id);
    assert_eq!(
        (first.state.as_str(), first.attempt, first.deterministic),
        (RUN_QUEUED, 1, false)
    );
    assert_eq!(first.stage, "retry-pending");
    assert!(fx.worker.claim().unwrap().is_none(), "backoff holds it");
    for attempt in 2..=3 {
        fx.now(fx.clock.0.load(Ordering::SeqCst) + 60_000);
        assert_eq!(fx.drain(), 1);
        assert_eq!(fx.run(&r.id).attempt, attempt);
    }
    let last = fx.run(&r.id);
    assert_eq!(last.state, RUN_FAILED, "{last:?}");
    fx.now(fx.clock.0.load(Ordering::SeqCst) + 600_000);
    assert_eq!(fx.drain(), 0, "never retried forever");
}

#[test]
fn an_interrupted_run_resumes_from_its_checkpoint_at_the_next_boot() {
    let _s = serial();
    let fx = Fx::new(|s| s.batch_units = 2);
    let src = Temp::new("src");
    let dir = src.join("resume");
    let files: Vec<(String, String)> = (0..10)
        .map(|i| (format!("m{i}.rs"), format!("fn f{i}() {{}}\n")))
        .collect();
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    repo(&dir, &refs);
    fx.base_directory(&src.0);
    fx.scan();
    let repo_id = fx.repository_at(&dir);
    let r = run_of(decision(fx.index(&repo_id, false, "1")));
    // Shutdown mid-run (after the fifth file): the run keeps its claim and checkpoint.
    let w = fx.worker.clone();
    *fx.parser.hook.lock().unwrap() = Some((5, Box::new(move || w.stop_all())));
    let claimed = fx.worker.claim().unwrap().unwrap();
    fx.worker.execute(claimed);
    let interrupted = fx.run(&r.id);
    assert_eq!(interrupted.state, RUN_RUNNING, "{interrupted:?}");
    let checkpoint = interrupted.generation_id.clone().unwrap();
    assert!(
        interrupted.completed_units > 0
            && interrupted.completed_units < interrupted.total_units.unwrap()
    );
    // The next boot resumes it: the same staging generation, only the rest is parsed.
    *fx.parser.hook.lock().unwrap() = None;
    let fx = fx.restart("owner-next-boot");
    assert_eq!(fx.worker.recover().unwrap(), 1);
    assert_eq!(fx.drain(), 1);
    let done = fx.run(&r.id);
    assert_eq!(
        (done.state.as_str(), done.attempt),
        (RUN_SUCCEEDED, 2),
        "{done:?}"
    );
    assert_eq!(done.generation_id, Some(checkpoint));
    assert!(
        fx.parser.count.load(Ordering::SeqCst) < 10,
        "resumed, not restarted"
    );
    let view = fx.published(&repo_id);
    for i in 0..10 {
        assert!(fx.unit(&view, &format!("m{i}.rs")).is_some());
    }
}

#[test]
fn a_slower_builder_never_overwrites_a_newer_publication() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("src");
    let dir = src.join("race");
    repo(&dir, &[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
    fx.base_directory(&src.0);
    fx.scan();
    let repo_id = fx.repository_at(&dir);
    run_of(decision(fx.index(&repo_id, false, "1")));
    fx.drain();
    let first = fx.published(&repo_id);
    // While the next run builds, another publication moves the pointer.
    let (store, r2, v2) = (fx.store.clone(), repo_id.clone(), first.view.clone());
    *fx.parser.hook.lock().unwrap() = Some((
        3,
        Box::new(move || {
            let (r, v) = (r2.clone(), v2.clone());
            run_write(&*store, WriteOptions::default(), move |tx| {
                let current = tx.indexing().published(&r)?.unwrap().revision;
                let mut other = v.clone();
                other.view_id = Id::new("view-concurrent").unwrap();
                other.view_digest = "f".repeat(64);
                tx.indexing().insert_view(&other)?;
                tx.indexing().publish(&r, &other.view_id, current)
            })
            .unwrap();
        }),
    ));
    write(&dir, "a.rs", "fn a() {}\nfn a3() {}\n");
    write(&dir, "c.rs", "fn c() {}\n");
    commit(&dir, "next");
    let r = run_of(decision(fx.index(&repo_id, false, "1")));
    let claimed = fx.worker.claim().unwrap().unwrap();
    fx.worker.execute(claimed);
    let moved = fx.run(&r.id);
    assert_eq!(moved.state, RUN_QUEUED, "{moved:?}");
    assert_eq!(moved.error_code.as_deref(), Some("pointer_moved"));
    let p = repo_id.clone();
    assert_eq!(
        fx.read(move |s| s.indexing().published(&p))
            .unwrap()
            .view_id
            .as_str(),
        "view-concurrent"
    );
    // Its retry re-plans over the current pointer and publishes.
    fx.now(fx.clock.0.load(Ordering::SeqCst) + 60_000);
    fx.drain();
    assert_eq!(fx.run(&r.id).state, RUN_SUCCEEDED);
    let p = repo_id.clone();
    let published = fx.read(move |s| s.indexing().published(&p)).unwrap();
    assert_eq!(published.revision, 3);
    // The repository refuses a publication from a stale pointer revision.
    let r3 = repo_id.clone();
    let stale = run_write(&*fx.store, WriteOptions::default(), move |tx| {
        tx.indexing()
            .publish(&r3, &Id::new("view-concurrent").unwrap(), 1)
    });
    assert!(matches!(stale, Err(StorageError::RevisionConflict(_))));
}

// ---------------------------------------------------------------------------------------------
// Configuration, scope and removal

#[test]
fn sections_validate_fence_by_revision_and_apply_exclusions_and_languages() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("src");
    let dir = src.join("cfg");
    repo(
        &dir,
        &[
            ("a.rs", "fn a() {}\n"),
            ("gen/b.rs", "fn b() {}\n"),
            ("c.py", "def c():\n    pass\n"),
        ],
    );
    fx.base_directory(&src.0);
    fx.scan();
    let repo_id = fx.repository_at(&dir);
    let configure = |expected: &str, config: Value| {
        decision(fx.command(
            "indexing.configure_sources",
            json!({"scope": {"kind": "Workspace"}, "expectedRevision": expected, "config": config}),
        ))
    };
    let cfg = |langs: Value, ex: Value| json!({"structuralLanguages": langs, "semanticPackageIds": [], "excludePatterns": ex, "maxFileBytes": "1000000"});
    assert_eq!(
        refused(configure("1", cfg(json!(["cobol"]), json!([])))).code,
        DomainErrorCode::InvalidArgument
    );
    assert_eq!(
        refused(configure("1", cfg(json!([]), json!(["a[b"])))).code,
        DomainErrorCode::InvalidArgument
    );
    assert_eq!(
        refused(configure("1", cfg(json!([]), json!(["../escape/**"])))).code,
        DomainErrorCode::InvalidArgument
    );
    assert!(matches!(
        configure("1", cfg(json!(["rust"]), json!(["gen/**"]))),
        DomainDecision::Applied { .. }
    ));
    assert_eq!(
        refused(configure("1", cfg(json!([]), json!([])))).code,
        DomainErrorCode::RevisionConflict
    );
    assert!(matches!(
        configure("2", cfg(json!(["rust"]), json!(["gen/**"]))),
        DomainDecision::NoChange { .. }
    ));
    // A run is fenced by the section revision its caller saw.
    assert_eq!(
        refused(decision(fx.index(&repo_id, false, "1"))).code,
        DomainErrorCode::RevisionConflict
    );
    run_of(decision(fx.index(&repo_id, false, "2")));
    fx.drain();
    let view = fx.published(&repo_id);
    assert!(fx.unit(&view, "gen/b.rs").is_none(), "excluded");
    let (_, py) = fx.unit(&view, "c.py").unwrap();
    assert_eq!(
        py.payload["mode"],
        json!("NotSelected"),
        "an unselected language is text only"
    );
    // A configuration change after publication shows the index stale.
    configure("2", cfg(json!([]), json!([])));
    let status = fx.status(&repo_id);
    let f = status.freshness.unwrap();
    assert!(!f.configuration_current);
    assert!(matches!(f.state, IndexFreshnessState::Stale));
    assert!(f.reasons.0.iter().any(|r| r.0 == "configuration-changed"));
}

#[test]
fn registrations_and_package_identities_link_across_files_and_follow_group_changes() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("src");
    let dir = src.join("svc");
    repo(
        &dir,
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"core\", \"app\"]\n[workspace.package]\nversion = \"0.4.0\"\n",
            ),
            (
                "core/Cargo.toml",
                "[package]\nname = \"core\"\nversion.workspace = true\n",
            ),
            // A function sharing the last segment of a Terraform address.
            ("core/src/lib.rs", "pub fn util() {}\npub fn logs() {}\n"),
            (
                "infra/main.tf",
                "resource \"aws_s3_bucket\" \"logs\" {\n  bucket = \"acme-logs\"\n}\n",
            ),
            (
                "infra/app.tf",
                "resource \"aws_instance\" \"web\" {\n  ami = \"ami-1\"\n  depends_on = [aws_s3_bucket.logs]\n}\n",
            ),
            (
                "app/Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"1.0.0\"\n[dependencies]\ncore = { path = \"../core\" }\n",
            ),
            (
                "app/src/main.rs",
                "use axum::{routing::get, Router};\nmod handlers;\nfn app() -> Router { Router::new().route(\"/users\", get(handlers::list)).route(\"/dup\", get(dup)) }\nfn dup() {}\n",
            ),
            ("app/src/handlers.rs", "pub fn list() {}\n"),
            (
                "app/src/other.rs",
                "use axum::{routing::get, Router};\nfn more() -> Router { Router::new().route(\"/dup\", get(dup2)) }\nfn dup2() {}\n",
            ),
            ("go/go.mod", "module example.com/gosvc\n"),
            (
                "go/api/routes.go",
                "package api\n\nimport (\n\t\"github.com/gin-gonic/gin\"\n\th \"example.com/gosvc/internal/handlers\"\n)\n\nfunc Register(r *gin.Engine) {\n\tr.GET(\"/items\", h.ListItems)\n}\n",
            ),
            (
                "go/internal/handlers/items.go",
                "package handlers\n\nfunc ListItems() {}\n",
            ),
        ],
    );
    fx.base_directory(&src.0);
    fx.scan();
    let repo_id = fx.repository_at(&dir);
    run_of(decision(fx.index(&repo_id, false, "1")));
    fx.drain();
    let view = fx.published(&repo_id);
    let edges = |key: &str| {
        let (v, k) = (view.clone(), key.to_owned());
        fx.read(move |s| views::unit_edges(s.indexing(), &v, &k))
    };
    let handler = |key: &str, route: &str| {
        let nodes = fx.nodes(&view, key);
        let r = nodes
            .iter()
            .find(|n| n.kind == "Route" && n.name == route)
            .unwrap_or_else(|| panic!("no route {route} in {key}"))
            .node_id
            .clone();
        edges(key)
            .into_iter()
            .find(|(e, _)| e.kind == "HANDLES" && e.source_node_id == r)
            .unwrap_or_else(|| panic!("no HANDLES of {route}"))
    };
    // A module-path handler resolves to that module's function.
    let (e, resolves) = handler("app/src/main.rs", "GET /users");
    assert_eq!(e.payload["resolution"], json!("Inferred"), "{e:?}");
    assert_eq!(e.payload["targetUnit"], json!("app/src/handlers.rs"));
    assert!(resolves);
    let list = fx
        .nodes(&view, "app/src/handlers.rs")
        .into_iter()
        .find(|n| n.name == "list")
        .unwrap();
    assert_eq!(e.target_node_id, list.node_id);
    // A route identity registered by two files proves no handler.
    let (dup, _) = handler("app/src/main.rs", "GET /dup");
    assert_eq!(dup.payload["resolution"], json!("Unresolved"));
    assert_eq!(dup.payload["registrationAmbiguous"], json!(true));
    // A Go handler in a module-relative package resolves through the nearest go.mod.
    let (g, _) = handler("go/api/routes.go", "GET /items");
    assert_eq!(g.payload["resolution"], json!("Inferred"), "{g:?}");
    assert_eq!(
        g.payload["targetUnit"],
        json!("go/internal/handlers/items.go")
    );
    // The app's path dependency points at core's definition node exactly.
    let core_def = |view: &views::ViewLayers| {
        fx.nodes(view, "core/Cargo.toml")
            .into_iter()
            .find(|n| n.kind == "Package")
            .unwrap()
    };
    let def = core_def(&view);
    assert_eq!(
        def.payload["qualifiedName"],
        json!("package:cargo:registry:crates.io:core@0.4.0")
    );
    let dep = edges("app/Cargo.toml")
        .into_iter()
        .find(|(e, _)| e.kind == "REFERENCES_PACKAGE")
        .unwrap();
    assert_eq!(dep.0.target_node_id, def.node_id);
    assert!(dep.1, "the target is visible in the view");
    // A domain reference names its target by qualified name, across files, never by the last
    // segment (`logs` is also a Rust function).
    let bucket = fx
        .nodes(&view, "infra/main.tf")
        .into_iter()
        .find(|n| n.payload["qualifiedName"] == json!("resource.aws_s3_bucket.logs"))
        .expect("the bucket resource");
    let depends = edges("infra/app.tf")
        .into_iter()
        .find(|(e, _)| e.kind == "DEPENDS_ON")
        .expect("a dependency");
    assert_eq!(
        depends.0.payload["resolution"],
        json!("Inferred"),
        "{depends:?}"
    );
    assert_eq!(depends.0.target_node_id, bucket.node_id);
    // A change to one member (the workspace version) re-extracts the group: core's definition
    // and the app's reference follow, though core/Cargo.toml itself did not change.
    write(
        &dir,
        "Cargo.toml",
        "[workspace]\nmembers = [\"core\", \"app\"]\n[workspace.package]\nversion = \"0.5.0\"\n",
    );
    commit(&dir, "bump the workspace version");
    run_of(decision(fx.index(&repo_id, false, "1")));
    fx.drain();
    let view2 = fx.published(&repo_id);
    let def2 = core_def(&view2);
    assert_eq!(
        def2.payload["qualifiedName"],
        json!("package:cargo:registry:crates.io:core@0.5.0")
    );
    let (v2, k2) = (view2.clone(), "app/Cargo.toml".to_owned());
    let dep2 = fx
        .read(move |s| views::unit_edges(s.indexing(), &v2, &k2))
        .into_iter()
        .find(|(e, _)| e.kind == "REFERENCES_PACKAGE")
        .unwrap();
    assert_eq!(dep2.0.target_node_id, def2.node_id);
    assert!(dep2.1);
}

#[test]
fn cancelling_a_batch_reports_and_changes_only_visible_runs() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("src");
    let (a, b) = (src.join("a"), src.join("b"));
    repo(&a, &[("a.rs", "fn a() {}\n")]);
    repo(&b, &[("b.rs", "fn b() {}\n")]);
    fx.base_directory(&src.0);
    fx.scan();
    let (ra, rb) = (fx.repository_at(&a), fx.repository_at(&b));
    // One workspace request indexes both: one batch.
    let batch = run_of(decision(fx.command(
        "indexing.index",
        json!({"scope": {"kind": "Workspace"}, "repositoryIds": [ra, rb], "expectedSourceRevision": "1",
            "views": {"kind": "Published", "repositoryIds": [ra, rb]}}),
    )));
    let bid = batch.id.clone();
    let runs = fx.read(move |s| s.indexing().batch_runs(&bid));
    assert_eq!(runs.len(), 2, "{runs:?}");
    let opener = runs.iter().find(|r| r.id == r.batch_id).unwrap().clone();
    let other = runs.iter().find(|r| r.id != opener.id).unwrap().clone();
    // A project that holds only the opener's repository.
    let project = match decision(fx.command(
        "projects.create",
        json!({"name": "P", "description": "", "repositoryIds": [opener.repository_id]}),
    )) {
        DomainDecision::Applied {
            result: OperationResult::ProjectsCreate { value },
        } => value,
        other => panic!("{other:?}"),
    };
    let in_project = fx.user().with_lineage(Some(project.id.clone()), None, None);
    let reply = fx.command_frame(
        &in_project,
        "indexing.cancel",
        json!({"protocol": "1.0-draft.1", "requestKey": "key-cancel-visible",
            "command": {"kind": "Cancel", "operationId": opener.id}}),
    );
    let d = decision(reply);
    let text = serde_json::to_string(&d).unwrap();
    assert!(
        !text.contains(other.id.as_str()),
        "the response names a run the caller cannot see: {text}"
    );
    assert_eq!(run_of(d).id, opener.id);
    assert_eq!(fx.run(&opener.id).state, RUN_CANCELLED);
    let untouched = fx.run(&other.id);
    assert_eq!(untouched.state, RUN_QUEUED);
    assert_eq!(untouched.cancel_requested_at_ms, None);
}

#[test]
fn archived_projects_accept_no_runs_and_their_queued_runs_cancel() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("src");
    let dir = src.join("p");
    repo(&dir, &[("a.rs", "fn a() {}\n")]);
    fx.base_directory(&src.0);
    fx.scan();
    let repo_id = fx.repository_at(&dir);
    let created = decision(fx.command(
        "projects.create",
        json!({"name": "P", "description": "", "repositoryIds": [repo_id]}),
    ));
    let project = match created {
        DomainDecision::Applied {
            result: OperationResult::ProjectsCreate { value },
        } => value,
        other => panic!("{other:?}"),
    };
    let index_in_project = || {
        decision(fx.command(
            "indexing.index",
            json!({"scope": {"kind": "Project", "projectId": project.id}, "repositoryIds": [repo_id], "expectedSourceRevision": "1",
                "views": {"kind": "Published", "repositoryIds": [repo_id]}}),
        ))
    };
    let queued = run_of(index_in_project());
    let p = project.id.clone();
    fx.write(move |tx| {
        let mut stored = tx.projects().project(&p)?.unwrap();
        let expected = stored.revision;
        stored.lifecycle = ARCHIVED.into();
        stored.revision += 1;
        tx.projects().update_project(&stored, expected)
    });
    assert_eq!(fx.drain(), 0);
    let cancelled = fx.run(&queued.id);
    assert_eq!(cancelled.state, RUN_CANCELLED);
    assert_eq!(cancelled.error_code.as_deref(), Some("project_archived"));
    let refusal = refused(index_in_project());
    assert_eq!(refusal.code, DomainErrorCode::Conflict);
    assert_eq!(
        refusal.target_kind.map(|k| k.0).as_deref(),
        Some("project_archived")
    );
}

#[test]
fn removing_an_index_drops_views_and_payload_but_keeps_registration_and_records() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("src");
    let dir = src.join("rm");
    repo(&dir, &[("README.md", "hi\n"), ("a.rs", "fn a() {}\n")]);
    fx.base_directory(&src.0);
    fx.scan();
    let repo_id = fx.repository_at(&dir);
    let r = run_of(decision(fx.index(&repo_id, false, "1")));
    fx.drain();
    let reg = {
        let id = repo_id.clone();
        fx.read(move |s| s.projects().repository(&id)).unwrap()
    };
    let removed = decision(fx.command(
        "repositories.remove_index",
        json!({"repositoryId": repo_id, "expectedRevision": reg.revision.to_string()}),
    ));
    assert!(
        matches!(removed, DomainDecision::Applied { .. }),
        "{removed:?}"
    );
    let readme = fault(fx.query("repositories.readme", json!({"repositoryId": repo_id, "views": {"kind": "Published", "repositoryIds": [repo_id]}}), None));
    assert_eq!(readme.code, DomainErrorCode::ViewUnavailable);
    let status = fx.status(&repo_id);
    assert!(matches!(
        status.freshness.unwrap().state,
        IndexFreshnessState::NotIndexed
    ));
    assert!(status.current_view_ids.0.is_empty());
    let id2 = repo_id.clone();
    assert!(
        fx.read(move |s| s.projects().repository(&id2)).is_some(),
        "the registration stays"
    );
    assert_eq!(fx.run(&r.id).state, RUN_SUCCEEDED, "run history stays");
    let reclaimed = fx.worker.housekeeping(false, &|| false).unwrap();
    assert!(reclaimed.reclaimed_rows > 0);
}

// ---------------------------------------------------------------------------------------------
// Overlays

#[test]
fn overlays_compose_branch_and_worktree_changes_isolated_per_worktree() {
    let _s = serial();
    let fx = Fx::new(|s| s.overlay_ttl_ms = 3_600_000);
    let src = Temp::new("src");
    let dir = src.join("ov");
    repo(
        &dir,
        &[
            (".gitignore", "ignored.rs\n"),
            ("a.rs", "fn a() {}\n"),
            ("b.py", "def b():\n    pass\n"),
            ("c.rs", "pub fn c() {}\n"),
            ("d.rs", "fn d() {}\n"),
        ],
    );
    fx.base_directory(&src.0);
    fx.scan();
    let repo_id = fx.repository_at(&dir);
    run_of(decision(fx.index(&repo_id, false, "1")));
    fx.drain();
    // Worktree one: a branch commit, then staged, unstaged, untracked and ignored changes.
    let wt1 = src.join("ov-topic");
    git(
        &dir,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "topic",
            wt1.to_str().unwrap(),
        ],
    );
    write(&wt1, "a.rs", "fn a() {}\nfn branch_a() {}\n");
    write(&wt1, "e.rs", "fn e() {}\n");
    std::fs::remove_file(wt1.join("b.py")).unwrap();
    let topic = commit(&wt1, "branch");
    write(&wt1, "e.rs", "fn e() {}\nfn staged_e() {}\n");
    git(&wt1, &["add", "e.rs"]);
    write(
        &wt1,
        "a.rs",
        "fn a() {}\nfn branch_a() {}\nfn unstaged_a() {}\n",
    );
    write(&wt1, "f.rs", "fn untracked_f() { c(); }\n");
    std::fs::remove_file(wt1.join("d.rs")).unwrap();
    write(&wt1, "ignored.rs", "fn ignored() {}\n");
    // Worktree two, with its own change.
    let wt2 = src.join("ov-other");
    git(
        &dir,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "other",
            wt2.to_str().unwrap(),
        ],
    );
    write(&wt2, "g.rs", "fn other_g() {}\n");
    fx.scan();
    let w1 = worktree_id(&repo_id, &fx.identity(&wt1));
    let w2 = worktree_id(&repo_id, &fx.identity(&wt2));
    let section_revision = std::cell::Cell::new("1");
    let refresh = |w: &Id| {
        decision(fx.index_command(
            "indexing.refresh_overlay",
            json!({"kind": "RefreshOverlay", "repositoryId": repo_id, "worktreeId": w,
                "expectedSourceRevision": section_revision.get()}),
        ))
    };
    let r1 = run_of(refresh(&w1));
    let r2 = run_of(refresh(&w2));
    assert_eq!(fx.drain(), 2);
    assert_eq!(fx.run(&r1.id).state, RUN_SUCCEEDED, "{:?}", fx.run(&r1.id));
    let ctx = fx.user();
    let (repo2, w1c) = (repo_id.clone(), w1.clone());
    let now = fx.clock.0.load(Ordering::SeqCst);
    let view1 = read_snapshot(&*fx.store, &ReadOptions::default(), |s| {
        views::worktree_view(s, &ctx, &repo2, &w1c, now)
            .map_err(|f| StorageError::InvalidRecord(format!("{f:?}")))
    })
    .unwrap();
    assert_eq!(view1.layers.len(), 3, "worktree, branch and baseline");
    assert!(
        fx.nodes(&view1, "a.rs")
            .iter()
            .any(|n| n.name == "unstaged_a"),
        "unstaged"
    );
    assert!(
        fx.nodes(&view1, "e.rs")
            .iter()
            .any(|n| n.name == "staged_e"),
        "staged"
    );
    assert!(
        fx.nodes(&view1, "f.rs")
            .iter()
            .any(|n| n.name == "untracked_f"),
        "untracked"
    );
    assert!(fx.unit(&view1, "b.py").is_none(), "deleted on the branch");
    assert!(fx.unit(&view1, "d.rs").is_none(), "deleted in the worktree");
    assert!(
        fx.unit(&view1, "ignored.rs").is_none(),
        "ignored files never count"
    );
    let (layer, _) = fx.unit(&view1, "c.rs").unwrap();
    assert_eq!(
        layer, view1.layers[2],
        "unchanged units come from the baseline"
    );
    assert!(
        !fx.nodes(&view1, "g.rs").iter().any(|_| true),
        "another worktree's changes are isolated"
    );
    // The untracked file's call links into the baseline.
    let v = view1.clone();
    let f_edges = fx.read(move |s| views::unit_edges(s.indexing(), &v, "f.rs"));
    let call = f_edges.iter().find(|(e, _)| e.kind == "CALLS").unwrap();
    assert_eq!(call.0.payload["targetUnit"], json!("c.rs"));
    assert!(call.1);
    // The baseline is untouched.
    let base = fx.published(&repo_id);
    assert!(fx.unit(&base, "b.py").is_some() && fx.unit(&base, "f.rs").is_none());
    let status = fx.status(&repo_id);
    let overlays = status.overlays.unwrap().0;
    let o1 = overlays
        .iter()
        .find(|o| o.worktree_id.as_ref() == Some(&w1))
        .unwrap();
    assert!(matches!(o1.state, OverlayState::Ready));
    assert_eq!(
        o1.head_commit.as_ref().map(|c| c.0.as_str()),
        Some(topic.as_str())
    );
    assert!(o1.capture_digest.is_some());
    assert!(matches!(
        overlays
            .iter()
            .find(|o| o.worktree_id.as_ref() == Some(&w2))
            .unwrap()
            .state,
        OverlayState::Ready
    ));
    let _ = r2;
    let first_layers = {
        let oid = o1.overlay_id.clone();
        let o = fx.read(move |s| s.indexing().overlay(&oid)).unwrap();
        (
            o.branch_generation_id.clone().unwrap(),
            o.worktree_generation_id.clone().unwrap(),
        )
    };

    // The same snapshot is reused; a change is observed stale and rebuilt.
    assert!(matches!(refresh(&w1), DomainDecision::NoChange { .. }));
    write(&wt1, "f.rs", "fn untracked_f() { c(); }\nfn more() {}\n");
    let hk = fx.worker.housekeeping(true, &|| false).unwrap();
    assert_eq!((hk.stale_overlays, hk.refreshes_queued), (1, 1), "{hk:?}");
    fx.drain();
    let rebuilt = fx
        .status(&repo_id)
        .overlays
        .unwrap()
        .0
        .into_iter()
        .find(|o| o.worktree_id.as_ref() == Some(&w1))
        .unwrap();
    assert!(matches!(rebuilt.state, OverlayState::Ready));
    assert_ne!(
        rebuilt.capture_digest.clone().map(|c| c.0),
        o1.capture_digest.clone().map(|c| c.0)
    );
    // A refresh observes the worktree itself: a change since the Ready build is never answered
    // by reusing it.
    write(
        &wt1,
        "f.rs",
        "fn untracked_f() { c(); }\nfn more() {}\nfn again() {}\n",
    );
    let again = refresh(&w1);
    assert!(matches!(again, DomainDecision::Applied { .. }), "{again:?}");
    fx.drain();
    // The layers the rebuilds replaced are retired, not left behind.
    let (b, w) = first_layers;
    let states = fx.read(move |s| {
        Ok((
            s.indexing().generation(&b)?.unwrap().state,
            s.indexing().generation(&w)?.unwrap().state,
        ))
    });
    assert_eq!(
        states,
        (GEN_RETIRED.to_owned(), GEN_RETIRED.to_owned()),
        "replaced overlay layers"
    );
    // A source configuration change makes a ready overlay stale at once, and a refresh never
    // reuses a build made under the earlier configuration.
    let o1_id = o1.overlay_id.clone();
    let record = |fx: &Fx| {
        let oid = o1_id.clone();
        fx.read(move |s| s.indexing().overlay(&oid)).unwrap()
    };
    assert_eq!(record(&fx).state, OVERLAY_READY);
    let configured = decision(fx.command(
        "indexing.configure_sources",
        json!({"scope": {"kind": "Workspace"}, "expectedRevision": "1", "config": {
            "structuralLanguages": ["rust"], "semanticPackageIds": [], "excludePatterns": [],
            "maxFileBytes": "524288"}}),
    ));
    assert!(
        matches!(configured, DomainDecision::Applied { .. }),
        "{configured:?}"
    );
    section_revision.set("2");
    assert_eq!(
        record(&fx).state,
        OVERLAY_STALE,
        "invalidated by the configuration write"
    );
    // Even a record still reading Ready is not reused under another configuration.
    let mut ready = record(&fx);
    let expected = ready.revision;
    ready.revision += 1;
    ready.state = OVERLAY_READY.into();
    fx.write(move |tx| tx.indexing().put_overlay(&ready, Some(expected)));
    let status_now = fx.status(&repo_id).overlays.unwrap().0;
    assert!(matches!(
        status_now
            .iter()
            .find(|o| o.worktree_id.as_ref() == Some(&w1))
            .unwrap()
            .state,
        OverlayState::Stale
    ));
    let rebuilt_again = refresh(&w1);
    assert!(
        matches!(rebuilt_again, DomainDecision::Applied { .. }),
        "{rebuilt_again:?}"
    );
    fx.drain();
    assert_eq!(record(&fx).state, OVERLAY_READY);
    // A new baseline makes every overlay over the old one stale.
    write(&dir, "h.rs", "fn h() {}\n");
    commit(&dir, "baseline moves");
    run_of(decision(fx.index(&repo_id, false, section_revision.get())));
    fx.drain();
    let after = fx.status(&repo_id).overlays.unwrap().0;
    assert!(
        after.iter().all(|o| matches!(o.state, OverlayState::Stale)),
        "{after:?}"
    );
    // Retirement: the derived view goes, the record and history stay.
    let o = after
        .iter()
        .find(|o| o.worktree_id.as_ref() == Some(&w2))
        .unwrap();
    let revision = {
        let oid = o.overlay_id.clone();
        fx.read(move |s| s.indexing().overlay(&oid))
            .unwrap()
            .revision
    };
    let retired = decision(fx.command(
        "indexing.retire_view",
        json!({"viewId": o.overlay_id, "expectedRevision": revision.to_string()}),
    ));
    assert!(
        matches!(retired, DomainDecision::Applied { .. }),
        "{retired:?}"
    );
    let o2 = fx
        .status(&repo_id)
        .overlays
        .unwrap()
        .0
        .into_iter()
        .find(|o| o.worktree_id.as_ref() == Some(&w2))
        .unwrap();
    assert!(matches!(o2.state, OverlayState::Unavailable));
    assert!(o2.view_id.is_none());
    // Expiry: past its retention an overlay reads expired and its view is unavailable.
    fx.now(fx.clock.0.load(Ordering::SeqCst) + 4_000_000);
    let expired = fx
        .status(&repo_id)
        .overlays
        .unwrap()
        .0
        .into_iter()
        .find(|o| o.worktree_id.as_ref() == Some(&w1))
        .unwrap();
    assert!(matches!(expired.state, OverlayState::Expired));
    fx.worker.housekeeping(false, &|| false).unwrap();
    let ctx = fx.user();
    let (repo2, w1c) = (repo_id.clone(), w1.clone());
    let now = fx.clock.0.load(Ordering::SeqCst);
    let gone = read_snapshot(&*fx.store, &ReadOptions::default(), |s| {
        Ok(views::worktree_view(s, &ctx, &repo2, &w1c, now).is_err())
    })
    .unwrap();
    assert!(
        gone,
        "a missing or expired view is unavailable, never the baseline"
    );
}

#[test]
fn links_in_a_repository_or_worktree_are_recorded_never_followed() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("src");
    let outside = Temp::new("outside");
    write(&outside.0, "secret.rs", "fn secret_outside() {}\n");
    let dir = src.join("links");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q"]);
    write(&dir, "a.rs", "fn a() {}\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.join("secret.rs"), dir.join("escape.rs")).unwrap();
    commit(&dir, "with a link");
    fx.base_directory(&src.0);
    fx.scan();
    let repo_id = fx.repository_at(&dir);
    run_of(decision(fx.index(&repo_id, false, "1")));
    fx.drain();
    let view = fx.published(&repo_id);
    #[cfg(unix)]
    {
        let (_, link) = fx.unit(&view, "escape.rs").unwrap();
        assert_eq!(link.payload["mode"], json!("Link"));
        assert!(
            !fx.nodes(&view, "escape.rs")
                .iter()
                .any(|n| n.name == "secret_outside")
        );
    }
    // Without a link (no symlink privilege is assumed off Unix) the checkout still indexes and
    // nothing outside it appears.
    #[cfg(not(unix))]
    {
        assert!(fx.unit(&view, "a.rs").is_some());
        assert!(fx.unit(&view, "escape.rs").is_none());
        assert!(
            !fx.nodes(&view, "a.rs")
                .iter()
                .any(|n| n.name == "secret_outside")
        );
    }
    // An untracked link in the worktree is captured as a link, not read.
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.join("secret.rs"), dir.join("escape2.rs")).unwrap();
    let main = worktree_id(&repo_id, &fx.identity(&dir));
    run_of(decision(fx.index_command(
        "indexing.refresh_overlay",
        json!({"kind": "RefreshOverlay", "repositoryId": repo_id, "worktreeId": main, "expectedSourceRevision": "1"}),
    )));
    fx.drain();
    let ctx = fx.user();
    let (repo2, m) = (repo_id.clone(), main.clone());
    let now = fx.clock.0.load(Ordering::SeqCst);
    let v = read_snapshot(&*fx.store, &ReadOptions::default(), |s| {
        views::worktree_view(s, &ctx, &repo2, &m, now)
            .map_err(|f| StorageError::InvalidRecord(format!("{f:?}")))
    })
    .unwrap();
    #[cfg(unix)]
    assert!(
        !fx.nodes(&v, "escape2.rs")
            .iter()
            .any(|n| n.name == "secret_outside")
    );
    // The worktree view composes over the baseline either way.
    assert!(fx.unit(&v, "a.rs").is_some());
}

// ---------------------------------------------------------------------------------------------
// Schema revision 2: ref overlays, the status of one overlay and run listing filters

/// `indexing.refresh_overlay` of a ref (schema revision 2) as the workspace user.
fn refresh_ref(fx: &Fx, repository: &Id, r: &str) -> DomainCommandReply {
    let key = format!("key-{}", fx.n.fetch_add(1, Ordering::SeqCst));
    fx.command_frame(
        &fx.user(),
        "indexing.refresh_overlay",
        json!({"protocol": "1.0-draft.1", "requestKey": key, "schemaRevision": 2,
            "command": {"kind": "RefreshOverlay", "repositoryId": repository, "ref": r,
                "expectedSourceRevision": "1"}}),
    )
}
/// A query at `revision` (schema revision 1 when `None`) under `ctx`.
fn query_as(
    fx: &Fx,
    ctx: &InvocationContext,
    op: &str,
    mut body: Value,
    revision: Option<u32>,
) -> DomainQueryReply {
    body["kind"] = json!(op);
    let mut frame =
        json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-1", "body": body});
    if let Some(r) = revision {
        frame["schemaRevision"] = json!(r);
    }
    let authority = fx.gateway_auth();
    let gateway = Gateway {
        authority: &authority,
        store: &*fx.store,
        ids: &*fx.ids,
    };
    fx.cases
        .query(&gateway, ctx, op, frame.to_string().as_bytes())
        .expect("addressable")
}
fn status_value(r: DomainQueryReply) -> RepositoryIndexStatus {
    match result(r) {
        OperationResult::IndexingStatus { value } => value,
        other => panic!("{other:?}"),
    }
}
fn overlay_at(fx: &Fx, repository: &Id, commit: &str) -> StoredOverlay {
    let (r, c) = (repository.clone(), commit.to_owned());
    fx.read(move |s| s.indexing().overlay_at(&r, &c))
        .expect("a ref overlay at the commit")
}
fn exact_view(fx: &Fx, view: &Id) -> views::ViewLayers {
    let ctx = fx.user();
    let v = view.clone();
    read_snapshot(&*fx.store, &ReadOptions::default(), |s| {
        views::select(
            s,
            &ctx,
            &devworks_core::protocol::data::ViewSelection::Exact { view_ids: vec![v] },
        )
        .map_err(|f| StorageError::InvalidRecord(format!("{f:?}")))
    })
    .expect("the overlay view")
    .remove(0)
}

#[test]
fn ref_overlays_index_a_branch_tag_or_commit_and_reuse_a_live_one() {
    let _s = serial();
    let fx = Fx::new(|s| s.overlay_ttl_ms = 3_600_000);
    let src = Temp::new("src");
    let dir = src.join("refs");
    let base = repo(&dir, &[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
    let other = src.join("other");
    repo(&other, &[("o.rs", "fn o() {}\n")]);
    fx.base_directory(&src.0);
    fx.scan();
    let repo_id = fx.repository_at(&dir);
    run_of(decision(fx.index(&repo_id, false, "1")));
    fx.drain();
    // A branch and a tag that exist only as refs: the checkout stays on main.
    git(&dir, &["checkout", "-q", "-b", "factory/feature-x"]);
    write(&dir, "c.rs", "fn feature_c() { a(); }\n");
    std::fs::remove_file(dir.join("b.rs")).unwrap();
    let feature = commit(&dir, "feature");
    git(&dir, &["tag", "v1"]);
    git(&dir, &["checkout", "-q", "main"]);

    // Queued; the same ref again while it is queued returns the active build.
    let queued = run_of(decision(refresh_ref(&fx, &repo_id, "factory/feature-x")));
    let again = decision(refresh_ref(&fx, &repo_id, "factory/feature-x"));
    assert!(
        matches!(again, DomainDecision::NoChange { .. }),
        "{again:?}"
    );
    assert_eq!(run_of(again).id, queued.id);
    assert_eq!(fx.drain(), 1);
    assert_eq!(
        fx.run(&queued.id).state,
        RUN_SUCCEEDED,
        "{:?}",
        fx.run(&queued.id)
    );
    // Identified by its repository and commit; no worktree, capture or worktree layer.
    let stored = overlay_at(&fx, &repo_id, &feature);
    assert_eq!(stored.worktree_id, None);
    assert_eq!(stored.worktree_path, None);
    assert_eq!(stored.worktree_generation_id, None);
    assert_eq!(stored.capture_digest, None);

    // indexing.status of the overlay: its repository's status listing exactly that overlay.
    let s = status_value(query_as(
        &fx,
        &fx.user(),
        "indexing.status",
        json!({"overlayId": stored.id}),
        Some(2),
    ));
    assert_eq!(s.repository_id, repo_id);
    let overlays = s.overlays.unwrap().0;
    assert_eq!(overlays.len(), 1, "{overlays:?}");
    let o = &overlays[0];
    assert_eq!(o.overlay_id, stored.id);
    assert!(matches!(o.state, OverlayState::Ready), "{:?}", o.state);
    assert_eq!(o.worktree_id, None);
    assert_eq!(
        o.head_commit.as_ref().map(|c| c.0.as_str()),
        Some(feature.as_str())
    );
    assert_eq!(
        o.head_ref.as_ref().map(|c| c.0.as_str()),
        Some("refs/heads/factory/feature-x")
    );
    assert_eq!(
        o.base_commit.as_ref().map(|c| c.0.as_str()),
        Some(base.as_str())
    );
    assert!(o.capture_digest.is_none());

    // The view: the branch layer (read from the commit) over the baseline.
    let view = exact_view(&fx, o.view_id.as_ref().unwrap());
    assert_eq!(view.layers.len(), 2, "branch and baseline");
    assert!(
        fx.nodes(&view, "c.rs")
            .iter()
            .any(|n| n.name == "feature_c")
    );
    assert!(fx.unit(&view, "b.rs").is_none(), "deleted on the branch");
    assert_eq!(fx.unit(&view, "a.rs").unwrap().0, view.layers[1]);
    let published = fx.published(&repo_id);
    assert!(fx.unit(&published, "b.rs").is_some() && fx.unit(&published, "c.rs").is_none());

    // The same commit by its ID or a tag reuses the live overlay: never rebuilt.
    for r in [feature.as_str(), "v1", "refs/tags/v1"] {
        let d = decision(refresh_ref(&fx, &repo_id, r));
        assert!(matches!(d, DomainDecision::NoChange { .. }), "{r}: {d:?}");
        assert_eq!(run_of(d).id, queued.id, "{r}");
    }
    assert_eq!(fx.drain(), 0);

    // A ref at the baseline commit gets its own (empty) branch layer, never the published view.
    run_of(decision(refresh_ref(&fx, &repo_id, "main")));
    assert_eq!(fx.drain(), 1);
    let at_base = overlay_at(&fx, &repo_id, &base);
    assert_eq!(at_base.state, OVERLAY_READY);
    assert_ne!(at_base.view_id.as_ref(), Some(&published.view.view_id));
    assert_eq!(
        exact_view(&fx, at_base.view_id.as_ref().unwrap())
            .layers
            .len(),
        2
    );
    // The repository status lists both; the overlay status only the one asked for.
    assert_eq!(fx.status(&repo_id).overlays.unwrap().0.len(), 2);
    let only = status_value(query_as(
        &fx,
        &fx.user(),
        "indexing.status",
        json!({"overlayId": at_base.id}),
        Some(2),
    ))
    .overlays
    .unwrap()
    .0;
    assert_eq!(
        only.iter().map(|o| &o.overlay_id).collect::<Vec<_>>(),
        [&at_base.id]
    );

    // Past its retention it is no longer live: the next request queues a new build.
    fx.now(T0 + 3_600_000 + 1);
    let rebuilt = decision(refresh_ref(&fx, &repo_id, "v1"));
    assert!(
        matches!(rebuilt, DomainDecision::Applied { .. }),
        "{rebuilt:?}"
    );
    let rebuilt = run_of(rebuilt);
    assert_ne!(rebuilt.id, queued.id);
    let requeued = overlay_at(&fx, &repo_id, &feature);
    assert_eq!(requeued.id, stored.id, "one overlay per commit");
    assert_eq!(requeued.state, OVERLAY_QUEUED);
    assert_eq!(requeued.head_ref.as_deref(), Some("refs/tags/v1"));

    // Unknown and abbreviated refs, and unknown repositories, are refused.
    for r in ["no-such-branch", &feature[..12]] {
        let f = refused(decision(refresh_ref(&fx, &repo_id, r)));
        assert_eq!(f.code, DomainErrorCode::InvalidArgument, "{r}");
        assert_eq!(f.target_kind.map(|k| k.0).as_deref(), Some("ref"), "{r}");
    }
    let f = refused(decision(refresh_ref(&fx, &id("repo-missing"), "main")));
    assert_eq!(f.code, DomainErrorCode::NotFound);
    // Revision 1 has no ref member: refused at admission.
    let r1 = fx.command_frame(
        &fx.user(),
        "indexing.refresh_overlay",
        json!({"protocol": "1.0-draft.1", "requestKey": "key-r1-ref",
            "command": {"kind": "RefreshOverlay", "repositoryId": repo_id, "ref": "main",
                "expectedSourceRevision": "1"}}),
    );
    assert!(matches!(r1, CommandReply::Rejected { .. }), "{r1:?}");

    // An unknown overlay, and an overlay of a repository the caller cannot see, are not found
    // (naming the overlay, never its repository).
    let f = fault(query_as(
        &fx,
        &fx.user(),
        "indexing.status",
        json!({"overlayId": "overlay-missing"}),
        Some(2),
    ));
    assert_eq!(f.code, DomainErrorCode::NotFound);
    let other_id = fx.repository_at(&other);
    let project = match decision(fx.command(
        "projects.create",
        json!({"name": "P", "description": "", "repositoryIds": [other_id]}),
    )) {
        DomainDecision::Applied {
            result: OperationResult::ProjectsCreate { value },
        } => value,
        other => panic!("{other:?}"),
    };
    let in_project = fx.user().with_lineage(Some(project.id.clone()), None, None);
    let f = fault(query_as(
        &fx,
        &in_project,
        "indexing.status",
        json!({"overlayId": stored.id}),
        Some(2),
    ));
    assert_eq!(f.code, DomainErrorCode::NotFound);
    assert_eq!(f.target_id.as_ref(), Some(&stored.id));
    assert_eq!(f.target_kind.map(|k| k.0).as_deref(), Some("overlay"));
}

#[test]
fn run_listings_filter_by_state_and_kind_and_page_within_the_filter() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("src");
    let (a, b) = (src.join("a"), src.join("b"));
    repo(&a, &[("a.rs", "fn a() {}\n")]);
    repo(&b, &[("b.rs", "fn b() {}\n")]);
    fx.base_directory(&src.0);
    fx.scan();
    let (ra, rb) = (fx.repository_at(&a), fx.repository_at(&b));
    // Two baseline runs (succeeded), then a rebuild and an overlay build (queued).
    let index_a = run_of(decision(fx.index(&ra, false, "1")));
    fx.now(T0 + 1_000);
    let index_b = run_of(decision(fx.index(&rb, false, "1")));
    fx.drain();
    fx.now(T0 + 2_000);
    let reindex = run_of(decision(fx.index(&ra, true, "1")));
    fx.now(T0 + 3_000);
    let overlay = run_of(decision(refresh_ref(&fx, &rb, "main")));
    let page = |filter: Value, limit: u32, cursor: Option<&str>| {
        let mut body = json!({"scope": {"kind": "Workspace"}, "page": {"limit": limit}});
        for (k, v) in filter.as_object().unwrap() {
            body[k] = v.clone();
        }
        if let Some(c) = cursor {
            body["page"]["cursor"] = json!(c);
        }
        query_as(&fx, &fx.user(), "indexing.runs", body, Some(2))
    };
    let ids = |r: DomainQueryReply| -> (Vec<Id>, Option<String>) {
        match result(r) {
            OperationResult::IndexingRuns { value } => (
                value.items.0.into_iter().map(|r| r.id).collect(),
                value.next_cursor.map(|c| c.0),
            ),
            other => panic!("{other:?}"),
        }
    };
    let all = |filter: Value| ids(page(filter, 50, None)).0;
    assert_eq!(all(json!({})).len(), 4);
    assert_eq!(all(json!({"kinds": ["Overlay"]})), vec![overlay.id.clone()]);
    assert_eq!(
        all(json!({"states": ["Queued"]})),
        vec![overlay.id.clone(), reindex.id.clone()]
    );
    assert_eq!(
        all(json!({"states": ["Succeeded", "Failed"], "kinds": ["Index"]})),
        vec![index_b.id.clone(), index_a.id.clone()]
    );
    assert!(all(json!({"states": ["Cancelled"]})).is_empty());
    assert_eq!(
        all(json!({"states": ["Queued"], "kinds": ["Reindex"], "repositoryId": ra})),
        vec![reindex.id.clone()]
    );
    // Pages of one item continue within the filter and never skip or repeat a run.
    let filter = json!({"kinds": ["Reindex", "Index"]});
    let (first, next) = ids(page(filter.clone(), 1, None));
    let (second, next) = ids(page(filter.clone(), 1, next.as_deref()));
    let (third, last) = ids(page(filter.clone(), 1, next.as_deref()));
    assert_eq!(
        [first, second, third].concat(),
        vec![reindex.id.clone(), index_b.id.clone(), index_a.id.clone()]
    );
    assert_eq!(last, None);
    // A cursor is bound to its filter.
    let (_, next) = ids(page(filter, 1, None));
    let f = fault(page(json!({"kinds": ["Index"]}), 1, next.as_deref()));
    assert_eq!(f.code, DomainErrorCode::CursorExpired);
    // Revision 1 has no filters.
    let r1 = query_as(
        &fx,
        &fx.user(),
        "indexing.runs",
        json!({"scope": {"kind": "Workspace"}, "page": {"limit": 5}, "states": ["Queued"]}),
        None,
    );
    assert!(matches!(r1, DomainQueryReply::Rejected { .. }), "{r1:?}");
}

#[test]
fn swift_extensions_module_scopes_and_call_families_link_by_the_retained_rules() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("src");
    let dir = src.join("mixed");
    repo(
        &dir,
        &[
            (
                "Package.swift",
                "// swift-tools-version:5.9\nimport PackageDescription\nlet package = Package(\n    name: \"Demo\",\n    targets: [\n        .target(name: \"App\"),\n        .target(name: \"Core\"),\n        .target(name: \"Lib\", dependencies: [\"Core\"]),\n    ]\n)\n",
            ),
            (
                "Sources/App/Thing.swift",
                "struct Thing {\n    func a() {}\n}\n",
            ),
            (
                "Sources/App/Ext.swift",
                "extension Thing: Codable {\n    func y() {}\n}\nextension String {\n    func z() {}\n}\n",
            ),
            (
                "Sources/App/main.swift",
                "func start() {\n    helper()\n}\n",
            ),
            ("Sources/App/A.swift", "func helper() {}\n"),
            ("Sources/App/S.swift", "func shared() {}\n"),
            (
                "Sources/App/Box.swift",
                "struct Box {\n    func open() {}\n}\n",
            ),
            (
                "Sources/Core/Box.swift",
                "struct Box {\n    func open() {}\n}\n",
            ),
            (
                "Sources/App/use.swift",
                "func go() {\n    let b = Box()\n    b.open()\n}\n",
            ),
            (
                "Sources/Core/C.swift",
                "func helper() {}\nfunc shared() {}\n",
            ),
            (
                "Sources/Lib/L.swift",
                "import Core\nfunc run() {\n    shared()\n}\n",
            ),
            (
                "src/ffi.rs",
                "pub fn run() {\n    cpp_only();\n    cpp_c();\n    py_fn();\n}\n",
            ),
            (
                "native/lib.cpp",
                "void cpp_only() {}\nextern \"C\" void cpp_c() {}\n",
            ),
            ("tools/x.py", "def py_fn():\n    pass\n"),
        ],
    );
    fx.base_directory(&src.0);
    fx.scan();
    let repo_id = fx.repository_at(&dir);
    run_of(decision(fx.index(&repo_id, false, "1")));
    fx.drain();
    let view = fx.published(&repo_id);
    let edges = |key: &str| {
        let (v, k) = (view.clone(), key.to_owned());
        fx.read(move |s| views::unit_edges(s.indexing(), &v, &k))
    };
    let node = |key: &str, name: &str| {
        fx.nodes(&view, key)
            .into_iter()
            .find(|n| n.name == name)
            .unwrap_or_else(|| panic!("no {name} in {key}"))
    };

    // Swift extension members (and the extension's conformance) attach to the extended type
    // once its path names one type; an external type's stay under the extension's file.
    let thing = node("Sources/App/Thing.swift", "Thing");
    let ext = edges("Sources/App/Ext.swift");
    let defines = |member: &str| {
        let m = node("Sources/App/Ext.swift", member);
        ext.iter()
            .find(|(e, _)| e.kind == "DEFINES_METHOD" && e.target_node_id == m.node_id)
            .unwrap_or_else(|| panic!("no DEFINES_METHOD of {member}: {ext:?}"))
            .0
            .clone()
    };
    let y = defines("y");
    assert_eq!(y.source_node_id, thing.node_id, "{y:?}");
    assert_eq!(y.payload["sourceUnit"], json!("Sources/App/Thing.swift"));
    assert_eq!(y.payload["sourceResolution"], json!("Inferred"));
    let codable = ext
        .iter()
        .find(|(e, _)| {
            e.kind == "IMPLEMENTS" && e.payload["targetName"] == json!("swift-type:Codable")
        })
        .unwrap_or_else(|| panic!("no Codable conformance: {ext:?}"))
        .0
        .clone();
    assert_eq!(codable.source_node_id, thing.node_id, "{codable:?}");
    let z = defines("z");
    let file = node("Sources/App/Ext.swift", "Ext.swift");
    assert_eq!(z.source_node_id, file.node_id, "{z:?}");
    assert_eq!(z.payload["sourceResolution"], json!("Unresolved"));
    assert_eq!(z.payload["sourceCandidates"], json!(0));

    // A Swift name several modules declare binds to the caller's own module's, else to the
    // single one of the modules it imports.
    let call = |key: &str, callee: &str| {
        edges(key)
            .into_iter()
            .find(|(e, _)| e.kind == "CALLS" && e.payload["targetName"] == json!(callee))
            .unwrap_or_else(|| panic!("no call of {callee} in {key}"))
            .0
    };
    let helper = call("Sources/App/main.swift", "helper");
    assert_eq!(
        helper.payload["resolution"],
        json!("Inferred"),
        "{helper:?}"
    );
    assert_eq!(helper.payload["targetUnit"], json!("Sources/App/A.swift"));
    let shared = call("Sources/Lib/L.swift", "shared");
    assert_eq!(
        shared.payload["resolution"],
        json!("Inferred"),
        "{shared:?}"
    );
    assert_eq!(shared.payload["targetUnit"], json!("Sources/Core/C.swift"));
    // A receiver type path two modules declare names the one the caller's module sees, so the
    // call binds through that type (not by its name alone).
    let open = call("Sources/App/use.swift", "open");
    assert_eq!(
        open.payload["properties"]["receiver_kind"],
        json!("Resolved"),
        "{open:?}"
    );
    assert_eq!(
        open.payload["targetUnit"],
        json!("Sources/App/Box.swift"),
        "{open:?}"
    );
    assert!(open.payload.get("resolutionBasis").is_none(), "{open:?}");

    // Rust reaches C++ only through C linkage, and never another family's function.
    let cpp_only = call("src/ffi.rs", "cpp_only");
    assert_eq!(
        cpp_only.payload["resolution"],
        json!("Unresolved"),
        "{cpp_only:?}"
    );
    assert_eq!(cpp_only.payload["candidates"], json!(0));
    let cpp_c = call("src/ffi.rs", "cpp_c");
    assert_eq!(cpp_c.payload["resolution"], json!("Inferred"), "{cpp_c:?}");
    assert_eq!(cpp_c.payload["targetUnit"], json!("native/lib.cpp"));
    let py = call("src/ffi.rs", "py_fn");
    assert_eq!(py.payload["candidates"], json!(0), "{py:?}");
}

#[test]
fn ended_ref_overlay_records_are_pruned_past_both_limits_in_bounded_passes() {
    let _s = serial();
    let fx = Fx::new(|s| s.overlay_prune_batch = 3);
    let src = Temp::new("src");
    let dir = src.join("prune");
    repo(&dir, &[("src/lib.rs", "pub fn a() {}\n")]);
    fx.base_directory(&src.0);
    fx.scan();
    let repo_id = fx.repository_at(&dir);
    let index = run_of(decision(fx.index(&repo_id, false, "1")));
    fx.drain();
    let view = fx.published(&repo_id).view;
    let day = 86_400_000;
    let overlay = |n: u32, state: &str, ended_at: i64| StoredOverlay {
        id: Id::new(format!("ovl-{n:04}")).unwrap(),
        repository_id: repo_id.clone(),
        worktree_id: None,
        worktree_path: None,
        state: state.into(),
        revision: 1,
        base_generation_id: None,
        base_oid: None,
        head_oid: Some(format!("{n:040x}")),
        head_ref: None,
        capture_digest: None,
        branch_generation_id: None,
        worktree_generation_id: None,
        view_id: None,
        run_id: None,
        reason: None,
        created_at_ms: ended_at,
        updated_at_ms: ended_at,
        expires_at_ms: T0 + 365 * day,
    };
    // 40 expired records ended at T0 + n; older than both limits: n = 0..8.
    let mut all: Vec<StoredOverlay> = (0..40)
        .map(|n| overlay(n, OVERLAY_EXPIRED, T0 + i64::from(n)))
        .collect();
    // One of them still holds a layer no view names (with graph rows).
    let layer = Id::new("gen-ovl-0000").unwrap();
    all[0].branch_generation_id = Some(layer.clone());
    let (g, r) = (layer.clone(), repo_id.clone());
    fx.write(move |tx| {
        tx.indexing().insert_generation(&StoredGeneration {
            id: g.clone(),
            repository_id: r,
            layer: LAYER_BRANCH.into(),
            state: GEN_STAGING.into(),
            source_digest: "b".repeat(40),
            watermark: 0,
            manifest: json!({}),
        })?;
        tx.indexing().put_unit(
            &StoredUnit {
                generation_id: g,
                unit_key: "src/x.rs".into(),
                content_digest: "c".repeat(64),
                action: UNIT_REPLACE.into(),
                payload: json!({}),
            },
            &[StoredNode {
                unit_key: "src/x.rs".into(),
                node_id: "n-x".into(),
                kind: "Function".into(),
                name: "x".into(),
                payload: json!({"qualifiedName": "x"}),
            }],
            &[],
        )
    });
    // The oldest of all, but a view still names one (its own view) or one of its layers.
    let mut named = overlay(100, OVERLAY_FAILED, T0 - 10);
    named.view_id = Some(view.view_id.clone());
    let mut layered = overlay(101, OVERLAY_UNAVAILABLE, T0 - 9);
    layered.branch_generation_id = Some(view.baseline_id.clone());
    all.extend([named, layered, overlay(102, OVERLAY_READY, T0 - 8)]);
    fx.write(move |tx| {
        for o in &all {
            tx.indexing().put_overlay(o, None)?;
        }
        Ok(())
    });
    // A finished run of an overlay that will be pruned: run history survives.
    let mut run = fx.run(&index.id);
    run.id = Id::new("run-ovl-0000").unwrap();
    run.kind = RUN_OVERLAY.into();
    run.overlay_id = Some(Id::new("ovl-0000").unwrap());
    run.state = RUN_QUEUED.into();
    run.stage = "queued".into();
    run.revision = 1;
    run.owner = None;
    run.lease_until_ms = None;
    run.finished_at_ms = None;
    let mut done = run.clone();
    done.state = RUN_CANCELLED.into();
    done.stage = "cancelled".into();
    done.revision = 2;
    done.finished_at_ms = Some(T0);
    fx.write(move |tx| {
        tx.indexing().insert_run(&run)?;
        tx.indexing().update_run(&done, 1)
    });
    let exists = |n: u32| {
        let id = Id::new(format!("ovl-{n:04}")).unwrap();
        fx.read(move |s| s.indexing().overlay(&id)).is_some()
    };
    let pass = || {
        fx.worker
            .housekeeping(false, &|| false)
            .unwrap()
            .pruned_overlays
    };

    // Beyond the newest 32 but within the 14-day window: kept.
    fx.now(T0 + day);
    assert_eq!(pass(), 0);
    // Past both limits: pruned, at most 3 per pass.
    fx.now(T0 + 15 * day);
    assert_eq!((pass(), pass(), pass(), pass()), (3, 3, 2, 0));
    assert!(
        (0..8).all(|n| !exists(n)),
        "the 8 oldest expired records are pruned"
    );
    let g = layer.clone();
    assert!(
        fx.read(move |s| s.indexing().generation(&g)).is_none(),
        "a pruned record's layer and graph rows are reclaimed"
    );
    assert!((8..40).all(exists), "the newest 32 ended records are kept");
    assert!(
        exists(100) && exists(101),
        "records a view still names are kept"
    );
    assert!(exists(102), "a live overlay is kept");
    let kept = fx.run(&Id::new("run-ovl-0000").unwrap());
    assert_eq!(kept.state, RUN_CANCELLED);
    assert_eq!(kept.overlay_id.as_ref().map(Id::as_str), Some("ovl-0000"));
    // The per-repository count is the workspace setting.
    let d = descriptor(OVERLAY_RECORDS_KEPT_KEY).unwrap();
    let assignment = serde_json::from_value(json!({
        "key": OVERLAY_RECORDS_KEPT_KEY,
        "value": {"schema": d.schema_ref(), "value": 30},
    }))
    .unwrap();
    let ctx = fx.user();
    fx.write(move |tx| {
        devworks_core::settings::service::apply_human_assignments(
            tx,
            &ctx,
            &ConfigScope::Workspace,
            &[assignment],
            json!({"test": true}),
        )
        .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    });
    assert_eq!(pass(), 2);
    assert!(!exists(8) && !exists(9) && exists(10));
}

/// `artifacts.read` of `content` under `ctx`.
fn read_artifact(
    fx: &Fx,
    ctx: &InvocationContext,
    content: &devworks_core::protocol::provider::ArtifactRef,
) -> DomainQueryReply {
    let frame = json!({"protocol": "1.0-draft.1", "queryId": "q-read", "limit": 1,
        "query": {"kind": "Read", "artifactId": content.artifact_id, "versionId": content.version_id}});
    let authority = fx.gateway_auth();
    let gateway = Gateway {
        authority: &authority,
        store: &*fx.store,
        ids: &*fx.ids,
    };
    fx.cases
        .query(
            &gateway,
            ctx,
            "artifacts.read",
            frame.to_string().as_bytes(),
        )
        .expect("addressable")
}

#[test]
fn repository_texts_are_readable_wherever_their_repository_is_a_member() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("src");
    let dir = src.join("texts");
    repo(&dir, &[("README.md", "# Texts\n"), ("a.rs", "fn a() {}\n")]);
    let other = src.join("other");
    repo(&other, &[("b.rs", "fn b() {}\n")]);
    fx.base_directory(&src.0);
    fx.scan();
    let (repo_id, other_id) = (fx.repository_at(&dir), fx.repository_at(&other));
    run_of(decision(fx.index(&repo_id, false, "1")));
    fx.drain();
    let create = |name: &str, member: &Id| match decision(fx.command(
        "projects.create",
        json!({"name": name, "description": "", "repositoryIds": [member]}),
    )) {
        DomainDecision::Applied {
            result: OperationResult::ProjectsCreate { value },
        } => value.id,
        other => panic!("{other:?}"),
    };
    let (member, outsider) = (create("Member", &repo_id), create("Outsider", &other_id));
    // A human in the member project gets the README through the repository...
    let human = fx.user().with_lineage(Some(member.clone()), None, None);
    let text = match result(query_as(
        &fx,
        &human,
        "repositories.readme",
        json!({"repositoryId": repo_id, "views": {"kind": "Published", "repositoryIds": [repo_id]}}),
        None,
    )) {
        OperationResult::RepositoriesReadme { value } => value,
        other => panic!("{other:?}"),
    };
    // ...and can read the artifact it names, as can an agent bound to that project.
    assert!(
        matches!(
            result(read_artifact(&fx, &human, &text.content)),
            OperationResult::ArtifactsRead { .. }
        ),
        "a project-scoped human reads its member repository's README"
    );
    let agent = |who: &str, project: &devworks_core::protocol::ProjectId| {
        let actor = ActorId(id(who));
        let (a, p, by) = (actor.clone(), project.clone(), fx.user());
        fx.write(move |tx| {
            tx.actors().register(&a, PrincipalKind::Agent, "agent")?;
            devworks_core::authority::service::bind_principal(
                tx,
                &by,
                NewPrincipalBinding {
                    actor: a.clone(),
                    kind: PrincipalKind::Agent,
                    role: Some(devworks_core::settings::catalog::AgentRole::Worker),
                    project_id: Some(p),
                    tool_families: None,
                    bound_by: ActorId(id("user")),
                },
                None,
            )
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
        });
        let info = fx.store.info();
        HostIngress::new(&*fx.store)
            .context(
                actor,
                PrincipalKind::Agent,
                id("invocation-1"),
                id("ui-main"),
                Counter::new(info.restore_epoch).unwrap(),
                Revision::new(1).unwrap(),
            )
            .with_lineage(Some(project.clone()), None, None)
    };
    assert!(
        matches!(
            result(read_artifact(
                &fx,
                &agent("agent-member", &member),
                &text.content
            )),
            OperationResult::ArtifactsRead { .. }
        ),
        "an agent of the member project reads it"
    );
    // A project without the repository reads neither the repository nor its text.
    let stranger = fx.user().with_lineage(Some(outsider.clone()), None, None);
    fault(read_artifact(&fx, &stranger, &text.content));
    fault(read_artifact(
        &fx,
        &agent("agent-outsider", &outsider),
        &text.content,
    ));
}

#[test]
fn an_overlay_over_a_baseline_of_another_configuration_reconciles_its_inherited_units() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("src");
    let dir = src.join("configured");
    repo(
        &dir,
        &[
            ("src/lib.rs", "pub fn kept() {}\n"),
            ("gen/a.rs", "pub fn generated() {}\n"),
            ("tools/b.py", "def tool():\n    pass\n"),
        ],
    );
    fx.base_directory(&src.0);
    fx.scan();
    let repo_id = fx.repository_at(&dir);
    let configure = |expected: &str, languages: Value, excludes: Value| {
        let d = decision(fx.command(
            "indexing.configure_sources",
            json!({"scope": {"kind": "Workspace"}, "expectedRevision": expected, "config": {
                "structuralLanguages": languages, "semanticPackageIds": [], "excludePatterns": excludes,
                "maxFileBytes": "524288"}}),
        ));
        assert!(matches!(d, DomainDecision::Applied { .. }), "{d:?}");
    };
    // The baseline: Rust only, nothing excluded.
    configure("1", json!(["rust"]), json!([]));
    run_of(decision(fx.index(&repo_id, false, "2")));
    fx.drain();
    let baseline = fx.published(&repo_id);
    assert!(!has_node(
        &fx.nodes(&baseline, "tools/b.py"),
        "Function",
        "#function:tool()"
    ));
    assert!(fx.unit(&baseline, "gen/a.rs").is_some());
    // Then every language, with gen/ excluded; the baseline is not rebuilt.
    configure("2", json!([]), json!(["**/gen/**"]));
    let worktree = worktree_id(&repo_id, &fx.identity(&dir));
    let run = run_of(decision(fx.index_command(
        "indexing.refresh_overlay",
        json!({"kind": "RefreshOverlay", "repositoryId": repo_id, "worktreeId": worktree,
            "expectedSourceRevision": "3"}),
    )));
    fx.drain();
    assert_eq!(
        fx.run(&run.id).state,
        RUN_SUCCEEDED,
        "{:?}",
        fx.run(&run.id)
    );
    let (r, w) = (repo_id.clone(), worktree.clone());
    let record = fx
        .read(move |s| s.indexing().overlay_for(&r, &w))
        .expect("the overlay");
    let view = exact_view(&fx, record.view_id.as_ref().expect("a view"));
    // A clean worktree at the baseline commit still gets a layer that reconciles: the newly
    // selected Python file is extracted...
    assert!(
        has_node(
            &fx.nodes(&view, "tools/b.py"),
            "Function",
            "#function:tool()"
        ),
        "{:?}",
        fx.nodes(&view, "tools/b.py")
    );
    // ...the newly excluded file is masked...
    assert!(fx.unit(&view, "gen/a.rs").is_none());
    // ...and an unaffected unit is still the baseline's.
    assert!(has_node(
        &fx.nodes(&view, "src/lib.rs"),
        "Function",
        "#function:kept()"
    ));
    let (_, lib) = fx.unit(&view, "src/lib.rs").unwrap();
    assert_eq!(
        lib.generation_id, baseline.view.baseline_id,
        "inherited, not restaged"
    );
    // The baseline itself is unchanged.
    let baseline = fx.published(&repo_id);
    assert!(fx.unit(&baseline, "gen/a.rs").is_some());
}

#[test]
fn a_request_mixing_reused_and_new_runs_is_identified_by_its_own_batch() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("src");
    let (da, db) = (src.join("a"), src.join("b"));
    repo(&da, &[("a.rs", "fn a() {}\n")]);
    repo(&db, &[("b.rs", "fn b() {}\n")]);
    fx.base_directory(&src.0);
    fx.scan();
    let (ra, rb) = (fx.repository_at(&da), fx.repository_at(&db));
    // An earlier request queued A.
    let earlier = run_of(decision(fx.index(&ra, false, "1")));
    // This request reuses A's active run and records a new one for B.
    let reply = run_of(decision(fx.command(
        "indexing.index",
        json!({"scope": {"kind": "Workspace"}, "repositoryIds": [ra, rb], "expectedSourceRevision": "1",
            "views": {"kind": "Published", "repositoryIds": [ra, rb]}}),
    )));
    let b = fx
        .read({
            let rb = rb.clone();
            move |s| s.indexing().active_run(&rb, None)
        })
        .expect("B's new run");
    assert_eq!(
        reply.id, b.id,
        "the record names the batch this request recorded"
    );
    assert_ne!(reply.id, earlier.id);
    let listed = serde_json::to_string(&reply.diagnostics).unwrap();
    assert!(
        listed.contains(&format!("run {} Queued (reused)", earlier.id.as_str())),
        "{listed}"
    );
    // Cancelling by the reply's ID cancels what this request recorded; the reused run belongs
    // to the earlier request and its own ID cancels it.
    let cancelled = run_of(decision(fx.index_command(
        "indexing.cancel",
        json!({"kind": "Cancel", "operationId": reply.id}),
    )));
    assert_eq!(cancelled.id, b.id);
    assert_eq!(fx.run(&b.id).state, RUN_CANCELLED);
    assert_eq!(fx.run(&earlier.id).state, RUN_QUEUED);
}

#[test]
fn a_capped_candidate_lookup_never_yields_a_unique_target() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("src");
    let dir = src.join("many");
    // 499 properties named `f` sort before the two functions named `f`: the first page of the
    // name lookup holds 500 nodes and only one callable among them.
    let many: String = (0..499)
        .map(|i| format!("struct T{i} {{\n    var f: Int {{ 0 }}\n}}\n"))
        .collect();
    repo(
        &dir,
        &[
            ("A_many.swift", many.as_str()),
            ("B_one.swift", "func f() {}\n"),
            ("Z_two.swift", "func f() {}\n"),
            ("main.swift", "func start() {\n    f()\n}\n"),
        ],
    );
    fx.base_directory(&src.0);
    fx.scan();
    let repo_id = fx.repository_at(&dir);
    run_of(decision(fx.index(&repo_id, false, "1")));
    fx.drain();
    let view = fx.published(&repo_id);
    let properties = fx
        .nodes(&view, "A_many.swift")
        .into_iter()
        .filter(|n| n.kind == "Property" && n.name == "f")
        .count();
    assert_eq!(properties, 499);
    let v = view.clone();
    let edges = fx.read(move |s| views::unit_edges(s.indexing(), &v, "main.swift"));
    let call = edges
        .iter()
        .find(|(e, _)| e.kind == "CALLS" && e.payload["targetName"] == json!("f"))
        .unwrap_or_else(|| panic!("no call of f: {edges:?}"))
        .0
        .clone();
    assert_eq!(call.payload["resolution"], json!("Unresolved"), "{call:?}");
    assert_eq!(call.payload["candidatesTruncated"], json!(true));
    assert!(call.payload.get("targetUnit").is_none());
}

#[test]
fn adding_or_removing_a_dependency_manifest_reconciles_the_files_it_governs() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("src");
    let pods = "Pods/Lib/A.swift";
    let pod_fn = "#function:pod_a()";
    // X has no Podfile at its baseline (its Pods/ files are indexed); Y has one (excluded).
    let x = src.join("x");
    let y = src.join("y");
    let base_x = repo(
        &x,
        &[
            (pods, "func pod_a() {}\n"),
            ("App.swift", "func app() {}\n"),
        ],
    );
    let base_y = repo(
        &y,
        &[
            ("Podfile", "pod 'Lib'\n"),
            (pods, "func pod_a() {}\n"),
            ("App.swift", "func app() {}\n"),
        ],
    );
    // Each gets a topic branch that adds or removes the Podfile.
    git(&x, &["checkout", "-q", "-b", "topic"]);
    write(&x, "Podfile", "pod 'Lib'\n");
    let topic_x = commit(&x, "add a Podfile");
    git(&x, &["checkout", "-q", "main"]);
    git(&y, &["checkout", "-q", "-b", "topic"]);
    git(&y, &["rm", "-q", "Podfile"]);
    let topic_y = commit(&y, "remove the Podfile");
    git(&y, &["checkout", "-q", "main"]);
    fx.base_directory(&src.0);
    fx.scan();
    let (rx, ry) = (fx.repository_at(&x), fx.repository_at(&y));
    for r in [&rx, &ry] {
        run_of(decision(fx.index(r, false, "1")));
    }
    fx.drain();
    let has_pod = |view: &views::ViewLayers| has_node(&fx.nodes(view, pods), "Function", pod_fn);
    assert!(
        has_pod(&fx.published(&rx)),
        "X indexes Pods/ without a Podfile"
    );
    assert!(fx.unit(&fx.published(&ry), pods).is_none(), "Y excludes it");
    let _ = (base_x, base_y);

    // Branch layer: the topic commit adds (X) or removes (Y) the Podfile; Pods/ itself is
    // unchanged between the baseline and the topic.
    for r in [&rx, &ry] {
        run_of(decision(refresh_ref(&fx, r, "topic")));
    }
    assert_eq!(fx.drain(), 2);
    let ref_view = |r: &Id, commit: &str| {
        let o = overlay_at(&fx, r, commit);
        assert_eq!(o.state, OVERLAY_READY, "{o:?}");
        exact_view(&fx, o.view_id.as_ref().unwrap())
    };
    let vx = ref_view(&rx, &topic_x);
    assert!(fx.unit(&vx, pods).is_none(), "an added Podfile masks Pods/");
    assert!(has_node(
        &fx.nodes(&vx, "App.swift"),
        "Function",
        "#function:app()"
    ));
    let vy = ref_view(&ry, &topic_y);
    assert!(
        has_pod(&vy),
        "a removed Podfile makes Pods/ eligible: staged"
    );

    // Worktree layer: the same changes made locally in the primary checkouts (at main).
    write(&x, "Podfile", "pod 'Lib'\n");
    std::fs::remove_file(y.join("Podfile")).unwrap();
    let worktree_view = |r: &Id, dir: &Path| {
        let w = worktree_id(r, &fx.identity(dir));
        run_of(decision(fx.index_command(
            "indexing.refresh_overlay",
            json!({"kind": "RefreshOverlay", "repositoryId": r, "worktreeId": w,
                "expectedSourceRevision": "1"}),
        )));
        assert_eq!(fx.drain(), 1);
        let (r2, w2) = (r.clone(), w.clone());
        let o = fx
            .read(move |s| s.indexing().overlay_for(&r2, &w2))
            .expect("the overlay");
        assert_eq!(o.state, OVERLAY_READY, "{o:?}");
        exact_view(&fx, o.view_id.as_ref().unwrap())
    };
    let wx = worktree_view(&rx, &x);
    assert!(fx.unit(&wx, pods).is_none(), "a local Podfile masks Pods/");
    let wy = worktree_view(&ry, &y);
    assert!(
        has_pod(&wy),
        "a locally deleted Podfile makes Pods/ eligible"
    );
}

#[test]
fn inherited_call_bindings_follow_the_composed_overlay_candidate_set() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("overlay-candidates");
    let dir = src.join("repo");
    repo(
        &dir,
        &[
            ("a.rs", "fn caller() { ping(); }\n"),
            ("b.rs", "fn ping() {}\n"),
        ],
    );
    git(&dir, &["checkout", "-q", "-b", "topic"]);
    write(&dir, "c.rs", "fn ping() {}\n");
    let topic = commit(&dir, "add another candidate");
    git(&dir, &["checkout", "-q", "main"]);
    fx.base_directory(&src.0);
    fx.scan();
    let r = fx.repository_at(&dir);
    run_of(decision(fx.index(&r, false, "1")));
    fx.drain();
    let baseline = fx.published(&r);
    let call = |view: &views::ViewLayers| {
        let v = view.clone();
        fx.read(move |s| views::unit_edges(s.indexing(), &v, "a.rs"))
            .into_iter()
            .find(|(e, _)| e.kind == "CALLS" && e.payload["targetName"] == json!("ping"))
            .unwrap()
    };
    assert_eq!(call(&baseline).0.payload["targetUnit"], json!("b.rs"));
    run_of(decision(refresh_ref(&fx, &r, "topic")));
    fx.drain();
    let branch = overlay_at(&fx, &r, &topic);
    let branch_view = exact_view(&fx, branch.view_id.as_ref().unwrap());
    let ambiguous = call(&branch_view);
    assert_eq!(ambiguous.0.payload["resolution"], json!("Unresolved"));
    assert_eq!(ambiguous.0.payload["candidates"], json!(2));
    assert!(!ambiguous.1);
    assert_eq!(
        call(&baseline).0.payload["targetUnit"],
        json!("b.rs"),
        "baseline remains immutable"
    );
    let refresh = || {
        let w = worktree_id(&r, &fx.identity(&dir));
        run_of(decision(fx.index_command("indexing.refresh_overlay", json!({"kind":"RefreshOverlay", "repositoryId":r,"worktreeId":w,"expectedSourceRevision":"1"}))));
        fx.drain();
        let (r, w) = (r.clone(), w.clone());
        let o = fx.read(move |s| s.indexing().overlay_for(&r, &w)).unwrap();
        assert_eq!(o.state, OVERLAY_READY);
        exact_view(&fx, o.view_id.as_ref().unwrap())
    };
    write(&dir, "c.rs", "fn ping() {}\n");
    assert_eq!(
        call(&refresh()).0.payload["resolution"],
        json!("Unresolved")
    );
    std::fs::remove_file(dir.join("b.rs")).unwrap();
    let rebound = call(&refresh());
    assert!(rebound.1);
    assert_eq!(rebound.0.payload["targetUnit"], json!("c.rs"));
}

#[test]
fn python_absolute_and_explicit_relative_imports_choose_their_own_modules() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("python-imports");
    let dir = src.join("repo");
    repo(
        &dir,
        &[
            ("foo.py", "root_value = 1\n"),
            ("bar.py", "shadowed_module = 0\n"),
            ("bar/__init__.py", "package_value = 3\n"),
            ("pkg/__init__.py", ""),
            ("pkg/foo.py", "local_value = 2\n"),
            ("pkg/sub/__init__.py", ""),
            (
                "pkg/consumer.py",
                "import foo\nimport bar\nfrom .foo import local_value\n",
            ),
            ("pkg/sub/consumer.py", "from ..foo import local_value\n"),
        ],
    );
    fx.base_directory(&src.0);
    fx.scan();
    let r = fx.repository_at(&dir);
    run_of(decision(fx.index(&r, false, "1")));
    fx.drain();
    let v = fx.published(&r);
    let imports = |key: &'static str| {
        let v = v.clone();
        fx.read(move |s| views::unit_edges(s.indexing(), &v, key))
            .into_iter()
            .filter(|(e, _)| e.kind == "IMPORTS")
            .map(|(e, resolves)| {
                assert!(resolves, "{e:?}");
                (
                    e.payload["targetName"].as_str().unwrap().to_owned(),
                    e.payload["targetUnit"].as_str().unwrap().to_owned(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let direct = imports("pkg/consumer.py");
    assert_eq!(direct.get("foo").map(String::as_str), Some("foo.py"));
    assert_eq!(
        direct.get("bar").map(String::as_str),
        Some("bar/__init__.py")
    );
    assert_eq!(direct.get(".foo").map(String::as_str), Some("pkg/foo.py"));
    assert_eq!(
        imports("pkg/sub/consumer.py")
            .get("..foo")
            .map(String::as_str),
        Some("pkg/foo.py")
    );
}

#[test]
fn review_queued_index_can_be_promoted_to_a_rebuild_without_replacing_its_identity() {
    let _s = serial();
    let fx = Fx::new(|_| {});
    let src = Temp::new("queued-rebuild");
    let dir = src.join("repo");
    repo(&dir, &[("lib.rs", "pub fn indexed() {}\n")]);
    fx.base_directory(&src.0);
    fx.scan();
    let repo_id = fx.repository_at(&dir);
    let queued = run_of(decision(fx.index(&repo_id, false, "1")));
    let before = fx.run(&queued.id);
    let promoted = run_of(decision(fx.index(&repo_id, true, "1")));
    assert_eq!(promoted.id, queued.id);
    let after = fx.run(&queued.id);
    assert_eq!(after.kind, RUN_REINDEX);
    assert_eq!(after.state, RUN_QUEUED);
    assert_eq!(after.batch_id, before.batch_id);
    assert_eq!(after.revision, before.revision + 1);
    fx.drain();
    assert_eq!(fx.run(&queued.id).state, RUN_SUCCEEDED);
    assert!(has_node(
        &fx.nodes(&fx.published(&repo_id), "lib.rs"),
        "Function",
        "#function:indexed()"
    ));
}

#[test]
fn review_overlay_records_its_build_configuration_when_the_queued_request_is_older() {
    let _s = serial();
    for is_ref in [false, true] {
        let fx = Fx::new(|_| {});
        let src = Temp::new("queued-config");
        let dir = src.join("repo");
        let head = repo(
            &dir,
            &[
                ("lib.rs", "pub fn kept() {}\n"),
                ("skip.rs", "pub fn excluded() {}\n"),
            ],
        );
        fx.base_directory(&src.0);
        fx.scan();
        let repo_id = fx.repository_at(&dir);
        run_of(decision(fx.index(&repo_id, false, "1")));
        fx.drain();
        let worktree = worktree_id(&repo_id, &fx.identity(&dir));
        let mut command = json!({"kind": "RefreshOverlay", "repositoryId": repo_id, "expectedSourceRevision": "1"});
        command[if is_ref { "ref" } else { "worktreeId" }] =
            if is_ref { json!(head) } else { json!(worktree) };
        let run = run_of(decision(fx.command_frame(&fx.user(), "indexing.refresh_overlay", json!({"protocol": "1.0-draft.1", "schemaRevision": 2, "requestKey": "queue-overlay", "command": command}))));
        let requested_digest = fx.run(&run.id).config_digest;
        let configure = |expected: &str, excludes: Value| {
            let d = decision(fx.command("indexing.configure_sources", json!({"scope": {"kind": "Workspace"}, "expectedRevision": expected, "config": {"structuralLanguages": [], "semanticPackageIds": [], "excludePatterns": excludes, "maxFileBytes": "524288"}})));
            assert!(matches!(d, DomainDecision::Applied { .. }), "{d:?}");
        };
        configure("1", json!(["skip.rs"]));
        fx.drain();
        let stored = fx.run(&run.id);
        assert_eq!(stored.state, RUN_SUCCEEDED, "{stored:?}");
        let overlay_id = stored.overlay_id.unwrap();
        let overlay = fx.read(move |s| s.indexing().overlay(&overlay_id)).unwrap();
        let status = fx.status(&repo_id);
        assert!(
            status
                .overlays
                .as_ref()
                .unwrap()
                .0
                .iter()
                .any(|o| o.overlay_id == overlay.id && matches!(o.state, OverlayState::Ready)),
            "{status:?}"
        );
        let view = exact_view(&fx, overlay.view_id.as_ref().unwrap());
        assert!(fx.unit(&view, "skip.rs").is_none());
        assert!(has_node(
            &fx.nodes(&view, "lib.rs"),
            "Function",
            "#function:kept()"
        ));
        assert_eq!(
            fx.run(&run.id).config_digest,
            requested_digest,
            "retain request provenance"
        );
        configure("2", json!([]));
        let status = fx.status(&repo_id);
        assert!(
            status
                .overlays
                .as_ref()
                .unwrap()
                .0
                .iter()
                .any(|o| o.overlay_id == overlay.id && matches!(o.state, OverlayState::Stale)),
            "{status:?}"
        );
    }
}
