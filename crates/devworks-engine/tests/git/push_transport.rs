//! Native client boundary only. The bare remote is a fixture-controlled server.
use super::*;
use devworks_core::ports::process::ProcessHost as _;
use std::path::PathBuf;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
fn null_config() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}
fn shell_word(path: &Path) -> String {
    let path =
        devworks_core::services::indexing::git::path_argument(path).expect("fixture helper path");
    format!("'{}'", path.replace('\'', "'\\''"))
}
fn config_value(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('\"', "\\\"")
            .replace('\n', "\\n")
            .replace('\t', "\\t")
    )
}
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
    failures: Mutex<Vec<String>>,
    credential_command: String,
    credential_calls: PathBuf,
    ssh_command: String,
    ssh_calls: PathBuf,
    pub marker: PathBuf,
}
impl State {
    pub fn new(root: &Path, https: bool) -> Self {
        let home = root.join("transport-home");
        std::fs::create_dir(&home).unwrap();
        let helper = home.join("trusted-ssh");
        let ssh_calls = home.join("fake-ssh-calls");
        std::fs::write(&helper,format!("#!/bin/sh\nprintf 'ssh\\n' >> {}\nfor argument do last=\"$argument\"; done\nunset GIT_DIR GIT_COMMON_DIR GIT_WORK_TREE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES\nexport GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_SYSTEM={} GIT_CONFIG_GLOBAL={} GIT_TERMINAL_PROMPT=0\nexec /bin/sh -c \"$last\"\n",shell_word(&ssh_calls),null_config(),null_config()))
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let credentials = home.join("trusted-credentials");
        let credential_calls = home.join("fake-credential-calls");
        std::fs::write(
            &credentials,
            format!("#!/bin/sh\ncase \"$1\" in get|store|erase) ;; *) exit 1 ;; esac\n/bin/cat >/dev/null\nprintf '%s\\n' \"$1\" >> {}\nif [ \"$1\" = get ]; then printf 'username=fixture\npassword=fixture-auth-secret\n'; fi\n",shell_word(&credential_calls)),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        // Git prefixes named helpers with `git credential-`; use its explicit shell
        // form so a shell-quoted absolute fixture path remains an executable path.
        let credential_command = format!("!{}", shell_word(&credentials));
        let ssh_command = shell_word(&helper);
        std::fs::write(home.join(".gitconfig"),format!("[core]\n sshCommand = {}\n[ssh]\n variant = ssh\n[http]\n sslVerify = false\n extraHeader = \"\"\n extraHeader = X-Fixture-Secret: fixture-auth-secret\n[credential]\n helper = \"\"\n helper = {}\n[commit]\n gpgSign = false\n[tag]\n gpgSign = false\n[push]\n gpgSign = false\n",config_value(&ssh_command),config_value(&credential_command))).unwrap();
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
            failures: Mutex::new(Vec::new()),
            credential_command,
            credential_calls,
            ssh_command,
            ssh_calls,
            marker: root.join("unreviewed-transport-marker"),
        }
    }
    /// Git strips caller GIT_* settings before constructing its own process spec. The fake
    /// native host therefore fences metadata/config reads here; isolated network specs must
    /// already carry the production fence, which is asserted rather than repaired.
    pub fn auth_spec(
        &self,
        spec: &devworks_core::ports::process::ProcessSpec,
    ) -> devworks_core::ports::process::ProcessSpec {
        let mut out = spec.clone();
        assert!(
            out.clear_env,
            "fixture Git must not inherit the user's process environment"
        );
        if out.env.iter().any(|(key, _)| key == "GIT_DIR") {
            for (key, value) in [
                ("GIT_CONFIG_NOSYSTEM", "1"),
                ("GIT_CONFIG_SYSTEM", null_config()),
                ("GIT_CONFIG_GLOBAL", null_config()),
                ("GIT_TERMINAL_PROMPT", "0"),
            ] {
                assert!(
                    out.env.iter().any(|(k, v)| k == key && v == value),
                    "private client lost its production config fence"
                );
            }
        } else {
            out.env.retain(|(key, _)| !key.starts_with("GIT_CONFIG_"));
            out.env.extend([
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
                ("GIT_CONFIG_SYSTEM".into(), null_config().into()),
                (
                    "GIT_CONFIG_GLOBAL".into(),
                    self.home.join(".gitconfig").to_string_lossy().into_owned(),
                ),
            ]);
        }
        assert!(
            !out.env.iter().any(|(key, _)| key.starts_with("SSH_")
                || key.starts_with("GPG_")
                || matches!(
                    key.as_str(),
                    "GIT_ASKPASS" | "GIT_SSH" | "GIT_SSH_COMMAND" | "GNUPGHOME" | "GPG_TTY"
                )),
            "fixture may use only its fixed fake auth helpers"
        );
        out
    }
    fn config_query(
        &self,
        program: &str,
        environment: &[(String, String)],
        cwd: &Path,
        pattern: &str,
    ) -> Vec<(String, String)> {
        let result = Command::new(program)
            .args(["config", "--includes", "--null", "--get-regexp", pattern])
            .current_dir(cwd)
            .env_clear()
            .envs(environment.iter().cloned())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "read-only effective fixture auth inspection failed"
        );
        result
            .stdout
            .split(|byte| *byte == 0)
            .filter(|row| !row.is_empty())
            .map(|row| {
                let row = std::str::from_utf8(row).expect("fixture config UTF-8");
                let (key, value) = row.split_once('\n').expect("fixture config record");
                (key.to_owned(), value.to_owned())
            })
            .collect()
    }
    pub fn assert_fake_auth(&self, spec: &devworks_core::ports::process::ProcessSpec) {
        let rows = self.config_query(
            &spec.program,
            &spec.env,
            spec.cwd.as_deref().unwrap(),
            "^(credential\\..*|http\\..*|core\\.sshcommand|core\\.askpass|push\\.gpgsign)$",
        );
        let helpers = rows
            .iter()
            .filter(|(key, _)| key == "credential.helper")
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>();
        assert!(
            helpers == ["", self.credential_command.as_str()],
            "effective network fixture helpers are not exactly the reset and fixed fake helper"
        );
        assert!(
            rows.iter()
                .all(|(key, _)| !key.starts_with("credential.") || key == "credential.helper"),
            "unexpected credential configuration"
        );
        assert!(
            !rows.iter().any(|(key, _)| key == "core.askpass"),
            "fixture must not use an inherited askpass"
        );
        assert!(
            rows.iter()
                .filter(|(key, _)| key.starts_with("http."))
                .all(|(key, value)| matches!(
                    key.as_str(),
                    "http.sslverify" | "http.followredirects"
                ) && value == "false"
                    || key == "http.extraheader"
                        && (value.is_empty() || value == "X-Fixture-Secret: fixture-auth-secret")),
            "fixture HTTP authentication profile is not closed"
        );
        assert!(
            rows.iter()
                .any(|(key, value)| key == "core.sshcommand" && value == &self.ssh_command),
            "fixture must select its fixed fake SSH command"
        );
        assert!(
            rows.iter()
                .any(|(key, value)| key == "push.gpgsign" && value == "false"),
            "fixture push signing must remain disabled"
        );
    }
    pub fn assert_fake_recorded(&self, local: bool) {
        if self.https.is_some() {
            let calls = std::fs::read_to_string(&self.credential_calls)
                .expect("fixed fake helper did not record any use");
            assert!(
                calls
                    .lines()
                    .all(|call| matches!(call, "get" | "store" | "erase"))
            );
            assert!(calls.lines().any(|call| call == "get"));
            assert!(calls.lines().any(|call| call == "store"));
        } else if !local {
            let calls = std::fs::read_to_string(&self.ssh_calls)
                .expect("fixed fake SSH helper did not record use");
            assert!(calls.lines().all(|call| call == "ssh"));
            assert!(!calls.is_empty());
        }
    }
    pub fn control_environment(&self) -> Vec<(String, String)> {
        let mut environment = NativeProcessHost.environment();
        self.environment(&mut environment);
        environment.extend([
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_CONFIG_SYSTEM".into(), null_config().into()),
            (
                "GIT_CONFIG_GLOBAL".into(),
                self.home.join(".gitconfig").to_string_lossy().into_owned(),
            ),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ]);
        environment
    }
    pub fn assert_control_auth(&self, cwd: &Path, environment: &[(String, String)]) {
        let rows = self.config_query(
            "git",
            environment,
            cwd,
            "^(credential\\..*helper|core\\.sshcommand|core\\.askpass|push\\.gpgsign)$",
        );
        let marker = format!("printf unsafe > '{}'", self.marker.display());
        let marker_helper = format!("!{marker}");
        assert!(
            rows.iter()
                .all(|(key, value)| if key.starts_with("credential.") {
                    value.is_empty() || value == &self.credential_command || value == &marker_helper
                } else if key == "core.sshcommand" {
                    value == &self.ssh_command || value == &marker
                } else {
                    key == "push.gpgsign" && value == "false"
                }),
            "unconfined control must still use only known fake authentication/signing paths"
        );
    }
    pub fn note_failure(
        &self,
        verb: &str,
        flags: &str,
        result: &Result<Vec<u8>, devworks_core::services::indexing::helper::HelperFault>,
    ) {
        if self.https.is_some()
            && matches!(verb, "ls-remote" | "push")
            && let Err(error) = result
        {
            self.failures.lock().unwrap().push(
                format!("{verb} {flags}: {error}")
                    .replace("fixture-auth-secret", "[fixture secret]"),
            );
        }
    }
    pub fn diagnostics(&self) -> String {
        let failures = self.failures.lock().unwrap().join("\n");
        let server = self.https.as_ref().map_or_else(String::new, |server| {
            let state = server.child.lock().unwrap().try_wait();
            let log = std::fs::File::open(&server.log)
                .and_then(|file| {
                    let mut text = String::new();
                    file.take(32 * 1024).read_to_string(&mut text)?;
                    Ok(text)
                })
                .unwrap_or_else(|e| format!("server log unavailable: {e}"));
            format!(
                "TLS server {state:?}\n{}",
                log.replace("fixture-auth-secret", "[fixture secret]")
            )
        });
        format!("transport failures:\n{failures}\n{server}")
    }
    pub fn https_url(&self) -> Option<String> {
        self.https
            .as_ref()
            .map(|server| format!("https://127.0.0.1:{}/remote.git", server.port))
    }
    pub fn environment(&self, environment: &mut Vec<(String, String)>) {
        environment.retain(|(key, _)| {
            !key.to_ascii_uppercase().starts_with("GIT_")
                && !key.to_ascii_uppercase().starts_with("SSH_")
                && !key.to_ascii_uppercase().starts_with("GPG_")
                && !matches!(
                    key.as_str(),
                    "HOME" | "USERPROFILE" | "XDG_CONFIG_HOME" | "GNUPGHOME" | "GPG_TTY"
                )
        });
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
        if spec.args.first().is_some_and(|arg| {
            arg == "push" || (arg == "ls-remote" && !spec.args.iter().any(|arg| arg == "--get-url"))
        }) {
            assert!(
                spec.env.iter().any(|(key, _)| key == "GIT_DIR"),
                "application network call escaped its private context"
            );
            self.assert_fake_auth(spec);
        }
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
        "{outcome:?}\n{}",
        f.state.diagnostics()
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
    let environment = f.state.control_environment();
    f.state.assert_control_auth(path, &environment);
    command.env_clear().envs(environment);
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
        "{recovered:?}\n{}",
        f.state.diagnostics()
    );
    assert!(f.state.journaled_reads.load(Ordering::SeqCst) > 0);
    assert_eq!(
        f.state.sent.load(Ordering::SeqCst),
        1,
        "recovery observes and never repeats push"
    );
    assert!(!f.state.marker.exists());
    f.state.assert_fake_recorded(local);
}

struct TlsServer {
    child: Mutex<std::process::Child>,
    port: u16,
    log: PathBuf,
}
impl TlsServer {
    fn start(root: &Path, home: &Path) -> Self {
        use std::io::BufRead;
        let certificate = home.join("fixture.crt");
        let key = home.join("fixture.key");
        let certificate_config = home.join("fixture-openssl.cnf");
        std::fs::write(
            &certificate_config,
            "[req]\ndistinguished_name=fixture_dn\n[fixture_dn]\nCN=localhost\n",
        )
        .unwrap();
        let output = fixture_command("openssl", home)
            .env("OPENSSL_CONF", null_config())
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
            .arg("-config")
            .arg(&certificate_config)
            .arg("-out")
            .arg(&certificate)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture certificate creation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let script = home.join("https_server.py");
        std::fs::write(&script, include_str!("https_server.py")).unwrap();
        let log = home.join("https-server.log");
        let error_log = std::fs::File::create(&log).unwrap();
        let mut child = fixture_command("python3", home)
            .env("OPENSSL_CONF", null_config())
            .arg(script)
            .arg(root)
            .arg(certificate)
            .arg(key)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::from(error_log))
            .spawn()
            .unwrap();
        let mut port = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut port)
            .unwrap();
        let port = port.trim().parse().unwrap();
        Self {
            child: Mutex::new(child),
            port,
            log,
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
