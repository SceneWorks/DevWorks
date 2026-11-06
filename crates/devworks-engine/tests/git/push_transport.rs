//! Native client boundary only. The bare remote is a fixture-controlled server.
use super::*;
use std::path::PathBuf;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
pub(super) struct State {
    home: PathBuf,
    https: Option<TlsServer>,
    credential_key: Mutex<Option<String>>,
    source: Mutex<Option<PathBuf>>,
    armed: AtomicBool,
    injected: AtomicBool,
    sent: std::sync::atomic::AtomicUsize,
    journaled_reads: std::sync::atomic::AtomicUsize,
    block_observation: AtomicBool,
    pub marker: PathBuf,
}
impl State {
    pub fn new(root: &Path, https: bool) -> Self {
        let home = root.join("transport-home");
        std::fs::create_dir(&home).unwrap();
        let helper = home.join("trusted-ssh");
        std::fs::write(
            &helper,
            "#!/bin/sh\nfor argument do last=\"$argument\"; done\nexec /bin/sh -c \"$last\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let credentials = home.join("trusted-credentials");
        std::fs::write(
            &credentials,
            "#!/bin/sh\nprintf 'username=fixture\npassword=fixture-auth-secret\n'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::write(home.join(".gitconfig"),format!("[core]\n sshCommand = {}\n[ssh]\n variant = ssh\n[http]\n sslVerify = false\n extraHeader = X-Fixture-Secret: fixture-auth-secret\n[credential]\n helper = {}\n",helper.display(),credentials.display())).unwrap();
        let https = https.then(|| TlsServer::start(root, &home));
        Self {
            home,
            https,
            credential_key: Mutex::new(None),
            source: Mutex::new(None),
            armed: AtomicBool::new(false),
            injected: AtomicBool::new(false),
            sent: std::sync::atomic::AtomicUsize::new(0),
            journaled_reads: std::sync::atomic::AtomicUsize::new(0),
            block_observation: AtomicBool::new(true),
            marker: root.join("unreviewed-transport-marker"),
        }
    }
    pub fn https_url(&self) -> Option<String> {
        self.https
            .as_ref()
            .map(|server| format!("https://127.0.0.1:{}/remote.git", server.port))
    }
    pub fn environment(&self, environment: &mut Vec<(String, String)>) {
        environment
            .retain(|(key, _)| !matches!(key.as_str(), "HOME" | "USERPROFILE" | "XDG_CONFIG_HOME"));
        environment.push(("NO_PROXY".into(), "127.0.0.1,localhost".into()));
        for key in ["HOME", "USERPROFILE", "XDG_CONFIG_HOME"] {
            environment.push((key.into(), self.home.to_string_lossy().into()));
        }
    }
    pub fn assert_recorded(
        &self,
        spec: &devworks_core::ports::process::ProcessSpec,
        recorded: bool,
    ) {
        if self.armed.load(Ordering::SeqCst)
            && spec.args.first().is_some_and(|arg| arg == "ls-remote")
            && !spec.args.iter().any(|arg| arg == "--get-url")
        {
            assert!(
                recorded,
                "network observations retain the originating operation owner"
            );
            self.journaled_reads.fetch_add(1, Ordering::SeqCst);
        }
    }
    pub fn refuse_postcondition(&self, spec: &devworks_core::ports::process::ProcessSpec) -> bool {
        if self.armed.load(Ordering::SeqCst) && spec.args.first().is_some_and(|arg| arg == "push") {
            self.sent.fetch_add(1, Ordering::SeqCst);
        }
        self.sent.load(Ordering::SeqCst) > 0
            && self.block_observation.load(Ordering::SeqCst)
            && spec.args.first().is_some_and(|arg| arg == "ls-remote")
            && spec.args.iter().any(|arg| arg == "--refs")
    }
    pub fn before_spawn(&self, spec: &devworks_core::ports::process::ProcessSpec) {
        if spec.args.first().is_some_and(|arg| arg == "ls-remote")
            && !spec.args.iter().any(|arg| arg == "--get-url")
            && self.armed.load(Ordering::SeqCst)
            && !self.injected.swap(true, Ordering::SeqCst)
        {
            let source = self.source.lock().unwrap().clone().unwrap();
            let private_config =
                std::fs::read_to_string(spec.cwd.as_deref().unwrap().join("config")).unwrap();
            assert!(
                private_config.contains("fixture-auth-secret"),
                "raw trusted auth material lives only in the private snapshot"
            );
            assert_ne!(
                spec.cwd.as_deref(),
                Some(source.as_path()),
                "network uses private cwd"
            );
            assert!(
                spec.env
                    .iter()
                    .any(|(key, value)| key == "GIT_DIR"
                        && value.contains("devworks-git-transport-"))
            );
            let script = format!("printf unsafe > '{}'", self.marker.display());
            git(&source, &["config", "core.sshCommand", &script]);
            let credential_key = if let Some(server) = &self.https {
                format!("credential.https://127.0.0.1:{}.helper", server.port)
            } else {
                "credential.https://fixture/.helper".into()
            };
            git(&source, &["config", "credential.helper", ""]);
            git(&source, &["config", &credential_key, &format!("!{script}")]);
            *self.credential_key.lock().unwrap() = Some(credential_key);
            git(&source, &["config", "hook.injected.event", "pre-push"]);
            git(&source, &["config", "hook.injected.command", &script]);
            git(
                &source,
                &[
                    "config",
                    "url.ssh://git@attacker/.insteadOf",
                    "ssh://git@fixture",
                ],
            );
        }
    }
}
pub(super) struct Fixture<'a> {
    pub h: &'a Harness,
    pub store: Arc<dyn ApplicationStore>,
    pub service: Arc<GitService>,
    pub gateway: &'a Gateway<'a>,
    pub agent: &'a devworks_core::context::InvocationContext,
    pub tree: &'a StoredGitWorktree,
    pub remote: &'a Path,
    pub state: &'a State,
}
pub(super) fn exercise(f: Fixture<'_>) {
    let path = Path::new(&f.tree.path);
    // Bind the actual registered endpoint, not a rewritten alias or caller-selected URL.
    let url = git(path, &["remote", "get-url", "--push", "origin"]);
    let local = Path::new(&url).is_absolute();
    let server_marker = f.remote.join("approved-server-hook-marker");
    if local {
        let hook = f.remote.join("hooks/pre-receive");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf approved > '{}'\n",
                server_marker.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let unreviewed = serde_json::json!({"protocol":"1.0-draft.1","requestKey":"unreviewed-local","command":{"kind":"Push","worktreeId":f.tree.id,"remoteId":"origin","targetRef":f.tree.branch,"expectedHead":f.tree.head,"mode":"Normal"}});
        let refusal = f
            .gateway
            .command(
                f.agent,
                "git.push",
                unreviewed.to_string().as_bytes(),
                f.service.clone(),
            )
            .unwrap();
        assert!(
            serde_json::to_string(&refusal)
                .unwrap()
                .contains("ApprovalRequired"),
            "{refusal:?}"
        );
        assert!(!server_marker.exists());
        let host_actor = f.store.info().host_actor;
        let prior_owner = read(&*f.store, |s| s.git().git_worktree(&f.tree.id))
            .unwrap()
            .owner;
        let tree_id = f.tree.id.clone();
        let owner = host_actor.clone();
        write(&*f.store, move |tx| {
            let mut tree = tx.git().git_worktree(&tree_id)?.unwrap();
            let revision = tree.revision;
            tree.owner = owner;
            tree.revision += 1;
            tree.ownership_revision += 1;
            tx.git().update_git_worktree(&tree, revision)
        })
        .unwrap();
        let host = HostIngress::new(&*f.store)
            .context(
                host_actor,
                PrincipalKind::Host,
                id("unattended-host"),
                id("unattended-host"),
                Counter::new(f.store.info().restore_epoch).unwrap(),
                Revision::new(1).unwrap(),
            )
            .with_lineage(
                f.agent.project_id().cloned(),
                f.agent.root_id().cloned(),
                None,
            );
        let mut host_request = unreviewed;
        host_request["requestKey"] = serde_json::json!("unattended-host-local-push");
        let refusal = f
            .gateway
            .command(
                &host,
                "git.push",
                host_request.to_string().as_bytes(),
                f.service.clone(),
            )
            .unwrap();
        assert!(
            serde_json::to_string(&refusal)
                .unwrap()
                .contains("ApprovalRequired"),
            "Host is not human consent: {refusal:?}"
        );
        assert!(!server_marker.exists());
        let tree_id = f.tree.id.clone();
        write(&*f.store, move |tx| {
            let mut tree = tx.git().git_worktree(&tree_id)?.unwrap();
            let revision = tree.revision;
            tree.owner = prior_owner;
            tree.revision += 1;
            tree.ownership_revision += 1;
            tx.git().update_git_worktree(&tree, revision)
        })
        .unwrap();
    }
    git(path, &["remote", "set-url", "origin", &url]);
    std::fs::write(path.join("a.txt"), "new exact reviewed transport commit\n").unwrap();
    git(path, &["commit", "-am", "reviewed transport commit"]);
    let current = git(path, &["rev-parse", "HEAD"]);
    let expected = git(f.remote, &["rev-parse", &f.tree.branch]);
    let request = serde_json::json!({"protocol":"1.0-draft.1","requestKey":"isolated-approved-push","command":{"kind":"Push","worktreeId":f.tree.id,"remoteId":"origin","targetRef":f.tree.branch,"expectedHead":{"format":"Sha1","hex":current},"expectedRemote":{"format":"Sha1","hex":expected},"mode":"Normal"}});
    if !local {
        let attack = format!("printf unsafe > '{}'", f.state.marker.display());
        git(path, &["config", "core.sshCommand", &attack]);
        let refusal = f
            .gateway
            .command(
                f.agent,
                "git.push",
                request.to_string().as_bytes(),
                f.service.clone(),
            )
            .unwrap();
        assert!(
            serde_json::to_string(&refusal)
                .unwrap()
                .contains("git_repository_transport_program"),
            "{refusal:?}"
        );
        assert!(
            !f.state.marker.exists(),
            "pre-consent local qualification executes no transport"
        );
        git(path, &["config", "--unset-all", "core.sshCommand"]);
    }
    let mut request = request;
    request["requestKey"] = serde_json::json!("approved-isolated-push-after-refusal");
    let operation = approvals::approve_owned_force(
        f.h,
        f.store.clone(),
        f.service.clone(),
        f.gateway,
        f.agent,
        &request,
    );
    let record = read(&*f.store, |s| s.operations().operation(&operation)).unwrap();
    assert!(!record.payload.to_string().contains("fixture-auth-secret"));
    assert_eq!(
        record.payload["reviewed_binding"]["native"]["destinationServer"]["authority"],
        "TrustedByThisExactPushApproval"
    );
    *f.state.source.lock().unwrap() = Some(path.into());
    f.state.armed.store(true, Ordering::SeqCst);
    let outcome = f.service.execute(&record);
    // Injection is after the configuration snapshot: it cannot affect any network helper.
    assert!(f.state.injected.load(Ordering::SeqCst));
    assert!(!f.state.marker.exists());
    assert!(
        matches!(
            outcome,
            devworks_core::services::git::EffectOutcome::Unknown(_)
        ),
        "{outcome:?}"
    );
    assert_eq!(git(f.remote, &["rev-parse", &f.tree.branch]), current);
    assert_eq!(
        server_marker.exists(),
        local,
        "only an approved local server executes its fixture hook"
    );
    // Recovery refuses a changed/untrusted client profile rather than executing it or replaying.
    let observed = f.service.observe(&record);
    assert!(
        matches!(
            observed,
            devworks_core::services::git::EffectOutcome::Unknown(_)
        ),
        "{observed:?}"
    );
    assert!(!f.state.marker.exists());
    // Positive control uses the same attacker-controlled repository configuration unconfined.
    git(
        path,
        &["config", "--unset-all", "url.ssh://git@attacker/.insteadOf"],
    );
    let mut command = Command::new("git");
    command.current_dir(path);
    command
        .env("HOME", &f.state.home)
        .env("XDG_CONFIG_HOME", &f.state.home)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("NO_PROXY", "127.0.0.1");
    if local {
        command.args(["hook", "run", "pre-push"]);
    } else {
        command.args(["ls-remote", "--", &url]);
    }
    let output = command.output().unwrap();
    if !local {
        assert!(!output.status.success());
    }
    assert!(
        f.state.marker.exists(),
        "unconfined repository client helper executes marker"
    );
    for key in [
        "core.sshCommand",
        "credential.helper",
        "hook.injected.event",
        "hook.injected.command",
    ] {
        git(path, &["config", "--unset-all", key]);
    }
    git(
        path,
        &[
            "config",
            "--unset-all",
            f.state.credential_key.lock().unwrap().as_ref().unwrap(),
        ],
    );
    std::fs::remove_file(&f.state.marker).unwrap();
    f.state.armed.store(false, Ordering::SeqCst);
    f.state.block_observation.store(false, Ordering::SeqCst);
    let recovered = f.service.observe(&record);
    assert!(
        matches!(
            recovered,
            devworks_core::services::git::EffectOutcome::Completed(_)
        ),
        "{recovered:?}"
    );
    assert!(f.state.journaled_reads.load(Ordering::SeqCst) > 0);
    assert_eq!(
        f.state.sent.load(Ordering::SeqCst),
        1,
        "recovery observes and never repeats push"
    );
    assert!(!f.state.marker.exists());
}

struct TlsServer {
    child: Mutex<std::process::Child>,
    port: u16,
}
impl TlsServer {
    fn start(root: &Path, home: &Path) -> Self {
        use std::io::BufRead;
        let certificate = home.join("fixture.crt");
        let key = home.join("fixture.key");
        let output = Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=localhost",
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&certificate)
            .output()
            .unwrap();
        assert!(output.status.success());
        let script = home.join("https_server.py");
        std::fs::write(&script, include_str!("https_server.py")).unwrap();
        let mut child = Command::new("python3")
            .arg(script)
            .arg(root)
            .arg(certificate)
            .arg(key)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut port = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut port)
            .unwrap();
        Self {
            child: Mutex::new(child),
            port: port.trim().parse().unwrap(),
        }
    }
}
impl Drop for TlsServer {
    fn drop(&mut self) {
        let child = self.child.get_mut().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
}
