use super::*;
use serde_json::json;

fn id(value: &str) -> Id {
    Id::new(value).expect("test ID")
}

/// The process exited while the caller still held the receiver it had just read the launch
/// report from: the monitor records the outcome rather than queueing it for nobody.
#[test]
fn a_reported_launch_leaves_its_outcome_to_the_monitor_while_the_caller_still_listens() {
    let exit = || EffectOutcome::Completed(json!({"exitCode": 7}));
    let (to_caller, from_monitor) = std::sync::mpsc::channel();
    let report = LaunchReport::new(to_caller);
    report.launched();
    assert!(matches!(from_monitor.recv(), Ok(None)));
    assert!(matches!(
        report.finish(exit()),
        Some(EffectOutcome::Completed(_))
    ));
    assert!(
        from_monitor.try_recv().is_err(),
        "the outcome was queued for the caller"
    );

    // Never launched: the caller receives the outcome as the effect result.
    let (to_caller, from_monitor) = std::sync::mpsc::channel();
    let report = LaunchReport::new(to_caller);
    assert!(report.finish(exit()).is_none());
    assert!(matches!(
        from_monitor.recv(),
        Ok(Some(EffectOutcome::Completed(_)))
    ));

    // Never launched and the caller is gone: the monitor still records it.
    let (to_caller, from_monitor) = std::sync::mpsc::channel();
    drop(from_monitor);
    assert!(LaunchReport::new(to_caller).finish(exit()).is_some());
}

fn spec(profile: &str, credential: bool) -> ProcessSpec {
    let environment = if credential {
        json!([{"name":"TOKEN","credentialRef":"credential-1"}])
    } else {
        json!([{"name":"MODE","value":"test"}])
    };
    serde_json::from_value(json!({
        "executableRef":"test-executable",
        "args":["--version"],
        "workingDirectory":{"rootId":"test-root","relative":"work"},
        "environment":environment,
        "profile":profile,
        "timeoutMs":"1000",
        "outputLimitBytes":"1024"
    }))
    .expect("test process spec")
}

#[test]
fn truncated_secret_prefix_is_withheld_before_output_publication() {
    let secrets = vec!["hunter2".to_owned()];
    assert_eq!(redacted_output(b"hello hunt", &secrets, true), b"hello ");
    for incomplete in [false, true] {
        let output = redacted_output(b"abcdabcd", &["abcdabcd".into()], incomplete);
        assert!(!String::from_utf8_lossy(&output).contains("abcd"));
    }
    let overlapping = redacted_output(b"abcabcabc", &["abcabc".into()], true);
    assert!(!String::from_utf8_lossy(&overlapping).contains("abc"));
    assert_eq!(
        redacted_output(b"hello hunter2!", &secrets, false),
        b"hello [REDACTED]!"
    );
    assert!(redacted_output(b"short=abc", &["abc".to_owned()], false).is_empty());
    assert_eq!(redacted_output(&[0xff], &[], true), vec![0xff]);
    assert_eq!(
        redacted_output(&[0xff, b'x'], &["test".into()], false),
        vec![0xff, b'x']
    );
    assert_eq!(
        redacted_output(b"\xfftest\x00", &["test".into()], false),
        b"\xff[REDACTED]\x00"
    );
}

#[test]
fn committed_process_intent_retains_host_bound_authority_sources() {
    let context = InvocationContext::new(
        crate::protocol::ActorId(id("agent-1")),
        PrincipalKind::Agent,
        id("invocation-1"),
        id("ingress-1"),
        crate::protocol::StoreId(id("store-1")),
        counter(0),
        rev(1),
    )
    .with_authority_source(id("grant-1"));
    let fingerprint = Fingerprint::from_hex(&"a".repeat(64)).expect("fingerprint");
    let job = ExecuteJob::from_context(&context, &fingerprint, spec("Full", false));
    let stored: ExecuteJob = serde_json::from_value(serde_json::to_value(job).unwrap()).unwrap();
    assert_eq!(stored.authority_sources, vec![id("grant-1")]);
}

#[test]
fn registered_ids_and_project_scope_bound_native_launch() {
    let base =
        std::env::temp_dir().join(format!("devworks-process-bindings-{}", std::process::id()));
    let work = base.join("work");
    std::fs::create_dir_all(&work).expect("working root");
    let executable = std::env::current_exe().expect("test binary");
    let executable = executable.canonicalize().expect("canonical test binary");
    let project = ProjectId(id("project-1"));
    let bindings = ToolBindings::new(
        [RegisteredExecutable {
            id: id("test-executable"),
            path: executable.clone(),
        }],
        [RegisteredWorkingRoot {
            id: id("test-root"),
            project: Some(project.clone()),
            path: base.canonicalize().expect("canonical test root"),
        }],
    )
    .expect("registrations");
    let resolved = bindings
        .resolve(&spec("Full", false), Some(&project))
        .expect("authorized project root");
    // The canonical file launches; on Windows without the verbatim prefix.
    assert_eq!(
        resolved.program,
        launch_path(executable.clone()).to_string_lossy()
    );
    assert_eq!(
        resolved.arg0.as_deref(),
        Some(&*executable.to_string_lossy())
    );
    assert_eq!(
        resolved.cwd,
        Some(work.canonicalize().expect("canonical work"))
    );
    assert_eq!(resolved.env, vec![("MODE".into(), "test".into())]);
    assert!(resolved.capture_output);
    assert_eq!(
        bindings.resolve(&spec("Full", false), Some(&ProjectId(id("project-2")))),
        Err(BindingRefusal::OutOfProject)
    );
    assert_eq!(
        bindings.resolve(&spec("Project", false), Some(&project)),
        Err(BindingRefusal::UnsupportedProfile)
    );
    assert_eq!(
        bindings.resolve(&spec("Confined", false), Some(&project)),
        Err(BindingRefusal::UnsupportedProfile)
    );
    assert_eq!(
        bindings.resolve(&spec("Full", true), Some(&project)),
        Err(BindingRefusal::UnknownCredential)
    );
    std::fs::remove_dir_all(base).expect("remove test root");
}

#[test]
fn a_watcher_tail_keeps_the_last_bytes_and_withholds_a_cut_credential_suffix() {
    let mut tail = TailOutput::new(8);
    tail.append(b"0123");
    assert!(!tail.truncated());
    tail.append(b"456789ab");
    tail.append(b"c");
    assert_eq!(tail.retained(), b"56789abc");
    assert!(tail.truncated());
    let mut big = TailOutput::new(4);
    big.append(b"abcdefghij");
    assert_eq!(big.retained(), b"ghij");
    // A complete credential is redacted; one cut at the front of a truncated tail is
    // withheld rather than shown as a suffix.
    let secret = vec!["s3cret-token".to_owned()];
    let whole = redacted_tail(b"key=s3cret-token done", &secret, false, false);
    assert!(!String::from_utf8_lossy(&whole).contains("s3cret"));
    let cut = redacted_tail(b"et-token done", &secret, true, false);
    assert_eq!(cut, b" done");
    let kept = redacted_tail(b"et-token done", &secret, false, false);
    assert_eq!(kept, b"et-token done");
    // A running process may not have written a credential's remainder yet.
    let live = redacted_tail(b"key=s3cret", &secret, false, true);
    assert_eq!(live, b"key=");
}

#[test]
fn combined_output_drains_without_retaining_the_discarded_tail() {
    let mut output = BoundedOutput::new(5);
    output.append(b"ab");
    output.append(b"cdefgh");
    assert_eq!(output.retained(), b"abcde");
    assert!(output.truncated());
    assert_eq!(output.page(2, 2), (2, b"cd".as_slice(), 4));
    assert_eq!(output.page(5, 2), (5, b"".as_slice(), 5));
    assert_eq!(output.page(u64::MAX, usize::MAX), (5, b"".as_slice(), 5));
    let mut later = BoundedOutput::from_offset(3, 4);
    later.append(b"abc");
    later.append(b"defghij");
    assert_eq!(later.retained(), b"efg");
    assert_eq!(later.seen(), 10);
    assert!(later.truncated());
}

/// Each supported manager's install runs with lifecycle scripts disabled unless the request
/// allows them; the allowed form is the manager's ordinary install.
#[test]
fn each_manager_installs_without_lifecycle_scripts_unless_allowed() {
    let root = Path::new("/repo");
    let home = Path::new("/host/home");
    let at = |relative: &str| root.join(relative).to_string_lossy().into_owned();
    let store = home.join("pnpm-store").to_string_lossy().into_owned();
    let cache = format!(
        "--config.cache-dir={}",
        home.join(".cache/pnpm").to_string_lossy()
    );
    let names = |args: &[&str]| args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    let requirements = b"requests==2.32.3 --hash=sha256:abc\n".as_slice();
    let host = HostPython {
        interpreter: "/host/python3".into(),
        environment: "/host/environments/uv/repo".into(),
    };
    // (manifest, without scripts, with scripts, extra environment without scripts)
    type Case<'a> = (&'a str, Vec<String>, Vec<String>, Vec<(&'a str, &'a str)>);
    let cases: Vec<Case> = vec![
        (
            "Cargo.toml",
            names(&["fetch", "--manifest-path", &at("Cargo.toml")]),
            names(&["build", "--manifest-path", &at("Cargo.toml")]),
            vec![],
        ),
        (
            "Cargo.lock",
            names(&["fetch", "--locked", "--manifest-path", &at("Cargo.toml")]),
            names(&["build", "--locked", "--manifest-path", &at("Cargo.toml")]),
            vec![],
        ),
        (
            "package-lock.json",
            names(&["ci", "--ignore-scripts", "--prefix", "/repo"]),
            names(&["ci", "--prefix", "/repo"]),
            vec![],
        ),
        (
            "package.json",
            names(&["install", "--ignore-scripts", "--prefix", "/repo"]),
            names(&["install", "--prefix", "/repo"]),
            vec![],
        ),
        (
            "pnpm-lock.yaml",
            names(&[
                "install",
                "--frozen-lockfile",
                "--ignore-scripts",
                "--ignore-pnpmfile",
                "--config.manage-package-manager-versions=false",
                "--config.pm-on-fail=ignore",
                "--store-dir",
                &store,
                &cache,
                "--dir",
                "/repo",
            ]),
            names(&[
                "install",
                "--frozen-lockfile",
                "--store-dir",
                &store,
                &cache,
                "--dir",
                "/repo",
            ]),
            vec![],
        ),
        (
            "yarn.lock",
            names(&[
                "install",
                "--frozen-lockfile",
                "--ignore-scripts",
                "--cwd",
                "/repo",
            ]),
            names(&["install", "--frozen-lockfile", "--cwd", "/repo"]),
            vec![("YARN_ENABLE_SCRIPTS", "false")],
        ),
        (
            "pyproject.toml",
            names(&[
                "sync",
                "--project",
                "/repo",
                "--no-build",
                "--no-install-workspace",
                "--python",
                "/host/python3",
            ]),
            names(&["sync", "--project", "/repo"]),
            vec![("UV_PROJECT_ENVIRONMENT", "/host/environments/uv/repo")],
        ),
        (
            "uv.lock",
            names(&[
                "sync",
                "--project",
                "/repo",
                "--frozen",
                "--no-build",
                "--no-install-workspace",
                "--python",
                "/host/python3",
            ]),
            names(&["sync", "--project", "/repo", "--frozen"]),
            vec![("UV_PROJECT_ENVIRONMENT", "/host/environments/uv/repo")],
        ),
        (
            "requirements.txt",
            names(&[
                "-I",
                "-m",
                "pip",
                "install",
                "--only-binary",
                ":all:",
                "--target",
                &at(".devworks-deps/python"),
                "-r",
                &at("requirements.txt"),
            ]),
            names(&[
                "-I",
                "-m",
                "pip",
                "install",
                "--target",
                &at(".devworks-deps/python"),
                "-r",
                &at("requirements.txt"),
            ]),
            vec![],
        ),
        (
            "go.mod",
            names(&["mod", "download"]),
            names(&["mod", "download"]),
            vec![],
        ),
        (
            "composer.lock",
            names(&[
                "install",
                "--no-scripts",
                "--no-plugins",
                "--working-dir",
                "/repo",
            ]),
            names(&["install", "--working-dir", "/repo"]),
            vec![],
        ),
    ];
    for (name, without, with, extra) in cases {
        let off = install_command(name, root, home, requirements, false, Some(&host)).expect(name);
        let on = install_command(name, root, home, requirements, true, None).expect(name);
        assert_eq!(off.program, on.program, "{name}");
        assert_eq!(off.args, without, "{name} without scripts");
        assert_eq!(on.args, with, "{name} with scripts");
        for (key, value) in &extra {
            assert!(
                off.env.contains(&((*key).to_owned(), (*value).to_owned())),
                "{name}: {:?}",
                off.env
            );
        }
        assert!(
            !on.env
                .iter()
                .any(|(key, _)| key == "YARN_ENABLE_SCRIPTS" || key == "UV_PROJECT_ENVIRONMENT"),
            "{name}"
        );
    }
    // Without a host interpreter and environment, a script-free uv install is refused.
    for name in ["pyproject.toml", "uv.lock"] {
        assert_eq!(
            install_command(name, root, home, b"", false, None),
            Err(BindingRefusal::ScriptsRequired),
            "{name}"
        );
    }
    // Bundler has no switch that keeps the Gemfile and native extensions from running.
    assert_eq!(
        install_command("Gemfile.lock", root, home, b"", false, None),
        Err(BindingRefusal::ScriptsRequired)
    );
    let bundle = install_command("Gemfile.lock", root, home, b"", true, None).expect("bundler");
    assert_eq!(
        bundle.args,
        names(&["install", "--gemfile", &at("Gemfile")])
    );
    // A requirements file that could build from source needs allowScripts.
    assert_eq!(
        install_command("requirements.txt", root, home, b"-e .\n", false, None),
        Err(BindingRefusal::ScriptsRequired)
    );
    assert!(install_command("requirements.txt", root, home, b"-e .\n", true, None).is_ok());
    // A Yarn 2+ lockfile selects Yarn 2+'s own switch; it has no `--ignore-scripts`.
    let berry = b"__metadata:\n  version: 8\n".as_slice();
    let off = install_command("yarn.lock", root, home, berry, false, None).expect("yarn 2+");
    assert_eq!(
        off.args,
        names(&["install", "--immutable", "--mode=skip-build"])
    );
    assert!(
        off.env
            .contains(&("YARN_ENABLE_SCRIPTS".into(), "false".into()))
    );
    let on = install_command("yarn.lock", root, home, berry, true, None).expect("yarn 2+");
    assert_eq!(on.args, names(&["install", "--immutable"]));
    assert!(on.env.is_empty());
}

#[test]
fn only_a_requirements_file_of_index_packages_installs_without_scripts() {
    for safe in [
        "",
        "# pinned\nrequests==2.32.3\n",
        "requests[socks]>=2,<3 ; python_version >= \"3.9\"  # comment\n",
        "--index-url https://pypi.org/simple\n--prefer-binary\nnumpy\n",
        "-i https://example.invalid/simple\nflask \\\n  ==3.0.0\n",
        "--require-hashes\nidna==3.7 --hash=sha256:aaaa --hash=sha256:bbbb\n",
    ] {
        assert!(binary_only_requirements(safe.as_bytes()), "{safe:?}");
    }
    for unsafe_file in [
        "--no-binary :all:\nrequests\n",
        "--only-binary :none:\nrequests\n",
        "-e .\n",
        "--editable ./pkg\n",
        "-r other.txt\n",
        "-c constraints.txt\n",
        "./local-package\n",
        "/abs/path/pkg\n",
        "pkg @ https://example.invalid/pkg-1.0.tar.gz\n",
        "git+https://example.invalid/repo.git#egg=pkg\n",
        "pkg-1.0.tar.gz\n",
        "pkg.tlz\n",
        "pkg.zip[extra]\n",
        "pkg.tar.gz [extra] ==1.0\n",
        "pkg-1.0.tar.lzma\n",
        "pkg-1.0.TAR.LZ\n",
        "pkg --config-settings=key=value\n",
        // Every option on a line counts, not only the first.
        "--no-index --requirement nested.txt\n",
        "--prefer-binary -r nested.txt\n",
        "-i https://example.invalid/simple -e .\n",
        "--no-index=1\n",
        "--index-url\n",
        "requests --hash=sha256:aaaa -r nested.txt\n",
        "requests -c constraints.txt\n",
        "requests --hash=sha256:aaaa \"-r\" nested.txt\n",
        "${PKG}\n",
        "requests\n.\n",
    ] {
        assert!(
            !binary_only_requirements(unsafe_file.as_bytes()),
            "{unsafe_file:?}"
        );
    }
    assert!(!binary_only_requirements(&[0xff, 0xfe]));
}

#[test]
fn a_manager_program_the_repository_configures_needs_install_scripts() {
    let root = std::env::temp_dir().join(format!("devworks-tool-rc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for directory in [".devworks-home", ".cargo", ".devworks-cache/cargo"] {
        std::fs::create_dir_all(root.join(directory)).expect("test root");
    }
    for name in ["yarn.lock", "Cargo.toml", "Cargo.lock", "package.json"] {
        assert!(!install_runs_repository_code(name, &root), "{name}");
    }
    // A toolchain file without a path is fine.
    std::fs::write(
        root.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.95\"\n",
    )
    .expect("toolchain");
    assert!(!install_runs_repository_code("Cargo.toml", &root));
    for (name, file, text) in [
        (
            "yarn.lock",
            ".yarnrc.yml",
            "\"yarn\\u0050ath\": ./local.cjs\n",
        ),
        ("yarn.lock", ".yarnrc", "yarn-path \"./tools/yarn.js\"\n"),
        ("yarn.lock", ".devworks-home/.yarnrc.yml", "yarnPath: x\n"),
        (
            "Cargo.toml",
            ".cargo/config.toml",
            "[build]\nrustc = \"./hook\"\n",
        ),
        (
            "Cargo.lock",
            ".cargo/config",
            "[build]\nrustc-wrapper = \"x\"\n",
        ),
        ("Cargo.toml", ".devworks-cache/cargo/config.toml", "[net]\n"),
        (
            "Cargo.toml",
            "rust-toolchain.toml",
            "[toolchain]\npath = \"./t\"\n",
        ),
        (
            "Cargo.toml",
            "rust-toolchain.toml",
            "[toolchain]\n\"\\u0070ath\" = \"t\"\n",
        ),
        ("Cargo.lock", "rust-toolchain", "./custom-toolchain\n"),
    ] {
        let before = std::fs::read(root.join(file)).ok();
        std::fs::write(root.join(file), text).expect("config");
        assert!(install_runs_repository_code(name, &root), "{file}: {text}");
        assert!(!install_runs_repository_code("package.json", &root));
        match before {
            Some(bytes) => std::fs::write(root.join(file), bytes).expect("restore"),
            None => std::fs::remove_file(root.join(file)).expect("remove config"),
        }
    }
    std::fs::remove_dir_all(root).expect("remove test root");
}

/// DW-362: a Yarn 2+ `.yarnrc.yml` of only inert settings installs without scripts; one
/// naming a program, loading code, or using YAML that another reader could resolve to a
/// different key (anchors, aliases, merge keys, tags, a second document) does not.
#[test]
fn only_a_yarn_rc_file_naming_no_program_installs_without_scripts() {
    let root = std::env::temp_dir().join(format!("devworks-tool-yarn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("test root");
    let inert = [
        "",
        "# only a comment\n",
        "nodeLinker: node-modules\nnpmRegistryServer: \"https://registry.example.invalid\"\n",
        "nodeLinker: pnpm\nenableGlobalCache: false\ncompressionLevel: mixed\n\
             cacheFolder: ./.yarn/cache\nglobalFolder: ./.yarn/global\nenableTelemetry: false\n",
        "npmScopes:\n  acme:\n    npmRegistryServer: https://npm.example.invalid\n    \
             npmAlwaysAuth: true\n    npmAuthToken: t\n",
        "---\nnodeLinker: pnp\nhttpTimeout: 60000\n",
        "defaultProtocol: \"npm:\"\n",
    ];
    for text in inert {
        std::fs::write(root.join(".yarnrc.yml"), text).expect("rc");
        assert!(!install_runs_repository_code("yarn.lock", &root), "{text}");
    }
    let refused = [
        "yarnPath: .yarn/releases/yarn-4.cjs\n",
        "\"yarn\\u0050ath\": ./local.cjs\n",
        "plugins:\n  - ./plugin.cjs\n",
        "plugins: []\n",
        "injectEnvironmentFiles:\n  - .env\n",
        "constraintsPath: ./c.pro\n",
        "packageExtensions:\n  a@*:\n    dependencies:\n      b: \"*\"\n",
        "nodeLinker: custom\n",
        "nodeLinker: node-modules\nunknownSetting: 1\n",
        "npmRegistryServer: file:///etc\n",
        "npmScopes:\n  acme:\n    yarnPath: x\n",
        "enableStrictSsl: false\n",
        "installStatePath: ./state\n",
        // YAML that another reader could resolve differently.
        "base: &base\n  nodeLinker: node-modules\nother: *base\n",
        "anchored: &a node-modules\nnodeLinker: *a\n",
        "defaults: &d\n  nodeLinker: node-modules\n<<: *d\n",
        "<<: {yarnPath: x}\nnodeLinker: pnp\n",
        "\"<<\": {nodeLinker: pnp}\n",
        "nodeLinker: !!str pnp\n",
        "!!map\nnodeLinker: pnp\n",
        "nodeLinker: pnp\n---\nyarnPath: x\n",
        "nodeLinker: pnp\nnodeLinker: pnp\n",
        "? [a, b]\n: c\n",
        "- nodeLinker\n",
        "just a string\n",
        "nodeLinker: [unterminated\n",
        "defaultProtocol: \"exec:\"\n",
        "defaultProtocol: \"file:\"\n",
        "# yarn lockfile v1\nnodeLinker: pnp\n",
        "# Yarn   Lockfile \t V1\r\nnodeLinker: pnp\r\n",
        "npmRegistryServer: \"https://r.example.invalid/${HOME}\"\n",
        "cacheFolder: \"${TMPDIR}/c\"\n",
    ];
    for text in refused {
        std::fs::write(root.join(".yarnrc.yml"), text).expect("rc");
        assert!(install_runs_repository_code("yarn.lock", &root), "{text}");
    }
    // Yarn 1's file is still refused, and so is a link in place of the file.
    std::fs::remove_file(root.join(".yarnrc.yml")).expect("remove");
    std::fs::write(root.join(".yarnrc"), "--install.frozen-lockfile true\n").expect("rc");
    assert!(install_runs_repository_code("yarn.lock", &root));
    std::fs::remove_file(root.join(".yarnrc")).expect("remove");
    std::fs::write(root.join("target.yml"), "nodeLinker: pnp\n").expect("target");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("target.yml"), root.join(".yarnrc.yml"))
            .expect("link");
        assert!(install_runs_repository_code("yarn.lock", &root));
    }
    std::fs::remove_dir_all(root).expect("remove test root");
}

/// DW-362: Yarn reads `.yarnrc.yml` and `.yarnrc` from every ancestor of the checkout, so one
/// there refuses a script-free install unless it is in a DevWorks-owned host folder.
#[test]
fn a_yarn_rc_file_above_the_checkout_needs_scripts() {
    let top = std::env::temp_dir().join(format!("devworks-yarn-up-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&top);
    let root = top.join("a/b/checkout");
    std::fs::create_dir_all(&root).expect("root");
    // The temporary folder itself may have an rc file above it; the test only asserts
    // relative to files it creates.
    let base = install_runs_repository_code("yarn.lock", &root);
    for (file, text) in [(".yarnrc.yml", "nodeLinker: pnp\n"), (".yarnrc", "x\n")] {
        let rc = top.join("a").join(file);
        std::fs::write(&rc, text).expect("rc");
        assert!(install_runs_repository_code("yarn.lock", &root), "{file}");
        std::fs::remove_file(&rc).expect("remove");
    }
    assert_eq!(install_runs_repository_code("yarn.lock", &root), base);
    std::fs::remove_dir_all(top).expect("remove");
}

/// DW-362: a script-free Yarn install overrides, by environment (which the install clears
/// first), the settings `.yarnrc.yml` could use to run a release or to write outside the
/// host's home.
#[test]
fn a_script_free_yarn_install_overrides_rc_program_and_folder_settings() {
    let (root, home) = (Path::new("/repo"), Path::new("/host/home"));
    let berry = b"__metadata:\n  version: 8\n".as_slice();
    let off = install_command("yarn.lock", root, home, berry, false, None).expect("yarn");
    // Paths are built with the platform's separator, as the install builds them.
    let global = home.join(".yarn/berry").to_string_lossy().into_owned();
    for (key, value) in [
        ("YARN_ENABLE_SCRIPTS", "false"),
        ("YARN_IGNORE_PATH", "true"),
        ("YARN_GLOBAL_FOLDER", global.as_str()),
    ] {
        assert!(
            off.env.contains(&(key.to_owned(), value.to_owned())),
            "{key}: {:?}",
            off.env
        );
    }
    let cache = install_home_environment(home);
    let cache_folder = home.join(".cache/yarn").to_string_lossy().into_owned();
    assert!(cache.contains(&("YARN_CACHE_FOLDER".into(), cache_folder)));
}

/// DW-319: a Cargo configuration of only inert settings installs without scripts; one
/// naming a program (or anything not on the allow-list, or that does not parse) does not.
#[test]
fn only_cargo_configuration_naming_no_program_installs_without_scripts() {
    let root = std::env::temp_dir().join(format!("devworks-tool-cargo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join(".cargo")).expect("test root");
    let inert = [
        "",
        "[registries.internal]\nindex = \"sparse+https://cargo.example.invalid/index/\"\n",
        "[registries.internal]\nindex = \"https://example.invalid/i\"\ntoken = \"t\"\n\
             protocol = \"sparse\"\n[registry]\ndefault = \"internal\"\n",
        "[source.crates-io]\nreplace-with = \"vendored\"\n\
             [source.vendored]\ndirectory = \"vendor\"\n",
        "[alias]\nxtask = \"run --package xtask --\"\nfetch = \"run\"\n",
        "[net]\nretry = 3\ngit-fetch-with-cli = true\noffline = false\n",
        "[http]\nproxy = \"http://proxy.example.invalid:3128\"\ntimeout = 30\n",
        "[build]\njobs = 4\ntarget-dir = \"out\"\ntarget = \"thumbv7em-none-eabihf\"\n",
        "[build]\ntarget = [\"x86_64-unknown-linux-gnu\", \"wasm32-unknown-unknown\"]\n",
        "[build]\nrustflags = [\"--cfg\", \"tokio_unstable\", \"-C\", \"target-cpu=native\"]\n",
        "[build]\nrustflags = \"-Dwarnings -Ctarget-feature=+crt-static --cfg=x\"\n",
        "[build]\nrustflags = [\"--cfg\", \"feature=\\\"serde\\\"\", \"--cap-lints\", \"warn\", \
             \"-D\", \"clippy::all\", \"--check-cfg\", \"cfg(tokio_unstable, values(\\\"x\\\"))\"]\n",
        "[target.'cfg(target_os = \"linux\")']\nrustflags = [\"-W\", \"unused\"]\n",
        "[profile.release]\nlto = true\ncodegen-units = 1\n[term]\nverbose = true\n",
        "[patch.crates-io]\nfoo = { path = \"../foo\" }\n[cargo-new]\nvcs = \"none\"\n",
        "paths = [\"../local\"]\n[resolver]\nincompatible-rust-versions = \"fallback\"\n",
    ];
    for file in ["config.toml", "config"] {
        for text in inert {
            std::fs::write(root.join(".cargo").join(file), text).expect("config");
            for name in ["Cargo.toml", "Cargo.lock"] {
                assert!(!install_runs_repository_code(name, &root), "{file}: {text}");
            }
        }
        std::fs::remove_file(root.join(".cargo").join(file)).expect("remove config");
    }
    let naming = [
        "[build]\nrustc = \"./planted\"\n",
        "[build]\nrustc-wrapper = \"./planted\"\n",
        "[build]\nrustc-workspace-wrapper = \"./planted\"\n",
        "[build]\nrustdoc = \"./planted\"\n",
        "build.rustc = \"./planted\"\n",
        "[build]\n\"rustc\\u002dwrapper\" = \"./planted\"\n",
        "[build]\nrustc_wrapper = \"./planted\"\n",
        "[build]\ntarget = \"./planted-spec.json\"\n",
        "[build]\ntarget = \"custom.JSON\"\n",
        "[build]\nrustflags = [\"-Z\", \"codegen-backend=./planted.so\"]\n",
        "[build]\nrustflags = \"-Ccodegen-backend=./planted.so\"\n",
        "[build]\nrustflags = [\"-C\", \"linker=./planted\"]\n",
        "[build]\nrustflags = [\"@./planted-args\"]\n",
        "[build]\nrustflags = [\"--sysroot\", \"./planted\"]\n",
        "[build]\nrustflags = [\"-C\"]\n",
        // rustc expands an `@file` argument wherever it stands, value positions included.
        "[build]\nrustflags = [\"--cfg\", \"@./args\"]\n",
        "[build]\nrustflags = [\"--check-cfg\", \"@./args\"]\n",
        "[build]\nrustflags = [\"-W\", \"@./args\"]\n",
        "[build]\nrustflags = [\"--cap-lints\", \"@./args\"]\n",
        "[build]\nrustflags = [\"-C\", \"@./args\"]\n",
        "[build]\nrustflags = [\"-C\", \"target-cpu=@./args\"]\n",
        "[build]\nrustflags = \"--cfg=@./args\"\n",
        "[build]\nrustflags = \"--cfg @./args\"\n",
        "[target.'cfg(unix)']\nrustflags = [\"--cfg\", \"@./args\"]\n",
        "[build]\nrustflags = [\"-D\", \"-Zcodegen-backend=./planted.so\"]\n",
        "[build]\nrustflags = [\"--cfg\", \"x=unquoted\"]\n",
        "[build]\nrustflags = [\"--cfg\", \"x=\\\"a\\\" --y\"]\n",
        "[build]\nrustflags = [\"--check-cfg\", \"values(@x)\"]\n",
        "[build]\nrustflags = [\"--cap-lints\", \"warnings\"]\n",
        "[build]\nrustdocflags = [\"--cfg\", \"x\"]\n",
        "[target.x86_64-unknown-linux-gnu]\nrunner = \"./planted\"\n",
        "[target.x86_64-unknown-linux-gnu]\nlinker = \"./planted\"\n",
        "[target.'cfg(unix)']\nrustflags = [\"-C\", \"link-arg=-fuse-ld=./planted\"]\n",
        "[target.x86_64-unknown-linux-gnu.openssl]\nrustc-link-lib = [\"ssl\"]\n",
        "[host]\nlinker = \"./planted\"\n",
        "[env]\nLD_PRELOAD = \"./planted.so\"\n",
        "[unstable]\nbuild-std = [\"std\"]\n",
        "include = \"./other.toml\"\n",
        "[doc]\nbrowser = \"./planted\"\n",
        "[registry]\ncredential-provider = \"./planted\"\n",
        "[registry]\nglobal-credential-providers = [\"./planted\"]\n",
        "[registries.internal]\ncredential-provider = [\"./planted\"]\n",
        "[credential-alias]\nplanted = \"./planted\"\n",
        "[net]\ngit-fetch-with-cli = true\ncommand = \"./planted\"\n",
        "[profile.dev]\nrustflags = [\"-Z\", \"x\"]\n",
        "[profile.dev.package.foo]\ncodegen-backend = \"./planted\"\n",
        "[source.planted]\nprocess = \"./planted\"\n",
        "[build]\nrustc = \"a\"\n[build]\njobs = 1\n",
        "not toml = = =\n",
    ];
    for text in naming {
        std::fs::write(root.join(".cargo/config.toml"), text).expect("config");
        assert!(install_runs_repository_code("Cargo.toml", &root), "{text}");
        // Other managers do not read it.
        assert!(!install_runs_repository_code("package.json", &root));
    }
    // An unsafe `config` counts even beside an inert `config.toml`, and invalid UTF-8 counts.
    std::fs::write(root.join(".cargo/config.toml"), "[net]\nretry = 1\n").expect("config");
    std::fs::write(
        root.join(".cargo/config"),
        "[build]\nrustc = \"./planted\"\n",
    )
    .expect("config");
    assert!(install_runs_repository_code("Cargo.lock", &root));
    std::fs::write(root.join(".cargo/config"), [0xff, 0xfe]).expect("config");
    assert!(install_runs_repository_code("Cargo.lock", &root));
    std::fs::remove_dir_all(root).expect("remove test root");
}

/// DW-386: a script-free Node install must not run repository code or read outside the
/// checkout: `exec:` and git dependencies and out-of-checkout `file:`/`link:`/`portal:`
/// paths, in any `package.json` or the lockfile, need `allowScripts`; registry dependencies,
/// `patch:`, and paths inside the checkout do not.
#[test]
fn a_dependency_that_runs_repository_code_needs_install_scripts() {
    let root = std::env::temp_dir().join(format!("devworks-tool-source-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("packages/a")).expect("test root");
    let managers = [
        "package-lock.json",
        "package.json",
        "pnpm-lock.yaml",
        "yarn.lock",
    ];
    let write = |file: &str, text: &str| std::fs::write(root.join(file), text).expect(file);
    let safe_lock = "__metadata:\n  version: 8\n\n\"exec@npm:^0.2.0\":\n  version: 0.2.0\n  \
             resolution: \"exec@npm:0.2.0\"\n  dependencies:\n    exec: ^1\n    \"@babel/core\": \"npm:^7\"\n\
             \"lodash@patch:lodash@npm%3A4.17.21#./p.patch::version=4.17.21&hash=1&locator=app%40workspace%3A.\":\n  \
             resolution: \"lodash@patch:lodash@npm%3A4.17.21#./p.patch::version=4.17.21&locator=app%40workspace%3A.\"\n  \
             resolved \"https://registry.yarnpkg.com/@babel/core/-/core-7.0.0.tgz#abc\"\n  \
             resolved \"https://npm.pkg.github.com/x/-/x-1.0.0.tgz\"\n  \
             \"link:./local\"\n  \"file:packages/a\"\n";
    write(
        "package.json",
        r#"{"name":"x","dependencies":{"lodash":"^4.17.21","a":"npm:b@^1","w":"workspace:*",
               "p":"patch:lodash@npm:4.17.21#./p.patch","l":"link:./local","f":"file:../root-sibling-ok-no"},
               "devDependencies":{"t":"https://example.invalid/t.tgz","s":"~1.2.3"}}"#,
    );
    // `file:../…` from the checkout root leaves it.
    assert!(install_runs_repository_code("package.json", &root));
    write(
        "package.json",
        r#"{"name":"x","dependencies":{"lodash":"^4.17.21","a":"npm:b@^1","w":"workspace:*",
               "p":"patch:lodash@npm:4.17.21#./p.patch","l":"link:./local","f":"file:packages/a",
               "e":"exec"},"devDependencies":{"t":"https://example.invalid/t.tgz","s":"~1.2.3",
               "dir":"./vendor/x","pr":"portal:./vendor/y"},"resolutions":{"lodash":"4.17.21"}}"#,
    );
    write(
        "packages/a/package.json",
        r#"{"dependencies":{"up":"link:../b"}}"#,
    );
    for name in managers {
        match name {
            "package.json" => {}
            "package-lock.json" => write(
                name,
                r#"{"lockfileVersion":3,"packages":{"":{},"node_modules/x":{"resolved":"https://registry.npmjs.org/x/-/x-1.tgz"},"node_modules/l":{"resolved":"local","link":true},"local":{}}}"#,
            ),
            _ => write(name, safe_lock),
        }
        assert!(!install_runs_repository_code(name, &root), "{name}");
    }
    let unsafe_specs = [
        "exec:./gen.js",
        "EXEC:./gen.js",
        "npm:foo@exec:./gen.js",
        "git+https://example.invalid/r.git",
        "git://example.invalid/r",
        "git@github.com:o/r.git",
        "github:o/r",
        "gitlab:o/r",
        "bitbucket:o/r",
        "o/r",
        "o/r#semver:^1",
        "https://example.invalid/r.git#abc",
        "https://github.com/o/r/archive/main.tar.gz",
        "file:/etc",
        "file:../outside",
        "link:~/x",
        "portal:../../x",
        "portal:C:/x",
        "../outside",
        "/abs",
        "~/home",
        "file:./a/../../out",
    ];
    for spec in unsafe_specs {
        for (field, wrapped) in [
            (
                "dependencies",
                format!(r#"{{"dependencies":{{"x":"{spec}"}}}}"#),
            ),
            (
                "devDependencies",
                format!(r#"{{"devDependencies":{{"x":"{spec}"}}}}"#),
            ),
            (
                "optionalDependencies",
                format!(r#"{{"optionalDependencies":{{"x":"{spec}"}}}}"#),
            ),
            (
                "peerDependencies",
                format!(r#"{{"peerDependencies":{{"x":"{spec}"}}}}"#),
            ),
            (
                "resolutions",
                format!(r#"{{"resolutions":{{"x@npm:1":"{spec}"}}}}"#),
            ),
            (
                "overrides",
                format!(r#"{{"overrides":{{"x":{{"y":"{spec}"}}}}}}"#),
            ),
            (
                "pnpm",
                format!(r#"{{"pnpm":{{"overrides":{{"x":"{spec}"}}}}}}"#),
            ),
        ] {
            write("package.json", &wrapped);
            for name in managers {
                if name != "package.json" {
                    let _ = std::fs::remove_file(root.join(name));
                }
                assert!(install_runs_repository_code(name, &root), "{field}: {spec}");
            }
        }
        // A workspace package.json is checked too.
        write("package.json", r#"{"name":"x"}"#);
        write(
            "packages/a/package.json",
            &format!(r#"{{"dependencies":{{"x":"{spec}"}}}}"#),
        );
        // From `packages/a` (two folders down), up to two `..` stay inside the checkout.
        let inside = matches!(
            spec,
            "file:../outside" | "portal:../../x" | "../outside" | "file:./a/../../out"
        );
        assert_eq!(
            install_runs_repository_code("yarn.lock", &root),
            !inside,
            "workspace: {spec}"
        );
        write("packages/a/package.json", r#"{"name":"a"}"#);
    }
    // Lockfiles: a Berry resolution, an exec: descriptor, a Yarn 1 git line, a pnpm git
    // resolution, an npm git `resolved`.
    for (name, text) in [
        (
            "yarn.lock",
            "\"x@exec:./gen.js\":\n  resolution: \"x@exec:./gen.js::locator=a%40workspace%3A.\"\n",
        ),
        (
            "yarn.lock",
            "\"x@patch:x@exec%3A./gen.js#./p.patch\":\n  version: 1\n",
        ),
        ("yarn.lock", "  dependencies:\n    x: exec:./gen.js\n"),
        (
            "yarn.lock",
            "  resolution: \"x@https://github.com/o/r.git#commit=abc\"\n",
        ),
        (
            "yarn.lock",
            "x@o/r:\n  resolved \"git+ssh://git@github.com/o/r.git#abc\"\n",
        ),
        (
            "yarn.lock",
            "  resolution: \"x@portal:../x::locator=a%40workspace%3A.\"\n",
        ),
        (
            "pnpm-lock.yaml",
            "packages:\n  x@1.0.0:\n    resolution: {commit: abc, repo: https://example.invalid/r, type: git}\n",
        ),
        ("pnpm-lock.yaml", "    x:\n      version: link:../x\n"),
        (
            "package-lock.json",
            "{\"packages\":{\"node_modules/x\":{\"resolved\":\"git+ssh://git@github.com/o/r.git#abc\"}}}",
        ),
        (
            "package-lock.json",
            "{\"packages\":{\"node_modules/x\":{\"resolved\":\"git+https://example.invalid/r\"}}}",
        ),
    ] {
        for other in managers {
            let _ = std::fs::remove_file(root.join(other));
        }
        write("package.json", r#"{"name":"x"}"#);
        write(name, text);
        assert!(install_runs_repository_code(name, &root), "{name}: {text}");
    }
    // An unparseable workspace package.json cannot be ruled out.
    for name in managers {
        let _ = std::fs::remove_file(root.join(name));
    }
    write("package.json", r#"{"name":"x"}"#);
    write("packages/a/package.json", "{");
    assert!(install_runs_repository_code("package.json", &root));
    std::fs::remove_dir_all(root).expect("remove test root");
}

/// DW-386 review: escapes, bare paths, workspace globs, patches and sibling links.
#[test]
fn lockfile_escapes_bare_paths_and_globs_are_checked() {
    let root = std::env::temp_dir().join(format!("devworks-tool-lockfiles-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("test root");
    let managers = [
        "package-lock.json",
        "npm-shrinkwrap.json",
        "pnpm-lock.yaml",
        "pnpm-workspace.yaml",
        "yarn.lock",
        "package.json",
    ];
    let reset = |files: &[(&str, &str)]| {
        for name in managers {
            let _ = std::fs::remove_file(root.join(name));
        }
        std::fs::write(root.join("package.json"), r#"{"name":"x"}"#).expect("package");
        for (name, text) in files {
            std::fs::write(root.join(name), text).expect(name);
        }
    };
    let refused = |manager: &str, files: &[(&str, &str)]| {
        reset(files);
        assert!(install_runs_repository_code(manager, &root), "{files:?}");
    };
    let allowed = |manager: &str, files: &[(&str, &str)]| {
        reset(files);
        assert!(!install_runs_repository_code(manager, &root), "{files:?}");
    };
    // Escapes that hide a source.
    refused(
        "yarn.lock",
        &[("yarn.lock", "\"x@\\u0065xec:./g.js\":\n  version: 1\n")],
    );
    refused(
        "yarn.lock",
        &[("yarn.lock", "x:\n  resolution: \"x@exec\\u003a./g.js\"\n")],
    );
    refused(
        "yarn.lock",
        &[("yarn.lock", "x:\n  resolution: \"x@ex\\\nec:./g.js\"\n")],
    );
    refused(
        "pnpm-lock.yaml",
        &[("pnpm-lock.yaml", "a: \"\\x65xec:./g.js\"\n")],
    );
    refused(
        "pnpm-lock.yaml",
        &[("pnpm-lock.yaml", "a: \"ex\\\n  ec:./g.js\"\n")],
    );
    for name in ["package-lock.json", "npm-shrinkwrap.json"] {
        refused(
            "package-lock.json",
            &[(
                name,
                r#"{"packages":{"node_modules/x":{"resolved":"\u0065xec:./g.js"}}}"#,
            )],
        );
        refused(
            "package-lock.json",
            &[(
                name,
                r#"{"packages":{"node_modules/x":{"resolved":"exec\u003a./g.js"}}}"#,
            )],
        );
        refused(
            "package-lock.json",
            &[(
                name,
                r#"{"packages":{"node_modules/x":{"resolved":"git+ssh://git@github.com/o/r.git"}}}"#,
            )],
        );
        refused("package-lock.json", &[(name, "{")]);
        // Funding metadata naming GitHub is not a source; a git resolution beside it is.
        allowed(
            "package-lock.json",
            &[(
                name,
                r#"{"packages":{"node_modules/x":{"resolved":"https://registry.npmjs.org/x/-/x-1.0.0.tgz","funding":{"url":"https://github.com/sponsors/o"},"license":"MIT"}}}"#,
            )],
        );
        refused(
            "package-lock.json",
            &[(
                name,
                r#"{"packages":{"node_modules/x":{"funding":{"url":"https://github.com/sponsors/o"},"dependencies":{"y":"github:o/y"}}}}"#,
            )],
        );
        // A linked package and a package location outside the checkout.
        refused(
            "package-lock.json",
            &[(
                name,
                r#"{"packages":{"node_modules/x":{"resolved":"../outside","link":true}}}"#,
            )],
        );
        refused(
            "package-lock.json",
            &[(name, r#"{"packages":{"../outside":{}}}"#)],
        );
        refused(
            "package-lock.json",
            &[(
                name,
                r#"{"packages":{"node_modules/x":{"resolved":"C:\\x","link":true}}}"#,
            )],
        );
    }
    allowed(
        "package-lock.json",
        &[(
            "npm-shrinkwrap.json",
            r#"{"packages":{"node_modules/x":{"resolved":"pkgs/x","link":true},"pkgs/x":{}}}"#,
        )],
    );
    // Bare paths in pnpm lockfiles, and a drive path.
    refused(
        "pnpm-lock.yaml",
        &[(
            "pnpm-lock.yaml",
            "packages:\n  x@file:../x:\n    resolution: {directory: ../x, type: directory}\n",
        )],
    );
    refused(
        "pnpm-lock.yaml",
        &[(
            "pnpm-lock.yaml",
            "packages:\n  x:\n    resolution:\n      directory: /abs\n",
        )],
    );
    refused(
        "pnpm-lock.yaml",
        &[(
            "pnpm-lock.yaml",
            "packages:\n  x:\n    resolution: {directory: 'C:/x'}\n",
        )],
    );
    allowed(
        "pnpm-lock.yaml",
        &[(
            "pnpm-lock.yaml",
            "packages:\n  x@file:vendor/x:\n    resolution: {directory: vendor/x, type: directory}\n",
        )],
    );
    // Sibling links resolve against the importer (pnpm) or workspace (Berry).
    let importers = |link: &str| {
        format!(
            "importers:\n  .:\n    dependencies:\n      r:\n        version: link:./r\n  packages/a:\n    dependencies:\n      b:\n        specifier: workspace:*\n        version: {link}\n"
        )
    };
    allowed(
        "pnpm-lock.yaml",
        &[("pnpm-lock.yaml", &importers("link:../b"))],
    );
    refused(
        "pnpm-lock.yaml",
        &[("pnpm-lock.yaml", &importers("link:../../../b"))],
    );
    refused(
        "pnpm-lock.yaml",
        &[(
            "pnpm-lock.yaml",
            "importers:\n  ../x:\n    dependencies: {}\n",
        )],
    );
    // The root importer cannot climb.
    refused(
        "pnpm-lock.yaml",
        &[(
            "pnpm-lock.yaml",
            "importers:\n  .:\n    dependencies:\n      b:\n        version: link:../b\n",
        )],
    );
    allowed(
        "yarn.lock",
        &[(
            "yarn.lock",
            "\"b@portal:../b::locator=a%40workspace%3Apackages/a\":\n  resolution: \"b@portal:../b::locator=a%40workspace%3Apackages/a\"\n\n\"a@workspace:packages/a\":\n  dependencies:\n    b: portal:../b\n",
        )],
    );
    refused(
        "yarn.lock",
        &[(
            "yarn.lock",
            "\"b@portal:../../../b::locator=a%40workspace%3Apackages/a\":\n  version: 0.0.0\n",
        )],
    );
    refused(
        "yarn.lock",
        &[("yarn.lock", "\"b@portal:../b\":\n  version: 0.0.0\n")],
    );
    // pnpm-workspace.yaml.
    allowed(
        "pnpm-lock.yaml",
        &[(
            "pnpm-workspace.yaml",
            "packages:\n  - packages/*\n  - '!**/test/**'\ncatalog:\n  a: ^1\noverrides:\n  b: npm:c@^1\n",
        )],
    );
    for text in [
        "packages:\n  - ../other/*\n",
        "packages:\n  - /abs/*\n",
        "packages:\n  - packages/*/../../..\n",
        "overrides:\n  a: exec:./g.js\n",
        "catalog:\n  a: github:o/r\n",
        "catalogs:\n  x:\n    a: link:../x\n",
        "overrides: &o\n  a: ^1\n",
        "overrides:\n  a: !!str exec:./g.js\n",
        "catalog:\n  a: *x\n",
        "packages: [",
    ] {
        refused("pnpm-lock.yaml", &[("pnpm-workspace.yaml", text)]);
    }
    // package.json: pnpm patches and extensions, workspace globs, Windows paths.
    allowed(
        "package.json",
        &[(
            "package.json",
            r#"{"workspaces":["packages/*"],"pnpm":{"patchedDependencies":{"x@1":"patches/x.patch"}}}"#,
        )],
    );
    allowed(
        "package.json",
        &[(
            "package.json",
            r#"{"workspaces":{"packages":["apps/*"]},"bundleDependencies":["../not-a-path"]}"#,
        )],
    );
    for text in [
        r#"{"pnpm":{"patchedDependencies":{"x@1":"../outside.patch"}}}"#,
        r#"{"pnpm":{"patchedDependencies":{"x@1":"/abs.patch"}}}"#,
        r#"{"pnpm":{"packageExtensions":{"x":{"dependencies":{"y":"exec:./g.js"}}}}}"#,
        r#"{"workspaces":["../*"]}"#,
        r#"{"workspaces":{"packages":["/abs/*"]}}"#,
        r#"{"workspaces":["packages/*/../../.."]}"#,
        r#"{"dependencies":{"x":"C:\\x"}}"#,
        r#"{"dependencies":{"x":"C:/x"}}"#,
        r#"{"dependencies":{"x":"..\\x"}}"#,
        r#"{"dependencies":{"x":".\\x"}}"#,
    ] {
        refused("package.json", &[("package.json", text)]);
    }
    // Windows drive and backslash paths in lockfiles.
    refused(
        "yarn.lock",
        &[("yarn.lock", "x:\n  dependencies:\n    y: file:..\\y\n")],
    );
    refused(
        "yarn.lock",
        &[("yarn.lock", "x:\n  dependencies:\n    y: C:/y\n")],
    );
    std::fs::remove_dir_all(root).expect("remove test root");
}

/// DW-318: Corepack downloads and runs the release a `packageManager` URL names before any
/// manager sees a script flag, so a Node install with one (or with a `package.json` whose
/// field cannot be checked) needs `allowScripts`; a registry version is fine.
#[test]
fn a_corepack_release_url_in_package_json_needs_install_scripts() {
    let root = std::env::temp_dir().join(format!("devworks-tool-corepack-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("test root");
    let managers = [
        "package-lock.json",
        "package.json",
        "pnpm-lock.yaml",
        "yarn.lock",
    ];
    for name in managers {
        assert!(!install_runs_repository_code(name, &root), "{name}");
    }
    for safe in [
        r#"{"name":"x"}"#,
        r#"{"packageManager":"yarn@1.22.22"}"#,
        r#"{"packageManager":"pnpm@9.15.0+sha512.76e2379760a4328ec4415815bcd6628dee727af3779aaa4c914e3944156c4299921a89f976381ee107d41f12cfa4b66681ca9c718f0668fa0831ed4c6d8ba56c"}"#,
    ] {
        std::fs::write(root.join("package.json"), safe).expect("package.json");
        for name in managers {
            assert!(!install_runs_repository_code(name, &root), "{name}: {safe}");
        }
    }
    for unsafe_file in [
        r#"{"packageManager":"yarn@https://example.invalid/yarn-4.0.0.tgz"}"#,
        r#"{"packageManager":"yarn@https://example.invalid/yarn.tgz#sha1.0"}"#,
        r#"{"packageManager":"yarn@1.22.22","packageManager":"yarn@file:./yarn.js"}"#,
        r#"{"packageManager":"yarn@ 1.22.22"}"#,
        r#"{"packageManager":["yarn@1.22.22"]}"#,
        r#"{"packageManager":"yarn@1.22.22","x":"\ud800"}"#,
        "[]",
    ] {
        std::fs::write(root.join("package.json"), unsafe_file).expect("package.json");
        for name in managers {
            assert!(
                install_runs_repository_code(name, &root),
                "{name}: {unsafe_file}"
            );
        }
        // Other managers do not read the field.
        assert!(!install_runs_repository_code("go.mod", &root));
    }
    std::fs::remove_dir_all(root).expect("remove test root");
}

/// DW-318: every install keeps its home, temporary folder and package caches in the host's
/// folder for the checkout, and a script-free install overrides each repository setting
/// the audit found that names a program.
#[test]
fn install_homes_and_caches_are_the_hosts_and_program_settings_are_overridden() {
    let home = Path::new("/host/home");
    let env = install_home_environment(home);
    for name in [
        "HOME",
        "TMPDIR",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "CARGO_HOME",
        "npm_config_cache",
        "YARN_CACHE_FOLDER",
        "COREPACK_HOME",
        "PIP_CACHE_DIR",
        "UV_CACHE_DIR",
        "GOPATH",
        "GOMODCACHE",
        "COMPOSER_HOME",
        "COMPOSER_CACHE_DIR",
    ] {
        let value = env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| Path::new(value))
            .unwrap_or_else(|| panic!("{name} is unset"));
        assert!(value.starts_with(home), "{name}={value:?}");
    }
    let env = script_free_environment();
    let value = |name: &str| {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    let shell = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" };
    for (name, expected) in [
        ("GIT_SSH_COMMAND", "ssh"),
        ("GIT_PROXY_COMMAND", ""),
        ("GIT_ASKPASS", ""),
        ("npm_config_git", "git"),
        ("npm_config_shell", shell),
        ("npm_config_script_shell", shell),
        ("npm_config_ignore_scripts", "true"),
        ("COREPACK_ENV_FILE", "0"),
        ("GOENV", "off"),
        ("GOTOOLCHAIN", "local"),
    ] {
        assert_eq!(value(name), Some(expected), "{name}");
    }
    let count: usize = value("GIT_CONFIG_COUNT").expect("count").parse().unwrap();
    let git: Vec<(&str, &str)> = (0..count)
        .map(|index| {
            (
                value(&format!("GIT_CONFIG_KEY_{index}")).unwrap(),
                value(&format!("GIT_CONFIG_VALUE_{index}")).unwrap(),
            )
        })
        .collect();
    for setting in [
        ("credential.helper", ""),
        ("core.fsmonitor", "false"),
        ("protocol.ext.allow", "never"),
        ("safe.bareRepository", "explicit"),
    ] {
        assert!(git.contains(&setting), "{setting:?} in {git:?}");
    }
    assert!(git.iter().any(|(key, _)| *key == "core.hooksPath"));
}

#[test]
fn install_scripts_run_only_for_the_user_an_approved_replay_or_a_native_grant() {
    let ctx = |kind| {
        InvocationContext::new(
            crate::protocol::ActorId(id("caller-1")),
            kind,
            id("invocation-1"),
            id("ingress-1"),
            crate::protocol::StoreId(id("store-1")),
            counter(0),
            rev(1),
        )
    };
    assert!(scripts_approved(&ctx(PrincipalKind::Human), false));
    for kind in [
        PrincipalKind::Agent,
        PrincipalKind::Host,
        PrincipalKind::Package,
        PrincipalKind::McpClient,
        PrincipalKind::Schedule,
    ] {
        assert!(!scripts_approved(&ctx(kind), false), "{kind:?}");
        assert!(scripts_approved(&ctx(kind), true), "{kind:?}");
    }
    let reviewed =
        ctx(PrincipalKind::Agent).with_reviewed_operation(crate::context::ReviewedOperation {
            approval_id: id("approval-1"),
            approver: crate::protocol::ActorId(id("user-1")),
            operation: "process.install_project_dependencies".into(),
            schema_revision: 8,
            request_digest: "a".repeat(64),
            owner_binding: json!({}),
        });
    assert!(scripts_approved(&reviewed, false));
}

#[test]
fn process_credential_is_resolved_only_from_registered_project_handle() {
    use crate::ports::secrets::{Secret, SecretStore, SessionSecrets};
    let base = std::env::current_dir().expect("current directory");
    let executable = std::env::current_exe()
        .expect("test binary")
        .canonicalize()
        .expect("canonical test binary");
    let project = ProjectId(id("project-1"));
    let bindings = ToolBindings::new(
        [RegisteredExecutable {
            id: id("test-executable"),
            path: executable,
        }],
        [RegisteredWorkingRoot {
            id: id("test-root"),
            project: Some(project.clone()),
            path: base.canonicalize().expect("canonical test root"),
        }],
    )
    .expect("bindings")
    .with_credentials([RegisteredCredential {
        id: id("credential-1"),
        project: Some(project.clone()),
        handle: id("secret-handle-1"),
    }])
    .expect("credential registry");
    let secrets = SessionSecrets::new();
    secrets
        .put(
            &SecretKey::new("store-1", id("secret-handle-1")),
            &Secret::new("private-token"),
        )
        .expect("store secret");
    let mut input = spec("Full", true);
    input.working_directory.relative.0 = String::new();
    let mut native = bindings
        .resolve(&input, Some(&project))
        .expect("bound spec");
    assert!(native.env.is_empty());
    let redactions = bindings
        .resolve_credentials(&input, Some(&project), &secrets, "store-1", &mut native)
        .expect("late secret resolution");
    assert_eq!(redactions, vec!["private-token"]);
    assert_eq!(native.env, vec![("TOKEN".into(), "private-token".into())]);
    assert_eq!(
        bindings.resolve(&input, Some(&ProjectId(id("project-2")))),
        Err(BindingRefusal::OutOfProject)
    );
    let intent = serde_json::to_string(&input).expect("serialize intent");
    assert!(!intent.contains("private-token"));
}
